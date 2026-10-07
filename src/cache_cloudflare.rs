//! Cloudflare-purge backend for [`crate::cache_invalidate::PageCacheInvalidator`].
//!
//! Wires the per-page cache-invalidation hook to Cloudflare's
//! [purge_cache] API. After a CMS save, the public URL is dropped
//! from Cloudflare's edge cache so the next request regenerates
//! against the freshly-saved markup instead of waiting for the
//! cache TTL to elapse.
//!
//! ## Usage
//!
//! Behind the `cache_cloudflare` feature. Two args:
//!
//! ```ignore
//! use rustango_cms::cache_cloudflare::CloudflareInvalidator;
//!
//! let inv = CloudflareInvalidator::new(
//!     std::env::var("CLOUDFLARE_ZONE_ID").unwrap(),
//!     std::env::var("CLOUDFLARE_API_TOKEN").unwrap(),
//! )
//! .with_origin_host("acme.example.com");
//!
//! let router = rustango_cms::admin::router_with_invalidator(
//!     tera.clone(),
//!     std::sync::Arc::new(inv),
//! );
//! ```
//!
//! The token needs `Zone:Cache Purge:Edit` on the target zone —
//! [token-permissions]. Failures are logged through `tracing`
//! at WARN; the calling admin handler doesn't propagate the error
//! (cache-invalidation is best-effort by trait contract).
//!
//! [purge_cache]: https://developers.cloudflare.com/api/operations/zone-purge
//! [token-permissions]: https://developers.cloudflare.com/fundamentals/api/get-started/create-token/

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use crate::cache_invalidate::PageCacheInvalidator;

/// Build absolute URLs the public router is serving by joining the
/// tenant's public host with the supplied url_path. Cloudflare's
/// purge API accepts full URLs (`https://example.com/about/`) and
/// drops the matching edge entry on every PoP. Per-tenant deploys
/// typically map one tenant to one origin host; multi-host setups
/// can register the additional hosts via
/// [`CloudflareInvalidator::with_tenant_host`].
///
/// Construct with [`CloudflareInvalidator::new`]; chain `.with_*`
/// configuration calls before installing on the admin router.
pub struct CloudflareInvalidator {
    /// Cloudflare zone id the URLs live under. Pulled from the
    /// dashboard or `cloudflare-cli zone list`.
    zone_id: String,
    /// API token with `Zone:Cache Purge:Edit` scoped to `zone_id`.
    api_token: String,
    /// Tenant-slug → list of public origin hosts. Most deploys map
    /// each tenant to one host; register additional ones here if
    /// the same content is served under multiple domains.
    tenant_hosts: HashMap<String, Vec<String>>,
    /// Default-when-no-tenant-mapping origin host. Set via
    /// [`Self::with_origin_host`]. Useful for single-tenant deploys.
    default_host: Option<String>,
    /// HTTPS / HTTP scheme to prepend to the URL. Defaults to
    /// `https` since Cloudflare-fronted origins are TLS-only in
    /// practice; expose a setter for the unusual dev case.
    scheme: String,
    /// Reuses [`rustango::http_client::HttpClient`] for the
    /// outbound POST so we inherit framework-wide retry + timeout
    /// behaviour.
    http: Arc<rustango::http_client::HttpClient>,
    /// Override of the API base URL — production points at
    /// `https://api.cloudflare.com`; the integration tests point at
    /// the httpmock server.
    api_base: String,
}

impl CloudflareInvalidator {
    /// Build a fresh invalidator. The HTTP client uses the
    /// framework's default 10-second timeout + exponential-backoff
    /// retry policy.
    ///
    /// # Panics
    /// If the framework HTTP client cannot be constructed (no TLS
    /// backend available, etc.). This is a startup-time failure;
    /// crashing loudly is better than silently dropping every purge.
    #[must_use]
    pub fn new(zone_id: impl Into<String>, api_token: impl Into<String>) -> Self {
        let http = rustango::http_client::HttpClient::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("build http client for Cloudflare invalidator");
        Self {
            zone_id: zone_id.into(),
            api_token: api_token.into(),
            tenant_hosts: HashMap::new(),
            default_host: None,
            scheme: "https".to_owned(),
            http: Arc::new(http),
            api_base: "https://api.cloudflare.com".to_owned(),
        }
    }

    /// Single-tenant convenience — every invalidation targets
    /// `<scheme>://<host><url_path>` regardless of tenant slug.
    /// Multi-tenant setups should use [`Self::with_tenant_host`]
    /// instead so each tenant resolves to its own public host(s).
    #[must_use]
    pub fn with_origin_host(mut self, host: impl Into<String>) -> Self {
        self.default_host = Some(host.into());
        self
    }

