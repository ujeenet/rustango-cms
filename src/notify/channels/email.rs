//! Email — the destination that already existed, behind the trait.

use crate::notify::{ConfigField, DeliverError, Delivery, Notification, NotificationChannel};

/// Sends through the tenant's configured mailer. Recipients are a
/// comma-separated list, matching what form settings already collected, so
/// an existing `notify_emails` value migrates as-is.
#[derive(Default)]
pub struct EmailChannel;

#[async_trait::async_trait]
impl NotificationChannel for EmailChannel {
    fn kind(&self) -> &'static str {
        "email"
    }
    fn label(&self) -> &'static str {
        "Email"
    }
    fn help(&self) -> &'static str {
        "Sends through this site's configured mailer. Needs a mailer wired by the host app."
    }
    fn config_fields(&self) -> Vec<ConfigField> {
        vec![ConfigField {
            name: "recipients",
            label: "Recipients",
            help: "Comma-separated addresses.",
            secret: false,
            required: true,
        }]
    }

    async fn deliver(&self, d: &Delivery<'_>, msg: &Notification) -> Result<(), DeliverError> {
        let Some(mailer) = d.mailer else {
            // Not transient: no amount of retrying wires a mailer. Failing
            // fast puts "no mailer configured" in the delivery log, which
            // is the actual fix.
            return Err(DeliverError::Permanent(
                "no mailer configured on this site".to_owned(),
            ));
        };
        let recipients: Vec<&str> = d
            .config
            .get("recipients")
            .map(String::as_str)
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if recipients.is_empty() {
            return Err(DeliverError::Permanent("no recipients set".to_owned()));
        }
        let body = msg.as_text();
        let mut last_err: Option<String> = None;
        for to in recipients {
            let email = rustango::email::Email::new()
                .to(to.to_owned())
                .from(d.mailer_from.to_owned())
                .subject(msg.title.clone())
                .body(body.clone())
                .header(
                    "X-Rustango-Cms-Notification".to_owned(),
                    msg.event.clone(),
                );
            if let Err(e) = email.send(mailer).await {
                last_err = Some(e.to_string());
            }
        }
        // One bad address should not discard the whole delivery, but the
        // row must not claim success either — surface it as transient so
        // the retry reaches the addresses that do work.
        match last_err {
            None => Ok(()),
            Some(e) => Err(DeliverError::Transient(e)),
        }
    }
}

crate::register_notification_channel!(EmailChannel);
