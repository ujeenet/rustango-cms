//! `PageLock` — best-effort editor lock on a `cms_page` row.
//!
//! Two editors opening the same page in a Wagtail-shaped CMS expect a
//! "Page X is being edited by Y" banner instead of a silent
//! last-write-wins clobber. This module is the foundational lock
//! primitive; multi-step workflows (#73), inline comments (#81), and
//! editing-session tracking (B11) all sit on top of it.
//!
//! ## Semantics
//!
//! - One lock row per page (UNIQUE on `page_id`).
//! - Lock is *advisory*: acquired on edit-form open, refreshed by the
//!   editor's heartbeat ping, released on a successful POST. A stale
//!   lock (no heartbeat past `expires_at`) is treated as released —
//!   the next editor reuses the row in place.
//! - Force-unlock is allowed for anyone with the page's Edit
//!   permission; it deletes the row and is audit-logged.
//!
//! Why best-effort + TTL instead of a hard SQL lock: editors close
//! browser tabs all the time; we can't tell the difference between
//! "closed the tab" and "still editing." A 30-minute sliding TTL gives
//! the owner room to be away from keyboard without permanently jamming
//! the page for everyone else.
//!
//! ## Wagtail parity
//!
//! Mirrors `wagtail.locks.BasicLock` shape. Workflow-driven locks
//! (`WorkflowLock`, `ScheduledForPublishLock`) ship with #73.
//!
//! See `src/admin/handlers.rs::page_edit_form` for the acquire site
//! and `src/admin/handlers.rs::page_edit_submit` for the release site.

use chrono::{DateTime, Duration, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// How long a lock survives without a heartbeat refresh. Matches the
/// default in Wagtail's `WAGTAILADMIN_LOCK_TIMEOUT` shape; tune via
/// `RUSTANGO_CMS_LOCK_TTL_SECS` if shorter sessions are needed.
pub const DEFAULT_TTL_SECS: i64 = 30 * 60;

/// Heartbeat cadence the editor JS uses to refresh the lock. Set
/// well under `DEFAULT_TTL_SECS / 2` so a missed ping doesn't drop
/// the lock mid-session.
pub const HEARTBEAT_INTERVAL_SECS: i64 = 60;

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_page_lock",
    app = "cms",
    display = "page_id",
    admin(
        list_display = "page_id, user_id, acquired_at, expires_at",
        ordering = "-acquired_at",
        list_filter = "user_id",
    )
)]
pub struct PageLock {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// FK to `cms_page.id`. Application-enforced UNIQUE: only one
    /// live lock row per page (we delete + insert on takeover rather
    /// than rely on an SQL constraint to avoid CI gymnastics on
    /// sqlite/MySQL/PG schema drift).
    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    /// FK to `rustango_users.id` — the editor holding the lock.
    #[rustango(fk = "rustango_users", on = "id", index)]
    pub user_id: i64,

    /// When the lock was first acquired (not refreshed). Surfaces in
    /// the banner as "User X started editing 5 minutes ago."
    #[rustango(auto_now_add)]
    pub acquired_at: Auto<DateTime<Utc>>,

    /// Sliding TTL — bumped by every heartbeat ping. After this
    /// instant the lock is treated as released.
    pub expires_at: DateTime<Utc>,
}

impl PageLock {
    /// True when this lock has aged past its TTL and should be
    /// treated as released by callers.
    #[must_use]
    pub fn is_stale(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now
    }

    /// True when `user_id` owns this lock (and it isn't stale).
    #[must_use]
    pub fn is_owned_by(&self, user_id: i64, now: DateTime<Utc>) -> bool {
        !self.is_stale(now) && self.user_id == user_id
    }
}

/// Outcome of an acquire attempt. `Acquired` means we now hold the
/// lock (new row, or refreshed our own row). `Held` means someone
/// else holds an unexpired lock; the caller should render the
/// read-only banner. `Stolen` is the result when `force=true` —
/// previous holder's row was deleted and we own the new one.
#[derive(Debug, Clone)]
pub enum AcquireOutcome {
    /// We hold the lock. Payload is the live row.
    Acquired(PageLock),
    /// Someone else holds the lock. Payload is their row (used to
    /// render the banner — username + when acquired).
    Held(PageLock),
}

