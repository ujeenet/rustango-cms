//! URL → `Page` resolution.
//!
//! Two paths:
//!
//! 1. **Materialized `url_path` short-circuit**. The
//!    request URL is normalized (strip trailing slash unless root)
//!    and looked up directly via `WHERE url_path = ?` — one query
//!    regardless of tree depth. The `url_path` column is indexed and
//!    maintained by `tree_ops` on every create / move / slug change.
//!
//! 2. **Slug-walk fallback** for cases the materialized lookup
//!    misses — historical rows from before slice 1, draft-preview
//!    routes, etc. Walks the slug chain one level at a time
//!    and filters to the public statuses.
//!
//! Both paths serve the **public statuses** — `published` and
//! `archived` (superseded-but-preserved content). Drafts,
//! scheduled, and `expired` (takedown) rows stay invisible to anonymous
//! visitors. The caller reads `page.status` to distinguish the two
//! served states.

use rustango::core::Column as _;
use rustango::extractors::Tenant;
use rustango::sql::FetcherPool as _;

use crate::page::Page;
use crate::tree_ops::TreeError;

/// Statuses a public page can have. `archived` joins `published`;
/// `scheduled` is included only so a page whose go-live time has passed is
/// served before the sweep flips it — [`visible_now`] decides. `expired`
/// is excluded so takedown still 404s. Owned `String`s for the `is_in` binds.
///
/// The query side of "is this page served": filter by these, then keep the
/// rows [`visible_now`] accepts. Anything that links to or lists served
/// pages uses the pair, so it agrees with what a visitor gets.
#[must_use]
pub fn served_statuses() -> [String; 3] {
    [
        crate::page::PageStatus::Published.as_str().to_owned(),
        crate::page::PageStatus::Archived.as_str().to_owned(),
        crate::page::PageStatus::Scheduled.as_str().to_owned(),
    ]
}

/// Whether a page is public at `now`, judged by its dates rather than by
/// whether the schedule sweep has run yet: a scheduled page goes
/// live at `go_live_at`, a published one comes down at `expire_at`.
#[must_use]
pub fn visible_now(page: &Page, now: chrono::DateTime<chrono::Utc>) -> bool {
    use crate::page::PageStatus;
    let status = page.status.as_str();
    if status == PageStatus::Archived.as_str() {
        true
    } else if status == PageStatus::Published.as_str() {
        !page.expire_at.is_some_and(|t| t <= now)
    } else if status == PageStatus::Scheduled.as_str() {
        page.go_live_at.is_some_and(|t| t <= now)
            && !page.expire_at.is_some_and(|t| t <= now)
    } else {
        false
    }
}

/// Normalize a request path into the canonical form stored in
/// `cms_page.url_path`. Leading slash kept; trailing slash stripped
/// for non-root URLs. Empty / missing path collapses to `/`. Exposed
/// for the router's cache-key builder.
pub fn canonical_request_path(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return "/".to_owned();
    }
    let with_leading = if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        format!("/{trimmed}")
    };
    let trailing_stripped = with_leading.trim_end_matches('/').to_owned();
    if trailing_stripped.is_empty() {
        "/".to_owned()
    } else {
        trailing_stripped
    }
}

/// Look up the page matching `url_path` directly. One indexed SELECT.
/// Returns `Ok(None)` when the row is missing OR isn't public
/// (published / archived). The caller reads `page.status` to tell the
/// two apart.
pub async fn resolve_by_url_path(t: &Tenant, url_path: &str) -> Result<Option<Page>, TreeError> {
    let canon = canonical_request_path(url_path);
    // Ordered so a duplicate URL (rows saved before #703 made edits
    // refuse one) always serves the same page: the oldest.
    let hits: Vec<Page> = Page::objects()
        .where_(Page::url_path.eq(canon))
        .where_(Page::status.is_in(served_statuses()))
        .order_by(&[("id", false)])
        .fetch(t.pool())
        .await?;
    let now = chrono::Utc::now();
    Ok(hits.into_iter().find(|p| visible_now(p, now)))
}

/// Walk the URL path one slug at a time. Used as a fallback when the
/// materialized `url_path` lookup misses (legacy rows, preview
/// routes, etc.) and for the per-segment ancestry queries that
/// rendered content depends on.
pub async fn resolve_by_walk(t: &Tenant, url_path: &str) -> Result<Option<Page>, TreeError> {
    let segments: Vec<&str> = url_path.split('/').filter(|s| !s.is_empty()).collect();

    let roots: Vec<Page> = Page::objects()
        .where_(Page::parent_id.is_null())
        .where_(Page::slug.eq(String::new()))
        .where_(Page::status.is_in(served_statuses()))
        .order_by(&[("id", false)])
        .fetch(t.pool())
        .await?;
    let now = chrono::Utc::now();
    let Some(mut current) = roots.into_iter().find(|p| visible_now(p, now)) else {
        return Ok(None);
    };

    for seg in segments {
        let parent_id = match current.id.get().copied() {
            Some(id) => id,
            None => return Ok(None),
        };
        let hits: Vec<Page> = Page::objects()
            .where_(Page::parent_id.eq(parent_id))
            .where_(Page::slug.eq(seg.to_string()))
            .where_(Page::status.is_in(served_statuses()))
            .order_by(&[("id", false)])
            .fetch(t.pool())
            .await?;
        let Some(child) = hits.into_iter().find(|p| visible_now(p, now)) else {
            return Ok(None);
        };
        current = child;
    }
    Ok(Some(current))
}

