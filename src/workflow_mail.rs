//! Workflow notification helpers (#85).
//!
//! Sends email on every workflow state change so reviewers know
//! when something is waiting on them and submitters know how their
//! submission was handled. Built on top of the framework's
//! [`rustango::email::Mailer`] trait — the host wires a backend
//! (`ConsoleMailer`, an SMTP one, an SES one); when no mailer is
//! attached, this module is a no-op.
//!
//! ## Notifications
//!
//! Sent automatically from the page-editor workflow handlers
//! (#84):
//!   * **submit**  — notify members of the FIRST task's role
//!   * **approve** to next step — notify members of the NEW current task's role
//!   * **approve** to finish — notify the submitter
//!   * **reject**  — notify the submitter
//!   * **cancel**  — notify the submitter (idempotent — they may
//!                   have triggered the cancel themselves)
//!
//! Failures during send are logged + swallowed; the workflow
//! transition completes regardless. Notifications are
//! "best-effort delivery" — production deployments should layer
//! their own retry/queue between this module and the network.

use rustango::email::{Email, Mailer};

/// Convenience: send to a list of recipients with a shared subject
/// + plain-text body. Each `recipients` entry is `(username,
/// email_address)`. Empty list is a no-op. Per-recipient errors
/// are logged but don't abort the loop.
pub async fn notify_many(
    mailer: &dyn Mailer,
    from: &str,
    recipients: &[(String, String)],
    subject: &str,
    body: &str,
) {
    for (username, addr) in recipients {
        if addr.trim().is_empty() {
            continue;
        }
        let email = Email::new()
            .from(from.to_owned())
            .to(addr.to_owned())
            .subject(subject.to_owned())
            .body(body.to_owned())
            .header(
                "X-Rustango-Cms-Notification".to_owned(),
                "workflow".to_owned(),
            );
        if let Err(e) = mailer.send(&email).await {
            tracing::warn!(
                target: "rustango_cms::workflow_mail",
                error = %e,
                recipient_username = %username,
                recipient_email = %addr,
                "notification send failed (continuing)"
            );
        } else {
            tracing::debug!(
                target: "rustango_cms::workflow_mail",
                recipient_username = %username,
                recipient_email = %addr,
                "workflow notification sent"
            );
        }
    }
}

/// User ids of every active member of a role. The notification
/// preference filter (#197) operates on user_ids before email
/// resolution so we don't synthesise fallback addresses for users
/// who have muted the event anyway.
///
/// # Errors
/// Driver / query failures.
pub async fn role_member_user_ids(
    pool: &rustango::sql::Pool,
    role_id: i64,
) -> Result<Vec<i64>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let memberships: Vec<rustango::tenancy::permissions::UserRole> =
        rustango::tenancy::permissions::UserRole::objects()
            .where_(rustango::tenancy::permissions::UserRole::role_id.eq(role_id))
            .fetch(pool)
            .await?;
    let user_ids: Vec<i64> = memberships.iter().map(|m| m.user_id).collect();
    if user_ids.is_empty() {
        return Ok(Vec::new());
    }
    let users: Vec<rustango::tenancy::auth::User> = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.is_in(user_ids))
        .where_(rustango::tenancy::auth::User::active.eq(true))
        .fetch(pool)
        .await?;
    Ok(users
        .into_iter()
        .filter_map(|u| u.id.get().copied())
        .collect())
}

