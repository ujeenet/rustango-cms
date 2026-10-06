//! A form's notification email can't be stored without `RUSTANGO_SECRET_KEY`
//! (the target's secret column is encrypted). The sync says so with an error
//! instead of panicking — it runs on every boot, and a panic there stopped
//! the site from starting once an editor had typed a notification address.
#![cfg(feature = "sqlite")]

use rustango::sql::{FetcherPool as _, Pool};

use rustango_cms::notify::model::NotificationTarget;

async fn ddl(pool: &Pool, schema: &rustango::core::ModelSchema) {
    let sql =
        rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .expect("ddl");
    }
}

#[tokio::test]
async fn without_the_key_the_sync_errors_instead_of_panicking() {
    // This binary holds one test, so removing the key races nothing.
    std::env::remove_var("RUSTANGO_SECRET_KEY");
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    ddl(&pool, &<NotificationTarget as rustango::core::Model>::SCHEMA).await;

    let err = rustango_cms::notify::targets::sync_form_email(&pool, 7, "Order", "orders@example.test")
        .await
        .expect_err("storing a target needs the key");
    assert!(err.to_string().contains("RUSTANGO_SECRET_KEY"), "names the missing key: {err}");
    let rows: Vec<NotificationTarget> = NotificationTarget::objects().fetch(&pool).await.expect("fetch");
    assert!(rows.is_empty(), "nothing half-written");

    // Clearing the recipients writes nothing encrypted, so it still works.
    rustango_cms::notify::targets::sync_form_email(&pool, 7, "Order", "")
        .await
        .expect("no recipients, no key needed");
}
