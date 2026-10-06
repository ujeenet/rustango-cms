//! The outbox: enqueue on the request path, deliver off it.

use chrono::Utc;
use rustango::core::Column as _;
use rustango::query::Q;
use rustango::sql::{ExecError, FetcherPool as _, Pool};

use super::model::{
    backoff_secs, NotificationDelivery, NotificationTarget, MAX_ATTEMPTS, STATUS_FAILED,
    STATUS_PENDING, STATUS_SENT,
};
use super::{channel_for, Delivery, Notification};

/// Queue `msg` for every enabled target subscribed to its event.
///
/// Returns how many rows were written. Cheap and synchronous — one SELECT
/// and N INSERTs — so it is safe on a request path; the network calls
/// happen in [`drain`].
///
/// # Errors
/// Driver / query failures. Callers on a visitor-facing path should log
/// and carry on rather than fail the submission: the content is already
/// saved, and losing the notification is better than losing the lead.
pub async fn enqueue(
    pool: &Pool,
    msg: &Notification,
    source_id: i64,
) -> Result<usize, ExecError> {
    let targets: Vec<NotificationTarget> = NotificationTarget::objects()
        .where_(NotificationTarget::enabled.eq(true))
        .fetch(pool)
        .await?;
    let payload = serde_json::to_string(msg).unwrap_or_else(|_| "{}".to_owned());
    let mut n = 0;
    for t in targets.iter().filter(|t| t.wants(&msg.event, source_id)) {
        let mut row = NotificationDelivery {
            id: rustango::sql::Auto::Unset,
            target_id: t.id.get().copied().unwrap_or_default(),
            event: msg.event.clone(),
            payload: payload.clone(),
            status: STATUS_PENDING.to_owned(),
            attempts: 0,
            last_error: String::new(),
            next_attempt_at: Some(Utc::now()),
            created_at: rustango::sql::Auto::Unset,
            sent_at: None,
        };
        row.insert_pool(pool).await?;
        n += 1;
    }
    Ok(n)
}

/// Context the worker needs that is not in the database.
pub struct DrainCtx<'a> {
    pub mailer: Option<&'a dyn rustango::email::Mailer>,
    pub mailer_from: &'a str,
}

/// What one drain pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    pub sent: usize,
    pub retried: usize,
    pub failed: usize,
}

/// Send every delivery that is due, up to `limit`.
///
/// Call it from a scheduled task, a background loop, or a management verb.
/// It takes no lock: two concurrent drains may both pick up a row and
/// deliver twice. That is the deliberate trade — for notifications a rare
/// duplicate is a much smaller harm than a lock that strands the queue
/// when a worker dies holding it, and the alternative (`SELECT … FOR
/// UPDATE SKIP LOCKED`) is unavailable on SQLite, which this CMS supports
/// first-class.
///
/// # Errors
/// Driver / query failures while reading the queue.
pub async fn drain(pool: &Pool, ctx: &DrainCtx<'_>, limit: usize) -> Result<DrainReport, ExecError> {
    let due: Vec<NotificationDelivery> = NotificationDelivery::objects()
        .where_(due_at(Utc::now()))
        .order_by(&[("created_at", false)])
        .limit(limit as i64)
        .fetch(pool)
        .await?;

    let mut report = DrainReport::default();
    for mut row in due {
        let target: Option<NotificationTarget> = NotificationTarget::objects()
            .where_(NotificationTarget::id.eq(row.target_id))
            .first(pool)
            .await?;
        let Some(target) = target.filter(|t| t.enabled) else {
            // The destination was deleted or switched off after this was
            // queued. Sending is meaningless; say so instead of retrying.
            row.status = STATUS_FAILED.to_owned();
            row.last_error = "target missing or disabled".to_owned();
            row.save_pool(pool).await?;
            report.failed += 1;
            continue;
        };
        let Some(channel) = channel_for(&target.kind) else {
            row.status = STATUS_FAILED.to_owned();
            row.last_error = format!("no channel registered for kind `{}`", target.kind);
            row.save_pool(pool).await?;
            report.failed += 1;
            continue;
        };
        let msg: Notification = match serde_json::from_str(&row.payload) {
            Ok(m) => m,
            Err(e) => {
                row.status = STATUS_FAILED.to_owned();
                row.last_error = format!("unreadable payload: {e}");
                row.save_pool(pool).await?;
                report.failed += 1;
                continue;
            }
        };

        let config = target.config_map();
        let secret = target.secret.clone().into_inner().to_string();
        let d = Delivery {
            config: &config,
            secret: &secret,
            mailer: ctx.mailer,
            mailer_from: ctx.mailer_from,
        };
        row.attempts += 1;
        match channel.deliver(&d, &msg).await {
            Ok(()) => {
                row.status = STATUS_SENT.to_owned();
                row.sent_at = Some(Utc::now());
                row.last_error = String::new();
                row.next_attempt_at = None;
                report.sent += 1;
            }
            Err(e) => {
                row.last_error = e.message().chars().take(500).collect();
                if e.is_permanent() || row.attempts >= MAX_ATTEMPTS {
                    row.status = STATUS_FAILED.to_owned();
                    row.next_attempt_at = None;
                    report.failed += 1;
                } else {
                    row.next_attempt_at =
                        Some(Utc::now() + chrono::Duration::seconds(backoff_secs(row.attempts)));
                    report.retried += 1;
                }
            }
        }
        row.save_pool(pool).await?;
    }
    Ok(report)
}

