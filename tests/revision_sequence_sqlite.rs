//! #762 — revision sequences stay unique per page, under concurrent saves
//! and over history written before the index existed.
#![cfg(feature = "sqlite")]

use rustango::core::Column as _;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango_cms::page::{Page, PageStatus};
use rustango_cms::page_type_model::PageType;
use rustango_cms::revision::{capture, ensure_unique_sequence, Revision};

async fn file_pool(tag: &str) -> Pool {
    let path = std::env::temp_dir().join(format!("rcms-rev-{tag}-{}.db", std::process::id()));
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
    for id in [3, 7, 8] {
        page(id).insert_pool(&pool).await.expect("page");
    }
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

async fn sequences(pool: &Pool, page_id: i64) -> Vec<i32> {
    let mut seqs: Vec<i32> = Revision::objects()
        .where_(Revision::page_id.eq(page_id))
        .fetch(pool)
        .await
        .expect("revisions")
        .into_iter()
        .map(|r| r.sequence)
        .collect();
    seqs.sort_unstable();
    seqs
}

async fn raw_revision(pool: &Pool, page_id: i64, sequence: i32) -> Result<(), rustango::sql::ExecError> {
    let mut r = Revision {
        id: Auto::Unset,
        page_id,
        sequence,
        snapshot: serde_json::json!({}),
        created_by: None,
        created_at: Auto::Unset,
    };
    r.insert_pool(pool).await
}

#[tokio::test]
async fn existing_duplicates_are_renumbered_and_then_refused() {
    let pool = file_pool("dup").await;
    for seq in [1, 2, 2, 3] {
        raw_revision(&pool, 7, seq).await.expect("legacy row");
    }
    raw_revision(&pool, 8, 1).await.expect("other page");

    ensure_unique_sequence(&pool).await.expect("ensure");
    assert_eq!(sequences(&pool, 7).await, vec![1, 2, 3, 4], "renumbered in order");
    assert_eq!(sequences(&pool, 8).await, vec![1], "untouched");
    ensure_unique_sequence(&pool).await.expect("idempotent");

    assert!(raw_revision(&pool, 7, 4).await.is_err(), "a duplicate is now refused");
    let next = capture(&pool, &page(7), None).await.expect("capture");
    assert_eq!(next.sequence, 5);
}

#[tokio::test]
async fn concurrent_captures_get_distinct_sequences() {
    let pool = file_pool("race").await;
    ensure_unique_sequence(&pool).await.expect("ensure");
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move { capture(&pool, &page(3), None).await })
        })
        .collect();
    for h in handles {
        h.await.expect("join").expect("each save captures");
    }
    assert_eq!(sequences(&pool, 3).await, (1..=8).collect::<Vec<_>>());
}
