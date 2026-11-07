//! Runtime extension-point registry.
//!
//! Hooks are typed: each hook has a precise signature, and registration is
//! compile-time via `inventory::submit!` rather than runtime
//! `HashMap<&str, Box<dyn Fn>>`. Plugins crate-side `submit!` a
//! handler; CMS-side call sites collect every registered handler
//! and fan out.
//!
//! ## Available hooks
//!
//! The v1 surface is intentionally narrow — the 6 hooks below cover
//! the common plugin use cases. More can land as
//! customers ask.
//!
//! | Hook | When | Signature |
//! |---|---|---|
//! | [`AfterPublishPage`]    | When a page goes live | `async fn(HookCtx, &Page)` |
//! | [`BeforeServePage`]     | Just before the public renderer fires | `async fn(HookCtx, &Page, &HeaderMap) -> Option<Response>` |
//! | [`AdminMenuItem`]       | Registers a sidebar entry | static struct |
//! | [`PageActionMenuItem`]  | Registers a kebab-menu entry on the page editor | static struct |
//! | [`PageListingButton`]   | Registers a per-row action on the page tree | static struct |
//! | [`AdminSearchArea`]     | Registers an extra source for global search | async fn returning hits |
//!
//! ## Registering
//!
//! The four behavioural hooks ([`AfterPublishPage`], [`BeforeServePage`],
//! [`AdminSearchArea`], [`DashboardWidget`]) are async and get a
//! [`HookCtx`]: the tenant's pool, its slug and the acting user. The rest
//! are static descriptors.
//!
//! ```ignore
//! use rustango_cms::hooks::{HookCtx, HookFuture};
//! use rustango_cms::page::Page;
//!
//! fn purge_cdn<'a>(ctx: HookCtx<'a>, page: &'a Page) -> HookFuture<'a, ()> {
//!     Box::pin(async move {
//!         tracing::info!(tenant = ctx.tenant, "purging {}", page.url_path);
//!         // …an async HTTP call, a query on ctx.pool…
//!     })
//! }
//! rustango_cms::register_after_publish_page!(purge_cdn);
//! ```
//!
//! ## Failure semantics
//!
//! Hooks fire in registration order. A panicking hook does NOT abort
//! the request — the panic is caught (across its awaits too), logged,
//! and the next handler runs. [`BeforeServePage`] can return
//! `Some(Response)` to short-circuit the renderer.

use std::future::Future;
use std::pin::Pin;

use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::FutureExt as _;
use rustango::sql::Pool;

use crate::page::Page;

/// The boxed future an async hook returns. It may borrow the context
/// and its other arguments.
pub type HookFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What an async hook is told about the request it runs in (#686).
///
/// `#[non_exhaustive]`, so a field added later breaks no registered
/// hook; a caller outside the crate builds one with [`HookCtx::new`].
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct HookCtx<'a> {
    /// The tenant's database.
    pub pool: &'a Pool,
    /// The tenant's slug. A hook that caches must key on it: the pool
    /// is tenant-specific, a process-wide cache is not.
    pub tenant: &'a str,
    /// The signed-in user the hook runs for, if any: an admin user on
    /// admin paths, an admin or a member on public page views (both live
    /// in the tenant's user table). `None` for anonymous visitors and the
    /// schedule sweep.
    pub user_id: Option<i64>,
}

impl<'a> HookCtx<'a> {
    #[must_use]
    pub fn new(pool: &'a Pool, tenant: &'a str, user_id: Option<i64>) -> Self {
        Self { pool, tenant, user_id }
    }
}

/// How long one hook may run. Hooks sit on request paths (a page view, a
/// save), so a hung call in one must not hang the request.
pub const HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Await `fut` for at most [`HOOK_TIMEOUT`], turning a panic or a timeout
/// into `None` and a warning. A hook that panics while holding a
/// `std::sync::Mutex` poisons it; keep locks out of hooks or use tokio's.
async fn guarded<T>(hook: &str, fut: HookFuture<'_, T>) -> Option<T> {
    let caught = std::panic::AssertUnwindSafe(fut).catch_unwind();
    match rustango::__private_runtime::tokio::time::timeout(HOOK_TIMEOUT, caught).await {
        Ok(Ok(v)) => Some(v),
        Ok(Err(e)) => {
            tracing::warn!(target: "rustango_cms::hooks", hook, error = ?e, "hook panicked (continuing)");
            None
        }
        Err(_) => {
            tracing::warn!(target: "rustango_cms::hooks", hook, "hook timed out after {HOOK_TIMEOUT:?} (continuing)");
            None
        }
    }
}

