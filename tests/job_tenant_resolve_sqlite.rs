//! #732 — a queued job for a tenant this process never served resolves
//! the tenant's pool from the registry instead of failing for good.
#![cfg(feature = "sqlite")]

use rustango::core::{Column as _, Model as _};
use rustango::jobs::Job as _;
use rustango::sql::{CounterPool as _, Pool};
use rustango::tenancy::Org;
use rustango_cms::reference_index::ReferenceIndex;
use rustango_cms::task_queue::{OwnedRef, ReferenceIndexJob};

async fn ddl(pool: &Pool, schema: &rustango::core::ModelSchema) {
    let sql = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new()).await.expect("ddl");
    }
}

#[tokio::test]
async fn a_job_finds_an_unregistered_tenant_through_the_registry() {
    let dir = std::env::temp_dir().join(format!("rcms-job-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let (reg_path, ten_path) = (dir.join("registry.db"), dir.join("acme.db"));
    for p in [&reg_path, &ten_path] {
        let _ = std::fs::remove_file(p);
    }
    let tenant_url = format!("sqlite:{}?mode=rwc", ten_path.display());
    let tenant = Pool::connect(&tenant_url).await.expect("tenant");
    ddl(&tenant, &ReferenceIndex::SCHEMA).await;

    let registry = Pool::connect(&format!("sqlite:{}?mode=rwc", reg_path.display())).await.expect("registry");
    ddl(&registry, &Org::SCHEMA).await;
    let mut org = Org {
        id: rustango::Auto::Unset,
        slug: "acme".to_owned(),
        display_name: "Acme".to_owned(),
        storage_mode: "database".to_owned(),
        backend_kind: "sqlite".to_owned(),
        database_url: Some(tenant_url),
        schema_name: None,
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: chrono::Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    org.save_pool(&registry).await.expect("org");
    rustango_cms::task_queue::set_registry(registry);

    let job = ReferenceIndexJob {
        tenant_slug: "acme".to_owned(),
        page_id: 7,
        refs: vec![OwnedRef { to_kind: "cms_media".to_owned(), to_id: 3, field_path: "body".to_owned() }],
    };
    job.run().await.expect("resolved through the registry, not Fatal");
    let rows = ReferenceIndex::objects()
        .where_(ReferenceIndex::from_id.eq(7_i64))
        .count(&tenant)
        .await
        .expect("count");
    assert_eq!(rows, 1, "the reference row landed in the tenant's own database");

    let unknown = ReferenceIndexJob { tenant_slug: "gone".to_owned(), page_id: 1, refs: Vec::new() };
    assert!(unknown.run().await.is_err(), "a tenant the registry doesn't know still fails");
}
