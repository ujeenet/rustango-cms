//! Every starter theme under `crates/rcms-scaffold/themes/` applies to an
//! empty site: its page types publish, its pages, menus and site settings
//! exist, its colours become the site default, and it never runs twice.
#![cfg(feature = "sqlite")]

use std::path::Path;

use rustango::core::Column as _;
use rustango::sql::{FetcherPool as _, Pool};
use serde_json::Value;

use rustango_cms::navigation::{Menu, MenuItem};
use rustango_cms::error_pages::{self, ErrorPageExt};
use rustango_cms::page::Page;
use rustango_cms::page_builder::{PageBuilderData, PageTypeSchema};
use rustango_cms::starter::{self, Outcome};
use rustango_cms::theme::Theme;

const THEMES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/crates/rcms-scaffold/themes");

// The Site settings form a themed project registers (assets/theme.rs):
// its footer badge is a picture field.
rustango_cms::register_site_setting!("site", "Site", || vec![
    rustango_cms::widget::Widget::new(rustango_cms::widget::WidgetKind::Text, "tagline", "Tagline"),
    rustango_cms::widget::Widget::new(rustango_cms::widget::WidgetKind::Textarea, "footer", "Footer text"),
    rustango_cms::widget::Widget::new(rustango_cms::widget::WidgetKind::MediaPicker, "footer_logo", "Footer badge"),
]);

async fn setup() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    for entry in inventory::iter::<rustango::core::ModelEntry> {
        if !entry.schema.managed {
            continue;
        }
        let sql = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), entry.schema);
        for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            rustango::sql::raw_execute_pool(&pool, stmt, Vec::new())
                .await
                .unwrap_or_else(|e| panic!("ddl for {}: {e}", entry.schema.table));
        }
    }
    rustango_cms::theme_seed::ensure_themes_seeded(&pool).await.expect("preset themes");
    pool
}

/// The theme's `theme.json` as `rcms new --theme` writes it: type names
/// replaced by the shared definitions (with the shared blocks), the shared error pages with the
/// theme's own wording (mirrors the scaffolder).
fn expanded(theme: &str) -> String {
    let read = |p: &str| std::fs::read_to_string(Path::new(THEMES).join(p)).unwrap();
    let mut seed: Value = serde_json::from_str(&read(&format!("{theme}/theme.json"))).unwrap();
    let shared: Vec<Value> = serde_json::from_str(&read("_shared/page_types.json")).unwrap();
    let names: Vec<String> = seed["page_types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let keep = |list: &Value| -> Value {
        list.as_array()
            .unwrap()
            .iter()
            .filter(|t| names.iter().any(|n| Some(n.as_str()) == t.as_str()))
            .cloned()
            .collect()
    };
    let blocks: Vec<Value> = serde_json::from_str(&read("_shared/blocks.json")).unwrap();
    let types: Vec<Value> = names
        .iter()
        .map(|n| {
            let mut def = shared.iter().find(|t| t["type_name"] == n.as_str()).cloned().unwrap();
            def["parents"] = keep(&def["parents"]);
            def["children"] = keep(&def["children"]);
            for zone in def["schema"]["nodes"].as_array_mut().unwrap().iter_mut().filter(|n| n["kind"] == "flex") {
                let groups: Vec<Value> = zone["allowed"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|a| blocks.iter().find(|b| b["key"] == *a).cloned())
                    .collect();
                zone["groups"] = Value::Array(groups);
            }
            def
        })
        .collect();
    seed["page_types"] = Value::Array(types);
    let shared_errors: Vec<Value> = serde_json::from_str(&read("_shared/error_pages.json")).unwrap();
    let own: Vec<Value> = seed["error_pages"].as_array().cloned().unwrap_or_default();
    let errors: Vec<Value> = shared_errors
        .into_iter()
        .map(|e| own.iter().find(|o| o["status"] == e["status"]).cloned().unwrap_or(e))
        .collect();
    seed["error_pages"] = Value::Array(errors);
    let shared_media: Vec<Value> = serde_json::from_str(&read("_shared/media.json")).unwrap();
    let mut used = Vec::new();
    media_refs(&seed, &mut used);
    seed["media"] = used
        .iter()
        .map(|k| shared_media.iter().find(|m| m["key"] == k.as_str()).cloned().unwrap_or_else(|| panic!("{theme}: no picture `{k}`")))
        .collect();
    seed.to_string()
}

