//! R-CMS admin section.
//!
//! Mounts at `/cms-admin/` (distinct from rustango's `/__admin/` —
//! the framework admin's POST handlers do raw `insert_returning`
//! against the DB, bypassing our `Page::create_root` /
//! `Page::create_child` paths and producing malformed rows with
//! empty `path`, `depth = 0`, and no type-whitelist enforcement).
//!
//! This module owns the page-tree-aware CRUD surface. [`router`] is
//! unauthenticated on its own: wrap it in [`with_login_required`], which
//! redirects anonymous visitors to the login URL and requires the
//! `cms_admin.access` codename (or superuser) of signed-in users.
//!
//! ## Wiring
//!
//! ```ignore
//! use std::sync::Arc;
//! use tera::Tera;
//!
//! let mut tera = Tera::new("templates/**/*.html")?;
//! rustango_cms::admin::register_templates(&mut tera)?;
//! let tera = Arc::new(tera);
//!
//! let cms_admin = rustango_cms::admin::with_login_required(
//!     rustango_cms::admin::router(tera.clone()),
//!     "/login",
//! );
//! let router = Router::new()
//!     .merge(cms_admin)
//!     .merge(rustango_cms::router(tera));
//! ```

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use tera::Tera;

use crate::page::PageStatus;
use crate::tree_ops::TreeError;

pub mod admin_page;

/// Fill a template context with the admin chrome — sidebar, branding,
/// locale, the signed-in user.
///
/// Re-exported for host applications implementing
/// [`admin_page::AdminPageHandler`]: a custom page that extends
/// `rcms_admin/_base.html` cannot render without it, and reproducing it
/// by hand means roughly two dozen variables that drift silently on the
/// next release.
pub use handlers::add_chrome;

pub mod chooser;
mod handlers;
mod i18n;
pub(crate) mod members;
/// Merge host-supplied UI-catalog entries into the shared admin translator
/// so a consumer's own template chrome localizes through the same
/// `translate` binding without clobbering the admin catalog.
pub use i18n::extend_ui_catalog;
pub mod model_admin;
pub mod pagination;
pub mod report;
pub mod resources;
mod route_perms;
pub(crate) mod notifications;
pub(crate) mod sites;
pub(crate) mod template_editor;
/// Permission codename gating the per-tenant template editor. Re-exported
/// so seeding and the handler cannot drift apart on the string.
pub use template_editor::CODENAME as TEMPLATE_EDIT_CODENAME;
/// Permission gating notification destinations — see
/// [`notifications::CODENAME`].
pub use notifications::CODENAME as NOTIFICATION_MANAGE_CODENAME;
/// Permission gating hostname → root-page mappings — see
/// [`sites::CODENAME`].
pub use sites::CODENAME as SITE_MANAGE_CODENAME;
/// Whether a template is a sensible choice for a page type. Shared so the
/// admin picker and the MCP tool cannot drift on the rule.
pub(crate) use template_editor::offerable as template_offerable;
pub use handlers::ListQuery;
// #587 — the crate's MCP tools reuse the admin write/read paths.
pub(crate) use handlers::{
    allowed_children_for, allowed_root_types, apply_page_edit, dedup_slug_under_parent,
    media_picker_payload, slugify, store_media, upsert_page_translations, MediaPickerQuery,
    PageEditActor, PageEditOutcome, PageEditRefusal,
};

/// Admin-handler state — the Tera instance the handlers render
/// against, plus an optional page-cache invalidator (#78). When the
/// host registers an invalidator, admin saves and deletes fire
/// purges on the affected URLs so editors don't have to wait for the
/// public-side TTL to elapse.
#[derive(Clone)]
pub(crate) struct AdminState {
    pub(crate) tera: Arc<Tera>,
    /// HMAC secret used to sign password-reset URLs. Loaded once at
    /// boot via [`rustango::session::SessionSecret::from_env_or_disk`]
    /// so reset links stay valid across server restarts. Keep
    /// `Arc<Vec<u8>>` (not `&[u8]`) so the state is `Clone` for
    /// axum's per-request state cloning.
    pub(crate) signing_secret: Arc<Vec<u8>>,
    /// Page-cache invalidator (#78). Defaults to
    /// [`cache_invalidate::Noop`] — the public layer's TTL is the
    /// only invalidation signal. Hosts that wire a real cache into
    /// the public router pass a matching [`crate::cache_invalidate::BoxedCacheInvalidator`]
    /// via [`router_with_invalidator`] so admin saves purge those
    /// keys immediately.
    pub(crate) cache_invalidator: Arc<dyn crate::cache_invalidate::PageCacheInvalidator>,
    /// Workflow notification mailer (#85). Defaults to `None` — no
    /// email is sent. Hosts wire a real backend
    /// (`rustango::email::ConsoleMailer`, an SMTP one, an SES one)
    /// via [`router_with_mailer`].
    pub(crate) mailer: Option<Arc<dyn rustango::email::Mailer>>,
    /// `From:` address stamped onto every workflow notification.
    /// Set via [`router_with_mailer`]; ignored when `mailer` is
    /// `None`.
    pub(crate) mailer_from: Arc<String>,
}

/// Build the admin router. Mount with `Router::merge` in the host
/// app's main router.
///
/// **Auth note:** the returned router is *unprotected* — every host
/// app should layer [`with_login_required`] on top (or equivalent
/// reverse-proxy basic auth).
///
/// Persists a process-local secret at `./var/.rustango_cms_signing.key`
/// to sign password-reset URLs (so links stay valid across server
/// restarts).
pub fn router(tera: Arc<Tera>) -> Router {
    let secret = load_or_generate_signing_secret(&std::path::PathBuf::from(
        "./var/.rustango_cms_signing.key",
    ));
    crate::perf::instrument(router_with_state(AdminState {
        tera,
        signing_secret: Arc::new(secret),
        cache_invalidator: crate::cache_invalidate::noop(),
        mailer: None,
        mailer_from: Arc::new(String::new()),
    }))
}

/// Like [`router`] but takes a page-cache invalidator (#78). Hosts
/// using the public router's `CachePageLayer` should wire the same
/// cache backend into a [`crate::cache_invalidate::BoxedCacheInvalidator`]
/// so admin saves purge the matching cache keys immediately.
pub fn router_with_invalidator(
    tera: Arc<Tera>,
    invalidator: Arc<dyn crate::cache_invalidate::PageCacheInvalidator>,
) -> Router {
    let secret = load_or_generate_signing_secret(&std::path::PathBuf::from(
        "./var/.rustango_cms_signing.key",
    ));
    // Queued purge jobs a worker drains without this router fall back to it (#732).
    crate::task_queue::set_default_invalidator(Arc::clone(&invalidator));
    router_with_state(AdminState {
        tera,
        signing_secret: Arc::new(secret),
        cache_invalidator: invalidator,
        mailer: None,
        mailer_from: Arc::new(String::new()),
    })
}

/// Like [`router`] but takes both a cache invalidator AND a mailer
/// (#85). The mailer powers workflow notifications — submit /
/// approve / reject / cancel each fire emails to the relevant
/// parties. `from_addr` is the `From:` address stamped onto every
/// notification.
pub fn router_with_mailer(
    tera: Arc<Tera>,
    invalidator: Arc<dyn crate::cache_invalidate::PageCacheInvalidator>,
    mailer: Arc<dyn rustango::email::Mailer>,
    from_addr: impl Into<String>,
) -> Router {
    let secret = load_or_generate_signing_secret(&std::path::PathBuf::from(
        "./var/.rustango_cms_signing.key",
    ));
    // Queued purge jobs a worker drains without this router fall back to it (#732).
    crate::task_queue::set_default_invalidator(Arc::clone(&invalidator));
    router_with_state(AdminState {
        tera,
        signing_secret: Arc::new(secret),
        cache_invalidator: invalidator,
        mailer: Some(mailer),
        mailer_from: Arc::new(from_addr.into()),
    })
}

/// Companion to [`router`] — routes that must remain reachable
/// *without* authentication (password reset). Host apps merge this
/// alongside the protected admin router so anonymous visitors can
/// hit `/cms-admin/password-reset*` even when the rest of
/// `/cms-admin/*` is gated by [`with_login_required`].
///
/// Typical wiring:
///
/// ```ignore
/// let admin = rustango_cms::admin::public_router(tera.clone())
///     .merge(rustango_cms::admin::with_login_required(
///         rustango_cms::admin::router(tera.clone()),
///         "/login",
///     ));
/// ```
pub fn public_router(tera: Arc<Tera>) -> Router {
    public_router_inner(tera, None, String::new())
}

/// Like [`public_router`], with the mailer that sends password-reset
/// emails (#846). Without it `/cms-admin/password-reset` accepts the
/// request but can't send anything. Pass the same mailer and `From:`
/// address as [`router_with_mailer`].
pub fn public_router_with_mailer(
    tera: Arc<Tera>,
    mailer: Arc<dyn rustango::email::Mailer>,
    from_addr: impl Into<String>,
) -> Router {
    public_router_inner(tera, Some(mailer), from_addr.into())
}

fn public_router_inner(
    tera: Arc<Tera>,
    mailer: Option<Arc<dyn rustango::email::Mailer>>,
    mailer_from: String,
) -> Router {
    let secret = load_or_generate_signing_secret(&std::path::PathBuf::from(
        "./var/.rustango_cms_signing.key",
    ));
    let state = AdminState {
        tera,
        signing_secret: Arc::new(secret),
        cache_invalidator: crate::cache_invalidate::noop(),
        mailer,
        mailer_from: Arc::new(mailer_from),
    };
    Router::new()
        .route(
            "/cms-admin/password-reset",
            get(handlers::password_reset_request_form)
                .post(handlers::password_reset_request_submit),
        )
        .route(
            "/cms-admin/password-reset/confirm",
            get(handlers::password_reset_confirm_form)
                .post(handlers::password_reset_confirm_submit),
        )
        // CMS-branded login GET — host apps merge this BEFORE the
        // framework's login router so this route wins on conflict.
        // The framework's POST `/login` (auth + cookie set) still
        // handles submits since we only override GET.
        .route("/login", get(handlers::cms_login_form))
        // Public members-area auth (#members) — self-service password
        // sign-up + sign-in, minting the member session cookie. SSO /
        // social sign-in for members is the framework's
        // `member_sso_router`, mounted by the host at `/members/auth`.
        // These are public (no `with_login_required`) but their POSTs
        // ride the host's CSRF layer like every other admin form.
        .route(
            "/members/login",
            get(members::member_login_form).post(members::member_login_submit),
        )
        .route(
            "/members/signup",
            get(members::member_signup_form).post(members::member_signup_submit),
        )
        .route("/members/logout", post(members::member_logout))
        // #199 — uploaded branding assets, served pre-auth so the login +
        // password-reset screens can show the tenant logo/favicon (the
        // authenticated sidebar uses the same URLs). Anonymous-OK: these are
        // public brand images, no auth-sensitive content.
        .route(
            "/__cms-branding/logo",
            get(handlers::settings_branding_serve_logo),
        )
        .route(
            "/__cms-branding/favicon",
            get(handlers::settings_branding_serve_favicon),
        )
        // #35 — `cms_admin.access` deny page. Lives in public_router
        // so the `with_cms_admin_access` middleware can redirect to
        // it without the gate then bouncing the redirect back.
        .route("/cms-admin/no-access", get(handlers::no_access_page))
        // Admin stylesheet — must be reachable pre-auth so the login
        // + password-reset screens can load it. Host apps override by
        // dropping `static/cms_admin/cms.css` in their project root.
        .route("/cms-admin/static/cms.css", get(serve_cms_css))
        // Static admin JS bundles — same pre-auth requirement as the
        // stylesheet. The login + password-reset chrome reaches for
        // `cms-ux.js` (confirm dialogs, toast hoist), so gating them
        // behind `with_login_required` (#258) would 302-loop the
        // login page on its own assets. Stream-editor + menu-builder
        // bundles are only used on protected pages but co-locating
        // them here keeps the routing block tight; they 200 either
        // way since they have no auth-sensitive content.
        // #615 — self-hosted typography + icon font. Pre-auth, like the
        // stylesheet: the login chrome uses the same families.
        .route("/cms-admin/static/fonts.css", get(serve_admin_fonts_css))
        .route(
            "/cms-admin/static/vendor/fonts/{file}",
            get(serve_admin_font_file),
        )
        // The Rustango CMS badge + icon: the login screen, the sidebar mark
        // and the favicon when a tenant hasn't uploaded its own. Pre-auth,
        // like the stylesheet.
        .route("/cms-admin/static/brand/{file}", get(serve_admin_brand_file))
        .route("/cms-admin/static/cms-ux.js", get(serve_cms_ux_js))
        .route(
            "/cms-admin/static/stream_editor.js",
            get(serve_stream_editor_js),
        )
        // Block-tree sidebar outline for the page editor (self-activating
        // on pages with a stream root).
        .route("/cms-admin/static/block_tree.js", get(serve_block_tree_js))
        .route("/cms-admin/static/cms-preview.js", get(serve_cms_preview_js))
        .route(
            "/cms-admin/static/menu-builder.js",
            get(serve_menu_builder_js),
        )
        // #533 FB-04 — visual Form Builder editor engine.
        .route(
            "/cms-admin/static/form_builder.js",
            get(serve_form_builder_js),
        )
        .route(
            "/cms-admin/static/page_type_builder.js",
            get(serve_page_type_builder_js),
        )
        // #564 — conditional-rules runtime for builder body fields.
        .route(
            "/cms-admin/static/page_builder_rules.js",
            get(serve_page_builder_rules_js),
        )
        // #208 — axe-core is self-hosted (553 KB minified). The
        // preview iframe loads it via this URL on every reload so
        // the editor's accessibility panel can surface client-side
        // findings alongside the server-side heuristics (#116).
        .route(
            "/cms-admin/static/vendor/axe-core.min.js",
            get(serve_axe_core_js),
        )
        // #208 — small companion glue script. Runs inside the
        // preview iframe after axe-core loads, kicks off `axe.run()`,
        // and `postMessage`s the violations back to the editor.
        .route(
            "/cms-admin/static/cms-axe-preview.js",
            get(serve_cms_axe_preview_js),
        )
        // Preview navigation guard — injected into the preview iframe so
        // link/form clicks don't navigate the contained render away.
        .route(
            "/cms-admin/static/cms-preview-guard.js",
            get(serve_cms_preview_guard_js),
        )
        // #294 — self-hosted TipTap (MIT) richtext editor: the vendored
        // engine bundle + the hand-authored toolbar/wiring glue.
        .route(
            "/cms-admin/static/template-editor.js",
            get(serve_template_editor_js),
        )
        .route(
            "/cms-admin/static/vendor/codemirror.bundle.js",
            get(serve_codemirror_bundle_js),
        )
        .route(
            "/cms-admin/static/vendor/tiptap.bundle.js",
            get(serve_tiptap_bundle_js),
        )
        .route(
            "/cms-admin/static/richtext-editor.js",
            get(serve_richtext_editor_js),
        )
        // #710 — the notices the vendored assets' licences require.
        .route(
            "/cms-admin/static/vendor/THIRD-PARTY-NOTICES.txt",
            get(serve_third_party_notices),
        )
        .with_state(state)
}

