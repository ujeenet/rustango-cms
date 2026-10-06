//! Generic model-admin (#420) — a `ModelViewSet`-style CRUD admin for
//! arbitrary `#[derive(Model)]` types, rendered inside the CMS admin
//! chrome.
//!
//! The framework's own `ListView`/`ViewSet` can't inject the CMS's
//! async, DB-backed `add_chrome`, so they can't back a chrome'd admin
//! screen. This is a CMS-native path: a host registers a model with
//! [`register_model_admin!`], which captures the model's
//! [`rustango::core::schema::ModelSchema`] + a type-erased "list rows
//! as JSON" thunk, and the admin serves a chrome'd index at
//! `/cms-admin/model/<slug>` driven by the model's `list_display`.
//!
//! PR 1 ships the registry + the read-only index. Create / edit /
//! delete (generic forms from `FieldSchema`) + filter / search / order
//! follow.

use std::future::Future;
use std::pin::Pin;

use rustango::core::ModelSchema;
use rustango::sql::Pool;

/// Type-erased "fetch every row of the model as JSON" — the index
/// data source. Built by [`register_model_admin!`] from the concrete
/// model's `objects().fetch()` (a derive-generated inherent
/// method, so it can't be reached generically — hence the thunk).
pub type ListRowsFn = fn(Pool) -> Pin<Box<dyn Future<Output = Vec<serde_json::Value>> + Send>>;

/// One registered model admin.
pub struct ModelAdmin {
    /// URL slug under `/cms-admin/model/<slug>` (use the table name).
    pub slug: &'static str,
    /// Human-readable plural label for the index heading / nav.
    pub label: &'static str,
    /// The model's schema — drives the index columns (`list_display`).
    pub schema: &'static ModelSchema,
    /// Fetch all rows as JSON for the index.
    pub list_rows: ListRowsFn,
}

inventory::collect!(ModelAdmin);

/// Every registered model admin, sorted by slug.
#[must_use]
pub fn registered() -> Vec<&'static ModelAdmin> {
    let mut v: Vec<_> = inventory::iter::<ModelAdmin>.into_iter().collect();
    v.sort_by_key(|m| m.slug);
    v
}

/// The model admin registered under `slug`, if any.
#[must_use]
pub fn find(slug: &str) -> Option<&'static ModelAdmin> {
    inventory::iter::<ModelAdmin>
        .into_iter()
        .find(|m| m.slug == slug)
}

/// The index columns as `(field_name, header_label)` pairs. Uses the
/// model's `admin(list_display = …)` when set; otherwise falls back to
/// the `display` field, else the first few scalar fields — so a model
/// with no admin hints still renders something useful.
#[must_use]
pub fn list_columns(schema: &ModelSchema) -> Vec<(String, String)> {
    let names: Vec<&'static str> = schema
        .admin
        .map(|a| a.list_display)
        .filter(|d| !d.is_empty())
        .map(<[&'static str]>::to_vec)
        .unwrap_or_else(|| {
            schema
                .display
                .map(|d| vec![d])
                .unwrap_or_else(|| schema.fields.iter().take(4).map(|f| f.name).collect())
        });
    names
        .iter()
        .map(|n| {
            let label = schema
                .field(n)
                .map_or_else(|| (*n).to_owned(), |f| f.display_label().to_owned());
            ((*n).to_owned(), label)
        })
        .collect()
}

/// Render one row's value for column `col` as a display string. Strings
/// pass through; null / missing → empty; everything else → its compact
/// JSON (numbers, bools, nested objects).
#[must_use]
pub fn row_cell(row: &serde_json::Value, col: &str) -> String {
    match row.get(col) {
        Some(serde_json::Value::String(s)) => s.clone(),
        None | Some(serde_json::Value::Null) => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Register a generic admin for a `#[derive(Model)]` type (#420).
///
/// ```ignore
/// rustango_cms::register_model_admin!(crate::Author, "author", "Authors");
/// // → a chrome'd index at /cms-admin/model/author, columns from the
/// //   model's admin(list_display = …).
/// ```
///
/// The model must derive `serde::Serialize` (rows are serialized to
/// JSON for the generic index).
#[macro_export]
macro_rules! register_model_admin {
    ($ty:ty, $slug:expr, $label:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::admin::model_admin::ModelAdmin {
                slug: $slug,
                label: $label,
                schema: <$ty as $crate::__private::Model>::SCHEMA,
                list_rows: |pool: $crate::__private::Pool| {
                    ::std::boxed::Box::pin(async move {
                        use $crate::__private::FetcherPool as _;
                        let rows: ::std::vec::Vec<$ty> =
                            <$ty>::objects().fetch(&pool).await.unwrap_or_default();
                        rows.iter()
                            .filter_map(|r| $crate::__private::serde_json::to_value(r).ok())
                            .collect::<::std::vec::Vec<$crate::__private::serde_json::Value>>()
                    })
                },
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    // Dogfood the macro on a real CMS model to prove it compiles +
    // registers (Locale has an `admin(list_display = …)`).
    crate::register_model_admin!(crate::locale::Locale, "__test_locale", "Test Locales");

    #[test]
    fn macro_registers_and_find_resolves() {
        let ma = find("__test_locale").expect("registered");
        assert_eq!(ma.label, "Test Locales");
        assert_eq!(ma.schema.table, "cms_locale");
        assert!(registered().iter().any(|m| m.slug == "__test_locale"));
    }

    #[test]
    fn columns_come_from_list_display_with_labels() {
        use rustango::core::Model as _;
        let cols = list_columns(crate::locale::Locale::SCHEMA);
        assert!(!cols.is_empty(), "a model with admin hints yields columns");
        // Each column is (field_name, label); labels are non-empty.
        assert!(cols.iter().all(|(n, l)| !n.is_empty() && !l.is_empty()));
    }

    #[test]
    fn row_cell_stringifies_by_json_kind() {
        let row = serde_json::json!({"code": "en", "sort_order": 3, "active": true, "x": null});
        assert_eq!(row_cell(&row, "code"), "en"); // string: bare
        assert_eq!(row_cell(&row, "sort_order"), "3"); // number
        assert_eq!(row_cell(&row, "active"), "true"); // bool
        assert_eq!(row_cell(&row, "x"), ""); // null → empty
        assert_eq!(row_cell(&row, "missing"), ""); // absent → empty
    }
}