fn media_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            if let Some(key) = s.strip_prefix("@media:") {
                if !out.iter().any(|k| k == key) {
                    out.push(key.to_owned());
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| media_refs(v, out)),
        Value::Object(map) => map.values().for_each(|v| media_refs(v, out)),
        _ => {}
    }
}

async fn error_type_id(pool: &Pool) -> i64 {
    let row = rustango_cms::page_type_model::PageType::objects()
        .where_(rustango_cms::page_type_model::PageType::type_name.eq(error_pages::TYPE_NAME.to_owned()))
        .first(pool)
        .await
        .unwrap()
        .expect("the Error page type is registered");
    *row.id.get().unwrap()
}

fn theme_dirs() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(THEMES)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().join("theme.json").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn count_pages(pages: &[Value]) -> usize {
    pages
        .iter()
        .map(|p| 1 + count_pages(p["children"].as_array().map_or(&[][..], Vec::as_slice)))
        .sum()
}

#[test]
fn every_theme_parses_and_validates() {
    let themes = theme_dirs();
    assert_eq!(themes, ["blog", "blog-portfolio", "news", "portfolio"]);
    for theme in themes {
        starter::parse(&expanded(&theme)).unwrap_or_else(|e| panic!("{theme}: {e}"));
    }
}

#[tokio::test]
async fn every_theme_builds_a_complete_site_once() {
    for theme in theme_dirs() {
        let pool = setup().await;
        let json = expanded(&theme);
        let seed: Value = serde_json::from_str(&json).unwrap();
        let want_pages = count_pages(seed["pages"].as_array().unwrap());

        let outcome = starter::apply(&pool, &json).await.unwrap_or_else(|e| panic!("{theme}: {e}"));
        assert_eq!(
            outcome,
            Outcome::Applied {
                page_types: seed["page_types"].as_array().unwrap().len(),
                pages: want_pages,
                menus: seed["menus"].as_array().unwrap().len(),
                error_pages: 4,
                media: 0,
            },
            "{theme}"
        );
        assert!(seed["media"].as_array().is_some_and(|m| !m.is_empty()), "{theme}: has sample pictures");

        // Every page is live, the home page sits at `/`, and pages with
        // content have their builder data.
        let all: Vec<Page> = Page::objects().fetch(&pool).await.unwrap();
        assert_eq!(all.len(), want_pages + 4, "{theme}");
        let error_type = error_type_id(&pool).await;
        let (errors, pages): (Vec<Page>, Vec<Page>) = all.into_iter().partition(|p| p.page_type_id == error_type);
        assert!(pages.iter().all(|p| p.status == "published"), "{theme}: all published");
        let home = pages.iter().find(|p| p.url_path == "/").unwrap_or_else(|| panic!("{theme}: no home"));
        let data: Vec<PageBuilderData> = PageBuilderData::objects().fetch(&pool).await.unwrap();
        assert!(data.len() >= want_pages - 1, "{theme}: builder data for {} of {want_pages}", data.len());
        let schemas: Vec<PageTypeSchema> = PageTypeSchema::objects().fetch(&pool).await.unwrap();
        assert!(schemas.iter().all(|s| s.status == "published" && s.version == 1), "{theme}");

        // Header and footer menus point at live pages.
        for slug in ["header", "footer"] {
            let menu = Menu::objects()
                .where_(Menu::slug.eq(slug.to_owned()))
                .first(&pool)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("{theme}: no {slug} menu"));
            let items: Vec<MenuItem> = MenuItem::objects()
                .where_(MenuItem::menu_id.eq(*menu.id.get().unwrap()))
                .fetch(&pool)
                .await
                .unwrap();
            assert!(!items.is_empty(), "{theme}: empty {slug} menu");
            for item in items.iter().filter_map(|i| i.page_id) {
                assert!(pages.iter().any(|p| p.id.get() == Some(&item)), "{theme}: {slug} links a missing page");
            }
        }

        // The theme's colours are the one public default, set on the root.
        let defaults: Vec<Theme> = Theme::objects()
            .where_(Theme::is_default.eq(true))
            .fetch(&pool)
            .await
            .unwrap();
        assert_eq!(defaults.len(), 1, "{theme}: one default theme");
        assert_eq!(defaults[0].slug, seed["theme"]["slug"].as_str().unwrap());
        assert_eq!(home.theme_id, defaults[0].id.get().copied());

        // The name, logo and icon go to Branding, shared with the admin.
        let branding = rustango_cms::branding::get(&pool).await;
        assert!(branding["site_name"].as_str().is_some_and(|n| !n.is_empty()), "{theme}: site name in Branding");
        let site = rustango_cms::site_setting::get(&pool, starter::SITE_SCOPE).await.unwrap().unwrap();
        assert!(site.value_json.get("name").is_none(), "{theme}: no second site name");

        // The error pages: under the home page, published, out of menus
        // and search, each served for its status with its content.
        for status in [404u16, 403, 500, 503] {
            let page = error_pages::find_for_status(&pool, status)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("{theme}: no {status} page"));
            assert_eq!(page.parent_id, home.id.get().copied(), "{theme}: {status}");
            assert!(!page.robots_index && !page.show_in_menus, "{theme}: {status} hidden");
            let ext = ErrorPageExt::objects()
                .where_(ErrorPageExt::page_id.eq(*page.id.get().unwrap()))
                .first(&pool)
                .await
                .unwrap()
                .unwrap();
            let body: Value = serde_json::from_str(ext.body.as_deref().unwrap()).unwrap();
            assert_eq!(body[0]["type"], "paragraph", "{theme}: {status}");
            assert!(body[0]["id"].is_string(), "{theme}: {status} blocks have ids");
        }
        assert_eq!(errors.len(), 4, "{theme}");
        let own_404 = seed["error_pages"][0]["title"].as_str().unwrap();
        let served = error_pages::find_for_status(&pool, 404).await.unwrap().unwrap();
        assert_eq!(served.title, own_404, "{theme}: the theme's own 404 wording");

        // Never twice.
        assert_eq!(starter::apply(&pool, &json).await.unwrap(), Outcome::AlreadyApplied, "{theme}");
        assert_eq!(Page::objects().fetch(&pool).await.unwrap().len(), want_pages + 4, "{theme}");
    }
}