/// Licence notices for the third-party assets compiled into the admin
/// (#710). Embedded so they travel with every binary, as MIT and the
/// SIL OFL require.
async fn serve_third_party_notices() -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        include_str!("static/vendor/THIRD-PARTY-NOTICES.txt"),
    )
}

/// Load the 32-byte signing key from `path` if present, otherwise
/// generate a fresh one + persist atomically. Mirrors the
/// `SessionSecret::from_env_or_disk` shape but returns raw bytes
/// (rustango's `SessionSecret` is opaque) so we can feed them to
/// `signed_url::sign` / `PasswordReset::verify` which want `&[u8]`.
fn load_or_generate_signing_secret(path: &std::path::Path) -> Vec<u8> {
    if let Ok(bytes) = std::fs::read(path) {
        if bytes.len() >= 32 {
            return bytes;
        }
    }
    // #319 — generate 32 random bytes straight from the OS CSPRNG via
    // `getrandom` (the source `OsRng` wrapped; rand 0.10 removed the
    // `OsRng` type). Explicit OS entropy at this signing-key boundary is
    // self-documenting for auditors and avoids relying on
    // `rand::random()`'s thread RNG (CSPRNG-backed today, but not
    // obviously so at a glance).
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS CSPRNG unavailable");
    let buf = buf.to_vec();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &buf).is_ok() && std::fs::rename(&tmp, path).is_ok() {
        tracing::info!(
            path = %path.display(),
            "generated new rustango-cms signing secret; password-reset \
             links signed with this key. Persisted to disk so links \
             survive restarts."
        );
    } else {
        tracing::warn!(
            "could not persist rustango-cms signing secret — \
             password-reset links will become invalid on every restart"
        );
    }
    buf
}

/// Layer [`rustango::auth_decorators::login_required`] on top of the
/// supplied router. Every protected route 302s anonymous visitors to
/// `login_url` with `?next=<original_url>` so the framework's login
/// handler can resume them after auth. Same pattern Django ships out
/// of the box (`@login_required(login_url=login_url)`).
///
/// A signed-in user also needs the `cms_admin.access` codename (or to be
/// a superuser); anyone else is redirected to `/cms-admin/no-access`.
/// Members sign up publicly and get an ordinary user row, so "has a
/// session" must never mean "may use the admin" (#671).
///
/// Use as:
///
/// ```ignore
/// let cms_admin = rustango_cms::admin::with_login_required(
///     rustango_cms::admin::router(tera.clone()),
///     "/login",
/// );
/// ```
///
/// Tri-dialect on every backend. Built on
/// [`rustango::extractors::SessionUser`], which became tri-dialect
/// in rustango #317 — anonymous requests get 302'd to `login_url`
/// with the original path preserved in `?next=` exactly like the
/// framework's PG-only `auth_decorators::login_required` decorator.
///
/// Closes #258 — the previous non-PG arm was a silent no-op that
/// left `/cms-admin/*` fully accessible without a session cookie on
/// `default = ["sqlite"]` builds.
pub fn with_login_required(router: Router, login_url: impl Into<String> + 'static) -> Router {
    use axum::middleware::{from_fn, Next};
    let login_url: Arc<str> = Arc::from(login_url.into());
    router.layer(from_fn(
        move |req: axum::http::Request<axum::body::Body>, next: Next| {
            let login_url = login_url.clone();
            async move {
                use axum::extract::FromRequestParts as _;
                let (mut parts, body) = req.into_parts();
                let user = rustango::extractors::SessionUser::from_request_parts(&mut parts, &())
                    .await
                    .unwrap_or(rustango::extractors::SessionUser(None));
                if let Some(user) = user.0 {
                    // A session alone is not admin access (#671): members sign
                    // up publicly and hold an ordinary user row, so the admin
                    // also needs `cms_admin.access` — every seeded staff role
                    // carries it, a self-registered member carries none.
                    // Static assets stay open so the no-access page can style
                    // itself; that page lives on `public_router`.
                    let allowed = user.is_superuser
                        || parts.uri.path().starts_with("/cms-admin/static/")
                        || matches!(
                            check_codename(&mut parts, "cms_admin.access").await,
                            AccessResult::Allowed
                        );
                    if !allowed {
                        use axum::response::IntoResponse as _;
                        return axum::response::Redirect::to("/cms-admin/no-access").into_response();
                    }
                    let req = axum::http::Request::from_parts(parts, body);
                    return next.run(req).await;
                }
                // Anonymous — bounce to login with `?next=` carrying the
                // original URL. Reuses the framework's redirect helper so
                // the response shape matches the PG-side decorator exactly.
                let original = parts
                    .uri
                    .path_and_query()
                    .map(|p| p.as_str().to_owned())
                    .unwrap_or_else(|| "/".to_owned());
                rustango::auth_decorators::redirect_to_login(&login_url, "next", &original)
            }
        },
    ))
}

/// Gate the CMS-admin router on the `cms_admin.access` codename (#35).
///
/// A logged-in tenant user is allowed through only when:
/// 1. they're a superuser, OR
/// 2. one of their roles grants the `cms_admin.access` codename.
///
/// Otherwise the request is redirected to `no_access_url` (defaults
/// to `/cms-admin/no-access`) so the user gets a friendly "you're
/// signed in but not allowed here" page instead of an empty 403.
///
/// **Tri-dialect.** Resolves the user via the framework's
/// `SessionUser` extractor. Anonymous requests pass through unchanged —
/// pair this with [`with_login_required`] above so that hits the login
/// flow first.
pub fn with_cms_admin_access(router: Router, no_access_url: impl Into<String>) -> Router {
    with_codename_gate(router, "cms_admin.access", no_access_url)
}

/// Hard-gate `router` on an arbitrary permission codename (#559/#561) —
/// the generalized form of [`with_cms_admin_access`]. A non-superuser
/// lacking `codename` is redirected to `no_access_url`; superusers pass;
/// anonymous flows fall through to the layered `login_required` bounce.
/// Static assets + the no-access page itself always pass so the redirect
/// can't loop and the page can style itself.
pub fn with_codename_gate(
    router: Router,
    codename: &'static str,
    no_access_url: impl Into<String>,
) -> Router {
    use axum::middleware::{from_fn, Next};
    use axum::response::{IntoResponse, Redirect};
    let no_access_url: Arc<str> = Arc::from(no_access_url.into());
    router.layer(from_fn(
        move |req: axum::http::Request<axum::body::Body>, next: Next| {
            let no_access_url = no_access_url.clone();
            async move {
                let path = req.uri().path();
                if path == &*no_access_url || path.starts_with("/cms-admin/static/") {
                    return next.run(req).await;
                }
                let (mut parts, body) = req.into_parts();
                let outcome = check_codename(&mut parts, codename).await;
                let req = axum::http::Request::from_parts(parts, body);
                match outcome {
                    AccessResult::Allowed | AccessResult::DeniedNoUser => next.run(req).await,
                    AccessResult::Denied => Redirect::to(&no_access_url).into_response(),
                }
            }
        },
    ))
}

enum AccessResult {
    Allowed,
    Denied,
    /// No session cookie / decode failed — let the layered
    /// `login_required` middleware handle the bounce to /login.
    DeniedNoUser,
}

/// Tri-dialect check that the request's user holds `codename`. After
/// rustango#317 landed (SessionUser is no longer PG-gated), we delegate
/// the cookie decode + user fetch to the framework's `SessionUser`
/// extractor instead of duplicating the logic here. The middleware
/// resolves the user, then checks for `codename` via
/// `permissions::user_codenames`. Superusers always pass.
/// The in-handler form of the codename gate, for screens gated per
/// handler rather than per router (#714): `None` to proceed, otherwise
/// where to send the user — anonymous to the login page, a signed-in user
/// without `codename` to no-access, so they aren't bounced through a login
/// form that wouldn't help. Superusers pass. One copy of the policy for
/// the template editor, sites, notifications and the page-type builder.
pub(crate) async fn codename_gate(
    tenant: &rustango::extractors::Tenant,
    user: Option<&rustango::tenancy::auth::User>,
    codename: &str,
) -> Option<axum::response::Response> {
    use axum::response::{IntoResponse, Redirect};
    let Some(user) = user else {
        return Some(Redirect::to("/login").into_response());
    };
    if user.is_superuser {
        return None;
    }
    let uid = user.id.get().copied().unwrap_or_default();
    let codes = crate::permissions::user_codenames(tenant.pool(), uid)
        .await
        .unwrap_or_default();
    if codes.contains(codename) {
        None
    } else {
        Some(Redirect::to("/cms-admin/no-access").into_response())
    }
}

async fn check_codename(parts: &mut axum::http::request::Parts, codename: &str) -> AccessResult {
    let (user, pool) = match request_user_and_pool(parts).await {
        Ok(Some(found)) => found,
        Ok(None) => return AccessResult::DeniedNoUser,
        Err(()) => return AccessResult::Denied,
    };
    if user.is_superuser {
        return AccessResult::Allowed;
    }
    let uid = user.id.get().copied().unwrap_or_default();
    let codenames = crate::permissions::user_codenames(&pool, uid)
        .await
        .unwrap_or_default();
    if codenames.contains(codename) {
        AccessResult::Allowed
    } else {
        AccessResult::Denied
    }
}

/// The request's session user and their tenant's pool. `Ok(None)` when
/// there is no session or no tenant — the layered `login_required`
/// bounce handles that; `Err` when there is a user but their tenant's
/// pool can't be had, which callers must treat as a denial.
pub(crate) async fn request_user_and_pool(
    parts: &mut axum::http::request::Parts,
) -> Result<Option<(rustango::tenancy::auth::User, rustango::sql::Pool)>, ()> {
    use axum::extract::FromRequestParts as _;
    use rustango::tenancy::OrgResolver as _;
    use std::sync::Arc;

    let Some(ctx) = parts
        .extensions
        .get::<Arc<rustango::extractors::TenantContext>>()
        .cloned()
    else {
        return Ok(None);
    };
    // SessionUser is `Infallible`, so `from_request_parts` always
    // returns `Ok` — the `Result` here is only the awaitable shape.
    let Ok(rustango::extractors::SessionUser(Some(user))) =
        rustango::extractors::SessionUser::from_request_parts(parts, &()).await
    else {
        return Ok(None);
    };
    let Ok(Some(org)) = ctx.resolver.resolve(parts, &ctx.pools.registry_pool()).await else {
        return Ok(None);
    };
    let pool = ctx.pools.scoped_pool_dyn(&org).await.map_err(|_| ())?;
    Ok(Some((user, pool)))
}

