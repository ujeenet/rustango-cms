//! Public axum router that the host app mounts.
//!
//! Two flavours, each with a cache-enabled variant:
//!
//! - [`router`] — fallback-style. The CMS resolver is the router's
//!   `.fallback()`, so any unmatched path renders as a page. Simple
//!   and correct **for non-tenancy apps**.
//! - [`router_at`] — explicit-prefix style. Mounts the CMS at a
//!   non-root prefix via `nest(prefix, /{*path})`. Required when
//!   composing with rustango's tenancy admin — see note below.
//!
//! ## Caching
//!
//! Both have `_cached` variants that wrap the router with
//! [`rustango::cache_page::CachePageLayer`] — a tower layer that
//! caches successful GET responses on a `(prefix, method, path,
//! Host, vary-on)` key and emits the matching `Cache-Control` /
//! `Vary` headers. GET-only, 200-only; the layer respects a
//! response's own `Cache-Control: no-store` so individual handlers
//! can opt out.
//!
//! By default entries expire on TTL only, so editors see stale content
//! for up to the TTL after a save. To purge on save, build a
//! [`crate::cache_invalidate::BoxedCacheInvalidator`] over the same
//! cache and pass it to [`crate::admin::router_with_invalidator`]; the
//! admin then drops the affected keys after each page mutation.
//!
//! Per-tenant scoping is automatic via the layer's built-in `Host`-
//! header partitioning. Vary on `cookie` / `accept-language` etc.
//! via the layer's `vary_on(...)` method when building via
//! [`PublicRouter`].
//!
//! ## Don't use [`router`] with `rustango` tenancy
//!
//! Rustango's tenancy [`server::Builder`] used to attach its own
//! admin as `Router::fallback_service(tenant_admin)` on the merged
//! user router. Axum semantics: that override silenced any
//! `.fallback()` set by the user's router. As of rustango 0.31 the
//! tenant admin mounts via explicit routes for `routes.admin_url`
//! only, so [`router`] is allowed to claim site root — see
//! `examples/cms_demo`.
//!
//! [`server::Builder`]: rustango::server::Builder

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use rustango::cache::BoxedCache;
use rustango::cache_page::CachePageLayer;
use rustango::extractors::Tenant;
use tera::Tera;

use crate::locale_mode::{resolve_request_locale, LocaleMode};
use crate::render::{render_negotiated, render_with_route};
use crate::resolver::{resolve_path, resolve_routable};

/// State shared with the page-rendering handler. Carries the Tera
/// instance, URL mount prefix, and the locale-resolution mode
/// (path vs. query). HTTP-level caching is layered onto the
/// router after construction via [`CachePageLayer`]; the handler
/// itself is stateless w.r.t. the cache.
#[derive(Clone)]
pub(crate) struct PublicState {
    pub(crate) tera: Arc<Tera>,
    pub(crate) url_prefix: String,
    locale_mode: LocaleMode,
    /// Optional mailer + from-addr for form-submission notifications.
    /// When `None`, submissions don't email
    /// anyone — the data still lands in `cms_form_entry`.
    pub(crate) mailer: Option<Arc<dyn rustango::email::Mailer>>,
    pub(crate) mailer_from: Arc<String>,
    /// When the router runs a page cache (`_cached` variants),
    /// the same backend + TTL are shared here so the render path can
    /// cache the menu-eligible tree query. `None` for the uncached
    /// variants (the render path then queries live every time).
    pub(crate) fragment_cache: Option<(BoxedCache, Duration)>,
    /// Per-tenant template overrides, when the host configured a
    /// directory for them. `None` — the default — renders every tenant
    /// from the global set, exactly as before.
    pub(crate) tenant_templates: Option<Arc<crate::tenant_templates::TenantTemplates>>,
}

impl PublicState {
    fn new(tera: Arc<Tera>, url_prefix: String) -> Self {
        Self {
            tera,
            url_prefix,
            locale_mode: LocaleMode::default(),
            mailer: None,
            mailer_from: Arc::new(String::new()),
            fragment_cache: None,
            tenant_templates: None,
        }
    }

    /// The Tera this tenant should render with: its own overrides
    /// layered on the global set, or the global set itself.
    ///
    /// A cached instance comes straight back; resolving one walks the
    /// tenant's directory and may rebuild a Tera, so that runs on the
    /// blocking pool rather than a runtime worker.
    pub(crate) async fn tera_for(&self, slug: &str) -> Arc<Tera> {
        let Some(tt) = self.tenant_templates.as_ref() else {
            return Arc::clone(&self.tera);
        };
        if let Some(cached) = tt.cached(slug) {
            return cached;
        }
        let (tt, owned) = (Arc::clone(tt), slug.to_owned());
        rustango::__private_runtime::tokio::task::spawn_blocking(move || tt.for_tenant(&owned))
            .await
            .unwrap_or_else(|_| Arc::clone(&self.tera))
    }
    fn with_fragment_cache(mut self, cache: BoxedCache, ttl: Duration) -> Self {
        self.fragment_cache = Some((cache, ttl));
        self
    }
    fn with_locale_mode(mut self, mode: LocaleMode) -> Self {
        self.locale_mode = mode;
        self
    }
    /// The configured locale-URL mode — read by the sitemap handler to
    /// emit per-locale `<url>` entries + hreflang alternates.
    pub(crate) fn locale_mode(&self) -> LocaleMode {
        self.locale_mode
    }
    fn with_mailer(
        mut self,
        mailer: Arc<dyn rustango::email::Mailer>,
        from: impl Into<String>,
    ) -> Self {
        self.mailer = Some(mailer);
        self.mailer_from = Arc::new(from.into());
        self
    }
}

