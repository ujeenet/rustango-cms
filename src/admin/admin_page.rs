//! Extension point for **custom admin pages**.
//!
//! Authors building on top of `rustango-cms` register a unit-struct
//! handler via [`crate::register_admin_page!`] and it appears as a
//! sidebar entry under its declared [`SidebarSection`]. All registered
//! handlers route through a single dispatcher mounted at
//! `/cms-admin/x/{slug}`; no per-page router scaffolding to maintain.
//!
//! Custom pages inherit the bundled admin chrome — they extend
//! [`rcms_admin/_base.html`](../templates/_base.html) and get the
//! sidebar, topbar, action-bar slot, flash banners, theme tokens,
//! and every class from [`cms.css`](../static/cms.css) for free.
//!
//! ## Example
//!
//! ```ignore
//! use axum::response::Response;
//! use async_trait::async_trait;
//! use rustango_cms::admin::admin_page::{AdminPageCtx, AdminPageHandler, SidebarSection};
//!
//! #[derive(Default)]
//! pub struct ReportsDashboard;
//!
//! #[async_trait]
//! impl AdminPageHandler for ReportsDashboard {
//!     fn slug(&self) -> &'static str { "reports" }              // → /cms-admin/x/reports
//!     fn label(&self) -> &'static str { "Reports" }
//!     fn icon(&self) -> Option<&'static str> { Some("insights") }
//!     fn section(&self) -> SidebarSection { SidebarSection::Site }
//!     async fn render(&self, ctx: AdminPageCtx<'_>) -> Response {
//!         // Build a Tera context, render `reports.html` (host-registered),
//!         // return the response.
//!         todo!()
//!     }
//! }
//!
//! rustango_cms::register_admin_page!(ReportsDashboard);
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rustango::extractors::Tenant;
pub use rustango::messages::Level as MessageLevel;

use crate::permissions::Action;

/// Where the custom page's sidebar entry lands. Built-ins:
/// `Content`, `Site`, `Settings`. Use `Custom(&'static str)` to spin
/// up a new section heading on demand (e.g. `Custom("Tools")`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarSection {
    Content,
    Site,
    Settings,
    Custom(&'static str),
}

impl SidebarSection {
    /// Human-readable section heading (also the bucket key when
    /// `_base.html` groups custom pages for rendering).
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Content => "Content",
            Self::Site => "Site",
            Self::Settings => "Settings",
            Self::Custom(name) => name,
        }
    }
}

/// Request-scoped context handed to every custom admin page.
/// References rather than owns so the handler doesn't have to clone
/// the Tera instance / per-tenant pool.
pub struct AdminPageCtx<'a> {
    pub tera: &'a Arc<tera::Tera>,
    pub tenant: &'a Tenant,
    pub headers: &'a HeaderMap,
    /// The signed-in user, which the dispatcher has already resolved.
    ///
    /// Without it a custom page can know only that *somebody* is logged
    /// in, so it cannot gate on a codename — and a page that manages
    /// anything sensitive has to. It is also what
    /// [`crate::admin::handlers::add_chrome`] needs to fill in the
    /// per-user half of the sidebar.
    pub user: Option<&'a rustango::tenancy::auth::User>,
    /// The request URI, for the query string (`?id=7`, `?q=…`).
    pub uri: &'a Uri,
    /// The path segment after the slug: `edit` for
    /// `/cms-admin/x/<slug>/edit`, `None` for the page itself.
    pub action: Option<&'a str>,
}

