//! Tree mutation + read API on top of [`Page`].
//!
//! All operations are dialect-agnostic. Read-only queries accept
//! `&rustango::sql::Pool`. Write operations that require atomic
//! transactions (`create_root`, `create_child`, `move_to`,
//! `cascade_url_path_to_descendants`) open a `PoolTx` via
//! `transaction_pool(&tenant.pool())` and use the `_tx` ORM helpers.
//!
//! Path generation is two-step: the materialized-path segment is the
//! row's auto-assigned id, which we don't know until after INSERT. To
//! avoid the brief window where `path = ""`, every create wraps the
//! insert + path update in a transaction.
//!
//! `move_to` also takes a transaction because it rewrites every row
//! in the moved subtree atomically.
//!
//! `created_at` / `updated_at` are framework-stamped (`auto_now_add`
//! / `auto_now`); callers don't fill them.

use chrono::Utc;
use rustango::core::Column as _;
use rustango::extractors::Tenant;
use rustango::sql::{
    sqlx, transaction_pool, Auto, ExecError, FetcherPool as _, FetcherTx as _, Pool, PoolTx,
};

use crate::page::{Page, PageStatus};
use crate::page_type::find_handler;
use crate::page_type_model::PageType;
use crate::tree::{depth_of, descendants_like, MaterializedPath, PathError};

/// Errors from tree operations.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error(transparent)]
    Db(#[from] ExecError),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Query(#[from] rustango::core::QueryError),
    #[error("page id was not assigned after insert")]
    MissingId,
    #[error("page {0} not found")]
    NotFound(i64),
    #[error("cms_page_type row {0} not found — has `ensure_seeded` run for this tenant?")]
    UnknownPageType(i64),
    #[error("cannot move a page beneath itself or its own descendant")]
    CycleAttempt,
    #[error(
        "page-type `{child_type}` is not in `{parent_type}.allowed_child_types` — \
         parent refuses this child"
    )]
    DisallowedChildType {
        parent_type: String,
        child_type: String,
    },
    #[error(
        "page-type `{child_type}.allowed_parent_types` does not include \
         `{parent_type}` — child refuses this parent"
    )]
    DisallowedParentType {
        parent_type: String,
        child_type: String,
    },
}

/// Run `f` inside a framework [`rustango::sql::atomic`] scope while
/// letting it return [`TreeError`] — the bridge that lets the #317
/// `on_commit` cache-invalidation pattern reach the tree-write paths.
///
/// `rustango::sql::atomic` fixes its closure's error to [`ExecError`],
/// but the tree helpers (`cascade_url_path_to_descendants`, `move_to`,
/// …) speak `TreeError`, and there's no `TreeError -> ExecError`
/// conversion (only the reverse, via [`TreeError::Db`]). This adapter
/// closes that gap: a `TreeError` from `f` is stashed and a sentinel
/// `ExecError` is returned to drive `atomic`'s rollback, then the real
/// error is recovered afterwards. Recovery keys off the **stash**, not
/// the sentinel value, so a genuine `ExecError` raised by `BEGIN` /
/// `COMMIT` itself is never misread as a domain error.
///
/// `on_commit` callbacks queued by `f` (e.g. via
/// [`crate::cache_invalidate::invalidate_urls_on_commit`]) fire only on
/// the commit path — a `TreeError` rolls the transaction back and drops
/// them, exactly like a native `atomic` block.
///
/// The cleaner long-term fix is a generic `atomic` in the framework
/// (closure returning `Result<T, E: From<ExecError>>`); until that
/// lands, this adapter keeps the rollout unblocked.
///
/// # Errors
/// The first [`TreeError`] returned by `f`, or a `BEGIN` / `COMMIT`
/// driver error surfaced as [`TreeError::Db`].
pub async fn atomic_tree<T, F>(pool: &Pool, f: F) -> Result<T, TreeError>
where
    T: Send + 'static,
    F: Send
        + 'static
        + for<'tx> FnOnce(
            &'tx mut PoolTx<'_>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<T, TreeError>> + Send + 'tx>,
        >,
{
    let stash: std::sync::Arc<std::sync::Mutex<Option<TreeError>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let stash_cb = std::sync::Arc::clone(&stash);
    let out = rustango::sql::atomic(pool, move |tx| {
        Box::pin(async move {
            let mut guard = tx.lock().await?;
            match f(&mut guard).await {
                Ok(v) => Ok(v),
                Err(e) => {
                    *stash_cb.lock().expect("atomic_tree stash poisoned") = Some(e);
                    // Sentinel: any ExecError drives atomic's rollback;
                    // the value is never read (recovery uses the stash).
                    Err(ExecError::EmptyReturning)
                }
            }
        })
    })
    .await;
    match out {
        Ok(v) => Ok(v),
        Err(commit_err) => {
            let domain = stash.lock().expect("atomic_tree stash poisoned").take();
            Err(domain.unwrap_or(TreeError::Db(commit_err)))
        }
    }
}

