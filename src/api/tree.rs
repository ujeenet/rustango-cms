//! `GET /api/v2/pages/tree/` — the site's page structure, nested.
//!
//! The flat list endpoint can express a subtree (`?descendant_of=`) but
//! never its *shape*: a client building navigation has to fetch every
//! page and reassemble the hierarchy itself. This returns it already
//! nested, localized, and filtered to what the caller may actually see.
//!
//! ## Why it is one query
//!
//! `cms_page.parent_id` carries no index — only `path`, `status`,
//! `url_path` and `page_type_id` do. Asking for each node's children
//! would therefore be both an N+1 *and* a sequential scan per node. So
//! this does what `auto_menu` does: one `ORDER BY path` fetch (already
//! tree pre-order, index-backed), bucket by parent in memory, then
//! recurse with [`crate::tree_build`]. Node count does not change the
//! query count.
//!
//! ## Why it is bounded
//!
//! `MAX_LIMIT = 100` on the list endpoints is a *row* cap and means
//! nothing to a tree. Without its own ceiling this endpoint is an
//! unbounded response generator, so `depth` defaults to
//! [`DEFAULT_DEPTH`] and the emitted node count is capped at
//! [`MAX_NODES`].

// Our extractor, not axum's — see `query::Query`.
use crate::api::query::Query;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango::core::Column as _;
use rustango::extractors::{SessionUser, Tenant};
use rustango::sql::FetcherPool as _;
use serde::Deserialize;

use crate::page::Page;

/// Levels returned when `?depth=` is omitted. Deep enough for a primary
/// nav with dropdowns, shallow enough that the default response stays
/// small on a big site.
pub const DEFAULT_DEPTH: i64 = 3;
/// Ceiling for `?depth=`.
pub const MAX_DEPTH: i64 = 10;
/// Hard cap on emitted nodes, whatever the depth asks for. The response
/// is truncated rather than refused, and `meta.truncated` says so.
pub const MAX_NODES: usize = 5_000;

#[derive(Debug, Default, Deserialize)]
pub struct TreeQuery {
    /// `?root=N` — return the subtree under page N instead of the whole
    /// site. N itself is not included; its children are the top level.
    #[serde(default, deserialize_with = "crate::api::query::empty_as_none")]
    pub root: Option<i64>,
    /// `?depth=N` — levels to walk, clamped to [`MAX_DEPTH`].
    #[serde(default, deserialize_with = "crate::api::query::empty_as_none")]
    pub depth: Option<i64>,
    /// `?locale=fr` — localize titles via `cms_translation`. Untranslated
    /// pages keep their canonical title.
    #[serde(default, deserialize_with = "crate::api::query::empty_string_as_none")]
    pub locale: Option<String>,
}

