//! Per-page audit log (#192, Wagtail parity).
//!
//! Mirrors Wagtail's `PageLogEntry`: every meaningful action on a
//! page (publish, edit, move, copy, alias create, restore, lock,
//! unlock, force-unlock, workflow submit / approve / reject / cancel,
//! comment new / resolved / reopened) lands a row. Revisions answer
//! "what changed"; log entries answer "who did what, when".
//!
//! Best-effort: a log-write failure is logged + swallowed; the
//! underlying action commits regardless. Compliance > availability
//! ordering — but in our case the action is the source of truth and
//! the log is observability.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

// Action constants. Strings (not enum) so plugins can extend the
// taxonomy without an upstream change.
pub const ACTION_EDIT: &str = "edit";
pub const ACTION_PUBLISH: &str = "publish";
pub const ACTION_UNPUBLISH: &str = "unpublish";
pub const ACTION_MOVE: &str = "move";
pub const ACTION_COPY: &str = "copy";
pub const ACTION_ALIAS_CREATE: &str = "alias_create";
pub const ACTION_RESTORE: &str = "restore";
pub const ACTION_LOCK: &str = "lock";
pub const ACTION_UNLOCK: &str = "unlock";
pub const ACTION_FORCE_UNLOCK: &str = "force_unlock";
pub const ACTION_WORKFLOW_SUBMIT: &str = "workflow_submit";
pub const ACTION_WORKFLOW_APPROVE: &str = "workflow_approve";
pub const ACTION_WORKFLOW_REJECT: &str = "workflow_reject";
pub const ACTION_WORKFLOW_CANCEL: &str = "workflow_cancel";
pub const ACTION_COMMENT_NEW: &str = "comment_new";
pub const ACTION_COMMENT_RESOLVE: &str = "comment_resolve";
pub const ACTION_COMMENT_REOPEN: &str = "comment_reopen";
pub const ACTION_DELETE: &str = "delete";

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_page_log_entry",
    app = "cms",
    display = "action",
    admin(
        list_display = "page_id, action, actor_id, created_at",
        ordering = "-created_at",
        list_filter = "action",
    )
)]
pub struct PageLogEntry {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    /// Symbolic action name; values from the `ACTION_*` constants.
    #[rustango(max_length = 64, index)]
    pub action: String,

    /// Who did it. None means a system / scheduled action — schedule
    /// publisher, automated workflow advance, sweep job.
    #[rustango(fk = "rustango_users", on = "id")]
    pub actor_id: Option<i64>,

    /// Short human-readable summary ("Published", "Moved to /blog/",
    /// "Forced unlock of bob's edit session"). Render verbatim in
    /// the timeline.
    #[rustango(max_length = 500, default = "''")]
    pub message: String,

    /// JSON blob — structured metadata for future filtering (e.g.
    /// `{"workflow_id": 3, "task_id": 7}` on a workflow_submit row).
    /// Stored as TEXT to stay ORM-portable across dialects.
    #[rustango(default = "'{}'")]
    pub data_json: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Insert a log entry. Best-effort; on failure logs + returns Ok so
/// the call-site doesn't have to special-case audit-write errors.
///
/// `data` is serialised into the `data_json` column. Pass `None` for
/// "no extra metadata"; pass any `serde_json::Value` otherwise.
///
/// # Errors
/// Driver / query failures bubble up — callers typically `let _ =`
/// the result so audit failures don't abort the underlying action.
pub async fn record(
    pool: &rustango::sql::Pool,
    page_id: i64,
    action: &str,
    actor_id: Option<i64>,
    message: impl Into<String>,
    data: Option<serde_json::Value>,
) -> Result<(), rustango::sql::ExecError> {
    let data_json = data
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".to_owned()))
        .unwrap_or_else(|| "{}".to_owned());
    let mut row = PageLogEntry {
        id: Auto::Unset,
        page_id,
        action: action.to_owned(),
        actor_id,
        message: message.into(),
        data_json,
        created_at: Auto::Unset,
    };
    row.insert_pool(pool).await?;
    Ok(())
}

/// Every log entry for a page, newest first. Bounded by `limit` so
/// the history tab doesn't render thousands of rows; pass `None` to
/// return everything (CSV export uses that).
///
/// # Errors
/// Driver / query failures.
pub async fn for_page(
    pool: &rustango::sql::Pool,
    page_id: i64,
    limit: Option<usize>,
) -> Result<Vec<PageLogEntry>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let mut rows: Vec<PageLogEntry> = PageLogEntry::objects()
        .where_(PageLogEntry::page_id.eq(page_id))
        .order_by(&[("created_at", true)])
        .fetch(pool)
        .await?;
    if let Some(cap) = limit {
        rows.truncate(cap);
    }
    Ok(rows)
}

/// One-shot fire-and-forget convenience: records a log entry and
/// swallows + logs any error. Use this at fire-sites where audit
/// failure should never block the underlying action.
pub async fn record_or_warn(
    pool: &rustango::sql::Pool,
    page_id: i64,
    action: &str,
    actor_id: Option<i64>,
    message: impl Into<String>,
    data: Option<serde_json::Value>,
) {
    let msg = message.into();
    if let Err(e) = record(pool, page_id, action, actor_id, msg.clone(), data).await {
        tracing::warn!(
            target: "rustango_cms::page_log",
            page_id, action, error = %e,
            "audit log write failed (continuing)"
        );
    }
}
