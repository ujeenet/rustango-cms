//! Targeted page-cache invalidation hook (#78).
//!
//! The public router (see [`crate::router`]) layers
//! [`rustango::cache_page::CachePageLayer`] in front of CMS page
//! responses; that layer is TTL-based and emits the response under
//! a key derived from `(method, path, host, vary-on)`. After an
//! admin save the public response is stale until the TTL elapses.
//!
//! This module defines a [`PageCacheInvalidator`] trait that the
//! host app can register on [`crate::admin::AdminState`]. When
//! present, every admin handler that mutates a page URL fires the
//! invalidator on the affected URLs. Hosts that don't register one
//! keep the old TTL-only behavior — the trait is purely opt-in.
//!
//! Two built-in invalidators ship:
//!
//! - [`BoxedCacheInvalidator`] — calls [`rustango::cache::BoxedCache::delete`]
//!   on the same key shape `CachePageLayer` writes. Use when the
//!   admin handler holds an `Arc<BoxedCache>` to the same cache the
//!   public layer is using.
//! - [`Noop`] — does nothing. Returned from `default()` so the
//!   trait object can be unconditionally invoked from handler code
//!   without `Option` plumbing.
//!
//! ## Key shape
//!
//! Mirrors the private `compute_cache_key` in `rustango::cache_page`:
//!
//! ```text
//! <prefix>|<method-len>:<method>|<path-len>:<path>|<query-len>:<query>|<host-len>:<host>|...vary_on...
//! ```
//!
//! For v0 we only target the unadorned GET path: query = "", no
//! `vary_on` headers. Hosts that configure their public layer with
//! `vary_on(...)` would need to invalidate every header
//! combination — declare those upfront via
//! [`BoxedCacheInvalidator::with_vary_values`].

use std::sync::Arc;

use async_trait::async_trait;
use rustango::cache::BoxedCache;

/// Hook called by admin handlers after a page-content mutation.
/// Implementations should fan out to whatever cache backend the
/// public router uses (in-memory `CachePageLayer`, Cloudflare,
/// Varnish, …) and drop the keys identified by the URL list.
///
/// Failures are intentionally swallowed by the handler (cache
/// invalidation is best-effort; a stale render is worse than a
/// failed save). Implementations should log internally.
#[async_trait]
pub trait PageCacheInvalidator: Send + Sync + 'static {
    /// Drop the cache entry for the public render of `url_path` on
    /// the given tenant. Hosts pass the unprefixed page URL —
    /// the implementation appends the configured URL prefix +
    /// host(s) before computing the cache key.
    async fn invalidate_url(&self, tenant_slug: &str, url_path: &str);

    /// Drop every cache entry whose URL starts with `url_path` /
    /// (the page itself + every descendant). Called after a slug
    /// rename, a move, or a subtree delete.
    async fn invalidate_subtree(&self, tenant_slug: &str, url_path: &str) {
        // Default: fan out by enumerating the explicit URL list at
        // the call site. Implementations that can do efficient
        // prefix purges (Cloudflare wildcard purge, Redis SCAN +
        // DEL on the key pattern) should override.
        self.invalidate_url(tenant_slug, url_path).await;
    }

    /// Drop the cache entries for many page URLs at once (#428). The
    /// default loops over [`Self::invalidate_url`] — preserving the
    /// per-URL behavior — but backends with a bulk-purge API
    /// (Cloudflare `files`, Azure CDN `contentPaths`) should override
    /// this to issue a single request instead of N. The purge fan-out
    /// in [`crate::task_queue::purge_urls`] calls this so a batch
    /// backend collapses a multi-URL save into one round-trip.
    async fn invalidate_urls(&self, tenant_slug: &str, url_paths: &[String]) {
        for url in url_paths {
            self.invalidate_url(tenant_slug, url).await;
        }
    }
}

/// Supplies the OAuth2 bearer token for the cloud-CDN backends that
/// authenticate against a Google / Azure management API (#428).
///
/// Both APIs take a short-lived (~1h) access token. Production
/// deployments should implement this against their environment's
/// token endpoint — the GCE/GKE metadata server, a workload-identity
/// exchange, or a refreshing `gcloud auth` / `az account` helper — so
/// the token is renewed before it expires. `None` means "no token
/// available right now"; the purge is then skipped (best-effort, per
/// the [`PageCacheInvalidator`] contract).
#[async_trait]
pub trait AccessTokenSource: Send + Sync + 'static {
    /// Return a fresh bearer token, or `None` to skip the purge.
    async fn token(&self) -> Option<String>;
}

