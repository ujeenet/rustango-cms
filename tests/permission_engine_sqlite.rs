//! #684 — the permission engine every admin and MCP gate resolves
//! through, and the MCP guards in front of each tool.
//!
//! The sweep calls every CMS tool as an active user who holds nothing,
//! so deleting any one tool's gate fails here.
#![cfg(feature = "sqlite")]

use chrono::Utc;
use rustango::mcp::{call_tool, CancelToken, McpAgent, McpContext, ProgressReporter};
use rustango::sql::{Auto, Pool};
use rustango::tenancy::auth::User;
use rustango::tenancy::permissions::{Role, RolePermission, UserRole};
use serde_json::{json, Value};

use rustango_cms::media::MediaCollection;
use rustango_cms::page::{Page, PageStatus};
use rustango_cms::page_type_model::PageType;
use rustango_cms::permissions::{
    user_can, user_can_in_collection, user_codenames, Action, CollectionPermission, PagePermission,
};

/// JSON-RPC code a refused tool call carries.
const FORBIDDEN: &str = "-32003";

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

async fn page_type(pool: &Pool) -> i64 {
    let mut pt = PageType {
        id: Auto::Unset,
        app_label: "cms".to_owned(),
        type_name: "TestPage".to_owned(),
        verbose_name: "Test page".to_owned(),
        default_template: "page.html".to_owned(),
        view_mode: "auto".to_owned(),
        is_creatable: true,
        allowed_parent_types: json!([]),
        allowed_child_types: json!([]),
        workflow: String::new(),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    pt.save_pool(pool).await.expect("page type");
    pt.id.get().copied().expect("type id")
}

async fn page(pool: &Pool, type_id: i64, slug: &str, parent: Option<i64>) -> i64 {
    let mut p = Page {
        id: Auto::Unset,
        page_type_id: type_id,
        title: slug.to_owned(),
        slug: slug.to_owned(),
        path: String::new(),
        url_path: format!("/{slug}"),
        preview_path: String::new(),
        template_override: String::new(),
        depth: if parent.is_some() { 2 } else { 1 },
        parent_id: parent,
        locale_variant_of: None,
        alias_of: None,
        theme_id: None,
        sort_order: 0,
        status: PageStatus::Draft.as_str().to_owned(),
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
    p.save_pool(pool).await.expect("page");
    p.id.get().copied().expect("page id")
}

async fn collection(pool: &Pool, name: &str, parent: Option<i64>) -> i64 {
    let mut c = MediaCollection {
        id: Auto::Unset,
        name: name.to_owned(),
        parent_id: parent,
        sort_order: 0,
        created_at: Auto::Unset,
    };
    c.save_pool(pool).await.expect("collection");
    c.id.get().copied().expect("collection id")
}

async fn user(pool: &Pool, name: &str, superuser: bool, active: bool) -> i64 {
    let mut u = User {
        id: Auto::Unset,
        username: name.to_owned(),
        password_hash: String::new(),
        email: None,
        is_superuser: superuser,
        active,
        created_at: Utc::now(),
        data: json!({}),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    u.save_pool(pool).await.expect("user");
    u.id.get().copied().expect("user id")
}

/// Put `uid` in a fresh role and return the role id.
async fn role_for(pool: &Pool, uid: i64, name: &str) -> i64 {
    let mut role = Role { id: Auto::Unset, name: name.to_owned(), description: String::new(), data: json!({}) };
    role.save_pool(pool).await.expect("role");
    let rid = role.id.get().copied().expect("role id");
    let mut ur = UserRole { id: Auto::Unset, user_id: uid, role_id: rid };
    ur.save_pool(pool).await.expect("membership");
    rid
}

#[tokio::test]
async fn a_page_grant_flows_to_descendants_only() {
    let pool = setup().await;
    let t = page_type(&pool).await;
    let parent = page(&pool, t, "parent", None).await;
    let child = page(&pool, t, "child", Some(parent)).await;
    let grandchild = page(&pool, t, "grandchild", Some(child)).await;
    let sibling = page(&pool, t, "sibling", None).await;

    let editor = user(&pool, "editor", false, true).await;
    let rid = role_for(&pool, editor, "editors").await;
    let mut grant = PagePermission {
        id: Auto::Unset,
        page_id: parent,
        role_id: rid,
        permission: Action::Edit.as_str().to_owned(),
        created_at: Auto::Unset,
    };
    grant.save_pool(&pool).await.expect("grant");

    for (pid, what) in [(parent, "the granted page"), (child, "a child"), (grandchild, "a grandchild")] {
        assert!(user_can(&pool, editor, pid, Action::Edit).await.expect("q"), "{what}");
    }
    assert!(!user_can(&pool, editor, sibling, Action::Edit).await.expect("q"), "a sibling tree");
    assert!(!user_can(&pool, editor, child, Action::Publish).await.expect("q"), "another action");
    assert!(!user_can(&pool, editor, 9_999, Action::Edit).await.expect("q"), "a missing page");

    let nobody = user(&pool, "nobody", false, true).await;
    assert!(!user_can(&pool, nobody, parent, Action::View).await.expect("q"), "no roles");
    let admin = user(&pool, "admin", true, true).await;
    assert!(user_can(&pool, admin, sibling, Action::Publish).await.expect("q"), "superuser");
    assert!(!user_can(&pool, 9_999, parent, Action::View).await.expect("q"), "unknown user");

    let gone = user(&pool, "gone", true, false).await;
    assert!(!user_can(&pool, gone, parent, Action::View).await.expect("q"), "inactive superuser");
}

#[tokio::test]
async fn a_collection_grant_flows_to_child_collections_only() {
    let pool = setup().await;
    let root = collection(&pool, "root", None).await;
    let inner = collection(&pool, "inner", Some(root)).await;
    let other = collection(&pool, "other", None).await;

    let uploader = user(&pool, "uploader", false, true).await;
    let rid = role_for(&pool, uploader, "uploaders").await;
    let mut grant = CollectionPermission {
        id: Auto::Unset,
        collection_id: root,
        role_id: rid,
        permission: Action::Add.as_str().to_owned(),
        created_at: Auto::Unset,
    };
    grant.save_pool(&pool).await.expect("grant");

    assert!(user_can_in_collection(&pool, uploader, inner, Action::Add).await.expect("q"));
    assert!(!user_can_in_collection(&pool, uploader, other, Action::Add).await.expect("q"));
    assert!(!user_can_in_collection(&pool, uploader, root, Action::Delete).await.expect("q"));
}

#[tokio::test]
async fn codenames_are_the_union_of_the_users_roles() {
    let pool = setup().await;
    let uid = user(&pool, "multi", false, true).await;
    for (role, code) in [("a", "cms_page.view"), ("b", "cms_media.add")] {
        let rid = role_for(&pool, uid, role).await;
        let mut rp = RolePermission { id: Auto::Unset, role_id: rid, codename: code.to_owned() };
        rp.save_pool(&pool).await.expect("role perm");
    }
    let codes = user_codenames(&pool, uid).await.expect("codenames");
    assert!(codes.contains("cms_page.view") && codes.contains("cms_media.add"));
    assert!(!codes.contains("cms_page.edit"));
}

async fn call(pool: &Pool, user_id: Option<i64>, tool: &str, args: Value) -> Result<Value, String> {
    let ctx = McpContext {
        pool: pool.clone(),
        agent: McpAgent {
            agent_id: 1,
            tenant: "t".to_owned(),
            skills: Vec::new(),
            tools: vec![tool.to_owned()],
            user_id,
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
async fn a_key_with_no_live_owner_is_refused() {
    let pool = setup().await;
    let err = call(&pool, None, "list_page_types", json!({})).await.expect_err("machine key");
    assert!(err.contains(FORBIDDEN), "{err}");
    let gone = user(&pool, "gone", true, false).await;
    let err = call(&pool, Some(gone), "list_page_types", json!({})).await.expect_err("inactive");
    assert!(err.contains(FORBIDDEN), "{err}");
    let err = call(&pool, Some(9_999), "list_page_types", json!({})).await.expect_err("missing");
    assert!(err.contains(FORBIDDEN), "{err}");
}

#[tokio::test]
async fn every_tool_refuses_a_user_who_holds_nothing() {
    let pool = setup().await;
    let t = page_type(&pool).await;
    let p = page(&pool, t, "target", None).await;
    let nobody = user(&pool, "nobody", false, true).await;

    let calls = [
        ("list_page_types", json!({})),
        ("search_pages", json!({ "query": "x" })),
        ("get_page", json!({ "page_id": p })),
        ("list_locales", json!({})),
        ("list_media", json!({})),
        ("list_collections", json!({})),
        ("list_snippets", json!({})),
        ("list_translatable_fields", json!({ "page_id": p })),
        ("list_templates", json!({})),
        ("read_template", json!({ "name": "page.html" })),
        ("write_template", json!({ "name": "page.html", "body": "x" })),
        ("validate_template", json!({ "name": "page.html", "body": "x" })),
        ("set_page_type_template", json!({ "page_type": "TestPage", "template": "page.html" })),
        ("create_page", json!({ "page_type": "TestPage", "title": "New", "parent_id": p })),
        ("create_page", json!({ "page_type": "TestPage", "title": "New root" })),
        ("update_page", json!({ "page_id": p, "title": "Renamed" })),
        ("publish_page", json!({ "page_id": p })),
        ("upload_media", json!({ "filename": "a.txt", "content_base64": "aGk=" })),
        ("attach_media", json!({ "page_id": p, "media_id": 1 })),
        ("upsert_translations", json!({ "page_id": p, "locale": "fr", "updates": [] })),
        ("upsert_snippet", json!({ "type_name": "form", "title": "New" })),
    ];
    let mut allowed = Vec::new();
    for (tool, args) in calls {
        match call(&pool, Some(nobody), tool, args.clone()).await {
            Err(e) if e.contains(FORBIDDEN) => {}
            other => allowed.push(format!("{tool} {args}: {other:?}")),
        }
    }
    assert!(allowed.is_empty(), "not refused as forbidden:\n{}", allowed.join("\n"));
}
