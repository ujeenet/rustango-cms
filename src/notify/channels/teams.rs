//! Microsoft Teams — incoming webhook (MessageCard).

use crate::notify::{ConfigField, DeliverError, Delivery, Notification, NotificationChannel};
use serde_json::json;

/// Posts a MessageCard to a Teams incoming webhook.
///
/// MessageCard rather than the newer Adaptive Card on purpose: connector
/// webhooks accept it everywhere, including the "Incoming Webhook"
/// connector most teams already have, whereas Adaptive Cards need a
/// Workflows/Power Automate endpoint. The older format is the one that
/// works without asking an admin for anything.
#[derive(Default)]
pub struct TeamsChannel;

#[async_trait::async_trait]
impl NotificationChannel for TeamsChannel {
    fn kind(&self) -> &'static str {
        "teams"
    }
    fn label(&self) -> &'static str {
        "Microsoft Teams"
    }
    fn help(&self) -> &'static str {
        "Add an Incoming Webhook connector to a Teams channel and paste its URL."
    }
    fn config_fields(&self) -> Vec<ConfigField> {
        vec![ConfigField {
            name: "webhook_url",
            label: "Webhook URL",
            help: "https://….webhook.office.com/webhookb2/…",
            secret: true,
            required: true,
        }]
    }

    async fn deliver(&self, d: &Delivery<'_>, msg: &Notification) -> Result<(), DeliverError> {
        if d.secret.is_empty() {
            return Err(DeliverError::Permanent("no webhook URL set".to_owned()));
        }
        let facts: Vec<serde_json::Value> = msg
            .fields
            .iter()
            .map(|(k, v)| json!({ "name": k, "value": super::slack::truncate(v, 1000) }))
            .collect();
        let mut card = json!({
            "@type": "MessageCard",
            "@context": "https://schema.org/extensions",
            "summary": msg.title,
            "themeColor": "4A6D9E",
            "title": msg.title,
            "sections": [{ "text": msg.summary, "facts": facts }],
        });
        if !msg.url.is_empty() {
            card["potentialAction"] = json!([{
                "@type": "OpenUri",
                "name": "Open",
                "targets": [{ "os": "default", "uri": msg.url }],
            }]);
        }
        super::slack::post_json(d.secret, &card).await
    }
}

crate::register_notification_channel!(TeamsChannel);
