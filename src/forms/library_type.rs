//! Forms as a snippet-backed library type (#536 / FB-03).
//!
//! Registering [`FormLibraryType`] makes every `Snippet` of `type_name`
//! `"form"` a managed library row: the existing Library admin gives us
//! list / create / edit / delete, folders, search, and revisions for
//! free. The form's structure ([`super::schema::Form`]) lives in the
//! snippet's `data` JSON — so there is **no extension table**.
//!
//! The dedicated visual builder replaces the default edit form in FB-04
//! (via `edit_template` / a dedicated build route); here we only wire the
//! type so forms can be created and listed.

use super::schema;
use crate::library::{LibraryTypeHandler, ListColumn};
use crate::snippet::Snippet;

/// Library handler for user-built forms.
#[derive(Default)]
pub struct FormLibraryType;

impl LibraryTypeHandler for FormLibraryType {
    fn app_label(&self) -> &'static str {
        "forms"
    }

    fn type_name(&self) -> &'static str {
        "form"
    }

    fn verbose_name(&self) -> &'static str {
        "Forms"
    }

    /// Schema is stored inline in `cms_snippet.data` — no typed table.
    fn extension_table(&self) -> Option<&'static str> {
        None
    }

    /// Title + structure counts + last-updated. The submission count is
    /// surfaced here in FB-12 (it needs a DB query, which `render_cell`
    /// can't do — it only sees the row).
    fn list_display(&self) -> Vec<ListColumn> {
        vec![
            ListColumn::Field {
                column: "title",
                label: "Title",
            },
            ListColumn::Computed {
                name: "pages",
                label: "Pages",
            },
            ListColumn::Computed {
                name: "fields",
                label: "Fields",
            },
            ListColumn::Field {
                column: "updated_at",
                label: "Updated",
            },
        ]
    }

    fn search_fields(&self) -> Vec<&'static str> {
        vec!["title", "slug"]
    }

    /// Versioned forms: every save snapshots a revision (FB-15 builds the
    /// draft/publish flow on top of this).
    fn revisions_enabled(&self) -> bool {
        true
    }

    /// Counts derived from the stored schema. Integers need no escaping.
    fn render_cell(&self, name: &str, snippet: &Snippet) -> String {
        let form = schema::parse(&snippet.data).unwrap_or_default();
        match name {
            "pages" => form.page_count().to_string(),
            "fields" => form.field_count().to_string(),
            _ => String::new(),
        }
    }
}

crate::register_library_type!(FormLibraryType);
