//! Pluggable background task queue for CMS heavy work (#432).
//!
//! Wagtail moved heavy post-request work (cache purge, search/reference
//! indexing, scheduled publish) onto a pluggable, retriable queue. The
//! `rustango` framework already ships the execution machinery —
//! [`rustango::jobs`]: a [`Job`] trait + a [`JobQueue`] with an
//! in-memory backend and a persistent Postgres backend
//! (retry/backoff/dead-letter). This module **wires CMS work onto it**.
//!
//! ## Why a `TaskSink`
//!
//! `JobQueue`'s methods are generic (`dispatch::<T>`), so it isn't
//! object-safe and the framework has no process-global queue — fine
//! for an app that threads its concrete queue around, but a *library*
//! buried under `on_commit` can't reach it. So the CMS keeps a small
//! **object-safe [`TaskSink`]** in a process global. Hosts wrap their
//! concrete queue in [`JobQueueSink`] (a blanket adapter) and install
//! it with [`set_sink`].
//!
//! ## Inline default
//!
//! With no sink installed (the common single-process deploy) the work
//! runs **inline** (synchronously) — identical to the pre-#432
//! detached-spawn behavior, just without retry/persistence.
//!
//! ## Context-free jobs
//!
//! [`Job::run`] receives only its deserialized payload. Tenant-scoped
//! jobs resolve their resources from a process registry (the pattern
//! `rustango::email_jobs` uses for its mailer): [`register_invalidator`]
//! stashes a tenant's [`PageCacheInvalidator`] so a deserialized
//! [`CachePurgeJob`] can find it in `run()`.

use crate::log_err::LogErr as _;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;
use rustango::jobs::{Job, JobError, JobQueue};
use serde::{Deserialize, Serialize};

use crate::cache_invalidate::PageCacheInvalidator;

// ---- object-safe sink + process-global handle --------------------

/// Object-safe enqueue surface the CMS holds globally. One method per
/// CMS job kind (the set is small + CMS-owned). Hosts don't implement
/// this — they wrap their concrete [`JobQueue`] in [`JobQueueSink`].
#[async_trait]
pub trait TaskSink: Send + Sync + 'static {
    /// Enqueue a [`CachePurgeJob`] for asynchronous, retriable run.
    async fn dispatch_cache_purge(&self, job: CachePurgeJob) -> Result<(), JobError>;

    /// Enqueue a [`ReferenceIndexJob`] for asynchronous, retriable run.
    async fn dispatch_reference_index(&self, job: ReferenceIndexJob) -> Result<(), JobError>;

    /// Enqueue a [`SearchIndexJob`] for asynchronous, retriable run.
    async fn dispatch_search_index(&self, job: SearchIndexJob) -> Result<(), JobError>;
}

static SINK: OnceLock<Arc<dyn TaskSink>> = OnceLock::new();

/// Install the background task sink. Call once at startup, after
/// [`register_cms_jobs`] + the queue's `start()`. Without it, CMS
/// background work runs inline.
pub fn set_sink(sink: Arc<dyn TaskSink>) {
    // Set once; a second install would be silently lost otherwise (#697).
    if SINK.set(sink).is_err() {
        tracing::error!(target: "rustango_cms", "set_sink: a task sink is already installed; this one is ignored");
    }
}

fn sink() -> Option<&'static Arc<dyn TaskSink>> {
    SINK.get()
}

/// Adapter: turn any concrete framework [`JobQueue`] into a
/// [`TaskSink`]. Hosts install it with
/// `set_sink(Arc::new(JobQueueSink(queue)))`.
pub struct JobQueueSink<Q: JobQueue>(pub Arc<Q>);

#[async_trait]
impl<Q: JobQueue> TaskSink for JobQueueSink<Q> {
    async fn dispatch_cache_purge(&self, job: CachePurgeJob) -> Result<(), JobError> {
        self.0.dispatch::<CachePurgeJob>(&job).await
    }
    async fn dispatch_reference_index(&self, job: ReferenceIndexJob) -> Result<(), JobError> {
        self.0.dispatch::<ReferenceIndexJob>(&job).await
    }
    async fn dispatch_search_index(&self, job: SearchIndexJob) -> Result<(), JobError> {
        self.0.dispatch::<SearchIndexJob>(&job).await
    }
}