    /// Register an additional public host for `tenant_slug`. The
    /// same page URL gets purged for every registered host; useful
    /// when a tenant is served under both `acme.example.com` and a
    /// vanity domain `acme.com`.
    #[must_use]
    pub fn with_tenant_host(
        mut self,
        tenant_slug: impl Into<String>,
        host: impl Into<String>,
    ) -> Self {
        self.tenant_hosts
            .entry(tenant_slug.into())
            .or_default()
            .push(host.into());
        self
    }

    /// Override the URL scheme (default: `https`). Useful for
    /// localhost / dev where the public origin is HTTP-only.
    #[must_use]
    pub fn with_scheme(mut self, scheme: impl Into<String>) -> Self {
        self.scheme = scheme.into();
        self
    }

    /// Override the Cloudflare API base URL. Used by tests to point
    /// at an httpmock server; production keeps the default.
    #[must_use]
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into();
        self
    }

    /// Resolve every public URL we need to purge for one
    /// `(tenant_slug, url_path)` pair. Joins each registered host
    /// (per-tenant + the fallback `default_host` if any) with the
    /// path. The path is taken verbatim — Cloudflare's API matches
    /// exact-URL purges, so callers should pre-normalise (leading
    /// `/`, no trailing slash for non-root).
    fn resolve_urls(&self, tenant_slug: &str, url_path: &str) -> Vec<String> {
        let mut hosts: Vec<&str> = self
            .tenant_hosts
            .get(tenant_slug)
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        if hosts.is_empty() {
            if let Some(h) = self.default_host.as_deref() {
                hosts.push(h);
            }
        }
        hosts
            .into_iter()
            .map(|h| format!("{}://{}{}", self.scheme, h, url_path))
            .collect()
    }

    /// Max `files` entries per `purge_cache` request. Cloudflare caps
    /// the Free/Pro/Business tiers at 30 URLs per call; Enterprise
    /// allows more. We chunk at 30 so a single save spanning many
    /// descendant URLs works on every plan.
    const MAX_FILES_PER_REQUEST: usize = 30;

    /// POST to `/client/v4/zones/{zone_id}/purge_cache` with the
    /// exact-URL `files` list. Best-effort. Chunked at
    /// [`Self::MAX_FILES_PER_REQUEST`] so large subtree purges don't
    /// 400 on the non-Enterprise per-request cap. Exposed at
    /// module scope (vs the trait fn) so the integration tests can
    /// drive it without juggling an `Arc<dyn Trait>` cast.
    async fn purge(&self, urls: Vec<String>) {
        if urls.is_empty() {
            return;
        }
        #[derive(Serialize)]
        struct Body<'a> {
            files: &'a [String],
        }
        for chunk in urls.chunks(Self::MAX_FILES_PER_REQUEST) {
            self.send_purge(&Body { files: chunk }, "files").await;
        }
    }

    /// Purge by **cache-tag**. Cloudflare Enterprise stamps
    /// responses with `Cache-Tag` headers; purging a tag drops every
    /// edge entry carrying it in one call — far cheaper than
    /// enumerating URLs. The CMS doesn't auto-tag responses, so this
    /// is a public hook: a host that sets `Cache-Tag` (e.g. per
    /// page-type or per snippet) calls this after the relevant save.
    /// No-op on non-Enterprise zones (the API rejects `tags`).
    pub async fn purge_tags(&self, tags: Vec<String>) {
        if tags.is_empty() {
            return;
        }
        #[derive(Serialize)]
        struct Body {
            tags: Vec<String>,
        }
        self.send_purge(&Body { tags }, "tags").await;
    }

    /// Purge by **URL prefix** — Enterprise-only. Drops every
    /// edge entry whose URL starts with one of `prefixes`
    /// (`example.com/blog/`), i.e. a true subtree purge without
    /// enumerating each descendant. No-op on non-Enterprise zones.
    pub async fn purge_prefixes(&self, prefixes: Vec<String>) {
        if prefixes.is_empty() {
            return;
        }
        #[derive(Serialize)]
        struct Body {
            prefixes: Vec<String>,
        }
        self.send_purge(&Body { prefixes }, "prefixes").await;
    }

    /// Send one `purge_cache` POST with an arbitrary body shape
    /// (`files` / `tags` / `prefixes`). `kind` is only for log
    /// context. Failures are logged at WARN and swallowed per the
    /// [`PageCacheInvalidator`] best-effort contract.
    async fn send_purge<B: Serialize>(&self, body: &B, kind: &str) {
        let url = format!(
            "{}/client/v4/zones/{}/purge_cache",
            self.api_base, self.zone_id
        );
        let req = self.http.post(url.as_str());
        let req = match req.header("authorization", format!("Bearer {}", self.api_token)) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::cache_invalidate",
                    error = %e, %kind,
                    "cloudflare purge: header build failed"
                );
                return;
            }
        };
        let req = match req.json(body) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::cache_invalidate",
                    error = %e, %kind,
                    "cloudflare purge: body serialise failed"
                );
                return;
            }
        };
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!(
                    target: "rustango_cms::cache_invalidate",
                    status = %resp.status(), %kind,
                    "cloudflare purge ok"
                );
            }
            Ok(resp) => {
                tracing::warn!(
                    target: "rustango_cms::cache_invalidate",
                    status = %resp.status(), %kind,
                    "cloudflare purge: non-2xx response"
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::cache_invalidate",
                    error = %e, %kind,
                    "cloudflare purge: network failure"
                );
            }
        }
    }
}

