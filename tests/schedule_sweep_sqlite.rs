//! #680 — the schedule sweep flips only due pages, by column, idempotently.
#![cfg(feature = "sqlite")]

use chrono::{Duration, Utc};
use rustango::core::{Column as _, Model as _};
use rustango::sql::{Auto, Pool};

use rustango_cms::page::{run_schedule_sweep, Page, PageStatus};
use rustango_cms::page_type_model::PageType;

async fn ddl(pool: &Pool, schema: &rustango::core::ModelSchema) {
    let sql =
        rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .expect("ddl");
    }
}

async fn setup() -> (Pool, i64) {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    for schema in [
        &PageType::SCHEMA,
        &Page::SCHEMA,
        &rustango_cms::media::Media::SCHEMA,
        &rustango_cms::theme::Theme::SCHEMA,
    ] {
        ddl(&pool, schema).await;
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
    (pool, pt.id.get().copied().expect("type id"))
}

async fn page(
    pool: &Pool,
    type_id: i64,
    n: i64,
    status: PageStatus,
    go_live: Option<chrono::DateTime<Utc>>,
    expire: Option<chrono::DateTime<Utc>>,
) -> i64 {
    let mut p = Page {
        id: Auto::Unset,
        page_type_id: type_id,
        title: format!("p{n}"),
        slug: format!("p{n}"),
        path: format!("{n:04x}/"),
        url_path: format!("/p{n}"),
        preview_path: String::new(),
        template_override: String::new(),
        depth: 1,
        parent_id: None,
        locale_variant_of: None,
        alias_of: None,
        theme_id: None,
        sort_order: 0,
        status: status.as_str().to_owned(),
        published_at: None,
        last_published_at: None,
        go_live_at: go_live,
        expire_at: expire,
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
    };
    p.save_pool(pool).await.expect("page");
    p.id.get().copied().expect("id")
}

async fn status(pool: &Pool, id: i64) -> String {
    Page::objects()
        .where_(Page::id.eq(id))
        .first(pool)
        .await
        .expect("query")
        .expect("row")
        .status
}

#[tokio::test]
async fn the_sweep_flips_only_due_pages_and_is_idempotent() {
    let (pool, t) = setup().await;
    let now = Utc::now();
    let due = page(&pool, t, 1, PageStatus::Scheduled, Some(now - Duration::minutes(1)), None).await;
    let later = page(&pool, t, 2, PageStatus::Scheduled, Some(now + Duration::hours(1)), None).await;
    let gone = page(&pool, t, 3, PageStatus::Published, None, Some(now - Duration::minutes(1))).await;
    let live = page(&pool, t, 4, PageStatus::Published, None, Some(now + Duration::hours(1))).await;
    let draft = page(&pool, t, 5, PageStatus::Draft, Some(now - Duration::minutes(1)), None).await;

    let r = run_schedule_sweep(&pool).await.expect("sweep");
    assert_eq!((r.published, r.expired), (1, 1));
    assert_eq!(r.changed_urls.len(), 2);
    // #692 — the sweep reports which pages went live and which went away,
    // so its callers can run the same go-live effects as any publish.
    assert_eq!(r.went_live.iter().filter_map(|p| p.id.get().copied()).collect::<Vec<_>>(), vec![due]);
    assert_eq!(r.went_live[0].status, "published", "reported as it now is");
    assert_eq!(r.taken_down, vec![gone]);

    assert_eq!(status(&pool, due).await, "published");
    assert_eq!(status(&pool, later).await, "scheduled");
    assert_eq!(status(&pool, gone).await, "expired");
    assert_eq!(status(&pool, live).await, "published");
    assert_eq!(status(&pool, draft).await, "draft", "drafts are never auto-published");

    let went_live = Page::objects().where_(Page::id.eq(due)).first(&pool).await.unwrap().unwrap();
    assert!(went_live.published_at.is_some() && went_live.last_published_at.is_some());

    // Nothing is due any more: a second tick changes nothing.
    let again = run_schedule_sweep(&pool).await.expect("sweep");
    assert_eq!((again.published, again.expired), (0, 0));
}