/// Resolve `(username, email)` pairs for every member of a role.
/// Used for "notify members of the current task's role" — the
/// reviewer routing.
///
/// ## Email source
///
/// The framework's `rustango_users` table (today) doesn't carry an
/// `email` column. As a transitional measure, we look for a
/// per-user `data.email` JSON entry (set by the host app when
/// provisioning users); when absent, falls back to
/// `<username>@<tenant_slug>.invalid` so logs surface who *would*
/// have been notified. Once the framework adds a first-class email
/// column, swap the lookup to the column and delete this fallback.
///
/// # Errors
/// Driver / query failures.
pub async fn role_member_emails(
    pool: &rustango::sql::Pool,
    role_id: i64,
    tenant_slug: &str,
) -> Result<Vec<(String, String)>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let memberships: Vec<rustango::tenancy::permissions::UserRole> =
        rustango::tenancy::permissions::UserRole::objects()
            .where_(rustango::tenancy::permissions::UserRole::role_id.eq(role_id))
            .fetch(pool)
            .await?;
    let user_ids: Vec<i64> = memberships.iter().map(|m| m.user_id).collect();
    if user_ids.is_empty() {
        return Ok(Vec::new());
    }
    let users: Vec<rustango::tenancy::auth::User> = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.is_in(user_ids))
        .where_(rustango::tenancy::auth::User::active.eq(true))
        .fetch(pool)
        .await?;
    Ok(users
        .into_iter()
        .map(|u| (u.username.clone(), email_for(&u, tenant_slug)))
        .collect())
}

/// Resolve the `(username, email)` pair for a single user.
///
/// # Errors
/// Driver / query failures.
pub async fn user_email(
    pool: &rustango::sql::Pool,
    user_id: i64,
    tenant_slug: &str,
) -> Result<Option<(String, String)>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<rustango::tenancy::auth::User> = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.eq(user_id))
        .fetch(pool)
        .await?;
    Ok(rows
        .into_iter()
        .next()
        .map(|u| (u.username.clone(), email_for(&u, tenant_slug))))
}

/// Look up an email for a user: first the `data.email` JSON field
/// (per-user prefs), then a synthetic
/// `<username>@<tenant_slug>.invalid` fallback that surfaces in
/// the mailer's logs but isn't a real address.
fn email_for(user: &rustango::tenancy::auth::User, tenant_slug: &str) -> String {
    if let Some(email) = user.data.get("email").and_then(|v| v.as_str()) {
        if !email.trim().is_empty() {
            return email.to_owned();
        }
    }
    format!("{}@{}.invalid", user.username, tenant_slug)
}

/// Compose the standard subject + body for a workflow event.
/// Format kept deliberately plain — production deployments can
/// override by replacing the call site with a Tera-rendered body.
#[must_use]
pub fn render_notification(
    event: &str,
    workflow_name: &str,
    page_title: &str,
    page_id: i64,
    task_name: Option<&str>,
    actor_username: Option<&str>,
    comment: Option<&str>,
) -> (String, String) {
    let subject = match event {
        "submit" => format!("[{workflow_name}] Review requested: {page_title}"),
        "approve_advance" => format!(
            "[{workflow_name}] Approved on “{task}” — moves to you for next step: {page_title}",
            task = task_name.unwrap_or("step")
        ),
        "approve_finish" => format!("[{workflow_name}] Approved — {page_title} is now published"),
        "reject" => format!("[{workflow_name}] Changes requested on {page_title}"),
        "cancel" => format!("[{workflow_name}] Review cancelled: {page_title}"),
        _ => format!("[{workflow_name}] {event}: {page_title}"),
    };

    let mut body = String::with_capacity(256);
    body.push_str(&format!("Page: {page_title}\n"));
    body.push_str(&format!("Workflow: {workflow_name}\n"));
    if let Some(t) = task_name {
        body.push_str(&format!("Task: {t}\n"));
    }
    if let Some(actor) = actor_username {
        body.push_str(&format!("Actor: {actor}\n"));
    }
    body.push('\n');
    let summary = match event {
        "submit" => "A page has been submitted for your review. Open the editor to approve or request changes.",
        "approve_advance" => "The current step has been approved. The page is now waiting on your review.",
        "approve_finish" => "The page has been approved by every reviewer and is now published.",
        "reject" => "A reviewer has requested changes. Edit the page and resubmit when ready.",
        "cancel" => "The review has been cancelled. The page returns to its current status.",
        other => other,
    };
    body.push_str(summary);
    body.push_str("\n\n");
    if let Some(c) = comment {
        if !c.trim().is_empty() {
            body.push_str("Reviewer comment:\n");
            body.push_str(c);
            body.push_str("\n\n");
        }
    }
    body.push_str(&format!("Edit: /cms-admin/pages/{page_id}/edit\n"));
    (subject, body)
}
