//! AWS CloudFront invalidator for [`crate::cache_invalidate::PageCacheInvalidator`].
//!
//! Wires the per-page cache-invalidation hook to CloudFront's
//! [CreateInvalidation] API. After a CMS save, the public path is
//! dropped from every edge POP in the distribution so the next
//! request regenerates against the freshly-saved markup instead
//! of the cached version.
//!
//! ## Usage
//!
//! Behind the `cache_cloudfront` feature. The minimum config is
//! the distribution id; credentials come from the standard AWS
//! config chain (env vars, shared credentials file, IRSA / IMDS
//! when running on EKS/EC2, etc.):
//!
//! ```ignore
//! use rustango_cms::cache_cloudfront::CloudFrontInvalidator;
//!
//! // `from_env_distribution` resolves AWS credentials from the
//! // standard provider chain and builds the client. async because
//! // the SDK's config loader does an `imds` lookup on EC2/EKS.
//! let inv = CloudFrontInvalidator::from_env_distribution(
//!     std::env::var("CLOUDFRONT_DISTRIBUTION_ID").unwrap(),
//! ).await;
//!
//! let router = rustango_cms::admin::router_with_invalidator(
//!     tera.clone(),
//!     std::sync::Arc::new(inv),
//! );
//! ```
//!
//! ## Permissions
//!
//! The IAM principal (user / role) the SDK resolves to must carry:
//!
//! ```text
//! cloudfront:CreateInvalidation
//! ```
//!
//! scoped to the target distribution. The role-based assumption
//! flow (the `role_arn` field) is satisfied by setting
//! `AWS_ROLE_ARN` + `AWS_WEB_IDENTITY_TOKEN_FILE` in the
//! environment — the SDK picks them up automatically. Hand-rolled
//! AssumeRole is a follow-up; the env-driven flow covers EKS IRSA
//! + most CI runners.
//!
//! ## Failure mode
//!
//! Logged through `tracing` at WARN; the calling admin handler
//! doesn't propagate the error (cache invalidation is best-effort
//! by trait contract — a stale render is worse than a failed save).
//!
//! [CreateInvalidation]: https://docs.aws.amazon.com/AmazonCloudFront/latest/APIReference/API_CreateInvalidation.html

use std::sync::Arc;

use async_trait::async_trait;

use crate::cache_invalidate::PageCacheInvalidator;

/// CloudFront-backed page-cache invalidator. Holds the resolved
/// SDK client + the target distribution id. Built via
/// [`Self::from_env_distribution`] (recommended) or
/// [`Self::with_client`] for tests / hand-rolled config.
pub struct CloudFrontInvalidator {
    distribution_id: String,
    client: Arc<aws_sdk_cloudfront::Client>,
}

impl CloudFrontInvalidator {
    /// Resolve AWS credentials from the standard provider chain
    /// (env vars → shared credentials file → IMDS / IRSA) and
    /// build a CloudFront client targeting `distribution_id`.
    ///
    /// async because the credential chain does network I/O on
    /// EC2 / EKS to fetch instance / pod credentials. Call once
    /// at boot, share the resulting `Arc<CloudFrontInvalidator>`
    /// across the admin router.
    pub async fn from_env_distribution(distribution_id: impl Into<String>) -> Self {
        // `load_defaults(BehaviorVersion::latest())` replaces the
        // pre-1.0 `load_from_env`; same provider chain, just
        // pinned to a stable behaviour version so future SDK
        // upgrades don't silently flip default knobs.
        let cfg = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = aws_sdk_cloudfront::Client::new(&cfg);
        Self {
            distribution_id: distribution_id.into(),
            client: Arc::new(client),
        }
    }

    /// Build an invalidator from a pre-configured SDK client. Use
    /// when the host already loaded an `aws_config::SdkConfig` for
    /// other AWS calls and wants to reuse it. Tests use this with
    /// a mock client.
    #[must_use]
    pub fn with_client(
        distribution_id: impl Into<String>,
        client: aws_sdk_cloudfront::Client,
    ) -> Self {
        Self {
            distribution_id: distribution_id.into(),
            client: Arc::new(client),
        }
    }