#[tokio::test]
async fn a_site_with_pages_is_left_alone() {
    let pool = setup().await;
    starter::apply(&pool, &expanded("blog")).await.unwrap();
    // Even without the marker, existing pages stop a second theme.
    rustango::sql::raw_execute_pool(&pool, "DELETE FROM cms_site_setting WHERE scope = 'starter'", Vec::new())
        .await
        .unwrap();
    assert_eq!(starter::apply(&pool, &expanded("news")).await.unwrap(), Outcome::SiteNotEmpty);
}

#[test]
fn a_broken_seed_is_rejected_before_any_write() {
    let mut seed: Value = serde_json::from_str(&expanded("blog")).unwrap();
    seed["menus"][0]["items"][0]["page"] = Value::String("nowhere".into());
    let err = starter::parse(&seed.to_string()).unwrap_err().to_string();
    assert!(err.contains("unknown page `nowhere`"), "{err}");

    let mut seed: Value = serde_json::from_str(&expanded("blog")).unwrap();
    seed["error_pages"][1]["status"] = Value::from(404);
    let err = starter::parse(&seed.to_string()).unwrap_err().to_string();
    assert!(err.contains("error page 404 is listed twice"), "{err}");

    let mut seed: Value = serde_json::from_str(&expanded("blog")).unwrap();
    seed["page_types"][0]["schema"]["nodes"][0]["widget"] = Value::String("password".into());
    assert!(starter::parse(&seed.to_string()).is_err());
}

