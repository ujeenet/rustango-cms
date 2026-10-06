//! Admin route handlers — `/cms-admin/...`.

use crate::log_err::LogErr as _;
use super::{AdminError, PageForm};
use crate::locale::Locale;
use crate::media::Media;
use crate::page::Page;
use crate::page_type::find_handler;
use crate::page_type_model::PageType;
use crate::tree_ops::NewPage;
use axum::extract::{Form, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Json;
use rustango::core::Column as _;
use rustango::extractors::Tenant;
use rustango::messages::Level as MsgLevel;
use rustango::sql::{transaction_pool, FetcherPool as _, FetcherTx as _};
use serde::Deserialize;
use std::sync::OnceLock;
use tera::Context;

/// Process-local HMAC key for signing the flash-messages cookie.
///
/// Seeded once per process from `rand::thread_rng().fill_bytes`.
/// Pending messages stage in the cookie itself, so the only
/// observable effect of a server restart is that any in-flight
/// flash from before the restart is silently discarded
/// ([`rustango::messages::drain`] returns an empty Vec on bad/
/// missing signatures). Acceptable trade-off for one-shot UI
/// hints; multi-instance deployments where flashes must survive a
/// pool of admin servers should run pinned sessions or move to
/// session-backed storage (queued upstream).
fn messages_secret() -> &'static [u8] {
    static SECRET: OnceLock<Vec<u8>> = OnceLock::new();
    SECRET.get_or_init(|| {
        // #319 — 32 random bytes straight from the OS CSPRNG via
        // `getrandom` (rand 0.10 removed `OsRng`; this is the source it
        // wrapped). Explicit OS entropy at this HMAC-secret boundary,
        // rather than `rand::random()`'s thread RNG.
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).expect("OS CSPRNG unavailable");
        k.to_vec()
    })
}

/// #294 — sanitize WYSIWYG (RichText-widget) HTML server-side before it
/// reaches a page-type handler's `save_extension`. The TipTap editor
/// produces constrained HTML, but a bypassed client could POST anything,
/// so we ammonia-clean every RichText-kind field value (looked up by the
/// handler's own widget definitions) before persisting. This is the
/// on-save enforcement point for extension fields; stream rich-text blocks
/// + markdown bodies already sanitize on their own render paths, and the
/// `| richtext` filter stays a pure linktype-rewriter that trusts the now-
/// sanitized stored HTML. Returns a sanitized clone of `form_map`.
async fn sanitized_extension_form(
    handler: &dyn crate::page_type::PageTypeHandler,
    pool: &rustango::sql::Pool,
    page_id: i64,
    form_map: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let mut out = form_map.clone();
    if let Ok(widgets) = handler.widgets(pool, page_id).await {
        for w in widgets {
            if w.kind == crate::widget::WidgetKind::RichText {
                if let Some(raw) = out.get(&w.name) {
                    let cleaned = crate::markdown::sanitize_html(raw);
                    out.insert(w.name.clone(), cleaned);
                }
            }
        }
    }
    out
}

/// How long a preview token stays valid: long enough for an editor to
/// click through and iterate, short enough that a URL pasted into a chat
/// stops working before it becomes a way in.
const PREVIEW_TOKEN_TTL_SECS: i64 = 3600;

/// Render a CMS-admin Tera template with the CSRF token + drained
/// flash messages stamped into the context. Returns a `Response`
/// that carries:
///   - any `Set-Cookie` the CSRF middleware needs to mint a fresh
///     token (first GET per session, or when the cookie was
///     invalidated);
///   - the clear-cookie that empties the messages store so each
///     flash renders exactly once.
///
/// Every form-rendering admin handler routes through this helper so
/// `{{ csrf_token | csrf_input | safe }}` and
/// `{% for msg in messages %}…{% endfor %}` in templates always
/// resolve correctly.
pub(crate) fn render_with_csrf(
    state: &super::AdminState,
    headers: &HeaderMap,
    template: &str,
    ctx: &mut Context,
) -> Result<Response, AdminError> {
    // #525/#526 — resolve the admin UI locale per request and expose it to the
    // template. Done here so every admin page picks it up without threading
    // through each handler. Precedence: sticky cookie → per-user preference
    // (#526, surfaced into the ctx by `add_user_chrome`) → Accept-Language →
    // per-tenant default (#526) → English.
    let user_pref = ctx
        .get("user_lang_pref")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let tenant_default = super::i18n::tenant_default_locale();
    let lang = super::i18n::negotiate(
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()),
        headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
        user_pref.as_deref(),
        tenant_default.as_deref(),
    );
    let lang_dir = rustango::i18n::text_direction(&lang);
    ctx.insert("LANG", &lang);
    ctx.insert("LANG_DIR", lang_dir);
    let csrf_cookie = rustango::forms::csrf::stamp_into_context(headers, ctx);
    let msg_clear = rustango::messages::stamp_into_context(messages_secret(), headers, ctx);
    let html = state.tera.render(template, ctx)?;
    let mut resp = Html(html).into_response();
    if let Some(c) = csrf_cookie {
        if let Ok(v) = HeaderValue::from_str(&c) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    if let Some(c) = msg_clear {
        if let Ok(v) = HeaderValue::from_str(&c) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    Ok(resp)
}

/// Redirect to a named route registered in [`crate::urls`] AND
/// stage a flash message ([`rustango::messages`]) in the response
/// cookie so the next GET at the destination renders it through
/// the `messages` Tera context variable. The standard "POST → 303
/// → render flash" idiom — every cms-admin POST handler returns
/// through this (or its `_with_params` cousin) so editors get
/// real feedback instead of a silent navigation.
fn redirect_named_with_message(
    name: &'static str,
    level: MsgLevel,
    body: &str,
    headers: &HeaderMap,
) -> Result<Response, AdminError> {
    let url = rustango::urls::reverse(name, &std::collections::HashMap::new())
        .map_err(|e| AdminError::Validation(format!("url reversal of `{name}` failed: {e}")))?;
    // #528 — localize the flash for the request's admin locale (English source
    // is the key; untranslated → unchanged).
    let body = super::i18n::tr(headers, body, &[]);
    Ok(rustango::messages::redirect_with_message(
        messages_secret(),
        headers,
        level,
        &body,
        &url,
    ))
}

/// Like [`redirect_named_with_message`] but the message is a **parameterized**
/// translation key (#528): the English source — with `{name}` placeholders — is
/// the catalog key, localized + interpolated for the request locale before
/// staging. Use instead of `&format!(…)` so the flash localizes. The framework
/// substitutes placeholders into the source-string fallback too, so English
/// (empty catalog) renders correctly without a catalog entry.
fn redirect_named_with_message_args(
    name: &'static str,
    level: MsgLevel,
    key: &str,
    args: &[(&str, &str)],
    headers: &HeaderMap,
) -> Result<Response, AdminError> {
    let url = rustango::urls::reverse(name, &std::collections::HashMap::new())
        .map_err(|e| AdminError::Validation(format!("url reversal of `{name}` failed: {e}")))?;
    let body = super::i18n::tr(headers, key, args);
    Ok(rustango::messages::redirect_with_message(
        messages_secret(),
        headers,
        level,
        &body,
        &url,
    ))
}

/// Count-aware (#528/#1102) cousin of [`redirect_named_with_message_args`]:
/// `key` is a plural source key, the form is chosen for `n` in the request
/// locale (CLDR rules) and `args` interpolated. Pass the count as an arg too
/// (e.g. `("count", &n.to_string())`) so the chosen form's `{count}` fills in.
fn redirect_named_with_message_plural(
    name: &'static str,
    level: MsgLevel,
    key: &str,
    n: i64,
    args: &[(&str, &str)],
    headers: &HeaderMap,
) -> Result<Response, AdminError> {
    let url = rustango::urls::reverse(name, &std::collections::HashMap::new())
        .map_err(|e| AdminError::Validation(format!("url reversal of `{name}` failed: {e}")))?;
    let body = super::i18n::tr_plural(headers, key, n, args);
    Ok(rustango::messages::redirect_with_message(
        messages_secret(),
        headers,
        level,
        &body,
        &url,
    ))
}

/// Plural (#528/#1102) cousin of [`redirect_named_with_params_and_message`] —
/// for routes with path params AND a count-aware message.
fn redirect_named_with_params_and_message_plural(
    name: &'static str,
    params: &[(&str, String)],
    level: MsgLevel,
    key: &str,
    n: i64,
    args: &[(&str, &str)],
    headers: &HeaderMap,
) -> Result<Response, AdminError> {
    let owned: std::collections::HashMap<String, String> = params
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect();
    let url = rustango::urls::reverse_owned(name, &owned)
        .map_err(|e| AdminError::Validation(format!("url reversal of `{name}` failed: {e}")))?;
    let body = super::i18n::tr_plural(headers, key, n, args);
    Ok(rustango::messages::redirect_with_message(
        messages_secret(),
        headers,
        level,
        &body,
        &url,
    ))
}

/// #259 / #285 — friendly 401 response for handlers that bail when
/// `SessionUser` resolves to `None`. Two content-types based on
/// `Accept`:
///
/// - **`application/json`** (#285) — JSON `{ error: "session_expired",
///   message, login_url }`. XHR / fetch clients (notably the bulk
///   media uploader) used to inline-dump the HTML page into a toast,
///   which read as garbage. JSON lets the client render a clean
///   "Sign in again to keep uploading" banner instead.
/// - **Anything else** — the HTML explainer page below, useful for
///   `<form>` POSTs that hit Submit after the cookie went stale.
///
/// The middleware-level `with_login_required` layer (#258) already
/// 302s anonymous GETs to /login. A POST hitting this guard means
/// the cookie was DECODED as anonymous (expired / wrong tenant /
/// stale), so a hard 401 with an explanation lands better than a
/// redirect loop.
fn unauthorized_no_session() -> Response {
    unauthorized_no_session_for(None)
}

/// Content-negotiating variant of [`unauthorized_no_session`]. Pass
/// the request `HeaderMap` so the client's `Accept` (and the legacy
/// `X-Requested-With: XMLHttpRequest`) can pick between the JSON
/// payload and the HTML page.
fn unauthorized_no_session_for(headers: Option<&HeaderMap>) -> Response {
    let wants_json = headers
        .map(|h| {
            let accept = h
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if accept.contains("application/json") {
                return true;
            }
            h.get("x-requested-with")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.eq_ignore_ascii_case("XMLHttpRequest"))
        })
        .unwrap_or(false);
    // #528 — localize for the request's admin locale when headers are present
    // (this is the one polished user-facing admin error page); English otherwise.
    let t = |key: &str| -> String {
        match headers {
            Some(h) => super::i18n::tr(h, key, &[]),
            None => key.to_owned(),
        }
    };
    if wants_json {
        let payload = serde_json::json!({
            "error": "session_expired",
            "message": t("Sign in again to keep uploading."),
            "login_url": "/login?next=/cms-admin/",
        });
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned()),
        )
            .into_response();
    }
    let lang = headers
        .map(|h| {
            super::i18n::negotiate(
                h.get(header::COOKIE).and_then(|v| v.to_str().ok()),
                h.get(header::ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok()),
                None,
                super::i18n::tenant_default_locale().as_deref(),
            )
        })
        .unwrap_or_else(|| "en".to_owned());
    let dir = rustango::i18n::text_direction(&lang);
    const STYLE: &str = r#"<style>
body{font:14px/1.5 -apple-system,system-ui,sans-serif;color:#222;background:#fafafa;margin:0;padding:48px 20px}
.box{max-width:560px;margin:0 auto;padding:32px;background:#fff;border-radius:12px;box-shadow:0 2px 8px rgba(0,0,0,.08)}
h1{font-size:18px;margin:0 0 12px}
p{margin:0 0 12px;color:#444}
ul{margin:0 0 12px;padding-left:20px;color:#444}
li{margin:4px 0}
a{color:#1565c0}
</style>"#;
    let mut body = String::with_capacity(STYLE.len() + 1024);
    body.push_str("<!doctype html>\n<html lang=\"");
    body.push_str(&crate::forms::render::html_escape(&lang));
    body.push_str("\" dir=\"");
    body.push_str(dir);
    body.push_str("\"><head><meta charset=\"utf-8\"><title>");
    body.push_str(&crate::forms::render::html_escape(&t("Session expired")));
    body.push_str(" — ");
    body.push_str(&crate::forms::render::html_escape(&t("CMS Admin")));
    body.push_str("</title>");
    body.push_str(STYLE);
    body.push_str("</head><body><div class=\"box\">\n<h1>");
    body.push_str(&crate::forms::render::html_escape(&t(
        "Your session has expired",
    )));
    body.push_str("</h1>\n<p>");
    body.push_str(&crate::forms::render::html_escape(&t(
        "The form couldn't be saved because your admin session decoded as anonymous. The most likely causes:",
    )));
    body.push_str("</p>\n<ul>\n<li>");
    body.push_str(&crate::forms::render::html_escape(&t(
        "Your login session expired or was signed out from another tab.",
    )));
    body.push_str("</li>\n<li>");
    body.push_str(&crate::forms::render::html_escape(&t(
        "The cookie was issued for a different tenant slug.",
    )));
    body.push_str("</li>\n<li>");
    body.push_str(&crate::forms::render::html_escape(&t(
        "On sqlite/mysql builds the framework's tenant session store may not be wired up yet (see issue #259).",
    )));
    body.push_str("</li>\n</ul>\n<p><a href=\"/login?next=/cms-admin/\">");
    body.push_str(&crate::forms::render::html_escape(&t("Sign back in")));
    body.push_str("</a>");
    body.push_str(&crate::forms::render::html_escape(&t(
        ", then re-submit the form.",
    )));
    body.push_str("</p>\n</div></body></html>");
    (
        axum::http::StatusCode::UNAUTHORIZED,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Like [`redirect_named_with_message`], but for routes with path
/// parameters (e.g. `rcms-admin:pages:edit` needing `{id}`).
pub(crate) fn redirect_named_with_params_and_message(
    name: &'static str,
    params: &[(&str, String)],
    level: MsgLevel,
    body: &str,
    headers: &HeaderMap,
) -> Result<Response, AdminError> {
    let owned: std::collections::HashMap<String, String> = params
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect();
    let url = rustango::urls::reverse_owned(name, &owned)
        .map_err(|e| AdminError::Validation(format!("url reversal of `{name}` failed: {e}")))?;
    // #528 — localize the flash for the request's admin locale.
    let body = super::i18n::tr(headers, body, &[]);
    Ok(rustango::messages::redirect_with_message(
        messages_secret(),
        headers,
        level,
        &body,
        &url,
    ))
}

/// Parameterized-key (#528) cousin of [`redirect_named_with_params_and_message`]
/// for routes that take path params AND a localized message with `{name}`
/// interpolation.
pub(crate) fn redirect_named_with_params_and_message_args(
    name: &'static str,
    params: &[(&str, String)],
    level: MsgLevel,
    key: &str,
    args: &[(&str, &str)],
    headers: &HeaderMap,
) -> Result<Response, AdminError> {
    let owned: std::collections::HashMap<String, String> = params
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect();
    let url = rustango::urls::reverse_owned(name, &owned)
        .map_err(|e| AdminError::Validation(format!("url reversal of `{name}` failed: {e}")))?;
    let body = super::i18n::tr(headers, key, args);
    Ok(rustango::messages::redirect_with_message(
        messages_secret(),
        headers,
        level,
        &body,
        &url,
    ))
}

/// #318 — resolve the post-save redirect from the three-action save
/// footer (Django/Wagtail's `ModelForm` `_save` / `_continue` /
/// `_addanother`). Shared by `page_new_submit` and `page_edit_submit`
/// so both honour the same button the editor pressed:
///
/// * `continue`   → back to **this** page's edit form (keep editing).
/// * `addanother` → a fresh new-page form, preselecting the same parent
///   so the next page lands as a sibling (`pages:new` honours
///   `?parent=<id>`); root pages omit the query.
/// * anything else (incl. the default `save` / missing) → the page list.
///
/// `body` must already be localized by the caller (via `tr` / `tr_plural`) —
/// this dispatcher only resolves the destination URL and stages the message,
/// so every branch (including `addanother`) treats `body` identically.
fn redirect_after_page_save(
    action: &str,
    page_id: i64,
    parent_id: Option<i64>,
    body: &str,
    headers: &HeaderMap,
) -> Result<Response, AdminError> {
    let url = match action {
        "continue" => rustango::urls::reverse_owned(
            "rcms-admin:pages:edit",
            &std::iter::once(("id".to_owned(), page_id.to_string())).collect(),
        )
        .map_err(|e| AdminError::Validation(format!("url reversal of `pages:edit` failed: {e}")))?,
        "addanother" => {
            let base =
                rustango::urls::reverse("rcms-admin:pages:new", &std::collections::HashMap::new())
                    .map_err(|e| {
                        AdminError::Validation(format!("url reversal of `pages:new` failed: {e}"))
                    })?;
            match parent_id {
                Some(pid) => format!("{base}?parent={pid}"),
                None => base,
            }
        }
        _ => rustango::urls::reverse("rcms-admin:pages:list", &std::collections::HashMap::new())
            .map_err(|e| {
                AdminError::Validation(format!("url reversal of `pages:list` failed: {e}"))
            })?,
    };
    Ok(rustango::messages::redirect_with_message(
        messages_secret(),
        headers,
        MsgLevel::Success,
        body,
        &url,
    ))
}

/// A user's email: the `email` column — what member sign-in, SSO and
/// sign-up use — else the address older admin versions kept in
/// `data.email`. Empty when neither is set.
pub(crate) fn user_email(u: &rustango::tenancy::auth::User) -> String {
    u.email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .or_else(|| u.data.get("email").and_then(|v| v.as_str()).map(str::trim))
        .unwrap_or("")
        .to_owned()
}

/// Add the chrome variables every section template reads from
/// `_base.html`: which sidebar tab is active, the brand-display
/// name, the current tenant slug, and — when a [`SessionUser`] is
/// in scope — the sidebar gating set (#33) + per-user timezone
/// (#13).
///
/// Pass `session_user.as_ref()` from the handler signature; pass
/// `None` for anon flows (login / password-reset). When `None`,
/// `sidebar_gating` stays `false` so every sidebar item renders.
///
/// Public so a host application's [`AdminPageHandler`] can render inside
/// the admin chrome. Without it the extension point only works for a page
/// that does NOT extend `rcms_admin/_base.html`, since that template
/// dereferences roughly two dozen chrome variables — and hand-supplying
/// them in a host app is a copy that drifts silently on every release.
///
/// [`AdminPageHandler`]: super::admin_page::AdminPageHandler
pub async fn add_chrome(
    ctx: &mut Context,
    tenant: &Tenant,
    active_tab: &str,
    session_user: Option<&rustango::tenancy::auth::User>,
) {
    add_chrome_sync(ctx, tenant, active_tab);
    // #532 — pull any operator edits from `rustango_translations` into the
    // shared translator (TTL-throttled) so admin chrome reflects them live.
    super::i18n::maybe_refresh_overrides(tenant.pool()).await;
    if let Some(user) = session_user {
        add_user_chrome(ctx, user, tenant.pool()).await;
    }
    // #199 — per-tenant logo + favicon URLs (see [`add_branding_urls`]).
    add_branding_urls(ctx, tenant).await;
}

/// #199 — inject the per-tenant admin logo + favicon URLs from the `branding`
/// site setting into a template context. Missing/empty rows leave the ctx slots
/// unset, so templates fall back to the brand-name text + default icon. Shared
/// by the authenticated chrome ([`add_chrome`]) and the pre-auth login /
/// password-reset screens (which have no `add_chrome`), so those can show the
/// tenant's branding too. The serve routes (`/__cms-branding/*`) live in
/// `public_router` so the images load before sign-in.
pub(crate) async fn add_branding_urls(ctx: &mut Context, tenant: &Tenant) {
    // The site's name as visitors know it: the owner's Branding "Site
    // name", else the tenant's display name (set by the operator).
    let mut site_name = tenant.org.display_name.clone();
    if let Ok(Some(row)) = crate::site_setting::get(tenant.pool(), "branding").await {
        if let Some(n) = row.value_json.get("site_name").and_then(|v| v.as_str()).filter(|n| !n.trim().is_empty()) {
            n.trim().clone_into(&mut site_name);
        }
        let is_set = |key: &str| {
            row.value_json
                .get(key)
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty())
        };
        if is_set("logo_storage_key") {
            ctx.insert("branding_logo_url", "/__cms-branding/logo");
        }
        if is_set("favicon_storage_key") {
            ctx.insert("branding_favicon_url", "/__cms-branding/favicon");
        }
    }
    ctx.insert("site_name", &site_name);
}

/// Sync core of [`add_chrome`] — every call site benefits from
/// re-using it, so it's broken out for clarity. Stuffs the static
/// chrome bits (tab, tenant, brand, default tz, registered admin
/// pages) into the context.
fn add_chrome_sync(ctx: &mut Context, tenant: &Tenant, active_tab: &str) {
    ctx.insert("active_tab", active_tab);
    ctx.insert("tenant_slug", &tenant.org.slug);
    // #524 — admin UI locale + text direction for `<html lang/dir>` and the
    // `translate(locale=LANG)` calls. The default here is overridden per
    // request by `render_with_csrf` (#525 negotiation).
    ctx.insert("LANG", "en");
    ctx.insert("LANG_DIR", "ltr");
    // #525 — the shipped UI locales, for the language switcher.
    ctx.insert(
        "ui_locales",
        &super::i18n::UI_LOCALES
            .iter()
            .map(|(c, n)| serde_json::json!({ "code": c, "name": n }))
            .collect::<Vec<_>>(),
    );
    let brand = tenant
        .org
        .brand_name
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| tenant.org.display_name.clone());
    ctx.insert("brand_name", &brand);
    // user_timezone defaults to empty; PG-authenticated requests
    // get it filled in via `add_user_chrome` (called explicitly by
    // handlers that have a SessionUser extractor in scope).
    ctx.insert("user_timezone", "");
    // #13 V2 — tenant default timezone. Sourced from
    // `RUSTANGO_CMS_DEFAULT_TIMEZONE` so multi-tenant deployments can
    // pin a sensible regional default without a schema change. The
    // boot script in _base.html resolves in order:
    //   user pref → tenant default → browser default.
    // Process configuration, read once rather than on every admin page.
    static DEFAULT_TZ: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let tenant_default_tz = DEFAULT_TZ
        .get_or_init(|| crate::config::var("DEFAULT_TIMEZONE").unwrap_or_default())
        .clone();
    ctx.insert("tenant_default_timezone", &tenant_default_tz);

    // Custom admin pages (#23) — bucket every registered handler by
    // its declared sidebar section so `_base.html` can group them
    // under the right heading. Sort within each bucket by
    // `(sort_order, label)` so authors can pin position.
    let mut by_section: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();
    for handler in super::admin_page::registered_admin_pages() {
        let section = handler.section().label().to_owned();
        by_section
            .entry(section)
            .or_default()
            .push(serde_json::json!({
                "slug": handler.slug(),
                "label": handler.label(),
                "icon": handler.icon(),
                "sort_order": handler.sort_order(),
            }));
    }
    for items in by_section.values_mut() {
        items.sort_by(|a, b| {
            let ao = a.get("sort_order").and_then(|v| v.as_i64()).unwrap_or(100);
            let bo = b.get("sort_order").and_then(|v| v.as_i64()).unwrap_or(100);
            ao.cmp(&bo).then_with(|| {
                a.get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .cmp(b.get("label").and_then(|v| v.as_str()).unwrap_or(""))
            })
        });
    }
    ctx.insert("extra_admin_pages_by_section", &by_section);
    // #439 — registry-driven report links. `_base.html` lists these
    // under the Reports group below the built-in reports, so a
    // `register_report!`ed report shows up in the nav with no template
    // edit. Already title-sorted by `registered_reports()`.
    let reports_nav: Vec<serde_json::Value> = super::report::registered_reports()
        .iter()
        .map(|r| {
            serde_json::json!({
                "slug": r.slug(),
                "title": r.title(),
                "icon": r.icon(),
            })
        })
        .collect();
    ctx.insert("registered_reports", &reports_nav);
    // #107 — plugin-contributed admin menu items (runtime hook B13).
    // Rendered at the bottom of the sidebar in stable key order.
    let plugin_menu_items: Vec<serde_json::Value> = crate::hooks::admin_menu_items()
        .into_iter()
        .map(|i| {
            serde_json::json!({
                "key": i.key,
                "label": i.label,
                "icon": i.icon,
                "href": i.href,
            })
        })
        .collect();
    ctx.insert("plugin_admin_menu_items", &plugin_menu_items);
    // #127 — help-menu entries (Wagtail parity C7). Rendered in a
    // dedicated "Help" sidebar section above Plugins.
    let help_menu_items: Vec<serde_json::Value> = crate::hooks::help_menu_items()
        .into_iter()
        .map(|i| {
            serde_json::json!({
                "key": i.key,
                "label": i.label,
                "icon": i.icon,
                "href": i.href,
            })
        })
        .collect();
    ctx.insert("help_menu_items", &help_menu_items);
    // #422 — plugin-injected global admin CSS / JS (Wagtail
    // insert_global_admin_css/js). `_base.html` emits each blob inside
    // a <style> / <script> tag.
    ctx.insert("admin_css_blobs", &crate::hooks::admin_css());
    ctx.insert("admin_js_blobs", &crate::hooks::admin_js());
    // Sidebar gating defaults: when no SessionUser is in scope (anon
    // public_router flows like login + password-reset), keep every
    // sidebar item visible. Handlers with a `SessionUser` extractor
    // override this via `add_user_chrome` below.
    ctx.insert("sidebar_gating", &false);
    ctx.insert("user_is_superuser", &false);
    ctx.insert(
        "allowed_resources",
        &std::collections::HashSet::<String>::new(),
    );
}

/// Per-request chrome extension for authenticated requests. Fills in
/// the bits `add_chrome` can't compute without a user in scope:
///
/// - `user_timezone` from `User.data.timezone` (#13 V2). Empty when
///   unset so the no-flash JS falls back to the browser TZ.
/// - `allowed_resources`: the union of role codenames the user
///   inherits, used to gate sidebar items + admin-side CRUD chips
///   (#33). Superusers skip the gating set entirely (the sidebar
///   template checks `user_is_superuser` first).
/// - `user_is_superuser`: short-circuit flag exposed to templates so
///   they don't iterate the codename set when access is unconditional.
/// - `sidebar_gating: true`: tells `_base.html` to start honoring
///   the gating set (anon flows leave this `false` so login pages
///   render their full chrome).
pub(crate) async fn add_user_chrome(
    ctx: &mut Context,
    user: &rustango::tenancy::auth::User,
    pool: &rustango::sql::Pool,
) {
    let user_timezone = user
        .data
        .get("timezone")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    ctx.insert("user_timezone", &user_timezone);
    // #526 — durable per-user admin UI language. Surfaced into the ctx here (the
    // one place with the authenticated user) so `render_with_csrf` can fold it
    // into locale negotiation without threading the user through every handler.
    if let Some(pref) = user
        .data
        .get("admin_lang")
        .and_then(serde_json::Value::as_str)
        .filter(|s| super::i18n::is_ui_locale(s))
    {
        ctx.insert("user_lang_pref", pref);
    }
    ctx.insert("user_is_superuser", &user.is_superuser);
    ctx.insert("user_username", &user.username);
    // #201 — profile chrome. The display name lives in
    // rustango_users.data.display_name; fall back to the username.
    // Avatar lives at /__cms-avatar/<user_id> when the user uploaded
    // one (avatar_path is set as a side-effect of the upload route).
    let display_name = user
        .data
        .get("display_name")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&user.username)
        .to_owned();
    ctx.insert("user_display_name", &display_name);
    let has_avatar = user
        .data
        .get("avatar_path")
        .and_then(serde_json::Value::as_str)
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if has_avatar {
        let uid = user.id.get().copied().unwrap_or(0);
        ctx.insert("user_avatar_url", &format!("/__cms-avatar/{uid}"));
    }
    let codenames = if user.is_superuser {
        // Superusers bypass gating; we still hand the template an
        // empty set so unconditional checks work without branching.
        std::collections::HashSet::<String>::new()
    } else {
        let uid = user.id.get().copied().unwrap_or(0);
        crate::permissions::user_codenames(pool, uid)
            .await
            .unwrap_or_default()
    };
    ctx.insert("allowed_resources", &codenames);
    ctx.insert("sidebar_gating", &true);
}

/// GET /cms-admin/media/collections — manage every collection in the
/// tenant. Tree view with depth-indented names, per-row rename /
/// delete + a "new" form per parent (#5 V2).
pub async fn collections_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let collections: Vec<crate::media::MediaCollection> = crate::media::MediaCollection::objects()
        .order_by(&[("parent_id", false), ("sort_order", false), ("name", false)])
        .fetch(tenant.pool())
        .await?;
    // Walk into a depth-tagged tree so the template can render
    // indented names without computing the depth itself.
    let by_id: std::collections::HashMap<i64, &crate::media::MediaCollection> = collections
        .iter()
        .filter_map(|c| c.id.get().copied().map(|id| (id, c)))
        .collect();
    fn depth(
        c: &crate::media::MediaCollection,
        by_id: &std::collections::HashMap<i64, &crate::media::MediaCollection>,
        cache: &mut std::collections::HashMap<i64, i32>,
    ) -> i32 {
        let id = c.id.get().copied().unwrap_or_default();
        if let Some(d) = cache.get(&id) {
            return *d;
        }
        let d = match c.parent_id {
            None => 0,
            Some(pid) => match by_id.get(&pid) {
                Some(parent) => depth(parent, by_id, cache) + 1,
                None => 0,
            },
        };
        cache.insert(id, d);
        d
    }
    // Media counts per collection so editors see usage at a glance.
    let media: Vec<Media> = Media::objects().fetch(tenant.pool()).await?;
    let mut counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for m in &media {
        if let Some(cid) = m.collection_id {
            *counts.entry(cid).or_insert(0) += 1;
        }
    }
    let mut depth_cache: std::collections::HashMap<i64, i32> = std::collections::HashMap::new();
    let rows: Vec<serde_json::Value> = collections
        .iter()
        .map(|c| {
            let id = c.id.get().copied().unwrap_or_default();
            serde_json::json!({
                "id": id,
                "name": c.name,
                "parent_id": c.parent_id,
                // #318 — resolve the parent FK to its display name so the
                // list shows "Photos" instead of a raw `#17`.
                "parent_name": c.parent_id.and_then(|pid| by_id.get(&pid)).map(|p| p.name.clone()),
                "sort_order": c.sort_order,
                "depth": depth(c, &by_id, &mut depth_cache),
                "media_count": counts.get(&id).copied().unwrap_or(0),
                "created_at": c.created_at.get().copied(),
            })
        })
        .collect();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "media", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("collections", &rows);
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/collections_list.html",
        &mut ctx,
    )
}

/// Form payload for create / rename of a collection.
#[derive(Debug, Deserialize)]
pub struct CollectionForm {
    pub name: String,
    #[serde(default, deserialize_with = "super::deserialize_optional_i64")]
    pub parent_id: Option<i64>,
    #[serde(default)]
    pub sort_order: Option<i32>,
}

/// POST /cms-admin/media/collections/new — create.
pub async fn collections_create(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Form(form): Form<CollectionForm>,
) -> Result<Response, AdminError> {
    let name = form.name.trim().to_owned();
    if name.is_empty() {
        return Err(AdminError::Validation(
            "Collection name is required.".to_owned(),
        ));
    }
    let mut row = crate::media::MediaCollection {
        id: rustango::sql::Auto::Unset,
        name: name.clone(),
        parent_id: form.parent_id,
        sort_order: form.sort_order.unwrap_or(100),
        created_at: rustango::sql::Auto::Unset,
    };
    row.insert_pool(tenant.pool()).await?;
    redirect_named_with_message_args(
        "rcms-admin:media:collections",
        MsgLevel::Success,
        "Collection “{name}” created.",
        &[("name", name.as_str())],
        &headers,
    )
}

/// POST /cms-admin/media/collections/{id}/edit — rename + re-parent
/// + reorder. The form posts all three fields; missing parent_id
/// (empty / "0") sends the collection to the root.
pub async fn collections_edit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<CollectionForm>,
) -> Result<Response, AdminError> {
    let mut row = crate::media::MediaCollection::objects()
        .where_(crate::media::MediaCollection::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let name = form.name.trim().to_owned();
    if name.is_empty() {
        return Err(AdminError::Validation(
            "Collection name is required.".to_owned(),
        ));
    }
    // Guard against parent_id = self (would orphan the row from
    // the tree walk).
    if form.parent_id == Some(id) {
        return Err(AdminError::Validation(
            "A collection cannot be its own parent.".to_owned(),
        ));
    }
    row.name = name;
    row.parent_id = form.parent_id;
    if let Some(so) = form.sort_order {
        row.sort_order = so;
    }
    row.save_pool(tenant.pool()).await?;
    redirect_named_with_message(
        "rcms-admin:media:collections",
        MsgLevel::Success,
        "Collection saved.",
        &headers,
    )
}

/// POST /cms-admin/media/collections/{id}/delete — soft delete via
/// re-parent then delete. Child collections are re-parented to the
/// deleted row's parent; media rows lose their collection_id (FK
/// nulls).
pub async fn collections_delete(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let row = crate::media::MediaCollection::objects()
        .where_(crate::media::MediaCollection::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let parent = row.parent_id;
    let name = row.name.clone();
    // Re-parent children so the subtree doesn't dangle.
    for mut child in crate::media::MediaCollection::objects()
        .where_(crate::media::MediaCollection::parent_id.eq(Some(id)))
        .fetch(tenant.pool())
        .await?
    {
        child.parent_id = parent;
        child.save_pool(tenant.pool()).await?;
    }
    // Null out media references — media doesn't get deleted, just
    // moves to "uncategorized".
    for mut m in Media::objects()
        .where_(Media::collection_id.eq(Some(id)))
        .fetch(tenant.pool())
        .await?
    {
        m.collection_id = None;
        m.save_pool(tenant.pool()).await?;
    }
    row.delete_pool(tenant.pool()).await?;
    redirect_named_with_message_args(
        "rcms-admin:media:collections",
        MsgLevel::Success,
        "Collection “{name}” deleted. Children re-parented; media moved to Uncategorized.",
        &[("name", name.as_str())],
        &headers,
    )
}

// ============================================================
// #557 — Categories / Taxonomies admin
//
// One vocabulary registry (`cms_taxonomy`) + a per-vocabulary tree of
// `cms_category` rows. The category editor renders as a `Vec<Widget>`
// through the shared `_widget.html` macro — the same engine the page
// editor uses: base fields (name/slug/description + featured-image /
// thumbnail MediaPickers + parent Select) plus any extension widgets a
// custom vocabulary's handler contributes.
// ============================================================

use crate::category::model::{self as category_model, Category, CategoryError};

/// Flash-redirect back to a vocabulary's category tree.
fn category_tree_redirect(
    slug: &str,
    headers: &HeaderMap,
    level: MsgLevel,
    key: &str,
    args: &[(&str, &str)],
) -> Result<Response, AdminError> {
    let body = super::i18n::tr(headers, key, args);
    let url = format!("/cms-admin/taxonomies/{slug}");
    Ok(rustango::messages::redirect_with_message(
        messages_secret(),
        headers,
        level,
        &body,
        &url,
    ))
}

fn opt_i64_from_form(form: &std::collections::HashMap<String, String>, key: &str) -> Option<i64> {
    form.get(key)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<i64>().ok())
}

/// The category editor's base fields as widgets (rendered via
/// `_widget.html`, same as page extension fields). `parent_opts` are the
/// selectable parents (excludes self on edit).
fn category_base_widgets(
    cat: Option<&Category>,
    parent_opts: Vec<(String, String)>,
) -> Vec<crate::widget::Widget> {
    use crate::widget::{Widget, WidgetKind};
    let mut opts = vec![(String::new(), "— none (top level) —".to_owned())];
    opts.extend(parent_opts);
    vec![
        Widget::new(WidgetKind::Text, "name", "Name")
            .required()
            .with_value(cat.map(|c| c.name.clone()).unwrap_or_default()),
        Widget::new(WidgetKind::Text, "slug", "Slug")
            .with_help("URL identifier — auto-generated from the name if left blank.")
            .with_value(cat.map(|c| c.slug.clone()).unwrap_or_default()),
        Widget::new(WidgetKind::Textarea, "description", "Description")
            .with_value(cat.map(|c| c.description.clone()).unwrap_or_default()),
        Widget::new(
            WidgetKind::MediaPicker,
            "featured_image_id",
            "Featured image",
        )
        .with_value(
            cat.and_then(|c| c.featured_image_id)
                .map(|i| i.to_string())
                .unwrap_or_default(),
        ),
        Widget::new(WidgetKind::MediaPicker, "thumbnail_id", "Thumbnail").with_value(
            cat.and_then(|c| c.thumbnail_id)
                .map(|i| i.to_string())
                .unwrap_or_default(),
        ),
        Widget::new(WidgetKind::Select, "parent_id", "Parent")
            .with_options(opts)
            .with_help("Nest this category under another (siblings share a parent).")
            .with_value(
                cat.and_then(|c| c.parent_id)
                    .map(|i| i.to_string())
                    .unwrap_or_default(),
            ),
    ]
}

/// Build indented `(id, "— — Name")` parent options for a vocabulary,
/// optionally excluding `exclude` (and, on edit, its descendants can't be
/// picked — the move guard rejects that at save, so we only drop self here).
async fn category_parent_options(
    pool: &rustango::sql::Pool,
    taxonomy_id: i64,
    exclude: Option<i64>,
) -> Result<Vec<(String, String)>, AdminError> {
    let cats = category_model::all_in_taxonomy(pool, taxonomy_id)
        .await
        .map_err(|e| AdminError::Validation(e.to_string()))?;
    Ok(cats
        .iter()
        .filter(|c| c.id.get().copied() != exclude)
        .map(|c| {
            let indent = "— ".repeat((c.depth.max(1) - 1) as usize);
            (
                c.id.get().copied().unwrap_or_default().to_string(),
                format!("{indent}{}", c.name),
            )
        })
        .collect())
}

/// GET /cms-admin/taxonomies — the vocabulary index.
pub async fn taxonomies_index(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let taxes = crate::category::taxonomies(tenant.pool())
        .await
        .map_err(|e| AdminError::Validation(e.to_string()))?;
    let all_cats: Vec<Category> = Category::objects().fetch(tenant.pool()).await?;
    let mut counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for c in &all_cats {
        *counts.entry(c.taxonomy_id).or_insert(0) += 1;
    }
    let rows: Vec<serde_json::Value> = taxes
        .iter()
        .map(|t| {
            serde_json::json!({
                "slug": t.slug,
                "verbose_name": t.verbose_name,
                "icon": if t.icon.is_empty() { "sell".to_owned() } else { t.icon.clone() },
                "description": t.description,
                "hierarchical": t.hierarchical,
                "count": counts.get(&t.id.get().copied().unwrap_or_default()).copied().unwrap_or(0),
            })
        })
        .collect();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "taxonomies", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("taxonomies", &rows);
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/taxonomies_list.html",
        &mut ctx,
    )
}

/// GET /cms-admin/taxonomies/{slug} — one vocabulary's category tree.
pub async fn category_tree(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(slug): Path<String>,
) -> Result<Response, AdminError> {
    let tax = crate::category::find_taxonomy(tenant.pool(), &slug)
        .await
        .map_err(|e| AdminError::Validation(e.to_string()))?
        .ok_or_else(|| AdminError::Validation(format!("unknown taxonomy `{slug}`")))?;
    let cats =
        category_model::all_in_taxonomy(tenant.pool(), tax.id.get().copied().unwrap_or_default())
            .await
            .map_err(|e| AdminError::Validation(e.to_string()))?;
    let rows: Vec<serde_json::Value> = cats
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id.get().copied(),
                "name": c.name,
                "slug": c.slug,
                "description": c.description,
                "depth": c.depth,
                "thumbnail_id": c.thumbnail_id,
                "featured_image_id": c.featured_image_id,
            })
        })
        .collect();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "taxonomies", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("taxonomy_slug", &tax.slug);
    ctx.insert("taxonomy_name", &tax.verbose_name);
    ctx.insert("categories", &rows);
    render_with_csrf(&state, &headers, "rcms_admin/category_tree.html", &mut ctx)
}

/// GET /cms-admin/taxonomies/{slug}/categories/new — create form.
pub async fn category_new_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(slug): Path<String>,
) -> Result<Response, AdminError> {
    let tax = crate::category::find_taxonomy(tenant.pool(), &slug)
        .await
        .map_err(|e| AdminError::Validation(e.to_string()))?
        .ok_or_else(|| AdminError::Validation(format!("unknown taxonomy `{slug}`")))?;
    let tid = tax.id.get().copied().unwrap_or_default();
    let mut fields = if tax.hierarchical {
        category_base_widgets(
            None,
            category_parent_options(tenant.pool(), tid, None).await?,
        )
    } else {
        // Flat vocabulary — drop the parent picker.
        let mut w = category_base_widgets(None, Vec::new());
        w.pop();
        w
    };
    // Extension fields are edited after create (they key on the new id).
    let _ = &mut fields;
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "taxonomies", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("taxonomy_slug", &tax.slug);
    ctx.insert("taxonomy_name", &tax.verbose_name);
    ctx.insert("mode", "new");
    ctx.insert("fields", &fields);
    render_with_csrf(&state, &headers, "rcms_admin/category_form.html", &mut ctx)
}

/// POST /cms-admin/taxonomies/{slug}/categories/new — create.
pub async fn category_create(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(slug): Path<String>,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let tax = crate::category::find_taxonomy(tenant.pool(), &slug)
        .await
        .map_err(|e| AdminError::Validation(e.to_string()))?
        .ok_or_else(|| AdminError::Validation(format!("unknown taxonomy `{slug}`")))?;
    let tid = tax.id.get().copied().unwrap_or_default();
    let new = category_model::NewCategory {
        name: form
            .get("name")
            .cloned()
            .unwrap_or_default()
            .trim()
            .to_owned(),
        slug: form.get("slug").cloned().unwrap_or_default(),
        description: form.get("description").cloned().unwrap_or_default(),
        featured_image_id: opt_i64_from_form(&form, "featured_image_id"),
        thumbnail_id: opt_i64_from_form(&form, "thumbnail_id"),
        parent_id: opt_i64_from_form(&form, "parent_id"),
    };
    if new.name.is_empty() {
        return category_tree_redirect(
            &slug,
            &headers,
            MsgLevel::Error,
            "Category name is required.",
            &[],
        );
    }
    match category_model::create(tenant.pool(), tid, new).await {
        Ok(cat) => {
            let id = cat.id.get().copied().unwrap_or_default();
            let body = super::i18n::tr(
                &headers,
                "Category “{name}” created.",
                &[("name", cat.name.as_str())],
            );
            let url = format!("/cms-admin/taxonomies/{slug}/categories/{id}/edit");
            Ok(rustango::messages::redirect_with_message(
                messages_secret(),
                &headers,
                MsgLevel::Success,
                &body,
                &url,
            ))
        }
        Err(e) => category_tree_redirect(&slug, &headers, MsgLevel::Error, &e.to_string(), &[]),
    }
}

/// GET /cms-admin/taxonomies/{slug}/categories/{id}/edit — edit form.
pub async fn category_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path((slug, id)): Path<(String, i64)>,
    Query(q): Query<EditQuery>,
) -> Result<Response, AdminError> {
    let tax = crate::category::find_taxonomy(tenant.pool(), &slug)
        .await
        .map_err(|e| AdminError::Validation(e.to_string()))?
        .ok_or_else(|| AdminError::Validation(format!("unknown taxonomy `{slug}`")))?;
    let tid = tax.id.get().copied().unwrap_or_default();
    let cat = Category::objects()
        .where_(Category::id.eq(id))
        .where_(Category::taxonomy_id.eq(tid))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let mut fields = if tax.hierarchical {
        category_base_widgets(
            Some(&cat),
            category_parent_options(tenant.pool(), tid, Some(id)).await?,
        )
    } else {
        let mut w = category_base_widgets(Some(&cat), Vec::new());
        w.pop();
        w
    };
    // Extension widgets from the vocabulary's handler (custom typed fields).
    if let Some(handler) = crate::category::find_handler(&slug) {
        if let Ok(ext) = handler.ext_widgets(tenant.pool(), id).await {
            fields.extend(ext);
        }
    }
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "taxonomies", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("taxonomy_slug", &tax.slug);
    ctx.insert("taxonomy_name", &tax.verbose_name);
    ctx.insert("mode", "edit");
    ctx.insert("category_id", &id);
    ctx.insert("fields", &fields);
    // #862 — `?locale=<code>` (a non-default active locale) shows the
    // name's translation beside the original, like pages and snippets.
    let locales: Vec<crate::locale::Locale> = crate::locale::Locale::objects()
        .where_(crate::locale::Locale::active.eq(true))
        .order_by(&[("sort_order", false), ("code", false)])
        .fetch(tenant.pool())
        .await?;
    let editing_locale = q
        .locale
        .as_deref()
        .filter(|c| !c.is_empty())
        .and_then(|code| locales.iter().find(|l| l.code == code && !l.is_default).cloned());
    if let Some(loc) = &editing_locale {
        let lid = loc.id.get().copied().unwrap_or_default();
        let mut existing = crate::category_translation::fetch_for_categories(tenant.pool(), &[id], lid)
            .await
            .unwrap_or_default();
        ctx.insert("translation_mode", &true);
        ctx.insert("editing_locale", loc);
        ctx.insert("category_name", &cat.name);
        ctx.insert("tr_name", &existing.remove(&id).and_then(|mut f| f.remove("name")).unwrap_or_default());
    }
    ctx.insert("locales", &locales);
    render_with_csrf(&state, &headers, "rcms_admin/category_form.html", &mut ctx)
}

/// POST /cms-admin/taxonomies/{slug}/categories/{id}/translate?locale=<code>
/// — save a category's per-locale name (#862). An empty box deletes the
/// row, so the original shows again.
pub async fn category_translate_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path((slug, id)): Path<(String, i64)>,
    Query(q): Query<EditQuery>,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    use crate::category_translation::{CategoryTranslation, TRANSLATABLE_FIELDS};
    let code = q
        .locale
        .as_deref()
        .filter(|c| !c.is_empty())
        .ok_or_else(|| AdminError::Validation("translate requires ?locale=".to_owned()))?;
    let locale = crate::locale::Locale::objects()
        .where_(crate::locale::Locale::code.eq(code.to_owned()))
        .where_(crate::locale::Locale::active.eq(true))
        .where_(crate::locale::Locale::is_default.eq(false))
        .first(tenant.pool())
        .await?
        .ok_or_else(|| AdminError::Validation(format!("unknown locale `{code}`")))?;
    let locale_id = locale.id.get().copied().unwrap_or_default();
    let tax = crate::category::find_taxonomy(tenant.pool(), &slug)
        .await
        .map_err(|e| AdminError::Validation(e.to_string()))?
        .ok_or_else(|| AdminError::Validation(format!("unknown taxonomy `{slug}`")))?;
    Category::objects()
        .where_(Category::id.eq(id))
        .where_(Category::taxonomy_id.eq(tax.id.get().copied().unwrap_or_default()))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    rustango::atomic!(tenant.pool(), |tx| {
        let mut guard = tx.lock().await?;
        let tx = &mut *guard;
        for field in TRANSLATABLE_FIELDS {
            let Some(value) = form.get(&format!("tr__{field}")) else {
                continue;
            };
            let value = value.trim().to_owned();
            let existing: Vec<CategoryTranslation> = CategoryTranslation::objects()
                .where_(CategoryTranslation::category_id.eq(id))
                .where_(CategoryTranslation::locale_id.eq(locale_id))
                .where_(CategoryTranslation::field_path.eq(field.to_owned()))
                .fetch_tx(tx)
                .await?;
            if value.is_empty() {
                for row in existing {
                    row.delete_tx(tx).await?;
                }
            } else if let Some(mut row) = existing.into_iter().next() {
                row.value = value;
                row.save_tx(tx).await?;
            } else {
                let mut row = CategoryTranslation {
                    id: rustango::sql::Auto::Unset,
                    category_id: id,
                    locale_id,
                    field_path: field.to_owned(),
                    value,
                    updated_at: rustango::sql::Auto::Unset,
                };
                row.save_tx(tx).await?;
            }
        }
        Ok::<_, rustango::sql::ExecError>(())
    })
    .await?;
    Ok(Redirect::to(&format!("/cms-admin/taxonomies/{slug}/categories/{id}/edit?locale={code}")).into_response())
}

/// POST /cms-admin/taxonomies/{slug}/categories/{id}/edit — update.
pub async fn category_update(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path((slug, id)): Path<(String, i64)>,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let tax = crate::category::find_taxonomy(tenant.pool(), &slug)
        .await
        .map_err(|e| AdminError::Validation(e.to_string()))?
        .ok_or_else(|| AdminError::Validation(format!("unknown taxonomy `{slug}`")))?;
    let tid = tax.id.get().copied().unwrap_or_default();
    let new = category_model::NewCategory {
        name: form
            .get("name")
            .cloned()
            .unwrap_or_default()
            .trim()
            .to_owned(),
        slug: form.get("slug").cloned().unwrap_or_default(),
        description: form.get("description").cloned().unwrap_or_default(),
        featured_image_id: opt_i64_from_form(&form, "featured_image_id"),
        thumbnail_id: opt_i64_from_form(&form, "thumbnail_id"),
        parent_id: opt_i64_from_form(&form, "parent_id"),
    };
    if new.name.is_empty() {
        return category_tree_redirect(
            &slug,
            &headers,
            MsgLevel::Error,
            "Category name is required.",
            &[],
        );
    }
    let new_parent = new.parent_id;
    if let Err(e) = category_model::update(tenant.pool(), tid, id, new).await {
        return category_tree_redirect(&slug, &headers, MsgLevel::Error, &e.to_string(), &[]);
    }
    // Re-parent if the parent changed (cycle-guarded inside move_to;
    // a no-op when the parent is unchanged).
    if let Err(e) = category_model::move_to(tenant.pool(), tid, id, new_parent).await {
        return category_tree_redirect(&slug, &headers, MsgLevel::Error, &e.to_string(), &[]);
    }
    // Extension row (custom vocabularies).
    if let Some(handler) = crate::category::find_handler(&slug) {
        if let Err(e) = handler.save_ext(tenant.pool(), id, &form).await {
            tracing::warn!(category_id = id, error = %e, "category extension save failed");
        }
    }
    category_tree_redirect(&slug, &headers, MsgLevel::Success, "Category saved.", &[])
}

/// POST /cms-admin/taxonomies/{slug}/categories/{id}/delete.
pub async fn category_delete(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path((slug, id)): Path<(String, i64)>,
) -> Result<Response, AdminError> {
    let tax = crate::category::find_taxonomy(tenant.pool(), &slug)
        .await
        .map_err(|e| AdminError::Validation(e.to_string()))?
        .ok_or_else(|| AdminError::Validation(format!("unknown taxonomy `{slug}`")))?;
    let tid = tax.id.get().copied().unwrap_or_default();
    match category_model::delete(tenant.pool(), tid, id, false).await {
        Ok(_) => {
            category_tree_redirect(&slug, &headers, MsgLevel::Success, "Category deleted.", &[])
        }
        Err(CategoryError::HasChildren(_, n)) => {
            let n = n.to_string();
            category_tree_redirect(
                &slug,
                &headers,
                MsgLevel::Error,
                "Can't delete a category with {count} child categor(ies) — delete or move them first.",
                &[("count", n.as_str())],
            )
        }
        Err(e) => category_tree_redirect(&slug, &headers, MsgLevel::Error, &e.to_string(), &[]),
    }
}

// ============================================================
// #559/#562 — Page-type field builder (visual schema constructor)
//
// A Developer authors a page type's body schema in the builder; save
// writes a DRAFT row, publish flips it live (versioned). Gated on the
// `cms_page_type.build` codename at the handler level (superuser bypass).
// ============================================================

/// Deny access to the builder unless the user is a superuser or holds
/// `cms_page_type.build`. Returns `Some(redirect)` when denied (anon →
/// login, otherwise → no-access), `None` when allowed.
async fn ensure_builder_access(
    tenant: &Tenant,
    user: Option<&rustango::tenancy::auth::User>,
) -> Option<Response> {
    super::codename_gate(tenant, user, "cms_page_type.build").await
}

/// GET /cms-admin/page-types/{id}/build — the visual builder.
pub async fn page_type_build_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let pt = crate::page_type_model::PageType::objects()
        .where_(crate::page_type_model::PageType::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;

    let draft = crate::page_builder::model::draft_for(tenant.pool(), id).await?;
    let published = crate::page_builder::model::published_for(tenant.pool(), id).await?;
    // Seed the canvas from the draft, else the live published schema,
    // else an empty document.
    let doc_value = draft
        .as_ref()
        .map(|d| d.document.clone())
        .or_else(|| published.as_ref().map(|p| p.document.clone()))
        .unwrap_or_else(|| serde_json::json!({ "nodes": [] }));
    let schema_json =
        serde_json::to_string(&doc_value).unwrap_or_else(|_| "{\"nodes\":[]}".to_owned());

    let components: Vec<serde_json::Value> =
        crate::page_builder::model::all_components(tenant.pool())
            .await?
            .iter()
            .map(|c| serde_json::json!({ "slug": c.slug, "label": c.label }))
            .collect();
    let components_json = serde_json::to_string(&components).unwrap_or_else(|_| "[]".to_owned());
    let versions: Vec<serde_json::Value> =
        crate::page_builder::model::list_versions(tenant.pool(), id)
            .await?
            .iter()
            .map(|v| {
                serde_json::json!({
                    "version": v.version,
                    "status": v.status,
                    "updated_at": v.updated_at.get().copied(),
                })
            })
            .collect();

    // #566 — deleting a UI-created type is a danger-zone action on this
    // page (not an inline row action). Only for `cms_ui` types, and only
    // while no pages use the type.
    let is_ui_type = pt.app_label == "cms_ui";
    let page_count = if is_ui_type {
        Page::objects()
            .where_(Page::page_type_id.eq(id))
            .fetch(tenant.pool())
            .await
            .map(|v| v.len())
            .unwrap_or(0)
    } else {
        0
    };

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "page-types", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("page_type_id", &id);
    ctx.insert("title", &pt.verbose_name);
    ctx.insert("schema_json", &schema_json);
    ctx.insert("build_url", &format!("/cms-admin/page-types/{id}/build"));
    ctx.insert(
        "publish_url",
        &format!("/cms-admin/page-types/{id}/build/publish"),
    );
    ctx.insert("delete_url", &format!("/cms-admin/page-types/{id}/delete"));
    ctx.insert("list_url", "/cms-admin/page-types");
    ctx.insert("components_json", &components_json);
    ctx.insert("versions", &versions);
    ctx.insert("has_unpublished", &draft.is_some());
    // #863 — the Translate menu: other languages, once the schema is live.
    let translate_locales: Vec<crate::locale::Locale> = if published.is_some() {
        crate::locale::Locale::objects()
            .where_(crate::locale::Locale::active.eq(true))
            .where_(crate::locale::Locale::is_default.eq(false))
            .order_by(&[("sort_order", false), ("code", false)])
            .fetch(tenant.pool())
            .await?
    } else {
        Vec::new()
    };
    ctx.insert("translate_locales", &translate_locales);
    ctx.insert("translate_url", &format!("/cms-admin/page-types/{id}/translate"));
    ctx.insert("is_ui_type", &is_ui_type);
    ctx.insert("page_count", &page_count);
    ctx.insert("deletable", &(is_ui_type && page_count == 0));
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/page_type_builder.html",
        &mut ctx,
    )
}

/// GET /cms-admin/page-types/{id}/translate?locale=<code> — the labels of
/// the type's choice options in another language (#863), side by side
/// with the originals like every other translation screen.
pub async fn page_type_translate_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Query(q): Query<EditQuery>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let pt = crate::page_type_model::PageType::objects()
        .where_(crate::page_type_model::PageType::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let locale = translate_locale(&tenant, q.locale.as_deref()).await?;
    let published = crate::page_builder::model::published_for(tenant.pool(), id).await?;
    let fields = published
        .as_ref()
        .map(|p| crate::page_builder::choice_i18n::choice_fields(&p.document, &locale.code))
        .unwrap_or_default();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "page-types", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("title", &pt.verbose_name);
    ctx.insert("editing_locale", &locale);
    ctx.insert("choice_fields", &fields);
    ctx.insert("build_url", &format!("/cms-admin/page-types/{id}/build"));
    ctx.insert("translate_url", &format!("/cms-admin/page-types/{id}/translate"));
    render_with_csrf(&state, &headers, "rcms_admin/page_type_translate.html", &mut ctx)
}

/// POST /cms-admin/page-types/{id}/translate?locale=<code> — store the
/// option labels (`tr__<field path>__<option value>`) on the published
/// schema and on the draft, if one is open, so publishing it keeps them.
pub async fn page_type_translate_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Query(q): Query<EditQuery>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let locale = translate_locale(&tenant, q.locale.as_deref()).await?;
    let labels: std::collections::HashMap<(String, String), String> = form
        .into_iter()
        .filter_map(|(k, v)| {
            let rest = k.strip_prefix("tr__")?;
            let (path, value) = rest.split_once("__")?;
            Some(((path.to_owned(), value.to_owned()), v))
        })
        .collect();
    for row in [
        crate::page_builder::model::published_for(tenant.pool(), id).await?,
        crate::page_builder::model::draft_for(tenant.pool(), id).await?,
    ]
    .into_iter()
    .flatten()
    {
        let mut row = row;
        if crate::page_builder::choice_i18n::set_choice_labels(&mut row.document, &locale.code, &labels) > 0 {
            row.save_pool(tenant.pool()).await?;
        }
    }
    let back = format!("/cms-admin/page-types/{id}/translate?locale={}", locale.code);
    Ok(rustango::messages::redirect_with_message(
        messages_secret(),
        &headers,
        MsgLevel::Success,
        &super::i18n::tr(&headers, "Translations saved.", &[]),
        &back,
    ))
}

/// The active, non-default locale named by `?locale=`, for the
/// translation screens.
async fn translate_locale(tenant: &Tenant, code: Option<&str>) -> Result<crate::locale::Locale, AdminError> {
    let code = code
        .filter(|c| !c.is_empty())
        .ok_or_else(|| AdminError::Validation("translate requires ?locale=".to_owned()))?;
    crate::locale::Locale::objects()
        .where_(crate::locale::Locale::code.eq(code.to_owned()))
        .where_(crate::locale::Locale::active.eq(true))
        .where_(crate::locale::Locale::is_default.eq(false))
        .first(tenant.pool())
        .await?
        .ok_or_else(|| AdminError::Validation(format!("unknown locale `{code}`")))
}

/// Shared body for save/publish: gate, load the page type, parse +
/// validate the posted schema. Returns the validated document + the
/// problem list, or an early access/redirect / not-found response.
async fn parse_builder_submit(
    tenant: &Tenant,
    user: Option<&rustango::tenancy::auth::User>,
    id: i64,
    schema_str: &str,
) -> Result<Result<(crate::page_builder::Document, Vec<String>), Response>, AdminError> {
    if let Some(r) = ensure_builder_access(tenant, user).await {
        return Ok(Err(r));
    }
    // Ensure the page type exists.
    let exists = crate::page_type_model::PageType::objects()
        .where_(crate::page_type_model::PageType::id.eq(id))
        .first(tenant.pool())
        .await?
        .is_some();
    if !exists {
        return Err(AdminError::NotFound(id));
    }
    let value: serde_json::Value = serde_json::from_str(schema_str)
        .map_err(|e| AdminError::Validation(format!("invalid schema JSON: {e}")))?;
    let doc = crate::page_builder::parse_schema(&value)
        .map_err(|e| AdminError::Validation(format!("invalid schema document: {e}")))?;
    let components = crate::page_builder::model::component_map(tenant.pool()).await?;
    let problems = crate::page_builder::validate_schema(&doc, &components);
    Ok(Ok((doc, problems)))
}

/// POST /cms-admin/page-types/{id}/build — save the schema as a draft.
/// Warnings are non-blocking (surfaced in the flash).
pub async fn page_type_build_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<FormBuildSubmit>,
) -> Result<Response, AdminError> {
    let (doc, problems) =
        match parse_builder_submit(&tenant, session_user.as_ref(), id, &form.schema).await? {
            Ok(v) => v,
            Err(resp) => return Ok(resp),
        };
    let document =
        serde_json::to_value(&doc).unwrap_or_else(|_| serde_json::json!({ "nodes": [] }));
    let created_by = session_user.as_ref().and_then(|u| u.id.get().copied());
    crate::page_builder::model::save_draft(tenant.pool(), id, document, created_by).await?;
    let (level, msg) = if problems.is_empty() {
        (MsgLevel::Success, "Draft saved.".to_owned())
    } else {
        (
            MsgLevel::Warning,
            format!("Draft saved with warnings: {}", problems.join(" ")),
        )
    };
    redirect_named_with_params_and_message(
        "rcms-admin:page-types:build",
        &[("id", id.to_string())],
        level,
        &msg,
        &headers,
    )
}

/// POST /cms-admin/page-types/{id}/build/publish — validate + publish the
/// schema. Validation errors block the publish (draft is still saved).
pub async fn page_type_publish_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<FormBuildSubmit>,
) -> Result<Response, AdminError> {
    let (doc, problems) =
        match parse_builder_submit(&tenant, session_user.as_ref(), id, &form.schema).await? {
            Ok(v) => v,
            Err(resp) => return Ok(resp),
        };
    // Persist what's on screen as the draft first, so a blocked publish
    // still saves the editor's work.
    let document =
        serde_json::to_value(&doc).unwrap_or_else(|_| serde_json::json!({ "nodes": [] }));
    let created_by = session_user.as_ref().and_then(|u| u.id.get().copied());
    crate::page_builder::model::save_draft(tenant.pool(), id, document, created_by).await?;
    if !problems.is_empty() {
        return redirect_named_with_params_and_message(
            "rcms-admin:page-types:build",
            &[("id", id.to_string())],
            MsgLevel::Error,
            &format!("Can't publish — fix these first: {}", problems.join(" ")),
            &headers,
        );
    }
    crate::page_builder::model::publish(tenant.pool(), id).await?;
    redirect_named_with_params_and_message(
        "rcms-admin:page-types:build",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Schema published — the page editor now uses this structure.",
        &headers,
    )
}

/// Persist a page's page-builder body values from a posted form, when
/// its page type has a published schema (#564). Additive to any code
/// handler's `save_extension` — runs regardless of whether a handler
/// exists.
///
/// # Errors
/// A failed schema/component lookup or value write. A type with no
/// published schema, or one that no longer parses, has nothing to save.
pub(crate) async fn save_page_builder_values(
    pool: &rustango::sql::Pool,
    page_type_id: i64,
    page_id: i64,
    form_map: &std::collections::HashMap<String, String>,
) -> Result<(), rustango::sql::ExecError> {
    let Some(row) = crate::page_builder::model::published_for(pool, page_type_id).await? else {
        return Ok(());
    };
    let Ok(doc) = crate::page_builder::parse_schema(&row.document) else {
        return Ok(());
    };
    let components = crate::page_builder::model::component_map(pool).await?;
    let compiled = crate::page_builder::compile(&doc, &components, row.version.max(1) as u32);
    crate::page_builder::values::save_from_form(pool, page_id, &compiled, form_map).await
}

/// GET-side: build the editor HTML + rules island + upgrade flag for a
/// page whose type has a published schema (#564). Returns `None` when
/// the type has no published schema.
async fn page_builder_editor_ctx(
    tenant: &Tenant,
    tera: &tera::Tera,
    page_id: i64,
    page_type_id: i64,
    held: Option<&std::collections::HashMap<String, String>>,
) -> Option<(String, String, bool)> {
    let row = crate::page_builder::model::published_for(tenant.pool(), page_type_id)
        .await
        .ok()??;
    let doc = crate::page_builder::parse_schema(&row.document).ok()?;
    let components = crate::page_builder::model::component_map(tenant.pool())
        .await
        .unwrap_or_default();
    let compiled = crate::page_builder::compile(&doc, &components, row.version.max(1) as u32);
    let (mut values, report) =
        crate::page_builder::values::load_upgraded(tenant.pool(), page_id, &compiled)
            .await
            .unwrap_or_else(|_| (serde_json::json!({}), Default::default()));
    // A change held for review shows in place of the stored values.
    if let Some(form) = held {
        values = serde_json::Value::Object(crate::page_builder::values::build_values(&compiled, form));
    }
    let html =
        crate::page_builder::values::render_body(&compiled, &values, tera);
    // Conditional-rules island: `[{field, rule}]` for page_builder_rules.js.
    let rules: Vec<serde_json::Value> = compiled
        .rules
        .iter()
        .map(|r| serde_json::json!({ "field": r.field, "rule": r.rule }))
        .collect();
    let rules_json = serde_json::to_string(&rules).unwrap_or_else(|_| "[]".to_owned());
    Some((html, rules_json, report.upgraded))
}

// ---- #563 reusable components library -----------------------------------

/// GET /cms-admin/components — the library list (Developer-gated).
pub async fn components_index(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let comps = crate::page_builder::model::all_components(tenant.pool()).await?;
    let mut rows = Vec::with_capacity(comps.len());
    for c in &comps {
        let refs = crate::page_builder::model::component_references(tenant.pool(), &c.slug)
            .await
            .unwrap_or_default();
        rows.push(serde_json::json!({
            "id": c.id.get().copied(),
            "slug": c.slug,
            "label": c.label,
            "icon": if c.icon.is_empty() { "extension".to_owned() } else { c.icon.clone() },
            "description": c.description,
            "version": c.version,
            "ref_count": refs.len(),
        }));
    }
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "components", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("components", &rows);
    ctx.insert("new_url", "/cms-admin/components/new");
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/components_list.html",
        &mut ctx,
    )
}

/// Shared render of the component builder shell (new + edit + error re-render).
#[allow(clippy::too_many_arguments)]
async fn render_component_builder(
    state: &super::AdminState,
    headers: &HeaderMap,
    tenant: &Tenant,
    user: Option<&rustango::tenancy::auth::User>,
    id: Option<i64>,
    slug: &str,
    label: &str,
    icon: &str,
    description: &str,
    schema_json: &str,
    error: Option<&str>,
) -> Result<Response, AdminError> {
    let mut ctx = Context::new();
    add_chrome(&mut ctx, tenant, "components", user).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("component_id", &id);
    ctx.insert("slug", slug);
    ctx.insert("label", label);
    ctx.insert("icon", icon);
    ctx.insert("description", description);
    ctx.insert("schema_json", schema_json);
    ctx.insert("is_edit", &id.is_some());
    let save_url = match id {
        Some(cid) => format!("/cms-admin/components/{cid}/edit"),
        None => "/cms-admin/components/new".to_owned(),
    };
    ctx.insert("save_url", &save_url);
    ctx.insert("list_url", "/cms-admin/components");
    if let Some(e) = error {
        ctx.insert("error", e);
    }
    render_with_csrf(
        state,
        headers,
        "rcms_admin/component_builder.html",
        &mut ctx,
    )
}

/// GET /cms-admin/components/new — empty component builder (fields + rows).
pub async fn component_new_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    render_component_builder(
        &state,
        &headers,
        &tenant,
        session_user.as_ref(),
        None,
        "",
        "",
        "",
        "",
        "{\"nodes\":[]}",
        None,
    )
    .await
}

/// GET /cms-admin/components/{id}/edit — builder seeded from the stored doc.
pub async fn component_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let c = crate::page_builder::model::component_by_id(tenant.pool(), id)
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // The builder JS reads `{nodes:[…]}`; a component doc stores `children`.
    let doc: crate::page_builder::schema::ComponentDoc =
        serde_json::from_value(c.document.clone()).unwrap_or_default();
    let schema_json =
        serde_json::to_string(&serde_json::json!({ "nodes": doc.children })).unwrap_or_default();
    render_component_builder(
        &state,
        &headers,
        &tenant,
        session_user.as_ref(),
        Some(id),
        &c.slug,
        &c.label,
        &c.icon,
        &c.description,
        &schema_json,
        None,
    )
    .await
}

/// Parse the posted `{nodes:[…]}` into a validated [`ComponentDoc`].
/// `Ok(doc)` on success; `Err(problems)` with the editor-facing errors.
fn parse_component_doc(
    schema_str: &str,
) -> Result<crate::page_builder::schema::ComponentDoc, String> {
    let value: serde_json::Value =
        serde_json::from_str(schema_str).map_err(|e| format!("invalid schema JSON: {e}"))?;
    let doc = crate::page_builder::parse_schema(&value)
        .map_err(|e| format!("invalid schema document: {e}"))?;
    let cdoc = crate::page_builder::schema::ComponentDoc {
        label_format: None,
        children: doc.nodes,
    };
    let problems = crate::page_builder::schema::validate_component(&cdoc);
    if problems.is_empty() {
        Ok(cdoc)
    } else {
        Err(problems.join(" "))
    }
}

/// POST /cms-admin/components/new — create a component (version 1).
pub async fn component_create(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(form): Form<FormComponentSubmit>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let slug = crate::forms::schema::slugify_key(&form.slug);
    // Validate slug → label → document, in order; first failure wins.
    let checked: Result<crate::page_builder::schema::ComponentDoc, String> =
        if !crate::forms::schema::is_valid_key(&slug) {
            Err(
                "Slug is invalid — use letters/digits/underscores, starting with a letter."
                    .to_owned(),
            )
        } else if form.label.trim().is_empty() {
            Err("Label is required.".to_owned())
        } else {
            parse_component_doc(&form.schema)
        };
    // On success, create; a unique-slug clash re-shows with a clear error.
    let msg: Option<String> = match checked {
        Ok(cdoc) => {
            let document = serde_json::to_value(&cdoc).unwrap_or_else(|_| serde_json::json!({}));
            match crate::page_builder::model::create_component(
                tenant.pool(),
                &slug,
                form.label.trim(),
                form.icon.trim(),
                form.description.trim(),
                document,
            )
            .await
            {
                Ok(_) => {
                    return redirect_named_with_message(
                        "rcms-admin:components:index",
                        MsgLevel::Success,
                        "Component created.",
                        &headers,
                    )
                }
                Err(_) => Some(format!(
                    "Could not create — a component with slug `{slug}` may already exist."
                )),
            }
        }
        Err(e) => Some(e),
    };
    render_component_builder(
        &state,
        &headers,
        &tenant,
        session_user.as_ref(),
        None,
        &form.slug,
        &form.label,
        &form.icon,
        &form.description,
        &form.schema,
        msg.as_deref(),
    )
    .await
}

/// POST /cms-admin/components/{id}/edit — update + bump version.
pub async fn component_update(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<FormComponentSubmit>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let existing = crate::page_builder::model::component_by_id(tenant.pool(), id)
        .await?
        .ok_or(AdminError::NotFound(id))?;
    if form.label.trim().is_empty() {
        return render_component_builder(
            &state,
            &headers,
            &tenant,
            session_user.as_ref(),
            Some(id),
            &existing.slug,
            &form.label,
            &form.icon,
            &form.description,
            &form.schema,
            Some("Label is required."),
        )
        .await;
    }
    let cdoc = match parse_component_doc(&form.schema) {
        Ok(d) => d,
        Err(e) => {
            return render_component_builder(
                &state,
                &headers,
                &tenant,
                session_user.as_ref(),
                Some(id),
                &existing.slug,
                &form.label,
                &form.icon,
                &form.description,
                &form.schema,
                Some(&e),
            )
            .await
        }
    };
    let document = serde_json::to_value(&cdoc).unwrap_or_else(|_| serde_json::json!({}));
    crate::page_builder::model::update_component(
        tenant.pool(),
        id,
        form.label.trim(),
        form.icon.trim(),
        form.description.trim(),
        document,
    )
    .await?;
    redirect_named_with_message(
        "rcms-admin:components:index",
        MsgLevel::Success,
        "Component saved — referencing page types pick it up on next open.",
        &headers,
    )
}

/// POST /cms-admin/components/{id}/delete — blocked while referenced.
pub async fn component_delete(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let c = crate::page_builder::model::component_by_id(tenant.pool(), id)
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let refs = crate::page_builder::model::component_references(tenant.pool(), &c.slug).await?;
    if !refs.is_empty() {
        // Resolve referencing page-type names for a clear error.
        let ids: Vec<i64> = refs.iter().map(|(pt, _)| *pt).collect();
        let pts: Vec<PageType> = PageType::objects()
            .where_(PageType::id.is_in(ids))
            .fetch(tenant.pool())
            .await
            .unwrap_or_default();
        let names: Vec<String> = pts.iter().map(|p| p.verbose_name.clone()).collect();
        let listed = if names.is_empty() {
            format!("{} page type(s)", refs.len())
        } else {
            names.join(", ")
        };
        return redirect_named_with_message(
            "rcms-admin:components:index",
            MsgLevel::Error,
            &format!("Can't delete `{}` — still used by: {listed}.", c.slug),
            &headers,
        );
    }
    crate::page_builder::model::delete_component(tenant.pool(), id).await?;
    redirect_named_with_message(
        "rcms-admin:components:index",
        MsgLevel::Success,
        "Component deleted.",
        &headers,
    )
}

// ---- #566 UI-created page types (Strapi-style) --------------------------

/// GET /cms-admin/page-types/new — the create-a-type form.
pub async fn page_type_new_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "page-types", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("list_url", "/cms-admin/page-types");
    insert_workflow_choices(&mut ctx, tenant.pool()).await;
    render_with_csrf(&state, &headers, "rcms_admin/page_type_form.html", &mut ctx)
}

/// The active workflows, by name, for the page-type form's Workflow field
/// (#843). A failed lookup leaves only "publish directly".
async fn insert_workflow_choices(ctx: &mut Context, pool: &rustango::sql::Pool) {
    let names: Vec<String> = crate::workflow::Workflow::objects()
        .where_(crate::workflow::Workflow::active.eq(true))
        .order_by(&[("name", false)])
        .fetch(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|w| w.name)
        .collect();
    ctx.insert("workflows", &names);
}

/// A comma-separated list of type identifiers as the JSON array a page-type
/// row stores (empty = no rule).
fn type_list_json(raw: &str) -> serde_json::Value {
    serde_json::Value::Array(
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| serde_json::Value::String(s.to_owned()))
            .collect(),
    )
}

/// An admin-made page type by id; `None` for a missing or code type, whose
/// settings are re-seeded from its handler at every boot.
async fn ui_page_type(
    pool: &rustango::sql::Pool,
    id: i64,
) -> Result<Option<crate::page_type_model::PageType>, AdminError> {
    Ok(crate::page_type_model::PageType::objects()
        .where_(crate::page_type_model::PageType::id.eq(id))
        .first(pool)
        .await?
        .filter(|t| crate::page_type::find_handler(&t.type_name).is_none()))
}

/// GET /cms-admin/page-types/{id}/edit — an admin-made type's settings (#843).
pub async fn page_type_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    Path(id): Path<i64>,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let Some(pt) = ui_page_type(tenant.pool(), id).await? else {
        return Err(AdminError::NotFound(id));
    };
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "page-types", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("list_url", "/cms-admin/page-types");
    ctx.insert("edit_id", &id);
    ctx.insert("form_verbose_name", &pt.verbose_name);
    ctx.insert("form_type_name", &pt.type_name);
    ctx.insert("form_allowed_parents", &pt.allowed_parents().join(", "));
    ctx.insert("form_allowed_children", &pt.allowed_children().join(", "));
    ctx.insert("form_workflow", &pt.workflow);
    insert_workflow_choices(&mut ctx, tenant.pool()).await;
    render_with_csrf(&state, &headers, "rcms_admin/page_type_form.html", &mut ctx)
}

/// POST /cms-admin/page-types/{id}/edit — save an admin-made type's name,
/// parent/child rules and workflow. The identifier can't change.
pub async fn page_type_update(
    tenant: Tenant,
    headers: HeaderMap,
    Path(id): Path<i64>,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(form): Form<FormPageTypeCreate>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let Some(mut pt) = ui_page_type(tenant.pool(), id).await? else {
        return Err(AdminError::NotFound(id));
    };
    if form.verbose_name.trim().is_empty() {
        return Err(AdminError::Validation("Name is required.".to_owned()));
    }
    pt.verbose_name = form.verbose_name.trim().to_owned();
    pt.allowed_parent_types = type_list_json(&form.allowed_parents);
    pt.allowed_child_types = type_list_json(&form.allowed_children);
    pt.workflow = form.workflow.trim().to_owned();
    pt.save_pool(tenant.pool()).await?;
    redirect_named_with_message(
        "rcms-admin:page-types",
        MsgLevel::Success,
        "Page type settings saved.",
        &headers,
    )
}

/// POST /cms-admin/page-types/new — create a UI page type
/// (`app_label = "cms_ui"`) + an empty draft schema, then jump to the
/// field builder so the Developer designs its body immediately.
pub async fn page_type_create(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(form): Form<FormPageTypeCreate>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    // Blank identifier derives from the name — `slugify_key` is a FIELD-key
    // slugifier whose empty-input fallback is the literal "field", which made
    // every no-identifier type `cms_ui.field` (and the second one collide).
    let ident_src = if form.type_name.trim().is_empty() {
        form.verbose_name.as_str()
    } else {
        form.type_name.as_str()
    };
    let slug = crate::forms::schema::slugify_key(ident_src);
    let err: Option<String> = if form.verbose_name.trim().is_empty() {
        Some("Name is required.".to_owned())
    } else if !crate::forms::schema::is_valid_key(&slug) {
        Some(
            "Identifier is invalid — use letters/digits/underscores, starting with a letter."
                .to_owned(),
        )
    } else if crate::page_type_model::PageType::objects()
        .where_(crate::page_type_model::PageType::type_name.eq(slug.clone()))
        .first(tenant.pool())
        .await?
        .is_some()
        || crate::page_type::find_handler(&slug).is_some()
    {
        Some(format!(
            "A page type with identifier `{slug}` already exists."
        ))
    } else {
        None
    };
    if let Some(msg) = err {
        let mut ctx = Context::new();
        add_chrome(&mut ctx, &tenant, "page-types", session_user.as_ref()).await;
        add_admin_theme(&mut ctx, tenant.pool()).await;
        ctx.insert("list_url", "/cms-admin/page-types");
        ctx.insert("error", &msg);
        ctx.insert("form_verbose_name", &form.verbose_name);
        ctx.insert("form_type_name", &form.type_name);
        ctx.insert("form_allowed_parents", &form.allowed_parents);
        ctx.insert("form_allowed_children", &form.allowed_children);
        ctx.insert("form_workflow", &form.workflow);
        insert_workflow_choices(&mut ctx, tenant.pool()).await;
        return render_with_csrf(&state, &headers, "rcms_admin/page_type_form.html", &mut ctx);
    }
    // An API type has no HTML representation, so it gets the blank
    // template sentinel rather than the generic schema template —
    // otherwise `page_view::resolve_kind` would see a template and
    // never reach the JSON branch.
    let view_mode = crate::page_view::PageViewMode::parse(&form.view_mode);
    let default_template = if view_mode == crate::page_view::PageViewMode::Api {
        String::new()
    } else {
        crate::page_builder::DEFAULT_SCHEMA_TEMPLATE.to_owned()
    };
    let mut row = crate::page_type_model::PageType {
        id: rustango::sql::Auto::Unset,
        app_label: "cms_ui".to_owned(),
        type_name: slug.clone(),
        verbose_name: form.verbose_name.trim().to_owned(),
        default_template,
        view_mode: view_mode.as_str().to_owned(),
        is_creatable: true,
        allowed_parent_types: type_list_json(&form.allowed_parents),
        allowed_child_types: type_list_json(&form.allowed_children),
        workflow: form.workflow.trim().to_owned(),
        created_at: rustango::sql::Auto::Unset,
        updated_at: rustango::sql::Auto::Unset,
    };
    row.insert_pool(tenant.pool()).await?;
    let new_id = row.id.get().copied().unwrap_or_default();
    // Seed an empty draft schema so the builder opens cleanly.
    let created_by = session_user.as_ref().and_then(|u| u.id.get().copied());
    crate::page_builder::model::save_draft(
        tenant.pool(),
        new_id,
        serde_json::json!({ "nodes": [] }),
        created_by,
    )
    .await
        .log_warn("empty builder draft not seeded");
    redirect_named_with_params_and_message(
        "rcms-admin:page-types:build",
        &[("id", new_id.to_string())],
        MsgLevel::Success,
        "Page type created — design its body, then Publish.",
        &headers,
    )
}

/// POST /cms-admin/page-types/{id}/delete — delete a UI-created type.
/// Refused for code types and for types with existing pages.
pub async fn page_type_delete(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_builder_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let pt = crate::page_type_model::PageType::objects()
        .where_(crate::page_type_model::PageType::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // Only UI-created types are deletable; code types never are.
    if pt.app_label != "cms_ui" {
        return redirect_named_with_message(
            "rcms-admin:page-types",
            MsgLevel::Error,
            "Only UI-created page types can be deleted.",
            &headers,
        );
    }
    let page_count = Page::objects()
        .where_(Page::page_type_id.eq(id))
        .fetch(tenant.pool())
        .await
        .map(|v| v.len())
        .unwrap_or(0);
    if page_count > 0 {
        return redirect_named_with_message(
            "rcms-admin:page-types",
            MsgLevel::Error,
            "Can't delete — pages still use this type. Delete or move them first.",
            &headers,
        );
    }
    // Drop the type's schema rows (draft/published/archived) then the row.
    for row in crate::page_builder::model::list_versions(tenant.pool(), id)
        .await
        .unwrap_or_default()
    {
        row.delete_pool(tenant.pool()).await.log_warn("page-builder schema version not deleted");
    }
    pt.delete_pool(tenant.pool()).await?;
    redirect_named_with_message(
        "rcms-admin:page-types",
        MsgLevel::Success,
        "Page type deleted.",
        &headers,
    )
}

/// The canonical column of a translation row shows rich text formatted,
/// not as `<p>…</p>` markup. Sanitized: it is author content rendered
/// into the admin. Empty for every other widget kind (shown as text).
fn translation_canonical_html(widget_tag: &str, canonical: &str) -> String {
    if widget_tag == "richtext" && !canonical.trim().is_empty() {
        crate::markdown::sanitize_html(canonical)
    } else {
        String::new()
    }
}

/// Translation-surface groups for a page's builder body (#567): the
/// canonical text leaves (fixed fields, group members, zone stream
/// blocks) grouped like the StreamField leaf rows, each prefilled with
/// its existing per-locale override. Returns `[]` when the type has no
/// published schema. Paths are `builder.<…>` (see `values::…leaves`).
async fn builder_translation_groups(
    tenant: &Tenant,
    page_id: i64,
    page_type_id: i64,
    existing: &std::collections::HashMap<String, String>,
) -> (Vec<serde_json::Value>, std::collections::HashSet<String>) {
    let mut paths = std::collections::HashSet::new();
    let Ok(Some(row)) =
        crate::page_builder::model::published_for(tenant.pool(), page_type_id).await
    else {
        return (Vec::new(), paths);
    };
    let Ok(doc) = crate::page_builder::parse_schema(&row.document) else {
        return (Vec::new(), paths);
    };
    let components = crate::page_builder::model::component_map(tenant.pool())
        .await
        .unwrap_or_default();
    let compiled = crate::page_builder::compile(&doc, &components, row.version.max(1) as u32);
    let (values, _) = crate::page_builder::values::load_upgraded(tenant.pool(), page_id, &compiled)
        .await
        .unwrap_or_else(|_| (serde_json::json!({}), Default::default()));
    let leaves = crate::page_builder::values::builder_translatable_leaves(&compiled, &values);
    // Group consecutively by block instance, mirroring stream_translations.
    let mut groups: Vec<serde_json::Value> = Vec::new();
    let mut cur = String::new();
    for leaf in &leaves {
        paths.insert(leaf.path.clone());
        let rowv = serde_json::json!({
            "path": leaf.path,
            "widget_kind": leaf.widget_kind.as_tag(),
            "field_label": leaf.field_label,
            "canonical": leaf.canonical_text,
            "canonical_html": translation_canonical_html(leaf.widget_kind.as_tag(), &leaf.canonical_text),
            "value": existing.get(&leaf.path).cloned().unwrap_or_default(),
        });
        if leaf.block_id != cur {
            cur.clone_from(&leaf.block_id);
            groups.push(serde_json::json!({
                "block_type": leaf.block_type,
                "block_label": leaf.block_label,
                "block_id": leaf.block_id,
                "depth": leaf.depth,
                "leaves": [rowv],
            }));
        } else if let Some(arr) = groups
            .last_mut()
            .and_then(|g| g.get_mut("leaves"))
            .and_then(|v| v.as_array_mut())
        {
            arr.push(rowv);
        }
    }
    (groups, paths)
}

/// POST /cms-admin/media/bulk-move-collection — move every checked
/// media id into the selected `collection_id` (or NULL for
/// uncategorized).
pub async fn media_bulk_move_collection(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, AdminError> {
    let target: Option<i64> = form
        .iter()
        .find(|(k, _)| k == "collection_id")
        .and_then(|(_, v)| v.parse::<i64>().ok())
        .filter(|id| *id > 0);
    let ids: Vec<i64> = form
        .iter()
        .filter(|(k, _)| k == "ids" || k == "id")
        .filter_map(|(_, v)| v.parse::<i64>().ok())
        .collect();
    let kind = form
        .iter()
        .find(|(k, _)| k == "kind")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "image".to_owned());
    let mut touched = 0;
    for id in ids {
        if let Some(mut m) = Media::objects()
            .where_(Media::id.eq(id))
            .first(tenant.pool())
            .await?
        {
            m.collection_id = target;
            m.save_pool(tenant.pool()).await?;
            touched += 1;
        }
    }
    let redirect_url = if kind == "document" {
        "rcms-admin:documents:list"
    } else {
        "rcms-admin:media:list"
    };
    let dest = match target {
        Some(id) => format!("to collection #{id}"),
        None => "to Uncategorized".to_owned(),
    };
    redirect_named_with_message_plural(
        redirect_url,
        MsgLevel::Success,
        "Moved {count} item(s) {dest}.",
        touched as i64,
        &[("count", &touched.to_string()), ("dest", &dest)],
        &headers,
    )
}

/// GET /cms-admin/media/collections/{id}/permissions — per-collection
/// permission editor (#19). Same role × action grid shape as the
/// page permissions tab, just retargeted at `cms_collection_permission`.
pub async fn collection_permissions_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let collection = crate::media::MediaCollection::objects()
        .where_(crate::media::MediaCollection::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let roles: Vec<rustango::tenancy::permissions::Role> =
        rustango::tenancy::permissions::Role::objects()
            .order_by(&[("name", false)])
            .fetch(tenant.pool())
            .await?;
    let grants: Vec<crate::permissions::CollectionPermission> =
        crate::permissions::CollectionPermission::objects()
            .where_(crate::permissions::CollectionPermission::collection_id.eq(id))
            .fetch(tenant.pool())
            .await?;
    let granted: std::collections::HashSet<(i64, String)> = grants
        .iter()
        .map(|g| (g.role_id, g.permission.clone()))
        .collect();
    let actions: Vec<&str> = crate::permissions::Action::all()
        .iter()
        .map(|a| a.as_str())
        .collect();
    let perm_grid: Vec<serde_json::Value> = roles
        .iter()
        .map(|r| {
            let rid = r.id.get().copied().unwrap_or_default();
            let row_cells: Vec<serde_json::Value> = actions
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "action": a,
                        "granted": granted.contains(&(rid, (*a).to_owned())),
                    })
                })
                .collect();
            serde_json::json!({
                "role_id": rid,
                "role_name": r.name,
                "role_description": r.description,
                "cells": row_cells,
            })
        })
        .collect();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "media", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("collection_id", &id);
    ctx.insert("collection_name", &collection.name);
    ctx.insert("perm_actions", &actions);
    ctx.insert("perm_grid", &perm_grid);

    // #195 — view restriction picker. Surface the current direct
    // restriction (if any) and the role list for the groups option.
    let restriction = crate::collection_view_restriction::direct_for_collection(tenant.pool(), id)
        .await
        .unwrap_or_default();
    let restriction_kind = restriction
        .as_ref()
        .map(|r| r.kind.clone())
        .unwrap_or_else(|| "none".to_owned());
    let restriction_groups: std::collections::HashSet<i64> = restriction
        .as_ref()
        .map(|r| r.parsed_groups().into_iter().collect())
        .unwrap_or_default();
    let restriction_has_password = restriction
        .as_ref()
        .map(|r| !r.password_hash.is_empty())
        .unwrap_or(false);
    let role_options: Vec<serde_json::Value> = roles
        .iter()
        .map(|r| {
            let rid = r.id.get().copied().unwrap_or_default();
            serde_json::json!({
                "id": rid,
                "name": r.name,
                "selected": restriction_groups.contains(&rid),
            })
        })
        .collect();
    // #members — permission-codename picker (the permission-engine gate).
    let restriction_codenames: Vec<String> = restriction
        .as_ref()
        .map(|r| r.parsed_codenames())
        .unwrap_or_default();
    let restriction_codenames_str = restriction_codenames.join(",");
    let codename_choices: Vec<serde_json::Value> = super::resources::membership_codenames()
        .into_iter()
        .map(|(codename, label)| {
            serde_json::json!({
                "codename": codename,
                "label": label,
                "checked": restriction_codenames.contains(&codename),
            })
        })
        .collect();
    ctx.insert("restriction_kind", &restriction_kind);
    ctx.insert("restriction_has_password", &restriction_has_password);
    ctx.insert("restriction_roles", &role_options);
    ctx.insert("restriction_codenames_str", &restriction_codenames_str);
    ctx.insert("restriction_codename_choices", &codename_choices);

    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/collection_permissions.html",
        &mut ctx,
    )
}

/// Form body for `POST /cms-admin/media/collections/{id}/privacy`.
#[derive(Deserialize)]
pub struct CollectionPrivacyForm {
    pub kind: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub group_ids: String,
    /// Comma-separated permission codenames — used when `kind =
    /// permission` (the permission-engine gate). Empty otherwise.
    #[serde(default)]
    pub codenames: String,
}

/// POST /cms-admin/media/collections/{id}/privacy — upsert the view
/// restriction for a collection. `kind=none` deletes the row.
/// Mirrors the page-privacy submit handler (#76) exactly.
pub async fn collection_privacy_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<CollectionPrivacyForm>,
) -> Result<Response, AdminError> {
    use rustango::sql::Auto;
    let existing =
        crate::collection_view_restriction::direct_for_collection(tenant.pool(), id).await?;
    if form.kind == "none" {
        if let Some(row) = existing {
            row.delete_pool(tenant.pool()).await?;
            return redirect_named_with_params_and_message(
                "rcms-admin:media:collection-permissions",
                &[("id", id.to_string())],
                MsgLevel::Success,
                "Removed view restriction — collection is now public.",
                &headers,
            );
        }
        return redirect_named_with_params_and_message(
            "rcms-admin:media:collection-permissions",
            &[("id", id.to_string())],
            MsgLevel::Info,
            "No restriction to remove.",
            &headers,
        );
    }
    let Some(kind) = crate::collection_view_restriction::RestrictionKind::parse(&form.kind) else {
        return Err(AdminError::Validation(format!(
            "unknown restriction kind `{}`",
            form.kind
        )));
    };
    let password_hash = match kind {
        crate::collection_view_restriction::RestrictionKind::Password => {
            if form.password.is_empty() {
                if let Some(row) = &existing {
                    if row.kind == kind.as_str() {
                        row.password_hash.clone()
                    } else {
                        return redirect_named_with_params_and_message(
                            "rcms-admin:media:collection-permissions",
                            &[("id", id.to_string())],
                            MsgLevel::Error,
                            "Password is required when switching the restriction kind to password.",
                            &headers,
                        );
                    }
                } else {
                    return redirect_named_with_params_and_message(
                        "rcms-admin:media:collection-permissions",
                        &[("id", id.to_string())],
                        MsgLevel::Error,
                        "Password is required for password-protected collections.",
                        &headers,
                    );
                }
            } else {
                crate::passwords::hash(&form.password).await
                    .map_err(|e| AdminError::Validation(format!("password hash failed: {e}")))?
            }
        }
        _ => String::new(),
    };
    let group_ids: Vec<i64> = if matches!(
        kind,
        crate::collection_view_restriction::RestrictionKind::Groups
    ) {
        form.group_ids
            .split(',')
            .filter_map(|s| s.trim().parse::<i64>().ok())
            .collect()
    } else {
        Vec::new()
    };
    let group_json = serde_json::json!(group_ids);
    let codenames: Vec<String> = if matches!(
        kind,
        crate::collection_view_restriction::RestrictionKind::Permission
    ) {
        form.codenames
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };
    let codenames_json = serde_json::json!(codenames);
    match existing {
        Some(mut row) => {
            row.kind = kind.as_str().to_owned();
            row.password_hash = password_hash;
            row.group_ids = group_json;
            row.codenames = codenames_json;
            row.save_pool(tenant.pool()).await?;
        }
        None => {
            let mut row = crate::collection_view_restriction::CollectionViewRestriction {
                id: Auto::Unset,
                collection_id: id,
                kind: kind.as_str().to_owned(),
                password_hash,
                group_ids: group_json,
                codenames: codenames_json,
                created_at: Auto::Unset,
                updated_at: Auto::Unset,
            };
            row.insert_pool(tenant.pool()).await?;
        }
    }
    redirect_named_with_params_and_message(
        "rcms-admin:media:collection-permissions",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Collection privacy updated. New requests to /__media__/<spec>/<id> will enforce it.",
        &headers,
    )
}

/// POST /cms-admin/media/collections/{id}/permissions — replace the
/// collection's permission set with the JSON body's grant list.
/// Idempotent want/have diff, returns 204 on success. Mirrors the
/// page-permission save endpoint exactly.
pub async fn collection_permissions_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Path(collection_id): Path<i64>,
    axum::Json(payload): axum::Json<PagePermissionsPayload>,
) -> Result<Response, AdminError> {
    use rustango::sql::Auto;
    let valid_actions: std::collections::HashSet<&str> = crate::permissions::Action::all()
        .iter()
        .map(|a| a.as_str())
        .collect();
    let want: std::collections::BTreeSet<(i64, String)> = payload
        .grants
        .iter()
        .filter(|g| valid_actions.contains(g.action.as_str()))
        .map(|g| (g.role_id, g.action.clone()))
        .collect();

    let existing: Vec<crate::permissions::CollectionPermission> =
        crate::permissions::CollectionPermission::objects()
            .where_(crate::permissions::CollectionPermission::collection_id.eq(collection_id))
            .fetch(tenant.pool())
            .await?;
    let have: std::collections::BTreeSet<(i64, String)> = existing
        .iter()
        .map(|p| (p.role_id, p.permission.clone()))
        .collect();

    for p in &existing {
        let key = (p.role_id, p.permission.clone());
        if !want.contains(&key) {
            p.clone().delete_pool(tenant.pool()).await?;
        }
    }
    for (role_id, action) in &want {
        if !have.contains(&(*role_id, action.clone())) {
            let mut row = crate::permissions::CollectionPermission {
                id: Auto::Unset,
                collection_id,
                role_id: *role_id,
                permission: action.clone(),
                created_at: Auto::Unset,
            };
            row.insert_pool(tenant.pool()).await?;
        }
    }
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

/// JSON payload for the permission tab's bulk save endpoint —
/// the grid POSTs a list of `(role_id, action)` pairs and the
/// handler replaces the page's grant set with that list.
#[derive(Debug, Deserialize)]
pub struct PagePermissionsPayload {
    pub grants: Vec<PagePermissionGrant>,
}
#[derive(Debug, Deserialize)]
pub struct PagePermissionGrant {
    pub role_id: i64,
    pub action: String,
}

/// POST /cms-admin/pages/{id}/permissions — replace the page's
/// permission set with the list in the JSON body. Idempotent;
/// computes the want/have diff and applies the minimum INSERTs +
/// DELETEs. Returns 204 on success.
pub async fn page_permissions_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Path(page_id): Path<i64>,
    axum::Json(payload): axum::Json<PagePermissionsPayload>,
) -> Result<Response, AdminError> {
    use rustango::sql::Auto;
    let valid_actions: std::collections::HashSet<&str> = crate::permissions::Action::all()
        .iter()
        .map(|a| a.as_str())
        .collect();
    let want: std::collections::BTreeSet<(i64, String)> = payload
        .grants
        .iter()
        .filter(|g| valid_actions.contains(g.action.as_str()))
        .map(|g| (g.role_id, g.action.clone()))
        .collect();

    let existing: Vec<crate::permissions::PagePermission> =
        crate::permissions::PagePermission::objects()
            .where_(crate::permissions::PagePermission::page_id.eq(page_id))
            .fetch(tenant.pool())
            .await?;
    let have: std::collections::BTreeSet<(i64, String)> = existing
        .iter()
        .map(|p| (p.role_id, p.permission.clone()))
        .collect();

    // Drop rows that aren't wanted.
    for p in &existing {
        let key = (p.role_id, p.permission.clone());
        if !want.contains(&key) {
            p.clone().delete_pool(tenant.pool()).await?;
        }
    }
    // Insert rows that don't exist yet.
    for (role_id, action) in &want {
        if !have.contains(&(*role_id, action.clone())) {
            let mut row = crate::permissions::PagePermission {
                id: Auto::Unset,
                page_id,
                role_id: *role_id,
                permission: action.clone(),
                created_at: Auto::Unset,
            };
            row.insert_pool(tenant.pool()).await?;
        }
    }

    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

/// Resolve every [`TabSpec`](crate::page_type::TabSpec) variant
/// into the flat shape page_form.html iterates: `{name, label, icon,
/// badge, html}` (#20). Failures on `Template` / `Widgets` rendering
/// fall back to an inline error banner instead of crashing the page
/// editor — the author sees the Tera message + can fix the template.
fn render_extra_tabs(
    specs: Vec<crate::page_type::TabSpec>,
    tera: &std::sync::Arc<tera::Tera>,
    base_ctx: &Context,
    headers: &HeaderMap,
) -> Vec<serde_json::Value> {
    use crate::page_type::TabBody;
    specs
        .into_iter()
        .map(|spec| {
            let html = match spec.body {
                TabBody::Html(s) => s,
                TabBody::Template { path, context } => {
                    let mut tab_ctx = base_ctx.clone();
                    for (k, v) in context {
                        tab_ctx.insert(&k, &v);
                    }
                    tera.render(&path, &tab_ctx).unwrap_or_else(|e| {
                        format!(
                            r#"<div class="rcms-flash rcms-flash--error" style="margin: 12px 0;">
    <span class="rcms-flash-body"><strong>{label}</strong> {failed} <code>{path}</code>: {e}</span>
</div>"#,
                            label = tera::escape_html(&super::i18n::tr(headers, "Tab template error:", &[])),
                            failed = tera::escape_html(&super::i18n::tr(headers, "failed to render", &[])),
                            path = tera::escape_html(&path),
                            e = tera::escape_html(&e.to_string()),
                        )
                    })
                }
                TabBody::Widgets(widgets) => {
                    let mut ctx = Context::new();
                    ctx.insert("widgets", &widgets);
                    tera.render("rcms_admin/_widget_group.html", &ctx)
                        .unwrap_or_else(|e| {
                            format!(
                                r#"<div class="rcms-flash rcms-flash--error" style="margin: 12px 0;">
    <span class="rcms-flash-body"><strong>{label}</strong> {e}</span>
</div>"#,
                                label = tera::escape_html(&super::i18n::tr(headers, "Widget-group tab error:", &[])),
                                e = tera::escape_html(&e.to_string()),
                            )
                        })
                }
                TabBody::Group { heading, help, layout, widgets } => {
                    // Wagtail-parity MultiFieldPanel / FieldRowPanel.
                    // Wrap the same _widget_group output in a
                    // <fieldset> with a heading + (optional) help
                    // paragraph + layout class.
                    let mut ctx = Context::new();
                    ctx.insert("widgets", &widgets);
                    let inner = tera
                        .render("rcms_admin/_widget_group.html", &ctx)
                        .unwrap_or_else(|e| {
                            format!(
                                r#"<div class="rcms-flash rcms-flash--error" style="margin: 12px 0;">
    <span class="rcms-flash-body"><strong>{label}</strong> {e}</span>
</div>"#,
                                label = tera::escape_html(&super::i18n::tr(headers, "Panel-group tab error:", &[])),
                                e = tera::escape_html(&e.to_string()),
                            )
                        });
                    let help_html = if help.is_empty() {
                        String::new()
                    } else {
                        format!(
                            r#"<p class="rcms-hint rcms-panel-group-help">{}</p>"#,
                            tera::escape_html(&help),
                        )
                    };
                    format!(
                        r#"<fieldset class="rcms-panel-group rcms-panel-group--{layout}">
    <legend>{heading}</legend>
    {help_html}
    <div class="rcms-panel-group-body">{inner}</div>
</fieldset>"#,
                        layout = layout.class_suffix(),
                        heading = tera::escape_html(&heading),
                        help_html = help_html,
                        inner = inner,
                    )
                }
                TabBody::Help { content } => {
                    // Static help block. Author-supplied HTML; emit
                    // raw inside a hint-styled container.
                    format!(
                        r#"<div class="rcms-panel-help">{content}</div>"#,
                        content = content,
                    )
                }
            };
            serde_json::json!({
                "name": spec.name,
                "label": spec.label,
                "icon": spec.icon,
                "badge": spec.badge,
                "html": html,
            })
        })
        .collect()
}

/// GET /cms-admin/x/{slug} — dispatcher for #23. Looks up the
/// registered [`AdminPageHandler`](super::admin_page::AdminPageHandler)
/// matching `slug` and forwards to its `render`. Unknown slugs map
/// to 404.
pub async fn admin_page_dispatch(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(slug): Path<String>,
) -> Result<Response, AdminError> {
    match super::admin_page::find_admin_page(&slug) {
        Some(handler) => Ok(handler
            .render(super::admin_page::AdminPageCtx {
                tera: &state.tera,
                tenant: &tenant,
                headers: &headers,
                user: session_user.as_ref(),
            })
            .await),
        None => Err(AdminError::NotFound(0)),
    }
}

/// Load the active admin theme + its brand colors and stuff the
/// rendered CSS / data-attrs into the Tera context so `_base.html`
/// can drop them straight into the page chrome. Falls back gracefully
/// if no theme is configured (admin renders with built-in defaults).
pub(crate) async fn add_admin_theme(ctx: &mut Context, pool: &rustango::sql::Pool) {
    // Pick the row flagged `is_admin_default`; if none, fall back to
    // whichever is `is_default`; if still none, the first row.
    let mut admin_default: Vec<crate::theme::Theme> = crate::theme::Theme::objects()
        .where_(crate::theme::Theme::is_admin_default.eq(true))
        .fetch(pool)
        .await
        .unwrap_or_default();
    let theme = if let Some(t) = admin_default.pop() {
        t
    } else {
        let mut any: Vec<crate::theme::Theme> = crate::theme::Theme::objects()
            .where_(crate::theme::Theme::is_default.eq(true))
            .fetch(pool)
            .await
            .unwrap_or_default();
        match any.pop() {
            Some(t) => t,
            None => {
                let mut first: Vec<crate::theme::Theme> = crate::theme::Theme::objects()
                    .order_by(&[("name", false)])
                    .fetch(pool)
                    .await
                    .unwrap_or_default();
                let Some(t) = first.pop() else {
                    // No themes at all — set empty strings so the
                    // template's `{% if %}` guards short-circuit.
                    ctx.insert("theme_css", &String::new());
                    ctx.insert("theme_slug", &String::new());
                    ctx.insert("theme_default_mode", &"light");
                    ctx.insert("theme_font_url", &String::new());
                    return;
                };
                t
            }
        }
    };
    let theme_id = theme.id.get().copied().unwrap_or_default();
    let brand_colors: Vec<crate::theme::BrandColor> = crate::theme::BrandColor::objects()
        .where_(crate::theme::BrandColor::theme_id.eq(theme_id))
        .order_by(&[("sort_order", false)])
        .fetch(pool)
        .await
        .unwrap_or_default();
    let css = crate::theme::emit_css(&theme, &brand_colors);
    ctx.insert("theme_css", &css);
    ctx.insert("theme_slug", &theme.slug);
    ctx.insert("theme_name", &theme.name);
    ctx.insert("theme_default_mode", &theme.default_mode);
    ctx.insert("theme_font_url", &theme.font_url);
}

/// Query string for the page list / search view.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Filter children of this parent only. Omitted = root listing.
    /// Ignored when `q` is set — search always spans the full tree.
    #[serde(default)]
    pub parent: Option<i64>,
    /// Full-tree search query (#3). Case-insensitive substring match
    /// across title, slug, and url_path. Empty string skips the
    /// search path and shows the current parent's children.
    #[serde(default)]
    pub q: Option<String>,
    /// Filter results by page type slug — pairs with `q` or stands
    /// alone. Matches `cms_page_type.type_name`.
    #[serde(default)]
    pub r#type: Option<String>,
    /// Filter results by status (`draft|scheduled|published|archived`).
    #[serde(default)]
    pub status: Option<String>,
}

/// GET /cms-admin/pages — flat list of pages directly under the
/// current parent (root by default). Editors drill into a subtree
/// via the per-row "View children" action; the breadcrumb trail
/// at the top lets them navigate back up. Replaces the previous
/// indented full-tree view (#2).
///
/// Per-page permissions (#18 phase 3): when the request carries a
/// session cookie, the resolver filters out rows the user can't
/// `view`. Superusers + anonymous / no-session contexts (sqlite
/// dev) see every row.
pub async fn page_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Query(q): Query<ListQuery>,
) -> Result<Response, AdminError> {
    // Best-effort schedule sweep so editors always see current state.
    // #207 — uses the mailer-aware variant so the pre-publish
    // reminder pass runs on the same tick.
    match crate::page::run_schedule_sweep_with_mailer(
        tenant.pool(),
        state.mailer.as_deref(),
        &state.mailer_from,
        &tenant.org.slug,
    )
    .await
    {
        // A page this sweep put live or took down must leave the cache
        // too, as the timed sweep's purge does (#692).
        Ok(result) if !result.changed_urls.is_empty() => {
            crate::task_queue::purge_urls(
                state.cache_invalidator.clone(),
                tenant.org.slug.clone(),
                result.changed_urls,
            )
            .await;
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(target: "rustango_cms::admin", error = %e, "schedule sweep failed"),
    }

    // Pull every page once — we need the full list both for
    // breadcrumb computation (walk ancestors of `parent`) and child
    // counts (how many pages live under each row in the current
    // listing). Tenants stay in the small-N range; the materialized
    // list is cheap.
    let all_pages: Vec<Page> = Page::objects()
        .order_by(&[("path", false), ("sort_order", false)])
        .fetch(tenant.pool())
        .await?;

    // Index pages by id for ancestor walk.
    let by_id: std::collections::HashMap<i64, &Page> = all_pages
        .iter()
        .filter_map(|p| p.id.get().copied().map(|id| (id, p)))
        .collect();

    // Current parent — None means root listing.
    let parent_page: Option<&Page> = q.parent.and_then(|pid| by_id.get(&pid).copied());

    // Breadcrumb: walk parent chain up to root, then reverse.
    let mut breadcrumbs: Vec<serde_json::Value> = Vec::new();
    if let Some(mut cur) = parent_page {
        breadcrumbs.push(serde_json::json!({
            "id": cur.id.get().copied(),
            "title": cur.title.clone(),
        }));
        while let Some(ppid) = cur.parent_id {
            if let Some(p) = by_id.get(&ppid).copied() {
                breadcrumbs.push(serde_json::json!({
                    "id": p.id.get().copied(),
                    "title": p.title.clone(),
                }));
                cur = p;
            } else {
                break;
            }
        }
        breadcrumbs.reverse();
    }

    // Determine the working set:
    //   - search mode (`q`): full tree, filtered by ILIKE on title/slug/url_path
    //   - browse mode (no `q`): direct children of `parent` only
    let search_query =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
    let status_filter = q
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let type_filter_slug = q
        .r#type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    // Per-page permissions filter (#18 phase 3). Skipped when the
    // request is anonymous (sqlite dev / no session) or the user
    // is a tenant superuser — both paths see every page. For
    // authenticated, non-superuser sessions we pre-compute the
    // viewable set so the browse/search filter below can apply it.
    let viewer_user_id: Option<i64> = session_user
        .as_ref()
        .filter(|u| u.active)
        .and_then(|u| u.id.get().copied());
    let viewer_is_superuser = session_user.as_ref().is_some_and(|u| u.is_superuser);
    let permission_filter_enabled = viewer_user_id.is_some() && !viewer_is_superuser;
    let viewable_ids: Option<std::collections::HashSet<i64>> = match viewer_user_id {
        Some(uid) if permission_filter_enabled => {
            let pages: Vec<(i64, Option<i64>)> = all_pages
                .iter()
                .filter_map(|p| p.id.get().copied().map(|id| (id, p.parent_id)))
                .collect();
            Some(
                crate::permissions::viewable_page_ids(tenant.pool(), uid, &pages)
                    .await
                    .unwrap_or_default(),
            )
        }
        _ => None,
    };

    // #408 — relevance-ranked, all-status admin search on Postgres: FTS
    // finds stemmed / word-boundary matches the substring filter misses
    // (across drafts too) and orders by relevance. `None` on non-PG / a
    // query error → the substring filter below stands.
    let fts_rank: Option<std::collections::HashMap<i64, usize>> = match search_query.as_deref() {
        Some(q) => crate::search::search_page_ids_all_statuses(tenant.pool(), q, 1000)
            .await
            .map(|ids| ids.iter().enumerate().map(|(i, id)| (*id, i)).collect()),
        None => None,
    };
    let visible: Vec<&Page> = if search_query.is_some()
        || status_filter.is_some()
        || type_filter_slug.is_some()
    {
        // Search / filter mode — full-tree.
        let q_lower = search_query.as_deref().map(str::to_lowercase);
        // Resolve type slug → id via the not-yet-fetched type list.
        // We pull `cms_page_type` below for the verbose-name map
        // either way; do it inline here for the filter id.
        let prefetch_types: Vec<PageType> = PageType::objects().fetch(tenant.pool()).await?;
        let type_id_by_slug: std::collections::HashMap<String, i64> = prefetch_types
            .iter()
            .filter_map(|t| t.id.get().copied().map(|id| (t.type_name.clone(), id)))
            .collect();
        let type_id_filter = type_filter_slug
            .as_deref()
            .and_then(|s| type_id_by_slug.get(s).copied());
        all_pages
            .iter()
            .filter(|p| {
                // A full-text hit OR a substring match (#702): full-text finds
                // stemmed words, but not a partial word, a slug fragment or a
                // URL path, so on Postgres it must add to the substring match
                // the other backends use, not replace it. Relevance order is
                // the re-sort below; substring-only hits follow the ranked ones.
                let matches_search = match &q_lower {
                    None => true,
                    Some(needle) => {
                        fts_rank.as_ref().is_some_and(|rank| {
                            p.id.get().copied().is_some_and(|id| rank.contains_key(&id))
                        }) || p.title.to_lowercase().contains(needle)
                            || p.slug.to_lowercase().contains(needle)
                            || p.url_path.to_lowercase().contains(needle)
                    }
                };
                let matches_status = match &status_filter {
                    None => true,
                    Some(s) => p.status == *s,
                };
                let matches_type = match type_id_filter {
                    None => true,
                    Some(id) => p.page_type_id == id,
                };
                matches_search && matches_status && matches_type
            })
            .collect()
    } else {
        // Browse mode — direct children only.
        all_pages
            .iter()
            .filter(|p| p.parent_id == q.parent)
            .collect()
    };
    // Snapshot the pre-permission visible count so the empty-state
    // copy can distinguish "tree is empty here" from "you can't see
    // any of these pages" (#70).
    let visible_before_permission_filter = visible.len();
    // Apply per-page view filter as a final pass — affects both
    // search and browse modes uniformly.
    let visible: Vec<&Page> = match &viewable_ids {
        None => visible,
        Some(set) => visible
            .into_iter()
            .filter(|p| p.id.get().copied().is_some_and(|id| set.contains(&id)))
            .collect(),
    };
    // #408 — when FTS drove the search, order results by relevance
    // (overriding the default path order in search mode).
    let visible: Vec<&Page> = if let Some(rank) = &fts_rank {
        let mut v = visible;
        v.sort_by_key(|p| {
            p.id.get()
                .copied()
                .and_then(|id| rank.get(&id).copied())
                .unwrap_or(usize::MAX)
        });
        v
    } else {
        visible
    };
    let is_search_mode =
        search_query.is_some() || status_filter.is_some() || type_filter_slug.is_some();
    // #70 — empty-state classification. Exactly one of these is
    // surfaced into the template when `rows | length == 0`:
    //   * `empty_filtered`         — search/filter mode dropped every row
    //   * `empty_hidden_by_perms`  — per-page permissions dropped every row
    //                                that would otherwise be visible
    //   * (default)                — genuinely empty section
    let empty_filtered = is_search_mode && visible.is_empty();
    let empty_hidden_by_perms = !is_search_mode
        && visible.is_empty()
        && visible_before_permission_filter > 0
        && permission_filter_enabled;

    // Child counts — for the "View children" CTA.
    let mut child_counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for p in &all_pages {
        if let Some(ppid) = p.parent_id {
            *child_counts.entry(ppid).or_insert(0) += 1;
        }
    }

    let types: Vec<PageType> = PageType::objects().fetch(tenant.pool()).await?;
    let type_map: std::collections::HashMap<i64, String> = types
        .iter()
        .filter_map(|t| t.id.get().copied().map(|id| (id, t.verbose_name.clone())))
        .collect();

    let live_urls = build_live_url_map(&all_pages);

    // Helper: walk a page's parent chain into a list of `{id, title}`
    // crumbs (root → page). Only computed in search mode where the
    // editor needs context for results that live deep in the tree.
    let ancestors_for = |p: &Page| -> Vec<serde_json::Value> {
        let mut chain: Vec<serde_json::Value> = Vec::new();
        let mut cur_parent = p.parent_id;
        while let Some(pid) = cur_parent {
            match by_id.get(&pid).copied() {
                Some(ancestor) => {
                    chain.push(serde_json::json!({
                        "id": ancestor.id.get().copied(),
                        "title": ancestor.title.clone(),
                    }));
                    cur_parent = ancestor.parent_id;
                }
                None => break,
            }
        }
        chain.reverse();
        chain
    };

    let rows: Vec<serde_json::Value> = visible
        .iter()
        .map(|p| {
            let id = p.id.get().copied();
            let ancestors = if is_search_mode {
                ancestors_for(p)
            } else {
                Vec::new()
            };
            serde_json::json!({
                "id": id,
                "title": p.title,
                "slug": p.slug,
                "path": p.path,
                "url_path": p.url_path,
                "status": p.status,
                "page_type_id": p.page_type_id,
                "go_live_at": p.go_live_at,
                "expire_at": p.expire_at,
                "updated_at": p.updated_at.get().copied(),
                "published_at": p.published_at,
                "locale_variant_of": p.locale_variant_of,
                "alias_of": p.alias_of,
                "child_count": id.and_then(|i| child_counts.get(&i).copied()).unwrap_or(0),
                "ancestors": ancestors,
            })
        })
        .collect();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "pages", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("rows", &rows);
    ctx.insert("type_map", &type_map);
    // #614 — the "+ New root page" menu must offer only root-legal types.
    ctx.insert("creatable_types", &allowed_root_types(&types));
    ctx.insert("live_urls", &live_urls);
    ctx.insert("breadcrumbs", &breadcrumbs);
    ctx.insert("parent_id", &q.parent);
    ctx.insert("parent_title", &parent_page.map(|p| p.title.clone()));
    ctx.insert("search_query", &search_query);
    ctx.insert("status_filter", &status_filter);
    ctx.insert("type_filter", &type_filter_slug);
    ctx.insert("is_search_mode", &is_search_mode);
    ctx.insert("empty_filtered", &empty_filtered);
    ctx.insert("empty_hidden_by_perms", &empty_hidden_by_perms);
    // For the type/status filter dropdowns the template renders, we
    // pass a slug → verbose-name map and a sorted slug list.
    let mut type_slugs: Vec<(String, String)> = types
        .iter()
        .map(|t| (t.type_name.clone(), t.verbose_name.clone()))
        .collect();
    type_slugs.sort_by(|a, b| a.1.cmp(&b.1));
    ctx.insert("type_choices", &type_slugs);

    // #65 — Move-to destination picker for the bulk action bar.
    // Every page (any depth) is a valid parent except the visible
    // selection itself; we expose the full tree pre-formatted with
    // an indent prefix so the dropdown reads as a tree. JS filters
    // out self/descendants of the actual selection at click time.
    let move_choices: Vec<serde_json::Value> = all_pages
        .iter()
        .map(|p| {
            let indent = "—".repeat(p.depth.max(1) as usize - 1);
            let label = if indent.is_empty() {
                p.title.clone()
            } else {
                format!("{indent} {}", p.title)
            };
            serde_json::json!({
                "id": p.id.get().copied(),
                "label": label,
                "path": p.path.clone(),
            })
        })
        .collect();
    ctx.insert("move_choices", &move_choices);
    // #107 — plugin-contributed per-row buttons. Hand the raw button
    // templates over; the template substitutes `{page_id}` per row.
    ctx.insert(
        "plugin_page_listing_buttons",
        &crate::hooks::page_listing_buttons_raw(),
    );
    // #134 — plugin-contributed bulk actions targeting `cms_page`.
    let plugin_bulk_actions: Vec<serde_json::Value> = crate::hooks::bulk_actions_for("cms_page")
        .into_iter()
        .map(|a| {
            serde_json::json!({
                "key": a.key,
                "label": a.label,
                "icon": a.icon,
                "danger": a.danger,
                "confirm_message": a.confirm_message,
                "action_url": a.action_url,
            })
        })
        .collect();
    ctx.insert("plugin_bulk_actions", &plugin_bulk_actions);
    // #438 — active non-default locales for the copy-to-locale bulk
    // action (the default locale already holds canonical content).
    let copy_locales: Vec<Locale> = Locale::objects()
        .where_(Locale::active.eq(true))
        .order_by(&[("sort_order", false), ("code", false)])
        .fetch(tenant.pool())
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|l| !l.is_default)
        .collect();
    ctx.insert("copy_locales", &copy_locales);
    render_with_csrf(&state, &headers, "rcms_admin/page_list.html", &mut ctx)
}

/// Build a map of `page_id → public URL` for every row in `pages`.
/// Walks the list once in tree order (the list is already sorted by
/// `path` + `sort_order`), so each parent's URL is computed before
/// its children. Root pages with empty slug map to `/`.
/// Reassemble inline-panel rows from the posted form map (#117).
///
/// Form keys are shaped `inline__<panel>__<row_idx>__<field>`. The
/// extractor iterates the spec's fields (so unknown / extraneous
/// keys are ignored), groups them by `row_idx`, and returns rows in
/// `row_idx` order so the handler can persist sort_order from the
/// position in the slice.
///
/// Rows are emitted whenever ANY of the spec's fields appears for
/// that index. The optional `inline__<panel>__<row_idx>____deleted`
/// sentinel hides a row entirely (so the editor can remove rows
/// without re-numbering everything).
fn extract_inline_panel_rows(
    form: &std::collections::HashMap<String, String>,
    spec: &crate::page_type::InlinePanelSpec,
) -> Vec<serde_json::Map<String, serde_json::Value>> {
    let mut by_idx: std::collections::BTreeMap<usize, serde_json::Map<String, serde_json::Value>> =
        std::collections::BTreeMap::new();
    let prefix = format!("inline__{}__", spec.name);
    for (key, value) in form {
        let Some(rest) = key.strip_prefix(&prefix) else {
            continue;
        };
        let Some((idx_str, field_name)) = rest.split_once("__") else {
            continue;
        };
        let Ok(idx) = idx_str.parse::<usize>() else {
            continue;
        };
        if field_name == "__deleted" && value == "1" {
            by_idx.remove(&idx);
            // Mark this index as tombstone — any later entries for
            // it get re-inserted into a fresh row that we then drop
            // at the end. Use a sentinel key to track.
            let mut row = serde_json::Map::new();
            row.insert("__deleted".to_owned(), serde_json::Value::Bool(true));
            by_idx.insert(idx, row);
            continue;
        }
        if !spec.fields.iter().any(|f| f.name == field_name) {
            continue;
        }
        let row = by_idx.entry(idx).or_default();
        if row.contains_key("__deleted") {
            continue;
        }
        row.insert(
            field_name.to_owned(),
            serde_json::Value::String(value.clone()),
        );
    }
    by_idx
        .into_iter()
        .filter_map(|(_, row)| {
            if row.contains_key("__deleted") {
                None
            } else {
                Some(row)
            }
        })
        .collect()
}

fn build_live_url_map(pages: &[Page]) -> std::collections::HashMap<String, String> {
    let mut by_id: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for p in pages {
        let Some(pid) = p.id.get().copied() else {
            continue;
        };
        let url = match p.parent_id {
            None => {
                if p.slug.is_empty() {
                    "/".to_owned()
                } else {
                    format!("/{}", p.slug)
                }
            }
            Some(parent_id) => {
                let parent_url = by_id
                    .get(&parent_id.to_string())
                    .cloned()
                    .unwrap_or_else(|| "/".to_owned());
                if p.slug.is_empty() {
                    parent_url
                } else if parent_url == "/" {
                    format!("/{}", p.slug)
                } else {
                    format!("{}/{}", parent_url, p.slug)
                }
            }
        };
        // Stringify the id key so Tera's `m[page.id]` lookup works
        // when the serializer flattens to JSON.
        by_id.insert(pid.to_string(), url);
    }
    by_id
}

/// GET /cms-admin/pages/new[?parent=<id>][&type=<page_type_id>] —
/// create flow. Two steps:
///
/// 1. Without `?type=`, render the page-type picker — a tile grid of
///    every creatable type (filtered to the parent's `allowed_child_
///    types` when `?parent=` is set). Picking a tile sends the
///    editor to step 2.
/// 2. With `?type=<id>`, render the page form with the chosen type
///    locked. The hidden `page_type_id` input carries it through to
///    the POST; the editor can NOT change it after this point.
///
/// Splitting the type selection out of the form is what makes the
/// "page-type is permanent" guarantee actually visible to authors —
/// once they're filling fields they're committed to the type.
#[derive(Debug, Deserialize)]
pub struct NewQuery {
    #[serde(default)]
    pub parent: Option<i64>,
    #[serde(default, rename = "type")]
    pub type_id: Option<i64>,
}

pub async fn page_new_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Query(q): Query<NewQuery>,
) -> Result<Response, AdminError> {
    let all_types: Vec<PageType> = PageType::objects().fetch(tenant.pool()).await?;

    let (parent, allowed_types) = match q.parent {
        Some(pid) => {
            let parent_row = Page::objects()
                .where_(Page::id.eq(pid))
                .first(tenant.pool())
                .await?
                .ok_or(AdminError::NotFound(pid))?;
            let allowed = allowed_children_for(&parent_row, &all_types);
            (Some(parent_row), allowed)
        }
        // Root: only types that may legally be roots (#614).
        None => (None, allowed_root_types(&all_types)),
    };

    // Step 1: no type picked yet → show the picker tile grid.
    let Some(type_id) = q.type_id else {
        // Enrich each PageType row with the registered handler's
        // `icon()` + `description()` so the picker can show them.
        // Plain `PageType` rows only carry table-level metadata
        // (type_name, verbose_name).
        #[derive(serde::Serialize)]
        struct PickerType<'a> {
            id: i64,
            type_name: &'a str,
            verbose_name: &'a str,
            icon: Option<&'static str>,
            description: Option<&'static str>,
        }
        let allowed_enriched: Vec<PickerType<'_>> = allowed_types
            .iter()
            .map(|t| {
                let handler = find_handler(&t.type_name);
                PickerType {
                    id: t.id.get().copied().unwrap_or_default(),
                    type_name: t.type_name.as_str(),
                    verbose_name: t.verbose_name.as_str(),
                    icon: handler.as_ref().and_then(|h| h.icon()),
                    description: handler.as_ref().and_then(|h| h.description()),
                }
            })
            .collect();

        let mut ctx = Context::new();
        add_chrome(&mut ctx, &tenant, "pages", session_user.as_ref()).await;
        add_admin_theme(&mut ctx, tenant.pool()).await;
        ctx.insert("parent", &parent);
        ctx.insert("allowed_types", &allowed_enriched);
        return render_with_csrf(
            &state,
            &headers,
            "rcms_admin/page_type_picker.html",
            &mut ctx,
        );
    };

    // Step 2: type picked → validate it sits in the allowed set for
    // this parent, then render the form with the type locked. Reject
    // out-of-set selections so a hand-crafted URL can't bypass the
    // parent's whitelist.
    let chosen_type = allowed_types
        .iter()
        .find(|t| t.id.get().copied() == Some(type_id))
        .cloned()
        .ok_or_else(|| {
            AdminError::Validation(format!(
                "page type {type_id} is not creatable under the chosen parent"
            ))
        })?;

    let mut page_seed = empty_page_form_values();
    if let Some(obj) = page_seed.as_object_mut() {
        obj.insert("page_type_id".to_owned(), serde_json::json!(type_id));
    }

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "pages", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("mode", "new");
    ctx.insert("parent", &parent);
    ctx.insert("chosen_type", &chosen_type);
    ctx.insert("page", &page_seed);
    let review_required = !session_user.as_ref().is_some_and(|u| u.is_superuser)
        && crate::workflow::review_workflow_for_type(tenant.pool(), type_id).await?.is_some();
    ctx.insert("review_required", &review_required);
    // Custom tabs (Wagtail-style) — only the existing-page editor
    // calls the handler for them. New-page mode shows the built-in
    // tabs only.
    ctx.insert("extra_tabs", &Vec::<crate::page_type::TabSpec>::new());
    // Stream-field picker options — every registered block surfaces
    // as an option even on the new-page form (so authors can drop
    // blocks before the first save). Empty
    // [`extension_fields`] keeps the new form lean otherwise.
    ctx.insert("extension_fields", &Vec::<crate::widget::Widget>::new());
    ctx.insert(
        "picker_options",
        &crate::block::admin::picker_options(&state.tera),
    );
    ctx.insert("page_tags", &Vec::<String>::new());
    ctx.insert("page_tags_csv", "");
    ctx.insert("all_tags", &Vec::<serde_json::Value>::new());
    insert_category_choices(&mut ctx, tenant.pool(), None).await;
    render_with_csrf(&state, &headers, "rcms_admin/page_form.html", &mut ctx)
}

/// Slugify a raw string for the `cms_page.slug` column. Mirrors the
/// JS-side slugifier in `page_form.html` so the auto-fill the
/// editor sees stays consistent with what the server would have
/// generated.
///
/// Lowercase ASCII, hyphens for whitespace, drop everything else,
/// collapse runs of hyphens, trim leading / trailing hyphens, cap
/// length at 60 chars.
pub(crate) fn slugify(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_hyphen = true;
    for ch in raw.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_hyphen = false;
        } else if !last_hyphen {
            out.push('-');
            last_hyphen = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 60 {
        out.truncate(60);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

/// Pick a slug that doesn't collide with any sibling of
/// `parent_id`. If `candidate` is free, return it unchanged. If
/// taken, try `<base>-copy`, then `<base>-copy-2`, `<base>-copy-3`,
/// … up to a safety limit of 50 attempts (past which we surface an
/// error so the editor sees the collision).
///
/// Roots compare against other roots (`parent_id IS NULL`); children
/// compare against same-parent siblings only.
pub(crate) async fn dedup_slug_under_parent(
    pool: &rustango::sql::Pool,
    parent_id: Option<i64>,
    candidate: &str,
) -> Result<String, AdminError> {
    let taken: std::collections::HashSet<String> = {
        let q = Page::objects();
        let rows = match parent_id {
            Some(pid) => q.where_(Page::parent_id.eq(pid)).fetch(pool).await?,
            None => q.where_(Page::parent_id.is_null()).fetch(pool).await?,
        };
        rows.into_iter().map(|p| p.slug).collect()
    };

    if !taken.contains(candidate) {
        return Ok(candidate.to_owned());
    }
    let with_copy = format!("{candidate}-copy");
    if !taken.contains(&with_copy) {
        return Ok(with_copy);
    }
    for n in 2..50_u32 {
        let trial = format!("{candidate}-copy-{n}");
        if !taken.contains(&trial) {
            return Ok(trial);
        }
    }
    Err(AdminError::Validation(format!(
        "could not find a free slug for “{candidate}” after 50 -copy attempts"
    )))
}

/// Whether a root page already has the empty slug, i.e. serves "/".
async fn front_page_taken(pool: &rustango::sql::Pool) -> Result<bool, AdminError> {
    use rustango::sql::CounterPool as _;
    let n = Page::objects()
        .where_(Page::parent_id.is_null())
        .where_(Page::slug.eq(String::new()))
        .count(pool)
        .await?;
    Ok(n > 0)
}

/// The page editor's Categories field (#842). A failed lookup leaves the
/// field out rather than failing the form.
async fn insert_category_choices(ctx: &mut tera::Context, pool: &rustango::sql::Pool, page_id: Option<i64>) {
    let (groups, checked) = match crate::category::assign::editor_groups(pool, page_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "category choices not loaded; the field is hidden");
            Default::default()
        }
    };
    ctx.insert("category_groups", &groups);
    ctx.insert("page_category_ids", &checked);
}

/// POST /cms-admin/pages/new — submit. Dispatches to
/// `Page::create_root` when no parent_id is in the form, otherwise
/// `Page::create_child`. Both paths compute path/depth/sort_order
/// and enforce the type whitelist.
pub async fn page_new_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(form): Form<PageForm>,
) -> Result<Response, AdminError> {
    // #614 — a type that names `allowed_parent_types` may only live under one
    // of those parents, so it can never be a root. The picker now hides such
    // types at the root, but `page_type_id` arrives in the form (and the
    // picker is reachable as `?type=<id>`), so re-check it here rather than
    // trusting the UI.
    if form.parent_id.is_none() {
        let types = PageType::objects().fetch(tenant.pool()).await?;
        if let Some(row) = types
            .iter()
            .find(|t| t.id.get().copied() == Some(form.page_type_id))
        {
            let parents = row.allowed_parents();
            if !parents.is_empty() {
                return Err(AdminError::Validation(format!(
                    "`{}` can't be a root page — it may only be created under: {}",
                    row.type_name,
                    parents.join(", ")
                )));
            }
        }
    }

    // An empty slug on a root page makes it the front page ("/"), as the
    // form's hint says — unless another root already is; otherwise the
    // slug comes from the title.
    let mut effective_slug = if !form.slug.trim().is_empty() {
        slugify(&form.slug)
    } else if form.parent_id.is_none() && !front_page_taken(tenant.pool()).await? {
        String::new()
    } else {
        slugify(&form.title)
    };
    if !effective_slug.is_empty() {
        effective_slug =
            dedup_slug_under_parent(tenant.pool(), form.parent_id, &effective_slug).await?;
    }

    // A new page that would go live takes the publish right (#761, as on
    // edit), and a type with a review workflow goes live only through
    // it. Decided before the insert and saved as a draft instead, so the
    // page is never live for a moment and nothing typed is lost.
    let intended = derive_scheduled_status(form.parsed_status().as_str(), form.parsed_go_live_at());
    let mut held_back: Option<String> = None;
    if let Some(user) = session_user.as_ref().filter(|u| !u.is_superuser) {
        if crate::page::PageStatus::str_goes_live(crate::page::PageStatus::Draft.as_str(), &intended) {
            let uid = user.id.get().copied().unwrap_or_default();
            let req = super::route_perms::Requirement {
                codename: "cms_page.publish".to_owned(),
                page: form.parent_id.map(|pid| (pid, crate::permissions::Action::Publish)),
            };
            if let Some(wf) = crate::workflow::review_workflow_for_type(tenant.pool(), form.page_type_id).await? {
                held_back = Some(super::i18n::tr(
                    &headers,
                    "Saved as a draft: pages of this type go live after review (“{workflow}”). Click Submit for review.",
                    &[("workflow", wf.name.as_str())],
                ));
            } else if !super::route_perms::allows_id(tenant.pool(), uid, false, &req).await {
                held_back = Some(super::i18n::tr(
                    &headers,
                    "Saved as a draft: you don't have permission to publish pages here.",
                    &[],
                ));
            }
        }
    }

    let new = NewPage::new(
        form.page_type_id,
        form.title.clone(),
        effective_slug.clone(),
    )
    .with_status(if held_back.is_some() { crate::page::PageStatus::Draft } else { form.parsed_status() })
    .with_seo(form.seo_title.clone(), form.seo_description.clone());

    let mut page = match form.parent_id {
        Some(pid) => {
            let parent = Page::objects()
                .where_(Page::id.eq(pid))
                .first(tenant.pool())
                .await?
                .ok_or(AdminError::NotFound(pid))?;
            Page::create_child(&tenant, &parent, new).await?
        }
        None => Page::create_root(&tenant, new).await?,
    };

    // Apply the non-tree-shape fields that NewPage doesn't carry
    // (robots, sitemap_priority) via a follow-up save.
    page.robots_index = form.parsed_robots_index();
    page.sitemap_priority = form.parsed_sitemap_priority();
    page.theme_id = form.theme_id;
    // A held-back page keeps no go-live date: a dated draft would be
    // scheduled, and go live without the review.
    page.go_live_at = if held_back.is_some() { None } else { form.parsed_go_live_at() };
    page.expire_at = form.parsed_expire_at();
    page.status = derive_scheduled_status(&page.status, page.go_live_at);
    page.show_in_menus = form.parsed_show_in_menus();
    page.preview_path = form.preview_path.trim().to_owned();
    page.template_override = form.template_override.trim().to_owned();
    // #183 — social sharing fields.
    page.og_title = form.og_title.clone();
    page.og_description = form.og_description.clone();
    page.og_image_media_id = form.og_image_media_id;
    if let Some(tc) = form.twitter_card.as_deref() {
        if matches!(tc, "summary" | "summary_large_image") {
            page.twitter_card = tc.to_owned();
        }
    }
    page.save_pool(tenant.pool()).await?;
    let new_id = page.id.get().copied().unwrap_or_default();
    // Tags and categories are on the new-page form too; they used to be
    // dropped here, and only an edit saved them.
    if let Some(raw) = form.tags.as_deref().filter(|t| !t.trim().is_empty()) {
        let names: Vec<String> = raw.split(',').map(str::to_owned).collect();
        crate::page_tag::replace_tags(tenant.pool(), new_id, &names).await?;
    }
    crate::category::assign::save_from_form(tenant.pool(), new_id, form.categories.as_deref()).await?;

    // Capture revision #1 for this page so the history
    // panel has something to show from the moment a page is born.
    crate::revision::capture(tenant.pool(), &page, None)
        .await
        .log_warn("revision not captured; the history has a gap");

    // Public render cache (rustango::cache_page::CachePageLayer)
    // expires on TTL; no manual invalidation needed here.
    let message = held_back.unwrap_or_else(|| {
        super::i18n::tr(&headers, "Created page “{title}”.", &[("title", page.title.as_str())])
    });
    redirect_after_page_save(
        &form.action,
        page.id.get().copied().unwrap_or_default(),
        page.parent_id,
        &message,
        &headers,
    )
}

/// GET /cms-admin/pages/{id}/edit — load and render the form.
/// Query string for the page-edit form. `locale=<code>` flips the
/// editor into translation mode for that locale; absent OR pointing
/// at the default locale shows the canonical edit form.
#[derive(Debug, Deserialize, Default)]
pub struct EditQuery {
    #[serde(default)]
    pub locale: Option<String>,
}

pub async fn page_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Query(q): Query<EditQuery>,
) -> Result<Response, AdminError> {
    let page = Page::objects()
        .where_(Page::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;

    // #75 — alias proxy. Editing an alias means editing the source;
    // bounce the editor there so they're never tweaking a row that
    // can't actually accept content changes. Flash explains the
    // redirect so it isn't surprising.
    if let Some(source_id) = page.alias_of {
        return redirect_named_with_params_and_message_args(
            "rcms-admin:pages:edit",
            &[("id", source_id.to_string())],
            MsgLevel::Info,
            "Editing the source — alias “{title}” shares content with this row.",
            &[("title", page.title.as_str())],
            &headers,
        );
    }

    // Load all active locales for the picker + translation lookup.
    let locales: Vec<Locale> = Locale::objects()
        .where_(Locale::active.eq(true))
        .order_by(&[("is_default", false), ("code", false)])
        .fetch(tenant.pool())
        .await?;
    let editing_locale: Option<Locale> = match q.locale.as_deref() {
        Some(code) if !code.is_empty() => locales.iter().find(|l| l.code == code).cloned(),
        _ => None,
    };
    let translation_mode = editing_locale
        .as_ref()
        .map(|l| !l.is_default)
        .unwrap_or(false);
    let pending = crate::pending_change::for_page(tenant.pool(), id).await?;
    let existing_translations = if translation_mode {
        let lid = editing_locale
            .as_ref()
            .and_then(|l| l.id.get().copied())
            .unwrap_or(0);
        crate::translation::fetch_for(tenant.pool(), id, lid)
            .await
            .unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };

    // Theme picker dropdown options.
    let themes_for_picker: Vec<crate::theme::Theme> = crate::theme::Theme::objects()
        .order_by(&[("name", false)])
        .fetch(tenant.pool())
        .await?;

    // Page type is permanent — assigned once at create-time, never
    // reassigned. We surface the type as a read-only chip in the
    // form so authors can still see what they're editing; the edit
    // submit handler also strips `page_type_id` from its canonical
    // keys as a defense-in-depth measure.
    let chosen_type: PageType = PageType::objects()
        .where_(PageType::id.eq(page.page_type_id))
        .first(tenant.pool())
        .await?
        .ok_or_else(|| {
            AdminError::Validation(format!(
                "page_type_id {} on cms_page row has no cms_page_type row",
                page.page_type_id
            ))
        })?;

    // Public URL for the "View live" link in the edit form chrome.
    let ancestors = page.ancestors(tenant.pool()).await.unwrap_or_default();
    let mut segments: Vec<&str> = ancestors
        .iter()
        .filter(|a| !a.slug.is_empty())
        .map(|a| a.slug.as_str())
        .collect();
    if !page.slug.is_empty() {
        segments.push(page.slug.as_str());
    }
    let live_url = if segments.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", segments.join("/"))
    };

    // Fetch the revision history for the sidebar
    // panel. Sorted newest first, capped at 20 for the panel; older
    // revisions stay queryable via direct URL.
    //
    // The cap is applied in SQL (`.limit`), not after the fetch: every
    // `Revision` row carries a full JSON `snapshot` of the page, so
    // fetching the whole history to show 20 grew the edit-form cost with
    // every save (a page edited 300 times pulled 300 page snapshots).
    let revisions: Vec<crate::revision::Revision> = crate::revision::Revision::objects()
        .where_(crate::revision::Revision::page_id.eq(id))
        .order_by(&[("sequence", true)]) // desc
        .limit(20)
        .fetch(tenant.pool())
        .await?;

    // Sort existing translations into "core" (title / seo_title /
    // seo_description) and "extra" so the template can render the
    // core fields side-by-side with the canonical, and list the rest
    // as editable rows below.
    let core_fields = ["title", "seo_title", "seo_description"];
    let core_translations: std::collections::HashMap<String, String> = core_fields
        .iter()
        .map(|f| {
            (
                (*f).to_owned(),
                existing_translations.get(*f).cloned().unwrap_or_default(),
            )
        })
        .collect();
    let mut extra_translations: Vec<(String, String)> = existing_translations
        .iter()
        .filter(|(k, _)| !core_fields.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    extra_translations.sort_by(|a, b| a.0.cmp(&b.0));

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "pages", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("mode", "edit");
    ctx.insert("parent", &Option::<Page>::None);
    ctx.insert("chosen_type", &chosen_type);
    ctx.insert("page", &page);
    ctx.insert("live_url", &live_url);
    // Headless preview: when the tenant has configured a decoupled
    // frontend, mint a token now and hand the editor a link that carries
    // it. The mint endpoint is inside the session-gated admin, so an
    // editor could otherwise only use preview by copying a token by hand
    // — which meant decoupled sites had no working preview at all.
    //
    // Minted per render rather than per click: the token is short-lived
    // and page-scoped, and generating it here keeps the link a plain
    // anchor instead of requiring JS. `None` (no secret configured, or
    // no frontend set) simply hides the action.
    // The site-wide template, which the page may override. A lookup
    // failure is not fatal — it costs the preview action, not the editor.
    let site_preview_base = match crate::preview_url::configured_base(tenant.pool()).await {
        Ok(base) => base,
        Err(e) => {
            tracing::warn!(
                target: "rustango_cms::admin",
                error = %e,
                "headless preview setting lookup failed",
            );
            None
        }
    };
    // Minted only when there is somewhere to send it: a page with no
    // override on a tenant with no frontend has no preview URL at all.
    let headless_preview_url = if site_preview_base.is_some() || !page.preview_path.trim().is_empty()
    {
        crate::preview_token::mint(&tenant.org.slug, id, chrono::Utc::now().timestamp() + PREVIEW_TOKEN_TTL_SECS)
            .and_then(|token| {
                crate::preview_url::for_page(
                    site_preview_base.as_deref(),
                    &page.preview_path,
                    &token,
                    id,
                    &page.url_path,
                )
            })
    } else {
        None
    };
    ctx.insert("headless_preview_url", &headless_preview_url);
    // #532 — full ancestor trail for the editor breadcrumb (each crumb links
    // to that level's children list); plus the immediate parent for "back".
    let ancestor_crumbs: Vec<serde_json::Value> = ancestors
        .iter()
        .map(|a| serde_json::json!({ "id": a.id.get().copied(), "title": a.title }))
        .collect();
    ctx.insert("ancestors", &ancestor_crumbs);
    ctx.insert(
        "parent_crumb",
        &ancestors
            .last()
            .map(|a| serde_json::json!({ "id": a.id.get().copied(), "title": a.title })),
    );
    // #107 — plugin-contributed kebab entries (Wagtail's
    // `register_page_action_menu_item`). Server-side substitutes the
    // page id into each entry's `href_template`.
    ctx.insert(
        "plugin_page_action_menu_items",
        &crate::hooks::page_action_menu_items(id),
    );
    // #140 — plugin-contributed page header buttons (next to Save).
    ctx.insert(
        "plugin_page_header_buttons",
        &crate::hooks::page_header_buttons(id),
    );
    // #115 — is the viewing user subscribed to publish events on
    // this page? Drives the Subscribe / Unsubscribe toggle on the
    // Status field.
    let is_subscribed = match session_user.as_ref().and_then(|u| u.id.get().copied()) {
        Some(uid) => crate::page_subscription::is_subscribed(tenant.pool(), id, uid)
            .await
            .unwrap_or(false),
        None => false,
    };
    ctx.insert("is_subscribed", &is_subscribed);
    // #189 — page tags.
    let page_tags: Vec<String> = crate::page_tag::tags_for_page(tenant.pool(), id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.name)
        .collect();
    let page_tags_csv = page_tags.join(", ");
    ctx.insert("page_tags", &page_tags);
    ctx.insert("page_tags_csv", &page_tags_csv);
    let all_tags = crate::page_tag::all_tags(tenant.pool())
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(name, count)| serde_json::json!({ "name": name, "count": count }))
        .collect::<Vec<_>>();
    ctx.insert("all_tags", &all_tags);
    insert_category_choices(&mut ctx, tenant.pool(), Some(id)).await;
    // #146/#419 — inbound usage ("what links here"), aggregated per
    // referrer with resolved titles + a "used in N places" count.
    // Best-effort; failure leaves the panel empty.
    let usage =
        crate::reference_index::usage_summary(tenant.pool(), crate::reference_index::KIND_PAGE, id)
            .await;
    ctx.insert("usage", &usage);

    // #116 — accessibility heuristics. Run server-side on every
    // page editor open. `body_html` comes from the handler's
    // `load_extension`-rendered HTML when one is available; only the
    // canonical Page fields are checked, since extension shape is
    // per-handler.
    let a11y_violations = crate::a11y::check_page(&page, None);
    let (a11y_errors, a11y_warnings, a11y_infos) = crate::a11y::count_by_severity(&a11y_violations);
    ctx.insert("a11y_violations", &a11y_violations);
    ctx.insert("a11y_errors", &a11y_errors);
    ctx.insert("a11y_warnings", &a11y_warnings);
    ctx.insert("a11y_infos", &a11y_infos);
    // #417 — extensible content/SEO checks (registry-driven). Rendered
    // in a sibling "Content & SEO" panel; same Severity buckets as a11y.
    let mut content_findings = crate::content_checks::run_all(&page);
    // No share image chosen is fine when the page has a photo: the public
    // page uses it (see `meta_tags::fallback_share_image`).
    if content_findings.iter().any(|f| f.code == "missing_og_image")
        && crate::meta_tags::fallback_share_image(tenant.pool(), &page).await.is_some()
    {
        content_findings.retain(|f| f.code != "missing_og_image");
    }
    let (content_errors, content_warnings, content_infos) =
        crate::content_checks::count_by_severity(&content_findings);
    ctx.insert("content_findings", &content_findings);
    ctx.insert("content_errors", &content_errors);
    ctx.insert("content_warnings", &content_warnings);
    ctx.insert("content_infos", &content_infos);
    ctx.insert("revisions", &revisions);

    // #192 — audit-log timeline for this page. Capped at 100 entries
    // for the inline table; the full set is downloadable via the CSV
    // export endpoint linked alongside the table.
    let log_entries = crate::page_log::for_page(tenant.pool(), id, Some(100))
        .await
        .unwrap_or_default();
    let log_user_ids: std::collections::BTreeSet<i64> =
        log_entries.iter().filter_map(|e| e.actor_id).collect();
    let log_users: std::collections::HashMap<i64, String> = if log_user_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        rustango::tenancy::auth::User::objects()
            .where_(
                rustango::tenancy::auth::User::id
                    .is_in(log_user_ids.into_iter().collect::<Vec<_>>()),
            )
            .fetch(tenant.pool())
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|u| u.id.get().copied().map(|id| (id, u.username)))
            .collect()
    };
    let log_view: Vec<serde_json::Value> = log_entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id.get().copied(),
                "action": e.action,
                "actor_id": e.actor_id,
                "actor_username": e.actor_id.and_then(|uid| log_users.get(&uid).cloned()),
                "message": e.message,
                "created_at": e.created_at.get().copied(),
            })
        })
        .collect();
    ctx.insert("page_log", &log_view);

    ctx.insert("locales", &locales);
    ctx.insert("editing_locale", &editing_locale);
    ctx.insert("translation_mode", &translation_mode);
    ctx.insert("core_translations", &core_translations);
    ctx.insert("extra_translations", &extra_translations);
    ctx.insert("themes_for_picker", &themes_for_picker);

    // Extension fields contributed by the page-type handler — typed
    // body / hero / etc. The admin form renders one widget per entry
    // alongside the canonical fields so authors edit "Content" in
    // one place.
    // Resolve the registered handler for this page's type once —
    // both `extension_fields` and `display_fields` come off the
    // same instance.
    // Reuses `chosen_type` (fetched above for the read-only type chip)
    // instead of re-querying the identical `cms_page_type` row.
    let resolved_handler: Option<Box<dyn crate::page_type::PageTypeHandler>> =
        find_handler(&chosen_type.type_name);

    // Which preview surface the pane should render: the iframe, the JSON
    // body, or a plain "not available" notice.
    //
    // "No template" is not just an empty string — a non-empty name that
    // was never registered in Tera fails identically at render time, so
    // both cases have to count or the pane would lazy-load an iframe
    // that can only 500.
    let preview_mode = {
        let kind = crate::page_view::kind_for(&chosen_type);
        let template_renders = !chosen_type.default_template.trim().is_empty()
            && state
                .tera
                .get_template_names()
                .any(|n| n == chosen_type.default_template);
        if kind.serves_json() && !kind.serves_html() {
            "json"
        } else if template_renders {
            "html"
        } else {
            "none"
        }
    };
    ctx.insert("preview_mode", preview_mode);

    // Pull widgets via the rich-typed `widgets()` method, which by
    // default delegates to the legacy `extension_fields()` shape
    // for handlers that haven't migrated. Returns `Vec<Widget>`
    // directly so new handlers can use the full 26-kind catalog
    // (Date, Color, Radio, Checkboxes, MultiSelect, Custom, …).
    let raw_widgets: Vec<crate::widget::Widget> = match &resolved_handler {
        Some(h) => h.widgets(tenant.pool(), id).await.unwrap_or_default(),
        None => Vec::new(),
    };

    // #416 — content metrics (word count / reading time / readability)
    // from the content-bearing widget values, so the editor shows them
    // without opening the preview. Computed before the widgets are
    // mutated for rendering (chooser options etc.).
    let content_metrics =
        crate::metrics::analyze(&crate::metrics::collect_widget_text(&raw_widgets));
    ctx.insert("content_metrics", &content_metrics);

    // #i18n — per-leaf StreamField translation rows. In translation mode,
    // walk each Stream widget's canonical JSON, enumerate every
    // translatable text leaf (recursively, incl. nested blocks), group by
    // block instance, and prefill each from the existing per-locale
    // overrides. `raw_widgets` still holds canonical values here (it is
    // mutated for chooser-option rendering further below).
    if translation_mode {
        let mut stream_translations: Vec<serde_json::Value> = Vec::new();
        let mut leaf_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
        for w in &raw_widgets {
            if !matches!(w.kind, crate::widget::WidgetKind::Stream) {
                continue;
            }
            let base: serde_json::Value = serde_json::from_str(&w.value)
                .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
            let leaves = crate::block::translate::collect_translatable_leaves(&w.name, &base);
            if leaves.is_empty() {
                continue;
            }
            let mut groups: Vec<serde_json::Value> = Vec::new();
            let mut cur_id = String::new();
            for leaf in &leaves {
                leaf_paths.insert(leaf.path.clone());
                let row = serde_json::json!({
                    "path": leaf.path,
                    "widget_kind": leaf.widget_kind.as_tag(),
                    "field_label": leaf.field_label,
                    "canonical": leaf.canonical_text,
                    "canonical_html": translation_canonical_html(leaf.widget_kind.as_tag(), &leaf.canonical_text),
                    "value": existing_translations.get(&leaf.path).cloned().unwrap_or_default(),
                });
                if leaf.block_id != cur_id {
                    cur_id.clone_from(&leaf.block_id);
                    groups.push(serde_json::json!({
                        "block_type": leaf.block_type,
                        "block_label": leaf.block_label,
                        "block_id": leaf.block_id,
                        "depth": leaf.depth,
                        "leaves": [row],
                    }));
                } else if let Some(arr) = groups
                    .last_mut()
                    .and_then(|g| g.get_mut("leaves"))
                    .and_then(|v| v.as_array_mut())
                {
                    arr.push(row);
                }
            }
            stream_translations.push(serde_json::json!({ "field": w.name, "label": w.label, "groups": groups }));
        }
        // A code-made type's own text fields (a home page's heading, …).
        // Flat keys — the field's name — which the renderer overlays onto
        // `extension.<name>` (`translation::overlay_translations`). They
        // were translatable but never listed, so the form could not reach
        // them.
        let field_rows: Vec<serde_json::Value> = raw_widgets
            .iter()
            .filter(|w| {
                use crate::widget::WidgetKind as K;
                matches!(w.kind, K::Text | K::Textarea | K::Markdown | K::RichText)
                    && !w.name.starts_with(crate::page_builder::compile::FIELD_PREFIX)
            })
            .map(|w| {
                leaf_paths.insert(w.name.clone());
                serde_json::json!({
                    "path": w.name,
                    "widget_kind": w.kind.as_tag(),
                    "field_label": w.label,
                    "canonical": w.value,
                    "canonical_html": translation_canonical_html(w.kind.as_tag(), &w.value),
                    "value": existing_translations.get(&w.name).cloned().unwrap_or_default(),
                })
            })
            .collect();
        if !field_rows.is_empty() {
            stream_translations.insert(
                0,
                serde_json::json!({
                    "field": "fields",
                    "label": super::i18n::tr(&headers, "Fields", &[]),
                    "groups": [{
                        "block_type": "fields",
                        "block_label": "",
                        "block_id": "fields",
                        "depth": 0,
                        "leaves": field_rows,
                    }],
                }),
            );
        }
        // #567 — page-builder body leaves (fixed fields, group members,
        // repeater/flex zone blocks). Listed as one more translatable
        // "field" alongside the StreamField leaves, keyed `builder.<…>`.
        let (builder_groups, builder_paths) =
            builder_translation_groups(&tenant, id, page.page_type_id, &existing_translations)
                .await;
        if !builder_groups.is_empty() {
            leaf_paths.extend(builder_paths);
            stream_translations
                .push(serde_json::json!({
                    "field": "builder",
                    "label": super::i18n::tr(&headers, "Fields", &[]),
                    "groups": builder_groups,
                }));
        }
        // Don't also list dotted leaf paths under the raw "custom keys"
        // section (orphaned paths whose block was deleted still appear
        // there so editors can clear them).
        extra_translations.retain(|(k, _)| !leaf_paths.contains(k));
        ctx.insert("stream_translations", &stream_translations);
        ctx.insert("extra_translations", &extra_translations);
    }

    // Computed display chips for the "At a glance" panel above the
    // form. See `PageTypeHandler::display_fields` — handlers can
    // surface word counts / derived metadata / linked-snippet
    // previews without subclassing a panel.
    let display_fields: Vec<crate::page_type::DisplayField> = match &resolved_handler {
        Some(h) => h
            .display_fields(tenant.pool(), id)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    ctx.insert("display_fields", &display_fields);

    // Handler-contributed tabs — appended to the built-in tab strip
    // (Content / Promote / Theme / Revisions). See
    // `PageTypeHandler::extra_tabs`.
    let mut raw_extra_tabs: Vec<crate::page_type::TabSpec> = match &resolved_handler {
        Some(h) => h.extra_tabs(tenant.pool(), id).await.unwrap_or_default(),
        None => Vec::new(),
    };

    // Role list — shared by the Permissions-tab grid below and the
    // Privacy tab's "allowed roles" picker further down. Fetched once:
    // both used to run the same full-table query independently.
    let all_roles: Vec<rustango::tenancy::permissions::Role> =
        rustango::tenancy::permissions::Role::objects()
            .order_by(&[("name", false)])
            .fetch(tenant.pool())
            .await
            .unwrap_or_default();

    // (#18) Permissions tab — every page gets one, regardless of
    // page type. Inject a TabSpec::template entry at the front of
    // the list so it sits right after Theme.
    {
        let grants: Vec<crate::permissions::PagePermission> =
            crate::permissions::PagePermission::objects()
                .where_(crate::permissions::PagePermission::page_id.eq(id))
                .fetch(tenant.pool())
                .await
                .unwrap_or_default();
        // (role_id, action) → granted?
        let granted: std::collections::HashSet<(i64, String)> = grants
            .iter()
            .map(|g| (g.role_id, g.permission.clone()))
            .collect();
        let actions: Vec<&str> = crate::permissions::Action::all()
            .iter()
            .map(|a| a.as_str())
            .collect();
        let perm_grid: Vec<serde_json::Value> = all_roles
            .iter()
            .map(|r| {
                let rid = r.id.get().copied().unwrap_or_default();
                let row_cells: Vec<serde_json::Value> = actions
                    .iter()
                    .map(|a| {
                        serde_json::json!({
                            "action": a,
                            "granted": granted.contains(&(rid, (*a).to_owned())),
                        })
                    })
                    .collect();
                serde_json::json!({
                    "role_id": rid,
                    "role_name": r.name,
                    "role_description": r.description,
                    "cells": row_cells,
                })
            })
            .collect();
        let perm_tab = crate::page_type::TabSpec::template(
            "permissions",
            "Permissions",
            "rcms_admin/_permissions_tab.html",
        )
        .with_icon("admin_panel_settings")
        .with_context_value(
            "perm_actions",
            serde_json::to_value(&actions).unwrap_or_default(),
        )
        .with_context_value(
            "perm_grid",
            serde_json::to_value(&perm_grid).unwrap_or_default(),
        )
        .with_context_value("perm_page_id", serde_json::Value::Number(id.into()));
        raw_extra_tabs.insert(0, perm_tab);
    }
    // (#20) Resolve each `TabSpec` into the flat `{name, label,
    // icon, badge, html}` shape the page_form.html template
    // expects. `Html` variants pass through; `Template` variants
    // render the named Tera template with the standard chrome
    // context + the spec's own context map merged in; `Widgets`
    // variants route through `_widget_group.html` so widget visuals
    // match the Content tab's main stack exactly.
    let extra_tabs: Vec<serde_json::Value> =
        render_extra_tabs(raw_extra_tabs, &state.tera, &ctx, &headers);
    ctx.insert("extra_tabs", &extra_tabs);

    let mut extension_fields: Vec<crate::widget::Widget> = raw_widgets;
    // A change held for review shows in place of the live values.
    let pending_form = pending.as_ref().filter(|_| !translation_mode).map(crate::pending_change::PendingChange::form_map);
    if let Some(form) = pending_form.as_ref() {
        overlay_pending_widgets(&mut extension_fields, form);
    }
    // Pre-render `Custom`-kind widgets via the inventory registry.
    // Tera's `{% include %}` requires a string literal, so we
    // render once on the Rust side and stash the result on each
    // widget — same shape `cms_snippet` uses for its
    // pre-fetched-snippet HTML map.
    for w in &mut extension_fields {
        if matches!(w.kind, crate::widget::WidgetKind::Custom) {
            if let Some(reg) = crate::widget::registry::find_custom_widget(&w.custom_name) {
                let mut sub = Context::new();
                sub.insert("w", &w);
                w.custom_html = state.tera.render(reg.template, &sub).unwrap_or_else(|e| {
                    tracing::warn!(
                        target: "rustango_cms::widget",
                        custom_name = %w.custom_name,
                        template = reg.template,
                        error = %e,
                        "custom widget template render failed",
                    );
                    String::new()
                });
            }
        }
    }

    // -- StreamField pre-render --------------------------------------
    // Walk each `Widget::Stream` once: parse its stored JSON, render
    // every contained block's editor markup + `<template>` clones
    // for each allowed type. The output lives on `w.custom_html`
    // (same stash slot the Custom widget arm uses); the `_widget.html`
    // macro emits it `| safe` for the stream arm.
    for w in &mut extension_fields {
        if matches!(w.kind, crate::widget::WidgetKind::Stream) {
            w.custom_html =
                crate::block::admin::render_stream_editor(w, &state.tera);
        }
    }
    ctx.insert("extension_fields", &extension_fields);

    // #564 — page-builder body. When the page type has a published
    // schema, pre-render its fixed fields/rows/groups + repeater/flex
    // stream editors (prefilled + lazily upgraded) into one HTML blob the
    // Body fieldset emits, plus a conditional-rules island.
    if let Some((body_html, rules_json, upgraded)) = page_builder_editor_ctx(
        &tenant,
        &state.tera,
        id,
        page.page_type_id,
        pending_form.as_ref(),
    )
    .await
    {
        ctx.insert("builder_body_html", &body_html);
        ctx.insert("builder_rules_json", &rules_json);
        ctx.insert("builder_upgraded", &upgraded);
    }

    // #117 — inline panels. Collect each spec the handler declared
    // and the persisted rows behind it. Each panel renders as a
    // section with N rows; per-row fields come from the spec.
    let mut inline_panels: Vec<serde_json::Value> = Vec::new();
    if let Some(h) = resolved_handler.as_deref() {
        for spec in h.inline_panels() {
            let rows = h
                .load_inline_panel(tenant.pool(), id, &spec.name)
                .await
                .unwrap_or_default();
            inline_panels.push(serde_json::json!({
                "spec": spec,
                "rows": rows,
            }));
        }
    }
    ctx.insert("inline_panels", &inline_panels);

    // Picker options — every registered block surfaces as an option in
    // the block-picker dialog inside each stream editor. The dialog
    // filters client-side by `data-allowed` at open time.
    ctx.insert(
        "picker_options",
        &crate::block::admin::picker_options(&state.tera),
    );

    // #72 — editor lock. Best-effort claim on form open: if the page
    // is unlocked or already owned by us, we hold the lock and the
    // form renders normally. If someone else holds an unexpired
    // lock, render the banner with their username + force-unlock
    // affordance; the form itself stays editable (we'll reject the
    // POST unless the user force-unlocks first).
    let mut lock_banner: Option<serde_json::Value> = None;
    let mut lock_owned_by_me = false;
    let reviewing_elsewhere = match session_user.as_ref() {
        Some(u) => read_only_under_review(tenant.pool(), id, u).await?,
        None => false,
    };
    if let Some(viewer_id) = session_user
        .as_ref()
        .and_then(|u| u.id.get().copied())
        .filter(|_| !reviewing_elsewhere)
    {
        match crate::lock::acquire(tenant.pool(), id, viewer_id, false).await {
            Ok(crate::lock::AcquireOutcome::Acquired(_)) => {
                lock_owned_by_me = true;
            }
            Ok(crate::lock::AcquireOutcome::Held(other)) => {
                let holder_name = rustango::tenancy::auth::User::objects()
                    .where_(rustango::tenancy::auth::User::id.eq(other.user_id))
                    .fetch(tenant.pool())
                    .await
                    .ok()
                    .and_then(|v| v.into_iter().next())
                    .map(|u| u.username)
                    .unwrap_or_else(|| format!("user#{}", other.user_id));
                lock_banner = Some(serde_json::json!({
                    "holder_username": holder_name,
                    "holder_id": other.user_id,
                    "acquired_at": other.acquired_at.get().copied(),
                    "expires_at": other.expires_at,
                }));
            }
            Err(e) => {
                tracing::warn!(target: "rustango_cms::admin", page_id = id, error = %e, "lock acquire failed");
            }
        }
    }
    ctx.insert("lock_banner", &lock_banner);
    ctx.insert("lock_owned_by_me", &lock_owned_by_me);
    ctx.insert("lock_heartbeat_secs", &crate::lock::HEARTBEAT_INTERVAL_SECS);

    // #73 PR 2 — workflow context for the action bar + side panel.
    // Surface:
    //   * `workflow_slug` — the slug the page-type opted into (None if direct-publish)
    //   * `workflow` — the resolved Workflow row, if found + active
    //   * `workflow_state` — the active WorkflowState row, if one exists
    //   * `workflow_current_task` — task currently waiting for a decision (when in_progress)
    //   * `workflow_history` — [{task_name, status, decided_by_username, decided_at, comment}, …]
    //   * `workflow_viewer_can_decide` — true when viewer is in the current task's role (or superuser)
    let workflow_slug: Option<String> = chosen_type.workflow_name();
    let mut workflow_ctx: Option<serde_json::Value> = None;
    let mut workflow_state_ctx: Option<serde_json::Value> = None;
    let mut workflow_current_task_ctx: Option<serde_json::Value> = None;
    let mut workflow_history_ctx: Vec<serde_json::Value> = Vec::new();
    let mut workflow_viewer_can_decide = false;
    if let Some(slug) = workflow_slug.as_deref() {
        let workflow = crate::workflow::find_by_name(tenant.pool(), slug)
            .await
            .unwrap_or(None);
        if let Some(wf) = workflow {
            let wf_id = wf.id.get().copied().unwrap_or_default();
            let tasks = crate::workflow::tasks_for(tenant.pool(), wf_id)
                .await
                .unwrap_or_default();
            workflow_ctx = Some(serde_json::json!({
                "id": wf_id,
                "name": wf.name,
                "description": wf.description,
                "task_count": tasks.len(),
            }));
            // The latest round, whatever its outcome: an approved page says
            // so (and can go through review again), not what an earlier
            // round ended with.
            if let Some(state_row) = crate::workflow::latest_state_for_page(tenant.pool(), id)
                .await
                .unwrap_or(None)
            {
                let state_id = state_row.id.get().copied().unwrap_or_default();
                // Current task (when in_progress).
                if let Some(current) = state_row
                    .current_task_id
                    .and_then(|tid| tasks.iter().find(|t| t.id.get().copied() == Some(tid)))
                {
                    workflow_current_task_ctx = Some(serde_json::json!({
                        "id": current.id.get().copied(),
                        "name": current.name,
                        "role_id": current.role_id,
                    }));
                    if let Some(viewer) = session_user.as_ref() {
                        let viewer_id = viewer.id.get().copied().unwrap_or_default();
                        workflow_viewer_can_decide = viewer.is_superuser
                            || user_is_in_role(tenant.pool(), viewer_id, current.role_id)
                                .await
                                .unwrap_or(false);
                    }
                }
                workflow_state_ctx = Some(serde_json::json!({
                    "id": state_id,
                    "status": state_row.status,
                    "requested_by": state_row.requested_by,
                    "requested_at": state_row.requested_at.get().copied(),
                    "finished_at": state_row.finished_at,
                }));
                // Decision history — flatten task name + reviewer username.
                if let Ok(history) = crate::workflow::history_for_page(tenant.pool(), id).await {
                    // Pre-resolve user ids to usernames in one batch.
                    let reviewer_ids: Vec<i64> =
                        history.iter().filter_map(|h| h.decided_by).collect();
                    let reviewers: std::collections::HashMap<i64, String> =
                        if reviewer_ids.is_empty() {
                            std::collections::HashMap::new()
                        } else {
                            rustango::tenancy::auth::User::objects()
                                .where_(rustango::tenancy::auth::User::id.is_in(reviewer_ids))
                                .fetch(tenant.pool())
                                .await
                                .unwrap_or_default()
                                .into_iter()
                                .filter_map(|u| u.id.get().copied().map(|uid| (uid, u.username)))
                                .collect()
                        };
                    let task_name_by_id: std::collections::HashMap<i64, String> = tasks
                        .iter()
                        .filter_map(|t| t.id.get().copied().map(|tid| (tid, t.name.clone())))
                        .collect();
                    workflow_history_ctx = history
                        .iter()
                        .map(|h| {
                            serde_json::json!({
                                "task_name": task_name_by_id
                                    .get(&h.task_id)
                                    .cloned()
                                    .unwrap_or_else(|| format!("task #{}", h.task_id)),
                                "status": h.status,
                                "comment": h.comment,
                                "decided_by_username": h
                                    .decided_by
                                    .and_then(|uid| reviewers.get(&uid).cloned()),
                                "decided_at": h.decided_at,
                            })
                        })
                        .collect();
                }
            }
        }
    }
    // #73 PR 3 — workflow-driven lock. When the page is in_progress
    // AND the viewer is not authorized for the current task (and not
    // a superuser), the form is read-only — the page belongs to
    // whoever is reviewing it right now. Submitter waits for the
    // verdict; non-reviewers can't sneak edits in mid-review.
    let workflow_locked = workflow_state_ctx
        .as_ref()
        .and_then(|s| s.get("status"))
        .and_then(|v| v.as_str())
        .map(|s| s == crate::workflow::WorkflowStatus::InProgress.as_str())
        .unwrap_or(false)
        && !workflow_viewer_can_decide;
    ctx.insert("workflow_slug", &workflow_slug);
    ctx.insert("workflow", &workflow_ctx);
    ctx.insert("workflow_state", &workflow_state_ctx);
    ctx.insert("workflow_current_task", &workflow_current_task_ctx);
    ctx.insert("workflow_history", &workflow_history_ctx);
    ctx.insert("workflow_viewer_can_decide", &workflow_viewer_can_decide);
    ctx.insert("workflow_locked", &workflow_locked);
    let review_required = !session_user.as_ref().is_some_and(|u| u.is_superuser)
        && crate::workflow::review_workflow_for_type(tenant.pool(), page.page_type_id).await?.is_some();
    ctx.insert("review_required", &review_required);
    if let (Some(held), Some(form)) = (pending.as_ref(), pending_form.as_ref()) {
        ctx.insert("page", &overlay_pending_page(&page, form));
        let by = match held.created_by {
            Some(uid) => rustango::tenancy::auth::User::objects()
                .where_(rustango::tenancy::auth::User::id.eq(uid))
                .first(tenant.pool())
                .await?
                .map(|u| u.username),
            None => None,
        };
        ctx.insert(
            "pending_change",
            &serde_json::json!({ "by": by, "updated_at": held.updated_at.get().copied() }),
        );
    }

    // #76 — view-restriction context for the Privacy widget.
    let view_restriction = crate::view_restriction::direct_for_page(tenant.pool(), id)
        .await
        .unwrap_or(None);
    let restriction_kind = view_restriction
        .as_ref()
        .map(|r| r.kind.clone())
        .unwrap_or_else(|| "none".to_owned());
    let restriction_groups = view_restriction
        .as_ref()
        .map(|r| r.parsed_groups())
        .unwrap_or_default();
    let restriction_groups_str = restriction_groups
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // For the role picker dropdown — reuses the `all_roles` list already
    // fetched once above for the Permissions tab.
    let role_choices: Vec<serde_json::Value> = all_roles
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id.get().copied(),
                "name": r.name,
                "checked": r.id.get().copied().is_some_and(|rid| restriction_groups.contains(&rid)),
            })
        })
        .collect();
    // #members — permission-codename picker (the permission-engine gate).
    let restriction_codenames = view_restriction
        .as_ref()
        .map(|r| r.parsed_codenames())
        .unwrap_or_default();
    let restriction_codenames_str = restriction_codenames.join(",");
    let codename_choices: Vec<serde_json::Value> = super::resources::membership_codenames()
        .into_iter()
        .map(|(codename, label)| {
            serde_json::json!({
                "codename": codename,
                "label": label,
                "checked": restriction_codenames.contains(&codename),
            })
        })
        .collect();
    ctx.insert("restriction_kind", &restriction_kind);
    ctx.insert("restriction_groups_str", &restriction_groups_str);
    ctx.insert("restriction_role_choices", &role_choices);
    ctx.insert("restriction_codenames_str", &restriction_codenames_str);
    ctx.insert("restriction_codename_choices", &codename_choices);
    ctx.insert(
        "restriction_has_password",
        &view_restriction
            .as_ref()
            .map(|r| !r.password_hash.is_empty())
            .unwrap_or(false),
    );

    // #81 PR 1 — comments side panel. Fetch every thread on this
    // page + every reply in one batched query, group per thread,
    // resolve author usernames, and surface as JSON for the panel
    // template. Threads with `resolved_at` set render in a
    // collapsed "Resolved" bucket client-side.
    let threads = crate::comment::threads_for_page(tenant.pool(), id)
        .await
        .unwrap_or_default();
    let thread_ids: Vec<i64> = threads.iter().filter_map(|c| c.id.get().copied()).collect();
    let replies_by_thread = crate::comment::replies_for_threads(tenant.pool(), &thread_ids)
        .await
        .unwrap_or_default();
    let mut comment_user_ids: std::collections::BTreeSet<i64> =
        threads.iter().map(|c| c.author_id).collect();
    for replies in replies_by_thread.values() {
        for r in replies {
            comment_user_ids.insert(r.author_id);
        }
    }
    let comment_users: std::collections::HashMap<i64, String> = if comment_user_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        rustango::tenancy::auth::User::objects()
            .where_(rustango::tenancy::auth::User::id.is_in(comment_user_ids.iter().copied()))
            .fetch(tenant.pool())
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|u| u.id.get().copied().map(|uid| (uid, u.username)))
            .collect()
    };
    let comment_threads: Vec<serde_json::Value> = threads
        .iter()
        .map(|c| {
            let tid = c.id.get().copied().unwrap_or_default();
            let replies = replies_by_thread.get(&tid).cloned().unwrap_or_default();
            let rendered_replies: Vec<serde_json::Value> = replies
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id.get().copied(),
                        "author_username": comment_users.get(&r.author_id).cloned(),
                        "body": r.body,
                        "created_at": r.created_at.get().copied(),
                    })
                })
                .collect();
            serde_json::json!({
                "id": tid,
                "field_path": c.field_path,
                "author_username": comment_users.get(&c.author_id).cloned(),
                "body": c.body,
                "created_at": c.created_at.get().copied(),
                "resolved_at": c.resolved_at,
                "replies": rendered_replies,
                "reply_count": replies.len(),
            })
        })
        .collect();
    ctx.insert("comment_threads", &comment_threads);
    ctx.insert(
        "comment_open_count",
        &comment_threads
            .iter()
            .filter(|t| t.get("resolved_at").map(|v| v.is_null()).unwrap_or(true))
            .count(),
    );
    ctx.insert(
        "comment_resolved_count",
        &comment_threads
            .iter()
            .filter(|t| t.get("resolved_at").map(|v| !v.is_null()).unwrap_or(false))
            .count(),
    );
    // For the field-picker dropdown when adding a new comment.
    ctx.insert(
        "comment_field_choices",
        &serde_json::json!([
            {"value": "title", "label": "Title"},
            {"value": "slug", "label": "Slug"},
            {"value": "seo_title", "label": "SEO title"},
            {"value": "seo_description", "label": "SEO description"},
            {"value": "body", "label": "Body / stream content"},
            {"value": "general", "label": "General — whole page"},
        ]),
    );

    render_with_csrf(&state, &headers, "rcms_admin/page_form.html", &mut ctx)
}

/// POST /cms-admin/pages/{id}/edit — apply edits.
/// POST /cms-admin/pages/{id}/autosave — silent revision capture
/// for #147 (Wagtail parity D5). Takes the same form payload as
/// `page_edit_submit` but:
///   - Skips status-promotion logic (draft → published doesn't fire).
///   - Captures a revision row tagged `autosave: true` so the
///     timeline distinguishes autosaves from manual saves.
///   - Does NOT cascade slug / url_path recomputation, redirects,
///     workflow events, cache invalidation, or page_subscription
///     notifications — autosaves are scratch state, not publish
///     events.
///   - Returns 204 No Content so the editor JS treats success as a
///     quiet acknowledgement (no flash banner).
pub async fn page_autosave_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form_map): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    use rustango::sql::FetcherPool as _;
    let mut page = Page::objects()
        .where_(Page::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // Lock-respect — same as the regular save path. Refuse the
    // autosave when someone else holds the lock; the editor JS
    // surfaces this by suspending future autosaves until the user
    // reloads.
    if let Some(viewer_id) = session_user.as_ref().and_then(|u| u.id.get().copied()) {
        if let Ok(Some(lock)) = crate::lock::current(tenant.pool(), id).await {
            if lock.user_id != viewer_id {
                return Ok((axum::http::StatusCode::CONFLICT, "locked").into_response());
            }
        }
    }
    // Autosave writes the page row itself, so it only touches drafts:
    // on a live (or scheduled) page it would publish half-typed edits,
    // skipping the publish right and any review. The real Save handles
    // those pages; here nothing is written.
    if page.status != crate::page::PageStatus::Draft.as_str() {
        return Ok((axum::http::StatusCode::NO_CONTENT, "").into_response());
    }
    if let Some(u) = session_user.as_ref() {
        if read_only_under_review(tenant.pool(), id, u).await? {
            return Ok((axum::http::StatusCode::CONFLICT, "under review").into_response());
        }
    }
    // Overlay mutable fields. Tree placement (parent / path /
    // url_path / depth / sort_order) intentionally left alone —
    // autosaves never move the page. That includes the slug: writing it
    // here without url_path made the next real save see "no slug change",
    // so the rename never cascaded to url_path, children or a 301 (#679).
    // The edited slug stays in the form and the real save applies it.
    if let Some(v) = form_map.get("title") {
        page.title = v.clone();
    }
    if let Some(v) = form_map.get("seo_title") {
        page.seo_title = v.clone();
    }
    if let Some(v) = form_map.get("seo_description") {
        page.seo_description = v.clone();
    }
    // Skip status promotion. If the editor changed status, we
    // record it for the diff but don't fire any publish-side
    // effects.
    let posted_status = form_map.get("status").cloned();
    page.save_pool(tenant.pool()).await?;
    // Persist extension fields via handler (idempotent upserts).
    let pts: Vec<PageType> = PageType::objects()
        .where_(PageType::id.eq(page.page_type_id))
        .fetch(tenant.pool())
        .await?;
    if let Some(pt) = pts.into_iter().next() {
        if let Some(handler) = find_handler(&pt.type_name) {
            let ext_form =
                sanitized_extension_form(handler.as_ref(), tenant.pool(), id, &form_map).await;
            handler.save_extension(tenant.pool(), id, &ext_form).await?;
        }
    }
    // #564 — page-builder body values (additive to any code handler).
    save_page_builder_values(tenant.pool(), page.page_type_id, id, &form_map).await?;
    // Capture an autosave-flagged revision. Failure here is
    // best-effort.
    let by = session_user.as_ref().and_then(|u| u.id.get().copied());
    if let Ok(mut rev) = crate::revision::capture(tenant.pool(), &page, by).await {
        // Inject the autosave marker into the snapshot before
        // re-saving the row. The snapshot column is a plain
        // serde_json::Value so we can mutate + re-persist.
        if let serde_json::Value::Object(ref mut map) = rev.snapshot {
            map.insert("__autosave".to_owned(), serde_json::Value::Bool(true));
            if let Some(status) = posted_status {
                map.insert(
                    "__autosave_posted_status".to_owned(),
                    serde_json::Value::String(status),
                );
            }
            rev.save_pool(tenant.pool()).await.log_warn("autosave revision not stored");
        }
    }
    Ok((axum::http::StatusCode::NO_CONTENT, "").into_response())
}

/// Allow-list of POST keys that `page_edit_submit` re-feeds into
/// [`PageForm`]. The submit handler filters the incoming form map
/// through this list to drop extension-field entries the page type
/// handler owns. Every field [`PageForm`] consumes — and every field
/// the body of `page_edit_submit` reads off the parsed form — MUST
/// be listed here, or the value is silently dropped on save.
///
/// `page_type_id` is intentionally absent: type is assigned at
/// create-time and is permanent (see comment in the handler).
/// Page fields the editor shows as text, overlaid from a held change.
const PENDING_TEXT_KEYS: &[&str] = &[
    "title",
    "slug",
    "seo_title",
    "seo_description",
    "og_title",
    "og_description",
    "preview_path",
];

/// `page` as the editor shows it with a held change on top: its text
/// fields take the proposed values.
fn overlay_pending_page(
    page: &Page,
    form: &std::collections::HashMap<String, String>,
) -> serde_json::Value {
    let mut value = serde_json::to_value(page).unwrap_or_default();
    if let Some(obj) = value.as_object_mut() {
        for key in PENDING_TEXT_KEYS {
            if let Some(v) = form.get(*key) {
                obj.insert((*key).to_owned(), serde_json::Value::String(v.clone()));
            }
        }
    }
    value
}

/// Give each widget its value from a held change. An unticked checkbox
/// isn't posted, so a Boolean the form lacks is off.
fn overlay_pending_widgets(
    widgets: &mut [crate::widget::Widget],
    form: &std::collections::HashMap<String, String>,
) {
    for w in widgets {
        match form.get(&w.name) {
            Some(v) => w.value.clone_from(v),
            None if matches!(w.kind, crate::widget::WidgetKind::Boolean) => w.value.clear(),
            None => {}
        }
    }
}

const PAGE_FORM_CANONICAL_KEYS: &[&str] = &[
    "title",
    "slug",
    "status",
    "seo_title",
    "seo_description",
    "robots_index",
    "sitemap_priority",
    "parent_id",
    "theme_id",
    // #207 — schedule + auto-archive.
    "go_live_at",
    "expire_at",
    // #22 — surface in navigation editor.
    "show_in_menus",
    // Headless preview — per-page frontend route override.
    "preview_path",
    // Per-page template, overriding the page type's.
    "template_override",
    // #183 — social sharing.
    "og_title",
    "og_description",
    "og_image_media_id",
    "twitter_card",
    // #189 — page tags (comma-separated).
    "tags",
];

/// The acting user for [`apply_page_edit`] — carries exactly what the
/// lock / workflow / attribution logic needs, decoupled from the HTTP
/// session type so the MCP tools can act as a key's owner.
pub(crate) struct PageEditActor {
    pub id: i64,
    pub username: String,
    pub is_superuser: bool,
}

/// A save refused for a *policy* reason (not an error): the caller
/// renders it in its own shape — the HTTP handler as a flash+redirect,
/// an MCP tool as a structured tool error.
pub(crate) enum PageEditRefusal {
    /// Page is mid-review on a workflow step the actor can't edit.
    WorkflowReview { task_name: String },
    /// Another editor holds the edit lock.
    LockedBy { holder: String },
    /// Archived pages can't be unpublished in place (#556).
    ArchivedUnpublish,
    /// The save would put the page live and the actor lacks the publish
    /// right (#761).
    PublishDenied,
    /// The save would put the page live, but pages of its type go live
    /// through the named review workflow.
    ReviewRequired { workflow: String },
}

/// What a successful [`apply_page_edit`] produced, for caller messaging.
pub(crate) struct PageEditOutcome {
    pub page: Page,
    pub redirects_created: usize,
    pub reapproval_kicked: bool,
    /// The save was kept for review; the live page is unchanged.
    pub held_for_review: bool,
}

/// Apply one page edit end-to-end from a canonical+extension form map —
/// the single write path behind both `page_edit_submit` (HTTP) and the
/// MCP `update_page`/`publish_page` tools. Covers: workflow + editor
/// lock policy, the canonical-key overlay (`PAGE_FORM_CANONICAL_KEYS`),
/// scheduled-status derivation + publish timestamps, the atomic tree
/// save with url-path cascade + on-commit cache purges, tags, revision
/// capture (attributed to `actor`), reference reindex, search sync,
/// publish hooks + page log, extension + inline-panel + page-builder
/// value persistence, auto-301s on slug moves, redirect auto-disable on
/// unpublish, and reapproval-on-edit. `notify = false` skips the
/// publish subscription mail (MCP v1). Flash/redirect rendering stays
/// with the HTTP handler.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_page_edit(
    pool: &rustango::sql::Pool,
    org_slug: &str,
    invalidator: &std::sync::Arc<dyn crate::cache_invalidate::PageCacheInvalidator>,
    mailer: Option<&dyn rustango::email::Mailer>,
    mailer_from: &str,
    notify: bool,
    id: i64,
    form_map: &std::collections::HashMap<String, String>,
    actor: Option<&PageEditActor>,
) -> Result<Result<PageEditOutcome, PageEditRefusal>, AdminError> {
    // #73 PR 3 — workflow lock. While the page is in_progress on a
    // workflow, only members of the current task's role (and
    // superusers) can save edits. Submitter waits for the verdict;
    // non-reviewers can't sneak fixes in mid-review. Checked BEFORE
    // the editor lock — workflow restrictions outrank concurrency.
    if let Some(actor) = actor {
        if let Some(active) = crate::workflow::active_state_for_page(pool, id).await? {
            if active.status == crate::workflow::WorkflowStatus::InProgress.as_str()
                && !actor.is_superuser
            {
                let tasks = crate::workflow::tasks_for(pool, active.workflow_id).await?;
                if let Some(current_task) = active
                    .current_task_id
                    .and_then(|tid| tasks.iter().find(|t| t.id.get().copied() == Some(tid)))
                {
                    if !user_is_in_role(pool, actor.id, current_task.role_id).await? {
                        return Ok(Err(PageEditRefusal::WorkflowReview {
                            task_name: current_task.name.clone(),
                        }));
                    }
                }
            }
        }
    }

    // #72 — refuse the write if the page is locked by someone else.
    // The GET path acquires the lock for the viewer; if they got
    // through, this is a no-op. If a second editor saves anyway
    // (race or curl), they hit a 409-flavored redirect with a flash
    // explaining who holds the lock. Force-unlock is a separate
    // POST so accidental clobbers stay impossible.
    if let Some(actor) = actor {
        match crate::lock::current(pool, id).await {
            Ok(Some(lock)) if lock.user_id != actor.id => {
                let holder = rustango::tenancy::auth::User::objects()
                    .where_(rustango::tenancy::auth::User::id.eq(lock.user_id))
                    .fetch(pool)
                    .await
                    .ok()
                    .and_then(|v| v.into_iter().next())
                    .map(|u| u.username)
                    .unwrap_or_else(|| format!("user#{}", lock.user_id));
                return Ok(Err(PageEditRefusal::LockedBy { holder }));
            }
            _ => {}
        }
    }

    // Re-deserialize as PageForm using only the canonical keys. The
    // form map also carries extension-field entries (typed body /
    // hero / etc.) the type's handler is interested in; we hand that
    // raw HashMap to `save_extension` after the canonical save.
    //
    // `page_type_id` is *not* in this list — page type is assigned at
    // create-time and is permanent. Any value posted is ignored. The
    // template hides the selector and the handler defensively drops
    // the key here. Caller scenarios that *must* change a page's
    // type can delete + recreate (preserves slug history via
    // `cms_redirect`).
    let canonical_keys = PAGE_FORM_CANONICAL_KEYS;
    let mut canonical_only: std::collections::HashMap<&str, String> = form_map
        .iter()
        .filter(|(k, _)| canonical_keys.contains(&k.as_str()))
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();

    let mut page = Page::objects()
        .where_(Page::id.eq(id))
        .first(pool)
        .await?
        .ok_or(AdminError::NotFound(id))?;

    // Re-inject the immutable `page_type_id` from the persisted row
    // so `PageForm`'s deserializer is satisfied without trusting
    // anything the client posted.
    let frozen_type_id = page.page_type_id.to_string();
    canonical_only.insert("page_type_id", frozen_type_id);
    let canonical_view: std::collections::HashMap<&str, &str> = canonical_only
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .collect();
    let form: PageForm = serde_urlencoded::from_str(
        &serde_urlencoded::to_string(&canonical_view).unwrap_or_default(),
    )
    .map_err(|e| AdminError::Validation(format!("canonical form: {e}")))?;

    // A live page of a type whose review workflow asks for re-approval
    // on edit: a non-superuser's save waits for that review instead of
    // changing what visitors see. The final approval applies it (a save
    // with no actor, so it isn't held again).
    if let Some(actor) = actor.filter(|a| !a.is_superuser) {
        if page.status == crate::page::PageStatus::Published.as_str() {
            if let Some(wf) = crate::workflow::review_workflow_for_type(pool, page.page_type_id)
                .await?
                .filter(|wf| wf.require_reapproval_on_edit)
            {
                crate::pending_change::hold(pool, id, form_map, Some(actor.id)).await?;
                let mut started = false;
                if crate::workflow::active_state_for_page(pool, id).await?.is_none() {
                    let tasks =
                        crate::workflow::tasks_for(pool, wf.id.get().copied().unwrap_or_default())
                            .await?;
                    crate::workflow::submit_for_review(pool, id, &wf, &tasks, actor.id).await?;
                    started = true;
                }
                return Ok(Ok(PageEditOutcome {
                    page,
                    redirects_created: 0,
                    reapproval_kicked: started,
                    held_for_review: true,
                }));
            }
        }
    }

    page.title = form.title.clone();
    // #703 — the same normalisation a new page gets, and no two siblings
    // on one slug: they would share a URL and serve whichever row the
    // driver returned. The pattern attribute alone is bypassed by any
    // direct POST (and by MCP update_page, which lands here).
    let new_slug = if form.slug.trim().is_empty() {
        String::new()
    } else {
        slugify(&form.slug)
    };
    let slug_changed = page.slug != new_slug;
    if slug_changed && !new_slug.is_empty() {
        let sibling = Page::objects()
            .where_(Page::slug.eq(new_slug.clone()))
            .where_(Page::id.ne(id));
        let sibling = match page.parent_id {
            Some(pid) => sibling.where_(Page::parent_id.eq(pid)),
            None => sibling.where_(Page::parent_id.is_null()),
        };
        if sibling.first(pool).await?.is_some() {
            return Err(AdminError::Validation(format!(
                "another page under the same parent already uses the slug “{new_slug}”"
            )));
        }
    }
    // Capture the OLD url_path BEFORE the recomputation block below
    // overwrites it. Used after commit to evict the now-stale cache
    // entry under the page's previous URL.
    let old_url_path = page.url_path.clone();
    page.slug = new_slug;
    // #553 — remember whether this save takes the page out of
    // Published, so linked redirect rules can be auto-disabled below.
    let was_published = page.status == crate::page::PageStatus::Published.as_str();
    // #556 — remember whether it was archived, to block an unpublish
    // (archived → draft) below. Archived → published is the unarchive
    // path and stays allowed.
    let was_archived = page.status == crate::page::PageStatus::Archived.as_str();
    let old_status = page.status.clone();
    // page.page_type_id stays as-is — see comment on `canonical_keys`.
    page.status = form.parsed_status().as_str().to_owned();
    page.seo_title = form.seo_title.clone();
    page.seo_description = form.seo_description.clone();
    page.robots_index = form.parsed_robots_index();
    page.sitemap_priority = form.parsed_sitemap_priority();
    page.theme_id = form.theme_id;
    // #207 — clear the pre-publish reminder flag when go_live_at
    // moves so a fresh schedule horizon gets a fresh reminder.
    let new_go_live = form.parsed_go_live_at();
    if page.go_live_at != new_go_live {
        page.notification_pre_published_sent = false;
    }
    page.go_live_at = new_go_live;
    page.expire_at = form.parsed_expire_at();
    // #264 — derive scheduled state from go_live_at AFTER the form's
    // chosen status has been applied above, so picking "Draft" +
    // setting a future go-live still parks the row in the sweep's
    // pickup queue.
    page.status = derive_scheduled_status(&page.status, page.go_live_at);
    page.show_in_menus = form.parsed_show_in_menus();
    page.preview_path = form.preview_path.trim().to_owned();
    page.template_override = form.template_override.trim().to_owned();
    // #251 — first/last publish timestamps. `published_at` is set
    // once on the first published transition (still the
    // chronological-ordering anchor). `last_published_at` bumps on
    // every save while status=published — sitemap / cache use it
    // for freshness.
    if page.status == crate::page::PageStatus::Published.as_str() {
        let now = chrono::Utc::now();
        if page.published_at.is_none() {
            page.published_at = Some(now);
        }
        page.last_published_at = Some(now);
    }
    // #556 — an archived page can't be unpublished to Draft in place;
    // it must be revived (→ Published) or explicitly unarchived first.
    // This runs before any DB mutation below.
    if was_archived && page.status == crate::page::PageStatus::Draft.as_str() {
        return Ok(Err(PageEditRefusal::ArchivedUnpublish));
    }
    // #761 — judged on the status this save actually lands on, after
    // `derive_scheduled_status`, so neither `archived` nor Draft plus a
    // go-live date gets a draft public without the publish right. Here so
    // the admin form and MCP share one rule. A save with no actor is the
    // CMS's own.
    if let Some(actor) = actor {
        if crate::page::PageStatus::str_goes_live(&old_status, &page.status) {
            // A workflow on the page type is only a promise if nobody can
            // publish around it: only its approval (or a superuser) puts
            // such a page live. Asked first — "submit it for review" is
            // the useful answer whatever the actor's rights.
            if !actor.is_superuser {
                if let Some(wf) = crate::workflow::review_workflow_for_type(pool, page.page_type_id).await? {
                    return Ok(Err(PageEditRefusal::ReviewRequired { workflow: wf.name }));
                }
            }
            let req = super::route_perms::Requirement {
                codename: "cms_page.publish".to_owned(),
                page: Some((id, crate::permissions::Action::Publish)),
            };
            if !super::route_perms::allows_id(pool, actor.id, actor.is_superuser, &req).await {
                return Ok(Err(PageEditRefusal::PublishDenied));
            }
        }
    }
    // #183 — social sharing fields.
    page.og_title = form.og_title.clone();
    page.og_description = form.og_description.clone();
    page.og_image_media_id = form.og_image_media_id;
    if let Some(tc) = form.twitter_card.as_deref() {
        if matches!(tc, "summary" | "summary_large_image") {
            page.twitter_card = tc.to_owned();
        }
    }

    // Recompute the materialized `url_path` when the
    // slug changes, and cascade the recomputation to
    // every descendant so deep trees stay correct after an ancestor
    // rename.
    if slug_changed {
        let parent_url_path = match page.parent_id {
            None => None,
            Some(pid) => Page::objects()
                .where_(Page::id.eq(pid))
                .first(pool)
                .await?
                .map(|p| p.url_path),
        };
        page.url_path = crate::tree_ops::compute_url_path(parent_url_path.as_deref(), &page.slug);
    }
    let new_url_path = page.url_path.clone();

    // Wrap self-save + cascade in one atomic_tree block so the tree
    // never observes a mixed state (own page updated but children
    // stale). The #78 purge of the affected URLs — this page's new
    // URL, plus (on a slug change) its old URL and every descendant —
    // is registered via on_commit, so it fires only once the save
    // commits (never on rollback) and runs detached. #317.
    let invalidator = invalidator.clone();
    let tenant_slug = org_slug.to_owned();
    let purge_new = new_url_path.clone();
    let purge_old = old_url_path.clone();
    let (page, descendant_urls) = crate::tree_ops::atomic_tree(pool, move |tx| {
        Box::pin(async move {
            page.save_tx(tx).await?;
            let descendants: Vec<Page> = if slug_changed {
                crate::tree_ops::cascade_url_path_to_descendants(tx, &page).await?;
                Page::objects()
                    .where_(Page::path.like(crate::tree::descendants_like(&page.path)))
                    .where_(Page::path.ne(page.path.clone()))
                    .fetch_tx(tx)
                    .await?
            } else {
                Vec::new()
            };
            // A rename moves this page and everything under it: keep the
            // old addresses of the public ones working (redirects that
            // follow the page).
            if slug_changed {
                let public = |p: &Page| matches!(p.status.as_str(), "published" | "archived");
                let mut moves = Vec::with_capacity(1 + descendants.len());
                if public(&page) {
                    moves.push((page.id.get().copied().unwrap_or_default(), purge_old.clone(), purge_new.clone()));
                }
                for d in descendants.iter().filter(|d| public(d)) {
                    if let Some(old) = crate::redirect::rebase_path(&d.url_path, &purge_new, &purge_old) {
                        moves.push((d.id.get().copied().unwrap_or_default(), old, d.url_path.clone()));
                    }
                }
                crate::redirect::record_renames_tx(tx, &moves).await?;
            }
            let descendant_urls: Vec<String> = descendants.into_iter().map(|d| d.url_path).collect();
            let mut urls = Vec::with_capacity(2 + descendant_urls.len());
            urls.push(purge_new.clone());
            if slug_changed && purge_old != purge_new && !purge_old.is_empty() {
                urls.push(purge_old);
            }
            urls.extend(descendant_urls.iter().cloned());
            crate::cache_invalidate::invalidate_urls_on_commit(invalidator, tenant_slug, urls);
            Ok((page, descendant_urls))
        })
    })
    .await?;
    // #705 — the page's content goes in straight after the core row, and
    // before anything announces the save: the revision, the reference
    // index, search, publish hooks, subscriber mail and the page log all
    // describe this content. A failed write is returned to the caller,
    // not logged and swallowed behind a "Saved" flash. (The writes can't
    // join the core row's transaction yet — the handler hooks take a
    // pool — so the core row stays committed; resubmitting is safe, the
    // upserts are idempotent.)
    //
    // #189 — tags from the comma-separated `tags` field. Missing → left
    // alone; posted empty → cleared.
    if let Some(tags_raw) = form_map.get("tags") {
        let names: Vec<String> = if tags_raw.is_empty() {
            Vec::new()
        } else {
            tags_raw.split(',').map(|s| s.to_owned()).collect()
        };
        crate::page_tag::replace_tags(pool, id, &names).await?;
    }
    // #842 — categories, same missing/empty rule as tags.
    crate::category::assign::save_from_form(
        pool,
        id,
        form_map.get(crate::category::assign::FORM_KEY).map(String::as_str),
    )
    .await?;
    let pts: Vec<PageType> = PageType::objects()
        .where_(PageType::id.eq(page.page_type_id))
        .fetch(pool)
        .await?;
    let pt_opt: Option<PageType> = pts.into_iter().next();
    if let Some(pt) = &pt_opt {
        if let Some(handler) = find_handler(&pt.type_name) {
            let ext_form = sanitized_extension_form(handler.as_ref(), pool, id, &form_map).await;
            handler.save_extension(pool, id, &ext_form).await?;
            // #117 — inline panels. Reassemble row dicts from the
            // posted `inline__<panel>__<idx>__<field>` keys and hand
            // them to the handler. Handler is responsible for the
            // upsert / delete-removed semantics.
            for spec in handler.inline_panels() {
                let rows = extract_inline_panel_rows(&form_map, &spec);
                handler.save_inline_panel(pool, id, &spec.name, &rows).await?;
            }
        }
    }
    // #564 — page-builder body values (additive to any code handler).
    save_page_builder_values(pool, page.page_type_id, id, &form_map).await?;

    // Capture revision after commit so the committed
    // page row is visible to the sequence SELECT inside capture.
    crate::revision::capture(pool, &page, actor.map(|a| a.id))
        .await
        .log_warn("revision not captured; the history has a gap");

    // #146 — reindex the reference graph from the just-saved form
    // values. The (cheap, in-memory) scan stays here where the form is
    // in hand; #432 moves the (DB-write) reindex onto the task queue
    // via `reindex_page` — dispatched + retried when a sink is
    // installed, inline otherwise. Best-effort either way.
    {
        let mut refs: Vec<crate::reference_index::Reference> = Vec::new();
        for (key, value) in form_map {
            // Scan every form field for /__media__/... + page/snippet
            // links. Field-scoped so the index records which input
            // produced each reference.
            let mut found = crate::reference_index::scan(value, key);
            refs.append(&mut found);
        }
        let page_id = page.id.get().copied().unwrap_or_default();
        crate::task_queue::reindex_page(pool, org_slug, page_id, refs).await;
    }

    // #408 — keep an external search index (Elasticsearch) in sync:
    // published → indexed, otherwise removed. No-op when no backend is
    // installed (the Postgres FTS path needs no maintained index).
    crate::task_queue::sync_page_search(org_slug, &page).await;

    // #107 — fan out to plugin `after_publish_page` hooks.
    let actor_id = actor.map(|a| a.id);
    if page.status == crate::page::PageStatus::Published.as_str() {
        crate::hooks::fire_after_publish_page(&page);
        // #115 — notify-on-publish subscribers.
        if notify {
            let actor_name = actor.map(|a| a.username.as_str());
            crate::page_subscription::notify_publish(
                mailer,
                mailer_from,
                pool,
                org_slug,
                page.id.get().copied().unwrap_or_default(),
                &page.title,
                &page.url_path,
                actor_name,
            )
            .await;
        }
        // #192 — record the publish in the audit log.
        crate::page_log::record_or_warn(
            pool,
            id,
            crate::page_log::ACTION_PUBLISH,
            actor_id,
            format!("Published “{}”.", page.title),
            None,
        )
        .await;
    } else {
        // #192 — log the edit even when the new status isn't Published.
        crate::page_log::record_or_warn(
            pool,
            id,
            crate::page_log::ACTION_EDIT,
            actor_id,
            format!("Edited “{}”.", page.title),
            Some(serde_json::json!({ "status": page.status })),
        )
        .await;
    }


    // #64 — auto-create 301 redirects from old paths to new ones
    // when a slug change cascades through the URL tree. Editors
    // rename pages all the time; keeping the old URL alive prevents
    // external links + SEO rank from going dead.
    let mut redirects_created = 0usize;
    if slug_changed && old_url_path != new_url_path && !old_url_path.is_empty() {
        // Capture the descendant before/after pairs alongside the
        // page itself. The cascade already updated every descendant's
        // url_path in the same transaction; rebuild the OLD → NEW
        // map from the path prefix relationship: a descendant whose
        // new url_path begins with `new_url_path` had its OLD url at
        // `old_url_path + suffix`.
        let new_prefix = if new_url_path == "/" {
            String::new()
        } else {
            new_url_path.clone()
        };
        let old_prefix = if old_url_path == "/" {
            String::new()
        } else {
            old_url_path.clone()
        };
        let mut pairs: Vec<(String, String)> = vec![(old_url_path.clone(), new_url_path.clone())];
        for new_d in &descendant_urls {
            if let Some(suffix) = new_d.strip_prefix(new_prefix.as_str()) {
                let old_d = format!("{old_prefix}{suffix}");
                if old_d != *new_d {
                    pairs.push((old_d, new_d.clone()));
                }
            }
        }
        // Upsert each pair. If an existing redirect already targets the
        // old path (e.g. from a previous rename), update its `to_path`
        // so the chain compresses to one hop instead of `foo → bar →
        // baz`. New rows land with is_permanent=true (301) + a stamped
        // note for auditability.
        let now = chrono::Utc::now().to_rfc3339();
        for (old_path, new_path) in pairs {
            // Look up any existing row keyed on the old URL.
            let existing: Vec<crate::redirect::Redirect> = crate::redirect::Redirect::objects()
                .where_(crate::redirect::Redirect::from_path.eq(old_path.clone()))
                .fetch(pool)
                .await
                .unwrap_or_default();
            if let Some(mut r) = existing.into_iter().next() {
                // Compress chain: rewrite the existing row's target
                // unless it already points at the new URL.
                if r.to_path != new_path {
                    r.to_path = new_path.clone();
                    r.is_permanent = true;
                    r.note = format!("auto-updated on page rename at {now}");
                    // #553 — link the source page so the rule
                    // auto-disables if the page is later removed.
                    if r.from_page_id.is_none() {
                        r.from_page_id = Some(id);
                    }
                    r.save_pool(pool).await.log_warn("redirect not linked to its source page");
                }
                continue;
            }
            // Compress incoming chain: if an OLD existing row was
            // pointing TO old_path, redirect it straight to new_path
            // instead so the chain stays one hop.
            let stale_chain: Vec<crate::redirect::Redirect> = crate::redirect::Redirect::objects()
                .where_(crate::redirect::Redirect::to_path.eq(old_path.clone()))
                .fetch(pool)
                .await
                .unwrap_or_default();
            for mut r in stale_chain {
                // #708 — renaming back (A → B, then B → A) would chain
                // A → B into A → A. The page lives at A again, so the
                // rule has nothing left to do.
                if r.from_path == new_path {
                    r.delete_pool(pool)
                        .await
                        .log_warn("self-redirect left after a rename back not deleted");
                    continue;
                }
                r.to_path = new_path.clone();
                r.note = format!("auto-chained on page rename at {now}");
                r.save_pool(pool).await.log_warn("redirect chain not updated after a rename");
            }
            // Insert the fresh old → new row.
            let mut row = crate::redirect::Redirect {
                id: rustango::sql::Auto::Unset,
                from_path: old_path,
                to_path: new_path,
                is_permanent: true,
                note: format!("auto-created on page rename at {now}"),
                hit_count: 0,
                overrides_live: false,
                // Link the renamed page so the rule auto-disables if the
                // page is later deleted/unpublished (#553).
                from_page_id: Some(id),
                to_page_id: None,
                is_active: true,
                disabled_reason: String::new(),
                last_hit_at: None,
                created_at: rustango::sql::Auto::Unset,
                updated_at: rustango::sql::Auto::Unset,
            };
            if row.insert_pool(pool).await.is_ok() {
                redirects_created += 1;
            }
        }
    }

    // #78 cache invalidation for the affected URLs is registered via
    // `on_commit` inside the save block above (#317), so it fires only
    // once the save commits and doesn't block this response.

    // #73 PR 3 — reapproval-on-edit. When the page type is bound to
    // a workflow whose `require_reapproval_on_edit` flag is set, and
    // the page has previously been through review (any approved
    // WorkflowState in its history), restart the workflow on this
    // save. The page stays at its current status; a fresh
    // WorkflowState pinned to task #1 kicks off a new approval
    // cycle. No-op when no active workflow exists for the type, or
    // when the page hasn't been approved yet (first-time submitters
    // explicitly submit via the action bar).
    let mut reapproval_kicked = false;
    if let (Some(viewer_id), Some(pt)) = (actor.map(|a| a.id), pt_opt.as_ref()) {
        if let Some(slug) = pt.workflow_name() {
            if let Some(workflow) = crate::workflow::find_by_name(pool, &slug).await? {
                if workflow.require_reapproval_on_edit
                    && crate::workflow::active_state_for_page(pool, id)
                        .await?
                        .is_none()
                    && crate::workflow::has_prior_approval(pool, id).await?
                {
                    let tasks = crate::workflow::tasks_for(
                        pool,
                        workflow.id.get().copied().unwrap_or_default(),
                    )
                    .await?;
                    if !tasks.is_empty() {
                        crate::workflow::submit_for_review(
                            pool, id, &workflow, &tasks, viewer_id,
                        )
                        .await?;
                        reapproval_kicked = true;
                    }
                }
            }
        }
    }

    // Public render cache (rustango::cache_page::CachePageLayer)
    // is TTL-based; the new/old/descendant URLs propagate after the
    // configured TTL elapses.
    let _ = (&new_url_path, slug_changed, &old_url_path, &descendant_urls);
    // #553 — this save took the page out of Published: redirect rules
    // linked to it must not keep serving (a dead source URL, or a
    // destination that now 404s). Best-effort; the save itself stands.
    if was_published && page.status != crate::page::PageStatus::Published.as_str() {
        match crate::redirect::disable_for_page(
            pool,
            id,
            &format!("Linked page “{}” was unpublished", page.title),
        )
        .await
        {
            Ok(0) => {}
            Ok(n) => {
                tracing::info!(
                    page_id = id,
                    disabled = n,
                    "disabled redirects for unpublished page"
                );
            }
            Err(e) => {
                tracing::warn!(page_id = id, error = %e, "failed to disable redirects for unpublished page");
            }
        }
    }

    // A save that went through directly (a superuser's) supersedes a
    // change held for review; the editor showed it pre-filled.
    if actor.is_some() {
        crate::pending_change::discard(pool, id).await?;
    }

    Ok(Ok(PageEditOutcome {
        page,
        redirects_created,
        reapproval_kicked,
        held_for_review: false,
    }))
}

pub async fn page_edit_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form_map): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let actor = session_user.as_ref().map(|u| PageEditActor {
        id: u.id.get().copied().unwrap_or_default(),
        username: u.username.clone(),
        is_superuser: u.is_superuser,
    });
    let outcome = apply_page_edit(
        tenant.pool(),
        &tenant.org.slug,
        &_state.cache_invalidator,
        _state.mailer.as_deref(),
        &_state.mailer_from,
        true,
        id,
        &form_map,
        actor.as_ref(),
    )
    .await?;
    let PageEditOutcome {
        page,
        redirects_created,
        reapproval_kicked,
        held_for_review,
    } = match outcome {
        Ok(o) => o,
        Err(PageEditRefusal::WorkflowReview { task_name }) => {
            return redirect_named_with_params_and_message_args(
                "rcms-admin:pages:edit",
                &[("id", id.to_string())],
                MsgLevel::Error,
                "Couldn't save — page is under review on “{name}”. Only reviewers of that step can edit during review.",
                &[("name", task_name.as_str())],
                &headers,
            );
        }
        Err(PageEditRefusal::LockedBy { holder }) => {
            return redirect_named_with_params_and_message_args(
                "rcms-admin:pages:edit",
                &[("id", id.to_string())],
                MsgLevel::Error,
                "Couldn't save — “{holder}” is editing this page right now. Force-unlock to take over.",
                &[("holder", holder.as_str())],
                &headers,
            );
        }
        Err(PageEditRefusal::ArchivedUnpublish) => {
            return redirect_named_with_params_and_message_args(
                "rcms-admin:pages:edit",
                &[("id", id.to_string())],
                MsgLevel::Error,
                "Can't unpublish an archived page. Set it back to Published to unarchive, or delete it after unarchiving.",
                &[],
                &headers,
            );
        }
        Err(PageEditRefusal::PublishDenied) => {
            return redirect_named_with_params_and_message(
                "rcms-admin:pages:edit",
                &[("id", id.to_string())],
                MsgLevel::Error,
                "You don't have permission to publish this page — save it as a draft or submit it for review.",
                &headers,
            );
        }
        Err(PageEditRefusal::ReviewRequired { workflow }) => {
            return redirect_named_with_params_and_message_args(
                "rcms-admin:pages:edit",
                &[("id", id.to_string())],
                MsgLevel::Error,
                "Pages of this type go live after review (“{workflow}”). Save it as a draft and click Submit for review.",
                &[("workflow", workflow.as_str())],
                &headers,
            );
        }
    };
    if held_for_review {
        return redirect_after_page_save(
            form_map.get("_action").map_or("save", String::as_str),
            id,
            page.parent_id,
            &super::i18n::tr(
                &headers,
                "Your changes to “{title}” wait for review. Visitors see the live page until they are approved.",
                &[("title", page.title.as_str())],
            ),
            &headers,
        );
    }
    let mut message = if redirects_created > 0 {
        let count = redirects_created.to_string();
        super::i18n::tr_plural(
            &headers,
            "Saved “{title}”. Auto-created {count} redirect(s) from the old URL(s).",
            redirects_created as i64,
            &[("title", page.title.as_str()), ("count", count.as_str())],
        )
    } else {
        super::i18n::tr(
            &headers,
            "Saved “{title}”.",
            &[("title", page.title.as_str())],
        )
    };
    if reapproval_kicked {
        message.push(' ');
        message.push_str(&super::i18n::tr(
            &headers,
            "Workflow restarted — page enters review again.",
            &[],
        ));
    }
    redirect_after_page_save(
        form_map.get("_action").map_or("save", String::as_str),
        id,
        page.parent_id,
        &message,
        &headers,
    )
}

/// POST /cms-admin/pages/{id}/translate?locale=<code> — upsert
/// `cms_translation` rows for the given locale. Empty values delete
/// the matching row so the canonical content shows through.
///
/// Accepts arbitrary field paths (the three core ones — `title`,
/// `seo_title`, `seo_description` — plus any custom `field_path`
/// referenced by the page's template, e.g. `intro`, `body`,
/// `nav_about`). The form sends them as `tr__<path>` keys; the
/// "add new key" row uses `new_field_path` + `new_field_value`.
/// Apply one batch of translation overrides for `(page_id, locale_id)`:
/// an empty value deletes the override row, a non-empty one upserts it.
/// Wrapped in `atomic!` so a mid-loop failure rolls back the whole batch —
/// callers never leave half-applied translations behind. Callers are
/// responsible for locale validation (in particular: never the default
/// locale — canonical content lives on `cms_page`).
pub(crate) async fn upsert_page_translations(
    pool: &rustango::sql::Pool,
    page_id: i64,
    locale_id: i64,
    updates: Vec<(String, String)>,
) -> Result<(), rustango::sql::ExecError> {
    rustango::atomic!(pool, |tx| {
        let mut guard = tx.lock().await?;
        let tx = &mut *guard;
        for (path, value) in updates {
            if value.is_empty() {
                let rows: Vec<crate::translation::Translation> =
                    crate::translation::Translation::objects()
                        .where_(crate::translation::Translation::page_id.eq(page_id))
                        .where_(crate::translation::Translation::locale_id.eq(locale_id))
                        .where_(crate::translation::Translation::field_path.eq(path.clone()))
                        .fetch_tx(tx)
                        .await?;
                for row in rows {
                    row.delete_tx(tx).await?;
                }
            } else {
                let existing: Vec<crate::translation::Translation> =
                    crate::translation::Translation::objects()
                        .where_(crate::translation::Translation::page_id.eq(page_id))
                        .where_(crate::translation::Translation::locale_id.eq(locale_id))
                        .where_(crate::translation::Translation::field_path.eq(path.clone()))
                        .fetch_tx(tx)
                        .await?;
                if let Some(mut row) = existing.into_iter().next() {
                    row.value = value;
                    row.save_tx(tx).await?;
                } else {
                    let mut row = crate::translation::Translation {
                        id: rustango::sql::Auto::Unset,
                        page_id,
                        locale_id,
                        field_path: path,
                        value,
                        updated_at: rustango::sql::Auto::Unset,
                    };
                    row.save_tx(tx).await?;
                }
            }
        }
        Ok::<_, rustango::sql::ExecError>(())
    })
    .await
}

pub async fn page_translate_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Path(id): Path<i64>,
    Query(q): Query<EditQuery>,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let code = q
        .locale
        .as_deref()
        .filter(|c| !c.is_empty())
        .ok_or_else(|| AdminError::Validation("translate requires ?locale=".to_owned()))?;
    let locale = Locale::objects()
        .where_(Locale::code.eq(code.to_owned()))
        .where_(Locale::active.eq(true))
        .first(tenant.pool())
        .await?
        .ok_or_else(|| AdminError::Validation(format!("unknown locale `{code}`")))?;
    if locale.is_default {
        return Ok((
            axum::http::StatusCode::BAD_REQUEST,
            "cannot translate INTO the default locale; canonical content lives on cms_page",
        )
            .into_response());
    }
    let locale_id = locale
        .id
        .get()
        .copied()
        .ok_or_else(|| AdminError::Validation("locale row missing id".to_owned()))?;
    // Verify the page exists; cheap one-row select.
    let _page = Page::objects()
        .where_(Page::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;

    // Collect updates from the form. Keys starting with `tr__` are
    // existing-row edits; `new_field_path` + `new_field_value` is the
    // single "add new key" row at the bottom of the editor.
    let mut updates: Vec<(String, String)> = Vec::new();
    for (k, v) in &form {
        if let Some(path) = k.strip_prefix("tr__") {
            if path.is_empty() {
                continue;
            }
            updates.push((path.to_owned(), v.clone()));
        }
    }
    let new_path = form
        .get("new_field_path")
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    if !new_path.is_empty() {
        let new_value = form.get("new_field_value").cloned().unwrap_or_default();
        // Validate shape via the shared guard (≤255, alphanumeric + `_`
        // `.` `-`) — allows dotted block-UUID paths into StreamFields.
        if let Ok(path) = crate::translation::validate_field_path(&new_path) {
            updates.push((path, new_value));
        }
    }

    upsert_page_translations(tenant.pool(), id, locale_id, updates).await?;

    // Invalidate cache for both the canonical and `?lang=` keys so
    // the next public hit re-renders with the updated translations.
    let page = Page::objects()
        .where_(Page::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // Public render cache (rustango::cache_page::CachePageLayer)
    // expires on TTL; translation edits propagate after the
    // configured timeout.
    let _ = (page, &code);

    Ok(Redirect::to(&format!("/cms-admin/pages/{id}/edit?locale={code}")).into_response())
}

/// POST /cms-admin/pages/{id}/delete — refuse if page has children,
/// otherwise delete the row. Cascade-delete-with-children deferred.
pub async fn page_delete_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let page = Page::objects()
        .where_(Page::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;

    // #556 — archived pages are frozen against deletion (archive is a
    // preservation lock, not a step toward removal). The editor must
    // unarchive first.
    if page.status == crate::page::PageStatus::Archived.as_str() {
        return redirect_named_with_message_args(
            "rcms-admin:pages:list",
            MsgLevel::Error,
            "Can't delete “{title}” while archived — unarchive it first.",
            &[("title", page.title.as_str())],
            &headers,
        );
    }

    let child_count: Vec<Page> = Page::objects()
        .where_(Page::parent_id.eq(id))
        .fetch(tenant.pool())
        .await?;
    if !child_count.is_empty() {
        let count = child_count.len().to_string();
        return redirect_named_with_message_plural(
            "rcms-admin:pages:list",
            MsgLevel::Error,
            "Can't delete “{title}” — it has {count} child page(s). Delete or move them first.",
            child_count.len() as i64,
            &[("title", page.title.as_str()), ("count", count.as_str())],
            &headers,
        );
    }

    // A hostname pointing at this root has a foreign key to it, so the
    // delete would fail with an opaque FK error. Refuse with the names
    // instead — and refuse rather than dropping the mapping, because a
    // live domain quietly falling back to a different site is a change
    // only visitors would notice.
    let serving_hosts = crate::site::hosts_for_root(tenant.pool(), id).await?;
    if !serving_hosts.is_empty() {
        let hosts = serving_hosts.join(", ");
        return redirect_named_with_message_args(
            "rcms-admin:pages:list",
            MsgLevel::Error,
            "Can't delete “{title}” — it is the root served by {hosts}. Change that on the Sites screen first.",
            &[("title", page.title.as_str()), ("hosts", hosts.as_str())],
            &headers,
        );
    }

    // #75 — promote any aliases of this page to standalone pages
    // before deleting the source. Each alias inherits a snapshot of
    // the source's content (title / SEO / status / theme) so its
    // public URL keeps rendering coherent content after the source
    // is gone. Without this step the FK guard would refuse the
    // delete, or worse — aliases would point at a dead row.
    let aliases: Vec<Page> = Page::objects()
        .where_(Page::alias_of.eq(id))
        .fetch(tenant.pool())
        .await?;
    let alias_count = aliases.len();
    let title = page.title.clone();
    let url_path = page.url_path.clone();

    // #317 — alias promotion + the delete run in one `atomic!` block,
    // so a failure can't leave aliases promoted against a still-present
    // source. The #78 cache purge is registered via `on_commit`, so it
    // fires only once the delete commits — a rollback never evicts a
    // still-valid public render. The purge runs detached, so the next
    // visitor sees a 404, not a stale 200, without blocking this
    // response on the cache round-trip.
    let invalidator = _state.cache_invalidator.clone();
    let tenant_slug = tenant.org.slug.clone();
    let purge_url = url_path.clone();
    rustango::atomic!(tenant.pool(), |tx| {
        let mut guard = tx.lock().await?;
        let tx = &mut *guard;
        for mut alias in aliases {
            alias.alias_of = None;
            alias.title = page.title.clone();
            alias.seo_title = page.seo_title.clone();
            alias.seo_description = page.seo_description.clone();
            alias.status = page.status.clone();
            alias.published_at = page.published_at;
            alias.last_published_at = page.last_published_at;
            alias.theme_id = page.theme_id;
            alias.show_in_menus = page.show_in_menus;
            alias.save_tx(tx).await?;
        }
        // Revisions, tags, logs… point at the page with no ON DELETE
        // action; clear them first or the delete fails.
        crate::page_delete::clear_page_references_tx(tx, id).await?;
        page.delete_tx(tx).await?;
        crate::cache_invalidate::invalidate_urls_on_commit(
            invalidator,
            tenant_slug,
            vec![purge_url],
        );
        Ok(())
    })
    .await?;
    // #408 — drop the deleted page from the external search index
    // (no-op when no backend is installed).
    crate::task_queue::remove_page_search(&tenant.org.slug, id).await;
    // #553 — redirect rules linked to this page must not keep serving.
    // Best-effort; the delete already committed.
    if let Err(e) = crate::redirect::disable_for_page(
        tenant.pool(),
        id,
        &format!("Linked page “{title}” was deleted"),
    )
    .await
    {
        tracing::warn!(page_id = id, error = %e, "failed to disable redirects for deleted page");
    }
    if alias_count > 0 {
        redirect_named_with_message_plural(
            "rcms-admin:pages:list",
            MsgLevel::Success,
            "Deleted “{title}”. Promoted {count} alias(es) to standalone page(s) with the source's last content.",
            alias_count as i64,
            &[("title", title.as_str()), ("count", &alias_count.to_string())],
            &headers,
        )
    } else {
        redirect_named_with_message_args(
            "rcms-admin:pages:list",
            MsgLevel::Success,
            "Deleted “{title}”.",
            &[("title", title.as_str())],
            &headers,
        )
    }
}

// =====================================================================
// Slice 6 — drag-reorder + locale variants
// =====================================================================

/// Form payload for `POST /cms-admin/pages/{id}/move` — emitted by
/// the tree view's drag-and-drop JS.
#[derive(Debug, Deserialize)]
pub struct MoveForm {
    /// The drop target's page id. The moved page becomes a child of
    /// this row (appended at the bottom of its existing children).
    /// Negative or zero values mean "move to root".
    pub new_parent: i64,
}

/// POST /cms-admin/pages/{id}/move — change a page's parent +
/// sort_order via the existing `Page::move_to`. Wraps the cascade so
/// every descendant's `url_path` is re-materialized.
pub async fn page_move_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<MoveForm>,
) -> Result<Response, AdminError> {
    let mut page = Page::objects()
        .where_(Page::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let new_parent: Option<Page> = if form.new_parent > 0 {
        Page::objects()
            .where_(Page::id.eq(form.new_parent))
            .first(tenant.pool())
            .await?
    } else {
        None
    };
    let old_url_path = page.url_path.clone();
    let old_descendant_urls: Vec<String> =
        page.descendants(tenant.pool()).await?.into_iter().map(|d| d.url_path).collect();
    // One transaction moves the tree and its URLs together (#706).
    page.move_to(&tenant, new_parent.as_ref()).await?;
    let new_url_path = page.url_path.clone();

    // #78 — purge the old and new URL of the page and every descendant,
    // now that the move has committed; detached so the response isn't
    // blocked on the cache round-trips.
    let mut urls = old_descendant_urls;
    urls.push(old_url_path.clone());
    urls.push(new_url_path.clone());
    urls.extend(page.descendants(tenant.pool()).await?.into_iter().map(|d| d.url_path));
    let invalidator = _state.cache_invalidator.clone();
    let tenant_slug = tenant.org.slug.clone();
    rustango::__private_runtime::tokio::spawn(async move {
        crate::task_queue::purge_urls(invalidator, tenant_slug, urls).await;
    });

    // #192 — audit-log the move.
    let actor_id = session_user.as_ref().and_then(|u| u.id.get().copied());
    crate::page_log::record_or_warn(
        tenant.pool(),
        id,
        crate::page_log::ACTION_MOVE,
        actor_id,
        format!(
            "Moved “{}” from {old_url_path} to {new_url_path}.",
            page.title
        ),
        Some(serde_json::json!({
            "old_url_path": old_url_path,
            "new_url_path": new_url_path,
            "new_parent_id": form.new_parent,
        })),
    )
    .await;

    Ok((axum::http::StatusCode::OK, "ok").into_response())
}

/// POST /cms-admin/pages/{id}/clone — duplicate the source page as a
/// draft sibling (#29). The copy carries everything the page stores —
/// core fields (SEO, OG/Twitter, theme, template override), tags, the
/// extension row, inline panels and the page-builder body — because it
/// is saved through the same pipeline as an edit, from the source's
/// [`crate::page_form::prefill`] (#742). The new row:
///
/// - Lives under the same parent as the source.
/// - Title gets a " (copy)" suffix.
/// - Slug gets a "-copy" suffix; on conflict with an existing
///   sibling slug, we suffix `-copy-2`, `-copy-3`, … until unique.
/// - Status forces back to Draft, with no schedule, so editors review
///   before publishing.
/// - Locale-variant pointer cleared (variants stay independent of
///   any single source).
///
/// Subtree clone (source + all descendants) is a V2 follow-up.
pub async fn page_clone_submit(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let source = Page::objects()
        .where_(Page::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;

    let title = format!("{} (copy)", source.title);
    let base_slug = source.slug.trim_matches('-').to_owned();
    let slug = unique_sibling_slug(
        tenant.pool(),
        source.parent_id,
        &format!("{base_slug}-copy"),
    )
    .await?;

    let new = crate::tree_ops::NewPage::new(source.page_type_id, title, slug)
        .with_status(crate::page::PageStatus::Draft)
        .with_seo(source.seo_title.clone(), source.seo_description.clone());

    let clone = match source.parent_id {
        Some(pid) => {
            let parent = Page::objects()
                .where_(Page::id.eq(pid))
                .first(tenant.pool())
                .await?
                .ok_or(AdminError::NotFound(pid))?;
            Page::create_child(&tenant, &parent, new).await?
        }
        None => Page::create_root(&tenant, new).await?,
    };
    let new_id = clone.id.get().copied().unwrap_or_default();

    // Copy the content by saving the source's full form onto the new
    // row: the edit pipeline writes the core fields, tags, extension,
    // inline panels and builder body, and captures the first revision
    // with all of it.
    let mut form = crate::page_form::prefill(tenant.pool(), &source).await;
    form.insert("title".into(), clone.title.clone());
    form.insert("slug".into(), clone.slug.clone());
    form.insert(
        "status".into(),
        crate::page::PageStatus::Draft.as_str().to_owned(),
    );
    form.remove("go_live_at");
    form.remove("expire_at");
    let actor = session_user.as_ref().map(|u| PageEditActor {
        id: u.id.get().copied().unwrap_or_default(),
        username: u.username.clone(),
        is_superuser: u.is_superuser,
    });
    let saved = apply_page_edit(
        tenant.pool(),
        &tenant.org.slug,
        &state.cache_invalidator,
        None,
        "",
        false,
        new_id,
        &form,
        actor.as_ref(),
    )
    .await?;
    if saved.is_err() {
        // A page created a moment ago has no workflow or editor lock.
        return Err(AdminError::Validation(
            "the copy was created but its content could not be saved".to_owned(),
        ));
    }
    // #192 — audit-log the clone on BOTH the new row (so the new
    // page's history shows where it came from) and the source (so
    // the source's history records its descendants).
    let actor_id = session_user.as_ref().and_then(|u| u.id.get().copied());
    crate::page_log::record_or_warn(
        tenant.pool(),
        new_id,
        crate::page_log::ACTION_COPY,
        actor_id,
        format!("Cloned from “{}” (page #{id}).", source.title),
        Some(serde_json::json!({ "source_page_id": id })),
    )
    .await;
    crate::page_log::record_or_warn(
        tenant.pool(),
        id,
        crate::page_log::ACTION_COPY,
        actor_id,
        format!("Cloned to new draft page #{new_id}."),
        Some(serde_json::json!({ "clone_page_id": new_id })),
    )
    .await;
    redirect_named_with_params_and_message_args(
        "rcms-admin:pages:edit",
        &[("id", new_id.to_string())],
        MsgLevel::Success,
        "Cloned “{title}” as a draft. Review before publishing.",
        &[("title", source.title.as_str())],
        &headers,
    )
}

/// Form payload for `POST /cms-admin/pages/{id}/privacy` (#76).
/// `kind` is one of `"none"`, `"login"`, `"groups"`, `"password"`.
/// `password` is the cleartext entered when `kind=password` (hashed
/// + discarded server-side). `group_ids` is the comma-separated list
/// for `kind=groups`.
#[derive(Deserialize)]
pub struct PrivacyForm {
    pub kind: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub group_ids: String,
    /// Comma-separated permission codenames — used when `kind =
    /// permission` (the permission-engine gate). Empty otherwise.
    #[serde(default)]
    pub codenames: String,
}

pub async fn page_privacy_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<PrivacyForm>,
) -> Result<Response, AdminError> {
    use rustango::sql::Auto;
    let existing = crate::view_restriction::direct_for_page(tenant.pool(), id).await?;
    if form.kind == "none" {
        if let Some(row) = existing {
            row.delete_pool(tenant.pool()).await?;
            return redirect_named_with_params_and_message(
                "rcms-admin:pages:edit",
                &[("id", id.to_string())],
                MsgLevel::Success,
                "Removed view restriction — page is now public.",
                &headers,
            );
        }
        return redirect_named_with_params_and_message(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Info,
            "No restriction to remove.",
            &headers,
        );
    }
    let Some(kind) = crate::view_restriction::RestrictionKind::parse(&form.kind) else {
        return Err(AdminError::Validation(format!(
            "unknown restriction kind `{}`",
            form.kind
        )));
    };
    let password_hash = match kind {
        crate::view_restriction::RestrictionKind::Password => {
            // Reuse the existing hash when the editor left the
            // field blank on a page that's already password-
            // protected — lets them flip groups/login back to
            // password without remembering the original.
            if form.password.is_empty() {
                if let Some(row) = &existing {
                    if row.kind == kind.as_str() {
                        row.password_hash.clone()
                    } else {
                        return redirect_named_with_params_and_message(
                            "rcms-admin:pages:edit",
                            &[("id", id.to_string())],
                            MsgLevel::Error,
                            "Password is required when switching the restriction kind to password.",
                            &headers,
                        );
                    }
                } else {
                    return redirect_named_with_params_and_message(
                        "rcms-admin:pages:edit",
                        &[("id", id.to_string())],
                        MsgLevel::Error,
                        "Password is required for password-protected pages.",
                        &headers,
                    );
                }
            } else {
                crate::passwords::hash(&form.password).await
                    .map_err(|e| AdminError::Validation(format!("password hash failed: {e}")))?
            }
        }
        _ => String::new(),
    };
    let group_ids: Vec<i64> = if matches!(kind, crate::view_restriction::RestrictionKind::Groups) {
        form.group_ids
            .split(',')
            .filter_map(|s| s.trim().parse::<i64>().ok())
            .collect()
    } else {
        Vec::new()
    };
    let group_json = serde_json::json!(group_ids);
    let codenames: Vec<String> =
        if matches!(kind, crate::view_restriction::RestrictionKind::Permission) {
            form.codenames
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        } else {
            Vec::new()
        };
    let codenames_json = serde_json::json!(codenames);
    match existing {
        Some(mut row) => {
            row.kind = kind.as_str().to_owned();
            row.password_hash = password_hash;
            row.group_ids = group_json;
            row.codenames = codenames_json;
            row.save_pool(tenant.pool()).await?;
        }
        None => {
            let mut row = crate::view_restriction::PageViewRestriction {
                id: Auto::Unset,
                page_id: id,
                kind: kind.as_str().to_owned(),
                password_hash,
                group_ids: group_json,
                codenames: codenames_json,
                created_at: Auto::Unset,
                updated_at: Auto::Unset,
            };
            row.insert_pool(tenant.pool()).await?;
        }
    }
    redirect_named_with_params_and_message_args(
        "rcms-admin:pages:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        match kind {
            crate::view_restriction::RestrictionKind::Login => "Only signed-in people can see this page now.",
            crate::view_restriction::RestrictionKind::Groups => "Only the chosen roles can see this page now.",
            crate::view_restriction::RestrictionKind::Permission => "Only people with the chosen permissions can see this page now.",
            crate::view_restriction::RestrictionKind::Password => "This page asks for a password now.",
        },
        &[],
        &headers,
    )
}

// =====================================================================
// Inline comments (#81 PR 1)
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct CommentNewForm {
    pub field_path: String,
    pub body: String,
}

pub async fn comment_new_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<CommentNewForm>,
) -> Result<Response, AdminError> {
    let Some(viewer_id) = session_user.as_ref().and_then(|u| u.id.get().copied()) else {
        return Ok(unauthorized_no_session());
    };
    let body = form.body.trim();
    let field_path = form.field_path.trim();
    if body.is_empty() || field_path.is_empty() {
        return redirect_named_with_params_and_message(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Warning,
            "Comment body and field are both required.",
            &headers,
        );
    }
    let mut row = crate::comment::Comment {
        id: rustango::sql::Auto::Unset,
        page_id: id,
        field_path: field_path.to_owned(),
        author_id: viewer_id,
        body: body.to_owned(),
        resolved_at: None,
        resolved_by: None,
        created_at: rustango::sql::Auto::Unset,
    };
    row.insert_pool(tenant.pool()).await?;
    redirect_named_with_params_and_message(
        "rcms-admin:pages:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Comment posted.",
        &headers,
    )
}

#[derive(Debug, Deserialize)]
pub struct CommentReplyForm {
    pub body: String,
}

pub async fn comment_reply_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path((id, comment_id)): Path<(i64, i64)>,
    Form(form): Form<CommentReplyForm>,
) -> Result<Response, AdminError> {
    let Some(viewer_id) = session_user.as_ref().and_then(|u| u.id.get().copied()) else {
        return Ok(unauthorized_no_session());
    };
    let body = form.body.trim();
    if body.is_empty() {
        return redirect_named_with_params_and_message(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Warning,
            "Reply body is required.",
            &headers,
        );
    }
    let mut row = crate::comment::CommentReply {
        id: rustango::sql::Auto::Unset,
        comment_id,
        author_id: viewer_id,
        body: body.to_owned(),
        created_at: rustango::sql::Auto::Unset,
    };
    row.insert_pool(tenant.pool()).await?;
    redirect_named_with_params_and_message(
        "rcms-admin:pages:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Reply posted.",
        &headers,
    )
}

pub async fn comment_resolve_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path((id, comment_id)): Path<(i64, i64)>,
) -> Result<Response, AdminError> {
    let Some(viewer_id) = session_user.as_ref().and_then(|u| u.id.get().copied()) else {
        return Ok(unauthorized_no_session());
    };
    let mut row = crate::comment::Comment::objects()
        .where_(crate::comment::Comment::id.eq(comment_id))
        .where_(crate::comment::Comment::page_id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(comment_id))?;
    row.resolved_at = Some(chrono::Utc::now());
    row.resolved_by = Some(viewer_id);
    row.save_pool(tenant.pool()).await?;
    redirect_named_with_params_and_message(
        "rcms-admin:pages:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Comment resolved.",
        &headers,
    )
}

pub async fn comment_reopen_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path((id, comment_id)): Path<(i64, i64)>,
) -> Result<Response, AdminError> {
    let mut row = crate::comment::Comment::objects()
        .where_(crate::comment::Comment::id.eq(comment_id))
        .where_(crate::comment::Comment::page_id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(comment_id))?;
    row.resolved_at = None;
    row.resolved_by = None;
    row.save_pool(tenant.pool()).await?;
    redirect_named_with_params_and_message(
        "rcms-admin:pages:edit",
        &[("id", id.to_string())],
        MsgLevel::Info,
        "Comment reopened.",
        &headers,
    )
}

/// POST /cms-admin/pages/{id}/alias — create a live alias of this
/// page as a sibling (#75). The alias is a real tree row at a
/// distinct URL but its content fields proxy to the source. Edits
/// to the source propagate instantly; edits to the alias redirect
/// the editor to the source.
///
/// Differentiation from sibling actions:
///   * Clone (`/clone`)         — deep copy, divergent content
///   * Alias (this)             — one source, multiple URLs
///
/// Localization uses the `cms_translation` per-field override table
/// (see [`crate::translation`]); the legacy structural-variant flow
/// was removed in #275.
pub async fn page_alias_submit(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let source = Page::objects()
        .where_(Page::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;

    // Refuse to alias an alias — the chain would be impossible to
    // resolve without cycle detection, and there's no editorial use
    // case for it. Authors should alias the underlying source.
    if let Some(src_id) = source.alias_of {
        let src_id_str = src_id.to_string();
        return redirect_named_with_params_and_message_args(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Error,
            "Can't alias an alias — point your new alias at the underlying source page (#{source_id}) instead.",
            &[("source_id", src_id_str.as_str())],
            &headers,
        );
    }

    let base_slug = source.slug.trim_matches('-').to_owned();
    let slug = unique_sibling_slug(
        tenant.pool(),
        source.parent_id,
        &if base_slug.is_empty() {
            "alias".to_owned()
        } else {
            format!("{base_slug}-alias")
        },
    )
    .await?;

    // tree_ops::NewPage carries title/slug/page_type but the title
    // gets shadowed by the source on render. Use the source's title
    // verbatim so the admin lists still show something meaningful.
    let new = crate::tree_ops::NewPage::new(source.page_type_id, source.title.clone(), slug)
        .with_status(crate::page::PageStatus::Draft)
        .with_seo(source.seo_title.clone(), source.seo_description.clone());

    let mut alias = match source.parent_id {
        Some(pid) => {
            let parent = Page::objects()
                .where_(Page::id.eq(pid))
                .first(tenant.pool())
                .await?
                .ok_or(AdminError::NotFound(pid))?;
            Page::create_child(&tenant, &parent, new).await?
        }
        None => Page::create_root(&tenant, new).await?,
    };
    alias.alias_of = Some(id);
    // Aliases inherit publish status from the source on render —
    // the underlying status column on the alias row is irrelevant
    // for live viewing, but setting it here keeps the admin list
    // visually accurate.
    alias.status = source.status.clone();
    alias.published_at = source.published_at;
    alias.last_published_at = source.last_published_at;
    alias.theme_id = source.theme_id;
    alias.show_in_menus = source.show_in_menus;
    alias.save_pool(tenant.pool()).await?;

    crate::revision::capture(tenant.pool(), &alias, None)
        .await
        .log_warn("revision not captured; the history has a gap");
    let new_id = alias.id.get().copied().unwrap_or_default();
    let _ = state;
    // #192 — audit log on both source + alias.
    let actor_id = session_user.as_ref().and_then(|u| u.id.get().copied());
    crate::page_log::record_or_warn(
        tenant.pool(),
        new_id,
        crate::page_log::ACTION_ALIAS_CREATE,
        actor_id,
        format!("Alias of “{}” (page #{id}).", source.title),
        Some(serde_json::json!({ "alias_of": id })),
    )
    .await;
    crate::page_log::record_or_warn(
        tenant.pool(),
        id,
        crate::page_log::ACTION_ALIAS_CREATE,
        actor_id,
        format!("New alias #{new_id} pointing here."),
        Some(serde_json::json!({ "alias_page_id": new_id })),
    )
    .await;
    redirect_named_with_params_and_message_args(
        "rcms-admin:pages:list",
        // Land on the page list (alias's edit form proxies to the
        // source, so /edit just bounces; the list is the right
        // landing). Include the alias's parent if available so the
        // editor sees the new alias in context.
        &[],
        MsgLevel::Success,
        "Created alias of “{title}”. Edits to the source will propagate to every alias's URL.",
        &[("title", source.title.as_str())],
        &headers,
    )
}

/// #438 — canonical fields seeded into `cms_translation` by the
/// copy-to-locale bulk action: the page's `title`, `seo_title`, and
/// `seo_description`, skipping empties. The source text gives
/// translators a starting point.
fn copy_fields_for(p: &Page) -> Vec<(&'static str, String)> {
    copy_translatable_fields(&p.title, &p.seo_title, &p.seo_description)
}

/// Pure core of [`copy_fields_for`] — selects the non-empty canonical
/// fields. Split out so the selection is unit-testable without a full
/// `Page`.
fn copy_translatable_fields(
    title: &str,
    seo_title: &str,
    seo_description: &str,
) -> Vec<(&'static str, String)> {
    [
        ("title", title),
        ("seo_title", seo_title),
        ("seo_description", seo_description),
    ]
    .into_iter()
    .filter(|(_, v)| !v.trim().is_empty())
    .map(|(k, v)| (k, v.to_owned()))
    .collect()
}

/// POST /cms-admin/pages/bulk — multi-select dispatcher for the
/// page list (#65). Body shape (urlencoded):
/// `action=<verb>&ids=<id>&ids=<id>[&new_parent=<id>]`.
///
/// Verbs:
///   - `publish`     → flips every selected page's `status` to
///                     Published (no-op for already-published rows).
///   - `unpublish`   → flips to Draft (no-op for already-draft rows).
///   - `delete`      → bulk delete. Rejects atomically if ANY selected
///                     page has children — matches the per-row guard.
///   - `move`        → reparent every selected page under `new_parent`
///                     (or to root when `new_parent=0`); cascades
///                     `url_path` to descendants. Filters out rows
///                     that would form a self-loop (you can't move a
///                     page under itself or one of its descendants).
///
/// All four are wrapped in a single transaction so a partial failure
/// rolls back the entire batch. The response is a redirect back to
/// the page list with a flash message summarizing how many rows were
/// touched + which were skipped (with reason).
pub async fn page_bulk_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, AdminError> {
    let action = form
        .iter()
        .find(|(k, _)| k == "action")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let ids: Vec<i64> = form
        .iter()
        .filter(|(k, _)| k == "ids" || k == "id")
        .filter_map(|(_, v)| v.parse::<i64>().ok())
        .collect();
    let new_parent_raw = form
        .iter()
        .find(|(k, _)| k == "new_parent")
        .and_then(|(_, v)| v.parse::<i64>().ok());
    // #556 — archive cascade: mark every descendant's own row, not just
    // rely on inheritance. Accepts the checkbox/select truthy forms.
    let archive_cascade = form.iter().any(|(k, v)| {
        k == "archive_cascade" && matches!(v.as_str(), "1" | "on" | "subtree" | "true")
    });

    if action.is_empty() || ids.is_empty() {
        return redirect_named_with_message(
            "rcms-admin:pages:list",
            MsgLevel::Warning,
            "Pick at least one row + an action before submitting.",
            &headers,
        );
    }

    // Load the selected rows in one shot so per-id validation can run
    // before any mutation. The IN-list is small (page list pagination
    // caps the visible set; even an unbounded multi-select is dwarfed
    // by site-wide page counts).
    let pool = tenant.pool();
    let pages: Vec<Page> = {
        let mut out = Vec::with_capacity(ids.len());
        for id in &ids {
            if let Some(p) = Page::objects().where_(Page::id.eq(*id)).first(pool).await? {
                out.push(p);
            }
        }
        out
    };
    if pages.is_empty() {
        return redirect_named_with_message(
            "rcms-admin:pages:list",
            MsgLevel::Warning,
            "None of the selected ids matched a live page.",
            &headers,
        );
    }

    // #672 — the route gate only knows this is a page edit; the action
    // posted decides what it really needs, on every selected page.
    let needs = match action.as_str() {
        "publish" | "unpublish" => crate::permissions::Action::Publish,
        "delete" => crate::permissions::Action::Delete,
        _ => crate::permissions::Action::Edit,
    };
    if let Some(user) = session_user.as_ref() {
        for p in &pages {
            let req = super::route_perms::Requirement {
                codename: format!("cms_page.{}", needs.as_str()),
                page: p.id.get().copied().map(|id| (id, needs)),
            };
            if !super::route_perms::allows(pool, user, &req).await {
                return redirect_named_with_message_args(
                    "rcms-admin:pages:list",
                    MsgLevel::Error,
                    "You don't have permission to {action} “{title}”.",
                    &[("action", action.as_str()), ("title", p.title.as_str())],
                    &headers,
                );
            }
        }
    }

    match action.as_str() {
        "publish" | "unpublish" => {
            let target = if action == "publish" {
                crate::page::PageStatus::Published
            } else {
                crate::page::PageStatus::Draft
            };
            let invalidator = _state.cache_invalidator.clone();
            let tenant_slug = tenant.org.slug.clone();
            let purge_urls: Vec<String> = pages.iter().map(|p| p.url_path.clone()).collect();
            let pages_for_tx = pages.clone();
            // #317 — flip the batch's status in one atomic! block and
            // register the #78 per-URL purge via on_commit, so it fires
            // only once the batch commits (detached; dropped on rollback).
            let touched = rustango::atomic!(pool, |tx| {
                let mut guard = tx.lock().await?;
                let tx = &mut *guard;
                let mut touched = 0usize;
                for mut p in pages_for_tx {
                    // #556 — archived pages are frozen against unpublish;
                    // they must be explicitly unarchived. Skip them in a
                    // bulk unpublish (bulk publish still revives them).
                    if matches!(target, crate::page::PageStatus::Draft)
                        && p.status == crate::page::PageStatus::Archived.as_str()
                    {
                        continue;
                    }
                    if p.status != target.as_str() {
                        p.status = target.as_str().to_owned();
                        if matches!(target, crate::page::PageStatus::Published) {
                            let now = chrono::Utc::now();
                            if p.published_at.is_none() {
                                p.published_at = Some(now);
                            }
                            // #251 — bulk publish also bumps last-publish.
                            p.last_published_at = Some(now);
                        }
                        p.save_tx(tx).await?;
                        touched += 1;
                    }
                }
                crate::cache_invalidate::invalidate_urls_on_commit(
                    invalidator,
                    tenant_slug,
                    purge_urls,
                );
                Ok::<usize, rustango::sql::ExecError>(touched)
            })
            .await?;
            // Capture revisions outside the transaction; failure here
            // only affects history bookkeeping.
            for p in &pages {
                crate::revision::capture(pool, p, None)
                    .await
                    .log_warn("revision not captured; the history has a gap");
            }
            // #553 — bulk unpublish: rules linked to pages leaving
            // Published must not keep serving. `pages` holds the
            // pre-flip rows, so filter on the OLD status.
            if !matches!(target, crate::page::PageStatus::Published) {
                for p in &pages {
                    if p.status != crate::page::PageStatus::Published.as_str() {
                        continue;
                    }
                    let pid = p.id.get().copied().unwrap_or_default();
                    if let Err(e) = crate::redirect::disable_for_page(
                        pool,
                        pid,
                        &format!("Linked page “{}” was unpublished", p.title),
                    )
                    .await
                    {
                        tracing::warn!(page_id = pid, error = %e, "failed to disable redirects for unpublished page");
                    }
                }
            }
            // #107 — plugin after_publish_page on bulk publish only.
            if matches!(target, crate::page::PageStatus::Published) {
                for p in &pages {
                    crate::hooks::fire_after_publish_page(p);
                }
                // #115 — notify-on-publish subscribers (per page).
                let actor = session_user.as_ref().map(|u| u.username.as_str());
                for p in &pages {
                    crate::page_subscription::notify_publish(
                        _state.mailer.as_deref(),
                        &_state.mailer_from,
                        pool,
                        &tenant.org.slug,
                        p.id.get().copied().unwrap_or_default(),
                        &p.title,
                        &p.url_path,
                        actor,
                    )
                    .await;
                }
            }
            let already = (pages.len() - touched).to_string();
            let key = if action == "publish" {
                "Published {count} page(s) ({already} already in the target state)."
            } else {
                "Unpublished {count} page(s) ({already} already in the target state)."
            };
            redirect_named_with_message_plural(
                "rcms-admin:pages:list",
                MsgLevel::Success,
                key,
                touched as i64,
                &[("count", &touched.to_string()), ("already", already.as_str())],
                &headers,
            )
        }
        "delete" => {
            // #556 — archived pages are a preservation lock: they can't
            // be deleted until explicitly unarchived. Reject the whole
            // batch if any selected page is archived (mirrors the child
            // guard's all-or-nothing rule).
            let archived_names: Vec<String> = pages
                .iter()
                .filter(|p| p.status == crate::page::PageStatus::Archived.as_str())
                .map(|p| format!("“{}”", p.title))
                .collect();
            if !archived_names.is_empty() {
                let list = archived_names.join(", ");
                return redirect_named_with_message_args(
                    "rcms-admin:pages:list",
                    MsgLevel::Error,
                    "Can't delete archived page(s) {names} — unarchive them first.",
                    &[("names", list.as_str())],
                    &headers,
                );
            }
            // Atomic guard: if ANY selected page has children, the
            // whole batch fails. Matches the per-row delete rule and
            // the acceptance criterion on the issue.
            let id_set: std::collections::HashSet<i64> =
                pages.iter().filter_map(|p| p.id.get().copied()).collect();
            let mut offenders: Vec<String> = Vec::new();
            for p in &pages {
                let pid = p.id.get().copied().unwrap_or_default();
                let children: Vec<Page> = Page::objects()
                    .where_(Page::parent_id.eq(pid))
                    .fetch(pool)
                    .await?;
                // Children that are themselves part of this batch
                // don't count as blockers — they'll be deleted in
                // the same transaction.
                let external_children = children
                    .iter()
                    .filter(|c| !id_set.contains(&c.id.get().copied().unwrap_or_default()))
                    .count();
                if external_children > 0 {
                    offenders.push(format!("#{pid} “{}” ({external_children})", p.title));
                }
            }
            if !offenders.is_empty() {
                let offenders_list = offenders.join(", ");
                return redirect_named_with_message_args(
                    "rcms-admin:pages:list",
                    MsgLevel::Error,
                    "Bulk delete blocked — these page(s) still have external children: {offenders}.",
                    &[("offenders", offenders_list.as_str())],
                    &headers,
                );
            }
            // Delete deepest-first so child rows go before parents
            // and FK guards stay green. Every row that points at a page
            // (revisions, tags, logs, host extension rows…) is cleared
            // first: those FKs have no ON DELETE action.
            let mut ordered = pages.clone();
            ordered.sort_by(|a, b| b.depth.cmp(&a.depth));
            let invalidator = _state.cache_invalidator.clone();
            let tenant_slug = tenant.org.slug.clone();
            // #78 — purge every deleted URL so the next visitor sees a
            // 404, not a stale 200. #317 — registered via on_commit so
            // it fires only once the whole batch delete commits.
            let purge_urls: Vec<String> = pages.iter().map(|p| p.url_path.clone()).collect();
            rustango::atomic!(pool, |tx| {
                let mut guard = tx.lock().await?;
                let tx = &mut *guard;
                for p in &ordered {
                    let pid = p.id.get().copied().unwrap_or_default();
                    crate::page_delete::clear_page_references_tx(tx, pid).await?;
                }
                for p in ordered {
                    p.delete_tx(tx).await?;
                }
                crate::cache_invalidate::invalidate_urls_on_commit(
                    invalidator,
                    tenant_slug,
                    purge_urls,
                );
                Ok::<(), rustango::sql::ExecError>(())
            })
            .await?;
            // #553 — disable redirect rules linked to any deleted page.
            for p in &pages {
                let pid = p.id.get().copied().unwrap_or_default();
                if let Err(e) = crate::redirect::disable_for_page(
                    pool,
                    pid,
                    &format!("Linked page “{}” was deleted", p.title),
                )
                .await
                {
                    tracing::warn!(page_id = pid, error = %e, "failed to disable redirects for deleted page");
                }
            }
            redirect_named_with_message_plural(
                "rcms-admin:pages:list",
                MsgLevel::Success,
                "Deleted {count} page(s).",
                pages.len() as i64,
                &[("count", &pages.len().to_string())],
                &headers,
            )
        }
        "move" => {
            let Some(new_parent_id) = new_parent_raw else {
                return redirect_named_with_message(
                    "rcms-admin:pages:list",
                    MsgLevel::Warning,
                    "Bulk move requires a destination — pick a parent page.",
                    &headers,
                );
            };
            let new_parent: Option<Page> = if new_parent_id > 0 {
                Page::objects()
                    .where_(Page::id.eq(new_parent_id))
                    .first(pool).await?
            } else {
                None
            };
            // Self-loop guard: can't move a page under itself or any
            // of its descendants. Path-prefix check is cheap.
            if let Some(np) = &new_parent {
                for p in &pages {
                    if np.path.starts_with(&p.path) {
                        return redirect_named_with_message_args(
                            "rcms-admin:pages:list",
                            MsgLevel::Error,
                            "Can't move “{title}” under “{parent}” — destination is itself or a descendant.",
                            &[("title", p.title.as_str()), ("parent", np.title.as_str())],
                            &headers,
                        );
                    }
                }
            }

            // Pre-validate page-type placement for every selected row
            // before mutating anything. If a single page rejects the
            // destination, the whole batch fails — matches the issue's
            // atomicity requirement so editors never end up with a
            // partial reparent.
            if let Some(np) = &new_parent {
                for p in &pages {
                    if let Err(e) =
                        crate::tree_ops::validate_child_placement(pool, np.page_type_id, p.page_type_id)
                            .await
                    {
                        let err = e.to_string();
                        return redirect_named_with_message_args(
                            "rcms-admin:pages:list",
                            MsgLevel::Error,
                            "Move blocked — “{title}” can't live under “{parent}”: {error}",
                            &[("title", p.title.as_str()), ("parent", np.title.as_str()), ("error", err.as_str())],
                            &headers,
                        );
                    }
                }
            }

            // F2 / #317 — reparent the whole batch in ONE atomic_tree so a
            // mid-batch failure rolls back every move (no partial reparent;
            // placement was already pre-validated above). Each page's
            // subtree is pre-read on the pool *before* the transaction, so
            // nothing inside the tx reads the pool (which would deadlock a
            // single-connection SQLite pool); `move_to_in_tx` then reparents
            // each page within the shared tx. The #78 purge of every page's
            // old + new URL + descendant URLs (captured pre-cascade) is
            // registered via on_commit, firing only once the batch commits.
            let mut to_move: Vec<(Page, Vec<Page>)> = Vec::with_capacity(pages.len());
            for p in pages.iter().cloned() {
                let subtree = p.descendants(pool).await?;
                to_move.push((p, subtree));
            }
            let moved = to_move.len();
            let invalidator = _state.cache_invalidator.clone();
            let tenant_slug = tenant.org.slug.clone();
            let np_for_tx = new_parent.clone();
            crate::tree_ops::atomic_tree(pool, move |tx| {
                Box::pin(async move {
                    let mut purge: Vec<String> = Vec::new();
                    for (mut p, subtree) in to_move {
                        let old_url = p.url_path.clone();
                        let descendant_old_urls: Vec<String> =
                            subtree.iter().map(|d| d.url_path.clone()).collect();
                        // Moves the URLs with the tree (#706).
                        p.move_to_in_tx(np_for_tx.as_ref(), subtree, tx).await?;
                        purge.push(old_url);
                        purge.push(p.url_path.clone());
                        purge.extend(descendant_old_urls);
                    }
                    crate::cache_invalidate::invalidate_urls_on_commit(
                        invalidator,
                        tenant_slug,
                        purge,
                    );
                    Ok(())
                })
            })
            .await?;
            redirect_named_with_message_plural(
                "rcms-admin:pages:list",
                MsgLevel::Success,
                "Moved {count} page(s) under “{parent}”.",
                moved as i64,
                &[
                    ("count", &moved.to_string()),
                    (
                        "parent",
                        &new_parent.map(|p| p.title).unwrap_or_else(|| "Root".to_owned()),
                    ),
                ],
                &headers,
            )
        }
        "copy-to-locale" => {
            // #438 — simple-translation bulk: seed `cms_translation`
            // rows for each selected page from its canonical text, so
            // translators start from the source rather than a blank
            // form. Existing translations are left untouched.
            let code = form
                .iter()
                .find(|(k, _)| k == "locale")
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            let locale = Locale::objects()
                .where_(Locale::code.eq(code.to_owned()))
                .where_(Locale::active.eq(true))
                .first(pool)
                .await?;
            let Some(locale) = locale.filter(|l| !l.is_default) else {
                return redirect_named_with_message(
                    "rcms-admin:pages:list",
                    MsgLevel::Warning,
                    "Copy to locale needs a target locale (and not the default — canonical content already lives there).",
                    &headers,
                );
            };
            let locale_id = locale.id.get().copied().unwrap_or_default();
            let pages_for_tx = pages.clone();
            let (seeded_pages, seeded_fields) = rustango::atomic!(pool, |tx| {
                let mut guard = tx.lock().await?;
                let tx = &mut *guard;
                let mut sp = 0usize;
                let mut sf = 0usize;
                for p in &pages_for_tx {
                    let Some(pid) = p.id.get().copied() else {
                        continue;
                    };
                    // Field paths already translated for this locale —
                    // never overwrite a translator's work.
                    let existing: Vec<crate::translation::Translation> =
                        crate::translation::Translation::objects()
                            .where_(crate::translation::Translation::page_id.eq(pid))
                            .where_(crate::translation::Translation::locale_id.eq(locale_id))
                            .fetch_tx(tx)
                            .await?;
                    let have: std::collections::HashSet<String> =
                        existing.into_iter().map(|t| t.field_path).collect();
                    let mut touched = false;
                    for (path, value) in copy_fields_for(p) {
                        if have.contains(path) {
                            continue;
                        }
                        let mut row = crate::translation::Translation {
                            id: rustango::sql::Auto::Unset,
                            page_id: pid,
                            locale_id,
                            field_path: path.to_owned(),
                            value,
                            updated_at: rustango::sql::Auto::Unset,
                        };
                        row.save_tx(tx).await?;
                        sf += 1;
                        touched = true;
                    }
                    if touched {
                        sp += 1;
                    }
                }
                Ok::<_, rustango::sql::ExecError>((sp, sf))
            })
            .await?;
            // Two independent counts → compose localized noun-phrases into a
            // localized sentence template (#528/#1102).
            let fields = super::i18n::tr_plural(
                &headers,
                "{count} field(s)",
                seeded_fields as i64,
                &[("count", &seeded_fields.to_string())],
            );
            let pages_ph = super::i18n::tr_plural(
                &headers,
                "{count} page(s)",
                seeded_pages as i64,
                &[("count", &seeded_pages.to_string())],
            );
            redirect_named_with_message_args(
                "rcms-admin:pages:list",
                MsgLevel::Success,
                "Seeded {fields} across {pages} into “{locale}”. Existing translations were left untouched.",
                &[
                    ("fields", fields.as_str()),
                    ("pages", pages_ph.as_str()),
                    ("locale", locale.name.as_str()),
                ],
                &headers,
            )
        }
        "archive" | "unarchive" => {
            // #556 — archive: preserve superseded content (serve 200,
            // stay searchable, frozen against delete/unpublish).
            // Unarchive: revive to published. Descendants inherit the
            // treatment at render time, so the whole public subtree is
            // purged; with `archive_cascade` their own rows flip too.
            let archive = action == "archive";
            let plan =
                crate::page::plan_archive(pool, &pages, archive, archive_cascade).await?;
            let invalidator = _state.cache_invalidator.clone();
            let tenant_slug = tenant.org.slug.clone();
            let purge = plan.changed_urls.clone();
            let flips = plan.to_flip.clone();
            let touched = rustango::atomic!(pool, |tx| {
                let mut guard = tx.lock().await?;
                let tx = &mut *guard;
                let mut n = 0usize;
                for mut p in flips {
                    p.save_tx(tx).await?;
                    n += 1;
                }
                crate::cache_invalidate::invalidate_urls_on_commit(
                    invalidator,
                    tenant_slug,
                    purge,
                );
                Ok::<usize, rustango::sql::ExecError>(n)
            })
            .await?;
            // Bookkeeping outside the tx (best-effort): a revision per
            // flipped row + search-index sync (archived stays indexed;
            // the FTS path is query-time so this is a no-op there).
            for p in &plan.to_flip {
                crate::revision::capture(pool, p, None)
                    .await
                    .log_warn("revision not captured; the history has a gap");
                crate::task_queue::sync_page_search(&tenant.org.slug, p).await;
            }
            let key = if archive {
                "Archived {count} page(s)."
            } else {
                "Unarchived {count} page(s)."
            };
            redirect_named_with_message_plural(
                "rcms-admin:pages:list",
                MsgLevel::Success,
                key,
                touched as i64,
                &[("count", &touched.to_string())],
                &headers,
            )
        }
        other => Err(AdminError::Validation(format!(
            "unknown bulk action `{other}` (expected publish / unpublish / archive / unarchive / delete / move / copy-to-locale)"
        ))),
    }
}

/// POST /cms-admin/pages/{id}/unlock — force-release the editor lock
/// (#72). Anyone with Edit permission on this page can call it. The
/// row is deleted and an audit-log entry is written so the action is
/// reversible to inspect.
/// POST /cms-admin/pages/{id}/lock-release — the editor was left: give
/// up our own lock right away, not 30 minutes later, so a colleague who
/// opens the page next can save. Only ever releases the caller's lock.
pub async fn page_lock_release(
    tenant: Tenant,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let Some(viewer_id) = session_user.as_ref().and_then(|u| u.id.get().copied()) else {
        return Ok(unauthorized_no_session());
    };
    crate::lock::release(tenant.pool(), id, viewer_id).await?;
    Ok((axum::http::StatusCode::NO_CONTENT, "").into_response())
}

pub async fn page_unlock_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let released = crate::lock::force_release(tenant.pool(), id).await?;
    let actor_id = session_user.as_ref().and_then(|u| u.id.get().copied());
    let message = if let Some(row) = released {
        let holder = rustango::tenancy::auth::User::objects()
            .where_(rustango::tenancy::auth::User::id.eq(row.user_id))
            .fetch(tenant.pool())
            .await
            .ok()
            .and_then(|v| v.into_iter().next())
            .map(|u| u.username)
            .unwrap_or_else(|| format!("user#{}", row.user_id));
        // #192 — audit log the forced unlock.
        crate::page_log::record_or_warn(
            tenant.pool(),
            id,
            crate::page_log::ACTION_FORCE_UNLOCK,
            actor_id,
            format!("Force-unlocked — “{holder}” no longer holds the editor lock."),
            Some(serde_json::json!({ "previous_holder_user_id": row.user_id })),
        )
        .await;
        format!("Lock released — “{holder}” no longer holds the editor lock on this page.")
    } else {
        "Lock was already released.".to_owned()
    };
    redirect_named_with_params_and_message(
        "rcms-admin:pages:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        &message,
        &headers,
    )
}

/// POST /cms-admin/pages/{id}/subscribe — opt the current user in
/// to publish notifications for this page (#115, Wagtail parity B3).
/// Idempotent — re-subscribing returns success without inserting a
/// duplicate row.
pub async fn page_subscribe_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let Some(viewer) = session_user.as_ref() else {
        return Ok(unauthorized_no_session());
    };
    let viewer_id = viewer.id.get().copied().unwrap_or_default();
    crate::page_subscription::subscribe(tenant.pool(), id, viewer_id).await?;
    redirect_named_with_params_and_message(
        "rcms-admin:pages:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Subscribed — you'll get an email when this page publishes.",
        &headers,
    )
}

/// POST /cms-admin/pages/{id}/unsubscribe — opt the current user out
/// of publish notifications. No-op when not subscribed.
pub async fn page_unsubscribe_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let Some(viewer) = session_user.as_ref() else {
        return Ok(unauthorized_no_session());
    };
    let viewer_id = viewer.id.get().copied().unwrap_or_default();
    crate::page_subscription::unsubscribe(tenant.pool(), id, viewer_id).await?;
    redirect_named_with_params_and_message(
        "rcms-admin:pages:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Unsubscribed — you won't get future publish notifications for this page.",
        &headers,
    )
}

/// True while the page is mid-review and `user` may not decide the
/// current step: the editor is read-only for them, so they must not
/// take (or keep) the edit lock — a submitter who reopens the page
/// would otherwise lock the reviewer out.
async fn read_only_under_review(
    pool: &rustango::sql::Pool,
    page_id: i64,
    user: &rustango::tenancy::auth::User,
) -> Result<bool, AdminError> {
    if user.is_superuser {
        return Ok(false);
    }
    let Some(state) = crate::workflow::active_state_for_page(pool, page_id).await? else {
        return Ok(false);
    };
    if state.status != crate::workflow::WorkflowStatus::InProgress.as_str() {
        return Ok(false);
    }
    let tasks = crate::workflow::tasks_for(pool, state.workflow_id).await?;
    let Some(task) = state
        .current_task_id
        .and_then(|tid| tasks.iter().find(|t| t.id.get().copied() == Some(tid)))
    else {
        return Ok(false);
    };
    let uid = user.id.get().copied().unwrap_or_default();
    Ok(!user_is_in_role(pool, uid, task.role_id).await?)
}

/// POST /cms-admin/pages/{id}/lock-heartbeat — refresh the TTL on
/// the caller's lock (#72). Called every
/// `lock::HEARTBEAT_INTERVAL_SECS` from the page-editor JS while the
/// tab stays open. Returns:
///   * 204 — heartbeat accepted (lock refreshed, or first claim).
///   * 409 — someone else owns the lock; the editor JS should
///           reload to surface the banner.
/// Heartbeat query: editors send `?is_editing=1` once they've
/// touched the form so the presence banner can show "editing" vs
/// "viewing" for other viewers.
#[derive(Debug, Deserialize)]
pub struct HeartbeatQuery {
    #[serde(default)]
    pub is_editing: Option<String>,
}

pub async fn page_lock_heartbeat(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Query(q): Query<HeartbeatQuery>,
) -> Result<Response, AdminError> {
    let Some(viewer_id) = session_user.as_ref().and_then(|u| u.id.get().copied()) else {
        return Ok(unauthorized_no_session());
    };

    // #106 — touch the editing-session row on every ping. Stale
    // rows GC lazily here so we don't need a cron.
    let is_editing = q.is_editing.as_deref() == Some("1");
    if let Err(e) = crate::editing_session::touch(tenant.pool(), id, viewer_id, is_editing).await {
        tracing::warn!(target: "rustango_cms::admin", page_id = id, error = %e, "editing-session touch failed (continuing)");
    }
    // GC every ~50 heartbeats to keep the table bounded without
    // doing the SELECT on every call. Probabilistic — fine for v1.
    if rand::random::<u8>() < 5 {
        crate::editing_session::gc_stale(tenant.pool())
            .await
            .log_warn("stale editing sessions not collected");
    }

    // Read-only during someone else's review step: no lock to keep.
    if let Some(u) = session_user.as_ref() {
        if read_only_under_review(tenant.pool(), id, u).await? {
            return Ok((axum::http::StatusCode::NO_CONTENT, "").into_response());
        }
    }
    match crate::lock::acquire(tenant.pool(), id, viewer_id, false).await {
        Ok(crate::lock::AcquireOutcome::Acquired(_)) => {
            Ok((axum::http::StatusCode::NO_CONTENT, "").into_response())
        }
        Ok(crate::lock::AcquireOutcome::Held(_)) => Ok((
            axum::http::StatusCode::CONFLICT,
            "lock held by another editor",
        )
            .into_response()),
        Err(e) => {
            tracing::warn!(target: "rustango_cms::admin", page_id = id, error = %e, "lock heartbeat failed");
            Err(e.into())
        }
    }
}

/// GET /cms-admin/pages/{id}/sessions — JSON list of OTHER active
/// editors / viewers on this page (#106). Returns
/// `[{user_id, username, last_seen_at, is_editing}, ...]`.
pub async fn page_sessions_json(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let Some(viewer_id) = session_user.as_ref().and_then(|u| u.id.get().copied()) else {
        return Ok(unauthorized_no_session());
    };
    let others = crate::editing_session::others_on_page(tenant.pool(), id, viewer_id)
        .await
        .unwrap_or_default();
    if others.is_empty() {
        return Ok(([(header::CONTENT_TYPE, "application/json")], "[]").into_response());
    }
    // Resolve usernames in one batched lookup.
    let user_ids: Vec<i64> = others.iter().map(|s| s.user_id).collect();
    let users: Vec<rustango::tenancy::auth::User> = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.is_in(user_ids))
        .fetch(tenant.pool())
        .await
        .unwrap_or_default();
    let username_by_id: std::collections::HashMap<i64, String> = users
        .into_iter()
        .filter_map(|u| u.id.get().copied().map(|id| (id, u.username)))
        .collect();
    let payload: Vec<serde_json::Value> = others
        .iter()
        .map(|s| {
            serde_json::json!({
                "user_id": s.user_id,
                "username": username_by_id.get(&s.user_id).cloned(),
                "last_seen_at": s.last_seen_at,
                "is_editing": s.is_editing,
            })
        })
        .collect();
    let body = serde_json::to_string(&payload).unwrap_or_else(|_| "[]".to_owned());
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

/// Pick a slug not already used by any sibling under `parent_id`.
/// Tries `desired`, then `desired-2`, `desired-3`, … up to 100
/// attempts. Returns the first available one.
async fn unique_sibling_slug(
    pool: &rustango::sql::Pool,
    parent_id: Option<i64>,
    desired: &str,
) -> Result<String, AdminError> {
    use rustango::core::Column as _;
    let qs = Page::objects().order_by(&[("slug", false)]);
    let siblings: Vec<Page> = match parent_id {
        Some(pid) => qs.where_(Page::parent_id.eq(pid)).fetch(pool).await?,
        None => qs.where_(Page::parent_id.is_null()).fetch(pool).await?,
    };
    let taken: std::collections::HashSet<String> = siblings.into_iter().map(|p| p.slug).collect();
    if !taken.contains(desired) {
        return Ok(desired.to_owned());
    }
    for n in 2..=100 {
        let candidate = format!("{desired}-{n}");
        if !taken.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err(AdminError::Validation(
        "Could not allocate a unique slug for the clone — over 100 siblings share the same base."
            .to_owned(),
    ))
}

/// POST /cms-admin/pages/{id}/revert/{rev_id} — replace the page's
/// mutable fields with those from the given revision, then capture a
/// NEW revision recording the revert. URL-shaped fields (slug, path,
/// depth, parent_id, sort_order) are NOT reverted — moving the page
/// is a tree-ops concern, not a content concern.
///
/// Returns to the edit form afterward so the editor can verify the
/// state and continue editing.
pub async fn page_revert_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path((id, rev_id)): Path<(i64, i64)>,
) -> Result<Response, AdminError> {
    let mut page = Page::objects()
        .where_(Page::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let rev = crate::revision::Revision::objects()
        .where_(crate::revision::Revision::id.eq(rev_id))
        .where_(crate::revision::Revision::page_id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(rev_id))?;

    // Decode the snapshot into a Page shell. Tree-shape fields
    // (parent_id, path, depth, sort_order, slug, url_path) are
    // preserved from the LIVE row — a revert restores content +
    // status, not tree placement.
    let snapshot: Page = match serde_json::from_value(rev.snapshot.clone()) {
        Ok(p) => p,
        Err(e) => {
            return Ok((
                axum::http::StatusCode::BAD_REQUEST,
                format!("revision snapshot could not be decoded: {e}"),
            )
                .into_response());
        }
    };

    let old_url_path = page.url_path.clone();
    page.title = snapshot.title;
    page.page_type_id = snapshot.page_type_id;
    page.status = snapshot.status;
    page.published_at = snapshot.published_at;
    page.go_live_at = snapshot.go_live_at;
    page.expire_at = snapshot.expire_at;
    page.seo_title = snapshot.seo_title;
    page.seo_description = snapshot.seo_description;
    page.robots_index = snapshot.robots_index;
    page.sitemap_priority = snapshot.sitemap_priority;

    let mut tx = transaction_pool(tenant.pool()).await?;
    page.save_tx(&mut tx).await?;
    tx.commit().await?;
    crate::revision::capture(tenant.pool(), &page, None)
        .await
        .log_warn("revision not captured; the history has a gap");

    let _ = old_url_path; // TTL-based cache propagation per rustango::cache_page
                          // #192 — audit log the restore.
    let actor_id = session_user.as_ref().and_then(|u| u.id.get().copied());
    crate::page_log::record_or_warn(
        tenant.pool(),
        id,
        crate::page_log::ACTION_RESTORE,
        actor_id,
        format!("Reverted to revision #{}.", rev.sequence),
        Some(serde_json::json!({ "revision_id": rev_id, "sequence": rev.sequence })),
    )
    .await;
    let sequence_str = rev.sequence.to_string();
    redirect_named_with_params_and_message_args(
        "rcms-admin:pages:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Reverted to revision #{sequence}.",
        &[("sequence", sequence_str.as_str())],
        &headers,
    )
}

// =====================================================================
// Live preview
// =====================================================================

/// Stamp the response with anti-cache headers so the editor's preview
/// iframe always renders the latest content. Browsers cache HTML
/// aggressively by default; the editor's "Refresh" button and the
/// initial load both need to hit fresh markup.
fn stamp_no_cache(mut response: Response) -> Response {
    let h = response.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate, max-age=0"),
    );
    h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    h.insert(header::EXPIRES, HeaderValue::from_static("0"));
    // Tell upstream caches the response varies per-request even
    // for the same URL (preview pulls live data on every load).
    h.insert(header::VARY, HeaderValue::from_static("*"));
    // Block embedding outside our own admin so other origins can't
    // frame the preview for clickjacking.
    h.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
    response
}

/// #208 — inject the axe-core preview-glue script into the
/// rendered preview HTML so the editor can surface client-side
/// accessibility findings alongside the server-side heuristics
/// (#116). Reads the response body, locates the closing `</body>`
/// tag (case-insensitive), and splices two `<script src="…">`
/// tags before it: axe-core itself, then the small glue script
/// that calls `axe.run()` + `postMessage`s the results back to
/// the editor's parent window.
///
/// Non-HTML responses pass through unchanged; pages without a
/// `</body>` tag (malformed templates, plain-text responses) also
/// pass through so the injection is best-effort + non-breaking.
async fn inject_axe_into_preview(response: Response) -> Response {
    // Only touch text/html responses.
    let is_html = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/html"));
    if !is_html {
        return response;
    }
    let (parts, body) = response.into_parts();
    // 4 MiB cap matches the rendition route's preview-safe upper
    // bound; admin preview HTML should never approach this.
    let bytes = match axum::body::to_bytes(body, 4 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            // Body read failed — return a minimal 500 since we
            // already consumed the original.
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "preview body read failed",
            )
                .into_response();
        }
    };
    let body_str = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => {
            // Non-UTF-8 HTML — rebuild without injection.
            return axum::http::Response::from_parts(parts, axum::body::Body::from(bytes));
        }
    };
    let new_body = inject_axe_into_body_str(body_str);
    // Rebuild the response with the patched body + a refreshed
    // Content-Length so axum doesn't emit the stale value.
    let mut resp = axum::http::Response::from_parts(parts, axum::body::Body::from(new_body));
    resp.headers_mut().remove(header::CONTENT_LENGTH);
    resp
}

/// Pure string helper for [`inject_axe_into_preview`] — splice the
/// axe-core + glue `<script>` tags before the closing `</body>`
/// (case-insensitive `rfind`). Pages with no closing body tag get
/// the scripts appended at the end (browsers tolerate trailing
/// `<script>` after `</html>`). Extracted so the injection logic
/// is unit-testable without spinning up an axum Response stack.
fn inject_axe_into_body_str(body: &str) -> String {
    // Fingerprinted like every other admin asset (`?v=`): these are served
    // `immutable`, so an unversioned URL would let a year-old cached copy
    // mask an update.
    let v = super::cms_asset_version();
    let scripts = format!(
        "<script src=\"/cms-admin/static/vendor/axe-core.min.js?v={v}\" defer></script>\n\
         <script src=\"/cms-admin/static/cms-preview-guard.js?v={v}\" defer></script>\n\
         <script src=\"/cms-admin/static/cms-axe-preview.js?v={v}\" defer></script>\n"
    );
    let lower = body.to_ascii_lowercase();
    if let Some(idx) = lower.rfind("</body>") {
        let mut out = String::with_capacity(body.len() + scripts.len());
        out.push_str(&body[..idx]);
        out.push_str(&scripts);
        out.push_str(&body[idx..]);
        out
    } else {
        let mut out = String::with_capacity(body.len() + scripts.len());
        out.push_str(body);
        out.push_str(&scripts);
        out
    }
}

#[cfg(test)]
mod axe_injection_tests {
    use super::inject_axe_into_body_str;

    #[test]
    fn splices_before_lowercase_body_close() {
        let html = "<html><body><h1>hi</h1></body></html>";
        let out = inject_axe_into_body_str(html);
        assert!(out.contains("axe-core.min.js"), "axe script missing: {out}");
        assert!(
            out.contains("cms-axe-preview.js"),
            "glue script missing: {out}"
        );
        assert!(
            out.contains("cms-preview-guard.js"),
            "preview-guard script missing: {out}"
        );
        // Both scripts land BEFORE the body close.
        let close = out.find("</body>").expect("body close kept");
        assert!(
            out[..close].contains("axe-core.min.js"),
            "axe script should be before </body>: {out}"
        );
        assert!(
            out[..close].contains("cms-axe-preview.js"),
            "glue script should be before </body>: {out}"
        );
    }

    #[test]
    fn splices_before_uppercase_body_close() {
        let html = "<HTML><BODY><h1>hi</h1></BODY></HTML>";
        let out = inject_axe_into_body_str(html);
        // The original close tag stays uppercase; injection is still
        // positioned in front of it.
        let close = out.find("</BODY>").expect("uppercase body close kept");
        assert!(out[..close].contains("axe-core.min.js"));
    }

    #[test]
    fn appends_when_no_body_close() {
        let html = "<html><h1>hi</h1>";
        let out = inject_axe_into_body_str(html);
        // URLs carry a `?v=<fingerprint>` cache-bust token, so assert on the
        // script identity + trailing position rather than an exact URL.
        assert!(out.contains("cms-axe-preview.js?v="));
        assert!(out.trim_end().ends_with("defer></script>"));
        assert!(out.contains("axe-core.min.js"));
        // Original content preserved.
        assert!(out.starts_with("<html><h1>hi</h1>"));
    }

    #[test]
    fn uses_rfind_for_nested_body_close_text() {
        // A template that mentions "</body>" inside a comment / text
        // string and ALSO has a real closing tag — we want to splice
        // before the LAST one, not the first.
        let html = "<html><body><pre>literal: &lt;/body&gt;</pre></body></html>";
        let out = inject_axe_into_body_str(html);
        let close = out.rfind("</body>").unwrap();
        assert!(out[..close].contains("axe-core.min.js"));
        // Make sure the literal in the <pre> survived.
        assert!(out.contains("literal: &lt;/body&gt;"));
    }

    #[test]
    fn empty_body_renders_clean() {
        let out = inject_axe_into_body_str("");
        // Nothing to splice before; just the scripts.
        assert!(out.contains("axe-core.min.js"));
        assert!(out.contains("cms-axe-preview.js"));
    }
}

/// Query string for the preview endpoints — carries the editing locale
/// when the editor is in translation mode (`?locale=fr`) so the preview
/// renders that locale's translation instead of the canonical content.
#[derive(serde::Deserialize, Default)]
pub struct PreviewLocaleQuery {
    #[serde(default)]
    locale: Option<String>,
    /// `?format=json` — show the page's API representation instead of
    /// its rendered HTML. This is the editor's preview switch, and it
    /// works for **any** page type, not just ones whose `view_mode`
    /// opts into a public JSON view: an authenticated editor asking to
    /// inspect the API shape is a different act from an anonymous
    /// client negotiating for it.
    #[serde(default)]
    format: Option<String>,
}

/// The Tera a preview must render through: the tenant's, not the global set.
///
/// The public renderer resolves templates per tenant (`router.rs` →
/// [`crate::tenant_templates::TenantTemplates::for_tenant`]), so a
/// preview built from `state.tera` shows a page the visitor will never
/// get. Three ways it goes wrong, all silent: a tenant's override of a
/// shared template is ignored; a `Page::template_override` naming a
/// template that exists only as an override fails `pick_template`'s
/// existence check and falls back to the page type's default; and a
/// block whose template is a tenant override renders its
/// "missing template" fallback.
///
/// Falls back to the global set when no overlay is installed, which is
/// every single-template-set deployment.
fn preview_tera(state: &super::AdminState, tenant: &Tenant) -> std::sync::Arc<tera::Tera> {
    crate::tenant_templates::installed()
        .map(|tt| tt.for_tenant(&tenant.org.slug))
        .unwrap_or_else(|| state.tera.clone())
}

/// GET /cms-admin/pages/{id}/preview — renders the saved page through
/// the public render pipeline (no admin chrome) so the editor's
/// iframe shows exactly what a visitor would see. Anti-cache headers
/// ensure the iframe always pulls fresh markup. `?locale=` (translation
/// mode) renders that locale's saved translation.
pub async fn page_preview(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    Path(id): Path<i64>,
    axum::extract::Query(q): axum::extract::Query<PreviewLocaleQuery>,
) -> Result<Response, AdminError> {
    let page = Page::objects()
        .where_(Page::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let response = crate::render::render(
        &tenant,
        &preview_tera(&state, &tenant),
        &page,
        "",
        // No site prefix: this path renders the tenant-absolute
        // tree, so links must stay absolute.
        "",
        q.locale.as_deref(),
        None,
        None,
    )
    .await;
    // #208 — splice axe-core + the iframe-glue script before the
    // closing </body> so the editor's accessibility panel can
    // surface client-side findings alongside the server-side ones.
    Ok(stamp_no_cache(inject_axe_into_preview(response).await))
}

/// GET /cms-admin/pages/{id}/preview-token — #430. Mint a short-lived
/// signed preview token so a decoupled / headless frontend can fetch
/// this page's DRAFT via `/api/v2/pages/{id}/?preview_token=…`. Returns
/// JSON `{token, expires_at, api_path}`. Auth is enforced by the admin
/// router's login guard. 422 when no signing secret is configured.
pub async fn page_preview_token(
    tenant: Tenant,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let exists = Page::objects()
        .where_(Page::id.eq(id))
        .first(tenant.pool())
        .await?
        .is_some();
    if !exists {
        return Err(AdminError::NotFound(id));
    }
    // 1-hour token: long enough to click through + iterate, short
    // enough to bound exposure of an unpublished page.
    let expires = chrono::Utc::now().timestamp() + PREVIEW_TOKEN_TTL_SECS;
    match crate::preview_token::mint(&tenant.org.slug, id, expires) {
        Some(token) => Ok(Json(serde_json::json!({
            "token": token,
            "expires_at": expires,
            "api_path": format!("/api/v2/pages/{id}/?preview_token={token}"),
        }))
        .into_response()),
        None => Err(AdminError::Validation(
            "Preview tokens are disabled — set RCMS_SECRET_KEY to enable headless draft preview."
                .to_owned(),
        )),
    }
}

/// POST /cms-admin/pages/{id}/preview — same as the GET variant, but
/// renders a *virtual* page assembled from the posted form values so
/// the editor can preview unsaved changes (Wagtail's
/// "virtual page data" pattern). Nothing is persisted. Anti-cache
/// headers + same-origin frame policy mirror the GET handler.
pub async fn page_preview_draft(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    Path(id): Path<i64>,
    axum::extract::Query(q): axum::extract::Query<PreviewLocaleQuery>,
    headers: HeaderMap,
    Form(form_map): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let mut page = Page::objects()
        .where_(Page::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;

    // Overlay the canonical form fields onto the virtual page. The
    // tree placement (parent / path / url_path) stays as-saved so
    // breadcrumb + ancestor lookups still work.
    if let Some(v) = form_map.get("title") {
        page.title = v.clone();
    }
    if let Some(v) = form_map.get("slug") {
        page.slug = v.clone();
    }
    if let Some(v) = form_map
        .get("page_type_id")
        .and_then(|s| s.parse::<i64>().ok())
    {
        page.page_type_id = v;
    }
    if let Some(v) = form_map.get("status") {
        page.status = v.clone();
    }
    if let Some(v) = form_map.get("seo_title") {
        page.seo_title = v.clone();
    }
    if let Some(v) = form_map.get("seo_description") {
        page.seo_description = v.clone();
    }
    // Checkboxes submit `on` when checked, absent when unchecked.
    page.robots_index = form_map
        .get("robots_index")
        .map(|v| matches!(v.as_str(), "on" | "true" | "1"))
        .unwrap_or(page.robots_index);
    if let Some(v) = form_map
        .get("sitemap_priority")
        .and_then(|s| s.parse::<f32>().ok())
    {
        page.sitemap_priority = v;
    }
    if let Some(raw) = form_map.get("theme_id") {
        page.theme_id = if raw.is_empty() || raw == "0" {
            None
        } else {
            raw.parse::<i64>().ok()
        };
    }

    // The editor's JSON pane sends `Accept: application/json`, so the
    // same renderer that answers the public URL answers the preview —
    // which is what makes the pane show the real response body rather
    // than an admin-side reconstruction of it.
    let accept = crate::page_view::parse_accept(
        headers
            .get(axum::http::header::ACCEPT)
            .and_then(|v| v.to_str().ok()),
    );
    let force_json = q.format.as_deref() == Some("json");
    let response = crate::render::render_preview(
        &tenant,
        &preview_tera(&state, &tenant),
        &page,
        "",
        // Preview renders tenant-absolute paths — see the GET twin.
        "",
        q.locale.as_deref(),
        &form_map,
        accept,
        force_json,
    )
    .await;
    // #208 — same axe splice as the GET preview handler above. It gates
    // on `text/html`, so a JSON preview passes through untouched and
    // never pulls axe-core into the pane.
    Ok(stamp_no_cache(inject_axe_into_preview(response).await))
}

// =====================================================================
// Library tab
// =====================================================================

/// GET /cms-admin/library — placeholder until a real
/// `register_library_type!` macro lands. Shows the registered
/// PageTypeHandlers as a stand-in so the chrome demos cleanly.
/// Query for the paginated, filtered, folder-scoped snippet list.
#[derive(Debug, Deserialize, Default)]
pub struct LibraryListQuery {
    /// When set, focuses the list to one library type. When absent,
    /// the overview (one card per registered type with counts) shows.
    #[serde(default)]
    pub r#type: Option<String>,
    /// Folder prefix to scope the list to. Empty / unset = root.
    #[serde(default)]
    pub folder: Option<String>,
    /// Free-text query — matched against the type handler's
    /// `search_fields()`.
    #[serde(default)]
    pub q: Option<String>,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Rows per page. Named so it is consumed here rather than falling
    /// into `raw_filters` and being read as a filter chip.
    #[serde(default)]
    pub per_page: Option<usize>,
    /// Catch-all bucket for `filter_<key>=<value>` filter chips. Any
    /// query parameter starting with `filter_` lands here.
    #[serde(flatten)]
    pub raw_filters: std::collections::HashMap<String, String>,
}

pub async fn library_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Query(q): Query<LibraryListQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Response, AdminError> {
    // Sidebar / overview data — all types + total counts. Forms are excluded:
    // they have their own dedicated area (`/cms-admin/forms`, sidebar "Forms")
    // with per-form submission counts, so listing them here would be a second,
    // weaker path to the same thing. Direct `?type=form` URLs still work.
    let types: Vec<crate::library::LibraryType> = crate::library::LibraryType::objects()
        .order_by(&[("type_name", false)])
        .fetch(tenant.pool())
        .await?
        .into_iter()
        .filter(|t| t.type_name != "form")
        .collect();
    let all_snippets: Vec<crate::snippet::Snippet> = crate::snippet::Snippet::objects()
        .fetch(tenant.pool())
        .await?;
    let mut counts_by_type: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    for s in &all_snippets {
        *counts_by_type.entry(s.type_name.clone()).or_default() += 1;
    }

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "library", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    // #316 — raw query string for the `| querystring(page=…)` filter in
    // the pagination links (preserves type/folder/q/filter_* + sets page).
    ctx.insert("query_string", &raw_query.unwrap_or_default());
    ctx.insert("types", &types);
    ctx.insert("counts_by_type", &counts_by_type);

    // Overview mode — no `?type=`, show one card per registered type.
    let Some(type_name) = q.r#type.as_deref().filter(|s| !s.is_empty()) else {
        ctx.insert("mode", "overview");
        return render_with_csrf(&state, &headers, "rcms_admin/library_list.html", &mut ctx);
    };

    // Resolve the handler so we can call list_display / search_fields
    // / list_filter / render_cell.
    let handler = crate::library::find_handler(type_name);
    let list_display = handler
        .as_ref()
        .map(|h| h.list_display())
        .unwrap_or_else(|| {
            vec![
                crate::library::ListColumn::Field {
                    column: "title",
                    label: "Title",
                },
                crate::library::ListColumn::Field {
                    column: "slug",
                    label: "Slug",
                },
                crate::library::ListColumn::Field {
                    column: "updated_at",
                    label: "Updated",
                },
            ]
        });
    let search_fields = handler
        .as_ref()
        .map(|h| h.search_fields())
        .unwrap_or_else(|| vec!["title", "slug"]);
    let filter_keys = handler
        .as_ref()
        .map(|h| h.list_filter())
        .unwrap_or_default();

    // Folder scoping. Snippets with `folder_path = <current>` are
    // direct contents; we also surface the immediate sub-folder
    // names so the user can drill in.
    let folder = crate::snippet::normalize_folder(q.folder.as_deref().unwrap_or(""));
    let child_folders = crate::snippet::child_folders(tenant.pool(), &folder)
        .await
        .unwrap_or_default();

    // Apply filters — type, folder, then per-data.<key> chips +
    // per-row `can_view` (#21 phase 1) so handler-supplied hooks
    // suppress rows from the list.
    let mut filtered: Vec<crate::snippet::Snippet> = all_snippets
        .iter()
        .filter(|s| s.type_name == type_name && s.folder_path == folder)
        .filter(|s| handler.as_ref().map(|h| h.can_view(s)).unwrap_or(true))
        .cloned()
        .collect();

    // Search — ILIKE pattern on each search_field, ANY match wins.
    let search_q = q.q.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty());
    if let Some(needle) = search_q {
        let needle_lower = needle.to_lowercase();
        filtered.retain(|s| {
            search_fields.iter().any(|field| {
                let hay = match *field {
                    "title" => s.title.to_lowercase(),
                    "slug" => s.slug.to_lowercase(),
                    "body_markdown" => s.body_markdown.to_lowercase(),
                    other => s
                        .data
                        .get(other)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_lowercase)
                        .unwrap_or_default(),
                };
                hay.contains(&needle_lower)
            })
        });
    }

    // `filter_<key>=<value>` chips — flatten raw_filters into a
    // typed map first so we don't carry useless keys into context.
    let mut active_filters: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for (k, v) in &q.raw_filters {
        if let Some(stripped) = k.strip_prefix("filter_") {
            if !v.is_empty() {
                active_filters.insert(stripped.to_owned(), v.clone());
            }
        }
    }
    if !active_filters.is_empty() {
        filtered.retain(|s| {
            active_filters.iter().all(|(key, want)| {
                let got = s
                    .data
                    .get(key)
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();
                got == *want
            })
        });
    }

    // Filter chip options (distinct values per filterable key).
    let filter_options: std::collections::HashMap<String, Vec<String>> = filter_keys
        .iter()
        .map(|k| {
            let mut opts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for s in &all_snippets {
                if s.type_name != type_name {
                    continue;
                }
                if let Some(v) = s.data.get(*k) {
                    let s = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    if !s.is_empty() {
                        opts.insert(s);
                    }
                }
            }
            ((*k).to_owned(), opts.into_iter().collect())
        })
        .collect();

    // Pagination — driven by `rustango::pagination::Paginator`.
    // `get_page()` never errors; out-of-range numbers clamp to the
    // last page (or 1 when empty). The slice indices come from
    // `start_index() - 1 .. end_index()` (the paginator returns
    // 1-based inclusive indices for "Showing X–Y" UI lines).
    let total = filtered.len();
    let page_size = crate::admin::pagination::clamp_per_page(q.per_page);
    let paginator = rustango::pagination::Paginator::new(total, page_size);
    let requested_page = q.page.unwrap_or(1).max(1) as i64;
    let page_obj = paginator.get_page(requested_page);
    let page = page_obj.number;
    let total_pages = paginator.num_pages().max(1);
    let page_rows: Vec<crate::snippet::Snippet> = if total == 0 {
        Vec::new()
    } else {
        let start = page_obj.start_index().saturating_sub(1);
        let end = page_obj.end_index();
        filtered[start..end].to_vec()
    };

    // Pre-render each row's cells so the template doesn't need to
    // know about ListColumn variants. Each row → Vec<String> of
    // pre-escaped HTML strings (cells), aligned with `column_headers`.
    let column_headers: Vec<serde_json::Value> = list_display
        .iter()
        .map(|c| {
            serde_json::json!({
                "key": c.key(),
                "label": c.label(),
            })
        })
        .collect();
    let rows_html: Vec<serde_json::Value> = page_rows
        .iter()
        .map(|row| {
            let cells: Vec<String> = list_display
                .iter()
                .map(|c| render_list_cell(c, row, handler.as_deref()))
                .collect();
            serde_json::json!({
                "id": row.id.get().copied().unwrap_or_default(),
                "slug": row.slug,
                "cells": cells,
            })
        })
        .collect();

    let active_filters_json: Vec<serde_json::Value> = active_filters
        .iter()
        .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
        .collect();

    // Handler-declared bulk + row actions (#21 phase 2). Serialize
    // the structs into the JSON shape the template iterates.
    let bulk_actions: Vec<serde_json::Value> = handler
        .as_ref()
        .map(|h| h.bulk_actions())
        .unwrap_or_default()
        .iter()
        .map(serialize_bulk_action)
        .collect();
    let row_actions: Vec<serde_json::Value> = handler
        .as_ref()
        .map(|h| h.row_actions())
        .unwrap_or_default()
        .iter()
        .map(serialize_bulk_action)
        .collect();

    // #134 — plugin-contributed bulk actions targeting `cms_snippet`.
    let plugin_bulk_actions: Vec<serde_json::Value> = crate::hooks::bulk_actions_for("cms_snippet")
        .into_iter()
        .map(|a| {
            serde_json::json!({
                "key": a.key,
                "label": a.label,
                "icon": a.icon,
                "danger": a.danger,
                "confirm_message": a.confirm_message,
                "action_url": a.action_url,
            })
        })
        .collect();

    // #196 — full folder tree for the left rail. Flat list of every
    // folder_path (including derived intermediates), sorted; the
    // template renders each row with depth-based indentation based
    // on the slash count.
    let folder_tree_paths = crate::snippet::folder_tree_for_type(tenant.pool(), type_name)
        .await
        .unwrap_or_default();
    let folder_tree: Vec<serde_json::Value> = folder_tree_paths
        .iter()
        .map(|p| {
            let depth = p.matches('/').count(); // root="" → 0; "a/" → 1; "a/b/" → 2
            let display = if p.is_empty() {
                "Root".to_owned()
            } else {
                // Last non-empty segment is the displayed name.
                p.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_owned()
            };
            serde_json::json!({
                "path": p,
                "name": display,
                "depth": depth,
                "active": *p == folder,
            })
        })
        .collect();

    ctx.insert("mode", "type");
    ctx.insert("active_type", type_name);
    ctx.insert("folder", &folder);
    ctx.insert("child_folders", &child_folders);
    ctx.insert("folder_tree", &folder_tree);
    ctx.insert("column_headers", &column_headers);
    ctx.insert("rows", &rows_html);
    ctx.insert("plugin_bulk_actions", &plugin_bulk_actions);
    ctx.insert("page", &page);
    ctx.insert("total_pages", &total_pages);
    // Shared control row (`rcms_admin/_pagination.html`); the bespoke
    // vars above stay for the rest of the template.
    ctx.insert(
        "pagination",
        &crate::admin::pagination::context(page as i64, page_size, total),
    );
    ctx.insert("total", &total);
    ctx.insert("page_size", &page_size);
    ctx.insert("search_q", &search_q.unwrap_or(""));
    ctx.insert("filter_keys", &filter_keys);
    ctx.insert("filter_options", &filter_options);
    ctx.insert("active_filters", &active_filters_json);
    ctx.insert("bulk_actions", &bulk_actions);
    ctx.insert("row_actions", &row_actions);
    render_with_csrf(&state, &headers, "rcms_admin/library_list.html", &mut ctx)
}

/// Bulk-action button shape the library_list template iterates.
fn serialize_bulk_action(a: &crate::library::BulkAction) -> serde_json::Value {
    use crate::library::BulkActionStyle;
    let style_class = match a.style {
        BulkActionStyle::Primary => "btn",
        BulkActionStyle::Outlined => "btn btn-outlined",
        BulkActionStyle::Danger => "btn btn-danger",
    };
    serde_json::json!({
        "name": a.name,
        "label": a.label,
        "icon": a.icon,
        "confirm_message": a.confirm_message,
        "style_class": style_class,
    })
}

/// POST /cms-admin/library/{type}/bulk — dispatch a bulk action
/// to the registered LibraryTypeHandler. Body shape:
/// `action=<name>&ids=<id>&ids=<id>...`. Returns the editor back
/// to the type's list view with a flash message reporting how many
/// rows the handler processed.
pub async fn library_bulk_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(type_name): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, AdminError> {
    let action = form
        .iter()
        .find(|(k, _)| k == "action")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let ids: Vec<i64> = form
        .iter()
        .filter(|(k, _)| k == "ids" || k == "id")
        .filter_map(|(_, v)| v.parse::<i64>().ok())
        .collect();
    if action.is_empty() || ids.is_empty() {
        return redirect_named_with_message(
            "rcms-admin:library:list",
            MsgLevel::Warning,
            "Pick at least one row + a bulk action before submitting.",
            &headers,
        );
    }
    let Some(handler) = crate::library::find_handler(&type_name) else {
        return Err(AdminError::Validation(format!(
            "unknown library type `{type_name}`"
        )));
    };
    // Validate that the named action is in the handler's declared
    // set — guards against forged form posts.
    let declared: std::collections::HashSet<&'static str> = handler
        .bulk_actions()
        .iter()
        .chain(handler.row_actions().iter())
        .map(|a| a.name)
        .collect();
    if !declared.contains(action.as_str()) {
        return Err(AdminError::Validation(format!(
            "library type `{type_name}` doesn't declare bulk action `{action}`"
        )));
    }
    let touched = handler
        .handle_bulk_action(tenant.pool(), &action, &ids)
        .await?;
    let count = touched.to_string();
    redirect_named_with_message_plural(
        "rcms-admin:library:list",
        MsgLevel::Success,
        "{action}: touched {count} row(s) in `{type_name}`.",
        touched as i64,
        &[
            ("action", action.as_str()),
            ("count", count.as_str()),
            ("type_name", type_name.as_str()),
        ],
        &headers,
    )
}

/// Render one cell for a [`ListColumn`] / row pair into pre-escaped
/// HTML. Handles type-aware formatting: booleans → ✓/✗, datetimes →
/// 10-char prefix, FK-ish text → escape, JSON lookups → kind-driven
/// formatting, method fields → handler-supplied HTML (trusted —
/// caller is responsible for escaping).
fn render_list_cell(
    col: &crate::library::ListColumn,
    row: &crate::snippet::Snippet,
    handler: Option<&dyn crate::library::LibraryTypeHandler>,
) -> String {
    use crate::library::{ListColumn, ListColumnKind};
    match col {
        ListColumn::Field { column, .. } => match *column {
            "id" => row.id.get().copied().unwrap_or_default().to_string(),
            "type_name" => html_escape(&row.type_name),
            "slug" => format!("<code>{}</code>", html_escape(&row.slug)),
            "folder_path" => html_escape(&row.folder_path),
            "title" => format!("<strong>{}</strong>", html_escape(&row.title)),
            "body_markdown" => {
                let preview: String = row.body_markdown.chars().take(80).collect();
                format!("<span>{}</span>", html_escape(&preview))
            }
            "created_at" => row
                .created_at
                .get()
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_default(),
            "updated_at" => row
                .updated_at
                .get()
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_default(),
            other => format!("<em>(unknown field: {})</em>", html_escape(other)),
        },
        ListColumn::JsonField { key, kind, .. } => {
            let val = row.data.get(*key);
            match (kind, val) {
                (ListColumnKind::Bool, Some(serde_json::Value::Bool(b))) => bool_checkbox_html(*b),
                (_, Some(serde_json::Value::Bool(b))) => bool_checkbox_html(*b),
                (ListColumnKind::Integer, Some(serde_json::Value::Number(n)))
                | (ListColumnKind::Float, Some(serde_json::Value::Number(n))) => {
                    html_escape(&n.to_string())
                }
                (_, Some(serde_json::Value::String(s))) => html_escape(s),
                (_, Some(other)) => html_escape(&other.to_string()),
                (_, None) => String::new(),
            }
        }
        ListColumn::Method { name, .. } | ListColumn::Computed { name, .. } => handler
            .map(|h| h.render_cell(name, row))
            .unwrap_or_default(),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn bool_checkbox_html(b: bool) -> String {
    if b {
        r#"<span class="material-symbols-rounded sm filled" style="color: var(--accent);" aria-label="true">check_box</span>"#.to_owned()
    } else {
        r#"<span class="material-symbols-rounded sm" style="color: var(--muted-2);" aria-label="false">check_box_outline_blank</span>"#.to_owned()
    }
}

/// GET /cms-admin/library/new?type=<type_name> — render the create
/// form for a new snippet. The type pre-selects from the query string
/// so the "+ New <Type>" CTA on the list page lands here.
#[derive(Debug, Deserialize)]
pub struct SnippetNewQuery {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub folder: Option<String>,
}

pub async fn snippet_new_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Query(q): Query<SnippetNewQuery>,
) -> Result<Response, AdminError> {
    let types: Vec<crate::library::LibraryType> = crate::library::LibraryType::objects()
        .order_by(&[("type_name", false)])
        .fetch(tenant.pool())
        .await?;
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "library", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("mode", "new");
    ctx.insert("types", &types);
    ctx.insert("preselect_type", &q.r#type.unwrap_or_default());
    let folder = q.folder.unwrap_or_default();
    ctx.insert(
        "snippet",
        &serde_json::json!({
            "type_name": "",
            "slug": "",
            "title": "",
            "body_markdown": "",
            "folder_path": folder,
        }),
    );
    render_with_csrf(&state, &headers, "rcms_admin/snippet_form.html", &mut ctx)
}

pub async fn snippet_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Query(q): Query<EditQuery>,
) -> Result<Response, AdminError> {
    let snippet = crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // FB-04 (#537) — forms are edited in the dedicated visual builder, not
    // the generic snippet form. The list's Edit link lands here; bounce to
    // the builder so the entry point "just works".
    if snippet.type_name == "form" {
        let url = rustango::urls::reverse_owned(
            "rcms-admin:forms:build",
            &std::collections::HashMap::from([("id".to_owned(), id.to_string())]),
        )
        .map_err(|e| AdminError::Validation(format!("url reversal failed: {e}")))?;
        return Ok(axum::response::Redirect::to(&url).into_response());
    }
    // Permission gate (#21 phase 3) — `can_view = false` denies
    // the edit page entirely (404 rather than 403 to avoid leaking
    // existence). `can_edit = false` lets the page render read-only
    // — we surface it via a `read_only` context var.
    let handler = crate::library::find_handler(&snippet.type_name);
    if let Some(h) = handler.as_ref() {
        if !h.can_view(&snippet) {
            return Err(AdminError::NotFound(id));
        }
    }
    let read_only = handler
        .as_ref()
        .map(|h| !h.can_edit(&snippet))
        .unwrap_or(false);
    let can_delete = handler
        .as_ref()
        .map(|h| h.can_delete(&snippet))
        .unwrap_or(true);
    // Optional custom edit template — falls back to the default
    // `snippet_form.html` when not overridden.
    let template_path = handler
        .as_ref()
        .and_then(|h| h.edit_template())
        .unwrap_or("rcms_admin/snippet_form.html");

    let types: Vec<crate::library::LibraryType> = crate::library::LibraryType::objects()
        .order_by(&[("type_name", false)])
        .fetch(tenant.pool())
        .await?;
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "library", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("mode", "edit");
    ctx.insert("types", &types);
    ctx.insert("snippet", &snippet);
    ctx.insert("read_only", &read_only);
    ctx.insert("can_delete", &can_delete);
    // Types that let each element name its own template get the field;
    // for every other type the concept doesn't exist and the form
    // shouldn't imply it does.
    ctx.insert(
        "allows_element_template",
        &handler
            .as_ref()
            .map(|h| h.allows_element_template())
            .unwrap_or(false),
    );
    // #123 — surface revision history + preview opt-in flags so the
    // edit template can render a History card + Preview link only
    // for types that opted in.
    let revisions_enabled = handler.as_ref().is_some_and(|h| h.revisions_enabled());
    let preview_enabled = handler.as_ref().is_some_and(|h| h.preview_enabled());
    let revisions: Vec<crate::snippet::SnippetRevision> = if revisions_enabled {
        crate::snippet::revisions_for(tenant.pool(), id)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    ctx.insert("revisions_enabled", &revisions_enabled);
    ctx.insert("preview_enabled", &preview_enabled);
    ctx.insert("snippet_revisions", &revisions);
    // #409 — snippet translation mode. `?locale=<code>` (a non-default
    // active locale) shows a per-field translation editor beside the
    // canonical content, mirroring the page translate flow.
    let locales: Vec<crate::locale::Locale> = crate::locale::Locale::objects()
        .where_(crate::locale::Locale::active.eq(true))
        .order_by(&[("sort_order", false), ("code", false)])
        .fetch(tenant.pool())
        .await?;
    let editing_locale = q
        .locale
        .as_deref()
        .filter(|c| !c.is_empty())
        .and_then(|code| locales.iter().find(|l| l.code == code).cloned());
    let translation_mode = editing_locale.as_ref().is_some_and(|l| !l.is_default);
    let mut core_translations: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut extra_translations: Vec<(String, String)> = Vec::new();
    if translation_mode {
        let lid = editing_locale
            .as_ref()
            .and_then(|l| l.id.get().copied())
            .unwrap_or_default();
        let existing = crate::snippet_translation::fetch_for(tenant.pool(), id, lid)
            .await
            .unwrap_or_default();
        let core_keys = ["title", "body_markdown"];
        core_translations = core_keys
            .iter()
            .map(|k| {
                (
                    (*k).to_owned(),
                    existing.get(*k).cloned().unwrap_or_default(),
                )
            })
            .collect();
        extra_translations = existing
            .iter()
            .filter(|(k, _)| !core_keys.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
    }
    ctx.insert("locales", &locales);
    ctx.insert("editing_locale", &editing_locale);
    ctx.insert("translation_mode", &translation_mode);
    ctx.insert("core_translations", &core_translations);
    ctx.insert("extra_translations", &extra_translations);
    render_with_csrf(&state, &headers, template_path, &mut ctx)
}

/// POST `/cms-admin/library/{id}/translate?locale=<code>` (#409) —
/// upsert per-field snippet translations for a non-default locale.
/// Mirrors [`page_translate_submit`]: `tr__<field>` keys edit existing
/// fields, `new_field_path` + `new_field_value` adds one; an empty
/// value deletes the row (canonical shows through). Atomic batch.
pub async fn snippet_translate_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Path(id): Path<i64>,
    Query(q): Query<EditQuery>,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    use crate::snippet_translation::SnippetTranslation;
    let code = q
        .locale
        .as_deref()
        .filter(|c| !c.is_empty())
        .ok_or_else(|| AdminError::Validation("translate requires ?locale=".to_owned()))?;
    let locale = crate::locale::Locale::objects()
        .where_(crate::locale::Locale::code.eq(code.to_owned()))
        .where_(crate::locale::Locale::active.eq(true))
        .first(tenant.pool())
        .await?
        .ok_or_else(|| AdminError::Validation(format!("unknown locale `{code}`")))?;
    if locale.is_default {
        return Ok((
            axum::http::StatusCode::BAD_REQUEST,
            "cannot translate INTO the default locale; canonical content lives on cms_snippet",
        )
            .into_response());
    }
    let locale_id = locale
        .id
        .get()
        .copied()
        .ok_or_else(|| AdminError::Validation("locale row missing id".to_owned()))?;
    // Verify the snippet exists.
    let snippet = crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;

    // Collect `tr__<field>` edits + an optional new-field row.
    let mut updates: Vec<(String, String)> = Vec::new();
    for (k, v) in &form {
        if let Some(path) = k.strip_prefix("tr__") {
            if !path.is_empty() {
                updates.push((path.to_owned(), v.clone()));
            }
        }
    }
    let new_path_raw = form.get("new_field_path").map(String::as_str).unwrap_or("");
    if !new_path_raw.trim().is_empty() {
        // Reuse the model's validator; silently skip a malformed path
        // rather than 400 the whole batch.
        if let Ok(path) = crate::snippet_translation::validate_field_path(new_path_raw) {
            let value = form.get("new_field_value").cloned().unwrap_or_default();
            updates.push((path, value));
        }
    }

    // Empty value → delete; non-empty → upsert. Atomic batch.
    rustango::atomic!(tenant.pool(), |tx| {
        let mut guard = tx.lock().await?;
        let tx = &mut *guard;
        for (path, value) in updates {
            let existing: Vec<SnippetTranslation> = SnippetTranslation::objects()
                .where_(SnippetTranslation::snippet_id.eq(id))
                .where_(SnippetTranslation::locale_id.eq(locale_id))
                .where_(SnippetTranslation::field_path.eq(path.clone()))
                .fetch_tx(tx)
                .await?;
            if value.is_empty() {
                for row in existing {
                    row.delete_tx(tx).await?;
                }
            } else if let Some(mut row) = existing.into_iter().next() {
                row.value = value;
                row.save_tx(tx).await?;
            } else {
                let mut row = SnippetTranslation {
                    id: rustango::sql::Auto::Unset,
                    snippet_id: id,
                    locale_id,
                    field_path: path,
                    value,
                    updated_at: rustango::sql::Auto::Unset,
                };
                row.save_tx(tx).await?;
            }
        }
        Ok::<_, rustango::sql::ExecError>(())
    })
    .await?;

    // A form is translated on its own screen; its library edit page
    // forwards to the builder and would drop the language.
    let back = if snippet.type_name == "form" {
        format!("/cms-admin/forms/{id}/build?locale={code}")
    } else {
        format!("/cms-admin/library/{id}/edit?locale={code}")
    };
    Ok(Redirect::to(&back).into_response())
}

/// POST /cms-admin/library/snippets/{id}/revert/{seq} — restore a
/// snippet to the snapshot stored in the named revision. Captures a
/// fresh revision after the revert so the timeline stays linear.
pub async fn snippet_revert_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path((id, sequence)): Path<(i64, i32)>,
) -> Result<Response, AdminError> {
    let Some(rev) = crate::snippet::revision_by_sequence(tenant.pool(), id, sequence).await? else {
        return Err(AdminError::NotFound(id));
    };
    let mut snippet = crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // Re-hydrate from the snapshot — title / body / slug / data /
    // folder_path / type_name. Skip primary key + audit timestamps.
    let snap = &rev.snapshot;
    if let Some(v) = snap.get("title").and_then(|v| v.as_str()) {
        snippet.title = v.to_owned();
    }
    if let Some(v) = snap.get("slug").and_then(|v| v.as_str()) {
        snippet.slug = v.to_owned();
    }
    if let Some(v) = snap.get("body_markdown").and_then(|v| v.as_str()) {
        snippet.body_markdown = v.to_owned();
    }
    if let Some(v) = snap.get("folder_path").and_then(|v| v.as_str()) {
        snippet.folder_path = v.to_owned();
    }
    if let Some(v) = snap.get("type_name").and_then(|v| v.as_str()) {
        snippet.type_name = v.to_owned();
    }
    if let Some(v) = snap.get("data") {
        snippet.data = v.clone();
    }
    snippet.save_pool(tenant.pool()).await?;
    // Capture the revert itself so the timeline shows it.
    if let Some(handler) = crate::library::find_handler(&snippet.type_name) {
        if handler.revisions_enabled() {
            let captured_by = session_user.as_ref().and_then(|u| u.id.get().copied());
            crate::snippet::capture_revision(tenant.pool(), &snippet, captured_by)
                .await
                .log_warn("revision not captured; the history has a gap");
        }
    }
    let sequence_str = sequence.to_string();
    redirect_named_with_params_and_message_args(
        "rcms-admin:library:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Reverted to revision #{sequence}.",
        &[("sequence", sequence_str.as_str())],
        &headers,
    )
}

pub async fn snippet_create_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let type_name = form.get("type_name").cloned().unwrap_or_default();
    let slug = form.get("slug").cloned().unwrap_or_default();
    let title = form.get("title").cloned().unwrap_or_default();
    let body = form.get("body_markdown").cloned().unwrap_or_default();
    let folder_path = form
        .get("folder_path")
        .map(|s| crate::snippet::normalize_folder(s))
        .unwrap_or_default();
    if type_name.is_empty() || slug.is_empty() || title.is_empty() {
        return Err(AdminError::Validation(
            "type, slug, and title are all required".to_owned(),
        ));
    }
    let mut row = crate::snippet::Snippet {
        id: rustango::sql::Auto::Unset,
        type_name,
        slug,
        folder_path,
        title,
        body_markdown: body,
        data: serde_json::Value::Object(serde_json::Map::new()),
        created_at: rustango::sql::Auto::Unset,
        updated_at: rustango::sql::Auto::Unset,
    };
    row.save_pool(tenant.pool()).await?;
    // A new form is empty until its fields are built — go straight to the
    // Form Builder rather than back to the Library list.
    if row.type_name == "form" {
        let id = row.id.get().copied().unwrap_or_default();
        return redirect_named_with_params_and_message(
            "rcms-admin:forms:build",
            &[("id", id.to_string())],
            MsgLevel::Success,
            &super::i18n::tr(
                &headers,
                "Created snippet “{title}”.",
                &[("title", row.title.as_str())],
            ),
            &headers,
        );
    }
    redirect_named_with_message_args(
        "rcms-admin:library:list",
        MsgLevel::Success,
        "Created snippet “{title}”.",
        &[("title", row.title.as_str())],
        &headers,
    )
}

pub async fn snippet_edit_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let mut snippet = crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // Permission gate (#21 phase 3) — handler-declared can_edit
    // hook short-circuits the save. V1 default is `true`; hosts
    // override to wire role-aware logic.
    if let Some(handler) = crate::library::find_handler(&snippet.type_name) {
        if !handler.can_edit(&snippet) {
            return Err(AdminError::Validation(format!(
                "Library type `{}` denies edit on this row.",
                snippet.type_name
            )));
        }
        // Cross-field validation hook — surfaces field-scoped error
        // strings before any mutation lands.
        if let Err(errors) = handler.validate(&form) {
            let summary: Vec<String> = errors
                .into_iter()
                .map(|(field, msg)| format!("{field}: {msg}"))
                .collect();
            return Err(AdminError::Validation(summary.join("; ")));
        }
    }
    if let Some(v) = form.get("type_name") {
        if !v.is_empty() {
            snippet.type_name = v.clone();
        }
    }
    if let Some(v) = form.get("slug") {
        if !v.is_empty() {
            snippet.slug = v.clone();
        }
    }
    if let Some(v) = form.get("title") {
        snippet.title = v.clone();
    }
    if let Some(v) = form.get("body_markdown") {
        snippet.body_markdown = v.clone();
    }
    if let Some(v) = form.get("folder_path") {
        snippet.folder_path = crate::snippet::normalize_folder(v);
    }
    // Per-element template name, for types that opted in via
    // `allows_element_template()`. Only that one `data` key is read
    // here: the generic form deliberately does not own `data` (typed
    // extras belong to a type's own `edit_template`), so a type that
    // never opted in cannot have its render repointed from this form.
    if crate::library::find_handler(&snippet.type_name)
        .is_some_and(|h| h.allows_element_template())
    {
        if let Some(v) = form.get("data__template") {
            crate::snippet::set_element_template(&mut snippet.data, v.trim());
        }
    }
    snippet.save_pool(tenant.pool()).await?;
    // #123 — capture a versioned snapshot when the handler opts in
    // via `revisions_enabled()`. Best-effort: capture failure logs
    // + does NOT abort the save.
    if let Some(handler) = crate::library::find_handler(&snippet.type_name) {
        if handler.revisions_enabled() {
            let captured_by = session_user.as_ref().and_then(|u| u.id.get().copied());
            if let Err(e) =
                crate::snippet::capture_revision(tenant.pool(), &snippet, captured_by).await
            {
                tracing::warn!(
                    target: "rustango_cms::admin",
                    snippet_id = id,
                    type_name = %snippet.type_name,
                    error = %e,
                    "snippet revision capture failed",
                );
            }
        }
    }
    // #146 — reindex references from the saved snippet form.
    {
        let mut refs: Vec<crate::reference_index::Reference> = Vec::new();
        for (key, value) in &form {
            let mut found = crate::reference_index::scan(value, key);
            refs.append(&mut found);
        }
        if let Err(e) = crate::reference_index::reindex_source(
            tenant.pool(),
            crate::reference_index::KIND_SNIPPET,
            id,
            refs,
        )
        .await
        {
            tracing::warn!(
                target: "rustango_cms::reference_index",
                snippet_id = id, error = %e,
                "snippet reindex failed (best-effort)"
            );
        }
    }
    redirect_named_with_message_args(
        "rcms-admin:library:list",
        MsgLevel::Success,
        "Saved snippet “{title}”.",
        &[("title", snippet.title.as_str())],
        &headers,
    )
}

pub async fn snippet_delete_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let snippet = crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // Permission gate (#21 phase 3) — handler can deny delete.
    if let Some(handler) = crate::library::find_handler(&snippet.type_name) {
        if !handler.can_delete(&snippet) {
            return Err(AdminError::Validation(format!(
                "Library type `{}` denies delete on this row.",
                snippet.type_name
            )));
        }
    }
    let title = snippet.title.clone();
    let is_form = snippet.type_name == "form";
    snippet.delete_pool(tenant.pool()).await?;
    // Forms are deleted from the Forms screen — go back there.
    if is_form {
        let body = super::i18n::tr(&headers, "Deleted snippet “{title}”.", &[("title", title.as_str())]);
        return Ok(rustango::messages::redirect_with_message(
            messages_secret(),
            &headers,
            MsgLevel::Success,
            &body,
            "/cms-admin/forms",
        ));
    }
    redirect_named_with_message_args(
        "rcms-admin:library:list",
        MsgLevel::Success,
        "Deleted snippet “{title}”.",
        &[("title", title.as_str())],
        &headers,
    )
}

// =====================================================================
// Media + Documents tabs
// =====================================================================

/// Parse a `YYYY-MM-DD` filter into the UTC instant at start-of-day.
/// Empty / unparseable → None.
fn parse_date_floor(raw: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .ok()
        .map(|d| {
            chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                d.and_hms_opt(0, 0, 0).unwrap_or_default(),
                chrono::Utc,
            )
        })
}

/// Parse a `YYYY-MM-DD` filter into the UTC instant at end-of-day
/// (23:59:59) so the inclusive ceiling matches editor intent.
fn parse_date_ceil(raw: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .ok()
        .map(|d| {
            chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                d.and_hms_opt(23, 59, 59).unwrap_or_default(),
                chrono::Utc,
            )
        })
}

/// Query string for the media + documents list views. All filters
/// optional; URL state persists so editors can bookmark filtered
/// views (#7).
#[derive(Debug, Deserialize)]
pub struct MediaListQuery {
    /// 1-based page number. Out-of-range values clamp to the last page.
    #[serde(default)]
    pub page: Option<i64>,
    /// Rows per page; clamped to `admin::pagination::MAX_PER_PAGE`.
    #[serde(default)]
    pub per_page: Option<usize>,
    /// Filter to a specific collection. Omitted = all collections.
    #[serde(default)]
    pub collection: Option<i64>,
    /// Case-insensitive substring search across title, filename,
    /// alt_text.
    #[serde(default)]
    pub q: Option<String>,
    /// Date range floor — `YYYY-MM-DD`. Matches `uploaded_at >= from`.
    #[serde(default)]
    pub from: Option<String>,
    /// Date range ceiling — `YYYY-MM-DD`. Matches `uploaded_at <= to`
    /// (end of day in UTC).
    #[serde(default)]
    pub to: Option<String>,
}

// =====================================================================
// Image hard crop (#24) — apply / clone-and-crop
// =====================================================================

/// Form payload for both crop handlers. Coords are in source-image
/// pixel space; `mode` distinguishes apply vs clone-and-crop.
#[derive(Debug, Deserialize)]
pub struct CropForm {
    pub crop_x: u32,
    pub crop_y: u32,
    pub crop_w: u32,
    pub crop_h: u32,
    /// Either "apply" (destructive — overwrites source) or "clone"
    /// (creates a new media row from the cropped bytes).
    pub mode: String,
}

/// POST /cms-admin/media/{id}/crop — apply the crop rect to the
/// source bytes. Two modes:
/// - `mode=apply` — replaces the row's bytes in-place, bumps
///   content_hash, recomputes width/height, purges every rendition
///   for the old hash. Destructive — the original bytes are gone.
/// - `mode=clone` — creates a new media row whose bytes are the
///   cropped output. Original row is untouched. New row inherits
///   title (" — crop" suffix) + alt_text + collection_id; focal
///   point clears so the editor re-marks on the crop.
pub async fn media_crop_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<CropForm>,
) -> Result<Response, AdminError> {
    use rustango::sql::Auto;
    let mut media = Media::objects()
        .where_(Media::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    if media.kind != "image" {
        return Err(AdminError::Validation(
            "Crop applies to images only.".to_owned(),
        ));
    }

    let bytes = crate::media_storage::disk_for(&tenant.org.slug)
        .load(&crate::media_storage::key(
            &tenant.org.slug,
            &media.storage_key,
        ))
        .await
        .map_err(|e| AdminError::Validation(format!("read source: {e}")))?;
    let (cropped_bytes, out_mime) = crate::rendition::crop_image_off_runtime(
        bytes,
        media.mime.clone(),
        (form.crop_x, form.crop_y, form.crop_w, form.crop_h),
    )
    .await
    .map_err(AdminError::Validation)?;

    let new_hash = {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(&cropped_bytes);
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for b in digest.iter() {
            hex.push_str(&format!("{b:02x}"));
        }
        hex
    };

    match form.mode.as_str() {
        "apply" => {
            // In-place: overwrite the same storage_key. The backend
            // writes the object wholesale (atomic PUT on S3), which
            // replaces the old tmp-file + rename dance.
            crate::media_storage::disk_for(&tenant.org.slug)
                .save(
                    &crate::media_storage::key(&tenant.org.slug, &media.storage_key),
                    &cropped_bytes,
                )
                .await
                .map_err(|e| AdminError::Validation(format!("write crop: {e}")))?;
            let old_hash = media.content_hash.clone();
            media.content_hash = new_hash;
            media.size = cropped_bytes.len() as i64;
            media.mime = out_mime;
            media.width = Some(form.crop_w as i32);
            media.height = Some(form.crop_h as i32);
            // Focal point in original coords no longer maps; clear.
            media.focal_point_x = None;
            media.focal_point_y = None;
            media.save_pool(tenant.pool()).await?;

            // Purge renditions for the OLD content_hash (the route
            // looks them up by hash + filter spec; bytes on disk
            // also need cleaning).
            for r in crate::rendition::MediaRendition::objects()
                .where_(crate::rendition::MediaRendition::content_hash.eq(old_hash))
                .fetch(tenant.pool())
                .await?
            {
                crate::media_storage::disk_for(&tenant.org.slug)
                    .delete(&crate::media_storage::key(&tenant.org.slug, &r.storage_key))
                    .await
                    .log_warn("stored file not deleted; the blob is orphaned");
                r.delete_pool(tenant.pool()).await?;
            }
            let crop_w = form.crop_w.to_string();
            let crop_h = form.crop_h.to_string();
            redirect_named_with_params_and_message_args(
                "rcms-admin:media:edit",
                &[("id", id.to_string())],
                MsgLevel::Success,
                "Crop applied. New dimensions {width}×{height}; renditions purged.",
                &[("width", crop_w.as_str()), ("height", crop_h.as_str())],
                &headers,
            )
        }
        "clone" => {
            // Save as new. Fresh storage_key under the source's
            // content-hash prefix so the on-disk bytes are
            // discoverable. Suffix the title with " — crop" so the
            // new row reads distinctly in lists.
            let ext = match out_mime.as_str() {
                "image/jpeg" => "jpg",
                "image/webp" => "webp",
                _ => "png",
            };
            let new_key = format!("{}-crop-{}.{ext}", &new_hash[..16.min(new_hash.len())], id);
            crate::media_storage::disk_for(&tenant.org.slug)
                .save(
                    &crate::media_storage::key(&tenant.org.slug, &new_key),
                    &cropped_bytes,
                )
                .await
                .map_err(|e| AdminError::Validation(format!("write clone: {e}")))?;

            let clone_title = if media.title.contains(" — crop") {
                media.title.clone()
            } else {
                format!("{} — crop", media.title)
            };
            let mut clone = Media {
                id: Auto::Unset,
                filename: format!(
                    "{}.crop.{ext}",
                    std::path::Path::new(&media.filename)
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "crop".to_owned())
                ),
                content_hash: new_hash,
                mime: out_mime,
                size: cropped_bytes.len() as i64,
                kind: "image".to_owned(),
                width: Some(form.crop_w as i32),
                height: Some(form.crop_h as i32),
                storage_key: new_key,
                title: clone_title,
                alt_text: media.alt_text.clone(),
                description: media.description.clone(),
                uploaded_by: media.uploaded_by,
                collection_id: media.collection_id,
                focal_point_x: None,
                focal_point_y: None,
                uploaded_at: Auto::Unset,
            };
            clone.insert_pool(tenant.pool()).await?;
            let new_id = clone.id.get().copied().unwrap_or_default();
            let new_id_str = new_id.to_string();
            let crop_w = form.crop_w.to_string();
            let crop_h = form.crop_h.to_string();
            redirect_named_with_params_and_message_args(
                "rcms-admin:media:edit",
                &[("id", new_id.to_string())],
                MsgLevel::Success,
                "Saved cropped clone as new media #{id} ({width}×{height}).",
                &[
                    ("id", new_id_str.as_str()),
                    ("width", crop_w.as_str()),
                    ("height", crop_h.as_str()),
                ],
                &headers,
            )
        }
        other => Err(AdminError::Validation(format!(
            "Unknown crop mode `{other}` — expected `apply` or `clone`."
        ))),
    }
}

// =====================================================================
// Image replace (#190)
// =====================================================================

/// POST /cms-admin/media/{id}/replace — re-upload bytes for an existing
/// media row. Keeps the row id (so every page that references it stays
/// linked) and the editorial metadata (title, alt_text). Updates the
/// physical fields: storage_key, content_hash, mime, size, width,
/// height, filename. Focal point survives unless the new dimensions
/// require it to be clamped — it's stored as a 0..1 fraction so it
/// already scales across resizes.
///
/// Old on-disk bytes are best-effort deleted; every cached rendition
/// for the old content_hash is purged so subsequent requests
/// regenerate from the replacement.
pub async fn media_replace_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    mut multipart: axum::extract::Multipart,
) -> Result<Response, AdminError> {
    use sha2::{Digest, Sha256};

    let mut media = Media::objects()
        .where_(Media::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;

    let mut filename = String::new();
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AdminError::Upload(format!("multipart parse: {e}")))?
    {
        if field.name().unwrap_or("") == "file" {
            if let Some(fname) = field.file_name() {
                filename = sanitize_filename(fname);
            }
            bytes = field
                .bytes()
                .await
                .map_err(|e| AdminError::Upload(format!("read bytes: {e}")))?
                .to_vec();
        } else {
            let _ = field.bytes().await;
        }
    }
    if filename.is_empty() || bytes.is_empty() {
        return Err(AdminError::Upload("no file submitted".into()));
    }

    let new_mime = mime_guess::from_path(&filename)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();
    refuse_active_upload(&new_mime)?;
    // Replacing an image with a non-image (or vice versa) would
    // silently break every consumer that branches on `kind`.
    let new_kind = Media::classify_mime(&new_mime).to_owned();
    if new_kind != media.kind {
        return Err(AdminError::Validation(format!(
            "Replacement kind `{new_kind}` does not match existing `{}` — replace must keep the media type.",
            media.kind
        )));
    }

    // EXIF strip + decode for dimensions, same as the upload path.
    let (stored, decoded_dims) =
        crate::rendition::strip_exif_off_runtime(bytes, new_mime.clone()).await;
    bytes = stored;

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let new_hash = digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });

    let new_storage_key = format!("{}-{}", &new_hash[..16.min(new_hash.len())], filename);
    crate::media_storage::disk_for(&tenant.org.slug)
        .save(
            &crate::media_storage::key(&tenant.org.slug, &new_storage_key),
            &bytes,
        )
        .await
        .map_err(|e| AdminError::Upload(format!("write: {e}")))?;

    let old_storage_key = media.storage_key.clone();
    let old_hash = media.content_hash.clone();

    let (new_w, new_h): (Option<i32>, Option<i32>) = decoded_dims
        .map(|(w, h)| (Some(w as i32), Some(h as i32)))
        .unwrap_or((None, None));

    media.filename = filename;
    media.storage_key = new_storage_key;
    media.content_hash = new_hash;
    media.mime = new_mime;
    media.size = bytes.len() as i64;
    media.width = new_w;
    media.height = new_h;
    media.save_pool(tenant.pool()).await?;

    // Best-effort cleanup of the old file on disk. Only remove when
    // the storage_key actually changed — content-addressed naming
    // means an identical replacement reuses the same path.
    if old_storage_key != media.storage_key {
        crate::media_storage::disk_for(&tenant.org.slug)
            .delete(&crate::media_storage::key(
                &tenant.org.slug,
                &old_storage_key,
            ))
            .await
            .log_warn("stored file not deleted; the blob is orphaned");
    }

    // Purge renditions for the old content_hash so the next request
    // regenerates from the replacement bytes.
    if old_hash != media.content_hash {
        for r in crate::rendition::MediaRendition::objects()
            .where_(crate::rendition::MediaRendition::content_hash.eq(old_hash))
            .fetch(tenant.pool())
            .await?
        {
            crate::media_storage::disk_for(&tenant.org.slug)
                .delete(&crate::media_storage::key(&tenant.org.slug, &r.storage_key))
                .await
                .log_warn("stored file not deleted; the blob is orphaned");
            r.delete_pool(tenant.pool()).await?;
        }
    }

    redirect_named_with_params_and_message(
        "rcms-admin:media:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Replaced. Old renditions purged; every page referencing this image now serves the new bytes.",
        &headers,
    )
}

// =====================================================================
// Image focal point (#4)
// =====================================================================

/// GET /cms-admin/media/{id}/edit — image detail page with the
/// focal-point picker. Image-only; other kinds return 404.
/// Locate every page that references `media_id` (#100). Returns
/// `(page_id, page_title, url_path, status)` rows. Scan reuses the
/// `harvest_media_ids` helper that powers `media_unused_report`,
/// inverted so we collect per-page hits instead of building a
/// tenant-wide used-set.
async fn pages_referencing_media(
    pool: &rustango::sql::Pool,
    media_id: i64,
) -> Result<Vec<(i64, String, String, String)>, rustango::sql::ExecError> {
    let pages: Vec<Page> = Page::objects().fetch(pool).await?;
    let mut hits: Vec<(i64, String, String, String)> = Vec::new();
    for p in &pages {
        let page_id = p.id.get().copied().unwrap_or_default();
        if page_id == 0 {
            continue;
        }
        let mut ids = std::collections::BTreeSet::new();
        // Walk the canonical page JSON first (defensive).
        if let Ok(v) = serde_json::to_value(p) {
            harvest_media_ids(&v, &mut ids);
        }
        // Walk the page-type extension JSON (hero_media_id +
        // stream-block image refs).
        if let Ok(pts) = PageType::objects()
            .where_(PageType::id.eq(p.page_type_id))
            .fetch(pool)
            .await
        {
            if let Some(pt_row) = pts.into_iter().next() {
                if let Some(handler) = crate::page_type::find_handler(&pt_row.type_name) {
                    if let Ok(ext) = handler.load_extension(pool, page_id).await {
                        harvest_media_ids(&ext, &mut ids);
                    }
                }
            }
        }
        if ids.contains(&media_id) {
            hits.push((
                page_id,
                p.title.clone(),
                p.url_path.clone(),
                p.status.clone(),
            ));
        }
    }
    Ok(hits)
}

pub async fn media_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let media = Media::objects()
        .where_(Media::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    if media.kind != "image" {
        return Err(AdminError::NotFound(id));
    }
    // #100 — per-image usage report.
    let usages = pages_referencing_media(tenant.pool(), id)
        .await
        .unwrap_or_default();
    let usage_rows: Vec<serde_json::Value> = usages
        .iter()
        .map(|(pid, title, url_path, status)| {
            serde_json::json!({
                "id": pid,
                "title": title,
                "url_path": url_path,
                "status": status,
            })
        })
        .collect();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "media", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert(
        "media",
        &serde_json::json!({
            "id": media.id.get().copied(),
            "title": media.title,
            "filename": media.filename,
            "alt_text": media.alt_text,
            "description": media.description,
            "mime": media.mime,
            "size": media.size,
            "width": media.width,
            "height": media.height,
            "focal_point_x": media.focal_point_x,
            "focal_point_y": media.focal_point_y,
            "uploaded_at": media.uploaded_at.get().copied(),
            // Cache-buster for the `rcms_image_url(..., v=media.content_hash)`
            // call in the template — without this the focal-point
            // preview never picks up an in-place re-crop.
            "content_hash": media.content_hash,
        }),
    );
    ctx.insert("usages", &usage_rows);
    ctx.insert("usage_count", &(usage_rows.len() as i64));
    render_with_csrf(&state, &headers, "rcms_admin/media_edit.html", &mut ctx)
}

/// Form payload for the focal-point editor (#4). All four fields
/// arrive as strings from the HTML form; we parse + validate
/// server-side. A submission with empty x/y clears the focal point.
#[derive(Debug, Deserialize)]
pub struct MediaEditForm {
    pub title: String,
    pub alt_text: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub focal_point_x: Option<String>,
    #[serde(default)]
    pub focal_point_y: Option<String>,
}

/// POST /cms-admin/media/{id}/edit — save title / alt + focal point.
/// When the focal point changes, every cached rendition for this
/// media row is purged so subsequent requests regenerate around the
/// new center.
pub async fn media_edit_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<MediaEditForm>,
) -> Result<Response, AdminError> {
    let mut media = Media::objects()
        .where_(Media::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    if media.kind != "image" {
        return Err(AdminError::NotFound(id));
    }
    media.title = form.title.trim().to_owned();
    media.alt_text = form.alt_text.trim().to_owned();
    media.description = form.description.trim().to_owned();

    let parse_fraction = |raw: Option<&str>| -> Option<f32> {
        raw.map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<f32>().ok())
            .map(|f| f.clamp(0.0, 1.0))
    };
    let new_x = parse_fraction(form.focal_point_x.as_deref());
    let new_y = parse_fraction(form.focal_point_y.as_deref());
    let focal_changed = media.focal_point_x != new_x || media.focal_point_y != new_y;
    media.focal_point_x = new_x;
    media.focal_point_y = new_y;
    media.save_pool(tenant.pool()).await?;

    // Invalidate rendition cache rows + delete their on-disk bytes
    // so subsequent requests regenerate with the new focal point.
    if focal_changed {
        let renditions: Vec<crate::rendition::MediaRendition> =
            crate::rendition::MediaRendition::objects()
                .where_(
                    crate::rendition::MediaRendition::content_hash.eq(media.content_hash.clone()),
                )
                .fetch(tenant.pool())
                .await?;
        for r in renditions {
            // Best-effort delete of the on-disk bytes; the DB row
            // delete is what counts (next render miss regenerates).
            crate::media_storage::disk_for(&tenant.org.slug)
                .delete(&crate::media_storage::key(&tenant.org.slug, &r.storage_key))
                .await
                .log_warn("stored file not deleted; the blob is orphaned");
            r.delete_pool(tenant.pool()).await?;
        }
    }
    redirect_named_with_params_and_message(
        "rcms-admin:media:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        if focal_changed {
            "Saved. Renditions purged — next request regenerates with the new focal point."
        } else {
            "Saved."
        },
        &headers,
    )
}

/// Recursively scan a JSON value for any string that contains
/// `needle` (already lowercased). Used by #204 admin search to
/// broaden recall into page-type extension fields without listing
/// every key on the handler trait.
fn json_contains_needle(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::String(s) => {
            if s.to_lowercase().contains(needle) {
                return true;
            }
            // Stream-block extension columns sometimes hold a JSON
            // array as a string — peek through if it looks like one.
            let trimmed = s.trim_start();
            if trimmed.starts_with('[') || trimmed.starts_with('{') {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                    return json_contains_needle(&parsed, needle);
                }
            }
            false
        }
        serde_json::Value::Array(arr) => arr.iter().any(|v| json_contains_needle(v, needle)),
        serde_json::Value::Object(map) => map.values().any(|v| json_contains_needle(v, needle)),
        _ => false,
    }
}

/// Recursively walk `value` (the JSON shape of a page's extension
/// row, plus any nested stream-block JSON we find as string values)
/// and collect every `media_id` reference. Catches:
///
/// - Direct `media_id` / `hero_media_id` / `*_media_id` fields on
///   the extension table (any object key matching `media_id` or
///   ending in `_media_id`).
/// - Stream-block image references: extension rows store the
///   `body_stream` column as a JSON string; we detect string values
///   that look like JSON and recurse into the parsed array.
/// - IDs stored as numbers OR strings — HTML forms submit text, so
///   stream-block `media_id` values land as strings in the JSON.
fn harvest_media_ids(value: &serde_json::Value, out: &mut std::collections::BTreeSet<i64>) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let is_media_key = key == "media_id" || key.ends_with("_media_id");
                if is_media_key {
                    match child {
                        Value::Number(n) => {
                            if let Some(id) = n.as_i64() {
                                if id > 0 {
                                    out.insert(id);
                                }
                            }
                        }
                        Value::String(s) => {
                            if let Ok(id) = s.parse::<i64>() {
                                if id > 0 {
                                    out.insert(id);
                                }
                            }
                        }
                        _ => harvest_media_ids(child, out),
                    }
                } else {
                    harvest_media_ids(child, out);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                harvest_media_ids(item, out);
            }
        }
        Value::String(s) => {
            let trimmed = s.trim_start();
            if trimmed.starts_with('[') || trimmed.starts_with('{') {
                if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                    harvest_media_ids(&parsed, out);
                }
            }
        }
        _ => {}
    }
}

/// GET /cms-admin/media/unused — list `cms_media` rows that no page
/// references (#8). Builds a reverse index by walking every page's
/// extension JSON for `media_id` keys, then diffs against the full
/// media table. Used by editors to garbage-collect stale uploads.
pub async fn media_unused_report(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let all_media: Vec<Media> = Media::objects()
        .order_by(&[("uploaded_at", true)])
        .fetch(tenant.pool())
        .await?;
    let pages: Vec<Page> = Page::objects().fetch(tenant.pool()).await?;

    let mut used: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();

    // 1) Page rows themselves carry no media_id columns today, but
    //    we walk the JSON-serialized page anyway for defense-in-depth
    //    — a future column would auto-surface.
    for p in &pages {
        if let Ok(v) = serde_json::to_value(p) {
            harvest_media_ids(&v, &mut used);
        }
    }

    // 2) Extension rows: every PageType handler loads its own
    //    extension row. The default impl returns Value::Null for
    //    unit-struct types; ours returns the row as JSON (including
    //    body_stream as a string we recurse into).
    for p in &pages {
        let page_id = p.id.get().copied().unwrap_or_default();
        // Resolve the PageType row → type_name → handler.
        // page_type lookup is cheap; tenants have a handful of types.
        let pt: Vec<crate::page_type_model::PageType> = crate::page_type_model::PageType::objects()
            .where_(crate::page_type_model::PageType::id.eq(p.page_type_id))
            .fetch(tenant.pool())
            .await?;
        let Some(pt_row) = pt.into_iter().next() else {
            continue;
        };
        let Some(handler) = crate::page_type::find_handler(&pt_row.type_name) else {
            continue;
        };
        if let Ok(ext_value) = handler.load_extension(tenant.pool(), page_id).await {
            harvest_media_ids(&ext_value, &mut used);
        }
    }

    // 3) Diff: media rows whose ID isn't in the used set are unused.
    let unused: Vec<&Media> = all_media
        .iter()
        .filter(|m| m.id.get().copied().is_some_and(|id| !used.contains(&id)))
        .collect();

    let unused_view: Vec<serde_json::Value> = unused
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id.get().copied(),
                "title": m.title,
                "filename": m.filename,
                "kind": m.kind,
                "mime": m.mime,
                "size": m.size,
                "uploaded_at": m.uploaded_at.get().copied(),
                "uploaded_by": m.uploaded_by,
            })
        })
        .collect();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "media-unused", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("unused", &unused_view);
    ctx.insert("total_media", &all_media.len());
    ctx.insert("used_count", &used.len());
    render_with_csrf(&state, &headers, "rcms_admin/media_unused.html", &mut ctx)
}

/// POST /cms-admin/media/unused/delete — bulk-delete the chosen
/// media rows. Receives a list of `id=N` form fields and deletes
/// each in sequence. The handler does NOT re-verify "unused" status
/// — that's the editor's job — but it stays well-behaved because
/// the form template only renders rows that ARE currently unused.
pub async fn media_unused_bulk_delete(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, AdminError> {
    use rustango::sql::Auto;
    let ids: Vec<i64> = form
        .iter()
        .filter(|(k, _)| k == "id" || k == "ids")
        .filter_map(|(_, v)| v.parse::<i64>().ok())
        .collect();
    let mut deleted = 0usize;
    for id in ids {
        let row = Media::objects()
            .where_(Media::id.eq(id))
            .first(tenant.pool())
            .await?;
        if let Some(m) = row {
            // Use Model::delete_pool via the row instance.
            // The framework's Auto wrapper holds the id; deletion
            // through the row resolves it cleanly.
            let _ = Auto::Unset::<i64>; // silence unused-import lint
            m.delete_pool(tenant.pool()).await?;
            deleted += 1;
        }
    }
    redirect_named_with_message_plural(
        "rcms-admin:media:unused",
        MsgLevel::Success,
        "Deleted {count} unused media item(s).",
        deleted as i64,
        &[("count", &deleted.to_string())],
        &headers,
    )
}

/// GET /cms-admin/media — list `cms_media` rows where `kind = 'image'`.
pub async fn media_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Query(q): Query<MediaListQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Response, AdminError> {
    // #142 — opportunistic scratch-upload sweep. Roughly 2% of
    // media-list visits trigger a stale-row GC so dead scratch uploads
    // don't accumulate. Best-effort: file unlink errors log + continue.
    if rand::random::<u8>() < 5 {
        if let Ok(unlinked) = crate::uploaded_file::gc_stale(tenant.pool()).await {
            for key in unlinked {
                crate::media_storage::disk_for(&tenant.org.slug)
                    .delete(&crate::media_storage::key(&tenant.org.slug, &key))
                    .await
                    .log_warn("stored file not deleted; the blob is orphaned");
            }
        }
    }
    render_media_list(
        &state,
        &tenant,
        &headers,
        session_user.as_ref(),
        "image",
        q,
        raw_query,
    )
    .await
}

/// GET /cms-admin/documents — same view filtered to non-image kinds.
pub async fn documents_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Query(q): Query<MediaListQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Response, AdminError> {
    render_media_list(
        &state,
        &tenant,
        &headers,
        session_user.as_ref(),
        "document",
        q,
        raw_query,
    )
    .await
}

/// Shared list renderer for the image / document views. `kind` is
/// the discriminator on `cms_media.kind` ("image" / "document").
async fn render_media_list(
    state: &super::AdminState,
    tenant: &Tenant,
    headers: &HeaderMap,
    session_user: Option<&rustango::tenancy::auth::User>,
    kind: &str,
    filters: MediaListQuery,
    raw_query: Option<String>,
) -> Result<Response, AdminError> {
    let collection_filter = filters.collection;
    let search = filters
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);
    let from_filter = parse_date_floor(filters.from.as_deref());
    let to_filter = parse_date_ceil(filters.to.as_deref());
    // Fetch every media row + every collection once. Collection
    // count is small (a handful per tenant); media count is bounded
    // by what fits comfortably in the admin list. Walk in Rust.
    let all: Vec<Media> = Media::objects()
        .order_by(&[("uploaded_at", true)])
        .fetch(tenant.pool())
        .await?;
    let collections: Vec<crate::media::MediaCollection> = crate::media::MediaCollection::objects()
        .order_by(&[("sort_order", false), ("name", false)])
        .fetch(tenant.pool())
        .await?;

    let items: Vec<&Media> = all
        .iter()
        .filter(|m| {
            let kind_match = if kind == "image" {
                m.kind == "image"
            } else {
                m.kind != "image"
            };
            let collection_match = match collection_filter {
                None => true,
                Some(cid) => m.collection_id == Some(cid),
            };
            let search_match = match &search {
                None => true,
                Some(needle) => {
                    m.title.to_lowercase().contains(needle)
                        || m.filename.to_lowercase().contains(needle)
                        || m.alt_text.to_lowercase().contains(needle)
                }
            };
            let date_match = m.uploaded_at.get().is_none_or(|when| {
                let after_from = from_filter.is_none_or(|floor| *when >= floor);
                let before_to = to_filter.is_none_or(|ceil| *when <= ceil);
                after_from && before_to
            });
            kind_match && collection_match && search_match && date_match
        })
        .collect();

    // Paginate the filtered set, so a large library renders one page of
    // cards and thumbnails. The fetch above is still unbounded: filtering
    // happens in Rust so the case-insensitive search behaves the same on
    // SQLite, Postgres and MySQL (bare LIKE does not) — see #632.
    let per_page = crate::admin::pagination::clamp_per_page(filters.per_page);
    let requested_page = filters.page.unwrap_or(1);
    let total_items = items.len();
    let pagination = crate::admin::pagination::context(requested_page, per_page, total_items);
    let current_page = pagination["page"].as_i64().unwrap_or(1);
    let skip = crate::admin::pagination::offset(current_page, per_page) as usize;
    let items: Vec<&Media> = items.into_iter().skip(skip).take(per_page).collect();

    // Count per collection so the sidebar shows "(N)" beside each
    // name. We count for the CURRENT kind only so the badges line
    // up with the filtered view the editor is looking at.
    let mut counts: std::collections::HashMap<Option<i64>, i64> = std::collections::HashMap::new();
    for m in &all {
        let in_kind = if kind == "image" {
            m.kind == "image"
        } else {
            m.kind != "image"
        };
        if in_kind {
            *counts.entry(m.collection_id).or_insert(0) += 1;
        }
    }

    let collections_view: Vec<serde_json::Value> = collections
        .iter()
        .map(|c| {
            let id = c.id.get().copied();
            serde_json::json!({
                "id": id,
                "name": c.name,
                "parent_id": c.parent_id,
                "count": id.and_then(|i| counts.get(&Some(i)).copied()).unwrap_or(0),
            })
        })
        .collect();
    let uncategorized_count = counts.get(&None).copied().unwrap_or(0);

    let mut ctx = Context::new();
    let active_tab = if kind == "image" {
        "media"
    } else {
        "documents"
    };
    add_chrome(&mut ctx, tenant, active_tab, session_user).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("items", &items);
    ctx.insert("collections", &collections_view);
    ctx.insert("collection_filter", &collection_filter);
    ctx.insert("uncategorized_count", &uncategorized_count);
    ctx.insert("search_q", &filters.q);
    ctx.insert("date_from", &filters.from);
    ctx.insert("date_to", &filters.to);
    ctx.insert(
        "is_filtered",
        &(collection_filter.is_some()
            || search.is_some()
            || from_filter.is_some()
            || to_filter.is_some()),
    );
    // The full match count, not the size of the visible page.
    ctx.insert("filtered_count", &total_items);
    ctx.insert("pagination", &pagination);
    ctx.insert(
        "kind_label",
        if kind == "image" {
            "Media (images)"
        } else {
            "Documents"
        },
    );
    ctx.insert(
        "upload_kind",
        if kind == "image" { "image" } else { "document" },
    );
    ctx.insert(
        "self_url",
        if kind == "image" {
            "/cms-admin/media"
        } else {
            "/cms-admin/documents"
        },
    );
    // #316 — raw query string for the `| querystring(collection=…)` rail
    // links: they preserve the q / from / to filters and just set or
    // clear `collection` (media has no page param). `qs_none` is a null
    // sentinel — Tera has no `null` literal, so `querystring(k=qs_none)`
    // is how the template drops a key.
    ctx.insert("query_string", &raw_query.unwrap_or_default());
    ctx.insert("qs_none", &serde_json::Value::Null);
    render_with_csrf(state, headers, "rcms_admin/media_list.html", &mut ctx)
}

/// GET /cms-admin/media/upload — upload form.
pub async fn media_upload_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Query(q): Query<UploadFormQuery>,
) -> Result<Response, AdminError> {
    let kind_label = if q.kind.as_deref() == Some("document") {
        "Documents"
    } else {
        "Media (images)"
    };
    let mut ctx = Context::new();
    add_chrome(
        &mut ctx,
        &tenant,
        if q.kind.as_deref() == Some("document") {
            "documents"
        } else {
            "media"
        },
        session_user.as_ref(),
    )
    .await;
    // Collections picker (#188) — pass into ctx so each queue row
    // can render a per-file dropdown without a follow-up fetch.
    let collections: Vec<crate::media::MediaCollection> = crate::media::MediaCollection::objects()
        .order_by(&[("sort_order", false), ("name", false)])
        .fetch(tenant.pool())
        .await?;
    let collections_view: Vec<serde_json::Value> = collections
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id.get().copied(),
                "name": c.name,
            })
        })
        .collect();
    ctx.insert("collections", &collections_view);
    ctx.insert("kind_label", kind_label);
    ctx.insert("upload_kind", &q.kind.as_deref().unwrap_or("image"));
    render_with_csrf(&state, &headers, "rcms_admin/media_upload.html", &mut ctx)
}

#[derive(Debug, Deserialize)]
pub struct UploadFormQuery {
    #[serde(default)]
    pub kind: Option<String>,
}

/// POST /cms-admin/media/upload — accept a multipart upload, hash
/// the bytes, drop them through the tenant `Storage` backend, and
/// stamp a `cms_media` row.
/// POST /cms-admin/media/upload-staged — write bytes to scratch
/// storage + return JSON `{id, filename, mime, size}` (#142, Wagtail
/// parity D9). Returned `id` is the [`crate::uploaded_file::UploadedFile`]
/// row to commit / cancel later.
///
/// Coexists with the existing single-step `media_upload_submit` — that
/// path keeps working unchanged. Use this endpoint for future
/// drag-drop / multi-file UIs where bytes need to land before
/// metadata is collected.
pub async fn media_upload_staged(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    mut multipart: axum::extract::Multipart,
) -> Result<Response, AdminError> {
    use sha2::{Digest, Sha256};
    let Some(viewer) = session_user.as_ref() else {
        // #285 — XHR/fetch clients (the multi-file upload UI) used to
        // get the raw HTML page injected into a toast on session
        // expiry. Pass the request headers so the responder returns
        // JSON when the client sent `Accept: application/json` or
        // `X-Requested-With: XMLHttpRequest`.
        return Ok(unauthorized_no_session_for(Some(&headers)));
    };
    let viewer_id = viewer.id.get().copied().unwrap_or_default();
    let mut filename = String::new();
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AdminError::Upload(format!("multipart parse: {e}")))?
    {
        if field.name().unwrap_or("") == "file" {
            if let Some(fname) = field.file_name() {
                filename = sanitize_filename(fname);
            }
            bytes = field
                .bytes()
                .await
                .map_err(|e| AdminError::Upload(format!("read bytes: {e}")))?
                .to_vec();
        } else {
            let _ = field.bytes().await;
        }
    }
    if filename.is_empty() || bytes.is_empty() {
        return Err(AdminError::Upload("no file submitted".into()));
    }
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let hash = digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    let mime = mime_guess::from_path(&filename)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();
    refuse_active_upload(&mime)?;
    // #284 — dedup by content hash. If this tenant already has a
    // cms_media row whose `content_hash` matches the bytes the
    // editor just uploaded, skip writing to scratch + creating a
    // new UploadedFile row. The client treats the response as a
    // "skipped" outcome and surfaces it as a toast linking back to
    // the existing media row. Per-tenant query — the
    // `tenant.pool()` is the tenant-scoped pool, so a hash that
    // collides across tenants doesn't dedup across the boundary.
    {
        use rustango::core::Column as _;
        use rustango::sql::FetcherPool as _;
        let existing: Vec<crate::media::Media> = crate::media::Media::objects()
            .where_(crate::media::Media::content_hash.eq(hash.clone()))
            .fetch(tenant.pool())
            .await
            .unwrap_or_default();
        if let Some(existing_media) = existing.into_iter().next() {
            let existing_id = existing_media.id.get().copied().unwrap_or_default();
            let payload = serde_json::json!({
                "id": null,
                "duplicate_of": existing_id,
                "duplicate_title": existing_media.title,
                "duplicate_filename": existing_media.filename,
                "filename": filename,
                "mime": existing_media.mime,
                "size": existing_media.size,
            });
            return Ok((
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned()),
            )
                .into_response());
        }
    }
    let storage_key = format!("scratch/{}-{filename}", &hash[..16]);
    crate::media_storage::disk_for(&tenant.org.slug)
        .save(
            &crate::media_storage::key(&tenant.org.slug, &storage_key),
            &bytes,
        )
        .await
        .map_err(|e| AdminError::Upload(format!("write: {e}")))?;
    let size = bytes.len() as i64;
    let row = crate::uploaded_file::create(
        tenant.pool(),
        viewer_id,
        storage_key,
        filename,
        mime,
        size,
        hash,
    )
    .await?;
    let payload = serde_json::json!({
        "id": row.id.get().copied().unwrap_or_default(),
        "filename": row.original_filename,
        "mime": row.mime,
        "size": row.size,
    });
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned()),
    )
        .into_response())
}

/// One entry in the [`StagedCommitPayload`] commit grid.
#[derive(Debug, Deserialize)]
pub struct StagedCommitItem {
    pub uploaded_file_id: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub alt_text: String,
    #[serde(default)]
    pub collection_id: Option<i64>,
}

/// Body of `POST /cms-admin/media/upload-staged/commit` (#188). The
/// editor uploaded N files into scratch storage; now this payload
/// names each scratch row + its editorial metadata in one
/// transaction-equivalent call.
#[derive(Debug, Deserialize)]
pub struct StagedCommitPayload {
    pub items: Vec<StagedCommitItem>,
}

/// POST /cms-admin/media/upload-staged/commit — promote staged rows
/// to `cms_media` with per-file metadata (#188). Each item:
///
/// 1. Loads the scratch row.
/// 2. Re-reads bytes from the scratch path; EXIF-strips images.
/// 3. Moves the file out of `scratch/` to the canonical
///    `<hash>-<filename>` storage_key.
/// 4. Inserts a `cms_media` row.
/// 5. Deletes the scratch DB row + (best-effort) the scratch file.
///
/// On a per-item failure, that scratch row is left in place so the
/// editor can retry; successful items still commit. The response is
/// JSON `{ committed: [media_id...], failed: [{id, error}...] }`.
pub async fn media_upload_staged_commit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Json(payload): Json<StagedCommitPayload>,
) -> Result<Response, AdminError> {
    use rustango::sql::Auto;
    let Some(viewer) = session_user.as_ref() else {
        // #285 — JSON-aware 401 for fetch clients.
        return Ok(unauthorized_no_session_for(Some(&headers)));
    };
    let viewer_id = viewer.id.get().copied().unwrap_or_default();
    let media_slug = tenant.org.slug.clone();

    let mut committed: Vec<i64> = Vec::new();
    let mut failed: Vec<serde_json::Value> = Vec::new();

    for item in payload.items {
        let result: Result<i64, String> = async {
            let row = crate::uploaded_file::get(tenant.pool(), item.uploaded_file_id)
                .await
                .map_err(|e| format!("lookup: {e}"))?
                .ok_or_else(|| "scratch row missing".to_owned())?;
            if row.user_id != viewer_id && !viewer.is_superuser {
                return Err("not your upload".to_owned());
            }

            let scratch_key = crate::media_storage::key(&media_slug, &row.storage_key);
            let mut bytes = crate::media_storage::disk_for(&media_slug)
                .load(&scratch_key)
                .await
                .map_err(|e| format!("read scratch: {e}"))?;

            // EXIF-strip + decode for pixel dims. Mirrors upload path.
            let (stored, decoded_dims) =
                crate::rendition::strip_exif_off_runtime(bytes, row.mime.clone()).await;
            bytes = stored;

            // Canonical storage_key under the tenant's media dir —
            // content-addressed prefix so renditions can find bytes
            // by hash without hitting the DB.
            let final_key = format!(
                "{}-{}",
                &row.content_hash[..16.min(row.content_hash.len())],
                row.original_filename
            );
            crate::media_storage::disk_for(&media_slug)
                .save(&crate::media_storage::key(&media_slug, &final_key), &bytes)
                .await
                .map_err(|e| format!("write final: {e}"))?;

            let kind = Media::classify_mime(&row.mime).to_owned();
            let (width, height): (Option<i32>, Option<i32>) = decoded_dims
                .map(|(w, h)| (Some(w as i32), Some(h as i32)))
                .unwrap_or((None, None));

            let title = if item.title.trim().is_empty() {
                row.original_filename
                    .rsplit_once('.')
                    .map(|(stem, _)| stem.to_owned())
                    .unwrap_or_else(|| row.original_filename.clone())
            } else {
                item.title.trim().to_owned()
            };

            let mut media_row = Media {
                id: Auto::Unset,
                filename: row.original_filename.clone(),
                content_hash: row.content_hash.clone(),
                mime: row.mime.clone(),
                size: bytes.len() as i64,
                kind,
                width,
                height,
                storage_key: final_key,
                title,
                alt_text: item.alt_text.trim().to_owned(),
                description: String::new(),
                uploaded_by: Some(viewer_id),
                collection_id: item.collection_id,
                focal_point_x: None,
                focal_point_y: None,
                uploaded_at: Auto::Unset,
            };
            media_row
                .insert_pool(tenant.pool())
                .await
                .map_err(|e| format!("insert media: {e}"))?;
            let new_id = media_row.id.get().copied().unwrap_or_default();

            // Best-effort scratch cleanup. Scratch file is a separate
            // path from the final storage_key (final lives outside
            // `scratch/`), so removing it is safe even when the bytes
            // are byte-identical.
            crate::media_storage::disk_for(&media_slug)
                .delete(&scratch_key)
                .await
                .log_warn("stored file not deleted; the blob is orphaned");
            crate::uploaded_file::delete(tenant.pool(), item.uploaded_file_id)
                .await
                .map_err(|e| format!("delete scratch: {e}"))?;
            Ok(new_id)
        }
        .await;

        match result {
            Ok(id) => committed.push(id),
            Err(e) => {
                failed.push(serde_json::json!({
                    "uploaded_file_id": item.uploaded_file_id,
                    "error": e,
                }));
            }
        }
    }

    let body = serde_json::json!({
        "committed": committed,
        "failed": failed,
    });
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_owned()),
    )
        .into_response())
}

/// POST /cms-admin/media/upload-staged/{id}/cancel — drop scratch row
/// + unlink the file. Returns 204.
pub async fn media_upload_staged_cancel(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let Some(viewer) = session_user.as_ref() else {
        // #285 — JSON-aware 401 for fetch clients.
        return Ok(unauthorized_no_session_for(Some(&headers)));
    };
    let viewer_id = viewer.id.get().copied().unwrap_or_default();
    let Some(row) = crate::uploaded_file::get(tenant.pool(), id).await? else {
        return Ok((axum::http::StatusCode::NOT_FOUND, "not found").into_response());
    };
    if row.user_id != viewer_id && !viewer.is_superuser {
        return Ok((axum::http::StatusCode::FORBIDDEN, "not your upload").into_response());
    }
    crate::media_storage::disk_for(&tenant.org.slug)
        .delete(&crate::media_storage::key(
            &tenant.org.slug,
            &row.storage_key,
        ))
        .await
        .log_warn("stored file not deleted; the blob is orphaned");
    crate::uploaded_file::delete(tenant.pool(), id).await?;
    Ok((axum::http::StatusCode::NO_CONTENT, "").into_response())
}

/// Refuse a file a browser would run as a page (#724): HTML, XHTML,
/// JavaScript and XML (which can carry script through XSLT or an XHTML
/// namespace). Serving neutralises every type already; this keeps such
/// files out of the library in the first place. SVG stays allowed — it is
/// served under a no-script CSP.
fn refuse_active_upload(mime: &str) -> Result<(), AdminError> {
    let active = matches!(
        mime,
        "text/html"
            | "application/xhtml+xml"
            | "text/javascript"
            | "application/javascript"
            | "application/ecmascript"
            | "text/xml"
            | "application/xml"
            | "text/xsl"
            | "application/xslt+xml"
    );
    if active {
        return Err(AdminError::Upload(format!(
            "files of type {mime} can't be uploaded: a browser would run them as a page"
        )));
    }
    Ok(())
}

/// Store one media file: sanitize the name, detect + classify the mime,
/// strip EXIF from images, hash, write
/// storage key `<org_slug>/<hash16>-<name>`, and insert the `cms_media`
/// row. The single canonical write path — the browser upload handlers and
/// the MCP `upload_media` tool all funnel through here.
///
/// `dedupe = true` short-circuits on an existing row with the same
/// content hash (returns it instead of storing a byte-identical copy) —
/// the same behavior the staged-upload flow gives the browser.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn store_media(
    pool: &rustango::sql::Pool,
    org_slug: &str,
    mut bytes: Vec<u8>,
    raw_filename: &str,
    title: Option<&str>,
    alt_text: &str,
    collection_id: Option<i64>,
    uploaded_by: Option<i64>,
    dedupe: bool,
) -> Result<Media, AdminError> {
    use rustango::sql::Auto;
    use sha2::{Digest, Sha256};

    let filename = sanitize_filename(raw_filename);
    if filename.is_empty() || bytes.is_empty() {
        return Err(AdminError::Upload("no file submitted".into()));
    }

    let mime = mime_guess::from_path(&filename)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();
    refuse_active_upload(&mime)?;
    let kind = Media::classify_mime(&mime).to_owned();

    // #184 — strip EXIF on image upload, BEFORE hashing so the hash
    // reflects the stored bytes. See `media_upload_submit` for the why.
    let (stored, decoded_dims) = crate::rendition::strip_exif_off_runtime(bytes, mime.clone()).await;
    bytes = stored;

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let hash = digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });

    if dedupe {
        if let Some(existing) = Media::objects()
            .where_(Media::content_hash.eq(hash.clone()))
            .first(pool)
            .await?
        {
            return Ok(existing);
        }
    }

    // Routed through the pluggable storage backend (Slice 8.x).
    let storage_key = format!("{}-{}", &hash[..16], filename);
    crate::media_storage::disk_for(&org_slug)
        .save(&crate::media_storage::key(org_slug, &storage_key), &bytes)
        .await
        .map_err(|e| AdminError::Upload(format!("write: {e}")))?;

    let title = match title {
        Some(t) if !t.is_empty() => t.to_owned(),
        _ => filename
            .rsplit_once('.')
            .map(|(stem, _)| stem.to_owned())
            .unwrap_or_else(|| filename.clone()),
    };

    // Pixel dimensions fall out of the EXIF-strip decode; non-images /
    // broken decodes get `None`.
    let (width, height): (Option<i32>, Option<i32>) = decoded_dims
        .map(|(w, h)| (Some(w as i32), Some(h as i32)))
        .unwrap_or((None, None));

    let mut row = Media {
        id: Auto::Unset,
        filename,
        content_hash: hash,
        mime,
        size: bytes.len() as i64,
        kind,
        width,
        height,
        storage_key,
        title,
        alt_text: alt_text.to_owned(),
        description: String::new(),
        uploaded_by,
        collection_id,
        focal_point_x: None,
        focal_point_y: None,
        uploaded_at: Auto::Unset,
    };
    row.insert_pool(pool).await?;
    Ok(row)
}

pub async fn media_upload_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    mut multipart: axum::extract::Multipart,
) -> Result<Response, AdminError> {
    let mut filename = String::new();
    let mut bytes: Vec<u8> = Vec::new();
    let mut title = String::new();
    let mut alt_text = String::new();
    // #24 V2 — upload-time crop. When the client submits all four
    // crop coords (source-image pixel space), we decode + crop the
    // bytes before storing so the on-disk canonical file matches the
    // editor's intent. Absence of any of the four = no crop.
    let mut crop_x: Option<u32> = None;
    let mut crop_y: Option<u32> = None;
    let mut crop_w: Option<u32> = None;
    let mut crop_h: Option<u32> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AdminError::Upload(format!("multipart parse: {e}")))?
    {
        let name = field.name().unwrap_or("").to_owned();
        match name.as_str() {
            "file" => {
                if let Some(fname) = field.file_name() {
                    filename = sanitize_filename(fname);
                }
                bytes = field
                    .bytes()
                    .await
                    .map_err(|e| AdminError::Upload(format!("read bytes: {e}")))?
                    .to_vec();
            }
            "title" => {
                title = field
                    .text()
                    .await
                    .map_err(|e| AdminError::Upload(format!("read title: {e}")))?;
            }
            "alt_text" => {
                alt_text = field
                    .text()
                    .await
                    .map_err(|e| AdminError::Upload(format!("read alt_text: {e}")))?;
            }
            "crop_x" | "crop_y" | "crop_w" | "crop_h" => {
                let value = field
                    .text()
                    .await
                    .map_err(|e| AdminError::Upload(format!("read {name}: {e}")))?;
                let parsed: Option<u32> = value.trim().parse().ok();
                match name.as_str() {
                    "crop_x" => crop_x = parsed,
                    "crop_y" => crop_y = parsed,
                    "crop_w" => crop_w = parsed,
                    "crop_h" => crop_h = parsed,
                    _ => {}
                }
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    if filename.is_empty() || bytes.is_empty() {
        return Err(AdminError::Upload("no file submitted".into()));
    }

    // Apply crop if all four coords arrived. Image-only — vector
    // sources can't be raster-cropped (mirrors the edit-time rule
    // in the V1 crop handler).
    if let (Some(x), Some(y), Some(w), Some(h)) = (crop_x, crop_y, crop_w, crop_h) {
        if w > 0 && h > 0 {
            let detected_mime = mime_guess::from_path(&filename)
                .first_or_octet_stream()
                .essence_str()
                .to_owned();
            refuse_active_upload(&detected_mime)?;
            if detected_mime == "image/svg+xml" {
                return Err(AdminError::Validation(
                    "Cropping SVG sources isn't supported; upload uncropped, then edit the markup."
                        .to_owned(),
                ));
            }
            if detected_mime.starts_with("image/") {
                let (cropped, _out_mime) = crate::rendition::crop_image_off_runtime(
                    bytes,
                    detected_mime.clone(),
                    (x, y, w, h),
                )
                .await
                .map_err(AdminError::Validation)?;
                bytes = cropped;
            }
        }
    }

    // This legacy single-file path ignores any collection choice (the
    // per-file picker lives on the staged upload flow): rows land in
    // "uncategorized" (NULL) under the root collection.
    let row = store_media(
        tenant.pool(),
        &tenant.org.slug,
        bytes,
        &filename,
        (!title.is_empty()).then_some(title.as_str()),
        &alt_text,
        None,
        None,
        false,
    )
    .await?;

    let redirect = if row.kind == "image" {
        "/cms-admin/media"
    } else {
        "/cms-admin/documents"
    };
    Ok(Redirect::to(redirect).into_response())
}

/// POST /cms-admin/media/{id}/delete — drop a media row.
pub async fn media_delete_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let row = Media::objects()
        .where_(Media::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // Best-effort storage cleanup — a missing object is not fatal.
    crate::media_storage::disk_for(&tenant.org.slug)
        .delete(&crate::media_storage::key(
            &tenant.org.slug,
            &row.storage_key,
        ))
        .await
        .log_warn("stored file not deleted; the blob is orphaned");
    let redirect = if row.kind == "image" {
        "/cms-admin/media"
    } else {
        "/cms-admin/documents"
    };
    row.delete_pool(tenant.pool()).await?;
    Ok(Redirect::to(redirect).into_response())
}

fn sanitize_filename(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// =====================================================================
// Locales tab
// =====================================================================

/// GET /cms-admin/locales — list every locale.
/// GET /cms-admin/model/{slug} — #420. Generic read-only index for a
/// model registered via `register_model_admin!`: columns come from the
/// model's `admin(list_display = …)`, rows from its type-erased
/// list-rows thunk, all inside the CMS admin chrome. 404s an unknown
/// slug.
pub async fn model_admin_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(slug): Path<String>,
) -> Result<Response, AdminError> {
    let Some(ma) = super::model_admin::find(&slug) else {
        return Ok((axum::http::StatusCode::NOT_FOUND, "unknown model admin").into_response());
    };
    let rows_json = (ma.list_rows)(tenant.pool().clone()).await;
    let columns = super::model_admin::list_columns(ma.schema);
    let header_labels: Vec<&str> = columns.iter().map(|(_, l)| l.as_str()).collect();
    let body_rows: Vec<Vec<String>> = rows_json
        .iter()
        .map(|r| {
            columns
                .iter()
                .map(|(name, _)| super::model_admin::row_cell(r, name))
                .collect()
        })
        .collect();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "model-admin", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("model_label", ma.label);
    ctx.insert("model_slug", &slug);
    ctx.insert("columns", &header_labels);
    ctx.insert("rows", &body_rows);
    ctx.insert("total", &body_rows.len());
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/model_admin_list.html",
        &mut ctx,
    )
}

/// #439 — `?format=csv` selector for [`report_view`].
#[derive(serde::Deserialize, Default)]
pub struct ReportQuery {
    pub format: Option<String>,
}

/// #439 — render any registered report through the shared chrome'd
/// table, or export it as CSV with `?format=csv`. `active_tab` is
/// `report:<slug>` so `_base.html` highlights the matching sidebar link.
pub async fn report_view(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(slug): Path<String>,
    Query(q): Query<ReportQuery>,
) -> Result<Response, AdminError> {
    let Some(report) = super::report::find_report(&slug) else {
        return Ok((axum::http::StatusCode::NOT_FOUND, "unknown report").into_response());
    };
    let user_id = session_user.as_ref().and_then(|u| u.id.get().copied());
    let columns = report.columns();
    let rows = report.rows(tenant.pool(), user_id).await;

    // CSV export — same rows, flattened to column order.
    if q.format.as_deref() == Some("csv") {
        let mut out = String::with_capacity(128 + rows.len() * 64);
        let header: Vec<String> = columns.iter().map(|c| csv_escape(c.label)).collect();
        out.push_str(&header.join(","));
        out.push('\n');
        for row in &rows {
            let line: Vec<String> = columns
                .iter()
                .map(|c| csv_escape(&report_cell_string(row.get(c.key))))
                .collect();
            out.push_str(&line.join(","));
            out.push('\n');
        }
        return Ok(csv_response(&slug, &out));
    }

    let header_labels: Vec<&str> = columns.iter().map(|c| c.label).collect();
    let view_rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let cells: Vec<serde_json::Value> = columns
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "kind": c.kind.as_str(),
                        "value": report_cell_string(row.get(c.key)),
                    })
                })
                .collect();
            serde_json::json!({
                "edit_url": row.get("edit_url").and_then(|v| v.as_str()),
                "cells": cells,
            })
        })
        .collect();

    let mut ctx = Context::new();
    add_chrome(
        &mut ctx,
        &tenant,
        &format!("report:{slug}"),
        session_user.as_ref(),
    )
    .await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("report_title", report.title());
    ctx.insert("report_slug", &slug);
    ctx.insert("report_description", &report.description());
    ctx.insert("columns", &header_labels);
    ctx.insert("rows", &view_rows);
    ctx.insert("total", &view_rows.len());
    render_with_csrf(&state, &headers, "rcms_admin/report.html", &mut ctx)
}

// ===================================================================== Form builder (#533)

/// GET `/cms-admin/forms` — dedicated Forms list: every form snippet with its
/// per-form submission count and last-submission time, linking straight into
/// the builder and the submission history. This exists because the generic
/// library table can't surface submissions (`LibraryTypeHandler::render_cell`
/// is sync and pool-less), which left "Forms" dead-ending on a Library view
/// with no way to see what a form collected.
pub async fn forms_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let pool = tenant.pool();
    let rows = forms_list_rows(pool).await?;

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "forms", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, pool).await;
    ctx.insert("rows", &rows);
    ctx.insert("total", &rows.len());
    render_with_csrf(&state, &headers, "rcms_admin/forms_list.html", &mut ctx)
}

/// Core of [`forms_list`] — the row model (per-form field count, submission
/// count, latest submission, action URLs), split out so unit tests can drive
/// it with an in-memory pool.
pub(crate) async fn forms_list_rows(
    pool: &rustango::sql::Pool,
) -> Result<Vec<serde_json::Value>, AdminError> {
    let mut forms: Vec<crate::snippet::Snippet> = crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::type_name.eq("form".to_owned()))
        .fetch(pool)
        .await?;
    // Newest-edited first.
    forms.sort_by(|a, b| {
        b.updated_at
            .get()
            .cloned()
            .cmp(&a.updated_at.get().cloned())
    });

    // Per-form submission count + latest submission, grouped in Rust — the
    // tri-dialect pattern used by `counts_by_type` / the pages `child_counts`
    // (no SQL GROUP BY helper in the codebase).
    let entries: Vec<crate::forms::submit::FormEntry> = crate::forms::submit::FormEntry::objects()
        .fetch(pool)
        .await?;
    let mut counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let mut last_at: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    for e in &entries {
        *counts.entry(e.form_snippet_id).or_default() += 1;
        if let Some(at) = e.submitted_at.get() {
            let s = at.format("%Y-%m-%d %H:%M").to_string();
            let slot = last_at.entry(e.form_snippet_id).or_default();
            if s > *slot {
                *slot = s;
            }
        }
    }

    let rows: Vec<serde_json::Value> = forms
        .iter()
        .map(|s| {
            let id = s.id.get().copied().unwrap_or_default();
            let parsed = crate::forms::schema::parse(&s.data).unwrap_or_default();
            serde_json::json!({
                "id": id,
                "title": s.title,
                "slug": s.slug,
                "fields": parsed.field_count(),
                "submissions": counts.get(&id).copied().unwrap_or(0),
                "last_submission": last_at.get(&id).cloned().unwrap_or_default(),
                "updated_at": s.updated_at.get().map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
                "build_url": format!("/cms-admin/forms/{id}/build"),
                "submissions_url": format!("/cms-admin/forms/{id}/submissions"),
                "delete_url": format!("/cms-admin/library/{id}/delete"),
            })
        })
        .collect();
    Ok(rows)
}

/// GET `/cms-admin/forms/{id}/build` — the visual form builder editor
/// (FB-04 / #537). Loads the form snippet's schema JSON into the page; the
/// `form_builder.js` engine renders the editor from it.
pub async fn form_build_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Query(q): Query<EditQuery>,
) -> Result<Response, AdminError> {
    let snippet = crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    if snippet.type_name != "form" {
        return Err(AdminError::NotFound(id));
    }
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "forms", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("title", &snippet.title);
    ctx.insert("form_id", &id);
    ctx.insert("build_url", &format!("/cms-admin/forms/{id}/build"));

    // #550 FB-17 — active non-default locales for the Translate picker.
    let locales: Vec<crate::locale::Locale> = crate::locale::Locale::objects()
        .where_(crate::locale::Locale::active.eq(true))
        .where_(crate::locale::Locale::is_default.eq(false))
        .fetch(tenant.pool())
        .await
        .unwrap_or_default();
    ctx.insert(
        "locales",
        &locales
            .iter()
            .map(|l| serde_json::json!({"code": l.code, "name": l.name}))
            .collect::<Vec<_>>(),
    );

    // Translate mode: ?locale=<code> renders a focused per-leaf editor that
    // posts to the existing snippet-translate endpoint.
    if let Some(code) = q.locale.as_deref().filter(|c| !c.is_empty()) {
        if let Some(loc) = locales.iter().find(|l| l.code == code) {
            let lid = loc.id.get().copied().unwrap_or(0);
            let form = crate::forms::schema::parse(&snippet.data).unwrap_or_default();
            let leaves = crate::forms::schema::translatable_leaves(&form);
            let existing = crate::snippet_translation::fetch_for(tenant.pool(), id, lid)
                .await
                .unwrap_or_default();
            // Consecutive leaves of one field form a group, like the
            // block groups of the page editor.
            let mut groups: Vec<serde_json::Value> = Vec::new();
            for l in &leaves {
                let row = serde_json::json!({
                    "path": l.path,
                    "part": l.part,
                    "n": l.n.to_string(),
                    "canonical": l.text,
                    "value": existing.get(&l.path).cloned().unwrap_or_default(),
                });
                let same = groups
                    .last()
                    .is_some_and(|g| g["name"].as_str() == Some(l.group.as_str()));
                if same {
                    if let Some(arr) = groups
                        .last_mut()
                        .and_then(|g| g.get_mut("leaves"))
                        .and_then(|v| v.as_array_mut())
                    {
                        arr.push(row);
                    }
                } else {
                    groups.push(serde_json::json!({ "name": l.group, "leaves": [row] }));
                }
            }
            ctx.insert("editing_locale", &loc.name);
            ctx.insert("editing_code", &code);
            ctx.insert("trans_groups", &groups);
            ctx.insert(
                "translate_url",
                &format!("/cms-admin/library/{id}/translate"),
            );
            return render_with_csrf(&state, &headers, "rcms_admin/form_translate.html", &mut ctx);
        }
    }

    // The builder's back button says "Forms", so it has to land on the
    // Forms list. Pointing it at the generic Library view dropped editors
    // onto a different listing of the same rows — its own search, its own
    // column picker, no Submissions column — which read as a second,
    // older Forms screen. Every other builder points at its own list.
    ctx.insert("list_url", "/cms-admin/forms");
    // FB-15 — the builder edits the DRAFT; Publish promotes it to live.
    ctx.insert("publish_url", &format!("/cms-admin/forms/{id}/publish"));
    ctx.insert(
        "has_unpublished",
        &crate::forms::schema::has_unpublished_changes(&snippet.data),
    );
    let mut draft = crate::forms::schema::parse_draft(&snippet.data);
    draft.sanitize_rich_text();
    let schema_json = serde_json::to_string(&crate::forms::schema::to_value(&draft))
        .unwrap_or_else(|_| "{}".to_owned());
    ctx.insert("schema_json", &schema_json);
    render_with_csrf(&state, &headers, "rcms_admin/form_builder.html", &mut ctx)
}

#[derive(Debug, Deserialize)]
pub struct FormBuildSubmit {
    pub schema: String,
}

/// Create-a-page-type submit (#566). `type_name` is slugified server-side.
#[derive(Debug, Deserialize)]
pub struct FormPageTypeCreate {
    #[serde(default)]
    pub verbose_name: String,
    #[serde(default)]
    pub type_name: String,
    #[serde(default)]
    pub allowed_parents: String,
    #[serde(default)]
    pub allowed_children: String,
    /// `"auto" | "html" | "api"` — see [`crate::page_view::PageViewMode`].
    /// Offered only for UI types: a code type's mode is re-seeded from
    /// its handler on every boot, so an admin edit would silently revert.
    #[serde(default)]
    pub view_mode: String,
    /// Workflow name, or empty to publish directly (#843).
    #[serde(default)]
    pub workflow: String,
}

/// Component builder submit (#563): builder `schema` (a `{nodes:[…]}` blob)
/// plus the component metadata fields.
#[derive(Debug, Deserialize)]
pub struct FormComponentSubmit {
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub schema: String,
}

/// POST `/cms-admin/forms/{id}/build` — persist the builder's schema into
/// GET|POST /cms-admin/forms/{id}/preview — render this form the way a
/// visitor sees it, for the editor's sidebar preview.
///
/// GET renders the saved draft; POST renders the schema in the request
/// body, which is how the builder shows unsaved edits — the same
/// saved-vs-unsaved split the page preview uses.
///
/// Crucially this calls [`crate::forms::render::render_form_html`] — the
/// one the public site calls. The builder used to draw its own preview in
/// JavaScript, rebuilding the layout a second time in a second language;
/// the two could only agree by coincidence, and every renderer change had
/// to be made twice or the preview quietly started lying.
pub async fn form_preview(
    tenant: Tenant,
    Path(id): Path<i64>,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let snippet = load_form_snippet(&tenant, id).await?;
    let form = crate::forms::schema::parse_draft(&snippet.data);
    Ok(render_form_preview(id, &form))
}

/// POST /cms-admin/forms/{id}/preview — render the schema in the body, so
/// the builder previews edits that have not been saved. Mirrors
/// `page_preview_draft`.
pub async fn form_preview_draft(
    tenant: Tenant,
    Path(id): Path<i64>,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Form(body): Form<FormBuildSubmit>,
) -> Result<Response, AdminError> {
    let snippet = load_form_snippet(&tenant, id).await?;
    // A schema that does not parse is the normal state mid-edit, not an
    // error worth blanking the pane for: fall back to the saved draft and
    // let the editor keep typing.
    let form = crate::forms::schema::parse_str(&body.schema)
        .unwrap_or_else(|_| crate::forms::schema::parse_draft(&snippet.data));
    Ok(render_form_preview(id, &form))
}

/// The snippet behind a form id, refusing anything that is not a form.
async fn load_form_snippet(
    tenant: &Tenant,
    id: i64,
) -> Result<crate::snippet::Snippet, AdminError> {
    let snippet = crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    if snippet.type_name != "form" {
        return Err(AdminError::NotFound(id));
    }
    Ok(snippet)
}

fn render_form_preview(id: i64, form: &crate::forms::schema::Form) -> Response {
    // `embed_id` is empty: this is the standalone render, with no page
    // embedding it and so no per-embed override to resolve.
    let inner = crate::forms::render::render_form_html(form, id, "");
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Form preview</title>\
         <style>body{{margin:0;padding:24px;font:14px/1.5 system-ui,sans-serif;}}</style>\
         </head><body>{inner}</body></html>"
    );
    stamp_no_cache(
        (
            [(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
            )],
            html,
        )
            .into_response(),
    )
}

/// the snippet's `data` column + capture a revision (FB-04 / #537).
pub async fn form_build_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<FormBuildSubmit>,
) -> Result<Response, AdminError> {
    let mut snippet = crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    if snippet.type_name != "form" {
        return Err(AdminError::NotFound(id));
    }
    let mut parsed = crate::forms::schema::parse_str(&form.schema)
        .map_err(|e| AdminError::Validation(format!("invalid form schema: {e}")))?;
    parsed.sanitize_rich_text();
    let mut warnings = crate::forms::schema::validate(&parsed);
    // FB-15 — saving stores a DRAFT; the published (public) form is untouched
    // until Publish.
    snippet.data = crate::forms::schema::save_draft_value(&snippet.data, &parsed);
    snippet.save_pool(tenant.pool()).await?;
    // Keep the notification target in step with the recipients typed into
    // form settings, so the two cannot disagree about who gets told.
    if let Err(e) = crate::notify::targets::sync_form_email(
        tenant.pool(),
        id,
        &snippet.title,
        &parsed.settings.notify_emails,
    )
    .await
    {
        tracing::warn!(
            target: "rustango_cms::notify",
            error = %e, form_id = id,
            "could not sync the form's email notification target"
        );
        warnings.push(format!("Email notifications are off: {e}."));
    }
    let (level, msg) = if warnings.is_empty() {
        (MsgLevel::Success, "Draft saved.".to_owned())
    } else {
        (
            MsgLevel::Warning,
            format!("Draft saved with warnings: {}", warnings.join(" ")),
        )
    };
    redirect_named_with_params_and_message(
        "rcms-admin:forms:build",
        &[("id", id.to_string())],
        level,
        &msg,
        &headers,
    )
}

/// POST `/cms-admin/forms/{id}/publish` — publish the submitted schema: it
/// becomes the live (public) form + the draft, and a revision is captured
/// (FB-15).
pub async fn form_publish_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<FormBuildSubmit>,
) -> Result<Response, AdminError> {
    let mut snippet = crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    if snippet.type_name != "form" {
        return Err(AdminError::NotFound(id));
    }
    let mut parsed = crate::forms::schema::parse_str(&form.schema)
        .map_err(|e| AdminError::Validation(format!("invalid form schema: {e}")))?;
    parsed.sanitize_rich_text();
    snippet.data = crate::forms::schema::publish_value(&parsed);
    snippet.save_pool(tenant.pool()).await?;
    if let Some(h) = crate::library::find_handler(&snippet.type_name) {
        if h.revisions_enabled() {
            let uid = session_user.as_ref().and_then(|u| u.id.get().copied());
            crate::snippet::capture_revision(tenant.pool(), &snippet, uid)
                .await
                .log_warn("revision not captured; the history has a gap");
        }
    }
    redirect_named_with_params_and_message(
        "rcms-admin:forms:build",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Form published — the live form is now up to date.",
        &headers,
    )
}

/// Shared loader for the submissions views (#545 / FB-12): the form
/// snippet, its parsed schema's value-columns, the entries, and a
/// source-page-id → url_path map for provenance links.
async fn load_form_submissions(
    pool: &rustango::sql::Pool,
    form_id: i64,
) -> Result<
    (
        crate::snippet::Snippet,
        Vec<(String, String)>,
        Vec<crate::forms::submit::FormEntry>,
        std::collections::HashMap<i64, String>,
    ),
    AdminError,
> {
    let snippet = crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::id.eq(form_id))
        .where_(crate::snippet::Snippet::type_name.eq("form".to_owned()))
        .first(pool)
        .await?
        .ok_or(AdminError::NotFound(form_id))?;
    let form = crate::forms::schema::parse(&snippet.data).unwrap_or_default();
    let columns: Vec<(String, String)> = form
        .fields()
        .filter(|f| f.field_type.collects_value() && !f.key.is_empty())
        .map(|f| {
            (
                f.key.clone(),
                if f.label.is_empty() {
                    f.key.clone()
                } else {
                    f.label.clone()
                },
            )
        })
        .collect();
    let entries: Vec<crate::forms::submit::FormEntry> = crate::forms::submit::FormEntry::objects()
        .where_(crate::forms::submit::FormEntry::form_snippet_id.eq(form_id))
        .order_by(&[("submitted_at", true)])
        .fetch(pool)
        .await?;
    // Resolve source pages for provenance links.
    let mut page_urls = std::collections::HashMap::new();
    let ids: Vec<i64> = entries
        .iter()
        .map(|e| e.source_page_id)
        .filter(|i| *i != 0)
        .collect();
    if !ids.is_empty() {
        if let Ok(pages) = crate::page::Page::objects().fetch(pool).await {
            for p in pages {
                if let Some(pid) = p.id.get().copied() {
                    if ids.contains(&pid) {
                        page_urls.insert(pid, p.url_path.clone());
                    }
                }
            }
        }
    }
    Ok((snippet, columns, entries, page_urls))
}

/// Render one submission answer cell from the `data_json` value.
fn submission_cell(data: &serde_json::Value, key: &str) -> String {
    match data.get(key) {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect::<Vec<_>>()
            .join("; "),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// GET `/cms-admin/forms/{id}/submissions` — the per-form submission
/// history with a Source-page column (#545 / FB-12).
pub async fn form_submissions_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let (snippet, columns, entries, page_urls) = load_form_submissions(tenant.pool(), id).await?;
    // Which columns are file uploads → render those cells as download links.
    let parsed = crate::forms::schema::parse(&snippet.data).unwrap_or_default();
    let file_keys: std::collections::HashSet<String> = parsed
        .fields()
        .filter(|f| matches!(f.field_type, crate::forms::schema::FieldType::File))
        .map(|f| f.key.clone())
        .collect();
    let rows: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            let cells: Vec<serde_json::Value> = columns
                .iter()
                .map(|(k, _)| {
                    let v = submission_cell(&e.data_json, k);
                    if file_keys.contains(k) && !v.is_empty() {
                        // value is the storage key; display the filename part.
                        let name = v.splitn(2, '-').nth(1).unwrap_or(&v).to_owned();
                        serde_json::json!({
                            "value": name,
                            "url": format!("/cms-admin/forms/{id}/submissions/file/{v}"),
                        })
                    } else {
                        serde_json::json!({ "value": v })
                    }
                })
                .collect();
            serde_json::json!({
                // RFC 3339, for the admin's `<time data-utc>` localizer; the
                // minute-precision text is what shows without JavaScript.
                "submitted_at": e
                    .submitted_at
                    .get()
                    .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                    .unwrap_or_default(),
                "submitted_at_text": e
                    .submitted_at
                    .get()
                    .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_default(),
                "ip": e.ip,
                "source_page": if e.source_page_id == 0 {
                    serde_json::Value::Null
                } else {
                    serde_json::json!({
                        "id": e.source_page_id,
                        "url": page_urls.get(&e.source_page_id).cloned().unwrap_or_default(),
                    })
                },
                "cells": cells,
            })
        })
        .collect();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "forms", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("form_title", &snippet.title);
    ctx.insert("form_id", &id);
    ctx.insert("build_url", &format!("/cms-admin/forms/{id}/build"));
    ctx.insert(
        "csv_url",
        &format!("/cms-admin/forms/{id}/submissions/export.csv"),
    );
    ctx.insert(
        "columns",
        &columns.iter().map(|(_, l)| l).collect::<Vec<_>>(),
    );
    ctx.insert("rows", &rows);
    ctx.insert("total", &entries.len());
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/form_submissions.html",
        &mut ctx,
    )
}

/// GET `/cms-admin/forms/{id}/submissions/export.csv` (#545 / FB-12).
pub async fn form_submissions_csv(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let (snippet, columns, entries, page_urls) = load_form_submissions(tenant.pool(), id).await?;
    let mut out = String::new();
    out.push_str("submitted_at,source_page,ip");
    for (_, label) in &columns {
        out.push(',');
        out.push_str(&csv_escape(label));
    }
    out.push('\n');
    for e in &entries {
        let when = e
            .submitted_at
            .get()
            .map(ToString::to_string)
            .unwrap_or_default();
        let src = if e.source_page_id == 0 {
            String::new()
        } else {
            page_urls
                .get(&e.source_page_id)
                .cloned()
                .unwrap_or_default()
        };
        out.push_str(&csv_escape(&when));
        out.push(',');
        out.push_str(&csv_escape(&src));
        out.push(',');
        out.push_str(&csv_escape(&e.ip));
        for (k, _) in &columns {
            out.push(',');
            out.push_str(&csv_escape(&submission_cell(&e.data_json, k)));
        }
        out.push('\n');
    }
    let filename = format!("submissions-{}.csv", snippet.slug);
    Ok(csv_response(&filename, &out))
}

/// GET `/cms-admin/forms/{id}/submissions/file/{key}` — download a file
/// uploaded through a form submission (admin-gated). Streams from
/// `./var/form-uploads/<tenant>/<key>`; rejects path traversal.
pub async fn form_submission_file(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path((_id, key)): Path<(i64, String)>,
) -> Result<Response, AdminError> {
    if key.contains('/') || key.contains('\\') || key.contains("..") || key.is_empty() {
        return Err(AdminError::NotFound(0));
    }
    let path = std::path::PathBuf::from("./var/form-uploads")
        .join(&tenant.org.slug)
        .join(&key);
    let bytes = std::fs::read(&path).map_err(|_| AdminError::NotFound(0))?;
    let mime = mime_guess::from_path(&key)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();
    let download = key.splitn(2, '-').nth(1).unwrap_or(&key).to_owned();
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{download}\""),
            ),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub struct SetLangQuery {
    pub lang: String,
}

/// POST `/cms-admin/set-language` (`lang=<code>`) — persist the sticky
/// admin-UI language cookie and the user's durable preference, then
/// redirect back to the Referer (#525). POST so a cross-site link can't
/// rewrite a signed-in user's preference: it goes through the CSRF check
/// like every other admin write (#738).
pub async fn admin_set_language(
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(q): Form<SetLangQuery>,
) -> Response {
    let lang = if super::i18n::is_ui_locale(&q.lang) {
        q.lang
    } else {
        "en".to_owned()
    };
    // #526 — persist to the user's durable pref too, so the switcher choice
    // follows them across devices (not just this browser's cookie). Best-effort:
    // a save failure still sets the cookie below.
    if let Some(uid) = session_user.as_ref().and_then(|u| u.id.get().copied()) {
        persist_user_admin_lang(tenant.pool(), uid, &lang)
            .await
            .log_warn("admin language preference not saved");
    }
    // The Referer's path, if it is a local one, else the admin root. The
    // path of `https://evil.test//evil.test/x` is `//evil.test/x`, so it
    // goes through safe_next like any other redirect target (#728).
    let back = headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(|r| match r.split_once("://") {
            Some((_, rest)) => rest.find('/').map(|i| &rest[i..]),
            None => Some(r),
        })
        .and_then(rustango::auth_decorators::safe_next)
        .unwrap_or_else(|| "/cms-admin/".to_owned());
    let cookie = format!(
        "{}={lang}; Path=/; SameSite=Lax; Max-Age=31536000",
        super::i18n::ADMIN_LANG_COOKIE
    );
    let mut resp = axum::response::Redirect::to(&back).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}

/// RFC-4180 CSV field escaping: quote when the value contains a comma,
/// quote, or newline, doubling any embedded quotes. Shared by every
/// admin CSV export (reports, audit log, pages, redirects, …).
fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
        out
    } else {
        s.to_owned()
    }
}

/// Stringify a report cell value for display / CSV: strings pass
/// through; null / missing → empty; numbers + bools → their literal.
fn report_cell_string(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        None | Some(serde_json::Value::Null) => String::new(),
        Some(other) => other.to_string(),
    }
}

pub async fn locale_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let items: Vec<Locale> = Locale::objects()
        .order_by(&[("sort_order", false), ("code", false)])
        .fetch(tenant.pool())
        .await?;
    // Enrich each row with core's static support level so the list can warn
    // where a content locale outruns the framework (no admin-UI catalog, no
    // language-specific plural rule, unknown to core). Serialize the row then
    // attach `support` — keeps every existing template field working.
    let rows: Vec<serde_json::Value> = items
        .iter()
        .map(|l| {
            let info = rustango::i18n::locale_info(&l.code);
            let ui = super::i18n::is_ui_locale(&l.code);
            let mut v = serde_json::to_value(l).unwrap_or_default();
            v["support"] = serde_json::json!({
                "ui_translated": ui,
                "known": info.known,
                "display_name": info.display_name,
                "has_plural_rules": info.has_plural_rules,
                "is_rtl": info.is_rtl,
                "content_only": !ui,
            });
            v
        })
        .collect();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "locales", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("items", &rows);
    render_with_csrf(&state, &headers, "rcms_admin/locale_list.html", &mut ctx)
}

/// Inject core's known-locale roster into a locale-form context: a
/// `known_locales` list (`code`, `name`) for the code datalist, and a
/// `known_locales_meta` map (`code → {ui, is_rtl, has_plural_rules}`) the
/// form JS reads to show a live support hint. Free-text codes stay valid;
/// this is guidance, not a whitelist.
fn insert_known_locales(ctx: &mut Context) {
    let known: Vec<serde_json::Value> = rustango::i18n::known_locales()
        .map(|(code, name)| serde_json::json!({ "code": code, "name": name }))
        .collect();
    let meta: serde_json::Map<String, serde_json::Value> = rustango::i18n::known_locales()
        .map(|(code, _)| {
            let info = rustango::i18n::locale_info(code);
            (
                code.to_owned(),
                serde_json::json!({
                    "ui": super::i18n::is_ui_locale(code),
                    "is_rtl": info.is_rtl,
                    "has_plural_rules": info.has_plural_rules,
                }),
            )
        })
        .collect();
    ctx.insert("known_locales", &known);
    ctx.insert("known_locales_meta", &serde_json::Value::Object(meta));
}

pub async fn locale_new_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "locales", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    insert_known_locales(&mut ctx);
    ctx.insert("mode", "new");
    ctx.insert(
        "locale",
        &serde_json::json!({
            "code": "",
            "name": "",
            "is_default": false,
            "active": true,
            "sort_order": 100,
        }),
    );
    render_with_csrf(&state, &headers, "rcms_admin/locale_form.html", &mut ctx)
}

#[derive(Debug, Deserialize)]
pub struct LocaleForm {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub is_default: Option<String>,
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub sort_order: Option<i32>,
}

impl LocaleForm {
    fn parsed_default(&self) -> bool {
        self.is_default.is_some()
    }
    fn parsed_active(&self) -> bool {
        self.active.is_some()
    }
    fn parsed_sort_order(&self) -> i32 {
        self.sort_order.unwrap_or(100)
    }
}

pub async fn locale_new_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Form(form): Form<LocaleForm>,
) -> Result<Response, AdminError> {
    use rustango::sql::Auto;
    let name = form.name.trim().to_owned();
    if name.is_empty() {
        return Err(AdminError::Validation(
            "Display name is required.".to_owned(),
        ));
    }
    let mut row = Locale {
        id: Auto::Unset,
        code: crate::locale::validate_code(&form.code).map_err(AdminError::Validation)?,
        name,
        is_default: form.parsed_default(),
        active: form.parsed_active(),
        sort_order: form.parsed_sort_order(),
        created_at: Auto::Unset,
    };
    if row.is_default {
        clear_default_flag(&tenant, None).await?;
    }
    row.insert_pool(tenant.pool()).await?;
    redirect_named_with_message_args(
        "rcms-admin:locales:list",
        MsgLevel::Success,
        "Added locale {name} ({code}).",
        &[("name", row.name.as_str()), ("code", row.code.as_str())],
        &headers,
    )
}

pub async fn locale_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let row = Locale::objects()
        .where_(Locale::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "locales", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    insert_known_locales(&mut ctx);
    ctx.insert("mode", "edit");
    ctx.insert("locale", &row);
    render_with_csrf(&state, &headers, "rcms_admin/locale_form.html", &mut ctx)
}

pub async fn locale_edit_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<LocaleForm>,
) -> Result<Response, AdminError> {
    let mut row = Locale::objects()
        .where_(Locale::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let name = form.name.trim().to_owned();
    if name.is_empty() {
        return Err(AdminError::Validation(
            "Display name is required.".to_owned(),
        ));
    }
    row.code = crate::locale::validate_code(&form.code).map_err(AdminError::Validation)?;
    row.name = name;
    row.is_default = form.parsed_default();
    row.active = form.parsed_active();
    row.sort_order = form.parsed_sort_order();
    if row.is_default {
        clear_default_flag(&tenant, Some(id)).await?;
    }
    row.save_pool(tenant.pool()).await?;
    redirect_named_with_message_args(
        "rcms-admin:locales:list",
        MsgLevel::Success,
        "Saved locale {name} ({code}).",
        &[("name", row.name.as_str()), ("code", row.code.as_str())],
        &headers,
    )
}

pub async fn locale_delete_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let row = Locale::objects()
        .where_(Locale::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    if row.is_default {
        return redirect_named_with_message(
            "rcms-admin:locales:list",
            MsgLevel::Error,
            "Cannot delete the default locale — promote another locale to default first.",
            &headers,
        );
    }
    let label = format!("{} ({})", row.name, row.code);
    // Translations reference the locale; they go with it, in one step,
    // or the foreign keys refuse the delete.
    let pages = crate::translation::Translation::objects()
        .where_(crate::translation::Translation::locale_id.eq(id))
        .compile_delete()
        .map_err(rustango::sql::ExecError::from)?;
    let snippets = crate::snippet_translation::SnippetTranslation::objects()
        .where_(crate::snippet_translation::SnippetTranslation::locale_id.eq(id))
        .compile_delete()
        .map_err(rustango::sql::ExecError::from)?;
    let menus = crate::menu_item_translation::MenuItemTranslation::objects()
        .where_(crate::menu_item_translation::MenuItemTranslation::locale_id.eq(id))
        .compile_delete()
        .map_err(rustango::sql::ExecError::from)?;
    let categories = crate::category_translation::CategoryTranslation::objects()
        .where_(crate::category_translation::CategoryTranslation::locale_id.eq(id))
        .compile_delete()
        .map_err(rustango::sql::ExecError::from)?;
    rustango::atomic!(tenant.pool(), |tx| {
        let mut guard = tx.lock().await?;
        let tx = &mut *guard;
        for q in [&pages, &snippets, &menus, &categories] {
            rustango::sql::delete_tx(tx, q).await?;
        }
        row.delete_tx(tx).await?;
        Ok::<_, rustango::sql::ExecError>(())
    })
    .await?;
    redirect_named_with_message_args(
        "rcms-admin:locales:list",
        MsgLevel::Success,
        "Deleted locale {label}.",
        &[("label", label.as_str())],
        &headers,
    )
}

// ============================================================ SSO Providers
//
// Admin-managed, DB-backed multi-provider SSO (the framework's
// `rustango::admin::SsoProvider`, one row per configurable OIDC/social
// provider). Each enabled row becomes a "Sign in with …" button on the
// login page (`/login/sso/{slug}`). This is the UI that replaces hand-
// editing `rustango_sso_providers` — mirrors the Locales CRUD.

/// Built-in provider presets offered in the `kind` dropdown, plus generic
/// `oidc` (needs an issuer URL). Free-text isn't allowed — the framework
/// only knows these preset keys + `oidc`.
const SSO_KINDS: &[(&str, &str)] = &[
    ("google", "Google"),
    ("microsoft", "Microsoft"),
    ("github", "GitHub"),
    ("gitlab", "GitLab"),
    ("discord", "Discord"),
    ("oidc", "Generic OpenID Connect"),
];

/// Serialize a `SsoProvider` row to JSON for the templates. The framework
/// model isn't `Serialize`, so map the fields by hand.
fn sso_provider_json(p: &rustango::admin::SsoProvider) -> serde_json::Value {
    serde_json::json!({
        "id": p.id.get().copied().unwrap_or_default(),
        "slug": p.slug,
        "label": p.label,
        "kind": p.kind,
        "issuer_url": p.issuer_url,
        "client_id": p.client_id,
        // Never surface the secret itself — just whether one is set (for the
        // edit form's "leave blank to keep" hint).
        "has_secret": !p.client_secret.is_empty(),
        "enabled": p.enabled,
        "sort_order": p.sort_order,
        "scopes": p.scopes,
    })
}

pub async fn sso_provider_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    // Identity administration is superuser-only (#672).
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    use rustango::sql::FetcherPool as _;
    let mut items: Vec<rustango::admin::SsoProvider> = rustango::admin::SsoProvider::objects()
        .fetch(tenant.pool())
        .await
        .unwrap_or_default();
    items.sort_by(|a, b| a.sort_order.cmp(&b.sort_order).then(a.slug.cmp(&b.slug)));
    let rows: Vec<serde_json::Value> = items.iter().map(sso_provider_json).collect();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "sso-providers", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("items", &rows);
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/sso_provider_list.html",
        &mut ctx,
    )
}

#[derive(Deserialize)]
pub struct SsoProviderForm {
    pub slug: String,
    pub label: String,
    pub kind: String,
    #[serde(default)]
    pub issuer_url: String,
    pub client_id: String,
    /// The raw OAuth client secret. On edit, blank means "keep the current one"
    /// (the field is never pre-filled — it's write-only).
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub scopes: String,
    #[serde(default)]
    pub enabled: Option<String>,
    #[serde(default)]
    pub sort_order: Option<i32>,
}

impl SsoProviderForm {
    /// Validate + normalize the form into field values, or an error message.
    fn validated(&self) -> Result<ValidatedProvider, String> {
        let slug = self.slug.trim().to_lowercase();
        if slug.is_empty()
            || !slug
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(
                "Slug is required and may only contain letters, numbers, - and _.".to_owned(),
            );
        }
        let label = self.label.trim().to_owned();
        if label.is_empty() {
            return Err("Button label is required.".to_owned());
        }
        let kind = self.kind.trim().to_lowercase();
        if !SSO_KINDS.iter().any(|(k, _)| *k == kind) {
            return Err("Pick a provider type.".to_owned());
        }
        let client_id = self.client_id.trim().to_owned();
        if client_id.is_empty() {
            return Err("Client ID is required.".to_owned());
        }
        let issuer = self.issuer_url.trim().to_owned();
        if kind == "oidc" && issuer.is_empty() {
            return Err("A generic OpenID Connect provider needs an issuer URL.".to_owned());
        }
        let scopes = self.scopes.trim().to_owned();
        Ok(ValidatedProvider {
            slug,
            label,
            kind,
            issuer_url: (!issuer.is_empty()).then_some(issuer),
            client_id,
            scopes: (!scopes.is_empty()).then_some(scopes),
            enabled: self.enabled.is_some(),
            sort_order: self.sort_order.unwrap_or(0),
        })
    }
}

struct ValidatedProvider {
    slug: String,
    label: String,
    kind: String,
    issuer_url: Option<String>,
    client_id: String,
    scopes: Option<String>,
    enabled: bool,
    sort_order: i32,
}

fn render_sso_provider_form(ctx: &mut Context, mode: &str, provider: serde_json::Value) {
    let kinds: Vec<serde_json::Value> = SSO_KINDS
        .iter()
        .map(|(k, name)| serde_json::json!({ "key": k, "name": name }))
        .collect();
    ctx.insert("mode", mode);
    ctx.insert("kinds", &kinds);
    ctx.insert("provider", &provider);
}

pub async fn sso_provider_new_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    // Identity administration is superuser-only (#672).
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "sso-providers", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    render_sso_provider_form(
        &mut ctx,
        "new",
        serde_json::json!({
            "slug": "", "label": "", "kind": "google", "issuer_url": null,
            "client_id": "", "has_secret": false, "scopes": null,
            "enabled": true, "sort_order": 0,
        }),
    );
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/sso_provider_form.html",
        &mut ctx,
    )
}

async fn find_sso_provider(
    tenant: &Tenant,
    id: i64,
) -> Result<rustango::admin::SsoProvider, AdminError> {
    use rustango::sql::FetcherPool as _;
    rustango::admin::SsoProvider::objects()
        .filter("id", id)
        .fetch(tenant.pool())
        .await
        .map_err(|e| AdminError::Validation(format!("db: {e}")))?
        .into_iter()
        .next()
        .ok_or(AdminError::NotFound(id))
}

pub async fn sso_provider_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    // Identity administration is superuser-only (#672).
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let row = find_sso_provider(&tenant, id).await?;
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "sso-providers", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    render_sso_provider_form(&mut ctx, "edit", sso_provider_json(&row));
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/sso_provider_form.html",
        &mut ctx,
    )
}

pub async fn sso_provider_new_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(form): Form<SsoProviderForm>,
) -> Result<Response, AdminError> {
    // Identity administration is superuser-only (#672).
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let v = form.validated().map_err(AdminError::Validation)?;
    let secret = form.client_secret.trim().to_owned();
    if secret.is_empty() {
        return Err(AdminError::Validation(
            "Client secret is required.".to_owned(),
        ));
    }
    let mut row = rustango::admin::SsoProvider {
        id: rustango::sql::Auto::Unset,
        slug: v.slug,
        label: v.label,
        kind: v.kind,
        issuer_url: v.issuer_url,
        client_id: v.client_id,
        client_secret: rustango::casts::Cast::new(secret),
        enabled: v.enabled,
        sort_order: v.sort_order,
        scopes: v.scopes,
        allow_email_link: false,
        created_at: rustango::sql::Auto::Unset,
        updated_at: rustango::sql::Auto::Unset,
    };
    row.insert_pool(tenant.pool()).await?;
    redirect_named_with_message_args(
        "rcms-admin:sso-providers:list",
        MsgLevel::Success,
        "Added SSO provider {label}.",
        &[("label", row.label.as_str())],
        &headers,
    )
}

pub async fn sso_provider_edit_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<SsoProviderForm>,
) -> Result<Response, AdminError> {
    // Identity administration is superuser-only (#672).
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let mut row = find_sso_provider(&tenant, id).await?;
    let v = form.validated().map_err(AdminError::Validation)?;
    row.slug = v.slug;
    row.label = v.label;
    row.kind = v.kind;
    row.issuer_url = v.issuer_url;
    row.client_id = v.client_id;
    // Write-only secret: only replace it when a new one was typed; a blank
    // field keeps the stored (encrypted) value.
    let new_secret = form.client_secret.trim();
    if !new_secret.is_empty() {
        row.client_secret = rustango::casts::Cast::new(new_secret.to_owned());
    }
    row.enabled = v.enabled;
    row.sort_order = v.sort_order;
    row.scopes = v.scopes;
    row.save_pool(tenant.pool()).await?;
    redirect_named_with_message_args(
        "rcms-admin:sso-providers:list",
        MsgLevel::Success,
        "Saved SSO provider {label}.",
        &[("label", row.label.as_str())],
        &headers,
    )
}

pub async fn sso_provider_delete_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    // Identity administration is superuser-only (#672).
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let row = find_sso_provider(&tenant, id).await?;
    let label = row.label.clone();
    row.delete_pool(tenant.pool()).await?;
    redirect_named_with_message_args(
        "rcms-admin:sso-providers:list",
        MsgLevel::Success,
        "Deleted SSO provider {label}.",
        &[("label", label.as_str())],
        &headers,
    )
}

/// Clear `is_default = true` on every other row so the new default
/// row owns the flag uniquely. Optionally exclude an id when
/// editing the same row.
async fn clear_default_flag(tenant: &Tenant, except: Option<i64>) -> Result<(), AdminError> {
    let rows: Vec<Locale> = Locale::objects()
        .where_(Locale::is_default.eq(true))
        .fetch(tenant.pool())
        .await?;
    for mut r in rows {
        if let (Some(skip), Some(id)) = (except, r.id.get().copied()) {
            if skip == id {
                continue;
            }
        }
        r.is_default = false;
        r.save_pool(tenant.pool()).await?;
    }
    Ok(())
}

// =====================================================================
// Workflows — multi-step approval CRUD (#73 PR 1)
// =====================================================================

pub async fn workflows_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let items: Vec<crate::workflow::Workflow> = crate::workflow::Workflow::objects()
        .order_by(&[("name", false)])
        .fetch(tenant.pool())
        .await?;
    // Task counts per workflow — surfaced in the list as a quick
    // indicator. One query for all tasks; bucket client-side.
    let all_tasks: Vec<crate::workflow::WorkflowTask> = crate::workflow::WorkflowTask::objects()
        .fetch(tenant.pool())
        .await?;
    let mut task_counts: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for t in &all_tasks {
        *task_counts.entry(t.workflow_id).or_insert(0) += 1;
    }
    let rows: Vec<serde_json::Value> = items
        .iter()
        .map(|w| {
            let id = w.id.get().copied().unwrap_or_default();
            serde_json::json!({
                "id": id,
                "name": w.name,
                "description": w.description,
                "active": w.active,
                "require_reapproval_on_edit": w.require_reapproval_on_edit,
                "task_count": task_counts.get(&id).copied().unwrap_or(0),
                "created_at": w.created_at.get().copied(),
            })
        })
        .collect();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "workflows", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("items", &rows);
    render_with_csrf(&state, &headers, "rcms_admin/workflow_list.html", &mut ctx)
}

pub async fn workflow_new_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "workflows", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("mode", "new");
    ctx.insert(
        "workflow",
        &serde_json::json!({
            "name": "",
            "description": "",
            "active": true,
            "require_reapproval_on_edit": false,
        }),
    );
    ctx.insert("tasks", &Vec::<serde_json::Value>::new());
    ctx.insert("roles", &Vec::<serde_json::Value>::new());
    render_with_csrf(&state, &headers, "rcms_admin/workflow_form.html", &mut ctx)
}

#[derive(Debug, Deserialize)]
pub struct WorkflowForm {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub require_reapproval_on_edit: Option<String>,
}

impl WorkflowForm {
    fn parsed_active(&self) -> bool {
        self.active.is_some()
    }
    fn parsed_reapproval(&self) -> bool {
        self.require_reapproval_on_edit.is_some()
    }
}

pub async fn workflow_new_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Form(form): Form<WorkflowForm>,
) -> Result<Response, AdminError> {
    use rustango::sql::Auto;
    let name = form.name.trim().to_owned();
    if name.is_empty() {
        return redirect_named_with_message(
            "rcms-admin:workflows:list",
            MsgLevel::Error,
            "Workflow name is required.",
            &headers,
        );
    }
    let mut row = crate::workflow::Workflow {
        id: Auto::Unset,
        name: name.clone(),
        description: form.description.trim().to_owned(),
        active: form.parsed_active(),
        require_reapproval_on_edit: form.parsed_reapproval(),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    row.insert_pool(tenant.pool()).await?;
    let id = row.id.get().copied().unwrap_or_default();
    redirect_named_with_params_and_message_args(
        "rcms-admin:workflows:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Created workflow “{name}”. Add review steps below.",
        &[("name", name.as_str())],
        &headers,
    )
}

pub async fn workflow_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let row = crate::workflow::Workflow::objects()
        .where_(crate::workflow::Workflow::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let tasks: Vec<crate::workflow::WorkflowTask> = crate::workflow::WorkflowTask::objects()
        .where_(crate::workflow::WorkflowTask::workflow_id.eq(id))
        .order_by(&[("sort_order", false), ("id", false)])
        .fetch(tenant.pool())
        .await?;
    // Role picker — every tenant role is a valid assignee for a
    // task. Sort alphabetically for predictable ordering.
    let roles: Vec<rustango::tenancy::permissions::Role> =
        rustango::tenancy::permissions::Role::objects()
            .order_by(&[("name", false)])
            .fetch(tenant.pool())
            .await?;
    let role_rows: Vec<serde_json::Value> = roles
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id.get().copied(),
                "name": r.name,
            })
        })
        .collect();
    let role_by_id: std::collections::HashMap<i64, String> = roles
        .iter()
        .filter_map(|r| r.id.get().copied().map(|id| (id, r.name.clone())))
        .collect();
    let task_rows: Vec<serde_json::Value> = tasks
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id.get().copied(),
                "name": t.name,
                "role_id": t.role_id,
                "role_name": role_by_id.get(&t.role_id).cloned().unwrap_or_default(),
                "sort_order": t.sort_order,
                "kind": t.kind,
                "webhook_url": t.webhook_url,
            })
        })
        .collect();
    // #191 — every registered task kind, surfaced to the template
    // so the Add-step form can offer the kind picker.
    // A person's approval first: it's what a review step normally is,
    // and the pre-selected kind.
    let mut task_kinds: Vec<serde_json::Value> = crate::task_kind::registered_kinds()
        .iter()
        .map(|k| {
            serde_json::json!({
                "name": k.kind_name(),
                "verbose_name": k.verbose_name(),
            })
        })
        .collect();
    task_kinds.sort_by_key(|k| k["name"] != "group_approval");
    let kind_label = |name: &str| {
        crate::task_kind::registered_kinds()
            .iter()
            .find(|k| k.kind_name() == name)
            .map_or_else(|| name.to_owned(), |k| k.verbose_name().to_owned())
    };
    let task_rows: Vec<serde_json::Value> = task_rows
        .into_iter()
        .map(|mut t| {
            let label = kind_label(t["kind"].as_str().unwrap_or_default());
            t["kind_label"] = serde_json::Value::String(label);
            t
        })
        .collect();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "workflows", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("mode", "edit");
    ctx.insert("workflow", &row);
    ctx.insert("tasks", &task_rows);
    ctx.insert("roles", &role_rows);
    ctx.insert("task_kinds", &task_kinds);
    render_with_csrf(&state, &headers, "rcms_admin/workflow_form.html", &mut ctx)
}

pub async fn workflow_edit_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<WorkflowForm>,
) -> Result<Response, AdminError> {
    let mut row = crate::workflow::Workflow::objects()
        .where_(crate::workflow::Workflow::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let old_name = row.name.clone();
    row.name = form.name.trim().to_owned();
    row.description = form.description.trim().to_owned();
    row.active = form.parsed_active();
    row.require_reapproval_on_edit = form.parsed_reapproval();
    row.save_pool(tenant.pool()).await?;
    // Page types name their workflow; a rename must move them along, or
    // their pages would silently publish without review.
    if old_name != row.name {
        crate::workflow::rebind_page_types(tenant.pool(), &old_name, &row.name).await?;
    }
    redirect_named_with_params_and_message_args(
        "rcms-admin:workflows:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Saved workflow “{name}”.",
        &[("name", row.name.as_str())],
        &headers,
    )
}

pub async fn workflow_delete_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let row = crate::workflow::Workflow::objects()
        .where_(crate::workflow::Workflow::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // Page types that use it would quietly stop requiring review.
    let users: Vec<String> = crate::workflow::page_types_using(tenant.pool(), &row.name)
        .await?
        .into_iter()
        .map(|pt| pt.verbose_name)
        .collect();
    if !users.is_empty() {
        let types = users.join(", ");
        return redirect_named_with_message_args(
            "rcms-admin:workflows:list",
            MsgLevel::Error,
            "Can't delete “{title}” — the page types {types} use it. Choose another workflow for them first.",
            &[("title", row.name.as_str()), ("types", types.as_str())],
            &headers,
        );
    }
    // Reject when any WorkflowState references this workflow — the
    // audit trail would otherwise lose its parent FK. Editors should
    // toggle `active = false` instead.
    let in_use: Vec<crate::workflow::WorkflowState> = crate::workflow::WorkflowState::objects()
        .where_(crate::workflow::WorkflowState::workflow_id.eq(id))
        .fetch(tenant.pool())
        .await?;
    if !in_use.is_empty() {
        let count = in_use.len().to_string();
        return redirect_named_with_message_plural(
            "rcms-admin:workflows:list",
            MsgLevel::Error,
            "Can't delete “{title}” — {count} page(s) have history under it. Toggle Active off instead.",
            in_use.len() as i64,
            &[("title", row.name.as_str()), ("count", count.as_str())],
            &headers,
        );
    }
    // Cascade the task rows by hand (FK isn't ON DELETE CASCADE).
    let tasks: Vec<crate::workflow::WorkflowTask> = crate::workflow::WorkflowTask::objects()
        .where_(crate::workflow::WorkflowTask::workflow_id.eq(id))
        .fetch(tenant.pool())
        .await?;
    for t in tasks {
        t.delete_pool(tenant.pool()).await?;
    }
    let name = row.name.clone();
    row.delete_pool(tenant.pool()).await?;
    redirect_named_with_message_args(
        "rcms-admin:workflows:list",
        MsgLevel::Success,
        "Deleted workflow “{name}”.",
        &[("name", name.as_str())],
        &headers,
    )
}

#[derive(Debug, Deserialize)]
pub struct WorkflowTaskForm {
    pub name: String,
    pub role_id: i64,
    /// #191 — task kind discriminator. Defaults to `group_approval`
    /// when the form input is missing or empty so the existing
    /// human-approval flow stays the default behavior.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub webhook_url: Option<String>,
}

pub async fn workflow_task_add(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(workflow_id): Path<i64>,
    Form(form): Form<WorkflowTaskForm>,
) -> Result<Response, AdminError> {
    use rustango::sql::Auto;
    let name = form.name.trim().to_owned();
    if name.is_empty() {
        return redirect_named_with_params_and_message(
            "rcms-admin:workflows:edit",
            &[("id", workflow_id.to_string())],
            MsgLevel::Error,
            "Step name is required.",
            &headers,
        );
    }
    let existing: Vec<crate::workflow::WorkflowTask> = crate::workflow::WorkflowTask::objects()
        .where_(crate::workflow::WorkflowTask::workflow_id.eq(workflow_id))
        .order_by(&[("sort_order", true)]) // desc
        .fetch(tenant.pool())
        .await?;
    let next_order = existing.first().map_or(10, |t| t.sort_order + 10);
    let kind = form
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| crate::task_kind::find(s).is_some())
        .unwrap_or("group_approval")
        .to_owned();
    let webhook_url = form
        .webhook_url
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .to_owned();
    let mut row = crate::workflow::WorkflowTask {
        id: Auto::Unset,
        workflow_id,
        name: name.clone(),
        role_id: form.role_id,
        sort_order: next_order,
        kind,
        webhook_url,
    };
    row.insert_pool(tenant.pool()).await?;
    redirect_named_with_params_and_message_args(
        "rcms-admin:workflows:edit",
        &[("id", workflow_id.to_string())],
        MsgLevel::Success,
        "Added step “{name}”.",
        &[("name", name.as_str())],
        &headers,
    )
}

pub async fn workflow_task_delete(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path((workflow_id, task_id)): Path<(i64, i64)>,
) -> Result<Response, AdminError> {
    let row = crate::workflow::WorkflowTask::objects()
        .where_(crate::workflow::WorkflowTask::id.eq(task_id))
        .where_(crate::workflow::WorkflowTask::workflow_id.eq(workflow_id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(task_id))?;
    let name = row.name.clone();
    row.delete_pool(tenant.pool()).await?;
    redirect_named_with_params_and_message_args(
        "rcms-admin:workflows:edit",
        &[("id", workflow_id.to_string())],
        MsgLevel::Success,
        "Removed step “{name}”.",
        &[("name", name.as_str())],
        &headers,
    )
}

// ---------------------------------------------------------------
// Per-page workflow actions (#73 PR 2): submit / approve / reject /
// cancel. Permission rules:
//   - submit: any logged-in user (page-Edit perm already gated the
//     editor URL).
//   - approve / reject: viewer must be a member of the role bound
//     to the current task. Tenant superusers bypass.
//   - cancel: any logged-in user (audit captures who).
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct WorkflowDecisionForm {
    #[serde(default)]
    pub comment: String,
    /// The step the reviewer was looking at (#707). A decision posted
    /// for a step that is no longer current is refused, not applied to
    /// whichever step is current now.
    #[serde(default)]
    pub task_id: Option<i64>,
}

/// #707 — the decision the reviewer posted is for a step that is no
/// longer the current one, or another request decided it first.
fn step_already_decided(page_id: i64, headers: &HeaderMap) -> Result<Response, AdminError> {
    redirect_named_with_params_and_message(
        "rcms-admin:pages:edit",
        &[("id", page_id.to_string())],
        MsgLevel::Error,
        "That review step was already decided — check the page's current step before deciding again.",
        headers,
    )
}

async fn user_is_in_role(
    pool: &rustango::sql::Pool,
    user_id: i64,
    role_id: i64,
) -> Result<bool, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<rustango::tenancy::permissions::UserRole> =
        rustango::tenancy::permissions::UserRole::objects()
            .where_(rustango::tenancy::permissions::UserRole::user_id.eq(user_id))
            .where_(rustango::tenancy::permissions::UserRole::role_id.eq(role_id))
            .fetch(pool)
            .await?;
    Ok(!rows.is_empty())
}

pub async fn page_workflow_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let Some(viewer_id) = session_user.as_ref().and_then(|u| u.id.get().copied()) else {
        return Ok(unauthorized_no_session());
    };
    let page = Page::objects()
        .where_(Page::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // Resolve the page-type → workflow slug.
    let pts: Vec<PageType> = PageType::objects()
        .where_(PageType::id.eq(page.page_type_id))
        .fetch(tenant.pool())
        .await?;
    let Some(pt) = pts.into_iter().next() else {
        return redirect_named_with_params_and_message(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Error,
            "Page type row not found.",
            &headers,
        );
    };
    let Some(slug) = pt.workflow_name() else {
        return redirect_named_with_params_and_message(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Error,
            "This page type isn't bound to a workflow — nothing to submit.",
            &headers,
        );
    };
    let Some(workflow) = crate::workflow::find_by_name(tenant.pool(), &slug).await? else {
        return redirect_named_with_params_and_message_args(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Error,
            "Workflow `{slug}` not found or inactive — ask an admin to enable it.",
            &[("slug", slug.as_str())],
            &headers,
        );
    };
    let tasks = crate::workflow::tasks_for(
        tenant.pool(),
        workflow.id.get().copied().unwrap_or_default(),
    )
    .await?;
    if tasks.is_empty() {
        return redirect_named_with_params_and_message(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Error,
            "Workflow has no review steps yet — ask an admin to add at least one step.",
            &headers,
        );
    }
    // Only block when an in-progress review already exists. A
    // `needs_changes` row means the prior cycle was rejected; the
    // author is allowed to resubmit, which leaves the rejected row
    // as audit history and starts a fresh cycle.
    if let Some(active) = crate::workflow::active_state_for_page(tenant.pool(), id).await? {
        if active.status == crate::workflow::WorkflowStatus::InProgress.as_str() {
            return redirect_named_with_params_and_message(
                "rcms-admin:pages:edit",
                &[("id", id.to_string())],
                MsgLevel::Warning,
                "This page is already under review.",
                &headers,
            );
        }
    }
    crate::workflow::submit_for_review(tenant.pool(), id, &workflow, &tasks, viewer_id).await?;
    // The submitter waits for the verdict now; their edit lock would
    // only stop the reviewer from editing.
    crate::lock::release(tenant.pool(), id, viewer_id)
        .await
        .log_warn("editor lock not released on submit");

    // #192 — audit log the workflow submission.
    crate::page_log::record_or_warn(
        tenant.pool(),
        id,
        crate::page_log::ACTION_WORKFLOW_SUBMIT,
        Some(viewer_id),
        format!(
            "Submitted for review — workflow “{}”, first step “{}”.",
            workflow.name, tasks[0].name
        ),
        Some(serde_json::json!({
            "workflow_id": workflow.id.get().copied(),
            "workflow_name": workflow.name,
            "first_task": tasks[0].name,
        })),
    )
    .await;

    // #85 — notify the first task's reviewers that a review is
    // waiting. No-op when no mailer is wired into AdminState.
    notify_workflow(
        _state.mailer.as_deref(),
        &_state.mailer_from,
        tenant.pool(),
        &tenant.org.slug,
        "submit",
        &workflow.name,
        &page.title,
        id,
        Some(&tasks[0].name),
        session_user.as_ref().map(|u| u.username.as_str()),
        None,
        WorkflowRecipients::Role(tasks[0].role_id),
    )
    .await;

    redirect_named_with_params_and_message_args(
        "rcms-admin:pages:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Submitted for review — first step: “{name}”.",
        &[("name", tasks[0].name.as_str())],
        &headers,
    )
}

/// Recipient selector for [`notify_workflow`].
enum WorkflowRecipients {
    /// Members of a single role (Wagtail reviewers on the active task).
    Role(i64),
    /// A single user — used to notify the original submitter on
    /// approve-finish / reject / cancel.
    User(i64),
}

/// Helper invoked by the workflow handlers to dispatch the right
/// notification per event. Resolves recipients, composes subject +
/// body, and hands off to the framework's `Mailer`. No-op if the
/// AdminState has no mailer attached.
#[allow(clippy::too_many_arguments)]
async fn notify_workflow(
    mailer: Option<&dyn rustango::email::Mailer>,
    from: &str,
    pool: &rustango::sql::Pool,
    tenant_slug: &str,
    event: &str,
    workflow_name: &str,
    page_title: &str,
    page_id: i64,
    task_name: Option<&str>,
    actor_username: Option<&str>,
    comment: Option<&str>,
    recipients: WorkflowRecipients,
) {
    let Some(mailer) = mailer else {
        return;
    };
    // #197 — resolve recipients to user_ids first so we can filter
    // out users who have muted this event class before composing
    // email bodies.
    let kind = crate::notification_pref::pref_kind_for_workflow_event(event);
    let candidate_user_ids: Vec<i64> = match recipients {
        WorkflowRecipients::Role(role_id) => {
            crate::workflow_mail::role_member_user_ids(pool, role_id)
                .await
                .unwrap_or_default()
        }
        WorkflowRecipients::User(user_id) => vec![user_id],
    };
    let allowed_ids = crate::notification_pref::filter_enabled(pool, &candidate_user_ids, kind)
        .await
        .unwrap_or(candidate_user_ids);
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(allowed_ids.len());
    for uid in allowed_ids {
        if let Ok(Some(pair)) = crate::workflow_mail::user_email(pool, uid, tenant_slug).await {
            pairs.push(pair);
        }
    }
    if pairs.is_empty() {
        return;
    }
    let (subject, body) = crate::workflow_mail::render_notification(
        event,
        workflow_name,
        page_title,
        page_id,
        task_name,
        actor_username,
        comment,
    );
    crate::workflow_mail::notify_many(mailer, from, &pairs, &subject, &body).await;
}

pub async fn page_workflow_approve(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<WorkflowDecisionForm>,
) -> Result<Response, AdminError> {
    let Some(viewer) = session_user.as_ref() else {
        return Ok(unauthorized_no_session());
    };
    let viewer_id = viewer.id.get().copied().unwrap_or_default();
    let Some(mut state) = crate::workflow::active_state_for_page(tenant.pool(), id).await? else {
        return redirect_named_with_params_and_message(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Error,
            "No active workflow on this page.",
            &headers,
        );
    };
    let tasks = crate::workflow::tasks_for(tenant.pool(), state.workflow_id).await?;
    let Some(current_task) = state
        .current_task_id
        .and_then(|tid| tasks.iter().find(|t| t.id.get().copied() == Some(tid)))
    else {
        return redirect_named_with_params_and_message(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Error,
            "Workflow has no current task — refresh the page.",
            &headers,
        );
    };
    if !viewer.is_superuser
        && !user_is_in_role(tenant.pool(), viewer_id, current_task.role_id).await?
    {
        return redirect_named_with_params_and_message_args(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Error,
            "Only members of the task's role can approve “{name}”.",
            &[("name", current_task.name.as_str())],
            &headers,
        );
    }
    if form.task_id.is_some_and(|t| current_task.id.get().copied() != Some(t)) {
        return step_already_decided(id, &headers);
    }
    // Capture submitter_id + page title BEFORE the transition so we
    // can route the notification on Finished.
    let submitter_id = state.requested_by;
    let page_title: String = Page::objects()
        .where_(Page::id.eq(id))
        .fetch(tenant.pool())
        .await
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|p| p.title)
        .unwrap_or_else(|| format!("page #{id}"));
    let workflow_name: String = crate::workflow::Workflow::objects()
        .where_(crate::workflow::Workflow::id.eq(state.workflow_id))
        .fetch(tenant.pool())
        .await
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|w| w.name)
        .unwrap_or_else(|| "Workflow".to_owned());
    let outcome = crate::workflow::approve_current(
        tenant.pool(),
        &mut state,
        &tasks,
        viewer_id,
        &form.comment,
    )
    .await?;
    let msg = match outcome {
        crate::workflow::ApproveOutcome::AlreadyDecided => {
            return step_already_decided(id, &headers);
        }
        crate::workflow::ApproveOutcome::Advanced { next_task_id } => {
            let next = tasks
                .iter()
                .find(|t| t.id.get().copied() == Some(next_task_id));
            let next_name = next.map(|t| t.name.as_str()).unwrap_or("next step");
            // #85 — notify reviewers of the new current step.
            if let Some(nt) = next {
                notify_workflow(
                    _state.mailer.as_deref(),
                    &_state.mailer_from,
                    tenant.pool(),
                    &tenant.org.slug,
                    "approve_advance",
                    &workflow_name,
                    &page_title,
                    id,
                    Some(&nt.name),
                    Some(&viewer.username),
                    Some(&form.comment),
                    WorkflowRecipients::Role(nt.role_id),
                )
                .await;
            }
            // #192 — audit log the approve-advance.
            crate::page_log::record_or_warn(
                tenant.pool(),
                id,
                crate::page_log::ACTION_WORKFLOW_APPROVE,
                Some(viewer_id),
                format!(
                    "Approved “{}” — advanced to “{next_name}”.",
                    current_task.name
                ),
                Some(serde_json::json!({
                    "outcome": "advanced",
                    "task_name": current_task.name,
                    "next_task_name": next_name,
                    "comment": form.comment,
                })),
            )
            .await;
            format!("Approved “{}” — moved to “{next_name}”.", current_task.name)
        }
        crate::workflow::ApproveOutcome::Finished => {
            // Changes to the live page held for this review go live now,
            // through the normal save path.
            if let Some(pending) = crate::pending_change::for_page(tenant.pool(), id).await? {
                match apply_page_edit(
                    tenant.pool(),
                    &tenant.org.slug,
                    &_state.cache_invalidator,
                    _state.mailer.as_deref(),
                    &_state.mailer_from,
                    false,
                    id,
                    &pending.form_map(),
                    None,
                )
                .await
                {
                    Ok(Ok(_)) => crate::pending_change::discard(tenant.pool(), id).await?,
                    Ok(Err(_)) | Err(_) => {
                        tracing::warn!(page_id = id, "held change could not be applied on approval");
                    }
                }
            }
            // Auto-publish on workflow completion.
            if let Some(mut page) = Page::objects()
                .where_(Page::id.eq(id))
                .first(tenant.pool())
                .await?
            {
                page.status = crate::page::PageStatus::Published.as_str().to_owned();
                let now = chrono::Utc::now();
                if page.published_at.is_none() {
                    page.published_at = Some(now);
                }
                // #251 — workflow-finish publish bumps last-publish.
                page.last_published_at = Some(now);
                page.save_pool(tenant.pool()).await?;
                // #692 — the same go-live effects as any other publish
                // (search index + after_publish_page hooks), and the page's
                // cached URL goes too: it used to keep serving the old copy.
                crate::page::went_live_effects(&tenant.org.slug, &page).await;
                crate::task_queue::purge_urls(
                    _state.cache_invalidator.clone(),
                    tenant.org.slug.clone(),
                    vec![page.url_path.clone()],
                )
                .await;
                // #115 — notify-on-publish subscribers.
                crate::page_subscription::notify_publish(
                    _state.mailer.as_deref(),
                    &_state.mailer_from,
                    tenant.pool(),
                    &tenant.org.slug,
                    id,
                    &page.title,
                    &page.url_path,
                    Some(&viewer.username),
                )
                .await;
            }
            // #85 — notify the submitter that their submission shipped.
            notify_workflow(
                _state.mailer.as_deref(),
                &_state.mailer_from,
                tenant.pool(),
                &tenant.org.slug,
                "approve_finish",
                &workflow_name,
                &page_title,
                id,
                Some(&current_task.name),
                Some(&viewer.username),
                Some(&form.comment),
                WorkflowRecipients::User(submitter_id),
            )
            .await;
            // #192 — audit log the workflow-finish approval + the
            // auto-publish it triggered.
            crate::page_log::record_or_warn(
                tenant.pool(),
                id,
                crate::page_log::ACTION_WORKFLOW_APPROVE,
                Some(viewer_id),
                format!(
                    "Approved “{}” — workflow complete; page auto-published.",
                    current_task.name
                ),
                Some(serde_json::json!({
                    "outcome": "finished",
                    "task_name": current_task.name,
                    "comment": form.comment,
                })),
            )
            .await;
            crate::page_log::record_or_warn(
                tenant.pool(),
                id,
                crate::page_log::ACTION_PUBLISH,
                Some(viewer_id),
                "Published via workflow approval.",
                Some(serde_json::json!({ "via": "workflow" })),
            )
            .await;
            format!(
                "Approved “{}” — workflow complete, page published.",
                current_task.name
            )
        }
    };
    redirect_named_with_params_and_message(
        "rcms-admin:pages:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        &msg,
        &headers,
    )
}

pub async fn page_workflow_reject(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<WorkflowDecisionForm>,
) -> Result<Response, AdminError> {
    let Some(viewer) = session_user.as_ref() else {
        return Ok(unauthorized_no_session());
    };
    let viewer_id = viewer.id.get().copied().unwrap_or_default();
    let Some(mut state) = crate::workflow::active_state_for_page(tenant.pool(), id).await? else {
        return redirect_named_with_params_and_message(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Error,
            "No active workflow on this page.",
            &headers,
        );
    };
    let tasks = crate::workflow::tasks_for(tenant.pool(), state.workflow_id).await?;
    let Some(current_task) = state
        .current_task_id
        .and_then(|tid| tasks.iter().find(|t| t.id.get().copied() == Some(tid)))
    else {
        return redirect_named_with_params_and_message(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Error,
            "Workflow has no current task — refresh the page.",
            &headers,
        );
    };
    if !viewer.is_superuser
        && !user_is_in_role(tenant.pool(), viewer_id, current_task.role_id).await?
    {
        return redirect_named_with_params_and_message_args(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Error,
            "Only members of the task's role can reject “{name}”.",
            &[("name", current_task.name.as_str())],
            &headers,
        );
    }
    if form.task_id.is_some_and(|t| current_task.id.get().copied() != Some(t)) {
        return step_already_decided(id, &headers);
    }
    if form.comment.trim().is_empty() {
        return redirect_named_with_params_and_message(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Error,
            "Say what should change — the author sees it in the review history.",
            &headers,
        );
    }
    let submitter_id = state.requested_by;
    let workflow_id = state.workflow_id;
    if !crate::workflow::reject_current(tenant.pool(), &mut state, viewer_id, &form.comment).await? {
        return step_already_decided(id, &headers);
    }
    // #192 — audit log the rejection.
    crate::page_log::record_or_warn(
        tenant.pool(),
        id,
        crate::page_log::ACTION_WORKFLOW_REJECT,
        Some(viewer_id),
        format!("Rejected “{}” — changes requested.", current_task.name),
        Some(serde_json::json!({
            "task_name": current_task.name,
            "comment": form.comment,
        })),
    )
    .await;
    // #85 — notify the submitter that changes are requested.
    let workflow_name: String = crate::workflow::Workflow::objects()
        .where_(crate::workflow::Workflow::id.eq(workflow_id))
        .fetch(tenant.pool())
        .await
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|w| w.name)
        .unwrap_or_else(|| "Workflow".to_owned());
    let page_title: String = Page::objects()
        .where_(Page::id.eq(id))
        .fetch(tenant.pool())
        .await
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|p| p.title)
        .unwrap_or_else(|| format!("page #{id}"));
    notify_workflow(
        _state.mailer.as_deref(),
        &_state.mailer_from,
        tenant.pool(),
        &tenant.org.slug,
        "reject",
        &workflow_name,
        &page_title,
        id,
        Some(&current_task.name),
        Some(&viewer.username),
        Some(&form.comment),
        WorkflowRecipients::User(submitter_id),
    )
    .await;
    redirect_named_with_params_and_message_args(
        "rcms-admin:pages:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Rejected “{name}” — page returned to author for changes.",
        &[("name", current_task.name.as_str())],
        &headers,
    )
}

pub async fn page_workflow_cancel(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let Some(viewer_id) = session_user.as_ref().and_then(|u| u.id.get().copied()) else {
        return Ok(unauthorized_no_session());
    };
    let Some(mut state) = crate::workflow::active_state_for_page(tenant.pool(), id).await? else {
        return redirect_named_with_params_and_message(
            "rcms-admin:pages:edit",
            &[("id", id.to_string())],
            MsgLevel::Warning,
            "This page isn't currently under review.",
            &headers,
        );
    };
    let submitter_id = state.requested_by;
    let workflow_id = state.workflow_id;
    crate::workflow::cancel(tenant.pool(), &mut state, viewer_id).await?;
    // #192 — audit log the cancellation.
    crate::page_log::record_or_warn(
        tenant.pool(),
        id,
        crate::page_log::ACTION_WORKFLOW_CANCEL,
        Some(viewer_id),
        "Workflow cancelled.",
        Some(serde_json::json!({ "workflow_id": workflow_id })),
    )
    .await;
    // #85 — notify the submitter the review was cancelled. No-op
    // when the submitter is the canceller themselves (the mailer
    // still sends but the user's inbox is the audit trail).
    let workflow_name: String = crate::workflow::Workflow::objects()
        .where_(crate::workflow::Workflow::id.eq(workflow_id))
        .fetch(tenant.pool())
        .await
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|w| w.name)
        .unwrap_or_else(|| "Workflow".to_owned());
    let page_title: String = Page::objects()
        .where_(Page::id.eq(id))
        .fetch(tenant.pool())
        .await
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|p| p.title)
        .unwrap_or_else(|| format!("page #{id}"));
    let actor_username = session_user.as_ref().map(|u| u.username.as_str());
    notify_workflow(
        _state.mailer.as_deref(),
        &_state.mailer_from,
        tenant.pool(),
        &tenant.org.slug,
        "cancel",
        &workflow_name,
        &page_title,
        id,
        None,
        actor_username,
        None,
        WorkflowRecipients::User(submitter_id),
    )
    .await;
    redirect_named_with_params_and_message(
        "rcms-admin:pages:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Workflow cancelled.",
        &headers,
    )
}

// =====================================================================
// Per-page audit log CSV export (#192)
// =====================================================================

/// GET /cms-admin/pages/{id}/log.csv — every audit-log entry for the
/// page, newest first, as CSV. Columns: timestamp, action, actor_id,
/// actor_username, message, data_json. The full log (no row cap)
/// emits — the editor explicitly clicked Download.
pub async fn page_log_csv(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let entries = crate::page_log::for_page(tenant.pool(), id, None).await?;
    let user_ids: std::collections::BTreeSet<i64> =
        entries.iter().filter_map(|e| e.actor_id).collect();
    let users: std::collections::HashMap<i64, String> = if user_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        rustango::tenancy::auth::User::objects()
            .where_(
                rustango::tenancy::auth::User::id.is_in(user_ids.into_iter().collect::<Vec<_>>()),
            )
            .fetch(tenant.pool())
            .await?
            .into_iter()
            .filter_map(|u| u.id.get().copied().map(|id| (id, u.username)))
            .collect()
    };

    let mut out = String::with_capacity(4096);
    out.push_str("timestamp,action,actor_id,actor_username,message,data_json\n");
    for e in entries {
        let ts = e
            .created_at
            .get()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();
        let actor_id = e.actor_id.map(|i| i.to_string()).unwrap_or_default();
        let actor_username = e
            .actor_id
            .and_then(|uid| users.get(&uid).cloned())
            .unwrap_or_default();
        out.push_str(&csv_escape(&ts));
        out.push(',');
        out.push_str(&csv_escape(&e.action));
        out.push(',');
        out.push_str(&csv_escape(&actor_id));
        out.push(',');
        out.push_str(&csv_escape(&actor_username));
        out.push(',');
        out.push_str(&csv_escape(&e.message));
        out.push(',');
        out.push_str(&csv_escape(&e.data_json));
        out.push('\n');
    }

    let filename = format!("page-{id}-audit-log.csv");
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/csv; charset=utf-8".to_owned(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        out,
    )
        .into_response())
}

// =====================================================================
// History — tenant-wide revision audit log (#17)
// =====================================================================

/// Query string for the history view. All filters optional; pagination
/// via `page` (1-indexed, defaults to 1 — `limit` is fixed at 50 to
/// keep the URL state simple).
#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub page_id: Option<i64>,
    #[serde(default)]
    pub user_id: Option<i64>,
    #[serde(default)]
    pub page: Option<u32>,
    /// Rows per page.
    #[serde(default)]
    pub per_page: Option<usize>,
}

/// GET /cms-admin/history — tenant-wide cross-page audit log built
/// from `cms_revision`. Each row joins to the page (for title +
/// current URL) and the editor (for username). One Rust-side stitch
/// pass instead of a SQL JOIN since the ORM doesn't expose joins
/// fluently yet.
pub async fn history_page(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Query(q): Query<HistoryQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Response, AdminError> {
    let page_size = crate::admin::pagination::clamp_per_page(q.per_page);

    // Build the base query with optional filters. The ORM's
    // `.where_()` is additive (AND); we apply each filter only when
    // present so the SQL stays minimal.
    let mut revisions_query =
        crate::revision::Revision::objects().order_by(&[("created_at", true)]); // desc — newest first
    if let Some(pid) = q.page_id {
        revisions_query = revisions_query.where_(crate::revision::Revision::page_id.eq(pid));
    }
    if let Some(uid) = q.user_id {
        revisions_query =
            revisions_query.where_(crate::revision::Revision::created_by.eq(Some(uid)));
    }

    // The ORM builder has no server-side limit/offset yet, so we fetch
    // all matching rows then page in memory via
    // `rustango::pagination::Paginator` (matches the snippet list).
    // `get_page()` clamps out-of-range numbers to the last page, so the
    // prev/next links never dangle. Acceptable on the 50-row scale.
    let all: Vec<crate::revision::Revision> = revisions_query.fetch(tenant.pool()).await?;
    let total = all.len();
    let paginator = rustango::pagination::Paginator::new(total, page_size);
    let page_obj = paginator.get_page(q.page.unwrap_or(1).max(1) as i64);
    let current_page = page_obj.number as u32;
    let has_next = page_obj.has_next();
    let has_prev = page_obj.has_previous();
    let revisions: Vec<crate::revision::Revision> = if total == 0 {
        Vec::new()
    } else {
        all[page_obj.start_index().saturating_sub(1)..page_obj.end_index()].to_vec()
    };

    // Batch-fetch every distinct page + user referenced by this slice.
    let page_ids: std::collections::BTreeSet<i64> = revisions.iter().map(|r| r.page_id).collect();
    let user_ids: std::collections::BTreeSet<i64> =
        revisions.iter().filter_map(|r| r.created_by).collect();

    let pages: Vec<Page> = if page_ids.is_empty() {
        Vec::new()
    } else {
        Page::objects()
            .where_(Page::id.is_in(page_ids.iter().copied()))
            .fetch(tenant.pool())
            .await?
    };
    let users: Vec<rustango::tenancy::auth::User> = if user_ids.is_empty() {
        Vec::new()
    } else {
        rustango::tenancy::auth::User::objects()
            .where_(rustango::tenancy::auth::User::id.is_in(user_ids.iter().copied()))
            .fetch(tenant.pool())
            .await?
    };

    let pages_by_id: std::collections::HashMap<i64, &Page> = pages
        .iter()
        .filter_map(|p| p.id.get().copied().map(|id| (id, p)))
        .collect();
    let users_by_id: std::collections::HashMap<i64, &rustango::tenancy::auth::User> = users
        .iter()
        .filter_map(|u| u.id.get().copied().map(|id| (id, u)))
        .collect();

    // Build a list of view-model dicts for the template — each row
    // carries everything Tera needs (no further lookups inside the
    // template loop). `is_latest` flags the most-recent revision for
    // each page so the UI can disable revert on the current state.
    let mut latest_seq_by_page: std::collections::HashMap<i64, i32> =
        std::collections::HashMap::new();
    for r in &all {
        let cur = latest_seq_by_page.entry(r.page_id).or_insert(r.sequence);
        if r.sequence > *cur {
            *cur = r.sequence;
        }
    }
    // #74 — surface the prior revision's id + sequence per row so
    // the history list's Compare button can deep-link to the diff
    // view. "Prior" = next-lower sequence on the same page. Built
    // from `all` (not the page slice) so a row at the slice boundary
    // still knows its predecessor.
    let mut by_page: std::collections::HashMap<i64, Vec<&crate::revision::Revision>> =
        std::collections::HashMap::new();
    for r in &all {
        by_page.entry(r.page_id).or_default().push(r);
    }
    let mut prev_by_id: std::collections::HashMap<i64, (i64, i32)> =
        std::collections::HashMap::new();
    for (_pid, mut list) in by_page {
        list.sort_by_key(|r| r.sequence);
        for window in list.windows(2) {
            let earlier = window[0];
            let later = window[1];
            if let (Some(later_id), Some(earlier_id)) =
                (later.id.get().copied(), earlier.id.get().copied())
            {
                prev_by_id.insert(later_id, (earlier_id, earlier.sequence));
            }
        }
    }

    let rows: Vec<serde_json::Value> = revisions
        .iter()
        .map(|r| {
            let page = pages_by_id.get(&r.page_id);
            let user = r.created_by.and_then(|uid| users_by_id.get(&uid));
            let created_at = r.created_at.get().copied();
            let (prev_id, prev_sequence) =
                r.id.get()
                    .copied()
                    .and_then(|id| prev_by_id.get(&id).copied())
                    .map(|(a, b)| (Some(a), Some(b)))
                    .unwrap_or((None, None));
            let is_latest = latest_seq_by_page
                .get(&r.page_id)
                .is_some_and(|s| *s == r.sequence);
            serde_json::json!({
                "id": r.id.get().copied(),
                "page_id": r.page_id,
                "sequence": r.sequence,
                "created_at": created_at,
                "page_title": page.map(|p| p.title.clone()),
                "page_url_path": page.map(|p| p.url_path.clone()),
                "page_status": page.map(|p| p.status.as_str()),
                "username": user.map(|u| u.username.clone()),
                "system_user": r.created_by.is_none(),
                "is_latest": is_latest,
                "prev_id": prev_id,
                "prev_sequence": prev_sequence,
            })
        })
        .collect();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "history", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("rows", &rows);
    ctx.insert("filter_page_id", &q.page_id);
    ctx.insert("filter_user_id", &q.user_id);
    // #316 — raw query string for the `| querystring(page=…)` pagination
    // links (preserves the page_id / user_id filters + sets page).
    ctx.insert("query_string", &raw_query.unwrap_or_default());
    ctx.insert("current_page", &current_page);
    ctx.insert("has_next", &has_next);
    ctx.insert("has_prev", &has_prev);
    ctx.insert("total", &total);
    ctx.insert("page_size", &page_size);
    ctx.insert(
        "pagination",
        &crate::admin::pagination::context(current_page as i64, page_size, total),
    );
    render_with_csrf(&state, &headers, "rcms_admin/history.html", &mut ctx)
}

/// Query string for the diff view: required `a` + `b` revision ids.
/// `only_changes` (default true) hides unchanged fields when the
/// template renders.
#[derive(Debug, Deserialize)]
pub struct HistoryDiffQuery {
    pub a: i64,
    pub b: i64,
    #[serde(default)]
    pub all: Option<String>,
}

/// GET /cms-admin/pages/{id}/history/diff?a=<rev_id>&b=<rev_id>
/// (#74). Side-by-side comparison of two revisions on the same
/// page. v1 surfaces a flat field-level diff; rich-text +
/// StreamField-aware diff is a follow-up.
pub async fn history_diff(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(page_id): Path<i64>,
    Query(q): Query<HistoryDiffQuery>,
) -> Result<Response, AdminError> {
    let revisions: Vec<crate::revision::Revision> = crate::revision::Revision::objects()
        .where_(crate::revision::Revision::page_id.eq(page_id))
        .where_(crate::revision::Revision::id.is_in([q.a, q.b]))
        .fetch(tenant.pool())
        .await?;
    let a_row = revisions
        .iter()
        .find(|r| r.id.get().copied() == Some(q.a))
        .cloned()
        .ok_or(AdminError::NotFound(q.a))?;
    let b_row = revisions
        .iter()
        .find(|r| r.id.get().copied() == Some(q.b))
        .cloned()
        .ok_or(AdminError::NotFound(q.b))?;

    let page = Page::objects()
        .where_(Page::id.eq(page_id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(page_id))?;

    // Reviewer name lookup for the header.
    let user_ids: Vec<i64> = [a_row.created_by, b_row.created_by]
        .into_iter()
        .flatten()
        .collect();
    let users: Vec<rustango::tenancy::auth::User> = if user_ids.is_empty() {
        Vec::new()
    } else {
        rustango::tenancy::auth::User::objects()
            .where_(rustango::tenancy::auth::User::id.is_in(user_ids))
            .fetch(tenant.pool())
            .await?
    };
    let username_for = |uid: Option<i64>| -> Option<String> {
        uid.and_then(|id| {
            users
                .iter()
                .find(|u| u.id.get().copied() == Some(id))
                .map(|u| u.username.clone())
        })
    };

    let diffs = crate::revision::diff_snapshots(&a_row.snapshot, &b_row.snapshot);
    let changed_count = diffs.iter().filter(|d| d.is_change()).count();
    let only_changes = q.all.is_none();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "history", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("page", &page);
    ctx.insert(
        "rev_a",
        &serde_json::json!({
            "id": a_row.id.get().copied(),
            "sequence": a_row.sequence,
            "created_at": a_row.created_at.get().copied(),
            "username": username_for(a_row.created_by),
        }),
    );
    ctx.insert(
        "rev_b",
        &serde_json::json!({
            "id": b_row.id.get().copied(),
            "sequence": b_row.sequence,
            "created_at": b_row.created_at.get().copied(),
            "username": username_for(b_row.created_by),
        }),
    );
    ctx.insert("diffs", &diffs);
    ctx.insert("changed_count", &changed_count);
    ctx.insert("only_changes", &only_changes);
    render_with_csrf(&state, &headers, "rcms_admin/history_diff.html", &mut ctx)
}

// =====================================================================
// Aging-pages report (#101)
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct AnalyticsQuery {
    /// Window in days back from now — defaults to 30, clamped 1..=365.
    #[serde(default)]
    pub days: Option<i64>,
}

/// GET /cms-admin/analytics — the default first-party analytics
/// dashboard: unique visitors, pageviews, bounce, avg time, top pages /
/// referrers / countries / devices, a per-day series, and "active now".
pub async fn analytics_dashboard(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Query(q): Query<AnalyticsQuery>,
) -> Result<Response, AdminError> {
    let days = q.days.unwrap_or(30).clamp(1, 365);
    let to = chrono::Utc::now();
    let from = to - chrono::Duration::days(days);
    let metrics = crate::analytics::query::compute(tenant.pool(), from, to).await?;
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let active_now = crate::analytics::presence_count(host);
    // Max pageviews across the series → the 100%-height reference for the
    // bar chart (min 1 to avoid divide-by-zero).
    let series_max = metrics
        .series
        .iter()
        .map(|p| p.pageviews)
        .max()
        .unwrap_or(0)
        .max(1);

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "analytics", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("metrics", &metrics);
    ctx.insert("active_now", &active_now);
    ctx.insert("series_max", &series_max);
    ctx.insert("days", &days);
    render_with_csrf(&state, &headers, "rcms_admin/analytics.html", &mut ctx)
}

#[derive(Debug, Deserialize)]
pub struct AgingQuery {
    /// Minimum days stale — defaults to 90.
    #[serde(default)]
    pub min_days: Option<i64>,
    /// Status filter — defaults to "published". Pass `all` to skip.
    #[serde(default)]
    pub status: Option<String>,
    /// 1-indexed page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Rows per page.
    #[serde(default)]
    pub per_page: Option<usize>,
}

/// GET /cms-admin/reports/aging — table of pages sorted by
/// `updated_at` ascending (oldest first). Editors use this to spot
/// pages that have gone stale.
pub async fn aging_pages_report(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Query(q): Query<AgingQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Response, AdminError> {
    let page_size = crate::admin::pagination::clamp_per_page(q.per_page);
    let min_days = q.min_days.unwrap_or(90).max(0);
    let status_filter = q
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "all")
        .map(str::to_owned)
        .unwrap_or_else(|| "published".to_owned());

    let now = chrono::Utc::now();
    let mut q_pages = Page::objects().order_by(&[("updated_at", false)]); // asc
    if status_filter != "all" {
        q_pages = q_pages.where_(Page::status.eq(status_filter.clone()));
    }
    let all: Vec<Page> = q_pages.fetch(tenant.pool()).await?;

    // Filter by min-days-stale.
    let stale: Vec<&Page> = all
        .iter()
        .filter(|p| match p.updated_at.get().copied() {
            Some(ts) => {
                let delta = now.signed_duration_since(ts).num_days();
                delta >= min_days
            }
            None => false,
        })
        .collect();
    let total = stale.len();
    // Page in memory via `rustango::pagination::Paginator` (matches the
    // snippet list); `get_page()` clamps out-of-range numbers to the
    // last page so prev/next links never dangle.
    let paginator = rustango::pagination::Paginator::new(total, page_size);
    let page_obj = paginator.get_page(q.page.unwrap_or(1).max(1) as i64);
    let current_page = page_obj.number as u32;
    let has_next = page_obj.has_next();
    let has_prev = page_obj.has_previous();
    let slice: Vec<&&Page> = if total == 0 {
        Vec::new()
    } else {
        stale[page_obj.start_index().saturating_sub(1)..page_obj.end_index()]
            .iter()
            .collect()
    };

    // Per-row last editor — pulled from the latest revision per
    // page in one batched query.
    let page_ids: Vec<i64> = slice.iter().filter_map(|p| p.id.get().copied()).collect();
    let revisions: Vec<crate::revision::Revision> = if page_ids.is_empty() {
        Vec::new()
    } else {
        crate::revision::Revision::objects()
            .where_(crate::revision::Revision::page_id.is_in(page_ids.iter().copied()))
            .order_by(&[("sequence", true)]) // desc, so newest sequence per page first
            .fetch(tenant.pool())
            .await
            .unwrap_or_default()
    };
    let mut last_editor_by_page: std::collections::HashMap<i64, Option<i64>> =
        std::collections::HashMap::new();
    for r in &revisions {
        // First insertion wins for each page_id since we ordered
        // desc by sequence — that's the most-recent revision.
        last_editor_by_page.entry(r.page_id).or_insert(r.created_by);
    }
    let user_ids: Vec<i64> = last_editor_by_page
        .values()
        .filter_map(|opt| *opt)
        .collect();
    let users: Vec<rustango::tenancy::auth::User> = if user_ids.is_empty() {
        Vec::new()
    } else {
        rustango::tenancy::auth::User::objects()
            .where_(rustango::tenancy::auth::User::id.is_in(user_ids))
            .fetch(tenant.pool())
            .await
            .unwrap_or_default()
    };
    let username_by_id: std::collections::HashMap<i64, String> = users
        .into_iter()
        .filter_map(|u| u.id.get().copied().map(|id| (id, u.username)))
        .collect();

    let rows: Vec<serde_json::Value> = slice
        .iter()
        .map(|p| {
            let pid = p.id.get().copied().unwrap_or_default();
            let updated = p.updated_at.get().copied();
            let days_stale = updated.map(|ts| now.signed_duration_since(ts).num_days());
            let editor = last_editor_by_page
                .get(&pid)
                .and_then(|v| *v)
                .and_then(|uid| username_by_id.get(&uid).cloned());
            serde_json::json!({
                "id": pid,
                "title": p.title,
                "url_path": p.url_path,
                "status": p.status,
                "updated_at": updated,
                "days_stale": days_stale,
                "last_editor": editor,
            })
        })
        .collect();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "reports", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("rows", &rows);
    ctx.insert("min_days", &min_days);
    ctx.insert("status_filter", &status_filter);
    // #316 — raw query string for the `| querystring(page=…)` pagination
    // links (preserves min_days / status + sets page).
    ctx.insert("query_string", &raw_query.unwrap_or_default());
    ctx.insert("current_page", &current_page);
    ctx.insert("has_next", &has_next);
    ctx.insert("has_prev", &has_prev);
    ctx.insert("total", &total);
    ctx.insert("page_size", &page_size);
    ctx.insert(
        "pagination",
        &crate::admin::pagination::context(current_page as i64, page_size, total),
    );
    render_with_csrf(&state, &headers, "rcms_admin/aging_pages.html", &mut ctx)
}

/// GET /cms-admin/reports/aging.csv — same data as the HTML view
/// but streamed as RFC 4180 CSV. Honors the same `min_days` +
/// `status` filters.
pub async fn aging_pages_csv(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Query(q): Query<AgingQuery>,
) -> Result<Response, AdminError> {
    let min_days = q.min_days.unwrap_or(90).max(0);
    let status_filter = q
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "all")
        .map(str::to_owned)
        .unwrap_or_else(|| "published".to_owned());
    let now = chrono::Utc::now();
    let mut q_pages = Page::objects().order_by(&[("updated_at", false)]);
    if status_filter != "all" {
        q_pages = q_pages.where_(Page::status.eq(status_filter.clone()));
    }
    let all: Vec<Page> = q_pages.fetch(tenant.pool()).await?;
    let stale: Vec<&Page> = all
        .iter()
        .filter(|p| match p.updated_at.get().copied() {
            Some(ts) => now.signed_duration_since(ts).num_days() >= min_days,
            None => false,
        })
        .collect();

    let mut out = String::with_capacity(256 + stale.len() * 96);
    out.push_str("page_id,title,url_path,status,updated_at,days_stale\n");
    for p in stale {
        let pid = p.id.get().copied().unwrap_or_default();
        let updated = p.updated_at.get().copied();
        let days = updated
            .map(|ts| now.signed_duration_since(ts).num_days())
            .unwrap_or_default();
        let when = updated.map(|t| t.to_rfc3339()).unwrap_or_default();
        out.push_str(&format!(
            "{},{},{},{},{},{}\n",
            pid,
            csv_escape(&p.title),
            csv_escape(&p.url_path),
            csv_escape(&p.status),
            csv_escape(&when),
            days
        ));
    }
    Ok(csv_response("aging-pages", &out))
}

/// Helper — attach CSV content-type + a date-stamped Content-Disposition.
fn csv_response(name: &str, body: &str) -> Response {
    let filename = format!("{name}-{}.csv", chrono::Utc::now().format("%Y%m%d"));
    let mut response = (
        [(header::CONTENT_TYPE, "text/csv; charset=utf-8")],
        body.to_owned(),
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, v);
    }
    response
}

// =====================================================================
// Site settings (#108)
// =====================================================================

pub async fn site_settings_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let rows = crate::site_setting::list_all(tenant.pool())
        .await
        .unwrap_or_default();
    let by_scope: std::collections::HashMap<&str, &crate::site_setting::SiteSetting> =
        rows.iter().map(|r| (r.scope.as_str(), r)).collect();
    // #406 — registered typed schemas come first and are always listed
    // (configured or not, Wagtail "settings menu" parity); then any
    // ad-hoc free-form JSON scopes that aren't backed by a schema.
    let mut view: Vec<serde_json::Value> = Vec::new();
    for schema in crate::site_setting::registered_schemas() {
        let row = by_scope.get(schema.scope).copied();
        view.push(serde_json::json!({
            "scope": schema.scope,
            "label": schema.label,
            "typed": true,
            "configured": row.is_some(),
            "preview": row
                .map(|r| serde_json::to_string(&r.value_json).unwrap_or_default())
                .unwrap_or_default(),
            "updated_at": row.and_then(|r| r.updated_at.get().copied()),
        }));
    }
    for s in &rows {
        if crate::site_setting::schema_for(&s.scope).is_some() {
            continue;
        }
        view.push(serde_json::json!({
            "scope": s.scope,
            "label": s.scope,
            "typed": false,
            "configured": true,
            "preview": serde_json::to_string(&s.value_json).unwrap_or_default(),
            "updated_at": s.updated_at.get().copied(),
        }));
    }
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "site-settings", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("rows", &view);
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/site_settings_list.html",
        &mut ctx,
    )
}

pub async fn site_setting_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(scope): Path<String>,
    Query(q): Query<EditQuery>,
) -> Result<Response, AdminError> {
    let row = crate::site_setting::get(tenant.pool(), &scope)
        .await
        .unwrap_or(None);
    let stored = row
        .as_ref()
        .map(|s| s.value_json.clone())
        .unwrap_or_else(|| serde_json::json!({}));
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "site-settings", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("scope", &scope);
    ctx.insert("is_new", &row.is_none());
    // #406 — a registered schema renders a typed form; otherwise the
    // free-form JSON textarea covers ad-hoc scopes.
    if let Some(schema) = crate::site_setting::schema_for(&scope) {
        let widgets = crate::site_setting::prefill_widgets((schema.fields)(), &stored);
        ctx.insert("typed", &true);
        ctx.insert("schema_label", schema.label);
        // Other languages: the text fields, translated side by side.
        let locales: Vec<crate::locale::Locale> = crate::locale::Locale::objects()
            .where_(crate::locale::Locale::active.eq(true))
            .order_by(&[("sort_order", false), ("code", false)])
            .fetch(tenant.pool())
            .await?;
        let editing = q
            .locale
            .as_deref()
            .and_then(|c| locales.iter().find(|l| l.code == c && !l.is_default).cloned());
        if let (Some(loc), false) = (&editing, row.is_none()) {
            let tr = crate::site_setting::translations(&stored, &loc.code);
            let rows: Vec<serde_json::Value> = widgets
                .iter()
                .filter(|w| site_setting_translatable(w))
                .map(|w| {
                    serde_json::json!({
                        "name": w.name,
                        "label": w.label,
                        "canonical": w.value,
                        "multiline": !matches!(w.kind, crate::widget::WidgetKind::Text),
                        "value": tr.get(&w.name).and_then(|v| v.as_str()).unwrap_or(""),
                    })
                })
                .collect();
            ctx.insert("translation_mode", &true);
            ctx.insert("editing_locale", loc);
            ctx.insert("tr_rows", &rows);
        }
        ctx.insert("locales", &locales);
        ctx.insert("widgets", &widgets);
    } else {
        ctx.insert("typed", &false);
        let body = serde_json::to_string_pretty(&stored).unwrap_or_else(|_| "{}".to_owned());
        ctx.insert("value_pretty", &body);
    }
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/site_setting_form.html",
        &mut ctx,
    )
}

pub async fn site_setting_edit_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(scope): Path<String>,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    // #406 — typed scopes coerce each declared field by widget kind;
    // unregistered scopes keep the raw-JSON textarea contract.
    let parsed: serde_json::Value = if let Some(schema) = crate::site_setting::schema_for(&scope) {
        let fields = (schema.fields)();
        let missing = crate::site_setting::missing_required(&fields, &form);
        if !missing.is_empty() {
            return Err(AdminError::Validation(format!(
                "These required fields are empty: {}.",
                missing.join(", ")
            )));
        }
        let mut value = crate::site_setting::value_json_from_form(&fields, &form);
        if let Ok(Some(old)) = crate::site_setting::get(tenant.pool(), &scope).await {
            crate::site_setting::keep_translations(&old.value_json, &mut value);
        }
        value
    } else {
        let raw = form.get("value_json").map_or("", |s| s.trim());
        if raw.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(raw)
                .map_err(|e| AdminError::Validation(format!("value_json isn't valid JSON: {e}")))?
        }
    };
    crate::site_setting::upsert(tenant.pool(), &scope, parsed).await?;
    redirect_named_with_message_args(
        "rcms-admin:site-settings:list",
        MsgLevel::Success,
        "Saved site setting “{scope}”.",
        &[("scope", scope.as_str())],
        &headers,
    )
}

/// A typed setting field worth translating: free text.
fn site_setting_translatable(w: &crate::widget::Widget) -> bool {
    use crate::widget::WidgetKind as K;
    matches!(w.kind, K::Text | K::Textarea | K::Markdown | K::RichText)
}

/// POST /cms-admin/site-settings/{scope}/translate?locale=<code> — store
/// the text fields' translations in the setting's `_i18n` bag.
pub async fn site_setting_translate_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(scope): Path<String>,
    Query(q): Query<EditQuery>,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let locale = translate_locale(&tenant, q.locale.as_deref()).await?;
    let schema = crate::site_setting::schema_for(&scope)
        .ok_or_else(|| AdminError::Validation(format!("`{scope}` has no typed fields to translate")))?;
    let row = crate::site_setting::get(tenant.pool(), &scope)
        .await?
        .ok_or_else(|| AdminError::Validation(format!("save `{scope}` before translating it")))?;
    let fields: serde_json::Map<String, serde_json::Value> = (schema.fields)()
        .iter()
        .filter(|w| site_setting_translatable(w))
        .filter_map(|w| {
            form.get(&format!("tr__{}", w.name))
                .map(|v| (w.name.clone(), serde_json::Value::String(v.trim().to_owned())))
        })
        .collect();
    let mut value = row.value_json;
    crate::site_setting::set_translations(&mut value, &locale.code, fields);
    crate::site_setting::upsert(tenant.pool(), &scope, value).await?;
    Ok(rustango::messages::redirect_with_message(
        messages_secret(),
        &headers,
        MsgLevel::Success,
        &super::i18n::tr(&headers, "Translations saved.", &[]),
        &format!("/cms-admin/site-settings/{scope}/edit?locale={}", locale.code),
    ))
}

pub async fn site_setting_delete_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(scope): Path<String>,
) -> Result<Response, AdminError> {
    if let Some(row) = crate::site_setting::get(tenant.pool(), &scope)
        .await
        .unwrap_or(None)
    {
        row.delete_pool(tenant.pool()).await?;
    }
    redirect_named_with_message_args(
        "rcms-admin:site-settings:list",
        MsgLevel::Success,
        "Deleted site setting “{scope}”.",
        &[("scope", scope.as_str())],
        &headers,
    )
}

// =====================================================================
// Locked-pages report (#109)
// =====================================================================

// ----- #143 — search promotions report (Wagtail parity D6) ---------

/// GET /cms-admin/reports/search — top queries by hit count with
/// counts of pinned promotions per query.
pub async fn search_promotions_report(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let top = crate::search_promotion::top_queries(tenant.pool(), 100)
        .await
        .unwrap_or_default();
    // Annotate each query with its promotion count.
    let mut rows: Vec<serde_json::Value> = Vec::with_capacity(top.len());
    for q in top {
        let qid = q.id.get().copied().unwrap_or_default();
        let promos = crate::search_promotion::promotions_for_query(tenant.pool(), qid)
            .await
            .unwrap_or_default();
        rows.push(serde_json::json!({
            "id": qid,
            "raw": q.raw,
            "normalized": q.normalized,
            "hits": q.hits,
            "last_seen_at": q.last_seen_at.get().copied(),
            "promotion_count": promos.len(),
        }));
    }
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "reports-search", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("rows", &rows);
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/search_promotions_report.html",
        &mut ctx,
    )
}

/// GET /cms-admin/reports/search/{id}/edit — edit pinned pages for
/// one query.
pub async fn search_promotion_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(query_id): Path<i64>,
) -> Result<Response, AdminError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let queries: Vec<crate::search_promotion::SearchQuery> =
        crate::search_promotion::SearchQuery::objects()
            .where_(crate::search_promotion::SearchQuery::id.eq(query_id))
            .fetch(tenant.pool())
            .await?;
    let Some(query) = queries.into_iter().next() else {
        return Err(AdminError::NotFound(query_id));
    };
    let promotions = crate::search_promotion::promotions_for_query(tenant.pool(), query_id)
        .await
        .unwrap_or_default();
    let pages: Vec<Page> = Page::objects()
        .where_(Page::status.eq("published".to_owned()))
        .order_by(&[("title", false)])
        .fetch(tenant.pool())
        .await
        .unwrap_or_default();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "reports-search", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("query", &query);
    ctx.insert("promotions", &promotions);
    ctx.insert("pages", &pages);
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/search_promotion_edit.html",
        &mut ctx,
    )
}

#[derive(Debug, Deserialize)]
pub struct SearchPromotionForm {
    /// Comma-separated page ids in pin order.
    #[serde(default)]
    pub page_ids: String,
    #[serde(default)]
    pub descriptions: String,
}

/// POST /cms-admin/reports/search/{id}/edit — replace the pinned
/// list with the posted ordered page ids.
pub async fn search_promotion_edit_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(query_id): Path<i64>,
    Form(form): Form<SearchPromotionForm>,
) -> Result<Response, AdminError> {
    use rustango::core::Column as _;
    use rustango::sql::Auto;
    use rustango::sql::FetcherPool as _;
    // Drop existing promotions then re-insert fresh — simpler than
    // diffing.
    let existing: Vec<crate::search_promotion::SearchPromotion> =
        crate::search_promotion::SearchPromotion::objects()
            .where_(crate::search_promotion::SearchPromotion::query_id.eq(query_id))
            .fetch(tenant.pool())
            .await?;
    for row in existing {
        row.delete_pool(tenant.pool()).await.log_warn("old search promotion not deleted");
    }
    let ids: Vec<i64> = form
        .page_ids
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<i64>().ok())
        .collect();
    let descriptions: Vec<String> = form
        .descriptions
        .split('\n')
        .map(|s| s.trim().to_owned())
        .collect();
    for (idx, page_id) in ids.iter().enumerate() {
        let mut row = crate::search_promotion::SearchPromotion {
            id: Auto::Unset,
            query_id,
            page_id: *page_id,
            sort_order: idx as i32,
            description: descriptions.get(idx).cloned().unwrap_or_default(),
            created_at: Auto::Unset,
        };
        row.insert_pool(tenant.pool()).await.log_warn("search promotion not saved");
    }
    redirect_named_with_message_plural(
        "rcms-admin:reports:search",
        MsgLevel::Success,
        "Saved {count} pinned result(s).",
        ids.len() as i64,
        &[("count", &ids.len().to_string())],
        &headers,
    )
}

// ----- #141 — scheduled-publish report (Wagtail parity D7) ---------

/// #264 — derive the effective lifecycle state from `go_live_at`.
/// Mirrors Wagtail's `_compute_state()`: any row carrying a future
/// `go_live_at` is treated as `scheduled` regardless of the editor's
/// literal status pick (draft / published) so the sweep job +
/// scheduled-report query pick it up. Clearing `go_live_at` on a
/// previously-scheduled row falls back to `draft` so the row stops
/// claiming a schedule it no longer has.
///
/// `archived` is preserved either way — archive is an explicit
/// lifecycle terminus and an editor un-archives via a different
/// path. A scheduled row whose `go_live_at` has already passed is
/// left alone too; the sweep is the one that flips it to `published`,
/// and stepping in here would steal that transition.
pub(crate) fn derive_scheduled_status(
    current: &str,
    go_live_at: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    let archived = crate::page::PageStatus::Archived.as_str();
    if current == archived {
        return current.to_owned();
    }
    let now = chrono::Utc::now();
    let future = go_live_at.is_some_and(|t| t > now);
    let scheduled = crate::page::PageStatus::Scheduled.as_str();
    let draft = crate::page::PageStatus::Draft.as_str();
    match (current, future, go_live_at) {
        (_, true, _) => scheduled.to_owned(),
        (s, false, None) if s == scheduled => draft.to_owned(),
        _ => current.to_owned(),
    }
}

#[cfg(test)]
mod scheduled_status_tests {
    use super::derive_scheduled_status;
    use chrono::{Duration, Utc};

    #[test]
    fn future_go_live_promotes_draft_to_scheduled() {
        let go = Utc::now() + Duration::hours(1);
        assert_eq!(derive_scheduled_status("draft", Some(go)), "scheduled");
    }

    #[test]
    fn future_go_live_also_promotes_published() {
        // Editor picked "published" + a future go-live. Per Wagtail
        // parity the row defers to the sweep — surface as scheduled.
        let go = Utc::now() + Duration::hours(2);
        assert_eq!(derive_scheduled_status("published", Some(go)), "scheduled");
    }

    #[test]
    fn cleared_go_live_demotes_scheduled_to_draft() {
        assert_eq!(derive_scheduled_status("scheduled", None), "draft");
    }

    #[test]
    fn past_go_live_on_scheduled_left_alone_for_sweep() {
        let past = Utc::now() - Duration::hours(1);
        assert_eq!(
            derive_scheduled_status("scheduled", Some(past)),
            "scheduled",
        );
    }

    #[test]
    fn archived_preserved_regardless_of_go_live() {
        let go = Utc::now() + Duration::hours(1);
        assert_eq!(derive_scheduled_status("archived", Some(go)), "archived");
        assert_eq!(derive_scheduled_status("archived", None), "archived");
    }

    #[test]
    fn draft_with_no_go_live_unchanged() {
        assert_eq!(derive_scheduled_status("draft", None), "draft");
    }

    #[test]
    fn published_with_no_go_live_unchanged() {
        assert_eq!(derive_scheduled_status("published", None), "published");
    }
}

/// Build the dataset for the scheduled-publish report: every page
/// with a future `go_live_at` plus every published page with a
/// future `expire_at`. Rows sorted by go_live_at ascending (soonest
/// first), with expirations interleaved by their expire_at.
async fn scheduled_pages_rows(
    pool: &rustango::sql::Pool,
) -> Result<Vec<serde_json::Value>, rustango::sql::ExecError> {
    use rustango::sql::FetcherPool as _;
    let pages: Vec<Page> = Page::objects().fetch(pool).await?;
    let now = chrono::Utc::now();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for p in &pages {
        let pid = p.id.get().copied().unwrap_or_default();
        if let Some(go) = p.go_live_at {
            // #264 — surface any page with a future `go_live_at`,
            // even if its status hasn't been coerced to "scheduled"
            // yet (the save handler now does the coercion, but legacy
            // rows or out-of-band edits may have `go_live_at` set on
            // a draft/published row — the editor still wants to see
            // the upcoming flip). Archived / expired rows are excluded
            // because both are explicit lifecycle termini.
            if go > now
                && p.status != crate::page::PageStatus::Archived.as_str()
                && p.status != crate::page::PageStatus::Expired.as_str()
            {
                let countdown = (go - now).num_seconds();
                rows.push(serde_json::json!({
                    "kind": "publish",
                    "when": go,
                    "countdown_secs": countdown,
                    "page_id": pid,
                    "title": p.title,
                    "url_path": p.url_path,
                    "status": p.status,
                }));
            }
        }
        if let Some(exp) = p.expire_at {
            // A scheduled page with both dates comes down too: list it.
            if exp > now
                && (p.status == crate::page::PageStatus::Published.as_str()
                    || p.status == crate::page::PageStatus::Scheduled.as_str())
            {
                let countdown = (exp - now).num_seconds();
                rows.push(serde_json::json!({
                    "kind": "expire",
                    "when": exp,
                    "countdown_secs": countdown,
                    "page_id": pid,
                    "title": p.title,
                    "url_path": p.url_path,
                    "status": p.status,
                }));
            }
        }
    }
    rows.sort_by_key(|r| {
        r.get("countdown_secs")
            .and_then(|v| v.as_i64())
            .unwrap_or(i64::MAX)
    });
    Ok(rows)
}

pub async fn scheduled_report(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let rows = scheduled_pages_rows(tenant.pool())
        .await
        .unwrap_or_default();
    let mut ctx = Context::new();
    add_chrome(
        &mut ctx,
        &tenant,
        "reports-scheduled",
        session_user.as_ref(),
    )
    .await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("rows", &rows);
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/scheduled_report.html",
        &mut ctx,
    )
}

pub async fn scheduled_report_csv(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let rows = scheduled_pages_rows(tenant.pool())
        .await
        .unwrap_or_default();
    let mut out = String::with_capacity(256 + rows.len() * 160);
    out.push_str("kind,page_id,title,url_path,status,when,countdown_secs\n");
    for r in &rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            csv_escape(r.get("kind").and_then(|v| v.as_str()).unwrap_or_default()),
            r.get("page_id")
                .and_then(|v| v.as_i64())
                .unwrap_or_default(),
            csv_escape(r.get("title").and_then(|v| v.as_str()).unwrap_or_default()),
            csv_escape(
                r.get("url_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
            csv_escape(r.get("status").and_then(|v| v.as_str()).unwrap_or_default()),
            csv_escape(&r.get("when").map(ToString::to_string).unwrap_or_default()),
            r.get("countdown_secs")
                .and_then(|v| v.as_i64())
                .unwrap_or_default(),
        ));
    }
    Ok(csv_response("scheduled-pages", &out))
}

/// Build the locked-pages dataset: every live PageLock joined with
/// its page + holder username. Returns rows sorted by acquired_at
/// desc (newest holds first). Excludes stale locks past their TTL.
async fn locked_pages_rows(
    pool: &rustango::sql::Pool,
) -> Result<Vec<serde_json::Value>, rustango::sql::ExecError> {
    use rustango::sql::FetcherPool as _;
    let locks: Vec<crate::lock::PageLock> = crate::lock::PageLock::objects()
        .order_by(&[("acquired_at", true)]) // desc
        .fetch(pool)
        .await?;
    let now = chrono::Utc::now();
    let live: Vec<&crate::lock::PageLock> = locks.iter().filter(|l| !l.is_stale(now)).collect();
    if live.is_empty() {
        return Ok(Vec::new());
    }
    let page_ids: Vec<i64> = live.iter().map(|l| l.page_id).collect();
    let user_ids: Vec<i64> = live.iter().map(|l| l.user_id).collect();
    let pages: Vec<Page> = Page::objects()
        .where_(Page::id.is_in(page_ids))
        .fetch(pool)
        .await
        .unwrap_or_default();
    let users: Vec<rustango::tenancy::auth::User> = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.is_in(user_ids))
        .fetch(pool)
        .await
        .unwrap_or_default();
    let page_by_id: std::collections::HashMap<i64, &Page> = pages
        .iter()
        .filter_map(|p| p.id.get().copied().map(|id| (id, p)))
        .collect();
    let user_by_id: std::collections::HashMap<i64, String> = users
        .into_iter()
        .filter_map(|u| u.id.get().copied().map(|id| (id, u.username)))
        .collect();
    Ok(live
        .into_iter()
        .map(|l| {
            let title = page_by_id
                .get(&l.page_id)
                .map(|p| p.title.clone())
                .unwrap_or_else(|| format!("page #{}", l.page_id));
            let url_path = page_by_id
                .get(&l.page_id)
                .map(|p| p.url_path.clone())
                .unwrap_or_default();
            let username = user_by_id
                .get(&l.user_id)
                .cloned()
                .unwrap_or_else(|| format!("user#{}", l.user_id));
            let ttl_remaining = (l.expires_at - now).num_seconds();
            serde_json::json!({
                "page_id": l.page_id,
                "title": title,
                "url_path": url_path,
                "user_id": l.user_id,
                "username": username,
                "acquired_at": l.acquired_at.get().copied(),
                "expires_at": l.expires_at,
                "ttl_remaining_secs": ttl_remaining,
            })
        })
        .collect())
}

pub async fn locked_pages_report(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let rows = locked_pages_rows(tenant.pool()).await.unwrap_or_default();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "reports-locked", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("rows", &rows);
    render_with_csrf(&state, &headers, "rcms_admin/locked_pages.html", &mut ctx)
}

pub async fn locked_pages_csv(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let rows = locked_pages_rows(tenant.pool()).await.unwrap_or_default();
    let mut out = String::with_capacity(256 + rows.len() * 128);
    out.push_str("page_id,title,url_path,username,acquired_at,expires_at\n");
    for r in &rows {
        out.push_str(&format!(
            "{},{},{},{},{},{}\n",
            r.get("page_id")
                .and_then(|v| v.as_i64())
                .unwrap_or_default(),
            csv_escape(r.get("title").and_then(|v| v.as_str()).unwrap_or_default()),
            csv_escape(
                r.get("url_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
            csv_escape(
                r.get("username")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
            csv_escape(
                r.get("acquired_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
            csv_escape(
                r.get("expires_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
        ));
    }
    Ok(csv_response("locked-pages", &out))
}

// =====================================================================
// Workflows report (#109)
// =====================================================================

async fn workflows_report_rows(
    pool: &rustango::sql::Pool,
) -> Result<Vec<serde_json::Value>, rustango::sql::ExecError> {
    use rustango::sql::FetcherPool as _;
    let states: Vec<crate::workflow::WorkflowState> = crate::workflow::WorkflowState::objects()
        .order_by(&[("requested_at", true)]) // desc
        .fetch(pool)
        .await?;
    if states.is_empty() {
        return Ok(Vec::new());
    }
    let workflow_ids: Vec<i64> = states.iter().map(|s| s.workflow_id).collect();
    let page_ids: Vec<i64> = states.iter().map(|s| s.page_id).collect();
    let user_ids: Vec<i64> = states.iter().map(|s| s.requested_by).collect();
    let workflows: Vec<crate::workflow::Workflow> = crate::workflow::Workflow::objects()
        .where_(crate::workflow::Workflow::id.is_in(workflow_ids))
        .fetch(pool)
        .await
        .unwrap_or_default();
    let pages: Vec<Page> = Page::objects()
        .where_(Page::id.is_in(page_ids))
        .fetch(pool)
        .await
        .unwrap_or_default();
    let users: Vec<rustango::tenancy::auth::User> = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.is_in(user_ids))
        .fetch(pool)
        .await
        .unwrap_or_default();
    let task_ids: Vec<i64> = states.iter().filter_map(|s| s.current_task_id).collect();
    let tasks: Vec<crate::workflow::WorkflowTask> = if task_ids.is_empty() {
        Vec::new()
    } else {
        crate::workflow::WorkflowTask::objects()
            .where_(crate::workflow::WorkflowTask::id.is_in(task_ids))
            .fetch(pool)
            .await
            .unwrap_or_default()
    };
    let workflow_by_id: std::collections::HashMap<i64, String> = workflows
        .into_iter()
        .filter_map(|w| w.id.get().copied().map(|id| (id, w.name)))
        .collect();
    let page_by_id: std::collections::HashMap<i64, &Page> = pages
        .iter()
        .filter_map(|p| p.id.get().copied().map(|id| (id, p)))
        .collect();
    let user_by_id: std::collections::HashMap<i64, String> = users
        .into_iter()
        .filter_map(|u| u.id.get().copied().map(|id| (id, u.username)))
        .collect();
    let task_by_id: std::collections::HashMap<i64, String> = tasks
        .into_iter()
        .filter_map(|t| t.id.get().copied().map(|id| (id, t.name)))
        .collect();
    Ok(states
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id.get().copied(),
                "workflow_name": workflow_by_id.get(&s.workflow_id).cloned().unwrap_or_else(|| format!("workflow #{}", s.workflow_id)),
                "page_id": s.page_id,
                "page_title": page_by_id.get(&s.page_id).map(|p| p.title.clone()).unwrap_or_else(|| format!("page #{}", s.page_id)),
                "page_url_path": page_by_id.get(&s.page_id).map(|p| p.url_path.clone()).unwrap_or_default(),
                "status": s.status,
                "current_task_name": s.current_task_id.and_then(|tid| task_by_id.get(&tid).cloned()),
                "requested_by_username": user_by_id.get(&s.requested_by).cloned(),
                "requested_at": s.requested_at.get().copied(),
                "finished_at": s.finished_at,
            })
        })
        .collect())
}

pub async fn workflows_report(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let rows = workflows_report_rows(tenant.pool())
        .await
        .unwrap_or_default();
    let mut ctx = Context::new();
    add_chrome(
        &mut ctx,
        &tenant,
        "reports-workflows",
        session_user.as_ref(),
    )
    .await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("rows", &rows);
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/workflow_report.html",
        &mut ctx,
    )
}

// =====================================================================
// #122 — revision storage report + manual prune.
// =====================================================================

/// GET /cms-admin/reports/revisions — list pages ordered by revision
/// count so editors can see which ones accumulate history fastest.
/// Surface a "Prune now" form scoped to the whole tenant.
pub async fn revisions_report(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    use rustango::sql::FetcherPool as _;
    let pages: Vec<Page> = Page::objects().fetch(tenant.pool()).await?;
    let revisions: Vec<crate::revision::Revision> = crate::revision::Revision::objects()
        .fetch(tenant.pool())
        .await
        .unwrap_or_default();
    let mut count_by_page: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for r in &revisions {
        *count_by_page.entry(r.page_id).or_default() += 1;
    }
    let mut rows: Vec<serde_json::Value> = pages
        .into_iter()
        .filter_map(|p| {
            let pid = p.id.get().copied()?;
            let count = count_by_page.get(&pid).copied().unwrap_or(0);
            if count == 0 {
                return None;
            }
            Some(serde_json::json!({
                "page_id": pid,
                "title": p.title,
                "url_path": p.url_path,
                "status": p.status,
                "revision_count": count,
            }))
        })
        .collect();
    rows.sort_by(|a, b| {
        let ac = a
            .get("revision_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let bc = b
            .get("revision_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        bc.cmp(&ac)
    });
    let total_revisions = revisions.len();
    let mut ctx = Context::new();
    add_chrome(
        &mut ctx,
        &tenant,
        "reports-revisions",
        session_user.as_ref(),
    )
    .await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("rows", &rows);
    ctx.insert("total_revisions", &total_revisions);
    ctx.insert("default_keep_latest", &crate::revision::DEFAULT_KEEP_LATEST);
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/revisions_report.html",
        &mut ctx,
    )
}

/// Form posted from the revisions report's "Prune now" button.
#[derive(Debug, Deserialize)]
pub struct RevisionsPruneForm {
    #[serde(default)]
    pub keep_latest: Option<usize>,
    #[serde(default)]
    pub include_published: Option<String>,
}

/// POST /cms-admin/reports/revisions/prune — manual prune. Honours
/// the same `(keep_latest, keep_published)` knobs as the library
/// helper. Redirects back to the report with a count flash.
pub async fn revisions_report_prune(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Form(form): Form<RevisionsPruneForm>,
) -> Result<Response, AdminError> {
    let keep_latest = form
        .keep_latest
        .unwrap_or(crate::revision::DEFAULT_KEEP_LATEST)
        .max(1);
    // The form check sends "1" when the box is ticked → keep
    // published-status revisions. Unchecked = include them in the
    // sweep (more aggressive).
    let keep_published = form.include_published.as_deref() != Some("1");
    let (pages_scanned, rows_deleted) =
        crate::revision::prune_all_pages(tenant.pool(), keep_latest, keep_published)
            .await
            .unwrap_or((0, 0));
    let revisions = super::i18n::tr_plural(
        &headers,
        "{count} revision(s)",
        rows_deleted as i64,
        &[("count", &rows_deleted.to_string())],
    );
    let pages_ph = super::i18n::tr_plural(
        &headers,
        "{count} page(s)",
        pages_scanned as i64,
        &[("count", &pages_scanned.to_string())],
    );
    let kl = keep_latest.to_string();
    let kp = keep_published.to_string();
    redirect_named_with_message_args(
        "rcms-admin:reports:revisions",
        MsgLevel::Success,
        "Pruned {revisions} across {pages} (keep_latest={keep_latest}, keep_published={keep_published}).",
        &[
            ("revisions", revisions.as_str()),
            ("pages", pages_ph.as_str()),
            ("keep_latest", kl.as_str()),
            ("keep_published", kp.as_str()),
        ],
        &headers,
    )
}

pub async fn workflows_report_csv(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let rows = workflows_report_rows(tenant.pool())
        .await
        .unwrap_or_default();
    let mut out = String::with_capacity(256 + rows.len() * 192);
    out.push_str("workflow,page_id,page_title,page_url,status,current_task,requested_by,requested_at,finished_at\n");
    for r in &rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{}\n",
            csv_escape(
                r.get("workflow_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
            r.get("page_id")
                .and_then(|v| v.as_i64())
                .unwrap_or_default(),
            csv_escape(
                r.get("page_title")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
            csv_escape(
                r.get("page_url_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
            csv_escape(r.get("status").and_then(|v| v.as_str()).unwrap_or_default()),
            csv_escape(
                r.get("current_task_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
            csv_escape(
                r.get("requested_by_username")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
            csv_escape(
                r.get("requested_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
            csv_escape(
                r.get("finished_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
        ));
    }
    Ok(csv_response("workflow-report", &out))
}

// =====================================================================
// Global admin search (#77 — stage 0)
// =====================================================================

/// Query for the global search. `q` is the search needle; when
/// `autocomplete=1` is set the handler returns a JSON shape suited
/// to the topbar dropdown (no surrounding chrome).
#[derive(Debug, Deserialize)]
pub struct AdminSearchQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub autocomplete: Option<String>,
}

/// Query string for `GET /cms-admin/__page-chooser`.
#[derive(Debug, Deserialize)]
pub struct PageChooserQuery {
    #[serde(default)]
    pub q: Option<String>,
}

/// Query string for the generic `GET /cms-admin/__chooser/{slug}` (#421).
#[derive(Debug, Default, Deserialize)]
pub struct ChooserSearchQuery {
    #[serde(default)]
    pub q: Option<String>,
}

/// GET `/cms-admin/__chooser/{slug}?q=<needle>` (#421) — JSON endpoint
/// backing any `register_chooser!`ed model, generalising the three
/// bespoke choosers. Returns `{ "items": [ { id, title, sub, … } ] }`,
/// the shape the shared overlay (`cms-ux.js`) renders. Unknown slug → 404.
pub async fn chooser_search_json(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Path(slug): Path<String>,
    Query(q): Query<ChooserSearchQuery>,
) -> Result<Response, AdminError> {
    let Some(cvs) = super::chooser::find(&slug) else {
        return Ok((axum::http::StatusCode::NOT_FOUND, "unknown chooser").into_response());
    };
    let items = (cvs.search_rows)(tenant.pool().clone(), q.q.unwrap_or_default()).await;
    let body = serde_json::json!({ "items": items });
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_owned()),
    )
        .into_response())
}

/// Query string for `GET /cms-admin/__media-picker`.
#[derive(Debug, Default, Deserialize)]
pub struct MediaPickerQuery {
    #[serde(default)]
    pub q: Option<String>,
    /// Collection filter: omitted = all collections, `0` = uncategorized
    /// (NULL collection_id), any other value = that exact collection.
    #[serde(default)]
    pub collection: Option<i64>,
    /// `"image"` (default) | `"document"` | `"other"` | `"any"`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Comma-separated media ids — resolve exactly these rows (any kind
    /// unless `kind` is set explicitly). Used by the widget-label
    /// hydration pass to turn a server-rendered `#42` into the title.
    #[serde(default)]
    pub ids: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// GET `/cms-admin/__media-picker` — the media picker's data endpoint.
///
/// Richer than the generic `/cms-admin/__chooser/media` (which stays for
/// back-compat): collection + kind filters, offset pagination, and
/// server-computed `thumb_url`/`preview_url` rendition URLs. The URLs are
/// built here (not in JS) because renditions can be signature-gated (#425)
/// and only the server can sign a spec.
///
/// Filtering is in-Rust over a full fetch — the crate's tri-dialect
/// convention (see `render_media_list`); no SQL LIKE.
pub async fn media_picker_json(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Query(q): Query<MediaPickerQuery>,
) -> Result<Response, AdminError> {
    let body = media_picker_payload(tenant.pool(), &q).await?;
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_owned()),
    )
        .into_response())
}

/// Core of [`media_picker_json`], split out so unit tests can drive it with
/// an in-memory pool (the handler's `Tenant`/`State` extractors need a full
/// admin app).
pub(crate) async fn media_picker_payload(
    pool: &rustango::sql::Pool,
    q: &MediaPickerQuery,
) -> Result<serde_json::Value, AdminError> {
    let mut rows: Vec<Media> = Media::objects()
        .order_by(&[("uploaded_at", true)])
        .fetch(pool)
        .await?;

    // Explicit-id lookup (label hydration): keep exactly those rows, and
    // default the kind filter OFF — a video/document id must resolve too.
    let id_set: Option<std::collections::HashSet<i64>> = q.ids.as_deref().map(|s| {
        s.split(',')
            .filter_map(|t| t.trim().parse::<i64>().ok())
            .collect()
    });
    if let Some(ids) = &id_set {
        rows.retain(|m| m.id.get().copied().is_some_and(|id| ids.contains(&id)));
    }
    // Kind filter — the picker is image-first ("Choose an image…");
    // documents have their own chooser kind. `any` opts out.
    let kind = q
        .kind
        .as_deref()
        .unwrap_or(if id_set.is_some() { "any" } else { "image" });
    if kind != "any" {
        rows.retain(|m| m.kind == kind);
    }
    // Per-collection counts for the dropdown, computed over the kind-filtered
    // set (before collection/search narrow it) so the numbers answer
    // "how many would this filter show?".
    let mut collection_counts: std::collections::HashMap<i64, i64> =
        std::collections::HashMap::new();
    let mut uncategorized_count: i64 = 0;
    for m in &rows {
        match m.collection_id {
            Some(cid) => *collection_counts.entry(cid).or_default() += 1,
            None => uncategorized_count += 1,
        }
    }
    match q.collection {
        Some(0) => rows.retain(|m| m.collection_id.is_none()),
        Some(cid) => rows.retain(|m| m.collection_id == Some(cid)),
        None => {}
    }
    if let Some(needle) = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let needle = needle.to_lowercase();
        rows.retain(|m| {
            m.title.to_lowercase().contains(&needle)
                || m.filename.to_lowercase().contains(&needle)
                || m.alt_text.to_lowercase().contains(&needle)
                || m.description.to_lowercase().contains(&needle)
        });
    }

    let total = rows.len();
    let limit = q.limit.unwrap_or(60).min(200);
    let offset = q.offset.unwrap_or(0).min(total);
    let has_more = offset + limit < total;
    let page: Vec<serde_json::Value> = rows[offset..(offset + limit).min(total)]
        .iter()
        .map(|m| {
            let id = m.id.get().copied().unwrap_or_default();
            let is_image = m.kind == "image";
            let thumb_url = is_image
                .then(|| {
                    crate::rendition::rendition_url_for(
                        id,
                        "fill-320x320|format-webp|quality-70",
                        Some(m.content_hash.as_str()),
                    )
                })
                .flatten();
            let preview_url = is_image
                .then(|| {
                    crate::rendition::rendition_url_for(
                        id,
                        "max-640x480|format-webp|quality-80",
                        Some(m.content_hash.as_str()),
                    )
                })
                .flatten();
            serde_json::json!({
                "id": id,
                "title": if m.title.is_empty() { m.filename.clone() } else { m.title.clone() },
                "filename": m.filename,
                "kind": m.kind,
                "mime": m.mime,
                "size": m.size,
                "width": m.width,
                "height": m.height,
                "collection_id": m.collection_id,
                "alt_text": m.alt_text,
                "uploaded_at": m.uploaded_at,
                "thumb_url": thumb_url,
                "preview_url": preview_url,
            })
        })
        .collect();

    // Collections in DFS order so the dropdown nests visually
    // ("— Child" under its parent). A visited set guards against
    // accidental parent cycles.
    let cols: Vec<crate::media::MediaCollection> =
        crate::media::MediaCollection::objects().fetch(pool).await?;
    let mut children: std::collections::HashMap<Option<i64>, Vec<&crate::media::MediaCollection>> =
        std::collections::HashMap::new();
    for c in &cols {
        children.entry(c.parent_id).or_default().push(c);
    }
    for v in children.values_mut() {
        v.sort_by(|a, b| a.sort_order.cmp(&b.sort_order).then(a.name.cmp(&b.name)));
    }
    let mut ordered: Vec<serde_json::Value> = Vec::with_capacity(cols.len());
    let mut visited: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut stack: Vec<(i64, usize)> = children
        .get(&None)
        .map(|roots| {
            roots
                .iter()
                .rev()
                .filter_map(|c| c.id.get().copied())
                .map(|id| (id, 0))
                .collect()
        })
        .unwrap_or_default();
    let by_id: std::collections::HashMap<i64, &crate::media::MediaCollection> = cols
        .iter()
        .filter_map(|c| c.id.get().copied().map(|id| (id, c)))
        .collect();
    while let Some((id, depth)) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        if let Some(c) = by_id.get(&id) {
            ordered.push(serde_json::json!({
                "id": id,
                "name": c.name,
                "depth": depth,
                "count": collection_counts.get(&id).copied().unwrap_or(0),
            }));
            if let Some(kids) = children.get(&Some(id)) {
                for k in kids.iter().rev() {
                    if let Some(kid) = k.id.get().copied() {
                        stack.push((kid, depth + 1));
                    }
                }
            }
        }
    }

    Ok(serde_json::json!({
        "items": page,
        "collections": ordered,
        "uncategorized_count": uncategorized_count,
        "total": total,
        "has_more": has_more,
    }))
}

/// GET /cms-admin/__page-chooser?q=<needle> — JSON endpoint backing
/// the Wagtail-parity PageChooser widget. Returns the top-N matching
/// pages by title / slug / url_path (LIKE, lowercased), bounded at 50.
/// Empty `q` returns the most recently updated pages so the chooser
/// has something useful to show on first open.
pub async fn page_chooser_json(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Query(q): Query<PageChooserQuery>,
) -> Result<Response, AdminError> {
    let needle =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);

    let mut pages: Vec<Page> = Page::objects()
        .order_by(&[("updated_at", true)])
        .fetch(tenant.pool())
        .await
        .unwrap_or_default();

    if let Some(n) = needle.as_ref() {
        pages.retain(|p| {
            p.title.to_lowercase().contains(n)
                || p.slug.to_lowercase().contains(n)
                || p.url_path.to_lowercase().contains(n)
        });
    }

    let items: Vec<serde_json::Value> = pages
        .iter()
        .take(50)
        .map(|p| {
            serde_json::json!({
                "id": p.id.get().copied(),
                "title": p.title,
                "slug": p.slug,
                "url_path": p.url_path,
                "depth": p.depth,
                "parent_id": p.parent_id,
                "status": p.status,
            })
        })
        .collect();

    let body = serde_json::json!({ "items": items });
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_owned()),
    )
        .into_response())
}

/// Query string for `GET /cms-admin/__snippet-chooser`. Optional
/// `type_name` narrows to one snippet kind — the `data-chooser-filter`
/// attribute on the widget threads through.
#[derive(Debug, Deserialize)]
pub struct SnippetChooserQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub type_name: Option<String>,
    /// Comma-separated snippet ids — resolve exactly these rows. Used by
    /// the widget-label hydration pass (a server-rendered value only has
    /// the raw id).
    #[serde(default)]
    pub ids: Option<String>,
}

/// GET /cms-admin/__snippet-chooser?q=…&type_name=… — JSON endpoint
/// backing the Wagtail-parity SnippetChooser widget. Returns the
/// top-50 matching snippets by title / slug; optionally filtered
/// to a single `type_name`.
pub async fn snippet_chooser_json(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Query(q): Query<SnippetChooserQuery>,
) -> Result<Response, AdminError> {
    let needle =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);
    let type_filter = q
        .type_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let mut snippets: Vec<crate::snippet::Snippet> = crate::snippet::Snippet::objects()
        .order_by(&[("updated_at", true)])
        .fetch(tenant.pool())
        .await
        .unwrap_or_default();

    if let Some(ids) = q.ids.as_deref() {
        let id_set: std::collections::HashSet<i64> = ids
            .split(',')
            .filter_map(|t| t.trim().parse::<i64>().ok())
            .collect();
        snippets.retain(|s| s.id.get().copied().is_some_and(|id| id_set.contains(&id)));
    }
    if let Some(tn) = type_filter.as_ref() {
        snippets.retain(|s| &s.type_name == tn);
    }
    if let Some(n) = needle.as_ref() {
        snippets
            .retain(|s| s.title.to_lowercase().contains(n) || s.slug.to_lowercase().contains(n));
    }

    let items: Vec<serde_json::Value> = snippets
        .iter()
        .take(50)
        .map(|s| {
            serde_json::json!({
                "id": s.id.get().copied(),
                "title": s.title,
                "slug": s.slug,
                "type_name": s.type_name,
            })
        })
        .collect();

    let body = serde_json::json!({ "items": items });
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_owned()),
    )
        .into_response())
}

/// Query string for `GET /cms-admin/__document-chooser`.
#[derive(Debug, Deserialize)]
pub struct DocumentChooserQuery {
    #[serde(default)]
    pub q: Option<String>,
}

/// GET /cms-admin/__document-chooser?q=… — JSON endpoint backing
/// the Wagtail-parity DocumentChooser widget. Returns top-50 media
/// rows where `kind = "document"` (i.e. non-images), filtered by
/// title / filename.
pub async fn document_chooser_json(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Query(q): Query<DocumentChooserQuery>,
) -> Result<Response, AdminError> {
    let needle =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);

    let mut rows: Vec<Media> = Media::objects()
        .where_(Media::kind.eq("document".to_owned()))
        .order_by(&[("uploaded_at", true)])
        .fetch(tenant.pool())
        .await
        .unwrap_or_default();

    if let Some(n) = needle.as_ref() {
        rows.retain(|m| {
            m.title.to_lowercase().contains(n) || m.filename.to_lowercase().contains(n)
        });
    }

    let items: Vec<serde_json::Value> = rows
        .iter()
        .take(50)
        .map(|m| {
            serde_json::json!({
                "id": m.id.get().copied(),
                "title": m.title,
                "filename": m.filename,
                "kind": m.kind,
                "mime": m.mime,
            })
        })
        .collect();

    let body = serde_json::json!({ "items": items });
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_owned()),
    )
        .into_response())
}

/// GET /cms-admin/search?q=<needle>[&autocomplete=1] — unified search
/// across pages, snippets, and media. In-memory case-insensitive
/// substring match only; the ranked `crate::search` backend is not
/// wired into this handler.
pub async fn admin_search(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Query(q): Query<AdminSearchQuery>,
) -> Result<Response, AdminError> {
    let needle =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);
    let autocomplete = q.autocomplete.as_deref() == Some("1");

    // #143 — log every non-empty query. Best-effort; never blocks
    // the response. Skip autocomplete pings so the analytics surface
    // tracks user-intent queries, not keystroke-level ones.
    if !autocomplete {
        if let Some(raw) = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if let Err(e) = crate::search_promotion::log_query(tenant.pool(), raw).await {
                tracing::debug!(
                    target: "rustango_cms::search",
                    error = %e,
                    "search query log failed (best-effort)",
                );
            }
        }
    }

    let mut pages_out: Vec<serde_json::Value> = Vec::new();
    let mut snippets_out: Vec<serde_json::Value> = Vec::new();
    let mut media_out: Vec<serde_json::Value> = Vec::new();

    if let Some(needle) = needle.as_ref() {
        // Pages. #204 — recall extends from title/slug/url to also
        // include seo_title, seo_description, og_title, og_description
        // and (best-effort) the page-type extension's `load_extension`
        // JSON. Title hits float to the top — they're more relevant
        // than a stray match in a body paragraph.
        let all_pages: Vec<Page> = Page::objects()
            .order_by(&[("updated_at", true)])
            .fetch(tenant.pool())
            .await
            .unwrap_or_default();
        let page_types: Vec<PageType> = PageType::objects()
            .fetch(tenant.pool())
            .await
            .unwrap_or_default();
        let page_type_by_id: std::collections::HashMap<i64, String> = page_types
            .into_iter()
            .filter_map(|pt| pt.id.get().copied().map(|id| (id, pt.type_name)))
            .collect();
        let mut title_hits: Vec<serde_json::Value> = Vec::new();
        let mut body_hits: Vec<serde_json::Value> = Vec::new();
        for p in all_pages {
            if title_hits.len() + body_hits.len() >= 20 {
                break;
            }
            let title = p.title.to_lowercase();
            let slug = p.slug.to_lowercase();
            let url_path = p.url_path.to_lowercase();
            let primary_hit =
                title.contains(needle) || slug.contains(needle) || url_path.contains(needle);

            // Wider body search — SEO/OG fields + extension JSON.
            let mut body_match = false;
            if !primary_hit {
                if p.seo_title.to_lowercase().contains(needle)
                    || p.seo_description.to_lowercase().contains(needle)
                    || p.og_title.to_lowercase().contains(needle)
                    || p.og_description.to_lowercase().contains(needle)
                {
                    body_match = true;
                }
                if !body_match {
                    if let Some(type_name) = page_type_by_id.get(&p.page_type_id) {
                        if let Some(handler) = crate::page_type::find_handler(type_name) {
                            let page_id = p.id.get().copied().unwrap_or_default();
                            if let Ok(ext) = handler.load_extension(tenant.pool(), page_id).await {
                                body_match = json_contains_needle(&ext, needle);
                            }
                        }
                    }
                }
            }

            if primary_hit || body_match {
                let row = serde_json::json!({
                    "id": p.id.get().copied(),
                    "title": p.title,
                    "slug": p.slug,
                    "url_path": p.url_path,
                    "status": p.status,
                });
                if primary_hit {
                    title_hits.push(row);
                } else {
                    body_hits.push(row);
                }
            }
        }
        // Title-first ordering — primary hits float above body hits.
        pages_out.extend(title_hits);
        pages_out.extend(body_hits);
        if pages_out.len() > 20 {
            pages_out.truncate(20);
        }

        // Snippets.
        let all_snippets: Vec<crate::snippet::Snippet> = crate::snippet::Snippet::objects()
            .order_by(&[("updated_at", true)])
            .fetch(tenant.pool())
            .await
            .unwrap_or_default();
        for s in all_snippets {
            if snippets_out.len() >= 20 {
                break;
            }
            let title = s.title.to_lowercase();
            let slug = s.slug.to_lowercase();
            if title.contains(needle) || slug.contains(needle) {
                snippets_out.push(serde_json::json!({
                    "id": s.id.get().copied(),
                    "title": s.title,
                    "slug": s.slug,
                    "type_name": s.type_name,
                }));
            }
        }

        // Media.
        let all_media: Vec<crate::media::Media> = crate::media::Media::objects()
            .order_by(&[("created_at", true)])
            .fetch(tenant.pool())
            .await
            .unwrap_or_default();
        for m in all_media {
            if media_out.len() >= 20 {
                break;
            }
            let title = m.title.to_lowercase();
            let filename = m.filename.to_lowercase();
            let alt = m.alt_text.to_lowercase();
            if title.contains(needle) || filename.contains(needle) || alt.contains(needle) {
                media_out.push(serde_json::json!({
                    "id": m.id.get().copied(),
                    "title": m.title,
                    "filename": m.filename,
                    "alt_text": m.alt_text,
                    "kind": m.kind,
                }));
            }
        }
    }

    // #107 — plugin-contributed search sources
    // (`register_admin_search_area!`). Each area returns its own
    // hit list keyed by area label. Empty when no query.
    let plugin_areas: Vec<serde_json::Value> = if let Some(query) = q.q.as_deref() {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            Vec::new()
        } else {
            crate::hooks::run_admin_search_areas(trimmed)
                .into_iter()
                .map(|(label, hits)| {
                    serde_json::json!({
                        "label": label,
                        "hits": hits,
                    })
                })
                .collect()
        }
    } else {
        Vec::new()
    };

    if autocomplete {
        // Compact JSON shape for the topbar dropdown.
        let payload = serde_json::json!({
            "q": q.q,
            "pages": pages_out.iter().take(5).collect::<Vec<_>>(),
            "snippets": snippets_out.iter().take(5).collect::<Vec<_>>(),
            "media": media_out.iter().take(5).collect::<Vec<_>>(),
            "plugin_areas": plugin_areas,
        });
        return Ok((
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned()),
        )
            .into_response());
    }

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "search", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("query", &q.q.unwrap_or_default());
    ctx.insert("pages", &pages_out);
    ctx.insert("snippets", &snippets_out);
    ctx.insert("media", &media_out);
    let plugin_hit_count: usize = plugin_areas
        .iter()
        .filter_map(|a| a.get("hits").and_then(|h| h.as_array()).map(Vec::len))
        .sum();
    ctx.insert("plugin_search_areas", &plugin_areas);
    ctx.insert(
        "total_hits",
        &(pages_out.len() + snippets_out.len() + media_out.len() + plugin_hit_count),
    );
    render_with_csrf(&state, &headers, "rcms_admin/admin_search.html", &mut ctx)
}

// =====================================================================
// Navigation menus (#22)
// =====================================================================

/// GET /cms-admin/navigation — list every menu.
pub async fn navigation_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let menus: Vec<crate::navigation::Menu> = crate::navigation::Menu::objects()
        .order_by(&[("name", false)])
        .fetch(tenant.pool())
        .await?;
    // Per-menu item count so the list view shows "(N items)".
    let all_items: Vec<crate::navigation::MenuItem> = crate::navigation::MenuItem::objects()
        .fetch(tenant.pool())
        .await?;
    let mut counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for it in &all_items {
        *counts.entry(it.menu_id).or_insert(0) += 1;
    }
    let rows: Vec<serde_json::Value> = menus
        .iter()
        .map(|m| {
            let id = m.id.get().copied().unwrap_or_default();
            serde_json::json!({
                "id": id,
                "slug": m.slug,
                "name": m.name,
                "item_count": counts.get(&id).copied().unwrap_or(0),
                "created_at": m.created_at.get().copied(),
            })
        })
        .collect();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "navigation", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("menus", &rows);
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/navigation_list.html",
        &mut ctx,
    )
}

/// Form payload for create / rename of a menu.
#[derive(Debug, Deserialize)]
pub struct MenuForm {
    pub slug: String,
    pub name: String,
}

/// POST /cms-admin/navigation/new — create a new menu.
pub async fn navigation_create_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Form(form): Form<MenuForm>,
) -> Result<Response, AdminError> {
    let slug = form.slug.trim().to_lowercase();
    let name = form.name.trim().to_owned();
    if slug.is_empty() || name.is_empty() {
        return redirect_named_with_message(
            "rcms-admin:navigation:list",
            MsgLevel::Warning,
            "Menu slug + name are both required.",
            &headers,
        );
    }
    let mut row = crate::navigation::Menu {
        id: rustango::sql::Auto::Unset,
        slug: slug.clone(),
        name,
        created_at: rustango::sql::Auto::Unset,
    };
    row.insert_pool(tenant.pool()).await?;
    redirect_named_with_message_args(
        "rcms-admin:navigation:list",
        MsgLevel::Success,
        "Menu “{slug}” created.",
        &[("slug", slug.as_str())],
        &headers,
    )
}

/// POST /cms-admin/navigation/{id}/delete — delete a menu (cascade
/// deletes its items via the schema FK).
pub async fn navigation_delete_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let menu = crate::navigation::Menu::objects()
        .where_(crate::navigation::Menu::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // Items, their translations, then the menu — all in one transaction.
    // This used to delete items in fetch order with pool-level writes, so
    // a nested menu tripped the self-FK partway through and left the menu
    // half-deleted, and a translated item made it fail every time.
    let items: Vec<crate::navigation::MenuItem> = crate::navigation::MenuItem::objects()
        .where_(crate::navigation::MenuItem::menu_id.eq(id))
        .fetch(tenant.pool())
        .await?;
    rustango::atomic!(tenant.pool(), |tx| {
        let mut guard = tx.lock().await?;
        let tx = &mut *guard;
        delete_menu_items_tx(tx, items).await?;
        menu.delete_tx(tx).await?;
        Ok::<_, rustango::sql::ExecError>(())
    })
    .await?;
    redirect_named_with_message(
        "rcms-admin:navigation:list",
        MsgLevel::Success,
        "Menu deleted.",
        &headers,
    )
}

/// POST /cms-admin/navigation/{id}/clone — duplicate a menu + all
/// its items (#30 V1). The new menu inherits the same item tree
/// (parent_id pointers remapped to the new MenuItem ids); page links
/// stay pointed at the same pages (no page duplication). Slug gets a
/// "-copy" suffix, then "-copy-2" / "-copy-3" / … to dodge the
/// unique constraint on `Menu.slug`. Name gets " (copy)" appended.
pub async fn navigation_clone_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let source = crate::navigation::Menu::objects()
        .where_(crate::navigation::Menu::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;

    // Pick a unique slug. Menu.slug is unique across the tenant.
    let base = source.slug.trim_matches('-').to_owned();
    let desired = format!("{base}-copy");
    let existing_slugs: std::collections::HashSet<String> = crate::navigation::Menu::objects()
        .fetch(tenant.pool())
        .await?
        .into_iter()
        .map(|m| m.slug)
        .collect();
    let slug = if !existing_slugs.contains(&desired) {
        desired
    } else {
        let mut chosen = None;
        for n in 2..=100 {
            let candidate = format!("{desired}-{n}");
            if !existing_slugs.contains(&candidate) {
                chosen = Some(candidate);
                break;
            }
        }
        chosen.ok_or_else(|| {
            AdminError::Validation(
                "Could not allocate a unique slug for the cloned menu.".to_owned(),
            )
        })?
    };

    let mut clone_menu = crate::navigation::Menu {
        id: rustango::sql::Auto::Unset,
        slug,
        name: format!("{} (copy)", source.name),
        created_at: rustango::sql::Auto::Unset,
    };
    // Clone items. `parent_id` needs remapping from source ids to the
    // freshly-inserted clone ids, so a parent must be inserted before its
    // children. `(parent_id.is_some(), sort_order, id)` is NOT that
    // order: it puts every non-root in one bucket sorted by `sort_order`,
    // which says nothing about depth. `seed_from_pages` numbers
    // `sort_order` per sibling bucket, so a grandchild routinely sorts
    // ahead of its parent — cloning a three-level menu then failed
    // partway and, with no transaction, left a junk `*-copy` behind.
    // Pre-order is the guarantee we actually need.
    let items = crate::navigation::order_pre_order(
        crate::navigation::MenuItem::objects()
            .where_(crate::navigation::MenuItem::menu_id.eq(id))
            .order_by(&[("sort_order", false), ("id", false)])
            .fetch(tenant.pool())
            .await?,
    );
    let count = items.len().to_string();

    // Menu row and items together: a failure halfway through used to
    // leave the tenant with an empty, half-built `*-copy` menu.
    let new_menu_id = rustango::atomic!(tenant.pool(), |tx| {
        let mut guard = tx.lock().await?;
        let tx = &mut *guard;
        clone_menu.save_tx(tx).await?;
        let new_menu_id = clone_menu.id.get().copied().unwrap_or_default();

        let mut id_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        for item in &items {
            let old_id = item.id.get().copied().unwrap_or_default();
            // Pre-order guarantees the parent is mapped. An unreachable
            // item (orphan or cycle) is appended by `order_pre_order`
            // and re-parents to the root rather than aborting the clone
            // — losing the nesting of already-broken data beats refusing
            // to copy the menu at all.
            let new_parent = item.parent_id.and_then(|p| id_map.get(&p).copied());
            let mut clone_item = crate::navigation::MenuItem {
                id: rustango::sql::Auto::Unset,
                menu_id: new_menu_id,
                parent_id: new_parent,
                sort_order: item.sort_order,
                label: item.label.clone(),
                page_id: item.page_id,
                external_url: item.external_url.clone(),
                open_in_new_tab: item.open_in_new_tab,
            };
            clone_item.save_tx(tx).await?;
            id_map.insert(old_id, clone_item.id.get().copied().unwrap_or_default());
        }
        Ok::<_, rustango::sql::ExecError>(new_menu_id)
    })
    .await?;

    redirect_named_with_params_and_message_args(
        "rcms-admin:navigation:edit",
        &[("id", new_menu_id.to_string())],
        MsgLevel::Success,
        "Cloned “{name}” with {count} items.",
        &[("name", source.name.as_str()), ("count", count.as_str())],
        &headers,
    )
}

/// POST /cms-admin/navigation/{id}/seed-from-pages — wagtailmenus
/// parity Option B (#257). Replaces this menu's items with a tree
/// seeded from every published page that has `show_in_menus = true`.
/// The page tree's structure is preserved: a child MenuItem points
/// at its parent MenuItem when both pages are in the menu set; if a
/// page's parent is NOT in the menu set, the menu item lands at
/// the top level.
///
/// Idempotent — running it twice rebuilds the same tree (page ids
/// stay constant, so the second pass produces identical items).
/// Drops the existing menu items first; failure rolls back via
/// the per-row delete loop (matches the save-tree handler's
/// drop-and-rebuild approach).
pub async fn navigation_seed_from_pages_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let menu = crate::navigation::Menu::objects()
        .where_(crate::navigation::Menu::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;

    // Step 1: collect what is there now. Deleted inside the transaction
    // below, not here — this used to delete row by row on the pool, in
    // fetch order, so re-seeding a nested menu tripped the self-FK
    // partway through and left the menu wiped with nothing re-inserted.
    // A second click on an "idempotent" button was straight data loss.
    let existing: Vec<crate::navigation::MenuItem> = crate::navigation::MenuItem::objects()
        .where_(crate::navigation::MenuItem::menu_id.eq(id))
        .fetch(tenant.pool())
        .await?;

    // Step 2: fetch every menu-eligible page in materialized-path
    // order (tree-pre-order: a parent always appears before its
    // children, so the parent's new MenuItem id is mapped by the
    // time the child's row is processed).
    let mut pages: Vec<crate::page::Page> = crate::page::Page::objects()
        .where_(crate::page::Page::show_in_menus.eq(true))
        .where_(crate::page::Page::status.eq("published".to_owned()))
        .order_by(&[("path", false), ("id", false)])
        .fetch(tenant.pool())
        .await?;
    // Error pages are routing furniture, not navigation — excluded here
    // as `auto_menu`, `sitemap` and the tree endpoint all exclude them.
    // A 404 handler marked "Show in menus" used to land in the navbar.
    let error_tid = crate::error_pages::error_page_type_id(tenant.pool()).await;
    pages.retain(|p| error_tid.is_none_or(|tid| p.page_type_id != tid));

    // Step 3: insert one MenuItem per page, mapping page.parent_id →
    // the freshly-inserted MenuItem id of that parent (if the parent
    // is also in the menu set). sort_order mirrors the sibling-
    // relative order in the page tree.
    let mut page_to_item: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let mut sibling_counter: std::collections::HashMap<Option<i64>, i32> =
        std::collections::HashMap::new();
    // One item per eligible page, so the count is known before the write
    // — and reading a counter mutated inside the transaction closure
    // would always have returned 0 anyway.
    let inserted = pages.len();
    rustango::atomic!(tenant.pool(), |tx| {
        let mut guard = tx.lock().await?;
        let tx = &mut *guard;
    delete_menu_items_tx(tx, existing).await?;
    for page in &pages {
        let page_id = page.id.get().copied().unwrap_or_default();
        let parent_menu_item_id: Option<i64> = page
            .parent_id
            .and_then(|pid| page_to_item.get(&pid).copied());
        // Sort order: monotonically increasing within each
        // parent-bucket so the menu builder shows pages in the same
        // sibling order the page tree has.
        let bucket = parent_menu_item_id;
        let sort_order = *sibling_counter
            .entry(bucket)
            .and_modify(|n| *n += 1)
            .or_insert(0);
        let mut item = crate::navigation::MenuItem {
            id: rustango::sql::Auto::Unset,
            menu_id: id,
            parent_id: parent_menu_item_id,
            sort_order,
            label: page.title.clone(),
            page_id: Some(page_id),
            external_url: None,
            open_in_new_tab: false,
        };
        item.save_tx(tx).await?;
        let new_item_id = item.id.get().copied().unwrap_or_default();
        page_to_item.insert(page_id, new_item_id);
    }
    Ok::<_, rustango::sql::ExecError>(())
    })
    .await?;

    redirect_named_with_params_and_message_plural(
        "rcms-admin:navigation:edit",
        &[("id", id.to_string())],
        MsgLevel::Success,
        "Seeded “{name}” with {count} item(s) from pages marked Show in menus.",
        inserted as i64,
        &[
            ("name", menu.name.as_str()),
            ("count", &inserted.to_string()),
        ],
        &headers,
    )
}

/// Delete menu items and their translations inside a transaction.
///
/// `cms_menu_item_translation.item_id` and `cms_menu_item.parent_id` are
/// both FKs with no `ON DELETE`, so the order is forced: translations
/// first, then items deepest-first. Three handlers got this wrong in
/// three different ways before it lived in one place — the menu delete
/// ignored translations entirely, the re-seed deleted in fetch order, and
/// save-tree sorted on `parent_id.is_none()`, which ties every non-root.
async fn delete_menu_items_tx(
    tx: &mut rustango::sql::PoolTx<'_>,
    items: Vec<crate::navigation::MenuItem>,
) -> Result<(), rustango::sql::ExecError> {
    use crate::menu_item_translation::MenuItemTranslation;
    if items.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = items.iter().filter_map(|it| it.id.get().copied()).collect();
    if !ids.is_empty() {
        let orphans: Vec<MenuItemTranslation> = MenuItemTranslation::objects()
            .where_(MenuItemTranslation::item_id.is_in(ids))
            .fetch_tx(tx)
            .await?;
        for row in orphans {
            row.delete_tx(tx).await?;
        }
    }
    for it in crate::navigation::order_deepest_first(items) {
        it.delete_tx(tx).await?;
    }
    Ok(())
}

/// POST /cms-admin/navigation/{id}/save-tree — WP-style menu builder
/// save (#44). Accepts a JSON array of items in display order; each
/// entry's `local_id` is its position in the array (0-indexed) and
/// `parent_local_id` references the array index of the parent (null
/// for top-level). The handler:
///
///   1. Validates that the tree is acyclic + every parent_local_id
///      < the child's local_id (insert-order invariant).
///   2. Updates rows the payload still carries an `id` for, **in
///      place**, and inserts the rest.
///   3. Deletes the rows the payload dropped, children before parents.
///
/// It used to delete every row and re-insert, which meant a menu item's
/// id changed on every save. That was survivable while nothing
/// referenced those ids — and stopped being survivable the moment
/// `cms_menu_item_translation` keyed translations on `item_id`, since a
/// reorder would silently discard every translated label. Ids are now
/// stable across a save.
///
/// Failure rolls back; no half-saved state.
#[derive(Debug, Deserialize)]
pub struct SaveTreeItem {
    /// Existing `cms_menu_item.id`. `None` for items added in this
    /// session, in which case the handler inserts a new row. An id that
    /// doesn't belong to this menu is treated the same way, so a stale
    /// or forged id can't reassign another menu's item.
    #[serde(default)]
    pub id: Option<i64>,
    /// 0-indexed position in the submitted array.
    pub local_id: i32,
    /// Array index of the parent, or `None` for top-level.
    #[serde(default)]
    pub parent_local_id: Option<i32>,
    pub label: String,
    #[serde(default)]
    pub page_id: Option<i64>,
    #[serde(default)]
    pub external_url: Option<String>,
    #[serde(default)]
    pub open_in_new_tab: bool,
}

#[derive(Debug, Deserialize)]
pub struct SaveTreePayload {
    pub items: Vec<SaveTreeItem>,
}

pub async fn navigation_save_tree_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Path(id): Path<i64>,
    axum::Json(payload): axum::Json<SaveTreePayload>,
) -> Result<Response, AdminError> {
    // Validate tree shape: every parent_local_id must reference an
    // earlier index (so we can insert in order without two passes).
    for (i, item) in payload.items.iter().enumerate() {
        if item.local_id as usize != i {
            return Err(AdminError::Validation(format!(
                "items[{i}].local_id must equal its array index"
            )));
        }
        if let Some(p) = item.parent_local_id {
            if (p as usize) >= i {
                return Err(AdminError::Validation(format!(
                    "items[{i}].parent_local_id {p} must reference an earlier item"
                )));
            }
        }
    }

    // Exactly one link target, and lengths the columns can hold. The
    // form handler enforced this and `save-tree` did not, so the builder
    // could store an item with both (`page_id` silently wins, and
    // unpublishing that page drops an item carrying a perfectly good
    // external fallback) or with neither (rendered as a link to `/`,
    // indistinguishable from a grouping header). An over-long label
    // reached the driver as a raw error and surfaced as a 500.
    for (i, item) in payload.items.iter().enumerate() {
        let external = item
            .external_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match (item.page_id, external) {
            (Some(_), Some(_)) => {
                return Err(AdminError::Validation(format!(
                    "items[{i}] sets both a page and an external URL — pick one"
                )))
            }
            (None, None) => {
                return Err(AdminError::Validation(format!(
                    "items[{i}] has no link target — set a page or an external URL"
                )))
            }
            _ => {}
        }
        if item.label.chars().count() > 200 {
            return Err(AdminError::Validation(format!(
                "items[{i}].label is too long (max 200 characters)"
            )));
        }
        if external.is_some_and(|u| u.chars().count() > 500) {
            return Err(AdminError::Validation(format!(
                "items[{i}].external_url is too long (max 500 characters)"
            )));
        }
    }

    // An id may appear at most once. The upsert below claims each kept row
    // out of `existing_by_id`, so a repeat would find the row already
    // taken — previously an `.expect` that panicked mid-transaction.
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for (i, item) in payload.items.iter().enumerate() {
        if let Some(iid) = item.id {
            if !seen.insert(iid) {
                return Err(AdminError::Validation(format!(
                    "items[{i}].id {iid} appears more than once"
                )));
            }
        }
    }

    // Every sibling handler verifies the menu first; this one went
    // straight to inserting rows with `menu_id: id`, so a stale or
    // mistyped id surfaced as a raw FK violation (a 500) instead of a
    // 404.
    if crate::navigation::Menu::objects()
        .where_(crate::navigation::Menu::id.eq(id))
        .first(tenant.pool())
        .await?
        .is_none()
    {
        return Err(AdminError::NotFound(id));
    }

    let existing: Vec<crate::navigation::MenuItem> = crate::navigation::MenuItem::objects()
        .where_(crate::navigation::MenuItem::menu_id.eq(id))
        .fetch(tenant.pool())
        .await?;
    let mut existing_by_id: std::collections::HashMap<i64, crate::navigation::MenuItem> = existing
        .into_iter()
        .filter_map(|it| it.id.get().copied().map(|iid| (iid, it)))
        .collect();

    // Claim each payload entry's existing row up front, so the write loop
    // below has no fallible lookup left to make. `Some(row)` updates in
    // place (keeping the id, and with it the item's translations);
    // `None` inserts. An id from another menu, or one that isn't in this
    // menu at all, resolves to `None` rather than stealing that row.
    let plan: Vec<Option<crate::navigation::MenuItem>> = payload
        .items
        .iter()
        .map(|it| it.id.and_then(|iid| existing_by_id.remove(&iid)))
        .collect();

    let items_saved = payload.items.len();
    rustango::atomic!(tenant.pool(), |tx| {
        let mut guard = tx.lock().await?;
        let tx = &mut *guard;
        // Upsert in array order, resolving each item's parent from the
        // slot its `local_id` indexes. The validation above pins
        // `local_id` to the array index and requires
        // `parent_local_id < local_id`, so a parent's slot is always
        // filled before a child reads it — no fallible lookup here, and
        // no way to express one.
        let mut resolved: Vec<i64> = Vec::with_capacity(payload.items.len());
        for ((i, item), claimed) in payload.items.iter().enumerate().zip(plan) {
            let parent_id = item.parent_local_id.map(|p| resolved[p as usize]);
            let external_url = item
                .external_url
                .as_ref()
                .filter(|s| !s.trim().is_empty())
                .cloned();
            let sort_order = (i as i32) * 10;

            let new_id = match claimed {
                Some(mut row) => {
                    // Update in place — this is what keeps the id, and
                    // therefore the item's translations, alive.
                    let iid = row.id.get().copied().unwrap_or_default();
                    row.parent_id = parent_id;
                    row.sort_order = sort_order;
                    row.label = item.label.clone();
                    row.page_id = item.page_id;
                    row.external_url = external_url;
                    row.open_in_new_tab = item.open_in_new_tab;
                    row.save_tx(tx).await?;
                    iid
                }
                None => {
                    let mut row = crate::navigation::MenuItem {
                        id: rustango::sql::Auto::Unset,
                        menu_id: id,
                        parent_id,
                        sort_order,
                        label: item.label.clone(),
                        page_id: item.page_id,
                        external_url,
                        open_in_new_tab: item.open_in_new_tab,
                    };
                    row.save_tx(tx).await?;
                    row.id.get().copied().unwrap_or_default()
                }
            };
            resolved.push(new_id);
        }

        // Whatever the payload dropped. Every surviving row has already
        // been repointed above, so nothing still references these — but
        // they may reference each other, and `MenuItem.parent_id` is an
        // FK to `cms_menu_item.id` with no `ON DELETE`, so a parent can
        // only go once its children are gone.
        delete_menu_items_tx(tx, existing_by_id.into_values().collect()).await?;
        Ok::<_, rustango::sql::ExecError>(())
    })
    .await?;

    Ok(axum::Json(serde_json::json!({
        "ok": true,
        "items_saved": items_saved,
    }))
    .into_response())
}

/// GET /cms-admin/navigation/{id}/edit — item editor for one menu.
pub async fn navigation_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Query(q): Query<EditQuery>,
) -> Result<Response, AdminError> {
    let menu = crate::navigation::Menu::objects()
        .where_(crate::navigation::Menu::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // Pre-order, not `ORDER BY parent_id`. The builder derives each
    // item's parent from DOM adjacency on save, so the DOM has to start
    // in pre-order or merely opening a menu and pressing Save rewrites
    // its nesting — see `navigation::order_pre_order`.
    let items = crate::navigation::order_pre_order(
        crate::navigation::MenuItem::objects()
            .where_(crate::navigation::MenuItem::menu_id.eq(id))
            .order_by(&[("sort_order", false), ("id", false)])
            .fetch(tenant.pool())
            .await?,
    );
    // Pages flagged `show_in_menus = true` are the suggested-picker
    // entries; fall back to "every page" if no editor has opted any
    // pages in yet.
    let suggested: Vec<Page> = Page::objects()
        .where_(Page::show_in_menus.eq(true))
        .order_by(&[("title", false)])
        .fetch(tenant.pool())
        .await?;
    // WP-style builder (#44): the picker always shows every page so
    // editors can drop any of them in. The legacy curated flag stays
    // available for downstream theme rendering but no longer hides
    // pages from the admin picker.
    let all_pages: Vec<Page> = Page::objects()
        .order_by(&[("title", false)])
        .fetch(tenant.pool())
        .await?;

    // Resolve item rows into a render-friendly shape (label, url,
    // page title for the dropdown when set).
    let by_page_id: std::collections::HashMap<i64, &Page> = all_pages
        .iter()
        .filter_map(|p| p.id.get().copied().map(|id| (id, p)))
        .collect();
    // ?locale=<non-default> switches the editor into translation mode:
    // the tree builder still edits canonical structure, and a second
    // panel edits this locale's label overrides. Same shape as the page
    // and snippet editors, so the locale switcher behaves identically
    // wherever an editor meets one.
    let locales: Vec<Locale> = Locale::objects()
        .where_(Locale::active.eq(true))
        .order_by(&[("is_default", true), ("code", false)])
        .fetch(tenant.pool())
        .await?;
    let editing_locale: Option<Locale> = match q.locale.as_deref().filter(|c| !c.is_empty()) {
        Some(code) => locales.iter().find(|l| l.code == code).cloned(),
        None => None,
    };
    let translation_mode = editing_locale.as_ref().is_some_and(|l| !l.is_default);
    let item_translations = match editing_locale.as_ref().filter(|l| !l.is_default) {
        Some(loc) => {
            let lid = loc.id.get().copied().unwrap_or_default();
            let ids: Vec<i64> = items.iter().filter_map(|it| it.id.get().copied()).collect();
            crate::menu_item_translation::fetch_for_items(tenant.pool(), &ids, lid).await?
        }
        None => std::collections::HashMap::new(),
    };

    let item_rows: Vec<serde_json::Value> = items
        .iter()
        .map(|it| {
            let resolved_label = if it.label.trim().is_empty() {
                it.page_id
                    .and_then(|pid| by_page_id.get(&pid).map(|p| p.title.clone()))
                    .unwrap_or_default()
            } else {
                it.label.clone()
            };
            let resolved_url = match (it.page_id, it.external_url.clone()) {
                (Some(pid), _) => by_page_id
                    .get(&pid)
                    .map(|p| p.url_path.clone())
                    .unwrap_or_else(|| "—".to_owned()),
                (None, Some(url)) => url,
                _ => "—".to_owned(),
            };
            let tr = it
                .id
                .get()
                .and_then(|iid| item_translations.get(iid));
            serde_json::json!({
                "id": it.id.get().copied(),
                "label": it.label,
                "resolved_label": resolved_label,
                "page_id": it.page_id,
                "external_url": it.external_url,
                "parent_id": it.parent_id,
                "sort_order": it.sort_order,
                "open_in_new_tab": it.open_in_new_tab,
                "resolved_url": resolved_url,
                "tr_label": tr.and_then(|m| m.get("label")).cloned().unwrap_or_default(),
                "tr_external_url": tr
                    .and_then(|m| m.get("external_url"))
                    .cloned()
                    .unwrap_or_default(),
            })
        })
        .collect();

    // Error pages are served for a status code; a menu never links one.
    let error_tid = crate::error_pages::error_page_type_id(tenant.pool()).await;
    let linkable = |p: &&Page| error_tid.map_or(true, |tid| p.page_type_id != tid);
    let picker_pages: Vec<serde_json::Value> = all_pages
        .iter()
        .filter(linkable)
        .map(|p| {
            serde_json::json!({
                "id": p.id.get().copied(),
                "title": p.title,
                "url_path": p.url_path,
            })
        })
        .collect();

    // #49c — recent-pages tab. 20 most-recently-updated pages so
    // editors who just published something can drop it into the menu
    // without scrolling the full picker. Fetched via a separate
    // updated_at-desc query so the existing alphabetical picker
    // stays unchanged.
    let recent_pages_raw: Vec<Page> = Page::objects()
        .order_by(&[("updated_at", true)])
        .fetch(tenant.pool())
        .await?;
    let picker_recent: Vec<serde_json::Value> = recent_pages_raw
        .iter()
        .filter(linkable)
        .take(20)
        .map(|p| {
            serde_json::json!({
                "id": p.id.get().copied(),
                "title": p.title,
                "url_path": p.url_path,
                "updated_at": p.updated_at,
            })
        })
        .collect();

    // #49d — Library tab. Each registered `LibraryTypeHandler` opts
    // in via `menu_picker_entries(pool)`; we flatten the result
    // across every handler and tag each entry with the contributing
    // type's verbose_name so editors can tell where an entry came
    // from. Failure in any handler short-circuits the response so
    // bugs surface loudly instead of silently hiding entries.
    let mut picker_library: Vec<serde_json::Value> = Vec::new();
    for handler in crate::library::registered_handlers() {
        let entries = handler
            .menu_picker_entries(tenant.pool())
            .await
            .map_err(|e| {
                AdminError::Validation(format!(
                    "library `{}::menu_picker_entries` failed: {e}",
                    handler.type_name()
                ))
            })?;
        for entry in entries {
            picker_library.push(serde_json::json!({
                "label": entry.label,
                "url": entry.url,
                "kind_hint": entry.kind_hint,
                "library_type": handler.verbose_name(),
            }));
        }
    }

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "navigation", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("menu_id", &menu.id.get().copied());
    ctx.insert("menu_slug", &menu.slug);
    ctx.insert("menu_name", &menu.name);
    ctx.insert("items", &item_rows);
    ctx.insert("picker_pages", &picker_pages);
    ctx.insert("picker_recent", &picker_recent);
    ctx.insert("picker_library", &picker_library);
    ctx.insert("has_library_picker", &!picker_library.is_empty());
    ctx.insert("has_curated_picker", &!suggested.is_empty());
    ctx.insert("locales", &locales);
    ctx.insert("editing_locale", &editing_locale);
    ctx.insert("translation_mode", &translation_mode);
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/navigation_edit.html",
        &mut ctx,
    )
}

/// POST /cms-admin/navigation/{id}/translate?locale=<code> — save this
/// locale's label overrides for a menu's items.
///
/// Separate from the tree save on purpose. The builder posts JSON and
/// owns *structure*; this posts a form and owns *text*, so an editor
/// translating labels cannot accidentally reshape the menu, and a
/// reorder cannot drop a translation.
///
/// Field names are `tr__<item id>__<field>`. An empty value deletes the
/// override rather than storing a blank, which is what makes the
/// canonical label show through again.
pub async fn navigation_translate_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    Path(id): Path<i64>,
    Query(q): Query<EditQuery>,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    use crate::menu_item_translation::MenuItemTranslation;

    let code = q
        .locale
        .as_deref()
        .filter(|c| !c.is_empty())
        .ok_or_else(|| AdminError::Validation("translate requires ?locale=".to_owned()))?;
    let locale = Locale::objects()
        .where_(Locale::code.eq(code.to_owned()))
        .where_(Locale::active.eq(true))
        .first(tenant.pool())
        .await?
        .ok_or_else(|| AdminError::Validation(format!("unknown locale `{code}`")))?;
    if locale.is_default {
        return Ok((
            axum::http::StatusCode::BAD_REQUEST,
            "cannot translate INTO the default locale; canonical labels live on cms_menu_item",
        )
            .into_response());
    }
    let locale_id = locale
        .id
        .get()
        .copied()
        .ok_or_else(|| AdminError::Validation("locale row missing id".to_owned()))?;

    // Only this menu's items are editable here — the field names carry
    // item ids, and without this check a crafted post could write
    // translations for another menu's items.
    let own_items: std::collections::HashSet<i64> = crate::navigation::MenuItem::objects()
        .where_(crate::navigation::MenuItem::menu_id.eq(id))
        .fetch(tenant.pool())
        .await?
        .iter()
        .filter_map(|it| it.id.get().copied())
        .collect();

    let mut updates: Vec<(i64, String, String)> = Vec::new();
    for (k, v) in &form {
        let Some(rest) = k.strip_prefix("tr__") else {
            continue;
        };
        let Some((raw_id, raw_field)) = rest.split_once("__") else {
            continue;
        };
        let Ok(item_id) = raw_id.parse::<i64>() else {
            continue;
        };
        if !own_items.contains(&item_id) {
            continue;
        }
        // A malformed field is skipped rather than 400-ing the batch, so
        // one bad input doesn't discard an editor's other edits.
        if let Ok(field) = crate::menu_item_translation::validate_field_path(raw_field) {
            updates.push((item_id, field, v.trim().to_owned()));
        }
    }

    rustango::atomic!(tenant.pool(), |tx| {
        let mut guard = tx.lock().await?;
        let tx = &mut *guard;
        for (item_id, field, value) in updates {
            let existing: Vec<MenuItemTranslation> = MenuItemTranslation::objects()
                .where_(MenuItemTranslation::item_id.eq(item_id))
                .where_(MenuItemTranslation::locale_id.eq(locale_id))
                .where_(MenuItemTranslation::field_path.eq(field.clone()))
                .fetch_tx(tx)
                .await?;
            if value.is_empty() {
                for row in existing {
                    row.delete_tx(tx).await?;
                }
            } else if let Some(mut row) = existing.into_iter().next() {
                row.value = value;
                row.save_tx(tx).await?;
            } else {
                let mut row = MenuItemTranslation {
                    id: rustango::sql::Auto::Unset,
                    item_id,
                    locale_id,
                    field_path: field,
                    value,
                    updated_at: rustango::sql::Auto::Unset,
                };
                row.save_tx(tx).await?;
            }
        }
        Ok::<_, rustango::sql::ExecError>(())
    })
    .await?;

    Ok(Redirect::to(&format!(
        "/cms-admin/navigation/{id}/edit?locale={code}"
    ))
    .into_response())
}

/// Form payload for adding / updating a menu item. Empty strings on
/// `page_id` and `external_url` map to None; the validator enforces
/// "exactly one of the two".
#[derive(Debug, Deserialize)]
pub struct MenuItemForm {
    pub label: String,
    #[serde(default, deserialize_with = "super::deserialize_optional_i64")]
    pub page_id: Option<i64>,
    #[serde(default)]
    pub external_url: Option<String>,
    #[serde(default, deserialize_with = "super::deserialize_optional_i64")]
    pub parent_id: Option<i64>,
    #[serde(default)]
    pub sort_order: Option<i32>,
    #[serde(default)]
    pub open_in_new_tab: Option<String>,
}

/// Reject a `parent_id` that does not belong to `menu_id`.
///
/// The form took the value straight from the request, so an item could be
/// parented into another menu — leaving it invisible in *both* (each
/// resolver buckets by its own `menu_id`, so the child is unreachable in
/// one and filtered out of the other) and making the other menu
/// permanently undeletable, because its item is now referenced by a row
/// the delete never sees.
async fn validate_menu_parent(
    tenant: &Tenant,
    menu_id: i64,
    parent_id: Option<i64>,
) -> Result<(), AdminError> {
    let Some(pid) = parent_id else {
        return Ok(());
    };
    let parent = crate::navigation::MenuItem::objects()
        .where_(crate::navigation::MenuItem::id.eq(pid))
        .first(tenant.pool())
        .await?;
    match parent {
        Some(p) if p.menu_id == menu_id => Ok(()),
        Some(_) => Err(AdminError::Validation(
            "That parent item belongs to a different menu.".to_owned(),
        )),
        None => Err(AdminError::Validation(
            "That parent item no longer exists.".to_owned(),
        )),
    }
}

impl MenuItemForm {
    fn parsed_external_url(&self) -> Option<String> {
        self.external_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }
}

/// POST /cms-admin/navigation/{id}/items — create a new menu item.
pub async fn navigation_item_create(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(menu_id): Path<i64>,
    Form(form): Form<MenuItemForm>,
) -> Result<Response, AdminError> {
    let external = form.parsed_external_url();
    // "Exactly one of page_id / external_url" — fail soft to the
    // edit page with a warning message.
    if form.page_id.is_some() == external.is_some() {
        return redirect_named_with_params_and_message(
            "rcms-admin:navigation:edit",
            &[("id", menu_id.to_string())],
            MsgLevel::Warning,
            "Pick exactly one of: a page link, or a custom URL.",
            &headers,
        );
    }
    // The parent must live in this menu — see `validate_menu_parent`.
    validate_menu_parent(&tenant, menu_id, form.parent_id).await?;
    let mut row = crate::navigation::MenuItem {
        id: rustango::sql::Auto::Unset,
        menu_id,
        parent_id: form.parent_id,
        sort_order: form.sort_order.unwrap_or(100),
        label: form.label.trim().to_owned(),
        page_id: form.page_id,
        external_url: external,
        open_in_new_tab: form.open_in_new_tab.is_some(),
    };
    row.insert_pool(tenant.pool()).await?;
    redirect_named_with_params_and_message(
        "rcms-admin:navigation:edit",
        &[("id", menu_id.to_string())],
        MsgLevel::Success,
        "Item added.",
        &headers,
    )
}

/// POST /cms-admin/navigation/{menu_id}/items/{id}/delete.
pub async fn navigation_item_delete(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path((menu_id, id)): Path<(i64, i64)>,
) -> Result<Response, AdminError> {
    let row = crate::navigation::MenuItem::objects()
        .where_(crate::navigation::MenuItem::id.eq(id))
        .where_(crate::navigation::MenuItem::menu_id.eq(menu_id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    // Re-parent any children to the deleted item's parent so the
    // tree doesn't orphan a whole subtree.
    let children: Vec<crate::navigation::MenuItem> = crate::navigation::MenuItem::objects()
        .where_(crate::navigation::MenuItem::parent_id.eq(Some(id)))
        .fetch(tenant.pool())
        .await?;
    for mut c in children {
        c.parent_id = row.parent_id;
        c.save_pool(tenant.pool()).await?;
    }
    row.delete_pool(tenant.pool()).await?;
    redirect_named_with_params_and_message(
        "rcms-admin:navigation:edit",
        &[("id", menu_id.to_string())],
        MsgLevel::Success,
        "Item deleted.",
        &headers,
    )
}

// =====================================================================
// Page types — usage dashboard (#16)
// =====================================================================

/// GET /cms-admin/page-types — registry of every page type the host
/// has compiled in, with how many pages of each currently exist + the
/// most-recent one. Useful before deprecating a type or refactoring
/// its declared fields.
pub async fn page_types_dashboard(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    // Pull every Page row once and bucket by `page_type_id` in Rust;
    // sites have N pages but only a handful of types, so one fetch +
    // a HashMap aggregate beats a per-type COUNT round-trip.
    let pages: Vec<Page> = Page::objects().fetch(tenant.pool()).await?;
    let page_types: Vec<PageType> = PageType::objects().fetch(tenant.pool()).await?;

    let mut counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let mut latest: std::collections::HashMap<i64, chrono::DateTime<chrono::Utc>> =
        std::collections::HashMap::new();
    for p in &pages {
        *counts.entry(p.page_type_id).or_insert(0) += 1;
        if let Some(when) = p.published_at.or_else(|| p.created_at.get().copied()) {
            latest
                .entry(p.page_type_id)
                .and_modify(|cur| {
                    if when > *cur {
                        *cur = when;
                    }
                })
                .or_insert(when);
        }
    }

    let type_id_by_name: std::collections::HashMap<&str, i64> = page_types
        .iter()
        .filter_map(|t| t.id.get().copied().map(|id| (t.type_name.as_str(), id)))
        .collect();

    // Cross-reference the in-process handler registry with the DB
    // `cms_page_type` rows so unregistered + un-seeded types both
    // surface (handler-only = "compiled in but never used"; row-only
    // = "DB drift from an old build").
    let mut handler_names = std::collections::BTreeSet::new();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for handler in crate::page_type::registered_handlers() {
        let name = handler.type_name().to_owned();
        handler_names.insert(name.clone());
        let row_id = type_id_by_name.get(name.as_str()).copied();
        let count = row_id.and_then(|id| counts.get(&id).copied()).unwrap_or(0);
        let last = row_id.and_then(|id| latest.get(&id).copied());
        rows.push(serde_json::json!({
            "type_name": name,
            "verbose_name": handler.verbose_name(),
            "app_label": handler.app_label(),
            "icon": handler.icon(),
            "description": handler.description(),
            "default_template": handler.default_template(),
            "feed_kind": handler.feed_kind(),
            "allowed_parents": handler.allowed_parent_types(),
            "allowed_children": handler.allowed_child_types(),
            "is_creatable": handler.is_creatable(),
            "count": count,
            "latest": last,
            "registered": true,
            "row_id": row_id,
        }));
    }

    // DB-only types — appear in `cms_page_type` but no handler is
    // compiled in. Two flavours: UI-created types (#566,
    // `app_label = "cms_ui"`, first-class — creatable + deletable) and
    // build drift (a stale row from an old binary — surfaced so editors
    // notice). Only UI types get a delete affordance, and only while no
    // pages use them; code types are never deletable from the UI.
    for t in &page_types {
        if !handler_names.contains(&t.type_name) {
            let id = t.id.get().copied().unwrap_or_default();
            let count = counts.get(&id).copied().unwrap_or(0);
            let ui_type = t.app_label == "cms_ui";
            rows.push(serde_json::json!({
                "type_name": t.type_name,
                "verbose_name": t.verbose_name,
                "app_label": t.app_label,
                "icon": if ui_type { serde_json::Value::String("dashboard_customize".to_owned()) } else { serde_json::Value::Null },
                "description": serde_json::Value::Null,
                "default_template": t.default_template,
                "feed_kind": serde_json::Value::Null,
                "allowed_parents": t.allowed_parent_types.clone(),
                "allowed_children": t.allowed_child_types.clone(),
                "is_creatable": ui_type && t.is_creatable,
                "count": count,
                "latest": latest.get(&id).copied(),
                "registered": false,
                "row_id": id,
                "ui_type": ui_type,
                "deletable": ui_type && count == 0,
            }));
        }
    }
    // Sort: most-used first; ties break by verbose_name.
    rows.sort_by(|a, b| {
        let ac = a.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
        let bc = b.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
        bc.cmp(&ac).then_with(|| {
            a.get("verbose_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get("verbose_name").and_then(|v| v.as_str()).unwrap_or(""))
        })
    });

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "page-types", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("rows", &rows);
    ctx.insert("total_pages", &pages.len());
    ctx.insert("total_types", &rows.len());
    // #566 — the New-page-type affordance is Developer-gated (same
    // codename as the field builder). The template also checks the flag.
    let can_build = session_user.as_ref().is_some_and(|u| u.is_superuser)
        || crate::permissions::user_codenames(
            tenant.pool(),
            session_user
                .as_ref()
                .and_then(|u| u.id.get().copied())
                .unwrap_or_default(),
        )
        .await
        .unwrap_or_default()
        .contains("cms_page_type.build");
    ctx.insert("can_build_types", &can_build);
    ctx.insert("new_type_url", "/cms-admin/page-types/new");
    render_with_csrf(&state, &headers, "rcms_admin/page_types.html", &mut ctx)
}

// =====================================================================
// Users / Roles — native CMS surface (#9). Replaces the prior
// `Redirect::to("/admin/rustango_users")` stub.
// =====================================================================

/// #642 — user management is a **superuser-only** operation.
///
/// The create/edit handlers set `is_superuser`, `active`, and role
/// memberships straight from the submitted form, and the handlers
/// themselves previously performed no authorization at all — so any user
/// whose request reached the admin router could mint a superuser or
/// assign themselves a privileged role. Because *every* one of those
/// fields is a privilege the caller might not hold, the safe invariant
/// is to restrict the whole user-management surface to superusers rather
/// than try to police individual field grants. (Delegated, codename-
/// scoped user management that forbids privilege escalation can be added
/// later as a deliberate feature.)
///
/// #672 — the same gate covers roles and SSO providers, which decide who
/// a user *is* as much as the user rows do: a role editor could grant its
/// own role every codename, and an SSO-provider editor could point an
/// issuer at an IdP it controls and sign in as any user by email.
///
/// Returns `Some(redirect)` when denied — anonymous → login, an
/// authenticated non-superuser → the no-access page — and `None` when
/// the caller is a superuser and may proceed.
fn require_superuser_for_user_admin(
    user: Option<&rustango::tenancy::auth::User>,
) -> Option<Response> {
    use axum::response::{IntoResponse, Redirect};
    user_admin_denial_target(user.map(|u| u.is_superuser))
        .map(|path| Redirect::to(path).into_response())
}

/// The redirect target for a user-management request, or `None` when the
/// caller (identified only by their superuser status: `None` = anonymous,
/// `Some(false)` = signed-in non-superuser, `Some(true)` = superuser) may
/// proceed. Split out from [`require_superuser_for_user_admin`] so the
/// deny/allow decision is unit-testable without constructing a framework
/// `User` (which has no `Default`).
fn user_admin_denial_target(is_superuser: Option<bool>) -> Option<&'static str> {
    match is_superuser {
        Some(true) => None,
        Some(false) => Some("/cms-admin/no-access"),
        None => Some("/login"),
    }
}

/// GET /cms-admin/users — every tenant user with their assigned
/// roles + a delta-since indicator on the last password rotation.
pub async fn users_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let users: Vec<rustango::tenancy::auth::User> = rustango::tenancy::auth::User::objects()
        .order_by(&[("username", false)])
        .fetch(tenant.pool())
        .await?;
    let roles: Vec<rustango::tenancy::permissions::Role> =
        rustango::tenancy::permissions::Role::objects()
            .fetch(tenant.pool())
            .await?;
    let user_roles: Vec<rustango::tenancy::permissions::UserRole> =
        rustango::tenancy::permissions::UserRole::objects()
            .fetch(tenant.pool())
            .await?;

    let roles_by_id: std::collections::HashMap<i64, &rustango::tenancy::permissions::Role> = roles
        .iter()
        .filter_map(|r| r.id.get().copied().map(|id| (id, r)))
        .collect();
    let mut roles_by_user: std::collections::HashMap<i64, Vec<String>> =
        std::collections::HashMap::new();
    for ur in &user_roles {
        if let Some(role) = roles_by_id.get(&ur.role_id) {
            roles_by_user
                .entry(ur.user_id)
                .or_default()
                .push(role.name.clone());
        }
    }

    let rows: Vec<serde_json::Value> = users
        .iter()
        .map(|u| {
            let id = u.id.get().copied();
            serde_json::json!({
                "id": id,
                "username": u.username,
                "email": user_email(u),
                "is_superuser": u.is_superuser,
                "active": u.active,
                "created_at": u.created_at,
                "password_changed_at": u.password_changed_at,
                "roles": id.and_then(|i| roles_by_user.get(&i).cloned()).unwrap_or_default(),
            })
        })
        .collect();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "users", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("users", &rows);
    ctx.insert("total_users", &users.len());
    ctx.insert("active_users", &users.iter().filter(|u| u.active).count());
    render_with_csrf(&state, &headers, "rcms_admin/users_list.html", &mut ctx)
}

/// GET /cms-admin/roles — list every role with its permission count
/// and member count.
pub async fn roles_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    // Identity administration is superuser-only (#672).
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let roles: Vec<rustango::tenancy::permissions::Role> =
        rustango::tenancy::permissions::Role::objects()
            .order_by(&[("name", false)])
            .fetch(tenant.pool())
            .await?;
    let perms: Vec<rustango::tenancy::permissions::RolePermission> =
        rustango::tenancy::permissions::RolePermission::objects()
            .fetch(tenant.pool())
            .await?;
    let user_roles: Vec<rustango::tenancy::permissions::UserRole> =
        rustango::tenancy::permissions::UserRole::objects()
            .fetch(tenant.pool())
            .await?;

    let mut perm_counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for p in &perms {
        *perm_counts.entry(p.role_id).or_insert(0) += 1;
    }
    let mut member_counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for ur in &user_roles {
        *member_counts.entry(ur.role_id).or_insert(0) += 1;
    }

    let rows: Vec<serde_json::Value> = roles
        .iter()
        .map(|r| {
            let id = r.id.get().copied().unwrap_or_default();
            serde_json::json!({
                "id": id,
                "name": r.name,
                "description": r.description,
                "perm_count": perm_counts.get(&id).copied().unwrap_or(0),
                "member_count": member_counts.get(&id).copied().unwrap_or(0),
            })
        })
        .collect();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "roles", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("roles", &rows);
    render_with_csrf(&state, &headers, "rcms_admin/roles_list.html", &mut ctx)
}

/// GET `/cms-admin/permissions` (#436) — tenant-wide permission
/// overview matrix. The role editor matrix shows one role at a time;
/// this is the read-only birds-eye: every resource (grouped by section)
/// × every role, each cell listing the granted actions. Answers "who
/// can publish pages?" / "what can this role touch?" at a glance.
pub async fn permissions_overview(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let roles: Vec<rustango::tenancy::permissions::Role> =
        rustango::tenancy::permissions::Role::objects()
            .order_by(&[("name", false)])
            .fetch(tenant.pool())
            .await?;
    let perms: Vec<rustango::tenancy::permissions::RolePermission> =
        rustango::tenancy::permissions::RolePermission::objects()
            .fetch(tenant.pool())
            .await?;
    let granted: std::collections::HashSet<(i64, String)> = perms
        .iter()
        .map(|p| (p.role_id, p.codename.clone()))
        .collect();

    let role_headers: Vec<serde_json::Value> = roles
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id.get().copied().unwrap_or_default(),
                "name": r.name,
            })
        })
        .collect();

    // sections → resource rows → one cell per role (granted action codes).
    let sections: Vec<serde_json::Value> = super::resources::grouped()
        .into_iter()
        .map(|(section, resources)| {
            let rows: Vec<serde_json::Value> = resources
                .iter()
                .map(|res| {
                    let cells: Vec<serde_json::Value> = roles
                        .iter()
                        .map(|r| {
                            let rid = r.id.get().copied().unwrap_or_default();
                            let actions = super::resources::granted_actions(res, rid, &granted);
                            serde_json::json!({
                                "actions": actions,
                                "any": !actions.is_empty(),
                            })
                        })
                        .collect();
                    serde_json::json!({
                        "label": res.label,
                        "key": res.key,
                        "cells": cells,
                    })
                })
                .collect();
            serde_json::json!({
                "label": section.label(),
                "rows": rows,
            })
        })
        .collect();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "permissions", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("roles", &role_headers);
    ctx.insert("sections", &sections);
    ctx.insert("role_count", &role_headers.len());
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/permissions_overview.html",
        &mut ctx,
    )
}

/// GET /cms-admin/roles/new — create form.
pub async fn roles_new_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    // Identity administration is superuser-only (#672).
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "roles", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("mode", "new");
    ctx.insert(
        "role",
        &serde_json::json!({ "id": serde_json::Value::Null, "name": "", "description": "", "permissions": [] }),
    );
    ctx.insert("matrix", &build_permission_matrix(&[]));
    ctx.insert("legacy_codenames", &Vec::<String>::new());
    render_with_csrf(&state, &headers, "rcms_admin/role_form.html", &mut ctx)
}

/// GET /cms-admin/roles/{id}/edit — edit form with permission
/// codenames pre-populated.
pub async fn roles_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    // Identity administration is superuser-only (#672).
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let role = rustango::tenancy::permissions::Role::objects()
        .where_(rustango::tenancy::permissions::Role::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let perms: Vec<rustango::tenancy::permissions::RolePermission> =
        rustango::tenancy::permissions::RolePermission::objects()
            .where_(rustango::tenancy::permissions::RolePermission::role_id.eq(id))
            .order_by(&[("codename", false)])
            .fetch(tenant.pool())
            .await?;
    let codenames: Vec<String> = perms.iter().map(|p| p.codename.clone()).collect();
    let (_matched, legacy) = split_matrix_codenames(&codenames);
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "roles", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("mode", "edit");
    ctx.insert(
        "role",
        &serde_json::json!({
            "id": role.id.get().copied(),
            "name": role.name,
            "description": role.description,
            "permissions": codenames,
        }),
    );
    ctx.insert("matrix", &build_permission_matrix(&codenames));
    ctx.insert("legacy_codenames", &legacy);
    render_with_csrf(&state, &headers, "rcms_admin/role_form.html", &mut ctx)
}

/// Build the matrix structure consumed by `role_form.html` (#33).
/// Returns one JSON object per [`super::resources::ResourceSection`]
/// with its rows pre-populated against the granted codename set so
/// the template can render checkboxes pre-checked.
///
/// The matrix is keyed by *resource key* (e.g. `pages`,
/// `library_item:author`) rather than codename so the same template
/// works regardless of which library types / custom admin pages a
/// downstream crate registers.
fn build_permission_matrix(granted: &[String]) -> serde_json::Value {
    use super::resources::{codename, grouped};
    use crate::permissions::Action;

    let granted_set: std::collections::HashSet<&str> = granted.iter().map(String::as_str).collect();

    let mut sections = Vec::new();
    for (section, rows) in grouped() {
        let rows_json: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let applicable: Vec<&str> = r.actions.iter().map(|a| a.as_str()).collect();
                let granted_actions: Vec<&str> = [
                    Action::Access,
                    Action::AccessAdmin,
                    Action::View,
                    Action::Add,
                    Action::Edit,
                    Action::Publish,
                    Action::Delete,
                ]
                .iter()
                .copied()
                .filter(|action| {
                    r.actions.contains(action)
                        && granted_set.contains(codename(r, *action).as_str())
                })
                .map(|a| a.as_str())
                .collect();
                serde_json::json!({
                    "key": r.key,
                    "label": r.label,
                    "applicable": applicable,
                    "granted": granted_actions,
                })
            })
            .collect();
        sections.push(serde_json::json!({
            "section_label": section.label(),
            "rows": rows_json,
        }));
    }
    serde_json::Value::Array(sections)
}

/// Split the supplied codename list into:
/// 1. The codenames represented in the matrix (`matched`) — these
///    get fully re-computed from the form submission.
/// 2. Everything else (`legacy`) — preserved across saves so
///    out-of-matrix grants (third-party codenames, ad-hoc strings
///    from before the matrix existed) survive a role edit.
fn split_matrix_codenames(granted: &[String]) -> (Vec<String>, Vec<String>) {
    use super::resources::{all_resources, codename};
    use crate::permissions::Action;

    let known: std::collections::HashSet<String> = all_resources()
        .iter()
        .flat_map(|r| {
            r.actions
                .iter()
                .copied()
                .filter(|a| {
                    // Mirror the matrix — only actions in `r.actions`
                    // can be toggled, so only those codenames are
                    // "matrix-owned".
                    matches!(
                        a,
                        Action::View
                            | Action::Add
                            | Action::Edit
                            | Action::Publish
                            | Action::Delete
                            | Action::Access
                            | Action::AccessAdmin
                    )
                })
                .map(|a| codename(r, a))
                .collect::<Vec<_>>()
        })
        .collect();
    let (matched, legacy): (Vec<String>, Vec<String>) =
        granted.iter().cloned().partition(|c| known.contains(c));
    (matched, legacy)
}

/// Parse the matrix POST shape (`perm[<key>][<action>]=1`) into a
/// codename list. Unknown keys / actions are silently dropped — the
/// matrix is authoritative for the resources it represents.
fn parse_matrix_form(form: &std::collections::HashMap<String, String>) -> Vec<String> {
    use super::resources::{all_resources, codename};
    use crate::permissions::Action;

    let resources = all_resources();
    let resource_by_key: std::collections::HashMap<&str, &super::resources::AdminResource> =
        resources.iter().map(|r| (r.key.as_str(), r)).collect();

    let mut out = Vec::new();
    for (raw_key, value) in form {
        // Form serializes `name="perm[<key>][<action>]"`. The bracket
        // shape stays intact in `serde_urlencoded` because it doesn't
        // try to descend nested keys — we parse manually.
        let Some(rest) = raw_key.strip_prefix("perm[") else {
            continue;
        };
        let Some((key, rest)) = rest.split_once("][") else {
            continue;
        };
        let Some(action_str) = rest.strip_suffix(']') else {
            continue;
        };
        // Checkbox semantics: present-and-truthy means granted;
        // absent means not granted. Browsers always send `1` for
        // checked boxes here.
        if value.is_empty() || value == "0" || value.eq_ignore_ascii_case("off") {
            continue;
        }
        let Some(resource) = resource_by_key.get(key) else {
            continue;
        };
        let action = match action_str {
            "view" => Action::View,
            "add" => Action::Add,
            "edit" => Action::Edit,
            "publish" => Action::Publish,
            "delete" => Action::Delete,
            "access" => Action::Access,
            "access_admin" => Action::AccessAdmin,
            _ => continue,
        };
        if !resource.actions.contains(&action) {
            continue;
        }
        out.push(codename(resource, action));
    }
    out.sort();
    out.dedup();
    out
}

/// Shared helper: replace the codename set for a role with the
/// supplied list. Idempotent — added/removed deltas only.
async fn sync_role_codenames(
    pool: &rustango::sql::Pool,
    role_id: i64,
    new_codenames: &[String],
) -> Result<(), AdminError> {
    let existing: Vec<rustango::tenancy::permissions::RolePermission> =
        rustango::tenancy::permissions::RolePermission::objects()
            .where_(rustango::tenancy::permissions::RolePermission::role_id.eq(role_id))
            .fetch(pool)
            .await?;
    let want: std::collections::BTreeSet<&str> = new_codenames.iter().map(String::as_str).collect();
    let have: std::collections::BTreeSet<&str> =
        existing.iter().map(|p| p.codename.as_str()).collect();
    // Drop the codenames that are no longer wanted.
    for p in &existing {
        if !want.contains(p.codename.as_str()) {
            p.clone().delete_pool(pool).await?;
        }
    }
    // Insert the codenames that aren't already there.
    for codename in new_codenames {
        if !have.contains(codename.as_str()) {
            let mut row = rustango::tenancy::permissions::RolePermission {
                id: rustango::sql::Auto::Unset,
                role_id,
                codename: codename.clone(),
            };
            row.insert_pool(pool).await?;
        }
    }
    Ok(())
}

/// POST /cms-admin/roles/new — create a role + sync its codenames.
/// Body shape (matrix #33): `name=`, `description=`, plus one
/// `perm[<resource>][<action>]=1` per granted cell.
pub async fn roles_create_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    // Identity administration is superuser-only (#672).
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let name = form.get("name").map(|s| s.trim()).unwrap_or("").to_owned();
    if name.is_empty() {
        return Err(AdminError::Validation("Role name is required.".to_owned()));
    }
    let description = form
        .get("description")
        .map(|s| s.trim())
        .unwrap_or("")
        .to_owned();
    let mut row = rustango::tenancy::permissions::Role {
        id: rustango::sql::Auto::Unset,
        name: name.clone(),
        description,
        data: serde_json::Value::Object(serde_json::Map::new()),
    };
    row.insert_pool(tenant.pool()).await?;
    let role_id = row.id.get().copied().unwrap_or_default();
    let codenames = parse_matrix_form(&form);
    sync_role_codenames(tenant.pool(), role_id, &codenames).await?;
    redirect_named_with_message_args(
        "rcms-admin:roles",
        MsgLevel::Success,
        "Role “{name}” created.",
        &[("name", name.as_str())],
        &headers,
    )
}

/// POST /cms-admin/roles/{id}/edit — update name/description +
/// sync the codename set. Out-of-matrix legacy codenames are
/// preserved across saves.
pub async fn roles_edit_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    // Identity administration is superuser-only (#672).
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let mut role = rustango::tenancy::permissions::Role::objects()
        .where_(rustango::tenancy::permissions::Role::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let name = form.get("name").map(|s| s.trim()).unwrap_or("").to_owned();
    if name.is_empty() {
        return Err(AdminError::Validation("Role name is required.".to_owned()));
    }
    let description = form
        .get("description")
        .map(|s| s.trim())
        .unwrap_or("")
        .to_owned();
    role.name = name.clone();
    role.description = description;
    role.save_pool(tenant.pool()).await?;
    // Carry over codenames that live outside the matrix surface so
    // legacy / third-party codenames aren't silently dropped when
    // an admin saves the matrix-only form.
    let existing: Vec<rustango::tenancy::permissions::RolePermission> =
        rustango::tenancy::permissions::RolePermission::objects()
            .where_(rustango::tenancy::permissions::RolePermission::role_id.eq(id))
            .fetch(tenant.pool())
            .await?;
    let existing_codenames: Vec<String> = existing.into_iter().map(|p| p.codename).collect();
    let (_old_matrix, legacy) = split_matrix_codenames(&existing_codenames);
    let mut codenames = parse_matrix_form(&form);
    codenames.extend(legacy);
    codenames.sort();
    codenames.dedup();
    sync_role_codenames(tenant.pool(), id, &codenames).await?;
    redirect_named_with_message_args(
        "rcms-admin:roles",
        MsgLevel::Success,
        "Role “{name}” saved.",
        &[("name", name.as_str())],
        &headers,
    )
}

/// POST /cms-admin/roles/{id}/delete — delete the role + its
/// codenames + every UserRole row referencing it.
pub async fn roles_delete_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    // Identity administration is superuser-only (#672).
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let role = rustango::tenancy::permissions::Role::objects()
        .where_(rustango::tenancy::permissions::Role::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let name = role.name.clone();
    // Delete dependent rows manually — the schema FK isn't ON DELETE CASCADE.
    for p in rustango::tenancy::permissions::RolePermission::objects()
        .where_(rustango::tenancy::permissions::RolePermission::role_id.eq(id))
        .fetch(tenant.pool())
        .await?
    {
        p.delete_pool(tenant.pool()).await?;
    }
    for ur in rustango::tenancy::permissions::UserRole::objects()
        .where_(rustango::tenancy::permissions::UserRole::role_id.eq(id))
        .fetch(tenant.pool())
        .await?
    {
        ur.delete_pool(tenant.pool()).await?;
    }
    role.delete_pool(tenant.pool()).await?;
    redirect_named_with_message_args(
        "rcms-admin:roles",
        MsgLevel::Success,
        "Role “{name}” deleted.",
        &[("name", name.as_str())],
        &headers,
    )
}

/// GET /cms-admin/users/new — create form.
pub async fn users_new_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "users", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("mode", "new");
    ctx.insert(
        "user",
        &serde_json::json!({
            "id": serde_json::Value::Null,
            "username": "",
            "is_superuser": false,
            "active": true,
        }),
    );
    // Roles can be given right away, not only after a second visit.
    let role_choices: Vec<serde_json::Value> = rustango::tenancy::permissions::Role::objects()
        .order_by(&[("name", false)])
        .fetch(tenant.pool())
        .await?
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id.get().copied().unwrap_or_default(),
                "name": r.name,
                "description": r.description,
                "assigned": false,
            })
        })
        .collect();
    ctx.insert("role_choices", &role_choices);
    render_with_csrf(&state, &headers, "rcms_admin/user_form.html", &mut ctx)
}

/// GET /cms-admin/users/{id}/edit — edit form. Password field stays
/// empty by default; leave blank to keep the existing password.
/// Includes a role-checkbox group so editors can grant / revoke
/// role memberships inline.
pub async fn users_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    render_user_edit_page(&state, &tenant, &headers, session_user.as_ref(), id, None).await
}

async fn render_user_edit_page(
    state: &super::AdminState,
    tenant: &Tenant,
    headers: &HeaderMap,
    session_user: Option<&rustango::tenancy::auth::User>,
    id: i64,
    mcp_one_time: Option<serde_json::Value>,
) -> Result<Response, AdminError> {
    let user = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let roles: Vec<rustango::tenancy::permissions::Role> =
        rustango::tenancy::permissions::Role::objects()
            .order_by(&[("name", false)])
            .fetch(tenant.pool())
            .await?;
    let memberships: Vec<rustango::tenancy::permissions::UserRole> =
        rustango::tenancy::permissions::UserRole::objects()
            .where_(rustango::tenancy::permissions::UserRole::user_id.eq(id))
            .fetch(tenant.pool())
            .await?;
    let assigned: std::collections::BTreeSet<i64> = memberships.iter().map(|m| m.role_id).collect();

    let role_choices: Vec<serde_json::Value> = roles
        .iter()
        .map(|r| {
            let rid = r.id.get().copied().unwrap_or_default();
            serde_json::json!({
                "id": rid,
                "name": r.name,
                "description": r.description,
                "assigned": assigned.contains(&rid),
            })
        })
        .collect();

    // #13 V2 — per-user timezone stored in `User.data.timezone`.
    // No framework column change required; the existing JSON bag
    // holds the IANA name. Empty / missing = browser-detected
    // fallback (the no-flash JS already handles that case).
    let user_timezone = user
        .data
        .get("timezone")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    let user_email = user_email(&user);

    let mut ctx = Context::new();
    add_chrome(&mut ctx, tenant, "users", session_user).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("mode", "edit");
    ctx.insert(
        "user",
        &serde_json::json!({
            "id": user.id.get().copied(),
            "username": user.username,
            "email": user_email,
            "is_superuser": user.is_superuser,
            "active": user.active,
            "timezone": user_timezone,
            "password_changed_at": user.password_changed_at,
            "created_at": user.created_at,
        }),
    );
    ctx.insert("role_choices", &role_choices);
    // #587 — the target user's MCP keys, for admin-managed minting
    // (the "register a claude user, give it a key" flow).
    let (keys, skills) = mcp_keys_ctx(tenant.pool(), id).await;
    ctx.insert("mcp_keys", &keys);
    ctx.insert("mcp_skills", &skills);
    if let Some(one_time) = &mcp_one_time {
        ctx.insert("mcp_one_time", one_time);
    }
    render_with_csrf(state, headers, "rcms_admin/user_form.html", &mut ctx)
}

// ------------------------------------------------------------- MCP keys (#587)

/// Parse a keys-create form: `label` (optional) + repeated `skill`
/// entries (optional scope — empty = the owner's full entitlement).
fn parse_mcp_key_form(pairs: &[(String, String)]) -> (String, Vec<String>) {
    let label = pairs
        .iter()
        .find(|(k, _)| k == "label")
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
        .unwrap_or("MCP key")
        .to_owned();
    let skills: Vec<String> = pairs
        .iter()
        .filter(|(k, v)| k == "skill" && !v.trim().is_empty())
        .map(|(_, v)| v.trim().to_owned())
        .collect();
    (label, skills)
}

/// Best-effort audit trail for key lifecycle events — explicit source
/// because the CMS admin doesn't install `audit::with_source`.
async fn audit_mcp_key(
    pool: &rustango::sql::Pool,
    actor_id: i64,
    what: &str,
    detail: serde_json::Value,
) {
    let entry = rustango::audit::PendingEntry {
        entity_table: "rustango_agents",
        entity_pk: detail
            .get("key_id")
            .and_then(serde_json::Value::as_i64)
            .map(|i| i.to_string())
            .unwrap_or_default(),
        operation: rustango::audit::AuditOp::Action,
        source: rustango::audit::AuditSource::User {
            id: actor_id.to_string(),
        },
        changes: serde_json::json!({ "event": what, "detail": detail }),
    };
    if let Err(e) = rustango::audit::emit_one_pool(pool, &entry).await {
        tracing::warn!(error = %e, "mcp key audit emit failed");
    }
}

/// Mint a user key for `owner_id`, mapping scope-validation failures to a
/// user-facing message. Returns the show-once payload for the dialog.
async fn mint_mcp_key(
    pool: &rustango::sql::Pool,
    actor_id: i64,
    owner_id: i64,
    label: &str,
    skills: &[String],
) -> Result<serde_json::Value, String> {
    match rustango::tenancy::create_user_key_pool(pool, owner_id, label, skills).await {
        Ok(issued) => {
            let key_id = issued.agent.id.get().copied().unwrap_or_default();
            audit_mcp_key(
                pool,
                actor_id,
                "created",
                serde_json::json!({ "key_id": key_id, "owner_id": owner_id, "label": label, "scope": skills }),
            )
            .await;
            Ok(serde_json::json!({ "token": issued.token, "label": label }))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// POST /cms-admin/me/mcp-keys — mint a key for the signed-in user.
/// Renders the account page directly with the show-once token (no PRG —
/// the token must never ride a redirect or a flash cookie).
pub async fn account_mcp_key_create(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, AdminError> {
    let Some(uid) = session_user.as_ref().and_then(|u| u.id.get().copied()) else {
        return Ok(unauthorized_no_session());
    };
    let (label, skills) = parse_mcp_key_form(&pairs);
    match mint_mcp_key(tenant.pool(), uid, uid, &label, &skills).await {
        Ok(one_time) => {
            render_account_page(
                &state,
                &tenant,
                &headers,
                session_user.as_ref(),
                Some(one_time),
            )
            .await
        }
        Err(msg) => redirect_named_with_message_args(
            "rcms-admin:account",
            MsgLevel::Error,
            "Couldn't create the key: {error}",
            &[("error", msg.as_str())],
            &headers,
        ),
    }
}

/// POST /cms-admin/me/mcp-keys/{id}/revoke — revoke one of the signed-in
/// user's keys (ownership-verified in the framework call).
pub async fn account_mcp_key_revoke(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(key_id): Path<i64>,
) -> Result<Response, AdminError> {
    let Some(uid) = session_user.as_ref().and_then(|u| u.id.get().copied()) else {
        return Ok(unauthorized_no_session());
    };
    match rustango::tenancy::revoke_user_key_pool(tenant.pool(), uid, key_id).await {
        Ok(()) => {
            audit_mcp_key(
                tenant.pool(),
                uid,
                "revoked",
                serde_json::json!({ "key_id": key_id, "owner_id": uid }),
            )
            .await;
            redirect_named_with_message_args(
                "rcms-admin:account",
                MsgLevel::Success,
                "MCP key revoked.",
                &[],
                &headers,
            )
        }
        Err(e) => {
            let msg = e.to_string();
            redirect_named_with_message_args(
                "rcms-admin:account",
                MsgLevel::Error,
                "Couldn't revoke the key: {error}",
                &[("error", msg.as_str())],
                &headers,
            )
        }
    }
}

/// POST /cms-admin/users/{id}/mcp-keys — an admin mints a key for another
/// user (the "register a claude user, give it a key" flow). Safe by
/// construction: the key's capabilities are bounded by the TARGET user's
/// entitlement at every token resolution, so this can't escalate beyond
/// what that user could already do.
pub async fn users_mcp_key_create(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(user_id): Path<i64>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, AdminError> {
    // #646 — user-management is superuser-only. Without this the two
    // mcp-key handlers gated on "a session exists" only, so a non-superuser
    // staff account could mint a key bounded by ANY target user's (e.g. a
    // superuser's) entitlement — a privilege escalation the other seven
    // user-admin handlers already block. Mirror them.
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let Some(actor_id) = session_user.as_ref().and_then(|u| u.id.get().copied()) else {
        return Ok(unauthorized_no_session());
    };
    let (label, skills) = parse_mcp_key_form(&pairs);
    match mint_mcp_key(tenant.pool(), actor_id, user_id, &label, &skills).await {
        Ok(one_time) => {
            render_user_edit_page(
                &state,
                &tenant,
                &headers,
                session_user.as_ref(),
                user_id,
                Some(one_time),
            )
            .await
        }
        Err(msg) => redirect_named_with_params_and_message_args(
            "rcms-admin:users:edit",
            &[("id", user_id.to_string())],
            MsgLevel::Error,
            "Couldn't create the key: {error}",
            &[("error", msg.as_str())],
            &headers,
        ),
    }
}

/// POST /cms-admin/users/{id}/mcp-keys/{key_id}/revoke — admin revoke of
/// another user's key (ownership pinned to the target user).
pub async fn users_mcp_key_revoke(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path((user_id, key_id)): Path<(i64, i64)>,
) -> Result<Response, AdminError> {
    // #646 — user-management is superuser-only. Without this the two
    // mcp-key handlers gated on "a session exists" only, so a non-superuser
    // staff account could mint a key bounded by ANY target user's (e.g. a
    // superuser's) entitlement — a privilege escalation the other seven
    // user-admin handlers already block. Mirror them.
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let Some(actor_id) = session_user.as_ref().and_then(|u| u.id.get().copied()) else {
        return Ok(unauthorized_no_session());
    };
    match rustango::tenancy::revoke_user_key_pool(tenant.pool(), user_id, key_id).await {
        Ok(()) => {
            audit_mcp_key(
                tenant.pool(),
                actor_id,
                "revoked",
                serde_json::json!({ "key_id": key_id, "owner_id": user_id }),
            )
            .await;
            redirect_named_with_params_and_message_args(
                "rcms-admin:users:edit",
                &[("id", user_id.to_string())],
                MsgLevel::Success,
                "MCP key revoked.",
                &[],
                &headers,
            )
        }
        Err(e) => {
            let msg = e.to_string();
            redirect_named_with_params_and_message_args(
                "rcms-admin:users:edit",
                &[("id", user_id.to_string())],
                MsgLevel::Error,
                "Couldn't revoke the key: {error}",
                &[("error", msg.as_str())],
                &headers,
            )
        }
    }
}

/// Form payload for create + edit. Password is optional on edit
/// (empty = "don't change"); required on create (rejected with a
/// 400 if missing).
#[derive(Deserialize)]
pub struct UserForm {
    pub username: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub is_superuser: Option<String>,
    #[serde(default)]
    pub active: Option<String>,
}

/// #435 — light email validation (presence of a single `@` with text
/// on both sides). Empty is allowed (email is optional). Returns the
/// trimmed value to store, or an error message.
fn validate_optional_email(raw: Option<&str>) -> Result<String, String> {
    let e = raw.map(str::trim).unwrap_or("");
    if e.is_empty() {
        return Ok(String::new());
    }
    let ok = e.split('@').count() == 2
        && e.split('@').all(|p| !p.is_empty())
        && !e.contains(char::is_whitespace);
    if ok {
        Ok(e.to_owned())
    } else {
        Err(format!("“{e}” is not a valid email address."))
    }
}

/// POST /cms-admin/users/new — create a new user. Password is
/// required; we hash via the framework's argon2id helper.
pub async fn users_create_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, AdminError> {
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    use rustango::sql::Auto;
    // Pairs, not the typed struct: the roles are a multi-value field
    // (see `users_edit_submit`), granted at creation too.
    let single = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let form = UserForm {
        username: single("username").unwrap_or_default(),
        email: single("email"),
        password: single("password"),
        is_superuser: single("is_superuser"),
        active: single("active"),
    };
    let role_ids: Vec<i64> = pairs
        .iter()
        .filter(|(k, _)| k == "role_id")
        .filter_map(|(_, v)| v.parse::<i64>().ok())
        .collect();
    let username = form.username.trim().to_owned();
    let password = form
        .password
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if username.is_empty() || password.is_none() {
        return Err(AdminError::Validation(
            "Username and initial password are both required.".to_owned(),
        ));
    }
    let hash = crate::passwords::hash(password.as_deref().unwrap_or_default()).await
        .map_err(|e| AdminError::Validation(format!("password hash failed: {e}")))?;
    let email = validate_optional_email(form.email.as_deref()).map_err(AdminError::Validation)?;
    // The framework's first-class `email` column is the SSO link target (the
    // admin-sso login matches the IdP's verified email against it). Set it here;
    // NULL when no email was given (that account simply can't sign in via SSO).
    let email_col = (!email.is_empty()).then(|| email.clone());

    // #435 — also mirror into the User.data bag (legacy readers: users list +
    // edit form still read `data.email`).
    let mut data = serde_json::Map::new();
    if !email.is_empty() {
        data.insert("email".to_owned(), serde_json::Value::String(email));
    }
    let mut row = rustango::tenancy::auth::User {
        id: Auto::Unset,
        username: username.clone(),
        password_hash: hash,
        email: email_col,
        is_superuser: form.is_superuser.is_some(),
        active: form.active.is_some(),
        created_at: chrono::Utc::now(),
        data: serde_json::Value::Object(data),
        password_changed_at: Some(chrono::Utc::now()),
        sessions_revoked_at: None,
    };
    row.insert_pool(tenant.pool()).await?;
    if let Some(new_id) = row.id.get().copied() {
        sync_user_roles(tenant.pool(), new_id, &role_ids).await?;
    }
    redirect_named_with_message_args(
        "rcms-admin:users",
        MsgLevel::Success,
        "User “{username}” created.",
        &[("username", username.as_str())],
        &headers,
    )
}

/// POST /cms-admin/users/{id}/edit — update flags + optionally
/// rotate the password (empty password field = no rotation) +
/// sync role memberships. Body parsed as `Vec<(String, String)>`
/// instead of the typed struct because role_id is a multi-value
/// form field — checkbox groups in HTML submit one entry per
/// checked box, and serde_urlencoded folds duplicates onto the
/// last write.
pub async fn users_edit_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, AdminError> {
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    // Pull each field out of the multi-pair vec. `find` honors the
    // first occurrence — fine for single-value fields.
    let single = |key: &str| -> Option<String> {
        form.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    };
    let username_raw = single("username").unwrap_or_default();
    let username = username_raw.trim().to_owned();
    if username.is_empty() {
        return Err(AdminError::Validation(
            "Username cannot be empty.".to_owned(),
        ));
    }
    let password = single("password");
    let is_superuser = form.iter().any(|(k, _)| k == "is_superuser");
    let active = form.iter().any(|(k, _)| k == "active");
    let timezone_raw = single("timezone").unwrap_or_default();
    let timezone = timezone_raw.trim().to_owned();
    // #435 — optional email, validated + stored in User.data.
    let email =
        validate_optional_email(single("email").as_deref()).map_err(AdminError::Validation)?;
    let role_ids: Vec<i64> = form
        .iter()
        .filter(|(k, _)| k == "role_id")
        .filter_map(|(_, v)| v.parse::<i64>().ok())
        .collect();

    let mut user = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    user.username = username.clone();
    user.is_superuser = is_superuser;
    user.active = active;
    // #13 V2 — persist the chosen IANA timezone (or unset key
    // entirely when blank) on the User.data JSON bag.
    {
        let map = user.data.as_object_mut().ok_or_else(|| {
            AdminError::Validation(
                "User.data is not a JSON object — refusing to overwrite.".to_owned(),
            )
        })?;
        if timezone.is_empty() {
            map.remove("timezone");
        } else {
            map.insert(
                "timezone".to_owned(),
                serde_json::Value::String(timezone.clone()),
            );
        }
        if email.is_empty() {
            map.remove("email");
        } else {
            map.insert("email".to_owned(), serde_json::Value::String(email.clone()));
        }
    }
    // Keep the first-class `email` column (the admin-sso link target) in sync
    // with the edited address — NULL clears the SSO link for this user.
    user.email = (!email.is_empty()).then(|| email.clone());

    let rotated = password.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(new_pw) = rotated {
        let hash = crate::passwords::hash(new_pw).await
            .map_err(|e| AdminError::Validation(format!("password hash failed: {e}")))?;
        user.password_hash = hash;
        user.password_changed_at = Some(chrono::Utc::now());
    }
    user.save_pool(tenant.pool()).await?;
    sync_user_roles(tenant.pool(), id, &role_ids).await?;
    if rotated.is_some() {
        redirect_named_with_message_args(
            "rcms-admin:users",
            MsgLevel::Success,
            "User “{username}” saved (password rotated).",
            &[("username", username.as_str())],
            &headers,
        )
    } else {
        redirect_named_with_message_args(
            "rcms-admin:users",
            MsgLevel::Success,
            "User “{username}” saved.",
            &[("username", username.as_str())],
            &headers,
        )
    }
}

/// Shared helper — replace a user's role-membership set with the
/// supplied list. Idempotent; computes adds + drops via set diff.
async fn sync_user_roles(
    pool: &rustango::sql::Pool,
    user_id: i64,
    new_role_ids: &[i64],
) -> Result<(), AdminError> {
    let existing: Vec<rustango::tenancy::permissions::UserRole> =
        rustango::tenancy::permissions::UserRole::objects()
            .where_(rustango::tenancy::permissions::UserRole::user_id.eq(user_id))
            .fetch(pool)
            .await?;
    let want: std::collections::BTreeSet<i64> = new_role_ids.iter().copied().collect();
    let have: std::collections::BTreeSet<i64> = existing.iter().map(|m| m.role_id).collect();
    for m in &existing {
        if !want.contains(&m.role_id) {
            m.clone().delete_pool(pool).await?;
        }
    }
    for role_id in new_role_ids {
        if !have.contains(role_id) {
            let mut row = rustango::tenancy::permissions::UserRole {
                id: rustango::sql::Auto::Unset,
                user_id,
                role_id: *role_id,
            };
            row.insert_pool(pool).await?;
        }
    }
    Ok(())
}

/// POST /cms-admin/users/{id}/deactivate — flip the `active` bit
/// off. Idempotent. Reversed via the edit form's checkbox.
pub async fn users_deactivate_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let mut user = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    user.active = false;
    user.save_pool(tenant.pool()).await?;
    redirect_named_with_message_args(
        "rcms-admin:users",
        MsgLevel::Success,
        "User “{username}” deactivated.",
        &[("username", user.username.as_str())],
        &headers,
    )
}

/// POST /cms-admin/users/bulk — enable or disable the selected users
/// (#435 bulk actions). `user_id` repeats once per checked row;
/// `action` is the clicked button (`enable` | `disable`). Disabling
/// your own account is skipped so an admin can't lock themselves out.
pub async fn users_bulk_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, AdminError> {
    if let Some(redirect) = require_superuser_for_user_admin(session_user.as_ref()) {
        return Ok(redirect);
    }
    let mut ids: Vec<i64> = Vec::new();
    let mut action = String::new();
    for (k, v) in &pairs {
        match k.as_str() {
            "user_id" => {
                if let Ok(id) = v.parse::<i64>() {
                    ids.push(id);
                }
            }
            "action" => action = v.clone(),
            _ => {}
        }
    }
    let target_active = match action.as_str() {
        "enable" => true,
        "disable" => false,
        _ => {
            return redirect_named_with_message(
                "rcms-admin:users",
                MsgLevel::Error,
                "Unknown bulk action.",
                &headers,
            );
        }
    };
    if ids.is_empty() {
        return redirect_named_with_message(
            "rcms-admin:users",
            MsgLevel::Info,
            "No users selected.",
            &headers,
        );
    }
    let me = session_user.as_ref().and_then(|u| u.id.get().copied());
    let mut changed = 0usize;
    let mut skipped_self = false;
    for id in ids {
        // Never disable the acting admin — that's a self-lockout.
        if !target_active && Some(id) == me {
            skipped_self = true;
            continue;
        }
        if let Some(mut user) = rustango::tenancy::auth::User::objects()
            .where_(rustango::tenancy::auth::User::id.eq(id))
            .first(tenant.pool())
            .await?
        {
            if user.active != target_active {
                user.active = target_active;
                user.save_pool(tenant.pool()).await?;
                changed += 1;
            }
        }
    }
    let verb = if target_active { "Enabled" } else { "Disabled" };
    let mut msg = format!("{verb} {changed} user(s).");
    if skipped_self {
        msg.push_str(" (Skipped your own account.)");
    }
    redirect_named_with_message("rcms-admin:users", MsgLevel::Success, &msg, &headers)
}

// =====================================================================
// Settings tab
// =====================================================================

pub async fn settings_page(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let locales: Vec<Locale> = Locale::objects()
        .order_by(&[("sort_order", false), ("code", false)])
        .fetch(tenant.pool())
        .await?;
    let media_count: Vec<Media> = Media::objects().fetch(tenant.pool()).await?;
    // #58 — themes available to the picker. We surface every row in
    // `cms_theme` plus its 4 marquee colors for a swatch preview.
    let themes: Vec<crate::theme::Theme> = crate::theme::Theme::objects()
        .order_by(&[("name", false)])
        .fetch(tenant.pool())
        .await?;
    let theme_rows: Vec<serde_json::Value> = themes
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id.get().copied(),
                "name": t.name,
                "slug": t.slug,
                "is_admin_default": t.is_admin_default,
                "is_default": t.is_default,
                "color_bg": t.color_bg,
                "color_surface": t.color_surface,
                "color_text_primary": t.color_text_primary,
                "color_link": t.color_link,
            })
        })
        .collect();
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "settings", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("locales", &locales);
    ctx.insert("media_count", &media_count.len());
    ctx.insert("org_slug", &tenant.org.slug);
    ctx.insert("org_display_name", &tenant.org.display_name);
    ctx.insert("themes", &theme_rows);

    // #199 — surface the current branding asset filenames + URLs
    // so the Branding form can render "Current logo: <filename>"
    // hints next to the upload inputs.
    if let Ok(Some(row)) = crate::site_setting::get(tenant.pool(), "branding").await {
        let logo_key = row
            .value_json
            .get("logo_storage_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let favicon_key = row
            .value_json
            .get("favicon_storage_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        ctx.insert("branding_logo_filename", &logo_key);
        ctx.insert("branding_favicon_filename", &favicon_key);
        ctx.insert(
            "branding_site_name",
            row.value_json.get("site_name").and_then(|v| v.as_str()).unwrap_or(""),
        );
    }

    render_with_csrf(&state, &headers, "rcms_admin/settings.html", &mut ctx)
}

/// POST /cms-admin/__richtext-preview — sanitize a richtext-widget's
/// textarea source through the same `ammonia` allow-list the public
/// render uses, return the cleaned HTML body so the editor's
/// preview swap matches what visitors will see (#263).
///
/// Accepts the source under a `body` form field. Other field names
/// fall through to the empty string. Output is `text/html` so the
/// client can drop it straight into `innerHTML` without re-parsing.
///
/// Session-gated like the rest of the admin POST surface — anonymous
/// requests get the friendly 401 page. CSRF is handled by the
/// framework's middleware (header-based for the fetch path the JS
/// uses).
pub async fn richtext_preview(
    State(_state): State<super::AdminState>,
    _tenant: Tenant,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    if session_user.is_none() {
        return Ok(unauthorized_no_session());
    }
    let src = form.get("body").map(String::as_str).unwrap_or("");
    let html = crate::markdown::sanitize_html(src);
    Ok((
        axum::http::StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response())
}

/// POST /cms-admin/settings/branding — multipart upload of logo
/// and/or favicon (#199). Files land in
/// storage key `<tenant>/branding/<name>` and the storage keys are
/// recorded in `cms_site_setting` under scope `branding`.
pub async fn settings_branding_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    mut multipart: axum::extract::Multipart,
) -> Result<Response, AdminError> {
    let mut logo_filename: Option<String> = None;
    let mut favicon_filename: Option<String> = None;
    let mut site_name: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AdminError::Upload(format!("multipart parse: {e}")))?
    {
        let field_name = field.name().unwrap_or("").to_owned();
        let original = field.file_name().map(|s| s.to_owned());
        let bytes = field
            .bytes()
            .await
            .map_err(|e| AdminError::Upload(format!("read field {field_name}: {e}")))?;
        // A text field; empty clears it (back to the tenant's name).
        if field_name == "site_name" {
            site_name = Some(String::from_utf8_lossy(&bytes).trim().chars().take(120).collect());
            continue;
        }
        if bytes.is_empty() {
            continue;
        }
        let (slot, default_ext) = match field_name.as_str() {
            "logo" => (&mut logo_filename, "png"),
            "favicon" => (&mut favicon_filename, "ico"),
            _ => continue,
        };
        let safe_ext = original
            .as_deref()
            .and_then(|n| n.rsplit_once('.'))
            .map(|(_, ext)| ext)
            .filter(|ext| ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| default_ext.to_owned());
        let storage_name = format!("{field_name}.{safe_ext}");
        crate::media_storage::disk_for(&tenant.org.slug)
            .save(
                &crate::media_storage::key(&tenant.org.slug, &format!("branding/{storage_name}")),
                &bytes,
            )
            .await
            .map_err(|e| AdminError::Upload(format!("write {field_name}: {e}")))?;
        *slot = Some(storage_name);
    }

    // Merge into the existing row so an upload of just the favicon
    // doesn't clobber a previously-uploaded logo.
    let existing = crate::site_setting::get(tenant.pool(), "branding")
        .await
        .unwrap_or_default()
        .map(|r| r.value_json)
        .unwrap_or_else(|| serde_json::json!({}));
    let mut value = existing.as_object().cloned().unwrap_or_default();
    if let Some(name) = logo_filename {
        value.insert("logo_storage_key".to_owned(), serde_json::json!(name));
    }
    if let Some(name) = favicon_filename {
        value.insert("favicon_storage_key".to_owned(), serde_json::json!(name));
    }
    match site_name {
        Some(n) if n.is_empty() => {
            value.remove("site_name");
        }
        Some(n) => {
            value.insert("site_name".to_owned(), serde_json::json!(n));
        }
        None => {}
    }
    let _ =
        crate::site_setting::upsert(tenant.pool(), "branding", serde_json::Value::Object(value))
            .await?;

    redirect_named_with_message(
        "rcms-admin:settings",
        MsgLevel::Success,
        "Branding saved. Reload to see the new logo / favicon.",
        &headers,
    )
}

/// GET /__cms-branding/logo — streams the per-tenant logo bytes.
/// Anonymous-OK: this is admin chrome but the favicon especially
/// needs to be reachable before login.
pub async fn settings_branding_serve_logo(headers: HeaderMap, tenant: Tenant) -> Response {
    serve_branding_asset(&headers, &tenant, "logo_storage_key").await
}

/// GET /__cms-branding/favicon — streams the per-tenant favicon.
pub async fn settings_branding_serve_favicon(headers: HeaderMap, tenant: Tenant) -> Response {
    serve_branding_asset(&headers, &tenant, "favicon_storage_key").await
}

async fn serve_branding_asset(headers: &HeaderMap, tenant: &Tenant, slot: &str) -> Response {
    let setting = match crate::site_setting::get(tenant.pool(), "branding").await {
        Ok(Some(s)) => s,
        _ => {
            return (axum::http::StatusCode::NOT_FOUND, "no branding set").into_response();
        }
    };
    let storage_name = match setting.value_json.get(slot).and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => {
            return (axum::http::StatusCode::NOT_FOUND, "asset not set").into_response();
        }
    };
    // Reject path traversal — storage_name comes from a setting we
    // wrote ourselves, but never trust round-tripped JSON.
    if storage_name.contains('/') || storage_name.contains("..") {
        return (axum::http::StatusCode::BAD_REQUEST, "invalid storage name").into_response();
    }
    let bytes = match crate::media_storage::disk_for(&tenant.org.slug)
        .load(&crate::media_storage::key(
            &tenant.org.slug,
            &format!("branding/{storage_name}"),
        ))
        .await
    {
        Ok(b) => b,
        Err(_) => {
            return (axum::http::StatusCode::NOT_FOUND, "branding bytes missing").into_response();
        }
    };
    let mime = mime_guess::from_path(&storage_name)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();
    // #199 fix — the URL is stable (`/__cms-branding/logo|favicon`) and
    // the storage key can stay the same across a re-upload, so the old
    // `public, max-age=3600` (no validator) left browsers showing the
    // OLD logo/favicon for up to an hour after a change (the reported
    // "upload didn't work"). Revalidate via a content ETag instead.
    revalidated_asset(headers, mime, bytes)
}

/// Response for a small, mutable asset served from a STABLE URL
/// (branding logo/favicon, user avatar): a content-hash ETag plus
/// `Cache-Control: no-cache` (cache, but always revalidate) so a
/// re-upload is picked up immediately — no stale-for-an-hour window —
/// while unchanged bytes still `304` cheaply (`If-None-Match` honored).
fn revalidated_asset(headers: &HeaderMap, mime: String, bytes: Vec<u8>) -> Response {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(&bytes);
    let mut etag = String::with_capacity(34);
    etag.push('"');
    for b in digest.iter().take(16) {
        use std::fmt::Write as _;
        let _ = write!(etag, "{b:02x}");
    }
    etag.push('"');
    let matches_etag = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|inm| inm.split(',').any(|t| t.trim() == etag));
    if matches_etag {
        return (
            axum::http::StatusCode::NOT_MODIFIED,
            [
                (axum::http::header::ETAG, etag),
                (axum::http::header::CACHE_CONTROL, "no-cache".to_owned()),
            ],
        )
            .into_response();
    }
    (
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (axum::http::header::CACHE_CONTROL, "no-cache".to_owned()),
            (axum::http::header::ETAG, etag),
        ],
        bytes,
    )
        .into_response()
}

/// POST /cms-admin/settings/theme — flip either `is_admin_default`
/// or `is_default` to a new theme (#58). Form fields:
///
/// - `surface`: either `admin` or `public` — picks which flag to toggle.
/// - `theme_id`: the row to set the flag on.
///
/// The flag is exclusive per surface (one row at a time carries it),
/// so we explicitly clear it from every other row in the same pass.
#[derive(Debug, Deserialize)]
pub struct ThemeSwitchForm {
    pub surface: String,
    pub theme_id: i64,
}

pub async fn settings_theme_switch(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    Form(form): Form<ThemeSwitchForm>,
) -> Result<Response, AdminError> {
    // #174 — only `surface=admin` is reachable through the UI now.
    // The `public` value is still accepted at the API level so host
    // crates that drive public-page theming externally keep working,
    // but no editor surface posts it anymore.
    if form.surface != "admin" && form.surface != "public" {
        return Err(AdminError::Validation(format!(
            "Unknown surface `{}` — expected `admin` or `public`.",
            form.surface
        )));
    }
    let target = crate::theme::Theme::objects()
        .where_(crate::theme::Theme::id.eq(form.theme_id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(form.theme_id))?;

    // Clear the exclusive flag everywhere else, then set it on the
    // picked row. Sequential ORM writes — atomicity isn't critical
    // here (worst case a transient request sees zero rows flagged
    // and falls through to the static default).
    let all_themes: Vec<crate::theme::Theme> =
        crate::theme::Theme::objects().fetch(tenant.pool()).await?;
    for mut t in all_themes {
        let target_id = target.id.get().copied().unwrap_or_default();
        let this_id = t.id.get().copied().unwrap_or_default();
        if form.surface == "admin" {
            t.is_admin_default = this_id == target_id;
        } else {
            t.is_default = this_id == target_id;
        }
        t.save_pool(tenant.pool()).await?;
    }
    // #274 — when the picker submits via fetch() (XHR), return JSON with
    // the freshly-rendered CSS + identifying bits so the client can swap
    // the `<style data-rcms-theme>` block in place. Detection: either an
    // explicit `Accept: application/json` (preferred — modern), or the
    // legacy `X-Requested-With: XMLHttpRequest` (kept so older host
    // crates that wrap the picker can still light it up). Plain form
    // submissions keep the redirect-with-flash behavior so the picker
    // degrades gracefully with JS off.
    if wants_json_response(&headers) && form.surface == "admin" {
        let target_id = target.id.get().copied().unwrap_or_default();
        let brand_colors: Vec<crate::theme::BrandColor> = crate::theme::BrandColor::objects()
            .where_(crate::theme::BrandColor::theme_id.eq(target_id))
            .order_by(&[("sort_order", false)])
            .fetch(tenant.pool())
            .await
            .unwrap_or_default();
        let css = crate::theme::emit_css(&target, &brand_colors);
        return Ok(Json(serde_json::json!({
            "ok": true,
            "id": target_id,
            "slug": target.slug,
            "name": target.name,
            "default_mode": target.default_mode,
            "css": css,
            "message": format!("Admin theme switched to “{}”.", target.name),
        }))
        .into_response());
    }
    // #174 — admin picker redirects back to Preferences (its new
    // home); the legacy `public` surface still redirects to Settings
    // so host integrations that POST manually keep their UX.
    let redirect_target = if form.surface == "admin" {
        "rcms-admin:account"
    } else {
        "rcms-admin:settings"
    };
    let theme_flash = if form.surface == "admin" {
        "Admin theme switched to “{name}”."
    } else {
        "Public theme switched to “{name}”."
    };
    redirect_named_with_message_args(
        redirect_target,
        MsgLevel::Success,
        theme_flash,
        &[("name", target.name.as_str())],
        &headers,
    )
}

/// True when the request prefers a JSON response — either set
/// `Accept: application/json` (modern fetch with `Accept` header) or
/// the legacy `X-Requested-With: XMLHttpRequest` marker. Used by the
/// theme picker (#274) so it can opt into a live-swap JSON payload
/// without losing the no-JS redirect fallback.
fn wants_json_response(headers: &HeaderMap) -> bool {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.contains("application/json") {
        return true;
    }
    headers
        .get("x-requested-with")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("XMLHttpRequest"))
}

// =====================================================================
// Redirects tab — editor-managed 301/302 maps. Backed by
// crate::redirect::Redirect; the public router consults this table
// when a slug lookup returns None (see crate::router).
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct RedirectForm {
    pub from_path: String,
    pub to_path: String,
    /// Checkbox — present when ticked, absent when not. `Some(_)` →
    /// 301 Moved Permanently. Default checked in the form template.
    #[serde(default)]
    pub is_permanent: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// #553 — checkbox: serve even when a published page exists at
    /// `from_path`.
    #[serde(default)]
    pub overrides_live: Option<String>,
    /// #553 — chooser hidden inputs; empty string when cleared.
    #[serde(default)]
    pub from_page_id: Option<String>,
    #[serde(default)]
    pub to_page_id: Option<String>,
}

impl RedirectForm {
    fn parsed_permanent(&self) -> bool {
        self.is_permanent.is_some()
    }
    fn parsed_overrides_live(&self) -> bool {
        self.overrides_live.is_some()
    }
    fn parsed_page_id(raw: &Option<String>) -> Option<i64> {
        raw.as_deref()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|id| *id > 0)
    }
    fn parsed_from_page_id(&self) -> Option<i64> {
        Self::parsed_page_id(&self.from_page_id)
    }
    fn parsed_to_page_id(&self) -> Option<i64> {
        Self::parsed_page_id(&self.to_page_id)
    }
}

/// #553 — resolve the effective from/to paths for a redirect form:
/// a picked source page snapshots its CURRENT `url_path` into
/// `from_path`; a picked destination page snapshots into `to_path`
/// (display/fallback — the serve path re-resolves it live). Returns
/// `(from_path, to_path)`; typed values pass through when no page is
/// picked.
async fn redirect_form_paths(
    pool: &rustango::sql::Pool,
    form: &RedirectForm,
) -> Result<(String, String), AdminError> {
    let mut from = form.from_path.trim().to_owned();
    let mut to = form.to_path.trim().to_owned();
    if let Some(pid) = form.parsed_from_page_id() {
        if let Some(p) = Page::objects().where_(Page::id.eq(pid)).first(pool).await? {
            from = p.url_path;
        }
    }
    if let Some(pid) = form.parsed_to_page_id() {
        if let Some(p) = Page::objects().where_(Page::id.eq(pid)).first(pool).await? {
            to = p.url_path;
        }
    }
    Ok((from, to))
}

/// #553 — save-time shadow check: a non-override rule whose
/// `from_path` matches a PUBLISHED page will never serve (live pages
/// win). Returns `true` when shadowed so the submit handlers can
/// flash a warning instead of a plain success.
async fn redirect_is_shadowed(tenant: &Tenant, from_path: &str, overrides_live: bool) -> bool {
    if overrides_live || crate::redirect::is_wildcard(from_path) {
        // Wildcards are meant to span subtrees (which may include live
        // pages); "shadow" only applies to exact non-override rules.
        return false;
    }
    matches!(
        crate::resolver::resolve_path(tenant, from_path).await,
        Ok(Some(_))
    )
}

/// #554 — reject a `from_path` with more than one `*`; a single glob
/// (typically `path/*` or `*/path`) is the supported shape. #708 — and a
/// rule whose destination matches its own from-path.
fn redirect_pattern_error(from_path: &str, to_path: &str) -> Option<&'static str> {
    if from_path.matches('*').count() > 1 {
        Some("The from-path may contain at most one “*” wildcard (e.g. /old/* or */feed).")
    } else if crate::redirect::redirect_loops(from_path, to_path) {
        Some("That redirect points back at its own from-path, so visitors would loop.")
    } else {
        None
    }
}

/// Query for the redirect list: free-text search, status filter chip,
/// sortable column + direction, and page number (#555).
#[derive(Debug, Deserialize, Default)]
pub struct RedirectListQuery {
    #[serde(default)]
    pub q: Option<String>,
    /// `all` | `active` | `disabled` | `stale`.
    #[serde(default)]
    pub filter: Option<String>,
    /// `from` | `last_used` | `created` | `hits` | `staleness`.
    #[serde(default)]
    pub sort: Option<String>,
    /// `asc` | `desc`.
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub page: Option<i64>,
}

const REDIRECTS_PER_PAGE: usize = 50;

pub async fn redirect_list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Query(q): Query<RedirectListQuery>,
) -> Result<Response, AdminError> {
    // The redirect table is small (editor-managed), so search / sort /
    // paginate happen in-memory over one fetch — same result as
    // SQL LIMIT/OFFSET without threading dialect-specific ordering.
    let mut all: Vec<crate::redirect::Redirect> = crate::redirect::Redirect::objects()
        .fetch(tenant.pool())
        .await?;
    let now = chrono::Utc::now();

    let needle = q.q.as_deref().unwrap_or("").trim().to_lowercase();
    if !needle.is_empty() {
        all.retain(|r| {
            r.from_path.to_lowercase().contains(&needle)
                || r.to_path.to_lowercase().contains(&needle)
                || r.note.to_lowercase().contains(&needle)
        });
    }

    // Chip counts reflect the search-filtered set (before the status filter).
    let count_all = all.len();
    let count_active = all.iter().filter(|r| r.is_active).count();
    let count_disabled = all.iter().filter(|r| !r.is_active).count();
    let count_stale = all
        .iter()
        .filter(|r| matches!(crate::redirect::staleness(r, now), "stale" | "unused"))
        .count();

    let filter = q.filter.as_deref().unwrap_or("all");
    all.retain(|r| match filter {
        "active" => r.is_active,
        "disabled" => !r.is_active,
        "stale" => matches!(crate::redirect::staleness(r, now), "stale" | "unused"),
        _ => true,
    });

    let sort = q.sort.as_deref().unwrap_or("from");
    let desc = q.dir.as_deref() == Some("desc");
    all.sort_by(|a, b| {
        let o = match sort {
            "last_used" => a.last_hit_at.cmp(&b.last_hit_at),
            "created" => a.created_at.get().cmp(&b.created_at.get()),
            "hits" => a.hit_count.cmp(&b.hit_count),
            "staleness" => {
                let rank = |r: &crate::redirect::Redirect| match crate::redirect::staleness(r, now)
                {
                    "disabled" => 0u8,
                    "unused" => 1,
                    "stale" => 2,
                    _ => 3,
                };
                rank(a).cmp(&rank(b))
            }
            _ => a.from_path.cmp(&b.from_path),
        };
        if desc {
            o.reverse()
        } else {
            o
        }
    });

    let total = all.len();
    let total_pages = total.div_ceil(REDIRECTS_PER_PAGE).max(1);
    let page = q.page.unwrap_or(1).clamp(1, total_pages as i64) as usize;
    let start = (page - 1) * REDIRECTS_PER_PAGE;
    let items: Vec<crate::redirect::Redirect> = all
        .into_iter()
        .skip(start)
        .take(REDIRECTS_PER_PAGE)
        .collect();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "redirects", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("items", &items);
    ctx.insert("q", &needle);
    ctx.insert("filter", filter);
    ctx.insert("sort", sort);
    ctx.insert("dir", if desc { "desc" } else { "asc" });
    ctx.insert("current_page", &page);
    ctx.insert("total_pages", &total_pages);
    ctx.insert("total", &total);
    ctx.insert("count_all", &count_all);
    ctx.insert("count_active", &count_active);
    ctx.insert("count_disabled", &count_disabled);
    ctx.insert("count_stale", &count_stale);
    render_with_csrf(&state, &headers, "rcms_admin/redirect_list.html", &mut ctx)
}

pub async fn redirect_new_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "redirects", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("mode", "new");
    ctx.insert(
        "redirect",
        &serde_json::json!({
            "from_path": "",
            "to_path": "",
            "is_permanent": true,
            "note": "",
            "hit_count": 0,
            "overrides_live": false,
            "from_page_id": null,
            "to_page_id": null,
        }),
    );
    render_with_csrf(&state, &headers, "rcms_admin/redirect_form.html", &mut ctx)
}

pub async fn redirect_new_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Form(form): Form<RedirectForm>,
) -> Result<Response, AdminError> {
    use rustango::sql::Auto;
    let (from, to) = redirect_form_paths(tenant.pool(), &form).await?;
    if from.is_empty() || to.is_empty() {
        return redirect_named_with_message(
            "rcms-admin:redirects:list",
            MsgLevel::Error,
            "From-path and to-path are both required.",
            &headers,
        );
    }
    if let Some(msg) = redirect_pattern_error(&from, &to) {
        return redirect_named_with_message(
            "rcms-admin:redirects:list",
            MsgLevel::Error,
            msg,
            &headers,
        );
    }
    let overrides_live = form.parsed_overrides_live();
    let is_permanent = form.parsed_permanent();
    let from_page_id = form.parsed_from_page_id();
    let to_page_id = form.parsed_to_page_id();
    let mut row = crate::redirect::Redirect {
        id: Auto::Unset,
        from_path: from,
        to_path: to,
        is_permanent,
        note: form.note.unwrap_or_default().trim().to_owned(),
        hit_count: 0,
        overrides_live,
        from_page_id,
        to_page_id,
        is_active: true,
        disabled_reason: String::new(),
        last_hit_at: None,
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    row.insert_pool(tenant.pool()).await?;
    redirect_save_response(&tenant, &row, overrides_live, true, &headers).await
}

/// Shared post-save flash for redirect create/edit: a strong warning
/// for a wildcard override (it can shadow many live pages), a warning
/// for an exact non-override rule shadowed by a live page, else
/// success. `is_new` picks the "Added…" vs "Saved…" wording so every
/// message stays a single fully-localizable sentence.
async fn redirect_save_response(
    tenant: &Tenant,
    row: &crate::redirect::Redirect,
    overrides_live: bool,
    is_new: bool,
    headers: &HeaderMap,
) -> Result<Response, AdminError> {
    let args = &[
        ("from", row.from_path.as_str()),
        ("to", row.to_path.as_str()),
    ];
    if overrides_live && crate::redirect::is_wildcard(&row.from_path) {
        let key = if is_new {
            "Added redirect {from} → {to} — heads up: a wildcard override can shadow many live pages under this pattern. Double-check the pattern."
        } else {
            "Saved redirect {from} → {to} — heads up: a wildcard override can shadow many live pages under this pattern. Double-check the pattern."
        };
        return redirect_named_with_message_args(
            "rcms-admin:redirects:list",
            MsgLevel::Warning,
            key,
            args,
            headers,
        );
    }
    if redirect_is_shadowed(tenant, &row.from_path, overrides_live).await {
        let key = if is_new {
            "Added redirect {from} → {to} — but a published page exists at {from}, so it won't serve until that page is unpublished (or tick “Apply even when a published page exists”)."
        } else {
            "Saved redirect {from} → {to} — but a published page exists at {from}, so it won't serve until that page is unpublished (or tick “Apply even when a published page exists”)."
        };
        return redirect_named_with_message_args(
            "rcms-admin:redirects:list",
            MsgLevel::Warning,
            key,
            args,
            headers,
        );
    }
    let key = if is_new {
        "Added redirect {from} → {to}."
    } else {
        "Saved redirect {from} → {to}."
    };
    redirect_named_with_message_args(
        "rcms-admin:redirects:list",
        MsgLevel::Success,
        key,
        args,
        headers,
    )
}

// ----- #186 — snippet CSV import (Wagtail-parity bulk migration) ---

/// One row from the snippet CSV / TSV. Mirrors the cms_snippet shape:
/// required slug + title, optional body_markdown + folder_path + a
/// flat `data` map (any extra column → data.<column_name>).
#[derive(Debug)]
struct SnippetImportRow {
    slug: String,
    title: String,
    body_markdown: String,
    folder_path: String,
    data: serde_json::Map<String, serde_json::Value>,
}

fn parse_snippet_csv(text: &str) -> Vec<SnippetImportRow> {
    // Header is required (we need to know what column is what).
    // Auto-detect TSV when tabs are present.
    let delimiter = if text.contains('\t') { '\t' } else { ',' };
    let mut lines = text.lines();
    let header = match lines.next() {
        Some(h) => h,
        None => return Vec::new(),
    };
    let cols: Vec<String> = header
        .split(delimiter)
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();
    // Need at least slug + title.
    let slug_idx = cols.iter().position(|c| c == "slug" || c == "key");
    let title_idx = cols.iter().position(|c| c == "title" || c == "name");
    let (Some(slug_idx), Some(title_idx)) = (slug_idx, title_idx) else {
        return Vec::new();
    };
    let body_idx = cols
        .iter()
        .position(|c| c == "body_markdown" || c == "body" || c == "content");
    let folder_idx = cols
        .iter()
        .position(|c| c == "folder_path" || c == "folder" || c == "path");
    let mut out = Vec::new();
    for raw in lines {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split(delimiter).map(str::trim).collect();
        let slug = parts.get(slug_idx).copied().unwrap_or("");
        let title = parts.get(title_idx).copied().unwrap_or("");
        if slug.is_empty() || title.is_empty() {
            continue;
        }
        let body_markdown = body_idx
            .and_then(|i| parts.get(i))
            .copied()
            .unwrap_or("")
            .to_owned();
        let folder_path = folder_idx
            .and_then(|i| parts.get(i))
            .copied()
            .unwrap_or("")
            .to_owned();
        // Every other column lands in `data`.
        let mut data = serde_json::Map::new();
        for (i, col) in cols.iter().enumerate() {
            if Some(i) == Some(slug_idx)
                || Some(i) == Some(title_idx)
                || Some(i) == body_idx
                || Some(i) == folder_idx
            {
                continue;
            }
            if col.is_empty() {
                continue;
            }
            let v = parts.get(i).copied().unwrap_or("");
            if !v.is_empty() {
                data.insert(col.clone(), serde_json::Value::String(v.to_owned()));
            }
        }
        out.push(SnippetImportRow {
            slug: slug.to_owned(),
            title: title.to_owned(),
            body_markdown,
            folder_path: crate::snippet::normalize_folder(&folder_path),
            data,
        });
    }
    out
}

/// GET /cms-admin/library/{type_name}/import — show upload form.
pub async fn snippet_import_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(type_name): Path<String>,
) -> Result<Response, AdminError> {
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "library", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("type_name", &type_name);
    render_with_csrf(&state, &headers, "rcms_admin/snippet_import.html", &mut ctx)
}

/// POST /cms-admin/library/{type_name}/import — bulk import snippets
/// for one library type from CSV / TSV.
pub async fn snippet_import_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(type_name): Path<String>,
    mut multipart: axum::extract::Multipart,
) -> Result<Response, AdminError> {
    use rustango::sql::Auto;
    let mut bytes: Vec<u8> = Vec::new();
    let mut filename = String::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AdminError::Upload(format!("multipart parse: {e}")))?
    {
        if field.name().unwrap_or("") == "file" {
            if let Some(name) = field.file_name() {
                filename = name.to_owned();
            }
            bytes = field
                .bytes()
                .await
                .map_err(|e| AdminError::Upload(format!("read bytes: {e}")))?
                .to_vec();
        } else {
            let _ = field.bytes().await;
        }
    }
    if bytes.is_empty() {
        return redirect_named_with_message(
            "rcms-admin:library:list",
            MsgLevel::Error,
            "No file uploaded.",
            &headers,
        );
    }
    let text = String::from_utf8_lossy(&bytes);
    let rows = parse_snippet_csv(&text);
    let total = rows.len();
    if total == 0 {
        return redirect_named_with_message(
            "rcms-admin:library:list",
            MsgLevel::Error,
            "Imported 0 snippets — CSV must have a header row with `slug` + `title` columns.",
            &headers,
        );
    }
    let mut inserted = 0usize;
    let mut skipped = 0usize;
    let _ = filename; // captured for the note below
    for row in rows {
        let mut snippet = crate::snippet::Snippet {
            id: Auto::Unset,
            type_name: type_name.clone(),
            folder_path: row.folder_path,
            slug: row.slug,
            title: row.title,
            body_markdown: row.body_markdown,
            data: serde_json::Value::Object(row.data),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        match snippet.insert_pool(tenant.pool()).await {
            Ok(_) => inserted += 1,
            Err(_) => skipped += 1,
        }
    }
    let inserted_str = inserted.to_string();
    let total_str = total.to_string();
    let skipped_str = skipped.to_string();
    redirect_named_with_message_args(
        "rcms-admin:library:list",
        MsgLevel::Success,
        "Imported {inserted}/{total} snippets ({skipped} skipped — likely duplicate slugs).",
        &[
            ("inserted", inserted_str.as_str()),
            ("total", total_str.as_str()),
            ("skipped", skipped_str.as_str()),
        ],
        &headers,
    )
}

/// GET /cms-admin/redirects/import — show the upload form (#145,
/// Wagtail parity D2).
pub async fn redirect_import_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "redirects", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/redirect_import.html",
        &mut ctx,
    )
}

/// Parsed shape of one row from the CSV / TSV upload. Either two
/// or three fields per row: `old_path`, `new_path`, optional
/// `is_permanent` (`true` / `false`, or `301` / `302`).
#[derive(Debug)]
struct RedirectImportRow {
    from_path: String,
    to_path: String,
    is_permanent: bool,
}

fn parse_redirect_csv(text: &str) -> Vec<RedirectImportRow> {
    // Best-effort CSV / TSV. Doesn't handle quoted commas inside
    // values (RFC-4180 strict) — redirects are URL paths, no
    // commas in them. TSV detection: if any line contains a tab,
    // use tab as the delimiter; otherwise comma.
    let delimiter = if text.contains('\t') { '\t' } else { ',' };
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split(delimiter).map(str::trim).collect();
        // Header row: if the first row's first two cells are exactly
        // `from_path` / `to_path` (case-insensitive), skip.
        if lineno == 0 {
            let h0 = parts.first().copied().unwrap_or("").to_ascii_lowercase();
            if h0 == "from_path" || h0 == "from" || h0 == "old" || h0 == "source" {
                continue;
            }
        }
        if parts.len() < 2 {
            continue;
        }
        let from = parts[0];
        let to = parts[1];
        if from.is_empty() || to.is_empty() {
            continue;
        }
        let is_permanent = parts
            .get(2)
            .map(|s| {
                let v = s.to_ascii_lowercase();
                !matches!(v.as_str(), "false" | "0" | "302" | "no")
            })
            .unwrap_or(true);
        out.push(RedirectImportRow {
            from_path: from.to_owned(),
            to_path: to.to_owned(),
            is_permanent,
        });
    }
    out
}

/// Serialize redirects to CSV: a header row + `from_path,to_path,
/// is_permanent`, with `is_permanent` rendered as `301`/`302` so the
/// importer ([`parse_redirect_csv`]) round-trips an export. #400
fn redirects_csv(items: &[crate::redirect::Redirect]) -> String {
    let mut out = String::from("from_path,to_path,is_permanent\n");
    for r in items {
        out.push_str(&csv_escape(&r.from_path));
        out.push(',');
        out.push_str(&csv_escape(&r.to_path));
        out.push(',');
        out.push_str(if r.is_permanent { "301" } else { "302" });
        out.push('\n');
    }
    out
}

/// GET /cms-admin/redirects/export.csv — download all redirects as a
/// CSV the importer can round-trip. #400
pub async fn redirect_export_csv(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let items: Vec<crate::redirect::Redirect> = crate::redirect::Redirect::objects()
        .order_by(&[("from_path", false)])
        .fetch(tenant.pool())
        .await?;
    let body = redirects_csv(&items);
    let filename = format!("redirects-{}.csv", chrono::Utc::now().format("%Y%m%d"));
    let mut response = ([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], body).into_response();
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(response)
}

#[cfg(test)]
mod user_email_column_tests {
    use super::user_email;

    fn user(email: Option<&str>, data: serde_json::Value) -> rustango::tenancy::auth::User {
        rustango::tenancy::auth::User {
            id: rustango::sql::Auto::Set(1),
            username: "u".into(),
            password_hash: String::new(),
            email: email.map(ToOwned::to_owned),
            is_superuser: false,
            active: true,
            created_at: chrono::Utc::now(),
            data,
            password_changed_at: None,
            sessions_revoked_at: None,
        }
    }

    #[test]
    fn column_first_then_legacy_data_bag() {
        // A member signed up on the site: the column only.
        assert_eq!(user_email(&user(Some("a@x.example"), serde_json::json!({}))), "a@x.example");
        // An admin-made user from before: the data bag only.
        assert_eq!(user_email(&user(None, serde_json::json!({"email": "b@x.example"}))), "b@x.example");
        assert_eq!(
            user_email(&user(Some("a@x.example"), serde_json::json!({"email": "old@x.example"}))),
            "a@x.example"
        );
        assert_eq!(user_email(&user(Some("  "), serde_json::json!({}))), "");
    }
}

#[cfg(test)]
mod translation_canonical_html_tests {
    use super::translation_canonical_html;

    #[test]
    fn rich_text_is_sanitized_html_other_kinds_empty() {
        let html = translation_canonical_html("richtext", "<p>A jar</p><script>x()</script>");
        assert_eq!(html, "<p>A jar</p>");
        assert_eq!(translation_canonical_html("richtext", "  "), "");
        assert_eq!(translation_canonical_html("textarea", "<p>A jar</p>"), "");
        assert_eq!(translation_canonical_html("text", "Moon jar"), "");
    }
}

#[cfg(test)]
mod user_email_tests {
    use super::validate_optional_email;

    #[test]
    fn accepts_valid_and_empty() {
        assert_eq!(validate_optional_email(Some("a@b.com")).unwrap(), "a@b.com");
        assert_eq!(
            validate_optional_email(Some("  a@b.com ")).unwrap(),
            "a@b.com"
        ); // trimmed
        assert_eq!(validate_optional_email(None).unwrap(), ""); // optional
        assert_eq!(validate_optional_email(Some("   ")).unwrap(), ""); // blank → empty
    }

    #[test]
    fn rejects_malformed() {
        assert!(validate_optional_email(Some("nope")).is_err()); // no @
        assert!(validate_optional_email(Some("a@b@c")).is_err()); // two @
        assert!(validate_optional_email(Some("@b.com")).is_err()); // empty local
        assert!(validate_optional_email(Some("a@")).is_err()); // empty domain
        assert!(validate_optional_email(Some("a b@c.com")).is_err()); // whitespace
    }
}

#[cfg(test)]
mod copy_to_locale_tests {
    use super::copy_translatable_fields;

    #[test]
    fn selects_nonempty_canonical_fields() {
        let out = copy_translatable_fields("Home", "", "  Welcome  ");
        // `seo_title` empty → skipped; whitespace-only would be too.
        assert_eq!(
            out,
            vec![
                ("title", "Home".to_owned()),
                ("seo_description", "  Welcome  ".to_owned()),
            ]
        );
    }

    #[test]
    fn all_empty_yields_nothing_to_seed() {
        assert!(copy_translatable_fields("", "   ", "").is_empty());
    }
}

#[cfg(test)]
mod redirect_csv_tests {
    use super::*;
    use rustango::sql::Auto;

    fn row(from: &str, to: &str, permanent: bool) -> crate::redirect::Redirect {
        crate::redirect::Redirect {
            id: Auto::Unset,
            from_path: from.to_owned(),
            to_path: to.to_owned(),
            is_permanent: permanent,
            note: String::new(),
            hit_count: 0,
            overrides_live: false,
            from_page_id: None,
            to_page_id: None,
            is_active: true,
            disabled_reason: String::new(),
            last_hit_at: None,
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        }
    }

    #[test]
    fn redirects_csv_header_and_301_302_mapping() {
        let csv = redirects_csv(&[row("/old", "/new", true), row("/tmp", "/dst", false)]);
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some("from_path,to_path,is_permanent"));
        assert_eq!(lines.next(), Some("/old,/new,301"));
        assert_eq!(lines.next(), Some("/tmp,/dst,302"));
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn redirects_csv_empty_is_header_only() {
        assert_eq!(redirects_csv(&[]), "from_path,to_path,is_permanent\n");
    }
}

/// POST /cms-admin/redirects/import — accepts a multipart upload of
/// CSV / TSV; bulk-inserts redirects.
pub async fn redirect_import_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    mut multipart: axum::extract::Multipart,
) -> Result<Response, AdminError> {
    use rustango::sql::Auto;
    let mut bytes: Vec<u8> = Vec::new();
    let mut filename = String::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AdminError::Upload(format!("multipart parse: {e}")))?
    {
        if field.name().unwrap_or("") == "file" {
            if let Some(name) = field.file_name() {
                filename = name.to_owned();
            }
            bytes = field
                .bytes()
                .await
                .map_err(|e| AdminError::Upload(format!("read bytes: {e}")))?
                .to_vec();
        } else {
            let _ = field.bytes().await;
        }
    }
    if bytes.is_empty() {
        return redirect_named_with_message(
            "rcms-admin:redirects:list",
            MsgLevel::Error,
            "No file uploaded.",
            &headers,
        );
    }
    let text = String::from_utf8_lossy(&bytes);
    let rows = parse_redirect_csv(&text);
    let total = rows.len();
    let mut inserted = 0usize;
    let mut skipped = 0usize;
    for row in rows {
        if redirect_pattern_error(&row.from_path, &row.to_path).is_some() {
            skipped += 1;
            continue;
        }
        let mut redirect = crate::redirect::Redirect {
            id: Auto::Unset,
            from_path: row.from_path,
            to_path: row.to_path,
            is_permanent: row.is_permanent,
            note: format!("Imported from {filename}"),
            hit_count: 0,
            overrides_live: false,
            from_page_id: None,
            to_page_id: None,
            is_active: true,
            disabled_reason: String::new(),
            last_hit_at: None,
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        match redirect.insert_pool(tenant.pool()).await {
            Ok(_) => inserted += 1,
            Err(_) => skipped += 1,
        }
    }
    let inserted_str = inserted.to_string();
    let total_str = total.to_string();
    let skipped_str = skipped.to_string();
    redirect_named_with_message_args(
        "rcms-admin:redirects:list",
        MsgLevel::Success,
        "Imported {inserted}/{total} redirects ({skipped} skipped — likely duplicates).",
        &[
            ("inserted", inserted_str.as_str()),
            ("total", total_str.as_str()),
            ("skipped", skipped_str.as_str()),
        ],
        &headers,
    )
}

pub async fn redirect_edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let row = crate::redirect::Redirect::objects()
        .where_(crate::redirect::Redirect::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "redirects", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("mode", "edit");
    ctx.insert("redirect", &row);
    render_with_csrf(&state, &headers, "rcms_admin/redirect_form.html", &mut ctx)
}

pub async fn redirect_edit_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
    Form(form): Form<RedirectForm>,
) -> Result<Response, AdminError> {
    let mut row = crate::redirect::Redirect::objects()
        .where_(crate::redirect::Redirect::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let (from, to) = redirect_form_paths(tenant.pool(), &form).await?;
    if from.is_empty() || to.is_empty() {
        return redirect_named_with_message(
            "rcms-admin:redirects:list",
            MsgLevel::Error,
            "From-path and to-path are both required.",
            &headers,
        );
    }
    if let Some(msg) = redirect_pattern_error(&from, &to) {
        return redirect_named_with_message(
            "rcms-admin:redirects:list",
            MsgLevel::Error,
            msg,
            &headers,
        );
    }
    row.from_path = from;
    row.to_path = to;
    row.is_permanent = form.parsed_permanent();
    row.overrides_live = form.parsed_overrides_live();
    row.from_page_id = form.parsed_from_page_id();
    row.to_page_id = form.parsed_to_page_id();
    row.note = form.note.unwrap_or_default().trim().to_owned();
    row.save_pool(tenant.pool()).await?;
    let overrides_live = row.overrides_live;
    redirect_save_response(&tenant, &row, overrides_live, false, &headers).await
}

/// POST /cms-admin/redirects/{id}/enable — re-enable a rule that was
/// auto-disabled when a linked page was deleted/unpublished (#553).
pub async fn redirect_enable_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let mut row = crate::redirect::Redirect::objects()
        .where_(crate::redirect::Redirect::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    row.is_active = true;
    row.disabled_reason = String::new();
    row.save_pool(tenant.pool()).await?;
    redirect_named_with_message_args(
        "rcms-admin:redirects:list",
        MsgLevel::Success,
        "Re-enabled redirect {from} → {to}.",
        &[
            ("from", row.from_path.as_str()),
            ("to", row.to_path.as_str()),
        ],
        &headers,
    )
}

pub async fn redirect_delete_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Path(id): Path<i64>,
) -> Result<Response, AdminError> {
    let row = crate::redirect::Redirect::objects()
        .where_(crate::redirect::Redirect::id.eq(id))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(id))?;
    let label = format!("{} → {}", row.from_path, row.to_path);
    row.delete_pool(tenant.pool()).await?;
    redirect_named_with_message_args(
        "rcms-admin:redirects:list",
        MsgLevel::Success,
        "Deleted redirect {label}.",
        &[("label", label.as_str())],
        &headers,
    )
}

// =====================================================================
// Helpers (existing)
// =====================================================================

// ---------- helpers ----------

/// `PageType` rows whose handler reports `is_creatable() = true`.
/// Falls back to the row's `is_creatable` column when no handler is
/// registered (stale row from a removed type).
pub(crate) fn creatable_types(all: &[PageType]) -> Vec<PageType> {
    all.iter()
        .filter(|t| {
            find_handler(&t.type_name)
                .map(|h| h.is_creatable())
                .unwrap_or(t.is_creatable)
        })
        .cloned()
        .collect()
}

/// `PageType` rows allowed at the ROOT of the tree (no parent).
///
/// A type that names `allowed_parent_types` is declaring it may only live
/// under those parents, so it cannot be a root. Without this the root picker
/// offered every creatable type and the save path accepted it, letting you
/// create e.g. an `ArticlePage` (parent-restricted to `HomePage`) at the root
/// — a tree the type rules forbid (#614). The child case has always been
/// gated by [`allowed_children_for`]; this is the missing mirror.
pub(crate) fn allowed_root_types(all: &[PageType]) -> Vec<PageType> {
    creatable_types(all)
        .into_iter()
        .filter(|t| t.allowed_parents().is_empty())
        .collect()
}

/// `PageType` rows allowed as direct children of `parent`. Consults
/// the parent's handler's `allowed_child_types` AND each candidate's
/// `allowed_parent_types`. Skip the check if either whitelist is
/// empty.
pub(crate) fn allowed_children_for(parent: &Page, all: &[PageType]) -> Vec<PageType> {
    let parent_row = all
        .iter()
        .find(|t| t.id.get().copied() == Some(parent.page_type_id));
    let parent_type_name = parent_row.map(|t| t.type_name.clone()).unwrap_or_default();
    // Rules come from the rows, so admin-made types count too (#843).
    let parent_allowed_children = parent_row.map(PageType::allowed_children).unwrap_or_default();

    creatable_types(all)
        .into_iter()
        .filter(|cand| {
            // Parent-side gate: empty list = no restriction.
            if !parent_allowed_children.is_empty()
                && !parent_allowed_children.iter().any(|t| t == &cand.type_name)
            {
                return false;
            }
            // Child-side gate.
            let allowed_parents = cand.allowed_parents();
            allowed_parents.is_empty() || allowed_parents.iter().any(|p| *p == parent_type_name)
        })
        .collect()
}

/// Empty values for a fresh create form — Tera doesn't auto-default
/// missing keys, so we pre-populate the field map.
fn empty_page_form_values() -> serde_json::Value {
    serde_json::json!({
        "title": "",
        "slug": "",
        "page_type_id": 0,
        "status": "draft",
        "seo_title": "",
        "seo_description": "",
        "robots_index": true,
        "sitemap_priority": 0.5,
        "show_in_menus": true,
        "go_live_at": serde_json::Value::Null,
        "expire_at": serde_json::Value::Null,
        "og_title": "",
        "og_description": "",
        "og_image_media_id": serde_json::Value::Null,
        "twitter_card": "summary_large_image",
    })
}

// =====================================================================
// Password reset
// =====================================================================
//
// The four screens — request form (GET), request submit (POST),
// confirm form (GET, signed-token URL), confirm submit (POST). Mounted
// by `admin::public_router` so anonymous visitors can reach them.
//
// Token model: signed-URL only (no DB table). The signing secret lives
// in `AdminState::signing_secret`, loaded once at boot. `PasswordReset`
// (rustango::auth_flows) handles issue + verify.
//
// Email model: `ConsoleMailer` for v1 — the reset URL prints to stdout
// + `tracing::info!`. Production deployments swap in `SmtpMailer` by
// editing `send_reset_email` below.
//
// Account-existence non-leak: every reset-request returns the same
// "if that account exists, check your email" page regardless of
// whether a user was found.

/// POST `/cms-admin/logout` — clear every session cookie the host
/// might have minted (bare-admin `rustango_admin_session`, tenant
/// `rustango_tenant_session`, operator `rustango_op_session`),
/// redirect to `/login`. Self-contained so the button works
/// regardless of which framework auth flow signed the user in —
/// avoids depending on the right cookie name being known here.
pub async fn cms_logout_submit() -> Response {
    let mut resp = Redirect::to("/login").into_response();
    // Reset all three cookies (`Max-Age=0` clears, the empty value
    // is a fallback for browsers that only honor expiry-by-value).
    // SameSite=Lax + HttpOnly match the framework's mint shape.
    let cookies = [
        "rustango_admin_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
        "rustango_tenant_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
        "rustango_op_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
    ];
    for c in cookies {
        if let Ok(v) = HeaderValue::from_str(c) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    resp
}

/// GET `/login` — render the CMS-branded login form. Shadows the
/// framework's default `/login` GET (which is wired to its own
/// embedded Tera + `login.html`). POST `/login` continues to be
/// handled by the framework's auth code — its handler validates
/// `username`+`password` against `rustango_users` and sets the
/// tenant session cookie.
/// GET `/cms-admin/no-access` — destination for the `cms_admin.access`
/// gate (#35). Reachable while signed in; doesn't require any
/// permission. The middleware adds an exception for this path so
/// the redirect doesn't loop.
pub async fn no_access_page(
    State(state): State<super::AdminState>,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let mut ctx = Context::new();
    ctx.insert("admin_title", &"CMS Admin");
    render_with_csrf(&state, &headers, "rcms_admin/no_access.html", &mut ctx)
}

/// GET `/cms-admin/me` — per-user appearance / text-size / accessibility
/// preferences (#27). Replaces the sidebar-footer toggles. The
/// preferences themselves persist in `localStorage` via the existing
/// no-flash boot script + the toggle JS in `_base.html`; this view
/// is purely the surface that hosts the same toggle widgets in a
/// dedicated page.
/// POST /cms-admin/dismissibles/{key} — record dismissal of the
/// banner identified by `key` for the current user (#139, Wagtail
/// parity D8). Returns 204 No Content on success.
pub async fn dismissible_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Path(key): Path<String>,
) -> Result<Response, AdminError> {
    let Some(viewer) = session_user.as_ref() else {
        return Ok(unauthorized_no_session());
    };
    let viewer_id = viewer.id.get().copied().unwrap_or_default();
    crate::dismissible::dismiss(tenant.pool(), viewer_id, &key).await?;
    Ok((axum::http::StatusCode::NO_CONTENT, "").into_response())
}

/// GET /cms-admin/dashboard — editor home / at-a-glance screen
/// (#144, Wagtail parity D1). Surfaces count widgets, recent edits,
/// items locked by the current user, and pages scheduled to publish
/// next.
pub async fn dashboard(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let pool = tenant.pool();
    let viewer_id = session_user.as_ref().and_then(|u| u.id.get().copied());

    // ---- counts ----
    let all_pages: Vec<Page> = Page::objects().fetch(pool).await.unwrap_or_default();
    let mut count_drafts = 0usize;
    let mut count_published = 0usize;
    let mut count_scheduled = 0usize;
    let mut count_archived = 0usize;
    for p in &all_pages {
        match p.status.as_str() {
            "draft" => count_drafts += 1,
            "published" => count_published += 1,
            "scheduled" => count_scheduled += 1,
            "archived" => count_archived += 1,
            _ => {}
        }
    }
    let snippet_count: Vec<crate::snippet::Snippet> = crate::snippet::Snippet::objects()
        .fetch(pool)
        .await
        .unwrap_or_default();
    let media_rows: Vec<Media> = Media::objects().fetch(pool).await.unwrap_or_default();
    let media_images = media_rows.iter().filter(|m| m.kind == "image").count();
    let media_docs = media_rows.len() - media_images;

    // ---- recent edits (last 10 revisions across all pages) ----
    let revs: Vec<crate::revision::Revision> = crate::revision::Revision::objects()
        .order_by(&[("created_at", true)]) // desc
        .limit(10)
        .fetch(pool)
        .await
        .unwrap_or_default();
    // Who made them, by name (one query).
    let user_ids: std::collections::BTreeSet<i64> = revs.iter().filter_map(|r| r.created_by).collect();
    let usernames: std::collections::HashMap<i64, String> = if user_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        rustango::tenancy::auth::User::objects()
            .where_(rustango::tenancy::auth::User::id.is_in(user_ids.into_iter()))
            .fetch(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|u| u.id.get().copied().map(|id| (id, u.username)))
            .collect()
    };
    let page_by_id: std::collections::HashMap<i64, &Page> = all_pages
        .iter()
        .filter_map(|p| p.id.get().copied().map(|id| (id, p)))
        .collect();
    let recent_edits: Vec<serde_json::Value> = revs
        .iter()
        .filter_map(|r| {
            let p = page_by_id.get(&r.page_id)?;
            Some(serde_json::json!({
                "page_id": r.page_id,
                "title": p.title,
                "url_path": p.url_path,
                "sequence": r.sequence,
                "created_at": r.created_at.get().copied(),
                "created_by": r.created_by.and_then(|id| usernames.get(&id).cloned()),
            }))
        })
        .collect();

    // ---- locked by me ----
    let locks: Vec<crate::lock::PageLock> = crate::lock::PageLock::objects()
        .fetch(pool)
        .await
        .unwrap_or_default();
    let now = chrono::Utc::now();
    let mine_locked: Vec<serde_json::Value> = locks
        .iter()
        .filter(|l| !l.is_stale(now))
        .filter(|l| Some(l.user_id) == viewer_id)
        .filter_map(|l| {
            let p = page_by_id.get(&l.page_id)?;
            Some(serde_json::json!({
                "page_id": l.page_id,
                "title": p.title,
                "url_path": p.url_path,
                "acquired_at": l.acquired_at.get().copied(),
            }))
        })
        .collect();

    // ---- waiting for your review ----
    // Pages mid-review whose current step belongs to one of the viewer's
    // roles (a superuser decides every step). Without this the reviewer
    // had no way to know someone was waiting.
    let mut review_rows: Vec<serde_json::Value> = Vec::new();
    if let Some(viewer) = session_user.as_ref() {
        let states: Vec<crate::workflow::WorkflowState> = crate::workflow::WorkflowState::objects()
            .where_(crate::workflow::WorkflowState::status.eq(crate::workflow::WorkflowStatus::InProgress.as_str().to_owned()))
            .fetch(pool)
            .await
            .unwrap_or_default();
        if !states.is_empty() {
            let my_roles: std::collections::BTreeSet<i64> = rustango::tenancy::permissions::UserRole::objects()
                .where_(rustango::tenancy::permissions::UserRole::user_id.eq(viewer_id.unwrap_or_default()))
                .fetch(pool)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|m| m.role_id)
                .collect();
            let task_ids: Vec<i64> = states.iter().filter_map(|st| st.current_task_id).collect();
            let tasks: std::collections::HashMap<i64, crate::workflow::WorkflowTask> = crate::workflow::WorkflowTask::objects()
                .where_(crate::workflow::WorkflowTask::id.is_in(task_ids.into_iter()))
                .fetch(pool)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter_map(|t| t.id.get().copied().map(|id| (id, t)))
                .collect();
            for st in &states {
                let Some(task) = st.current_task_id.and_then(|id| tasks.get(&id)) else { continue };
                if !viewer.is_superuser && !my_roles.contains(&task.role_id) {
                    continue;
                }
                let Some(p) = page_by_id.get(&st.page_id) else { continue };
                review_rows.push(serde_json::json!({
                    "page_id": st.page_id,
                    "title": p.title,
                    "url_path": p.url_path,
                    "step": task.name,
                    "requested_at": st.requested_at.get().copied(),
                }));
            }
        }
    }

    // ---- scheduled next ----
    let mut scheduled: Vec<&Page> = all_pages
        .iter()
        .filter(|p| {
            p.status == "scheduled" && p.go_live_at.is_some() && p.go_live_at.unwrap() > now
        })
        .collect();
    scheduled.sort_by_key(|p| p.go_live_at);
    scheduled.truncate(5);
    let scheduled_rows: Vec<serde_json::Value> = scheduled
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "page_id": p.id.get().copied().unwrap_or_default(),
                "title": p.title,
                "url_path": p.url_path,
                "go_live_at": p.go_live_at,
            })
        })
        .collect();

    // ---- plugin widgets ----
    let plugin_widgets: Vec<serde_json::Value> = crate::hooks::dashboard_widgets()
        .into_iter()
        .map(|w| {
            serde_json::json!({
                "key": w.key,
                "label": w.label,
                "icon": w.icon,
                "html": w.html,
            })
        })
        .collect();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "dashboard", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("review_rows", &review_rows);
    ctx.insert("count_drafts", &count_drafts);
    ctx.insert("count_published", &count_published);
    ctx.insert("count_scheduled", &count_scheduled);
    ctx.insert("count_archived", &count_archived);
    ctx.insert("count_snippets", &snippet_count.len());
    ctx.insert("count_images", &media_images);
    ctx.insert("count_documents", &media_docs);
    ctx.insert("recent_edits", &recent_edits);
    ctx.insert("mine_locked", &mine_locked);
    ctx.insert("scheduled_rows", &scheduled_rows);
    ctx.insert("plugin_widgets", &plugin_widgets);
    render_with_csrf(&state, &headers, "rcms_admin/dashboard.html", &mut ctx)
}

/// GET /cms-admin/styleguide — component gallery (#129, Wagtail
/// parity C8). Lists every registered block + every widget kind so
/// editors + reviewers can see the available palette at a glance.
pub async fn styleguide(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "styleguide", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;

    // Registered blocks — one row per registration. Author-curated
    // sample data lives in the template (the trait surface doesn't
    // require blocks to ship examples).
    let blocks: Vec<serde_json::Value> = crate::block::registered_blocks()
        .map(|b| {
            serde_json::json!({
                "type_name": b.type_name(),
                "verbose_name": b.verbose_name(),
                "icon": b.icon(),
                "group": b.group(),
                "field_count": b.fields().len(),
            })
        })
        .collect();
    ctx.insert("blocks", &blocks);

    // Registered page types — second registry editors care about.
    let page_types: Vec<serde_json::Value> = crate::page_type::registered_handlers()
        .map(|h| {
            serde_json::json!({
                "type_name": h.type_name(),
                "verbose_name": h.verbose_name(),
                "icon": h.icon(),
                "description": h.description(),
            })
        })
        .collect();
    ctx.insert("page_types", &page_types);

    // Library types.
    let library_types: Vec<serde_json::Value> = crate::library::registered_handlers()
        .map(|h| {
            serde_json::json!({
                "type_name": h.type_name(),
                "verbose_name": h.verbose_name(),
            })
        })
        .collect();
    ctx.insert("library_types", &library_types);

    render_with_csrf(&state, &headers, "rcms_admin/styleguide.html", &mut ctx)
}

pub async fn account_preferences(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    render_account_page(&state, &tenant, &headers, session_user.as_ref(), None).await
}

/// Serialize a user's MCP keys (+ per-key skill scope) and the tenant's
/// skill catalog for the key-management cards (#587).
async fn mcp_keys_ctx(
    pool: &rustango::sql::Pool,
    owner_id: i64,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    use rustango::tenancy::{AgentGrant, AgentSkill};

    let keys = rustango::tenancy::list_user_keys_pool(pool, owner_id)
        .await
        .unwrap_or_default();
    let skills: Vec<AgentSkill> = rustango::tenancy::list_skills_pool(pool)
        .await
        .unwrap_or_default();
    let skill_name = |sid: i64| -> Option<String> {
        skills
            .iter()
            .find(|s| s.id.get().copied() == Some(sid))
            .map(|s| s.codename.clone())
    };
    let mut rows = Vec::with_capacity(keys.len());
    for key in &keys {
        let key_id = key.id.get().copied().unwrap_or_default();
        let grants: Vec<AgentGrant> = AgentGrant::objects()
            .where_(AgentGrant::agent_id.eq(key_id))
            .fetch(pool)
            .await
            .unwrap_or_default();
        let scope: Vec<String> = grants
            .iter()
            .filter_map(|g| skill_name(g.skill_id))
            .collect();
        rows.push(serde_json::json!({
            "id": key_id,
            "label": key.data.get("label").and_then(|v| v.as_str()).unwrap_or(&key.name),
            "prefix": key.secret_prefix,
            "active": key.active,
            "created_at": key.created_at.get().map(|t| t.format("%Y-%m-%d %H:%M").to_string()),
            "scope": scope,
        }));
    }
    let skill_rows: Vec<serde_json::Value> = skills
        .iter()
        .map(|s| {
            serde_json::json!({
                "codename": s.codename,
                "name": s.name,
                "description": s.description,
            })
        })
        .collect();
    (rows, skill_rows)
}

async fn render_account_page(
    state: &super::AdminState,
    tenant: &Tenant,
    headers: &HeaderMap,
    session_user: Option<&rustango::tenancy::auth::User>,
    mcp_one_time: Option<serde_json::Value>,
) -> Result<Response, AdminError> {
    // #202 — surface the current + pending email + mailer presence so
    // the Profile card can render the change-email form variant.
    let (current_email, pending_email, mailer_wired) = {
        let mailer = state.mailer.is_some();
        let user_data = session_user
            .map(|u| u.data.clone())
            .unwrap_or_else(|| serde_json::json!({}));
        let cur = session_user.map(user_email).unwrap_or_default();
        let pending = user_data
            .get("pending_email")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        (cur, pending, mailer)
    };
    let mut ctx = Context::new();
    add_chrome(&mut ctx, tenant, "", session_user).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    // #174 — admin theme picker lives in Preferences (was on
    // Settings). Surface every cms_theme row + its 4 marquee
    // colors for a swatch preview.
    let themes: Vec<crate::theme::Theme> = crate::theme::Theme::objects()
        .order_by(&[("name", false)])
        .fetch(tenant.pool())
        .await
        .unwrap_or_default();
    let theme_rows: Vec<serde_json::Value> = themes
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id.get().copied(),
                "name": t.name,
                "slug": t.slug,
                "is_admin_default": t.is_admin_default,
                "color_bg": t.color_bg,
                "color_surface": t.color_surface,
                "color_text_primary": t.color_text_primary,
                "color_link": t.color_link,
            })
        })
        .collect();
    ctx.insert("themes", &theme_rows);
    // #128 — plugin-contributed Account settings panels (Wagtail
    // parity C6). Each panel is a pre-rendered HTML fragment hosts
    // contribute via `register_account_settings_panel!`.
    let account_panels: Vec<serde_json::Value> = crate::hooks::account_settings_panels()
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "key": p.key,
                "label": p.label,
                "icon": p.icon,
                "html": p.html,
            })
        })
        .collect();
    ctx.insert("account_panels", &account_panels);

    // #197 — per-user notification preferences. We pass every kind
    // through with its current toggle state so the template renders
    // checked/unchecked without a second fetch. Missing rows default
    // to true (opt-out semantics).
    let pref_map = session_user
        .and_then(|u| u.id.get().copied())
        .map(|uid| crate::notification_pref::map_for_user(tenant.pool(), uid))
        .map(|fut| async move { fut.await.unwrap_or_default() });
    let pref_lookup: std::collections::HashMap<String, bool> = match pref_map {
        Some(f) => f.await,
        None => std::collections::HashMap::new(),
    };
    let notification_rows: Vec<serde_json::Value> = crate::notification_pref::ALL_KINDS
        .iter()
        .map(|(label, kind, hint)| {
            let enabled = pref_lookup.get(*kind).copied().unwrap_or(true);
            serde_json::json!({
                "label": label,
                "kind": kind,
                "hint": hint,
                "enabled": enabled,
            })
        })
        .collect();
    ctx.insert("notification_prefs", &notification_rows);

    ctx.insert("account_email", &current_email);
    ctx.insert("account_pending_email", &pending_email);
    ctx.insert("account_mailer_wired", &mailer_wired);

    // #587 — MCP keys card: the viewer's keys + the skill catalog for
    // the optional-scope picker; `mcp_one_time` carries a just-minted
    // token for the show-once dialog.
    if let Some(uid) = session_user.and_then(|u| u.id.get().copied()) {
        let (keys, skills) = mcp_keys_ctx(tenant.pool(), uid).await;
        ctx.insert("mcp_keys", &keys);
        ctx.insert("mcp_skills", &skills);
    }
    if let Some(one_time) = &mcp_one_time {
        ctx.insert("mcp_one_time", one_time);
    }

    render_with_csrf(
        state,
        headers,
        "rcms_admin/account_preferences.html",
        &mut ctx,
    )
}

/// POST /cms-admin/me/notifications — save the on/off toggles for
/// every notification kind (#197). The form posts a checkbox per kind;
/// absent inputs mean "off". Iterates every known kind so the user
/// can disable categories from a single submit.
pub async fn account_notifications_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let Some(viewer) = session_user.as_ref() else {
        return Ok(unauthorized_no_session());
    };
    let Some(uid) = viewer.id.get().copied() else {
        return Ok((axum::http::StatusCode::UNAUTHORIZED, "no user id").into_response());
    };
    for (_label, kind, _hint) in crate::notification_pref::ALL_KINDS {
        let key = format!("kind_{kind}");
        let enabled = form
            .get(&key)
            .map(|v| matches!(v.as_str(), "on" | "true" | "1" | "yes"))
            .unwrap_or(false);
        crate::notification_pref::set_enabled(tenant.pool(), uid, kind, enabled).await?;
    }
    redirect_named_with_params_and_message(
        "rcms-admin:account",
        &[],
        MsgLevel::Success,
        "Notification preferences saved.",
        &headers,
    )
}

/// POST /cms-admin/me/profile — display name + avatar upload (#201).
/// Multipart submit; absence of either field leaves the existing
/// value untouched. Avatar bytes land at
/// `./var/avatars/<tenant-slug>/<user_id>.<ext>` (slug-namespaced so
/// per-tenant user ids don't collide) and the filename is recorded in
/// `rustango_users.data.avatar_path`.
pub async fn account_profile_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    mut multipart: axum::extract::Multipart,
) -> Result<Response, AdminError> {
    let Some(viewer) = session_user.as_ref() else {
        return Ok(unauthorized_no_session());
    };
    let Some(uid) = viewer.id.get().copied() else {
        return Ok((axum::http::StatusCode::UNAUTHORIZED, "no user id").into_response());
    };

    let mut new_display_name: Option<String> = None;
    let mut new_avatar_path: Option<String> = None;
    // Namespace avatars by tenant slug. Per-tenant DBs each start user
    // ids at 1, so a flat `./var/avatars/{uid}.{ext}` collided across
    // tenants (tenant B's user #1 overwriting / exposing tenant A's).
    let avatar_dir = std::path::PathBuf::from("./var/avatars").join(&tenant.org.slug);

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AdminError::Upload(format!("multipart parse: {e}")))?
    {
        let field_name = field.name().unwrap_or("").to_owned();
        match field_name.as_str() {
            "display_name" => {
                new_display_name = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| AdminError::Upload(format!("read display_name: {e}")))?
                        .trim()
                        .to_owned(),
                );
            }
            "avatar" => {
                let original = field.file_name().map(|s| s.to_owned());
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| AdminError::Upload(format!("read avatar: {e}")))?;
                if bytes.is_empty() {
                    continue;
                }
                let ext = original
                    .as_deref()
                    .and_then(|n| n.rsplit_once('.'))
                    .map(|(_, ext)| ext)
                    .filter(|ext| ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_else(|| "png".to_owned());
                std::fs::create_dir_all(&avatar_dir)
                    .map_err(|e| AdminError::Upload(format!("create avatar dir: {e}")))?;
                let storage_name = format!("{uid}.{ext}");
                let path = avatar_dir.join(&storage_name);
                std::fs::write(&path, &bytes)
                    .map_err(|e| AdminError::Upload(format!("write avatar: {e}")))?;
                new_avatar_path = Some(storage_name);
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    // Reload the user row + merge changes into `.data`. Using the
    // pool fetch (rather than mutating the extracted `viewer`) keeps
    // the save isolated from any session-cache invariants.
    let mut user_rows: Vec<rustango::tenancy::auth::User> =
        rustango::tenancy::auth::User::objects()
            .where_(rustango::tenancy::auth::User::id.eq(uid))
            .fetch(tenant.pool())
            .await?;
    let Some(mut user) = user_rows.pop() else {
        return Ok((axum::http::StatusCode::NOT_FOUND, "user not found").into_response());
    };

    let mut data = user.data.as_object().cloned().unwrap_or_default();
    if let Some(name) = new_display_name {
        if name.is_empty() {
            data.remove("display_name");
        } else {
            data.insert("display_name".to_owned(), serde_json::json!(name));
        }
    }
    if let Some(path) = new_avatar_path {
        data.insert("avatar_path".to_owned(), serde_json::json!(path));
    }
    user.data = serde_json::Value::Object(data);
    user.save_pool(tenant.pool()).await?;

    redirect_named_with_message(
        "rcms-admin:account",
        MsgLevel::Success,
        "Profile saved.",
        &headers,
    )
}

/// Persist an editor's preferred admin UI locale to
/// `rustango_users.data.admin_lang` (#526). Shared by the account-prefs form and
/// the sidebar switcher so both keep the durable pref in sync with the cookie.
async fn persist_user_admin_lang(
    pool: &rustango::sql::Pool,
    uid: i64,
    lang: &str,
) -> Result<(), AdminError> {
    let mut rows: Vec<rustango::tenancy::auth::User> = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.eq(uid))
        .fetch(pool)
        .await?;
    if let Some(mut user) = rows.pop() {
        let mut data = user.data.as_object().cloned().unwrap_or_default();
        data.insert("admin_lang".to_owned(), serde_json::json!(lang));
        user.data = serde_json::Value::Object(data);
        user.save_pool(pool).await?;
    }
    Ok(())
}

/// Form body for `POST /cms-admin/me/language` (#526).
#[derive(Debug, Deserialize)]
pub struct AccountLanguageForm {
    pub lang: String,
}

/// POST /cms-admin/me/language — set the editor's preferred admin UI locale
/// (#526). Persists to `rustango_users.data.admin_lang` (durable + cross-device,
/// folded into negotiation by `add_user_chrome` → `render_with_csrf`) AND sets
/// the sticky `rcms_admin_lang` cookie so the change applies on the next render.
/// An unshipped code resets to English.
pub async fn account_language_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(form): Form<AccountLanguageForm>,
) -> Result<Response, AdminError> {
    let Some(viewer) = session_user.as_ref() else {
        return Ok(unauthorized_no_session());
    };
    let Some(uid) = viewer.id.get().copied() else {
        return Ok((axum::http::StatusCode::UNAUTHORIZED, "no user id").into_response());
    };
    let lang = if super::i18n::is_ui_locale(&form.lang) {
        form.lang.clone()
    } else {
        "en".to_owned()
    };
    persist_user_admin_lang(tenant.pool(), uid, &lang).await?;

    let mut resp = redirect_named_with_message(
        "rcms-admin:account",
        MsgLevel::Success,
        "Language updated.",
        &headers,
    )?;
    let cookie = format!(
        "{}={lang}; Path=/; SameSite=Lax; Max-Age=31536000",
        super::i18n::ADMIN_LANG_COOKIE
    );
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    Ok(resp)
}

/// Form body for `POST /cms-admin/me/email` (#202).
#[derive(Debug, Deserialize)]
pub struct AccountEmailForm {
    pub new_email: String,
}

/// POST /cms-admin/me/email — request an email change (#202).
///
/// When a mailer is wired, the change goes through a confirmation
/// flow: a random 32-hex token is stashed on `user.data.pending_email_*`
/// and the new address receives a confirm link. Clicking the link
/// finalizes the change.
///
/// When no mailer is wired, the change applies immediately + a banner
/// explains no confirmation email was sent.
pub async fn account_email_submit(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(form): Form<AccountEmailForm>,
) -> Result<Response, AdminError> {
    let Some(viewer) = session_user.as_ref() else {
        return Ok(unauthorized_no_session());
    };
    let Some(uid) = viewer.id.get().copied() else {
        return Ok((axum::http::StatusCode::UNAUTHORIZED, "no user id").into_response());
    };

    let new_email = form.new_email.trim().to_owned();
    if new_email.is_empty() || !new_email.contains('@') {
        return redirect_named_with_message(
            "rcms-admin:account",
            MsgLevel::Error,
            "Enter a valid email address.",
            &headers,
        );
    }

    let mut user = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.eq(uid))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(uid))?;
    let mut data = user.data.as_object().cloned().unwrap_or_default();

    if state.mailer.is_none() {
        // No mailer → write directly + banner.
        data.insert("email".to_owned(), serde_json::json!(new_email));
        user.email = Some(new_email.clone());
        // Clear any in-flight confirmation token from a previous attempt.
        data.remove("pending_email");
        user.data = serde_json::Value::Object(data);
        user.save_pool(tenant.pool()).await?;
        return redirect_named_with_message(
            "rcms-admin:account",
            MsgLevel::Warning,
            "Email updated. No confirmation email was sent — the host hasn't wired a mailer.",
            &headers,
        );
    }

    // Stash the pending email so the editor sees a banner + the
    // confirm handler can replay-guard against a different change
    // having kicked off in the meantime. Drop legacy keys from the
    // previous hand-rolled token scheme so they don't shadow the
    // new flow.
    data.insert(
        "pending_email".to_owned(),
        serde_json::json!(new_email.clone()),
    );
    data.remove("pending_email_token");
    data.remove("pending_email_expires_at");
    user.data = serde_json::Value::Object(data);
    user.save_pool(tenant.pool()).await?;

    // Send the confirmation. Best-effort. The signed URL carries
    // user_id + email + TTL — `EmailVerification::verify` on the
    // confirm endpoint replaces the hand-rolled (token, expires_at)
    // round-trip we used to stash in user.data.
    if let Some(mailer) = state.mailer.as_deref() {
        let link = rustango::auth_flows::EmailVerification::issue(
            "/cms-admin/me/email/confirm",
            uid,
            &new_email,
            state.signing_secret.as_slice(),
            std::time::Duration::from_secs(3600),
        );
        let subject = "Confirm your CMS email change";
        let body = format!(
            "Click the link below to confirm your CMS email change to {new_email}.\n\nLink: {link}\n\nThe link expires in one hour. If you didn't request this change, ignore this email.\n",
        );
        let email = rustango::email::Email::new()
            .from((*state.mailer_from).clone())
            .to(new_email.clone())
            .subject(subject.to_owned())
            .body(body);
        if let Err(e) = mailer.send(&email).await {
            tracing::warn!(
                target: "rustango_cms::admin::account",
                user_id = uid, error = %e,
                "email-change confirmation send failed (pending_email stashed; user can re-request)"
            );
        }
    }

    redirect_named_with_message(
        "rcms-admin:account",
        MsgLevel::Success,
        "Confirmation link sent to the new email. Click it within an hour to finalize the change.",
        &headers,
    )
}

/// GET /cms-admin/me/email/confirm — verify the signed URL from
/// [`account_email_submit`] (`EmailVerification::verify`) and apply
/// the pending email change. The URL itself carries user_id + new
/// email + TTL signature — we just check the click came from the
/// account that started the change and that the change is still
/// the one the user wants to apply.
pub async fn account_email_confirm(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> Result<Response, AdminError> {
    let Some(viewer) = session_user.as_ref() else {
        return Ok(unauthorized_no_session());
    };
    let Some(uid) = viewer.id.get().copied() else {
        return Ok((axum::http::StatusCode::UNAUTHORIZED, "no user id").into_response());
    };

    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or_default()
        .to_owned();
    let (token_uid, signed_email) = match rustango::auth_flows::EmailVerification::verify(
        &path_and_query,
        state.signing_secret.as_slice(),
    ) {
        Ok(pair) => pair,
        Err(_) => {
            return redirect_named_with_message(
                "rcms-admin:account",
                MsgLevel::Error,
                "Confirmation link is invalid or has expired. Request a new one.",
                &headers,
            );
        }
    };
    // Clicking your own confirm link from a different session is a
    // common case (verify from inbox on phone, signed in on
    // desktop). We require the session user to match the user_id
    // baked into the signed URL — anything else is a cross-account
    // hijack attempt.
    if token_uid != uid {
        return redirect_named_with_message(
            "rcms-admin:account",
            MsgLevel::Error,
            "Confirmation link belongs to a different account. Sign in as that account to confirm.",
            &headers,
        );
    }

    let mut user = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.eq(uid))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(uid))?;
    let mut data = user.data.as_object().cloned().unwrap_or_default();
    // Stale-link guard: if the user kicked off a different change
    // since this link was minted (or none at all), reject — the
    // signed URL is still valid but no longer the one they want.
    let pending = data
        .get("pending_email")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if pending != signed_email {
        return redirect_named_with_message(
            "rcms-admin:account",
            MsgLevel::Error,
            "This link is for a different email change than the one currently pending. Request a fresh confirmation.",
            &headers,
        );
    }

    data.insert("email".to_owned(), serde_json::json!(signed_email));
    user.email = Some(signed_email.clone());
    data.remove("pending_email");
    // Keys the earlier email-change flow stored — dropped on confirm so
    // old rows clean themselves up.
    data.remove("pending_email_token");
    data.remove("pending_email_expires_at");
    user.data = serde_json::Value::Object(data);
    user.save_pool(tenant.pool()).await?;

    redirect_named_with_message(
        "rcms-admin:account",
        MsgLevel::Success,
        "Email confirmed. Notifications go to the new address from now on.",
        &headers,
    )
}

/// Form body for `POST /cms-admin/me/password`.
#[derive(Deserialize)]
pub struct AccountPasswordForm {
    pub current_password: String,
    pub new_password: String,
    pub new_password_confirm: String,
}

/// POST /cms-admin/me/password — verify current password, hash + write
/// the new one (#202).
pub async fn account_password_submit(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    Form(form): Form<AccountPasswordForm>,
) -> Result<Response, AdminError> {
    let Some(viewer) = session_user.as_ref() else {
        return Ok(unauthorized_no_session());
    };
    let Some(uid) = viewer.id.get().copied() else {
        return Ok((axum::http::StatusCode::UNAUTHORIZED, "no user id").into_response());
    };
    if form.new_password.len() < 12 {
        return redirect_named_with_message(
            "rcms-admin:account",
            MsgLevel::Error,
            "New password must be at least 12 characters.",
            &headers,
        );
    }
    if form.new_password != form.new_password_confirm {
        return redirect_named_with_message(
            "rcms-admin:account",
            MsgLevel::Error,
            "New password and confirmation don't match.",
            &headers,
        );
    }

    let mut user = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.eq(uid))
        .first(tenant.pool())
        .await?
        .ok_or(AdminError::NotFound(uid))?;

    let verified =
        crate::passwords::verify(&form.current_password, &user.password_hash).await.unwrap_or(false);
    if !verified {
        return redirect_named_with_message(
            "rcms-admin:account",
            MsgLevel::Error,
            "Current password is incorrect.",
            &headers,
        );
    }
    let hash = crate::passwords::hash(&form.new_password).await
        .map_err(|e| AdminError::Validation(format!("password hash failed: {e}")))?;
    user.password_hash = hash;
    user.password_changed_at = Some(chrono::Utc::now());
    user.save_pool(tenant.pool()).await?;

    redirect_named_with_message(
        "rcms-admin:account",
        MsgLevel::Success,
        "Password changed. Old sessions on other devices will be signed out on next request.",
        &headers,
    )
}

/// GET /__cms-avatar/{user_id} — stream the user's avatar bytes.
/// Anonymous-OK so the sidebar avatar shows on the dashboard
/// pre-login. Image not found → 404.
pub async fn account_avatar_serve(
    headers: HeaderMap,
    tenant: Tenant,
    Path(user_id): Path<i64>,
) -> Response {
    let user_rows: Vec<rustango::tenancy::auth::User> =
        match rustango::tenancy::auth::User::objects()
            .where_(rustango::tenancy::auth::User::id.eq(user_id))
            .fetch(tenant.pool())
            .await
        {
            Ok(v) => v,
            Err(_) => {
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "user lookup failed",
                )
                    .into_response();
            }
        };
    let Some(user) = user_rows.into_iter().next() else {
        return (axum::http::StatusCode::NOT_FOUND, "user not found").into_response();
    };
    let storage_name = match user.data.get("avatar_path").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => return (axum::http::StatusCode::NOT_FOUND, "no avatar").into_response(),
    };
    if storage_name.contains('/') || storage_name.contains("..") {
        return (axum::http::StatusCode::BAD_REQUEST, "invalid storage name").into_response();
    }
    // Tenant-scoped path (matches the upload) — never read another
    // tenant's avatar dir.
    let path = std::path::PathBuf::from("./var/avatars")
        .join(&tenant.org.slug)
        .join(&storage_name);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => {
            return (axum::http::StatusCode::NOT_FOUND, "avatar bytes missing").into_response()
        }
    };
    let mime = mime_guess::from_path(&storage_name)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();
    // Same stable-URL staleness fix as branding: revalidate via a
    // content ETag so a changed avatar is picked up immediately.
    revalidated_asset(&headers, mime, bytes)
}

pub async fn cms_login_form(
    State(state): State<super::AdminState>,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    tenant: Tenant,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let mut ctx = Context::new();
    // The framework's POST handler expects to receive the form at
    // `/login`. Set the action to a relative URL so the same shape
    // works under reverse proxies.
    ctx.insert("action", &"/login");
    ctx.insert("admin_title", &"CMS Admin");
    // #279 — make sure the CMS-branded login form always carries a
    // `next` destination so the framework's POST handler doesn't
    // fall back to `/` (the public homepage). Precedence:
    //   1. Explicit `?next=` (passed in by `with_login_required`
    //      when an anonymous request hit an admin URL).
    //   2. Default `/cms-admin/` when the user navigated to /login
    //      manually — they almost certainly want the admin, not
    //      the public site root.
    // The framework's `sanitize_next` rejects open-redirect-shaped
    // values (anything not starting with a single `/`, scheme-bearing,
    // or pointing back at /login itself), so we don't need to filter
    // here; the worst case is the framework strips ours and uses `/`.
    let next = q
        .get("next")
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("/cms-admin/");
    ctx.insert("next", next);
    // Friendly error from a redirect (e.g. `?error=invalid_credentials`).
    if let Some(err) = q.get("error") {
        ctx.insert("error", err);
    }
    // SSO (admin-sso) — one "Sign in with <provider>" button per enabled
    // `SsoProvider` row (the DB-backed, admin-managed, multi-provider model
    // that replaced the flat `Org.sso_*` columns). The framework serves
    // `/login/sso/{slug}` + `/login/sso/{slug}/callback` (mints the same
    // rustango_tenant_session cookie password login does), so no auth code
    // lives here — just the buttons. Mirrors the framework's own login context.
    let sso_providers = rustango::admin::sso_provider::list_enabled(tenant.pool(), "/login").await;
    ctx.insert("sso_enabled", &!sso_providers.is_empty());
    ctx.insert("sso_providers", &sso_providers);
    // Surface the callback's failure reason (it redirects with `?sso_error=...`).
    if let Some(err) = q.get("sso_error") {
        ctx.insert("sso_error", err);
    }
    // #199 — show the tenant's uploaded logo/favicon in the login brand badge
    // (falls back to the default icon when none is set). Same source as the
    // authenticated sidebar; the `/__cms-branding/*` routes are pre-auth.
    add_branding_urls(&mut ctx, &tenant).await;
    render_with_csrf(&state, &headers, "login.html", &mut ctx)
}

/// GET `/cms-admin/password-reset` — show the username form.
pub async fn password_reset_request_form(
    State(state): State<super::AdminState>,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
) -> Result<Response, AdminError> {
    let mut ctx = Context::new();
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/password_reset_request.html",
        &mut ctx,
    )
}

/// POST `/cms-admin/password-reset` — issue a token, email the link,
/// render the "check your email" page either way (so the form never
/// confirms or denies account existence).
/// The key a password-reset link is signed with (#673): the process secret
/// narrowed to one tenant and one password.
///
/// Tenants number their users from 1, so a link signed with the process
/// secret alone — which names only `user_id` — resets the same id on every
/// tenant. Binding the tenant slug makes a link verify only where it was
/// issued; binding the current password hash makes it die the moment the
/// password changes, so it is single-use without any cache.
fn password_reset_key(secret: &[u8], tenant_slug: &str, password_hash: &str) -> Vec<u8> {
    let msg = format!("rcms-password-reset\0{tenant_slug}\0{password_hash}");
    crate::signing::hmac_sha256_hex(secret, msg.as_bytes()).into_bytes()
}

/// The active user a reset link was issued for, in this request's tenant.
///
/// The `user_id` in the link is read before it is trusted only to find
/// whose key to verify against; the link is accepted solely when it
/// verifies under that user's tenant- and password-bound key.
async fn password_reset_user(
    tenant: &Tenant,
    secret: &[u8],
    signed_url: &str,
) -> Option<rustango::tenancy::auth::User> {
    let claimed: i64 = signed_url
        .split_once('?')?
        .1
        .split('&')
        .find_map(|kv| kv.strip_prefix("user_id="))?
        .parse()
        .ok()?;
    let user = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.eq(claimed))
        .where_(rustango::tenancy::auth::User::active.eq(true))
        .first(tenant.pool())
        .await
        .ok()??;
    let key = password_reset_key(secret, &tenant.org.slug, &user.password_hash);
    match rustango::auth_flows::PasswordReset::verify(signed_url, &key) {
        Ok(uid) if uid == claimed => Some(user),
        _ => None,
    }
}

pub async fn password_reset_request_submit(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let username = form.get("username").cloned().unwrap_or_default();
    if !username.trim().is_empty() {
        // Look up the rustango_users row by username. Active users
        // only; soft-disabled accounts get the same silent treatment
        // as missing ones.
        let user: Option<rustango::tenancy::auth::User> = rustango::tenancy::auth::User::objects()
            .where_(rustango::tenancy::auth::User::username.eq(username.clone()))
            .where_(rustango::tenancy::auth::User::active.eq(true))
            .fetch(tenant.pool())
            .await
            .unwrap_or_default()
            .into_iter()
            .next();
        if let Some(u) = user {
            let user_id = u.id.get().copied().unwrap_or(0);
            // #435 — the reset link goes to the user's real address.
            // No email on file → nothing to send; the response below
            // still renders "sent" so the form never confirms or
            // denies account existence.
            let to_email = Some(user_email(&u)).filter(|e| !e.is_empty());
            match to_email {
                Some(to) => {
                    // Sign a relative URL — the verify-side recomputes
                    // against the request path+query (no host involved).
                    let key = password_reset_key(
                        state.signing_secret.as_slice(),
                        &tenant.org.slug,
                        &u.password_hash,
                    );
                    let token_url = rustango::auth_flows::PasswordReset::issue(
                        "/cms-admin/password-reset/confirm",
                        user_id,
                        &key,
                        std::time::Duration::from_secs(3600),
                    );
                    send_reset_email(
                        state.mailer.as_deref(),
                        &state.mailer_from,
                        &to,
                        &u.username,
                        &token_url,
                    )
                    .await;
                }
                None => {
                    tracing::info!(
                        username = %u.username,
                        "password reset requested but the account has no email on file; nothing sent"
                    );
                }
            }
        }
    }
    let mut ctx = Context::new();
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/password_reset_sent.html",
        &mut ctx,
    )
}

/// GET `/cms-admin/password-reset/confirm` — verify the signed token
/// in the URL, render the new-password form when valid. Invalid /
/// expired links land on a friendly error inside the same template.
pub async fn password_reset_confirm_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> Result<Response, AdminError> {
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or_default()
        .to_owned();
    let mut ctx = Context::new();
    match password_reset_user(&tenant, state.signing_secret.as_slice(), &path_and_query).await {
        Some(_) => {
            // Pass the signed URL through so the POST handler can
            // re-verify before writing the new password.
            ctx.insert("signed_token", &path_and_query);
            render_with_csrf(
                &state,
                &headers,
                "rcms_admin/password_reset_confirm.html",
                &mut ctx,
            )
        }
        None => {
            ctx.insert(
                "error",
                &"This reset link is invalid or has expired. Request a fresh one.",
            );
            render_with_csrf(
                &state,
                &headers,
                "rcms_admin/password_reset_request.html",
                &mut ctx,
            )
        }
    }
}

/// POST `/cms-admin/password-reset/confirm` — verify the token + new
/// password fields, hash the password, write to `rustango_users`,
/// render the "all set" page.
pub async fn password_reset_confirm_submit(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    rustango::extractors::SessionUser(_session_user): rustango::extractors::SessionUser,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let token = form.get("token").cloned().unwrap_or_default();
    let password = form.get("password").cloned().unwrap_or_default();
    let password_confirm = form.get("password_confirm").cloned().unwrap_or_default();

    let mut ctx = Context::new();
    let render_confirm_with_error = |state: &super::AdminState,
                                     headers: &HeaderMap,
                                     token: &str,
                                     msg: &str|
     -> Result<Response, AdminError> {
        let mut c = Context::new();
        c.insert("signed_token", &token);
        c.insert("error", &msg);
        render_with_csrf(
            state,
            headers,
            "rcms_admin/password_reset_confirm.html",
            &mut c,
        )
    };

    if password.len() < 12 {
        return render_confirm_with_error(
            &state,
            &headers,
            &token,
            "Password must be at least 12 characters.",
        );
    }
    if password != password_confirm {
        return render_confirm_with_error(
            &state,
            &headers,
            &token,
            "Password confirmation didn't match.",
        );
    }

    // Tenant- and password-bound (#673): a link from another tenant, or one
    // already used to change this password, does not verify.
    let Some(mut user) = password_reset_user(&tenant, state.signing_secret.as_slice(), &token).await
    else {
        ctx.insert(
            "error",
            &"This reset link is invalid or has expired. Request a fresh one.",
        );
        return render_with_csrf(
            &state,
            &headers,
            "rcms_admin/password_reset_request.html",
            &mut ctx,
        );
    };
    let new_hash = match crate::passwords::hash(&password).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "password hash failed");
            return render_confirm_with_error(
                &state,
                &headers,
                &token,
                "Could not save the new password. Try again.",
            );
        }
    };
    user.password_hash = new_hash;
    if user.save_pool(tenant.pool()).await.is_err() {
        return render_confirm_with_error(
            &state,
            &headers,
            &token,
            "Could not save the new password. Try again.",
        );
    }

    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/password_reset_done.html",
        &mut ctx,
    )
}

/// Send the reset email via `ConsoleMailer`. Prints the URL to stdout
/// in dev; production deployments swap in `SmtpMailer` here.
///
/// The `rustango_users` table doesn't carry an email column, so the
/// "to" address is synthesized from the username for now. Replace
/// with `user.email` once the host extends the user model.
/// #435 — deliver the reset link to the user's real address through
/// the host-configured mailer (`router_with_mailer`), falling back to
/// the console mailer in dev setups with no mailer wired.
async fn send_reset_email(
    mailer: Option<&dyn rustango::email::Mailer>,
    mailer_from: &str,
    to_email: &str,
    username: &str,
    reset_url: &str,
) {
    let from = if mailer_from.trim().is_empty() {
        "noreply@cms.local"
    } else {
        mailer_from
    };
    let email = rustango::email::Email::new()
        .to(to_email)
        .from(from)
        .subject("Reset your CMS password")
        .body(format!(
            "Hi {username},\n\n\
             We received a request to reset your CMS password.\n\
             Click the link below within the next hour to set a new one:\n\n\
             {reset_url}\n\n\
             If you didn't request this, you can safely ignore this email.\n"
        ));
    let result = match mailer {
        Some(m) => m.send(&email).await,
        None => {
            let console = rustango::email::ConsoleMailer;
            <rustango::email::ConsoleMailer as rustango::email::Mailer>::send(&console, &email)
                .await
        }
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, username = %username, "password-reset mail send failed");
    }
    tracing::info!(username = %username, "issued password-reset link");
}

#[cfg(test)]
mod tests {
    //! Role-permission matrix translation tests (#33). Exercise the
    //! pure helpers — no DB, no Tera.
    use super::*;

    #[test]
    fn parse_matrix_form_translates_brackets_to_codenames() {
        let mut form = std::collections::HashMap::new();
        form.insert("name".to_owned(), "Editor".to_owned());
        form.insert("perm[pages][view]".to_owned(), "1".to_owned());
        form.insert("perm[pages][add]".to_owned(), "1".to_owned());
        form.insert("perm[pages][edit]".to_owned(), "1".to_owned());
        // Unchecked → not present in the form. View-only.
        form.insert("perm[history][view]".to_owned(), "1".to_owned());
        // Inapplicable cell — `history` doesn't accept `edit`. Must be
        // dropped silently.
        form.insert("perm[history][edit]".to_owned(), "1".to_owned());
        // Off-state checkbox shouldn't produce a grant.
        form.insert("perm[redirects][delete]".to_owned(), "0".to_owned());
        // Unknown resource key — dropped.
        form.insert("perm[bogus][view]".to_owned(), "1".to_owned());
        // Malformed key — dropped.
        form.insert("perm[pages]".to_owned(), "1".to_owned());

        let codenames = parse_matrix_form(&form);
        assert!(codenames.contains(&"cms_page.view".to_owned()));
        assert!(codenames.contains(&"cms_page.add".to_owned()));
        assert!(codenames.contains(&"cms_page.edit".to_owned()));
        assert!(codenames.contains(&"cms_history.view".to_owned()));
        assert!(!codenames.contains(&"cms_history.edit".to_owned()));
        assert!(!codenames.contains(&"cms_redirect.delete".to_owned()));
        // No bogus / malformed leakage.
        assert!(!codenames.iter().any(|c| c.contains("bogus")));
    }

    #[test]
    fn split_matrix_codenames_isolates_legacy() {
        let granted = vec![
            "cms_page.view".to_owned(),
            "cms_page.edit".to_owned(),
            "rustango_roles.view".to_owned(),
            "acme_custom.do_thing".to_owned(),
            "free_form_string".to_owned(),
        ];
        let (matched, legacy) = split_matrix_codenames(&granted);
        assert!(matched.contains(&"cms_page.view".to_owned()));
        assert!(matched.contains(&"cms_page.edit".to_owned()));
        assert!(matched.contains(&"rustango_roles.view".to_owned()));
        assert!(legacy.contains(&"acme_custom.do_thing".to_owned()));
        assert!(legacy.contains(&"free_form_string".to_owned()));
    }

    #[test]
    fn build_permission_matrix_marks_granted_cells() {
        let granted = vec![
            "cms_page.view".to_owned(),
            "cms_page.add".to_owned(),
            "cms_media.view".to_owned(),
        ];
        let matrix = build_permission_matrix(&granted);
        let arr = matrix.as_array().expect("matrix is an array of sections");
        // Find the Content section → Pages row → granted list.
        let pages_row = arr
            .iter()
            .flat_map(|s| s["rows"].as_array().unwrap().iter())
            .find(|r| r["key"] == "pages")
            .expect("pages row present");
        let granted_actions: Vec<&str> = pages_row["granted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(granted_actions.contains(&"view"));
        assert!(granted_actions.contains(&"add"));
        assert!(!granted_actions.contains(&"edit"));
        assert!(!granted_actions.contains(&"delete"));
    }

    #[test]
    fn round_trip_form_to_codenames_to_matrix() {
        // Submit a matrix; reverse-render it; expect every checked cell
        // to come back as `granted`.
        let mut form = std::collections::HashMap::new();
        form.insert("perm[pages][view]".to_owned(), "1".to_owned());
        form.insert("perm[pages][publish]".to_owned(), "1".to_owned());
        form.insert("perm[media][view]".to_owned(), "1".to_owned());
        let codenames = parse_matrix_form(&form);
        let matrix = build_permission_matrix(&codenames);

        let pages = matrix
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s["rows"].as_array().unwrap().iter())
            .find(|r| r["key"] == "pages")
            .unwrap();
        let granted: Vec<&str> = pages["granted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(granted, vec!["view", "publish"]);
    }

    // -------- #117 InlinePanel extractor --------

    fn sample_spec() -> crate::page_type::InlinePanelSpec {
        crate::page_type::InlinePanelSpec::new(
            "team",
            "Team",
            vec![
                crate::page_type::ExtensionField {
                    name: "name".to_owned(),
                    label: "Name".to_owned(),
                    kind: crate::page_type::ExtensionFieldKind::Text,
                    value: String::new(),
                    help: String::new(),
                    max_length: None,
                },
                crate::page_type::ExtensionField {
                    name: "role".to_owned(),
                    label: "Role".to_owned(),
                    kind: crate::page_type::ExtensionFieldKind::Text,
                    value: String::new(),
                    help: String::new(),
                    max_length: None,
                },
            ],
        )
    }

    #[test]
    fn inline_extractor_groups_by_row_index() {
        let spec = sample_spec();
        let mut form = std::collections::HashMap::new();
        form.insert("inline__team__0__name".to_owned(), "Ada".to_owned());
        form.insert("inline__team__0__role".to_owned(), "Engineer".to_owned());
        form.insert("inline__team__1__name".to_owned(), "Bob".to_owned());
        form.insert("inline__team__1__role".to_owned(), "Designer".to_owned());
        let rows = extract_inline_panel_rows(&form, &spec);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "Ada");
        assert_eq!(rows[1]["role"], "Designer");
    }

    #[test]
    fn inline_extractor_drops_tombstoned_rows() {
        let spec = sample_spec();
        let mut form = std::collections::HashMap::new();
        form.insert("inline__team__0__name".to_owned(), "Ada".to_owned());
        form.insert("inline__team__1__name".to_owned(), "Bob".to_owned());
        form.insert("inline__team__1____deleted".to_owned(), "1".to_owned());
        let rows = extract_inline_panel_rows(&form, &spec);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "Ada");
    }

    #[test]
    fn inline_extractor_ignores_unknown_field_names() {
        let spec = sample_spec();
        let mut form = std::collections::HashMap::new();
        form.insert("inline__team__0__name".to_owned(), "Ada".to_owned());
        form.insert("inline__team__0__bogus".to_owned(), "x".to_owned());
        let rows = extract_inline_panel_rows(&form, &spec);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].contains_key("bogus"));
    }

    #[test]
    fn inline_extractor_ignores_unrelated_form_keys() {
        let spec = sample_spec();
        let mut form = std::collections::HashMap::new();
        form.insert("title".to_owned(), "irrelevant".to_owned());
        form.insert("inline__other_panel__0__name".to_owned(), "x".to_owned());
        let rows = extract_inline_panel_rows(&form, &spec);
        assert!(rows.is_empty());
    }

    /// #268 — the page-edit submit handler funnels the raw posted
    /// form through an allow-list before re-deserializing as
    /// `PageForm`. The bug behind #268 was that schedule fields
    /// (`go_live_at`, `expire_at`) and `show_in_menus` were missing
    /// from that allow-list, so any datetime the editor set on the
    /// Promote tab was silently dropped on save — and never made it
    /// to the scheduled-pages report. Lock these in so a future
    /// trim of the array can't reintroduce the regression.
    #[test]
    fn page_form_canonical_keys_include_schedule_and_menu_fields() {
        assert!(
            PAGE_FORM_CANONICAL_KEYS.contains(&"go_live_at"),
            "go_live_at must be in canonical keys — see #268"
        );
        assert!(
            PAGE_FORM_CANONICAL_KEYS.contains(&"expire_at"),
            "expire_at must be in canonical keys — see #268"
        );
        assert!(
            PAGE_FORM_CANONICAL_KEYS.contains(&"show_in_menus"),
            "show_in_menus must be in canonical keys — see #268"
        );
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod media_picker_tests {
    //! `media_picker_payload` filtering / pagination / collection-tree
    //! tests over an in-memory sqlite pool (mirrors redirect.rs's
    //! `mem_pool` pattern).
    use super::{media_picker_payload, MediaPickerQuery};
    use crate::media::{Media, MediaCollection};
    use rustango::core::Model as _;
    use rustango::sql::{Auto, FetcherPool as _, Pool};

    async fn mem_pool() -> Pool {
        let pool = Pool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        for schema in [&Media::SCHEMA, &MediaCollection::SCHEMA] {
            let ddl = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
                pool.dialect(),
                schema,
            );
            for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                rustango::sql::raw_execute_pool(&pool, stmt, Vec::new())
                    .await
                    .expect("ddl");
            }
        }
        pool
    }

    async fn mk_collection(pool: &Pool, name: &str, parent_id: Option<i64>, sort: i32) -> i64 {
        MediaCollection {
            id: Auto::Unset,
            name: name.to_owned(),
            parent_id,
            sort_order: sort,
            created_at: Auto::Unset,
        }
        .insert_pool(pool)
        .await
        .expect("collection insert");
        MediaCollection::objects()
            .fetch(pool)
            .await
            .expect("fetch")
            .into_iter()
            .find(|c| c.name == name)
            .and_then(|c| c.id.get().copied())
            .expect("collection id")
    }

    async fn mk_media(
        pool: &Pool,
        title: &str,
        filename: &str,
        kind: &str,
        collection_id: Option<i64>,
        alt: &str,
    ) {
        Media {
            id: Auto::Unset,
            filename: filename.to_owned(),
            content_hash: format!("{:0>64}", title.len()),
            mime: if kind == "image" {
                "image/png"
            } else {
                "application/pdf"
            }
            .to_owned(),
            size: 123,
            kind: kind.to_owned(),
            width: (kind == "image").then_some(100),
            height: (kind == "image").then_some(80),
            storage_key: format!("k-{filename}"),
            title: title.to_owned(),
            alt_text: alt.to_owned(),
            description: String::new(),
            uploaded_by: None,
            collection_id,
            focal_point_x: None,
            focal_point_y: None,
            uploaded_at: Auto::Unset,
        }
        .insert_pool(pool)
        .await
        .expect("media insert");
    }

    #[tokio::test]
    async fn default_kind_excludes_documents() {
        let pool = mem_pool().await;
        mk_media(&pool, "Photo", "p.png", "image", None, "").await;
        mk_media(&pool, "Spec", "s.pdf", "document", None, "").await;
        let body = media_picker_payload(&pool, &MediaPickerQuery::default())
            .await
            .expect("payload");
        let items = body["items"].as_array().expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["kind"], "image");
        // kind=any opts back in.
        let q = MediaPickerQuery {
            kind: Some("any".to_owned()),
            ..Default::default()
        };
        let body = media_picker_payload(&pool, &q).await.expect("payload");
        assert_eq!(body["items"].as_array().expect("items").len(), 2);
    }

    #[tokio::test]
    async fn collection_zero_means_uncategorized_only() {
        let pool = mem_pool().await;
        let cid = mk_collection(&pool, "Root", None, 0).await;
        mk_media(&pool, "In", "in.png", "image", Some(cid), "").await;
        mk_media(&pool, "Loose", "loose.png", "image", None, "").await;
        let q = MediaPickerQuery {
            collection: Some(0),
            ..Default::default()
        };
        let body = media_picker_payload(&pool, &q).await.expect("payload");
        let items = body["items"].as_array().expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "Loose");
        assert_eq!(body["uncategorized_count"], 1);
        // Exact collection id narrows to its members.
        let q = MediaPickerQuery {
            collection: Some(cid),
            ..Default::default()
        };
        let body = media_picker_payload(&pool, &q).await.expect("payload");
        assert_eq!(body["items"].as_array().expect("items").len(), 1);
        assert_eq!(body["items"][0]["title"], "In");
    }

    #[tokio::test]
    async fn search_matches_alt_text() {
        let pool = mem_pool().await;
        mk_media(
            &pool,
            "IMG_0001",
            "a.png",
            "image",
            None,
            "sunset glow over harbour",
        )
        .await;
        mk_media(&pool, "IMG_0002", "b.png", "image", None, "office desk").await;
        let q = MediaPickerQuery {
            q: Some("Sunset".to_owned()),
            ..Default::default()
        };
        let body = media_picker_payload(&pool, &q).await.expect("payload");
        let items = body["items"].as_array().expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "IMG_0001");
    }

    #[tokio::test]
    async fn pagination_math_and_has_more() {
        let pool = mem_pool().await;
        for i in 0..5 {
            mk_media(
                &pool,
                &format!("M{i}"),
                &format!("m{i}.png"),
                "image",
                None,
                "",
            )
            .await;
        }
        let q = MediaPickerQuery {
            limit: Some(2),
            offset: Some(0),
            ..Default::default()
        };
        let body = media_picker_payload(&pool, &q).await.expect("payload");
        assert_eq!(body["items"].as_array().expect("items").len(), 2);
        assert_eq!(body["total"], 5);
        assert_eq!(body["has_more"], true);
        let q = MediaPickerQuery {
            limit: Some(2),
            offset: Some(4),
            ..Default::default()
        };
        let body = media_picker_payload(&pool, &q).await.expect("payload");
        assert_eq!(body["items"].as_array().expect("items").len(), 1);
        assert_eq!(body["has_more"], false);
        // Offset past the end is clamped, not a panic.
        let q = MediaPickerQuery {
            limit: Some(2),
            offset: Some(99),
            ..Default::default()
        };
        let body = media_picker_payload(&pool, &q).await.expect("payload");
        assert_eq!(body["items"].as_array().expect("items").len(), 0);
    }

    #[tokio::test]
    async fn collections_come_back_in_dfs_order_with_depth() {
        let pool = mem_pool().await;
        let root_a = mk_collection(&pool, "Alpha", None, 0).await;
        let _sub = mk_collection(&pool, "Alpha Child", Some(root_a), 0).await;
        let _root_b = mk_collection(&pool, "Beta", None, 1).await;
        mk_media(&pool, "In A", "a.png", "image", Some(root_a), "").await;
        let body = media_picker_payload(&pool, &MediaPickerQuery::default())
            .await
            .expect("payload");
        let cols = body["collections"].as_array().expect("collections");
        let names: Vec<&str> = cols.iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Alpha", "Alpha Child", "Beta"]);
        assert_eq!(cols[0]["depth"], 0);
        assert_eq!(cols[1]["depth"], 1);
        assert_eq!(cols[2]["depth"], 0);
        assert_eq!(cols[0]["count"], 1);
        assert_eq!(cols[2]["count"], 0);
    }

    #[tokio::test]
    async fn ids_lookup_resolves_any_kind() {
        // The label-hydration pass resolves exact ids — including
        // non-image kinds, which the default filter would drop.
        let pool = mem_pool().await;
        mk_media(&pool, "Photo", "p.png", "image", None, "").await;
        mk_media(&pool, "Spec", "s.pdf", "document", None, "").await;
        let all = media_picker_payload(
            &pool,
            &MediaPickerQuery {
                kind: Some("any".to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("payload");
        let ids: Vec<i64> = all["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_i64().unwrap())
            .collect();
        let q = MediaPickerQuery {
            ids: Some(
                ids.iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            ..Default::default()
        };
        let body = media_picker_payload(&pool, &q).await.expect("payload");
        let items = body["items"].as_array().expect("items");
        assert_eq!(
            items.len(),
            2,
            "document resolves too when ids are explicit"
        );
        // Explicit kind still narrows an ids lookup.
        let q = MediaPickerQuery {
            ids: Some(
                ids.iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            kind: Some("image".to_owned()),
            ..Default::default()
        };
        let body = media_picker_payload(&pool, &q).await.expect("payload");
        assert_eq!(body["items"].as_array().expect("items").len(), 1);
    }

    #[tokio::test]
    async fn image_items_carry_rendition_thumb_urls() {
        let pool = mem_pool().await;
        mk_media(&pool, "Pic", "p.png", "image", None, "").await;
        let body = media_picker_payload(&pool, &MediaPickerQuery::default())
            .await
            .expect("payload");
        let item = &body["items"][0];
        let thumb = item["thumb_url"].as_str().expect("thumb_url");
        assert!(
            thumb.starts_with("/__media__/fill-320x320"),
            "spec-first URL: {thumb}"
        );
        assert!(thumb.contains("v="), "cache-buster present: {thumb}");
        let preview = item["preview_url"].as_str().expect("preview_url");
        assert!(preview.contains("max-640x480"), "{preview}");
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod forms_list_tests {
    //! `forms_list_rows` row-model tests over an in-memory sqlite pool.
    use super::forms_list_rows;
    use crate::forms::submit::FormEntry;
    use crate::snippet::Snippet;
    use rustango::core::Model as _;
    use rustango::sql::{Auto, FetcherPool as _, Pool};

    async fn mem_pool() -> Pool {
        let pool = Pool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        for schema in [&Snippet::SCHEMA, &FormEntry::SCHEMA] {
            let ddl = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
                pool.dialect(),
                schema,
            );
            for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                rustango::sql::raw_execute_pool(&pool, stmt, Vec::new())
                    .await
                    .expect("ddl");
            }
        }
        pool
    }

    async fn mk_form(pool: &Pool, slug: &str, title: &str, fields: usize) -> i64 {
        let field_json: Vec<serde_json::Value> = (0..fields)
            .map(|i| serde_json::json!({"id": format!("f{i}"), "key": format!("k{i}"), "type": "text", "label": "F"}))
            .collect();
        Snippet {
            id: Auto::Unset,
            type_name: "form".to_owned(),
            folder_path: String::new(),
            slug: slug.to_owned(),
            title: title.to_owned(),
            body_markdown: String::new(),
            data: serde_json::json!({
                "settings": {},
                "pages": [{"id": "p1", "sections": [{"id": "s1", "rows": [{"id": "r1",
                    "columns": [{"id": "c1", "width": 12, "fields": field_json}]}]}]}]
            }),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        }
        .insert_pool(pool)
        .await
        .expect("form insert");
        Snippet::objects()
            .fetch(pool)
            .await
            .expect("fetch")
            .into_iter()
            .find(|s| s.slug == slug)
            .and_then(|s| s.id.get().copied())
            .expect("form id")
    }

    async fn mk_entry(pool: &Pool, form_id: i64) {
        FormEntry {
            id: Auto::Unset,
            form_snippet_id: form_id,
            form_version: 1,
            source_page_id: 0,
            source_url: "/contact".to_owned(),
            data_json: serde_json::json!({"k0": "hi"}),
            locale: "en".to_owned(),
            ip: String::new(),
            submitted_at: Auto::Unset,
        }
        .insert_pool(pool)
        .await
        .expect("entry insert");
    }

    #[tokio::test]
    async fn rows_carry_field_and_submission_counts() {
        let pool = mem_pool().await;
        let a = mk_form(&pool, "contact", "Contact", 3).await;
        let b = mk_form(&pool, "rsvp", "RSVP", 1).await;
        mk_entry(&pool, a).await;
        mk_entry(&pool, a).await;
        let rows = forms_list_rows(&pool).await.expect("rows");
        assert_eq!(rows.len(), 2);
        let by_slug = |slug: &str| rows.iter().find(|r| r["slug"] == slug).expect("row");
        let ra = by_slug("contact");
        assert_eq!(ra["fields"], 3);
        assert_eq!(ra["submissions"], 2);
        assert!(
            ra["last_submission"].as_str().unwrap().len() >= 16,
            "has a timestamp"
        );
        assert_eq!(ra["build_url"], format!("/cms-admin/forms/{a}/build"));
        assert_eq!(
            ra["submissions_url"],
            format!("/cms-admin/forms/{a}/submissions")
        );
        let rb = by_slug("rsvp");
        assert_eq!(rb["fields"], 1);
        assert_eq!(rb["submissions"], 0);
        assert_eq!(rb["last_submission"], "");
        assert_eq!(rb["delete_url"], format!("/cms-admin/library/{b}/delete"));
    }

    #[tokio::test]
    async fn only_form_snippets_appear() {
        let pool = mem_pool().await;
        mk_form(&pool, "contact", "Contact", 1).await;
        Snippet {
            id: Auto::Unset,
            type_name: "branding".to_owned(),
            folder_path: String::new(),
            slug: "site-branding".to_owned(),
            title: "Branding".to_owned(),
            body_markdown: String::new(),
            data: serde_json::json!({}),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        }
        .insert_pool(&pool)
        .await
        .expect("snippet insert");
        let rows = forms_list_rows(&pool).await.expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["slug"], "contact");
    }

    #[test]
    fn user_admin_is_superuser_only() {
        // #642 — user management sets is_superuser / active / roles from
        // the form, so the whole surface is restricted to superusers.
        // A superuser proceeds; a signed-in non-superuser is bounced to
        // no-access; an anonymous caller to login. A regression that drops
        // the guard (or lets a non-superuser through) fails here.
        assert_eq!(super::user_admin_denial_target(Some(true)), None);
        assert_eq!(
            super::user_admin_denial_target(Some(false)),
            Some("/cms-admin/no-access")
        );
        assert_eq!(super::user_admin_denial_target(None), Some("/login"));
    }

    #[test]
    fn mcp_key_admin_handlers_enforce_the_superuser_gate() {
        // #646 — the two /cms-admin/users/{id}/mcp-keys handlers must carry
        // the same superuser gate as every other user-admin handler. They
        // once gated on "a session exists" only, so a non-superuser could
        // mint a key bounded by a superuser's entitlement. Pin the wiring so
        // a future edit can't silently drop it again (the gap that untested
        // wiring hid the first time). Runs in the default suite — no DB.
        let src = include_str!("handlers.rs");
        for name in ["users_mcp_key_create", "users_mcp_key_revoke"] {
            let start = src
                .find(&format!("pub async fn {name}("))
                .unwrap_or_else(|| panic!("handler {name} not found"));
            // Bound the scan to this fn's body: up to the next top-level fn.
            let rest = &src[start + 1..];
            let end = rest.find("\npub async fn ").unwrap_or(rest.len());
            assert!(
                rest[..end].contains("require_superuser_for_user_admin(session_user.as_ref())"),
                "{name} is missing the superuser gate (#646)"
            );
        }
    }
}

#[cfg(test)]
mod password_reset_key_tests {
    use super::password_reset_key;
    use rustango::auth_flows::PasswordReset;
    use std::time::Duration;

    const SECRET: &[u8] = b"process-wide-signing-secret";
    const BASE: &str = "/cms-admin/password-reset/confirm";

    fn link(tenant: &str, hash: &str) -> String {
        PasswordReset::issue(BASE, 1, &password_reset_key(SECRET, tenant, hash), Duration::from_secs(3600))
    }

    #[test]
    fn a_link_verifies_for_the_tenant_and_password_it_was_issued_for() {
        let url = link("acme", "$argon2id$v=19$old");
        let key = password_reset_key(SECRET, "acme", "$argon2id$v=19$old");
        assert_eq!(PasswordReset::verify(&url, &key).ok(), Some(1));
    }

    #[test]
    fn a_link_issued_on_one_tenant_does_not_verify_on_another() {
        // User ids restart at 1 per tenant: this is the takeover in #673.
        let url = link("acme", "$argon2id$v=19$same");
        let other = password_reset_key(SECRET, "globex", "$argon2id$v=19$same");
        assert!(PasswordReset::verify(&url, &other).is_err());
    }

    #[test]
    fn a_link_stops_verifying_once_the_password_changes() {
        let url = link("acme", "$argon2id$v=19$before");
        let after = password_reset_key(SECRET, "acme", "$argon2id$v=19$after");
        assert!(PasswordReset::verify(&url, &after).is_err());
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod publish_gate_tests {
    //! #761 — a save that puts a page live takes the publish right,
    //! judged on the status it lands on after go-live derivation.
    use super::{apply_page_edit, PageEditActor, PageEditRefusal};
    use crate::page::{Page, PageStatus};
    use crate::page_type_model::PageType;
    use rustango::core::Column as _;
    use rustango::sql::{Auto, Pool};

    async fn mem_pool() -> Pool {
        let pool = Pool::connect("sqlite::memory:").await.expect("in-memory sqlite");
        for entry in inventory::iter::<rustango::core::ModelEntry> {
            if !entry.schema.managed {
                continue;
            }
            let ddl = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
                pool.dialect(),
                entry.schema,
            );
            for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                rustango::sql::raw_execute_pool(&pool, stmt, Vec::new())
                    .await
                    .expect("ddl");
            }
        }
        pool
    }

    async fn draft(pool: &Pool) -> Page {
        let mut pt = PageType {
            id: Auto::Unset,
            app_label: "cms".to_owned(),
            type_name: "TestPage".to_owned(),
            verbose_name: "Test page".to_owned(),
            default_template: "page.html".to_owned(),
            view_mode: "auto".to_owned(),
            is_creatable: true,
            allowed_parent_types: serde_json::json!([]),
            allowed_child_types: serde_json::json!([]),
            workflow: String::new(),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        pt.save_pool(pool).await.expect("page type");
        let mut p = draft_template(pt.id.get().copied().expect("type id"));
        p.save_pool(pool).await.expect("page");
        p
    }

    fn draft_template(type_id: i64) -> Page {
        Page {
            id: Auto::Unset,
            page_type_id: type_id,
            title: "Draft".to_owned(),
            slug: "draft".to_owned(),
            path: "0001/".to_owned(),
            url_path: "/draft".to_owned(),
            preview_path: String::new(),
            template_override: String::new(),
            depth: 1,
            parent_id: None,
            locale_variant_of: None,
            alias_of: None,
            theme_id: None,
            sort_order: 0,
            status: PageStatus::Draft.as_str().to_owned(),
            published_at: None,
            last_published_at: None,
            go_live_at: None,
            expire_at: None,
            seo_title: String::new(),
            seo_description: String::new(),
            robots_index: true,
            sitemap_priority: 0.5,
            show_in_menus: true,
            og_title: String::new(),
            og_description: String::new(),
            og_image_media_id: None,
            twitter_card: "summary".to_owned(),
            notification_pre_published_sent: false,
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        }
    }

    async fn save(
        pool: &Pool,
        page: &Page,
        status: &str,
        go_live_days: Option<i64>,
        superuser: bool,
    ) -> Result<String, &'static str> {
        let mut form = crate::page_form::prefill(pool, page).await;
        form.insert("status".into(), status.to_owned());
        match go_live_days {
            Some(d) => form.insert(
                "go_live_at".into(),
                (chrono::Utc::now() + chrono::Duration::days(d))
                    .format("%Y-%m-%dT%H:%M")
                    .to_string(),
            ),
            None => form.remove("go_live_at"),
        };
        // A user with no roles: edit reached the handler, publish is not held.
        let actor = PageEditActor { id: 999, username: "editor".to_owned(), is_superuser: superuser };
        let invalidator: std::sync::Arc<dyn crate::cache_invalidate::PageCacheInvalidator> =
            std::sync::Arc::new(crate::cache_invalidate::Noop);
        let id = page.id.get().copied().expect("id");
        match apply_page_edit(pool, "t", &invalidator, None, "", false, id, &form, Some(&actor))
            .await
            .expect("save")
        {
            Ok(o) => Ok(o.page.status),
            Err(PageEditRefusal::PublishDenied) => Err("publish denied"),
            Err(PageEditRefusal::ReviewRequired { .. }) => Err("review required"),
            Err(_) => Err("other refusal"),
        }
    }

    async fn stored_status(pool: &Pool, page: &Page) -> String {
        Page::objects()
            .where_(Page::id.eq(page.id.get().copied().expect("id")))
            .first(pool)
            .await
            .expect("query")
            .expect("page")
            .status
    }

    #[tokio::test]
    async fn going_live_by_any_route_needs_the_publish_right() {
        let pool = mem_pool().await;
        let page = draft(&pool).await;
        for (status, go_live) in [
            ("archived", None),
            ("published", None),
            // Draft plus a future go-live is parked as scheduled.
            ("draft", Some(1)),
            // Scheduled with a go-live already past: the sweep publishes it.
            ("scheduled", Some(-1)),
        ] {
            assert_eq!(
                save(&pool, &page, status, go_live, false).await,
                Err("publish denied"),
                "{status} / {go_live:?}"
            );
            assert_eq!(stored_status(&pool, &page).await, "draft", "nothing written");
        }
        assert_eq!(
            save(&pool, &page, "scheduled", None, false).await,
            Ok("draft".to_owned()),
            "scheduled without a date is parked as a draft, so it is allowed"
        );
        assert_eq!(
            save(&pool, &page, "draft", Some(1), true).await,
            Ok("scheduled".to_owned()),
            "the publish right schedules"
        );
    }

    /// A page type with a review workflow goes live only through it: a
    /// save straight to Published is refused for everyone but a
    /// superuser — the owner's review can't be skipped.
    /// Bind `type_id` to a one-step "Owner review" workflow.
    async fn bind_review(pool: &Pool, type_id: i64, reapproval: bool) -> crate::workflow::Workflow {
        let mut wf = crate::workflow::Workflow {
            id: Auto::Unset,
            name: "Owner review".to_owned(),
            description: String::new(),
            active: true,
            require_reapproval_on_edit: reapproval,
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        wf.save_pool(pool).await.expect("workflow");
        rustango::sql::raw_execute_pool(pool, "INSERT INTO rustango_roles (id, name, description) VALUES (1, 'Owner', '')", Vec::new())
            .await
            .expect("role");
        let mut task = crate::workflow::WorkflowTask {
            id: Auto::Unset,
            workflow_id: wf.id.get().copied().expect("id"),
            name: "Owner checks".to_owned(),
            role_id: 1,
            sort_order: 10,
            kind: "group_approval".to_owned(),
            webhook_url: String::new(),
        };
        task.save_pool(pool).await.expect("task");
        let mut pt = PageType::objects()
            .where_(PageType::id.eq(type_id))
            .first(pool)
            .await
            .expect("q")
            .expect("type");
        pt.workflow = "Owner review".to_owned();
        pt.save_pool(pool).await.expect("bind");
        wf
    }

    #[tokio::test]
    async fn a_review_workflow_cannot_be_published_around() {
        let pool = mem_pool().await;
        let page = draft(&pool).await;
        let mut wf = bind_review(&pool, page.page_type_id, false).await;

        assert_eq!(save(&pool, &page, "published", None, false).await, Err("review required"));
        assert_eq!(save(&pool, &page, "draft", Some(1), false).await, Err("review required"), "nor scheduled");
        assert_eq!(stored_status(&pool, &page).await, "draft");
        assert_eq!(save(&pool, &page, "draft", None, false).await, Ok("draft".to_owned()), "drafts still save");
        assert_eq!(save(&pool, &page, "published", None, true).await, Ok("published".to_owned()), "the owner can");

        // Renaming the workflow moves the page type along: still reviewed.
        let other = draft_named(&pool, page.page_type_id, "second").await;
        wf.name = "Studio review".to_owned();
        wf.save_pool(&pool).await.expect("rename");
        assert_eq!(crate::workflow::rebind_page_types(&pool, "Owner review", "Studio review").await.expect("rebind"), 1);
        assert_eq!(save(&pool, &other, "published", None, false).await, Err("review required"));
    }

    /// Save `page` with a new title; `actor` None is the CMS's own save.
    async fn retitle(pool: &Pool, page: &Page, title: &str, actor: Option<&PageEditActor>) -> super::PageEditOutcome {
        let mut form = crate::page_form::prefill(pool, page).await;
        form.insert("title".into(), title.to_owned());
        let invalidator: std::sync::Arc<dyn crate::cache_invalidate::PageCacheInvalidator> =
            std::sync::Arc::new(crate::cache_invalidate::Noop);
        let id = page.id.get().copied().expect("id");
        match apply_page_edit(pool, "t", &invalidator, None, "", false, id, &form, actor).await.expect("save") {
            Ok(o) => o,
            Err(_) => panic!("refused"),
        }
    }

    async fn stored_title(pool: &Pool, page: &Page) -> String {
        Page::objects()
            .where_(Page::id.eq(page.id.get().copied().expect("id")))
            .first(pool)
            .await
            .expect("query")
            .expect("page")
            .title
    }

    /// With re-approval on, an editor's save of a live page waits for the
    /// review: visitors keep the live version until the approval applies
    /// it; cancelling the review drops it; a superuser's save supersedes it.
    #[tokio::test]
    async fn a_live_page_edit_waits_for_review() {
        let pool = mem_pool().await;
        let page = draft(&pool).await;
        bind_review(&pool, page.page_type_id, true).await;
        assert_eq!(save(&pool, &page, "published", None, true).await, Ok("published".to_owned()));
        let id = page.id.get().copied().expect("id");
        let mut mia = rustango::tenancy::auth::User {
            id: Auto::Unset,
            username: "mia".to_owned(),
            password_hash: String::new(),
            email: None,
            is_superuser: false,
            active: true,
            created_at: chrono::Utc::now(),
            data: serde_json::json!({}),
            password_changed_at: None,
            sessions_revoked_at: None,
        };
        mia.save_pool(&pool).await.expect("user");
        let editor = PageEditActor { id: mia.id.get().copied().expect("id"), username: "mia".to_owned(), is_superuser: false };

        let out = retitle(&pool, &page, "Proposed", Some(&editor)).await;
        assert!(out.held_for_review && out.reapproval_kicked);
        assert_eq!(stored_title(&pool, &page).await, "Draft", "the live page is unchanged");
        let held = crate::pending_change::for_page(&pool, id).await.expect("q").expect("held");
        assert_eq!(held.form_map().get("title").map(String::as_str), Some("Proposed"));
        let mut state = crate::workflow::active_state_for_page(&pool, id).await.expect("q").expect("review started");

        // The approval applies it through the normal save path.
        let tasks = crate::workflow::tasks_for(&pool, state.workflow_id).await.expect("tasks");
        crate::workflow::approve_current(&pool, &mut state, &tasks, editor.id, "").await.expect("approve");
        let applied = retitle(&pool, &page, "Proposed", None).await;
        assert!(!applied.held_for_review);
        assert_eq!(stored_title(&pool, &page).await, "Proposed");

        // Cancelling the review drops a held change.
        retitle(&pool, &page, "Second try", Some(&editor)).await;
        let mut state = crate::workflow::active_state_for_page(&pool, id).await.expect("q").expect("review restarted");
        crate::workflow::cancel(&pool, &mut state, editor.id).await.expect("cancel");
        assert!(crate::pending_change::for_page(&pool, id).await.expect("q").is_none());

        // A superuser's direct save supersedes one.
        retitle(&pool, &page, "Third try", Some(&editor)).await;
        let owner = PageEditActor { id: 1, username: "owner".to_owned(), is_superuser: true };
        assert!(!retitle(&pool, &page, "Owner's", Some(&owner)).await.held_for_review);
        assert_eq!(stored_title(&pool, &page).await, "Owner's");
        assert!(crate::pending_change::for_page(&pool, id).await.expect("q").is_none());
    }

    async fn draft_named(pool: &Pool, type_id: i64, slug: &str) -> Page {
        let mut p = draft_template(type_id);
        slug.clone_into(&mut p.slug);
        p.path = "0002/".to_owned();
        p.url_path = format!("/{slug}");
        p.save_pool(pool).await.expect("page");
        p
    }
}

#[cfg(test)]
mod active_upload_tests {
    use super::refuse_active_upload;

    #[test]
    fn page_like_types_are_refused_and_media_is_not() {
        for bad in ["text/html", "application/xhtml+xml", "text/javascript", "application/xml"] {
            assert!(refuse_active_upload(bad).is_err(), "{bad}");
        }
        for ok in ["image/png", "image/svg+xml", "application/pdf", "text/plain", "video/mp4"] {
            assert!(refuse_active_upload(ok).is_ok(), "{ok}");
        }
    }
}
