//! #761 — through MCP, putting a page live takes the publish right
//! whichever status gets it there: `archived` is served like `published`.
//! The go-live paths (the form's `go_live_at`) are covered beside
//! `apply_page_edit`.
#![cfg(feature = "sqlite")]

use chrono::Utc;
use rustango::core::Column as _;
use rustango::mcp::{call_tool, CancelToken, McpAgent, McpContext, ProgressReporter};
use rustango::sql::{Auto, Pool};
use rustango::tenancy::auth::User;
use rustango::tenancy::permissions::{Role, UserRole};
use serde_json::{json, Value};

use rustango_cms::page::{Page, PageStatus};
use rustango_cms::page_type_model::PageType;
use rustango_cms::permissions::PagePermission;

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

async fn draft_page(pool: &Pool) -> i64 {
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
    let mut p = Page {
        id: Auto::Unset,
        page_type_id: pt.id.get().copied().expect("type id"),
        title: "Draft".to_owned(),
        slug: "draft".to_owned(),
        path: "0001/".to_owned(),
        url_path: "/draft".to_owned(),
        preview_path: String::new(),
        template_override: String::new(),
        depth: 1,
        parent_id: None,
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

/// A non-superuser holding exactly `actions` on `page`.
async fn user_with(pool: &Pool, name: &str, page: i64, actions: &[&str]) -> i64 {
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
    for action in actions {
        let mut g = PagePermission {
            id: Auto::Unset,
            page_id: page,
            role_id: rid,
            permission: (*action).to_owned(),
            created_at: Auto::Unset,
        };
        g.save_pool(pool).await.expect("grant");
    }
    let mut ur = UserRole { id: Auto::Unset, user_id: uid, role_id: rid };
    ur.save_pool(pool).await.expect("membership");
    uid
}

async fn update_page(pool: &Pool, uid: i64, args: Value) -> Result<Value, String> {
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
        .map_err(|e| format!("{e:?}"))?;
    if out.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(out.to_string());
    }
    Ok(out)
}

async fn status_of(pool: &Pool, id: i64) -> String {
    Page::objects()
        .where_(Page::id.eq(id))
        .first(pool)
        .await
        .expect("query")
        .expect("page")
        .status
}

#[tokio::test]
async fn edit_only_cannot_put_a_draft_live_by_any_status() {
    let pool = setup().await;
    let page = draft_page(&pool).await;
    let editor = user_with(&pool, "editor", page, &["view", "edit"]).await;

    for args in [
        json!({ "page_id": page, "status": "archived" }),
        json!({ "page_id": page, "status": "published" }),
    ] {
        let out = update_page(&pool, editor, args.clone()).await;
        let err = out.expect_err(&format!("edit-only must not go live: {args}"));
        assert!(err.contains("publish"), "refused on the publish right: {err}");
        assert_eq!(status_of(&pool, page).await, "draft", "nothing written for {args}");
    }

    update_page(&pool, editor, json!({ "page_id": page, "title": "Renamed" }))
        .await
        .expect("an edit that stays a draft is allowed");
    // Scheduled with no go-live is parked back as a draft, so it is not
    // going live either.
    update_page(&pool, editor, json!({ "page_id": page, "status": "scheduled" }))
        .await
        .expect("scheduled without a date stays a draft");
    assert_eq!(status_of(&pool, page).await, "draft");
}

#[tokio::test]
async fn the_publish_right_can_archive() {
    let pool = setup().await;
    let page = draft_page(&pool).await;
    let publisher = user_with(&pool, "publisher", page, &["view", "edit", "publish"]).await;
    update_page(&pool, publisher, json!({ "page_id": page, "status": "archived" }))
        .await
        .expect("publish right archives");
    assert_eq!(status_of(&pool, page).await, "archived");
}

/// #658 — a page created published with content takes its status in the
/// same edit that writes the content, and ends up published and stamped.
#[tokio::test]
async fn create_page_publishes_through_the_content_edit() {
    let pool = setup().await;
    rustango_cms::seed::seed_tenant(&pool, &test_org()).await.expect("seed page types");
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
    admin.save_pool(&pool).await.expect("admin");
    let uid = admin.id.get().copied().expect("uid");
    let ctx = McpContext {
        pool: pool.clone(),
        agent: McpAgent {
            agent_id: 1,
            tenant: "t".to_owned(),
            skills: Vec::new(),
            tools: vec!["create_page".to_owned()],
            user_id: Some(uid),
            jti: "j".to_owned(),
        },
        progress: ProgressReporter::disabled(),
        cancel: CancelToken::default(),
    };
    let out = call_tool(
        ctx,
        json!({ "name": "create_page", "arguments": {
            "page_type": "ErrorPage", "title": "Gone", "status": "published", "show_in_menus": false
        }}),
    )
    .await
    .expect("call");
    assert_ne!(out.get("isError").and_then(Value::as_bool), Some(true), "{out}");
    let page = Page::objects().first(&pool).await.expect("q").expect("page");
    assert_eq!(page.status, "published");
    assert!(page.published_at.is_some(), "stamped by the edit");
    assert!(!page.show_in_menus, "the content edit applied");
}

fn test_org() -> rustango::tenancy::Org {
    rustango::tenancy::Org {
        id: rustango::Auto::Unset,
        slug: "t".to_owned(),
        display_name: "t".to_owned(),
        storage_mode: "database".to_owned(),
        backend_kind: "sqlite".to_owned(),
        database_url: None,
        schema_name: None,
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    }
}
