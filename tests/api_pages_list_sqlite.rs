//! #655 — `GET /api/v2/pages/` counts, orders and pages in SQL when it
//! can, and that fast path returns exactly what the in-memory path does.
//!
//! The two paths are told apart by the viewer: a login restriction on a
//! draft page denies the anonymous viewer something (so its listing is
//! filtered in memory) while a signed-in viewer is denied nothing (so its
//! listing runs in SQL). The draft is never listed, so both must see the
//! same rows in the same order with the same totals.
#![cfg(feature = "sqlite")]

use chrono::{DateTime, Duration, TimeZone, Utc};
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};

use rustango_cms::api::pages::{list_inner, PageListQuery};
use rustango_cms::locale::Locale;
use rustango_cms::page::{Page, PageStatus};
use rustango_cms::page_tag::PageTag;
use rustango_cms::page_type_model::PageType;
use rustango_cms::view_restriction::{access_context, PageViewRestriction};

async fn ddl(pool: &Pool, schema: &rustango::core::ModelSchema) {
    let sql =
        rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .expect("ddl");
    }
}

fn member() -> rustango::tenancy::auth::User {
    rustango::tenancy::auth::User {
        id: Auto::Set(1),
        username: "member".to_owned(),
        password_hash: String::new(),
        email: None,
        is_superuser: false,
        active: true,
        created_at: Utc::now(),
        data: serde_json::json!({}),
        password_changed_at: None,
        sessions_revoked_at: None,
    }
}

async fn page_type(pool: &Pool, name: &str) -> i64 {
    let mut pt = PageType {
        id: Auto::Unset,
        app_label: "cms".to_owned(),
        type_name: name.to_owned(),
        verbose_name: name.to_owned(),
        default_template: "page.html".to_owned(),
        view_mode: "auto".to_owned(),
        is_creatable: true,
        allowed_parent_types: serde_json::json!([]),
        allowed_child_types: serde_json::json!([]),
        workflow: String::new(),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    pt.save(pool).await.expect("page type");
    pt.id.get().copied().expect("type id")
}

struct Row<'a> {
    type_id: i64,
    title: &'a str,
    path: &'a str,
    parent: Option<i64>,
    status: PageStatus,
    published_at: Option<DateTime<Utc>>,
    sort_order: i32,
    variant_of: Option<i64>,
}

