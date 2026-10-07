//! Per-field, per-locale translation overrides for **categories**.
//!
//! The fourth member of the family, after [`crate::translation`] (pages),
//! [`crate::snippet_translation`] (snippets) and
//! [`crate::menu_item_translation`] (menu items), with the same tall
//! `(row_id, locale_id, field_path, value)` shape. A category's name is
//! shown on every page filed under it (`page.categories`), so without
//! this a French page read "JARS" next to a French title.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// One row per `(category_id, locale_id, field_path)`. An empty `value`
/// is "no override": the canonical text shows through.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_category_translation",
    app = "cms",
    admin(
        list_display = "category_id, locale_id, field_path, value, updated_at",
        ordering = "category_id",
        list_filter = "locale_id, field_path",
    )
)]
pub struct CategoryTranslation {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_category", on = "id", index)]
    pub category_id: i64,

    #[rustango(fk = "cms_locale", on = "id", index)]
    pub locale_id: i64,

    /// Field on `cms_category`: one of [`TRANSLATABLE_FIELDS`].
    #[rustango(max_length = 64, index)]
    pub field_path: String,

    /// Localized value. Empty string = "no override".
    pub value: String,

    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

/// Field paths an editor may translate on a category. Closed, like the
/// menu item's: the submit handler rejects anything else. Only the name
/// reaches public pages (`page.categories`).
pub const TRANSLATABLE_FIELDS: [&str; 1] = ["name"];

/// Translations for every category in `category_ids` under one locale, as
/// `{ category_id -> { field_path -> value } }`. One query; empty ids →
/// empty map without touching the database.
///
/// # Errors
/// Driver / query failures.
pub async fn fetch_for_categories(
    pool: &rustango::sql::Pool,
    category_ids: &[i64],
    locale_id: i64,
) -> Result<
    std::collections::HashMap<i64, std::collections::HashMap<String, String>>,
    rustango::sql::ExecError,
> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    if category_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows: Vec<CategoryTranslation> = CategoryTranslation::objects()
        .where_(CategoryTranslation::locale_id.eq(locale_id))
        .where_(CategoryTranslation::category_id.is_in(category_ids.iter().copied()))
        .fetch(pool)
        .await?;
    let mut out: std::collections::HashMap<i64, std::collections::HashMap<String, String>> =
        std::collections::HashMap::new();
    for row in rows {
        if row.value.is_empty() {
            continue;
        }
        out.entry(row.category_id)
            .or_default()
            .insert(row.field_path, row.value);
    }
    Ok(out)
}

/// Delete every translation of `category_id` (category deletion cascade:
/// the foreign key would refuse the category delete otherwise).
///
/// # Errors
/// Propagates query / delete failures.
pub async fn delete_for_category_tx(
    tx: &mut rustango::sql::PoolTx<'_>,
    category_id: i64,
) -> Result<(), rustango::sql::ExecError> {
    use rustango::core::Column as _;
    let query = CategoryTranslation::objects()
        .where_(CategoryTranslation::category_id.eq(category_id))
        .compile_delete()?;
    rustango::sql::delete_tx(tx, &query).await?;
    Ok(())
}
