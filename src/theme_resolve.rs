//! Per-request theme resolution.
//!
//! Walks this chain for each public page request, returning the first
//! hit (or `None` if nothing's configured):
//!
//! 1. `page.theme_id` — explicit theme on the rendered page itself.
//! 2. Each ancestor's `theme_id`, climbing toward the root.
//! 3. The `cms_theme` row with `is_default = true` (site default).
//! 4. The `cms_theme` row with `is_admin_default = true` (last resort,
//!    so the public site doesn't render naked when the operator only
//!    set the admin theme).
//!
//! The walk uses the page's materialized `path` to fetch every
//! ancestor in one indexed SELECT — no recursive query, no
//! per-segment round-trip.

use rustango::core::Column as _;
use rustango::sql::{ExecError, FetcherPool as _, Pool};

use crate::page::Page;
use crate::theme::{BrandColor, Theme};

/// What [`resolve_for_page`] returns: the matched theme + its brand
/// colors, ready to feed into [`crate::theme::emit_css`].
pub struct ResolvedTheme {
    pub theme: Theme,
    pub brand_colors: Vec<BrandColor>,
}

/// Walk the resolution chain and return the active theme for `page`.
/// `None` means "no theme configured anywhere"; the caller should
/// skip emitting the `<style>` block and let the public template
/// fall back to its own defaults.
pub async fn resolve_for_page(
    pool: &Pool,
    page: &Page,
) -> Result<Option<ResolvedTheme>, ExecError> {
    // 1. The page itself.
    if let Some(tid) = page.theme_id {
        if let Some(rt) = load_by_id(pool, tid).await? {
            return Ok(Some(rt));
        }
    }
    // 2. Ancestors. Materialized-path lookup → one indexed SELECT.
    // tree::MaterializedPath stores `0001/0005/0007/` shape — split
    // on `/`, drop the trailing empty, drop the self segment, then
    // rebuild each prefix into the canonical "with trailing slash"
    // form so the `IN (…)` query lands on the index.
    if !page.path.is_empty() {
        let segs: Vec<&str> = page.path.split('/').filter(|s| !s.is_empty()).collect();
        // Drop the self segment (last one).
        let ancestor_segs = if segs.len() > 1 {
            &segs[..segs.len() - 1]
        } else {
            &[][..]
        };
        if !ancestor_segs.is_empty() {
            let mut prefixes: Vec<String> = Vec::with_capacity(ancestor_segs.len());
            let mut accum = String::new();
            for seg in ancestor_segs {
                accum.push_str(seg);
                accum.push('/');
                prefixes.push(accum.clone());
            }
            let mut ancestors: Vec<Page> = Page::objects()
                .where_(Page::path.is_in(prefixes))
                .fetch(pool)
                .await
                .unwrap_or_default();
            // Closest first (deepest).
            ancestors.sort_by_key(|p| std::cmp::Reverse(p.depth));
            for a in ancestors {
                if let Some(tid) = a.theme_id {
                    if let Some(rt) = load_by_id(pool, tid).await? {
                        return Ok(Some(rt));
                    }
                }
            }
        }
    }
    // 3. Site default.
    if let Some(rt) = load_by_flag(pool, "is_default").await? {
        return Ok(Some(rt));
    }
    // 4. Admin default (last resort).
    if let Some(rt) = load_by_flag(pool, "is_admin_default").await? {
        return Ok(Some(rt));
    }
    Ok(None)
}

async fn load_by_id(pool: &Pool, theme_id: i64) -> Result<Option<ResolvedTheme>, ExecError> {
    let theme = Theme::objects()
        .where_(Theme::id.eq(theme_id))
        .first(pool)
        .await?;
    let Some(theme) = theme else { return Ok(None) };
    let brand_colors = BrandColor::objects()
        .where_(BrandColor::theme_id.eq(theme_id))
        .order_by(&[("sort_order", false)])
        .fetch(pool)
        .await?;
    Ok(Some(ResolvedTheme {
        theme,
        brand_colors,
    }))
}

async fn load_by_flag(pool: &Pool, flag: &str) -> Result<Option<ResolvedTheme>, ExecError> {
    let theme = if flag == "is_admin_default" {
        Theme::objects()
            .where_(Theme::is_admin_default.eq(true))
            .fetch(pool)
            .await?
    } else {
        Theme::objects()
            .where_(Theme::is_default.eq(true))
            .fetch(pool)
            .await?
    };
    let Some(theme) = theme.into_iter().next() else {
        return Ok(None);
    };
    let theme_id = theme.id.get().copied().unwrap_or_default();
    let brand_colors = BrandColor::objects()
        .where_(BrandColor::theme_id.eq(theme_id))
        .order_by(&[("sort_order", false)])
        .fetch(pool)
        .await?;
    Ok(Some(ResolvedTheme {
        theme,
        brand_colors,
    }))
}