/// Fixed bearer token. Fine for one-shot scripts, short-lived jobs,
/// and tests; **not** for a long-running server, where the token will
/// expire (~1h) — implement [`AccessTokenSource`] against your token
/// endpoint instead so it refreshes.
#[derive(Debug, Clone)]
pub struct StaticToken(pub String);

#[async_trait]
impl AccessTokenSource for StaticToken {
    async fn token(&self) -> Option<String> {
        Some(self.0.clone())
    }
}

/// No-op invalidator returned from [`Arc<dyn PageCacheInvalidator>::default`].
/// Lets handler code call `invalidator.invalidate_url(...).await`
/// without `if let Some(...) = ...` plumbing when the host hasn't
/// registered a real one.
#[derive(Default, Debug, Clone, Copy)]
pub struct Noop;

#[async_trait]
impl PageCacheInvalidator for Noop {
    async fn invalidate_url(&self, _tenant_slug: &str, _url_path: &str) {}
}

/// Convenience constructor — returns an `Arc<dyn PageCacheInvalidator>`
/// pointing at [`Noop`].
#[must_use]
pub fn noop() -> Arc<dyn PageCacheInvalidator> {
    Arc::new(Noop)
}

/// Register an after-commit purge of `url_paths` for the enclosing
/// [`rustango::sql::atomic`] block (#317). The purge fires **only if
/// the transaction commits** — on rollback the queued callback is
/// dropped and nothing is evicted, so a failed save can never knock a
/// still-valid entry out of the public cache.
///
/// `on_commit` callbacks are synchronous; the (async) purge is
/// therefore driven on a detached [`tokio::spawn`] task, so the admin
/// response isn't blocked on the purge round-trips.
///
/// #432 — inside that task the purge goes through
/// [`crate::task_queue::purge_urls`]: when a background queue is
/// configured it dispatches a **retriable** `CachePurgeJob`; otherwise
/// it purges **inline** in list order (the original behavior).
///
/// No-op (registers nothing) when `url_paths` is empty.
///
/// # Panics
/// Panics if called outside an `atomic` scope — the callback would
/// never fire. See [`rustango::sql::on_commit`].
pub fn invalidate_urls_on_commit(
    invalidator: Arc<dyn PageCacheInvalidator>,
    tenant_slug: String,
    url_paths: Vec<String>,
) {
    if url_paths.is_empty() {
        return;
    }
    rustango::sql::on_commit(move || {
        rustango::__private_runtime::tokio::spawn(async move {
            crate::task_queue::purge_urls(invalidator, tenant_slug, url_paths).await;
        });
    });
}

/// Built-in invalidator that drops keys directly from a
/// [`BoxedCache`]. Use when the host wires the same cache instance
/// into both [`CachePageLayer`] and the admin state. The key shape
/// mirrors what [`rustango::cache_page`] writes.
pub struct BoxedCacheInvalidator {
    cache: BoxedCache,
    /// Same value passed to `CachePageLayer::key_prefix(...)`.
    key_prefix: String,
    /// Tenant-slug → public Host header values that should be
    /// invalidated. Most apps map each tenant to one host; some
    /// (apex + subdomain) supply multiple. If a tenant slug isn't
    /// in the map, the invalidator falls back to `default_hosts`.
    hosts_by_tenant: std::collections::HashMap<String, Vec<String>>,
    /// Fallback host list (used when a tenant isn't named in the
    /// map). Useful for single-tenant deployments where every URL
    /// lives under the same Host.
    default_hosts: Vec<String>,
    /// Vary-on header values to invalidate per combo. Empty means
    /// the public layer didn't configure `vary_on(...)`.
    vary_values: Vec<(String, String)>,
}

impl BoxedCacheInvalidator {
    /// Build a new invalidator targeting `cache` with the public
    /// router's `key_prefix`.
    #[must_use]
    pub fn new(cache: BoxedCache, key_prefix: impl Into<String>) -> Self {
        Self {
            cache,
            key_prefix: key_prefix.into(),
            hosts_by_tenant: std::collections::HashMap::new(),
            default_hosts: Vec::new(),
            vary_values: Vec::new(),
        }
    }