/// Put a failed delivery back in the queue — the admin's "try again".
///
/// # Errors
/// Driver / query failures.
pub async fn retry(pool: &Pool, delivery_id: i64) -> Result<bool, ExecError> {
    let Some(mut row): Option<NotificationDelivery> = NotificationDelivery::objects()
        .where_(NotificationDelivery::id.eq(delivery_id))
        .first(pool)
        .await?
    else {
        return Ok(false);
    };
    row.status = STATUS_PENDING.to_owned();
    // Reset the counter: a human retrying after fixing the webhook wants a
    // fresh budget, not the one attempt the old run had left.
    row.attempts = 0;
    row.last_error = String::new();
    row.next_attempt_at = Some(Utc::now());
    row.save_pool(pool).await?;
    Ok(true)
}

/// A tenant's drain in progress. `dirty` is set when a drain was asked for
/// while this one was running; `wake` cuts short its wait for a retry.
struct Slot {
    dirty: bool,
    wake: std::sync::Arc<rustango::__private_runtime::tokio::sync::Notify>,
}

/// Drains in progress, so a burst of submissions does not start a drain per
/// request. Keyed by tenant, since each has its own pool.
static IN_FLIGHT: std::sync::Mutex<Option<std::collections::HashMap<String, Slot>>> =
    std::sync::Mutex::new(None);

/// Start draining `key` and return its wake handle — or, when a drain is
/// already running, mark it dirty, wake it, and return `None`.
fn claim(key: &str) -> Option<std::sync::Arc<rustango::__private_runtime::tokio::sync::Notify>> {
    let mut g = IN_FLIGHT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let map = g.get_or_insert_with(Default::default);
    if let Some(slot) = map.get_mut(key) {
        slot.dirty = true;
        slot.wake.notify_one();
        return None;
    }
    let wake = std::sync::Arc::default();
    map.insert(
        key.to_owned(),
        Slot {
            dirty: false,
            wake: std::sync::Arc::clone(&wake),
        },
    );
    Some(wake)
}

/// Finish a pass: `true` keeps the claim for another one. It is kept when a
/// drain was requested during the pass — a row enqueued after the pass read
/// the queue would otherwise wait for the next submission (#741) — or when
/// the caller still has a retry to wait for.
fn finish_pass(key: &str, waiting: bool) -> bool {
    let mut g = IN_FLIGHT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(map) = g.as_mut() else { return false };
    let Some(slot) = map.get_mut(key) else { return false };
    if std::mem::take(&mut slot.dirty) || waiting {
        return true;
    }
    map.remove(key);
    false
}

/// Pending and due at `now`. Filtered in SQL (#741): selecting the oldest
/// pending rows and skipping backed-off ones in Rust let a window full of
/// retries (a webhook that is down) starve newer rows that were due.
fn due_at(now: chrono::DateTime<Utc>) -> Q {
    Q::eq("status", STATUS_PENDING)
        .and(Q::is_null("next_attempt_at").or(Q::lte("next_attempt_at", now)))
}