impl AdminPageCtx<'_> {
    /// The query string as a map. A repeated key keeps its last value.
    #[must_use]
    pub fn query(&self) -> HashMap<String, String> {
        self.uri
            .query()
            .and_then(|q| serde_urlencoded::from_str(q).ok())
            .unwrap_or_default()
    }

    /// One query-string value, trimmed; `None` when absent or blank.
    #[must_use]
    pub fn query_param(&self, name: &str) -> Option<String> {
        self.query()
            .remove(name)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    }

    /// Render `template` inside the admin chrome. Adds what every admin
    /// screen needs to `ctx`: the sidebar (with this page marked active),
    /// the signed-in user, branding, the admin theme, the admin language,
    /// `csrf_token` and pending flash `messages`. A template error renders a 500.
    pub async fn render(&self, template: &str, ctx: &mut tera::Context) -> Response {
        let slug = self.slug();
        super::handlers::add_chrome(ctx, self.tenant, &format!("custom:{slug}"), self.user).await;
        super::handlers::add_admin_theme(ctx, self.tenant.pool()).await;
        ctx.insert("admin_page_slug", &slug);
        match super::handlers::render_tera(self.tera, self.headers, template, ctx) {
            Ok(resp) => resp,
            Err(e) => e.into_response(),
        }
    }

    /// `303 See Other` to `to`, with a flash message the next admin page
    /// shows. The message is translated for the admin language when the
    /// catalogs have it.
    #[must_use]
    pub fn redirect(&self, to: &str, level: MessageLevel, message: &str) -> Response {
        let body = super::i18n::tr(self.headers, message, &[]);
        rustango::messages::redirect_with_message(
            super::handlers::messages_secret(),
            self.headers,
            level,
            &body,
            to,
        )
    }

    /// Whether the signed-in user holds `codename`. Superusers hold every
    /// codename; no user holds none.
    pub async fn has_perm(&self, codename: &str) -> bool {
        let Some(user) = self.user else { return false };
        user.is_superuser
            || crate::permissions::user_codenames(self.tenant.pool(), user.id.get().copied().unwrap_or_default())
                .await
                .is_ok_and(|codes| codes.iter().any(|c| c == codename))
    }

    /// `/cms-admin/x/<slug>`, the URL of this page.
    #[must_use]
    pub fn page_url(&self) -> String {
        format!("/cms-admin/x/{}", self.slug())
    }

    fn slug(&self) -> &str {
        self.uri
            .path()
            .strip_prefix("/cms-admin/x/")
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default()
    }
}

/// A posted `application/x-www-form-urlencoded` body, in field order.
/// Repeated names (checkbox lists, multi-selects) keep every value.
#[derive(Debug, Clone, Default)]
pub struct AdminForm {
    pairs: Vec<(String, String)>,
}

impl AdminForm {
    /// Parse a urlencoded body. Malformed input gives an empty form.
    #[must_use]
    pub fn parse(body: &[u8]) -> Self {
        Self { pairs: serde_urlencoded::from_bytes(body).unwrap_or_default() }
    }

    /// The last value posted under `name`, as sent.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.pairs.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The value under `name`, trimmed; empty when absent.
    #[must_use]
    pub fn text(&self, name: &str) -> String {
        self.get(name).map(str::trim).unwrap_or_default().to_owned()
    }

    /// The trimmed value, or `None` when absent or blank.
    #[must_use]
    pub fn opt_text(&self, name: &str) -> Option<String> {
        Some(self.text(name)).filter(|v| !v.is_empty())
    }

    /// A checkbox: `on`, `true`, `1` or `yes` is checked.
    #[must_use]
    pub fn checked(&self, name: &str) -> bool {
        matches!(self.get(name).map(str::trim), Some("on" | "true" | "1" | "yes"))
    }

    /// The value parsed as an integer; `None` when absent, blank or not a number.
    #[must_use]
    pub fn int(&self, name: &str) -> Option<i64> {
        self.get(name).and_then(|v| v.trim().parse().ok())
    }

    /// The value parsed as a decimal number.
    #[must_use]
    pub fn float(&self, name: &str) -> Option<f64> {
        self.get(name).and_then(|v| v.trim().parse().ok())
    }

    /// Every value posted under `name`, in order.
    #[must_use]
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.pairs.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect()
    }

    /// Every value under `name` that parses as an integer.
    #[must_use]
    pub fn ints(&self, name: &str) -> Vec<i64> {
        self.all(name).into_iter().filter_map(|v| v.trim().parse().ok()).collect()
    }

    /// The form as a map: the last value of each name.
    #[must_use]
    pub fn to_map(&self) -> HashMap<String, String> {
        self.pairs.iter().cloned().collect()
    }

    /// The raw `(name, value)` pairs.
    #[must_use]
    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }
}

