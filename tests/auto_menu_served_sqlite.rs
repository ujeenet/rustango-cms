//! #763 — the automatic menu lists the pages the resolver serves:
//! archived ones and scheduled ones past go-live included, drafts, expired
//! and not-yet-live pages not.
#![cfg(feature = "sqlite")]

use rustango::sql::{Auto, Pool};
use rustango_cms::page::{Page, PageStatus};
use rustango_cms::page_type_model::PageType;

async fn file_pool(tag: &str) -> Pool {
    let path = std::env::temp_dir().join(format!("rcms-menu-{tag}-{}.db", std::process::id()));
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
    let mut pt = PageType {
        id: Auto::Unset,
        app_label: "cms".to_owned(),
        type_name: "TestPage".to_owned(),
        verbose_name: "Test page".to_owned(),
        default_template: "page.html".to_owned(),
        view_mode: "auto".to_owned(),
        is_creatable: true,
        allowed_parent_types: serde_json::json!([]),
        allowed_child_types: serde_json::json!([]),
        workflow: String::new(),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    pt.save_pool(&pool).await.expect("page type");
    pool
}

fn page(id: i64) -> Page {
    Page {
        id: Auto::Set(id),
        page_type_id: 1,
        title: "P".to_owned(),
        slug: format!("p{id}"),
        path: String::new(),
        url_path: format!("/p{id}"),
        preview_path: String::new(),
        template_override: String::new(),
        depth: 1,
        parent_id: None,
        locale_variant_of: None,
        alias_of: None,
        theme_id: None,
        sort_order: 0,
        status: PageStatus::Draft.as_str().to_owned(),
        published_at: None,
        last_published_at: None,
        go_live_at: None,
        expire_at: None,
        seo_title: String::new(),
        seo_description: String::new(),
        robots_index: true,
        sitemap_priority: 0.5,
        show_in_menus: true,
        og_title: String::new(),
        og_description: String::new(),
        og_image_media_id: None,
        twitter_card: "summary".to_owned(),
        notification_pre_published_sent: false,
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    }
}

#[tokio::test]
async fn the_auto_menu_lists_what_the_resolver_serves() {
    let pool = file_pool("served").await;
    let hour = chrono::Duration::hours(1);
    let now = chrono::Utc::now();
    let rows: [(i64, PageStatus, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>, bool); 6] = [
        (1, PageStatus::Published, None, None, true),
        (2, PageStatus::Archived, None, None, true),
        (3, PageStatus::Scheduled, Some(now - hour), None, true),
        (4, PageStatus::Scheduled, Some(now + hour), None, false),
        (5, PageStatus::Draft, None, None, false),
        (6, PageStatus::Published, None, Some(now - hour), false),
    ];
    for (id, status, go_live, expire, _) in &rows {
        let mut p = page(*id);
        p.status = status.as_str().to_owned();
        p.go_live_at = *go_live;
        p.expire_at = *expire;
        p.insert_pool(&pool).await.expect("page");
    }
    let listed: Vec<String> = rustango_cms::auto_menu::prefetch(&pool)
        .await
        .into_iter()
        .map(|r| r.url_path)
        .collect();
    for (id, _, _, _, shown) in rows {
        assert_eq!(listed.contains(&format!("/p{id}")), shown, "page {id}: {listed:?}");
    }
}
