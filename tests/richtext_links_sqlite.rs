//! #682 — rich-text internal links resolve against the database.
//!
//! The editor stores links as `<a linktype="page|media" id="N">`. Before
//! this, only pages an extension `*_id` field happened to reference were
//! resolvable, and media links never were, so internal links rendered as
//! `href="#"`.
#![cfg(feature = "sqlite")]

use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};

use rustango_cms::media::Media;
use rustango_cms::page::{Page, PageStatus};
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

#[tokio::test]
async fn page_and_document_links_resolve_and_missing_ones_go_inert() {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    for schema in [&PageType::SCHEMA, &Page::SCHEMA, &rustango_cms::media::MediaCollection::SCHEMA, &Media::SCHEMA, &rustango_cms::theme::Theme::SCHEMA] {
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

    let mut about = Page {
        id: Auto::Unset,
        page_type_id: pt.id.get().copied().expect("type id"),
        title: "About".to_owned(),
        slug: "about".to_owned(),
        path: "0001/".to_owned(),
        url_path: "/about".to_owned(),
        preview_path: String::new(),
        template_override: String::new(),
        depth: 1,
        parent_id: None,
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
    about.save_pool(&pool).await.expect("page");
    let page_id = about.id.get().copied().expect("page id");

    let mut doc = Media {
        id: Auto::Unset,
        filename: "report.pdf".to_owned(),
        content_hash: "h".to_owned(),
        mime: "application/pdf".to_owned(),
        size: 1,
        kind: "document".to_owned(),
        width: None,
        height: None,
        storage_key: "k".to_owned(),
        title: "Report".to_owned(),
        alt_text: String::new(),
        description: String::new(),
        uploaded_by: None,
        collection_id: None,
        focal_point_x: None,
        focal_point_y: None,
        uploaded_at: Auto::Unset,
    };
    doc.save_pool(&pool).await.expect("media");
    let media_id = doc.id.get().copied().expect("media id");

    let html = format!(
        r#"<p><a linktype="page" id="{page_id}">About us</a>, <a linktype="media" id="{media_id}">the report</a>, <a linktype="media" id="999">gone</a>, <a href="/plain">plain</a></p>"#
    );
    let out = rustango_cms::richtext::resolve_internal_links(&pool, html).await;
    assert_eq!(
        out,
        format!(
            r##"<p><a href="/about">About us</a>, <a href="/__media__/raw/{media_id}">the report</a>, <a href="#">gone</a>, <a href="/plain">plain</a></p>"##
        )
    );
}