/// Apply the framework's `CachePageLayer` to `router` with the
/// supplied backend + TTL. Used by the `_cached` shortcuts and
/// `PublicRouter::build` to produce the layered router.
///
/// Varies on `Accept-Language` AND the persisted locale cookie
/// (#i18n): the renderer resolves the active locale from path / `?lang=`
/// (both already in the cache key via path+query) / the `rcms_locale`
/// cookie / `Accept-Language`, so the cache must key on each input or a
/// cached response could be served in the wrong language. Varying on the
/// raw `Cookie` header would fragment the cache by the rotating CSRF
/// token, so a `map_request` layer first distills just the locale cookie
/// into a synthetic `x-rcms-locale` request header and the cache keys on
/// that. (Trade-off: bare-URL responses fragment by chosen locale +
/// header; explicit path/`?lang=` URLs cache per-URL as before. The
/// default un-cached `router()` is unaffected.)
fn apply_cache_layer(router: Router, cache: BoxedCache, ttl: Duration) -> Router {
    router
        .layer(
            CachePageLayer::new(cache)
                .timeout(ttl)
                .key_prefix("rcms:page")
                .vary_on(["accept-language", "x-rcms-locale", "x-rcms-accept"]),
        )
        // Outermost: run before the cache layer so the synthetic headers
        // are present when the cache key is computed.
        .layer(axum::middleware::map_request(inject_locale_cookie_header))
        .layer(axum::middleware::map_request(inject_accept_kind_header))
}

/// Distill `Accept` into a synthetic `x-rcms-accept` request header so
/// the page cache keys on the *negotiated representation*.
///
/// Without this, an anonymous `Accept: application/json` request to an
/// API-view page stores a JSON body under a key an anonymous browser
/// request then reads — cross-representation cache poisoning. (Bounded,
/// since the cache layer skips requests carrying `Cookie`/
/// `Authorization`, but bot / CDN / first-visit traffic is exposed.)
///
/// Deliberately *not* `vary_on(["accept"])`: every browser family sends
/// a different `Accept` string, which would fragment the cache several
/// times over for a feature only a couple of page types use. The header
/// is stamped **only** for an explicit JSON preference, so HTML and
/// wildcard callers keep sharing one key — and every
/// [`crate::page_view::PageViewKind`] resolves those two to the same
/// body, so collapsing them is provably safe.
async fn inject_accept_kind_header(mut req: axum::extract::Request) -> axum::extract::Request {
    let accept = crate::page_view::parse_accept(
        req.headers()
            .get(axum::http::header::ACCEPT)
            .and_then(|v| v.to_str().ok()),
    );
    if accept == crate::page_view::Accept::Json {
        req.headers_mut().insert(
            axum::http::HeaderName::from_static("x-rcms-accept"),
            axum::http::HeaderValue::from_static("json"),
        );
    }
    req
}

/// Distill the `rcms_locale` cookie into an `x-rcms-locale` request
/// header so [`apply_cache_layer`]'s page cache can key on the chosen
/// locale without fragmenting on the whole (CSRF-bearing) `Cookie`
/// header. No-op when the cookie is absent/malformed.
async fn inject_locale_cookie_header(mut req: axum::extract::Request) -> axum::extract::Request {
    let code = crate::locale_mode::locale_cookie_value(
        req.headers()
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    );
    if let Some(code) = code {
        if let Ok(hv) = axum::http::HeaderValue::from_str(&code) {
            req.headers_mut()
                .insert(axum::http::HeaderName::from_static("x-rcms-locale"), hv);
        }
    }
    req
}

/// Builder for the public CMS router. Use when you need to combine
/// mount-prefix, response caching, and/or path-based locale resolution
/// — the free [`router`] / [`router_at`] / `*_cached` functions are
/// shortcuts for common combinations and don't expose every knob.
///
/// ```no_run
/// use std::sync::Arc;
/// use std::time::Duration;
/// use rustango_cms::{LocaleMode, PublicRouter};
/// # let tera = Arc::new(tera::Tera::default());
/// # let cache: rustango::cache::BoxedCache = Arc::new(rustango::cache::NullCache);
/// let router = PublicRouter::new(tera)
///     .at("/p")
///     .cached(cache, Duration::from_secs(60))
///     .locale_mode(LocaleMode::PathOrQuery)
///     .build();
/// ```
pub struct PublicRouter {
    tera: Arc<Tera>,
    prefix: String,
    cache: Option<(BoxedCache, Duration)>,
    locale_mode: LocaleMode,
    mailer: Option<(Arc<dyn rustango::email::Mailer>, String)>,
    /// Pairs of `(URL prefix, filesystem root)` to mount as
    /// static-file handlers. Stacked: every entry becomes a route
    /// taking a wildcard path.
    static_dirs: Vec<(String, std::path::PathBuf)>,
    /// Single-file entries baked from `include_bytes!`. Each
    /// triple is `(URL path, bytes, content-type)`.
    static_files: Vec<(String, &'static [u8], &'static str)>,
    /// Directory holding one subdirectory of template overrides per
    /// tenant slug. `None` renders every tenant from the global set.
    tenant_templates: Option<Arc<crate::tenant_templates::TenantTemplates>>,
}

impl PublicRouter {
    /// Start a builder bound to the given Tera instance, mounted at
    /// the root with no cache and query-mode locale resolution.
    #[must_use]
    pub fn new(tera: Arc<Tera>) -> Self {
        Self {
            tera,
            prefix: String::new(),
            cache: None,
            locale_mode: LocaleMode::default(),
            mailer: None,
            static_dirs: Vec::new(),
            static_files: Vec::new(),
            tenant_templates: None,
        }
    }

    /// Mount a filesystem directory as a static-file server at
    /// `url_prefix`. Files under `fs_root` become reachable at
    /// `{url_prefix}/{relative path}`. `fs_root` is anchored to the
    /// process's current working directory (matches what a host's
    /// `Path::new("./static")` would resolve to).
    ///
    /// Stacks: call multiple times to expose multiple dirs (e.g.
    /// `/static/` for app assets + `/admin-static/` for shared
    /// chrome).
    ///
    /// # Path-traversal safety
    /// The handler rejects any request whose normalized path
    /// escapes `fs_root` — `../` segments, absolute paths, and
    /// embedded `..` after URL-decode all 404. Files outside the
    /// root are never served.
    #[must_use]
    pub fn static_dir(
        mut self,
        url_prefix: impl Into<String>,
        fs_root: impl Into<std::path::PathBuf>,
    ) -> Self {
        self.static_dirs.push((url_prefix.into(), fs_root.into()));
        self
    }

