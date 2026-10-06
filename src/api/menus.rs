//! `GET /api/v2/menus/` + `GET /api/v2/menus/{slug}/` (#568 / 15).
//!
//! Exposes the navigation the admin builds so headless frontends can
//! render the navbar. Reuses [`crate::navigation::resolve_menus_with`] —
//! the same resolved tree the server-side Tera path produces (page links
//! resolve to `url_path`, external items pass through, nesting
//! preserved). Menu items target `cms_page` rows, so code-defined and
//! UI-created pages appear alike.
//!
//! Both endpoints are viewer-aware (#members) and both take `?locale=`.
//! `detail` additionally takes `?current=<page id>` and marks the active
//! item and its trail, so a client renders highlighting without
//! re-deriving ancestry from URLs.

// Our extractors, not axum's: they refuse a malformed value in the same
// JSON envelope as everything else. See `query::Query`.
use crate::api::query::{Path, Query};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango::core::Column as _;
use rustango::extractors::{SessionUser, Tenant};
use rustango::sql::FetcherPool as _;
use rustango::tenancy::member_auth::CurrentMember;
use serde::Deserialize;

use crate::navigation::{Menu, MenuOptions};

/// Query string shared by both endpoints.
#[derive(Debug, Default, Deserialize)]
pub struct MenuQuery {
    /// `?locale=fr` — localized labels on the **detail** endpoint. Items
    /// with no override for this locale keep their canonical label, so a
    /// partly-translated menu degrades per item rather than blanking. An
    /// unknown or inactive code falls back to the tenant default.
    ///
    /// Accepted but ignored by the list endpoint, which returns only
    /// slugs and names — neither of which is translatable.
    #[serde(default, deserialize_with = "crate::api::query::empty_string_as_none")]
    pub locale: Option<String>,
    /// `?current=<page id>` — mark the item targeting this page
    /// `is_active`, and everything above it `in_active_trail`.
    /// Ignored on the list endpoint, which returns no items.
    #[serde(default, deserialize_with = "crate::api::query::empty_as_none")]
    pub current: Option<i64>,
    /// Paging, so the list honours the same `ListEnvelope` contract as
    /// every other v2 list. It used to return every menu unpaginated and
    /// omit `limit`/`offset` from `meta`, so a client typed against the
    /// shared envelope broke on this one endpoint alone.
    #[serde(default, deserialize_with = "crate::api::query::empty_as_none")]
    pub limit: Option<usize>,
    #[serde(default, deserialize_with = "crate::api::query::empty_as_none")]
    pub offset: Option<usize>,
}

