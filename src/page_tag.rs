//! Page tag system (#189, Wagtail parity).
//!
//! Tags are how editors organize blog-style content. Each \`PageTag\`
//! row is a (page_id, name) pair; tag names are tenant-global
//! strings (lowercased + trimmed on insert).
//!
//! Editor UI: a multi-select on the Promote tab with autocomplete
//! from existing tags. List + API filter chips lookup by tag.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_page_tag",
    app = "cms",
    display = "name",
    admin(
        list_display = "page_id, name",
        ordering = "name, page_id",
        list_filter = "name",
    )
)]
pub struct PageTag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    /// Tag name, lowercased + trimmed. Per-page-unique enforced at
    /// the application layer (composite unique on the SQL side is
    /// dialect-drifty in our ORM today).
    #[rustango(max_length = 64, index)]
    pub name: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Normalize a tag name: trim + lowercase + collapse internal
/// whitespace to single dashes (so \"Hello World\" → \"hello-world\").
#[must_use]
pub fn normalize_name(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase()
}

/// Every tag for a page, in alphabetical order.
///
/// # Errors
/// Driver / query failures.
pub async fn tags_for_page(
    pool: &rustango::sql::Pool,
    page_id: i64,
) -> Result<Vec<PageTag>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    PageTag::objects()
        .where_(PageTag::page_id.eq(page_id))
        .order_by(&[("name", false)])
        .fetch(pool)
        .await
}

/// Replace the tag set for a page. Idempotent — repeats produce no
/// duplicate rows. Tag names are normalized via [`normalize_name`].
///
/// # Errors
/// Driver / query failures.
pub async fn replace_tags(
    pool: &rustango::sql::Pool,
    page_id: i64,
    names: &[String],
) -> Result<(), rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let existing: Vec<PageTag> = PageTag::objects()
        .where_(PageTag::page_id.eq(page_id))
        .fetch(pool)
        .await?;
    for row in existing {
        row.delete_pool(pool).await?;
    }
    let mut seen = std::collections::HashSet::new();
    for n in names {
        let norm = normalize_name(n);
        if norm.is_empty() || !seen.insert(norm.clone()) {
            continue;
        }
        let mut row = PageTag {
            id: Auto::Unset,
            page_id,
            name: norm,
            created_at: Auto::Unset,
        };
        row.insert_pool(pool).await?;
    }
    Ok(())
}

/// Pre-fetch every tag attached to any page in `page_ids`, grouped
/// by `page_id` (#252). Single `IN (…)` query instead of N point
/// lookups so the public-render handler can join tags onto every
/// `children` ctx entry without an N+1.
///
/// Tags within each bucket arrive in `name` ascending order so
/// templates iterate predictably.
///
/// # Errors
/// Driver / query failures.
pub async fn prefetch_for_pages(
    pool: &rustango::sql::Pool,
    page_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<PageTag>>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    if page_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows: Vec<PageTag> = PageTag::objects()
        .where_(PageTag::page_id.is_in(page_ids.iter().copied()))
        .order_by(&[("name", false)])
        .fetch(pool)
        .await?;
    let mut by_page: std::collections::HashMap<i64, Vec<PageTag>> =
        std::collections::HashMap::new();
    for row in rows {
        by_page.entry(row.page_id).or_default().push(row);
    }
    Ok(by_page)
}

/// Every page id tagged with `name`.
///
/// # Errors
/// Driver / query failures.
pub async fn pages_with_tag(
    pool: &rustango::sql::Pool,
    name: &str,
) -> Result<Vec<i64>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let norm = normalize_name(name);
    let rows: Vec<PageTag> = PageTag::objects()
        .where_(PageTag::name.eq(norm))
        .fetch(pool)
        .await?;
    Ok(rows.into_iter().map(|r| r.page_id).collect())
}

/// Every distinct tag in the tenant + its usage count, sorted by
/// (-count, name).
///
/// # Errors
/// Driver / query failures.
pub async fn all_tags(
    pool: &rustango::sql::Pool,
) -> Result<Vec<(String, usize)>, rustango::sql::ExecError> {
    use rustango::sql::FetcherPool as _;
    let rows: Vec<PageTag> = PageTag::objects().fetch(pool).await?;
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for row in rows {
        *counts.entry(row.name).or_default() += 1;
    }
    let mut out: Vec<(String, usize)> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lowercases() {
        assert_eq!(normalize_name("Hello"), "hello");
    }

    #[test]
    fn normalize_collapses_whitespace_to_dash() {
        assert_eq!(normalize_name("  Hello   World "), "hello-world");
    }

    #[test]
    fn normalize_empty_stays_empty() {
        assert_eq!(normalize_name("   "), "");
    }

    #[test]
    fn normalize_idempotent() {
        let once = normalize_name("Foo Bar");
        let twice = normalize_name(&once);
        assert_eq!(once, twice);
    }
}
