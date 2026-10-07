//! Notifications — one delivery path, many destinations.
//!
//! Before this, every notification in the CMS was an email loop written
//! where it was needed: form submissions built theirs inline in the submit
//! handler, page publishes built theirs in `page_subscription`, workflow
//! events in `workflow_mail`. Adding Slack meant writing a fourth loop, and
//! adding Teams a fifth — per event, forever.
//!
//! So the destination is a plugin, not a branch:
//!
//! * [`Notification`] is what happened, in channel-agnostic terms — a
//!   title, a summary, some labelled fields, maybe a link. It never knows
//!   who will render it.
//! * [`NotificationChannel`] renders and delivers one. Email writes a
//!   message, Slack posts blocks, Telegram sends Markdown. Register one
//!   with [`register_notification_channel!`](crate::register_notification_channel!) and it is available
//!   everywhere, including from a host app's own crate.
//! * A [`Target`](model::NotificationTarget) row is a configured
//!   destination: which channel, its settings, its secret, and which
//!   events it wants.
//!
//! ## Delivery is durable on purpose
//!
//! [`enqueue`](worker::enqueue) writes a row per target and returns; a worker
//! ([`worker::drain`]) sends with backoff. Notifications are the part of a
//! form nobody watches until it silently stops working, and the failure
//! modes here are all *transient and external* — a rate limit, an expired
//! webhook, a five-minute Slack outage. Sending inline would mean the
//! visitor waits on Slack, and a failure would be a log line nobody reads
//! while the lead is gone. A row that says `failed, 3 attempts, 429` is
//! recoverable; a warning in yesterday's journal is not.

pub mod channels;
pub mod model;
pub mod targets;
pub mod worker;

/// Event keys. Constants rather than literals so a typo at the emit site
/// cannot silently produce an event no target subscribes to.
pub mod events {
    /// A visitor submitted a form. `source_id` is the form's snippet id.
    pub const FORM_SUBMITTED: &str = "form.submitted";
}

use std::collections::BTreeMap;

/// What happened, in terms every channel can render.
///
/// Deliberately not a string: a channel that only gets pre-formatted text
/// can do nothing intelligent with it. Slack wants fields it can lay out,
/// email wants a subject distinct from a body, and a webhook wants the
/// structure back as JSON. Keeping the parts separate lets each decide.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Notification {
    /// Event key this arose from (`"form.submitted"`), matched against a
    /// target's subscriptions.
    pub event: String,
    /// One line: "New submission: Contact form".
    pub title: String,
    /// Optional prose under the title.
    #[serde(default)]
    pub summary: String,
    /// Ordered label → value pairs. `BTreeMap` would sort them; a form's
    /// fields must arrive in the order the form asks them.
    #[serde(default)]
    pub fields: Vec<(String, String)>,
    /// Where to go to act on this — an admin URL, absolute or relative.
    #[serde(default)]
    pub url: String,
    /// Free-form extras for channels that want the raw shape (webhooks).
    #[serde(default)]
    pub meta: BTreeMap<String, String>,
}

impl Notification {
    #[must_use]
    pub fn new(event: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            event: event.into(),
            title: title.into(),
            summary: String::new(),
            fields: Vec::new(),
            url: String::new(),
            meta: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn summary(mut self, s: impl Into<String>) -> Self {
        self.summary = s.into();
        self
    }

    #[must_use]
    pub fn field(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.push((label.into(), value.into()));
        self
    }

    #[must_use]
    pub fn url(mut self, u: impl Into<String>) -> Self {
        self.url = u.into();
        self
    }

    /// Plain-text rendering — the fallback every channel can use, and what
    /// email sends verbatim.
    #[must_use]
    pub fn as_text(&self) -> String {
        let mut out = String::new();
        if !self.summary.is_empty() {
            out.push_str(&self.summary);
            out.push_str("\n\n");
        }
        for (k, v) in &self.fields {
            out.push_str(k);
            out.push_str(": ");
            out.push_str(v);
            out.push('\n');
        }
        if !self.url.is_empty() {
            out.push('\n');
            out.push_str(&self.url);
            out.push('\n');
        }
        out
    }
}

/// Why a delivery failed, which decides whether the worker tries again.
#[derive(Debug)]
pub enum DeliverError {
    /// The destination is wrong and will stay wrong — a malformed webhook
    /// URL, a revoked token, a 404 channel. Retrying just burns attempts
    /// and delays the dead-letter that tells someone to fix the config.
    Permanent(String),
    /// A rate limit, a timeout, a 5xx. Worth another go.
    Transient(String),
}

impl DeliverError {
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        matches!(self, Self::Permanent(_))
    }

    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Permanent(m) | Self::Transient(m) => m,
        }
    }
}

impl std::fmt::Display for DeliverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Permanent(m) => write!(f, "permanent: {m}"),
            Self::Transient(m) => write!(f, "transient: {m}"),
        }
    }
}

