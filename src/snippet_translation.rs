//! Per-field, per-locale translation overrides for **snippets**.
//!
//! Mirrors [`crate::translation`] (which localizes pages) for the
//! `cms_snippet` library. The canonical `cms_snippet` row holds the
//! default-locale content; `cms_snippet_translation` overrides
//! individual fields (`title`, `body_markdown`, or dotted `data.*`
//! paths) per locale. A narrow tall table — same rationale as the page
//! table: column-friendly in PG/MySQL, easy to migrate, keeps the
//! canonical row free of locale concerns.
//!
//! pt 1 (this module + the admin translate editor) lets editors author
//! snippet translations. Wiring the public render to substitute them
//! for the active locale is pt 2 — the `fetch_for*` helpers here are
//! the seam it will use.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// One row per `(snippet_id, locale_id, field_path)`. An empty `value`
/// is treated as "no override" and the canonical content shows through.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_snippet_translation",
    app = "cms",
    admin(
        list_display = "snippet_id, locale_id, field_path, value, updated_at",
        ordering = "snippet_id",
        list_filter = "locale_id, field_path",
    )
)]
pub struct SnippetTranslation {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_snippet", on = "id", index)]
    pub snippet_id: i64,

    #[rustango(fk = "cms_locale", on = "id", index)]
    pub locale_id: i64,

    /// Field on `cms_snippet`: `title`, `body_markdown`, or a dotted
    /// `data.<key>` path into the JSON bag.
    #[rustango(max_length = 64, index)]
    pub field_path: String,

    /// Localized value. Empty string = "no override".
    pub value: String,

    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

/// Fetch every translation for `(snippet_id, locale_id)` as a map keyed
/// on `field_path`. Empty values are dropped (canonical shows through).
///
/// # Errors
/// Driver / query failures.
pub async fn fetch_for(
    pool: &rustango::sql::Pool,
    snippet_id: i64,
    locale_id: i64,
) -> Result<std::collections::HashMap<String, String>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let rows: Vec<SnippetTranslation> = SnippetTranslation::objects()
        .where_(SnippetTranslation::snippet_id.eq(snippet_id))
        .where_(SnippetTranslation::locale_id.eq(locale_id))
        .fetch(pool)
        .await?;
    Ok(rows
        .into_iter()
        .filter(|t| !t.value.is_empty())
        .map(|t| (t.field_path, t.value))
        .collect())
}

/// Fetch translations for every snippet in `snippet_ids` under one
/// `locale_id`, as `{ snippet_id -> { field_path -> value } }`. Single
/// `WHERE locale_id = $1 AND snippet_id IN (...)` SELECT — the seam the
/// public render (pt 2) will use to localize a page's snippets without
/// N round-trips. Empty `snippet_ids` → empty map; empty values dropped.
///
/// # Errors
/// Driver / query failures.
pub async fn fetch_for_snippets(
    pool: &rustango::sql::Pool,
    snippet_ids: &[i64],
    locale_id: i64,
) -> Result<
    std::collections::HashMap<i64, std::collections::HashMap<String, String>>,
    rustango::sql::ExecError,
> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    if snippet_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows: Vec<SnippetTranslation> = SnippetTranslation::objects()
        .where_(SnippetTranslation::locale_id.eq(locale_id))
        .where_(SnippetTranslation::snippet_id.is_in(snippet_ids.iter().copied()))
        .fetch(pool)
        .await?;
    let mut out: std::collections::HashMap<i64, std::collections::HashMap<String, String>> =
        std::collections::HashMap::new();
    for row in rows {
        if row.value.is_empty() {
            continue;
        }
        out.entry(row.snippet_id)
            .or_default()
            .insert(row.field_path, row.value);
    }
    Ok(out)
}

/// Validate a translation `field_path`: ≤64 chars, ASCII alphanumeric
/// plus `_` and `.` (the dotted `data.*` form). Mirrors the page
/// translate handler's new-field guard. Returns the trimmed path on
/// success. Pure — unit-tested + reused by the submit handler.
///
/// # Errors
/// A human-readable message when the path is empty, too long, or has
/// disallowed characters.
pub fn validate_field_path(raw: &str) -> Result<String, String> {
    let p = raw.trim();
    if p.is_empty() {
        return Err("Field name is required.".to_owned());
    }
    if p.chars().count() > 64 {
        return Err("Field name is too long (max 64 characters).".to_owned());
    }
    if !p
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        return Err("Field name may only contain letters, digits, '_' and '.'.".to_owned());
    }
    Ok(p.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_field_path_accepts_scalar_and_dotted() {
        assert_eq!(validate_field_path("title").unwrap(), "title");
        assert_eq!(
            validate_field_path("  body_markdown ").unwrap(),
            "body_markdown"
        );
        assert_eq!(
            validate_field_path("data.cta_label").unwrap(),
            "data.cta_label"
        );
    }

    #[test]
    fn validate_field_path_rejects_bad_input() {
        assert!(validate_field_path("").is_err());
        assert!(validate_field_path("   ").is_err());
        assert!(validate_field_path("has space").is_err());
        assert!(validate_field_path("inject;drop").is_err());
        assert!(validate_field_path(&"x".repeat(65)).is_err());
    }
}
