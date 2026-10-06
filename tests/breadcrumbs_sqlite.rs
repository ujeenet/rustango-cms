//! #747 — breadcrumbs resolve the ancestor chain in one query.
#![cfg(feature = "sqlite")]

use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};

use rustango_cms::page::{Page, PageStatus};

use rustango_cms::breadcrumbs::{resolve_breadcrumbs, BreadcrumbsOptions};
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
    path: &str,
) -> i64 {
    let mut p = Page {
        id: Auto::Unset,
        page_type_id: type_id,
        title: format!("p{n}"),
        slug: format!("p{n}"),
        path: path.to_owned(),
        url_path: format!("/p{n}"),
        preview_path: String::new(),
        template_override: String::new(),
        depth: 1,
        parent_id: parent,
        locale_variant_of: None,
        alias_of: None,
        theme_id: None,
        sort_order: 0,
        status: PageStatus::Published.as_str().to_owned(),
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
    };
    p.save_pool(pool).await.expect("page");
    p.id.get().copied().expect("id")
}

#[tokio::test]
async fn crumbs_run_root_to_leaf_and_honour_the_options() {
    let (pool, t) = setup().await;
    let root = page(&pool, t, 1, None, "0001/").await;
    let mid = page(&pool, t, 2, Some(root), "0001/0002/").await;
    let leaf = page(&pool, t, 3, Some(mid), "0001/0002/0003/").await;
    let _sibling = page(&pool, t, 4, Some(root), "0001/0004/").await;

    let titles = |c: Vec<rustango_cms::breadcrumbs::Crumb>| c.into_iter().map(|c| c.title).collect::<Vec<_>>();
    let all = resolve_breadcrumbs(&pool, leaf, BreadcrumbsOptions::default()).await.unwrap();
    assert!(all.last().unwrap().is_current);
    assert_eq!(titles(all), ["p1", "p2", "p3"]);

    let near = BreadcrumbsOptions { max_depth: 1, ..BreadcrumbsOptions::default() };
    assert_eq!(titles(resolve_breadcrumbs(&pool, leaf, near).await.unwrap()), ["p2", "p3"]);

    let trimmed = BreadcrumbsOptions { include_root: false, include_current: false, ..BreadcrumbsOptions::default() };
    assert_eq!(titles(resolve_breadcrumbs(&pool, leaf, trimmed).await.unwrap()), ["p2"]);

    assert_eq!(titles(resolve_breadcrumbs(&pool, root, BreadcrumbsOptions::default()).await.unwrap()), ["p1"]);
    assert!(resolve_breadcrumbs(&pool, 999, BreadcrumbsOptions::default()).await.unwrap().is_empty());
}