// ---------------------------------------------------------------
// AfterPublishPage
// ---------------------------------------------------------------

/// Fired when a page goes live: an editor's save, a bulk publish, a
/// workflow approval or the schedule sweep. Use for side effects —
/// purge a CDN, ping a webhook, push to a feed indexer.
#[derive(Clone, Copy)]
pub struct AfterPublishPage(pub for<'a> fn(HookCtx<'a>, &'a Page) -> HookFuture<'a, ()>);

inventory::collect!(AfterPublishPage);

/// Register an [`AfterPublishPage`] handler at compile time.
///
/// ```ignore
/// fn my_handler<'a>(ctx: HookCtx<'a>, page: &'a Page) -> HookFuture<'a, ()> {
///     Box::pin(async move { /* ... */ })
/// }
/// register_after_publish_page!(my_handler);
/// ```
#[macro_export]
macro_rules! register_after_publish_page {
    ($f:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::AfterPublishPage($f)
        }
    };
}

/// Run every registered `AfterPublishPage` handler in registration
/// order. Panics are caught + logged so one buggy plugin can't break
/// the publish path.
pub async fn fire_after_publish_page(ctx: HookCtx<'_>, page: &Page) {
    for hook in inventory::iter::<AfterPublishPage>() {
        guarded("after_publish_page", (hook.0)(ctx, page)).await;
    }
}

// ---------------------------------------------------------------
// BeforeServePage
// ---------------------------------------------------------------

/// Fired in the public router just after `resolve_path` returns a
/// page and *before* the renderer fires. Return `Some(Response)` to
/// short-circuit (redirect, 403, custom render, etc.); `None` to
/// let the normal renderer handle it.
///
/// Multiple hooks register in order; the first one to return
/// `Some(Response)` wins.
#[derive(Clone, Copy)]
pub struct BeforeServePage(
    pub for<'a> fn(HookCtx<'a>, &'a Page, &'a HeaderMap) -> HookFuture<'a, Option<Response>>,
);

inventory::collect!(BeforeServePage);

#[macro_export]
macro_rules! register_before_serve_page {
    ($f:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::BeforeServePage($f)
        }
    };
}

/// Fan out: returns the first short-circuit response or `None`.
pub async fn fire_before_serve_page(ctx: HookCtx<'_>, page: &Page, headers: &HeaderMap) -> Option<Response> {
    for hook in inventory::iter::<BeforeServePage>() {
        if let Some(Some(resp)) = guarded("before_serve_page", (hook.0)(ctx, page, headers)).await {
            return Some(resp);
        }
    }
    None
}

// ---------------------------------------------------------------
// AdminMenuItem
// ---------------------------------------------------------------

/// One sidebar entry contributed by a plugin. Renders below the
/// built-in Settings section. The icon name is a Material-symbols
/// glyph (e.g. `"analytics"`, `"chat"`, `"science"`).
#[derive(Debug, Clone, Copy)]
pub struct AdminMenuItem {
    /// Stable identifier (used for dedup + active-state matching).
    pub key: &'static str,
    /// Display label.
    pub label: &'static str,
    /// Material-symbols icon name.
    pub icon: &'static str,
    /// Absolute URL or admin-relative URL the entry links to.
    pub href: &'static str,
}

inventory::collect!(AdminMenuItem);

#[macro_export]
macro_rules! register_admin_menu_item {
    ($key:expr, $label:expr, $icon:expr, $href:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::AdminMenuItem {
                key: $key,
                label: $label,
                icon: $icon,
                href: $href,
            }
        }
    };
}

/// Collect every registered AdminMenuItem for the sidebar
/// templates. Stable ordering by `key`.
#[must_use]
pub fn admin_menu_items() -> Vec<&'static AdminMenuItem> {
    let mut items: Vec<&'static AdminMenuItem> = inventory::iter::<AdminMenuItem>().collect();
    items.sort_by_key(|i| i.key);
    items
}

// ---------------------------------------------------------------
// HelpMenuItem (#127)
// ---------------------------------------------------------------

/// One sidebar entry rendered under a dedicated "Help" section.
/// Same field shape as [`AdminMenuItem`] — kept distinct so plugins
/// can contribute docs / shortcuts links without polluting the
/// generic plugin section.
#[derive(Debug, Clone, Copy)]
pub struct HelpMenuItem {
    pub key: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub href: &'static str,
}