/// Builder for the user-supplied fields when creating a page. Tree
/// fields (path, depth, parent_id, sort_order) and timestamps are
/// computed by the create_* helpers — callers don't fill them.
#[derive(Debug, Clone)]
pub struct NewPage {
    pub page_type_id: i64,
    pub title: String,
    pub slug: String,
    pub status: PageStatus,
    pub seo_title: String,
    pub seo_description: String,
}

impl NewPage {
    pub fn new(page_type_id: i64, title: impl Into<String>, slug: impl Into<String>) -> Self {
        Self {
            page_type_id,
            title: title.into(),
            slug: slug.into(),
            status: PageStatus::Draft,
            seo_title: String::new(),
            seo_description: String::new(),
        }
    }

    pub fn with_status(mut self, status: PageStatus) -> Self {
        self.status = status;
        self
    }

    pub fn with_seo(mut self, title: impl Into<String>, description: impl Into<String>) -> Self {
        self.seo_title = title.into();
        self.seo_description = description.into();
        self
    }

    fn into_page(self, parent_id: Option<i64>, sort_order: i32) -> Page {
        let published_at = matches!(self.status, PageStatus::Published).then(Utc::now);
        // #251 — first_published_at + last_published_at mirror on
        // first publish (same instant). Subsequent edits bump only
        // `last_published_at`; `published_at` stays as the
        // chronological-ordering anchor.
        let last_published_at = published_at;
        Page {
            id: Auto::Unset,
            page_type_id: self.page_type_id,
            title: self.title,
            slug: self.slug,
            path: String::new(),
            url_path: String::new(),
            // Empty means "follow url_path" — a new page inherits the
            // frontend's default routing until someone says otherwise.
            preview_path: String::new(),
            // Empty means "use the page type's template" — a new page
            // looks like its type until someone overrides it.
            template_override: String::new(),
            depth: 0,
            parent_id,
            locale_variant_of: None,
            alias_of: None,
            theme_id: None,
            sort_order,
            status: self.status.as_str().to_owned(),
            published_at,
            last_published_at,
            go_live_at: None,
            expire_at: None,
            seo_title: self.seo_title,
            seo_description: self.seo_description,
            robots_index: true,
            sitemap_priority: 0.5,
            show_in_menus: false,
            og_title: String::new(),
            og_description: String::new(),
            og_image_media_id: None,
            twitter_card: "summary_large_image".to_owned(),
            notification_pre_published_sent: false,
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        }
    }
}

/// Compute the public URL path a page is reachable at,
/// given its parent's `url_path` (or `None` for root) and its own
/// slug. Maintained on every create / slug-change / move.
///
/// Invariants:
/// - Always starts with `/`.
/// - No trailing slash, except for the empty-slug root which is `/`.
/// - Empty slug under a non-root parent is treated as "use parent's
///   path" (rare but useful for landing pages that share a parent's
///   URL). Use sparingly — the resolver picks the highest-priority
///   match among equal URLs.
pub fn compute_url_path(parent_url_path: Option<&str>, slug: &str) -> String {
    match parent_url_path {
        None => {
            // Root page.
            if slug.is_empty() {
                "/".to_owned()
            } else {
                format!("/{slug}")
            }
        }
        Some(parent) => {
            if slug.is_empty() {
                parent.to_owned()
            } else if parent == "/" {
                format!("/{slug}")
            } else {
                format!("{parent}/{slug}")
            }
        }
    }
}

