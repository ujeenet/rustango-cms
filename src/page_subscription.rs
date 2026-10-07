//! Per-user opt-in subscriptions to page publish events.
//!
//! Editors / stakeholders subscribe to pages they care about and
//! receive an email when the page transitions into Published. Fires
//! from the same three publish paths the `after_publish_page` hook
//! does (single page edit, bulk publish, workflow auto-publish).
//!
//! Notification delivery is best-effort and depends on the host
//! having wired a mailer via `router_with_mailer` / `with_mailer_from`.
//! Hosts that haven't wired one log a debug line and skip — no
//! invariants broken.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_page_subscription",
    app = "cms",
    display = "id",
    admin(
        list_display = "page_id, user_id, comment_notifications, created_at",
        ordering = "page_id, user_id",
    )
)]
pub struct PageSubscription {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    #[rustango(fk = "rustango_users", on = "id", index)]
    pub user_id: i64,

    /// Bonus track — when set, also notify on new comments.
    /// Defaults to false so subscriptions stay scoped to publish
    /// events; opt-in to broader notifications explicitly.
    #[rustango(default = "false")]
    pub comment_notifications: bool,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Whether `user_id` has an active subscription on `page_id`.
///
/// # Errors
/// Driver / query failures.
pub async fn is_subscribed(
    pool: &rustango::sql::Pool,
    page_id: i64,
    user_id: i64,
) -> Result<bool, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<PageSubscription> = PageSubscription::objects()
        .where_(PageSubscription::page_id.eq(page_id))
        .where_(PageSubscription::user_id.eq(user_id))
        .fetch(pool)
        .await?;
    Ok(!rows.is_empty())
}

/// Subscribe `user_id` to publish events for `page_id`. Idempotent —
/// re-subscribing does not insert a duplicate row.
///
/// # Errors
/// Driver / query failures.
pub async fn subscribe(
    pool: &rustango::sql::Pool,
    page_id: i64,
    user_id: i64,
) -> Result<PageSubscription, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let existing: Vec<PageSubscription> = PageSubscription::objects()
        .where_(PageSubscription::page_id.eq(page_id))
        .where_(PageSubscription::user_id.eq(user_id))
        .fetch(pool)
        .await?;
    if let Some(row) = existing.into_iter().next() {
        return Ok(row);
    }
    let mut row = PageSubscription {
        id: Auto::Unset,
        page_id,
        user_id,
        comment_notifications: false,
        created_at: Auto::Unset,
    };
    row.insert_pool(pool).await?;
    Ok(row)
}

/// Remove `user_id`'s subscription on `page_id`. No-op when no row
/// exists. Returns the number of rows deleted (0 or 1).
///
/// # Errors
/// Driver / query failures.
pub async fn unsubscribe(
    pool: &rustango::sql::Pool,
    page_id: i64,
    user_id: i64,
) -> Result<usize, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let existing: Vec<PageSubscription> = PageSubscription::objects()
        .where_(PageSubscription::page_id.eq(page_id))
        .where_(PageSubscription::user_id.eq(user_id))
        .fetch(pool)
        .await?;
    let mut removed = 0;
    for row in existing {
        row.delete_pool(pool).await?;
        removed += 1;
    }
    Ok(removed)
}

/// Every subscriber of `page_id`. Used by the notification fan-out
/// when a page publishes.
///
/// # Errors
/// Driver / query failures.
pub async fn subscribers_for_page(
    pool: &rustango::sql::Pool,
    page_id: i64,
) -> Result<Vec<PageSubscription>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    PageSubscription::objects()
        .where_(PageSubscription::page_id.eq(page_id))
        .fetch(pool)
        .await
}

/// Compose the publish-notification subject + body for one page.
#[must_use]
pub fn render_publish_notification(
    page_title: &str,
    page_url: &str,
    actor_username: Option<&str>,
) -> (String, String) {
    let subject = format!("Published: {page_title}");
    let actor_line = actor_username
        .map(|u| format!("Published by: {u}\n"))
        .unwrap_or_default();
    let body = format!(
        "“{page_title}” just went live.\n\n\
         {actor_line}\
         View live: {page_url}\n\n\
         You're receiving this because you subscribed to publish notifications for this page in the CMS.\n\
         To stop receiving these, open the page editor and click \"Unsubscribe\" on the Status side panel.\n"
    );
    (subject, body)
}