fn router_with_state(state: AdminState) -> Router {
    Router::new()
        // Pages tab — Wagtail-style tree CRUD.
        .route("/cms-admin", get(redirect_root))
        .route("/cms-admin/", get(redirect_root))
        .route("/cms-admin/pages", get(handlers::page_list))
        .route(
            "/cms-admin/pages/new",
            get(handlers::page_new_form).post(handlers::page_new_submit),
        )
        .route(
            "/cms-admin/pages/{id}/edit",
            get(handlers::page_edit_form).post(handlers::page_edit_submit),
        )
        .route(
            "/cms-admin/pages/{id}/translate",
            post(handlers::page_translate_submit),
        )
        .route(
            "/cms-admin/pages/{id}/delete",
            post(handlers::page_delete_submit),
        )
        .route(
            "/cms-admin/pages/{id}/preview",
            get(handlers::page_preview).post(handlers::page_preview_draft),
        )
        // #430 — mint a headless preview token for the decoupled frontend.
        .route(
            "/cms-admin/pages/{id}/preview-token",
            get(handlers::page_preview_token),
        )
        // One-click revert to any past revision.
        .route(
            "/cms-admin/pages/{id}/revert/{rev_id}",
            post(handlers::page_revert_submit),
        )
        // #74 — side-by-side revision diff.
        .route(
            "/cms-admin/pages/{id}/history/diff",
            get(handlers::history_diff),
        )
        // #192 — per-page audit log CSV export.
        .route("/cms-admin/pages/{id}/log.csv", get(handlers::page_log_csv))
        // Per-page permissions (#18) — JSON POST replaces the
        // grant set for a single page.
        .route(
            "/cms-admin/pages/{id}/permissions",
            post(handlers::page_permissions_submit),
        )
        // Drag-reorder (`new_parent` form field).
        .route(
            "/cms-admin/pages/{id}/move",
            post(handlers::page_move_submit),
        )
        // #29 V1 — duplicate a page as a draft sibling.
        .route(
            "/cms-admin/pages/{id}/clone",
            post(handlers::page_clone_submit),
        )
        // #75 — create a live alias of this page as a sibling.
        .route(
            "/cms-admin/pages/{id}/alias",
            post(handlers::page_alias_submit),
        )
        // #76 — set or clear the per-page view restriction.
        .route(
            "/cms-admin/pages/{id}/privacy",
            post(handlers::page_privacy_submit),
        )
        // #65 — multi-select bulk dispatch (publish / unpublish /
        // delete / move). Body shape: `action=<verb>&ids=<id>[&ids=…]`
        // (Move adds `new_parent`).
        .route("/cms-admin/pages/bulk", post(handlers::page_bulk_submit))
        // #72 — editor lock force-release + heartbeat.
        .route(
            "/cms-admin/pages/{id}/unlock",
            post(handlers::page_unlock_submit),
        )
        .route(
            "/cms-admin/pages/{id}/lock-release",
            post(handlers::page_lock_release),
        )
        .route(
            "/cms-admin/pages/{id}/lock-heartbeat",
            post(handlers::page_lock_heartbeat),
        )
        // #147 — draft autosave.
        .route(
            "/cms-admin/pages/{id}/autosave",
            post(handlers::page_autosave_submit),
        )
        // #106 — concurrent-editor presence (JSON for the banner).
        .route(
            "/cms-admin/pages/{id}/sessions",
            get(handlers::page_sessions_json),
        )
        // #115 — page subscriptions (notify-on-publish opt-in).
        .route(
            "/cms-admin/pages/{id}/subscribe",
            post(handlers::page_subscribe_submit),
        )
        .route(
            "/cms-admin/pages/{id}/unsubscribe",
            post(handlers::page_unsubscribe_submit),
        )
        // #81 PR 1 — inline comments.
        .route(
            "/cms-admin/pages/{id}/comments/new",
            post(handlers::comment_new_submit),
        )
        .route(
            "/cms-admin/pages/{id}/comments/{comment_id}/reply",
            post(handlers::comment_reply_submit),
        )
        .route(
            "/cms-admin/pages/{id}/comments/{comment_id}/resolve",
            post(handlers::comment_resolve_submit),
        )
        .route(
            "/cms-admin/pages/{id}/comments/{comment_id}/reopen",
            post(handlers::comment_reopen_submit),
        )
        // #73 PR 2 — per-page workflow actions.
        .route(
            "/cms-admin/pages/{id}/workflow/submit",
            post(handlers::page_workflow_submit),
        )
        .route(
            "/cms-admin/pages/{id}/workflow/approve",
            post(handlers::page_workflow_approve),
        )
        .route(
            "/cms-admin/pages/{id}/workflow/reject",
            post(handlers::page_workflow_reject),
        )
        .route(
            "/cms-admin/pages/{id}/workflow/cancel",
            post(handlers::page_workflow_cancel),
        )
        // Library tab — reusable content blocks (Wagtail "snippets").
        .route("/cms-admin/library", get(handlers::library_list))
        // Forms tab — dedicated list of form snippets with per-form
        // submission counts (the generic library table can't surface
        // those: `render_cell` is sync and pool-less).
        .route("/cms-admin/forms", get(handlers::forms_list))
        // #525 — persist the admin UI language choice.
        .route("/cms-admin/set-language", post(handlers::admin_set_language))
        // #533 FB-04 — dedicated visual Form Builder (forms are snippet-backed;
        // the library Edit link bounces here).
        .route(
            "/cms-admin/forms/{id}/build",
            get(handlers::form_build_form).post(handlers::form_build_submit),
        )
        // Sidebar preview for the builder — the same shared pane the page
        // editor uses, rendering through the real public form renderer.
        .route(
            "/cms-admin/forms/{id}/preview",
            get(handlers::form_preview).post(handlers::form_preview_draft),
        )
        // #548 FB-15 — publish the draft to the live form.
        .route(
            "/cms-admin/forms/{id}/publish",
            post(handlers::form_publish_submit),
        )
        // #545 FB-12 — per-form submission history + CSV export.
        .route(
            "/cms-admin/forms/{id}/submissions",
            get(handlers::form_submissions_list),
        )
        .route(
            "/cms-admin/forms/{id}/submissions/export.csv",
            get(handlers::form_submissions_csv),
        )
        // download a file uploaded through a submission.
        .route(
            "/cms-admin/forms/{id}/submissions/file/{key}",
            get(handlers::form_submission_file),
        )
        .route(
            "/cms-admin/library/new",
            get(handlers::snippet_new_form).post(handlers::snippet_create_submit),
        )
        .route(
            "/cms-admin/library/{id}/edit",
            get(handlers::snippet_edit_form).post(handlers::snippet_edit_submit),
        )
        // #409 — per-field snippet translations for a non-default locale.
        .route(
            "/cms-admin/library/{id}/translate",
            post(handlers::snippet_translate_submit),
        )
        // Bulk-action dispatch (#21 phase 2). The path param is the
        // library type's `type_name` slug, not an id — bulk actions
        // are per-type, not per-snippet.
        .route(
            "/cms-admin/library/{type_name}/bulk",
            post(handlers::library_bulk_submit),
        )
        .route(
            "/cms-admin/library/{id}/delete",
            post(handlers::snippet_delete_submit),
        )
        // #123 — opt-in snippet revisions.
        .route(
            "/cms-admin/library/{id}/revert/{seq}",
            post(handlers::snippet_revert_submit),
        )
        // #186 — bulk import snippets from CSV / TSV.
        .route(
            "/cms-admin/library/{type_name}/import",
            get(handlers::snippet_import_form).post(handlers::snippet_import_submit),
        )
        // Media + Documents — single `cms_media` table, two filtered views.
        .route("/cms-admin/media", get(handlers::media_list))
        // Image detail / focal-point editor (#4).
        .route(
            "/cms-admin/media/{id}/edit",
            get(handlers::media_edit_form).post(handlers::media_edit_submit),
        )
        // Hard crop — apply destructive OR clone-and-crop (#24).
        .route(
            "/cms-admin/media/{id}/crop",
            post(handlers::media_crop_submit),
        )
        // Replace image bytes in place — keeps id, alt, focal point (#190).
        .route(
            "/cms-admin/media/{id}/replace",
            post(handlers::media_replace_submit),
        )
        // Collection management (#5 V2).
        .route(
            "/cms-admin/media/collections",
            get(handlers::collections_list),
        )
        .route(
            "/cms-admin/media/collections/new",
            post(handlers::collections_create),
        )
        .route(
            "/cms-admin/media/collections/{id}/edit",
            post(handlers::collections_edit),
        )
        .route(
            "/cms-admin/media/collections/{id}/delete",
            post(handlers::collections_delete),
        )
        .route(
            "/cms-admin/media/bulk-move-collection",
            post(handlers::media_bulk_move_collection),
        )
        // #557 — Categories / taxonomies: vocabulary index + per-vocabulary
        // category tree CRUD.
        .route("/cms-admin/taxonomies", get(handlers::taxonomies_index))
        .route("/cms-admin/taxonomies/{slug}", get(handlers::category_tree))
        .route(
            "/cms-admin/taxonomies/{slug}/categories/new",
            get(handlers::category_new_form).post(handlers::category_create),
        )
        .route(
            "/cms-admin/taxonomies/{slug}/categories/{id}/edit",
            get(handlers::category_edit_form).post(handlers::category_update),
        )
        .route(
            "/cms-admin/taxonomies/{slug}/categories/{id}/delete",
            post(handlers::category_delete),
        )
        // #862 — a category's per-locale name.
        .route(
            "/cms-admin/taxonomies/{slug}/categories/{id}/translate",
            post(handlers::category_translate_submit),
        )
        // Per-collection permission editor (#19).
        .route(
            "/cms-admin/media/collections/{id}/permissions",
            get(handlers::collection_permissions_form)
                .post(handlers::collection_permissions_submit),
        )
        // #195 — collection view restriction (login / groups / password).
        .route(
            "/cms-admin/media/collections/{id}/privacy",
            post(handlers::collection_privacy_submit),
        )
        // Unused-media report (#8) — reverse index over every page's
        // extension JSON; lists media rows that no page references.
        .route(
            "/cms-admin/media/unused",
            get(handlers::media_unused_report),
        )
        .route(
            "/cms-admin/media/unused/delete",
            post(handlers::media_unused_bulk_delete),
        )
        .route(
            "/cms-admin/media/upload",
            get(handlers::media_upload_form).post(handlers::media_upload_submit),
        )
        // #142 — staged-upload scratch storage. The per-route body-limit
        // override lifts axum's 2 MiB default: photos dragged into the
        // media picker (and the upload page) routinely exceed it.
        .route(
            "/cms-admin/media/upload-staged",
            post(handlers::media_upload_staged)
                .route_layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024)),
        )
        .route(
            "/cms-admin/media/upload-staged/{id}/cancel",
            post(handlers::media_upload_staged_cancel),
        )
        // #188 — bulk promote staged uploads to cms_media with metadata.
        .route(
            "/cms-admin/media/upload-staged/commit",
            post(handlers::media_upload_staged_commit),
        )
        .route(
            "/cms-admin/media/{id}/delete",
            post(handlers::media_delete_submit),
        )
        .route("/cms-admin/documents", get(handlers::documents_list))
        // #420 — generic model-admin index for `register_model_admin!`ed models.
        .route("/cms-admin/model/{slug}", get(handlers::model_admin_list))
        // #439 — extensible report framework: one chrome'd dispatcher
        // for every `register_report!`ed report (`?format=csv` exports).
        .route("/cms-admin/report/{slug}", get(handlers::report_view))
        // #421 — generic chooser search endpoint for `register_chooser!`ed
        // models. The shared chooser overlay (cms-ux.js) resolves an
        // unregistered `data-chooser-kind` to this route.
        .route(
            "/cms-admin/__chooser/{slug}",
            get(handlers::chooser_search_json),
        )
        // Media picker data endpoint — collection/kind filters, pagination,
        // and server-signed rendition thumb URLs (richer than the generic
        // `__chooser/media`, which stays registered for back-compat).
        .route(
            "/cms-admin/__media-picker",
            get(handlers::media_picker_json),
        )
        // SSO providers — per-tenant OIDC/social login CRUD (admin-sso).
        .route("/cms-admin/sso-providers", get(handlers::sso_provider_list))
        .route(
            "/cms-admin/sso-providers/new",
            get(handlers::sso_provider_new_form).post(handlers::sso_provider_new_submit),
        )
        .route(
            "/cms-admin/sso-providers/{id}/edit",
            get(handlers::sso_provider_edit_form).post(handlers::sso_provider_edit_submit),
        )
        .route(
            "/cms-admin/sso-providers/{id}/delete",
            post(handlers::sso_provider_delete_submit),
        )
        // Locales — per-tenant locale registry.
        .route("/cms-admin/locales", get(handlers::locale_list))
        .route(
            "/cms-admin/locales/new",
            get(handlers::locale_new_form).post(handlers::locale_new_submit),
        )
        .route(
            "/cms-admin/locales/{id}/edit",
            get(handlers::locale_edit_form).post(handlers::locale_edit_submit),
        )
        .route(
            "/cms-admin/locales/{id}/delete",
            post(handlers::locale_delete_submit),
        )
        // Workflows — multi-step approval CRUD (#73 PR 1). Per-page-type
        // assignment + page-editor UI lands with PR 2.
        // #77 — global admin search (pages + snippets + media).
        .route("/cms-admin/search", get(handlers::admin_search))
        // #101 — aging-pages report.
        // #108 — site settings.
        .route(
            "/cms-admin/site-settings",
            get(handlers::site_settings_list),
        )
        .route(
            "/cms-admin/site-settings/{scope}/edit",
            get(handlers::site_setting_edit_form).post(handlers::site_setting_edit_submit),
        )
        .route(
            "/cms-admin/site-settings/{scope}/delete",
            post(handlers::site_setting_delete_submit),
        )
        // Translate a typed setting's text fields.
        .route(
            "/cms-admin/site-settings/{scope}/translate",
            post(handlers::site_setting_translate_submit),
        )
        // Per-tenant template overrides. Superuser-only — see
        // `template_editor`'s module docs on why.
        .route("/cms-admin/templates", get(template_editor::list))
        .route(
            "/cms-admin/templates/edit",
            get(template_editor::edit_form).post(template_editor::save),
        )
        .route("/cms-admin/templates/new", post(template_editor::create))
        .route(
            "/cms-admin/templates/validate",
            post(template_editor::validate),
        )
        .route("/cms-admin/templates/delete", post(template_editor::delete))
        // Point a page type at a template. Same permission as the editor
        // — it is template routing, not content modelling.
        .route(
            "/cms-admin/page-types/{id}/template",
            get(template_editor::assign_form).post(template_editor::assign_save),
        )
        .route("/cms-admin/analytics", get(handlers::analytics_dashboard))
        .route(
            "/cms-admin/reports/aging",
            get(handlers::aging_pages_report),
        )
        .route(
            "/cms-admin/reports/aging.csv",
            get(handlers::aging_pages_csv),
        )
        .route(
            "/cms-admin/reports/locked-pages",
            get(handlers::locked_pages_report),
        )
        .route(
            "/cms-admin/reports/locked-pages.csv",
            get(handlers::locked_pages_csv),
        )
        .route(
            "/cms-admin/reports/workflows",
            get(handlers::workflows_report),
        )
        .route(
            "/cms-admin/reports/workflows.csv",
            get(handlers::workflows_report_csv),
        )
        // #122 — revision storage + manual prune.
        .route(
            "/cms-admin/reports/revisions",
            get(handlers::revisions_report),
        )
        .route(
            "/cms-admin/reports/revisions/prune",
            post(handlers::revisions_report_prune),
        )
        // #141 — scheduled-publish report.
        .route(
            "/cms-admin/reports/scheduled",
            get(handlers::scheduled_report),
        )
        .route(
            "/cms-admin/reports/scheduled.csv",
            get(handlers::scheduled_report_csv),
        )
        // #143 — search promotions + query log.
        .route(
            "/cms-admin/reports/search",
            get(handlers::search_promotions_report),
        )
        .route(
            "/cms-admin/reports/search/{id}/edit",
            get(handlers::search_promotion_edit_form).post(handlers::search_promotion_edit_submit),
        )
        .route("/cms-admin/workflows", get(handlers::workflows_list))
        .route(
            "/cms-admin/workflows/new",
            get(handlers::workflow_new_form).post(handlers::workflow_new_submit),
        )
        .route(
            "/cms-admin/workflows/{id}/edit",
            get(handlers::workflow_edit_form).post(handlers::workflow_edit_submit),
        )
        .route(
            "/cms-admin/workflows/{id}/delete",
            post(handlers::workflow_delete_submit),
        )
        .route(
            "/cms-admin/workflows/{id}/tasks/add",
            post(handlers::workflow_task_add),
        )
        .route(
            "/cms-admin/workflows/{id}/tasks/{task_id}/delete",
            post(handlers::workflow_task_delete),
        )
        // Redirects — editor-managed 301/302 maps. Backed by the
        // public router's fallthrough on slug-misses (see
        // crate::router) + the framework's `rustango::redirects`.
        // Notification destinations + the delivery log.
        .route("/cms-admin/notifications", get(notifications::list))
        .route("/cms-admin/notifications/new", post(notifications::create))
        .route(
            "/cms-admin/notifications/{id}/toggle",
            post(notifications::toggle),
        )
        .route(
            "/cms-admin/notifications/{id}/delete",
            post(notifications::delete),
        )
        .route(
            "/cms-admin/notifications/deliveries/{id}/retry",
            post(notifications::retry_delivery),
        )
        // Sites — hostname → root page, plus the registry rows that make
        // the hostname reach this tenant at all. Hostnames travel in the
        // form body, not the path: a dotted host in a path segment is one
        // proxy normalisation away from arriving mangled.
        .route("/cms-admin/sites", get(sites::list))
        .route("/cms-admin/sites/new", post(sites::add))
        .route("/cms-admin/sites/root", post(sites::set_root))
        .route("/cms-admin/sites/toggle", post(sites::toggle))
        .route("/cms-admin/sites/delete", post(sites::remove))
        .route("/cms-admin/redirects", get(handlers::redirect_list))
        .route(
            "/cms-admin/redirects/new",
            get(handlers::redirect_new_form).post(handlers::redirect_new_submit),
        )
        .route(
            "/cms-admin/redirects/{id}/edit",
            get(handlers::redirect_edit_form).post(handlers::redirect_edit_submit),
        )
        .route(
            "/cms-admin/redirects/{id}/delete",
            post(handlers::redirect_delete_submit),
        )
        // #553 — re-enable a rule auto-disabled by a page delete/unpublish.
        .route(
            "/cms-admin/redirects/{id}/enable",
            post(handlers::redirect_enable_submit),
        )
        // #145 — bulk import from CSV / TSV.
        .route(
            "/cms-admin/redirects/import",
            get(handlers::redirect_import_form).post(handlers::redirect_import_submit),
        )
        // #400 — bulk export to CSV (round-trippable by the importer).
        .route(
            "/cms-admin/redirects/export.csv",
            get(handlers::redirect_export_csv),
        )
        // Users + Roles — native CMS surface (#9).
        .route("/cms-admin/users", get(handlers::users_list))
        // #435 — bulk enable/disable selected users.
        .route("/cms-admin/users/bulk", post(handlers::users_bulk_submit))
        .route(
            "/cms-admin/users/new",
            get(handlers::users_new_form).post(handlers::users_create_submit),
        )
        .route(
            "/cms-admin/users/{id}/edit",
            get(handlers::users_edit_form).post(handlers::users_edit_submit),
        )
        .route(
            "/cms-admin/users/{id}/deactivate",
            post(handlers::users_deactivate_submit),
        )
        .route("/cms-admin/roles", get(handlers::roles_list))
        // #436 — tenant-wide read-only permission overview matrix.
        .route(
            "/cms-admin/permissions",
            get(handlers::permissions_overview),
        )
        .route(
            "/cms-admin/roles/new",
            get(handlers::roles_new_form).post(handlers::roles_create_submit),
        )
        .route(
            "/cms-admin/roles/{id}/edit",
            get(handlers::roles_edit_form).post(handlers::roles_edit_submit),
        )
        .route(
            "/cms-admin/roles/{id}/delete",
            post(handlers::roles_delete_submit),
        )
        // Navigation menus (#22).
        .route("/cms-admin/navigation", get(handlers::navigation_list))
        .route(
            "/cms-admin/navigation/new",
            post(handlers::navigation_create_submit),
        )
        .route(
            "/cms-admin/navigation/{id}/delete",
            post(handlers::navigation_delete_submit),
        )
        .route(
            "/cms-admin/navigation/{id}/clone",
            post(handlers::navigation_clone_submit),
        )
        .route(
            "/cms-admin/navigation/{id}/save-tree",
            post(handlers::navigation_save_tree_submit),
        )
        // #257 — Wagtailmenus parity Option B: seed the menu's items
        // from every `show_in_menus = true AND status = published` page.
        // Idempotent rebuild — drops + re-inserts so a fresh menu
        // can be hand-curated from the implicit tree.
        .route(
            "/cms-admin/navigation/{id}/seed-from-pages",
            post(handlers::navigation_seed_from_pages_submit),
        )
        .route(
            "/cms-admin/navigation/{id}/edit",
            get(handlers::navigation_edit_form),
        )
        .route(
            "/cms-admin/navigation/{id}/translate",
            post(handlers::navigation_translate_submit),
        )
        .route(
            "/cms-admin/navigation/{id}/items",
            post(handlers::navigation_item_create),
        )
        .route(
            "/cms-admin/navigation/{menu_id}/items/{id}/delete",
            post(handlers::navigation_item_delete),
        )
        // History — tenant-wide revision audit log (#17).
        .route("/cms-admin/history", get(handlers::history_page))
        // Page-types — registry + usage stats (#16).
        .route("/cms-admin/page-types", get(handlers::page_types_dashboard))
        // #559/#566 — create/delete UI page types (Developer-gated in-handler).
        .route(
            "/cms-admin/page-types/new",
            get(handlers::page_type_new_form).post(handlers::page_type_create),
        )
        .route(
            "/cms-admin/page-types/{id}/edit",
            get(handlers::page_type_edit_form).post(handlers::page_type_update),
        )
        .route(
            "/cms-admin/page-types/{id}/delete",
            post(handlers::page_type_delete),
        )
        // #559/#562 — page-type field builder (Developer-gated in-handler).
        .route(
            "/cms-admin/page-types/{id}/build",
            get(handlers::page_type_build_form).post(handlers::page_type_build_submit),
        )
        .route(
            "/cms-admin/page-types/{id}/build/publish",
            post(handlers::page_type_publish_submit),
        )
        // #863 — the choice options' labels in other languages.
        .route(
            "/cms-admin/page-types/{id}/translate",
            get(handlers::page_type_translate_form).post(handlers::page_type_translate_submit),
        )
        // #559/#563 — reusable components library (Developer-gated in-handler).
        .route("/cms-admin/components", get(handlers::components_index))
        .route(
            "/cms-admin/components/new",
            get(handlers::component_new_form).post(handlers::component_create),
        )
        .route(
            "/cms-admin/components/{id}/edit",
            get(handlers::component_edit_form).post(handlers::component_update),
        )
        .route(
            "/cms-admin/components/{id}/delete",
            post(handlers::component_delete),
        )
        // Settings — site-wide preferences.
        .route("/cms-admin/settings", get(handlers::settings_page))
        // #199 — per-tenant logo + favicon upload.
        .route(
            "/cms-admin/settings/branding",
            post(handlers::settings_branding_submit),
        )
        // NOTE: the public serve routes for the uploaded branding assets
        // (`/__cms-branding/logo|favicon`) live in `public_router` so the
        // pre-auth login + password-reset screens can show the tenant logo
        // (#199). Keeping them here would 302 anonymous requests to /login.
        .route(
            "/cms-admin/settings/theme",
            post(handlers::settings_theme_switch),
        )
        // #263 — richtext preview pane. JS in the editor POSTs the
        // textarea source here on blur; we sanitize through the same
        // ammonia allow-list the public render uses and return the
        // styled HTML for the in-editor preview swap.
        .route(
            "/cms-admin/__richtext-preview",
            post(handlers::richtext_preview),
        )
        // Per-user account preferences (#27) — appearance, text size,
        // accessibility. localStorage-backed, no DB write.
        .route("/cms-admin/me", get(handlers::account_preferences))
        // JSON endpoint backing the PageChooser widget.
        .route(
            "/cms-admin/__page-chooser",
            get(handlers::page_chooser_json),
        )
        // JSON endpoints backing the Snippet + Document choosers.
        .route(
            "/cms-admin/__snippet-chooser",
            get(handlers::snippet_chooser_json),
        )
        .route(
            "/cms-admin/__document-chooser",
            get(handlers::document_chooser_json),
        )
        // #197 — per-user notification preferences. POST form from
        // /cms-admin/me; checkbox set per kind.
        .route(
            "/cms-admin/me/notifications",
            post(handlers::account_notifications_submit),
        )
        // #201 — profile (avatar upload + display name change).
        .route(
            "/cms-admin/me/profile",
            post(handlers::account_profile_submit),
        )
        // #526 — per-user admin UI language preference.
        .route(
            "/cms-admin/me/language",
            post(handlers::account_language_submit),
        )
        // #202 — email + password change.
        .route("/cms-admin/me/email", post(handlers::account_email_submit))
        .route(
            "/cms-admin/me/email/confirm",
            get(handlers::account_email_confirm),
        )
        .route(
            "/cms-admin/me/password",
            post(handlers::account_password_submit),
        )
        // #587 — self-service MCP keys (mint shows the token once; revoke
        // is immediate — the MCP endpoint re-checks liveness per request).
        .route(
            "/cms-admin/me/mcp-keys",
            post(handlers::account_mcp_key_create),
        )
        .route(
            "/cms-admin/me/mcp-keys/{id}/revoke",
            post(handlers::account_mcp_key_revoke),
        )
        // #587 — admin-managed MCP keys for another user (the "register a
        // claude user, give it a key" flow); capabilities stay bounded by
        // the TARGET user's entitlement.
        .route(
            "/cms-admin/users/{id}/mcp-keys",
            post(handlers::users_mcp_key_create),
        )
        .route(
            "/cms-admin/users/{id}/mcp-keys/{key_id}/revoke",
            post(handlers::users_mcp_key_revoke),
        )
        // Public-ish serve route for the avatar bytes. Anonymous-OK
        // because the sidebar avatar shows on every authed page (and
        // the brand glyph fallback handles the no-avatar case).
        .route(
            "/__cms-avatar/{user_id}",
            get(handlers::account_avatar_serve),
        )
        // #129 — styleguide / component gallery.
        .route("/cms-admin/styleguide", get(handlers::styleguide))
        // #144 — editor dashboard / home screen.
        .route("/cms-admin/dashboard", get(handlers::dashboard))
        // #139 — banner dismissibles (per-user persistent close).
        .route(
            "/cms-admin/dismissibles/{key}",
            post(handlers::dismissible_submit),
        )
        // Static JS bundles (`cms-ux.js`, `stream_editor.js`,
        // `menu-builder.js`) live on `public_router` — they need to
        // be reachable pre-auth so the login + password-reset pages
        // can fetch them. See `public_router` for the routes.
        // Logout — clears every framework session cookie + redirects
        // to /login. POST-only (state-changing); CSRF-protected by
        // the host's CsrfLayer like every other form POST.
        .route("/cms-admin/logout", post(handlers::cms_logout_submit))
        // Custom admin pages (#23) — dispatcher for every
        // `register_admin_page!`-registered handler. The slug path
        // param selects the handler at request time; unknown slugs
        // return 404.
        .route("/cms-admin/x/{slug}", get(handlers::admin_page_dispatch))
        // #672 — every matched route needs the role-matrix codename for
        // its section and verb, not just admin access.
        .route_layer(axum::middleware::from_fn(route_perms::gate))
        // #689 — outermost, so a tenant provisioned after boot is seeded
        // (roles included) before the gate above reads them.
        .route_layer(axum::middleware::from_fn(crate::seed::lazy_seed_layer))
        .with_state(state)
}

