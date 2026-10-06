//! Page-type handlers for this site.
//!
//! Both types here reuse the base `Page` columns (title, slug,
//! seo_description) and ship no typed extension table. To add typed
//! fields, define an extension model + a migration and load it in
//! `load_extension` / `save_extension` (see the rustango-cms cookbook).

use async_trait::async_trait;
use rustango::core::Column as _;
use rustango::sql::{ExecError, FetcherPool as _, Pool};
use rustango_cms::{register_page_type, DisplayField, Page, PageTypeHandler};

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

    /// Example of the page-level computed-display hook: chips appear in
    /// the page-edit form's "At a glance" panel without touching the
    /// form HTML.
    async fn display_fields(
        &self,
        pool: &Pool,
        page_id: i64,
    ) -> Result<Vec<DisplayField>, ExecError> {
        let mut out = Vec::new();
        let page = Page::objects()
            .where_(Page::id.eq(page_id))
            .fetch(pool)
            .await?
            .into_iter()
            .next();
        let Some(page) = page else {
            return Ok(out);
        };

        let (status_icon, status_pretty) = match page.status.as_str() {
            "published" => ("check_circle", "Published"),
            "draft" => ("edit_note", "Draft"),
            "scheduled" => ("schedule", "Scheduled"),
            "archived" => ("inventory_2", "Archived"),
            other => ("help", other),
        };
        out.push(
            DisplayField::new("Status", format!("<strong>{status_pretty}</strong>"))
                .with_icon(status_icon),
        );

        if let Some(ts) = page.published_at {
            out.push(
                DisplayField::new(
                    "Published",
                    format!("<code>{}</code>", ts.format("%Y-%m-%d %H:%M UTC")),
                )
                .with_icon("event_available"),
            );
        }

        Ok(out)
    }
}

register_page_type!(ArticlePage);
