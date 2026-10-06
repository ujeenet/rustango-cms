//! Host-registered stream enrichment, through the real pre-render.
//!
//! The unit test beside the hook can only assert that nothing is
//! registered by default — an `inventory` submission is process-wide,
//! so the moment a test registers an enricher every test in that binary
//! sees it. This file is therefore the one place an enricher exists,
//! and it checks the thing that actually matters: that a value filled
//! in asynchronously reaches the synchronous block template.
#![cfg(feature = "sqlite")]

use std::future::Future;
use std::pin::Pin;

use rustango::core::Model as _;
use rustango::sql::Pool;
use rustango_cms::block::enrich::{any_registered, EnrichCtx, EnrichFuture};
use rustango_cms::widget::Widget;

/// Marks every `heading` block with a value only an async pass could
/// have supplied. A real host would run a query here; the point under
/// test is the plumbing, so this just proves the mutation survives the
/// hand-off to the sync renderer.
fn enrich<'a>(stream: &'a mut serde_json::Value, ctx: EnrichCtx<'a>) -> EnrichFuture<'a> {
    Box::pin(async move {
        // Touch the pool so the borrow in the signature is exercised —
        // a host's enricher holds it across an await, and a lifetime
        // that only compiles when unused would be a trap.
        let dialect = ctx.pool.dialect().name().to_owned();
        let Some(items) = stream.as_array_mut() else {
            return;
        };
        for item in items {
            // `rpt_probe` is a UI-defined group (a `DynBlockDef`), not a
            // code-registered block. An enricher matches on the `type`
            // string in raw JSON, so it cannot tell the two apart — which
            // is exactly the property the page-builder zone test relies on.
            let ty = item.get("type").and_then(|t| t.as_str());
            if ty != Some("heading") && ty != Some("rpt_probe") {
                continue;
            }
            if let Some(value) = item.get_mut("value").and_then(|v| v.as_object_mut()) {
                value.insert("_dialect".to_owned(), serde_json::json!(dialect));
                value.insert(
                    "_page".to_owned(),
                    serde_json::json!(ctx.page_id.unwrap_or(-1)),
                );
            }
        }
    })
}

rustango_cms::register_block_enricher!(enrich);

/// Force-link the submission. Without a reference the optimizer is
/// free to drop the static initializer, and the test would pass
/// vacuously against zero registered enrichers.
fn link() {
    let _: for<'a> fn(&'a mut serde_json::Value, EnrichCtx<'a>) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> =
        enrich;
}

async fn mem_pool() -> Pool {
    Pool::connect("sqlite::memory:").await.expect("mem pool")
}

fn stream_widget(name: &str, allowed: &[&str]) -> Vec<Widget> {
    vec![Widget::stream(name, "Body", allowed.iter().copied())]
}

#[tokio::test]
async fn a_registered_enricher_is_visible_to_the_fan_out() {
    link();
    assert!(
        any_registered(),
        "the enricher registered in this file should be collected"
    );
}

#[tokio::test]
async fn enriched_values_reach_the_block_template() {
    link();
    let pool = mem_pool().await;

    let mut tera = tera::Tera::default();
    // Stand in for the bundled `blocks/heading.html`. Reading `_dialect`
    // is the assertion: nothing but the async pass could have put it in
    // the value, and `render` itself cannot await.
    tera.add_raw_template("blocks/heading.html", "[{{ value._dialect }}]")
        .expect("template");

    let extension = serde_json::json!({
        "body": [
            { "type": "heading", "id": "a", "value": { "text": "Quarterly totals" } }
        ]
    });

    let out = rustango_cms::block::tera_helpers::prerender_extension_streams_async(
        &stream_widget("body", &["heading"]),
        &extension,
        &tera,
        &pool,
        &std::collections::HashMap::new(),
        None,
    )
    .await
    .expect("prerender");

    let html = out.get("body").expect("body stream rendered");
    assert!(
        html.contains("[sqlite]"),
        "enriched `_dialect` should have reached the template, got: {html}"
    );
}

/// A block that picks its template at render time, from a value the
/// enricher supplied. This is the shape a host needs for "each library
/// element renders through its own template": the element's template
/// name travels in the block value, and `render` resolves it against
/// the Tera it is handed — which the public router has already overlaid
/// with the tenant's own templates.
#[derive(Default)]
struct PerElementBlock;

