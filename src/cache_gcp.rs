//! Google Cloud CDN purge backend for
//! [`crate::cache_invalidate::PageCacheInvalidator`].
//!
//! Wires the per-page cache-invalidation hook to Cloud CDN's
//! [`urlMaps.invalidateCache`] Compute Engine API. After a CMS save
//! the public URL is dropped from the Cloud CDN edge so the next
//! request regenerates against the freshly-saved markup instead of
//! waiting for the cache TTL.
//!
//! ## Usage
//!
//! Behind the `cache_gcp` feature. Cloud CDN authenticates against
//! the Compute Engine management API with a short-lived OAuth2 bearer
//! token, so the invalidator takes an [`AccessTokenSource`] (use
//! [`StaticToken`](crate::cache_invalidate::StaticToken) for scripts/tests; implement the trait against the
//! GCE metadata server for a long-running server):
//!
//! ```ignore
//! use std::sync::Arc;
//! use rustango_cms::cache_gcp::GoogleCloudCdnInvalidator;
//! use rustango_cms::cache_invalidate::StaticToken;
//!
//! let inv = GoogleCloudCdnInvalidator::new(
//!     "my-gcp-project",
//!     "my-url-map",
//!     Arc::new(StaticToken(std::env::var("GCP_ACCESS_TOKEN").unwrap())),
//! )
//! .with_origin_host("acme.example.com");
//!
//! let router = rustango_cms::admin::router_with_invalidator(
//!     tera.clone(),
//!     Arc::new(inv),
//! );
//! ```
//!
//! The token's principal needs `compute.urlMaps.invalidateCache` on
//! the target URL map. Failures are logged through `tracing` at WARN;
//! the calling admin handler doesn't propagate the error
//! (cache-invalidation is best-effort by trait contract).
//!
//! Cloud CDN invalidation is **per-path** — one API call per `path`
//! (optionally scoped to a `host`) — so there's no bulk endpoint to
//! override; the default [`PageCacheInvalidator::invalidate_urls`]
//! loop applies. Subtree purges use Cloud CDN's `/*` path glob.
//!
//! [`urlMaps.invalidateCache`]: https://cloud.google.com/compute/docs/reference/rest/v1/urlMaps/invalidateCache

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use crate::cache_invalidate::{AccessTokenSource, PageCacheInvalidator};

/// Cloud CDN purge backend. Construct with [`Self::new`]; chain
/// `.with_*` configuration before installing on the admin router.
pub struct GoogleCloudCdnInvalidator {
    /// GCP project id the URL map lives in.
    project: String,
    /// URL map fronting the backend the CMS is served from. Each
    /// invalidation drops cached entries for this map.
    url_map: String,
    /// Supplies the OAuth2 bearer token for the Compute Engine API.
    token: Arc<dyn AccessTokenSource>,
    /// Tenant-slug → list of public hosts. Cloud CDN scopes an
    /// invalidation to a `host` when given one, so the same path is
    /// purged for every registered host. Most deploys map each tenant
    /// to one host.
    tenant_hosts: HashMap<String, Vec<String>>,
    /// Default-when-no-tenant-mapping host. When neither a per-tenant
    /// nor a default host is set, the invalidation is issued with no
    /// `host` (drops the path across the whole URL map).
    default_host: Option<String>,
    /// Reuses [`rustango::http_client::HttpClient`] for the outbound
    /// POST so we inherit framework-wide retry + timeout behaviour.
    http: Arc<rustango::http_client::HttpClient>,
    /// Override of the API base URL — production points at
    /// `https://compute.googleapis.com`; tests point at httpmock.
    api_base: String,
}

impl GoogleCloudCdnInvalidator {
    /// Build a fresh invalidator. The HTTP client uses the framework's
    /// default 10-second timeout + exponential-backoff retry policy.
    ///
    /// # Panics
    /// If the framework HTTP client cannot be constructed (no TLS
    /// backend, etc.) — a startup-time failure where crashing loudly
    /// beats silently dropping every purge.
    #[must_use]
    pub fn new(
        project: impl Into<String>,
        url_map: impl Into<String>,
        token: Arc<dyn AccessTokenSource>,
    ) -> Self {
        let http = rustango::http_client::HttpClient::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("build http client for Google Cloud CDN invalidator");
        Self {
            project: project.into(),
            url_map: url_map.into(),
            token,
            tenant_hosts: HashMap::new(),
            default_host: None,
            http: Arc::new(http),
            api_base: "https://compute.googleapis.com".to_owned(),
        }
    }

    /// Single-tenant convenience — every invalidation scopes to
    /// `host` regardless of tenant slug.
    #[must_use]
    pub fn with_origin_host(mut self, host: impl Into<String>) -> Self {
        self.default_host = Some(host.into());
        self
    }

    /// Register an additional public host for `tenant_slug`. The same
    /// path is purged once per registered host.
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

