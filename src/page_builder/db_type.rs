//! UI-created page-type fallback handler (#559/#566).
//!
//! When a `cms_page_type` row has no code-registered handler (Phase-2,
//! Strapi-style types authored entirely in the UI), this synthetic
//! handler lets the render + editor pipelines treat it like any other
//! page type. It supplies the trait's required `&'static str`s (interned
//! from the row — leak-once + bounded, same pool as [`super::dyn_block`])
//! and defaults everything else: extension data is empty (the body lives
//! in `cms_page_builder_data`, rendered by the page-builder context), and
//! the template is the row's `default_template` — a generic
//! [`DEFAULT_SCHEMA_TEMPLATE`] unless the host overrode it per row.

use crate::page_type::PageTypeHandler;
use crate::page_type_model::PageType;

/// Generic public template used by UI-created types unless the row's
/// `default_template` names a host-provided override.
pub const DEFAULT_SCHEMA_TEMPLATE: &str = "rcms_admin/schema_page.html";

/// A [`PageTypeHandler`] synthesized from a `cms_page_type` row that has
/// no code handler. Substituted at the `find_handler` miss points.
pub struct DbSchemaPageType {
    app_label: &'static str,
    type_name: &'static str,
    verbose_name: &'static str,
    default_template: &'static str,
    view_mode: crate::page_view::PageViewMode,
    is_creatable: bool,
}

impl DbSchemaPageType {
    /// Build a fallback handler from a registry row.
    #[must_use]
    pub fn from_row(row: &PageType) -> Self {
        let view_mode = crate::page_view::PageViewMode::parse(&row.view_mode);
        let tpl = if row.default_template.trim().is_empty() {
            // An API-mode UI type genuinely has no HTML representation.
            // Substituting the generic schema template here would hand
            // `page_view::resolve_kind` a template and erase the very
            // signal that makes it JSON-only.
            if view_mode == crate::page_view::PageViewMode::Api {
                ""
            } else {
                DEFAULT_SCHEMA_TEMPLATE
            }
        } else {
            super::dyn_block::intern(&row.default_template)
        };
        Self {
            app_label: super::dyn_block::intern(&row.app_label),
            type_name: super::dyn_block::intern(&row.type_name),
            verbose_name: super::dyn_block::intern(&row.verbose_name),
            default_template: tpl,
            view_mode,
            is_creatable: row.is_creatable,
        }
    }
}

impl PageTypeHandler for DbSchemaPageType {
    fn app_label(&self) -> &'static str {
        self.app_label
    }
    fn type_name(&self) -> &'static str {
        self.type_name
    }
    fn verbose_name(&self) -> &'static str {
        self.verbose_name
    }
    fn default_template(&self) -> &'static str {
        self.default_template
    }
    fn view_mode(&self) -> crate::page_view::PageViewMode {
        self.view_mode
    }
    fn is_creatable(&self) -> bool {
        self.is_creatable
    }
    fn icon(&self) -> Option<&'static str> {
        Some("dashboard_customize")
    }
    // Everything else (load_extension → Null, widgets → [], public_context
    // / route_context / inline panels → defaults) is inherited: the page's
    // authored body comes from the page-builder value store, not a typed
    // extension table.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_type::PageTypeHandler;
    use rustango::sql::Auto;

    fn row(default_template: &str) -> PageType {
        row_with_mode(default_template, "auto")
    }

    fn row_with_mode(default_template: &str, view_mode: &str) -> PageType {
        PageType {
            id: Auto::Unset,
            app_label: "cms_ui".to_owned(),
            type_name: "landing_page".to_owned(),
            verbose_name: "Landing Page".to_owned(),
            default_template: default_template.to_owned(),
            view_mode: view_mode.to_owned(),
            is_creatable: true,
            allowed_parent_types: serde_json::json!([]),
            allowed_child_types: serde_json::json!([]),
            workflow: String::new(),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        }
    }

    #[test]
    fn from_row_exposes_interned_metadata() {
        let h = DbSchemaPageType::from_row(&row("host/landing.html"));
        assert_eq!(h.type_name(), "landing_page");
        assert_eq!(h.verbose_name(), "Landing Page");
        assert_eq!(h.app_label(), "cms_ui");
        assert_eq!(h.default_template(), "host/landing.html");
        assert!(h.is_creatable());
    }

    #[test]
    fn blank_template_falls_back_to_generic() {
        let h = DbSchemaPageType::from_row(&row(""));
        assert_eq!(h.default_template(), DEFAULT_SCHEMA_TEMPLATE);
        assert_eq!(h.view_mode(), crate::page_view::PageViewMode::Auto);
    }

    #[test]
    fn an_api_type_keeps_its_blank_template() {
        // The generic schema template must NOT be substituted here: it
        // would hand `resolve_kind` a template and turn a JSON-only type
        // back into an HTML one.
        let h = DbSchemaPageType::from_row(&row_with_mode("", "api"));
        assert_eq!(h.default_template(), "");
        assert_eq!(h.view_mode(), crate::page_view::PageViewMode::Api);
        assert_eq!(
            crate::page_view::resolve_kind(h.view_mode().as_str(), h.default_template()),
            crate::page_view::PageViewKind::ApiOnly
        );
    }
}