/// Register every CMS job type on `queue`. Call once at startup before
/// the queue's `start()`; pair with [`set_sink`].
pub async fn register_cms_jobs<Q: JobQueue>(queue: &Q) {
    queue.register::<CachePurgeJob>().await;
    queue.register::<ReferenceIndexJob>().await;
    queue.register::<SearchIndexJob>().await;
}

// ---- per-tenant resource registry (for context-free run()) -------

fn invalidator_registry() -> &'static RwLock<HashMap<String, Arc<dyn PageCacheInvalidator>>> {
    static REG: OnceLock<RwLock<HashMap<String, Arc<dyn PageCacheInvalidator>>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Stash a tenant's cache invalidator so a deserialized
/// [`CachePurgeJob`] can resolve it in `run()`. Idempotent. Called
/// automatically by [`purge_urls`] from the in-hand invalidator, so
/// hosts don't normally call it.
pub fn register_invalidator(
    tenant_slug: impl Into<String>,
    invalidator: Arc<dyn PageCacheInvalidator>,
) {
    if let Ok(mut m) = invalidator_registry().write() {
        m.insert(tenant_slug.into(), invalidator);
    }
}

fn resolve_invalidator(tenant_slug: &str) -> Option<Arc<dyn PageCacheInvalidator>> {
    invalidator_registry()
        .read()
        .ok()
        .and_then(|m| m.get(tenant_slug).cloned())
        .or_else(|| DEFAULT_INVALIDATOR.get().cloned())
}

/// The invalidator a job falls back to when its tenant has no entry in
/// this process — a durable queue drained by a worker that never served
/// the tenant, or a job run after a restart (#732). Set automatically by
/// [`crate::admin::router_with_invalidator`]; a worker-only process calls
/// [`set_default_invalidator`] itself.
static DEFAULT_INVALIDATOR: OnceLock<Arc<dyn PageCacheInvalidator>> = OnceLock::new();

/// The process-wide invalidator, if one is set — for paths with no admin
/// state of their own, such as MCP tools (#692).
#[must_use]
pub fn default_invalidator() -> Option<Arc<dyn PageCacheInvalidator>> {
    DEFAULT_INVALIDATOR.get().cloned()
}

/// Set the fallback cache invalidator for queued purge jobs (#732). The
/// first call wins.
pub fn set_default_invalidator(invalidator: Arc<dyn PageCacheInvalidator>) {
    let _ = DEFAULT_INVALIDATOR.set(invalidator);
}

fn pool_registry() -> &'static RwLock<HashMap<String, rustango::sql::Pool>> {
    static REG: OnceLock<RwLock<HashMap<String, rustango::sql::Pool>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Stash a tenant's DB pool so a deserialized [`ReferenceIndexJob`] can
/// resolve it in `run()`. Idempotent. Called automatically by
/// [`reindex_page`] from the in-hand pool, so hosts don't normally
/// call it.
pub fn register_pool(tenant_slug: impl Into<String>, pool: rustango::sql::Pool) {
    if let Ok(mut m) = pool_registry().write() {
        m.insert(tenant_slug.into(), pool);
    }
}

fn registered_pool(tenant_slug: &str) -> Option<rustango::sql::Pool> {
    pool_registry().read().ok()?.get(tenant_slug).cloned()
}

/// The tenant registry, recorded by the boot-time seed so a job can find
/// a tenant this process never served (#732).
static REGISTRY: OnceLock<rustango::sql::Pool> = OnceLock::new();

/// Record the tenant registry pool for [`resolve_pool`]'s fallback.
/// Called by [`crate::seed::ensure_seeded`]; the first call wins.
pub fn set_registry(registry: rustango::sql::Pool) {
    let _ = REGISTRY.set(registry);
}

/// A tenant's pool: the one this process registered, or — for a durable
/// job drained by another process or after a restart — resolved from the
/// registry by slug, and then registered. `None` when neither knows it.
async fn resolve_pool(tenant_slug: &str) -> Option<rustango::sql::Pool> {
    if let Some(pool) = registered_pool(tenant_slug) {
        return Some(pool);
    }
    let registry = REGISTRY.get()?;
    let pool = scoped_pool_for(registry, tenant_slug).await?;
    register_pool(tenant_slug, pool.clone());
    Some(pool)
}

