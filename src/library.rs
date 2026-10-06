//! `Library` — Wagtail-shape reusable content (renamed from "snippets").
//!
//! Library items are non-page records that pages reference by id —
//! authors, FAQ entries, hero banners, callout blocks, etc. The
//! pattern mirrors `register_page_type!`: each type implements
//! [`LibraryTypeHandler`], registers via the
//! [`crate::register_library_type!`] macro, gets a row in
//! `cms_library_type`, and exposes its own extension table for the
//! actual content.
//!
//! ```ignore
//! #[derive(Default)]
//! pub struct AuthorEntry;
//!
//! #[async_trait::async_trait]
//! impl rustango_cms::library::LibraryTypeHandler for AuthorEntry {
//!     fn app_label(&self) -> &'static str { "blog" }
//!     fn type_name(&self) -> &'static str { "Author" }
//!     fn verbose_name(&self) -> &'static str { "Author" }
//!     fn extension_table(&self) -> Option<&'static str> { Some("blog_author") }
//! }
//!
//! rustango_cms::register_library_type!(AuthorEntry);
//! ```

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// Registry row keyed on `type_name`. Seeded on boot from the
/// inventory of `register_library_type!`-registered handlers,
/// mirroring `PageType`.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_library_type",
    app = "cms",
    admin(
        list_display = "app_label, type_name, verbose_name, extension_table",
        ordering = "type_name",
    )
)]
pub struct LibraryType {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64, index)]
    pub app_label: String,
    #[rustango(max_length = 64, index)]
    pub type_name: String,
    #[rustango(max_length = 128)]
    pub verbose_name: String,
    /// Optional name of the user-authored extension table that holds
    /// the typed fields. `None` when the handler is metadata-only.
    #[rustango(max_length = 128)]
    pub extension_table: String,
    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

/// One column shown in the snippet list view for a given type.
/// Mirrors Django admin's `list_display` shape — a column can be a
/// scalar field on `cms_snippet`, a `data.<key>` JSON lookup, a
/// method-field rendered by the handler, or a computed value
/// derived at render time.
#[derive(Debug, Clone)]
pub enum ListColumn {
    /// Scalar column on `cms_snippet` (`slug`, `title`,
    /// `updated_at`, …). Renders with type-aware formatting.
    Field {
        column: &'static str,
        label: &'static str,
    },
    /// `data.<key>` JSON lookup. Renders with the requested kind so
    /// e.g. a boolean in `data` shows as a checkbox.
    JsonField {
        key: &'static str,
        label: &'static str,
        kind: ListColumnKind,
    },
    /// Method field — the admin calls
    /// [`LibraryTypeHandler::render_cell`] with the column name plus
    /// the snippet row; the handler returns pre-escaped display
    /// HTML.
    Method {
        name: &'static str,
        label: &'static str,
    },
    /// Runtime-derived column. Same render pathway as `Method`
    /// (cells route through `render_cell`), but declared separately
    /// so authors can express intent — `Method` connotes "computed
    /// by a method on the typed extension", `Computed` connotes
    /// "derived from any context the handler wants".
    Computed {
        name: &'static str,
        label: &'static str,
    },
}

impl ListColumn {
    /// Visible header label for this column.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Field { label, .. }
            | Self::JsonField { label, .. }
            | Self::Method { label, .. }
            | Self::Computed { label, .. } => label,
        }
    }

    /// Stable identifier — used by the per-user column picker (#133)
    /// to persist hide/show state across reloads.
    #[must_use]
    pub fn key(&self) -> &'static str {
        match self {
            Self::Field { column, .. } => column,
            Self::JsonField { key, .. } => key,
            Self::Method { name, .. } | Self::Computed { name, .. } => name,
        }
    }
}

/// Sort direction for [`LibraryTypeHandler::default_ordering`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDir {
    Asc,
    Desc,
}

impl OrderDir {
    #[must_use]
    pub fn is_desc(self) -> bool {
        matches!(self, Self::Desc)
    }
}

