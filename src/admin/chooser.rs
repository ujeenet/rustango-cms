//! Generic chooser registry — register a chooser for any
//! `#[derive(Model)]` type, generalising the three hand-rolled choosers
//! (page / snippet / document).
//!
//! A host registers with
//! [`register_chooser!`](crate::register_chooser!)`(Ty, "slug", "Label", "title_field", "sub_field")`;
//! the admin then serves `/cms-admin/__chooser/<slug>?q=…` returning the
//! same `{ "items": [ { id, title, sub, … } ] }` shape the bespoke
//! choosers use. The editor's chooser overlay (`cms-ux.js`) resolves an
//! unregistered `data-chooser-kind` to this endpoint automatically, so a
//! registered chooser needs **no** JavaScript change — a widget just
//! sets its slug as the kind.
//!
//! Search runs over the model's `admin(search_fields = …)` (the same
//! metadata the framework admin uses), matched case-insensitively as a
//! substring on each declared field — mirroring the in-memory filter the
//! bespoke choosers already do. A dedicated form `WidgetKind` for model
//! choosers is the natural next step.

use std::future::Future;
use std::pin::Pin;

use rustango::core::ModelSchema;
use rustango::sql::Pool;
use serde_json::Value;

/// Cap on returned rows — matches the bespoke choosers' limit.
pub const CHOOSER_LIMIT: usize = 50;

/// Type-erased "search rows as normalised chooser items". Built by
/// [`register_chooser!`](crate::register_chooser!) from the concrete model's
/// `objects().fetch()` + its schema `search_fields` (the fetch is a
/// derive-generated inherent method, so it can't be reached generically —
/// hence the thunk, same shape as [`crate::admin::model_admin`]).
pub type SearchRowsFn = fn(Pool, String) -> Pin<Box<dyn Future<Output = Vec<Value>> + Send>>;

/// One registered chooser.
pub struct ChooserViewSet {
    /// URL slug under `/cms-admin/__chooser/<slug>` + the chooser kind.
    pub slug: &'static str,
    /// Human-readable singular label (dialog heading).
    pub label: &'static str,
    /// The model's schema — exposes `search_fields` + `display`.
    pub schema: &'static ModelSchema,
    /// Row field shown as the item title.
    pub title_field: &'static str,
    /// Row field shown as the secondary (sub) line.
    pub sub_field: &'static str,
    /// Search the model + normalise rows to `{id, title, sub, …}`.
    pub search_rows: SearchRowsFn,
}

inventory::collect!(ChooserViewSet);

/// Every registered chooser, sorted by slug.
#[must_use]
pub fn registered() -> Vec<&'static ChooserViewSet> {
    let mut v: Vec<_> = inventory::iter::<ChooserViewSet>.into_iter().collect();
    v.sort_by_key(|c| c.slug);
    v
}

/// The chooser registered under `slug`, if any.
#[must_use]
pub fn find(slug: &str) -> Option<&'static ChooserViewSet> {
    inventory::iter::<ChooserViewSet>
        .into_iter()
        .find(|c| c.slug == slug)
}

/// Filter already-serialized `rows` by `query` (case-insensitive
/// substring over `search_fields`), cap at `limit`, and normalise each
/// to carry a `title` + `sub` key (from `title_field` / `sub_field`)
/// alongside its original fields. An empty / blank query returns the
/// first `limit` rows. Pure — the [`register_chooser!`](crate::register_chooser!) thunk and the
/// unit tests both call it.
#[must_use]
pub fn filter_and_normalize(
    rows: Vec<Value>,
    query: &str,
    search_fields: &[&str],
    title_field: &str,
    sub_field: &str,
    limit: usize,
) -> Vec<Value> {
    let needle = query.trim().to_lowercase();
    rows.into_iter()
        .filter(|row| {
            if needle.is_empty() {
                return true;
            }
            search_fields.iter().any(|f| {
                row.get(*f)
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.to_lowercase().contains(&needle))
            })
        })
        .take(limit)
        .map(|mut row| {
            let title = field_string(&row, title_field);
            let sub = field_string(&row, sub_field);
            if let Value::Object(map) = &mut row {
                map.insert("title".to_owned(), Value::String(title));
                map.insert("sub".to_owned(), Value::String(sub));
            }
            row
        })
        .collect()
}