async fn scoped_pool_for(
    registry: &rustango::sql::Pool,
    tenant_slug: &str,
) -> Option<rustango::sql::Pool> {
    use rustango::core::Column as _;
    use rustango::tenancy::{Org, TenantPools};
    let org = Org::objects()
        .where_(Org::slug.eq(tenant_slug.to_owned()))
        .first(registry)
        .await
        .ok()??;
    match registry {
        #[cfg(feature = "postgres")]
        rustango::sql::Pool::Postgres(pg) => {
            TenantPools::<rustango::sql::sqlx::Postgres>::new(pg.clone()).scoped_pool_dyn(&org).await.ok()
        }
        #[cfg(feature = "sqlite")]
        rustango::sql::Pool::Sqlite(sq) => {
            TenantPools::<rustango::sql::sqlx::Sqlite>::new(sq.clone()).scoped_pool_dyn(&org).await.ok()
        }
        #[cfg(feature = "mysql")]
        rustango::sql::Pool::Mysql(my) => {
            TenantPools::<rustango::sql::sqlx::MySql>::new(my.clone()).scoped_pool_dyn(&org).await.ok()
        }
    }
}

// ---- cache-purge job ---------------------------------------------

/// Drop the public-cache entries for `urls` on one tenant. Enqueued
/// after a page mutation commits (#317/#432). Retriable — a transient
/// backend hiccup is retried with backoff; a missing tenant
/// registration is fatal (a startup-wiring gap, not transient).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachePurgeJob {
    pub tenant_slug: String,
    pub urls: Vec<String>,
}

#[async_trait]
impl Job for CachePurgeJob {
    const NAME: &'static str = "rcms.cache_purge";

    async fn run(&self) -> Result<(), JobError> {
        let Some(invalidator) = resolve_invalidator(&self.tenant_slug) else {
            return Err(JobError::Fatal(format!(
                "cache_purge: no invalidator registered for tenant `{}`",
                self.tenant_slug
            )));
        };
        // #428 — one batch call so a bulk-purge backend (Cloudflare
        // `files`, Azure `contentPaths`) collapses the save into a
        // single round-trip; the default impl still loops per-URL.
        invalidator
            .invalidate_urls(&self.tenant_slug, &self.urls)
            .await;
        Ok(())
    }
}

/// Purge `urls` for `tenant_slug`. Dispatches a retriable
/// [`CachePurgeJob`] when a sink is installed; otherwise purges
/// **inline** against the in-hand `invalidator` (today's behavior).
/// Drive this from the detached `on_commit` task in
/// [`crate::cache_invalidate::invalidate_urls_on_commit`].
pub async fn purge_urls(
    invalidator: Arc<dyn PageCacheInvalidator>,
    tenant_slug: String,
    urls: Vec<String>,
) {
    if urls.is_empty() {
        return;
    }
    let Some(sink) = sink() else {
        purge_inline(invalidator.as_ref(), &tenant_slug, &urls).await;
        return;
    };
    // Make the invalidator resolvable inside the deserialized job.
    register_invalidator(tenant_slug.clone(), invalidator.clone());
    let job = CachePurgeJob {
        tenant_slug: tenant_slug.clone(),
        urls: urls.clone(),
    };
    if let Err(e) = sink.dispatch_cache_purge(job).await {
        tracing::warn!(
            target: "rustango_cms::task_queue",
            error = %e,
            "cache_purge dispatch failed; purging inline so stale entries aren't stranded",
        );
        purge_inline(invalidator.as_ref(), &tenant_slug, &urls).await;
    }
}

async fn purge_inline(invalidator: &dyn PageCacheInvalidator, tenant_slug: &str, urls: &[String]) {
    // #428 — batch fan-out (see `CachePurgeJob::run`).
    invalidator.invalidate_urls(tenant_slug, urls).await;
}

// ---- reference-index job -----------------------------------------

/// Serializable form of [`crate::reference_index::Reference`]. The
/// `Reference` itself can't be deserialized (its `to_kind` is a
/// `&'static str`); the job payload carries owned strings and
/// [`to_reference`] maps `to_kind` back to the interned constant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedRef {
    pub to_kind: String,
    pub to_id: i64,
    pub field_path: String,
}

