//! Search query log + editor-pinned results.
//!
//! Two tables:
//! - `cms_search_query` — one row per normalized query string, with
//!   a rolling hit counter. Logged best-effort from the admin search
//!   handler.
//! - `cms_search_promotion` — editor-pinned `(query, page)` pairs that
//!   surface at the top of matching search responses.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_search_query",
    app = "cms",
    display = "raw",
    admin(
        list_display = "normalized, raw, hits, last_seen_at",
        ordering = "-hits, -last_seen_at",
        search_fields = "normalized, raw",
    )
)]
pub struct SearchQuery {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Trimmed + lowercased + whitespace-collapsed form. Indexed for
    /// upsert lookups; the canonical key the rest of the system uses.
    #[rustango(max_length = 200, index)]
    pub normalized: String,

    /// First-seen original casing — used as the friendly display in
    /// the analytics report.
    #[rustango(max_length = 200)]
    pub raw: String,

    /// Total number of times this query has been logged.
    pub hits: i64,

    #[rustango(auto_now_add)]
    pub first_seen_at: Auto<DateTime<Utc>>,

    #[rustango(auto_now)]
    pub last_seen_at: Auto<DateTime<Utc>>,
}

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_search_promotion",
    app = "cms",
    display = "id",
    admin(
        list_display = "query_id, page_id, sort_order",
        ordering = "query_id, sort_order",
    )
)]
pub struct SearchPromotion {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_search_query", on = "id", index)]
    pub query_id: i64,

    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    /// Ordering among pinned pages for this query.
    pub sort_order: i32,

    /// Editor's optional note explaining why this result is pinned.
    /// Surfaces in the admin promotions list, not on the public side.
    #[rustango(max_length = 200)]
    pub description: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Normalize a user-typed query: trim + collapse whitespace +
/// lowercase. The normalized form drives both the dedup key and the
/// promotion-match lookup.
#[must_use]
pub fn normalize_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Longest visitor query that is logged; longer ones are noise or abuse.
const MAX_LOGGED_QUERY: usize = 100;

/// Log a visitor's search from the public API, so the search report
/// shows what visitors look for, not only admin searches. First page only
/// (paging through results isn't a new search), capped in length, run in
/// the background, and errors only logged: it never slows or fails the
/// search itself.
pub fn log_public_query(pool: &rustango::sql::Pool, raw: &str, offset: usize) {
    if offset != 0 {
        return;
    }
    let query = normalize_query(raw);
    if query.is_empty() || query.chars().count() > MAX_LOGGED_QUERY {
        return;
    }
    let pool = pool.clone();
    rustango::__private_runtime::tokio::spawn(async move {
        if let Err(e) = log_query(&pool, &query).await {
            tracing::warn!(target: "rustango_cms::search", error = %e, "visitor search not logged");
        }
    });
}

/// Increment the hit count for `raw` (upserting a new row when this
/// is the first time it's been logged). Best-effort — caller should
/// ignore the `Result` if logging shouldn't gate the request path.
///
/// # Errors
/// Driver / query failures.
pub async fn log_query(
    pool: &rustango::sql::Pool,
    raw: &str,
) -> Result<SearchQuery, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let normalized = normalize_query(raw);
    if normalized.is_empty() {
        // Defensive — empty queries shouldn't pollute the table.
        // Return a synthetic row caller can discard.
        return Ok(SearchQuery {
            id: Auto::Unset,
            normalized: String::new(),
            raw: String::new(),
            hits: 0,
            first_seen_at: Auto::Unset,
            last_seen_at: Auto::Unset,
        });
    }
    let existing: Vec<SearchQuery> = SearchQuery::objects()
        .where_(SearchQuery::normalized.eq(normalized.clone()))
        .fetch(pool)
        .await?;
    if let Some(mut row) = existing.into_iter().next() {
        row.hits += 1;
        row.save_pool(pool).await?;
        return Ok(row);
    }
    let mut row = SearchQuery {
        id: Auto::Unset,
        normalized,
        raw: raw.trim().to_owned(),
        hits: 1,
        first_seen_at: Auto::Unset,
        last_seen_at: Auto::Unset,
    };
    row.insert_pool(pool).await?;
    Ok(row)
}

/// Look up the SearchQuery row whose `normalized` form matches.
/// Returns `None` if the query has never been logged.
///
/// # Errors
/// Driver / query failures.
pub async fn find_by_query(
    pool: &rustango::sql::Pool,
    raw: &str,
) -> Result<Option<SearchQuery>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let normalized = normalize_query(raw);
    if normalized.is_empty() {
        return Ok(None);
    }
    let mut rows: Vec<SearchQuery> = SearchQuery::objects()
        .where_(SearchQuery::normalized.eq(normalized))
        .fetch(pool)
        .await?;
    Ok(rows.pop())
}

/// Every pinned promotion for `query_id`, sorted by `sort_order`.
///
/// # Errors
/// Driver / query failures.
pub async fn promotions_for_query(
    pool: &rustango::sql::Pool,
    query_id: i64,
) -> Result<Vec<SearchPromotion>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    SearchPromotion::objects()
        .where_(SearchPromotion::query_id.eq(query_id))
        .order_by(&[("sort_order", false)])
        .fetch(pool)
        .await
}

/// Top N queries by hit count, newest-tie-breaker on `last_seen_at`.
///
/// # Errors
/// Driver / query failures.
pub async fn top_queries(
    pool: &rustango::sql::Pool,
    limit: usize,
) -> Result<Vec<SearchQuery>, rustango::sql::ExecError> {
    use rustango::sql::FetcherPool as _;
    let mut rows: Vec<SearchQuery> = SearchQuery::objects().fetch(pool).await?;
    rows.sort_by(|a, b| {
        b.hits
            .cmp(&a.hits)
            .then_with(|| b.last_seen_at.get().cmp(&a.last_seen_at.get()))
    });
    rows.truncate(limit);
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lowercases_and_collapses() {
        assert_eq!(normalize_query("  Hello   WORLD "), "hello world");
    }

    #[test]
    fn normalize_drops_empty() {
        assert_eq!(normalize_query("   "), "");
    }

    #[test]
    fn normalize_idempotent() {
        let once = normalize_query("Foo Bar");
        let twice = normalize_query(&once);
        assert_eq!(once, twice);
    }
}