async fn redirect_root() -> axum::response::Redirect {
    // #144 — root now lands on the dashboard. Was /cms-admin/pages.
    axum::response::Redirect::permanent("/cms-admin/dashboard")
}

/// A bundled admin asset, cached for a year as immutable. Only safe
/// because every URL that loads one carries `?v={{ cms_asset_version() }}`
/// (or, for a bundle loaded from script, the loader's own `?v=`), so a
/// new build is a new URL. The one place that policy is written (#696).
fn immutable_asset<B: axum::response::IntoResponse>(
    content_type: &'static str,
    body: B,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static(content_type),
            ),
            (
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
        ],
        body,
    )
        .into_response()
}

/// Serve the bundled stream-editor JS, cached as an [`immutable_asset`].
async fn serve_stream_editor_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/stream_editor.js"))
}

/// Shared sidebar-preview driver. Any editor that includes
/// `_preview_pane.html` loads this; it reads its configuration from the
/// pane's data attributes, so it is content-agnostic.
async fn serve_cms_preview_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/cms-preview.js"))
}

/// Block-tree sidebar outline (page editor).
async fn serve_block_tree_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/block_tree.js"))
}

/// Bundled UX helper script (#25). Hosts `rcmsConfirm` modal +
/// `rcmsToast` notification stack + the auto-wiring that intercepts
/// `data-confirm` forms / links and hoists server-rendered flash
/// banners into toasts. Cached for one day so the asset doesn't
/// re-fetch on every page.
async fn serve_cms_ux_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/cms-ux.js"))
}

