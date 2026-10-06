//! Per-field, per-locale translation overrides for **menu items**.
//!
//! The third member of the family, after [`crate::translation`] (pages)
//! and [`crate::snippet_translation`] (snippets), and deliberately the
//! same tall `(row_id, locale_id, field_path, value)` shape so the
//! admin editor, the fetch helpers and the overlay semantics all
//! transfer unchanged.
//!
//! Menus needed it for two separate reasons:
//!
//! * An **authored** label (`cms_menu_item.label`) had nowhere to store
//!   a translation at all, so a headless client asking for `?locale=fr`
//!   got English chrome around French content.
//! * An **empty** label falls back to the target page's title, and that
//!   fallback was untranslated too — even though the page's own title
//!   translation was already sitting in `cms_translation`. That half is
//!   fixed by the resolver reading page translations, not by this table.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// One row per `(item_id, locale_id, field_path)`. An empty `value` is
/// treated as "no override" and the canonical content shows through, so
/// a partially-translated menu degrades per item rather than blanking.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_menu_item_translation",
    app = "cms",
    admin(
        list_display = "item_id, locale_id, field_path, value, updated_at",
        ordering = "item_id",
        list_filter = "locale_id, field_path",
    )
)]
pub struct MenuItemTranslation {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_menu_item", on = "id", index)]
    pub item_id: i64,

    #[rustango(fk = "cms_locale", on = "id", index)]
    pub locale_id: i64,

    /// Field on `cms_menu_item`. `label` in practice; `external_url` is
    /// accepted too, since a per-locale outbound link (a country site,
    /// a translated PDF) is a real editorial need.
    #[rustango(max_length = 64, index)]
    pub field_path: String,

    /// Localized value. Empty string = "no override".
    pub value: String,

    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

/// Field paths an editor may translate on a menu item.
///
/// Closed on purpose: unlike a snippet's open `data.*` bag, a menu item
/// has a fixed set of columns, and the submit handler validates against
/// this list rather than letting a typo create a row that nothing reads.
pub const TRANSLATABLE_FIELDS: [&str; 2] = ["label", "external_url"];

/// Fetch translations for every item in `item_ids` under one locale, as
/// `{ item_id -> { field_path -> value } }`.
///
/// One `WHERE locale_id = ? AND item_id IN (…)` SELECT for a whole menu.
/// This is the batching seam: resolving a menu must not cost a query per
/// item, which is the N+1 that `resolve_all_menus` already suffers from.
/// Empty `item_ids` → empty map, without touching the database.
///
/// # Errors
/// Driver / query failures.
pub async fn fetch_for_items(
    pool: &rustango::sql::Pool,
    item_ids: &[i64],
    locale_id: i64,
) -> Result<
    std::collections::HashMap<i64, std::collections::HashMap<String, String>>,
    rustango::sql::ExecError,
> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    if item_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows: Vec<MenuItemTranslation> = MenuItemTranslation::objects()
        .where_(MenuItemTranslation::locale_id.eq(locale_id))
        .where_(MenuItemTranslation::item_id.is_in(item_ids.iter().copied()))
        .fetch(pool)
        .await?;
    let mut out: std::collections::HashMap<i64, std::collections::HashMap<String, String>> =
        std::collections::HashMap::new();
    for row in rows {
        if row.value.is_empty() {
            continue;
        }
        out.entry(row.item_id)
            .or_default()
            .insert(row.field_path, row.value);
    }
    Ok(out)
}

/// Validate a translation `field_path` against [`TRANSLATABLE_FIELDS`].
/// Pure, so the submit handler and the tests share one definition of
/// what is editable.
///
/// # Errors
/// A human-readable message naming the accepted fields.
pub fn validate_field_path(raw: &str) -> Result<String, String> {
    let p = raw.trim();
    if TRANSLATABLE_FIELDS.contains(&p) {
        return Ok(p.to_owned());
    }
    Err(format!(
        "Unknown menu-item field '{p}'. Translatable fields: {}.",
        TRANSLATABLE_FIELDS.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_field_path_accepts_the_translatable_columns() {
        assert_eq!(validate_field_path("label").unwrap(), "label");
        assert_eq!(validate_field_path("  external_url ").unwrap(), "external_url");
    }

    #[test]
    fn validate_field_path_rejects_anything_else() {
        // A typo must fail loudly rather than storing a row no reader
        // will ever look up.
        assert!(validate_field_path("labl").is_err());
        assert!(validate_field_path("").is_err());
        assert!(validate_field_path("page_id").is_err());
        assert!(validate_field_path("sort_order").is_err());
    }
}