/// A fresh directory under the system temp dir, removed on drop.
struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rcms-starter-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn a_packed_site_installs_on_an_empty_site_with_its_pages() {
    use rustango_cms::starter::package;
    let from = setup().await;
    starter::apply(&from, &expanded("blog-portfolio")).await.unwrap();
    let pages_before = Page::objects().fetch(&from).await.unwrap().len() - 4;

    let seed = package::export(&from, "Packed", true).await.unwrap();
    let tmp = TempDir::new("pack");
    let zip = tmp.0.join("site.zip");
    let templates = Path::new(THEMES).join("_shared/templates");
    let files = package::pack(&seed, &templates, &zip).unwrap();
    assert!(files >= 10, "{files} template files packed");

    let dest = tmp.0.join("unpacked");
    let (json, written) = package::unpack(&zip, &dest).unwrap();
    assert_eq!(written, files);
    assert!(dest.join("_base.html").exists() && dest.join("blocks/cta.html").exists());
    starter::parse(&json).unwrap();

    let to = setup().await;
    let outcome = package::install(&to, &json).await.unwrap();
    assert!(
        matches!(outcome, Outcome::Applied { pages, error_pages: 4, .. } if pages == pages_before),
        "{outcome:?}"
    );
    assert!(seed["error_pages"].as_array().is_some_and(|e| e.len() == 4), "error pages travel");
    let header = Menu::objects()
        .where_(Menu::slug.eq("header".to_owned()))
        .first(&to)
        .await
        .unwrap()
        .expect("header menu travels");
    let items: Vec<MenuItem> = MenuItem::objects()
        .where_(MenuItem::menu_id.eq(*header.id.get().unwrap()))
        .fetch(&to)
        .await
        .unwrap();
    assert!(items.iter().all(|i| i.page_id.is_some()), "menu links point at the new site's pages");
}

#[tokio::test]
async fn installing_on_a_site_with_content_changes_only_its_look() {
    use rustango_cms::starter::package;
    let site = setup().await;
    starter::apply(&site, &expanded("news")).await.unwrap();
    let pages_before = Page::objects().fetch(&site).await.unwrap().len();

    // The blog look, without pages.
    let blog = setup().await;
    starter::apply(&blog, &expanded("blog")).await.unwrap();
    let look = package::export(&blog, "Blog look", false).await.unwrap();
    assert!(look.get("pages").is_none());

    let outcome = package::install(&site, &look.to_string()).await.unwrap();
    // Blog and Post are new to the news site; Home and Page already exist
    // with the same templates.
    // The news site already has every error page, so none are added.
    assert_eq!(
        outcome,
        Outcome::Switched { page_types_added: 2, page_types_retemplated: 0, error_pages_added: 0 }
    );
    assert_eq!(Page::objects().fetch(&site).await.unwrap().len(), pages_before, "content untouched");
    let default = Theme::objects()
        .where_(Theme::is_default.eq(true))
        .fetch(&site)
        .await
        .unwrap();
    assert_eq!(default.len(), 1);
    assert_eq!(default[0].slug, "starter-blog");

    // Installing the same look again updates the theme row in place.
    let again = package::install(&site, &look.to_string()).await.unwrap();
    assert_eq!(
        again,
        Outcome::Switched { page_types_added: 0, page_types_retemplated: 0, error_pages_added: 0 }
    );
    let starters = Theme::objects()
        .where_(Theme::slug.eq("starter-blog".to_owned()))
        .fetch(&site)
        .await
        .unwrap();
    assert_eq!(starters.len(), 1);
}

