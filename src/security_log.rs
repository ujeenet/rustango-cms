//! Sign-in security log and account lockout (#433).
//!
//! Every password sign-in, lockout, sign-out, password reset and change
//! to who may do what (user flags, role memberships, role permissions)
//! lands a row in `cms_security_event`, per tenant, which superusers read
//! at `/cms-admin/security-log`.
//!
//! Two kinds of login feed it:
//!
//! * **Logins the CMS owns** — the member form (`/members/login`), the
//!   headless API login, `/cms-admin/logout`, the password-reset
//!   screens and the user and role screens — record their rows directly
//!   with [`record`].
//! * **Logins the framework owns** — the tenant admin `POST /login` and
//!   its JWT login — are recorded from the framework's auth signals,
//!   connected by [`install`]. The signals carry no tenant, so the
//!   receiver takes it from [`rustango::tenant_log`], which the
//!   framework's request log fills (on unless `[logging] access_log =
//!   false`). Without it these events are not stored.
//!
//! ## Lockout
//!
//! Every password login goes through the framework's shared
//! [`rustango::login_throttle`] gate in the tenant scope, so the admin
//! form, the member form and the API login share one per-account lock
//! (keyed by the submitted name, so an unknown name locks exactly like a
//! real one and the lock is no account-enumeration oracle), plus per-IP
//! and per-tenant limits. The lock is time-bounded: an attacker can hold
//! an account locked only for `RCMS_LOGIN_LOCKOUT_SECONDS` after their
//! last burst, never for good.
//!
//! The policy defaults to the framework's (5 failures in an hour lock
//! the account for 15 minutes, or `[auth] lockout_*`). Set any of
//! `RCMS_LOGIN_LOCKOUT_ATTEMPTS`, `RCMS_LOGIN_LOCKOUT_SECONDS`,
//! `RCMS_LOGIN_LOCKOUT_WINDOW_SECONDS` to override it.
//!
//! Attempts refused while an account is locked are not logged one by
//! one — that would let an anonymous client grow the table without any
//! limit — only the failure that locked it is.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rustango::login_throttle::ClientIp;
use rustango::signals::auth::AuthRequestMeta;
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// A password sign-in succeeded.
pub const EVENT_LOGIN: &str = "login";
/// A password sign-in was refused (unknown name, wrong password,
/// disabled account).
pub const EVENT_LOGIN_FAILED: &str = "login_failed";
/// A failure locked the account.
pub const EVENT_LOCKOUT: &str = "lockout";
/// A session was ended.
pub const EVENT_LOGOUT: &str = "logout";
/// A password-reset link was issued.
pub const EVENT_PASSWORD_RESET_REQUESTED: &str = "password_reset_requested";
/// A password was changed through a reset link.
pub const EVENT_PASSWORD_RESET: &str = "password_reset";
/// A user's superuser flag, active flag or roles, or a role's
/// permissions, were changed.
pub const EVENT_PERMISSIONS_CHANGED: &str = "permissions_changed";

/// Every event, in the order the viewer offers them as filters.
pub const EVENTS: &[&str] = &[
    EVENT_LOGIN,
    EVENT_LOGIN_FAILED,
    EVENT_LOCKOUT,
    EVENT_LOGOUT,
    EVENT_PASSWORD_RESET_REQUESTED,
    EVENT_PASSWORD_RESET,
    EVENT_PERMISSIONS_CHANGED,
];