inventory::collect!(HelpMenuItem);

#[macro_export]
macro_rules! register_help_menu_item {
    ($key:expr, $label:expr, $icon:expr, $href:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::HelpMenuItem {
                key: $key,
                label: $label,
                icon: $icon,
                href: $href,
            }
        }
    };
}

#[must_use]
pub fn help_menu_items() -> Vec<&'static HelpMenuItem> {
    let mut items: Vec<&'static HelpMenuItem> = inventory::iter::<HelpMenuItem>().collect();
    items.sort_by_key(|i| i.key);
    items
}

// ---------------------------------------------------------------
// AccountSettingsPanel (#128)
// ---------------------------------------------------------------

/// One panel contributed to the per-user `/cms-admin/me` page.
/// Pre-rendered HTML the template emits via `| safe` — hosts wire
/// their own POST endpoints (the registry stays read-only).
#[derive(Debug, Clone, Copy)]
pub struct AccountSettingsPanel {
    pub key: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub html: &'static str,
}

inventory::collect!(AccountSettingsPanel);

#[macro_export]
macro_rules! register_account_settings_panel {
    ($key:expr, $label:expr, $icon:expr, $html:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::AccountSettingsPanel {
                key: $key,
                label: $label,
                icon: $icon,
                html: $html,
            }
        }
    };
}

// ---------------------------------------------------------------
// UserbarItem (#148)
// ---------------------------------------------------------------

/// Plugin-contributed entry in the on-site editor userbar.
/// `href_template` may include `{page_id}` — the renderer substitutes
/// the live page id (or empty string when no page context).
#[derive(Debug, Clone, Copy)]
pub struct UserbarItem {
    pub key: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub href_template: &'static str,
}

inventory::collect!(UserbarItem);

#[macro_export]
macro_rules! register_userbar_item {
    ($key:expr, $label:expr, $icon:expr, $href:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::UserbarItem {
                key: $key,
                label: $label,
                icon: $icon,
                href_template: $href,
            }
        }
    };
}

#[must_use]
pub fn userbar_items() -> Vec<&'static UserbarItem> {
    let mut items: Vec<&'static UserbarItem> = inventory::iter::<UserbarItem>().collect();
    items.sort_by_key(|i| i.key);
    items
}

// ---------------------------------------------------------------
// DashboardWidget (#144)
// ---------------------------------------------------------------

/// One widget contributed to /cms-admin/dashboard. `render` builds its
/// HTML fragment per request, with the tenant's pool and the viewer, so
/// it can show live numbers (charts, counts, recent rows…). The HTML is
/// inserted as is: escape anything that came from users.
#[derive(Clone, Copy)]
pub struct DashboardWidget {
    pub key: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub render: for<'a> fn(HookCtx<'a>) -> HookFuture<'a, String>,
}

inventory::collect!(DashboardWidget);

#[macro_export]
macro_rules! register_dashboard_widget {
    ($key:expr, $label:expr, $icon:expr, $render:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::DashboardWidget {
                key: $key,
                label: $label,
                icon: $icon,
                render: $render,
            }
        }
    };
}

#[must_use]
pub fn dashboard_widgets() -> Vec<&'static DashboardWidget> {
    let mut items: Vec<&'static DashboardWidget> = inventory::iter::<DashboardWidget>().collect();
    items.sort_by_key(|i| i.key);
    items
}

/// Render every dashboard widget, sorted by key: `(widget, html)`. A
/// widget whose render panics is left out.
pub async fn render_dashboard_widgets(ctx: HookCtx<'_>) -> Vec<(&'static DashboardWidget, String)> {
    let mut out = Vec::new();
    for w in dashboard_widgets() {
        if let Some(html) = guarded("dashboard_widget", (w.render)(ctx)).await {
            out.push((w, html));
        }
    }
    out
}

#[must_use]
pub fn account_settings_panels() -> Vec<&'static AccountSettingsPanel> {
    let mut items: Vec<&'static AccountSettingsPanel> =
        inventory::iter::<AccountSettingsPanel>().collect();
    items.sort_by_key(|i| i.key);
    items
}

// ---------------------------------------------------------------
// BulkActionSpec (#134)
// ---------------------------------------------------------------