    /// Serve a single static file from baked bytes. Pair with
    /// `include_bytes!("…")` to embed asset bytes directly in the
    /// binary — useful for `favicon.ico`, a small logo, or a brand
    /// stylesheet that ships with the host.
    #[must_use]
    pub fn static_file(
        mut self,
        url_path: impl Into<String>,
        bytes: &'static [u8],
        content_type: &'static str,
    ) -> Self {
        self.static_files
            .push((url_path.into(), bytes, content_type));
        self
    }

    /// Wire a mailer for form-submission notifications.
    /// Each \`cms_form_settings.notify_emails\` recipient gets a copy
    /// of every submission to that page. `from_addr` is the `From:`
    /// stamped on every notification.
    #[must_use]
    pub fn with_form_mailer(
        mut self,
        mailer: Arc<dyn rustango::email::Mailer>,
        from_addr: impl Into<String>,
    ) -> Self {
        self.mailer = Some((mailer, from_addr.into()));
        self
    }

    /// Mount the CMS under `prefix` instead of the root. Same shape
    /// as [`router_at`].
    #[must_use]
    pub fn at(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Layer [`rustango::cache_page::CachePageLayer`] onto the built
    /// router. `cache` is the storage backend (any
    /// [`rustango::cache::BoxedCache`] — `InMemoryCache`,
    /// `RedisCache`, …); `ttl` is the per-entry expiration. Cache
    /// invalidation is TTL-based — admin edits don't surgically drop
    /// affected keys; editors see stale content for up to `ttl`
    /// seconds after a save.
    #[must_use]
    pub fn cached(mut self, cache: BoxedCache, ttl: Duration) -> Self {
        self.cache = Some((cache, ttl));
        self
    }

    /// Pick how the router recognizes the request's locale code.
    /// See [`LocaleMode`] for the trade-offs.
    #[must_use]
    pub fn locale_mode(mut self, mode: LocaleMode) -> Self {
        self.locale_mode = mode;
        self
    }

    /// Let each tenant override templates from its own directory.
    ///
    /// `root` holds one subdirectory per tenant slug; a name found there
    /// wins over the global set and anything absent falls through, so a
    /// tenant only stores what differs. Keep `root` **outside** the glob
    /// this router's Tera was built from, or every tenant's files land in
    /// every instance.
    ///
    /// Templates become deploy artifacts rather than code: dropping a
    /// file changes a site with no rebuild, and — depending on
    /// [`Reload`](crate::tenant_templates::Reload) — no restart either.
    ///
    /// ```no_run
    /// # use std::sync::Arc; use tera::Tera;
    /// # use rustango_cms::{PublicRouter, tenant_templates::{TenantTemplates, Reload}};
    /// # let tera = Arc::new(Tera::default());
    /// let overrides = TenantTemplates::new(Arc::clone(&tera), "./templates_tenants")
    ///     .with_reload(Reload::Every(std::time::Duration::from_secs(30)));
    /// let router = PublicRouter::new(tera).tenant_templates(overrides).build();
    /// ```
    #[must_use]
    pub fn tenant_templates(mut self, tt: crate::tenant_templates::TenantTemplates) -> Self {
        self.tenant_templates = Some(Arc::new(tt));
        self
    }

    /// Like [`tenant_templates`](Self::tenant_templates) but takes an
    /// already-shared instance, so the same one can also be handed to
    /// [`crate::tenant_templates::install`] for the admin editor —
    /// otherwise the editor would be writing files a *different* cache
    /// is serving from.
    #[must_use]
    pub fn tenant_templates_shared(
        mut self,
        tt: Arc<crate::tenant_templates::TenantTemplates>,
    ) -> Self {
        self.tenant_templates = Some(tt);
        self
    }

    /// Finalize the builder into an axum [`Router`].
    #[must_use]
    pub fn build(self) -> Router {
        let mut state =
            PublicState::new(self.tera, self.prefix.clone()).with_locale_mode(self.locale_mode);
        state.tenant_templates = self.tenant_templates;
        if let Some((mailer, from)) = self.mailer {
            state = state.with_mailer(mailer, from);
        }
        // #316 — share the page-cache backend with the render path so
        // the menu-tree query is cached too. Cloned before the layer
        // below consumes `self.cache` (BoxedCache is an Arc; cheap).
        if let Some((cache, ttl)) = &self.cache {
            state = state.with_fragment_cache(cache.clone(), *ttl);
        }
        let cms_router = if self.prefix.is_empty() {
            fallback_router(state)
        } else {
            explicit_router(&self.prefix, state)
        };
        let cms_router = if let Some((cache, ttl)) = self.cache {
            apply_cache_layer(cms_router, cache, ttl)
        } else {
            cms_router
        };

        // #254 — stack static routes BEFORE the CMS fallback so a
        // hit on `/static/css/styles.css` doesn't fall through to
        // the slug resolver. Each `static_dir` becomes a wildcard
        // route `{prefix}/{*path}`; each `static_file` is an exact
        // GET route.
        let mut router = Router::new();
        for (url_prefix, fs_root) in self.static_dirs {
            let trimmed = url_prefix.trim_end_matches('/').to_owned();
            let pattern = format!("{trimmed}/{{*path}}");
            let root = Arc::new(fs_root);
            router = router.route(
                &pattern,
                axum::routing::get({
                    let root = root.clone();
                    move |path: axum::extract::Path<String>| {
                        let root = root.clone();
                        // canonicalize + read are blocking file I/O; a
                        // large asset must not hold a runtime worker (#727).
                        async move {
                            rustango::__private_runtime::tokio::task::spawn_blocking(move || {
                                serve_static_path(&root, &path.0)
                            })
                            .await
                            .unwrap_or_else(|_| not_found())
                        }
                    }
                }),
            );
        }
        for (url_path, bytes, ct) in self.static_files {
            router = router.route(
                &url_path,
                axum::routing::get(move || async move { serve_static_bytes(bytes, ct) }),
            );
        }
        router.merge(cms_router)
    }
}

/// Serve a single file relative to `fs_root`. Rejects `..` traversal
/// + absolute paths + symlinks that escape the root. Returns 404 on
/// any miss (file absent, traversal attempt, IO error).
fn serve_static_path(fs_root: &std::path::Path, requested: &str) -> Response {
    // Reject anything that looks like a traversal attempt up front —
    // URL-decoded `..` segments, absolute paths, embedded NULs. The
    // canonicalize step below is the real defense, but bailing early
    // keeps the error consistent across platforms.
    if requested.is_empty() || requested.contains('\0') {
        return not_found();
    }
    for seg in requested.split('/') {
        if seg == ".." || seg.starts_with('/') {
            return not_found();
        }
    }
    let candidate = fs_root.join(requested);
    // Canonicalize both root + candidate, then confirm the candidate
    // is INSIDE the root. Symlink-escape lands on the wrong side of
    // the prefix check and 404s.
    let canon_root = match fs_root.canonicalize() {
        Ok(p) => p,
        Err(_) => return not_found(),
    };
    let canon = match candidate.canonicalize() {
        Ok(p) => p,
        Err(_) => return not_found(),
    };
    if !canon.starts_with(&canon_root) {
        return not_found();
    }
    let bytes = match std::fs::read(&canon) {
        Ok(b) => b,
        Err(_) => return not_found(),
    };
    let ct = mime_guess::from_path(&canon)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();
    static_response(bytes, &ct)
}

fn serve_static_bytes(bytes: &'static [u8], content_type: &'static str) -> Response {
    static_response(bytes.to_vec(), content_type)
}

fn static_response(bytes: Vec<u8>, content_type: &str) -> Response {
    use axum::http::header;
    // 1 hour cache by default — shorter than image renditions
    // (immutable, content-addressed) since static assets at a
    // stable URL change with deploys, not with content hashes.
    (
        axum::http::StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_str(content_type).unwrap_or_else(|_| {
                    axum::http::HeaderValue::from_static("application/octet-stream")
                }),
            ),
            (
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("public, max-age=3600"),
            ),
        ],
        bytes,
    )
        .into_response()
}

