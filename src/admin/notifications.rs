//! Admin for notification targets + the delivery log.
//!
//! One screen rather than a CRUD spread over four: a destination is a
//! handful of fields, and the thing an editor actually needs is to see
//! whether the last submissions got through. Splitting the log onto its own
//! page would hide the only part that answers "did it work?".

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use rustango::core::Column as _;
use rustango::sql::FetcherPool as _;
use rustango::extractors::{SessionUser, Tenant};
use serde::Deserialize;

use tera::Context;

use super::handlers::{add_admin_theme, add_chrome, render_with_csrf};
use super::AdminError;
use crate::notify::model::{NotificationDelivery, NotificationTarget, STATUS_FAILED};
use crate::notify::{channel_for, registered_channels};

/// Permission required to manage notification destinations.
///
/// Separate from page editing: a destination holds a credential and can
/// forward every submission off-site, which is a different kind of trust
/// from editing copy.
pub const CODENAME: &str = "cms_notification.manage";

async fn ensure_access(
    tenant: &Tenant,
    user: Option<&rustango::tenancy::auth::User>,
) -> Option<Response> {
    super::codename_gate(tenant, user, CODENAME).await
}

/// `GET /cms-admin/notifications`
pub(crate) async fn list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    SessionUser(session_user): SessionUser,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let targets: Vec<NotificationTarget> = NotificationTarget::objects()
        .order_by(&[("label", false)])
        .fetch(tenant.pool())
        .await?;
    let deliveries: Vec<NotificationDelivery> = NotificationDelivery::objects()
        .order_by(&[("created_at", true)])
        .limit(40)
        .fetch(tenant.pool())
        .await?;

    let by_id: std::collections::HashMap<i64, String> = targets
        .iter()
        .map(|t| (t.id.get().copied().unwrap_or_default(), t.label.clone()))
        .collect();

    let target_rows: Vec<serde_json::Value> = targets
        .iter()
        .map(|t| {
            let cfg = t.config_map();
            serde_json::json!({
                "id": t.id.get().copied().unwrap_or_default(),
                "label": t.label,
                "kind": t.kind,
                "kind_label": channel_for(&t.kind).map_or_else(
                    // A target whose channel this binary lacks must still be
                    // visible: hiding it would make a silently-dead
                    // destination invisible.
                    || format!("{} (no channel registered)", t.kind),
                    |c| c.label().to_owned(),
                ),
                "events": if t.events.trim().is_empty() { "all events".to_owned() } else { t.events.clone() },
                "enabled": t.enabled,
                // Managed rows come from a form's settings; editing them here
                // would be overwritten on the next form save.
                "managed": cfg.get("managed_by").is_some(),
                "summary": cfg.get("recipients").or_else(|| cfg.get("url")).or_else(|| cfg.get("chat_id")).cloned().unwrap_or_default(),
            })
        })
        .collect();

    let delivery_rows: Vec<serde_json::Value> = deliveries
        .iter()
        .map(|d| {
            serde_json::json!({
                "id": d.id.get().copied().unwrap_or_default(),
                "event": d.event,
                "target": by_id.get(&d.target_id).cloned()
                    .unwrap_or_else(|| format!("#{} (deleted)", d.target_id)),
                "status": d.status,
                "attempts": d.attempts,
                "last_error": d.last_error,
                "created_at": d.created_at.get().map(ToString::to_string).unwrap_or_default(),
                "retryable": d.status == STATUS_FAILED,
            })
        })
        .collect();

    let channels: Vec<serde_json::Value> = registered_channels()
        .map(|c| {
            serde_json::json!({
                "kind": c.kind(),
                "label": c.label(),
                "help": c.help(),
                "fields": c.config_fields().iter().map(|f| serde_json::json!({
                    "name": f.name, "label": f.label, "help": f.help,
                    "secret": f.secret, "required": f.required,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "notifications", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("targets", &target_rows);
    ctx.insert("deliveries", &delivery_rows);
    ctx.insert("channels", &channels);
    render_with_csrf(&state, &headers, "rcms_admin/notifications.html", &mut ctx)
}

#[derive(Deserialize)]
pub(crate) struct TargetForm {
    pub label: String,
    pub kind: String,
    #[serde(default)]
    pub events: String,
    #[serde(default)]
    pub secret: String,
    /// Every non-secret config field, flattened — the channel decides which
    /// keys matter, so the form cannot know them at compile time.
    #[serde(flatten)]
    pub rest: std::collections::HashMap<String, String>,
}

/// `POST /cms-admin/notifications/new`
pub(crate) async fn create(
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    Form(form): Form<TargetForm>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let Some(channel) = channel_for(&form.kind) else {
        return Err(AdminError::Validation(format!(
            "unknown channel `{}`",
            form.kind
        )));
    };
    // Only keys the channel declares — a stray form field must not end up
    // in stored config where it would look meaningful.
    let mut config = serde_json::Map::new();
    for f in channel.config_fields() {
        if f.secret {
            continue;
        }
        let v = form.rest.get(f.name).cloned().unwrap_or_default();
        if f.required && v.trim().is_empty() {
            return Err(AdminError::Validation(format!("{} is required", f.label)));
        }
        config.insert(f.name.to_owned(), serde_json::Value::String(v));
    }
    let mut row = NotificationTarget {
        id: rustango::sql::Auto::Unset,
        label: form.label.trim().to_owned(),
        kind: form.kind.clone(),
        config: serde_json::Value::Object(config).to_string(),
        secret: rustango::casts::Cast::new(form.secret.trim().to_owned().into()),
        events: form.events.trim().to_owned(),
        source_id: 0,
        enabled: true,
        created_at: rustango::sql::Auto::Unset,
    };
    row.insert_pool(tenant.pool()).await?;
    Ok(Redirect::to("/cms-admin/notifications").into_response())
}

/// `POST /cms-admin/notifications/{id}/toggle`
pub(crate) async fn toggle(
    tenant: Tenant,
    Path(id): Path<i64>,
    SessionUser(session_user): SessionUser,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    if let Some(mut t) = NotificationTarget::objects()
        .where_(NotificationTarget::id.eq(id))
        .first(tenant.pool())
        .await?
    {
        t.enabled = !t.enabled;
        t.save_pool(tenant.pool()).await?;
    }
    Ok(Redirect::to("/cms-admin/notifications").into_response())
}

/// `POST /cms-admin/notifications/{id}/delete`
pub(crate) async fn delete(
    tenant: Tenant,
    Path(id): Path<i64>,
    SessionUser(session_user): SessionUser,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    if let Some(t) = NotificationTarget::objects()
        .where_(NotificationTarget::id.eq(id))
        .first(tenant.pool())
        .await?
    {
        t.delete_pool(tenant.pool()).await?;
    }
    Ok(Redirect::to("/cms-admin/notifications").into_response())
}

/// `POST /cms-admin/notifications/deliveries/{id}/retry`
pub(crate) async fn retry_delivery(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    Path(id): Path<i64>,
    SessionUser(session_user): SessionUser,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    crate::notify::worker::retry(tenant.pool(), id).await?;
    // Send it now rather than leaving it for the next submission — someone
    // pressing Retry is watching for the result.
    crate::notify::worker::spawn_drain(
        tenant.pool().clone(),
        tenant.org.slug.clone(),
        state.mailer.clone(),
        state.mailer_from.as_str().to_owned(),
    );
    Ok(Redirect::to("/cms-admin/notifications").into_response())
}