/// Best-effort acquire: returns `Acquired` if the page is free or
/// already owned by `user_id`, else `Held` with the other user's
/// row. Refreshes `expires_at` on every call when we own the lock.
///
/// `force=true` (admin force-unlock) deletes any existing row and
/// inserts a fresh one owned by `user_id`. Callers are responsible
/// for the audit-log entry.
///
/// # Errors
/// Driver / query failures.
pub async fn acquire(
    pool: &rustango::sql::Pool,
    page_id: i64,
    user_id: i64,
    force: bool,
) -> Result<AcquireOutcome, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let now = Utc::now();
    let ttl = Duration::seconds(DEFAULT_TTL_SECS);
    let new_expires = now + ttl;

    let existing: Vec<PageLock> = PageLock::objects()
        .where_(PageLock::page_id.eq(page_id))
        .fetch(pool)
        .await?;

    if let Some(mut row) = existing.into_iter().next() {
        let stale = row.is_stale(now);
        let mine = row.user_id == user_id;
        if mine || stale || force {
            // Take over: refresh / claim the row in place.
            row.user_id = user_id;
            row.expires_at = new_expires;
            row.save_pool(pool).await?;
            Ok(AcquireOutcome::Acquired(row))
        } else {
            Ok(AcquireOutcome::Held(row))
        }
    } else {
        let mut row = PageLock {
            id: Auto::Unset,
            page_id,
            user_id,
            acquired_at: Auto::Unset,
            expires_at: new_expires,
        };
        row.insert_pool(pool).await?;
        Ok(AcquireOutcome::Acquired(row))
    }
}

/// Release a lock owned by `user_id`. No-op if the row is missing,
/// stale, or owned by someone else (the caller's session ended; we
/// shouldn't silently unlock another user's edit).
///
/// # Errors
/// Driver / query failures.
pub async fn release(
    pool: &rustango::sql::Pool,
    page_id: i64,
    user_id: i64,
) -> Result<bool, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let existing: Vec<PageLock> = PageLock::objects()
        .where_(PageLock::page_id.eq(page_id))
        .fetch(pool)
        .await?;
    let Some(row) = existing.into_iter().next() else {
        return Ok(false);
    };
    let now = Utc::now();
    if row.user_id == user_id || row.is_stale(now) {
        row.delete_pool(pool).await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Look up the current lock on a page without mutating it. Returns
/// `None` if no row exists OR the row is stale. The admin
/// `page_edit_form` calls this before deciding whether to render the
/// banner.
///
/// # Errors
/// Driver / query failures.
pub async fn current(
    pool: &rustango::sql::Pool,
    page_id: i64,
) -> Result<Option<PageLock>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let existing: Vec<PageLock> = PageLock::objects()
        .where_(PageLock::page_id.eq(page_id))
        .fetch(pool)
        .await?;
    let now = Utc::now();
    Ok(existing.into_iter().find(|r| !r.is_stale(now)))
}

/// Force-release a lock regardless of ownership. Used by the
/// `POST /cms-admin/pages/{id}/unlock` admin handler. Returns the
/// row that was deleted (so the caller can audit-log who held it),
/// or `None` if the row was already gone.
///
/// # Errors
/// Driver / query failures.
pub async fn force_release(
    pool: &rustango::sql::Pool,
    page_id: i64,
) -> Result<Option<PageLock>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let existing: Vec<PageLock> = PageLock::objects()
        .where_(PageLock::page_id.eq(page_id))
        .fetch(pool)
        .await?;
    let Some(row) = existing.into_iter().next() else {
        return Ok(None);
    };
    let snapshot = row.clone();
    row.delete_pool(pool).await?;
    Ok(Some(snapshot))
}
