//! [`PageType`] — the per-tenant `cms_page_type` row that gives each
//! registered [`PageTypeHandler`](crate::PageTypeHandler) a stable id
//! for `cms_page.page_type_id`. Rows are upserted by
//! [`ensure_seeded`](crate::ensure_seeded).

use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// Per-tenant registry row describing a registered page type.
///
/// Each `PageTypeHandler` (the `Send + Sync` Rust trait) maps to a
/// row in this table. The row is the database identity used by
/// [`Page::page_type_id`](crate::Page) — handlers come and go with
/// process restarts, but pages need a stable FK target.
///
/// Mirrors `rustango::ContentType` in shape: `(app_label,
/// type_name)` is the natural key; `type_name` alone is unique
/// across the process so user code can resolve a handler by name.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_page_type",
    app = "cms",
    display = "verbose_name",
    admin(
        list_display = "type_name, verbose_name, app_label, default_template, is_creatable",
        search_fields = "type_name, verbose_name",
        ordering = "app_label, type_name",
        list_filter = "app_label, is_creatable",
    )
)]
pub struct PageType {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Logical app this type belongs to (e.g. `"cms_articles"`).
    /// Lets the admin group page types and lets handlers in
    /// different crates coexist without collision.
    #[rustango(max_length = 100, index)]
    pub app_label: String,

    /// Stable identifier matching `PageTypeHandler::type_name()`.
    /// Globally unique within the tenant; this is what user code
    /// reads to find the right handler at render time.
    #[rustango(max_length = 100, unique)]
    pub type_name: String,

    /// Human-readable label shown in the admin's "Add page" picker.
    #[rustango(max_length = 200)]
    pub verbose_name: String,

    /// Tera template path the renderer falls back to when the
    /// `Page` row doesn't override it. Resolved against the host
    /// app's Tera instance.
    ///
    /// An **empty string is the "no template" sentinel** — a type that
    /// exists only to answer JSON. See [`crate::page_view`].
    #[rustango(max_length = 255)]
    pub default_template: String,

    /// Which representation this type serves on its public URL:
    /// `"auto"` (default), `"html"`, or `"api"`.
    ///
    /// `auto` derives it from [`Self::default_template`] — blank means
    /// JSON-only — so every pre-existing row keeps rendering exactly as
    /// before. `api` serves JSON at the page's own URL under
    /// `Accept: application/json`; `html` forces the template path.
    /// Parsed leniently by [`crate::page_view::PageViewMode::parse`],
    /// so an unrecognised value read by an older binary during a
    /// rolling deploy behaves as `auto` rather than failing the request.
    #[rustango(max_length = 16, default = "'auto'")]
    pub view_mode: String,

    /// When `false` the type cannot be picked from the "Add page"
    /// menu — used for abstract / programmatically-created types.
    pub is_creatable: bool,

    /// JSON array of `type_name`s allowed as direct parents. Empty
    /// array means no restriction. Stored as JSONB for cheap
    /// updates without a schema migration when the rule shifts.
    pub allowed_parent_types: serde_json::Value,

    /// JSON array of `type_name`s allowed as direct children.
    /// Symmetric with [`Self::allowed_parent_types`] — both lists
    /// are checked at create / move time. Default `'[]'::jsonb`
    /// ("no restriction") so existing rows pre-dating slice 0003
    /// keep working.
    #[rustango(default = "'[]'::jsonb")]
    pub allowed_child_types: serde_json::Value,

    /// Slug of the workflow pages of this type go through, chosen on the
    /// admin's page-type form; empty means publish directly. A code
    /// type's `PageTypeHandler::workflow_slug()` takes precedence.
    #[rustango(max_length = 100, default = "''")]
    pub workflow: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<chrono::DateTime<chrono::Utc>>,
}

impl PageType {
    /// Type names this type may be created under; empty means anywhere.
    ///
    /// Read from the row, which holds the rule for every type: a code type
    /// writes its handler's list at seed, an admin-made type stores what was
    /// entered on its form.
    #[must_use]
    pub fn allowed_parents(&self) -> Vec<String> {
        names(&self.allowed_parent_types)
    }

    /// The workflow pages of this type go through, by name, or `None` to
    /// publish directly. A code type's `workflow_slug()` comes first, then
    /// the choice made on the admin's page-type form.
    #[must_use]
    pub fn workflow_name(&self) -> Option<String> {
        crate::page_type::find_handler(&self.type_name)
            .and_then(|h| h.workflow_slug().map(str::to_owned))
            .or_else(|| Some(self.workflow.trim().to_owned()).filter(|w| !w.is_empty()))
    }

    /// Type names allowed as this type's children; empty means any.
    #[must_use]
    pub fn allowed_children(&self) -> Vec<String> {
        names(&self.allowed_child_types)
    }
}

/// A JSON array of type names; anything else is no names.
fn names(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}