/// `GET /api/v2/menus/` — list configured menus (slug + name).
///
/// One query. It used to call `resolve_all_menus` and throw away every
/// resolved tree, which made listing N menus cost `1 + 3N` queries to
/// return `2N` strings.
pub async fn list(tenant: Tenant, Query(q): Query<MenuQuery>) -> Response {
    match list_inner(tenant.pool(), &q).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, "menus list failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

pub async fn list_inner(
    pool: &rustango::sql::Pool,
    q: &MenuQuery,
) -> Result<Response, rustango::sql::ExecError> {
    let mut menus: Vec<Menu> = Menu::objects().fetch(pool).await?;
    menus.sort_by(|a, b| a.slug.cmp(&b.slug));

    let total_count = menus.len();
    let limit = crate::api::query::clamp_limit(q.limit);
    let offset = q.offset.unwrap_or(0);

    let items: Vec<serde_json::Value> = menus
        .iter()
        .skip(offset)
        .take(limit)
        .map(|m| {
            serde_json::json!({
                "slug": m.slug,
                "name": m.name,
                // Percent-encoded: a slug is only lowercased on save, so
                // one containing `/` would otherwise render a path the
                // single-segment route can never match.
                "detail_url": format!("/api/v2/menus/{}/", encode_slug(&m.slug)),
            })
        })
        .collect();

    // No `locale` echo here: a menu's `slug` and `name` have no
    // translations (only `cms_menu_item_translation` exists), so echoing
    // one told a client its code had been honoured when nothing in the
    // payload was localized. `?locale=` is still accepted and ignored,
    // for clients that pass it uniformly. The detail endpoint, where
    // labels *are* localized, does echo it.
    let meta = crate::api::query::ListMeta::paged(total_count, limit, offset);
    Ok(Json(crate::api::query::ListEnvelope { meta, items }).into_response())
}

/// `GET /api/v2/menus/{slug}/` — the resolved nested tree for one menu.
/// Viewer-aware (#members): items the caller can't access are hidden.
pub async fn detail(
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    CurrentMember(member): CurrentMember,
    crate::api::auth::BearerMember(bearer): crate::api::auth::BearerMember,
    Path(slug): Path<String>,
    Query(q): Query<MenuQuery>,
) -> Response {
    let viewer = member.as_ref().or(bearer.as_ref()).or(session_user.as_ref());
    match detail_inner(tenant.pool(), viewer, &slug, &q).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, slug = %slug, "menu detail failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

/// Takes a `&Pool` for the same reason [`crate::api::tree::tree_inner`]
/// does — so the resolved tree is testable without a PostgreSQL tenant.
pub async fn detail_inner(
    pool: &rustango::sql::Pool,
    viewer: Option<&rustango::tenancy::auth::User>,
    slug: &str,
    q: &MenuQuery,
) -> Result<Response, rustango::sql::ExecError> {
    let locale = crate::translation::resolve_locale(pool, q.locale.as_deref()).await?;

    // `?current=` is a hint for highlighting, not a lookup: an id that
    // doesn't resolve simply means nothing is marked active. Failing the
    // whole menu over it would take the navbar down for a bad link.
    // `?current=` is a highlighting hint. It must not become an oracle:
    // echoing `meta.current` back only for rows that exist told an
    // anonymous caller whether a given id is a real (draft, or
    // member-only) page, and `in_active_trail` then leaked which
    // published subtree an unreleased draft lives under. Only a page this
    // caller could actually be looking at counts.
    let current = match q.current {
        Some(id) => {
            let found = crate::page::Page::objects()
                .where_(crate::page::Page::id.eq(id))
                .where_(crate::page::Page::status.is_in([
                    crate::page::PageStatus::Published.as_str().to_owned(),
                    crate::page::PageStatus::Archived.as_str().to_owned(),
                ]))
                .first(pool)
                .await?;
            match found {
                Some(p) => {
                    let pid = p.id.get().copied().unwrap_or_default();
                    let denied = crate::view_restriction::denied_page_ids(
                        pool,
                        viewer,
                        &[(pid, p.path.clone(), p.page_type_id)],
                    )
                    .await;
                    (!denied.contains(&pid)).then_some(p)
                }
                None => None,
            }
        }
        None => None,
    };

    let opts = MenuOptions {
        viewer,
        locale: locale.as_ref(),
        current: current.as_ref(),
    };
    let Some(menu) = crate::navigation::resolve_menu_with(pool, slug, opts).await? else {
        return Ok(crate::api::error::ApiError::not_found("menu").into_response());
    };

    // The `meta` + `items` envelope every other v2 endpoint uses. `detail`
    // used to return a bare object, so a client had to special-case it.
    Ok(Json(serde_json::json!({
        "meta": {
            "slug": menu.slug,
            "name": menu.name,
            "locale": locale.as_ref().map(|l| l.code.clone()),
            "current": current.as_ref().and_then(|p| p.id.get().copied()),
            "total_count": count_items(&menu.items),
        },
        "items": menu.items,
    }))
    .into_response())
}

/// Percent-encode a menu slug for use as one path segment.
///
/// Slugs are only `trim().to_lowercase()`d on save, so `a/b` is storable
/// and would produce a `detail_url` of `/api/v2/menus/a/b/` — a path the
/// single-segment route cannot match, i.e. a dead link the API itself
/// handed out.
fn encode_slug(slug: &str) -> String {
    let mut out = String::with_capacity(slug.len());
    for b in slug.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Total nodes in the resolved tree, not just top-level ones — the count
/// a client uses to sanity-check what it received.
fn count_items(items: &[crate::navigation::ResolvedMenuItem]) -> usize {
    items.len() + items.iter().map(|i| count_items(&i.children)).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigation::ResolvedMenuItem;

    fn item(children: Vec<ResolvedMenuItem>) -> ResolvedMenuItem {
        ResolvedMenuItem {
            id: None,
            label: String::new(),
            url: Some("/".to_owned()),
            is_page: false,
            page_id: None,
            open_in_new_tab: false,
            is_active: false,
            in_active_trail: false,
            children,
        }
    }

    #[test]
    fn a_slug_with_a_slash_is_encoded_into_one_segment() {
        // Otherwise the advertised detail_url is unroutable.
        assert_eq!(encode_slug("a/b"), "a%2Fb");
        assert_eq!(encode_slug("my menu"), "my%20menu");
    }

    #[test]
    fn an_ordinary_slug_is_left_alone() {
        assert_eq!(encode_slug("main-nav_2.0~x"), "main-nav_2.0~x");
    }

    #[test]
    fn count_items_counts_the_whole_tree() {
        // A top-level-only count would report 2 for a menu of 4 nodes.
        let tree = vec![item(vec![item(vec![]), item(vec![])]), item(vec![])];
        assert_eq!(count_items(&tree), 4);
    }

    #[test]
    fn count_items_on_an_empty_menu_is_zero() {
        assert_eq!(count_items(&[]), 0);
    }
}