#[tokio::test]
async fn installing_on_a_site_without_error_pages_adds_the_missing_ones() {
    use rustango_cms::starter::package;
    let site = setup().await;
    starter::apply(&site, &expanded("news")).await.unwrap();
    // The editor removed the 403 and 503 pages and keeps their own 404.
    for status in [403u16, 503] {
        let page = error_pages::find_for_status(&site, status).await.unwrap().unwrap();
        let id = page.id.get().unwrap();
        for table in ["cms_error_page", "cms_revision"] {
            rustango::sql::raw_execute_pool(&site, &format!("DELETE FROM {table} WHERE page_id = {id}"), Vec::new())
                .await
                .unwrap();
        }
        rustango::sql::raw_execute_pool(&site, &format!("DELETE FROM cms_page WHERE id = {id}"), Vec::new())
            .await
            .unwrap();
    }
    let news_404 = error_pages::find_for_status(&site, 404).await.unwrap().unwrap().title;

    let blog = setup().await;
    starter::apply(&blog, &expanded("blog")).await.unwrap();
    // A look-only package still carries the error pages.
    let look = package::export(&blog, "Blog look", false).await.unwrap();
    assert!(look.get("pages").is_none() && look["error_pages"].as_array().is_some_and(|e| e.len() == 4));

    let outcome = package::install(&site, &look.to_string()).await.unwrap();
    assert!(matches!(outcome, Outcome::Switched { error_pages_added: 2, .. }), "{outcome:?}");
    for status in [403u16, 503] {
        assert!(error_pages::find_for_status(&site, status).await.unwrap().is_some(), "{status} added");
    }
    let kept = error_pages::find_for_status(&site, 404).await.unwrap().unwrap();
    assert_eq!(kept.title, news_404, "the site's own 404 stays");
}

#[tokio::test]
async fn sample_pictures_land_in_the_media_library_and_the_pages() {
    use rustango::storage::{InMemoryStorage, StorageRegistry};
    // Keep the pictures off the disk.
    rustango_cms::media_storage::install(
        StorageRegistry::new().set("media", std::sync::Arc::new(InMemoryStorage::default())).with_default("media"),
    );
    let json = expanded("portfolio");
    let seed: Value = serde_json::from_str(&json).unwrap();
    let files: Vec<(String, Vec<u8>)> = seed["media"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let file = m["file"].as_str().unwrap().to_owned();
            let bytes = std::fs::read(Path::new(THEMES).join("_shared/media").join(&file)).unwrap();
            (file, bytes)
        })
        .collect();
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(f, b)| (f.as_str(), b.as_slice())).collect();

    let pool = setup().await;
    let outcome = starter::apply_with_media(&pool, "demo", &json, &refs).await.unwrap();
    assert!(matches!(outcome, Outcome::Applied { media, .. } if media == refs.len()), "{outcome:?}");

    let media: Vec<rustango_cms::media::Media> = rustango_cms::media::Media::objects().fetch(&pool).await.unwrap();
    assert_eq!(media.len(), refs.len());
    assert!(media.iter().all(|m| m.kind == "image" && m.width.is_some() && !m.alt_text.is_empty()));

    // Every reference became a stored picture's id; none is left behind.
    let ids: Vec<String> = media.iter().map(|m| m.id.get().unwrap().to_string()).collect();
    let data: Vec<PageBuilderData> = PageBuilderData::objects().fetch(&pool).await.unwrap();
    let all = data.iter().map(|d| d.data.to_string()).collect::<String>();
    assert!(!all.contains("@media:"), "unresolved reference");
    let mut covers = 0;
    for d in &data {
        if let Some(cover) = d.data["cover"].as_str() {
            assert!(ids.iter().any(|id| id == cover), "cover {cover}");
            covers += 1;
        }
    }
    assert_eq!(covers, 3, "the three projects have covers");

    // Every seeded picture is in use (covers, blocks, list-block items,
    // Branding's logo and icon), so none shows as unused.
    let used = rustango_cms::media_usage::used(&pool).await.unwrap();
    for m in &media {
        let id = *m.id.get().unwrap();
        assert!(used.contains(&id), "{} counted as unused", m.title);
    }
    let logo = rustango_cms::branding::get(&pool).await["logo"].as_str().unwrap().parse::<i64>().unwrap();
    let uses = rustango_cms::media_usage::uses_of(&pool, logo).await.unwrap();
    assert!(uses.iter().any(|u| matches!(u, rustango_cms::media_usage::Use::Setting { scope } if scope == "branding")), "{uses:?}");
}
