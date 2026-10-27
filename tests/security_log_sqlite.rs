//! Account lockout and the security log (#433): the member login shares
//! the framework's per-account lock, and every attempt lands in
//! `cms_security_event`.
#![cfg(feature = "sqlite")]

use rustango::core::Model as _;
use rustango::login_throttle::{self, ClientIp, LoginRefused, LoginScope};
use rustango::sql::{Auto, Pool};
use rustango::tenancy::auth::User;

use rustango_cms::admin::{authenticate_member, MemberLoginError};
use rustango_cms::security_log::{self, Filter, LoginRequest};

async fn ddl(pool: &Pool, schema: &rustango::core::ModelSchema) {
    let sql =
        rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new()).await.expect("ddl");
    }
}

/// A pool with the user and log tables, and two members: `reader`
/// (active) and `gone` (disabled), both with the password `right-pass`.
async fn pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    ddl(&pool, User::SCHEMA).await;
    ddl(&pool, security_log::SecurityEvent::SCHEMA).await;
    let hash = rustango::passwords::hash("right-pass").expect("hash");
    for (name, active) in [("reader", true), ("gone", false)] {
        User {
            id: Auto::Unset,
            username: name.to_owned(),
            password_hash: hash.clone(),
            email: Some(format!("{name}@example.com")),
            is_superuser: false,
            active,
            created_at: chrono::Utc::now(),
            data: serde_json::json!({}),
            password_changed_at: None,
            sessions_revoked_at: None,
        }
        .insert_pool(&pool)
        .await
        .expect("insert user");
    }
    pool
}

fn request() -> LoginRequest {
    let mut request = LoginRequest::default();
    request.meta.ip_address = Some("203.0.113.9".to_owned());
    request.meta.user_agent = Some("test-agent".to_owned());
    request
}

async fn login(pool: &Pool, slug: &str, ident: &str, password: &str) -> Result<User, MemberLoginError> {
    authenticate_member(pool, slug, &request(), "member", ident, password).await
}

async fn events(pool: &Pool, event: &str) -> Vec<security_log::SecurityEvent> {
    let filter = Filter { event: event.to_owned(), username: String::new() };
    security_log::list(pool, &filter, 100, 0).await.expect("list")
}

#[tokio::test]
async fn a_good_login_is_logged_with_the_account_and_client() {
    let pool = pool().await;
    let user = login(&pool, "sec-ok", "reader@example.com", "right-pass").await.expect("signs in");
    assert_eq!(user.username, "reader");
    let rows = events(&pool, security_log::EVENT_LOGIN).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].username, "reader");
    assert_eq!(rows[0].user_id, user.id.get().copied());
    assert_eq!(rows[0].source, "member");
    assert_eq!(rows[0].ip_address, "203.0.113.9");
    assert_eq!(rows[0].user_agent, "test-agent");
}

#[tokio::test]
async fn repeated_failures_lock_the_account_even_for_the_right_password() {
    let pool = pool().await;
    let slug = "sec-lock";
    for _ in 0..5 {
        let r = login(&pool, slug, "reader", "wrong").await;
        assert!(matches!(r, Err(MemberLoginError::Invalid)));
    }
    let r = login(&pool, slug, "reader", "right-pass").await;
    assert!(
        matches!(r, Err(MemberLoginError::Refused(LoginRefused::Throttled { .. }))),
        "a locked account is refused before its password is checked: {r:?}"
    );
    // Another spelling of the same account shares the lock.
    let r = login(&pool, slug, "READER", "right-pass").await;
    assert!(matches!(r, Err(MemberLoginError::Refused(_))));

    let failed = events(&pool, security_log::EVENT_LOGIN_FAILED).await;
    assert_eq!(failed.len(), 5, "refused attempts are not logged one by one");
    assert!(failed.iter().all(|e| e.reason == "invalid_credentials"));
    let locks = events(&pool, security_log::EVENT_LOCKOUT).await;
    assert_eq!(locks.len(), 1, "the failure that locked the account is logged once");
    assert_eq!(locks[0].username, "reader");

    // The lock is per tenant.
    assert!(login(&pool, "sec-lock-other", "reader", "right-pass").await.is_ok());

    // A password reset lifts it.
    security_log::unlock(slug, "reader").await;
    assert!(login(&pool, slug, "reader", "right-pass").await.is_ok());
}

#[tokio::test]
async fn unknown_and_disabled_accounts_fail_alike_but_log_why() {
    let pool = pool().await;
    let r = login(&pool, "sec-why", "nobody", "right-pass").await;
    assert!(matches!(r, Err(MemberLoginError::Invalid)));
    let r = login(&pool, "sec-why", "gone", "right-pass").await;
    assert!(matches!(r, Err(MemberLoginError::Invalid)));

    let failed = events(&pool, security_log::EVENT_LOGIN_FAILED).await;
    let nobody = failed.iter().find(|e| e.username == "nobody").expect("unknown name logged");
    assert_eq!(nobody.user_id, None);
    assert_eq!(nobody.reason, "invalid_credentials");
    let gone = failed.iter().find(|e| e.username == "gone").expect("disabled account logged");
    assert!(gone.user_id.is_some());
    assert_eq!(gone.reason, "inactive");
}

/// The lock key this crate computes must be the one the framework's login
/// gate uses, or a lock set by the admin `/login` would go unseen here.
#[tokio::test]
async fn lock_keys_match_the_framework_gate() {
    let slug = "sec-key";
    let scope = LoginScope::Tenant(slug.to_owned());
    for _ in 0..5 {
        login_throttle::shared()
            .begin(&scope, &ClientIp::default(), "Admin")
            .await
            .expect("admitted")
            .failed()
            .await;
    }
    assert!(security_log::is_locked(slug, &["admin"]).await);
    assert!(!security_log::is_locked("sec-key-other", &["admin"]).await);
    security_log::unlock(slug, " ADMIN ").await;
    assert!(!security_log::is_locked(slug, &["admin"]).await);
}

#[tokio::test]
async fn the_viewer_filters_and_counts() {
    let pool = pool().await;
    let _ = login(&pool, "sec-view", "reader", "right-pass").await;
    let _ = login(&pool, "sec-view", "reader", "wrong").await;
    let _ = login(&pool, "sec-view", "nobody", "wrong").await;

    let all = Filter::default();
    assert_eq!(security_log::count(&pool, &all).await.expect("count"), 3);
    let reader = Filter { event: String::new(), username: "reader".to_owned() };
    assert_eq!(security_log::count(&pool, &reader).await.expect("count"), 2);
    // An unknown event name filters nothing out rather than everything.
    let bogus = Filter { event: "nope".to_owned(), username: String::new() };
    assert_eq!(security_log::count(&pool, &bogus).await.expect("count"), 3);

    let page = security_log::list(&pool, &all, 2, 0).await.expect("list");
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].username, "nobody", "newest first");
    assert_eq!(security_log::list(&pool, &all, 2, 2).await.expect("list").len(), 1);
}

#[tokio::test]
async fn prune_drops_rows_past_the_retention_window() {
    let pool = pool().await;
    let _ = login(&pool, "sec-prune", "reader", "right-pass").await;
    assert_eq!(security_log::prune(&pool, 1).await.expect("prune"), 0, "a fresh row stays");
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(security_log::prune(&pool, 0).await.expect("prune"), 1);
    assert_eq!(security_log::count(&pool, &Filter::default()).await.expect("count"), 0);
}