/// Rows older than this many days are pruned unless
/// `RCMS_SECURITY_LOG_RETENTION_DAYS` says otherwise.
pub const DEFAULT_RETENTION_DAYS: i64 = 90;

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "cms_security_event", app = "cms", display = "event")]
pub struct SecurityEvent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// One of the `EVENT_*` constants.
    #[rustango(max_length = 32, index)]
    pub event: String,

    /// Which sign-in surface: `admin`, `member`, `api`, `jwt`, …
    #[rustango(max_length = 32)]
    pub source: String,

    /// The account, when one was identified.
    pub user_id: Option<i64>,

    /// The name as submitted (or the account's username). For a role
    /// change, empty.
    #[rustango(max_length = 150, index)]
    pub username: String,

    /// Who made a permission change; empty for sign-in events.
    #[rustango(max_length = 150, default = "''")]
    pub actor: String,

    /// What a permission change changed, in words.
    #[rustango(max_length = 1000, default = "''")]
    pub detail: String,

    /// Why a sign-in failed: `invalid_credentials` or `inactive`.
    #[rustango(max_length = 32, default = "''")]
    pub reason: String,

    #[rustango(max_length = 64, default = "''")]
    pub ip_address: String,

    #[rustango(max_length = 255, default = "''")]
    pub user_agent: String,

    #[rustango(auto_now_add, index)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// One event to store. Build with [`Entry::new`] and the setters.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub event: &'static str,
    pub source: &'static str,
    pub user_id: Option<i64>,
    pub username: String,
    pub reason: &'static str,
    pub actor: String,
    pub detail: String,
    pub request: AuthRequestMeta,
}

