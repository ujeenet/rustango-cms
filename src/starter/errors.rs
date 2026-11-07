//! A theme's error pages: what visitors see on a missing page (404), a
//! refused one (403), a server failure (500) or maintenance (503).
//!
//! Each `error_pages` entry of `theme.json` becomes a published page of
//! the built-in Error page type ([`crate::error_pages`]) under the home
//! page, hidden from menus and search engines. The theme's
//! `error_page.html` template renders it inside the site's own layout,
//! so a missing URL keeps the header, footer and colours.

use rustango::core::Column as _;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{with_block_ids, StarterError, UI_APP_LABEL};
use crate::error_pages::{self as ep, ErrorPageExt};
use crate::page::{Page, PageStatus};
use crate::page_type_model::PageType;
use crate::tree_ops::{NewPage, TreeError};

/// One `error_pages` entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorPageSeed {
    /// 404, 403, 500 or 503.
    pub status: u16,
    pub title: String,
    /// Empty: `error-<status>`.
    #[serde(default)]
    pub slug: String,
    /// Content blocks (`heading`, `paragraph`, `quote`, …), as a list of
    /// `{ "type": …, "value": { … } }` entries.
    #[serde(default)]
    pub body: Value,
}

impl ErrorPageSeed {
    fn slug(&self) -> String {
        if self.slug.is_empty() {
            format!("error-{}", self.status)
        } else {
            self.slug.clone()
        }
    }
}

/// What is wrong with the entries, checked before anything is written.
/// `home_slugs` are the slugs of the pages the theme puts under its home
/// page, which an error page's slug must not repeat.
pub(super) fn problems(seeds: &[ErrorPageSeed], home_slugs: &[&str]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = Vec::new();
    for seed in seeds {
        let status = seed.status;
        if !ep::STATUSES.contains(&status) {
            problems.push(format!("error page {status}: status must be one of 404, 403, 500, 503"));
        }
        if seen.contains(&status) {
            problems.push(format!("error page {status} is listed twice"));
        }
        seen.push(status);
        if home_slugs.contains(&seed.slug().as_str()) {
            problems.push(format!("error page {status}: slug `{}` is taken by another page", seed.slug()));
        }
        match &seed.body {
            Value::Null => {}
            Value::Array(entries) => {
                for entry in entries {
                    let kind = entry["type"].as_str().unwrap_or_default();
                    if !ep::BODY_BLOCKS.contains(&kind) {
                        problems.push(format!(
                            "error page {status}: block `{kind}` is not allowed (use {})",
                            ep::BODY_BLOCKS.join(", ")
                        ));
                    }
                }
            }
            _ => problems.push(format!("error page {status}: `body` must be a list of blocks")),
        }
    }
    problems
}

/// Create the error pages under `home`, published. Returns how many.
pub(super) async fn create(pool: &Pool, home: &Page, seeds: &[ErrorPageSeed]) -> Result<usize, StarterError> {
    add(pool, home, seeds, false).await
}

/// Create the error pages a site lacks: a status that already has a
/// published error page, or a slug already used under `home`, is skipped,
/// so an editor's own pages stay. A home page whose type (defined in code)
/// refuses error pages gets none. Returns how many were added.
pub(super) async fn add_missing(pool: &Pool, home: &Page, seeds: &[ErrorPageSeed]) -> Result<usize, StarterError> {
    add(pool, home, seeds, true).await
}

async fn add(pool: &Pool, home: &Page, seeds: &[ErrorPageSeed], only_missing: bool) -> Result<usize, StarterError> {
    if seeds.is_empty() {
        return Ok(0);
    }
    let type_id = error_page_type(pool).await?;
    allow_under(pool, home.page_type_id).await?;
    let home_id = *home.id.get().ok_or(TreeError::MissingId)?;
    let mut added = 0;
    for seed in seeds {
        let slug = seed.slug();
        if only_missing {
            let slug_taken = Page::objects()
                .where_(Page::parent_id.eq(home_id))
                .where_(Page::slug.eq(slug.clone()))
                .first(pool)
                .await?
                .is_some();
            if slug_taken || ep::find_for_status(pool, seed.status).await?.is_some() {
                continue;
            }
        }
        let new = NewPage::new(type_id, &seed.title, &slug).with_status(PageStatus::Published);
        let mut page = match Page::create_child_pool(pool, home, new).await {
            Ok(page) => page,
            Err(TreeError::DisallowedChildType { .. } | TreeError::DisallowedParentType { .. }) if only_missing => {
                return Ok(added);
            }
            Err(e) => return Err(e.into()),
        };
        page.robots_index = false;
        page.show_in_menus = false;
        page.save(pool).await?;
        let page_id = *page.id.get().ok_or(TreeError::MissingId)?;
        let body = match &seed.body {
            Value::Null => None,
            body => {
                let keyed = with_block_ids(&json!({ "body": body }), &format!("error-{}", seed.status));
                Some(keyed["body"].to_string())
            }
        };
        let mut row = ErrorPageExt {
            id: Auto::Unset,
            page_id,
            status_code: seed.status.to_string(),
            body,
        };
        row.insert(pool).await?;
        crate::revision::capture(pool, &page, None).await?;
        added += 1;
    }
    Ok(added)
}

