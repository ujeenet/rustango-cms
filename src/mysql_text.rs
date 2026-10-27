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
//! `Hero` and `hero` could not coexist. [`exact_identity_columns`] gives
//! the columns the CMS looks things up by a case- and accent-sensitive
//! collation instead.

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
/// codes — which must compare case- and accent-sensitively, as they do on
/// Postgres and SQLite.
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

/// Case- and accent-sensitive collations that are not binary, best first:
/// MySQL 8.0+, then MariaDB 10.10+.
///
/// A binary collation (`utf8mb4_bin`, `utf8mb4_0900_bin`) would compare
/// exactly too, but the server then flags the column `BINARY` in every
/// result set and sqlx refuses to decode it as a `String` ("not
/// compatible with SQL type `VARBINARY`").
#[cfg_attr(not(feature = "mysql"), allow(dead_code))]
const EXACT_COLLATIONS: &[&str] = &["utf8mb4_0900_as_cs", "utf8mb4_uca1400_as_cs"];

/// The collation [`exact_identity_columns`] moves the identity columns to.
#[cfg_attr(not(feature = "mysql"), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// A collation from [`EXACT_COLLATIONS`]: every identity column moves to it.
    Exact(&'static str),
    /// The server has none, so matching stays case-insensitive; only columns
    /// an earlier boot made binary — undecodable — move back to this one.
    Fallback(String),
}

#[cfg_attr(not(feature = "mysql"), allow(dead_code))]
impl Target {
    /// Picks from the collations the server has, else falls back to the
    /// schema's default when that is a non-binary utf8mb4 one.
    fn pick(available: &[String], schema_default: Option<&str>) -> Self {
        if let Some(c) = EXACT_COLLATIONS.iter().find(|c| available.iter().any(|a| a == *c)) {
            return Self::Exact(c);
        }
        match schema_default {
            Some(d) if d.starts_with("utf8mb4_") && !is_binary(d) => Self::Fallback(d.to_owned()),
            _ => Self::Fallback("utf8mb4_general_ci".to_owned()),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Exact(c) => c,
            Self::Fallback(c) => c,
        }
    }

    /// Whether a column now on `current` has to be altered.
    fn wants(&self, current: Option<&str>) -> bool {
        match self {
            Self::Exact(c) => current != Some(*c),
            Self::Fallback(_) => current.is_some_and(is_binary),
        }
    }
}

#[cfg_attr(not(feature = "mysql"), allow(dead_code))]
fn is_binary(collation: &str) -> bool {
    collation == "binary" || collation.ends_with("_bin")
}

/// On MySQL, give the CMS's identity columns a case- and accent-sensitive
/// collation (`utf8mb4_0900_as_cs` on MySQL 8), keeping each column's
/// type, nullability and default. Returns how many columns it changed; a
/// no-op off MySQL and on a column already converted.
///
/// Also repairs columns an earlier version set to `utf8mb4_bin`, which
/// made every page and admin screen fail to decode them. On a server with
/// no such collation (MySQL 5.7, older MariaDB) matching stays
/// case-insensitive and only those binary columns are moved back to the
/// schema default.
///
/// Relaxes nothing that matters: a unique column that refused `Hero` next
/// to `hero` now accepts both, as the other backends do.
///
/// # Errors
/// Driver failures reading `information_schema` or altering a column.
pub async fn exact_identity_columns(pool: &Pool) -> Result<usize, ExecError> {
    #[cfg(feature = "mysql")]
    if let Some(my) = pool.as_mysql() {
        let listed = EXACT_COLLATIONS.iter().map(|c| format!("'{c}'")).collect::<Vec<_>>().join(", ");
        let available: Vec<String> = rustango::sql::sqlx::query_scalar(&format!(
            "SELECT CAST(COLLATION_NAME AS CHAR) FROM information_schema.COLLATIONS \
             WHERE COLLATION_NAME IN ({listed})"
        ))
        .fetch_all(my)
        .await
        .map_err(ExecError::from)?;
        let schema_default: Option<String> = rustango::sql::sqlx::query_scalar(
            "SELECT CAST(DEFAULT_COLLATION_NAME AS CHAR) FROM information_schema.SCHEMATA \
             WHERE SCHEMA_NAME = DATABASE()",
        )
        .fetch_optional(my)
        .await
        .map_err(ExecError::from)?;
        let target = Target::pick(&available, schema_default.as_deref());
        if let Target::Fallback(_) = target {
            tracing::warn!(
                target: "rustango_cms::seed",
                "no case-sensitive utf8mb4 collation on this server; URL and slug matching stays case-insensitive"
            );
        }
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
                || !target.wants(collation.as_deref())
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
                "ALTER TABLE {} MODIFY COLUMN {} {column_type} CHARACTER SET utf8mb4 COLLATE {} {null}{default}",
                quote(&table),
                quote(&column),
                target.name(),
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
    use super::{unbounded_string_columns, Target};

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_target_is_a_case_sensitive_collation_never_a_binary_one() {
        let mysql8 = names(&["utf8mb4_0900_as_cs"]);
        let target = Target::pick(&mysql8, Some("utf8mb4_0900_ai_ci"));
        assert_eq!(target, Target::Exact("utf8mb4_0900_as_cs"));
        assert!(target.wants(Some("utf8mb4_0900_ai_ci")), "the default is converted");
        assert!(target.wants(Some("utf8mb4_bin")), "an earlier boot's binary column is repaired");
        assert!(!target.wants(Some("utf8mb4_0900_as_cs")), "a converted column is left alone");

        let mariadb = names(&["utf8mb4_uca1400_as_cs"]);
        assert_eq!(Target::pick(&mariadb, None), Target::Exact("utf8mb4_uca1400_as_cs"));
    }

    #[test]
    fn without_one_only_binary_columns_move_back() {
        let target = Target::pick(&[], Some("utf8mb4_unicode_ci"));
        assert_eq!(target, Target::Fallback("utf8mb4_unicode_ci".to_owned()));
        assert!(target.wants(Some("utf8mb4_bin")));
        assert!(!target.wants(Some("utf8mb4_general_ci")), "a decodable column is left alone");
        for default in [Some("utf8mb4_bin"), Some("latin1_swedish_ci"), None] {
            assert_eq!(
                Target::pick(&[], default),
                Target::Fallback("utf8mb4_general_ci".to_owned()),
                "{default:?} is not a usable fallback"
            );
        }
    }

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