/// One configurable setting on a channel, rendered into the admin form.
/// Reuses the widget vocabulary the rest of the admin speaks rather than
/// inventing a second one.
pub struct ConfigField {
    pub name: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    /// Held in the target's encrypted column instead of its plain config.
    pub secret: bool,
    pub required: bool,
}

/// Everything a channel needs to deliver: its resolved configuration and
/// the decrypted secret.
pub struct Delivery<'a> {
    pub config: &'a BTreeMap<String, String>,
    pub secret: &'a str,
    /// The tenant's outbound mailer, when one is wired. Only the email
    /// channel needs it; it is here rather than in the trait so channels
    /// stay constructible without one.
    pub mailer: Option<&'a dyn rustango::email::Mailer>,
    pub mailer_from: &'a str,
}

/// A destination kind. Implement it, register it, and it appears in the
/// admin's channel picker with its own settings.
#[async_trait::async_trait]
pub trait NotificationChannel: Send + Sync {
    /// Stable key stored on the target row (`"slack"`). Changing it
    /// orphans existing targets, so treat it as data, not a label.
    fn kind(&self) -> &'static str;

    /// Human name for the picker.
    fn label(&self) -> &'static str;

    /// One line describing what the admin has to go and fetch.
    fn help(&self) -> &'static str {
        ""
    }

    fn config_fields(&self) -> Vec<ConfigField>;

    async fn deliver(&self, d: &Delivery<'_>, msg: &Notification) -> Result<(), DeliverError>;
}

/// Registration slot — see [`register_notification_channel!`](crate::register_notification_channel!).
pub struct ChannelRegistration {
    pub factory: fn() -> Box<dyn NotificationChannel>,
}

inventory::collect!(ChannelRegistration);

/// Register a [`NotificationChannel`] so the admin can configure it.
///
/// ```ignore
/// register_notification_channel!(MyPagerDutyChannel);
/// ```
#[macro_export]
macro_rules! register_notification_channel {
    ($ty:ty) => {
        $crate::inventory::submit! {
            $crate::notify::ChannelRegistration {
                factory: || Box::new(<$ty>::default()),
            }
        }
    };
    ($factory:expr) => {
        $crate::inventory::submit! {
            $crate::notify::ChannelRegistration { factory: $factory }
        }
    };
}

/// Every registered channel, in registration order.
pub fn registered_channels() -> impl Iterator<Item = Box<dyn NotificationChannel>> {
    inventory::iter::<ChannelRegistration>
        .into_iter()
        .map(|r| (r.factory)())
}

/// The channel handling `kind`, or `None` when a target names a channel
/// this binary was not built with — a real case for host-registered
/// channels, so it must not panic.
#[must_use]
pub fn channel_for(kind: &str) -> Option<Box<dyn NotificationChannel>> {
    registered_channels().find(|c| c.kind() == kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_text_renders_summary_fields_and_link() {
        let n = Notification::new(events::FORM_SUBMITTED, "New submission")
            .summary("Someone got in touch.")
            .field("Name", "Ada")
            .field("Email", "ada@example.com")
            .url("https://example.com/admin");
        let t = n.as_text();
        assert!(t.contains("Someone got in touch."));
        assert!(t.contains("Name: Ada"));
        assert!(t.contains("Email: ada@example.com"));
        assert!(t.contains("https://example.com/admin"));
        // Field order is the form's order, not sorted — a submission read
        // out of order is harder to scan than the form that produced it.
        assert!(t.find("Name: Ada").unwrap() < t.find("Email:").unwrap());
    }

    /// Every built-in must be registered, or it silently cannot be chosen in
    /// the admin — the failure looks like "Slack isn't supported".
    #[test]
    fn built_in_channels_are_registered() {
        let kinds: Vec<&str> = registered_channels().map(|c| c.kind()).collect();
        for want in ["email", "slack", "teams", "telegram", "webhook"] {
            assert!(kinds.contains(&want), "channel `{want}` not registered; have {kinds:?}");
        }
    }

    #[test]
    fn channel_kinds_are_unique() {
        let mut kinds: Vec<String> =
            registered_channels().map(|c| c.kind().to_owned()).collect();
        kinds.sort();
        let before = kinds.len();
        kinds.dedup();
        // A duplicate kind means `channel_for` silently picks one of them and
        // half the targets deliver through the wrong adapter.
        assert_eq!(before, kinds.len(), "duplicate channel kind: {kinds:?}");
    }

    #[test]
    fn unknown_channel_is_none_not_a_panic() {
        // A target can name a channel a host registered but this binary
        // lacks; that must degrade, not crash the worker.
        assert!(channel_for("pagerduty-not-built-in").is_none());
    }

    #[test]
    fn every_channel_declares_at_least_one_config_field() {
        for c in registered_channels() {
            assert!(
                !c.config_fields().is_empty(),
                "channel `{}` has no config fields, so the admin form would \
                 render an empty box that saves an unusable target",
                c.kind()
            );
        }
    }
}
