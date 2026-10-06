//! Per-field, per-locale translation overrides.
//!
//! Design rationale lives in `docs/i18n-decision.md`. The
//! TL;DR: the canonical `cms_page` row holds the default-locale
//! content; `cms_translation` overrides individual fields per locale.
//! This table is the sole supported localization path — the
//! `cms_page.locale_variant_of` escape hatch was retired in #275.
//!
//! Storage rationale: a wide JSON blob per page works for
//! document-shaped DBs (Sanity) but is awkward in PG/MySQL where
//! ordinary SELECT + indexes want columns. The narrow tall table is
//! easier to query, easier to migrate, and keeps the canonical
//! `cms_page` row free of locale concerns.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// One row per `(page_id, locale_id, field_path)`. An empty
/// `value` is treated as "no override" and the canonical content
/// shows through.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_translation",
    app = "cms",
    admin(
        list_display = "page_id, locale_id, field_path, value, updated_at",
        ordering = "page_id",
        list_filter = "locale_id, field_path",
    )
)]
pub struct Translation {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    #[rustango(fk = "cms_locale", on = "id", index)]
    pub locale_id: i64,

    /// Field name on `cms_page`. Scalar columns (`title`, `seo_title`,
    /// `seo_description`); a whole-body StreamField blob (the bare stream
    /// field name, e.g. `body`); or a dotted **block-UUID path** into a
    /// StreamField for per-leaf translation, incl. nested blocks —
    /// `<field>.<uuid>.<name>` / `<field>.<uuid>.<sub>.<uuid>.<name>`
    /// (see [`crate::block::translate`]). 255 fits ~2 levels of UUIDs.
    #[rustango(max_length = 255, index)]
    pub field_path: String,

    /// Localized value. Empty string = "no override".
    pub value: String,

    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

/// Fetch every translation for `(page_id, locale_id)` as a map
/// keyed on `field_path`. Single SELECT, materialized once per
/// rendered request.
///
/// # Errors
/// Driver / query failures.
pub async fn fetch_for(
    pool: &rustango::sql::Pool,
    page_id: i64,
    locale_id: i64,
) -> Result<std::collections::HashMap<String, String>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let rows: Vec<Translation> = Translation::objects()
        .where_(Translation::page_id.eq(page_id))
        .where_(Translation::locale_id.eq(locale_id))
        .fetch(pool)
        .await?;
    Ok(rows
        .into_iter()
        .filter(|t| !t.value.is_empty())
        .map(|t| (t.field_path, t.value))
        .collect())
}

/// Fetch translations for every page in `page_ids` under one
/// `locale_id`, materialized as `{ page_id -> { field_path -> value } }`.
///
/// Single `WHERE locale_id = $1 AND page_id IN (...)` SELECT — used by
/// the renderer to populate `translations_by_page` so child / ancestor
/// templates can translate the fields they display (titles in card
/// grids, breadcrumbs, etc.) without N round-trips.
///
/// Returns an empty map when `page_ids` is empty. Empty `value`s are
/// dropped — the canonical content shows through.
///
/// # Errors
/// Driver / query failures.
pub async fn fetch_for_pages(
    pool: &rustango::sql::Pool,
    page_ids: &[i64],
    locale_id: i64,
) -> Result<
    std::collections::HashMap<i64, std::collections::HashMap<String, String>>,
    rustango::sql::ExecError,
> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    if page_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows: Vec<Translation> = Translation::objects()
        .where_(Translation::locale_id.eq(locale_id))
        .where_(Translation::page_id.is_in(page_ids.iter().copied()))
        .fetch(pool)
        .await?;
    let mut out: std::collections::HashMap<i64, std::collections::HashMap<String, String>> =
        std::collections::HashMap::new();
    for row in rows {
        if row.value.is_empty() {
            continue;
        }
        out.entry(row.page_id)
            .or_default()
            .insert(row.field_path, row.value);
    }
    Ok(out)
}

/// Overlay seeded per-page scalar translations onto a JSON context value
/// so templates render localized content with **no** `t`-filter calls —
/// the engine localizes `page`, `extension`, `children`, and any
/// handler-supplied card/list objects uniformly (the same "it just
/// works" contract StreamField bodies already get via the stream
/// prerender).
///
/// Recurses through objects/arrays. For any object that identifies a
/// page (a `page_id` or `id` field whose value is a key in `by_page`),
/// each **scalar** field translation (`field_path` has no `.`, so
/// StreamField body leaves — already localized upstream — are skipped)
/// **replaces** the object's existing same-named field. Fields the
/// object does not already expose are never injected, so an unrelated
/// object that merely shares an id value is left untouched.
///
/// No-op when `by_page` is empty (the default locale), so canonical
/// content shows through unchanged.
pub fn overlay_translations(
    value: &mut serde_json::Value,
    by_page: &std::collections::HashMap<i64, std::collections::HashMap<String, String>>,
) {
    match value {
        serde_json::Value::Object(map) => {
            let pid = map
                .get("page_id")
                .or_else(|| map.get("id"))
                .and_then(serde_json::Value::as_i64);
            if let Some(tr) = pid.and_then(|p| by_page.get(&p)) {
                for (field_path, translated) in tr {
                    if !field_path.contains('.')
                        && !translated.is_empty()
                        && map.contains_key(field_path)
                    {
                        map.insert(
                            field_path.clone(),
                            serde_json::Value::String(translated.clone()),
                        );
                    }
                }
            }
            for v in map.values_mut() {
                overlay_translations(v, by_page);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                overlay_translations(v, by_page);
            }
        }
        _ => {}
    }
}