/// Plugin-contributed bulk action declaration.
///
/// Each spec targets a specific listing resource (e.g. `"cms_page"`,
/// `"cms_snippet"`, `"cms_media"`). The framework renders the button
/// on every matching listing's bulk bar; clicking it POSTs the
/// selected ids to `action_url`. Hosts wire that route themselves —
/// the hook stays decoupled from the request lifecycle so plugins
/// don't need to interact with the framework's tenancy / session
/// extractors.
///
/// The existing per-handler `bulk_actions()` shape on
/// [`crate::library::LibraryTypeHandler`] keeps working unchanged;
/// this hook provides an additional, cross-listing extension point
/// for plugins that want to contribute actions without owning a
/// page-type or library-type handler.
#[derive(Debug, Clone, Copy)]
pub struct BulkActionSpec {
    /// Resource slug — `"cms_page"`, `"cms_snippet"`, `"cms_media"`,
    /// `"cms_redirect"`, etc. Listings filter by this.
    pub resource: &'static str,
    /// Stable identifier (used for dedup + the POST body's `action`
    /// key).
    pub key: &'static str,
    /// Display label rendered in the bulk-bar dropdown.
    pub label: &'static str,
    /// Material-symbols icon name.
    pub icon: &'static str,
    /// When true, the button renders in the danger style + the
    /// confirm modal is shown before submit.
    pub danger: bool,
    /// Confirmation message shown via the existing `data-confirm`
    /// modal. Empty means no confirm step.
    pub confirm_message: &'static str,
    /// URL the form POSTs to. The framework appends the selected
    /// ids as `ids=<id>&ids=<id>…` and the spec's `key` as
    /// `action=<key>`. Host wires the route to receive that shape.
    pub action_url: &'static str,
}

inventory::collect!(BulkActionSpec);

#[macro_export]
macro_rules! register_bulk_action {
    (
        resource = $resource:expr,
        key = $key:expr,
        label = $label:expr,
        icon = $icon:expr,
        danger = $danger:expr,
        confirm = $confirm:expr,
        action_url = $action_url:expr $(,)?
    ) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::BulkActionSpec {
                resource: $resource,
                key: $key,
                label: $label,
                icon: $icon,
                danger: $danger,
                confirm_message: $confirm,
                action_url: $action_url,
            }
        }
    };
}

/// Every plugin-contributed bulk action whose `resource` matches
/// `resource`. Stable ordering by `key`.
#[must_use]
pub fn bulk_actions_for(resource: &str) -> Vec<&'static BulkActionSpec> {
    let mut items: Vec<&'static BulkActionSpec> = inventory::iter::<BulkActionSpec>()
        .filter(|a| a.resource == resource)
        .collect();
    items.sort_by_key(|i| i.key);
    items
}

// Built-ins shipped by rustango-cms itself. Hosts can register more
// via `register_help_menu_item!`.
inventory::submit! {
    HelpMenuItem {
        key: "docs",
        label: "Documentation",
        icon: "menu_book",
        // The guides, read inside the admin (`admin::help_center`).
        href: "/cms-admin/help",
    }
}
inventory::submit! {
    HelpMenuItem {
        key: "keyboard-shortcuts",
        // Special marker href — the sidebar template renders this
        // entry as a `data-kbd-modal-open` button instead of a link,
        // so the click opens the cheat sheet from #121 in-page.
        label: "Keyboard shortcuts",
        icon: "keyboard",
        href: "#kbd-cheatsheet",
    }
}
inventory::submit! {
    HelpMenuItem {
        key: "styleguide",
        label: "Styleguide",
        icon: "palette",
        href: "/cms-admin/styleguide",
    }
}

// ---------------------------------------------------------------
// PageActionMenuItem
// ---------------------------------------------------------------

/// One kebab-menu entry on the page editor (next to the existing
/// "View live" / "Open preview" / "Form fields" entries).
#[derive(Debug, Clone, Copy)]
pub struct PageActionMenuItem {
    pub key: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    /// Receives `(page_id)` as a path parameter — hosts use a
    /// `{page_id}` placeholder if needed (left as-is otherwise).
    pub href_template: &'static str,
}

inventory::collect!(PageActionMenuItem);

#[macro_export]
macro_rules! register_page_action_menu_item {
    ($key:expr, $label:expr, $icon:expr, $href:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::PageActionMenuItem {
                key: $key,
                label: $label,
                icon: $icon,
                href_template: $href,
            }
        }
    };
}

