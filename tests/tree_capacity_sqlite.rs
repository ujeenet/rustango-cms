//! #678 — a tenant can create pages after its row ids pass 65 535.
//!
//! A tree path segment is the node's row id. The encoding used to stop at
//! 4 hex digits, so once a tenant's id sequence passed 65 535 every page
//! create failed with `SegmentOverflow`, permanently — deleting pages does
//! not give ids back. This drives the real create path past that point.
#![cfg(feature = "sqlite")]

use rustango::core::{Column as _, Model as _};
use rustango::sql::{Auto, FetcherPool as _, Pool};

use rustango_cms::page::{Page, PageStatus};
use rustango_cms::page_type_model::PageType;
use rustango_cms::tree_ops::NewPage;

async fn ddl(pool: &Pool, schema: &rustango::core::ModelSchema) {
    let sql =
        rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .expect("ddl");
    }
}

fn new_page(type_id: i64, title: &str, slug: &str) -> NewPage {
    NewPage {
        page_type_id: type_id,
        title: title.to_owned(),
        slug: slug.to_owned(),
        status: PageStatus::Published,
        seo_title: String::new(),
        seo_description: String::new(),
    }
}

#[tokio::test]
async fn pages_can_be_created_after_ids_pass_65535() {
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
    let type_id = pt.id.get().copied().expect("page type id");

    let root = Page::create_root_pool(&pool, new_page(type_id, "Home", "home"))
        .await
        .expect("root");
    assert_eq!(root.path, "0001/");

    // Stand in for a long-lived tenant: move the root to id 69 999 so the
    // next insert is handed 70 000, past the old 4-hex-digit ceiling.
    rustango::sql::raw_execute_pool(&pool, "UPDATE cms_page SET id = 69999 WHERE id = 1", Vec::new())
        .await
        .expect("advance id");
    let root = Page::objects()
        .where_(Page::id.eq(69_999_i64))
        .first(&pool)
        .await
        .expect("query")
        .expect("root row");

    let child = Page::create_child_pool(&pool, &root, new_page(type_id, "Deep", "deep"))
        .await
        .expect("a page with id 70000 can be created");
    assert_eq!(child.id.get().copied(), Some(70_000));
    assert_eq!(child.path, "0001/~00011170/");
    assert_eq!(child.depth, 2);
    assert_eq!(child.url_path, "/home/deep");

    let grandchild = Page::create_child_pool(&pool, &child, new_page(type_id, "Deeper", "deeper"))
        .await
        .expect("and a child under it");
    assert_eq!(grandchild.path, "0001/~00011170/~00011171/");
    assert_eq!(grandchild.depth, 3);

    // Subtree queries are path-prefix LIKEs; they must see the wide segments.
    let subtree: Vec<Page> = Page::objects()
        .where_(Page::path.like(rustango_cms::tree::descendants_like(&child.path)))
        .where_(Page::path.ne(child.path.clone()))
        .fetch(&pool)
        .await
        .expect("descendants");
    assert_eq!(subtree.len(), 1);
    assert_eq!(subtree[0].id.get().copied(), grandchild.id.get().copied());
}
