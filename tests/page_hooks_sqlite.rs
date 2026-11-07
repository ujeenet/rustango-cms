//! #934 / #935 / #938 — page-type hooks see the page's stored row, and a
//! routed URL can pick its template or answer 404.
#![cfg(feature = "sqlite")]

use std::collections::HashMap;

use async_trait::async_trait;
use rustango::core::Model as _;
use rustango::sql::{Auto, ExecError, Pool};
use rustango::Model;
use rustango_cms::page::Page;
use rustango_cms::routable::{match_routes, RouteMatch, RouteOutcome, RouteSpec};
use rustango_cms::{find_handler, PageType, PageTypeOverrides};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

#[derive(Model, PageType, Debug, Clone, Default, Serialize, Deserialize)]
#[rustango(table = "test_hook_event", app = "test")]
#[page_type(type_name = "TestHookEvent", verbose_name = "Event", template = "event.html")]
pub struct TestHookEvent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub page_id: i64,
    #[field(widget = Text, label = "Headline")]
    pub headline: String,
}

#[async_trait]
impl PageTypeOverrides for TestHookEvent {
    async fn public_context(&self, _pool: &Pool, _page: &Page) -> Result<Map<String, Value>, ExecError> {
        let mut m = Map::new();
        m.insert("headline_seen".into(), json!(self.headline));
        Ok(m)
    }

    fn routes(&self) -> Vec<RouteSpec> {
        vec![RouteSpec::new(r"speaker/(?P<name>[a-z]+)", "speaker")]
    }

    async fn route_context(&self, _pool: &Pool, _page: &Page, matched: &RouteMatch) -> Result<Map<String, Value>, ExecError> {
        let mut m = Map::new();
        m.insert("headline_seen".into(), json!(self.headline));
        // Only "ada" is a known speaker.
        if matched.captures.get("name").map(String::as_str) == Some("ada") {
            m.insert("speaker".into(), json!("Ada"));
        }
        Ok(m)
    }

    fn route_outcome(&self, _matched: &RouteMatch, route_ctx: &Map<String, Value>) -> RouteOutcome {
        if route_ctx.contains_key("speaker") {
            RouteOutcome::Template("event_speaker.html".into())
        } else {
            RouteOutcome::NotFound
        }
    }
}

async fn setup() -> (Pool, Page) {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    let sql = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), TestHookEvent::SCHEMA);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(&pool, stmt, Vec::new()).await.expect("ddl");
    }
    let mut row = TestHookEvent { id: Auto::Unset, page_id: 7, headline: "Stored headline".into() };
    row.save(&pool).await.expect("ext row");
    let page: Page = serde_json::from_value(json!({
        "id": 7, "page_type_id": 1, "title": "Event", "slug": "event", "path": "0001/",
        "url_path": "/event", "preview_path": "", "template_override": "", "depth": 1,
        "parent_id": null, "locale_variant_of": null, "alias_of": null, "theme_id": null,
        "sort_order": 0, "status": "published", "published_at": null, "last_published_at": null,
        "go_live_at": null, "expire_at": null, "seo_title": "", "seo_description": "",
        "robots_index": true, "sitemap_priority": 0.5, "show_in_menus": true, "og_title": "",
        "og_description": "", "og_image_media_id": null, "twitter_card": "summary",
        "notification_pre_published_sent": false,
        "created_at": "2026-11-06T00:00:00Z", "updated_at": "2026-11-06T00:00:00Z"
    }))
    .expect("page");
    (pool, page)
}

#[tokio::test]
async fn hooks_run_on_the_stored_row_not_the_registered_default() {
    let (pool, page) = setup().await;
    let handler = find_handler("TestHookEvent").expect("registered");

    let ctx = handler.public_context(&pool, &page).await.expect("public ctx");
    assert_eq!(ctx["headline_seen"], "Stored headline");

    let matched = match_routes(&handler.routes(), "speaker/ada").expect("route");
    let ctx = handler.route_context(&pool, &page, &matched).await.expect("route ctx");
    assert_eq!(ctx["headline_seen"], "Stored headline");
}

#[tokio::test]
async fn a_route_picks_its_template_or_answers_not_found() {
    let (pool, page) = setup().await;
    let handler = find_handler("TestHookEvent").expect("registered");

    let known = match_routes(&handler.routes(), "speaker/ada").expect("route");
    let ctx = handler.route_context(&pool, &page, &known).await.expect("ctx");
    assert_eq!(handler.route_outcome(&known, &ctx), RouteOutcome::Template("event_speaker.html".into()));

    let unknown = RouteMatch { name: "speaker", captures: HashMap::from([("name".to_owned(), "bob".to_owned())]) };
    let ctx = handler.route_context(&pool, &page, &unknown).await.expect("ctx");
    assert_eq!(handler.route_outcome(&unknown, &ctx), RouteOutcome::NotFound);
}

#[test]
fn a_type_without_an_override_renders_its_own_template() {
    #[derive(PageType, Default)]
    #[page_type(app = "test", type_name = "TestHookPlain", verbose_name = "Plain", template = "plain.html")]
    pub struct TestHookPlain;
    impl PageTypeOverrides for TestHookPlain {}

    let handler = find_handler("TestHookPlain").expect("registered");
    let m = RouteMatch { name: "x", captures: HashMap::new() };
    assert_eq!(handler.route_outcome(&m, &Map::new()), RouteOutcome::Render);
}

// ---- inline panels reach the PageTypeOverrides of a derived type ----

#[derive(PageType, Default)]
#[page_type(app = "test", type_name = "TestHookPanels", verbose_name = "Panels", template = "panels.html")]
pub struct TestHookPanels;

static SAVED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

#[async_trait]
impl PageTypeOverrides for TestHookPanels {
    async fn inline_panels(&self, _pool: &Pool, page_id: i64) -> Result<Vec<rustango_cms::InlinePanelSpec>, ExecError> {
        Ok(vec![rustango_cms::InlinePanelSpec::with_widgets(
            "links",
            format!("Links of {page_id}"),
            vec![rustango_cms::Widget::new(rustango_cms::WidgetKind::Text, "name", "Name")],
        )
        .in_tab("Links")])
    }

    async fn load_inline_panel(&self, _pool: &Pool, _page_id: i64, panel: &str) -> Result<Vec<Map<String, Value>>, ExecError> {
        let mut row = Map::new();
        row.insert("name".into(), json!(format!("{panel} row")));
        Ok(vec![row])
    }

    async fn save_inline_panel(&self, _pool: &Pool, _page_id: i64, panel: &str, rows: &[Map<String, Value>]) -> Result<(), ExecError> {
        SAVED.lock().unwrap().push(format!("{panel}:{}", rows.len()));
        Ok(())
    }
}

#[tokio::test]
async fn inline_panel_hooks_are_forwarded_by_the_derive() {
    let (pool, _page) = setup().await;
    let handler = find_handler("TestHookPanels").expect("registered");

    let specs = handler.inline_panels(&pool, 7).await.expect("specs");
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].label, "Links of 7", "the override got the page id");
    assert_eq!(specs[0].tab.as_deref(), Some("Links"));

    let rows = handler.load_inline_panel(&pool, 7, "links").await.expect("rows");
    assert_eq!(rows[0]["name"], "links row");

    handler.save_inline_panel(&pool, 7, "links", &[Map::new(), Map::new()]).await.expect("save");
    assert_eq!(SAVED.lock().unwrap().as_slice(), ["links:2"]);
}