impl OwnedRef {
    fn from_ref(r: &crate::reference_index::Reference) -> Self {
        Self {
            to_kind: r.to_kind.to_owned(),
            to_id: r.to_id,
            field_path: r.field_path.clone(),
        }
    }

    /// Back to a `Reference` with an interned `to_kind`, or `None` for
    /// an unknown kind (forward-compat guard — skipped on rebuild).
    fn to_reference(&self) -> Option<crate::reference_index::Reference> {
        let kind = kind_const(&self.to_kind)?;
        Some(crate::reference_index::Reference {
            to_kind: kind,
            to_id: self.to_id,
            field_path: self.field_path.clone(),
        })
    }
}

fn kind_const(s: &str) -> Option<&'static str> {
    use crate::reference_index::{KIND_MEDIA, KIND_PAGE, KIND_SNIPPET};
    [KIND_PAGE, KIND_SNIPPET, KIND_MEDIA]
        .into_iter()
        .find(|k| *k == s)
}

/// Rebuild a page's outbound reference rows (#146/#432). Carries the
/// already-scanned refs in its payload, so `run()` only needs the
/// tenant pool (resolved from the registry) to replace the rows —
/// retriable if that write hits a transient error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceIndexJob {
    pub tenant_slug: String,
    pub page_id: i64,
    pub refs: Vec<OwnedRef>,
}

#[async_trait]
impl Job for ReferenceIndexJob {
    const NAME: &'static str = "rcms.reference_index";

    async fn run(&self) -> Result<(), JobError> {
        let Some(pool) = resolve_pool(&self.tenant_slug).await else {
            return Err(JobError::Fatal(format!(
                "reference_index: no pool registered for tenant `{}`",
                self.tenant_slug
            )));
        };
        let refs = self
            .refs
            .iter()
            .filter_map(OwnedRef::to_reference)
            .collect();
        crate::reference_index::reindex_source(
            &pool,
            crate::reference_index::KIND_PAGE,
            self.page_id,
            refs,
        )
        .await
        .map_err(|e| JobError::Retryable(format!("reference_index: {e}")))
    }
}

/// Rebuild `page_id`'s reference rows from the already-scanned `refs`.
/// Dispatches a retriable [`ReferenceIndexJob`] when a sink is
/// installed; otherwise reindexes **inline** (today's behavior).
/// Best-effort — errors are logged, never propagated, so a transient
/// index hiccup can't block a save.
pub async fn reindex_page(
    pool: &rustango::sql::Pool,
    tenant_slug: &str,
    page_id: i64,
    refs: Vec<crate::reference_index::Reference>,
) {
    let Some(sink) = sink() else {
        reindex_inline(pool, page_id, refs).await;
        return;
    };
    register_pool(tenant_slug.to_owned(), pool.clone());
    let job = ReferenceIndexJob {
        tenant_slug: tenant_slug.to_owned(),
        page_id,
        refs: refs.iter().map(OwnedRef::from_ref).collect(),
    };
    if let Err(e) = sink.dispatch_reference_index(job).await {
        tracing::warn!(
            target: "rustango_cms::task_queue",
            error = %e,
            "reference_index dispatch failed; reindexing inline",
        );
        reindex_inline(pool, page_id, refs).await;
    }
}

async fn reindex_inline(
    pool: &rustango::sql::Pool,
    page_id: i64,
    refs: Vec<crate::reference_index::Reference>,
) {
    if let Err(e) = crate::reference_index::reindex_source(
        pool,
        crate::reference_index::KIND_PAGE,
        page_id,
        refs,
    )
    .await
    {
        tracing::warn!(
            target: "rustango_cms::task_queue",
            page_id,
            error = %e,
            "inline reference reindex failed",
        );
    }
}

// ---- search-index job (#408 × #432) ------------------------------

/// What a [`SearchIndexJob`] does to a page's search document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SearchIndexAction {
    /// Upsert the page into the external search index (it's published).
    Index(crate::search::SearchDoc),
    /// Remove the page from the index (deleted / unpublished / expired).
    Delete(i64),
}

/// Keep an external [`crate::search::SearchBackend`] (Elasticsearch) in
/// sync with a page change (#408). A no-op at run time when no backend
/// is installed — the built-in Postgres FTS path queries live data and
/// needs no index. Retriable: an ES hiccup is retried rather than
/// silently dropping the update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchIndexJob {
    pub tenant_slug: String,
    pub action: SearchIndexAction,
}

