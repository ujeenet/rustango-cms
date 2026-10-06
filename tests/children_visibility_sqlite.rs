//! #688 — the default public children listing holds only live pages.
#![cfg(feature = "sqlite")]

use chrono::{Duration, Utc};
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};

use rustango_cms::page::{Page, PageStatus};
use rustango_cms::{all_children, default_children};
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
    parent: Option<i64>,
    status: PageStatus,
    go_live: Option<chrono::DateTime<Utc>>,
    expire: Option<chrono::DateTime<Utc>>,
) -> i64 {
    let mut p = Page {
        id: Auto::Unset,
        page_type_id: type_id,
        title: format!("p{n}"),
        slug: format!("p{n}"),
        path: match parent {
            Some(_) => format!("0001/{n:04x}/"),
            None => format!("{n:04x}/"),
        },
        url_path: format!("/p{n}"),
        preview_path: String::new(),
        template_override: String::new(),
        depth: 1,
        parent_id: parent,
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

#[tokio::test]
async fn default_children_lists_only_what_a_visitor_can_open() {
    let (pool, t) = setup().await;
    let now = Utc::now();
    let root = page(&pool, t, 1, None, PageStatus::Published, None, None).await;
    let p = Some(root);
    let live = page(&pool, t, 2, p, PageStatus::Published, None, None).await;
    let _draft = page(&pool, t, 3, p, PageStatus::Draft, None, None).await;
    let _gone = page(&pool, t, 4, p, PageStatus::Published, None, Some(now - Duration::minutes(1))).await;
    let due = page(&pool, t, 5, p, PageStatus::Scheduled, Some(now - Duration::minutes(1)), None).await;
    let _later = page(&pool, t, 6, p, PageStatus::Scheduled, Some(now + Duration::hours(1)), None).await;
    let _expired = page(&pool, t, 7, p, PageStatus::Expired, None, None).await;

    let parent = Page::objects().first(&pool).await.unwrap().unwrap();
    assert_eq!(parent.id.get().copied(), Some(root));
    let ids = |v: Vec<Page>| v.into_iter().filter_map(|p| p.id.get().copied()).collect::<Vec<_>>();

    assert_eq!(ids(default_children(&pool, &parent).await.unwrap()), vec![live, due]);
    assert_eq!(all_children(&pool, &parent).await.unwrap().len(), 6, "the admin helper keeps everything");
}

/// Error pages are served for a status code, never listed as content: a
/// home page's cards must not show "Page not found".
#[tokio::test]
async fn default_children_leave_out_error_pages() {
    let (pool, t) = setup().await;
    let mut err_type = PageType {
        id: Auto::Unset,
        app_label: "cms".to_owned(),
        type_name: rustango_cms::error_pages::TYPE_NAME.to_owned(),
        verbose_name: "Error page".to_owned(),
        default_template: "error_page.html".to_owned(),
        view_mode: "auto".to_owned(),
        is_creatable: true,
        allowed_parent_types: serde_json::json!([]),
        allowed_child_types: serde_json::json!([]),
        workflow: String::new(),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    err_type.save_pool(&pool).await.expect("error type");
    let e = err_type.id.get().copied().expect("id");
    let root = page(&pool, t, 1, None, PageStatus::Published, None, None).await;
    let about = page(&pool, t, 2, Some(root), PageStatus::Published, None, None).await;
    let _not_found = page(&pool, e, 3, Some(root), PageStatus::Published, None, None).await;

    let parent = Page::objects().first(&pool).await.unwrap().unwrap();
    let ids: Vec<i64> = default_children(&pool, &parent)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|p| p.id.get().copied())
        .collect();
    assert_eq!(ids, vec![about]);
}