/// Resolve a request URL to a `Page`. Tries the materialized
/// `url_path` first (one query); falls back to the slug walk when no
/// row is found that way (legacy data, in-flight backfills, etc.).
///
/// `url_path` should be the request path (e.g. `"/"`, `"/about/"`,
/// `"/blog/post-1/"`). Leading + trailing slashes are normalized.
///
/// # Errors
/// Driver / query failures.
pub async fn resolve_path(t: &Tenant, url_path: &str) -> Result<Option<Page>, TreeError> {
    if let Some(page) = resolve_by_url_path(t, url_path).await? {
        return Ok(Some(page));
    }
    resolve_by_walk(t, url_path).await
}

/// When [`resolve_path`] misses, walk ancestor prefixes of
/// `request_path` looking for a `Page` whose registered
/// `PageTypeHandler::routes()` accepts the remaining suffix. Returns
/// the matched page + match details on first hit (longest prefix
/// wins because [`crate::routable::ancestor_prefixes`] orders from
/// most-specific to root).
///
/// Returns `None` when no ancestor + route combination accepts the
/// request — the router then falls through to its redirect /
/// 404 path.
///
/// # Errors
/// Driver / query failures from the per-ancestor `Page` lookup.
pub async fn resolve_routable(
    t: &Tenant,
    request_path: &str,
) -> Result<Option<(Page, crate::routable::RouteMatch)>, TreeError> {
    let canon = canonical_request_path(request_path);
    for ancestor in crate::routable::ancestor_prefixes(&canon) {
        let Some(page) = resolve_by_url_path(t, &ancestor).await? else {
            continue;
        };
        // Pull the handler — fall through to the next ancestor if
        // the page type doesn't have one registered (a real
        // misconfiguration; surfacing it as "no route matched"
        // keeps the failure mode aligned with the slug resolver).
        let Some(pt_row) = crate::page_type_model::PageType::objects()
            .where_(crate::page_type_model::PageType::id.eq(page.page_type_id))
            .first(t.pool())
            .await?
        else {
            continue;
        };
        let Some(handler) = crate::page_type::find_handler(&pt_row.type_name) else {
            continue;
        };
        let routes = handler.routes();
        if routes.is_empty() {
            continue;
        }
        // The suffix is whatever's BELOW the ancestor's url_path. For
        // root (`/`), strip the leading slash. For deeper ancestors,
        // skip the ancestor prefix plus its trailing slash.
        let suffix = if ancestor == "/" {
            canon.trim_start_matches('/')
        } else {
            canon
                .strip_prefix(&ancestor)
                .unwrap_or("")
                .trim_start_matches('/')
        };
        if let Some(matched) = crate::routable::match_routes(&routes, suffix) {
            return Ok(Some((page, matched)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod visible_now_tests {
    use super::visible_now;
    use crate::page::{Page, PageStatus};
    use chrono::{Duration, Utc};

    fn page(status: PageStatus) -> Page {
        use rustango::sql::Auto;
        Page {
            id: Auto::Unset,
            page_type_id: 1,
            title: "t".to_owned(),
            slug: "t".to_owned(),
            path: "0001/".to_owned(),
            url_path: "/t".to_owned(),
            preview_path: String::new(),
            template_override: String::new(),
            depth: 1,
            parent_id: None,
            locale_variant_of: None,
            alias_of: None,
            theme_id: None,
            sort_order: 0,
            status: status.as_str().to_owned(),
            published_at: None,
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
        }
    }

    #[test]
    fn a_scheduled_page_is_public_once_its_go_live_time_passes() {
        let now = Utc::now();
        let mut p = page(PageStatus::Scheduled);
        assert!(!visible_now(&p, now), "no go-live time: not public");
        p.go_live_at = Some(now + Duration::minutes(5));
        assert!(!visible_now(&p, now));
        p.go_live_at = Some(now - Duration::minutes(5));
        assert!(visible_now(&p, now), "due but not yet swept: public");
    }

    #[test]
    fn a_published_page_comes_down_at_its_expiry() {
        let now = Utc::now();
        let mut p = page(PageStatus::Published);
        assert!(visible_now(&p, now));
        p.expire_at = Some(now - Duration::seconds(1));
        assert!(!visible_now(&p, now), "expired but not yet swept: hidden");
    }

    #[test]
    fn drafts_and_expired_pages_are_never_public() {
        let now = Utc::now();
        assert!(!visible_now(&page(PageStatus::Draft), now));
        assert!(!visible_now(&page(PageStatus::Expired), now));
        assert!(visible_now(&page(PageStatus::Archived), now));
    }
}