    /// Override the API base URL. Used by tests to point at an
    /// httpmock server; production keeps the default.
    #[must_use]
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into();
        self
    }

    /// Hosts to scope an invalidation to for `tenant_slug`: the
    /// per-tenant list, else the default (as a 1-elem list), else a
    /// single `None` meaning "no host scope" (whole URL map).
    fn hosts_for(&self, tenant_slug: &str) -> Vec<Option<&str>> {
        if let Some(hosts) = self.tenant_hosts.get(tenant_slug) {
            return hosts.iter().map(|h| Some(h.as_str())).collect();
        }
        match self.default_host.as_deref() {
            Some(h) => vec![Some(h)],
            None => vec![None],
        }
    }

    /// POST one `invalidateCache` for `(path, host?)`. Best-effort:
    /// logs at WARN + returns on any failure.
    async fn invalidate(&self, path: &str, host: Option<&str>) {
        let Some(token) = self.token.token().await else {
            tracing::warn!(
                target: "rustango_cms::cache_invalidate",
                "gcp purge: no access token available — skipping"
            );
            return;
        };
        #[derive(Serialize)]
        struct Body<'a> {
            path: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            host: Option<&'a str>,
        }
        let url = format!(
            "{}/compute/v1/projects/{}/global/urlMaps/{}/invalidateCache",
            self.api_base, self.project, self.url_map
        );
        let req = self.http.post(url.as_str());
        let req = match req.header("authorization", format!("Bearer {token}")) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::cache_invalidate",
                    error = %e,
                    "gcp purge: header build failed"
                );
                return;
            }
        };
        let req = match req.json(&Body { path, host }) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::cache_invalidate",
                    error = %e,
                    "gcp purge: body serialise failed"
                );
                return;
            }
        };
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!(
                    target: "rustango_cms::cache_invalidate",
                    status = %resp.status(), %path,
                    "gcp purge ok"
                );
            }
            Ok(resp) => tracing::warn!(
                target: "rustango_cms::cache_invalidate",
                status = %resp.status(), %path,
                "gcp purge: non-2xx response"
            ),
            Err(e) => tracing::warn!(
                target: "rustango_cms::cache_invalidate",
                error = %e, %path,
                "gcp purge: network failure"
            ),
        }
    }
}

#[async_trait]
impl PageCacheInvalidator for GoogleCloudCdnInvalidator {
    async fn invalidate_url(&self, tenant_slug: &str, url_path: &str) {
        for host in self.hosts_for(tenant_slug) {
            self.invalidate(url_path, host).await;
        }
    }

    async fn invalidate_subtree(&self, tenant_slug: &str, url_path: &str) {
        // Cloud CDN accepts a trailing `/*` glob on the path to drop
        // the page + every descendant in one call.
        let glob = format!("{}/*", url_path.trim_end_matches('/'));
        for host in self.hosts_for(tenant_slug) {
            self.invalidate(&glob, host).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_invalidate::StaticToken;
    use httpmock::Method::POST;
    use httpmock::MockServer;

    fn inv(base: String) -> GoogleCloudCdnInvalidator {
        GoogleCloudCdnInvalidator::new("proj", "umap", Arc::new(StaticToken("tok".to_owned())))
            .with_api_base(base)
    }

    #[tokio::test]
    async fn invalidate_url_posts_path_and_host() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/compute/v1/projects/proj/global/urlMaps/umap/invalidateCache")
                .header("authorization", "Bearer tok")
                .json_body_includes(r#"{"path": "/about", "host": "example.com"}"#);
            then.status(200).body(r#"{"status": "DONE"}"#);
        });
        inv(server.base_url())
            .with_origin_host("example.com")
            .invalidate_url("acme", "/about")
            .await;
        mock.assert();
    }

    #[tokio::test]
    async fn no_host_omits_host_field() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/compute/v1/projects/proj/global/urlMaps/umap/invalidateCache")
                .json_body_includes(r#"{"path": "/about"}"#);
            then.status(200).body(r#"{"status": "DONE"}"#);
        });
        // No origin host registered → host field omitted entirely.
        inv(server.base_url())
            .invalidate_url("acme", "/about")
            .await;
        mock.assert();
    }

    #[tokio::test]
    async fn per_tenant_hosts_fan_out() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST);
            then.status(200).body(r#"{"status": "DONE"}"#);
        });
        inv(server.base_url())
            .with_tenant_host("acme", "acme.example.com")
            .with_tenant_host("acme", "acme.com")
            .invalidate_url("acme", "/pricing")
            .await;
        assert_eq!(mock.calls(), 2, "one invalidateCache per registered host");
    }

    #[tokio::test]
    async fn subtree_appends_glob() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .json_body_includes(r#"{"path": "/blog/*"}"#);
            then.status(200).body(r#"{"status": "DONE"}"#);
        });
        inv(server.base_url())
            .with_origin_host("example.com")
            .invalidate_subtree("acme", "/blog")
            .await;
        mock.assert();
    }

    #[tokio::test]
    async fn non_2xx_doesnt_panic() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST);
            then.status(403).body(r#"{"error": {"code": 403}}"#);
        });
        inv(server.base_url())
            .with_origin_host("example.com")
            .invalidate_url("acme", "/about")
            .await;
    }

    /// A token source returning `None` (e.g. expired + refresh failed)
    /// must skip the POST, not panic.
    #[tokio::test]
    async fn missing_token_skips_request() {
        struct NoToken;
        #[async_trait]
        impl AccessTokenSource for NoToken {
            async fn token(&self) -> Option<String> {
                None
            }
        }
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST);
            then.status(200);
        });
        GoogleCloudCdnInvalidator::new("proj", "umap", Arc::new(NoToken))
            .with_origin_host("example.com")
            .with_api_base(server.base_url())
            .invalidate_url("acme", "/about")
            .await;
        assert_eq!(mock.calls(), 0, "no token should mean no HTTP");
    }
}
