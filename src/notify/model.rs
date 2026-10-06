//! Persisted notification config + the delivery outbox.

use chrono::{DateTime, Utc};
use rustango::casts::{Cast, EncryptedString};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// Statuses a delivery row moves through. Strings, not an enum column, to
/// match how the rest of the CMS stores small vocabularies (page status,
/// media kind) and to keep a future status from needing a migration.
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_SENT: &str = "sent";
/// Retries exhausted, or the channel said the config is wrong. Terminal
/// until a human retries it.
pub const STATUS_FAILED: &str = "failed";

/// A configured destination: which channel, how it is set up, and which
/// events it wants.
#[derive(Model, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_notification_target",
    app = "cms",
    display = "label",
    admin(
        list_display = "label, kind, enabled, events",
        ordering = "label",
    )
)]
pub struct NotificationTarget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Editor-facing name — "#leads in Slack", "Sales inbox".
    #[rustango(max_length = 150)]
    pub label: String,

    /// Which [`NotificationChannel`](super::NotificationChannel) handles
    /// it. A kind with no registered channel is skipped, not an error: a
    /// host app can register channels this binary does not have.
    #[rustango(max_length = 32, index)]
    pub kind: String,

    /// Non-secret settings as a JSON object — the channel decides the
    /// keys via `config_fields()`. JSON rather than columns because every
    /// channel wants different ones, and a new channel must not need a
    /// migration.
    pub config: String,

    /// The one secret a destination needs: an incoming-webhook URL, a bot
    /// token. Encrypted at rest (XChaCha20-Poly1305, `RUSTANGO_SECRET_KEY`)
    /// with the same primitive SSO client secrets use — a Slack webhook URL
    /// *is* a credential, since anyone holding it can post to the channel.
    pub secret: Cast<EncryptedString>,

    /// Comma-separated event keys this target wants (`"form.submitted"`).
    /// Empty means every event — the useful default for "tell me
    /// everything" destinations.
    #[rustango(max_length = 512)]
    pub events: String,

    /// Scope to one source object, e.g. a single form's snippet id. 0 =
    /// every source of its subscribed events.
    pub source_id: i64,

    pub enabled: bool,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Debug without the secret (#734): `Cast`'s own Debug prints the
/// decrypted value, so a derived impl would log a webhook URL or bot token
/// from any `{:?}` of a target.
impl std::fmt::Debug for NotificationTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotificationTarget")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("kind", &self.kind)
            .field("config", &self.config)
            .field("secret", &"<redacted>")
            .field("events", &self.events)
            .field("source_id", &self.source_id)
            .field("enabled", &self.enabled)
            .field("created_at", &self.created_at)
            .finish()
    }
}

impl NotificationTarget {
    /// Whether this target wants `event` from `source_id`.
    #[must_use]
    pub fn wants(&self, event: &str, source_id: i64) -> bool {
        if !self.enabled {
            return false;
        }
        if self.source_id != 0 && self.source_id != source_id {
            return false;
        }
        let list = self.events.trim();
        list.is_empty() || list.split(',').any(|e| e.trim() == event)
    }

    /// Parsed `config`. A row whose JSON has rotted yields an empty map
    /// rather than failing the send: the channel then reports a missing
    /// setting, which points at the real problem.
    #[must_use]
    pub fn config_map(&self) -> std::collections::BTreeMap<String, String> {
        serde_json::from_str(&self.config).unwrap_or_default()
    }
}

/// One queued attempt to deliver one notification to one target.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_notification_delivery",
    app = "cms",
    display = "event",
    admin(
        list_display = "event, target_id, status, attempts, created_at",
        ordering = "-created_at",
    )
)]
pub struct NotificationDelivery {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_notification_target", on = "id", index)]
    pub target_id: i64,

    #[rustango(max_length = 64, index)]
    pub event: String,

    /// The serialized [`Notification`](super::Notification). Stored rather
    /// than re-derived so a retry days later sends what actually happened,
    /// not what the source looks like now — the submission may have been
    /// edited or deleted in between.
    pub payload: String,