#[async_trait]
impl Job for SearchIndexJob {
    const NAME: &'static str = "rcms.search_index";

    async fn run(&self) -> Result<(), JobError> {
        // The backend is a process global; no per-tenant resource
        // registry needed. No backend installed → nothing to index.
        let Some(backend) = crate::search::backend() else {
            return Ok(());
        };
        match &self.action {
            SearchIndexAction::Index(doc) => backend.index(&self.tenant_slug, doc).await,
            SearchIndexAction::Delete(page_id) => backend.delete(&self.tenant_slug, *page_id).await,
        }
        Ok(())
    }
}

/// Keep the external search index in step with a saved page: a page in
/// a public status (published or archived, #556) is indexed, any other
/// status is removed (so draft / scheduled / expired pages drop out of
/// search — archived pages now STAY findable). **No-op when no external
/// [`crate::search::SearchBackend`] is installed** — the Postgres FTS
/// path needs no maintained index. Dispatched via the queue when a sink
/// is set, else run inline. Best-effort.
pub async fn sync_page_search(tenant_slug: &str, page: &crate::page::Page) {
    if crate::search::backend().is_none() {
        return;
    }
    let Some(page_id) = page.id.get().copied() else {
        return;
    };
    let is_public = page.status == crate::page::PageStatus::Published.as_str()
        || page.status == crate::page::PageStatus::Archived.as_str();
    let action = if is_public {
        SearchIndexAction::Index(crate::search::SearchDoc {
            page_id,
            title: page.title.clone(),
            seo_title: page.seo_title.clone(),
            seo_description: page.seo_description.clone(),
            url_path: page.url_path.clone(),
            status: page.status.clone(),
        })
    } else {
        SearchIndexAction::Delete(page_id)
    };
    let job = SearchIndexJob {
        tenant_slug: tenant_slug.to_owned(),
        action,
    };
    match sink() {
        Some(s) => {
            if let Err(e) = s.dispatch_search_index(job.clone()).await {
                tracing::warn!(
                    target: "rustango_cms::task_queue",
                    error = %e,
                    "search_index dispatch failed; running inline",
                );
                job.run().await.log_warn("search index job failed; the index may be stale");
            }
        }
        None => {
            job.run().await.log_warn("search index job failed; the index may be stale");
        }
    }
}