/// When the earliest pending delivery is due, if any is. Two lookups, not
/// one `ORDER BY next_attempt_at`: where NULLs sort differs per dialect.
async fn next_due(pool: &Pool) -> Result<Option<chrono::DateTime<Utc>>, ExecError> {
    let now = Utc::now();
    if NotificationDelivery::objects().where_(due_at(now)).first(pool).await?.is_some() {
        return Ok(Some(now));
    }
    let later: Option<NotificationDelivery> = NotificationDelivery::objects()
        .where_(NotificationDelivery::status.eq(STATUS_PENDING.to_owned()))
        .where_(Q::gt("next_attempt_at", now))
        .order_by(&[("next_attempt_at", false)])
        .first(pool)
        .await?;
    Ok(later.and_then(|r| r.next_attempt_at))
}

/// Drain in the background, off the request path.
///
/// Called right after [`enqueue`] so a notification normally goes out in the
/// same second it was queued, without the visitor waiting on Slack. It
/// drains everything *due*, not just what this request added, so a retry
/// queued earlier rides along.
///
/// The task stays up while retries are pending, sleeping until the next
/// one is due, so a backed-off delivery goes out without new traffic. It
/// ends once the queue has nothing pending: a retry never waits longer than
/// the last backoff step, and a delivery gives up after [`MAX_ATTEMPTS`].
pub fn spawn_drain(
    pool: rustango::sql::Pool,
    key: String,
    mailer: Option<std::sync::Arc<dyn rustango::email::Mailer>>,
    mailer_from: String,
) {
    let Some(wake) = claim(&key) else {
        return; // one already running for this tenant; it goes round again
    };
    rustango::__private_runtime::tokio::spawn(async move {
        let ctx = DrainCtx {
            mailer: mailer.as_deref(),
            mailer_from: &mailer_from,
        };
        loop {
            // A failed pass or lookup does not wait: retrying a broken
            // database on a timer would only spin.
            let due = match drain(&pool, &ctx, 50).await {
                Ok(_) => next_due(&pool).await.unwrap_or_else(|e| {
                    tracing::warn!(target: "rustango_cms::notify", error = %e, "notification queue lookup failed");
                    None
                }),
                Err(e) => {
                    tracing::warn!(target: "rustango_cms::notify", error = %e, "notification drain failed");
                    None
                }
            };
            if !finish_pass(&key, due.is_some()) {
                break;
            }
            if let Some(at) = due {
                let wait = (at - Utc::now()).to_std().unwrap_or_default();
                // A new submission wakes the wait early; timing out is the
                // normal path.
                let _ = rustango::__private_runtime::tokio::time::timeout(wait, wake.notified()).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{claim, finish_pass};

    #[test]
    fn a_request_during_a_pass_runs_another() {
        let key = "test-dirty";
        let wake = claim(key).expect("first claim starts a drain");
        assert!(claim(key).is_none(), "second claim joins the running one");
        assert!(finish_pass(key, false), "the joined request gets its pass");
        assert!(!finish_pass(key, false), "nothing more asked: released");
        assert!(claim(key).is_some(), "released keys can be claimed again");
        assert!(!finish_pass(key, false));
        drop(wake);
    }

    #[test]
    fn a_pending_retry_keeps_the_claim() {
        let key = "test-waiting";
        let _wake = claim(key).expect("claim");
        assert!(finish_pass(key, true), "a retry to wait for keeps the claim");
        assert!(claim(key).is_none(), "still held while waiting");
        assert!(finish_pass(key, false), "the request made while waiting runs");
        assert!(!finish_pass(key, false));
    }

    #[tokio::test]
    async fn a_new_request_wakes_the_wait() {
        let key = "test-wake";
        let wake = claim(key).expect("claim");
        assert!(finish_pass(key, true));
        assert!(claim(key).is_none());
        // notify_one stored a permit, so this returns at once rather than
        // sitting out the retry's backoff.
        tokio::time::timeout(std::time::Duration::from_secs(5), wake.notified())
            .await
            .expect("woken, not timed out");
        assert!(finish_pass(key, false));
        assert!(!finish_pass(key, false));
    }
}