impl rustango_cms::Block for PerElementBlock {
    fn type_name(&self) -> &'static str {
        "per_element"
    }
    fn verbose_name(&self) -> &'static str {
        "Per element"
    }
    fn fields(&self) -> Vec<rustango_cms::BlockField> {
        vec![rustango_cms::BlockField::char("slug", "Slug")]
    }
    fn render(
        &self,
        value: &serde_json::Value,
        ctx: &rustango_cms::block::BlockRenderCtx<'_>,
    ) -> Result<String, rustango_cms::block::BlockError> {
        let name = value
            .get("_template")
            .and_then(|v| v.as_str())
            .unwrap_or("blocks/per_element.html");
        let mut tctx = tera::Context::new();
        tctx.insert("value", value);
        ctx.tera.render(name, &tctx).map_err(|e| {
            rustango_cms::block::BlockError::TemplateRender {
                block_type: "per_element".to_owned(),
                template: name.to_owned(),
                source: e,
            }
        })
    }
}

rustango_cms::register_block!(PerElementBlock);

#[tokio::test]
async fn a_block_can_resolve_its_template_from_the_enriched_value() {
    link();
    let pool = mem_pool().await;

    let mut tera = tera::Tera::default();
    // Two elements of the same block type, each with its own markup.
    tera.add_raw_template("widgets/alpha.html", "ALPHA:{{ value.slug }}")
        .expect("alpha");
    tera.add_raw_template("widgets/beta.html", "BETA:{{ value.slug }}")
        .expect("beta");

    let extension = serde_json::json!({
        "body": [
            { "type": "per_element", "id": "1",
              "value": { "slug": "one", "_template": "widgets/alpha.html" } },
            { "type": "per_element", "id": "2",
              "value": { "slug": "two", "_template": "widgets/beta.html" } }
        ]
    });

    let out = rustango_cms::block::tera_helpers::prerender_extension_streams_async(
        &stream_widget("body", &["per_element"]),
        &extension,
        &tera,
        &pool,
        &std::collections::HashMap::new(),
        None,
    )
    .await
    .expect("prerender");

    let html = out.get("body").expect("body stream rendered");
    assert!(
        html.contains("ALPHA:one") && html.contains("BETA:two"),
        "each element should render through its own template, got: {html}"
    );
}

#[tokio::test]
async fn the_page_id_reaches_the_enricher_through_the_with_variant() {
    link();
    let pool = mem_pool().await;

    let mut tera = tera::Tera::default();
    tera.add_raw_template("blocks/heading.html", "[{{ value._page }}]")
        .expect("template");

    let extension = serde_json::json!({
        "body": [{ "type": "heading", "id": "a", "value": { "text": "t" } }]
    });

    let out = rustango_cms::block::tera_helpers::prerender_extension_streams_async_with(
        &stream_widget("body", &["heading"]),
        &extension,
        &tera,
        &pool,
        &std::collections::HashMap::new(),
        None,
        Some(42),
        "acme",
    )
    .await
    .expect("prerender");

    assert!(
        out.get("body").expect("rendered").contains("[42]"),
        "the page id should have reached the enricher"
    );
}

#[tokio::test]
async fn the_plain_variant_still_reports_no_page() {
    // The back-compat path: existing callers keep working and see
    // `None` rather than a wrong id.
    link();
    let pool = mem_pool().await;

    let mut tera = tera::Tera::default();
    tera.add_raw_template("blocks/heading.html", "[{{ value._page }}]")
        .expect("template");

    let extension = serde_json::json!({
        "body": [{ "type": "heading", "id": "a", "value": { "text": "t" } }]
    });

    let out = rustango_cms::block::tera_helpers::prerender_extension_streams_async(
        &stream_widget("body", &["heading"]),
        &extension,
        &tera,
        &pool,
        &std::collections::HashMap::new(),
        None,
    )
    .await
    .expect("prerender");

    assert!(out.get("body").expect("rendered").contains("[-1]"));
}

#[tokio::test]
async fn an_enricher_leaves_block_types_it_does_not_own_alone() {
    link();
    let pool = mem_pool().await;

    let mut tera = tera::Tera::default();
    tera.add_raw_template("blocks/paragraph.html", "[{{ value._dialect | default(value='none') }}]")
        .expect("template");

    let extension = serde_json::json!({
        "body": [
            { "type": "paragraph", "id": "b", "value": { "text": "untouched" } }
        ]
    });

    let out = rustango_cms::block::tera_helpers::prerender_extension_streams_async(
        &stream_widget("body", &["paragraph"]),
        &extension,
        &tera,
        &pool,
        &std::collections::HashMap::new(),
        None,
    )
    .await
    .expect("prerender");

    let html = out.get("body").expect("body stream rendered");
    assert!(
        html.contains("[none]"),
        "paragraph is not this enricher's block type and must be untouched, got: {html}"
    );
}