fn not_found() -> Response {
    (axum::http::StatusCode::NOT_FOUND, "Not Found").into_response()
}

/// Build the CMS router using axum's `.fallback()` for page
/// rendering. Use only when the CMS is the sole surface — see the
/// module docs for the tenancy gotcha.
pub fn router(tera: Arc<Tera>) -> Router {
    fallback_router(PublicState::new(tera, String::new()))
}

/// Like [`router`], wrapped in [`rustango::cache_page::CachePageLayer`].
/// GET responses are cached for `ttl` keyed on
/// `(method, path, Host, vary-on)`; admin edits propagate after the
/// TTL elapses (no manual invalidation).
pub fn router_cached(tera: Arc<Tera>, cache: BoxedCache, ttl: Duration) -> Router {
    apply_cache_layer(
        fallback_router(PublicState::new(tera, String::new())),
        cache,
        ttl,
    )
}

/// Build the CMS router mounted under an explicit URL prefix.
///
/// Registers `{prefix}` + `{prefix}/{*path}` as **explicit** routes
/// (not a fallback), which survives a downstream
/// `Router::fallback_service(...)` such as the one rustango's
/// tenancy admin attaches.
///
/// Pass `"/p"`, `"/site"`, `"/pages"`, etc. Passing `""` (empty
/// prefix) is allowed and mounts the wildcard at the root.
///
/// The prefix is exposed to templates via the `url_prefix` Tera
/// variable so breadcrumb / sibling links can be built without
/// hard-coding the host's URL layout.
pub fn router_at(prefix: &str, tera: Arc<Tera>) -> Router {
    explicit_router(prefix, PublicState::new(tera, prefix.to_owned()))
}

/// Like [`router_at`], wrapped in [`rustango::cache_page::CachePageLayer`].
/// See [`router_cached`] for the trade-off (TTL-based invalidation).
pub fn router_at_cached(prefix: &str, tera: Arc<Tera>, cache: BoxedCache, ttl: Duration) -> Router {
    apply_cache_layer(
        explicit_router(prefix, PublicState::new(tera, prefix.to_owned())),
        cache,
        ttl,
    )
}

fn fallback_router(state: PublicState) -> Router {
    crate::perf::instrument(fallback_router_inner(state))
}

fn fallback_router_inner(state: PublicState) -> Router {
    Router::new()
        .route("/sitemap.xml", get(crate::sitemap::handle_sitemap))
        // Advertises the sitemap above and keeps crawlers out of
        // the admin. Only honoured at the origin root by crawlers,
        // but mounted alongside the sitemap in both routers so the
        // two never disagree about where they exist.
        .route("/robots.txt", get(crate::robots::handle_robots))
        // #187 — sitemap shards for tenants > 50k URLs.
        .route("/sitemap/{n}", get(crate::sitemap::handle_sitemap_shard))
        .route("/feed/{kind}/rss.xml", get(crate::feed::handle_feed_rss))
        .route("/feed/{kind}/atom.xml", get(crate::feed::handle_feed_atom))
        // #76 — password-prompt page for view-restricted pages.
        .route(
            "/__cms-view-password",
            get(handle_view_password_form).post(handle_view_password_submit),
        )
        // #544 FB-11 — public form-submission endpoint.
        .route(
            "/forms/submit/{form_id}",
            axum::routing::post(crate::forms::submit::handle_form_submit)
                .layer(axum::extract::DefaultBodyLimit::max(crate::forms::submit::MAX_SUBMISSION_BYTES)),
        )
        // #543 FB-10 — public form runtime (paging, validation, conditional logic).
        .route("/forms/runtime.js", get(serve_form_runtime_js))
        // standard form styles (auto-linked; replaceable per-form).
        .route("/forms/forms.css", get(serve_form_css))
        // default analytics — CDN-/cache-proof collection.
        .route(
            crate::analytics::COLLECT_PATH,
            axum::routing::post(crate::analytics::collect::handle_collect),
        )
        .route(
            crate::analytics::WS_PATH,
            get(crate::analytics::ws::handle_ws),
        )
        .route(
            crate::analytics::BEACON_JS_PATH,
            get(crate::analytics::serve_beacon_js),
        )
        .fallback(get(handle_page_request))
        // #689 — seed a tenant provisioned after boot on its first request.
        .layer(axum::middleware::from_fn(crate::seed::lazy_seed_layer))
        .with_state(state)
}

