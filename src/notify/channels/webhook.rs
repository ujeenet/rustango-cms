//! Generic webhook — the escape hatch.

use crate::notify::{ConfigField, DeliverError, Delivery, Notification, NotificationChannel};

/// POSTs the notification as JSON to any URL.
///
/// This is what makes "or any other tool" true without new code: Zapier,
/// n8n, Discord (with `/slack` appended), a PagerDuty Events endpoint, or
/// something internal. The body is the [`Notification`] itself, so
/// consumers get the structure rather than a rendered string.
#[derive(Default)]
pub struct WebhookChannel;

#[async_trait::async_trait]
impl NotificationChannel for WebhookChannel {
    fn kind(&self) -> &'static str {
        "webhook"
    }
    fn label(&self) -> &'static str {
        "Webhook (JSON)"
    }
    fn help(&self) -> &'static str {
        "POSTs the notification as JSON. Use this for Zapier, n8n, Discord, or your own endpoint."
    }
    fn config_fields(&self) -> Vec<ConfigField> {
        vec![ConfigField {
            name: "url",
            label: "Endpoint URL",
            help: "Receives a JSON POST.",
            // Not secret by default — a plain endpoint often is not one —
            // but the shared-secret header below is.
            secret: false,
            required: true,
        }]
    }

    async fn deliver(&self, d: &Delivery<'_>, msg: &Notification) -> Result<(), DeliverError> {
        let Some(url) = d.config.get("url").filter(|s| !s.trim().is_empty()) else {
            return Err(DeliverError::Permanent("no endpoint URL set".to_owned()));
        };
        let payload = serde_json::to_value(msg)
            .map_err(|e| DeliverError::Permanent(format!("serialize: {e}")))?;
        let mut req = super::client_for(url).await?
            .post(url.as_str())
            .json(&payload)
            .map_err(|e| DeliverError::Permanent(format!("encode: {e}")))?;
        // Lets the receiver verify the call came from this CMS. Optional:
        // an internal endpoint behind a VPN does not need one.
        if !d.secret.is_empty() {
            req = req
                .header("X-Rustango-Cms-Token", d.secret)
                .map_err(|e| DeliverError::Permanent(format!("header: {e}")))?;
        }
        let resp = req
            .send()
            .await
            .map_err(|e| DeliverError::Transient(e.to_string()))?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(super::classify(status, &body))
    }
}

crate::register_notification_channel!(WebhookChannel);