/// Visual emphasis for a bulk action button. Maps to the
/// `.btn` / `.btn-outlined` / `.btn-danger` CSS variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkActionStyle {
    Primary,
    Outlined,
    Danger,
}

/// One bulk action exposed above the list table — pair the
/// multi-select checkbox column with a labeled button. Submits
/// as POST `/cms-admin/library/{type}/bulk` with
/// `action=<name>&ids=<id>&ids=<id>...`; the handler routes the
/// selection to [`LibraryTypeHandler::handle_bulk_action`].
#[derive(Debug, Clone)]
pub struct BulkAction {
    /// Stable identifier sent as the `action=` form field.
    pub name: &'static str,
    /// Visible button label.
    pub label: &'static str,
    /// Optional Material-symbols icon name.
    pub icon: Option<&'static str>,
    /// Optional confirmation prompt — when set, the JS submit
    /// handler calls `window.confirm(message)` first.
    pub confirm_message: Option<&'static str>,
    /// Visual emphasis.
    pub style: BulkActionStyle,
}

impl BulkAction {
    /// Smallest constructor — `(name, label, style)`. Add icon /
    /// confirm fluently.
    #[must_use]
    pub fn new(name: &'static str, label: &'static str, style: BulkActionStyle) -> Self {
        Self {
            name,
            label,
            icon: None,
            confirm_message: None,
            style,
        }
    }

    #[must_use]
    pub fn with_icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }

    #[must_use]
    pub fn with_confirm(mut self, msg: &'static str) -> Self {
        self.confirm_message = Some(msg);
        self
    }
}

/// One row contributed by [`LibraryTypeHandler::menu_picker_entries`]
/// to the WP-style menu builder's Library tab (#49). Each entry
/// renders as a draggable picker card; dropping one into the tree
/// creates a MenuItem with `external_url = url` and `label`
/// inherited.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MenuPickerEntry {
    /// Human label shown on the picker card and used as the menu
    /// item's default label.
    pub label: String,
    /// Destination URL the menu item points at. Stored as
    /// `MenuItem.external_url` on save.
    pub url: String,
    /// Optional kind hint surfaced as a chip on the picker card
    /// (e.g. "Partner", "Doc", "Microsite"). Useful when one
    /// library type contributes multiple kinds.
    #[serde(default)]
    pub kind_hint: Option<String>,
}

impl MenuPickerEntry {
    #[must_use]
    pub fn new(label: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            url: url.into(),
            kind_hint: None,
        }
    }

    #[must_use]
    pub fn with_kind_hint(mut self, hint: impl Into<String>) -> Self {
        self.kind_hint = Some(hint.into());
        self
    }
}

/// How a scalar value should render in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListColumnKind {
    Text,
    Bool,
    Integer,
    Float,
    Date,
    DateTime,
}

