//! Sites served under a path on the main domain (`example.com/acme/`)
//! rather than on a host of their own.
//!
//! The framework sends a path-prefix tenant's requests to the host app
//! with the prefix still on the path (`/acme/about`), but every CMS route,
//! page path and link is written from the site's root. [`serve`] wraps the
//! host's router so a site works the same under a prefix:
//!
//! - on the way in, the prefix comes off the path (`/acme/about` is served
//!   as `/about`). The framework finds the tenant again in every handler
//!   from the request, which no longer shows the prefix, so the site is
//!   named in a header the host's resolver reads ([`tenant_header`]);
//!   a copy sent by the client is always dropped first;
//! - on the way out, it goes back on: redirects, cookie paths, and the
//!   root-relative addresses in HTML, CSS, JavaScript and JSON (links,
//!   pictures, forms, and the admin's own `/cms-admin/…` calls). Absolute
//!   addresses (sitemap, feeds, robots.txt) carry it through
//!   [`PREFIX_HEADER`].
//!
//! A host wires both halves:
//!
//! ```ignore
//! rustango::manage::Cli::new()
//!     .tenancy()
//!     .tenant_header(rustango_cms::path_prefix::tenant_header())
//!     .api(rustango_cms::path_prefix::serve(api))
//! ```
//!
//! Sites on their own host pass through untouched.

use std::convert::Infallible;
use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::FromRequestParts as _;
use axum::http::{header, HeaderName, HeaderValue, Request, Response};
use axum::Router;
use regex::{Captures, Regex};
use tower::ServiceExt as _;

/// Names the site a stripped request belongs to, for the framework's
/// resolver. Set only by [`serve`].
pub const TENANT_HEADER: &str = "x-rcms-path-tenant";

/// The prefix taken off the request (`/acme`). Set only by [`serve`];
/// absolute addresses add it after the host.
pub const PREFIX_HEADER: &str = "x-rcms-path-prefix";

/// Largest response body rewritten; a bigger one is sent as it is.
const MAX_REWRITE: usize = 32 * 1024 * 1024;

/// The resolver that finds a stripped request's site, for
/// `Cli::tenant_header`.
#[must_use]
pub fn tenant_header() -> rustango::tenancy::HeaderResolver {
    rustango::tenancy::HeaderResolver::new(HeaderName::from_static(TENANT_HEADER))
}

/// The prefix [`serve`] took off this request, if any.
#[must_use]
pub fn of(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers.get(PREFIX_HEADER).and_then(|v| v.to_str().ok()).filter(|p| p.starts_with('/'))
}

/// `path` as the site sees it: without the prefix [`serve`] took off this
/// request. For addresses the browser reports (`location.pathname`).
#[must_use]
pub fn site_path<'a>(headers: &axum::http::HeaderMap, path: &'a str) -> &'a str {
    match of(headers).and_then(|p| path.strip_prefix(p)) {
        Some("") => "/",
        Some(rest) if rest.starts_with('/') => rest,
        _ => path,
    }
}

/// `router`, serving path-prefix sites from their prefix.
///
/// Its routes stay as they are, so merging other routes into it (the
/// framework's admin and sign-in) works as before. A prefixed path matches
/// none of them and reaches the fallback, which strips the prefix and asks
/// `router` again.
pub fn serve(router: Router) -> Router {
    let inner = router.layer(axum::middleware::from_fn(drop_client_headers));
    let again = inner.clone();
    inner.fallback_service(tower::service_fn(move |req: Request<Body>| {
        let router = again.clone();
        async move { Ok::<_, Infallible>(handle(router, req).await) }
    }))
}

/// Set on a request [`under_prefix`] stripped; a client can't send it.
#[derive(Clone, Copy)]
struct Stripped;

/// Drop the private headers unless [`under_prefix`] set them.
async fn drop_client_headers(mut req: Request<Body>, next: axum::middleware::Next) -> Response<Body> {
    if req.extensions().get::<Stripped>().is_none() {
        req.headers_mut().remove(TENANT_HEADER);
        req.headers_mut().remove(PREFIX_HEADER);
    }
    next.run(req).await
}

