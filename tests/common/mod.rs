//! Shared in-memory fixture for the headless-API integration tests.
//!
//! An in-memory SQLite pool with the CMS tables created straight from
//! the model registry, then a small site built through the real models.
//! `Tenant::for_test` needs a live PostgreSQL connection, so the API
//! handlers' `*_inner` functions take a `&Pool` precisely so this
//! fixture can drive them — including the restriction-leak test, which
//! is the one that most needs to run in the normal suite.
//!
//! Gated on `sqlite` like every other DB-backed test in this crate (the
//! default feature set is `postgres`, which has no in-memory mode):
//! `cargo test --features sqlite`.
#![cfg(feature = "sqlite")]

use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};

use rustango_cms::locale::Locale;
use rustango_cms::menu_item_translation::MenuItemTranslation;
use rustango_cms::navigation::{Menu, MenuItem};
use rustango_cms::page::{Page, PageStatus};
use rustango_cms::page_type_model::PageType;
use rustango_cms::translation::Translation;
use rustango_cms::view_restriction::PageViewRestriction;

/// The site every test in this suite runs against.
///
/// ```text
/// Home  (0001/)                     published
/// ├── About     (0001/0002/)        published
/// ├── Docs      (0001/0003/)        published
/// │   ├── Intro (0001/0003/0004/)   published
/// │   └── Deep  (0001/0003/0005/)   published
/// ├── Members   (0001/0006/)        published  ← login-gated
/// │   └── Secret(0001/0006/0007/)   published  ← gated by its parent
/// └── Draft     (0001/0008/)        draft
/// ```
pub struct Fixture {
    pub pool: Pool,
    pub home: i64,
    pub about: i64,
    pub docs: i64,
    pub intro: i64,
    pub deep: i64,
    pub members: i64,
    pub secret: i64,
    pub draft: i64,
    /// `fr` — a non-default active locale with a few translations.
    pub fr: i64,
}

async fn ddl(pool: &Pool, schema: &rustango::core::ModelSchema) {
    let sql =
        rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .expect("ddl");
    }
}