/// Trait every library type implements. Mirrors
/// [`crate::PageTypeHandler`] — same registration story, same
/// per-tenant seeding pass.
#[async_trait]
pub trait LibraryTypeHandler: Send + Sync + 'static {
    /// Django-shape app label.
    fn app_label(&self) -> &'static str;
    /// Canonical type name. Unique per tenant.
    fn type_name(&self) -> &'static str;
    /// Human-readable name for the admin sidebar.
    fn verbose_name(&self) -> &'static str;
    /// SQL table where the typed extension data lives, or `None` for
    /// a metadata-only registration (rare; useful for "Author" rows
    /// that just hold name + bio in `cms_library_type` itself).
    fn extension_table(&self) -> Option<&'static str> {
        None
    }

    /// Optional Tera template the public-render handler uses to
    /// pre-render this snippet type's rows. When `Some("<path>")`
    /// the renderer pre-fetches every row of this type, renders
    /// each through the template (with `{ snippet: <row JSON> }`
    /// in context), and stamps the resulting HTML into the public
    /// Tera ctx map `_snippets_html_by_id` keyed on the snippet's
    /// `id`.
    ///
    /// Templates pull the pre-rendered HTML via
    /// `{{ snippet_html(type="Category", id=4) }}` so multiple
    /// pages that reference the same snippet share one render.
    ///
    /// `None` (default) — snippet has no public-side surface; the
    /// only consumers are admin / API.
    fn render_template(&self) -> Option<&'static str> {
        None
    }

    /// Whether an individual element of this type may name its own
    /// template, overriding [`Self::render_template`] for that row
    /// alone. The name is read from the element's `data.template`.
    ///
    /// `false` (default) — every row of the type renders through the
    /// one template the handler declares, which is what a "Category"
    /// or "Author" wants: uniform presentation is the point.
    ///
    /// `true` — the element *is* the presentation. A reporting widget
    /// that carries its own query and its own markup, a campaign
    /// banner an editor restyles per placement: these differ per row
    /// by design, and a type-wide template would just be a dispatch
    /// table keyed on the row.
    ///
    /// Opt-in rather than always-on so a stray `template` key in a
    /// row's `data` can't silently repoint an existing type's render.
    ///
    /// Types that opt in participate in the public pre-render even
    /// when [`Self::render_template`] is `None` — an element template
    /// alone is enough. Rows that name nothing and have no type
    /// default are skipped, exactly as before.
    fn allows_element_template(&self) -> bool {
        false
    }

    /// Columns rendered in this type's list view. Defaults to the
    /// title + slug + updated_at trio when not overridden.
    fn list_display(&self) -> Vec<ListColumn> {
        vec![
            ListColumn::Field {
                column: "title",
                label: "Title",
            },
            ListColumn::Field {
                column: "slug",
                label: "Slug",
            },
            ListColumn::Field {
                column: "updated_at",
                label: "Updated",
            },
        ]
    }

    /// Fields the search bar matches against. Defaults to
    /// `["title", "slug"]`; case-insensitive `ILIKE '%q%'`.
    fn search_fields(&self) -> Vec<&'static str> {
        vec!["title", "slug"]
    }

    /// `data.<key>` keys exposed as filter dropdowns. The admin
    /// reads distinct values + counts per key and renders one chip
    /// per option. Defaults to no filters.
    fn list_filter(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// Render a method-field cell. Called for every
    /// [`ListColumn::Method`] and [`ListColumn::Computed`] entry.
    /// Return pre-escaped HTML — the admin emits it via Tera's
    /// `| safe`. The default empty implementation is fine when a
    /// handler declares no method / computed columns.
    fn render_cell(&self, _name: &str, _snippet: &crate::snippet::Snippet) -> String {
        String::new()
    }

    /// Default ordering for the list view — applied in order. The
    /// admin's `?ordering=` query param overrides this. Defaults to
    /// "most-recently-updated first" so freshly-edited rows surface
    /// at the top without authors needing to think about ordering.
    fn default_ordering(&self) -> Vec<(&'static str, OrderDir)> {
        vec![("updated_at", OrderDir::Desc)]
    }

    /// Bulk actions exposed above the list table. Each entry adds
    /// a labeled button next to a "Select all" checkbox; the
    /// multi-select checkbox column is auto-mounted when this returns
    /// any entry. Implementations that declare bulk actions MUST
    /// override [`Self::handle_bulk_action`] — the default panics.
    fn bulk_actions(&self) -> Vec<BulkAction> {
        Vec::new()
    }

    /// Per-row action chips overriding the built-in Edit / Delete
    /// pair when set. Each [`BulkAction`] becomes a small inline
    /// button; the `name` is sent as the form action via a fetch
    /// `POST /cms-admin/library/{type}/row-action/{id}?action=<name>`.
    /// Defaults to empty (built-in chips render).
    fn row_actions(&self) -> Vec<BulkAction> {
        Vec::new()
    }

    /// Override the per-row HTML template. Defaults to `None` — the
    /// admin renders rows via the built-in column loop. Setting this
    /// gives full control of the row markup; the template receives
    /// `row` (the snippet + computed cells) and `columns`
    /// (`list_display` metadata) in scope.
    fn list_row_template(&self) -> Option<&'static str> {
        None
    }

    /// Override the edit-form template body. Defaults to `None` —
    /// the admin renders the default linear field stack. Setting
    /// this gives full control of the form layout; the template
    /// runs inside the page-edit `<form>` element so inputs submit
    /// alongside the canonical save.
    fn edit_template(&self) -> Option<&'static str> {
        None
    }

    /// Opt this library type in to versioned snapshots (#123,
    /// Wagtail parity B12). When `true`, every successful save on a
    /// snippet of this type also captures a [`crate::snippet::SnippetRevision`]
    /// row, and the editor surfaces a History card with revert.
    /// Defaults to `false` — fixed-shape snippet types keep the
    /// existing zero-overhead lifecycle.
    fn revisions_enabled(&self) -> bool {
        false
    }

    /// Opt this library type in to preview-on-save (#123, Wagtail
    /// parity B12). When `true`, the editor surfaces an
    /// \"Open preview\" action that renders the unsaved form state.
    /// Defaults to `false`. Hosts that want preview implement their
    /// own preview route + override this.
    fn preview_enabled(&self) -> bool {
        false
    }

    /// Permission hook — return `false` to hide a row from the list
    /// view + reject direct GET access to its edit page. V1 grants
    /// every row (true). Hosts wire role-aware logic by overriding.
    fn can_view(&self, _snippet: &crate::snippet::Snippet) -> bool {
        true
    }

    /// Permission hook — return `false` to disable the Edit chip +
    /// reject POST edits on the row. V1 grants every row (true).
    fn can_edit(&self, _snippet: &crate::snippet::Snippet) -> bool {
        true
    }

    /// Permission hook — return `false` to hide the Delete chip +
    /// reject DELETE on the row. V1 grants every row (true).
    fn can_delete(&self, _snippet: &crate::snippet::Snippet) -> bool {
        true
    }

    /// Cross-field validation hook. Called BEFORE save with the
    /// form payload as a key/value map. Return `Err` with one or
    /// more field-scoped error messages to fail the save and render
    /// them inline. Defaults to "always ok".
    fn validate(
        &self,
        _form: &std::collections::HashMap<String, String>,
    ) -> Result<(), Vec<(String, String)>> {
        Ok(())
    }

    /// Opt-in hook for the WP-style menu builder (#49). Return a
    /// list of [`MenuPickerEntry`] values to surface this library
    /// type's snippets in the **Library** picker tab on the menu
    /// edit screen. Each entry contributes a draggable picker card
    /// that drops into the menu as an external-URL `MenuItem`.
    ///
    /// Default: empty — most library types (authors, callouts,
    /// blocks) don't have a single canonical URL and shouldn't show
    /// up in the menu picker. Override on types whose snippets
    /// represent linkable resources (e.g. external partner sites,
    /// downloadable docs, microsite shortcuts).
    ///
    /// # Errors
    /// Driver errors from the snippet fetch when an implementation
    /// queries the pool. The default returns `Ok(Vec::new())` so
    /// implementors only opt-in when they have real data to surface.
    async fn menu_picker_entries(
        &self,
        _pool: &rustango::sql::Pool,
    ) -> Result<Vec<MenuPickerEntry>, rustango::sql::ExecError> {
        Ok(Vec::new())
    }

    /// Async bulk-action dispatcher. Called when the list view
    /// submits a bulk action. Returns the number of rows touched.
    /// Default panics — handlers that declare [`Self::bulk_actions`]
    /// MUST override this.
    ///
    /// # Errors
    /// Driver errors from the underlying ORM calls.
    async fn handle_bulk_action(
        &self,
        _pool: &rustango::sql::Pool,
        action: &str,
        _ids: &[i64],
    ) -> Result<usize, rustango::sql::ExecError> {
        unimplemented!(
            "library type `{}` declared bulk actions but didn't override handle_bulk_action(); unknown action `{action}`",
            self.type_name(),
        )
    }
}