/// Serve the cacheable public form runtime.
async fn serve_form_runtime_js() -> impl IntoResponse {
    // Unversioned public URL: the 1-hour static policy, not immutable (#696).
    serve_static_bytes(include_str!("forms/runtime.js").as_bytes(), "text/javascript; charset=utf-8")
}

/// Serve the standard, replaceable public form stylesheet.
async fn serve_form_css() -> impl IntoResponse {
    serve_static_bytes(include_str!("forms/forms.css").as_bytes(), "text/css; charset=utf-8")
}

fn explicit_router(prefix: &str, state: PublicState) -> Router {
    let inner = Router::new()
        .route("/sitemap.xml", get(crate::sitemap::handle_sitemap))
        // Advertises the sitemap above and keeps crawlers out of
        // the admin. Only honoured at the origin root by crawlers,
        // but mounted alongside the sitemap in both routers so the
        // two never disagree about where they exist.
        .route("/robots.txt", get(crate::robots::handle_robots))
        // #187 — sitemap shards for tenants > 50k URLs.
        .route("/sitemap/{n}", get(crate::sitemap::handle_sitemap_shard))
        .route("/feed/{kind}/rss.xml", get(crate::feed::handle_feed_rss))
        .route("/feed/{kind}/atom.xml", get(crate::feed::handle_feed_atom))
        .route(
            "/__cms-view-password",
            get(handle_view_password_form).post(handle_view_password_submit),
        )
        // #544 FB-11 — public form-submission endpoint.
        .route(
            "/forms/submit/{form_id}",
            axum::routing::post(crate::forms::submit::handle_form_submit)
                .layer(axum::extract::DefaultBodyLimit::max(crate::forms::submit::MAX_SUBMISSION_BYTES)),
        )
        // #543 FB-10 — public form runtime.
        .route("/forms/runtime.js", get(serve_form_runtime_js))
        .route("/forms/forms.css", get(serve_form_css))
        // default analytics — CDN-/cache-proof collection.
        .route(
            crate::analytics::COLLECT_PATH,
            axum::routing::post(crate::analytics::collect::handle_collect),
        )
        .route(
            crate::analytics::WS_PATH,
            get(crate::analytics::ws::handle_ws),
        )
        .route(
            crate::analytics::BEACON_JS_PATH,
            get(crate::analytics::serve_beacon_js),
        )
        .route("/", get(handle_page_request))
        .route("/{*path}", get(handle_page_request))
        .layer(axum::middleware::from_fn(crate::seed::lazy_seed_layer))
        .with_state(state);
    if prefix.is_empty() {
        return inner;
    }
    let trailing = format!("{prefix}/");
    let bare = prefix.to_owned();
    Router::new().nest(prefix, inner).route(
        &trailing,
        get(move || {
            let bare = bare.clone();
            async move { Redirect::permanent(&bare) }
        }),
    )
}

/// The request's bare hostname, lowercased and without the port.
pub(crate) fn host_header(headers: &axum::http::HeaderMap) -> &str {
    headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map_or("", |h| h.split(':').next().unwrap_or(""))
}