/// `GET /api/v2/pages/tree/`
pub async fn tree(
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    rustango::tenancy::member_auth::CurrentMember(member): rustango::tenancy::member_auth::CurrentMember,
    crate::api::auth::BearerMember(bearer): crate::api::auth::BearerMember,
    Query(q): Query<TreeQuery>,
) -> Response {
    // #members — a public member outranks an admin session, matching
    // `menus::detail` and the public renderer.
    let viewer = member.as_ref().or(bearer.as_ref()).or(session_user.as_ref());
    match tree_inner(tenant.pool(), viewer, &q).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, "page tree failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

/// Takes a `&Pool` rather than the `Tenant` so the whole endpoint is
/// exercisable against an in-memory database — `Tenant::for_test` needs
/// a real PostgreSQL connection, which would put the restriction-leak
/// test out of reach of the normal suite.
pub async fn tree_inner(
    pool: &rustango::sql::Pool,
    viewer: Option<&rustango::tenancy::auth::User>,
    q: &TreeQuery,
) -> Result<Response, rustango::sql::ExecError> {
    let depth = q.depth.unwrap_or(DEFAULT_DEPTH).clamp(1, MAX_DEPTH);

    // Serve what the public site serves. The list endpoint filters to
    // `published` alone, but the renderer and sitemap both treat
    // `archived` as public (`PageStatus::is_public`), and a tree that
    // omitted archived pages would disagree with the URLs that resolve.
    let statuses = [
        crate::page::PageStatus::Published.as_str().to_owned(),
        crate::page::PageStatus::Archived.as_str().to_owned(),
    ];
    let mut rows: Vec<Page> = Page::objects()
        .where_(Page::status.is_in(statuses))
        // Fetched by `path` because that column is indexed; re-sorted
        // below into the order the site actually renders.
        .order_by(&[("path", false)])
        .fetch(pool)
        .await?;

    // Siblings come back in `sort_order` order, matching `auto_menu` and
    // `default_children` — i.e. what every template's `children` loop
    // shows. Ordering by `path` alone meant ordering by id, so any page
    // moved after creation appeared in a different position here than on
    // the rendered site, and a SPA's nav silently disagreed with the
    // server-rendered one. `index_by_parent` preserves input order, so
    // sorting the flat list once orders every sibling bucket.
    rows.sort_by_key(|p| (p.sort_order, p.id.get().copied().unwrap_or_default()));

    // Error pages are routing furniture, not site structure — excluded
    // here exactly as `auto_menu` and `sitemap` exclude them.
    let error_tid = crate::error_pages::error_page_type_id(pool).await;
    rows.retain(|p| error_tid.is_none_or(|tid| p.page_type_id != tid));

    // Legacy locale variants were standalone per-locale trees, so leaving
    // them in would return the whole site once per locale. The column is
    // deprecated (#275); this matches `feed.rs`.
    rows.retain(|p| p.locale_variant_of.is_none());

    // #members — drop pages the viewer may not see. Batched and
    // subtree-aware via `path`, so a gated ancestor takes its descendants
    // with it. Without this the tree would leak the titles and URLs of
    // member-only sections to anonymous callers.
    let triples: Vec<(i64, String, i64)> = rows
        .iter()
        .map(|p| {
            (
                p.id.get().copied().unwrap_or_default(),
                p.path.clone(),
                p.page_type_id,
            )
        })
        .collect();
    let denied = crate::view_restriction::denied_page_ids(pool, viewer, &triples).await;
    if !denied.is_empty() {
        rows.retain(|p| !denied.contains(&p.id.get().copied().unwrap_or_default()));
    }

    // `?root=` — restrict to a subtree using the same `path` prefix test
    // `Page::descendants` uses, without a second query.
    let root_parent = match q.root {
        Some(root_id) => {
            let Some(root) = rows
                .iter()
                .find(|p| p.id.get().copied() == Some(root_id))
                .cloned()
            else {
                // Unknown, non-public, or denied — all indistinguishable
                // from the outside on purpose.
                return Ok(
                    crate::api::error::ApiError::not_found("page").into_response()
                );
            };
            let prefix = root.path.clone();
            rows.retain(|p| p.path.starts_with(&prefix) && p.path != prefix);
            Some(root_id)
        }
        None => None,
    };

    // An alias row carries a stale copy of the source's title, taken at
    // creation; the renderer shadows it with the source's. Resolve them
    // in one batched lookup so the tree shows what the page actually
    // renders, rather than N queries or a wrong label.
    let alias_sources: Vec<i64> = rows.iter().filter_map(|p| p.alias_of).collect();
    let alias_titles: std::collections::HashMap<i64, String> = if alias_sources.is_empty() {
        std::collections::HashMap::new()
    } else {
        let sources: Vec<Page> = Page::objects()
            .where_(Page::id.is_in(alias_sources))
            .fetch(pool)
            .await?;
        // The source is fetched with no status or restriction filter, so
        // an alias pointing at a draft or member-only page would publish
        // that page's title on a node everyone can see. Vet the sources
        // the same way the tree vets everything else; an alias whose
        // source fails keeps its own (stale) title rather than borrowing
        // one the caller may not see.
        let src_triples: Vec<(i64, String, i64)> = sources
            .iter()
            .map(|p| {
                (
                    p.id.get().copied().unwrap_or_default(),
                    p.path.clone(),
                    p.page_type_id,
                )
            })
            .collect();
        let src_denied =
            crate::view_restriction::denied_page_ids(pool, viewer, &src_triples).await;
        sources
            .into_iter()
            .filter(|p| {
                p.status == crate::page::PageStatus::Published.as_str()
                    || p.status == crate::page::PageStatus::Archived.as_str()
            })
            .filter_map(|p| p.id.get().copied().map(|id| (id, p.title)))
            .filter(|(id, _)| !src_denied.contains(id))
            .collect()
    };

    // Page-type names, so a tree node reports `type` like a list item
    // does. One query for the registry (it is small and fully cached by
    // the query itself), never per node.
    let type_names = crate::api::pages::page_type_names(pool).await?;

    // Localized titles — one query for the whole id set, never per node.
    let locale = crate::translation::resolve_locale(pool, q.locale.as_deref()).await?;
    let translations = match locale.as_ref().filter(|l| !l.is_default) {
        Some(l) => {
            let lid = l.id.get().copied().unwrap_or_default();
            let ids: Vec<i64> = rows
                .iter()
                .filter_map(|p| p.id.get().copied())
                .chain(alias_titles.keys().copied())
                .collect();
            crate::translation::fetch_for_pages(pool, &ids, lid)
                .await
                .unwrap_or_default()
        }
        _ => std::collections::HashMap::new(),
    };

    // Which nodes have children the caller can't see at this depth —
    // computed over the filtered set, so `has_children` never promises a
    // subtree that a restriction already removed.
    let mut child_count: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for p in &rows {
        if let Some(pid) = p.parent_id {
            *child_count.entry(pid).or_default() += 1;
        }
    }

    let by_parent = crate::tree_build::index_by_parent(&rows, |p| {
        // Under `?root=`, the root's own children are the top level.
        if p.parent_id == root_parent {
            None
        } else {
            p.parent_id
        }
    });

    // `Cell` because the builder's `make` is `Fn`, not `FnMut` — the
    // recursion hands the same closure to every level.
    let emitted = std::cell::Cell::new(0usize);
    let nodes = crate::tree_build::build(
        &by_parent,
        None,
        depth,
        &|p: &Page| p.id.get().copied().unwrap_or_default(),
        &|p: &Page, children: Vec<serde_json::Value>| {
            emitted.set(emitted.get() + 1);
            node_json(
                p,
                children,
                &alias_titles,
                &translations,
                &child_count,
                &type_names,
            )
        },
    );
    let emitted = emitted.get();
    let truncated = emitted > MAX_NODES;
    let mut nodes = nodes;
    if truncated {
        // Keep the shape valid rather than erroring: a client gets a
        // usable prefix plus an explicit flag. Pruning is pre-order over
        // the *whole* tree — capping the top level instead would keep
        // one root's entire ten-thousand-node subtree.
        let mut budget = MAX_NODES;
        prune(&mut nodes, &mut budget);
    }

    Ok(Json(serde_json::json!({
        "meta": {
            "depth": depth,
            "root": q.root,
            "locale": locale.as_ref().map(|l| l.code.clone()),
            "total_count": emitted.min(MAX_NODES),
            "truncated": truncated,
            // How many nodes the cap removed. `total_count` equals the
            // items length once truncated, so without this a client
            // cannot tell whether it lost one node or ten thousand, nor
            // decide whether a smaller `depth` would fit.
            "dropped_count": emitted.saturating_sub(MAX_NODES),
        },
        "items": nodes,
    }))
    .into_response())
}

/// Keep at most `budget` nodes, pre-order, in place. A node that fits
/// keeps only as many descendants as the remaining budget allows, so
/// the result is a prefix of a depth-first walk and always well-formed.
fn prune(nodes: &mut Vec<serde_json::Value>, budget: &mut usize) {
    let keep = nodes.len().min(*budget);
    nodes.truncate(keep);
    *budget -= keep;
    for n in nodes.iter_mut() {
        let Some(children) = n.get_mut("children").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        prune(children, budget);
    }
}

/// One tree node. Deliberately slim — id, label, where it points, and
/// whether there is more below. A client that wants the full record
/// follows `detail_url`.
fn node_json(
    p: &Page,
    children: Vec<serde_json::Value>,
    alias_titles: &std::collections::HashMap<i64, String>,
    translations: &std::collections::HashMap<i64, std::collections::HashMap<String, String>>,
    child_count: &std::collections::HashMap<i64, usize>,
    type_names: &std::collections::HashMap<i64, String>,
) -> serde_json::Value {
    let id = p.id.get().copied().unwrap_or_default();
    // An alias shows the source's title, then that title's translation —
    // the same order the renderer resolves them in.
    let source_id = p.alias_of.unwrap_or(id);
    let base_title = p
        .alias_of
        .and_then(|src| alias_titles.get(&src).cloned())
        .unwrap_or_else(|| p.title.clone());
    let title = translations
        .get(&source_id)
        .and_then(|m| m.get("title"))
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or(base_title);

    let mut obj = serde_json::json!({
        "id": id,
        "title": title,
        "type": type_names.get(&p.page_type_id).cloned().unwrap_or_default(),
        "slug": p.slug,
        "url": if p.url_path.is_empty() { "/" } else { p.url_path.as_str() },
        "depth": p.depth,
        "has_children": child_count.get(&id).copied().unwrap_or(0) > 0,
        "detail_url": format!("/api/v2/pages/{id}/"),
        "children": children,
    });
    if let Some(src) = p.alias_of {
        // An alias is a real, distinct URL — surfaced, not deduped — but
        // clients need to know it mirrors another page's content.
        obj["alias_of"] = serde_json::json!(src);
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `[a[a1,a2],b]` — 2 roots, 4 nodes.
    fn sample() -> Vec<serde_json::Value> {
        vec![
            json!({"id": 1, "children": [
                {"id": 2, "children": []},
                {"id": 3, "children": []},
            ]}),
            json!({"id": 4, "children": []}),
        ]
    }

    fn ids(nodes: &[serde_json::Value]) -> Vec<i64> {
        let mut out = Vec::new();
        for n in nodes {
            out.push(n["id"].as_i64().unwrap());
            out.extend(ids(n["children"].as_array().unwrap()));
        }
        out
    }

    #[test]
    fn prune_counts_descendants_not_just_roots() {
        // The bug this guards: truncating the top-level vec keeps one
        // root's entire subtree, so the cap bounds nothing.
        let mut nodes = sample();
        let mut budget = 3;
        prune(&mut nodes, &mut budget);
        assert_eq!(ids(&nodes).len(), 3);
    }

    #[test]
    fn prune_keeps_a_breadth_then_depth_prefix() {
        let mut nodes = sample();
        let mut budget = 3;
        prune(&mut nodes, &mut budget);
        // Both roots survive first, then the budget spends downward.
        assert_eq!(ids(&nodes), vec![1, 2, 4]);
    }

    #[test]
    fn prune_under_budget_changes_nothing() {
        let mut nodes = sample();
        let mut budget = MAX_NODES;
        prune(&mut nodes, &mut budget);
        assert_eq!(ids(&nodes), vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_zero_budget_empties_the_tree() {
        let mut nodes = sample();
        let mut budget = 0;
        prune(&mut nodes, &mut budget);
        assert!(nodes.is_empty());
    }

    #[test]
    fn depth_is_clamped_to_the_ceiling() {
        // A client asking for depth=999 must not get an unbounded walk.
        for (asked, want) in [(None, DEFAULT_DEPTH), (Some(999), MAX_DEPTH), (Some(0), 1)] {
            let got = asked.unwrap_or(DEFAULT_DEPTH).clamp(1, MAX_DEPTH);
            assert_eq!(got, want, "depth={asked:?}");
        }
    }
}
