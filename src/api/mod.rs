//! REST API v2 — public reads plus member login.
//!
//! Lets headless frontends (Next.js / Nuxt / Astro / SvelteKit) read
//! pages, images, and documents over JSON — pagination, sparse-field
//! selection, tree filters
//! (child_of / descendant_of / ancestor_of), translation siblings,
//! locale, search, ordering.
//!
//! ## Mounting
//!
//! Host apps build the API router alongside the public CMS router:
//!
//! ```ignore
//! use std::sync::Arc;
//! use axum::Router;
//!
//! let api = rustango_cms::api::router();
//! let public = rustango_cms::router_at("/p", tera);
//! let app = Router::new().merge(api).merge(public);
//! ```
//!
//! All routes live under `/api/v2/…`.
//!
//! ## Endpoints
//!
//! The core routes are below; [`ROUTE_PATHS`] and
//! `GET /api/v2/openapi.json` are the complete list (tree, menus,
//! locales, search, changes).
//!
//! | Path | Returns |
//! |---|---|
//! | `GET /api/v2/pages/` | paginated page list (published only) |
//! | `GET /api/v2/pages/find/?html_path=…` | resolve a URL path → 302 to its detail |
//! | `GET /api/v2/pages/{id}/` | single page + extension fields |
//! | `GET /api/v2/images/` | paginated image list |
//! | `GET /api/v2/images/{id}/` | single image metadata |
//! | `GET /api/v2/documents/` | paginated document list |
//! | `GET /api/v2/documents/{id}/` | single document metadata |
//! | `GET /api/v2/snippets/` | paginated snippet list (`?type=` to filter) |
//! | `GET /api/v2/snippets/{id}/` | single snippet |
//! | `POST /api/v2/auth/login` | member bearer token (see [`auth`]) |
//!
//! ## Response shape
//!
//! Every list response carries `{ meta: { total_count, limit, offset }, items: [...] }`.
//! Per-row shape: a `meta` sub-object
//! with `type`, `detail_url`, plus the content fields hoisted to the
//! top level. Sparse selection (`?fields=`) filters which content
//! fields appear; `meta` is always present.
//!
//! View-restricted pages are gated by the same `view_restriction_guard`
//! the public renderer uses; a member authenticates with the session
//! cookie or the bearer token from `auth/login`.
//!
//! ## Not provided
//!
//! - Write API (`POST`/`PUT`/`PATCH` on content)
//! - GraphQL
//! - Browsable HTML renderer

use axum::routing::get;
use axum::Router;
use rustango::cors::CorsRouterExt as _;

pub mod auth;
pub mod changes;
pub mod documents;
pub mod error;
pub mod http_cache;
pub mod images;
pub mod locales;
pub mod menus;
pub mod pages;
pub mod openapi;
pub mod query;
pub mod search;
pub mod snippets;
pub mod tree;

/// Path prefix every v2 endpoint lives under.
///
/// Hosts pass this to `CsrfConfig::exempt_prefix`. Apart from the
/// credential-based member login, the API registers only `GET`, so
/// there is no cookie-authenticated write for CSRF to protect. Without
/// the exemption the layer rejects unsafe verbs *before* routing, and a
/// client attempting a write is told `403 Forbidden` rather than
/// `405 Method Not Allowed`: it goes looking for a permissions problem
/// instead of concluding the endpoint does not exist.
pub const PREFIX: &str = "/api/v2";

/// Response headers a cross-origin browser client is allowed to read.
///
/// Only a handful of headers are readable from JavaScript by default
/// (`Cache-Control`, `Content-Type`, `Last-Modified`, and a couple
/// more); everything else is hidden unless the server names it here.
/// `ETag` and `Allow` were both hidden, which quietly disabled two
/// features for the exact audience CORS exists to serve:
///
/// * **Conditional requests.** The API attaches a weak `ETag` to every
///   cacheable body and answers `If-None-Match` with a 304 — but a SPA
///   that cannot *read* the `ETag` can never send it back, so a polling
///   client re-transferred the whole body every tick.
/// * **`Allow` on a 405.** The refusal names the verbs that would have
///   worked, and a cross-origin client could not see them.
const EXPOSED_HEADERS: [&str; 2] = ["etag", "allow"];