async fn handle_page_request(
    tenant: Tenant,
    State(state): State<PublicState>,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    rustango::tenancy::member_auth::CurrentMember(member): rustango::tenancy::member_auth::CurrentMember,
    headers: axum::http::HeaderMap,
    uri: Uri,
) -> Response {
    // #members — the page-gating viewer is a public member (member
    // cookie) OR an admin (tenant-session cookie); a member session
    // takes precedence on public pages. Both are `Option<User>` from
    // the same `rustango_users` table, so restriction checks (login /
    // groups / permission) work identically for either.
    let viewer = member.as_ref().or(session_user.as_ref());
    let raw_path = uri.path();
    // #553 — override-live redirect rules beat page resolution. The rule
    // set is fragment-cached (and usually empty), so page views on
    // rule-free tenants stay one-query — and none of the locale/page work
    // below runs on a hit.
    let _perf_t = crate::perf::start();
    if let Some(resp) = crate::redirect::serve_override(
        &tenant,
        state.fragment_cache.as_ref().map(|(c, ttl)| (c, *ttl)),
        raw_path,
        uri.query(),
    )
    .await
    {
        crate::perf::mark("redirect_rules", _perf_t);
        return resp;
    }
    crate::perf::mark("redirect_rules", _perf_t);
    // Locale resolved from path / query per the
    // configured mode. The resolver returns a stripped path so the
    // canonical url_path used for the page lookup never carries the
    // locale prefix.
    let accept_language = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok());
    // Media-type preference for page types that serve a JSON view.
    // Parsed once here and passed down; `render_inner` is the only thing
    // that reads it, and only for those types.
    let accept = crate::page_view::parse_accept(
        headers
            .get(axum::http::header::ACCEPT)
            .and_then(|v| v.to_str().ok()),
    );
    // #i18n — persistence cookie keeps the chosen locale across links
    // that don't carry it (bare nav paths, in-content links).
    let cookie_code = crate::locale_mode::locale_cookie_value(
        headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    );
    let _perf_t = crate::perf::start();
    let request_locale = resolve_request_locale(
        tenant.pool(),
        state.locale_mode,
        raw_path,
        uri.query(),
        cookie_code.as_deref(),
        accept_language,
    )
    .await;
    crate::perf::mark("locale", _perf_t);
    let locale_code = request_locale.code.clone();
    let locale_explicit = request_locale.explicit;
    let lookup_path = request_locale.stripped_path;

    // Multi-site: which root does this hostname serve? With no `cms_site`
    // rows this resolves to the conventional root and contributes an empty
    // prefix, so the lookup below is byte-identical to what it always was.
    let _perf_t = crate::perf::start();
    let site_prefix = crate::site::prefix_for_host(tenant.pool(), host_header(&headers))
        .await
        .unwrap_or_default();
    crate::perf::mark("site", _perf_t);
    // Stored paths are tenant-absolute; the visitor's path is relative to
    // the site root.
    let lookup_path = crate::site::to_lookup_path(&site_prefix, &lookup_path);

    let _perf_t = crate::perf::start();
    let _resolved = resolve_path(&tenant, &lookup_path).await;
    crate::perf::mark("resolve_path", _perf_t);
    match _resolved {
        Ok(Some(page)) => {
            // #76 — per-page view restrictions. Walks ancestors for the
            // nearest restriction; if found, enforces login / groups /
            // password before delegating to the renderer. The
            // unrestricted path stays a single resolve + render.
            let _perf_t = crate::perf::start();
            let _blocked =
                crate::view_restriction_guard::enforce(&tenant, &page, viewer, &headers, raw_path)
                    .await;
            crate::perf::mark("view_restriction", _perf_t);
            if let Some(blocked) = _blocked {
                return styled_denial(&tenant, &state, blocked, locale_code.as_deref()).await;
            }
            // #107 — plugin `before_serve_page` hook. First handler
            // to return `Some(Response)` short-circuits the renderer.
            if let Some(resp) = crate::hooks::fire_before_serve_page(&page, &headers) {
                return resp;
            }
            // #401 — stash the per-page language-switcher data for the
            // `language_switcher()` Tera fn. render_inner re-installs it
            // in its await-free block (see stash_pending) so the
            // thread-local lands on the thread that runs `tera.render`;
            // this guard clears the stash after render regardless.
            let _perf_t = crate::perf::start();
            // Per-tenant overrides layered on the global set. Resolved
            // here rather than inside render so the render path keeps
            // taking a plain `&Tera` and stays testable without a cache.
            let tera = state.tera_for(&tenant.org.slug).await;
            // After `tera_for`: the stash is per-thread, and an `.await`
            // between it and the render can move the task to another
            // thread, stranding the entries for some other page's render.
            let _switcher =
                install_locale_switcher(&tenant, &state, &page, locale_code.as_deref()).await;
            // `site_origin` for absolute canonical / og:image URLs. Stashed
            // here, with no `.await` before the render reads it.
            let _origin = crate::meta_tags::stash_site_origin(crate::sitemap::base_url(&headers));
            let resp = render_negotiated(
                &tenant,
                &tera,
                &page,
                &state.url_prefix,
                &site_prefix,
                locale_code.as_deref(),
                viewer,
                accept,
                state.fragment_cache.as_ref().map(|(c, ttl)| (c, *ttl)),
            )
            .await;
            crate::perf::mark("render", _perf_t);
            // Substitute the CMS-editable 500 page for render failures
            // (the plain-text diagnostics are already tracing::error!'d
            // inside render; visitors get the styled page instead).
            let resp = if resp.status().is_server_error() {
                crate::error_pages::respond(&tenant, &state, resp.status(), locale_code.as_deref())
                    .await
            } else {
                resp
            };
            persist_locale(resp, locale_explicit, locale_code.as_deref())
        }
        Ok(None) => {
            // #198 — routable_page: before falling through to
            // redirects / 404, ask each ancestor's handler if it
            // owns a route pattern that matches the suffix. First
            // match wins; the renderer threads `route_name` +
            // `route_captures` into the Tera ctx.
            match resolve_routable(&tenant, &lookup_path).await {
                Ok(Some((page, route_match))) => {
                    if let Some(blocked) = crate::view_restriction_guard::enforce(
                        &tenant, &page, viewer, &headers, raw_path,
                    )
                    .await
                    {
                        return styled_denial(&tenant, &state, blocked, locale_code.as_deref()).await;
                    }
                    if let Some(resp) = crate::hooks::fire_before_serve_page(&page, &headers) {
                        return resp;
                    }
                    let tera = state.tera_for(&tenant.org.slug).await;
                    let _switcher =
                        install_locale_switcher(&tenant, &state, &page, locale_code.as_deref())
                            .await;
                    let _origin =
                        crate::meta_tags::stash_site_origin(crate::sitemap::base_url(&headers));
                    let resp = render_with_route(
                        &tenant,
                        &tera,
                        &page,
                        &state.url_prefix,
                        &site_prefix,
                        locale_code.as_deref(),
                        viewer,
                        &route_match,
                        accept,
                        state.fragment_cache.as_ref().map(|(c, ttl)| (c, *ttl)),
                    )
                    .await;
                    let resp = if resp.status().is_server_error() {
                        crate::error_pages::respond(
                            &tenant,
                            &state,
                            resp.status(),
                            locale_code.as_deref(),
                        )
                        .await
                    } else {
                        resp
                    };
                    return persist_locale(resp, locale_explicit, locale_code.as_deref());
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(path = %raw_path, error = %e, "resolve_routable failed");
                }
            }
            fallthrough_to_redirect(
                &tenant,
                &state,
                raw_path,
                uri.query(),
                locale_code.as_deref(),
            )
            .await
        }
        Err(e) => {
            tracing::error!(path = %raw_path, error = %e, "resolve_path failed");
            crate::error_pages::respond(
                &tenant,
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                locale_code.as_deref(),
            )
            .await
        }
    }
}

/// Attach locale-persistence headers to a page response. `Vary: Cookie`
/// always (the rendered language depends on the `rcms_locale` cookie, so
/// a shared/CDN cache must key on it); a `Set-Cookie` writing the chosen
/// locale only when it was an EXPLICIT pick (`?lang=` / `/<code>/`), so a
/// plain nav click — which resolves the locale *from* the cookie — never
/// rewrites it. An explicit switch to the default locale still writes the
/// cookie, overriding a stale non-default one.
fn persist_locale(mut resp: Response, explicit: bool, code: Option<&str>) -> Response {
    use axum::http::header::{HeaderValue, SET_COOKIE, VARY};
    resp.headers_mut()
        .append(VARY, HeaderValue::from_static("Cookie"));
    if explicit {
        if let Some(code) = code {
            let cookie = format!(
                "{}={code}; Path=/; SameSite=Lax; HttpOnly; Max-Age=31536000",
                crate::locale_mode::LOCALE_COOKIE
            );
            if let Ok(hv) = HeaderValue::from_str(&cookie) {
                resp.headers_mut().append(SET_COOKIE, hv);
            }
        }
    }
    resp
}

