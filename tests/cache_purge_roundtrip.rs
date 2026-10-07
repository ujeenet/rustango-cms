//! The CMS purges pages from the framework's page cache by rebuilding the
//! layer's cache key. When the framework added the tenant to that key, the
//! rebuilt key stopped matching: every purge deleted nothing, and an
//! edited page stayed stale until its TTL ran out. A unit test pinned the
//! old shape, so nothing failed. This drives the real `CachePageLayer`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use rustango::cache::{BoxedCache, InMemoryCache};
use rustango::cache_page::CachePageLayer;
use rustango_cms::cache_invalidate::{BoxedCacheInvalidator, PageCacheInvalidator};
use tower::ServiceExt as _;

const HOST: &str = "acme.example.com";
const TENANT: &str = "acme";

fn app(cache: BoxedCache) -> Router {
    Router::new()
        .route("/about", get(|| async { "About us" }))
        .layer(CachePageLayer::new(cache).key_prefix("rcms:page"))
        // What the tenancy layer leaves on a resolved request.
        .layer(axum::middleware::from_fn(
            |mut req: Request<Body>, next: axum::middleware::Next| async move {
                req.extensions_mut()
                    .insert(rustango::tenancy::TenantSlug(TENANT.to_owned()));
                next.run(req).await
            },
        ))
}

async fn cache_status(app: &Router) -> String {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/about")
                .header("host", HOST)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    res.headers()
        .get("x-cache-status")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn a_purged_page_is_rendered_again() {
    let cache: BoxedCache = Arc::new(InMemoryCache::new());
    let app = app(cache.clone());

    assert_eq!(cache_status(&app).await, "MISS");
    assert_eq!(cache_status(&app).await, "HIT", "the layer caches the page");

    BoxedCacheInvalidator::new(cache, "rcms:page")
        .with_tenant_hosts(TENANT, [HOST.to_owned()])
        .invalidate_url(TENANT, "/about")
        .await;

    assert_eq!(
        cache_status(&app).await,
        "MISS",
        "the purge must remove the entry the layer wrote",
    );
}
