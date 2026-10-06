//! Demo page-type handlers.
//!
//! `ArticlePage` carries a **typed extension table** (`cms_article_page`)
//! with a Markdown body + optional hero image — Wagtail-shape multi-table
//! inheritance: the shared tree/status fields live on `cms_page`, the
//! typed fields live here. The extension is edited inline in the page
//! editor via `extension_fields` / `save_extension`.
//!
//! This mirrors the project the getting-started guide builds, so a reader
//! can diff against it. Its migration is generated, never hand-authored:
//!     cargo run --example cms_demo -- makemigrations
//!     cargo run --example cms_demo -- migrate-tenants

use std::collections::HashMap;

use async_trait::async_trait;
use rustango::core::Column as _;
use rustango::sql::{Auto, ExecError, FetcherPool as _, Pool};
use rustango::Model;
use rustango_cms::{register_page_type, ExtensionField, ExtensionFieldKind, PageTypeHandler};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Default)]
pub struct HomePage;

#[async_trait]
impl PageTypeHandler for HomePage {
    fn app_label(&self) -> &'static str {
        "demo"
    }
    fn type_name(&self) -> &'static str {
        "HomePage"
    }
    fn verbose_name(&self) -> &'static str {
        "Home page"
    }
    fn default_template(&self) -> &'static str {
        "home_page.html"
    }
    fn allowed_child_types(&self) -> &'static [&'static str] {
        &["ArticlePage", "MembersPage", "ProductFeed"]
    }
}

register_page_type!(HomePage);

/// Typed extension row for `ArticlePage` — one per page (`page_id` is
/// unique). `cargo run -- makemigrations` emits its `CREATE TABLE`.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "cms_article_page", app = "demo")]
pub struct ArticleBody {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// FK to the canonical `cms_page` row.
    #[rustango(fk = "cms_page", on = "id", index, unique)]
    pub page_id: i64,
    #[rustango(max_length = 16000)]
    pub body_markdown: String,
    /// Optional hero image (`cms_media.id`).
    #[rustango(fk = "cms_media", on = "id")]
    pub hero_media_id: Option<i64>,
}

#[derive(Default)]
pub struct ArticlePage;

#[async_trait]
impl PageTypeHandler for ArticlePage {
    fn app_label(&self) -> &'static str {
        "demo"
    }
    fn type_name(&self) -> &'static str {
        "ArticlePage"
    }
    fn verbose_name(&self) -> &'static str {
        "Article"
    }
    fn default_template(&self) -> &'static str {
        "article_page.html"
    }
    fn allowed_parent_types(&self) -> &'static [&'static str] {
        &["HomePage"]
    }

    /// Read path: the extension row as JSON for the public template
    /// (`extension.body_markdown`, `extension.hero_media_id`).
    ///
    /// A page always spends some time with no extension row: the create form
    /// has no page id to hang the fields on, so the row is first written on
    /// the save *after* creation. Return an empty row rather than `null` for
    /// that gap — otherwise `{{ extension.body_markdown }}` raises Tera's
    /// "Variable not found" and the public page 500s instead of rendering an
    /// empty article (#620). Templates should never have to tell "no row yet"
    /// apart from "empty row".
    async fn load_extension(&self, pool: &Pool, page_id: i64) -> Result<Value, ExecError> {
        let row = ArticleBody::objects()
            .where_(ArticleBody::page_id.eq(page_id))
            .fetch(pool)
            .await?
            .into_iter()
            .next();
        Ok(row
            .and_then(|r| serde_json::to_value(r).ok())
            .unwrap_or_else(
                || serde_json::json!({ "body_markdown": "", "hero_media_id": Value::Null }),
            ))
    }

    /// Declare the editable fields — the page editor renders these inputs.
    async fn extension_fields(
        &self,
        pool: &Pool,
        page_id: i64,
    ) -> Result<Vec<ExtensionField>, ExecError> {
        let row = ArticleBody::objects()
            .where_(ArticleBody::page_id.eq(page_id))
            .fetch(pool)
            .await?
            .into_iter()
            .next();
        let (body, hero) = match row {
            Some(r) => (
                r.body_markdown,
                r.hero_media_id.map(|i| i.to_string()).unwrap_or_default(),
            ),
            None => (String::new(), String::new()),
        };
        Ok(vec![
            ExtensionField {
                name: "body_markdown".to_owned(),
                label: "Body (Markdown)".to_owned(),
                kind: ExtensionFieldKind::Markdown,
                value: body,
                help: String::new(),
                max_length: Some(16000),
            },
            ExtensionField {
                name: "hero_media_id".to_owned(),
                label: "Hero image".to_owned(),
                kind: ExtensionFieldKind::MediaPicker,
                value: hero,
                help: "Optional hero image shown above the article.".to_owned(),
                max_length: None,
            },
        ])
    }

    /// Write path: upsert the extension row from the posted form.
    async fn save_extension(
        &self,
        pool: &Pool,
        page_id: i64,
        form: &HashMap<String, String>,
    ) -> Result<(), ExecError> {
        let body = form.get("body_markdown").cloned().unwrap_or_default();
        let hero = form
            .get("hero_media_id")
            .and_then(|s| s.trim().parse::<i64>().ok());
        let existing = ArticleBody::objects()
            .where_(ArticleBody::page_id.eq(page_id))
            .fetch(pool)
            .await?
            .into_iter()
            .next();
        match existing {
            Some(mut r) => {
                r.body_markdown = body;
                r.hero_media_id = hero;
                r.save_pool(pool).await?;
            }
            None => {
                let mut r = ArticleBody {
                    id: Auto::Unset,
                    page_id,
                    body_markdown: body,
                    hero_media_id: hero,
                };
                r.insert_pool(pool).await?;
            }
        }
        Ok(())
    }
}

