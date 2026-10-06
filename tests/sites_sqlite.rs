//! Host → root resolution, against a real in-memory database.
//!
//! `src/site.rs`'s unit tests cover the pure path translation. What they
//! cannot cover is the half that decides *which* prefix is in play: the
//! lookup, the case folding, and the fallbacks. Those are the parts a
//! wrong answer in makes a whole domain serve the wrong site, so they get
//! a database.
#![cfg(feature = "sqlite")]

use rustango::core::{Column as _, Model as _};
use rustango::sql::{Auto, Pool};

use rustango_cms::page::{Page, PageStatus};
use rustango_cms::page_type_model::PageType;
use rustango_cms::site::{self, Site};

/// The tables `Page` needs to save: its own, the type it points at, the
/// media its OG image column references, and the theme.
async fn create_schema(pool: &Pool) {
    for schema in [
        &PageType::SCHEMA,
        &Page::SCHEMA,
        &Site::SCHEMA,
        &rustango_cms::media::Media::SCHEMA,
        &rustango_cms::theme::Theme::SCHEMA,
    ] {
        ddl(pool, schema).await;
    }
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

/// Two roots: the conventional empty-slug one at `/`, and `/shop`.
struct Tree {
    pool: Pool,
    home: i64,
    shop: i64,
}

async fn tree() -> Tree {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    create_schema(&pool).await;
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

    let mk = |title: &str, slug: &str, path: &str, url: &str| {
        let pool = pool.clone();
        let mut page = Page {
            id: Auto::Unset,
            page_type_id: type_id,
            title: title.to_owned(),
            slug: slug.to_owned(),
            path: path.to_owned(),
            url_path: url.to_owned(),
            preview_path: String::new(),
            template_override: String::new(),
            depth: 1,
            parent_id: None,
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
        async move {
            page.save_pool(&pool).await.expect("page");
            page.id.get().copied().expect("page id")
        }
    };
    let home = mk("Home", "", "0001/", "/").await;
    let shop = mk("Shop", "shop", "0002/", "/shop").await;
    Tree { pool, home, shop }
}

async fn map(pool: &Pool, hostname: &str, root_page_id: i64) {
    let mut row = Site {
        id: Auto::Unset,
        hostname: hostname.to_owned(),
        root_page_id,
        created_at: Auto::Unset,
    };
    row.save_pool(pool).await.expect("site row");
}

/// The non-breaking guarantee: a site that has never heard of this
/// feature resolves exactly as it always did.
#[tokio::test]
async fn an_unmapped_host_contributes_no_prefix() {
    let t = tree().await;
    let prefix = site::prefix_for_host(&t.pool, "anything.example")
        .await
        .expect("resolve");
    assert_eq!(prefix, "");
    // …and so every path passes through untouched.
    assert_eq!(site::to_lookup_path(&prefix, "/about"), "/about");
}

#[tokio::test]
async fn a_mapped_host_serves_its_own_root_under_that_root_s_prefix() {
    let t = tree().await;
    map(&t.pool, "shop.example", t.shop).await;
    let prefix = site::prefix_for_host(&t.pool, "shop.example")
        .await
        .expect("resolve");
    assert_eq!(prefix, "/shop");
    assert_eq!(site::to_lookup_path(&prefix, "/"), "/shop");
    assert_eq!(site::to_lookup_path(&prefix, "/catalog"), "/shop/catalog");
    assert_eq!(site::to_public_path(&prefix, "/shop/catalog"), "/catalog");
}

/// The admin lets someone pick the conventional root explicitly, which
/// writes a row whose page `url_path` is `/`. That must still contribute
/// nothing — a `/` prefix would turn every lookup into `//about`.
#[tokio::test]
async fn mapping_a_host_to_the_conventional_root_contributes_no_prefix() {
    let t = tree().await;
    map(&t.pool, "main.example", t.home).await;
    let prefix = site::prefix_for_host(&t.pool, "main.example")
        .await
        .expect("resolve");
    assert_eq!(prefix, "");
    assert_eq!(site::to_lookup_path(&prefix, "/about"), "/about");
}

/// `Host` headers arrive in whatever case the client sent, and the admin
/// stores them lowercased. Matching them byte-for-byte would make
/// `Shop.Example` fall silently back to the wrong site.
#[tokio::test]
async fn host_matching_ignores_case_and_surrounding_space() {
    let t = tree().await;
    map(&t.pool, "shop.example", t.shop).await;
    for raw in ["Shop.Example", "SHOP.EXAMPLE", "  shop.example  "] {
        assert_eq!(
            site::prefix_for_host(&t.pool, raw).await.expect("resolve"),
            "/shop",
            "{raw:?} did not match the mapping"
        );
    }
}

/// A mid-tree page is a first-class target — a campaign section can own
/// a domain without being promoted to a root, which would rewrite every
/// descendant's URL. This covers both halves at once: the page starts
/// as a root, is moved under another, and the mapping keeps serving it
/// from its new, deeper path.
#[tokio::test]
async fn a_mid_tree_page_serves_a_hostname_from_its_own_path() {
    let t = tree().await;
    map(&t.pool, "shop.example", t.shop).await;

    let mut shop: Page = Page::objects()
        .where_(Page::id.eq(t.shop))
        .first(&t.pool)
        .await
        .expect("fetch")
        .expect("shop");
    shop.parent_id = Some(t.home);
    shop.path = "0001/0002/".to_owned();
    shop.url_path = "/home/shop".to_owned();
    shop.depth = 2;
    shop.save_pool(&t.pool).await.expect("move");

    let prefix = site::prefix_for_host(&t.pool, "shop.example")
        .await
        .expect("resolve");
    assert_eq!(prefix, "/home/shop");
    assert_eq!(
        site::to_lookup_path(&prefix, "/catalog"),
        "/home/shop/catalog"
    );
    assert_eq!(
        site::to_public_path(&prefix, "/home/shop/catalog"),
        "/catalog"
    );
}

/// An empty `Host` header must not be treated as a mapping key — an
/// empty-hostname row would otherwise capture every such request.
#[tokio::test]
async fn an_empty_host_header_serves_the_conventional_root() {
    let t = tree().await;
    map(&t.pool, "", t.shop).await;
    assert_eq!(
        site::prefix_for_host(&t.pool, "").await.expect("resolve"),
        ""
    );
}

/// The guard on deleting a page: every alias domain has to be named, or
/// the operator unbinds one and hits the same wall again.
#[tokio::test]
async fn hosts_for_root_lists_every_alias_of_one_root() {
    let t = tree().await;
    map(&t.pool, "shop.example", t.shop).await;
    map(&t.pool, "boutique.example", t.shop).await;
    map(&t.pool, "main.example", t.home).await;
    assert_eq!(
        site::hosts_for_root(&t.pool, t.shop).await.expect("hosts"),
        vec!["boutique.example".to_owned(), "shop.example".to_owned()],
    );
    assert_eq!(
        site::hosts_for_root(&t.pool, t.home).await.expect("hosts"),
        vec!["main.example".to_owned()],
    );
}

#[tokio::test]
async fn hosts_for_root_is_empty_for_an_unmapped_root() {
    let t = tree().await;
    map(&t.pool, "shop.example", t.shop).await;
    assert!(site::hosts_for_root(&t.pool, t.home)
        .await
        .expect("hosts")
        .is_empty());
}

/// A tenant with no pages at all (a fresh install) resolves to no
/// prefix rather than failing — the router then 404s on its own.
#[tokio::test]
async fn an_empty_tree_contributes_no_prefix() {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    create_schema(&pool).await;
    assert_eq!(
        site::prefix_for_host(&pool, "anything.example")
            .await
            .expect("resolve"),
        ""
    );
}