#[async_trait]
impl PageCacheInvalidator for CloudflareInvalidator {
    async fn invalidate_url(&self, tenant_slug: &str, url_path: &str) {
        self.purge(self.resolve_urls(tenant_slug, url_path)).await;
    }

    async fn invalidate_subtree(&self, tenant_slug: &str, url_path: &str) {
        // Cloudflare's `purge_cache` API supports `prefixes` for
        // subtree purges, but it's an Enterprise-only feature. The
        // free / Pro / Business tiers only accept exact URLs. Fall
        // back to a single-URL purge — callers on an Enterprise zone
        // can call [`Self::purge_prefixes`] explicitly for a true
        // subtree drop. Keeping the default to the single-URL purge
        // gives Free/Pro/Business operators a working purge for the
        // most-common case.
        self.purge(self.resolve_urls(tenant_slug, url_path)).await;
    }

    async fn invalidate_urls(&self, tenant_slug: &str, url_paths: &[String]) {
        // #428 — batch every (path × host) into one `files` array
        // (chunked by `purge`) instead of one POST per path. A save
        // that touches a page + its descendants now costs a single
        // round-trip on the common case (≤30 URLs).
        let urls: Vec<String> = url_paths
            .iter()
            .flat_map(|p| self.resolve_urls(tenant_slug, p))
            .collect();
        self.purge(urls).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::Method::POST;
    use httpmock::MockServer;

    /// Pure-Rust check — the `resolve_urls` build is exercised by
    /// the live tests below but a sync check keeps the URL-shape
    /// regression guard cheap.
    #[test]
    fn resolve_urls_uses_default_host_when_no_per_tenant() {
        let inv = CloudflareInvalidator::new("z", "t").with_origin_host("example.com");
        let urls = inv.resolve_urls("acme", "/about");
        assert_eq!(urls, vec!["https://example.com/about"]);
    }

    #[test]
    fn resolve_urls_per_tenant_overrides_default() {
        let inv = CloudflareInvalidator::new("z", "t")
            .with_origin_host("fallback.example.com")
            .with_tenant_host("acme", "acme.example.com")
            .with_tenant_host("acme", "acme.com");
        let urls = inv.resolve_urls("acme", "/pricing");
        assert!(urls.contains(&"https://acme.example.com/pricing".to_owned()));
        assert!(urls.contains(&"https://acme.com/pricing".to_owned()));
        assert!(
            !urls.contains(&"https://fallback.example.com/pricing".to_owned()),
            "fallback shouldn't fire when tenant has its own host: {urls:?}"
        );
    }

    #[test]
    fn resolve_urls_no_host_no_urls() {
        // No default host + no per-tenant registration → empty
        // list. The purge fn short-circuits before sending.
        let inv = CloudflareInvalidator::new("z", "t");
        assert!(inv.resolve_urls("acme", "/about").is_empty());
    }

    #[test]
    fn resolve_urls_custom_scheme() {
        let inv = CloudflareInvalidator::new("z", "t")
            .with_origin_host("example.com")
            .with_scheme("http");
        let urls = inv.resolve_urls("acme", "/about");
        assert_eq!(urls, vec!["http://example.com/about"]);
    }

    #[tokio::test]
    async fn purge_sends_correct_url_and_headers() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/client/v4/zones/zone-xyz/purge_cache")
                .header("authorization", "Bearer secret-token")
                .header("content-type", "application/json")
                .json_body_includes(r#"{"files": ["https://example.com/about"]}"#);
            then.status(200).body(r#"{"success": true}"#);
        });
        let inv = CloudflareInvalidator::new("zone-xyz", "secret-token")
            .with_origin_host("example.com")
            .with_api_base(server.base_url());
        inv.invalidate_url("acme", "/about").await;
        mock.assert();
    }