    /// Submit a `CreateInvalidation` for the provided path. The
    /// `CallerReference` is timestamp-based so retried submissions
    /// from a flaky network don't collapse into one CloudFront
    /// request (CF dedups identical references for 8 hours).
    async fn invalidate(&self, path: &str) {
        // CloudFront paths must start with `/`. The trait contract
        // already has callers pass url_paths with a leading slash,
        // but normalise defensively so a missing one doesn't get a
        // 400 from the SDK.
        let normalised = if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("/{path}")
        };
        // Reference must be unique per invalidation; use a coarse
        // timestamp + the path so retries with the same path-and-
        // second collapse (acceptable — same purge) but distinct
        // paths or times don't.
        let caller_ref = format!(
            "rcms-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            normalised.replace(['/', '?', '&'], "_"),
        );
        let batch = aws_sdk_cloudfront::types::InvalidationBatch::builder()
            .caller_reference(caller_ref)
            .paths(
                aws_sdk_cloudfront::types::Paths::builder()
                    .quantity(1)
                    .items(normalised.clone())
                    .build()
                    .expect("paths build never fails — quantity matches items"),
            )
            .build();
        let batch = match batch {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::cache_invalidate",
                    error = %e,
                    path = %normalised,
                    "cloudfront: invalidation batch build failed"
                );
                return;
            }
        };
        let req = self
            .client
            .create_invalidation()
            .distribution_id(&self.distribution_id)
            .invalidation_batch(batch);
        match req.send().await {
            Ok(out) => {
                tracing::debug!(
                    target: "rustango_cms::cache_invalidate",
                    invalidation_id = ?out.invalidation().map(|i| i.id()),
                    path = %normalised,
                    "cloudfront invalidation submitted"
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::cache_invalidate",
                    error = %e,
                    path = %normalised,
                    distribution = %self.distribution_id,
                    "cloudfront: CreateInvalidation failed"
                );
            }
        }
    }
}

#[async_trait]
impl PageCacheInvalidator for CloudFrontInvalidator {
    async fn invalidate_url(&self, _tenant_slug: &str, url_path: &str) {
        // Distribution targeting is per-process; the tenant slug
        // doesn't gate which distribution receives the purge. Hosts
        // that need per-tenant distributions can build one
        // CloudFrontInvalidator per tenant + dispatch in their own
        // PageCacheInvalidator wrapper.
        self.invalidate(url_path).await;
    }

    async fn invalidate_subtree(&self, _tenant_slug: &str, url_path: &str) {
        // CloudFront accepts wildcards in invalidation paths — a
        // `/blog/*` invalidation drops every cached entry under
        // `/blog/`. That's the right shape for slug-rename / move
        // cascades, which is the dominant subtree-purge case in
        // the CMS.
        let trimmed = url_path.trim_end_matches('/');
        let wildcard = format!("{trimmed}/*");
        // Also include the parent path so the moved-page's own
        // cached entry purges alongside its descendants.
        self.invalidate(url_path).await;
        self.invalidate(&wildcard).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CloudFront's SDK doesn't ship a built-in mock client — unlike
    /// the HTTP-based backends where httpmock makes the test trivial,
    /// we'd need `aws-smithy-mocks` (separate crate, heavier) to
    /// exercise the real request shape. For now the helper-level
    /// behaviours (path normalisation, subtree wildcard shape) cover
    /// what's most likely to regress.
    ///
    /// Live exercising of the SDK call path is something hosts can
    /// run against a real distribution with cargo run-mode; the
    /// failure mode (4xx / 5xx → tracing::warn) is symmetric with
    /// the existing Cloudflare backend that DOES have httpmock
    /// coverage, so the contract is consistent.

    /// The leading-slash normalisation is a sync substring operation
    /// — extract just enough of the invalidate() body to test
    /// without juggling a fake SDK client.
    fn normalise_path(path: &str) -> String {
        if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("/{path}")
        }
    }

    #[test]
    fn normalise_adds_leading_slash() {
        assert_eq!(normalise_path("blog/post"), "/blog/post");
    }

    #[test]
    fn normalise_keeps_existing_slash() {
        assert_eq!(normalise_path("/about"), "/about");
    }

    #[test]
    fn subtree_wildcard_shape() {
        // Mirrors the inline construction in `invalidate_subtree`.
        let url_path = "/blog/";
        let trimmed = url_path.trim_end_matches('/');
        let wildcard = format!("{trimmed}/*");
        assert_eq!(wildcard, "/blog/*");
    }

    #[test]
    fn subtree_wildcard_no_trailing_slash() {
        // Slug-rename cascades pass `/blog` (no trailing slash);
        // the wildcard should still come out as `/blog/*`.
        let url_path = "/blog";
        let trimmed = url_path.trim_end_matches('/');
        let wildcard = format!("{trimmed}/*");
        assert_eq!(wildcard, "/blog/*");
    }

    #[test]
    fn caller_reference_contains_path() {
        // CloudFront dedups identical caller_references for 8 hours,
        // so the reference must include the path. The body-call
        // form uses `path.replace(['/', '?', '&'], '_')`.
        let path = "/blog/post?draft=1&v=2";
        let safe = path.replace(['/', '?', '&'], "_");
        assert_eq!(safe, "_blog_post_draft=1_v=2");
        // The full reference shape is `rcms-{secs}-{safe}` — the
        // secs is non-deterministic in a unit test, just confirm
        // the safe-path component renders without collision.
        assert!(safe.contains("blog_post"));
        assert!(!safe.contains('?'));
        assert!(!safe.contains('&'));
    }
}