/// Recompute and persist `url_path` for every
/// descendant of `root_page` after its own `url_path` changes.
///
/// Walks in path order so each descendant's parent has been updated
/// before the descendant itself is processed. Caches each updated
/// `(id → url_path)` in-memory to avoid re-fetching parents. Runs
/// inside `conn` so callers can wrap the whole self-edit + cascade
/// in one transaction.
///
/// # Errors
/// Driver / query failures or ORM save errors.
pub async fn cascade_url_path_to_descendants(
    tx: &mut PoolTx<'_>,
    root_page: &Page,
) -> Result<usize, TreeError> {
    let root_id = page_id(root_page)?;
    let pattern = descendants_like(&root_page.path);
    let descendants: Vec<Page> = Page::objects()
        .where_(Page::path.like(pattern))
        .where_(Page::path.ne(root_page.path.clone()))
        .order_by(&[("path", false)])
        .fetch_tx(tx)
        .await?;

    let mut by_id: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    by_id.insert(root_id, root_page.url_path.clone());

    let mut updated = 0usize;
    for mut d in descendants {
        let Some(parent_id) = d.parent_id else {
            continue;
        };
        let parent_url_path = by_id.get(&parent_id).cloned().unwrap_or_default();
        let new_url = compute_url_path(Some(parent_url_path.as_str()), &d.slug);
        if d.url_path == new_url {
            let did = d.id.get().copied().unwrap_or_default();
            by_id.insert(did, new_url);
            continue;
        }
        d.url_path = new_url.clone();
        let did = d.id.get().copied().unwrap_or_default();
        by_id.insert(did, new_url);
        d.save_tx(tx).await?;
        updated += 1;
    }
    Ok(updated)
}

/// Recompute `url_path` for every page in the tenant
/// from scratch. Walks all pages in `path` order so parents update
/// before children. Useful as a one-shot recovery / backfill verb
/// when external SQL has touched `cms_page` outside the admin.
///
/// # Errors
/// Driver / query failures or ORM save errors.
pub async fn rebuild_all_url_paths(pool: &Pool) -> Result<usize, TreeError> {
    // #317 — stream pages in `path` order (a materialized path sorts
    // parents before their children) in chunks rather than materializing
    // every `Page` row up front: a 10k+ page tenant would otherwise
    // buffer the entire tree of full Page structs in memory at once.
    // Each page is only needed transiently to recompute its `url_path`;
    // the sole cross-row state is the compact id→url_path map for parent
    // lookups, so chunked iteration keeps peak memory bounded to one
    // chunk + that map. Recomputing `url_path` never mutates `path` (the
    // ORDER BY / cursor key), so the LIMIT/OFFSET window stays stable
    // across chunks — no row is skipped or visited twice.
    let mut iter = Page::objects()
        .order_by(&[("path", false)])
        .iterator(2_000)?;

    let mut by_id: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    let mut updated = 0usize;
    while let Some(mut p) = iter.next_row(pool).await? {
        let id = page_id(&p)?;
        let new_url = match p.parent_id {
            None => compute_url_path(None, &p.slug),
            Some(parent_id) => {
                let parent_url = by_id.get(&parent_id).cloned().unwrap_or_default();
                compute_url_path(Some(parent_url.as_str()), &p.slug)
            }
        };
        if p.url_path != new_url {
            p.url_path = new_url.clone();
            p.save_pool(pool).await?;
            updated += 1;
        }
        by_id.insert(id, new_url);
    }
    Ok(updated)
}

impl Page {
    /// Insert a new root page. Returns the persisted row with `id`,
    /// `path`, `depth`, and `sort_order` populated.
    pub async fn create_root(tenant: &Tenant, new: NewPage) -> Result<Self, TreeError> {
        Self::create_root_pool(tenant.pool(), new).await
    }