/// Trait every custom admin page implements. Compiled-in via the
/// `inventory` registry — see [`crate::register_admin_page!`].
#[async_trait::async_trait]
pub trait AdminPageHandler: Send + Sync + 'static {
    /// URL slug. The page mounts at `/cms-admin/x/<slug>`. Must
    /// stay stable; bookmarks / links reference this value.
    fn slug(&self) -> &'static str;

    /// Visible label rendered in the sidebar.
    fn label(&self) -> &'static str;

    /// Optional Material-symbols icon name (e.g. `"insights"`,
    /// `"bug_report"`).
    fn icon(&self) -> Option<&'static str> {
        None
    }

    /// Which sidebar section the entry lives under. Defaults to a
    /// generic `Custom("Tools")` bucket so out-of-the-box behaviour
    /// keeps third-party pages visually separated from built-ins.
    fn section(&self) -> SidebarSection {
        SidebarSection::Custom("Tools")
    }

    /// Sibling order within the section (ascending). Defaults to 100
    /// so most third-party pages slot in between built-ins (which use
    /// lower numbers) without needing to think about ordering.
    fn sort_order(&self) -> i32 {
        100
    }

    /// Codename prefix to surface this page in the role permissions
    /// matrix. Returning `Some("acme_reports")` adds a
    /// "Custom admin pages → `<label>`" row with `view` + `edit`
    /// columns, granting / revoking codenames `acme_reports.view`
    /// and `acme_reports.edit`. Default `None` means the page is
    /// not access-controlled by the matrix — hosts can still gate
    /// inside `render()` manually.
    fn permission_resource(&self) -> Option<&'static str> {
        None
    }

    /// The matrix columns for [`Self::permission_resource`], and so the
    /// codenames the dispatcher checks. The default, view + edit, gates
    /// `GET` on `<resource>.view` and every `POST` on `<resource>.edit`.
    /// Including [`Action::Add`] or [`Action::Delete`] makes a `POST` to
    /// the `new` or `delete` action need `.add` or `.delete` instead.
    fn permission_actions(&self) -> &'static [Action] {
        &[Action::View, Action::Edit]
    }

    /// Render the page. The implementation owns its template + form
    /// dispatch (handlers usually render via Tera with the bundled
    /// chrome by extending `rcms_admin/_base.html`).
    async fn render(&self, ctx: AdminPageCtx<'_>) -> Response;

    /// Handle a form posted to `/cms-admin/x/<slug>` or
    /// `/cms-admin/x/<slug>/<action>` (see [`AdminPageCtx::action`]).
    /// The dispatcher has already checked the login, the CSRF token and
    /// the codename (see [`Self::permission_actions`]). The usual reply
    /// is [`AdminPageCtx::redirect`]. The default answers
    /// `405 Method Not Allowed`.
    async fn post(&self, _ctx: AdminPageCtx<'_>, _form: AdminForm) -> Response {
        StatusCode::METHOD_NOT_ALLOWED.into_response()
    }
}

/// The codename a request to admin page `slug` needs, or `None` when
/// the page declares no [`AdminPageHandler::permission_resource`] (or
/// no page has that slug: the dispatcher answers 404).
#[must_use]
pub fn required_codename(slug: &str, reads: bool, action: Option<&str>) -> Option<String> {
    let handler = find_admin_page(slug)?;
    let resource = handler.permission_resource()?;
    let declared = handler.permission_actions();
    let verb = if reads {
        Action::View
    } else {
        match action {
            Some("delete") if declared.contains(&Action::Delete) => Action::Delete,
            Some("new") if declared.contains(&Action::Add) => Action::Add,
            _ => Action::Edit,
        }
    };
    Some(format!("{resource}.{}", verb.as_str()))
}

/// Registration entry submitted via `inventory::submit!` by the
/// `register_admin_page!` macro. Mirrors `PageTypeHandlerRegistration`.
pub struct AdminPageHandlerRegistration {
    pub factory: fn() -> Box<dyn AdminPageHandler>,
}

inventory::collect!(AdminPageHandlerRegistration);

/// Iterator over every registered handler. Used by the sidebar
/// enumeration in `add_chrome()` and by the route dispatcher.
pub fn registered_admin_pages() -> impl Iterator<Item = Box<dyn AdminPageHandler>> {
    inventory::iter::<AdminPageHandlerRegistration>
        .into_iter()
        .map(|r| (r.factory)())
}

/// Find a registered handler by `slug`. Returns `None` for unknown
/// slugs — the dispatch route maps that to a 404.
#[must_use]
pub fn find_admin_page(slug: &str) -> Option<Box<dyn AdminPageHandler>> {
    registered_admin_pages().find(|h| h.slug() == slug)
}
