//! #706 — moving a page moves its subtree's URLs in the same
//! transaction, so a library caller of `move_to_in_tx` (and `move_to`)
//! never leaves the tree and the URLs disagreeing.
#![cfg(feature = "sqlite")]

use rustango::core::Column as _;
use rustango::sql::{transaction_pool, Auto, Pool};
use rustango_cms::page::Page;
use rustango_cms::page_type_model::PageType;
use rustango_cms::tree_ops::NewPage;

async fn file_pool(tag: &str) -> Pool {
    let path = std::env::temp_dir().join(format!("rcms-move-{tag}-{}.db", std::process::id()));
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

async fn get(pool: &Pool, id: i64) -> Page {
    Page::objects()
        .where_(Page::id.eq(id))
        .first(pool)
        .await
        .expect("query")
        .expect("page")
}

#[tokio::test]
async fn a_move_carries_every_url_in_the_subtree() {
    let pool = file_pool("urls").await;
    let tid = PageType::objects().first(&pool).await.expect("q").expect("type").id.get().copied().expect("tid");
    let blog = Page::create_root_pool(&pool, NewPage::new(tid, "Blog", "blog")).await.expect("blog");
    let post = Page::create_child_pool(&pool, &blog, NewPage::new(tid, "Post", "post")).await.expect("post");
    let note = Page::create_child_pool(&pool, &post, NewPage::new(tid, "Note", "note")).await.expect("note");
    let news = Page::create_root_pool(&pool, NewPage::new(tid, "News", "news")).await.expect("news");
    assert_eq!(get(&pool, note.id.get().copied().unwrap()).await.url_path, "/blog/post/note");

    let mut moving = get(&pool, post.id.get().copied().unwrap()).await;
    let subtree = moving.descendants(&pool).await.expect("subtree");
    let mut tx = transaction_pool(&pool).await.expect("tx");
    moving.move_to_in_tx(Some(&news), subtree, &mut tx).await.expect("move");
    tx.commit().await.expect("commit");

    let moved = get(&pool, post.id.get().copied().unwrap()).await;
    let child = get(&pool, note.id.get().copied().unwrap()).await;
    assert_eq!(moved.url_path, "/news/post");
    assert_eq!(child.url_path, "/news/post/note", "descendants follow");
    assert!(child.path.starts_with(&news.path), "the tree moved too");
    assert_eq!(moving.url_path, "/news/post", "the caller's copy is current");
}
