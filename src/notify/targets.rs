//! Keeping a form's `notify_emails` setting and the target table in step.

use rustango::core::Column as _;
use rustango::sql::{ExecError, FetcherPool as _, Pool};

use super::model::NotificationTarget;

/// Marks a target this code owns, so a sync can rewrite it without
/// clobbering one an editor made by hand.
pub const MANAGED_BY_FORM: &str = "form_settings";

/// Mirror a form's `notify_emails` into a real target.
///
/// Form settings had their own recipient list and their own send loop. Rather
/// than keep a second notification path alive beside the new one — with its
/// own bugs and no retries — the setting becomes an ordinary email target
/// scoped to that form. It then gains the outbox, the delivery log and the
/// retry button for free, and shows up in the admin next to Slack and Teams
/// instead of being invisible unless you open the form.
///
/// Idempotent: creates, updates, or removes the managed target to match
/// `emails`. Hand-made targets for the same form are untouched.
///
/// # Errors
/// Driver / query failures.
pub async fn sync_form_email(
    pool: &Pool,
    form_id: i64,
    form_title: &str,
    emails: &str,
) -> Result<(), ExecError> {
    let existing: Vec<NotificationTarget> = NotificationTarget::objects()
        .where_(NotificationTarget::source_id.eq(form_id))
        .where_(NotificationTarget::kind.eq("email".to_owned()))
        .fetch(pool)
        .await?;
    let mut managed = existing.into_iter().find(|t| {
        t.config_map().get("managed_by").map(String::as_str) == Some(MANAGED_BY_FORM)
    });

    let clean: Vec<&str> = emails
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if clean.is_empty() {
        // Cleared in the form editor → the target goes too, otherwise
        // turning notifications off in one place leaves them on in another.
        if let Some(t) = managed {
            t.delete_pool(pool).await?;
        }
        return Ok(());
    }

    // Writing a target encrypts its `secret` column (empty here, but still
    // encrypted), and the framework's cast panics without the key. Say so
    // instead: this runs on every boot, so a panic would stop the site from
    // starting the moment an editor typed a notification address.
    if std::env::var_os("RUSTANGO_SECRET_KEY").is_none() {
        return Err(ExecError::Driver(rustango::sql::sqlx::Error::Configuration(
            "RUSTANGO_SECRET_KEY is not set, so email notifications can't be stored".into(),
        )));
    }

    let config = serde_json::json!({
        "recipients": clean.join(", "),
        "managed_by": MANAGED_BY_FORM,
    })
    .to_string();
    let label = format!("{form_title} — email");

    match managed.take() {
        Some(mut t) => {
            t.label = label;
            t.config = config;
            t.events = "form.submitted".to_owned();
            t.enabled = true;
            t.save_pool(pool).await?;
        }
        None => {
            let mut row = NotificationTarget {
                id: rustango::sql::Auto::Unset,
                label,
                kind: "email".to_owned(),
                config,
                // No secret: the mailer holds the credentials, not the target.
                secret: rustango::casts::Cast::new(String::new().into()),
                events: "form.submitted".to_owned(),
                source_id: form_id,
                enabled: true,
                created_at: rustango::sql::Auto::Unset,
            };
            row.insert_pool(pool).await?;
        }
    }
    Ok(())
}