register_page_type!(ArticlePage);

// ---------------------------------------------------------------- Members page
/// A page type gated by the framework **permission engine** (#members):
/// every `MembersPage` (and its descendants, unless they set their own
/// restriction) requires the `members.access` permission codename — the
/// per-**type** analogue of the per-page Privacy tab. A public visitor
/// without the codename is redirected to `/members/login`; a signed-in
/// member lacking it gets 403. Superusers bypass. No extension table —
/// it renders the shared `members_page.html`.
#[derive(Default)]
pub struct MembersPage;

#[async_trait]
impl PageTypeHandler for MembersPage {
    fn app_label(&self) -> &'static str {
        "demo"
    }
    fn type_name(&self) -> &'static str {
        "MembersPage"
    }
    fn verbose_name(&self) -> &'static str {
        "Members page"
    }
    fn default_template(&self) -> &'static str {
        "members_page.html"
    }
    fn allowed_parent_types(&self) -> &'static [&'static str] {
        &["HomePage"]
    }
    /// Per-type gate — the permission-engine members-area check.
    fn view_restriction(&self) -> Option<rustango_cms::TypeViewRestriction> {
        Some(rustango_cms::TypeViewRestriction::permission([
            "members.access",
        ]))
    }
}

register_page_type!(MembersPage);

// ---------------------------------------------------------------- Sectioned page
/// Derive-based page type with a **StreamField** body — the counterpart to
/// the manual `PageTypeHandler` examples above, and the page the E2E suite
/// uses to exercise the block-tree sidebar (nested blocks via the builtin
/// `typed_table`'s repeated rows). `#[derive(PageType)]` registers the type
/// and renders the stream editor; the extension table ships as a checked-in
/// migration (regenerate with `makemigrations` after changing fields).
#[derive(
    rustango::Model, rustango_cms::PageType, Default, Debug, Clone, Serialize, Deserialize,
)]
#[rustango(table = "demo_sectioned_page", app = "demo")]
#[page_type(
    type_name = "SectionedPage",
    verbose_name = "Sectioned page",
    template = "sectioned_page.html",
    icon = "view_agenda"
)]
pub struct SectionedPage {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "cms_page", on = "id", unique)]
    pub page_id: i64,
    #[field(
        widget = Stream,
        label = "Body",
        help = "Compose the page from blocks — headings, text, quotes, tables.",
        allowed(heading, paragraph, quote, typed_table)
    )]
    pub body: Option<String>,
}
#[async_trait]
impl rustango_cms::PageTypeOverrides for SectionedPage {
    /// Act as an index: list only *published* children (drafts stay
    /// hidden) in curated `sort_order`. Demonstrates the derive
    /// `children_query` override; `sectioned_page.html` renders these
    /// with the `page_href` link helper.
    async fn children_query(
        &self,
        pool: &rustango::sql::Pool,
        page: &rustango_cms::Page,
    ) -> Result<Option<Vec<rustango_cms::Page>>, rustango::sql::ExecError> {
        Ok(Some(rustango_cms::published_children(pool, page).await?))
    }
}

// ---------------------------------------------------------------
// ProductFeed — a JSON-only page type (#api-view)
// ---------------------------------------------------------------

/// A page that exists purely to answer JSON: no template, no HTML
/// representation. `view_mode = "api"` is what makes the otherwise
/// mandatory `template` attribute optional, and what tells the renderer
/// to serialize instead of calling Tera.
///
/// Its URL answers `application/json` for every client, because JSON is
/// the only representation it has — see `rustango_cms::page_view`.
#[derive(
    rustango::Model, rustango_cms::PageType, Default, Debug, Clone, Serialize, Deserialize,
)]
#[rustango(table = "demo_product_feed", app = "demo")]
#[page_type(
    type_name = "ProductFeed",
    verbose_name = "Product feed (JSON)",
    view_mode = "api",
    icon = "data_object",
    description = "Serves JSON on its own URL — no template."
)]
pub struct ProductFeed {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,
    #[field(widget = Text, label = "Feed name")]
    pub feed_name: Option<String>,
    #[field(widget = Integer, label = "Items per response")]
    pub page_size: Option<i64>,
}

impl rustango_cms::PageTypeOverrides for ProductFeed {}