/// Bundled WP-style menu builder script (#44). Hosts the
/// drag-and-drop tree + JSON save POST. Loaded only on
/// `/cms-admin/navigation/{id}/edit` via an inline `<script src>`.
async fn serve_menu_builder_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/menu-builder.js"))
}

/// #533 FB-04 — the visual Form Builder editor engine. Loaded only on
/// `/cms-admin/forms/{id}/build` via an inline `<script src>`.
async fn serve_form_builder_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/form_builder.js"))
}

async fn serve_page_type_builder_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/page_type_builder.js"))
}

async fn serve_page_builder_rules_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/page_builder_rules.js"))
}

/// #208 — bundled axe-core (553 KB minified). Self-hosted under
/// `/cms-admin/static/vendor/` to satisfy strict CSP setups that
/// disable third-party CDN script-src. Loaded into the preview
/// iframe by [`serve_cms_axe_preview_js`].
async fn serve_axe_core_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/vendor/axe-core.min.js"))
}

/// #208 — the small editor↔iframe glue that runs INSIDE the preview
/// frame: waits for axe-core to load, fires `axe.run()`, and posts
/// results back to the editor's parent window via `postMessage`.
/// Also listens for `cms-axe-scroll-to` messages so the editor can
/// scroll the offending element into view.
async fn serve_cms_axe_preview_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/cms-axe-preview.js"))
}

/// Preview navigation guard — runs INSIDE the preview iframe alongside
/// the axe glue. Neutralizes link/form navigation so the preview stays a
/// contained render: same-document hash anchors scroll; every other link
/// or form submit is a no-op.
async fn serve_cms_preview_guard_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/cms-preview-guard.js"))
}

/// #615 — self-hosted admin typography. The admin used to pull Hanken
/// Grotesk, Literata and Material Symbols from `fonts.googleapis.com`.
/// Material Symbols is a LIGATURE icon font, so when that request failed —
/// offline, air-gapped, strict CSP, or an ad blocker — every icon in the
/// chrome rendered as its own name ("article", "error", "image"). It also
/// sent each editor's IP to a third party on every page load. Vendored
/// here and served from the admin's own static route, same rationale as
/// the TipTap bundle below (#294).
///
/// Subsets: latin, latin-ext, cyrillic, cyrillic-ext — enough for the
/// shipped admin locales (de/fr/pl/uk). CJK was never covered by these
/// families and still falls back to the system face.
const ADMIN_FONT_FILES: &[(&str, &[u8])] = &[
    (
        "icons-00.woff2",
        include_bytes!("static/vendor/fonts/icons-00.woff2"),
    ),
    (
        "text-00.woff2",
        include_bytes!("static/vendor/fonts/text-00.woff2"),
    ),
    (
        "text-02.woff2",
        include_bytes!("static/vendor/fonts/text-02.woff2"),
    ),
    (
        "text-03.woff2",
        include_bytes!("static/vendor/fonts/text-03.woff2"),
    ),
    (
        "text-16.woff2",
        include_bytes!("static/vendor/fonts/text-16.woff2"),
    ),
    (
        "text-17.woff2",
        include_bytes!("static/vendor/fonts/text-17.woff2"),
    ),
    (
        "text-21.woff2",
        include_bytes!("static/vendor/fonts/text-21.woff2"),
    ),
    (
        "text-22.woff2",
        include_bytes!("static/vendor/fonts/text-22.woff2"),
    ),
];

/// `@font-face` declarations pointing at the vendored files above.
/// Pre-auth like the stylesheet: the login screen uses the same chrome.
async fn serve_admin_fonts_css() -> impl axum::response::IntoResponse {
    immutable_asset("text/css; charset=utf-8", include_str!("static/vendor/fonts/fonts.css"))
}

/// Serve one vendored `.woff2`. Unknown names 404 rather than falling
/// through to the page router, which would answer with a CMS 404 page.
async fn serve_admin_font_file(axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    match ADMIN_FONT_FILES.iter().find(|(name, _)| *name == file) {
        Some((_, bytes)) => immutable_asset("font/woff2", *bytes)
            .into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, "unknown font").into_response(),
    }
}

/// The default Rustango CMS brand images, light and dark: the full badge
/// (login screen), the RS icon (sidebar mark, favicon) and the favicon /
/// touch icon. `(file name, content type, bytes)`.
const ADMIN_BRAND_FILES: &[(&str, &str, &[u8])] = &[
    ("logo-light.png", "image/png", include_bytes!("static/brand/logo-light.png")),
    ("logo-dark.png", "image/png", include_bytes!("static/brand/logo-dark.png")),
    ("icon-light.png", "image/png", include_bytes!("static/brand/icon-light.png")),
    ("icon-dark.png", "image/png", include_bytes!("static/brand/icon-dark.png")),
    ("favicon.ico", "image/x-icon", include_bytes!("static/brand/favicon.ico")),
    ("apple-touch-icon.png", "image/png", include_bytes!("static/brand/apple-touch-icon.png")),
];

async fn serve_admin_brand_file(axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    match ADMIN_BRAND_FILES.iter().find(|(name, _, _)| *name == file) {
        Some((_, content_type, bytes)) => immutable_asset(content_type, *bytes).into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, "unknown brand file").into_response(),
    }
}

/// #294 — vendored TipTap (MIT) engine bundle exposing
/// `window.RcmsRichtext.create(...)`. Self-hosted under
/// `/cms-admin/static/vendor/` (CSP-friendly, offline-capable). Content
/// is content-addressed by the committed build, so cache hard.
async fn serve_tiptap_bundle_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/vendor/tiptap.bundle.js"))
}

/// The template editor's hand-authored glue: progressively enhances the
/// source `<textarea>` into CodeMirror, lazy-loading the vendored bundle
/// only on that page.
async fn serve_template_editor_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/template-editor.js"))
}

/// Vendored CodeMirror 5 (MIT) + the `tera` mode. Served only for the
/// template editor, which injects it on demand — see
/// `static/vendor/codemirror.bundle.README.md`.
async fn serve_codemirror_bundle_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/vendor/codemirror.bundle.js"))
}

/// #294 — hand-authored richtext glue: progressively enhances every
/// `<textarea data-widget-mode="richtext">` into a TipTap WYSIWYG with a
/// toolbar, syncing HTML back into the textarea for submit. Loads after
/// the engine bundle.
async fn serve_richtext_editor_js() -> impl axum::response::IntoResponse {
    immutable_asset("text/javascript; charset=utf-8", include_str!("static/richtext-editor.js"))
}

/// Bundled admin stylesheet. Single source of truth for every
/// `/cms-admin/*` screen + the standalone auth flow. Built at compile
/// time via `include_str!` so the binary ships self-contained.
const BUNDLED_CMS_CSS: &str = include_str!("static/cms.css");

/// Serve the admin stylesheet. Host apps can override the bundled
/// CSS by dropping a `static/cms_admin/cms.css` file in their project
/// root — the handler returns that file when present, otherwise the
/// compile-time bundled stylesheet.
///
/// Mounted by [`public_router`] (not the protected router) so the
/// login + password-reset screens can load it pre-auth.
async fn serve_cms_css() -> Response {
    let override_path = std::path::Path::new("static/cms_admin/cms.css");
    if override_path.exists() {
        match std::fs::read_to_string(override_path) {
            Ok(source) => {
                return (
                    [
                        (
                            axum::http::header::CONTENT_TYPE,
                            axum::http::HeaderValue::from_static("text/css; charset=utf-8"),
                        ),
                        // Safe to cache hard: `cms_asset_version()` folds this
                        // override file's mtime+len into the `?v=` token, so a
                        // dev edit mints a fresh URL — no stale copy can stick.
                        (
                            axum::http::header::CACHE_CONTROL,
                            axum::http::HeaderValue::from_static(
                                "public, max-age=31536000, immutable",
                            ),
                        ),
                    ],
                    source,
                )
                    .into_response();
            }
            Err(err) => {
                tracing::warn!(
                    target: "rustango_cms::admin",
                    error = %err,
                    path = %override_path.display(),
                    "failed to read cms.css override; falling back to bundled stylesheet",
                );
            }
        }
    }
    immutable_asset("text/css; charset=utf-8", BUNDLED_CMS_CSS)
        .into_response()
}

