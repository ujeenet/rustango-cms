//! Azure CDN purge backend for
//! [`crate::cache_invalidate::PageCacheInvalidator`].
//!
//! Wires the per-page cache-invalidation hook to the Azure CDN
//! [endpoints purge] management API. After a CMS save the public URL
//! is dropped from the Azure edge so the next request regenerates
//! against the freshly-saved markup instead of waiting for the TTL.
//!
//! ## Usage
//!
//! Behind the `cache_azure` feature. The Azure Resource Manager API
//! authenticates with a short-lived Azure AD bearer token, so the
//! invalidator takes an [`AccessTokenSource`] (use [`StaticToken`](crate::cache_invalidate::StaticToken)
//! for scripts/tests; implement the trait against your managed-identity
//! / `az account get-access-token` endpoint for a long-running server):
//!
//! ```ignore
//! use std::sync::Arc;
//! use rustango_cms::cache_azure::AzureCdnInvalidator;
//! use rustango_cms::cache_invalidate::StaticToken;
//!
//! let inv = AzureCdnInvalidator::new(
//!     "00000000-0000-0000-0000-000000000000", // subscription id
//!     "my-rg",                                  // resource group
//!     "my-cdn-profile",                         // profile
//!     "my-endpoint",                            // endpoint
//!     Arc::new(StaticToken(std::env::var("AZURE_ACCESS_TOKEN").unwrap())),
//! );
//!
//! let router = rustango_cms::admin::router_with_invalidator(
//!     tera.clone(),
//!     Arc::new(inv),
//! );
//! ```
//!
//! The token's principal needs `Microsoft.Cdn/profiles/endpoints/purge/action`
//! on the endpoint. Failures are logged through `tracing` at WARN; the
//! calling admin handler doesn't propagate the error
//! (cache-invalidation is best-effort by trait contract).
//!
//! Azure's purge takes a `contentPaths` array, so a multi-URL save is
//! collapsed into a single request via the
//! [`PageCacheInvalidator::invalidate_urls`] override. Purges are
//! path-based (no per-host scope). Subtree purges use Azure's `/*`
//! path glob.
//!
//! [endpoints purge]: https://learn.microsoft.com/rest/api/cdn/endpoints/purge-content

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use crate::cache_invalidate::{AccessTokenSource, PageCacheInvalidator};

/// Azure CDN purge backend. Construct with [`Self::new`]; chain
/// `.with_*` configuration before installing on the admin router.
pub struct AzureCdnInvalidator {
    subscription_id: String,
    resource_group: String,
    profile: String,
    endpoint: String,
    /// Supplies the OAuth2 bearer token for the ARM API.
    token: Arc<dyn AccessTokenSource>,
    /// Reuses [`rustango::http_client::HttpClient`] for the outbound
    /// POST so we inherit framework-wide retry + timeout behaviour.
    http: Arc<rustango::http_client::HttpClient>,
    /// Override of the API base URL — production points at
    /// `https://management.azure.com`; tests point at httpmock.
    api_base: String,
    /// ARM API version query param.
    api_version: String,
}

impl AzureCdnInvalidator {
    /// Max `contentPaths` per purge request. Azure accepts large
    /// batches, but we chunk to stay well within request-size limits
    /// on a big subtree purge.
    const MAX_PATHS_PER_REQUEST: usize = 50;

    /// Build a fresh invalidator. The HTTP client uses the framework's
    /// default 10-second timeout + exponential-backoff retry policy.
    ///
    /// # Panics
    /// If the framework HTTP client cannot be constructed (no TLS
    /// backend, etc.) — a startup-time failure where crashing loudly
    /// beats silently dropping every purge.
    #[must_use]
    pub fn new(
        subscription_id: impl Into<String>,
        resource_group: impl Into<String>,
        profile: impl Into<String>,
        endpoint: impl Into<String>,
        token: Arc<dyn AccessTokenSource>,
    ) -> Self {
        let http = rustango::http_client::HttpClient::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("build http client for Azure CDN invalidator");
        Self {
            subscription_id: subscription_id.into(),
            resource_group: resource_group.into(),
            profile: profile.into(),
            endpoint: endpoint.into(),
            token,
            http: Arc::new(http),
            api_base: "https://management.azure.com".to_owned(),
            api_version: "2023-05-01".to_owned(),
        }
    }

