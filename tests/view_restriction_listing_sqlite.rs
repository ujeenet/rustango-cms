//! #757 — the listing-side restriction checks (menus, navigation, search)
//! fail closed when the restriction rows can't be read.
#![cfg(feature = "sqlite")]

use chrono::Utc;
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango::tenancy::auth::User;

use rustango_cms::view_restriction::{access_context, effective_restrictions_for, PageViewRestriction};

async fn ddl(pool: &Pool, schema: &rustango::core::ModelSchema) {
    let sql =
        rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .expect("ddl");
    }
}

fn viewer(superuser: bool) -> User {
    User {
        id: Auto::Set(1),
        username: "v".to_owned(),
        password_hash: String::new(),
        email: None,
        is_superuser: superuser,
        active: true,
        created_at: Utc::now(),
        data: serde_json::json!({}),
        password_changed_at: None,
        sessions_revoked_at: None,
    }
}

#[tokio::test]
async fn an_unreadable_restriction_table_hides_pages_from_all_but_superusers() {
    // No cms_page_view_restriction table: every lookup errors.
    let broken = Pool::connect("sqlite::memory:").await.expect("mem pool");

    let anon = access_context(&broken, None).await;
    assert!(!anon.can_view("0001/", 1), "anonymous must not see listings it can't check");
    let member = viewer(false);
    assert!(!access_context(&broken, Some(&member)).await.can_view("0001/0002/", 1));
    let admin = viewer(true);
    assert!(access_context(&broken, Some(&admin)).await.can_view("0001/", 1));

    let map = effective_restrictions_for(&broken, &[(7, "0001/".to_owned())]).await;
    assert!(map.contains_key(&7), "every page counts as restricted when rows can't be read");
}

#[tokio::test]
async fn a_readable_empty_table_restricts_nothing() {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    ddl(&pool, &PageViewRestriction::SCHEMA).await;
    assert!(access_context(&pool, None).await.can_view("0001/", 1));
    assert!(effective_restrictions_for(&pool, &[(7, "0001/".to_owned())]).await.is_empty());
}