/// Cross-origin policy for [`router_with`].
///
/// Off by default, and deliberately so. Turning this on makes every
/// endpoint readable by any page on the internet that a browser will
/// load — including the ones that still leak gated content — so it stays
/// a decision a host makes explicitly rather than one it inherits.
#[derive(Debug, Clone, Default)]
pub enum Cors {
    /// No CORS headers at all. A browser SPA must be same-origin, or sit
    /// behind a reverse proxy that makes it so.
    #[default]
    Disabled,
    /// Allow exactly these origins. Credentials are permitted, so the
    /// member session cookie can travel — which is why a wildcard is not
    /// offered here.
    Origins(Vec<String>),
    /// Reflect any origin. Development only: combined with credentials
    /// this would let any site read a signed-in member's content, so
    /// credentials are *not* enabled in this mode.
    Any,
}

impl Cors {
    /// Read an allowlist from `RCMS_API_CORS_ORIGINS` — comma-separated,
    /// or the literal `*` for [`Cors::Any`]. Absent or empty is
    /// [`Cors::Disabled`], so an unset environment changes nothing.
    #[must_use]
    pub fn from_env() -> Self {
        Self::parse(crate::config::var("API_CORS_ORIGINS").as_deref())
    }

    /// The parsing half of [`Self::from_env`], split out so it can be tested.
    ///
    /// Process environment is global mutable state, and Rust runs tests
    /// in parallel threads — a test that sets a variable races every
    /// other test that reads one. Keeping the logic pure sidesteps that
    /// instead of papering over it with a mutex.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::Disabled;
        };
        if raw.trim() == "*" {
            return Self::Any;
        }
        let origins: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if origins.is_empty() {
            Self::Disabled
        } else {
            Self::Origins(origins)
        }
    }
}

/// Build the public JSON API router rooted at `/api/v2`, with no CORS.
///
/// Returns an [`axum::Router`] host apps can merge / nest at the root
/// of their application Router.
pub fn router() -> Router {
    router_with(&Cors::Disabled)
}

/// Every path this router serves, in one list.
///
/// Shared with [`openapi`] so the published schema cannot describe a
/// different API than the one running: `router_with` zips this against
/// its handler array, and a length mismatch is a compile error, while
/// `openapi::document` is built from the same constant.
pub const ROUTE_PATHS: [&str; 15] = [
    "/api/v2/pages/",
    // `find/` and `tree/` are static segments, so axum matches them ahead
    // of the `{id}` capture regardless of registration order.
    "/api/v2/pages/find/",
    "/api/v2/pages/tree/",
    "/api/v2/pages/{id}/",
    "/api/v2/images/",
    "/api/v2/images/{id}/",
    "/api/v2/documents/",
    "/api/v2/documents/{id}/",
    // #431 — snippets (the admin "Library" rows).
    "/api/v2/snippets/",
    "/api/v2/snippets/{id}/",
    // #584 — navigation menus (resolved trees) for headless navbars.
    "/api/v2/menus/",
    "/api/v2/menus/{slug}/",
    // Locale discovery: without it a headless client has to hard-code the
    // tenant's language list and cannot tell a typo from a real code,
    // because an unknown one falls back silently.
    "/api/v2/locales/",
    // One search box across pages, images, documents and snippets.
    "/api/v2/search/",
    // The cheap polling tick: what changed, without asking every
    // collection in turn.
    "/api/v2/changes/",
];