/// Cache-bust token for the bundled admin assets loaded by every
/// `/cms-admin/*` page (`cms.css` + the editor JS). The admin chrome
/// links these from stable URLs, so without a version query a browser
/// that cached an older build keeps serving it — the symptom being a
/// stale `cms.css` that misses newer styles (e.g. dark-mode shades)
/// while the (query-versioned) public site looks fine.
///
/// The token folds two signals:
///   * a compile-time fingerprint of the bundled CSS + JS, so an
///     upgraded build always mints a fresh URL; and
///   * the hot-override `static/cms_admin/cms.css` file's mtime+len
///     when present, so a dev edit busts the cache without a rebuild
///     (mirrors how [`serve_cms_css`] reads that override live).
fn cms_asset_version() -> String {
    static BUNDLED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let bundled = *BUNDLED.get_or_init(|| {
        // FNV-1a over the bundled asset bytes — cheap, stable, and
        // collision-resistant enough for a cache key.
        let mut h: u64 = 0xcbf29ce4_84222325;
        let mut feed_bytes = |bytes: &[u8]| {
            for &b in bytes {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        // The brand images are linked with the same `?v=`.
        for (_, _, bytes) in ADMIN_BRAND_FILES {
            feed_bytes(bytes);
        }
        let mut feed = |s: &str| feed_bytes(s.as_bytes());
        feed(BUNDLED_CMS_CSS);
        feed(include_str!("static/cms-ux.js"));
        feed(include_str!("static/stream_editor.js"));
        feed(include_str!("static/richtext-editor.js"));
        // Every other bundled admin script too — leaving one out means a
        // changed file ships under an unchanged ?v= and browsers keep the
        // stale cached copy (bit us with page_type_builder.js).
        feed(include_str!("static/block_tree.js"));
        feed(include_str!("static/page_type_builder.js"));
        feed(include_str!("static/form_builder.js"));
        feed(include_str!("static/menu-builder.js"));
        feed(include_str!("static/cms-axe-preview.js"));
        feed(include_str!("static/cms-preview-guard.js"));
        feed(include_str!("static/cms-preview.js"));
        feed(include_str!("static/page_builder_rules.js"));
        h
    });
    if let Ok(meta) = std::fs::metadata("static/cms_admin/cms.css") {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        return format!("{bundled:x}.{mtime:x}.{:x}", meta.len());
    }
    format!("{bundled:x}")
}

/// Tera glue for [`cms_asset_version`]: `{{ cms_asset_version() }}`.
struct CmsAssetVersionFn;

impl tera::Function for CmsAssetVersionFn {
    fn call(
        &self,
        _args: &std::collections::HashMap<String, tera::Value>,
    ) -> tera::Result<tera::Value> {
        Ok(tera::Value::String(cms_asset_version()))
    }
}

/// `{{ value | json_script }}` — JSON for a `<script>` body or a quoted
/// attribute (#760).
///
/// `json_encode | safe` leaves `<`, `>`, `&` and `'` alone, so a stored
/// string containing `</script>` ends the element in the HTML tokenizer.
/// This emits the same JSON with those four as `\uXXXX` escapes, which
/// every JSON parser and JS engine reads back unchanged. Marked safe, so
/// templates use it without `| safe`.
struct JsonScriptFilter;

impl tera::Filter for JsonScriptFilter {
    fn filter(
        &self,
        value: &tera::Value,
        _args: &std::collections::HashMap<String, tera::Value>,
    ) -> tera::Result<tera::Value> {
        Ok(tera::Value::String(json_script(value)))
    }

    fn is_safe(&self) -> bool {
        true
    }
}

fn json_script(value: &tera::Value) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "null".to_owned())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\'', "\\u0027")
}