async fn handle(router: Router, req: Request<Body>) -> Response<Body> {
    let (mut parts, body) = req.into_parts();
    // Only `serve` may name the site or the prefix.
    parts.headers.remove(TENANT_HEADER);
    parts.headers.remove(PREFIX_HEADER);
    let prefix = match <rustango::extractors::Tenant>::from_request_parts(&mut parts, &()).await {
        Ok(tenant) => tenant
            .org
            .path_prefix
            .as_deref()
            .map(|p| p.trim_end_matches('/'))
            .filter(|p| !p.is_empty() && is_under(parts.uri.path(), p))
            .map(|p| (p.to_owned(), tenant.org.slug.clone())),
        Err(_) => None,
    };
    let req = Request::from_parts(parts, body);
    match prefix {
        Some((prefix, slug)) => under_prefix(router, req, &prefix, &slug).await,
        None => into_response(router.oneshot(req).await),
    }
}

/// Serve `req` from the site's root, as site `slug` at `prefix`.
async fn under_prefix(router: Router, req: Request<Body>, prefix: &str, slug: &str) -> Response<Body> {
    let (mut parts, body) = req.into_parts();
    let (Ok(slug), Ok(value)) = (HeaderValue::from_str(slug), HeaderValue::from_str(prefix)) else {
        return into_response(router.oneshot(Request::from_parts(parts, body)).await);
    };
    parts.uri = strip(&parts.uri, prefix);
    parts.headers.insert(TENANT_HEADER, slug);
    parts.headers.insert(PREFIX_HEADER, value);
    parts.extensions.insert(Stripped);
    let resp = into_response(router.oneshot(Request::from_parts(parts, body)).await);
    restore(resp, prefix).await
}

fn into_response(r: Result<Response<Body>, Infallible>) -> Response<Body> {
    match r {
        Ok(r) => r,
        Err(e) => match e {},
    }
}

/// Is `path` the prefix itself or below it?
fn is_under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `uri` without `prefix` (`/acme/about?x` → `/about?x`, `/acme` → `/`).
fn strip(uri: &axum::http::Uri, prefix: &str) -> axum::http::Uri {
    let rest = uri.path().strip_prefix(prefix).unwrap_or(uri.path());
    let path = if rest.is_empty() { "/" } else { rest };
    let pq = match uri.query() {
        Some(q) => format!("{path}?{q}"),
        None => path.to_owned(),
    };
    pq.parse().unwrap_or_else(|_| uri.clone())
}

/// The response with `prefix` put back on its addresses.
async fn restore(resp: Response<Body>, prefix: &str) -> Response<Body> {
    let (mut parts, body) = resp.into_parts();
    if let Some(to) = parts.headers.get(header::LOCATION).and_then(|v| v.to_str().ok()) {
        if let Ok(v) = HeaderValue::from_str(&location(to, prefix)) {
            parts.headers.insert(header::LOCATION, v);
        }
    }
    let cookies: Vec<HeaderValue> = parts
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().ok().map(|c| cookie_path(c, prefix)).and_then(|c| HeaderValue::from_str(&c).ok()).unwrap_or_else(|| v.clone()))
        .collect();
    if !cookies.is_empty() {
        parts.headers.remove(header::SET_COOKIE);
        for c in cookies {
            parts.headers.append(header::SET_COOKIE, c);
        }
    }
    let rewritable = parts.headers.get(header::CONTENT_ENCODING).is_none()
        && parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| {
                let t = t.to_ascii_lowercase();
                ["text/html", "text/css", "javascript", "application/json"].iter().any(|k| t.contains(k))
            });
    if !rewritable {
        return Response::from_parts(parts, body);
    }
    let Ok(bytes) = axum::body::to_bytes(body, MAX_REWRITE).await else {
        // Too big (or broken): nothing is left to send but an error.
        return Response::from_parts(parts, Body::empty());
    };
    let out = match std::str::from_utf8(&bytes) {
        Ok(text) => Body::from(rewrite(text, prefix)),
        Err(_) => Body::from(bytes),
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, out)
}