    /// Backend-agnostic [`Self::create_root`] — takes a `&Pool` directly,
    /// so content can be seeded / scripted outside an HTTP request on any
    /// dialect. `create_root` needs a `&Tenant`, but `Tenant` is only
    /// publicly constructible on Postgres (test-gated), leaving no way to
    /// seed pages on SQLite/MySQL; this is that way. #450
    pub async fn create_root_pool(pool: &Pool, new: NewPage) -> Result<Self, TreeError> {
        let mut tx = transaction_pool(pool).await?;
        // #317 — compute sort_order inside the tx under a FOR UPDATE lock
        // on the existing roots so concurrent root inserts can't read the
        // same max and collide.
        let sort_order = next_root_sort_order_tx(&mut tx).await?;
        let mut page = new.into_page(None, sort_order);

        page.insert_tx(&mut tx).await?;
        let id = page_id(&page)?;

        let path = MaterializedPath::root(id)?;
        page.path = path.into_string();
        page.depth = 1;
        page.url_path = compute_url_path(None, &page.slug);
        page.save_tx(&mut tx).await?;
        tx.commit().await?;

        Ok(page)
    }

    /// Append a new child under `parent`. Sort order is the next
    /// integer after the parent's existing children.
    ///
    /// Validates the type constraint in both directions before
    /// inserting: the parent's `allowed_child_types` must permit
    /// the child's type, and the child's `allowed_parent_types`
    /// must permit the parent's type. Either failure short-circuits
    /// the write.
    pub async fn create_child(
        tenant: &Tenant,
        parent: &Page,
        new: NewPage,
    ) -> Result<Self, TreeError> {
        Self::create_child_pool(tenant.pool(), parent, new).await
    }

    /// Backend-agnostic [`Self::create_child`] — takes a `&Pool` directly
    /// so content can be seeded / scripted outside an HTTP request on any
    /// dialect (see [`Self::create_root_pool`]). #450
    pub async fn create_child_pool(
        pool: &Pool,
        parent: &Page,
        new: NewPage,
    ) -> Result<Self, TreeError> {
        let parent_id = page_id(parent)?;
        validate_child_placement(pool, parent.page_type_id, new.page_type_id).await?;

        let mut tx = transaction_pool(pool).await?;
        // #317 — serialize concurrent inserts under this parent. Lock the
        // parent row (FOR UPDATE) so a second create_child blocks until
        // the first commits; otherwise both read the same
        // MAX(sort_order) and write duplicate sort_orders. Locking the
        // parent (not the sibling set) covers the first child too, when
        // there are no siblings to lock. The lock is held for the whole
        // tx — dropping the result Vec doesn't release it. No-op on
        // SQLite (no row locks; writers serialize anyway).
        let _locked_parent: Vec<Page> = Page::objects()
            .where_(Page::id.eq(parent_id))
            .select_for_update()
            .fetch_tx(&mut tx)
            .await?;
        let sort_order = next_child_sort_order_tx(&mut tx, parent_id).await?;
        let mut page = new.into_page(Some(parent_id), sort_order);

        page.insert_tx(&mut tx).await?;
        let id = page_id(&page)?;

        let path = MaterializedPath::child_of(&parent.path, id)?;
        page.depth = path.depth();
        page.path = path.into_string();
        page.url_path = compute_url_path(Some(parent.url_path.as_str()), &page.slug);
        page.save_tx(&mut tx).await?;
        tx.commit().await?;

        Ok(page)
    }

    /// Immediate children, ordered by `sort_order`.
    pub async fn children(&self, pool: &Pool) -> Result<Vec<Page>, TreeError> {
        let id = page_id(self)?;
        let rows = Page::objects()
            .where_(Page::parent_id.eq(id))
            .order_by(&[("sort_order", false), ("id", false)])
            .fetch(pool)
            .await?;
        Ok(rows)
    }

    /// Pages that share this page's parent (excluding self), ordered
    /// by `sort_order`. Roots return all other roots.
    pub async fn siblings(&self, pool: &Pool) -> Result<Vec<Page>, TreeError> {
        let id = page_id(self)?;
        let qs = Page::objects().order_by(&[("sort_order", false), ("id", false)]);
        let rows = match self.parent_id {
            Some(pid) => qs.where_(Page::parent_id.eq(pid)).fetch(pool).await?,
            None => qs.where_(Page::parent_id.is_null()).fetch(pool).await?,
        };
        Ok(rows
            .into_iter()
            .filter(|p| p.id.get().copied() != Some(id))
            .collect())
    }

