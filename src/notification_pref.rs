//! Per-user notification preferences.
//!
//! Editors opt out of categories of notifications they don't want
//! to receive. Every fire-site (`workflow_mail`, `page_subscription`,
//! comment replies) checks [`is_enabled`] before delivering so the
//! mailer doesn't burn cycles building emails for unsubscribed users.
//!
//! Missing rows are treated as **enabled** — first-touch users get
//! every notification until they explicitly disable a category. This
//! matches the principle of least surprise: a new editor hears about
//! everything, then trims.

use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// Every category an editor can mute. String keys (not an enum)
/// because the set is editor-facing and host plugins may
/// register their own kinds via the same table.
pub const WORKFLOW_SUBMITTED: &str = "workflow_submitted";
pub const WORKFLOW_APPROVED: &str = "workflow_approved";
pub const WORKFLOW_REJECTED: &str = "workflow_rejected";
pub const COMMENT_REPLIED: &str = "comment_replied";
pub const COMMENT_RESOLVED: &str = "comment_resolved";
pub const PAGE_PUBLISHED_SUBSCRIBED: &str = "page_published_subscribed";

/// The full canonical list — rendered in order on the preferences
/// page. (label, kind, hint) for each row.
pub const ALL_KINDS: &[(&str, &str, &str)] = &[
    (
        "Workflow: review requested",
        WORKFLOW_SUBMITTED,
        "A page enters a workflow step that you can approve / reject.",
    ),
    (
        "Workflow: approved",
        WORKFLOW_APPROVED,
        "A page you submitted has been approved and published.",
    ),
    (
        "Workflow: rejected / cancelled",
        WORKFLOW_REJECTED,
        "A reviewer requested changes or the workflow was cancelled.",
    ),
    (
        "Comments: replies",
        COMMENT_REPLIED,
        "Someone replied to a comment thread you participate in.",
    ),
    (
        "Comments: resolved",
        COMMENT_RESOLVED,
        "A comment thread you participate in was marked resolved.",
    ),
    (
        "Page subscriptions",
        PAGE_PUBLISHED_SUBSCRIBED,
        "A page you subscribed to via the page editor is published.",
    ),
];

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_notification_pref",
    app = "cms",
    display = "kind",
    admin(list_display = "user_id, kind, enabled", ordering = "user_id, kind",)
)]
pub struct NotificationPref {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "rustango_users", on = "id", index)]
    pub user_id: i64,

    #[rustango(max_length = 64, index)]
    pub kind: String,

    pub enabled: bool,
}

/// Whether `user_id` wants to receive notifications of `kind`. A
/// missing row is treated as `true` — opt-out semantics.
///
/// # Errors
/// Driver / query failures.
pub async fn is_enabled(
    pool: &rustango::sql::Pool,
    user_id: i64,
    kind: &str,
) -> Result<bool, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<NotificationPref> = NotificationPref::objects()
        .where_(NotificationPref::user_id.eq(user_id))
        .where_(NotificationPref::kind.eq(kind.to_owned()))
        .fetch(pool)
        .await?;
    Ok(rows.into_iter().next().is_none_or(|r| r.enabled))
}

/// Upsert the toggle for `(user_id, kind)`.
///
/// # Errors
/// Driver / query failures.
pub async fn set_enabled(
    pool: &rustango::sql::Pool,
    user_id: i64,
    kind: &str,
    enabled: bool,
) -> Result<(), rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<NotificationPref> = NotificationPref::objects()
        .where_(NotificationPref::user_id.eq(user_id))
        .where_(NotificationPref::kind.eq(kind.to_owned()))
        .fetch(pool)
        .await?;
    if let Some(mut row) = rows.into_iter().next() {
        row.enabled = enabled;
        row.save_pool(pool).await?;
    } else {
        let mut row = NotificationPref {
            id: Auto::Unset,
            user_id,
            kind: kind.to_owned(),
            enabled,
        };
        row.insert_pool(pool).await?;
    }
    Ok(())
}

/// Every preference row for a user, keyed by kind.
///
/// # Errors
/// Driver / query failures.
pub async fn map_for_user(
    pool: &rustango::sql::Pool,
    user_id: i64,
) -> Result<std::collections::HashMap<String, bool>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<NotificationPref> = NotificationPref::objects()
        .where_(NotificationPref::user_id.eq(user_id))
        .fetch(pool)
        .await?;
    Ok(rows.into_iter().map(|r| (r.kind, r.enabled)).collect())
}

/// Filter `user_ids` to those who haven't opted out of `kind`.
/// Missing rows are kept (default = enabled). Used at fan-out sites
/// to skip email composition for muted users.
///
/// # Errors
/// Driver / query failures.
pub async fn filter_enabled(
    pool: &rustango::sql::Pool,
    user_ids: &[i64],
    kind: &str,
) -> Result<Vec<i64>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    if user_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<NotificationPref> = NotificationPref::objects()
        .where_(NotificationPref::user_id.is_in(user_ids.to_vec()))
        .where_(NotificationPref::kind.eq(kind.to_owned()))
        .fetch(pool)
        .await?;
    let disabled: std::collections::HashSet<i64> = rows
        .into_iter()
        .filter(|r| !r.enabled)
        .map(|r| r.user_id)
        .collect();
    Ok(user_ids
        .iter()
        .copied()
        .filter(|id| !disabled.contains(id))
        .collect())
}

/// Map a workflow event name to the preference kind a recipient
/// uses to mute it. Centralised so the routing stays consistent
/// across every workflow notification call site.
#[must_use]
pub fn pref_kind_for_workflow_event(event: &str) -> &'static str {
    match event {
        "submit" | "approve_advance" => WORKFLOW_SUBMITTED,
        "approve_finish" => WORKFLOW_APPROVED,
        "reject" | "cancel" => WORKFLOW_REJECTED,
        _ => WORKFLOW_SUBMITTED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pref_kind_routes_workflow_events() {
        assert_eq!(pref_kind_for_workflow_event("submit"), WORKFLOW_SUBMITTED);
        assert_eq!(
            pref_kind_for_workflow_event("approve_advance"),
            WORKFLOW_SUBMITTED
        );
        assert_eq!(
            pref_kind_for_workflow_event("approve_finish"),
            WORKFLOW_APPROVED
        );
        assert_eq!(pref_kind_for_workflow_event("reject"), WORKFLOW_REJECTED);
        assert_eq!(pref_kind_for_workflow_event("cancel"), WORKFLOW_REJECTED);
        assert_eq!(pref_kind_for_workflow_event("unknown"), WORKFLOW_SUBMITTED);
    }

    #[test]
    fn all_kinds_lists_six_categories() {
        assert_eq!(ALL_KINDS.len(), 6);
        let kinds: Vec<&str> = ALL_KINDS.iter().map(|(_, k, _)| *k).collect();
        assert!(kinds.contains(&WORKFLOW_SUBMITTED));
        assert!(kinds.contains(&PAGE_PUBLISHED_SUBSCRIBED));
    }
}