/// A redirect target with the prefix, and so is its `next=`.
fn location(to: &str, prefix: &str) -> String {
    if !is_root_relative(to) {
        return to.to_owned();
    }
    let (path, query) = to.split_once('?').map_or((to, None), |(p, q)| (p, Some(q)));
    let query = query.map(|q| {
        q.split('&')
            .map(|pair| match pair.split_once('=') {
                Some(("next", v)) => {
                    let decoded = rustango::url_codec::url_decode(v);
                    if is_root_relative(&decoded) {
                        format!("next={}", rustango::url_codec::url_encode(&format!("{prefix}{decoded}")))
                    } else {
                        pair.to_owned()
                    }
                }
                _ => pair.to_owned(),
            })
            .collect::<Vec<_>>()
            .join("&")
    });
    match query {
        Some(q) => format!("{prefix}{path}?{q}"),
        None => format!("{prefix}{path}"),
    }
}

fn is_root_relative(url: &str) -> bool {
    url.starts_with('/') && !url.starts_with("//") && !url.starts_with("/\\")
}

/// A `Set-Cookie` value scoped under the prefix (`Path=/` → `Path=/acme`).
fn cookie_path(cookie: &str, prefix: &str) -> String {
    cookie
        .split(';')
        .map(|attr| {
            let t = attr.trim_start();
            let lead = &attr[..attr.len() - t.len()];
            match t.get(..5).filter(|n| n.eq_ignore_ascii_case("path=")).map(|_| &t[5..]) {
                Some("/") => format!("{lead}Path={prefix}"),
                Some(path) if is_root_relative(path) => format!("{lead}Path={prefix}{path}"),
                _ => attr.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// Root-relative addresses, one alternative each, matched in one pass so
/// none is prefixed twice:
///
/// - `srcset` values, every candidate;
/// - URL attributes (`href`, `src`, `action`, `formaction`, `poster`,
///   `data-*`);
/// - CSS `url(/…)`;
/// - quoted strings naming a CMS address (`"/cms-admin/…"`,
///   `'/__media__/…'`, …) in scripts and JSON.
static ADDRESSES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?P<ss>\s(?i:srcset)\s*=\s*["'])(?P<ssv>[^"']*)"#,
        r#"|(?P<attr>\s(?i:href|src|action|formaction|poster|data-[a-z0-9_-]+)\s*=\s*["'])(?:/|&#x2[fF];|&#47;)(?P<an>&#x2[fF];|&#47;|[^/\\])"#,
        r#"|(?P<url>url\(\s*["']?)/(?P<un>[^/\\])"#,
        r#"|(?P<lit>["'`])/(?P<seg>(?:cms-admin|__media__|__cms[a-z_-]*|api|members|forms|login|logout|admin|feed|sitemap)(?:[/?#.'"`]|$))"#,
    ))
    .expect("valid address pattern")
});

/// `/`, as written or as an HTML escape (Tera escapes it in attributes).
fn is_slash(s: &str) -> bool {
    s == "/" || s == "&#47;" || s.eq_ignore_ascii_case("&#x2f;")
}

fn unescape_slashes(s: &str) -> String {
    s.replace("&#x2F;", "/").replace("&#x2f;", "/").replace("&#47;", "/")
}

/// `text` with `prefix` put on its root-relative addresses.
fn rewrite(text: &str, prefix: &str) -> String {
    ADDRESSES
        .replace_all(text, |c: &Captures<'_>| {
            if let (Some(lead), Some(value)) = (c.name("ss"), c.name("ssv")) {
                let candidates: Vec<String> = value
                    .as_str()
                    .split(',')
                    .map(|cand| {
                        let t = cand.trim_start();
                        if is_root_relative(&unescape_slashes(t)) {
                            format!("{}{prefix}{t}", &cand[..cand.len() - t.len()])
                        } else {
                            cand.to_owned()
                        }
                    })
                    .collect();
                format!("{}{}", lead.as_str(), candidates.join(","))
            } else if let (Some(lead), Some(next)) = (c.name("attr"), c.name("an")) {
                // `//host/…` (escaped or not) is another site: left alone.
                if is_slash(next.as_str()) {
                    return c[0].to_owned();
                }
                // Keeps the template's escaping: `&#x2F;about` stays escaped.
                let slash = &c[0][lead.len()..c[0].len() - next.len()];
                format!("{}{prefix}{slash}{}", lead.as_str(), next.as_str())
            } else if let (Some(lead), Some(next)) = (c.name("url"), c.name("un")) {
                format!("{}{prefix}/{}", lead.as_str(), next.as_str())
            } else if let (Some(quote), Some(seg)) = (c.name("lit"), c.name("seg")) {
                format!("{}{prefix}/{}", quote.as_str(), seg.as_str())
            } else {
                c[0].to_owned()
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prefix_comes_off_the_request_path() {
        let s = |u: &str| strip(&u.parse().unwrap(), "/acme").to_string();
        assert_eq!(s("/acme"), "/");
        assert_eq!(s("/acme/"), "/");
        assert_eq!(s("/acme/about?x=1"), "/about?x=1");
        assert!(is_under("/acme/x", "/acme"));
        assert!(is_under("/acme", "/acme"));
        assert!(!is_under("/acmeco/x", "/acme"));
    }

    #[test]
    fn html_addresses_get_the_prefix_once() {
        let html = r#"<a href="/">Home</a><a href='/about'>A</a><a href="//cdn.x/y">C</a><a href="https://x/y">X</a>
<img src="/__media__/raw/5" srcset="/__media__/r/5-w400.webp 400w, /__media__/r/5-w800.webp 800w">
<form action="/forms/submit/3"></form><div style="background:url('/__media__/raw/2')"></div>
<a href="&#x2F;work&#x2F;x">E</a><a href="&#x2F;&#x2F;cdn.x&#x2F;y">P</a>
<script>fetch(`/cms-admin/search?q=${q}`); const u = "/__media__/raw/" + id; const p = "/about";</script>"#;
        let out = rewrite(html, "/acme");
        for want in [
            r#"href="/acme/""#,
            r#"href='/acme/about'"#,
            // Tera escapes `/` in attributes; the escaping is kept.
            r#"href="/acme&#x2F;work&#x2F;x""#,
            r#"href="&#x2F;&#x2F;cdn.x&#x2F;y""#,
            r#"href="//cdn.x/y""#,
            r#"href="https://x/y""#,
            r#"src="/acme/__media__/raw/5""#,
            r#"srcset="/acme/__media__/r/5-w400.webp 400w, /acme/__media__/r/5-w800.webp 800w""#,
            r#"action="/acme/forms/submit/3""#,
            "url('/acme/__media__/raw/2')",
            "fetch(`/acme/cms-admin/search?q=${q}`)",
            r#""/acme/__media__/raw/" + id"#,
            // A page path in a script is not a CMS address: left alone.
            r#"const p = "/about""#,
        ] {
            assert!(out.contains(want), "missing {want}\n{out}");
        }
        assert!(!out.contains("/acme/acme"), "{out}");
    }

    #[test]
    fn json_addresses_get_the_prefix() {
        let out = rewrite(r#"{"url":"/cms-admin/pages/4/edit","thumb":"/__media__/r/4.webp","path":"/news"}"#, "/acme");
        assert_eq!(out, r#"{"url":"/acme/cms-admin/pages/4/edit","thumb":"/acme/__media__/r/4.webp","path":"/news"}"#);
    }

    #[test]
    fn redirects_and_their_next_get_the_prefix() {
        assert_eq!(location("/login?next=%2Fcms-admin%2F", "/acme"), "/acme/login?next=%2Facme%2Fcms-admin%2F");
        assert_eq!(location("/cms-admin/pages", "/acme"), "/acme/cms-admin/pages");
        assert_eq!(location("https://x.example/a", "/acme"), "https://x.example/a");
        assert_eq!(location("//evil.example/a", "/acme"), "//evil.example/a");
    }

    #[test]
    fn a_reported_path_loses_the_prefix() {
        let mut h = axum::http::HeaderMap::new();
        assert_eq!(site_path(&h, "/acme/about"), "/acme/about");
        h.insert(PREFIX_HEADER, HeaderValue::from_static("/acme"));
        assert_eq!(site_path(&h, "/acme/about"), "/about");
        assert_eq!(site_path(&h, "/acme"), "/");
        assert_eq!(site_path(&h, "/acmeco/x"), "/acmeco/x");
    }

    /// What the host's router saw, as a page linking to its root.
    fn site() -> Router {
        use axum::routing::get;
        let seen = |headers: axum::http::HeaderMap, uri: axum::http::Uri| async move {
            let named = headers.get(TENANT_HEADER).map(|v| v.to_str().unwrap_or("").to_owned()).unwrap_or_default();
            axum::response::Html(format!(r#"<p>{uri} {named}</p><a href="/about">About</a>"#))
        };
        Router::new()
            .route("/about", get(seen))
            .route("/go", get(|| async { axum::response::Redirect::to("/login?next=%2Fcms-admin%2F") }))
    }

    async fn body_of(resp: Response<Body>) -> String {
        String::from_utf8(axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_prefixed_request_is_served_from_the_site_root() {
        let req = Request::get("/acme/about?x=1").body(Body::empty()).unwrap();
        let out = body_of(under_prefix(site(), req, "/acme", "acme").await).await;
        assert!(out.contains("<p>/about?x=1 acme</p>"), "{out}");
        assert!(out.contains(r#"href="/acme/about""#), "{out}");

        let req = Request::get("/acme/go").body(Body::empty()).unwrap();
        let resp = under_prefix(site(), req, "/acme", "acme").await;
        assert_eq!(resp.headers()[header::LOCATION], "/acme/login?next=%2Facme%2Fcms-admin%2F");
    }

    #[tokio::test]
    async fn a_client_cannot_name_the_site() {
        // No tenant here: the request passes through, minus the header.
        let req = Request::get("/about").header(TENANT_HEADER, "other").body(Body::empty()).unwrap();
        let out = body_of(handle(site(), req).await).await;
        assert!(out.contains("<p>/about </p>"), "{out}");
        assert!(out.contains(r#"href="/about""#), "untouched: {out}");
    }

    #[tokio::test]
    async fn the_host_routes_stay_and_ignore_a_client_header() {
        // Matched directly, not through the fallback: still no header.
        let req = Request::get("/about").header(TENANT_HEADER, "other").header(PREFIX_HEADER, "/x").body(Body::empty()).unwrap();
        let out = body_of(into_response(serve(site()).oneshot(req).await)).await;
        assert!(out.contains("<p>/about </p>"), "{out}");
        // Routes merged in later don't collide with a catch-all.
        let merged = serve(site()).merge(Router::new().route("/login", axum::routing::get(|| async { "login" })));
        let req = Request::get("/login").body(Body::empty()).unwrap();
        assert_eq!(body_of(into_response(merged.oneshot(req).await)).await, "login");
    }

    #[test]
    fn cookies_are_scoped_under_the_prefix() {
        assert_eq!(cookie_path("a=1; Path=/; HttpOnly", "/acme"), "a=1; Path=/acme; HttpOnly");
        assert_eq!(cookie_path("a=1; path=/cms-admin", "/acme"), "a=1; Path=/acme/cms-admin");
        assert_eq!(cookie_path("a=1; HttpOnly", "/acme"), "a=1; HttpOnly");
    }
}