impl Entry {
    #[must_use]
    pub fn new(event: &'static str, source: &'static str, request: &AuthRequestMeta) -> Self {
        Self {
            event,
            source,
            request: request.clone(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn user(mut self, user_id: Option<i64>, username: &str) -> Self {
        self.user_id = user_id;
        self.username = username.to_owned();
        self
    }

    #[must_use]
    pub fn reason(mut self, reason: &'static str) -> Self {
        self.reason = reason;
        self
    }

    /// Who made a permission change, and what it changed.
    #[must_use]
    pub fn change(mut self, actor: &str, detail: impl Into<String>) -> Self {
        self.actor = actor.to_owned();
        self.detail = detail.into();
        self
    }
}

/// `s` cut to at most `max` characters.
fn clip(s: &str, max: usize) -> String {
    s.trim().chars().take(max).collect()
}

/// Store `entry` in `tenant_slug`'s log. Best-effort: a failed write is
/// logged, never returned, so a broken log cannot break signing in.
pub async fn record(pool: &rustango::sql::Pool, tenant_slug: &str, entry: Entry) {
    let mut row = SecurityEvent {
        id: Auto::Unset,
        event: entry.event.to_owned(),
        source: entry.source.to_owned(),
        user_id: entry.user_id,
        username: clip(&entry.username, 150),
        reason: entry.reason.to_owned(),
        actor: clip(&entry.actor, 150),
        detail: clip(&entry.detail, 1000),
        ip_address: clip(entry.request.ip_address.as_deref().unwrap_or(""), 64),
        user_agent: clip(entry.request.user_agent.as_deref().unwrap_or(""), 255),
        created_at: Auto::Unset,
    };
    if let Err(e) = row.insert_pool(pool).await {
        tracing::warn!(target: "rustango_cms::security_log", error = %e, event = entry.event, "security event not stored");
        return;
    }
    prune_if_due(pool, tenant_slug).await;
}

// ------------------------------------------------------------------ permission changes

/// What a user may do: the flags and roles a permission change compares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Access {
    pub superuser: bool,
    pub active: bool,
    /// Role names, in any order.
    pub roles: Vec<String>,
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

fn role_list(roles: &[String]) -> String {
    let mut sorted = roles.to_vec();
    sorted.sort();
    if sorted.is_empty() {
        "none".to_owned()
    } else {
        sorted.join(", ")
    }
}

/// `before → after` in words (`superuser: off → on; roles: none →
/// Editor`), or `None` when nothing that grants access changed.
#[must_use]
pub fn describe_user_change(before: &Access, after: &Access) -> Option<String> {
    let mut parts = Vec::new();
    if before.superuser != after.superuser {
        parts.push(format!("superuser: {} → {}", on_off(before.superuser), on_off(after.superuser)));
    }
    if before.active != after.active {
        parts.push(format!("active: {} → {}", on_off(before.active), on_off(after.active)));
    }
    let (old_roles, new_roles) = (role_list(&before.roles), role_list(&after.roles));
    if old_roles != new_roles {
        parts.push(format!("roles: {old_roles} → {new_roles}"));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// A new user's access in words.
#[must_use]
pub fn describe_new_user(access: &Access) -> String {
    format!(
        "created; superuser: {}; active: {}; roles: {}",
        on_off(access.superuser),
        on_off(access.active),
        role_list(&access.roles),
    )
}

/// The codenames a role gained and lost (`role “Editor”: + cms_page.publish;
/// − cms_page.delete`), or `None` when its set is unchanged.
#[must_use]
pub fn describe_role_change(name: &str, before: &[String], after: &[String]) -> Option<String> {
    use std::collections::BTreeSet;
    let before: BTreeSet<&str> = before.iter().map(String::as_str).collect();
    let after: BTreeSet<&str> = after.iter().map(String::as_str).collect();
    let added: Vec<&str> = after.difference(&before).copied().collect();
    let removed: Vec<&str> = before.difference(&after).copied().collect();
    let mut parts = Vec::new();
    if !added.is_empty() {
        parts.push(format!("+ {}", added.join(", ")));
    }
    if !removed.is_empty() {
        parts.push(format!("− {}", removed.join(", ")));
    }
    (!parts.is_empty()).then(|| format!("role “{name}”: {}", parts.join("; ")))
}

// ------------------------------------------------------------------ retention

fn retention_days() -> i64 {
    crate::config::var("SECURITY_LOG_RETENTION_DAYS")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|d| *d > 0)
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}

/// Delete rows past the retention window, at most once an hour per
/// tenant, so the log of a site under attack stays bounded.
async fn prune_if_due(pool: &rustango::sql::Pool, tenant_slug: &str) {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static LAST: Mutex<Option<HashMap<String, std::time::Instant>>> = Mutex::new(None);
    {
        let mut g = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let map = g.get_or_insert_with(HashMap::new);
        if map.get(tenant_slug).is_some_and(|t| t.elapsed() < Duration::from_secs(3600)) {
            return;
        }
        map.insert(tenant_slug.to_owned(), std::time::Instant::now());
    }
    if let Err(e) = prune(pool, retention_days()).await {
        tracing::warn!(target: "rustango_cms::security_log", error = %e, "security log prune failed");
    }
}

/// Delete rows older than `days`. Returns how many went.
///
/// # Errors
/// Driver / query failures.
pub async fn prune(pool: &rustango::sql::Pool, days: i64) -> Result<u64, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    let cutoff = Utc::now() - chrono::Duration::days(days);
    let query = SecurityEvent::objects()
        .where_(SecurityEvent::created_at.lt(cutoff))
        .compile_delete()?;
    rustango::sql::delete_pool(pool, &query).await
}

// ------------------------------------------------------------------ reading

/// What the viewer filters by. Empty strings mean "any".
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub event: String,
    pub username: String,
}

fn filtered(filter: &Filter) -> rustango::query::QuerySet<SecurityEvent> {
    use rustango::core::Column as _;
    let mut qs = SecurityEvent::objects();
    let event = filter.event.trim();
    if EVENTS.contains(&event) {
        qs = qs.where_(SecurityEvent::event.eq(event.to_owned()));
    }
    let username = filter.username.trim();
    if !username.is_empty() {
        qs = qs.where_(SecurityEvent::username.eq(username.to_owned()));
    }
    qs
}

/// How many rows match `filter`.
///
/// # Errors
/// Driver / query failures.
pub async fn count(pool: &rustango::sql::Pool, filter: &Filter) -> Result<i64, rustango::sql::ExecError> {
    use rustango::sql::CounterPool as _;
    filtered(filter).count(pool).await
}

/// One page of the rows matching `filter`, newest first.
///
/// # Errors
/// Driver / query failures.
pub async fn list(
    pool: &rustango::sql::Pool,
    filter: &Filter,
    limit: i64,
    offset: i64,
) -> Result<Vec<SecurityEvent>, rustango::sql::ExecError> {
    use rustango::sql::FetcherPool as _;
    filtered(filter)
        .order_by(&[("created_at", true), ("id", true)])
        .limit(limit)
        .offset(offset)
        .fetch(pool)
        .await
}

// ------------------------------------------------------------------ requests

/// What a login handler needs from the request: the client address the
/// throttle limits by, and the details a log row keeps.
#[derive(Debug, Clone, Default)]
pub struct LoginRequest {
    pub ip: ClientIp,
    pub meta: AuthRequestMeta,
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for LoginRequest {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self {
            ip: ClientIp::from_parts(&parts.extensions, &parts.headers),
            meta: rustango::signals::auth::meta_from_parts(
                &parts.extensions,
                &parts.headers,
                Some(parts.uri.path()),
            ),
        })
    }
}

// ------------------------------------------------------------------ lockout

/// The lock key the framework's throttle uses for `username` in this
/// tenant: `login:tenant:<slug>:<sha256 of the trimmed, lowercased
/// name>`. The framework keeps it private; `tests/security_log_sqlite.rs`
/// fails if the two drift.
fn lock_key(tenant_slug: &str, username: &str) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(username.trim().to_lowercase().as_bytes());
    let mut key = format!("login:tenant:{tenant_slug}:");
    for b in digest {
        let _ = write!(key, "{b:02x}");
    }
    key
}

