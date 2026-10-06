//! #758 — the MCP snippet and media writers authorize the write they
//! perform, not `.view`.
#![cfg(feature = "sqlite")]

use chrono::Utc;
use rustango::core::Model as _;
use rustango::mcp::{call_tool, CancelToken, McpAgent, McpContext, ProgressReporter};
use rustango::sql::{Auto, Pool};
use rustango::tenancy::auth::User;
use rustango::tenancy::permissions::{Role, RolePermission, UserRole};
use serde_json::{json, Value};

async fn ddl(pool: &Pool, schema: &rustango::core::ModelSchema) {
    let sql =
        rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .expect("ddl");
    }
}

async fn setup() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    for schema in [
        &User::SCHEMA,
        &Role::SCHEMA,
        &RolePermission::SCHEMA,
        &UserRole::SCHEMA,
        &rustango_cms::snippet::Snippet::SCHEMA,
        &rustango_cms::snippet::SnippetRevision::SCHEMA,
    ] {
        ddl(&pool, schema).await;
    }
    pool
}

/// A non-superuser holding exactly `codenames`.
async fn user_with(pool: &Pool, name: &str, codenames: &[&str]) -> i64 {
    let mut u = User {
        id: Auto::Unset,
        username: name.to_owned(),
        password_hash: String::new(),
        email: None,
        is_superuser: false,
        active: true,
        created_at: Utc::now(),
        data: json!({}),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    u.save_pool(pool).await.expect("user");
    let uid = u.id.get().copied().expect("user id");
    let mut role = Role { id: Auto::Unset, name: name.to_owned(), description: String::new(), data: json!({}) };
    role.save_pool(pool).await.expect("role");
    let rid = role.id.get().copied().expect("role id");
    for code in codenames {
        let mut rp = RolePermission { id: Auto::Unset, role_id: rid, codename: (*code).to_owned() };
        rp.save_pool(pool).await.expect("role perm");
    }
    let mut ur = UserRole { id: Auto::Unset, user_id: uid, role_id: rid };
    ur.save_pool(pool).await.expect("membership");
    uid
}

async fn call(pool: &Pool, uid: i64, tool: &str, args: Value) -> Result<Value, String> {
    let ctx = McpContext {
        pool: pool.clone(),
        agent: McpAgent {
            agent_id: 1,
            tenant: "t".to_owned(),
            skills: Vec::new(),
            tools: vec![tool.to_owned()],
            user_id: Some(uid),
            jti: "j".to_owned(),
        },
        progress: ProgressReporter::disabled(),
        cancel: CancelToken::default(),
    };
    let out = call_tool(ctx, json!({ "name": tool, "arguments": args }))
        .await
        .map_err(|e| format!("{e:?}"))?;
    if out.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(out.to_string());
    }
    Ok(out)
}

#[tokio::test]
async fn upsert_snippet_needs_add_or_edit_not_view() {
    let pool = setup().await;
    let viewer = user_with(&pool, "viewer", &["cms_library.view"]).await;
    let adder = user_with(&pool, "adder", &["cms_library_item__form.add"]).await;
    let editor = user_with(&pool, "editor", &["cms_library.edit"]).await;
    let create = json!({ "type_name": "form", "title": "Contact" });

    let denied = call(&pool, viewer, "upsert_snippet", create.clone()).await;
    assert!(denied.is_err(), "view must not create: {denied:?}");

    let made = call(&pool, adder, "upsert_snippet", create).await.expect("per-type add creates");
    let text = made.to_string();
    let id = rustango_cms::snippet::Snippet::objects()
        .first(&pool)
        .await
        .expect("query")
        .and_then(|s| s.id.get().copied())
        .unwrap_or_else(|| panic!("no snippet saved: {text}"));

    let update = json!({ "type_name": "form", "id": id, "title": "Renamed" });
    assert!(
        call(&pool, adder, "upsert_snippet", update.clone()).await.is_err(),
        "add must not edit an existing snippet"
    );
    assert!(call(&pool, viewer, "upsert_snippet", update.clone()).await.is_err());
    call(&pool, editor, "upsert_snippet", update).await.expect("library-wide edit updates");
}

#[tokio::test]
async fn upload_media_without_a_collection_needs_media_add() {
    let pool = setup().await;
    let viewer = user_with(&pool, "viewer", &["cms_media.view"]).await;
    let out = call(
        &pool,
        viewer,
        "upload_media",
        json!({ "filename": "a.txt", "content_base64": "aGk=" }),
    )
    .await;
    let err = out.expect_err("view must not upload");
    assert!(err.contains("cms_media.add"), "refused on the codename, not later: {err}");
}

/// #724 — a page-like file is refused however privileged the uploader.
#[tokio::test]
async fn upload_media_refuses_files_a_browser_would_run() {
    let pool = setup().await;
    let mut admin = User {
        id: Auto::Unset,
        username: "admin".to_owned(),
        password_hash: String::new(),
        email: None,
        is_superuser: true,
        active: true,
        created_at: Utc::now(),
        data: json!({}),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    admin.save_pool(&pool).await.expect("user");
    let uid = admin.id.get().copied().expect("uid");
    for name in ["x.html", "x.xhtml", "x.js", "x.xml"] {
        let err = call(&pool, uid, "upload_media", json!({ "filename": name, "content_base64": "aGk=" }))
            .await
            .expect_err(name);
        assert!(err.contains("can't be uploaded"), "{name}: {err}");
    }
}
