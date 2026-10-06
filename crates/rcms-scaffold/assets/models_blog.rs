//! Page-type handlers for this site (blog template).
//!
//! `ArticlePage` carries a **typed extension table** (`cms_article_page`)
//! with a Markdown body + optional hero image — Wagtail-shape multi-table
//! inheritance: the shared tree/status fields live on `cms_page`, the
//! typed fields live here. The extension is edited inline in the page
//! editor via `extension_fields` / `save_extension`.
//!
//! NOTE: after the first build, generate the extension table's migration:
//!     cargo run -- makemigrations
//!     cargo run -- migrate-tenants
//! (Never hand-author migration JSON — let `makemigrations` emit it.)

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
        "site"
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
        &["ArticlePage"]
    }
}

register_page_type!(HomePage);

/// Typed extension row for `ArticlePage` — one per page (`page_id` is
/// unique). `cargo run -- makemigrations` emits its `CREATE TABLE`.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "cms_article_page", app = "site")]
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
        "site"
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
        Ok(row.and_then(|r| serde_json::to_value(r).ok()).unwrap_or_else(
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