/// Collect every registered PageActionMenuItem, with the page id
/// interpolated into each entry's `href_template`.
#[must_use]
pub fn page_action_menu_items(page_id: i64) -> Vec<serde_json::Value> {
    let mut items: Vec<&'static PageActionMenuItem> =
        inventory::iter::<PageActionMenuItem>().collect();
    items.sort_by_key(|i| i.key);
    items
        .into_iter()
        .map(|i| {
            serde_json::json!({
                "key": i.key,
                "label": i.label,
                "icon": i.icon,
                "href": i.href_template.replace("{page_id}", &page_id.to_string()),
            })
        })
        .collect()
}

// ---------------------------------------------------------------
// PageListingButton
// ---------------------------------------------------------------

/// One per-row action on the page tree (next to "Edit" / "+ Child"
/// / "Clone" / "Delete").
#[derive(Debug, Clone, Copy)]
pub struct PageListingButton {
    pub key: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub href_template: &'static str,
}

inventory::collect!(PageListingButton);

#[macro_export]
macro_rules! register_page_listing_button {
    ($key:expr, $label:expr, $icon:expr, $href:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::PageListingButton {
                key: $key,
                label: $label,
                icon: $icon,
                href_template: $href,
            }
        }
    };
}

#[must_use]
pub fn page_listing_buttons(page_id: i64) -> Vec<serde_json::Value> {
    let mut items: Vec<&'static PageListingButton> =
        inventory::iter::<PageListingButton>().collect();
    items.sort_by_key(|i| i.key);
    items
        .into_iter()
        .map(|i| {
            serde_json::json!({
                "key": i.key,
                "label": i.label,
                "icon": i.icon,
                "href": i.href_template.replace("{page_id}", &page_id.to_string()),
            })
        })
        .collect()
}

// ---------------------------------------------------------------
// PageHeaderButton (#140)
// ---------------------------------------------------------------

/// One button rendered alongside Save / Publish on the page editor
/// header. Differs from [`PageActionMenuItem`] — that lives inside the
/// kebab dropdown; this is a top-level button always visible.
#[derive(Debug, Clone, Copy)]
pub struct PageHeaderButton {
    pub key: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub href_template: &'static str,
    /// Visual style. Accepted: `"primary"` (filled), `"tonal"`,
    /// `"outlined"` (default), `"danger"`. Unknown values fall back
    /// to outlined so a typo doesn't kill the page.
    pub style: &'static str,
}

inventory::collect!(PageHeaderButton);

#[macro_export]
macro_rules! register_page_header_button {
    (
        key = $key:expr,
        label = $label:expr,
        icon = $icon:expr,
        href = $href:expr,
        style = $style:expr $(,)?
    ) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::PageHeaderButton {
                key: $key,
                label: $label,
                icon: $icon,
                href_template: $href,
                style: $style,
            }
        }
    };
}

/// Resolve every registered header button against `page_id`,
/// substituting `{page_id}` into each href.
#[must_use]
pub fn page_header_buttons(page_id: i64) -> Vec<serde_json::Value> {
    let mut items: Vec<&'static PageHeaderButton> = inventory::iter::<PageHeaderButton>().collect();
    items.sort_by_key(|i| i.key);
    items
        .into_iter()
        .map(|i| {
            serde_json::json!({
                "key": i.key,
                "label": i.label,
                "icon": i.icon,
                "style": i.style,
                "href": i.href_template.replace("{page_id}", &page_id.to_string()),
            })
        })
        .collect()
}

/// Collect raw page-listing buttons (no `{page_id}` substitution).
/// Used by the page-list template which loops over rows and does the
/// substitution per row inside Tera.
#[must_use]
pub fn page_listing_buttons_raw() -> Vec<serde_json::Value> {
    let mut items: Vec<&'static PageListingButton> =
        inventory::iter::<PageListingButton>().collect();
    items.sort_by_key(|i| i.key);
    items
        .into_iter()
        .map(|i| {
            serde_json::json!({
                "key": i.key,
                "label": i.label,
                "icon": i.icon,
                "href_template": i.href_template,
            })
        })
        .collect()
}

// ---------------------------------------------------------------
// AdminSearchArea
// ---------------------------------------------------------------

/// One hit row from a plugin-contributed search source.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdminSearchHit {
    pub label: String,
    pub href: String,
    /// Optional one-line subtitle (used for slug / path / category).
    pub subtitle: String,
}

/// Plugin-contributed search source for `/cms-admin/search`.
/// Receives the query string (and the tenant's pool, so it can query);
/// returns up to N hits to merge into the global search response.
#[derive(Clone, Copy)]
pub struct AdminSearchArea {
    pub key: &'static str,
    pub label: &'static str,
    pub search: for<'a> fn(HookCtx<'a>, &'a str) -> HookFuture<'a, Vec<AdminSearchHit>>,
}

