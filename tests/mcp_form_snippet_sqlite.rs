//! #758 — `upsert_snippet` on a form does what the form builder does: the
//! schema lands as a sanitized draft, the live form is untouched, and the
//! email notification target follows the recipients.
#![cfg(feature = "sqlite")]

use rustango::core::{Column as _, Model as _};
use rustango::mcp::{call_tool, CancelToken, McpAgent, McpContext, ProgressReporter};
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::auth::User;
use serde_json::{json, Value};

use rustango_cms::notify::model::NotificationTarget;
use rustango_cms::snippet::Snippet;

async fn ddl(pool: &Pool, schema: &rustango::core::ModelSchema) {
    let sql =
        rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .expect("ddl");
    }
}

async fn upsert(pool: &Pool, uid: i64, args: Value) -> Value {
    let ctx = McpContext {
        pool: pool.clone(),
        agent: McpAgent {
            agent_id: 1,
            tenant: "t".to_owned(),
            skills: Vec::new(),
            tools: vec!["upsert_snippet".to_owned()],
            user_id: Some(uid),
            jti: "j".to_owned(),
        },
        progress: ProgressReporter::disabled(),
        cancel: CancelToken::default(),
    };
    let out = call_tool(ctx, json!({ "name": "upsert_snippet", "arguments": args }))
        .await
        .expect("call");
    assert_ne!(out.get("isError").and_then(Value::as_bool), Some(true), "{out}");
    out
}

fn schema(emails: &str, html: &str) -> Value {
    json!({
        "settings": { "notify_emails": emails },
        "pages": [{ "sections": [{ "rows": [{ "columns": [{ "fields": [
            { "key": "intro", "type": "richtext", "content": html }
        ]}]}]}]}],
    })
}

#[tokio::test]
async fn a_form_write_is_a_sanitized_draft_with_its_notify_target() {
    // The notify target's secret is an encrypted column. This binary holds
    // one test, so setting the key races nothing.
    std::env::set_var("RUSTANGO_SECRET_KEY", "mcp-form-snippet-test-key");
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    for s in [&User::SCHEMA, &Snippet::SCHEMA, &NotificationTarget::SCHEMA] {
        ddl(&pool, s).await;
    }
    let mut admin = User {
        id: rustango::sql::Auto::Unset,
        username: "admin".to_owned(),
        password_hash: String::new(),
        email: None,
        is_superuser: true,
        active: true,
        created_at: chrono::Utc::now(),
        data: json!({}),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    admin.save_pool(&pool).await.expect("user");
    let uid = admin.id.get().copied().expect("uid");

    let mut live = Snippet {
        id: rustango::sql::Auto::Unset,
        type_name: "form".to_owned(),
        slug: "contact".to_owned(),
        folder_path: String::new(),
        title: "Contact".to_owned(),
        body_markdown: String::new(),
        data: schema("", "<p>live</p>"),
        created_at: rustango::sql::Auto::Unset,
        updated_at: rustango::sql::Auto::Unset,
    };
    live.save_pool(&pool).await.expect("form");
    let id = live.id.get().copied().expect("id");

    upsert(
        &pool,
        uid,
        json!({
            "type_name": "form",
            "id": id,
            "data": schema("ops@example.com", "<p>new</p><script>alert(1)</script>"),
        }),
    )
    .await;

    let saved = Snippet::objects()
        .where_(Snippet::id.eq(id))
        .first(&pool)
        .await
        .expect("query")
        .expect("form");
    let content = |v: &Value| {
        v["pages"][0]["sections"][0]["rows"][0]["columns"][0]["fields"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(content(&saved.data), "<p>live</p>", "the published form is untouched");
    let draft = content(&saved.data["_draft"]);
    assert!(draft.contains("new"), "the write is the draft: {draft}");
    assert!(!draft.contains("script"), "rich text is sanitized: {draft}");

    let targets = NotificationTarget::objects()
        .where_(NotificationTarget::source_id.eq(id))
        .fetch(&pool)
        .await
        .expect("targets");
    assert_eq!(targets.len(), 1, "the recipients got a managed target");
    assert!(targets[0].config.contains("ops@example.com"));
}