/// Handler-registration entry submitted via `inventory::submit!`. The
/// `factory` reconstructs the handler at runtime; mirrors
/// `PageTypeHandlerRegistration`.
pub struct LibraryTypeHandlerRegistration {
    pub factory: fn() -> Box<dyn LibraryTypeHandler>,
}

inventory::collect!(LibraryTypeHandlerRegistration);

/// Walk every `register_library_type!`-registered handler. Used by
/// the seeder + admin to enumerate types at runtime.
pub fn registered_handlers() -> impl Iterator<Item = Box<dyn LibraryTypeHandler>> {
    inventory::iter::<LibraryTypeHandlerRegistration>
        .into_iter()
        .map(|r| (r.factory)())
}

/// Locate a handler by `type_name`. Returns `None` for unknown
/// types — useful for the admin's lookup-then-route flow.
#[must_use]
pub fn find_handler(type_name: &str) -> Option<Box<dyn LibraryTypeHandler>> {
    registered_handlers().find(|h| h.type_name() == type_name)
}

/// Seed `cms_library_type` rows for every registered handler across
/// every active org. Wire into the same `Cli::seed` callback the
/// page-type seeder uses.
///
/// # Errors
/// Driver / tenancy errors from any tenant; short-circuits on the
/// first failure (mirrors `ensure_seeded` for `PageType`).
/// Upsert one `cms_library_type` row keyed by `type_name`. Exposed
/// so `crate::seed::ensure_seeded` can share the seeding loop with
/// `register_page_type!`.
pub async fn ensure_library_type_row(
    pool: &rustango::sql::Pool,
    handler: &dyn LibraryTypeHandler,
) -> Result<(), rustango::sql::ExecError> {
    upsert_one(pool, handler).await
}

