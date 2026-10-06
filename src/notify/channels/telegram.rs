//! Telegram — Bot API `sendMessage`.

use crate::notify::{ConfigField, DeliverError, Delivery, Notification, NotificationChannel};
use serde_json::json;

/// Sends via a bot to one chat.
///
/// Unlike the webhook channels the destination is two parts — the bot
/// token (secret) and the chat id (not) — because one bot serves many
/// chats and only the token is a credential.
#[derive(Default)]
pub struct TelegramChannel;

#[async_trait::async_trait]
impl NotificationChannel for TelegramChannel {
    fn kind(&self) -> &'static str {
        "telegram"
    }
    fn label(&self) -> &'static str {
        "Telegram"
    }
    fn help(&self) -> &'static str {
        "Create a bot with @BotFather for the token, then add it to the chat and use that chat's id."
    }
    fn config_fields(&self) -> Vec<ConfigField> {
        vec![
            ConfigField {
                name: "chat_id",
                label: "Chat ID",
                help: "e.g. -1001234567890 for a group, or a numeric user id.",
                secret: false,
                required: true,
            },
            ConfigField {
                name: "bot_token",
                label: "Bot token",
                help: "From @BotFather, e.g. 123456:ABC-DEF…",
                secret: true,
                required: true,
            },
        ]
    }

    async fn deliver(&self, d: &Delivery<'_>, msg: &Notification) -> Result<(), DeliverError> {
        if d.secret.is_empty() {
            return Err(DeliverError::Permanent("no bot token set".to_owned()));
        }
        let Some(chat_id) = d.config.get("chat_id").filter(|s| !s.trim().is_empty()) else {
            return Err(DeliverError::Permanent("no chat id set".to_owned()));
        };
        // HTML rather than MarkdownV2: Markdown requires escaping some
        // eighteen characters, and unescaped user content in a form answer
        // would make the API reject the whole message. HTML needs three.
        let mut text = format!("<b>{}</b>\n", esc(&msg.title));
        if !msg.summary.is_empty() {
            text.push_str(&format!("{}\n", esc(&msg.summary)));
        }
        for (k, v) in &msg.fields {
            text.push_str(&format!("\n<b>{}</b>: {}", esc(k), esc(v)));
        }
        if !msg.url.is_empty() {
            text.push_str(&format!("\n\n{}", esc(&msg.url)));
        }
        // Telegram rejects anything over 4096 characters outright.
        let text = super::slack::truncate(&text, 4000);
        let url = format!("https://api.telegram.org/bot{}/sendMessage", d.secret);
        let payload = json!({ "chat_id": chat_id, "text": text, "parse_mode": "HTML",
                              "disable_web_page_preview": true });
        super::slack::post_json(&url, &payload).await
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

crate::register_notification_channel!(TelegramChannel);
