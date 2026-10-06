//! #741 — a window full of backed-off retries does not starve due rows.
#![cfg(feature = "sqlite")]

use chrono::{Duration, Utc};
use rustango::casts::Cast;
use rustango::core::{Column as _, Model as _};
use rustango::sql::{Auto, FetcherPool as _, Pool};

use rustango_cms::notify::model::{
    NotificationDelivery, NotificationTarget, STATUS_FAILED, STATUS_PENDING,
};
use rustango_cms::notify::worker::{drain, DrainCtx, DrainReport};

async fn ddl(pool: &Pool, schema: &rustango::core::ModelSchema) {
    let sql =
        rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .expect("ddl");
    }
}

async fn delivery(pool: &Pool, target_id: i64, age_mins: i64, next_in_secs: i64) -> i64 {
    let mut row = NotificationDelivery {
        id: Auto::Unset,
        target_id,
        event: "form.submitted".to_owned(),
        payload: "{}".to_owned(),
        status: STATUS_PENDING.to_owned(),
        attempts: 1,
        last_error: String::new(),
        next_attempt_at: Some(Utc::now() + Duration::seconds(next_in_secs)),
        created_at: Auto::Set(Utc::now() - Duration::minutes(age_mins)),
        sent_at: None,
    };
    row.insert_pool(pool).await.expect("delivery");
    row.id.get().copied().expect("delivery id")
}

#[tokio::test]
async fn backed_off_rows_do_not_starve_a_due_one() {
    // The target's secret is an encrypted column. This binary holds one
    // test, so setting the key races nothing.
    std::env::set_var("RUSTANGO_SECRET_KEY", "notify-drain-test-key");
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    ddl(&pool, &NotificationTarget::SCHEMA).await;
    ddl(&pool, &NotificationDelivery::SCHEMA).await;
    // No channel is registered for this kind, so a due row fails at once
    // instead of making a network call — enough to show it was reached.
    let mut target = NotificationTarget {
        id: Auto::Unset,
        label: "t".to_owned(),
        kind: "test-no-such-channel".to_owned(),
        config: "{}".to_owned(),
        secret: Cast::new(String::new().into()),
        events: String::new(),
        source_id: 0,
        enabled: true,
        created_at: Auto::Unset,
    };
    target.insert_pool(&pool).await.expect("target");
    let target_id = target.id.get().copied().expect("target id");

    // 60 older rows, all backing off: more than one drain window.
    for age in 0..60 {
        delivery(&pool, target_id, 120 - age, 600).await;
    }
    let due = delivery(&pool, target_id, 0, -1).await;

    let ctx = DrainCtx { mailer: None, mailer_from: "" };
    let report = drain(&pool, &ctx, 50).await.expect("drain");
    assert_eq!(report, DrainReport { sent: 0, retried: 0, failed: 1 });

    let row: NotificationDelivery = NotificationDelivery::objects()
        .where_(NotificationDelivery::id.eq(due))
        .first(&pool)
        .await
        .expect("fetch")
        .expect("row");
    assert_eq!(row.status, STATUS_FAILED);
    let pending = NotificationDelivery::objects()
        .where_(NotificationDelivery::status.eq(STATUS_PENDING.to_owned()))
        .fetch(&pool)
        .await
        .expect("pending");
    assert_eq!(pending.len(), 60, "backed-off rows are left alone");
}