async fn upsert_one(
    pool: &rustango::sql::Pool,
    handler: &dyn LibraryTypeHandler,
) -> Result<(), rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let mut existing: Vec<LibraryType> = LibraryType::objects()
        .where_(LibraryType::type_name.eq(handler.type_name().to_string()))
        .fetch(pool)
        .await?;
    let extension_table = handler.extension_table().unwrap_or("").to_owned();
    if let Some(mut row) = existing.pop() {
        row.app_label = handler.app_label().to_owned();
        row.verbose_name = handler.verbose_name().to_owned();
        row.extension_table = extension_table;
        row.save_pool(pool).await?;
    } else {
        let mut row = LibraryType {
            id: Auto::Unset,
            app_label: handler.app_label().to_owned(),
            type_name: handler.type_name().to_owned(),
            verbose_name: handler.verbose_name().to_owned(),
            extension_table,
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        row.insert_pool(pool).await?;
    }
    Ok(())
}

/// Register a library type at compile time. Pair with a
/// `#[derive(Default)] impl LibraryTypeHandler` block. Mirrors
/// [`crate::register_page_type!`].
#[macro_export]
macro_rules! register_library_type {
    ($ty:ty) => {
        $crate::inventory::submit! {
            $crate::library::LibraryTypeHandlerRegistration {
                factory: || ::std::boxed::Box::new(<$ty>::default()),
            }
        }
    };
}

#[cfg(test)]
mod menu_picker_entry_tests {
    use super::*;

    #[test]
    fn serializes_with_optional_kind_hint() {
        let e = MenuPickerEntry::new("Partner site", "https://partner.example.com")
            .with_kind_hint("Partner");
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["label"], "Partner site");
        assert_eq!(v["url"], "https://partner.example.com");
        assert_eq!(v["kind_hint"], "Partner");
    }

    #[test]
    fn omits_kind_hint_when_absent() {
        let e = MenuPickerEntry::new("Docs", "https://docs.example.com");
        let v = serde_json::to_value(&e).unwrap();
        // serde defaults to `null` for `Option::None` — the template
        // uses `e.kind_hint | default(value=e.library_type)` so null
        // falls through to the library-type label.
        assert!(v["kind_hint"].is_null());
    }
}
