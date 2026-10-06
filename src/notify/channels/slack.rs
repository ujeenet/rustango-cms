//! Slack — incoming webhook.

use crate::notify::{ConfigField, DeliverError, Delivery, Notification, NotificationChannel};
use serde_json::json;

/// Posts to a Slack incoming webhook using Block Kit, so fields render as
/// a readable list rather than one run-on line.
#[derive(Default)]
pub struct SlackChannel;

#[async_trait::async_trait]
impl NotificationChannel for SlackChannel {
    fn kind(&self) -> &'static str {
        "slack"
    }
    fn label(&self) -> &'static str {
        "Slack"
    }
    fn help(&self) -> &'static str {
        "Create an Incoming Webhook in your Slack app settings and paste its URL."
    }
    fn config_fields(&self) -> Vec<ConfigField> {
        vec![ConfigField {
            name: "webhook_url",
            label: "Webhook URL",
            help: "https://hooks.slack.com/services/…",
            // The URL *is* the credential — anyone holding it can post to
            // the channel — so it lives in the encrypted column.
            secret: true,
            required: true,
        }]
    }

    async fn deliver(&self, d: &Delivery<'_>, msg: &Notification) -> Result<(), DeliverError> {
        if d.secret.is_empty() {
            return Err(DeliverError::Permanent("no webhook URL set".to_owned()));
        }
        let mut blocks = vec![json!({
            "type": "header",
            "text": { "type": "plain_text", "text": truncate(&msg.title, 150), "emoji": true }
        })];
        if !msg.summary.is_empty() {
            blocks.push(json!({
                "type": "section",
                "text": { "type": "mrkdwn", "text": truncate(&msg.summary, 2900) }
            }));
        }
        // Slack caps a section at 10 fields; chunk rather than truncate so
        // a long form still delivers every answer.
        for chunk in msg.fields.chunks(10) {
            blocks.push(json!({
                "type": "section",
                "fields": chunk.iter().map(|(k, v)| json!({
                    "type": "mrkdwn",
                    "text": format!("*{}*\n{}", escape(k), escape(&truncate(v, 1900))),
                })).collect::<Vec<_>>(),
            }));
        }
        if !msg.url.is_empty() {
            blocks.push(json!({
                "type": "actions",
                "elements": [{
                    "type": "button",
                    "text": { "type": "plain_text", "text": "Open" },
                    "url": msg.url,
                }],
            }));
        }
        let payload = json!({ "text": msg.title, "blocks": blocks });
        post_json(d.secret, &payload).await
    }
}

/// Slack's mrkdwn needs only these three escaped.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

/// Shared by Slack + Teams + generic webhook: POST JSON, classify status.
pub(crate) async fn post_json(
    url: &str,
    payload: &serde_json::Value,
) -> Result<(), DeliverError> {
    let resp = super::client_for(url).await?
        .post(url)
        .json(payload)
        .map_err(|e| DeliverError::Permanent(format!("encode: {e}")))?
        .send()
        .await
        // A DNS blip or refused connection is exactly what the outbox is
        // for; never permanent.
        .map_err(|e| DeliverError::Transient(e.to_string()))?;
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(super::classify(status, &body))
}

crate::register_notification_channel!(SlackChannel);
