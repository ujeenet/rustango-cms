//! #759 — MCP list tools show only the rows their detail view allows:
//! search_pages the pages the user may view, list_snippets the rows a
//! library type's `can_view` lets through.
#![cfg(feature = "sqlite")]

use chrono::Utc;
use rustango::mcp::{call_tool, CancelToken, McpAgent, McpContext, ProgressReporter};
use rustango::sql::{Auto, Pool};
use rustango::tenancy::auth::User;
use rustango::tenancy::permissions::{Role, RolePermission, UserRole};
use serde_json::{json, Value};

use rustango_cms::page::{Page, PageStatus};
use rustango_cms::page_type_model::PageType;
use rustango_cms::permissions::{
    user_can, viewable_page_ids, Action, PagePermission,
};

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


/// Hides any snippet whose title starts with `secret`.
#[derive(Default)]
struct PrivateNotes;

#[async_trait::async_trait]
impl rustango_cms::library::LibraryTypeHandler for PrivateNotes {
    fn app_label(&self) -> &'static str {
        "test"
    }
    fn type_name(&self) -> &'static str {
        "private_note"
    }
    fn verbose_name(&self) -> &'static str {
        "Private note"
    }
    fn can_view(&self, snippet: &rustango_cms::snippet::Snippet) -> bool {
        !snippet.title.starts_with("secret")
    }
}
rustango_cms::register_library_type!(PrivateNotes);

async fn codename(pool: &Pool, uid: i64, role: &str, code: &str) -> i64 {
    let rid = role_for(pool, uid, role).await;
    let mut rp = RolePermission { id: Auto::Unset, role_id: rid, codename: code.to_owned() };
    rp.save_pool(pool).await.expect("role perm");
    rid
}

/// blog → post, blog → post → comment, and a separate hr → review tree.
async fn two_trees(pool: &Pool) -> [i64; 5] {
    let t = page_type(pool).await;
    let blog = page(pool, t, "blog", None).await;
    let post = page(pool, t, "post", Some(blog)).await;
    let comment = page(pool, t, "comment", Some(post)).await;
    let hr = page(pool, t, "hr", None).await;
    let review = page(pool, t, "review", Some(hr)).await;
    [blog, post, comment, hr, review]
}

async fn grant_view(pool: &Pool, role_id: i64, page_id: i64) {
    let mut g = PagePermission {
        id: Auto::Unset,
        page_id,
        role_id,
        permission: Action::View.as_str().to_owned(),
        created_at: Auto::Unset,
    };
    g.save_pool(pool).await.expect("grant");
}

#[tokio::test]
async fn viewable_page_ids_agrees_with_user_can() {
    let pool = setup().await;
    let pages = two_trees(&pool).await;
    let [_, post, ..] = pages;
    let editor = user(&pool, "editor", false, true).await;
    let rid = role_for(&pool, editor, "blog-readers").await;
    grant_view(&pool, rid, post).await;

    let pairs: Vec<(i64, Option<i64>)> = {
        use rustango::core::Column as _;
        use rustango::sql::FetcherPool as _;
        Page::objects()
            .where_(Page::id.is_in(pages))
            .fetch(&pool)
            .await
            .expect("pages")
            .into_iter()
            .map(|p| (p.id.get().copied().expect("id"), p.parent_id))
            .collect()
    };
    let viewable = viewable_page_ids(&pool, editor, &pairs).await.expect("viewable");
    for id in pages {
        assert_eq!(
            viewable.contains(&id),
            user_can(&pool, editor, id, Action::View).await.expect("q"),
            "page {id}"
        );
    }
    assert_eq!(viewable.len(), 2, "post and its comment");

    let admin = user(&pool, "admin", true, true).await;
    assert_eq!(viewable_page_ids(&pool, admin, &pairs).await.expect("q").len(), 5);
    let nobody = user(&pool, "nobody", false, true).await;
    assert!(viewable_page_ids(&pool, nobody, &pairs).await.expect("q").is_empty());
}

#[tokio::test]
async fn search_pages_lists_only_viewable_pages() {
    let pool = setup().await;
    let [blog, post, comment, ..] = two_trees(&pool).await;
    let reader = user(&pool, "reader", false, true).await;
    let rid = codename(&pool, reader, "blog-readers", "cms_page.view").await;
    grant_view(&pool, rid, blog).await;

    let out = call(&pool, Some(reader), "search_pages", json!({})).await.expect("search");
    let text = out.to_string();
    for (id, slug) in [(blog, "blog"), (post, "post"), (comment, "comment")] {
        assert!(text.contains(&format!("\\\"slug\\\":\\\"{slug}\\\"")), "{slug} ({id}) listed: {text}");
    }
    for slug in ["hr", "review"] {
        assert!(!text.contains(&format!("\\\"slug\\\":\\\"{slug}\\\"")), "{slug} hidden: {text}");
    }
}

#[tokio::test]
async fn list_snippets_applies_the_types_can_view() {
    let pool = setup().await;
    let reader = user(&pool, "reader", false, true).await;
    codename(&pool, reader, "library-readers", "cms_library.view").await;
    for title in ["public note", "secret note"] {
        let mut s = rustango_cms::snippet::Snippet {
            id: Auto::Unset,
            type_name: "private_note".to_owned(),
            slug: title.replace(' ', "-"),
            folder_path: String::new(),
            title: title.to_owned(),
            body_markdown: String::new(),
            data: json!({}),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        s.save_pool(&pool).await.expect("snippet");
    }
    let out = call(&pool, Some(reader), "list_snippets", json!({})).await.expect("list");
    let text = out.to_string();
    assert!(text.contains("public note"), "{text}");
    assert!(!text.contains("secret note"), "can_view hides it: {text}");
}
