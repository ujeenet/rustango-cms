//! Long content columns on MySQL.
//!
//! The framework renders an unbounded `String` as `TEXT` on MySQL, which
//! holds 65,535 bytes; Postgres `text` and SQLite `TEXT` are unbounded. A
//! long StreamField body, translation blob or snippet overflows it: strict
//! mode refuses the save (ERROR 1406) and non-strict mode truncates the
//! JSON. Until the framework maps these to a long type,
//! [`widen_long_text`] converts them in place.
//!
//! Its text columns also inherit MySQL's default collation, which is
//! case- and accent-insensitive, so `/ABOUT` served `/about` and slugs
//! `Hero` and `hero` could not coexist. Until the framework sets a
//! binary collation, [`exact_identity_columns`] gives the
//! columns the CMS looks things up by one.

use rustango::sql::{ExecError, Pool};

/// On MySQL, turn every `TEXT` column that a registered model declares as
/// an unbounded `String` into `LONGTEXT`, keeping its nullability and
/// collation. Returns how many columns it changed.
///
/// Idempotent: a column already widened is no longer `TEXT`, so a re-run
/// finds nothing. Covers host page-type extension tables too, as long as
/// they exist when it runs — one created later is widened on the next
/// boot. A no-op off MySQL.
///
/// # Errors
/// Driver failures reading `information_schema` or altering a column.
pub async fn widen_long_text(pool: &Pool) -> Result<usize, ExecError> {
    #[cfg(feature = "mysql")]
    if let Some(my) = pool.as_mysql() {
        let wanted = unbounded_string_columns();
        // information_schema reports names as binary strings on MySQL 8+;
        // the casts keep them decodable as `String`.
        let rows: Vec<(String, String, String, Option<String>)> = rustango::sql::sqlx::query_as(
            "SELECT CAST(TABLE_NAME AS CHAR), CAST(COLUMN_NAME AS CHAR), \
             CAST(IS_NULLABLE AS CHAR), CAST(COLLATION_NAME AS CHAR) \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() AND DATA_TYPE = 'text'",
        )
        .fetch_all(my)
        .await
        .map_err(ExecError::from)?;
        let mut changed = 0;
        for (table, column, nullable, collation) in rows {
            if !wanted.contains(&(table.as_str(), column.as_str())) {
                continue;
            }
            let null = if nullable == "YES" { "NULL" } else { "NOT NULL" };
            let collate = collation.map(|c| format!(" COLLATE {c}")).unwrap_or_default();
            let sql = format!(
                "ALTER TABLE {} MODIFY COLUMN {} LONGTEXT{collate} {null}",
                quote(&table),
                quote(&column),
            );
            rustango::sql::raw_execute_pool(pool, &sql, Vec::new()).await?;
            changed += 1;
        }
        return Ok(changed);
    }
    let _ = pool;
    Ok(0)
}

/// Columns the CMS matches exactly — URLs, slugs, hostnames, locale
/// codes — which must compare byte-wise as they do on Postgres and SQLite.
#[cfg_attr(not(feature = "mysql"), allow(dead_code))]
const IDENTITY_COLUMNS: &[(&str, &str)] = &[
    ("cms_page", "url_path"),
    ("cms_page", "slug"),
    ("cms_redirect", "from_path"),
    ("cms_snippet", "slug"),
    ("cms_site", "hostname"),
    ("cms_theme", "slug"),
    ("cms_category", "slug"),
    ("cms_taxonomy", "slug"),
    ("cms_locale", "code"),
];

/// On MySQL, give the CMS's identity columns the binary `utf8mb4_bin`
/// collation, keeping each column's type, nullability and default.
/// Returns how many columns it changed; a no-op off MySQL and on a
/// column already binary.
///
/// Relaxes nothing that matters: a unique column that refused `Hero` next
/// to `hero` now accepts both, as the other backends do.
///
/// # Errors
/// Driver failures reading `information_schema` or altering a column.
pub async fn exact_identity_columns(pool: &Pool) -> Result<usize, ExecError> {
    #[cfg(feature = "mysql")]
    if let Some(my) = pool.as_mysql() {
        let rows: Vec<(String, String, String, String, Option<String>, Option<String>)> =
            rustango::sql::sqlx::query_as(
                "SELECT CAST(TABLE_NAME AS CHAR), CAST(COLUMN_NAME AS CHAR), \
                 CAST(COLUMN_TYPE AS CHAR), CAST(IS_NULLABLE AS CHAR), \
                 CAST(COLUMN_DEFAULT AS CHAR), CAST(COLLATION_NAME AS CHAR) \
                 FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = DATABASE() AND COLLATION_NAME IS NOT NULL",
            )
            .fetch_all(my)
            .await
            .map_err(ExecError::from)?;
        let mut changed = 0;
        for (table, column, column_type, nullable, default, collation) in rows {
            if !IDENTITY_COLUMNS.contains(&(table.as_str(), column.as_str()))
                || collation.as_deref() == Some("utf8mb4_bin")
            {
                continue;
            }
            let null = if nullable == "YES" { "NULL" } else { "NOT NULL" };
            // A TEXT column takes no literal default on MySQL; a VARCHAR
            // keeps the one it had.
            let default = match default {
                Some(d) if !column_type.contains("text") => format!(" DEFAULT '{}'", d.replace('\'', "''")),
                _ => String::new(),
            };
            let sql = format!(
                "ALTER TABLE {} MODIFY COLUMN {} {column_type} CHARACTER SET utf8mb4 COLLATE utf8mb4_bin {null}{default}",
                quote(&table),
                quote(&column),
            );
            rustango::sql::raw_execute_pool(pool, &sql, Vec::new()).await?;
            changed += 1;
        }
        return Ok(changed);
    }
    let _ = pool;
    Ok(0)
}

/// `(table, column)` for every unbounded `String` field of a registered
/// model — the fields the framework renders as `TEXT` on MySQL.
#[cfg_attr(not(feature = "mysql"), allow(dead_code))]
fn unbounded_string_columns() -> std::collections::HashSet<(&'static str, &'static str)> {
    inventory::iter::<rustango::core::ModelEntry>
        .into_iter()
        .flat_map(|entry| {
            let table = entry.schema.table;
            entry
                .schema
                .fields
                .iter()
                .filter(|f| matches!(f.ty, rustango::core::FieldType::String) && f.max_length.is_none())
                .map(move |f| (table, f.column))
        })
        .collect()
}

#[cfg_attr(not(feature = "mysql"), allow(dead_code))]
fn quote(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

#[cfg(test)]
mod tests {
    use super::unbounded_string_columns;

    #[test]
    fn the_editor_content_columns_are_covered() {
        let cols = unbounded_string_columns();
        for want in [
            ("cms_translation", "value"),
            ("cms_snippet_translation", "value"),
            ("cms_snippet", "body_markdown"),
            ("cms_error_page", "body"),
        ] {
            assert!(cols.contains(&want), "{want:?} not found among unbounded strings");
        }
        assert!(!cols.contains(&("cms_page", "slug")), "bounded columns are left alone");
    }
}