async fn page(pool: &Pool, r: Row<'_>) -> i64 {
    let slug = r.title.to_lowercase();
    let mut p = Page {
        id: Auto::Unset,
        page_type_id: r.type_id,
        title: r.title.to_owned(),
        slug: slug.clone(),
        path: r.path.to_owned(),
        url_path: format!("/{slug}"),
        preview_path: String::new(),
        template_override: String::new(),
        depth: r.path.trim_end_matches('/').split('/').count() as i32,
        parent_id: r.parent,
        locale_variant_of: r.variant_of,
        alias_of: None,
        theme_id: None,
        sort_order: r.sort_order,
        status: r.status.as_str().to_owned(),
        published_at: r.published_at,
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
    p.save(pool).await.expect("page");
    p.id.get().copied().expect("page id")
}

struct Site {
    pool: Pool,
    home: i64,
    docs: i64,
    leaf: i64,
}

/// Home with eleven children (three share one `published_at`, two have
/// none, one is archived), two grandchildren under Docs, a draft gated
/// by a login restriction, a legacy locale variant and a few tags.
async fn site() -> Site {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    for schema in [
        &PageType::SCHEMA,
        &Page::SCHEMA,
        &Locale::SCHEMA,
        &PageViewRestriction::SCHEMA,
        &PageTag::SCHEMA,
        &rustango_cms::media::Media::SCHEMA,
        &rustango_cms::theme::Theme::SCHEMA,
    ] {
        ddl(&pool, schema).await;
    }
    let plain = page_type(&pool, "TestPage").await;
    let other = page_type(&pool, "OtherPage").await;
    for (code, default, order) in [("en", true, 0), ("fr", false, 1)] {
        let mut l = Locale {
            id: Auto::Unset,
            code: code.to_owned(),
            name: code.to_owned(),
            is_default: default,
            active: true,
            sort_order: order,
            created_at: Auto::Unset,
        };
        l.save(&pool).await.expect("locale");
    }

    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let day = |n: i64| Some(t0 + Duration::days(n));
    let home = page(
        &pool,
        Row {
            type_id: plain,
            title: "Home",
            path: "0001/",
            parent: None,
            status: PageStatus::Published,
            published_at: day(0),
            sort_order: 0,
            variant_of: None,
        },
    )
    .await;
    // (title, published_at, sort_order, type, status)
    let kids: [(&str, Option<DateTime<Utc>>, i32, i64, PageStatus); 11] = [
        ("Zeta", day(5), 3, plain, PageStatus::Published),
        ("Alpha", day(5), 1, other, PageStatus::Published),
        ("Mid", day(5), 2, plain, PageStatus::Published),
        ("Docs", day(9), 0, plain, PageStatus::Published),
        ("News", day(7), 5, other, PageStatus::Published),
        ("Undated B", None, 4, plain, PageStatus::Published),
        ("Undated A", None, 4, plain, PageStatus::Published),
        ("Old", day(1), 9, plain, PageStatus::Archived),
        ("Fresh", day(12), 7, plain, PageStatus::Published),
        ("Tied B", day(3), 6, plain, PageStatus::Published),
        ("Tied A", day(3), 6, other, PageStatus::Published),
    ];
    let mut ids = Vec::new();
    for (i, (title, at, order, tid, status)) in kids.into_iter().enumerate() {
        let path = format!("0001/{:04x}/", i + 2);
        ids.push(
            page(
                &pool,
                Row {
                    type_id: tid,
                    title,
                    path: &path,
                    parent: Some(home),
                    status,
                    published_at: at,
                    sort_order: order,
                    variant_of: None,
                },
            )
            .await,
        );
    }
    let docs = ids[3];
    let docs_path = "0001/0005/";
    let intro = page(
        &pool,
        Row {
            type_id: plain,
            title: "Intro",
            path: &format!("{docs_path}0020/"),
            parent: Some(docs),
            status: PageStatus::Published,
            published_at: day(5),
            sort_order: 1,
            variant_of: None,
        },
    )
    .await;
    let leaf = page(
        &pool,
        Row {
            type_id: plain,
            title: "Leaf",
            path: &format!("{docs_path}0021/"),
            parent: Some(docs),
            status: PageStatus::Published,
            published_at: None,
            sort_order: 0,
            variant_of: None,
        },
    )
    .await;
    let draft = page(
        &pool,
        Row {
            type_id: plain,
            title: "Draft",
            path: "0001/0030/",
            parent: Some(home),
            status: PageStatus::Draft,
            published_at: None,
            sort_order: 0,
            variant_of: None,
        },
    )
    .await;
    page(
        &pool,
        Row {
            type_id: plain,
            title: "Accueil",
            path: "0031/",
            parent: None,
            status: PageStatus::Published,
            published_at: day(2),
            sort_order: 0,
            variant_of: Some(home),
        },
    )
    .await;

    let mut gate = PageViewRestriction {
        id: Auto::Unset,
        page_id: draft,
        kind: "login".to_owned(),
        password_hash: String::new(),
        group_ids: serde_json::json!([]),
        codenames: serde_json::json!([]),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    gate.save(&pool).await.expect("restriction");

    for id in [ids[0], ids[1], ids[4], intro] {
        let mut tag = PageTag {
            id: Auto::Unset,
            page_id: id,
            name: "news".to_owned(),
            created_at: Auto::Unset,
        };
        tag.save(&pool).await.expect("tag");
    }
    Site {
        pool,
        home,
        docs,
        leaf,
    }
}

async fn list(site: &Site, signed_in: bool, query: &str) -> (Vec<String>, u64) {
    let qs: PageListQuery = serde_urlencoded::from_str(query).expect("query");
    let user = member();
    let viewer = signed_in.then_some(&user);
    let resp = list_inner(&site.pool, "demo", viewer, &qs).await.expect("list");
    assert_eq!(resp.status(), axum::http::StatusCode::OK, "{query}");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let titles = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["title"].as_str().unwrap_or_default().to_owned())
        .collect();
    (titles, body["meta"]["total_count"].as_u64().expect("total_count"))
}

/// Every window of `filter` over both paths: same rows, same order,
/// same total — and the windows stitch together into the full listing.
async fn assert_parity(site: &Site, filter: &str) -> Vec<String> {
    let sep = if filter.is_empty() { "" } else { "&" };
    let (full_mem, total_mem) = list(site, false, &format!("{filter}{sep}limit=100")).await;
    let (full_sql, total_sql) = list(site, true, &format!("{filter}{sep}limit=100")).await;
    assert_eq!(full_sql, full_mem, "full listing differs for `{filter}`");
    assert_eq!(total_sql, total_mem, "total differs for `{filter}`");
    assert_eq!(total_sql as usize, full_sql.len(), "total for `{filter}`");
    for limit in 1..=4 {
        let mut stitched = Vec::new();
        let mut offset = 0;
        while offset <= full_sql.len() {
            let q = format!("{filter}{sep}limit={limit}&offset={offset}");
            let (mem, t_mem) = list(site, false, &q).await;
            let (sql, t_sql) = list(site, true, &q).await;
            assert_eq!(sql, mem, "window differs for `{q}`");
            assert_eq!((t_sql, t_mem), (total_sql, total_sql), "total for `{q}`");
            stitched.extend(sql);
            offset += limit;
        }
        assert_eq!(stitched, full_sql, "windows of {limit} for `{filter}`");
    }
    full_sql
}

#[tokio::test]
async fn the_two_paths_are_the_ones_under_test() {
    let s = site().await;
    // The draft's restriction denies an anonymous viewer (in-memory
    // path) and nothing to a signed-in one (SQL path).
    assert!(!access_context(&s.pool, None).await.can_view("0001/0030/", 1));
    assert!(access_context(&s.pool, Some(&member())).await.can_view("0001/0030/", 1));
}

#[tokio::test]
async fn the_default_order_matches_and_breaks_ties_by_title() {
    let s = site().await;
    let titles = assert_parity(&s, "").await;
    assert_eq!(
        titles,
        [
            "Fresh", "Docs", "News", "Alpha", "Intro", "Mid", "Zeta", "Tied A", "Tied B", "Old",
            "Home", "Leaf", "Undated A", "Undated B",
        ],
        "newest first, ties by title, undated last; no draft, no variant",
    );
}

#[tokio::test]
async fn date_and_number_orders_match() {
    let s = site().await;
    for order in [
        "published_at",
        "-published_at",
        "sort_order",
        "-sort_order,published_at",
        "depth,-published_at",
        "-depth,sort_order",
        "updated_at",
        "-created_at",
    ] {
        assert_parity(&s, &format!("order={order}")).await;
    }
    let asc = assert_parity(&s, "order=published_at").await;
    assert_eq!(&asc[..3], ["Undated B", "Undated A", "Leaf"], "undated first, by id");
}

#[tokio::test]
async fn filters_match() {
    let s = site().await;
    let home = s.home;
    let docs = s.docs;
    let leaf = s.leaf;
    for filter in [
        format!("child_of={home}"),
        format!("child_of={docs}"),
        format!("descendant_of={home}"),
        format!("descendant_of={docs}&order=-sort_order"),
        format!("ancestor_of={leaf}"),
        format!("ancestor_of={home}"),
        "type=OtherPage".to_owned(),
        "type=cms.OtherPage&order=sort_order".to_owned(),
        "type=Nope".to_owned(),
        "tag=news".to_owned(),
        "tag=News&child_of=".to_owned() + &home.to_string(),
        format!("id_in={home},{docs},{leaf},999999"),
        "id_in=,".to_owned(),
        "updated_since=2000-01-01T00:00:00Z".to_owned(),
        "updated_since=2999-01-01T00:00:00Z".to_owned(),
        format!("translation_of={home}"),
        "locale=fr".to_owned(),
        "locale=en".to_owned(),
        format!("descendant_of={docs}&tag=news"),
    ] {
        assert_parity(&s, &filter).await;
    }

    let (tagged, _) = list(&s, true, "tag=news&order=sort_order").await;
    assert_eq!(tagged, ["Alpha", "Intro", "Zeta", "News"]);
    let (ancestors, _) = list(&s, true, &format!("ancestor_of={leaf}&order=depth")).await;
    assert_eq!(ancestors, ["Home", "Docs"]);
    let (variants, _) = list(&s, true, &format!("translation_of={home}")).await;
    assert_eq!(variants, ["Accueil"]);
}

#[tokio::test]
async fn a_gated_subtree_stays_out_of_an_anonymous_listing() {
    let s = site().await;
    let type_id = page_type(&s.pool, "GatedPage").await;
    let members = page(
        &s.pool,
        Row {
            type_id,
            title: "Members",
            path: "0001/0040/",
            parent: Some(s.home),
            status: PageStatus::Published,
            published_at: None,
            sort_order: 0,
            variant_of: None,
        },
    )
    .await;
    page(
        &s.pool,
        Row {
            type_id,
            title: "Secret",
            path: "0001/0040/0041/",
            parent: Some(members),
            status: PageStatus::Published,
            published_at: None,
            sort_order: 0,
            variant_of: None,
        },
    )
    .await;
    let mut gate = PageViewRestriction {
        id: Auto::Unset,
        page_id: members,
        kind: "login".to_owned(),
        password_hash: String::new(),
        group_ids: serde_json::json!([]),
        codenames: serde_json::json!([]),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    gate.save(&s.pool).await.expect("restriction");

    let (anon, anon_total) = list(&s, false, "limit=100").await;
    let (signed, signed_total) = list(&s, true, "limit=100").await;
    assert!(!anon.iter().any(|t| t == "Members" || t == "Secret"));
    assert!(signed.iter().any(|t| t == "Secret"));
    assert_eq!(anon_total + 2, signed_total);
    // The anchor itself is gated, so its subtree is empty, not leaked.
    let under = format!("descendant_of={members}");
    assert_eq!(list(&s, false, &under).await, (Vec::new(), 0));
    assert_eq!(list(&s, true, &under).await, (vec!["Secret".to_owned()], 1));
}
