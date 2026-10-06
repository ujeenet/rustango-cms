//! MySQL column fixes, each shown failing before and passing after: long
//! bodies save once `widen_long_text` has run (#683), and identity lookups
//! are exact once `exact_identity_columns` has (#726). Needs a live server:
//!
//! ```text
//! RCMS_TEST_MYSQL_URL=mysql://root@127.0.0.1:3399 \
//!   cargo test --no-default-features --features mysql --test mysql_long_text -- --ignored
//! ```
#![cfg(feature = "mysql")]

use rustango::core::{Column as _, Model as _};
use rustango::sql::{Auto, Pool};
use rustango_cms::snippet::Snippet;

async fn fresh_db(tag: &str) -> Pool {
    let server = std::env::var("RCMS_TEST_MYSQL_URL").expect("RCMS_TEST_MYSQL_URL is not set");
    let name = format!("rcms_{tag}_{}", std::process::id());
    let admin = Pool::connect(&server).await.expect("connect server");
    for sql in [format!("DROP DATABASE IF EXISTS {name}"), format!("CREATE DATABASE {name}")] {
        rustango::sql::raw_execute_pool(&admin, &sql, Vec::new()).await.expect("create db");
    }
    let pool = Pool::connect(&format!("{}/{name}", server.trim_end_matches('/')))
        .await
        .expect("connect db");
    let ddl = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
        pool.dialect(),
        &Snippet::SCHEMA,
    );
    for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(&pool, stmt, Vec::new()).await.expect("ddl");
    }
    pool
}

fn snippet(body: String) -> Snippet {
    Snippet {
        id: Auto::Unset,
        type_name: "callout".to_owned(),
        slug: "long".to_owned(),
        folder_path: String::new(),
        title: "Long".to_owned(),
        body_markdown: body,
        data: serde_json::json!({}),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    }
}

#[tokio::test]
#[ignore = "needs a live MySQL server (RCMS_TEST_MYSQL_URL)"]
async fn a_long_body_saves_once_widened() {
    let pool = fresh_db("longtext").await;
    let body = "x".repeat(100_000);

    assert!(
        snippet(body.clone()).save_pool(&pool).await.is_err(),
        "a TEXT column refuses 100 KB in strict mode"
    );

    let changed = rustango_cms::mysql_text::widen_long_text(&pool).await.expect("widen");
    assert!(changed >= 1, "body_markdown is widened");
    assert_eq!(
        rustango_cms::mysql_text::widen_long_text(&pool).await.expect("rerun"),
        0,
        "nothing left to widen"
    );

    let mut row = snippet(body.clone());
    row.save_pool(&pool).await.expect("saves once widened");
    let back = Snippet::objects()
        .where_(Snippet::id.eq(row.id.get().copied().expect("id")))
        .first(&pool)
        .await
        .expect("query")
        .expect("row");
    assert_eq!(back.body_markdown.len(), body.len(), "stored whole, not truncated");
}

/// #726 — slugs differing only in case coexist and resolve exactly once
/// the identity columns are binary, as on Postgres and SQLite.
#[tokio::test]
#[ignore = "needs a live MySQL server (RCMS_TEST_MYSQL_URL)"]
async fn identity_lookups_are_exact_once_binary() {
    let pool = fresh_db("collate").await;
    let mut hero = snippet(String::new());
    hero.slug = "Hero".to_owned();
    hero.save_pool(&pool).await.expect("Hero");
    let mut lower = snippet(String::new());
    lower.slug = "hero".to_owned();
    assert!(lower.save_pool(&pool).await.is_err(), "the default collation folds case");

    let changed = rustango_cms::mysql_text::exact_identity_columns(&pool).await.expect("collate");
    assert!(changed >= 1, "cms_snippet.slug is converted");
    assert_eq!(rustango_cms::mysql_text::exact_identity_columns(&pool).await.expect("rerun"), 0);

    let mut lower = snippet(String::new());
    lower.slug = "hero".to_owned();
    lower.save_pool(&pool).await.expect("hero coexists with Hero");
    let found = Snippet::objects()
        .where_(Snippet::slug.eq("HERO".to_owned()))
        .first(&pool)
        .await
        .expect("query");
    assert!(found.is_none(), "no case-folded match");
}
