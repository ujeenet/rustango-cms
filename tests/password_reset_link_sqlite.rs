//! `admin::issue_password_reset_link` — the operator's way into an
//! account the reset form cannot reach because it has no email on file.
#![cfg(feature = "sqlite")]

use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango::tenancy::auth::User;

async fn pool_with_users() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    let sql =
        rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), User::SCHEMA);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(&pool, stmt, Vec::new()).await.expect("ddl");
    }
    for (name, active) in [("admin", true), ("gone", false)] {
        User {
            id: Auto::Unset,
            username: name.to_owned(),
            password_hash: "hash".to_owned(),
            email: None,
            is_superuser: true,
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

#[tokio::test]
async fn an_account_without_email_still_gets_a_link() {
    let pool = pool_with_users().await;
    let link = rustango_cms::admin::issue_password_reset_link(&pool, "blog", "admin")
        .await
        .expect("query")
        .expect("a link for an active user");
    assert!(link.starts_with("/cms-admin/password-reset/confirm?"), "{link}");
    assert!(link.contains("purpose=pwreset") && link.contains("signature="), "{link}");
}

#[tokio::test]
async fn unknown_and_inactive_users_get_none() {
    let pool = pool_with_users().await;
    for name in ["nobody", "gone"] {
        assert_eq!(
            rustango_cms::admin::issue_password_reset_link(&pool, "blog", name)
                .await
                .expect("query"),
            None,
            "{name}",
        );
    }
}