/// A page-builder **zone** must enrich like an extension stream does.
///
/// `Block::render` is synchronous, so a block that needs a query result
/// depends entirely on the async pass running first. That pass had one
/// call site — the extension-stream pre-render — and a UI-created page
/// type has no extension body at all: its body lives in the page-builder
/// value store. So a UI-defined block could hold authored values but
/// never carry data, and a chooser dropped into a zone silently lost its
/// resolved `_url`/`_title`.
///
/// This renders a real published schema whose flexible zone holds a
/// UI-defined group, and asserts the enricher reached it.
#[tokio::test]
async fn a_page_builder_zone_enriches_like_an_extension_stream() {
    use rustango_cms::page_builder::{Component, PageBuilderData, PageTypeSchema};

    link();
    let pool = mem_pool().await;
    // Both builder tables carry a foreign key onto `cms_page_type` /
    // `cms_page`, and sqlite enforces it, so the parents have to exist
    // even though neither participates in what is under test.
    for schema in [
        &rustango_cms::page_type_model::PageType::SCHEMA,
        // `cms_page` in turn points at media and themes.
        &rustango_cms::media::Media::SCHEMA,
        &rustango_cms::theme::Theme::SCHEMA,
        &rustango_cms::page::Page::SCHEMA,
        &PageTypeSchema::SCHEMA,
        &Component::SCHEMA,
        &PageBuilderData::SCHEMA,
    ] {
        let sql = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
            pool.dialect(),
            schema,
        );
        for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            rustango::sql::raw_execute_pool(&pool, stmt, Vec::new())
                .await
                .expect("ddl");
        }
    }

    // Minimal parents, so the foreign keys resolve. Every column without
    // a default is named; the rest take theirs.
    for sql in [
        "INSERT INTO cms_page_type \
           (id, app_label, type_name, verbose_name, default_template, is_creatable, allowed_parent_types) \
         VALUES (7, 'cms', 'UiReport', 'UI report', 'ui_report.html', 1, '[]')",
        "INSERT INTO cms_page \
           (id, page_type_id, title, slug, path, depth, sort_order, status, seo_title, seo_description) \
         VALUES (42, 7, 'Probe', 'probe', '0001', 1, 0, 'published', '', '')",
    ] {
        rustango::sql::raw_execute_pool(&pool, sql, Vec::new())
            .await
            .expect("parent row");
    }

    // A published schema: one flexible zone, one zone-local group. The
    // group is UI-defined — nothing registers `rpt_probe` in Rust.
    let document = serde_json::json!({
        "nodes": [{
            "kind": "flex",
            "key": "sections",
            "label": "Sections",
            "allowed": ["rpt_probe"],
            "groups": [{
                "key": "rpt_probe",
                "label": "Probe",
                "children": [
                    { "kind": "field", "key": "caption", "label": "Caption", "widget": "text" }
                ]
            }]
        }]
    });
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO cms_page_type_schema (page_type_id, status, version, document) \
         VALUES (7, 'published', 1, ?)",
        vec![rustango::core::SqlValue::Json(document.clone())],
    )
    .await
    .expect("insert schema");

    // One page's filled values: a single instance of the UI-defined group.
    let data = serde_json::json!({
        "sections": [
            { "type": "rpt_probe", "id": "z1", "value": { "caption": "authored" } }
        ]
    });
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO cms_page_builder_data (page_id, schema_version, component_versions, data) \
         VALUES (42, 1, '{}', ?)",
        vec![rustango::core::SqlValue::Json(data.clone())],
    )
    .await
    .expect("insert data");

    // A host template for the UI-defined group. `_dialect` and `_page`
    // can only have come from the async pass; `caption` is the author's.
    let mut tera = tera::Tera::default();
    tera.add_raw_template(
        "blocks/rpt_probe.html",
        "[{{ value.caption }}|{{ value._dialect | default(value='NOT-ENRICHED') }}|{{ value._page | default(value='no-page') }}]",
    )
    .expect("template");

    // Narrow the failure if the fixture ever drifts: `public_render_ctx`
    // returns `None` for several different reasons and says which.
    let row = rustango_cms::page_builder::model::published_for(&pool, 7)
        .await
        .expect("schema query")
        .expect("a published schema row for page type 7");
    rustango_cms::page_builder::parse_schema(&row.document).expect("schema document parses");
    rustango_cms::page_builder::model::data_for_page(&pool, 42)
        .await
        .expect("builder data query")
        .expect("a builder data row for page 42");

    let (_values, zone_html, _body) = rustango_cms::page_builder::values::public_render_ctx(
        &pool,
        42,
        7,
        None,
        &std::collections::HashMap::new(),
        &tera,
        None,
        Some(42),
        "acme",
    )
    .await
    .expect("the type has a published schema");

    let html = zone_html.get("sections").expect("zone rendered");
    assert!(
        html.contains("authored"),
        "the authored value should still render, got: {html}"
    );
    assert!(
        !html.contains("NOT-ENRICHED"),
        "the host enricher never reached the zone, got: {html}"
    );
    assert!(
        html.contains("sqlite"),
        "the enricher's async value should reach the zone template, got: {html}"
    );
    assert!(
        html.contains("42"),
        "the page id should reach the enricher, got: {html}"
    );
}