    /// Map a tenant slug to one or more Host header values. Both
    /// `acme.localhost` and `acme.localhost:8090` should be added
    /// when the same site is reachable via both.
    #[must_use]
    pub fn with_tenant_hosts(
        mut self,
        tenant_slug: impl Into<String>,
        hosts: impl IntoIterator<Item = String>,
    ) -> Self {
        self.hosts_by_tenant
            .insert(tenant_slug.into(), hosts.into_iter().collect());
        self
    }

    /// Fallback host list — used when a tenant slug isn't in the
    /// per-tenant map. Set this in single-tenant deployments where
    /// every CMS URL lives under the same Host.
    #[must_use]
    pub fn with_default_hosts(mut self, hosts: impl IntoIterator<Item = String>) -> Self {
        self.default_hosts = hosts.into_iter().collect();
        self
    }

    /// Mirror the public layer's `vary_on(...)` declarations. The
    /// invalidator will purge the cross-product of (vary-name,
    /// vary-value) for each URL.
    #[must_use]
    pub fn with_vary_values(mut self, pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        self.vary_values = pairs.into_iter().collect();
        self
    }

    fn hosts_for(&self, tenant_slug: &str) -> &[String] {
        self.hosts_by_tenant
            .get(tenant_slug)
            .map(Vec::as_slice)
            .unwrap_or(&self.default_hosts)
    }

    fn compute_key(&self, path: &str, host: &str) -> String {
        use std::fmt::Write as _;
        let mut k = String::with_capacity(self.key_prefix.len() + 128);
        let _ = write!(&mut k, "{}|", self.key_prefix);
        write_lp(&mut k, "GET");
        write_lp(&mut k, path);
        write_lp(&mut k, "");
        write_lp(&mut k, host);
        for (name, value) in &self.vary_values {
            write_lp(&mut k, name);
            write_lp(&mut k, value);
        }
        k
    }
}

fn write_lp(buf: &mut String, s: &str) {
    use std::fmt::Write as _;
    let _ = write!(buf, "{}:{}|", s.len(), s);
}

#[cfg(test)]
mod key_shape_tests {
    use super::*;
    use rustango::cache::NullCache;

    fn fake_invalidator() -> BoxedCacheInvalidator {
        BoxedCacheInvalidator::new(Arc::new(NullCache), "rcms:page")
    }

    /// Locks in the key shape so a framework-side change to
    /// `compute_cache_key` becomes a hard test failure here.
    /// The format mirrors `rustango::cache_page::compute_cache_key`:
    ///   `<prefix>|<method-len>:<method>|<path-len>:<path>|<query-len>:<query>|<host-len>:<host>|`
    /// followed by any `vary_on` pairs.
    #[test]
    fn key_shape_matches_cache_page_layer() {
        let inv = fake_invalidator();
        let key = inv.compute_key("/about", "acme.localhost:8090");
        assert_eq!(key, "rcms:page|3:GET|6:/about|0:|19:acme.localhost:8090|");
    }

    #[test]
    fn key_with_vary_values_appends_pairs() {
        let inv = fake_invalidator()
            .with_vary_values(vec![("accept-language".to_owned(), "en".to_owned())]);
        let key = inv.compute_key("/about", "acme.localhost");
        assert_eq!(
            key,
            "rcms:page|3:GET|6:/about|0:|14:acme.localhost|15:accept-language|2:en|"
        );
    }
}

#[async_trait]
impl PageCacheInvalidator for BoxedCacheInvalidator {
    async fn invalidate_url(&self, tenant_slug: &str, url_path: &str) {
        let hosts = self.hosts_for(tenant_slug);
        if hosts.is_empty() {
            tracing::debug!(
                target: "rustango_cms::cache_invalidate",
                tenant = %tenant_slug,
                "no hosts configured — skipping cache invalidation"
            );
            return;
        }
        for host in hosts {
            let key = self.compute_key(url_path, host);
            match self.cache.delete(&key).await {
                Ok(()) => tracing::debug!(
                    target: "rustango_cms::cache_invalidate",
                    %key,
                    "purged"
                ),
                Err(e) => tracing::warn!(
                    target: "rustango_cms::cache_invalidate",
                    %key,
                    error = %e,
                    "cache delete failed (continuing — TTL will catch up)"
                ),
            }
        }
    }
}