/// `true` when any of `names` is locked out of `tenant_slug` right now.
pub async fn is_locked(tenant_slug: &str, names: &[&str]) -> bool {
    let lockout = rustango::account_lockout::shared();
    for name in names {
        if lockout.is_locked(&lock_key(tenant_slug, name)).await {
            return true;
        }
    }
    false
}

/// Clear the lock and the failure count of `username` in `tenant_slug`,
/// as a successful sign-in would.
pub async fn unlock(tenant_slug: &str, username: &str) {
    rustango::account_lockout::shared().clear(&lock_key(tenant_slug, username)).await;
}

/// The lockout policy from `RCMS_LOGIN_LOCKOUT_*`, or `None` when none
/// is set (the framework default or `[auth] lockout_*` then applies).
fn lockout_from_env() -> Option<rustango::account_lockout::Lockout> {
    let num = |name: &str| {
        crate::config::var(name).and_then(|v| match v.trim().parse::<u64>() {
            Ok(n) if n > 0 => Some(n),
            _ => {
                tracing::warn!(target: "rustango_cms::security_log", setting = name, value = %v, "ignored: not a positive whole number");
                None
            }
        })
    };
    let attempts = num("LOGIN_LOCKOUT_ATTEMPTS");
    let seconds = num("LOGIN_LOCKOUT_SECONDS");
    let window = num("LOGIN_LOCKOUT_WINDOW_SECONDS");
    if attempts.is_none() && seconds.is_none() && window.is_none() {
        return None;
    }
    let mut lockout =
        rustango::account_lockout::Lockout::new(Arc::new(rustango::cache::InMemoryCache::new()));
    if let Some(n) = attempts {
        lockout = lockout.max_attempts(u32::try_from(n).unwrap_or(u32::MAX));
    }
    if let Some(s) = seconds {
        lockout = lockout.lockout_duration(Duration::from_secs(s));
    }
    if let Some(s) = window {
        lockout = lockout.counter_ttl(Duration::from_secs(s));
    }
    Some(lockout)
}

// ------------------------------------------------------------------ framework logins

/// Apply the `RCMS_LOGIN_LOCKOUT_*` policy and start recording the
/// framework's own logins. Idempotent; [`crate::ensure_seeded`] calls it
/// at boot.
pub fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Some(lockout) = lockout_from_env() {
            if !rustango::account_lockout::configure_shared(lockout) {
                tracing::warn!(
                    target: "rustango_cms::security_log",
                    "RCMS_LOGIN_LOCKOUT_* ignored: the application already installed a lockout policy"
                );
            }
        }
        connect_receivers();
    });
}

