//! #659 — a partial MCP `update_page` keeps the page-builder body.
//!
//! `apply_page_edit` rebuilds the whole body from the form it is given, so
//! `update_page` has to start from the stored values (`page_form::prefill`).
//! The unit tests pin the prefill primitives; this drives the real tool
//! against a page with stored builder data, the only place the call site
//! is observable.
#![cfg(feature = "sqlite")]

use chrono::Utc;
use rustango::core::Column as _;
use rustango::core::SqlValue;
use rustango::mcp::{call_tool, CancelToken, McpAgent, McpContext, ProgressReporter};
use rustango::sql::{Auto, Pool};
use rustango::tenancy::auth::User;
use serde_json::{json, Value};

use rustango_cms::page::Page;

/// Every managed table the edit path might touch.
async fn setup() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    for entry in inventory::iter::<rustango::core::ModelEntry> {
        if !entry.schema.managed {
            continue;
        }
        let sql = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
            pool.dialect(),
            entry.schema,
        );
        for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            rustango::sql::raw_execute_pool(&pool, stmt, Vec::new())
                .await
                .unwrap_or_else(|e| panic!("ddl for {}: {e}", entry.schema.table));
        }
    }
    pool
}

async fn superuser(pool: &Pool) -> i64 {
    let mut u = User {
        id: Auto::Unset,
        username: "root".to_owned(),
        password_hash: String::new(),
        email: None,
        is_superuser: true,
        active: true,
        created_at: Utc::now(),
        data: json!({}),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    u.save_pool(pool).await.expect("user");
    u.id.get().copied().expect("user id")
}

async fn update_page(pool: &Pool, uid: i64, args: Value) -> Value {
    let ctx = McpContext {
        pool: pool.clone(),
        agent: McpAgent {
            agent_id: 1,
            tenant: "t".to_owned(),
            skills: Vec::new(),
            tools: vec!["update_page".to_owned()],
            user_id: Some(uid),
            jti: "j".to_owned(),
        },
        progress: ProgressReporter::disabled(),
        cancel: CancelToken::default(),
    };
    let out = call_tool(ctx, json!({ "name": "update_page", "arguments": args }))
        .await
        .expect("tool call");
    assert_ne!(out.get("isError").and_then(Value::as_bool), Some(true), "update_page failed: {out}");
    out
}

#[tokio::test]
async fn a_partial_update_keeps_the_builder_body() {
    let pool = setup().await;
    let uid = superuser(&pool).await;

    for sql in [
        "INSERT INTO cms_page_type \
           (id, app_label, type_name, verbose_name, default_template, is_creatable, allowed_parent_types) \
         VALUES (7, 'cms', 'UiReport', 'UI report', 'ui_report.html', 1, '[]')",
        "INSERT INTO cms_page \
           (id, page_type_id, title, slug, path, depth, sort_order, status, seo_title, seo_description) \
         VALUES (42, 7, 'Report', 'report', '0001/', 1, 0, 'draft', '', '')",
    ] {
        rustango::sql::raw_execute_pool(&pool, sql, Vec::new())
            .await
            .expect("parent row");
    }

    // A plain field and a flexible zone holding one UI-defined group.
    let document = json!({
        "nodes": [
            { "kind": "field", "key": "headline", "label": "Headline", "widget": "text" },
            {
                "kind": "flex",
                "key": "sections",
                "label": "Sections",
                "allowed": ["rpt_note"],
                "groups": [{
                    "key": "rpt_note",
                    "label": "Note",
                    "children": [
                        { "kind": "field", "key": "caption", "label": "Caption", "widget": "text" }
                    ]
                }]
            }
        ]
    });
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO cms_page_type_schema (page_type_id, status, version, document) \
         VALUES (7, 'published', 1, ?)",
        vec![SqlValue::Json(document)],
    )
    .await
    .expect("schema");
    let stored = json!({
        "headline": "Keep me",
        "sections": [{ "type": "rpt_note", "id": "z1", "value": { "caption": "authored" } }]
    });
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO cms_page_builder_data (page_id, schema_version, component_versions, data) \
         VALUES (42, 1, '{}', ?)",
        vec![SqlValue::Json(stored)],
    )
    .await
    .expect("builder data");

    // A one-field SEO edit that says nothing about the body.
    update_page(&pool, uid, json!({ "page_id": 42, "seo_title": "New SEO title" })).await;

    let page = Page::objects()
        .where_(Page::id.eq(42_i64))
        .first(&pool)
        .await
        .expect("query")
        .expect("page");
    assert_eq!(page.seo_title, "New SEO title", "the edit itself landed");

    let data = rustango_cms::page_builder::model::data_for_page(&pool, 42)
        .await
        .expect("builder data query")
        .expect("builder data row")
        .data;
    assert_eq!(data["headline"], "Keep me", "plain field kept: {data}");
    let sections = data["sections"].as_array().expect("zone is an array");
    assert_eq!(sections.len(), 1, "zone kept: {data}");
    assert_eq!(sections[0]["type"], "rpt_note");
    assert_eq!(sections[0]["value"]["caption"], "authored", "zone content kept: {data}");
}