/// Fan out a publish notification to every subscriber of `page_id`.
/// Best-effort — if the mailer is missing or any send fails, the
/// publish itself is unaffected.
pub async fn notify_publish(
    mailer: Option<&dyn rustango::email::Mailer>,
    from: &str,
    pool: &rustango::sql::Pool,
    tenant_slug: &str,
    page_id: i64,
    page_title: &str,
    page_url: &str,
    actor_username: Option<&str>,
) {
    let Some(mailer) = mailer else {
        return;
    };
    let subs = match subscribers_for_page(pool, page_id).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                target: "rustango_cms::page_subscription",
                page_id, error = %e,
                "subscribers lookup failed; skipping notify"
            );
            return;
        }
    };
    if subs.is_empty() {
        return;
    }
    // #197 — drop subscribers who have muted the publish category
    // before resolving emails. Default-enabled, so the first-touch
    // user still gets a notification.
    let sub_user_ids: Vec<i64> = subs.iter().map(|s| s.user_id).collect();
    let allowed: std::collections::HashSet<i64> = crate::notification_pref::filter_enabled(
        pool,
        &sub_user_ids,
        crate::notification_pref::PAGE_PUBLISHED_SUBSCRIBED,
    )
    .await
    .map(|v| v.into_iter().collect())
    .unwrap_or_else(|_| sub_user_ids.iter().copied().collect());
    let subs: Vec<_> = subs
        .into_iter()
        .filter(|s| allowed.contains(&s.user_id))
        .collect();
    if subs.is_empty() {
        return;
    }
    // Skip notifying the actor (they don't need an email about
    // their own publish). Match by user id; fall back to no-skip
    // when actor isn't identifiable.
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(subs.len());
    for sub in subs {
        match crate::workflow_mail::user_email(pool, sub.user_id, tenant_slug).await {
            Ok(Some(pair)) => pairs.push(pair),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::page_subscription",
                    user_id = sub.user_id, error = %e,
                    "subscriber email lookup failed; skipping",
                );
            }
        }
    }
    if let Some(actor) = actor_username {
        pairs.retain(|(username, _)| username != actor);
    }
    if pairs.is_empty() {
        return;
    }
    let (subject, body) = render_publish_notification(page_title, page_url, actor_username);
    crate::workflow_mail::notify_many(mailer, from, &pairs, &subject, &body).await;
}

/// Compose the subject + body for the pre-publish reminder.
/// Mirrors `render_publish_notification` but flags that the publish
/// hasn't happened yet so subscribers can intervene.
#[must_use]
pub fn render_pre_publish_notification(
    page_title: &str,
    page_url: &str,
    go_live_at: chrono::DateTime<chrono::Utc>,
) -> (String, String) {
    let subject = format!("Publishing soon: {page_title}");
    let body = format!(
        "Heads up — “{page_title}” is scheduled to publish at {go_live_at}.\n\n         \
         View live (once published): {page_url}\n\n         \
         You're receiving this because you subscribed to publish notifications for this page in the CMS.\n         \
         To stop receiving these, open the page editor and click \"Unsubscribe\" on the Status side panel.\n",
        page_title = page_title,
        go_live_at = go_live_at.to_rfc3339(),
        page_url = page_url,
    );
    (subject, body)
}

/// Fan out a pre-publish reminder. Called from the schedule
/// sweep when a page's `go_live_at` is within the next hour. Same
/// muting + actor-skip semantics as [`notify_publish`].
pub async fn notify_pre_publish(
    mailer: Option<&dyn rustango::email::Mailer>,
    from: &str,
    pool: &rustango::sql::Pool,
    tenant_slug: &str,
    page_id: i64,
    page_title: &str,
    page_url: &str,
    go_live_at: chrono::DateTime<chrono::Utc>,
) {
    let Some(mailer) = mailer else {
        return;
    };
    let subs = match subscribers_for_page(pool, page_id).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                target: "rustango_cms::page_subscription",
                page_id, error = %e,
                "subscribers lookup failed; skipping pre-publish notify"
            );
            return;
        }
    };
    if subs.is_empty() {
        return;
    }
    let sub_user_ids: Vec<i64> = subs.iter().map(|s| s.user_id).collect();
    let allowed: std::collections::HashSet<i64> = crate::notification_pref::filter_enabled(
        pool,
        &sub_user_ids,
        crate::notification_pref::PAGE_PUBLISHED_SUBSCRIBED,
    )
    .await
    .map(|v| v.into_iter().collect())
    .unwrap_or_else(|_| sub_user_ids.iter().copied().collect());
    let subs: Vec<_> = subs
        .into_iter()
        .filter(|s| allowed.contains(&s.user_id))
        .collect();
    if subs.is_empty() {
        return;
    }
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(subs.len());
    for sub in subs {
        match crate::workflow_mail::user_email(pool, sub.user_id, tenant_slug).await {
            Ok(Some(pair)) => pairs.push(pair),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::page_subscription",
                    user_id = sub.user_id, error = %e,
                    "subscriber email lookup failed; skipping",
                );
            }
        }
    }
    if pairs.is_empty() {
        return;
    }
    let (subject, body) = render_pre_publish_notification(page_title, page_url, go_live_at);
    crate::workflow_mail::notify_many(mailer, from, &pairs, &subject, &body).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_includes_page_title() {
        let (subject, _) = render_publish_notification("Spring sale", "https://x/", None);
        assert!(subject.contains("Spring sale"));
        assert!(subject.starts_with("Published:"));
    }

    #[test]
    fn body_mentions_actor_when_present() {
        let (_, body) = render_publish_notification("X", "https://y/", Some("alice"));
        assert!(body.contains("Published by: alice"));
    }

    #[test]
    fn body_skips_actor_line_when_absent() {
        let (_, body) = render_publish_notification("X", "https://y/", None);
        assert!(!body.contains("Published by:"));
    }

    #[test]
    fn body_includes_unsubscribe_hint() {
        let (_, body) = render_publish_notification("X", "https://y/", None);
        assert!(body.contains("Unsubscribe"));
    }
}