/// [`router`], with a cross-origin policy.
///
/// Every route is registered twice, with and without the trailing slash.
/// axum does not redirect between them, so `/api/v2/pages` used to fall
/// through to the host's catch-all and answer a JSON client with the
/// CMS 404 *page*. Accepting both is cheaper than teaching every client
/// a rule it will get wrong once.
pub fn router_with(cors: &Cors) -> Router {
    // Same order as `ROUTE_PATHS`; the array lengths must match, so
    // adding a route without listing its path fails to compile.
    let handlers = [
        get(pages::list),
        get(pages::find),
        get(tree::tree),
        get(pages::detail),
        get(images::list),
        get(images::detail),
        get(documents::list),
        get(documents::detail),
        get(snippets::list),
        get(snippets::detail),
        get(menus::list),
        get(menus::detail),
        get(locales::list),
        get(search::search),
        get(changes::changes),
    ];

    let mut r = Router::new();
    for (path, route) in ROUTE_PATHS.iter().zip(handlers) {
        r = r
            .route(path, route.clone())
            .route(path.trim_end_matches('/'), route);
    }
    // The schema itself, so a client can discover the API without one.
    r = r.route(openapi::PATH, get(openapi::serve));

    // The only write on the whole surface, and the only reason a browser
    // SPA on another origin can read gated content at all: the member
    // session cookie is `SameSite=Lax` and simply is not sent
    // cross-site. Accepts JSON or form encoding.
    r = r.route(auth::LOGIN_PATH, axum::routing::post(auth::login));

    let r = r
        // Inner: fills in the body of a 405 the MethodRouter
        // produced, so a write attempt is refused in JSON like
        // every other failure.
        .layer(axum::middleware::from_fn(error::method_not_allowed_json))
        .layer(axum::middleware::from_fn(http_cache::layer));

    // `POST` is for the member login (`auth::LOGIN_PATH`) and
    // `authorization` for the bearer token it returns — the way a SPA on
    // another origin reads gated content, since the session cookie is
    // `SameSite=Lax` and never rides a cross-site fetch. Without them the
    // browser's preflight refuses both before the request is sent.
    const ALLOWED_METHODS: [&str; 4] = ["GET", "HEAD", "OPTIONS", "POST"];
    const ALLOWED_HEADERS: [&str; 3] = ["content-type", "if-none-match", "authorization"];

    match cors {
        Cors::Disabled => r,
        Cors::Any => r.cors(
            rustango::cors::CorsLayer::new()
                .allow_methods(ALLOWED_METHODS.to_vec())
                .allow_headers(ALLOWED_HEADERS.to_vec())
                .expose_headers(EXPOSED_HEADERS),
        ),
        Cors::Origins(origins) => r.cors(
            rustango::cors::CorsLayer::new()
                .allow_origins(origins.clone())
                .allow_methods(ALLOWED_METHODS.to_vec())
                .allow_headers(ALLOWED_HEADERS.to_vec())
                .expose_headers(EXPOSED_HEADERS)
                // The member session is a cookie, so a cross-origin SPA
                // needs credentials to fetch gated content at all.
                .allow_credentials(true)
                .max_age(std::time::Duration::from_secs(3600)),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_value_leaves_cors_off() {
        // The safety property: deploying this release must not open the
        // API to other origins by itself.
        assert!(matches!(Cors::parse(None), Cors::Disabled));
    }

    #[test]
    fn an_empty_or_whitespace_value_is_also_off() {
        assert!(matches!(Cors::parse(Some("")), Cors::Disabled));
        assert!(matches!(Cors::parse(Some("  , ,")), Cors::Disabled));
    }

    #[test]
    fn a_comma_separated_list_is_parsed_and_trimmed() {
        match Cors::parse(Some("https://a.example.com, https://b.example.com")) {
            Cors::Origins(o) => {
                assert_eq!(o, ["https://a.example.com", "https://b.example.com"]);
            }
            other => panic!("expected an allowlist, got {other:?}"),
        }
    }

    #[test]
    fn a_literal_star_is_the_reflect_any_mode() {
        assert!(matches!(Cors::parse(Some("*")), Cors::Any));
        assert!(matches!(Cors::parse(Some("  *  ")), Cors::Any));
    }

    #[test]
    fn the_default_is_disabled() {
        assert!(matches!(Cors::default(), Cors::Disabled));
    }
}