#[allow(clippy::too_many_lines)]
pub async fn fixture() -> Fixture {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    for schema in [
        &PageType::SCHEMA,
        &Page::SCHEMA,
        &Locale::SCHEMA,
        &Translation::SCHEMA,
        &Menu::SCHEMA,
        &MenuItem::SCHEMA,
        &MenuItemTranslation::SCHEMA,
        &PageViewRestriction::SCHEMA,
        &rustango_cms::media::Media::SCHEMA,
        &rustango_cms::theme::Theme::SCHEMA,
    ] {
        ddl(&pool, schema).await;
    }

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
    let type_id = pt.id.get().copied().expect("page type id");

    // Locales: `en` default, `fr` active. Ordered so `en` is created
    // first — `resolve_locale` falls back to the default.
    let mut en = Locale {
        id: Auto::Unset,
        code: "en".to_owned(),
        name: "English".to_owned(),
        is_default: true,
        active: true,
        sort_order: 0,
        created_at: Auto::Unset,
    };
    en.save_pool(&pool).await.expect("en locale");
    let mut fr = Locale {
        id: Auto::Unset,
        code: "fr".to_owned(),
        name: "Français".to_owned(),
        is_default: false,
        active: true,
        sort_order: 1,
        created_at: Auto::Unset,
    };
    fr.save_pool(&pool).await.expect("fr locale");
    let fr_id = fr.id.get().copied().expect("fr id");

    // Pages. `path` is written by hand rather than through `NewPage` so
    // the shape is visible in one place, and so a test reading a
    // `path`-prefix assertion can check it against this table.
    let mut mk = |title: &str,
                  slug: &str,
                  path: &str,
                  url: &str,
                  parent: Option<i64>,
                  status: PageStatus| {
        let pool = pool.clone();
        let page = Page {
            id: Auto::Unset,
            page_type_id: type_id,
            title: title.to_owned(),
            slug: slug.to_owned(),
            path: path.to_owned(),
            url_path: url.to_owned(),
            preview_path: String::new(),
            template_override: String::new(),
            depth: path.trim_end_matches('/').split('/').count() as i32,
            parent_id: parent,
            locale_variant_of: None,
            alias_of: None,
            theme_id: None,
            sort_order: 0,
            status: status.as_str().to_owned(),
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
        async move {
            let mut page = page;
            page.save_pool(&pool).await.expect("page");
            page.id.get().copied().expect("page id")
        }
    };

    let home = mk("Home", "home", "0001/", "/", None, PageStatus::Published).await;
    let about = mk(
        "About",
        "about",
        "0001/0002/",
        "/about",
        Some(home),
        PageStatus::Published,
    )
    .await;
    let docs = mk(
        "Docs",
        "docs",
        "0001/0003/",
        "/docs",
        Some(home),
        PageStatus::Published,
    )
    .await;
    let intro = mk(
        "Intro",
        "intro",
        "0001/0003/0004/",
        "/docs/intro",
        Some(docs),
        PageStatus::Published,
    )
    .await;
    let deep = mk(
        "Deep",
        "deep",
        "0001/0003/0005/",
        "/docs/deep",
        Some(docs),
        PageStatus::Published,
    )
    .await;
    let members = mk(
        "Members",
        "members",
        "0001/0006/",
        "/members",
        Some(home),
        PageStatus::Published,
    )
    .await;
    let secret = mk(
        "Secret",
        "secret",
        "0001/0006/0007/",
        "/members/secret",
        Some(members),
        PageStatus::Published,
    )
    .await;
    let draft = mk(
        "Draft",
        "draft",
        "0001/0008/",
        "/draft",
        Some(home),
        PageStatus::Draft,
    )
    .await;

    // Login-gate the members subtree. `denied_page_ids` is subtree-aware
    // via `path`, so Secret inherits this without its own row.
    let mut restriction = PageViewRestriction {
        id: Auto::Unset,
        page_id: members,
        kind: rustango_cms::RestrictionKind::Login.as_str().to_owned(),
        password_hash: String::new(),
        group_ids: serde_json::json!([]),
        codenames: serde_json::json!([]),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    restriction.save_pool(&pool).await.expect("restriction");

    // French titles for two pages only — the rest must fall back to
    // canonical rather than blanking.
    for (page_id, value) in [(docs, "Documentation"), (intro, "Introduction")] {
        let mut tr = Translation {
            id: Auto::Unset,
            page_id,
            locale_id: fr_id,
            field_path: "title".to_owned(),
            value: value.to_owned(),
            updated_at: Auto::Unset,
        };
        tr.save_pool(&pool).await.expect("translation");
    }

    Fixture {
        pool,
        home,
        about,
        docs,
        intro,
        deep,
        members,
        secret,
        draft,
        fr: fr_id,
    }
}

/// Add a menu and return `(menu_id, item_ids)` in creation order.
///
/// `main`:
/// ```text
/// Home        → home page
/// Docs        → docs page
///   └ Intro   → intro page
/// Members     → members page   (gated)
/// Elsewhere   → https://example.com
/// ```
pub async fn seed_menu(f: &Fixture) -> (i64, Vec<i64>) {
    let mut menu = Menu {
        id: Auto::Unset,
        slug: "main".to_owned(),
        name: "Main navigation".to_owned(),
        created_at: Auto::Unset,
    };
    menu.save_pool(&f.pool).await.expect("menu");
    let menu_id = menu.id.get().copied().expect("menu id");

    let mut ids = Vec::new();
    let mut add = |label: &str,
                   page_id: Option<i64>,
                   external: Option<&str>,
                   parent: Option<i64>,
                   order: i32| {
        let pool = f.pool.clone();
        let item = MenuItem {
            id: Auto::Unset,
            menu_id,
            parent_id: parent,
            sort_order: order,
            label: label.to_owned(),
            page_id,
            external_url: external.map(str::to_owned),
            open_in_new_tab: false,
        };
        async move {
            let mut item = item;
            item.save_pool(&pool).await.expect("menu item");
            item.id.get().copied().expect("item id")
        }
    };

    ids.push(add("Home", Some(f.home), None, None, 10).await);
    let docs_item = add("Docs", Some(f.docs), None, None, 20).await;
    ids.push(docs_item);
    // Empty label on purpose — it must fall back to the page title, and
    // to the *translated* page title under `?locale=fr`.
    ids.push(add("", Some(f.intro), None, Some(docs_item), 30).await);
    ids.push(add("Members", Some(f.members), None, None, 40).await);
    ids.push(add("Elsewhere", None, Some("https://example.com"), None, 50).await);
    (menu_id, ids)
}

/// Store one menu-item label override.
pub async fn translate_item(f: &Fixture, item_id: i64, field: &str, value: &str) {
    let mut row = MenuItemTranslation {
        id: Auto::Unset,
        item_id,
        locale_id: f.fr,
        field_path: field.to_owned(),
        value: value.to_owned(),
        updated_at: Auto::Unset,
    };
    row.save_pool(&f.pool).await.expect("item translation");
}

/// Read a `Response` body as JSON.
pub async fn json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json body")
}

/// Every `title` in a nested `items` array, depth-first.
pub fn titles(items: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    collect(items, "title", &mut out);
    out
}

/// Every `label` in a nested `items` array, depth-first.
pub fn labels(items: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    collect(items, "label", &mut out);
    out
}

fn collect(items: &serde_json::Value, key: &str, out: &mut Vec<String>) {
    let Some(arr) = items.as_array() else { return };
    for node in arr {
        if let Some(s) = node.get(key).and_then(serde_json::Value::as_str) {
            out.push(s.to_owned());
        }
        collect(&node["children"], key, out);
    }
}

/// Find a node anywhere in a nested `items` array by its `title`/`label`.
pub fn find<'a>(items: &'a serde_json::Value, key: &str, want: &str) -> Option<&'a serde_json::Value> {
    let arr = items.as_array()?;
    for node in arr {
        if node.get(key).and_then(serde_json::Value::as_str) == Some(want) {
            return Some(node);
        }
        if let Some(hit) = find(&node["children"], key, want) {
            return Some(hit);
        }
    }
    None
}