    /// All ancestors of this page from root → immediate parent.
    /// Empty for roots. Order is root-first.
    pub async fn ancestors(&self, pool: &Pool) -> Result<Vec<Page>, TreeError> {
        let paths = MaterializedPath::ancestors_of(&self.path);
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut rows = Page::objects()
            .where_(Page::path.is_in(paths.iter().cloned()))
            .order_by(&[("path", false)])
            .fetch(pool)
            .await?;
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(rows)
    }

    /// All descendants (excluding self), ordered by `path` (which is
    /// also tree-pre-order).
    pub async fn descendants(&self, pool: &Pool) -> Result<Vec<Page>, TreeError> {
        let pattern = descendants_like(&self.path);
        let rows = Page::objects()
            .where_(Page::path.like(pattern))
            .where_(Page::path.ne(self.path.clone()))
            .order_by(&[("path", false)])
            .fetch(pool)
            .await?;
        Ok(rows)
    }

    /// Move this page (and its entire subtree) under `new_parent`. Pass
    /// `None` to promote it to a root. The subtree's `url_path`s follow in
    /// the same transaction, so the tree and the URLs never disagree.
    ///
    /// Refuses cycles (moving under self or a descendant) and
    /// type-constraint violations (re-checks both directions against
    /// the new parent before writing).
    pub async fn move_to(
        &mut self,
        tenant: &Tenant,
        new_parent: Option<&Page>,
    ) -> Result<(), TreeError> {
        let self_id = page_id(self)?;

        if let Some(np) = new_parent {
            let np_id = page_id(np)?;
            if np_id == self_id || np.path.starts_with(&self.path) {
                return Err(TreeError::CycleAttempt);
            }
            validate_child_placement(tenant.pool(), np.page_type_id, self.page_type_id).await?;
        }

        let new_id_path = match new_parent {
            Some(np) => MaterializedPath::child_of(&np.path, self_id)?.into_string(),
            None => MaterializedPath::root(self_id)?.into_string(),
        };
        if new_id_path == self.path {
            return Ok(());
        }

        let subtree = self.descendants(tenant.pool()).await?;
        let mut tx = transaction_pool(tenant.pool()).await?;
        self.move_to_in_tx(new_parent, subtree, &mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Transaction-scoped core of [`move_to`]. Assumes cycle / placement
    /// validation has already run and `subtree` (this page's descendants)
    /// was read on the pool **before** the transaction opened — so this
    /// makes **no pool reads**. That matters: reading the pool while
    /// holding a tx connection can deadlock a single-connection SQLite
    /// pool. Pre-reading lets a batch (bulk move) reparent many pages in
    /// one transaction. #317.
    ///
    /// # Errors
    /// Driver errors from the lock / saves, or [`TreeError`] from path
    /// math.
    pub async fn move_to_in_tx(
        &mut self,
        new_parent: Option<&Page>,
        subtree: Vec<Page>,
        tx: &mut PoolTx<'_>,
    ) -> Result<(), TreeError> {
        let self_id = page_id(self)?;
        let new_id_path = match new_parent {
            Some(np) => MaterializedPath::child_of(&np.path, self_id)?.into_string(),
            None => MaterializedPath::root(self_id)?.into_string(),
        };
        if new_id_path == self.path {
            return Ok(());
        }
        let old_prefix = self.path.clone();

        // #317 — compute the destination sort_order inside the tx, under a
        // FOR UPDATE lock on the new parent row (or the roots set when
        // promoting to a root), so a concurrent insert/move into the same
        // parent can't hand out a duplicate sort_order.
        let new_sort_order = match new_parent {
            Some(np) => {
                let np_id = page_id(np)?;
                let _locked_parent: Vec<Page> = Page::objects()
                    .where_(Page::id.eq(np_id))
                    .select_for_update()
                    .fetch_tx(tx)
                    .await?;
                next_child_sort_order_tx(tx, np_id).await?
            }
            None => next_root_sort_order_tx(tx).await?,
        };

        self.parent_id = new_parent.and_then(|p| p.id.get().copied());
        self.sort_order = new_sort_order;
        self.depth = depth_of(&new_id_path);
        self.path = new_id_path.clone();
        // #706 — the URL moves with the tree, in this transaction. It used
        // to be left to the admin handler, in a second transaction a
        // library caller never ran, so a moved subtree kept its old URLs.
        self.url_path = compute_url_path(new_parent.map(|p| p.url_path.as_str()), &self.slug);
        self.save_tx(tx).await?;

        // `subtree` is in path order, so a parent's new URL is known
        // before its children's.
        let mut url_of: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
        url_of.insert(self_id, self.url_path.clone());
        for mut row in subtree {
            let suffix = row
                .path
                .strip_prefix(&old_prefix)
                .expect("descendant path must start with old prefix");
            row.path = format!("{new_id_path}{suffix}");
            row.depth = depth_of(&row.path);
            let parent_url = row.parent_id.and_then(|pid| url_of.get(&pid).cloned());
            row.url_path = compute_url_path(parent_url.as_deref(), &row.slug);
            if let Some(id) = row.id.get().copied() {
                url_of.insert(id, row.url_path.clone());
            }
            row.save_tx(tx).await?;
        }
        Ok(())
    }
}

// ---------- helpers ----------

fn page_id(page: &Page) -> Result<i64, TreeError> {
    page.id.get().copied().ok_or(TreeError::MissingId)
}

/// Next `sort_order` for a new root, read inside `tx` after taking a
/// `FOR UPDATE` lock on **one** deterministic root row (the lowest id) so
/// concurrent root inserts serialize (#317).
///
/// Roots have no parent row to lock, but we must NOT lock the whole roots
/// set: concurrent transactions acquire the multiple row locks in
/// conflicting orders and PG aborts one with a deadlock (40P01). Locking a
/// single, always-the-same row (lowest id) can't deadlock — every tx
/// contends on that one row — mirroring `create_child`'s single-parent
/// lock. The subsequent max read is serialized by that lock.
///
/// Empty roots → nothing to lock → a narrow gap when the first couple of
/// roots are created concurrently (rare; acceptable). `FOR UPDATE` is a
/// no-op on SQLite, which serializes writers via its implicit tx lock.
async fn next_root_sort_order_tx(tx: &mut PoolTx<'_>) -> Result<i32, TreeError> {
    let _lock: Vec<Page> = Page::objects()
        .where_(Page::parent_id.is_null())
        .order_by(&[("id", false)])
        .limit(1)
        .select_for_update()
        .fetch_tx(tx)
        .await?;
    let rows = Page::objects()
        .where_(Page::parent_id.is_null())
        .order_by(&[("sort_order", true)])
        .fetch_tx(tx)
        .await?;
    Ok(rows.first().map(|p| p.sort_order + 1).unwrap_or(0))
}

/// Next `sort_order` for a new child of `parent_id`, read inside `tx`.
/// The caller locks the parent row with `FOR UPDATE` first (see
/// [`Page::create_child`]), which serializes every insert under that
/// parent — including the first child, when no siblings exist to lock —
/// so this is a plain read of the current max.
async fn next_child_sort_order_tx(tx: &mut PoolTx<'_>, parent_id: i64) -> Result<i32, TreeError> {
    let rows = Page::objects()
        .where_(Page::parent_id.eq(parent_id))
        .order_by(&[("sort_order", true)])
        .fetch_tx(tx)
        .await?;
    Ok(rows.first().map(|p| p.sort_order + 1).unwrap_or(0))
}

/// Check that the (parent_type, child_type) pairing is permitted by
/// both handlers' whitelists. Skipped when either type's whitelist is
/// empty (the convention for "no restriction"). Public to make it
/// usable from admin form hooks and other write paths.
///
/// Fetches both `cms_page_type` rows in a single `WHERE id IN (..)`
/// query, then dispatches to the in-process handler registry for the
/// declared whitelists.
///
/// # Errors
/// * [`TreeError::UnknownPageType`] when either id doesn't resolve to
///   a `cms_page_type` row.
/// * [`TreeError::DisallowedChildType`] when the parent rejects.
/// * [`TreeError::DisallowedParentType`] when the child rejects.
pub async fn validate_child_placement(
    pool: &Pool,
    parent_type_id: i64,
    child_type_id: i64,
) -> Result<(), TreeError> {
    let rows: Vec<PageType> = PageType::objects()
        .where_(PageType::id.is_in([parent_type_id, child_type_id]))
        .fetch(pool)
        .await?;

    let parent = rows
        .iter()
        .find(|r| r.id.get().copied() == Some(parent_type_id))
        .ok_or(TreeError::UnknownPageType(parent_type_id))?;
    let child = rows
        .iter()
        .find(|r| r.id.get().copied() == Some(child_type_id))
        .ok_or(TreeError::UnknownPageType(child_type_id))?;

    // #448 — a leaf type (code only) accepts no children at all.
    if find_handler(&parent.type_name).is_some_and(|h| h.is_leaf()) {
        return Err(TreeError::DisallowedChildType {
            parent_type: parent.type_name.clone(),
            child_type: child.type_name.clone(),
        });
    }
    // The rows hold both lists for code and admin-made types alike (#843).
    let allowed = parent.allowed_children();
    if !allowed.is_empty() && !allowed.iter().any(|t| *t == child.type_name) {
        return Err(TreeError::DisallowedChildType {
            parent_type: parent.type_name.clone(),
            child_type: child.type_name.clone(),
        });
    }
    let allowed = child.allowed_parents();
    if !allowed.is_empty() && !allowed.iter().any(|t| *t == parent.type_name) {
        return Err(TreeError::DisallowedParentType {
            parent_type: parent.type_name.clone(),
            child_type: child.type_name.clone(),
        });
    }

    Ok(())
}

// MaterializedPath::ancestors() returns Vec<String>; we want a static-style
// helper available without an instance for the `ancestors` query above.
impl MaterializedPath {
    pub fn ancestors_of(path: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut acc = String::new();
        for seg in path.split(crate::tree::SEPARATOR).filter(|s| !s.is_empty()) {
            acc.push_str(seg);
            acc.push(crate::tree::SEPARATOR);
            if acc != path {
                out.push(acc.clone());
            }
        }
        if let Some(last) = out.last() {
            if last == path {
                out.pop();
            }
        }
        out
    }
}

// `atomic_tree`'s commit/rollback behavior is exercised against a
// serverless SQLite-in-memory pool, so it's gated on the `sqlite`
// feature:
//   cargo test --lib --no-default-features --features sqlite atomic_tree
#[cfg(all(test, feature = "sqlite"))]
mod atomic_tree_tests {
    use super::*;
    use rustango::sql::Pool;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    async fn mem_pool() -> Pool {
        Pool::connect("sqlite::memory:")
            .await
            .expect("sqlite in-memory pool")
    }

    #[tokio::test]
    async fn commit_returns_value_and_fires_on_commit() {
        let pool = mem_pool().await;
        let fired = Arc::new(AtomicBool::new(false));
        let f = fired.clone();
        let out = atomic_tree(&pool, move |_tx| {
            Box::pin(async move {
                rustango::sql::on_commit(move || f.store(true, Ordering::SeqCst));
                Ok::<i32, TreeError>(7)
            })
        })
        .await;
        assert_eq!(out.expect("commit"), 7);
        // on_commit callbacks drain synchronously after COMMIT, before
        // `atomic` returns — so the flag is already set here.
        assert!(
            fired.load(Ordering::SeqCst),
            "on_commit must fire on commit"
        );
    }

    #[tokio::test]
    async fn tree_error_propagates_unchanged_and_drops_on_commit() {
        let pool = mem_pool().await;
        let fired = Arc::new(AtomicBool::new(false));
        let f = fired.clone();
        let out: Result<(), TreeError> = atomic_tree(&pool, move |_tx| {
            Box::pin(async move {
                rustango::sql::on_commit(move || f.store(true, Ordering::SeqCst));
                Err(TreeError::CycleAttempt)
            })
        })
        .await;
        // The original `TreeError` is recovered from the stash — not the
        // `ExecError` sentinel (which would surface as `TreeError::Db`).
        assert!(matches!(out, Err(TreeError::CycleAttempt)), "got {out:?}");
        assert!(
            !fired.load(Ordering::SeqCst),
            "on_commit must be dropped on a TreeError rollback"
        );
    }
}