/// Remove a page from the external search index (on hard delete). No-op
/// when no backend is installed. Queue-dispatched or inline.
pub async fn remove_page_search(tenant_slug: &str, page_id: i64) {
    if crate::search::backend().is_none() {
        return;
    }
    let job = SearchIndexJob {
        tenant_slug: tenant_slug.to_owned(),
        action: SearchIndexAction::Delete(page_id),
    };
    match sink() {
        Some(s) => {
            if s.dispatch_search_index(job.clone()).await.is_err() {
                job.run().await.log_warn("search index job failed; the index may be stale");
            }
        }
        None => {
            job.run().await.log_warn("search index job failed; the index may be stale");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts how many `invalidate_url` calls it received.
    struct Counting(Arc<AtomicUsize>);

    #[async_trait]
    impl PageCacheInvalidator for Counting {
        async fn invalidate_url(&self, _tenant_slug: &str, _url_path: &str) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn job_round_trips_and_has_stable_name() {
        let job = CachePurgeJob {
            tenant_slug: "acme".to_owned(),
            urls: vec!["/a".to_owned(), "/b".to_owned()],
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: CachePurgeJob = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tenant_slug, "acme");
        assert_eq!(back.urls, vec!["/a", "/b"]);
        assert_eq!(CachePurgeJob::NAME, "rcms.cache_purge");
    }

    #[tokio::test]
    async fn run_purges_when_invalidator_registered() {
        let n = Arc::new(AtomicUsize::new(0));
        register_invalidator("acme-run", Arc::new(Counting(n.clone())));
        let job = CachePurgeJob {
            tenant_slug: "acme-run".to_owned(),
            urls: vec!["/x".to_owned(), "/y".to_owned(), "/z".to_owned()],
        };
        assert!(job.run().await.is_ok());
        assert_eq!(n.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn run_is_fatal_without_registration() {
        let job = CachePurgeJob {
            tenant_slug: "never-registered-tenant".to_owned(),
            urls: vec!["/x".to_owned()],
        };
        match job.run().await {
            Err(JobError::Fatal(_)) => {}
            other => panic!("expected Fatal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn purge_urls_runs_inline_when_no_sink() {
        // No sink installed in tests → the inline path drives the
        // in-hand invalidator directly.
        let n = Arc::new(AtomicUsize::new(0));
        let inv: Arc<dyn PageCacheInvalidator> = Arc::new(Counting(n.clone()));
        purge_urls(
            inv,
            "acme-inline".to_owned(),
            vec!["/a".to_owned(), "/b".to_owned()],
        )
        .await;
        assert_eq!(n.load(Ordering::SeqCst), 2);
        // Empty url list is a no-op.
        let inv2: Arc<dyn PageCacheInvalidator> = Arc::new(Counting(n.clone()));
        purge_urls(inv2, "acme-inline".to_owned(), vec![]).await;
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn owned_ref_round_trips_known_kind_skips_unknown() {
        use crate::reference_index::{Reference, KIND_MEDIA};
        let r = Reference {
            to_kind: KIND_MEDIA,
            to_id: 7,
            field_path: "body".to_owned(),
        };
        let owned = OwnedRef::from_ref(&r);
        assert_eq!(owned.to_kind, "cms_media");
        let back = owned.to_reference().expect("known kind");
        assert_eq!(back.to_kind, KIND_MEDIA);
        assert_eq!(back.to_id, 7);
        // Unknown kind → dropped on rebuild (forward-compat guard).
        let bogus = OwnedRef {
            to_kind: "cms_widget".to_owned(),
            to_id: 1,
            field_path: "x".to_owned(),
        };
        assert!(bogus.to_reference().is_none());
    }

    #[test]
    fn reference_index_job_round_trips_and_named() {
        let job = ReferenceIndexJob {
            tenant_slug: "acme".to_owned(),
            page_id: 9,
            refs: vec![OwnedRef {
                to_kind: "cms_media".to_owned(),
                to_id: 3,
                field_path: "hero".to_owned(),
            }],
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: ReferenceIndexJob = serde_json::from_str(&json).unwrap();
        assert_eq!(back.page_id, 9);
        assert_eq!(back.refs.len(), 1);
        assert_eq!(ReferenceIndexJob::NAME, "rcms.reference_index");
    }

    #[tokio::test]
    async fn reference_index_job_is_fatal_without_pool() {
        let job = ReferenceIndexJob {
            tenant_slug: "tenant-with-no-registered-pool".to_owned(),
            page_id: 1,
            refs: vec![],
        };
        match job.run().await {
            Err(JobError::Fatal(_)) => {}
            other => panic!("expected Fatal, got {other:?}"),
        }
    }

    #[test]
    fn search_index_job_round_trips_both_actions() {
        let idx = SearchIndexJob {
            tenant_slug: "acme".to_owned(),
            action: SearchIndexAction::Index(crate::search::SearchDoc {
                page_id: 5,
                title: "Hi".to_owned(),
                seo_title: String::new(),
                seo_description: String::new(),
                url_path: "/hi".to_owned(),
                status: "published".to_owned(),
            }),
        };
        let back: SearchIndexJob =
            serde_json::from_str(&serde_json::to_string(&idx).unwrap()).unwrap();
        assert!(matches!(back.action, SearchIndexAction::Index(d) if d.page_id == 5));
        let del = SearchIndexJob {
            tenant_slug: "acme".to_owned(),
            action: SearchIndexAction::Delete(9),
        };
        let back2: SearchIndexJob =
            serde_json::from_str(&serde_json::to_string(&del).unwrap()).unwrap();
        assert!(matches!(back2.action, SearchIndexAction::Delete(9)));
        assert_eq!(SearchIndexJob::NAME, "rcms.search_index");
    }

    #[tokio::test]
    async fn search_index_job_is_noop_without_backend() {
        // No SearchBackend installed (the default) → run() succeeds as a
        // no-op; the Postgres FTS path needs no maintained index.
        let job = SearchIndexJob {
            tenant_slug: "acme".to_owned(),
            action: SearchIndexAction::Delete(1),
        };
        assert!(job.run().await.is_ok());
    }
}
