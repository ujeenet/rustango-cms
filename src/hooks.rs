//! Runtime extension-point registry (#107, Wagtail parity B13).
//!
//! Mirrors the shape of Wagtail's `@hooks.register('NAME', fn)` but
//! typed: each hook has a precise signature, and registration is
//! compile-time via `inventory::submit!` rather than runtime
//! `HashMap<&str, Box<dyn Fn>>`. Plugins crate-side `submit!` a
//! handler; CMS-side call sites collect every registered handler
//! and fan out.
//!
//! ## Available hooks
//!
//! The v1 surface is intentionally narrow — the 6 hooks below cover
//! ~80% of Wagtail's plugin ecosystem use cases. More can land as
//! customers ask.
//!
//! | Hook | When | Signature |
//! |---|---|---|
//! | [`AfterPublishPage`]    | After `page_edit_submit` saves a Published row | `fn(&Page)` |
//! | [`BeforeServePage`]     | Just before the public renderer fires | `fn(&Page, &HeaderMap) -> Option<Response>` |
//! | [`AdminMenuItem`]       | Registers a sidebar entry | static struct |
//! | [`PageActionMenuItem`]  | Registers a kebab-menu entry on the page editor | static struct |
//! | [`PageListingButton`]   | Registers a per-row action on the page tree | static struct |
//! | [`AdminSearchArea`]     | Registers an extra source for global search | static fn returning hits |
//!
//! ## Registering
//!
//! ```ignore
//! use rustango_cms::hooks::{register_after_publish_page, AfterPublishPage};
//! use rustango_cms::Page;
//!
//! fn warm_cdn_cache(page: &Page) {
//!     tracing::info!("warming CDN for {}", page.url_path);
//! }
//!
//! register_after_publish_page!(warm_cdn_cache);
//! ```
//!
//! ## Failure semantics
//!
//! Hooks fire in registration order. A panicking hook does NOT abort
//! the request — the call site catches the panic, logs it, and
//! continues to the next handler. The only exception is
//! [`BeforeServePage`], which can return `Some(Response)` to
//! short-circuit the renderer.

use axum::http::HeaderMap;
use axum::response::Response;

use crate::page::Page;

// ---------------------------------------------------------------
// AfterPublishPage
// ---------------------------------------------------------------

/// Fired after a page transitions to (or stays in) Published
/// following `page_edit_submit`. Use for fire-and-forget side
/// effects: warm a CDN, ping a webhook, push to a feed indexer.
#[derive(Debug, Clone, Copy)]
pub struct AfterPublishPage(pub fn(&Page));

inventory::collect!(AfterPublishPage);

/// Register an [`AfterPublishPage`] handler at compile time.
///
/// ```ignore
/// fn my_handler(page: &Page) { /* ... */ }
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

/// Fan out: call every registered `AfterPublishPage` handler in
/// registration order. Panics are caught + logged so one buggy
/// plugin can't break the publish path.
pub fn fire_after_publish_page(page: &Page) {
    for hook in inventory::iter::<AfterPublishPage>() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (hook.0)(page)));
        if let Err(e) = result {
            tracing::warn!(
                target: "rustango_cms::hooks",
                hook = "after_publish_page",
                error = ?e,
                "hook panicked (continuing)"
            );
        }
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
#[derive(Debug, Clone, Copy)]
pub struct BeforeServePage(pub fn(&Page, &HeaderMap) -> Option<Response>);

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
pub fn fire_before_serve_page(page: &Page, headers: &HeaderMap) -> Option<Response> {
    for hook in inventory::iter::<BeforeServePage>() {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (hook.0)(page, headers)));
        match result {
            Ok(Some(resp)) => return Some(resp),
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::hooks",
                    hook = "before_serve_page",
                    error = ?e,
                    "hook panicked (continuing)"
                );
            }
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
// HelpMenuItem (#127, Wagtail parity C7)
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
// AccountSettingsPanel (#128, Wagtail parity C6)
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
// UserbarItem (#148, Wagtail parity D4)
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
// DashboardWidget (#144, Wagtail parity D1)
// ---------------------------------------------------------------

/// One widget contributed to /cms-admin/dashboard. Pre-rendered
/// HTML; hosts compose whatever data shape they like (charts,
/// counts, recent rows, etc.) and emit the fragment.
#[derive(Debug, Clone, Copy)]
pub struct DashboardWidget {
    pub key: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub html: &'static str,
}

inventory::collect!(DashboardWidget);

#[macro_export]
macro_rules! register_dashboard_widget {
    ($key:expr, $label:expr, $icon:expr, $html:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::hooks::DashboardWidget {
                key: $key,
                label: $label,
                icon: $icon,
                html: $html,
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

#[must_use]
pub fn account_settings_panels() -> Vec<&'static AccountSettingsPanel> {
    let mut items: Vec<&'static AccountSettingsPanel> =
        inventory::iter::<AccountSettingsPanel>().collect();
    items.sort_by_key(|i| i.key);
    items
}

// ---------------------------------------------------------------
// BulkActionSpec (#134, Wagtail parity C4)
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
        href: "https://rustango-cms.dev/docs",
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
// PageHeaderButton (#140, Wagtail parity D10)
// ---------------------------------------------------------------

/// One button rendered alongside Save / Publish on the page editor
/// header. Wagtail equivalent: the `register_page_header_buttons`
/// hook. Differs from [`PageActionMenuItem`] — that lives inside the
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
/// Receives the query string; returns up to N hits to merge into
/// the global search response.
#[derive(Debug, Clone, Copy)]
pub struct AdminSearchArea {
    pub key: &'static str,
    pub label: &'static str,
    pub search: fn(&str) -> Vec<AdminSearchHit>,
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
pub fn run_admin_search_areas(query: &str) -> Vec<(&'static str, Vec<AdminSearchHit>)> {
    let mut out = Vec::new();
    for area in inventory::iter::<AdminSearchArea>() {
        let hits = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (area.search)(query)))
            .unwrap_or_else(|e| {
                tracing::warn!(
                    target: "rustango_cms::hooks",
                    hook = "admin_search_area",
                    area = %area.key,
                    error = ?e,
                    "search area panicked (continuing)"
                );
                Vec::new()
            });
        if !hits.is_empty() {
            out.push((area.label, hits));
        }
    }
    out
}

// ---------------------------------------------------------------
// AdminCss / AdminJs (#422, Wagtail insert_global_admin_css/js)
// ---------------------------------------------------------------

/// A blob of CSS injected into every admin page's `<head>` — Wagtail's
/// `insert_global_admin_css` parity. The string is emitted verbatim
/// inside a `<style>` tag (host-trusted), so plugins can restyle the
/// chrome, hide elements, or theme their own admin pages without
/// shipping a separate stylesheet route.
#[derive(Debug, Clone, Copy)]
pub struct AdminCss(pub &'static str);

inventory::collect!(AdminCss);

/// Register global admin CSS (#422).
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

/// A blob of JS injected just before `</body>` on every admin page —
/// Wagtail's `insert_global_admin_js` parity. Emitted verbatim inside a
/// `<script>` tag (host-trusted).
#[derive(Debug, Clone, Copy)]
pub struct AdminJs(pub &'static str);

inventory::collect!(AdminJs);

/// Register global admin JS (#422).
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
