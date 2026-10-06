//! Concurrent-editor tracking (#106, Wagtail parity B11).
//!
//! Pairs with the editor lock (#72): the lock prevents conflicting
//! writes; the editing-session model surfaces a banner so people
//! *see* each other before they collide. Every time the editor JS
//! pings the lock heartbeat (`POST /cms-admin/pages/{id}/lock-heartbeat`),
//! we also touch a row in `cms_editing_session` so a separate JSON
//! endpoint can list the other active viewers/editors.
//!
//! ## Liveness
//!
//! A session is "active" when `last_seen_at` is within the last
//! [`ACTIVE_WINDOW_SECS`] seconds. Older rows are garbage-collected
//! lazily on heartbeat. Wagtail's GC threshold is 1 hour; we mirror
//! that — long enough to survive transient network blips, short
//! enough that a closed-tab session falls off in reasonable time.
//!
//! ## "Viewing" vs "Editing"
//!
//! The `is_editing` boolean is a hint, not a hard guarantee. The
//! editor JS sets it `true` once the form is touched (any `input`
//! event) and resets it on every page load. Useful UX signal —
//! "alice is editing" reads more urgent than "alice is viewing".

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// How long a session row counts as "active". Stale rows past this
/// window are filtered out of the JSON response and reaped on the
/// next heartbeat.
pub const ACTIVE_WINDOW_SECS: i64 = 90;

/// Rows older than this are garbage-collected on heartbeat — long
/// past the active window so transient outages don't lose sessions
/// the editor would otherwise rejoin.
pub const GC_THRESHOLD_SECS: i64 = 60 * 60;

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_editing_session",
    app = "cms",
    display = "user_id",
    admin(
        list_display = "page_id, user_id, is_editing, last_seen_at",
        ordering = "-last_seen_at",
        list_filter = "page_id, is_editing",
    )
)]
pub struct EditingSession {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    #[rustango(fk = "rustango_users", on = "id", index)]
    pub user_id: i64,

    /// Bumped by each heartbeat. The active-vs-stale gate compares
    /// this against `now() - ACTIVE_WINDOW_SECS`.
    pub last_seen_at: DateTime<Utc>,

    /// `true` once the editor JS has detected a form touch on this
    /// page. Set by the heartbeat ping body (`?is_editing=1`) and
    /// reset to `false` on a fresh page load.
    pub is_editing: bool,
}

/// UPSERT the heartbeat row for `(page_id, user_id)` and bump
/// `last_seen_at` to now. Application-enforced uniqueness — we
/// don't have a SQL UNIQUE constraint because not every dialect
/// agrees on composite uniqueness syntax. The race window is small
/// enough that the worst-case duplicate row is fine; the GC drops
/// them.
///
/// # Errors
/// Driver / query failures.
pub async fn touch(
    pool: &rustango::sql::Pool,
    page_id: i64,
    user_id: i64,
    is_editing: bool,
) -> Result<(), rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let existing: Vec<EditingSession> = EditingSession::objects()
        .where_(EditingSession::page_id.eq(page_id))
        .where_(EditingSession::user_id.eq(user_id))
        .fetch(pool)
        .await?;

    if let Some(mut row) = existing.into_iter().next() {
        row.last_seen_at = Utc::now();
        // `is_editing` is sticky-true within a session: once any
        // ping reports editing, the row stays editing until a fresh
        // page-load resets it. Reduces UI flicker between viewing
        // and editing banners.
        if is_editing {
            row.is_editing = true;
        }
        row.save_pool(pool).await?;
    } else {
        let mut row = EditingSession {
            id: Auto::Unset,
            page_id,
            user_id,
            last_seen_at: Utc::now(),
            is_editing,
        };
        row.insert_pool(pool).await?;
    }
    Ok(())
}

/// Other active sessions on this page (viewer's own session filtered
/// out). Returns `(user_id, last_seen_at, is_editing)` tuples sorted
/// by `last_seen_at` desc.
///
/// # Errors
/// Driver / query failures.
pub async fn others_on_page(
    pool: &rustango::sql::Pool,
    page_id: i64,
    viewer_id: i64,
) -> Result<Vec<EditingSession>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let cutoff = Utc::now() - chrono::Duration::seconds(ACTIVE_WINDOW_SECS);
    let rows: Vec<EditingSession> = EditingSession::objects()
        .where_(EditingSession::page_id.eq(page_id))
        .where_(EditingSession::user_id.ne(viewer_id))
        .order_by(&[("last_seen_at", true)]) // desc
        .fetch(pool)
        .await?;
    Ok(rows
        .into_iter()
        .filter(|r| r.last_seen_at >= cutoff)
        .collect())
}

/// Garbage-collect rows older than [`GC_THRESHOLD_SECS`]. Called
/// from the heartbeat handler so the table doesn't grow unbounded
/// — we don't need a cron.
///
/// # Errors
/// Driver / query failures.
pub async fn gc_stale(pool: &rustango::sql::Pool) -> Result<usize, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let cutoff = Utc::now() - chrono::Duration::seconds(GC_THRESHOLD_SECS);
    let stale: Vec<EditingSession> = EditingSession::objects()
        .where_(EditingSession::last_seen_at.lt(cutoff))
        .fetch(pool)
        .await?;
    let count = stale.len();
    for row in stale {
        row.delete_pool(pool).await?;
    }
    Ok(count)
}
