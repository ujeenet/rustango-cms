//! #686 — the behavioural hooks are async and see their tenant, pool and
//! user; a hook that panics mid-await is caught and the next one runs.
#![cfg(feature = "sqlite")]

use std::sync::Mutex;

use rustango::sql::Pool;
use rustango_cms::hooks::{HookCtx, HookFuture};
use rustango_cms::page::Page;

static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn panics<'a>(_ctx: HookCtx<'a>, _page: &'a Page) -> HookFuture<'a, ()> {
    Box::pin(async move {
        tokio::task::yield_now().await;
        panic!("a buggy plugin");
    })
}
rustango_cms::register_after_publish_page!(panics);

fn records<'a>(ctx: HookCtx<'a>, page: &'a Page) -> HookFuture<'a, ()> {
    Box::pin(async move {
        // The pool is usable from the hook.
        let ok = rustango::sql::raw_execute_pool(ctx.pool, "SELECT 1", Vec::new()).await.is_ok();
        SEEN.lock().unwrap().push(format!("{}:{}:{:?}:{ok}", ctx.tenant, page.title, ctx.user_id));
    })
}
rustango_cms::register_after_publish_page!(records);

fn widget<'a>(ctx: HookCtx<'a>) -> HookFuture<'a, String> {
    Box::pin(async move { format!("<b>{}</b>", ctx.tenant) })
}
rustango_cms::register_dashboard_widget!("zz-test", "Test", "star", widget);

fn gate<'a>(
    ctx: HookCtx<'a>,
    page: &'a Page,
    _headers: &'a axum::http::HeaderMap,
) -> HookFuture<'a, Option<axum::response::Response>> {
    Box::pin(async move {
        use axum::response::IntoResponse as _;
        (ctx.tenant == "acme" && page.slug == "launch")
            .then(|| (axum::http::StatusCode::IM_A_TEAPOT, "held").into_response())
    })
}
rustango_cms::register_before_serve_page!(gate);

fn search<'a>(ctx: HookCtx<'a>, query: &'a str) -> HookFuture<'a, Vec<rustango_cms::hooks::AdminSearchHit>> {
    Box::pin(async move {
        vec![rustango_cms::hooks::AdminSearchHit {
            label: format!("{query} in {}", ctx.tenant),
            href: "/x".into(),
            subtitle: String::new(),
        }]
    })
}
rustango_cms::register_admin_search_area!("test-area", "Test area", search);

fn page() -> Page {
    serde_json::from_value(serde_json::json!({
        "id": 7, "page_type_id": 1, "title": "Launch", "slug": "launch", "path": "0001/",
        "url_path": "/launch", "preview_path": "", "template_override": "", "depth": 1,
        "parent_id": null, "locale_variant_of": null, "alias_of": null, "theme_id": null,
        "sort_order": 0, "status": "published", "published_at": null, "last_published_at": null,
        "go_live_at": null, "expire_at": null, "seo_title": "", "seo_description": "",
        "robots_index": true, "sitemap_priority": 0.5, "show_in_menus": true, "og_title": "",
        "og_description": "", "og_image_media_id": null, "twitter_card": "summary",
        "notification_pre_published_sent": false,
        "created_at": "2026-11-06T00:00:00Z", "updated_at": "2026-11-06T00:00:00Z"
    }))
    .expect("page")
}

#[tokio::test]
async fn hooks_get_their_context_and_survive_a_panicking_neighbour() {
    let pool = Pool::connect("sqlite::memory:").await.expect("pool");
    let ctx = HookCtx::new(&pool, "acme", Some(3));

    rustango_cms::hooks::fire_after_publish_page(ctx, &page()).await;
    assert_eq!(SEEN.lock().unwrap().as_slice(), ["acme:Launch:Some(3):true"]);

    let widgets = rustango_cms::hooks::render_dashboard_widgets(ctx).await;
    assert!(widgets.iter().any(|(w, html)| w.key == "zz-test" && html == "<b>acme</b>"));
}

#[tokio::test]
async fn before_serve_and_search_hooks_see_the_tenant() {
    let pool = Pool::connect("sqlite::memory:").await.expect("pool");
    let headers = axum::http::HeaderMap::new();

    let held = rustango_cms::hooks::fire_before_serve_page(HookCtx::new(&pool, "acme", None), &page(), &headers).await;
    assert_eq!(held.map(|r| r.status()), Some(axum::http::StatusCode::IM_A_TEAPOT));
    let other = rustango_cms::hooks::fire_before_serve_page(HookCtx::new(&pool, "globex", None), &page(), &headers).await;
    assert!(other.is_none(), "another tenant is served normally");

    let hits = rustango_cms::hooks::run_admin_search_areas(HookCtx::new(&pool, "acme", Some(1)), "mugs").await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, "Test area");
    assert_eq!(hits[0].1[0].label, "mugs in acme");
}