fn connect_receivers() {
    use rustango::signals::auth::{
        connect_user_logged_in, connect_user_logged_out, connect_user_login_failed,
        AuthFailureReason,
    };
    connect_user_logged_in(|ctx| async move {
        let Some((slug, pool)) = request_tenant().await else { return };
        let entry = Entry::new(EVENT_LOGIN, signal_source(ctx.source), &ctx.request)
            .user(Some(ctx.user_id), &ctx.username);
        record(&pool, &slug, entry).await;
    });
    connect_user_login_failed(|ctx| async move {
        let Some((slug, pool)) = request_tenant().await else { return };
        let username = ctx.attempted_username.unwrap_or_default();
        let reason = match ctx.reason {
            AuthFailureReason::Inactive => "inactive",
            _ => "invalid_credentials",
        };
        let source = signal_source(ctx.source);
        record(
            &pool,
            &slug,
            Entry::new(EVENT_LOGIN_FAILED, source, &ctx.request)
                .user(None, &username)
                .reason(reason),
        )
        .await;
        // The framework counts the failure before it signals, so a lock
        // seen here is the one this failure set.
        if is_locked(&slug, &[&username]).await {
            let entry = Entry::new(EVENT_LOCKOUT, source, &ctx.request).user(None, &username);
            record(&pool, &slug, entry).await;
        }
    });
    connect_user_logged_out(|ctx| async move {
        if ctx.user_id.is_none() {
            return;
        }
        let Some((slug, pool)) = request_tenant().await else { return };
        let entry = Entry::new(EVENT_LOGOUT, signal_source(ctx.source), &ctx.request)
            .user(ctx.user_id, ctx.username.as_deref().unwrap_or(""));
        record(&pool, &slug, entry).await;
    });
}

/// The framework's surface tag, in this log's vocabulary.
fn signal_source(source: &'static str) -> &'static str {
    match source {
        "tenant_admin" => "admin",
        other => other,
    }
}

/// The tenant the current request resolved to, and its pool. `None`
/// outside a tenant request (operator console, bare admin) or when the
/// request log that carries the tenant is off.
async fn request_tenant() -> Option<(String, rustango::sql::Pool)> {
    let label = rustango::tenant_log::current()?;
    let pool = crate::task_queue::resolve_pool(&label.slug).await?;
    Some((label.slug, pool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_counts_characters_not_bytes() {
        assert_eq!(clip("  żółw  ", 3), "żół");
        assert_eq!(clip("abc", 10), "abc");
    }

    fn access(superuser: bool, active: bool, roles: &[&str]) -> Access {
        Access {
            superuser,
            active,
            roles: roles.iter().map(|r| (*r).to_owned()).collect(),
        }
    }

    #[test]
    fn user_changes_name_only_what_changed() {
        let before = access(false, true, &["Viewer"]);
        assert_eq!(describe_user_change(&before, &before), None);
        // Role order is not a change.
        let reordered = access(false, true, &["Viewer"]);
        assert_eq!(describe_user_change(&before, &reordered), None);
        let after = access(true, true, &["Viewer", "Editor"]);
        assert_eq!(
            describe_user_change(&before, &after).as_deref(),
            Some("superuser: off → on; roles: Viewer → Editor, Viewer"),
        );
        let after = access(false, false, &[]);
        assert_eq!(
            describe_user_change(&before, &after).as_deref(),
            Some("active: on → off; roles: Viewer → none"),
        );
        assert_eq!(
            describe_new_user(&access(false, true, &[])),
            "created; superuser: off; active: on; roles: none",
        );
    }

    #[test]
    fn role_changes_list_gained_and_lost_codenames() {
        let s = |v: &[&str]| v.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>();
        assert_eq!(describe_role_change("Editor", &s(&["a"]), &s(&["a"])), None);
        assert_eq!(
            describe_role_change("Editor", &s(&["a", "b"]), &s(&["a", "c"])).as_deref(),
            Some("role “Editor”: + c; − b"),
        );
    }

    #[test]
    fn lock_keys_ignore_case_and_padding() {
        assert_eq!(lock_key("acme", " Alice "), lock_key("acme", "alice"));
        assert_ne!(lock_key("acme", "alice"), lock_key("other", "alice"));
    }
}
