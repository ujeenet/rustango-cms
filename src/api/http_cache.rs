//! Conditional requests and cache directives for the v2 API.
//!
//! Every API response used to carry no `ETag`, no `Cache-Control`, no
//! `Last-Modified` and no `Vary`. Two consequences, one for each side:
//!
//! * **Clients re-transfer everything.** A SPA polling its navbar pays
//!   the full body on every request because there is nothing to
//!   revalidate against.
//! * **Shared caches can leak.** `tree`, `menus/{slug}` and `detail`'s
//!   `children` are all functions of the session cookie, and a cache
//!   keyed on the URL alone may hand a member's response to the next
//!   anonymous caller. The HTML path already guards against this —
//!   `persist_locale` appends `Vary: Cookie` to every page response — but
//!   the API router is mounted as a sibling and never saw that layer.
//!
//! This middleware fixes both, and it is deliberately dumb: it hashes the
//! rendered body. No `updated_at` bookkeeping to keep in sync, no way for
//! the validator to drift from the content, and it works for endpoints
//! (like the tree) whose body depends on rows from four tables plus the
//! caller's permissions. The cost is that the handler still runs — this
//! saves bandwidth, not database work.

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH, VARY};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// `private` because the body depends on the caller's session, and
/// `no-cache` because it must be revalidated rather than served blind —
/// which is exactly what turns the `ETag` below into a 304.
const CACHE_DIRECTIVE: &str = "private, no-cache";

/// The body varies with the session cookie on every viewer-aware
/// endpoint. Stated on all of them rather than only the ones that
/// currently read a cookie, so adding a viewer to an endpoint later
/// can't silently make a cached response wrong.
const VARY_ON: &str = "Cookie";

/// Largest body worth buffering to hash. Responses above this skip the
/// `ETag` and are simply passed through — no endpoint here should reach
/// it (the tree caps itself at 5000 nodes), and a streaming body must
/// not be silently collected into memory.
const MAX_ETAG_BODY: usize = 8 * 1024 * 1024;

/// Attach validators and directives, and answer `If-None-Match` with 304.
///
/// Only 2xx responses get an `ETag`; an error body is not a cacheable
/// representation of the resource, and giving a 404 a validator invites
/// a client to revalidate its way into believing the page still doesn't
/// exist.
pub async fn layer(req: Request, next: Next) -> Response {
    let if_none_match = req
        .headers()
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let resp = next.run(req).await;
    let status = resp.status();

    let (mut parts, body) = resp.into_parts();
    parts.headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(CACHE_DIRECTIVE),
    );
    append_vary(&mut parts.headers, VARY_ON);

    if !status.is_success() {
        return Response::from_parts(parts, body);
    }

    let bytes = match axum::body::to_bytes(body, MAX_ETAG_BODY).await {
        Ok(b) => b,
        // Too large, or the body errored mid-stream. Either way there is
        // nothing left to hand back, so report it rather than pretending.
        Err(e) => {
            tracing::warn!(target: "rustango_cms::api", error = %e, "could not buffer body for ETag");
            return crate::api::error::ApiError::internal().into_response();
        }
    };

    let etag = weak_etag(&bytes);
    if let Ok(value) = HeaderValue::from_str(&etag) {
        parts.headers.insert(ETAG, value);
    }

    // A client that already holds this exact representation gets the
    // headers and no body.
    if if_none_match.is_some_and(|h| if_none_match_matches(&h, &etag)) {
        parts.status = StatusCode::NOT_MODIFIED;
        parts.headers.remove(axum::http::header::CONTENT_LENGTH);
        return Response::from_parts(parts, Body::empty());
    }

    Response::from_parts(parts, Body::from(bytes))
}

/// A weak validator over the body bytes.
///
/// Weak (`W/`) is the honest label: this says "semantically equivalent",
/// not "byte-identical down to the transfer encoding", and nothing here
/// promises the latter.
fn weak_etag(bytes: &[u8]) -> String {
    // FNV-1a, 64-bit. Not cryptographic — a validator only needs to
    // change when the content does, and an attacker who can pick the
    // body can already read it.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("W/\"{hash:016x}-{}\"", bytes.len())
}

/// RFC 9110 `If-None-Match`: `*`, or a comma-separated list in which any
/// entry matches under the weak comparison (so `W/"x"` matches `"x"`).
fn if_none_match_matches(header: &str, etag: &str) -> bool {
    let strip = |s: &str| s.trim().trim_start_matches("W/").trim().to_owned();
    let want = strip(etag);
    header
        .split(',')
        .any(|candidate| candidate.trim() == "*" || strip(candidate) == want)
}

/// Add a value to `Vary` without dropping one that is already there.
fn append_vary(headers: &mut axum::http::HeaderMap, value: &str) {
    let existing = headers
        .get(VARY)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    if existing
        .split(',')
        .any(|v| v.trim().eq_ignore_ascii_case(value))
    {
        return;
    }
    let merged = if existing.is_empty() {
        value.to_owned()
    } else {
        format!("{existing}, {value}")
    };
    if let Ok(v) = HeaderValue::from_str(&merged) {
        headers.insert(VARY, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_etag_changes_with_the_body_and_not_otherwise() {
        assert_eq!(weak_etag(b"hello"), weak_etag(b"hello"));
        assert_ne!(weak_etag(b"hello"), weak_etag(b"hellp"));
        // Length is part of the tag, so a hash collision alone is not
        // enough to make two different-length bodies look equal.
        assert_ne!(weak_etag(b"hello"), weak_etag(b"hello "));
    }

    #[test]
    fn the_etag_is_marked_weak() {
        assert!(weak_etag(b"x").starts_with("W/\""));
    }

    #[test]
    fn if_none_match_compares_weakly() {
        let tag = weak_etag(b"body");
        assert!(if_none_match_matches(&tag, &tag));
        // A client may echo the tag without the weakness prefix.
        let stripped = tag.trim_start_matches("W/").to_owned();
        assert!(if_none_match_matches(&stripped, &tag));
    }

    #[test]
    fn if_none_match_accepts_a_list_and_a_wildcard() {
        let tag = weak_etag(b"body");
        assert!(if_none_match_matches(&format!("W/\"other\", {tag}"), &tag));
        assert!(if_none_match_matches("*", &tag));
        assert!(!if_none_match_matches("W/\"other\"", &tag));
    }

    #[test]
    fn append_vary_is_idempotent_and_additive() {
        let mut h = axum::http::HeaderMap::new();
        append_vary(&mut h, "Cookie");
        append_vary(&mut h, "Cookie");
        assert_eq!(h.get(VARY).unwrap(), "Cookie");

        let mut h = axum::http::HeaderMap::new();
        h.insert(VARY, HeaderValue::from_static("Accept"));
        append_vary(&mut h, "Cookie");
        assert_eq!(h.get(VARY).unwrap(), "Accept, Cookie");
    }

    #[test]
    fn append_vary_matches_case_insensitively() {
        // `Vary` values are case-insensitive field names; adding "Cookie"
        // to an existing "cookie" must not duplicate it.
        let mut h = axum::http::HeaderMap::new();
        h.insert(VARY, HeaderValue::from_static("cookie"));
        append_vary(&mut h, "Cookie");
        assert_eq!(h.get(VARY).unwrap(), "cookie");
    }
}