    #[tokio::test]
    async fn non_2xx_response_doesnt_panic() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST);
            then.status(403).body(
                r#"{"success": false, "errors": [{"code": 1000, "message": "Invalid token"}]}"#,
            );
        });
        let inv = CloudflareInvalidator::new("z", "bad-token")
            .with_origin_host("example.com")
            .with_api_base(server.base_url());
        // Should swallow the error per the trait contract.
        inv.invalidate_url("acme", "/about").await;
    }

    #[tokio::test]
    async fn no_host_no_request_fires() {
        // Forgetting to register a host shouldn't 500 the admin —
        // the empty URL list is short-circuited before the POST.
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST);
            then.status(200);
        });
        let inv = CloudflareInvalidator::new("z", "t").with_api_base(server.base_url());
        inv.invalidate_url("acme", "/about").await;
        assert_eq!(mock.calls(), 0, "no host registered should mean no HTTP");
    }

    #[tokio::test]
    async fn subtree_falls_back_to_url_purge() {
        // Enterprise-only `prefixes` purge isn't shipped in v1 — the
        // subtree variant should send the same exact-URL purge as
        // the single-URL one.
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/client/v4/zones/z/purge_cache");
            then.status(200).body(r#"{"success": true}"#);
        });
        let inv = CloudflareInvalidator::new("z", "t")
            .with_origin_host("example.com")
            .with_api_base(server.base_url());
        inv.invalidate_subtree("acme", "/blog").await;
        mock.assert();
    }

    #[tokio::test]
    async fn invalidate_urls_batches_into_one_request() {
        // #428 — three paths must collapse into ONE purge_cache POST
        // carrying all three exact URLs, not three separate calls.
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/client/v4/zones/z/purge_cache")
                .json_body_includes(
                    r#"{"files": ["https://example.com/a", "https://example.com/b", "https://example.com/c"]}"#,
                );
            then.status(200).body(r#"{"success": true}"#);
        });
        let inv = CloudflareInvalidator::new("z", "t")
            .with_origin_host("example.com")
            .with_api_base(server.base_url());
        inv.invalidate_urls("acme", &["/a".to_owned(), "/b".to_owned(), "/c".to_owned()])
            .await;
        assert_eq!(mock.calls(), 1, "three paths should batch into one POST");
    }

    #[tokio::test]
    async fn invalidate_urls_chunks_over_thirty() {
        // 31 URLs exceeds the 30-per-request cap → exactly two POSTs.
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/client/v4/zones/z/purge_cache");
            then.status(200).body(r#"{"success": true}"#);
        });
        let inv = CloudflareInvalidator::new("z", "t")
            .with_origin_host("example.com")
            .with_api_base(server.base_url());
        let paths: Vec<String> = (0..31).map(|i| format!("/p{i}")).collect();
        inv.invalidate_urls("acme", &paths).await;
        assert_eq!(mock.calls(), 2, "31 URLs should split into 30 + 1");
    }

    #[tokio::test]
    async fn purge_tags_sends_tags_body() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/client/v4/zones/z/purge_cache")
                .header("authorization", "Bearer tok")
                .json_body_includes(r#"{"tags": ["page-type:blog", "snippet:42"]}"#);
            then.status(200).body(r#"{"success": true}"#);
        });
        let inv = CloudflareInvalidator::new("z", "tok").with_api_base(server.base_url());
        inv.purge_tags(vec!["page-type:blog".to_owned(), "snippet:42".to_owned()])
            .await;
        mock.assert();
    }

    #[tokio::test]
    async fn purge_prefixes_sends_prefixes_body() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/client/v4/zones/z/purge_cache")
                .json_body_includes(r#"{"prefixes": ["example.com/blog/"]}"#);
            then.status(200).body(r#"{"success": true}"#);
        });
        let inv = CloudflareInvalidator::new("z", "t").with_api_base(server.base_url());
        inv.purge_prefixes(vec!["example.com/blog/".to_owned()])
            .await;
        mock.assert();
    }

    #[tokio::test]
    async fn empty_tag_and_prefix_purges_are_noops() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST);
            then.status(200);
        });
        let inv = CloudflareInvalidator::new("z", "t").with_api_base(server.base_url());
        inv.purge_tags(vec![]).await;
        inv.purge_prefixes(vec![]).await;
        assert_eq!(mock.calls(), 0, "empty lists must not POST");
    }
}