#[cfg(test)]
mod on_commit_tests {
    use super::*;

    #[test]
    fn empty_url_list_is_a_noop_outside_atomic() {
        // No URLs → returns before touching `on_commit`, so it must not
        // panic even though we're not inside an `atomic` scope.
        invalidate_urls_on_commit(noop(), "acme".to_owned(), vec![]);
    }

    #[test]
    #[should_panic(expected = "on_commit")]
    fn non_empty_outside_atomic_panics() {
        // A non-empty list registers an `on_commit` callback, which the
        // framework rejects outside an `atomic` scope — proof the helper
        // is wired to the commit hook (and won't silently drop a purge).
        invalidate_urls_on_commit(noop(), "acme".to_owned(), vec!["/about".to_owned()]);
    }
}

// Behavioral commit/rollback coverage runs against a serverless
// SQLite-in-memory pool, so it's gated on the `sqlite` feature:
//   cargo test --lib --no-default-features --features sqlite on_commit_db
#[cfg(all(test, feature = "sqlite"))]
mod on_commit_db_tests {
    use super::*;
    use rustango::sql::Pool;
    use std::sync::Mutex;

    /// Records every purged URL so the test can assert the after-commit
    /// fan-out (and its absence on rollback).
    struct Recorder(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl PageCacheInvalidator for Recorder {
        async fn invalidate_url(&self, _tenant_slug: &str, url_path: &str) {
            self.0
                .lock()
                .expect("recorder mutex")
                .push(url_path.to_owned());
        }
    }

    async fn mem_pool() -> Pool {
        Pool::connect("sqlite::memory:")
            .await
            .expect("sqlite in-memory pool")
    }

    /// The purge runs on a detached task; yield until it records at
    /// least `n` URLs (bounded so a regression fails instead of hangs).
    async fn wait_until(urls: &Arc<Mutex<Vec<String>>>, n: usize) -> Vec<String> {
        for _ in 0..5000 {
            {
                let g = urls.lock().expect("recorder mutex");
                if g.len() >= n {
                    return g.clone();
                }
            }
            tokio::task::yield_now().await;
        }
        urls.lock().expect("recorder mutex").clone()
    }

    #[tokio::test]
    async fn commit_fires_the_purge_in_order() {
        let pool = mem_pool().await;
        let urls = Arc::new(Mutex::new(Vec::new()));
        let inv: Arc<dyn PageCacheInvalidator> = Arc::new(Recorder(urls.clone()));

        rustango::atomic!(&pool, |_tx| {
            invalidate_urls_on_commit(
                inv.clone(),
                "acme".to_owned(),
                vec!["/about".to_owned(), "/news".to_owned()],
            );
            assert_eq!(rustango::sql::on_commit_pending(), 1, "one callback queued");
            Ok::<(), rustango::sql::ExecError>(())
        })
        .await
        .expect("atomic commit");

        assert_eq!(
            wait_until(&urls, 2).await,
            vec!["/about".to_owned(), "/news".to_owned()]
        );
    }

    #[tokio::test]
    async fn rollback_drops_the_purge() {
        let pool = mem_pool().await;
        let urls = Arc::new(Mutex::new(Vec::new()));
        let inv: Arc<dyn PageCacheInvalidator> = Arc::new(Recorder(urls.clone()));

        let res = rustango::atomic!(&pool, |_tx| {
            invalidate_urls_on_commit(inv.clone(), "acme".to_owned(), vec!["/about".to_owned()]);
            assert_eq!(
                rustango::sql::on_commit_pending(),
                1,
                "callback queued pre-rollback"
            );
            Err::<(), rustango::sql::ExecError>(rustango::sql::ExecError::EmptyReturning)
        })
        .await;
        assert!(res.is_err(), "atomic should roll back");

        // Give any (erroneously) spawned task room to run, then assert
        // nothing was purged — the callback must have been dropped.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(
            urls.lock().expect("recorder mutex").is_empty(),
            "rollback must not purge"
        );
    }
}