/// A row field as a display string: strings bare, null / missing empty,
/// everything else its compact JSON.
fn field_string(row: &Value, field: &str) -> String {
    match row.get(field) {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Register a generic chooser for a `#[derive(Model)]` type.
///
/// ```ignore
/// rustango_cms::register_chooser!(crate::Author, "author", "Author", "name", "email");
/// // → /cms-admin/__chooser/author?q=… ; a widget with
/// //   data-chooser-kind="author" drives it via the shared overlay.
/// ```
///
/// The model must derive `serde::Serialize` (rows are serialised to JSON
/// for the search filter). Search uses the model's
/// `admin(search_fields = …)`.
#[macro_export]
macro_rules! register_chooser {
    ($ty:ty, $slug:expr, $label:expr, $title_field:expr, $sub_field:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::admin::chooser::ChooserViewSet {
                slug: $slug,
                label: $label,
                schema: <$ty as $crate::__private::Model>::SCHEMA,
                title_field: $title_field,
                sub_field: $sub_field,
                search_rows: |pool: $crate::__private::Pool, query: ::std::string::String| {
                    ::std::boxed::Box::pin(async move {
                        use $crate::__private::FetcherPool as _;
                        use $crate::__private::Model as _;
                        let rows: ::std::vec::Vec<$ty> =
                            <$ty>::objects().fetch(&pool).await.unwrap_or_default();
                        let json_rows: ::std::vec::Vec<$crate::__private::serde_json::Value> = rows
                            .iter()
                            .filter_map(|r| $crate::__private::serde_json::to_value(r).ok())
                            .collect();
                        let search_fields: &[&str] = <$ty>::SCHEMA
                            .admin
                            .map(|a| a.search_fields)
                            .unwrap_or(&[]);
                        $crate::admin::chooser::filter_and_normalize(
                            json_rows,
                            &query,
                            search_fields,
                            $title_field,
                            $sub_field,
                            $crate::admin::chooser::CHOOSER_LIMIT,
                        )
                    })
                },
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Dogfood the macro on a real CMS model to prove it compiles +
    // registers (Locale has `admin(search_fields = …)`).
    crate::register_chooser!(
        crate::locale::Locale,
        "__test_locale",
        "Test Locale",
        "code",
        "label"
    );

    #[test]
    fn macro_registers_and_find_resolves() {
        let cvs = find("__test_locale").expect("registered");
        assert_eq!(cvs.label, "Test Locale");
        assert_eq!(cvs.title_field, "code");
        assert!(registered().iter().any(|c| c.slug == "__test_locale"));
    }

    fn rows() -> Vec<Value> {
        vec![
            json!({"id": 1, "code": "en", "label": "English"}),
            json!({"id": 2, "code": "fr", "label": "French"}),
            json!({"id": 3, "code": "es", "label": "Spanish"}),
        ]
    }

    #[test]
    fn empty_query_returns_all_normalized() {
        let out = filter_and_normalize(rows(), "", &["code", "label"], "code", "label", 50);
        assert_eq!(out.len(), 3);
        // Each item carries id + title (title_field) + sub (sub_field).
        assert_eq!(out[0]["id"], json!(1));
        assert_eq!(out[0]["title"], json!("en"));
        assert_eq!(out[0]["sub"], json!("English"));
    }

    #[test]
    fn query_matches_any_search_field_case_insensitively() {
        // "spa" matches the label "Spanish".
        let out = filter_and_normalize(rows(), "SPA", &["code", "label"], "code", "label", 50);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], json!(3));
    }

    #[test]
    fn query_only_searches_declared_fields() {
        // "english" is in `label`, but if we don't declare label, no match.
        let out = filter_and_normalize(rows(), "english", &["code"], "code", "label", 50);
        assert!(out.is_empty());
    }

    #[test]
    fn limit_is_respected() {
        let out = filter_and_normalize(rows(), "", &["code"], "code", "label", 2);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn missing_sub_field_normalizes_to_empty() {
        let out = filter_and_normalize(rows(), "", &["code"], "code", "nonexistent", 50);
        assert_eq!(out[0]["sub"], json!(""));
    }
}