/// A signed-in visitor who may not see a restricted page gets the site's
/// 403 page (an editor-made error page, else the styled fallback) instead
/// of a bare text body; login and password redirects pass through.
async fn styled_denial(
    tenant: &Tenant,
    state: &PublicState,
    blocked: Response,
    locale_code: Option<&str>,
) -> Response {
    if blocked.status() == StatusCode::FORBIDDEN {
        crate::error_pages::respond(tenant, state, StatusCode::FORBIDDEN, locale_code).await
    } else {
        blocked
    }
}

/// Fetch the active locales and stash the per-page language
/// switcher (the URL to *this* page in each locale, built per the
/// configured `LocaleMode`) for the `language_switcher()` Tera fn. The
/// stash is consumed and re-installed on the render thread inside
/// `render_inner`'s await-free block (so the thread-local survives
/// tokio's thread-hops across render's awaits); the returned guard
/// clears any un-consumed stash when it drops, so the caller must hold
/// it across the `render*` call. The final `stash_pending` runs after
/// this fn's only `.await`, and the caller polls the render future with
/// no intervening `.await`, so render's synchronous entry sees it.
pub(crate) async fn install_locale_switcher(
    tenant: &Tenant,
    state: &PublicState,
    page: &crate::page::Page,
    current_code: Option<&str>,
) -> crate::language_switcher::PendingGuard {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let locales: Vec<crate::locale::Locale> = crate::locale::Locale::objects()
        .where_(crate::locale::Locale::active.eq(true))
        .order_by(&[("sort_order", false)])
        .fetch(tenant.pool())
        .await
        .unwrap_or_default();
    let entries = crate::language_switcher::build(
        &locales,
        &state.url_prefix,
        &page.url_path,
        state.locale_mode,
        current_code,
    );
    crate::language_switcher::stash_pending(entries)
}

/// When the slug resolver returns `None` (no published page at this
/// URL), consult the tenant's `cms_redirect` table before giving
/// up. Redirects hook into the 404 path: live pages always win, redirects fill in for
/// retired URLs.
///
/// On a redirect hit, the framework's
/// [`rustango::redirects::build_redirect_response`] emits the
/// 301/302 with `Location` carrying any incoming query string. Hit
/// count is bumped best-effort in the background; failure to
/// increment is logged and swallowed.
async fn fallthrough_to_redirect(
    tenant: &Tenant,
    state: &PublicState,
    raw_path: &str,
    query: Option<&str>,
    locale_code: Option<&str>,
) -> Response {
    match crate::redirect::find_for_path(tenant.pool(), raw_path).await {
        Ok(Some(row)) => {
            // #553 — a `to_page_id` destination resolves to the page's
            // CURRENT url_path (follows moves); `to_path` is the fallback.
            let to = crate::redirect::resolve_destination(tenant.pool(), &row).await;
            let rule = rustango::redirects::RedirectRule {
                to,
                permanent: row.is_permanent,
            };
            let resp = rustango::redirects::build_redirect_response(&rule, query);
            // #555 — record the hit in-memory (no per-request DB write);
            // the throttled flush persists the delta + last_hit_at.
            crate::redirect::record_hit(
                tenant.pool(),
                &tenant.org.slug,
                row.id.get().copied().unwrap_or_default(),
            );
            resp
        }
        // No exact rule, no live page — try #554 wildcard rules before
        // giving up. Non-override wildcards fill in for URLs with no
        // live page, exactly like exact non-override rules.
        Ok(None) => {
            let cache = state.fragment_cache.as_ref().map(|(c, ttl)| (c, *ttl));
            if let Some(resp) =
                crate::redirect::serve_wildcard(tenant, cache, raw_path, query).await
            {
                return resp;
            }
            crate::error_pages::respond(tenant, state, StatusCode::NOT_FOUND, locale_code).await
        }
        Err(e) => {
            tracing::warn!(path = %raw_path, error = %e, "redirect lookup failed");
            crate::error_pages::respond(tenant, state, StatusCode::NOT_FOUND, locale_code).await
        }
    }
}

// =====================================================================
// #76 — view-restriction password prompt.
// =====================================================================

