//! Per-user persistent banner / announcement dismissals.
//!
//! One row per `(user_id, key)` — when present, the banner with that
//! `key` stays dismissed across sessions. The CMS itself ships no
//! announcements yet; this module exists as the primitive so future
//! upgrade banners / what's-new prompts persist correctly.
//!
//! ## Usage
//!
//! Templates check the dismissal state via the `is_dismissed(key=…)`
//! Tera function. Any element with `data-dismissible="<key>"` plus a
//! child `[data-dismiss-btn]` posts to
//! `/cms-admin/dismissibles/<key>` and hides itself.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_dismissible",
    app = "cms",
    display = "key",
    admin(list_display = "user_id, key, dismissed_at", ordering = "user_id, key",)
)]
pub struct Dismissible {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "rustango_users", on = "id", index)]
    pub user_id: i64,

    /// Stable identifier. Keep snake_case + namespaced
    /// (`upgrade.v0_4`, `announce.holiday_freeze`).
    #[rustango(max_length = 128, index)]
    pub key: String,

    #[rustango(auto_now_add)]
    pub dismissed_at: Auto<DateTime<Utc>>,
}

/// Whether `user_id` has dismissed the banner identified by `key`.
///
/// # Errors
/// Driver / query failures.
pub async fn is_dismissed(
    pool: &rustango::sql::Pool,
    user_id: i64,
    key: &str,
) -> Result<bool, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<Dismissible> = Dismissible::objects()
        .where_(Dismissible::user_id.eq(user_id))
        .where_(Dismissible::key.eq(key.to_owned()))
        .fetch(pool)
        .await?;
    Ok(!rows.is_empty())
}

/// Record a dismissal. Idempotent — repeated calls don't insert
/// duplicates.
///
/// # Errors
/// Driver / query failures.
pub async fn dismiss(
    pool: &rustango::sql::Pool,
    user_id: i64,
    key: &str,
) -> Result<Dismissible, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let existing: Vec<Dismissible> = Dismissible::objects()
        .where_(Dismissible::user_id.eq(user_id))
        .where_(Dismissible::key.eq(key.to_owned()))
        .fetch(pool)
        .await?;
    if let Some(row) = existing.into_iter().next() {
        return Ok(row);
    }
    let mut row = Dismissible {
        id: Auto::Unset,
        user_id,
        key: key.to_owned(),
        dismissed_at: Auto::Unset,
    };
    row.insert_pool(pool).await?;
    Ok(row)
}

/// Clear a previous dismissal (re-show the banner). Useful for
/// targeted re-prompts (e.g. \"Hey, this announcement was updated\").
///
/// # Errors
/// Driver / query failures.
pub async fn undismiss(
    pool: &rustango::sql::Pool,
    user_id: i64,
    key: &str,
) -> Result<usize, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<Dismissible> = Dismissible::objects()
        .where_(Dismissible::user_id.eq(user_id))
        .where_(Dismissible::key.eq(key.to_owned()))
        .fetch(pool)
        .await?;
    let mut removed = 0;
    for row in rows {
        row.delete_pool(pool).await?;
        removed += 1;
    }
    Ok(removed)
}
