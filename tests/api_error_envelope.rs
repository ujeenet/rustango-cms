//! `docs/api.md` promises **every** failure is JSON with a stable code.
//! These are the failures that never reach a handler, and so were the
//! ones that broke that promise.
//!
//! * A write verb is refused by axum's `MethodRouter` before routing —
//!   correctly, with the `Allow` header RFC 9110 requires, and with an
//!   empty body.
//! * A malformed query value (`?limit=abc`) or path id (`/pages/abc/`) is
//!   refused by the extractor, in `text/plain`.
//!
//! In each case a client whose fetch wrapper parses failures as JSON —
//! the normal shape — got a `SyntaxError` from its own parser instead of
//! the 400 or 405 that explains what it did wrong. The malformed-query
//! case is the same surface `?root=` was reported on.
//!
//! Driven through the real router: these responses are produced by axum,
//! not by this crate, so a test that built its own would be asserting
//! against a value we made up.
//!
//! Only the routing-level refusals are here. The extractor ones need a
//! tenant — every handler takes `Tenant` before its `Query`, so without
//! the tenant middleware the request fails there first — and are
//! asserted against a live server in
//! `e2e/tests/api/spa-integration.spec.ts` instead.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt as _;

/// Read a JSON body out of a response.
async fn json(res: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("the refusal must be JSON")
}

#[tokio::test]
async fn a_write_to_a_read_endpoint_is_refused_in_json() {
    let res = rustango_cms::api::router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v2/pages/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");

    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        res.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default(),
        "application/json",
        "a JSON API must not refuse in a format its clients cannot parse",
    );
    // axum computes this *outside* `Router::layer`, so the middleware
    // never sees it and cannot repeat it in the message — which is why
    // the body says only "method not allowed" and this header is the
    // thing a client reads to learn what would work.
    let allow = res
        .headers()
        .get(axum::http::header::ALLOW)
        .and_then(|v| v.to_str().ok())
        .expect("405 must carry Allow")
        .to_owned();
    assert!(allow.contains("GET"), "got {allow}");

    let body = json(res).await;
    assert_eq!(body["error"]["code"], "method_not_allowed");
    assert!(
        body["error"]["message"].is_string(),
        "the envelope carries a message like every other error: {body}",
    );
}

#[tokio::test]
async fn the_allow_header_reflects_the_route_not_a_constant() {
    // `/api/v2/auth/login` is the one POST on the surface. If the
    // middleware hard-coded "GET, HEAD, OPTIONS" this would say so.
    let res = rustango_cms::api::router()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v2/auth/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");

    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    let allow = res
        .headers()
        .get(axum::http::header::ALLOW)
        .and_then(|v| v.to_str().ok())
        .expect("Allow")
        .to_owned();
    assert!(allow.contains("POST"), "got {allow}");
    assert!(
        !allow.contains("GET"),
        "login does not serve GET, so Allow must not claim it: {allow}",
    );

    let body = json(res).await;
    assert_eq!(body["error"]["code"], "method_not_allowed");
}

#[tokio::test]
async fn a_successful_read_is_untouched_by_the_middleware() {
    // The 405 branch must not intercept anything else. `/locales/` needs
    // no database for the router to route it; a 405 here would mean the
    // middleware is rewriting responses it should pass through.
    let res = rustango_cms::api::router()
        .oneshot(
            Request::builder()
                .uri("/api/v2/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");

    assert_eq!(res.status(), StatusCode::OK);
    let body = json(res).await;
    assert_eq!(body["openapi"].as_str().unwrap_or_default().get(..1), Some("3"));
}

/// Headers a cross-origin browser client must be able to read.
///
/// Browsers hide every response header from JavaScript except a short
/// safelist. `ETag` is not on it — so without naming it in
/// `Access-Control-Expose-Headers` a SPA can never read the validator it
/// is meant to send back in `If-None-Match`, and the whole conditional-
/// request path is dead for browser clients specifically. `Allow` has
/// the same problem on a 405: the refusal names the verbs that would
/// have worked and the client could not see them.
///
/// Asserted here rather than in the e2e suite because the demo server
/// deliberately runs with CORS **off** (other tests pin that default),
/// and this needs a router built with an allowlist.
#[tokio::test]
async fn cors_exposes_the_validator_and_allow_headers() {
    use rustango_cms::api::Cors;

    let cors = Cors::Origins(vec!["https://spa.example.com".to_owned()]);
    let res = rustango_cms::api::router_with(&cors)
        .oneshot(
            Request::builder()
                .uri("/api/v2/openapi.json")
                .header("Origin", "https://spa.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");

    let exposed = res
        .headers()
        .get("access-control-expose-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    assert!(
        exposed.contains("etag"),
        "a client that cannot read ETag cannot ask for a 304: {exposed:?}",
    );
    assert!(
        exposed.contains("allow"),
        "Allow names the verbs that would work: {exposed:?}",
    );
}

/// A SPA on another origin logs a member in with `POST /api/v2/auth/login`
/// and then sends `Authorization: Bearer …`. Both need the preflight to
/// allow them; when it only allowed GET and `content-type`, the browser
/// refused the login and every authorized read before they were sent.
#[tokio::test]
async fn cors_preflight_allows_the_member_login_and_bearer_token() {
    use rustango_cms::api::Cors;

    for (method, header) in [("POST", "content-type"), ("GET", "authorization")] {
        let cors = Cors::Origins(vec!["https://spa.example.com".to_owned()]);
        let res = rustango_cms::api::router_with(&cors)
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/api/v2/auth/login")
                    .header("Origin", "https://spa.example.com")
                    .header("Access-Control-Request-Method", method)
                    .header("Access-Control-Request-Headers", header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("oneshot");

        let get = |name: &str| {
            res.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_ascii_lowercase()
        };
        assert!(
            get("access-control-allow-methods").contains(&method.to_ascii_lowercase()),
            "preflight must allow {method}: {:?}",
            get("access-control-allow-methods"),
        );
        assert!(
            get("access-control-allow-headers").contains(header),
            "preflight must allow the {header} header: {:?}",
            get("access-control-allow-headers"),
        );
    }
}
