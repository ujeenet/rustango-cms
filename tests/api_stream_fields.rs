//! Page detail hands a StreamField extension column to API clients as the
//! block array, not as the JSON text it is stored as — so a headless
//! frontend reads `extension.body[0].type` instead of parsing a string
//! inside the response.
//!
//! Gated on `sqlite` like every other DB-backed test in this crate:
//! `cargo test --features sqlite`.
#![cfg(feature = "sqlite")]

mod common;

use async_trait::async_trait;
use common::fixture;
use rustango::sql::{ExecError, Pool};
use rustango_cms::api::pages::detail_object_with_type;
use rustango_cms::page::Page;
use rustango_cms::widget::{Widget, WidgetKind};
use rustango_cms::{register_page_type, PageTypeHandler};
use serde_json::json;

#[derive(Default)]
struct StreamPage;

#[async_trait]
impl PageTypeHandler for StreamPage {
    fn app_label(&self) -> &'static str {
        "tests"
    }
    fn type_name(&self) -> &'static str {
        "ApiStreamPage"
    }
    fn verbose_name(&self) -> &'static str {
        "API stream page"
    }
    fn default_template(&self) -> &'static str {
        "page.html"
    }
    async fn widgets(&self, _pool: &Pool, _page_id: i64) -> Result<Vec<Widget>, ExecError> {
        Ok(vec![
            Widget::new(WidgetKind::Stream, "body", "Body"),
            Widget::new(WidgetKind::Stream, "aside", "Aside"),
            Widget::new(WidgetKind::Text, "note", "Note"),
        ])
    }
}

register_page_type!(StreamPage);

async fn detail_with(extension: serde_json::Value) -> serde_json::Value {
    use rustango::core::Column as _;
    let f = fixture().await;
    let page: Page = Page::objects()
        .where_(Page::id.eq(f.about))
        .first(&f.pool)
        .await
        .expect("query")
        .expect("page");
    serde_json::Value::Object(
        detail_object_with_type(
            &f.pool,
            &page,
            "ApiStreamPage",
            Some(extension),
            None,
            None,
            None,
        )
        .await,
    )
}

#[tokio::test]
async fn a_stream_field_arrives_as_the_block_array() {
    let stored = r#"[{"type":"heading","id":"a1","value":{"text":"Hello","level":"2"}}]"#;
    let body = detail_with(json!({ "body": stored, "aside": "", "note": "[1,2]" })).await;
    let ext = &body["extension"];

    assert_eq!(
        ext["body"][0]["type"], "heading",
        "body is decoded, got {}",
        ext["body"]
    );
    assert_eq!(ext["body"][0]["value"]["text"], "Hello");
    assert_eq!(
        ext["aside"],
        json!([]),
        "an empty stream column is an empty stream"
    );
    assert_eq!(
        ext["note"], "[1,2]",
        "a text field that merely looks like JSON is left alone — only declared streams decode",
    );
}

#[tokio::test]
async fn a_stream_that_is_not_an_array_is_passed_through() {
    let body = detail_with(json!({ "body": "not json" })).await;
    assert_eq!(body["extension"]["body"], "not json");
}
