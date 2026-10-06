//! #648 — chooser enrichment resolves a stream's references with one
//! query per table, however many blocks point at them.
#![cfg(feature = "sqlite")]

use rustango::sql::{Auto, Pool};
use rustango::test_assertions::QueryCounter;
use rustango_cms::media::Media;
use rustango_cms::page::Page;
use rustango_cms::page_type_model::PageType;
use rustango_cms::tree_ops::NewPage;

async fn file_pool(tag: &str) -> Pool {
    let path = std::env::temp_dir().join(format!("rcms-enrich-{tag}-{}.db", std::process::id()));
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

async fn media(pool: &Pool, title: &str) -> i64 {
    let mut m = Media {
        id: Auto::Unset,
        filename: format!("{title}.png"),
        content_hash: format!("hash-{title}"),
        mime: "image/png".to_owned(),
        size: 1,
        kind: "image".to_owned(),
        width: Some(10),
        height: Some(10),
        storage_key: format!("{title}.png"),
        title: title.to_owned(),
        alt_text: format!("alt {title}"),
        description: String::new(),
        uploaded_by: None,
        collection_id: None,
        focal_point_x: None,
        focal_point_y: None,
        uploaded_at: Auto::Unset,
    };
    m.save_pool(pool).await.expect("media");
    m.id.get().copied().expect("id")
}

#[tokio::test]
async fn forty_images_cost_one_media_query() {
    let pool = file_pool("batch").await;
    let tid = PageType::objects().first(&pool).await.expect("q").expect("type").id.get().copied().unwrap();
    let about = Page::create_root_pool(&pool, NewPage::new(tid, "About", "about")).await.expect("page");
    let ids = [media(&pool, "a").await, media(&pool, "b").await, media(&pool, "c").await];

    let mut blocks: Vec<serde_json::Value> = (0..40)
        .map(|i| serde_json::json!({ "type": "image", "id": format!("b{i}"), "value": { "media_id": ids[i % 3].to_string() } }))
        .collect();
    blocks.push(serde_json::json!({ "type": "page_chooser", "id": "p", "value": { "page_id": about.id.get().copied().unwrap() } }));
    blocks.push(serde_json::json!({ "type": "nested_stream", "id": "n", "value": { "inner": [
        { "type": "image", "id": "deep", "value": { "media_id": ids[2] } }
    ] } }));
    let mut stream = serde_json::Value::Array(blocks);

    let queries = QueryCounter::scope(async {
        rustango_cms::block::tera_helpers::enrich_chooser_refs_async(&mut stream, &pool, None).await;
        QueryCounter::current()
    })
    .await;
    assert_eq!(queries, 2, "one media query and one page query, not one per block");

    assert_eq!(stream[0]["value"]["_alt_text"], "alt a");
    assert_eq!(stream[40]["value"]["_url"], "/about");
    assert_eq!(stream[41]["value"]["inner"][0]["value"]["_title"], "c", "nested blocks too");
}
