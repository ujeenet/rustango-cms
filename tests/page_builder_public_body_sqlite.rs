//! #844 — the default public body of an admin-made page type renders each
//! field by kind: a photo as an `<img>`, rich text as HTML, a choice by its
//! label. It used to print the photo's id and escape the rich text.
#![cfg(feature = "sqlite")]

use rustango::sql::{Auto, Pool};
use serde_json::json;

use rustango_cms::media::Media;
use rustango_cms::page::Page;
use rustango_cms::page_builder::{PageBuilderData, PageTypeSchema};
use rustango_cms::page_type_model::PageType;

async fn setup() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    for entry in inventory::iter::<rustango::core::ModelEntry> {
        if !entry.schema.managed {
            continue;
        }
        let sql = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), entry.schema);
        for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            rustango::sql::raw_execute_pool(&pool, stmt, Vec::new())
                .await
                .unwrap_or_else(|e| panic!("ddl for {}: {e}", entry.schema.table));
        }
    }
    pool
}

#[tokio::test]
async fn the_default_body_renders_fields_by_kind() {
    let pool = setup().await;

    let mut pt = PageType {
        id: Auto::Unset,
        app_label: "cms_ui".into(),
        type_name: "product".into(),
        verbose_name: "Product".into(),
        default_template: "rcms_admin/schema_page.html".into(),
        view_mode: "auto".into(),
        is_creatable: true,
        allowed_parent_types: json!([]),
        allowed_child_types: json!([]),
        workflow: String::new(),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    pt.save_pool(&pool).await.expect("page type");
    let type_id = pt.id.get().copied().expect("type id");

    let mut schema = PageTypeSchema {
        id: Auto::Unset,
        page_type_id: type_id,
        status: "published".into(),
        version: 1,
        document: json!({ "nodes": [
            { "kind": "field", "key": "price", "label": "Price", "widget": "number" },
            { "kind": "field", "key": "glaze", "label": "Glaze", "widget": "select",
              "options": [{ "value": "blue", "label": "Deep blue", "labels": { "fr": "Bleu profond" } },
                          ["white", "Satin white"]] },
            { "kind": "field", "key": "photo", "label": "Photo", "widget": "mediapicker" },
            { "kind": "field", "key": "description", "label": "Description", "widget": "richtext" }
        ]}),
        created_by: None,
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    schema.save_pool(&pool).await.expect("schema");

    let mut media = Media {
        id: Auto::Unset,
        filename: "blue-mug.png".into(),
        content_hash: "abc123".into(),
        mime: "image/png".into(),
        size: 100,
        kind: "image".into(),
        width: Some(900),
        height: Some(900),
        storage_key: "blue-mug.png".into(),
        title: "Blue mug".into(),
        alt_text: "A blue mug".into(),
        description: String::new(),
        uploaded_by: None,
        collection_id: None,
        focal_point_x: None,
        focal_point_y: None,
        uploaded_at: Auto::Unset,
    };
    media.save_pool(&pool).await.expect("media");
    let media_id = media.id.get().copied().expect("media id");

    let mut page = Page {
        id: Auto::Unset,
        page_type_id: type_id,
        title: "Blue mug".into(),
        slug: "blue-mug".into(),
        path: "0001/".into(),
        url_path: "/blue-mug".into(),
        preview_path: String::new(),
        template_override: String::new(),
        depth: 1,
        parent_id: None,
        locale_variant_of: None,
        alias_of: None,
        theme_id: None,
        sort_order: 0,
        status: "published".into(),
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
        twitter_card: "summary".into(),
        notification_pre_published_sent: false,
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    page.save_pool(&pool).await.expect("page");
    let page_id = page.id.get().copied().expect("page id");

    let mut data = PageBuilderData {
        id: Auto::Unset,
        page_id,
        schema_version: 1,
        component_versions: json!({}),
        data: json!({
            "price": 24.0,
            "glaze": "blue",
            "photo": media_id,
            "description": "<p>Thrown <strong>by hand</strong>.</p><script>x</script>"
        }),
        updated_at: Auto::Unset,
    };
    data.save_pool(&pool).await.expect("builder data");

    let mut tera = tera::Tera::default();
    rustango_cms::admin::register_templates(&mut tera).expect("templates");
    let (_values, _zones, body) = rustango_cms::page_builder::values::public_render_ctx(
        &pool,
        page_id,
        type_id,
        None,
        &std::collections::HashMap::new(),
        &tera,
        None,
        Some(page_id),
        "acme",
    )
    .await
    .expect("the type has a published schema");

    assert!(body.contains("<img"), "photo is an image: {body}");
    assert!(body.contains(&format!("/{media_id}")), "rendition of the chosen photo: {body}");
    assert!(body.contains("<strong>by hand</strong>"), "rich text is HTML: {body}");
    assert!(!body.contains("<script>"), "and sanitized: {body}");
    assert!(body.contains("Deep blue"), "a choice shows its label: {body}");
    assert!(body.contains(r#"<span class="pb-value">24</span>"#), "a whole number without .0: {body}");

    // The first photo field is the page's share image when none is chosen.
    let built = rustango_cms::page_builder::values::public_render(
        &pool,
        page_id,
        type_id,
        None,
        &std::collections::HashMap::new(),
        &tera,
        None,
        Some(page_id),
        "acme",
        None,
    )
    .await
    .expect("the type has a published schema");
    assert_eq!(built.first_media_id, Some(media_id));
    // #863 — a choice's display label, in the default language and in
    // one it has a translation for; the stored value stays as it is.
    assert_eq!(built.labels["glaze"], "Deep blue");
    assert_eq!(built.values["glaze"], "blue");
    let fr = rustango_cms::page_builder::values::public_render(
        &pool,
        page_id,
        type_id,
        None,
        &std::collections::HashMap::new(),
        &tera,
        None,
        Some(page_id),
        "acme",
        Some("fr"),
    )
    .await
    .expect("rendered");
    assert_eq!(fr.labels["glaze"], "Bleu profond");
    assert!(fr.body_html.contains("Bleu profond"), "the generic body too: {}", fr.body_html);

    // A listing gets the same for each child, batched: values with the
    // child's text translations applied, and localized choice labels.
    let mut tr = std::collections::HashMap::new();
    tr.insert(page_id, std::collections::HashMap::new());
    let kids = rustango_cms::page_builder::values::children_builder(&pool, &[page.clone()], &tr, Some("fr"))
        .await
        .expect("children");
    let (values, labels, photo) = &kids[&page_id];
    assert_eq!(*photo, Some(media_id), "the card image when no share image is chosen");
    assert_eq!(values["glaze"], "blue");
    assert_eq!(labels["glaze"], "Bleu profond");
    assert_eq!(fr.labels.get("price"), None, "only choice fields have labels");
}