    /// Override the API base URL. Used by tests to point at an
    /// httpmock server; production keeps the default.
    #[must_use]
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into();
        self
    }

    /// Override the ARM API version (default `2023-05-01`).
    #[must_use]
    pub fn with_api_version(mut self, api_version: impl Into<String>) -> Self {
        self.api_version = api_version.into();
        self
    }

    fn purge_url(&self) -> String {
        format!(
            "{base}/subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Cdn/profiles/{profile}/endpoints/{endpoint}/purge?api-version={ver}",
            base = self.api_base,
            sub = self.subscription_id,
            rg = self.resource_group,
            profile = self.profile,
            endpoint = self.endpoint,
            ver = self.api_version,
        )
    }

    /// POST one `purge` with the given `contentPaths`. Best-effort:
    /// logs at WARN + returns on any failure.
    async fn purge(&self, content_paths: &[String]) {
        if content_paths.is_empty() {
            return;
        }
        let Some(token) = self.token.token().await else {
            tracing::warn!(
                target: "rustango_cms::cache_invalidate",
                "azure purge: no access token available — skipping"
            );
            return;
        };
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body<'a> {
            content_paths: &'a [String],
        }
        let url = self.purge_url();
        for chunk in content_paths.chunks(Self::MAX_PATHS_PER_REQUEST) {
            let req = self.http.post(url.as_str());
            let req = match req.header("authorization", format!("Bearer {token}")) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        target: "rustango_cms::cache_invalidate",
                        error = %e,
                        "azure purge: header build failed"
                    );
                    return;
                }
            };
            let req = match req.json(&Body {
                content_paths: chunk,
            }) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        target: "rustango_cms::cache_invalidate",
                        error = %e,
                        "azure purge: body serialise failed"
                    );
                    return;
                }
            };
            match req.send().await {
                // Azure returns 200 (done) or 202 (accepted, async).
                Ok(resp) if resp.status().is_success() => tracing::debug!(
                    target: "rustango_cms::cache_invalidate",
                    status = %resp.status(),
                    "azure purge ok"
                ),
                Ok(resp) => tracing::warn!(
                    target: "rustango_cms::cache_invalidate",
                    status = %resp.status(),
                    "azure purge: non-2xx response"
                ),
                Err(e) => tracing::warn!(
                    target: "rustango_cms::cache_invalidate",
                    error = %e,
                    "azure purge: network failure"
                ),
            }
        }
    }
}

#[async_trait]
impl PageCacheInvalidator for AzureCdnInvalidator {
    async fn invalidate_url(&self, _tenant_slug: &str, url_path: &str) {
        self.purge(&[url_path.to_owned()]).await;
    }

    async fn invalidate_subtree(&self, _tenant_slug: &str, url_path: &str) {
        // Azure accepts a trailing `/*` glob to drop the page + every
        // descendant in one call.
        let glob = format!("{}/*", url_path.trim_end_matches('/'));
        self.purge(&[glob]).await;
    }

    async fn invalidate_urls(&self, _tenant_slug: &str, url_paths: &[String]) {
        // #428 — one purge carrying every path's contentPaths (chunked
        // by `purge`) instead of one request per path.
        self.purge(url_paths).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_invalidate::StaticToken;
    use httpmock::Method::POST;
    use httpmock::MockServer;

    fn inv(base: String) -> AzureCdnInvalidator {
        AzureCdnInvalidator::new(
            "sub-1",
            "rg-1",
            "profile-1",
            "endpoint-1",
            Arc::new(StaticToken("tok".to_owned())),
        )
        .with_api_base(base)
    }

    #[tokio::test]
    async fn invalidate_url_posts_content_paths() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/subscriptions/sub-1/resourceGroups/rg-1/providers/Microsoft.Cdn/profiles/profile-1/endpoints/endpoint-1/purge")
                .query_param("api-version", "2023-05-01")
                .header("authorization", "Bearer tok")
                .json_body_includes(r#"{"contentPaths": ["/about"]}"#);
            then.status(200);
        });
        inv(server.base_url())
            .invalidate_url("acme", "/about")
            .await;
        mock.assert();
    }

    #[tokio::test]
    async fn invalidate_urls_batches_into_one_request() {
        // #428 — three paths must collapse into ONE purge POST.
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .json_body_includes(r#"{"contentPaths": ["/a", "/b", "/c"]}"#);
            then.status(202);
        });
        inv(server.base_url())
            .invalidate_urls("acme", &["/a".to_owned(), "/b".to_owned(), "/c".to_owned()])
            .await;
        assert_eq!(mock.calls(), 1, "three paths should batch into one POST");
    }

    #[tokio::test]
    async fn subtree_appends_glob() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .json_body_includes(r#"{"contentPaths": ["/blog/*"]}"#);
            then.status(200);
        });
        inv(server.base_url())
            .invalidate_subtree("acme", "/blog")
            .await;
        mock.assert();
    }

    #[tokio::test]
    async fn batches_chunk_over_limit() {
        // 51 paths exceeds the 50-per-request cap → exactly two POSTs.
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST);
            then.status(200);
        });
        let paths: Vec<String> = (0..51).map(|i| format!("/p{i}")).collect();
        inv(server.base_url()).invalidate_urls("acme", &paths).await;
        assert_eq!(mock.calls(), 2, "51 paths should split into 50 + 1");
    }

    #[tokio::test]
    async fn non_2xx_doesnt_panic() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST);
            then.status(401)
                .body(r#"{"error": {"code": "AuthenticationFailed"}}"#);
        });
        inv(server.base_url())
            .invalidate_url("acme", "/about")
            .await;
    }

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
        AzureCdnInvalidator::new("s", "r", "p", "e", Arc::new(NoToken))
            .with_api_base(server.base_url())
            .invalidate_url("acme", "/about")
            .await;
        assert_eq!(mock.calls(), 0, "no token should mean no HTTP");
    }
}
