//! #684 — the permission engine and the MCP gates built on it.
//!
//! Nothing tested these: any gate could be deleted and the suite stayed
//! green. Every assertion here has a deny side, so removing a check, a
//! superuser short-circuit or the ancestor walk fails a test.
#![cfg(feature = "sqlite")]

use chrono::Utc;
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango::tenancy::auth::User;
use rustango::tenancy::permissions::{Role, RolePermission, UserRole};

use rustango_cms::mcp::{require_codename, ToolActor};
use rustango_cms::page::{Page, PageStatus};
use rustango_cms::page_type_model::PageType;
use rustango_cms::permissions::{user_can, user_codenames, Action, PagePermission};

async fn ddl(pool: &Pool, schema: &rustango::core::ModelSchema) {
    let sql =
        rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .expect("ddl");
    }
}

struct World {
    pool: Pool,
    admin: i64,
    editor: i64,
    nobody: i64,
    editor_role: i64,
    root: i64,
    child: i64,
    sibling: i64,
}

async fn user(pool: &Pool, name: &str, superuser: bool) -> i64 {
    let mut u = User {
        id: Auto::Unset,
        username: name.to_owned(),
        password_hash: String::new(),
        email: None,
        is_superuser: superuser,
        active: true,
        created_at: Utc::now(),
        data: serde_json::json!({}),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    u.save_pool(pool).await.expect("user");
    u.id.get().copied().expect("user id")
}

async fn page(pool: &Pool, type_id: i64, slug: &str, path: &str, parent: Option<i64>) -> i64 {
    let mut p = Page {
        id: Auto::Unset,
        page_type_id: type_id,
        title: slug.to_owned(),
        slug: slug.to_owned(),
        path: path.to_owned(),
        url_path: format!("/{slug}"),
        preview_path: String::new(),
        template_override: String::new(),
        depth: path.split('/').filter(|s| !s.is_empty()).count() as i32,
        parent_id: parent,
        locale_variant_of: None,
        alias_of: None,
        theme_id: None,
        sort_order: 0,
        status: PageStatus::Published.as_str().to_owned(),
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

async fn world() -> World {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    for schema in [
        &User::SCHEMA,
        &Role::SCHEMA,
        &RolePermission::SCHEMA,
        &UserRole::SCHEMA,
        &PageType::SCHEMA,
        &rustango_cms::media::MediaCollection::SCHEMA,
        &rustango_cms::media::Media::SCHEMA,
        &rustango_cms::theme::Theme::SCHEMA,
        &Page::SCHEMA,
        &PagePermission::SCHEMA,
    ] {
        ddl(&pool, schema).await;
    }
    let admin = user(&pool, "admin", true).await;
    let editor = user(&pool, "editor", false).await;
    let nobody = user(&pool, "nobody", false).await;

    let mut role = Role { id: Auto::Unset, name: "Editor".to_owned(), description: String::new(), data: serde_json::json!({}) };
    role.save_pool(&pool).await.expect("role");
    let editor_role = role.id.get().copied().expect("role id");
    for code in ["cms_admin.access", "cms_page.change"] {
        let mut rp = RolePermission { id: Auto::Unset, role_id: editor_role, codename: code.to_owned() };
        rp.save_pool(&pool).await.expect("role perm");
    }
    let mut ur = UserRole { id: Auto::Unset, user_id: editor, role_id: editor_role };
    ur.save_pool(&pool).await.expect("membership");

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
    let t = pt.id.get().copied().expect("type id");
    let root = page(&pool, t, "root", "0001/", None).await;
    let child = page(&pool, t, "child", "0001/0002/", Some(root)).await;
    let sibling = page(&pool, t, "sibling", "0003/", None).await;

    World { pool, admin, editor, nobody, editor_role, root, child, sibling }
}

fn actor(id: i64, superuser: bool) -> ToolActor {
    ToolActor { id, username: format!("u{id}"), is_superuser: superuser }
}

#[tokio::test]
async fn codenames_are_the_union_of_a_users_roles_and_empty_without_roles() {
    let w = world().await;
    let codes = user_codenames(&w.pool, w.editor).await.expect("codenames");
    assert!(codes.contains("cms_admin.access") && codes.contains("cms_page.change"));
    assert!(user_codenames(&w.pool, w.nobody).await.expect("codenames").is_empty());
}

#[tokio::test]
async fn the_mcp_codename_gate_denies_without_the_codename() {
    let w = world().await;
    assert!(require_codename(&w.pool, &actor(w.editor, false), "cms_page.change").await.is_ok());
    assert!(
        require_codename(&w.pool, &actor(w.editor, false), "cms_template.edit").await.is_err(),
        "a codename the role lacks must be refused"
    );
    assert!(
        require_codename(&w.pool, &actor(w.nobody, false), "cms_page.change").await.is_err(),
        "a user with no roles must be refused"
    );
    assert!(
        require_codename(&w.pool, &actor(w.admin, true), "cms_template.edit").await.is_ok(),
        "superusers pass every codename gate"
    );
}

#[tokio::test]
async fn page_grants_inherit_down_the_tree_and_nowhere_else() {
    let w = world().await;
    let mut grant = PagePermission {
        id: Auto::Unset,
        page_id: w.root,
        role_id: w.editor_role,
        permission: Action::Edit.as_str().to_owned(),
        created_at: Auto::Unset,
    };
    grant.save_pool(&w.pool).await.expect("grant");

    assert!(user_can(&w.pool, w.editor, w.root, Action::Edit).await.unwrap(), "granted on the page");
    assert!(user_can(&w.pool, w.editor, w.child, Action::Edit).await.unwrap(), "inherited by a descendant");
    assert!(!user_can(&w.pool, w.editor, w.sibling, Action::Edit).await.unwrap(), "not on another subtree");
    assert!(!user_can(&w.pool, w.editor, w.root, Action::Publish).await.unwrap(), "only the granted action");
    assert!(!user_can(&w.pool, w.nobody, w.root, Action::Edit).await.unwrap(), "no roles, no access");
    assert!(user_can(&w.pool, w.admin, w.sibling, Action::Delete).await.unwrap(), "superusers can do anything");
}