    #[rustango(max_length = 16, index)]
    pub status: String,

    pub attempts: i32,

    /// Why the last attempt failed, shown in the admin so a broken
    /// destination is diagnosable without reading logs.
    #[rustango(max_length = 512)]
    pub last_error: String,

    /// Earliest time the worker may try again — backoff lives here rather
    /// than in the worker's memory so it survives a restart.
    pub next_attempt_at: Option<DateTime<Utc>>,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,

    pub sent_at: Option<DateTime<Utc>>,
}

/// Give up after this many tries. Roughly 1s + 5s + 25s + 125s + 625s of
/// backoff — about thirteen minutes, which covers a short outage or a rate
/// limit without leaving a genuinely broken destination retrying forever.
pub const MAX_ATTEMPTS: i32 = 5;

/// Backoff before attempt `n` (1-based).
#[must_use]
pub fn backoff_secs(attempt: i32) -> i64 {
    5i64.saturating_pow(u32::try_from(attempt.max(1) - 1).unwrap_or(0).min(6))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(events: &str, source_id: i64, enabled: bool) -> NotificationTarget {
        NotificationTarget {
            id: Auto::Unset,
            label: "t".into(),
            kind: "slack".into(),
            config: "{}".into(),
            secret: Cast::new(String::new().into()),
            events: events.into(),
            source_id,
            enabled,
            created_at: Auto::Unset,
        }
    }

    #[test]
    fn empty_event_list_means_every_event() {
        let t = target("", 0, true);
        assert!(t.wants("form.submitted", 1));
        assert!(t.wants("anything.else", 1));
    }

    #[test]
    fn a_listed_event_matches_and_others_do_not() {
        let t = target("form.submitted, page.published", 0, true);
        assert!(t.wants("form.submitted", 7));
        assert!(t.wants("page.published", 7));
        assert!(!t.wants("form.deleted", 7));
    }

    #[test]
    fn source_scope_pins_a_target_to_one_form() {
        let t = target("form.submitted", 42, true);
        assert!(t.wants("form.submitted", 42));
        // The whole point of scoping: another form's submissions must not
        // reach a destination configured for this one.
        assert!(!t.wants("form.submitted", 43));
    }

    #[test]
    fn source_zero_accepts_every_source() {
        let t = target("form.submitted", 0, true);
        assert!(t.wants("form.submitted", 1));
        assert!(t.wants("form.submitted", 999));
    }

    #[test]
    fn disabled_targets_want_nothing() {
        let t = target("", 0, false);
        assert!(!t.wants("form.submitted", 1));
    }

    #[test]
    fn config_map_survives_rotten_json() {
        let mut t = target("", 0, true);
        t.config = "{not json".into();
        // Must not panic: a bad row should surface as "missing setting" from
        // the channel, not take the worker down.
        assert!(t.config_map().is_empty());
    }

    #[test]
    fn backoff_grows_and_stays_bounded() {
        assert_eq!(backoff_secs(1), 1);
        assert_eq!(backoff_secs(2), 5);
        assert_eq!(backoff_secs(3), 25);
        assert!(backoff_secs(MAX_ATTEMPTS) > backoff_secs(MAX_ATTEMPTS - 1));
        // No overflow panic on a nonsense attempt count.
        let _ = backoff_secs(i32::MAX);
        let _ = backoff_secs(0);
    }
}

#[cfg(test)]
mod debug_tests {
    use super::NotificationTarget;

    #[test]
    fn debug_never_prints_the_secret() {
        let t = NotificationTarget {
            id: rustango::sql::Auto::Unset,
            label: "Slack".to_owned(),
            kind: "slack".to_owned(),
            config: "{}".to_owned(),
            secret: rustango::casts::Cast::new("https://hooks.example/T0/B0/very-secret".to_owned().into()),
            events: String::new(),
            source_id: 0,
            enabled: true,
            created_at: rustango::sql::Auto::Unset,
        };
        let shown = format!("{t:?}");
        assert!(!shown.contains("very-secret"), "{shown}");
        assert!(shown.contains("<redacted>"));
    }
}
