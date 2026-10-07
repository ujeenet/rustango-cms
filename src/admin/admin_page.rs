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

use axum::http::HeaderMap;
use axum::response::Response;
use rustango::extractors::Tenant;
use std::sync::Arc;

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

    /// Render the page. The implementation owns its template + form
    /// dispatch (handlers usually render via Tera with the bundled
    /// chrome by extending `rcms_admin/_base.html`).
    async fn render(&self, ctx: AdminPageCtx<'_>) -> Response;
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
