//! #653 — the bounce rate is counted in SQL, and counts what it did in Rust:
//! a single-pageview session whose longest engagement is under 10s.
#![cfg(feature = "sqlite")]

use chrono::{Duration, Utc};
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango_cms::analytics::model::AnalyticsEvent;

fn event(session: &str, kind: &str, duration_ms: i64) -> AnalyticsEvent {
    AnalyticsEvent {
        id: Auto::Unset,
        visitor_id: format!("v-{session}"),
        session_id: session.to_owned(),
        event_type: kind.to_owned(),
        path: "/".to_owned(),
        referrer_host: String::new(),
        country: String::new(),
        device: String::new(),
        browser: String::new(),
        os: String::new(),
        screen_w: 0,
        screen_h: 0,
        duration_ms,
        max_scroll_pct: 0,
        is_bounce: false,
        locale: String::new(),
        created_at: Utc::now() - Duration::minutes(5),
    }
}

#[tokio::test]
async fn bounce_rate_counts_short_single_page_sessions() {
    let pool = Pool::connect("sqlite::memory:").await.expect("pool");
    let sql = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
        pool.dialect(),
        &AnalyticsEvent::SCHEMA,
    );
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(&pool, stmt, Vec::new()).await.expect("ddl");
    }
    for mut e in [
        event("a", "pageview", 0),               // bounce: one view, no engagement
        event("b", "pageview", 0),               // not: two views
        event("b", "pageview", 0),
        event("c", "pageview", 0),               // not: engaged 20s
        event("c", "engagement", 20_000),
        event("d", "pageview", 0),               // bounce: engaged only 5s
        event("d", "engagement", 5_000),
    ] {
        e.insert_pool(&pool).await.expect("event");
    }
    let m = rustango_cms::analytics::query::compute(&pool, Utc::now() - Duration::hours(1), Utc::now())
        .await
        .expect("compute");
    assert_eq!(m.bounce_rate, 50, "2 of 4 sessions bounced");
}