/// The site's Error page type row, registered first when the site was
/// never seeded (a seeder normally runs after the code types are).
async fn error_page_type(pool: &Pool) -> Result<i64, StarterError> {
    if let Some(id) = ep::error_page_type_id(pool).await {
        return Ok(id);
    }
    let handler = crate::page_type::find_handler(ep::TYPE_NAME)
        .ok_or_else(|| StarterError::Invalid("the Error page type is not registered".into()))?;
    crate::seed::upsert_page_type(pool, handler.as_ref()).await?;
    ep::error_page_type_id(pool)
        .await
        .ok_or_else(|| StarterError::Invalid("the Error page type could not be set up".into()))
}

/// Let a theme-made home type hold error pages when it names the child
/// types it accepts. (An empty list already accepts any type.)
async fn allow_under(pool: &Pool, type_id: i64) -> Result<(), StarterError> {
    let Some(mut row) = PageType::objects().where_(PageType::id.eq(type_id)).first(pool).await? else {
        return Ok(());
    };
    let mut children = row.allowed_children();
    if row.app_label != UI_APP_LABEL || children.is_empty() || children.iter().any(|t| t == ep::TYPE_NAME) {
        return Ok(());
    }
    children.push(ep::TYPE_NAME.to_owned());
    row.allowed_child_types = json!(children);
    row.save(pool).await?;
    Ok(())
}

/// The site's error pages under `home`, as `error_pages` entries for a
/// package. Image blocks are left out: the pictures they point at exist
/// only on this site.
pub(super) async fn export(pool: &Pool, home: &Page) -> Result<Vec<Value>, StarterError> {
    let Some(type_id) = ep::error_page_type_id(pool).await else {
        return Ok(Vec::new());
    };
    let Some(home_id) = home.id.get().copied() else {
        return Ok(Vec::new());
    };
    let pages: Vec<Page> = Page::objects()
        .where_(Page::parent_id.eq(home_id))
        .where_(Page::page_type_id.eq(type_id))
        .order_by(&[("sort_order", false), ("id", false)])
        .fetch(pool)
        .await?;
    let mut out: Vec<Value> = Vec::new();
    for page in pages {
        let Some(page_id) = page.id.get().copied() else { continue };
        let Some(ext) = ErrorPageExt::objects()
            .where_(ErrorPageExt::page_id.eq(page_id))
            .first(pool)
            .await?
        else {
            continue;
        };
        let Ok(status) = ext.status_code.parse::<u16>() else { continue };
        if out.iter().any(|e| e["status"] == json!(status)) {
            continue;
        }
        let mut blocks: Vec<Value> = ext
            .body
            .as_deref()
            .and_then(|b| serde_json::from_str::<Value>(b).ok())
            .and_then(|b| b.as_array().cloned())
            .unwrap_or_default();
        blocks.retain(|b| b["type"] != "image");
        for b in &mut blocks {
            if let Value::Object(fields) = b {
                fields.remove("id");
            }
        }
        out.push(json!({ "status": status, "title": page.title, "slug": page.slug, "body": blocks }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(status: u16, slug: &str, body: Value) -> ErrorPageSeed {
        ErrorPageSeed { status, title: "T".into(), slug: slug.into(), body }
    }

    #[test]
    fn entries_are_checked_before_anything_is_written() {
        let ok = [
            seed(404, "", json!([{ "type": "paragraph", "value": { "body": "x" } }])),
            seed(500, "oops", Value::Null),
        ];
        assert!(problems(&ok, &["about"]).is_empty());

        let bad = [
            seed(418, "", Value::Null),
            seed(404, "about", json!([{ "type": "gallery", "value": {} }])),
            seed(404, "", json!("text")),
        ];
        let found = problems(&bad, &["about"]).join("; ");
        for expected in [
            "status must be one of",
            "slug `about` is taken",
            "block `gallery` is not allowed",
            "listed twice",
            "must be a list of blocks",
        ] {
            assert!(found.contains(expected), "{expected}: {found}");
        }
    }

    #[test]
    fn the_slug_defaults_to_the_status() {
        assert_eq!(seed(503, "", Value::Null).slug(), "error-503");
        assert_eq!(seed(404, "page-not-found", Value::Null).slug(), "page-not-found");
    }
}