inventory::collect!(AdminSearchArea);

#[macro_export]
macro_rules! register_admin_search_area {
    ($key:expr, $label:expr, $search:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::AdminSearchArea {
                key: $key,
                label: $label,
                search: $search,
            }
        }
    };
}

/// Run every registered AdminSearchArea against `query` and return
/// a flattened `(area_label, hits)` list.
#[must_use]
pub async fn run_admin_search_areas(ctx: HookCtx<'_>, query: &str) -> Vec<(&'static str, Vec<AdminSearchHit>)> {
    let mut out = Vec::new();
    for area in inventory::iter::<AdminSearchArea>() {
        let hits = guarded("admin_search_area", (area.search)(ctx, query)).await.unwrap_or_default();
        if !hits.is_empty() {
            out.push((area.label, hits));
        }
    }
    out
}

// ---------------------------------------------------------------
// AdminCss / AdminJs (#422)
// ---------------------------------------------------------------

/// A blob of CSS injected into every admin page's `<head>`. The string is emitted verbatim
/// inside a `<style>` tag (host-trusted), so plugins can restyle the
/// chrome, hide elements, or theme their own admin pages without
/// shipping a separate stylesheet route.
#[derive(Debug, Clone, Copy)]
pub struct AdminCss(pub &'static str);

inventory::collect!(AdminCss);

/// Register global admin CSS.
///
/// ```ignore
/// register_admin_css!(".my-plugin-badge { color: rebeccapurple; }");
/// ```
#[macro_export]
macro_rules! register_admin_css {
    ($css:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::AdminCss($css)
        }
    };
}

/// Every registered global admin CSS blob, in registration order.
#[must_use]
pub fn admin_css() -> Vec<&'static str> {
    inventory::iter::<AdminCss>().map(|c| c.0).collect()
}

/// A blob of JS injected just before `</body>` on every admin page.
/// Emitted verbatim inside a
/// `<script>` tag (host-trusted).
#[derive(Debug, Clone, Copy)]
pub struct AdminJs(pub &'static str);

inventory::collect!(AdminJs);

/// Register global admin JS.
#[macro_export]
macro_rules! register_admin_js {
    ($js:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::AdminJs($js)
        }
    };
}

/// Every registered global admin JS blob, in registration order.
#[must_use]
pub fn admin_js() -> Vec<&'static str> {
    inventory::iter::<AdminJs>().map(|j| j.0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Dogfood the registries (compiled only in test builds).
    inventory::submit! { AdminCss(".rcms-admin-css-marker{display:none}") }
    inventory::submit! { AdminJs("/* rcms-admin-js-marker */") }

    #[test]
    fn admin_css_and_js_collect_registered_blobs() {
        assert!(
            admin_css()
                .iter()
                .any(|c| c.contains("rcms-admin-css-marker")),
            "registered CSS blob is collected"
        );
        assert!(
            admin_js()
                .iter()
                .any(|j| j.contains("rcms-admin-js-marker")),
            "registered JS blob is collected"
        );
    }

    #[test]
    fn page_action_href_template_substitutes_page_id() {
        let raw = PageActionMenuItem {
            key: "x",
            label: "X",
            icon: "extension",
            href_template: "/cms-admin/x/plugin?page={page_id}",
        };
        let rendered = raw.href_template.replace("{page_id}", "42");
        assert_eq!(rendered, "/cms-admin/x/plugin?page=42");
    }

    #[test]
    fn bulk_action_spec_fields_round_trip() {
        let spec = BulkActionSpec {
            resource: "cms_page",
            key: "archive_old",
            label: "Archive old",
            icon: "archive",
            danger: false,
            confirm_message: "Archive every selected old page?",
            action_url: "/plugin/archive",
        };
        assert_eq!(spec.resource, "cms_page");
        assert!(!spec.danger);
        assert!(spec.confirm_message.contains("Archive"));
    }

    #[test]
    fn page_listing_button_href_template_substitutes_page_id() {
        let raw = PageListingButton {
            key: "x",
            label: "X",
            icon: "extension",
            href_template: "/plugin/{page_id}/do",
        };
        let rendered = raw.href_template.replace("{page_id}", "7");
        assert_eq!(rendered, "/plugin/7/do");
    }
}
