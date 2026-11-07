//! Generic model-admin — a `ModelViewSet`-style CRUD admin for
//! arbitrary `#[derive(Model)]` types, rendered inside the CMS admin
//! chrome.
//!
//! The framework's own `ListView`/`ViewSet` can't inject the CMS's
//! async, DB-backed `add_chrome`, so they can't back a chrome'd admin
//! screen. This is a CMS-native path: a host registers a model with
//! [`register_model_admin!`](crate::register_model_admin!), which captures the model's
//! [`rustango::core::ModelSchema`] + a type-erased "list rows
//! as JSON" thunk, and the admin serves a chrome'd index at
//! `/cms-admin/model/<slug>` driven by the model's `list_display`.
//!
//! It ships the registry + the read-only, paged index. Create / edit /
//! delete (generic forms from `FieldSchema`) + filter / search / order
//! are not implemented yet.

use std::future::Future;
use std::pin::Pin;

use rustango::core::ModelSchema;
use rustango::sql::Pool;

/// Type-erased "fetch one page of the model's rows as JSON" — the index
/// data source. Called with `(pool, limit, offset)`; returns that window
/// in primary-key order plus the total row count. Built by
/// [`register_model_admin!`](crate::register_model_admin!) from the
/// concrete model's `objects()` (a derive-generated inherent method, so
/// it can't be reached generically — hence the thunk).
pub type ListRowsFn =
    fn(Pool, i64, i64) -> Pin<Box<dyn Future<Output = (Vec<serde_json::Value>, i64)> + Send>>;

/// One registered model admin.
pub struct ModelAdmin {
    /// URL slug under `/cms-admin/model/<slug>` (use the table name).
    pub slug: &'static str,
    /// Human-readable plural label for the index heading / nav.
    pub label: &'static str,
    /// The model's schema — drives the index columns (`list_display`).
    pub schema: &'static ModelSchema,
    /// Fetch one page of rows as JSON for the index, with the total.
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

/// Register a generic admin for a `#[derive(Model)]` type.
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
                list_rows: |pool: $crate::__private::Pool, limit: i64, offset: i64| {
                    ::std::boxed::Box::pin(async move {
                        use $crate::__private::{CounterPool as _, FetcherPool as _};
                        let total = <$ty>::objects().count(&pool).await.unwrap_or_default();
                        let mut qs = <$ty>::objects();
                        // A stable order, so paging neither repeats nor skips rows.
                        if let ::std::option::Option::Some(pk) =
                            <$ty as $crate::__private::Model>::SCHEMA.primary_key()
                        {
                            qs = qs.order_by(&[(pk.name, false)]);
                        }
                        let rows: ::std::vec::Vec<$ty> = qs
                            .limit(limit)
                            .offset(offset)
                            .fetch(&pool)
                            .await
                            .unwrap_or_default();
                        let rows = rows
                            .iter()
                            .filter_map(|r| $crate::__private::serde_json::to_value(r).ok())
                            .collect::<::std::vec::Vec<$crate::__private::serde_json::Value>>();
                        (rows, total)
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

#[cfg(all(test, feature = "sqlite"))]
mod paging_tests {
    use super::find;
    use rustango::core::Model as _;
    use rustango::sql::{Auto, Pool};

    // Registered in `tests` above.
    const SLUG: &str = "__test_locale";

    #[tokio::test]
    async fn list_rows_returns_one_window_in_key_order_and_the_total() {
        let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
        let ddl = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
            pool.dialect(),
            crate::locale::Locale::SCHEMA,
        );
        for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            rustango::sql::raw_execute_pool(&pool, stmt, Vec::new())
                .await
                .expect("ddl");
        }
        for (i, code) in ["en", "fr", "de", "uk", "pl"].into_iter().enumerate() {
            let mut l = crate::locale::Locale {
                id: Auto::Unset,
                code: code.to_owned(),
                name: code.to_owned(),
                is_default: i == 0,
                active: true,
                // Reverse of insertion, so key order is not sort order.
                sort_order: 10 - i as i32,
                created_at: Auto::Unset,
            };
            l.save(&pool).await.expect("locale");
        }
        let ma = find(SLUG).expect("registered");
        let codes = |rows: &[serde_json::Value]| -> Vec<String> {
            rows.iter().map(|r| r["code"].as_str().unwrap_or_default().to_owned()).collect()
        };
        let (first, total) = (ma.list_rows)(pool.clone(), 2, 0).await;
        assert_eq!(total, 5);
        assert_eq!(codes(&first), ["en", "fr"]);
        let (last, _) = (ma.list_rows)(pool.clone(), 2, 4).await;
        assert_eq!(codes(&last), ["pl"]);
        let (past, total) = (ma.list_rows)(pool, 2, 10).await;
        assert!(past.is_empty());
        assert_eq!(total, 5);
    }
}