/// Register the bundled admin templates **and** the Tera helpers
/// they depend on (named URL reversal via `{{ url(...) }}`, the
/// `{% querystring %}` filter, `humanize` filters, and the
/// `csrf_input` filter) on a `Tera` instance.
///
/// Host apps call this once during Tera setup so the admin
/// handlers can find their templates without filesystem access.
///
/// # Customizing the admin
///
/// Drop a Tera template at `templates/cms_admin/<template-name>.html`
/// in your project root to override the bundled CMS admin template
/// of the same name. The override walk runs *after* the bundled
/// defaults register, so a host file wins over the built-in copy via
/// Tera's name-collision semantics.
///
/// Directory layout mirrors the template namespace:
///
/// ```text
/// templates/cms_admin/
///   login.html                 → overrides "login.html"
///   _auth_layout.html          → overrides "rcms_admin/_auth_layout.html"
///   rcms_admin/_base.html      → overrides "rcms_admin/_base.html"
///   blocks/heading.html        → overrides "blocks/heading.html"
/// ```
///
/// Top-level `.html` files at `templates/cms_admin/*.html` get a
/// best-fit name: the bare basename is used as the Tera template
/// name (so `login.html` overrides the framework's login, and
/// `_auth_layout.html` overrides the bundled `rcms_admin/_auth_layout.html`
/// via an explicit aliased registration). Files in nested
/// subdirectories use their relative path (e.g.
/// `blocks/heading.html`).
///
/// Customizing the stylesheet uses the same pattern but with
/// `static/cms_admin/cms.css` (the bundled CSS handler does its own
/// override discovery at request time — see [`serve_cms_css`]).
///
/// # Errors
/// Propagates Tera errors from template registration (only fails on
/// internal parse errors of the bundled templates, which would be a
/// compile-time bug in this crate, not a runtime concern) and I/O
/// errors when reading an unreadable host override.
pub fn register_templates(tera: &mut Tera) -> Result<(), tera::Error> {
    crate::urls::register_tera_helpers(tera);
    // `{{ cms_asset_version() }}` cache-bust token for the admin chrome's
    // own static assets (cms.css + editor JS), used in `_base.html`.
    tera.register_function("cms_asset_version", CmsAssetVersionFn);
    tera.register_filter("json_script", JsonScriptFilter);
    // #524 — admin UI i18n: register the framework `translate` fn/filter
    // backed by the embedded launch-language catalogs.
    // Share the ONE cached translator with the Tera bindings so DB overrides
    // (#532, applied via `i18n::maybe_refresh_overrides`) reach both the
    // template `translate`/`translate_plural` filters and server-string `tr`.
    rustango::i18n::tera_tags::register(tera, i18n::cached_translator().clone());
    // #641 — Tera registers `get_env` as a default builtin, and it reads
    // the process environment (`RUSTANGO_SECRET_KEY`, `DATABASE_URL`, SSO
    // secrets, …). Authored templates — written through `write_template`
    // over MCP or the admin template editor — compile into this same
    // engine and are semi-trusted content, not operator code. Left in
    // place, `{{ get_env(name="RUSTANGO_SECRET_KEY") }}` in a template
    // would render the deployment's master secret into a page, so a
    // template author could forge any tenant's sessions and decrypt every
    // stored DSN. `register_function` overrides the builtin (Tera has no
    // `unregister`), and per-tenant overlays are `base.clone()` so they
    // inherit this neutered version.
    tera.register_function(
        "get_env",
        |_args: &std::collections::HashMap<String, tera::Value>| -> tera::Result<tera::Value> {
            Err(tera::Error::msg(
                "get_env is disabled in CMS templates for security reasons",
            ))
        },
    );
    tera.add_raw_templates(vec![
        (
            "rcms_admin/_base.html",
            include_str!("templates/_base.html"),
        ),
        (
            "rcms_admin/_auth_layout.html",
            include_str!("templates/_auth_layout.html"),
        ),
        // Favicon links for every admin page: the tenant's own, else the
        // Rustango CMS icon.
        (
            "rcms_admin/_brand_icons.html",
            include_str!("templates/_brand_icons.html"),
        ),
        // Sits between `_base.html` and every list/report page: it owns the
        // intro card so the page description looks the same everywhere.
        (
            "rcms_admin/_list_base.html",
            include_str!("templates/_list_base.html"),
        ),
        // One-shot wrapper used to render a single widget through the
        // `_widget.html` macro (block/stream editors). Registered here so
        // `render_widget_via_macro_with` can render it on the SHARED Tera:
        // it used to `tera.clone()` + `add_raw_template` on every call,
        // deep-copying every compiled admin template ~173x per page-edit
        // load (~2.4ms each, 423ms total).
        (
            "rcms_admin/_stream_widget.html",
            "{% import \"rcms_admin/_widget.html\" as widget %}{{ widget::render(w=w) }}",
        ),
        (
            "rcms_admin/member_login.html",
            include_str!("templates/member_login.html"),
        ),
        (
            "rcms_admin/member_signup.html",
            include_str!("templates/member_signup.html"),
        ),
        (
            "rcms_admin/_mcp_keys_card.html",
            include_str!("templates/_mcp_keys_card.html"),
        ),
        (
            "rcms_admin/_pagination.html",
            include_str!("templates/_pagination.html"),
        ),
        (
            "rcms_admin/_preview_pane.html",
            include_str!("templates/_preview_pane.html"),
        ),
        (
            "rcms_admin/notifications.html",
            include_str!("templates/notifications.html"),
        ),
        ("rcms_admin/sites.html", include_str!("templates/sites.html")),
        (
            "rcms_admin/_widget.html",
            include_str!("templates/_widget.html"),
        ),
        (
            "rcms_admin/_widget_group.html",
            include_str!("templates/_widget_group.html"),
        ),
        (
            "rcms_admin/_permissions_tab.html",
            include_str!("templates/_permissions_tab.html"),
        ),
        (
            "rcms_admin/page_list.html",
            include_str!("templates/page_list.html"),
        ),
        (
            "rcms_admin/page_form.html",
            include_str!("templates/page_form.html"),
        ),
        (
            "rcms_admin/page_type_picker.html",
            include_str!("templates/page_type_picker.html"),
        ),
        (
            "rcms_admin/library_list.html",
            include_str!("templates/library_list.html"),
        ),
        (
            "rcms_admin/snippet_form.html",
            include_str!("templates/snippet_form.html"),
        ),
        (
            "rcms_admin/form_builder.html",
            include_str!("templates/form_builder.html"),
        ),
        (
            "rcms_admin/form_submissions.html",
            include_str!("templates/form_submissions.html"),
        ),
        (
            "rcms_admin/forms_list.html",
            include_str!("templates/forms_list.html"),
        ),
        (
            "rcms_admin/form_translate.html",
            include_str!("templates/form_translate.html"),
        ),
        (
            "rcms_admin/page_type_translate.html",
            include_str!("templates/page_type_translate.html"),
        ),
        (
            "rcms_admin/media_list.html",
            include_str!("templates/media_list.html"),
        ),
        (
            "rcms_admin/media_upload.html",
            include_str!("templates/media_upload.html"),
        ),
        (
            "rcms_admin/model_admin_list.html",
            include_str!("templates/model_admin_list.html"),
        ),
        (
            "rcms_admin/report.html",
            include_str!("templates/report.html"),
        ),
        (
            "rcms_admin/locale_list.html",
            include_str!("templates/locale_list.html"),
        ),
        (
            "rcms_admin/locale_form.html",
            include_str!("templates/locale_form.html"),
        ),
        (
            "rcms_admin/sso_provider_list.html",
            include_str!("templates/sso_provider_list.html"),
        ),
        (
            "rcms_admin/sso_provider_form.html",
            include_str!("templates/sso_provider_form.html"),
        ),
        (
            "rcms_admin/workflow_list.html",
            include_str!("templates/workflow_list.html"),
        ),
        (
            "rcms_admin/workflow_form.html",
            include_str!("templates/workflow_form.html"),
        ),
        (
            "rcms_admin/history_diff.html",
            include_str!("templates/history_diff.html"),
        ),
        (
            "rcms_admin/admin_search.html",
            include_str!("templates/admin_search.html"),
        ),
        (
            "rcms_admin/analytics.html",
            include_str!("templates/analytics.html"),
        ),
        (
            "rcms_admin/aging_pages.html",
            include_str!("templates/aging_pages.html"),
        ),
        (
            "rcms_admin/locked_pages.html",
            include_str!("templates/locked_pages.html"),
        ),
        (
            "rcms_admin/workflow_report.html",
            include_str!("templates/workflow_report.html"),
        ),
        (
            "rcms_admin/revisions_report.html",
            include_str!("templates/revisions_report.html"),
        ),
        (
            "rcms_admin/scheduled_report.html",
            include_str!("templates/scheduled_report.html"),
        ),
        (
            "rcms_admin/search_promotions_report.html",
            include_str!("templates/search_promotions_report.html"),
        ),
        (
            "rcms_admin/search_promotion_edit.html",
            include_str!("templates/search_promotion_edit.html"),
        ),
        (
            "rcms_admin/styleguide.html",
            include_str!("templates/styleguide.html"),
        ),
        (
            "rcms_admin/dashboard.html",
            include_str!("templates/dashboard.html"),
        ),
        (
            "rcms_admin/redirect_import.html",
            include_str!("templates/redirect_import.html"),
        ),
        (
            "rcms_admin/snippet_import.html",
            include_str!("templates/snippet_import.html"),
        ),
        (
            "rcms_admin/site_settings_list.html",
            include_str!("templates/site_settings_list.html"),
        ),
        (
            "rcms_admin/template_list.html",
            include_str!("templates/template_list.html"),
        ),
        (
            "rcms_admin/template_form.html",
            include_str!("templates/template_form.html"),
        ),
        (
            "rcms_admin/page_type_template_form.html",
            include_str!("templates/page_type_template_form.html"),
        ),
        (
            "rcms_admin/site_setting_form.html",
            include_str!("templates/site_setting_form.html"),
        ),
        // #76 — public-facing password-prompt page for view-restricted
        // pages. Sits under the `rcms/` namespace (vs `rcms_admin/`)
        // because it renders to anonymous visitors.
        (
            "rcms/view_password.html",
            include_str!("templates/view_password.html"),
        ),
        (
            "rcms_admin/redirect_list.html",
            include_str!("templates/redirect_list.html"),
        ),
        (
            "rcms_admin/redirect_form.html",
            include_str!("templates/redirect_form.html"),
        ),
        (
            "rcms_admin/settings.html",
            include_str!("templates/settings.html"),
        ),
        (
            "rcms_admin/history.html",
            include_str!("templates/history.html"),
        ),
        (
            "rcms_admin/navigation_list.html",
            include_str!("templates/navigation_list.html"),
        ),
        (
            "rcms_admin/navigation_edit.html",
            include_str!("templates/navigation_edit.html"),
        ),
        (
            "rcms_admin/page_types.html",
            include_str!("templates/page_types.html"),
        ),
        (
            "rcms_admin/page_type_builder.html",
            include_str!("templates/page_type_builder.html"),
        ),
        (
            "rcms_admin/components_list.html",
            include_str!("templates/components_list.html"),
        ),
        (
            "rcms_admin/schema_page.html",
            include_str!("templates/schema_page.html"),
        ),
        (
            "rcms_admin/page_type_form.html",
            include_str!("templates/page_type_form.html"),
        ),
        (
            "rcms_admin/component_builder.html",
            include_str!("templates/component_builder.html"),
        ),
        (
            "rcms_admin/users_list.html",
            include_str!("templates/users_list.html"),
        ),
        (
            "rcms_admin/user_form.html",
            include_str!("templates/user_form.html"),
        ),
        (
            "rcms_admin/roles_list.html",
            include_str!("templates/roles_list.html"),
        ),
        (
            "rcms_admin/permissions_overview.html",
            include_str!("templates/permissions_overview.html"),
        ),
        (
            "rcms_admin/role_form.html",
            include_str!("templates/role_form.html"),
        ),
        (
            "rcms_admin/media_unused.html",
            include_str!("templates/media_unused.html"),
        ),
        (
            "rcms_admin/media_edit.html",
            include_str!("templates/media_edit.html"),
        ),
        (
            "rcms_admin/collection_permissions.html",
            include_str!("templates/collection_permissions.html"),
        ),
        (
            "rcms_admin/collections_list.html",
            include_str!("templates/collections_list.html"),
        ),
        // #557 — categories / taxonomies admin.
        (
            "rcms_admin/taxonomies_list.html",
            include_str!("templates/taxonomies_list.html"),
        ),
        (
            "rcms_admin/category_tree.html",
            include_str!("templates/category_tree.html"),
        ),
        (
            "rcms_admin/category_form.html",
            include_str!("templates/category_form.html"),
        ),
        // Authentication chrome — five stand-alone screens that
        // share `rcms_admin/_auth_layout.html`. The `login.html`
        // name is unprefixed because the framework's login_view
        // looks it up by that bare name; registering it here wins
        // over the framework's bundled copy.
        ("login.html", include_str!("templates/login.html")),
        (
            "rcms_admin/password_reset_request.html",
            include_str!("templates/password_reset_request.html"),
        ),
        (
            "rcms_admin/password_reset_sent.html",
            include_str!("templates/password_reset_sent.html"),
        ),
        (
            "rcms_admin/password_reset_confirm.html",
            include_str!("templates/password_reset_confirm.html"),
        ),
        (
            "rcms_admin/password_reset_done.html",
            include_str!("templates/password_reset_done.html"),
        ),
        (
            "rcms_admin/no_access.html",
            include_str!("templates/no_access.html"),
        ),
        (
            "rcms_admin/account_preferences.html",
            include_str!("templates/account_preferences.html"),
        ),
        (
            "rcms_admin/_breadcrumbs.html",
            include_str!("templates/_breadcrumbs.html"),
        ),
        // Block render templates — used by the public-render path
        // (`stream_render` Tera function). Same Tera instance keeps
        // them reachable from anywhere (`blocks/heading.html`,
        // `blocks/paragraph.html`, …). Host apps override per-block
        // via the filesystem walk below.
        (
            "blocks/heading.html",
            include_str!("templates/blocks/heading.html"),
        ),
        (
            "blocks/paragraph.html",
            include_str!("templates/blocks/paragraph.html"),
        ),
        (
            "blocks/image.html",
            include_str!("templates/blocks/image.html"),
        ),
        (
            "blocks/quote.html",
            include_str!("templates/blocks/quote.html"),
        ),
        (
            "blocks/embed.html",
            include_str!("templates/blocks/embed.html"),
        ),
        (
            "blocks/code.html",
            include_str!("templates/blocks/code.html"),
        ),
        // #194 — expanded block catalog (Wagtail parity).
        (
            "blocks/text.html",
            include_str!("templates/blocks/text.html"),
        ),
        (
            "blocks/boolean.html",
            include_str!("templates/blocks/boolean.html"),
        ),
        (
            "blocks/date.html",
            include_str!("templates/blocks/date.html"),
        ),
        (
            "blocks/datetime.html",
            include_str!("templates/blocks/datetime.html"),
        ),
        (
            "blocks/email.html",
            include_str!("templates/blocks/email.html"),
        ),
        ("blocks/url.html", include_str!("templates/blocks/url.html")),
        (
            "blocks/integer.html",
            include_str!("templates/blocks/integer.html"),
        ),
        (
            "blocks/choice.html",
            include_str!("templates/blocks/choice.html"),
        ),
        (
            "blocks/table.html",
            include_str!("templates/blocks/table.html"),
        ),
        // Wagtail-parity routes.
        (
            "blocks/float.html",
            include_str!("templates/blocks/float.html"),
        ),
        (
            "blocks/decimal.html",
            include_str!("templates/blocks/decimal.html"),
        ),
        (
            "blocks/regex.html",
            include_str!("templates/blocks/regex.html"),
        ),
        (
            "blocks/rich_text.html",
            include_str!("templates/blocks/rich_text.html"),
        ),
        (
            "blocks/typed_table_row.html",
            include_str!("templates/blocks/typed_table_row.html"),
        ),
        (
            "blocks/typed_table.html",
            include_str!("templates/blocks/typed_table.html"),
        ),
        (
            "blocks/time.html",
            include_str!("templates/blocks/time.html"),
        ),
        (
            "blocks/multiple_choice.html",
            include_str!("templates/blocks/multiple_choice.html"),
        ),
        (
            "blocks/static.html",
            include_str!("templates/blocks/static.html"),
        ),
        (
            "blocks/raw_html.html",
            include_str!("templates/blocks/raw_html.html"),
        ),
        (
            "blocks/page_chooser.html",
            include_str!("templates/blocks/page_chooser.html"),
        ),
        (
            "blocks/snippet_chooser.html",
            include_str!("templates/blocks/snippet_chooser.html"),
        ),
        (
            "blocks/document_chooser.html",
            include_str!("templates/blocks/document_chooser.html"),
        ),
    ])?;

    // Public error-page default (the built-in ErrorPage type's
    // `default_template()`), registered ONLY when the host hasn't
    // shipped its own `error_page.html`. Hosts build their Tera from
    // their template glob BEFORE calling `register_templates`, so an
    // unconditional add here would silently clobber a host override
    // that extends the site's own base/nav/footer.
    if !tera.get_template_names().any(|n| n == "error_page.html") {
        tera.add_raw_template("error_page.html", include_str!("templates/error_page.html"))?;
    }

    // Filesystem override walk. Bundled defaults are already
    // registered above; anything found under `templates/cms_admin/`
    // re-registers with the same Tera name, winning the lookup. See
    // the rustdoc above for the directory-to-template-name mapping.
    let override_root = std::path::PathBuf::from("templates/cms_admin");
    if override_root.is_dir() {
        let count = load_template_overrides(&override_root, "", tera)?;
        if count > 0 {
            tracing::info!(
                target: "rustango_cms::admin",
                count,
                path = %override_root.display(),
                "loaded admin template overrides",
            );
        }
    }
    Ok(())
}