#[derive(Debug, serde::Deserialize)]
struct PasswordPromptQuery {
    page: i64,
    #[serde(default)]
    next: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct PasswordSubmitForm {
    page: i64,
    password: String,
    #[serde(default)]
    next: Option<String>,
}

async fn handle_view_password_form(
    tenant: Tenant,
    State(state): State<PublicState>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<PasswordPromptQuery>,
) -> Response {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let _ = &tenant;
    let page_rows: Vec<crate::page::Page> = crate::page::Page::objects()
        .where_(crate::page::Page::id.eq(q.page))
        .fetch(tenant.pool())
        .await
        .unwrap_or_default();
    let title = page_rows
        .first()
        .map(|p| p.title.clone())
        .unwrap_or_else(|| "Protected page".to_owned());
    let next = q.next.clone().unwrap_or_else(|| "/".to_owned());
    let error_msg = q.error.as_deref().unwrap_or("");
    let mut ctx = tera::Context::new();
    ctx.insert("page_id", &q.page);
    ctx.insert("page_title", &title);
    ctx.insert("next_path", &next);
    ctx.insert("error_message", &error_msg);
    // Seed the double-submit token on the GET so a host CSRF layer does not
    // reject the first POST (#677, same shape as the framework login).
    let (csrf_token, csrf_cookie) =
        rustango::forms::csrf::ensure_token(&headers, rustango::forms::csrf::CSRF_COOKIE);
    ctx.insert("csrf_token", &csrf_token);
    match state.tera.render("rcms/view_password.html", &ctx) {
        Ok(body) => {
            let mut resp = axum::response::Html(body).into_response();
            if let Some(v) = csrf_cookie.and_then(|c| axum::http::HeaderValue::from_str(&c).ok()) {
                resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
            }
            resp
        }
        Err(e) => {
            tracing::error!(error = %e, "view-password template render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "template error").into_response()
        }
    }
}

async fn handle_view_password_submit(
    tenant: Tenant,
    axum::extract::Form(form): axum::extract::Form<PasswordSubmitForm>,
) -> Response {
    let Some(restriction) = crate::view_restriction::direct_for_page(tenant.pool(), form.page)
        .await
        .unwrap_or(None)
        .filter(|r| r.kind == crate::view_restriction::RestrictionKind::Password.as_str())
    else {
        return (
            StatusCode::BAD_REQUEST,
            "no password restriction on this page",
        )
            .into_response();
    };
    let _ = &tenant;
    let _ = &restriction.password_hash;
    // Verify the password against the stored argon2id hash.
    let ok =
        crate::passwords::verify(&form.password, &restriction.password_hash).await.unwrap_or(false);
    // A local path only — this form is reachable from any link (#728).
    let next = form
        .next
        .as_deref()
        .and_then(rustango::auth_decorators::safe_next)
        .unwrap_or_else(|| "/".to_owned());
    if !ok {
        let next_enc = crate::view_restriction_guard::url_encode_path(&next);
        return Redirect::to(&format!(
            "/__cms-view-password?page={}&next={next_enc}&error=Wrong+password",
            form.page
        ))
        .into_response();
    }
    // 7-day grant, expired on the server too; rotate the password to
    // revoke early.
    let cookie =
        crate::view_restriction_guard::grant_cookie(&tenant.org.slug, &restriction, form.page);
    let mut response = Redirect::to(&next).into_response();
    if let Ok(hv) = axum::http::HeaderValue::from_str(&cookie) {
        response
            .headers_mut()
            .append(axum::http::header::SET_COOKIE, hv);
    }
    response
}

#[cfg(test)]
mod static_serve_tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Make a temporary directory for a single test. Files live
    /// under `target/tmp/<name>/` to avoid colliding across parallel
    /// runs while still being predictable to clean up.
    fn tempdir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rustango_cms_static_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("create tempdir");
        p
    }

    #[test]
    fn serves_existing_file_with_guessed_mime() {
        let dir = tempdir("hit");
        fs::write(dir.join("styles.css"), b"body{color:red}").unwrap();
        let resp = serve_static_path(&dir, "styles.css");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.starts_with("text/css"), "got `{ct}`");
        let cc = resp
            .headers()
            .get(axum::http::header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cc.contains("max-age=3600"));
    }

    #[test]
    fn rejects_dotdot_traversal_in_segment() {
        let dir = tempdir("trav");
        fs::write(dir.join("ok.txt"), b"in-root").unwrap();
        // `..` segment — must 404 even if the file outside exists.
        let resp = serve_static_path(&dir, "../etc/passwd");
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn rejects_absolute_segment() {
        let dir = tempdir("abs");
        // Leading slash in a segment shouldn't escape — handler bails.
        let resp = serve_static_path(&dir, "/etc/passwd");
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn rejects_null_byte_in_path() {
        let dir = tempdir("null");
        let resp = serve_static_path(&dir, "ok\0.txt");
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn missing_file_is_404_not_500() {
        let dir = tempdir("miss");
        let resp = serve_static_path(&dir, "does-not-exist.css");
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn serves_static_bytes_with_supplied_content_type() {
        let resp = serve_static_bytes(b"<svg/>", "image/svg+xml");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("image/svg+xml"),
        );
    }

    #[test]
    fn nested_subdir_is_served() {
        let dir = tempdir("nested");
        fs::create_dir_all(dir.join("css/sub")).unwrap();
        fs::write(dir.join("css/sub/x.css"), b"x{}").unwrap();
        let resp = serve_static_path(&dir, "css/sub/x.css");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[test]
    fn empty_path_is_404() {
        let dir = tempdir("empty");
        let resp = serve_static_path(&dir, "");
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }
}

#[cfg(test)]
mod accept_cache_key_tests {
    use super::*;
    use axum::http::{HeaderValue, Request};

    fn req_with_accept(accept: Option<&str>) -> axum::extract::Request {
        let mut b = Request::builder().uri("/some-page");
        if let Some(a) = accept {
            b = b.header(
                axum::http::header::ACCEPT,
                HeaderValue::from_str(a).unwrap(),
            );
        }
        b.body(axum::body::Body::empty()).unwrap()
    }

    async fn stamped(accept: Option<&str>) -> Option<String> {
        let req = inject_accept_kind_header(req_with_accept(accept)).await;
        req.headers()
            .get("x-rcms-accept")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    /// The cache keys on this header, so a JSON caller must be the only
    /// one that changes it. If a browser ever stamped it too, ordinary
    /// traffic would fragment the cache for no benefit.
    #[tokio::test]
    async fn only_an_explicit_json_preference_changes_the_cache_key() {
        assert_eq!(
            stamped(Some("application/json")).await.as_deref(),
            Some("json")
        );
        assert_eq!(
            stamped(Some("application/json, text/plain, */*"))
                .await
                .as_deref(),
            Some("json")
        );

        // Everything else shares the header-absent key.
        let chrome = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,\
                      image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7";
        assert_eq!(stamped(Some(chrome)).await, None);
        assert_eq!(stamped(Some("*/*")).await, None);
        assert_eq!(stamped(None).await, None);
        // A page that prefers HTML but tolerates JSON is an HTML caller.
        assert_eq!(
            stamped(Some("application/json;q=0.1, text/html")).await,
            None
        );
        // An explicit refusal is not a request.
        assert_eq!(stamped(Some("application/json;q=0")).await, None);
    }

    /// The poisoning scenario, stated as an assertion: a browser and a
    /// JSON client must not land on the same cache key for the same URL,
    /// or one gets the other's body.
    #[tokio::test]
    async fn a_browser_and_a_json_client_do_not_share_a_key() {
        let browser = stamped(Some("text/html,application/xhtml+xml,*/*;q=0.8")).await;
        let json = stamped(Some("application/json")).await;
        assert_ne!(
            browser, json,
            "browser and JSON callers must key differently or the page \
             cache will serve one the other's representation"
        );
    }
}
