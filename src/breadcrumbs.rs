//! Breadcrumb engine — walks the page tree once and returns an
//! ordered list of `(title, url, is_current)` crumbs.
//!
//! Powers both public templates (via [`to_context_value`]) and
//! the admin chrome (admin handlers can call [`resolve_breadcrumbs`]
//! directly and stuff the result into Tera context). The shared
//! `_breadcrumbs.html` macro in [`crate::admin::register_templates`]
//! renders the standard `<nav aria-label="Breadcrumb">` shape plus a
//! parallel JSON-LD `BreadcrumbList` block for SEO.
//!
//! ## Public-side example
//!
//! ```text
//! {# In a Tera template: #}
//! {% set crumbs = breadcrumbs(page_id=page.id) %}
//! {% import "_breadcrumbs.html" as bc %}
//! {{ bc::render(crumbs=crumbs) | safe }}
//! ```
//!
//! ## Options
//!
//! - `include_root` — keep the tenant root crumb (defaults `true`).
//! - `include_current` — keep the leaf crumb (defaults `true`;
//!   public templates usually want this off when the breadcrumb sits
//!   right above the page's `<h1>`).
//! - `max_depth` — cap the walk to N ancestors; oversized trees
//!   degrade gracefully.

use rustango::core::Column as _;

use crate::page::Page;

/// One link in the breadcrumb trail.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Crumb {
    pub title: String,
    /// Tenant-absolute, like `url_path`. On a hostname-mapped site, pass it
    /// through [`crate::site::to_public_path`] with the site's prefix.
    pub url: String,
    pub is_current: bool,
    pub page_id: Option<i64>,
}

/// Options controlling [`resolve_breadcrumbs`].
#[derive(Debug, Clone, Copy)]
pub struct BreadcrumbsOptions {
    /// Keep the root-most ancestor in the trail. Defaults `true`.
    pub include_root: bool,
    /// Keep the leaf (the page passed to `resolve_breadcrumbs`).
    /// Defaults `true`; flip off when the breadcrumb is rendered
    /// next to the page's `<h1>` to avoid duplication.
    pub include_current: bool,
    /// Hard cap on ancestor walks. Defaults 32 — guards against
    /// pathological tree depths.
    pub max_depth: usize,
}

impl Default for BreadcrumbsOptions {
    fn default() -> Self {
        Self {
            include_root: true,
            include_current: true,
            max_depth: 32,
        }
    }
}

/// Walk the page's ancestor chain and return an ordered crumb list
/// from root to leaf.
///
/// # Errors
/// Propagates driver / query failures from the page lookups.
pub async fn resolve_breadcrumbs(
    pool: &rustango::sql::Pool,
    page_id: i64,
    options: BreadcrumbsOptions,
) -> Result<Vec<Crumb>, rustango::sql::ExecError> {
    let target = Page::objects()
        .where_(Page::id.eq(page_id))
        .first(pool)
        .await?;
    let Some(leaf) = target else {
        return Ok(Vec::new());
    };

    // Every ancestor in one query from the materialized path (#747) — the
    // walk it replaces cost one round trip per level. Root first; keep the
    // `max_depth` nearest the leaf.
    let paths = crate::tree::MaterializedPath::ancestors_of(&leaf.path);
    let mut chain: Vec<Page> = if paths.is_empty() {
        Vec::new()
    } else {
        use rustango::sql::FetcherPool as _;
        Page::objects()
            .where_(Page::path.is_in(paths))
            .order_by(&[("path", false)])
            .fetch(pool)
            .await?
    };
    // Byte order, not the collation's: a path's prefixes sort first then.
    chain.sort_by(|a, b| a.path.cmp(&b.path));
    let excess = chain.len().saturating_sub(options.max_depth);
    chain.drain(..excess);
    chain.push(leaf);

    if !options.include_root && !chain.is_empty() {
        chain.remove(0);
    }
    if !options.include_current && !chain.is_empty() {
        chain.pop();
    }

    let last_index = chain.len().saturating_sub(1);
    Ok(chain
        .into_iter()
        .enumerate()
        .map(|(i, page)| Crumb {
            title: page.title.clone(),
            url: if page.url_path.is_empty() {
                "/".to_owned()
            } else {
                page.url_path.clone()
            },
            is_current: i == last_index,
            page_id: page.id.get().copied(),
        })
        .collect())
}

/// Convenience: render the breadcrumb crumb list as a JSON value
/// suitable for stuffing into a Tera context. Use this in admin or
/// public-render handlers right before calling `tera.render(...)`:
///
/// ```ignore
/// let crumbs = breadcrumbs::resolve_breadcrumbs(pool, page_id, opts).await?;
/// ctx.insert("breadcrumbs", &breadcrumbs::to_context_value(&crumbs));
/// ```
///
/// Tera's sync function signature can't host the async DB walk
/// directly without a tokio runtime handle, so we keep the API on
/// the handler side — every chrome'd handler already has the pool
/// in scope, so pre-fetching is free.
#[must_use]
pub fn to_context_value(crumbs: &[Crumb]) -> serde_json::Value {
    serde_json::to_value(crumbs).unwrap_or(serde_json::Value::Array(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_defaults() {
        let o = BreadcrumbsOptions::default();
        assert!(o.include_root);
        assert!(o.include_current);
        assert_eq!(o.max_depth, 32);
    }
}