/// Recursive walk of `dir` registering every `.html` file as a
/// template override. `prefix` accumulates the subdirectory path
/// relative to the override root, so `templates/cms_admin/blocks/heading.html`
/// becomes Tera template name `blocks/heading.html`.
///
/// Top-level files get a special-case for `_auth_layout.html`,
/// rewriting the bare name to its prefixed form so the override
/// reaches the bundled template (which is registered under
/// `rcms_admin/_auth_layout.html`). All other top-level files keep
/// their bare basename — that's the convention for `login.html` and
/// other framework-aligned names.
fn load_template_overrides(
    dir: &std::path::Path,
    prefix: &str,
    tera: &mut Tera,
) -> Result<usize, tera::Error> {
    let mut count = 0;
    let entries = std::fs::read_dir(dir)
        .map_err(|e| tera::Error::msg(format!("read_dir {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry
            .map_err(|e| tera::Error::msg(format!("read_dir entry in {}: {e}", dir.display())))?;
        let path = entry.path();
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if path.is_dir() {
            let nested_prefix = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            count += load_template_overrides(&path, &nested_prefix, tera)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("html") {
            let template_name = derive_override_template_name(prefix, &name);
            let source = std::fs::read_to_string(&path)
                .map_err(|e| tera::Error::msg(format!("read {}: {e}", path.display())))?;
            tera.add_raw_template(&template_name, &source)?;
            tracing::info!(
                target: "rustango_cms::admin",
                path = %path.display(),
                template = %template_name,
                "registered admin template override",
            );
            count += 1;
        }
    }
    Ok(count)
}

/// Map an override file's relative path to the Tera template name it
/// should re-register as. Subdirectory files use their literal path;
/// top-level `_auth_layout.html` is special-cased to the prefixed
/// `rcms_admin/_auth_layout.html` name so hosts can override the
/// bundled auth chrome with the natural bare filename.
fn derive_override_template_name(prefix: &str, file_name: &str) -> String {
    if prefix.is_empty() {
        // Top-level file — match the bundled namespace for the
        // auth-layout partial. Everything else (login.html,
        // anything host-side) registers under its bare name.
        match file_name {
            "_auth_layout.html" => "rcms_admin/_auth_layout.html".to_owned(),
            other => other.to_owned(),
        }
    } else {
        format!("{prefix}/{file_name}")
    }
}

/// Errors surfaced through admin handlers. Renders as HTTP responses
/// via `IntoResponse`.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AdminError {
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Exec(#[from] rustango::sql::ExecError),
    #[error(transparent)]
    Sqlx(#[from] rustango::sql::sqlx::Error),
    #[error(transparent)]
    Tera(#[from] tera::Error),
    #[error("page {0} not found")]
    NotFound(i64),
    #[error("upload failed: {0}")]
    Upload(String),
    #[error("{0}")]
    Validation(String),
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let status = match &self {
            AdminError::NotFound(_) => StatusCode::NOT_FOUND,
            AdminError::Tree(TreeError::DisallowedChildType { .. })
            | AdminError::Tree(TreeError::DisallowedParentType { .. })
            | AdminError::Tree(TreeError::CycleAttempt) => StatusCode::BAD_REQUEST,
            AdminError::Upload(_) => StatusCode::BAD_REQUEST,
            AdminError::Validation(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let mut chain = self.to_string();
        let mut src: Option<&dyn std::error::Error> = std::error::Error::source(&self);
        while let Some(s) = src {
            chain.push_str("\n  caused by: ");
            chain.push_str(&s.to_string());
            src = s.source();
        }
        if status != StatusCode::INTERNAL_SERVER_ERROR {
            // 4xx messages are written for the user.
            return (status, chain).into_response();
        }
        // A 500 carries driver and template internals — table and column
        // names, hosts, template source. Log them; give the caller only a
        // reference to find the log line by (#711).
        let reference = crate::mcp::mint_uuid4();
        tracing::error!(target: "rustango_cms::admin", error_ref = %reference, "{chain}");
        (status, format!("Internal server error (ref {reference})")).into_response()
    }
}

/// Form payload for the new-page and edit-page POST handlers.
#[derive(Debug, Deserialize)]
pub struct PageForm {
    pub title: String,
    pub slug: String,
    pub page_type_id: i64,
    pub status: String,
    /// #318 — which submit button the editor pressed: `save` (→ page
    /// list), `continue` (→ keep editing this page), or `addanother`
    /// (→ a fresh new-page form under the same parent). Mirrors
    /// Django/Wagtail's three-action ModelForm footer. Missing or
    /// unknown values fall back to `save` in `redirect_after_page_save`.
    #[serde(rename = "_action", default)]
    pub action: String,
    #[serde(default)]
    pub seo_title: String,
    #[serde(default)]
    pub seo_description: String,
    #[serde(default)]
    pub robots_index: Option<String>,
    #[serde(default)]
    pub sitemap_priority: Option<f32>,
    /// "Show in menus" opt-in checkbox (#22). Pages with this flag
    /// set surface in the navigation editor's page picker as
    /// suggested entries.
    #[serde(default)]
    pub show_in_menus: Option<String>,
    /// Optional parent id — only honored on `new`. Edit ignores it
    /// (moves go through a separate verb, not implemented in v0.1
    /// admin).
    #[serde(default)]
    pub parent_id: Option<i64>,
    /// Optional per-page theme override. Empty / 0 = inherit from
    /// ancestors / site default / admin default.
    #[serde(default, deserialize_with = "deserialize_optional_i64")]
    pub theme_id: Option<i64>,
    /// Schedule — page auto-publishes at this moment when the chosen
    /// status is `scheduled`. Empty string from the datetime input
    /// → None. Local-time per the editor's browser; we parse with
    /// `chrono::NaiveDateTime::parse_from_str("%Y-%m-%dT%H:%M")` and
    /// stamp UTC. Per-user timezone override lands with #13.
    #[serde(default)]
    pub go_live_at: Option<String>,
    /// Schedule — page auto-archives at this moment.
    #[serde(default)]
    pub expire_at: Option<String>,
    /// Where this page lives on a decoupled frontend, when that differs
    /// from its CMS `url_path`. Empty means "same as `url_path`"; a path
    /// replaces `{path}` in the site-wide preview template; an absolute
    /// URL replaces the template outright.
    #[serde(default)]
    pub preview_path: String,
    /// Template this page renders through, instead of its page type's.
    /// Empty means "use the type's template".
    #[serde(default)]
    pub template_override: String,
    /// #183 — Social sharing fields. All optional; the renderer
    /// falls back to title / seo_description / first body image
    /// when empty.
    #[serde(default)]
    pub og_title: String,
    #[serde(default)]
    pub og_description: String,
    #[serde(default, deserialize_with = "deserialize_optional_i64")]
    pub og_image_media_id: Option<i64>,
    #[serde(default)]
    pub twitter_card: Option<String>,
    /// Comma-separated tags (#189). Read by the create path; an edit reads
    /// the raw form.
    #[serde(default)]
    pub tags: Option<String>,
    /// JSON array of category ids (#842), as `tags`.
    #[serde(default)]
    pub categories: Option<String>,
}

/// HTML forms submit empty strings for unselected dropdowns; serde
/// would reject them as `Option<i64>`. This deserializer maps empty
/// string + `"0"` to `None`.
pub(crate) fn deserialize_optional_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let s = String::deserialize(deserializer)?;
    if s.is_empty() || s == "0" {
        return Ok(None);
    }
    s.parse::<i64>().map(Some).map_err(serde::de::Error::custom)
}

impl PageForm {
    /// Parse the `status` field into the typed enum, defaulting to
    /// `Draft` on any unrecognized value rather than failing the
    /// submit. Editors fix the value on the next save.
    fn parsed_status(&self) -> PageStatus {
        match self.status.as_str() {
            "published" => PageStatus::Published,
            "scheduled" => PageStatus::Scheduled,
            "archived" => PageStatus::Archived,
            _ => PageStatus::Draft,
        }
    }

    /// HTML form checkboxes submit `on` when checked, are absent when
    /// unchecked. Map to bool.
    fn parsed_robots_index(&self) -> bool {
        self.robots_index.is_some()
    }

    fn parsed_sitemap_priority(&self) -> f32 {
        self.sitemap_priority.unwrap_or(0.5)
    }

    fn parsed_show_in_menus(&self) -> bool {
        self.show_in_menus.is_some()
    }

    /// Parse an HTML5 `datetime-local` value (`YYYY-MM-DDTHH:MM`)
    /// into a UTC instant. Empty / unparseable → `None`. The browser
    /// emits naive local time; until per-user timezones (#13) land
    /// we treat the value as UTC. Editors set realistic times in
    /// their own zone and accept the small offset for now.
    fn parsed_go_live_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        parse_datetime_local(self.go_live_at.as_deref())
    }

    fn parsed_expire_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        parse_datetime_local(self.expire_at.as_deref())
    }
}

fn parse_datetime_local(raw: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    // The admin's datetime inputs send UTC with a `Z` (converted from the
    // editor's timezone in the browser); an offset is honoured too.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    if let Some(utc) = raw.strip_suffix('Z') {
        return parse_datetime_local(Some(utc));
    }
    // Without JS the input posts its UTC value bare: "YYYY-MM-DDTHH:MM"
    // or "YYYY-MM-DDTHH:MM:SS".
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S"))
        .ok()
        .map(|naive| chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc))
}

#[cfg(test)]
mod datetime_input_tests {
    use super::parse_datetime_local;

    /// The schedule inputs post UTC (`…Z`), converted in the browser from the
    /// editor's timezone; without JS they post the bare UTC value back.
    #[test]
    fn utc_offset_and_bare_values_parse_to_the_same_instant() {
        let want = chrono::DateTime::parse_from_rfc3339("2026-12-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        for raw in ["2026-12-01T12:00Z", "2026-12-01T12:00:00Z", "2026-12-01T09:00:00-03:00", "2026-12-01T12:00", " 2026-12-01T12:00:00 "] {
            assert_eq!(parse_datetime_local(Some(raw)), Some(want), "{raw}");
        }
        assert_eq!(parse_datetime_local(Some("")), None);
        assert_eq!(parse_datetime_local(Some("tomorrow")), None);
    }
}

#[cfg(test)]
mod admin_error_tests {
    use super::AdminError;
    use axum::response::IntoResponse as _;

    async fn body(e: AdminError) -> (u16, String) {
        let resp = e.into_response();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.expect("body");
        (status, String::from_utf8(bytes.to_vec()).expect("utf8"))
    }

    /// #711 — a 500 names no table, host or template; a 4xx keeps its
    /// user-facing message.
    #[tokio::test]
    async fn server_errors_hide_their_cause_and_client_errors_do_not() {
        let tera_err = tera::Tera::one_off("{{ secret_column | nope }}", &tera::Context::new(), false)
            .expect_err("bad template");
        let (status, text) = body(AdminError::Tera(tera_err)).await;
        assert_eq!(status, 500);
        assert!(text.starts_with("Internal server error (ref "), "{text}");
        assert!(!text.contains("secret_column") && !text.contains("nope"), "{text}");

        let (status, text) = body(AdminError::Validation("slug is taken".into())).await;
        assert_eq!(status, 400);
        assert!(text.contains("slug is taken"));
    }
}

#[cfg(test)]
mod template_shape_tests {
    /// #760 — JSON inlined into a script body cannot end the element, and
    /// still parses back to the same value.
    #[test]
    fn json_script_cannot_close_the_script_element() {
        let hostile = serde_json::json!([{
            "name": "</script><script>alert('x')</script> & <!--",
        }]);
        let mut tera = tera::Tera::default();
        super::register_templates(&mut tera).expect("register");
        tera.add_raw_template("t", "<script>const X = {{ v | json_script }};</script>")
            .expect("template");
        let mut ctx = tera::Context::new();
        ctx.insert("v", &hostile);
        let out = tera.render("t", &ctx).expect("render");
        let body = out
            .strip_prefix("<script>const X = ")
            .and_then(|r| r.strip_suffix(";</script>"))
            .expect("one script element");
        assert!(!body.contains('<') && !body.contains('>') && !body.contains('\''), "{body}");
        let back: serde_json::Value = serde_json::from_str(body).expect("still JSON");
        assert_eq!(back, hostile);
    }

    /// Guard: `json_encode | safe` is how #760 got in. Script and
    /// attribute contexts use `json_script`.
    #[test]
    fn no_admin_template_inlines_raw_json() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/admin/templates");
        let offenders: Vec<String> = std::fs::read_dir(&dir)
            .expect("read templates dir")
            .filter_map(|e| {
                let path = e.expect("dir entry").path();
                let src = std::fs::read_to_string(&path).ok()?;
                src.contains("json_encode | safe")
                    .then(|| path.file_name().unwrap().to_string_lossy().to_string())
            })
            .collect();
        assert!(offenders.is_empty(), "use `| json_script` instead: {offenders:?}");
    }

    /// Every admin page opens the same way, and the opening comes from
    /// one place.
    ///
    /// It used not to. Thirteen templates dropped a bare
    /// `<p class="rcms-hint">` straight into the content column while
    /// eight wrapped the same prose in a `.rcms-card`, so Taxonomies and
    /// Library described themselves in visibly different boxes — and
    /// each one got fixed only when somebody noticed it on screen.
    /// `_list_base.html` owns the intro card now; this keeps the next
    /// page from hand-rolling its own.
    #[test]
    fn page_descriptions_come_from_the_shared_intro_card() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/admin/templates");
        let mut offenders = Vec::new();
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("read templates dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("html") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("read template");
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if src.contains("rcms_admin/_list_base.html") {
                // Conformant by construction: the base emits the card.
                checked += 1;
                continue;
            }
            let Some(rest) = src.split_once("{% block content %}") else {
                continue;
            };
            checked += 1;
            let head = rest.1.trim_start();
            if head.starts_with("<p class=\"rcms-hint") {
                offenders.push(format!(
                    "{name}: bare <p class=\"rcms-hint\"> opens the content \
                     column — extend rcms_admin/_list_base.html and put the \
                     prose in {{% block description %}}"
                ));
            } else if rest
                .1
                .contains("<div class=\"rcms-card\">\n    <p class=\"rcms-hint")
            {
                // Two pages hid the intro card inside an `{% if %}` branch,
                // so it looked right on screen while still being a private
                // copy — checking only the first element missed them.
                offenders.push(format!(
                    "{name}: hand-rolled intro card — extend \
                     rcms_admin/_list_base.html instead"
                ));
            }
        }
        assert!(
            checked > 40,
            "expected to scan most admin templates, scanned {checked} — \
             the `{{% block content %}}` probe is broken"
        );
        assert!(
            offenders.is_empty(),
            "{} page(s) don't use the shared intro card:\n{}",
            offenders.len(),
            offenders.join("\n")
        );
    }

    /// #641 — an authored template must not be able to read the process
    /// environment through Tera's `get_env` builtin. `register_templates`
    /// overrides it; here we prove a template calling it cannot leak one.
    /// Removing the override makes this fail.
    ///
    /// Probes `CARGO_PKG_NAME`, which cargo sets for every test binary,
    /// rather than setting a variable: `set_var` while other tests run in
    /// parallel is a data race (#749).
    #[test]
    fn get_env_is_disabled_in_the_render_engine() {
        let secret = std::env::var("CARGO_PKG_NAME").expect("cargo sets CARGO_PKG_NAME");
        let mut tera = tera::Tera::default();
        super::register_templates(&mut tera).expect("register_templates");
        tera.add_raw_template("ssti.html", r#"{{ get_env(name="CARGO_PKG_NAME") }}"#)
            .expect("add template");
        match tera.render("ssti.html", &tera::Context::new()) {
            // The override returns an error, so the render fails closed.
            Err(_) => {}
            // Belt-and-braces: even if a future change makes it fail open,
            // the value must never appear in the output.
            Ok(out) => assert!(
                !out.contains(&secret),
                "get_env leaked the process environment into a template: {out}"
            ),
        }
    }
}