/// Validate a translation `field_path`: ≤255 chars, ASCII alphanumeric
/// plus `_`, `.`, and `-`. The `.` allows dotted block-UUID paths into
/// StreamFields (`<field>.<uuid>.<name>`); the `-` is required because
/// block UUIDs are lowercase hex with hyphens. Returns the trimmed path
/// on success. Pure — unit-tested + reused by the page translate
/// handler's "add new key" guard.
///
/// # Errors
/// A human-readable message when the path is empty, too long, or has
/// disallowed characters.
pub fn validate_field_path(raw: &str) -> Result<String, String> {
    let p = raw.trim();
    if p.is_empty() {
        return Err("Field name is required.".to_owned());
    }
    if p.chars().count() > 255 {
        return Err("Field name is too long (max 255 characters).".to_owned());
    }
    if !p
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
    {
        return Err("Field name may only contain letters, digits, '_', '.' and '-'.".to_owned());
    }
    Ok(p.to_owned())
}

/// Resolve a locale, taking precedence:
///
/// 1. `explicit_code` (an active `cms_locale` code).
/// 2. The tenant's default locale (`cms_locale.is_default = true`).
///
/// `Accept-Language` negotiation happens upstream, in
/// `locale_mode::resolve_request_locale`.
///
/// Returns the matched `cms_locale.id`, or `None` if no locale was
/// requested AND no default exists (the canonical row stays as-is).
pub async fn resolve_locale(
    pool: &rustango::sql::Pool,
    explicit_code: Option<&str>,
) -> Result<Option<crate::locale::Locale>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    if let Some(code) = explicit_code {
        let mut hits: Vec<crate::locale::Locale> = crate::locale::Locale::objects()
            .where_(crate::locale::Locale::code.eq(code.to_owned()))
            .where_(crate::locale::Locale::active.eq(true))
            .fetch(pool)
            .await?;
        if let Some(l) = hits.pop() {
            return Ok(Some(l));
        }
        // Unknown code → silently fall through to default. The
        // alternative (404) would surprise users following a stale
        // link.
    }
    let mut defaults: Vec<crate::locale::Locale> = crate::locale::Locale::objects()
        .where_(crate::locale::Locale::is_default.eq(true))
        .fetch(pool)
        .await?;
    Ok(defaults.pop())
}

// ---------- Tera filter ----------

/// Tera filter `t(field=..., translations=...)` — substitute the
/// localized value for `field` from the supplied translations map,
/// falling back to the original input on miss / empty.
///
/// Usage:
/// ```tera
/// <h1>{{ page.title | t(field="title", translations=translations) }}</h1>
/// ```
pub fn register_tera_filter(tera: &mut tera::Tera) {
    tera.register_filter("t", t_filter);
}

fn t_filter(
    value: &tera::Value,
    args: &std::collections::HashMap<String, tera::Value>,
) -> tera::Result<tera::Value> {
    let field = args
        .get("field")
        .and_then(tera::Value::as_str)
        .ok_or_else(|| tera::Error::msg("t filter: `field` (string) is required"))?;

    // Primary form: `translations` is a flat field→value map for the
    // current page. Used everywhere the rendered page itself owns the
    // text (titles, body, lead, etc.).
    if let Some(map) = args.get("translations").and_then(tera::Value::as_object) {
        if let Some(v) = map.get(field).and_then(tera::Value::as_str) {
            if !v.is_empty() {
                return Ok(tera::Value::String(v.to_owned()));
            }
        }
    }

    // Loop form: inside `{% for c in children %}` etc., templates pass
    // `by_page=translations_by_page, page_id=c.id` so each iteration
    // translates against the child's own row.
    if let (Some(by_page), Some(page_id)) = (args.get("by_page"), args.get("page_id")) {
        let key = match page_id {
            tera::Value::Number(n) => n.to_string(),
            tera::Value::String(s) => s.clone(),
            _ => String::new(),
        };
        if let Some(per_page) = by_page.as_object().and_then(|m| m.get(&key)) {
            if let Some(v) = per_page
                .as_object()
                .and_then(|m| m.get(field))
                .and_then(tera::Value::as_str)
            {
                if !v.is_empty() {
                    return Ok(tera::Value::String(v.to_owned()));
                }
            }
        }
    }

    Ok(value.clone())
}

#[cfg(test)]
mod tests {
    use super::validate_field_path;

    #[test]
    fn validate_field_path_accepts_scalar_dotted_and_block_paths() {
        assert_eq!(validate_field_path("title").unwrap(), "title");
        assert_eq!(validate_field_path("  body ").unwrap(), "body");
        // dotted block-UUID path into a StreamField (incl. nested)
        let p = "body.550e8400-e29b-41d4-a716-446655440000.title";
        assert_eq!(validate_field_path(p).unwrap(), p);
        let nested = "body.550e8400-e29b-41d4-a716-446655440000.params.\
                      11111111-2222-3333-4444-555555555555.description";
        assert_eq!(validate_field_path(nested).unwrap(), nested);
    }

    #[test]
    fn validate_field_path_rejects_garbage_and_overlong() {
        assert!(validate_field_path("").is_err());
        assert!(validate_field_path("   ").is_err());
        assert!(validate_field_path("has space").is_err());
        assert!(validate_field_path("semi;colon").is_err());
        assert!(validate_field_path(&"x".repeat(256)).is_err());
        assert!(validate_field_path(&"a".repeat(255)).is_ok());
    }
}
