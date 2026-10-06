//! #689 — a tenant provisioned after boot is seeded on first use, once.
#![cfg(feature = "sqlite")]

use rustango::sql::{CounterPool as _, Pool};
use rustango_cms::page_type_model::PageType;

async fn file_pool(tag: &str) -> Pool {
    let path = std::env::temp_dir().join(format!("rcms-seed-{tag}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let pool = Pool::connect(&format!("sqlite:{}?mode=rwc", path.display())).await.expect("pool");
    for entry in inventory::iter::<rustango::core::ModelEntry> {
        if !entry.schema.managed {
            continue;
        }
        let sql = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), entry.schema);
        for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            rustango::sql::raw_execute_pool(&pool, stmt, Vec::new()).await.expect("ddl");
        }
    }
    pool
}

fn org(slug: &str) -> rustango::tenancy::Org {
    rustango::tenancy::Org {
        id: rustango::Auto::Unset,
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        storage_mode: "database".to_owned(),
        backend_kind: "sqlite".to_owned(),
        database_url: None,
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
    }
}

#[tokio::test]
async fn a_late_tenant_is_seeded_once() {
    let pool = file_pool("late").await;
    assert_eq!(PageType::objects().count(&pool).await.expect("count"), 0, "fresh tenant");

    let late = org("late-tenant");
    rustango_cms::seed::ensure_tenant_seeded(&pool, &late).await.expect("seed");
    let types = PageType::objects().count(&pool).await.expect("count");
    assert!(types > 0, "page types seeded");
    let roles = rustango::tenancy::permissions::Role::objects().count(&pool).await.expect("roles");
    assert!(roles >= 3, "default roles seeded: {roles}");

    // Marked seeded: a second call doesn't run the seed again.
    rustango::sql::raw_execute_pool(&pool, "DELETE FROM cms_page_type", Vec::new()).await.expect("wipe");
    rustango_cms::seed::ensure_tenant_seeded(&pool, &late).await.expect("again");
    assert_eq!(PageType::objects().count(&pool).await.expect("count"), 0, "not re-run");
}
