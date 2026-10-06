//! Inline comments on the page editor (#81, Wagtail parity A3).
//!
//! Editors can leave a per-field comment thread on any page they're
//! editing — `TODO: confirm date with marketing` on the body field,
//! a reviewer reply, a resolve action when the concern is addressed.
//!
//! PR 1 of N lands the data model + the side-panel surface + thread
//! API. PR 2 adds inline anchor pins (click-to-comment next to each
//! field).
//!
//! ## Shape
//!
//! - One [`Comment`] row per thread root (pinned to a `field_path`)
//! - N [`CommentReply`] rows per thread (chronological)
//! - Threads resolve at the root via `Comment.resolved_at`
//!
//! ## Field-path shape
//!
//! Same shape `cms_translation.field_path` uses — dotted path into
//! a page's scalar columns or its StreamField blocks. Examples:
//! `title`, `body.<block-uuid>.heading`, `seo_description`.
//! Resilient to field renames at the StreamField level (block UUIDs
//! survive); scalar renames invalidate the thread until manually
//! moved.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_comment",
    app = "cms",
    display = "id",
    admin(
        list_display = "page_id, field_path, author_id, resolved_at, created_at",
        ordering = "-created_at",
        list_filter = "page_id, resolved_at",
    )
)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    /// Same shape as `cms_translation.field_path`. Indexed so the
    /// per-field annotation badge can count threads in one query.
    #[rustango(max_length = 255, index)]
    pub field_path: String,

    #[rustango(fk = "rustango_users", on = "id", index)]
    pub author_id: i64,

    /// Free-form text. No markdown rendering for v1 — kept plain so
    /// rendering can't be a security surface. Multi-line wraps in
    /// the side panel via `white-space: pre-wrap`.
    pub body: String,

    /// Set when an editor (typically the original author or any
    /// reviewer) marks the thread resolved. `None` while open.
    pub resolved_at: Option<DateTime<Utc>>,

    /// User id who clicked Resolve. `None` while open.
    pub resolved_by: Option<i64>,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_comment_reply",
    app = "cms",
    display = "id",
    admin(
        list_display = "comment_id, author_id, created_at",
        ordering = "comment_id, created_at",
        list_filter = "comment_id",
    )
)]
pub struct CommentReply {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// FK to the thread root.
    #[rustango(fk = "cms_comment", on = "id", index)]
    pub comment_id: i64,

    #[rustango(fk = "rustango_users", on = "id", index)]
    pub author_id: i64,

    pub body: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Fetch every open + resolved thread for a page, in newest-first
/// order. The side panel filters resolved threads to a separate
/// "Resolved" bucket client-side.
///
/// # Errors
/// Driver / query failures.
pub async fn threads_for_page(
    pool: &rustango::sql::Pool,
    page_id: i64,
) -> Result<Vec<Comment>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    Comment::objects()
        .where_(Comment::page_id.eq(page_id))
        .order_by(&[("created_at", true)]) // desc
        .fetch(pool)
        .await
}

/// Fetch every reply for a list of thread ids in one query, then
/// bucket per thread. Returns `comment_id → Vec<replies>`.
///
/// # Errors
/// Driver / query failures.
pub async fn replies_for_threads(
    pool: &rustango::sql::Pool,
    thread_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<CommentReply>>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    if thread_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let all: Vec<CommentReply> = CommentReply::objects()
        .where_(CommentReply::comment_id.is_in(thread_ids.iter().copied()))
        .order_by(&[("created_at", false), ("id", false)])
        .fetch(pool)
        .await?;
    let mut out: std::collections::HashMap<i64, Vec<CommentReply>> =
        std::collections::HashMap::new();
    for r in all {
        out.entry(r.comment_id).or_default().push(r);
    }
    Ok(out)
}
