//! `GET /api/v2/pages/` + `GET /api/v2/pages/{id}/`.
//!
//! Lists + retrieves published pages over JSON.
//!
//! Every surface here is viewer-aware. `detail` runs
//! [`crate::view_restriction_guard`] and answers a denial with 401 / 403;
//! `list` (and its `?child_of=` / `?descendant_of=` / `?ancestor_of=`
//! filters), `find` and the `children` summaries all drop what the caller
//! may not see via [`crate::view_restriction::denied_page_ids`]. `find`
//! answers a gated page and a missing one identically, so it cannot be
//! used to probe for hidden URLs.

// Our extractors, not axum's: they refuse a malformed value in the same
// JSON envelope as everything else. See `query::Query`.
use crate::api::query::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango::core::Column as _;
use rustango::extractors::{SessionUser, Tenant};
use rustango::sql::FetcherPool as _;
use serde::Deserialize;

use super::query as q;
use crate::page::{Page, PageStatus};
use crate::page_type::{find_handler, PageTypeHandler};
use crate::page_type_model::PageType;

/// Per-endpoint filters layered on top of [`crate::api::query::ListQuery`].
#[derive(Debug, Default, Deserialize)]
pub struct PageFilters {
    /// `?type=cms_pages.HomePage` — handler type_name match.
    #[serde(default, deserialize_with = "q::empty_string_as_none")]
    pub r#type: Option<String>,
    /// `?child_of=N` — direct children of page N.
    #[serde(default, deserialize_with = "q::empty_as_none")]
    pub child_of: Option<i64>,
    /// `?descendant_of=N` — every descendant of page N (excluding N).
    #[serde(default, deserialize_with = "q::empty_as_none")]
    pub descendant_of: Option<i64>,
    /// `?ancestor_of=N` — every ancestor of page N (excluding N).
    #[serde(default, deserialize_with = "q::empty_as_none")]
    pub ancestor_of: Option<i64>,
    /// `?translation_of=N` — sibling locale variants of page N.
    #[serde(default, deserialize_with = "q::empty_as_none")]
    pub translation_of: Option<i64>,
    /// `?locale=en` — restrict to a specific locale.
    #[serde(default, deserialize_with = "q::empty_string_as_none")]
    pub locale: Option<String>,
    /// `?tag=release` — restrict to pages carrying this tag (#189).
    #[serde(default, deserialize_with = "q::empty_string_as_none")]
    pub tag: Option<String>,
    /// `?id_in=3,9,14` — fetch several pages by id in one request.
    ///
    /// Without it, hydrating a resolved menu (which returns `page_id`
    /// per node and nothing else) costs one request per item. Unknown
    /// and non-public ids are simply absent from the result rather than
    /// an error, so a stale client id degrades to a missing row instead
    /// of failing the whole batch. Bounded by `?limit=` like any list.
    #[serde(default, deserialize_with = "q::empty_string_as_none")]
    pub id_in: Option<String>,
}

/// Combined query string for `GET /api/v2/pages/`.
#[derive(Debug, Default, Deserialize)]
pub struct PageListQuery {
    #[serde(flatten)]
    pub list: q::ListQuery,
    #[serde(flatten)]
    pub filters: PageFilters,
}

/// Query string for `GET /api/v2/pages/find/`.
#[derive(Debug, Default, Deserialize)]
pub struct FindQuery {
    /// Public URL path to resolve, e.g. `/about` (with or without a
    /// trailing slash).
    #[serde(default, deserialize_with = "q::empty_string_as_none")]
    pub html_path: Option<String>,
}

/// Query for the page detail endpoint.
#[derive(Default, Deserialize)]
pub struct DetailQuery {
    /// #430 — a signed preview token (from the admin mint endpoint).
    /// When valid for this page id, the endpoint serves the page even
    /// if it isn't published, so a decoupled frontend can render the
    /// draft.
    #[serde(default, deserialize_with = "q::empty_string_as_none")]
    pub preview_token: Option<String>,
    /// `?locale=fr` — return this locale's content. Overrides from
    /// `cms_translation` are applied per field, so anything untranslated
    /// falls back to canonical rather than blanking. An unknown or
    /// inactive code falls back to the tenant default.
    ///
    /// Note this is a *content selector*, unlike the list endpoint's
    /// `?locale=`, which is a legacy row filter over the deprecated
    /// `locale_variant_of` column.
    #[serde(default, deserialize_with = "q::empty_string_as_none")]
    pub locale: Option<String>,
    /// `?fields=title,seo_title` — same sparse selection the list
    /// endpoints accept. It was list-only, so a client fetching one page
    /// had no way to avoid pulling `extension`, `builder` and the whole
    /// `children` array. `id` and `meta` are always kept.
    #[serde(default, deserialize_with = "q::empty_string_as_none")]
    pub fields: Option<String>,
}

/// `GET /api/v2/pages/`
pub async fn list(
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    rustango::tenancy::member_auth::CurrentMember(member): rustango::tenancy::member_auth::CurrentMember,
    crate::api::auth::BearerMember(bearer): crate::api::auth::BearerMember,
    Query(qs): Query<PageListQuery>,
) -> Response {
    // #members — a public member outranks an admin session, matching
    // `tree`, `menus::detail` and the public renderer.
    let viewer = member.as_ref().or(bearer.as_ref()).or(session_user.as_ref());
    match list_inner(&tenant, viewer, &qs).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, "pages list failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

async fn list_inner(
    tenant: &Tenant,
    viewer: Option<&rustango::tenancy::auth::User>,
    qs: &PageListQuery,
) -> Result<Response, rustango::sql::ExecError> {
    let pool = tenant.pool();
    let (limit, offset) = q::paginate(&qs.list);
    let fields = q::field_set(&qs.list);
    let orderings = match q::parse_order_in(&qs.list, &PAGE_ORDER_FIELDS) {
        Ok(v) => v,
        Err(msg) => return Ok(crate::api::error::ApiError::bad_request(msg).into_response()),
    };

    // Load every public page once. Tenants stay small; downstream
    // filters are applied in memory so the API can serve compound
    // queries (descendant_of + locale + search + order) without
    // composing one mega-WHERE clause.
    //
    // Public means `published` **or** `archived` (`PageStatus::is_public`),
    // matching the renderer, the sitemap, the tree, the child summaries
    // and the menu resolver. This was the last surface filtering to
    // `published` alone, which made it describe a different site than
    // every other endpoint: an archived page resolved at its URL, showed
    // up in the tree and in a menu, and was absent here — so
    // `?id_in=` could not even re-fetch a page a menu had just named.
    let mut pages: Vec<Page> = Page::objects()
        .where_(Page::status.is_in([
            PageStatus::Published.as_str().to_owned(),
            PageStatus::Archived.as_str().to_owned(),
        ]))
        // A deterministic base order. Without it the driver decides, and
        // on Postgres an unrelated UPDATE moves a row to the end of the
        // heap — so a client walking `?offset=` could see one page twice
        // and miss another. `id` is unique, which is what the in-memory
        // comparators below lack.
        .order_by(&[("id", false)])
        .fetch(pool)
        .await?;

    // Legacy per-locale variant trees were standalone copies of the whole
    // site, so leaving them in returned it once per locale. Every other
    // surface drops them (#275); this one did not.
    //
    // `?translation_of=` is the exception — it exists precisely to list
    // those rows, so it opts out.
    if qs.filters.translation_of.is_none() {
        pages.retain(|p| p.locale_variant_of.is_none());
    }

    // #members — drop what this viewer may not see, *before* any filter
    // runs. This endpoint applied no restrictions at all, so every gated
    // page's title, SEO text and URL was public — and `?descendant_of=`
    // against a member-only section dumped the lot, the very thing
    // `tree/?root=` refuses. Batched and subtree-aware via `path`, so a
    // gated ancestor takes its descendants with it.
    let triples: Vec<(i64, String, i64)> = pages
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
        pages.retain(|p| !denied.contains(&p.id.get().copied().unwrap_or_default()));
    }

    // ----- type filter --------------------------------------------------
    if let Some(type_name) = qs.filters.r#type.as_deref().filter(|s| !s.is_empty()) {
        // `cms_page_type.type_name` stores the bare handler name and
        // keeps `app_label` in its own column, so the dotted form the
        // doc comment advertises (`cms_pages.HomePage`) could never
        // match. Accept both rather than making the documentation wrong.
        let bare = type_name.rsplit('.').next().unwrap_or(type_name);
        if let Some(pt) = lookup_page_type(pool, bare).await? {
            let pt_id = pt.id.get().copied().unwrap_or_default();
            pages.retain(|p| p.page_type_id == pt_id);
        } else {
            pages.clear();
        }
    }

    // #changes — `?updated_since=` so a polling client can ask for only
    // what moved, instead of re-fetching the collection and diffing it.
    let updated_since = match crate::api::query::parse_updated_since(&qs.list) {
        Ok(v) => v,
        Err(msg) => return Ok(crate::api::error::ApiError::bad_request(msg).into_response()),
    };
    if let Some(since) = updated_since {
        pages.retain(|p| p.updated_at.get().copied().is_some_and(|t| t >= since));
    }

    // ----- explicit id set ----------------------------------------------
    if let Some(raw) = qs.filters.id_in.as_deref() {
        // An *unknown* id is deliberately absent rather than an error, so
        // one stale id costs a batch one row instead of all of them. A
        // segment that is not a number at all is a different thing: it
        // was never an id, so it cannot be stale, and dropping it made a
        // client's own bug look like a deleted page — the worst possible
        // reading, on the endpoint whose whole job is hydrating a list of
        // ids a menu just handed over.
        let mut wanted = std::collections::HashSet::new();
        for seg in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match seg.parse::<i64>() {
                Ok(id) => {
                    wanted.insert(id);
                }
                Err(_) => {
                    return Ok(crate::api::error::ApiError::bad_request(format!(
                        "`id_in` expects comma-separated integers; `{seg}` is not one",
                    ))
                    .into_response())
                }
            }
        }
        pages.retain(|p| p.id.get().copied().is_some_and(|id| wanted.contains(&id)));
    }

    // ----- tree filters -------------------------------------------------
    if let Some(parent_id) = qs.filters.child_of {
        pages.retain(|p| p.parent_id == Some(parent_id));
    }
    // The anchors are resolved against the *visible* set, not the whole
    // table. Looking them up unrestricted would let a caller pivot off a
    // page they cannot see: the anchor's path is all `?descendant_of=`
    // needs, so an unfiltered lookup re-opened the leak the filter above
    // just closed.
    if let Some(root_id) = qs.filters.descendant_of {
        if let Some(prefix) = visible_path(&pages, root_id) {
            pages.retain(|p| p.path != prefix && p.path.starts_with(&prefix));
        } else {
            pages.clear();
        }
    }
    if let Some(leaf_id) = qs.filters.ancestor_of {
        if let Some(path) = visible_path(&pages, leaf_id) {
            let ancestor_paths: std::collections::HashSet<String> = ancestor_paths_of(&path);
            pages.retain(|p| ancestor_paths.contains(&p.path));
        } else {
            pages.clear();
        }
    }

    // ----- translation_of (locale-variant siblings) ---------------------
    if let Some(canon_id) = qs.filters.translation_of {
        // The *other* translations, not the anchor — asking for a page's
        // translations and getting the page back is Wagtail's contract
        // and the only useful answer.
        pages.retain(|p| p.locale_variant_of == Some(canon_id));
    }

    // ----- locale -------------------------------------------------------
    if let Some(code) = qs.filters.locale.as_deref().filter(|s| !s.is_empty()) {
        // Resolve through the shared helper: it honours `active` and
        // falls back to the tenant default on an unknown code, which is
        // what `tree`, `detail` and `menus` all do. This used to query
        // `cms_locale` directly with no `active` filter and *empty the
        // list* on an unknown code — two divergences from every other
        // endpoint in one branch.
        let locale = crate::translation::resolve_locale(pool, Some(code)).await?;
        if let Some(loc) = locale {
            let loc_id = loc.id.get().copied().unwrap_or_default();
            if loc.is_default {
                // Canonical content lives on the page row; default-locale
                // filter is a no-op (every page is "in" the default).
            } else {
                // For non-default locales, a page is "in" the locale if
                // it's a `locale_variant_of` chain rooted in that locale.
                // V1 ships a simpler proxy: only return rows explicitly
                // marked as variants. Future revisions can join the
                // translation table to surface partially-translated rows.
                pages.retain(|p| {
                    p.locale_variant_of.is_some() && {
                        // Fetch the canonical's locale would require a join;
                        // for v1 we accept the variant rows themselves and
                        // leave canonical filtering as a follow-up.
                        let _ = loc_id;
                        true
                    }
                });
            }
        }
    }

    // ----- tag ----------------------------------------------------------
    if let Some(raw) = qs.filters.tag.as_deref().filter(|s| !s.is_empty()) {
        let tagged: std::collections::HashSet<i64> = crate::page_tag::pages_with_tag(pool, raw)
            .await?
            .into_iter()
            .collect();
        pages.retain(|p| p.id.get().copied().is_some_and(|id| tagged.contains(&id)));
    }

    // ----- search -------------------------------------------------------
    // Capture the raw needle so the promotion-pinning step below can run
    // off the same value the title/slug/url_path filter uses.
    let search_needle = qs.list.search.as_deref().filter(|s| !s.trim().is_empty());
    // #408 — relevance-ranked search. An installed external backend
    // (Elasticsearch) takes priority; else Postgres full-text; else
    // (None) the substring filter below. The ranked hit list also drives
    // result order (Wagtail-style relevance default).
    let mut fts_ordered = false;
    if let Some(needle) = search_needle {
        crate::search_promotion::log_public_query(pool, needle, offset);
        let ranked = if let Some(b) = crate::search::backend() {
            Some(b.search(&tenant.org.slug, needle, 1000).await)
        } else {
            crate::search::search_page_ids(pool, needle, 1000).await
        };
        // A ranked hit OR a substring match (#702): the ranked backend finds
        // stemmed words but not a partial word, slug fragment or URL path,
        // so it adds to the substring match every backend runs rather than
        // replacing it. Ranked hits lead, in rank order; substring-only
        // hits follow in their existing order (the sort is stable).
        let rank: Option<std::collections::HashMap<i64, usize>> =
            ranked.map(|r| r.iter().enumerate().map(|(i, id)| (*id, i)).collect());
        let n = crate::api::search::fold(needle);
        pages.retain(|p| {
            rank.as_ref()
                .is_some_and(|r| p.id.get().copied().is_some_and(|id| r.contains_key(&id)))
                || crate::api::search::contains_folded(&p.title, &n)
                || crate::api::search::contains_folded(&p.slug, &n)
                || crate::api::search::contains_folded(&p.url_path, &n)
        });
        if let Some(rank) = &rank {
            pages.sort_by_key(|p| {
                p.id.get()
                    .copied()
                    .and_then(|id| rank.get(&id).copied())
                    .unwrap_or(usize::MAX)
            });
            fts_ordered = true;
        }
    }

    // ----- ordering -----------------------------------------------------
    // FTS already ordered by relevance; an explicit ?order= still wins.
    // Drop specs naming a field this endpoint cannot sort on, *before*
    // deciding whether an ordering was requested. `parse_order` accepts
    // any name and `compare_pages` skipped unknown ones per pair, so
    // `random` reshuffles per request, so an offset into it is not a
    // second page — it is a fresh shuffle with rows skipped. Refuse the
    // combination rather than quietly returning overlapping garbage.
    if orderings.iter().any(|o| o.random) && offset > 0 {
        return Ok(crate::api::error::ApiError::bad_request(
            "`order=random` cannot be paged — omit `offset`, or order by a field",
        )
        .into_response());
    }

    if fts_ordered && orderings.is_empty() {
        // keep the relevance order from the FTS hit list
    } else if orderings.iter().any(|o| o.random) {
        // Random is one-shot per request; cheap on bounded list size.
        use rand::seq::SliceRandom as _;
        pages.shuffle(&mut rand::rng());
    } else if !orderings.is_empty() {
        pages.sort_by(|a, b| compare_pages(a, b, &orderings));
    } else {
        // Default: most recently published first, then title.
        pages.sort_by(|a, b| {
            b.published_at
                .cmp(&a.published_at)
                .then_with(|| a.title.cmp(&b.title))
                // Unique tiebreaker: `sort_by` is stable, so without one
                // tied rows keep the fetch order and paging is only as
                // stable as the driver happens to be.
                .then_with(|| a.id.get().cmp(&b.id.get()))
        });
    }

    // ----- search promotions (#289) -------------------------------------
    // After the search filter + ordering settle, pin editor-promoted
    // pages to the front of the list. Skipped when the request didn't
    // include a search needle (no query to look up), when no
    // SearchQuery row matches the normalized form (visitor's query
    // hasn't been logged + promoted), or when the query has no
    // promotions attached.
    //
    // Promotion order: pages are inserted in `sort_order` ascending,
    // de-duplicated against whatever was already at the head of the
    // filtered list (a page that organically ranked first AND was
    // promoted stays where the promotion put it, doesn't appear
    // twice).
    if let Some(needle) = search_needle {
        if let Ok(Some(q)) = crate::search_promotion::find_by_query(pool, needle).await {
            let query_id = q.id.get().copied().unwrap_or_default();
            if query_id != 0 {
                if let Ok(promos) =
                    crate::search_promotion::promotions_for_query(pool, query_id).await
                {
                    // Pull each promoted page out of the filtered list
                    // (preserving its full Page row — we still want
                    // status, published_at, etc. as ranked) and reinsert
                    // at the front in promotion order. Promoted pages
                    // that aren't in the filtered result set (because
                    // they failed the type/locale/tag/search filter)
                    // stay dropped; the promotion table doesn't
                    // override the visitor's filter scope.
                    let mut pinned: Vec<Page> = Vec::with_capacity(promos.len());
                    for p in &promos {
                        if let Some(pos) = pages
                            .iter()
                            .position(|page| page.id.get().copied() == Some(p.page_id))
                        {
                            pinned.push(pages.remove(pos));
                        }
                    }
                    // Splice pinned to the front in promotion order.
                    for (i, page) in pinned.into_iter().enumerate() {
                        pages.insert(i, page);
                    }
                }
            }
        }
    }

    // ----- pagination ---------------------------------------------------
    let total_count = pages.len();
    let window: Vec<&Page> = pages.iter().skip(offset).take(limit).collect();

    // #408 — a fuzzy "did you mean?" when an FTS search matched nothing
    // but a near-match title exists (Postgres + pg_trgm only; None
    // otherwise). Only offered for FTS-backed searches that came up empty.
    // #408 — the suggestion comes from a similarity query that joins
    // nothing and spans `published + archived`, so left alone it will
    // happily name a member-only or archived page by title. Confirm the
    // suggested title actually belongs to something this caller can see
    // before offering it. One extra query, and only on a zero-hit search.
    let did_you_mean = match (fts_ordered, total_count, search_needle) {
        (true, 0, Some(needle)) => match crate::search::did_you_mean(pool, needle, 0.3).await {
            Some(title) => visible_title(pool, viewer, &title).await,
            None => None,
        },
        _ => None,
    };

    // ----- serialize ----------------------------------------------------
    let type_names = page_type_names(pool).await?;
    let mut items = Vec::with_capacity(window.len());
    for p in window {
        let mut obj = serialize_summary(p, &type_names);
        // Validated against the first serialized row rather than a
        // hand-written list, so it cannot drift from what we return.
        if let Err(msg) = q::validate_fields(fields.as_ref(), &obj) {
            return Ok(crate::api::error::ApiError::bad_request(msg).into_response());
        }
        q::apply_fields(&mut obj, fields.as_ref());
        items.push(serde_json::Value::Object(obj));
    }

    let envelope = q::ListEnvelope {
        meta: q::ListMeta {
            did_you_mean,
            ..q::ListMeta::paged(total_count, limit, offset)
        },
        items,
    };
    Ok(Json(envelope).into_response())
}

/// `GET /api/v2/pages/{id}/`
pub async fn detail(
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    rustango::tenancy::member_auth::CurrentMember(member): rustango::tenancy::member_auth::CurrentMember,
    crate::api::auth::BearerMember(bearer): crate::api::auth::BearerMember,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(qs): Query<DetailQuery>,
) -> Response {
    // #members — gate against a public member OR an admin (member wins).
    let viewer = member.as_ref().or(bearer.as_ref()).or(session_user.as_ref());
    // #430 — a valid preview token for THIS page unlocks draft access.
    let allow_draft = qs
        .preview_token
        .as_deref()
        .and_then(|t| crate::preview_token::verify(&tenant.org.slug, t, chrono::Utc::now().timestamp()))
        == Some(id);
    match detail_inner(
        &tenant,
        viewer,
        &headers,
        id,
        allow_draft,
        qs.locale.as_deref(),
        qs.fields.as_deref(),
    )
    .await
    {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, "page detail failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

/// `GET /api/v2/pages/find/?html_path=/about` — resolve a public URL
/// path to its page and 302-redirect to the detail endpoint. Wagtail's
/// `/api/v2/pages/find/` parity. Resolves **published** pages only; view
/// restrictions are enforced by the detail endpoint the client follows
/// to (matching Wagtail — `find` itself doesn't pre-check them).
pub async fn find(
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    rustango::tenancy::member_auth::CurrentMember(member): rustango::tenancy::member_auth::CurrentMember,
    crate::api::auth::BearerMember(bearer): crate::api::auth::BearerMember,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    Query(qs): Query<FindQuery>,
) -> Response {
    let Some(html_path) = qs.html_path.filter(|p| !p.is_empty()) else {
        return crate::api::error::ApiError::bad_request("missing `html_path` query parameter")
            .into_response();
    };
    let viewer = member.as_ref().or(bearer.as_ref()).or(session_user.as_ref());
    let carry = carried_query(raw_query.as_deref());
    match find_inner(&tenant, viewer, &html_path, &carry).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, "page find failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

/// The part of `find/`'s own query string that the detail URL should
/// inherit, rendered as `?a=b&c=d` (or empty).
///
/// A client resolves its route with `find/` and lets `fetch` follow the
/// 302 — that is the whole point of answering with a redirect. But the
/// `Location` named only the id, so every other parameter was dropped in
/// flight: `find/?html_path=/&locale=fr` returned the **English** page,
/// and a SPA had no way to ask for anything else in one hop. Worse, it
/// looked like it had worked; only `meta.locale` gave it away.
///
/// `html_path` is consumed here and must not travel. Everything else is
/// forwarded verbatim, including parameters this endpoint knows nothing
/// about — the detail endpoint ignores names it does not recognise, and
/// hard-coding a list here would silently drop the next one added.
fn carried_query(raw: Option<&str>) -> String {
    let Some(raw) = raw.filter(|r| !r.is_empty()) else {
        return String::new();
    };
    let kept: Vec<&str> = raw
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let name = pair.split('=').next().unwrap_or("");
            name != "html_path"
        })
        .collect();
    if kept.is_empty() {
        String::new()
    } else {
        format!("?{}", kept.join("&"))
    }
}

async fn find_inner(
    tenant: &Tenant,
    viewer: Option<&rustango::tenancy::auth::User>,
    html_path: &str,
    carry: &str,
) -> Result<Response, rustango::sql::ExecError> {
    let pool = tenant.pool();
    // url_path is stored without a trailing slash (e.g. "/about"); accept
    // either form from callers by trying the path and its slash-variant.
    let mut rows: Vec<Page> = Page::objects()
        .where_(Page::url_path.eq(html_path))
        .fetch(pool)
        .await?;
    if rows.is_empty() {
        let alt = if let Some(stripped) = html_path.strip_suffix('/') {
            stripped.to_owned()
        } else {
            format!("{html_path}/")
        };
        rows = Page::objects()
            .where_(Page::url_path.eq(alt))
            .fetch(pool)
            .await?;
    }
    // `is_public()`, not `Published`: `list`, `tree` and `detail` all
    // serve archived pages, and the tree hands out a `detail_url` for
    // one. Matching on `Published` alone left `find` as the single
    // endpoint that 404'd a page the rest of the API was happy to
    // return — the same contradiction archived pages already caused on
    // `detail`, one endpoint over.
    let hit = rows
        .into_iter()
        .find(|p| PageStatus::str_is_public(&p.status));

    // #members — a gated page must be indistinguishable from a missing
    // one. Redirecting for one and 404-ing for the other turned this
    // endpoint into an oracle: an anonymous caller could walk URLs and
    // learn both which member-only pages exist and their ids, straight
    // out of the `Location` header.
    let id = match hit {
        Some(p) => {
            let pid = p.id.get().copied().unwrap_or_default();
            let denied = crate::view_restriction::denied_page_ids(
                pool,
                viewer,
                &[(pid, p.path.clone(), p.page_type_id)],
            )
            .await;
            if denied.contains(&pid) {
                None
            } else {
                Some(pid)
            }
        }
        None => None,
    };
    match id {
        // 302 Found → detail endpoint (which enforces view restrictions).
        Some(id) => Ok((
            StatusCode::FOUND,
            [(
                axum::http::header::LOCATION,
                format!("/api/v2/pages/{id}/{carry}"),
            )],
        )
            .into_response()),
        // `not_found` appends " not found", so this must be the noun
        // alone — and it must be the *same* noun the detail endpoint
        // uses, because a gated page and a missing one are deliberately
        // indistinguishable here.
        None => Ok(crate::api::error::ApiError::not_found("page").into_response()),
    }
}

async fn detail_inner(
    tenant: &Tenant,
    viewer: Option<&rustango::tenancy::auth::User>,
    headers: &HeaderMap,
    id: i64,
    allow_draft: bool,
    locale_code: Option<&str>,
    fields: Option<&str>,
) -> Result<Response, rustango::sql::ExecError> {
    let pool = tenant.pool();
    let Some(page) = lookup_page(pool, id).await? else {
        return Ok(crate::api::error::ApiError::not_found("page").into_response());
    };
    // Public means `published` **or** `archived` — the same
    // `PageStatus::is_public` test the renderer and sitemap use. Serving
    // only `published` here made `detail` 404 pages that `tree` and
    // `children` list *and* hand out a `detail_url` for, so every
    // archived node advertised a dead link. Drafts still 404 unless a
    // valid preview token unlocked them (#430).
    let is_public = PageStatus::str_is_public(&page.status);
    if !is_public && !allow_draft {
        return Ok(crate::api::error::ApiError::not_found("page").into_response());
    }
    // Honour view restrictions — the same decision the public renderer
    // makes, reported the way a JSON client can act on.
    //
    // The HTML path answers a denial with `303 → /members/login`, which
    // is right for a browser and useless here: `fetch()` follows it and
    // the caller ends up with a login page under status 200, with no
    // signal that it is unauthenticated. So the API renders the same
    // `Denial` as a status code instead.
    if let Some(denial) =
        crate::view_restriction_guard::decide(tenant, &page, viewer, headers).await
    {
        use crate::view_restriction_guard::Denial;
        return Ok(match denial {
            Denial::NeedsLogin => crate::api::error::ApiError::unauthenticated(),
            Denial::Forbidden => crate::api::error::ApiError::forbidden(),
            Denial::NeedsPassword { .. } => crate::api::error::ApiError::password_required(),
        }
        .into_response());
    }

    // #75 — an alias serves the source's content under its own URL. The
    // gating above ran against the alias row (its own restrictions), and
    // only the *content* is borrowed. Without this, detail returned the
    // stale title copied at creation, no `extension` and no `builder`,
    // while `tree` and the renderer all showed the source's live content.
    let mut alias_owned = None;
    if let Some(source_id) = page.alias_of {
        match lookup_page(pool, source_id).await? {
            Some(src) => alias_owned = Some(crate::page::alias_hybrid(&page, &src)),
            None => tracing::warn!(
                target: "rustango_cms::api",
                alias_id = ?page.id.get().copied(),
                source_id,
                "alias source missing — serving the alias row verbatim",
            ),
        }
    }
    let page = alias_owned.as_ref().unwrap_or(&page);

    let type_name = page_type_names(pool)
        .await?
        .get(&page.page_type_id)
        .cloned()
        .unwrap_or_default();
    // Default locale resolves to `None` — canonical content already is
    // the default-locale content, so the overlay would be a wasted query.
    let locale = crate::translation::resolve_locale(pool, locale_code)
        .await
        .ok()
        .flatten();
    // Only a non-default locale needs an overlay — canonical content
    // already *is* the default-locale content.
    let locale_id = locale
        .as_ref()
        .filter(|l| !l.is_default)
        .and_then(|l| l.id.get().copied());
    let mut obj =
        detail_object_with_type(pool, page, &type_name, None, None, locale_id, viewer).await;

    echo_locale(&mut obj, locale.as_ref());

    // Sparse selection, matching the list endpoints.
    let keep = fields.map(|raw| -> std::collections::HashSet<String> {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    });
    q::apply_fields(&mut obj, keep.as_ref());

    Ok(Json(serde_json::Value::Object(obj)).into_response())
}

/// Return `title` only if some published page with that title is visible
/// to `viewer`.
///
/// Used to vet a search suggestion before echoing it back — otherwise the
/// "did you mean?" hint becomes a way to enumerate the titles of pages the
/// caller is not allowed to see.
async fn visible_title(
    pool: &rustango::sql::Pool,
    viewer: Option<&rustango::tenancy::auth::User>,
    title: &str,
) -> Option<String> {
    let matches: Vec<Page> = Page::objects()
        .where_(Page::title.eq(title.to_owned()))
        .where_(Page::status.eq(PageStatus::Published.as_str().to_owned()))
        .fetch(pool)
        .await
        .ok()?;
    if matches.is_empty() {
        return None;
    }
    let triples: Vec<(i64, String, i64)> = matches
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
    triples
        .iter()
        .any(|(id, _, _)| !denied.contains(id))
        .then(|| title.to_owned())
}

/// The materialized path of `id`, but only if it survived filtering.
///
/// Returning `None` for "gated" and for "absent" alike is deliberate: a
/// filter that behaved differently would answer "does this page exist?"
/// for pages the caller may not see.
fn visible_path(pages: &[Page], id: i64) -> Option<String> {
    pages
        .iter()
        .find(|p| p.id.get().copied() == Some(id))
        .map(|p| p.path.clone())
}

/// Echo the locale a detail body was built in as `meta.locale`. An
/// unknown or inactive code falls back to the tenant default silently, so
/// without this a client cannot tell `?locale=fr` from `?locale=fr-typo`.
/// Every page detail body carries it, so the page URL's negotiated JSON
/// stays equal to `/api/v2/pages/{id}/`.
pub(crate) fn echo_locale(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    locale: Option<&crate::locale::Locale>,
) {
    if let Some(meta) = obj.get_mut("meta").and_then(|m| m.as_object_mut()) {
        meta.insert(
            "locale".to_owned(),
            locale.map_or(serde_json::Value::Null, |l| serde_json::Value::String(l.code.clone())),
        );
    }
}

/// Build the canonical page-detail object.
///
/// Shared by `GET /api/v2/pages/{id}/` and the public `Accept`-negotiated
/// JSON view, so the two are byte-identical by construction rather than
/// by discipline — the whole reason the editor's JSON preview can be
/// trusted to show what a client will actually receive.
///
/// The two `Option` parameters are the preview seam. `None` loads from
/// the database, which is what the saved-row API path wants; `Some(_)`
/// substitutes *virtual* values built from unsaved editor form state.
/// Callers that already hold the registry row pass `type_name` directly
/// to skip the full `cms_page_type` scan `page_type_names` performs.
///
/// `locale_id` localizes the result. `None` (or the default locale)
/// returns canonical content; `Some(id)` overlays that locale's
/// `cms_translation` rows — scalars (`title`, `seo_*`, extension fields),
/// per-leaf StreamField overrides, and page-builder text leaves. Every
/// JSON surface shares this, so localizing here localizes the v2 endpoint
/// and the `Accept`-negotiated page view together.
///
/// Note this deliberately performs **no** access control *on the page
/// itself*: the published check and `view_restriction_guard::enforce`
/// live in the callers, because a draft preview must bypass exactly
/// those gates. `viewer` is used only to filter the `children` listing —
/// see [`child_summaries`].
pub async fn detail_object_with_type(
    pool: &rustango::sql::Pool,
    page: &Page,
    type_name: &str,
    extension_override: Option<serde_json::Value>,
    builder_override: Option<serde_json::Value>,
    locale_id: Option<i64>,
    viewer: Option<&rustango::tenancy::auth::User>,
) -> serde_json::Map<String, serde_json::Value> {
    let id = page.id.get().copied().unwrap_or_default();
    // #75 — per-type extension rows and builder documents live against
    // the *source* for an alias; the alias row has none of its own. Same
    // resolution the renderer applies (`extension_page_id`).
    let content_id = page.alias_of.unwrap_or(id);
    let mut type_names = std::collections::HashMap::new();
    type_names.insert(page.page_type_id, type_name.to_owned());
    let mut obj = serialize_summary(page, &type_names);

    // Extension fields via the registered handler. `load_extension`
    // is opt-in per handler; the default returns `Value::Null`.
    let handler: Option<Box<dyn PageTypeHandler>> = find_handler(type_name);
    if let Some(h) = handler {
        // #442 — expose this page's routable sub-URL patterns so API
        // consumers can resolve/reverse them client-side. Omitted when
        // the page type declares none.
        let routes = h.routes();
        if !routes.is_empty() {
            let arr: Vec<serde_json::Value> = routes
                .iter()
                .map(|r| serde_json::json!({ "name": r.name, "pattern": r.pattern.as_str() }))
                .collect();
            obj.insert("routes".to_owned(), serde_json::Value::Array(arr));
        }
        match extension_override {
            Some(ext) => {
                if !ext.is_null() {
                    obj.insert("extension".to_owned(), ext);
                }
            }
            None => match h.load_extension(pool, content_id).await {
                Ok(ext) if !ext.is_null() => {
                    obj.insert("extension".to_owned(), ext);
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "rustango_cms::api",
                        page_id = id,
                        error = %e,
                        "load_extension failed in api detail",
                    );
                }
            },
        }
    }

    // #572 — page-builder body. When the page type has a published schema
    // (code type with a UI body, or a UI-created type), expose the filled,
    // lazily-upgraded values so headless clients render the UI-defined body.
    // Values only — clients render; no server-side zone HTML here.
    //
    // The compiled schema is kept so the localization pass below can reach
    // the builder's text leaves — they are keyed `builder.<path>`, which
    // the flat scalar overlay cannot address.
    let mut compiled_schema = None;
    match builder_override {
        Some(values) => {
            obj.insert("builder".to_owned(), values);
            if locale_id.is_some() {
                compiled_schema = crate::page_builder::values::values_for(
                    pool,
                    content_id,
                    page.page_type_id,
                    None,
                )
                .await
                .map(|(_, c)| c);
            }
        }
        None => {
            if let Some((values, compiled)) =
                crate::page_builder::values::values_for(pool, content_id, page.page_type_id, None)
                    .await
            {
                obj.insert("builder".to_owned(), values);
                compiled_schema = Some(compiled);
            }
        }
    }

    if let Some(lid) = locale_id {
        localize_detail_object(pool, id, type_name, lid, &mut obj, compiled_schema.as_ref()).await;
    }

    // Direct children, so a client can walk the site from any page without
    // first fetching the whole tree. One level only — `/api/v2/pages/tree/`
    // is the endpoint for depth.
    obj.insert(
        "children".to_owned(),
        serde_json::Value::Array(child_summaries(pool, page, viewer, locale_id).await),
    );

    obj
}

/// Most children a detail response will inline.
///
/// `tree` caps itself at `MAX_NODES` and says so via `meta.truncated`;
/// `children` had no equivalent, so a flat blog's homepage returned every
/// one of its posts on every request — including on each
/// `Accept: application/json` render of `/`. Clients that need the whole
/// set have `/api/v2/pages/?child_of=`, which is paged.
pub const MAX_CHILDREN: usize = 100;

/// Summaries of a page's **direct** children — `id, title, slug, url,
/// has_children`.
///
/// One query covers both levels: children *and* grandchildren, sliced by
/// `depth`, so `has_children` costs nothing extra. It filters on `path`
/// rather than `parent_id` because `cms_page` indexes `path` and does not
/// index `parent_id` (`migrations/0001_cms_initial.json`).
///
/// `viewer` gates the listing: without it a page whose child subtree is
/// member-only would publish those titles and URLs to anonymous callers,
/// which is exactly what the restriction is for. This is the *only* thing
/// `detail_object_with_type` uses the viewer for — the page itself is
/// gated by its callers.
async fn child_summaries(
    pool: &rustango::sql::Pool,
    page: &Page,
    viewer: Option<&rustango::tenancy::auth::User>,
    locale_id: Option<i64>,
) -> Vec<serde_json::Value> {
    // An unsaved preview row has no id and therefore no children yet.
    let Some(&parent_id) = page.id.get() else {
        return Vec::new();
    };
    if page.path.is_empty() {
        return Vec::new();
    }

    // Match the public renderer, not `list`: `archived` still resolves to
    // a real URL (`PageStatus::is_public`), so omitting it would advertise
    // fewer children than the site actually serves.
    let statuses = [
        PageStatus::Published.as_str().to_owned(),
        PageStatus::Archived.as_str().to_owned(),
    ];
    let mut rows: Vec<Page> = match Page::objects()
        .where_(Page::path.like(crate::tree::descendants_like(&page.path)))
        .where_(Page::depth.is_in([page.depth + 1, page.depth + 2]))
        .where_(Page::status.is_in(statuses))
        .order_by(&[("path", false)])
        .fetch(pool)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                target: "rustango_cms::api",
                page_id = parent_id,
                error = %e,
                "child summaries lookup failed",
            );
            return Vec::new();
        }
    };

    // Error pages are routing furniture, not site structure — excluded
    // here exactly as `tree` and `auto_menu` exclude them. Without this a
    // 404 handler parented under Home showed up as one of its children.
    let error_tid = crate::error_pages::error_page_type_id(pool).await;
    rows.retain(|p| error_tid.is_none_or(|tid| p.page_type_id != tid));

    // #members — drop what the viewer may not see, before anything is
    // counted, so `has_children` never advertises a gated subtree either.
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

    let visible: Vec<&Page> = rows
        .iter()
        .filter(|p| !denied.contains(&p.id.get().copied().unwrap_or_default()))
        // Legacy per-locale variant trees are not children of this page in
        // any sense a client cares about (#275) — `feed` and the tree
        // endpoint drop them too.
        .filter(|p| p.locale_variant_of.is_none())
        .collect();

    let mut grandchildren: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for p in visible.iter().filter(|p| p.depth == page.depth + 2) {
        if let Some(pid) = p.parent_id {
            grandchildren.insert(pid);
        }
    }

    let mut children: Vec<&&Page> = visible
        .iter()
        .filter(|p| p.depth == page.depth + 1)
        .collect();
    // Same ordering the rendered site uses (`sort_order, id`), not the
    // `path` order the query returned — see the note in `api::tree`.
    children.sort_by_key(|p| (p.sort_order, p.id.get().copied().unwrap_or_default()));
    if children.is_empty() {
        return Vec::new();
    }

    // Localized titles for the whole set in one query, never per child.
    let translations = match locale_id {
        Some(lid) => {
            let ids: Vec<i64> = children.iter().filter_map(|p| p.id.get().copied()).collect();
            crate::translation::fetch_for_pages(pool, &ids, lid)
                .await
                .unwrap_or_default()
        }
        None => std::collections::HashMap::new(),
    };

    children
        .into_iter()
        .take(MAX_CHILDREN)
        .map(|p| {
            let id = p.id.get().copied().unwrap_or_default();
            let title = translations
                .get(&id)
                .and_then(|m| m.get("title"))
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| p.title.clone());
            serde_json::json!({
                "id": id,
                "title": title,
                "slug": p.slug,
                "url": if p.url_path.is_empty() { "/" } else { p.url_path.as_str() },
                "has_children": grandchildren.contains(&id),
                "detail_url": format!("/api/v2/pages/{id}/"),
            })
        })
        .collect()
}

/// Overlay one locale's `cms_translation` rows onto a built detail object.
///
/// Three storage shapes, three mechanisms — a single flat overlay cannot
/// cover them, which is why the HTML renderer needs three calls too:
///
/// * **scalars** (`title`, `seo_*`, extension columns, and the legacy
///   whole-body stream blob) — [`crate::translation::overlay_translations`],
///   which replaces only keys the object already carries and recurses into
///   `extension`, matching on its `page_id`.
/// * **StreamField leaves** keyed `<field>.<uuid>.<name>` —
///   [`crate::block::translate::apply_translatable_overrides`], per stream
///   field, since dotted paths address block contents the flat overlay skips.
/// * **page-builder leaves** keyed `builder.<path>` —
///   [`crate::page_builder::values::apply_builder_translations`].
///
/// One `fetch_for_pages` query feeds all three. Missing or empty overrides
/// leave canonical content in place, so a partially-translated page degrades
/// per field rather than blanking.
async fn localize_detail_object(
    pool: &rustango::sql::Pool,
    page_id: i64,
    type_name: &str,
    locale_id: i64,
    obj: &mut serde_json::Map<String, serde_json::Value>,
    compiled: Option<&crate::page_builder::compile::CompiledSchema>,
) {
    let by_page = crate::translation::fetch_for_pages(pool, &[page_id], locale_id)
        .await
        .unwrap_or_default();
    let Some(here) = by_page.get(&page_id) else {
        return; // nothing translated for this page in this locale
    };

    let mut value = serde_json::Value::Object(std::mem::take(obj));
    crate::translation::overlay_translations(&mut value, &by_page);

    // Per-leaf stream overrides. Which extension fields are streams comes
    // from the handler's declared widgets rather than sniffing whether a
    // string happens to parse as a JSON array — that heuristic corrupts a
    // prose field whose text is something like "[1,2]".
    if let Some(h) = find_handler(type_name) {
        let stream_fields: Vec<String> = h
            .widgets(pool, page_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|w| w.kind == crate::widget::WidgetKind::Stream)
            .map(|w| w.name)
            .collect();
        if let Some(ext) = value.get_mut("extension") {
            for name in stream_fields {
                if let Some(raw) = ext.get(&name).cloned() {
                    let localized =
                        crate::block::translate::apply_translatable_overrides(&name, &raw, here);
                    if let Some(m) = ext.as_object_mut() {
                        m.insert(name, localized);
                    }
                }
            }
        }
    }

    if let (Some(compiled), Some(builder)) = (compiled, value.get_mut("builder")) {
        if let Some(map) = builder.as_object_mut() {
            crate::page_builder::values::apply_builder_translations(compiled, map, here);
        }
    }

    if let serde_json::Value::Object(m) = value {
        *obj = m;
    }
}

// ---------------------------------------------------------------
// helpers
// ---------------------------------------------------------------

async fn lookup_page(
    pool: &rustango::sql::Pool,
    id: i64,
) -> Result<Option<Page>, rustango::sql::ExecError> {
    let mut rows: Vec<Page> = Page::objects().where_(Page::id.eq(id)).fetch(pool).await?;
    Ok(rows.pop())
}

async fn lookup_page_type(
    pool: &rustango::sql::Pool,
    type_name: &str,
) -> Result<Option<PageType>, rustango::sql::ExecError> {
    let mut rows: Vec<PageType> = PageType::objects()
        .where_(PageType::type_name.eq(type_name.to_owned()))
        .fetch(pool)
        .await?;
    Ok(rows.pop())
}

/// Build a `{ page_type_id → type_name }` lookup so each row can
/// emit its symbolic type in the response without an N+1.
pub(crate) async fn page_type_names(
    pool: &rustango::sql::Pool,
) -> Result<std::collections::HashMap<i64, String>, rustango::sql::ExecError> {
    let rows: Vec<PageType> = PageType::objects().fetch(pool).await?;
    Ok(rows
        .into_iter()
        .filter_map(|pt| pt.id.get().copied().map(|id| (id, pt.type_name)))
        .collect())
}

fn serialize_summary(
    p: &Page,
    type_names: &std::collections::HashMap<i64, String>,
) -> serde_json::Map<String, serde_json::Value> {
    let id = p.id.get().copied().unwrap_or_default();
    let type_name = type_names.get(&p.page_type_id).cloned().unwrap_or_default();
    let mut obj = serde_json::Map::new();
    obj.insert("id".to_owned(), serde_json::json!(id));
    obj.insert(
        "meta".to_owned(),
        serde_json::json!({
            "type": type_name,
            "detail_url": format!("/api/v2/pages/{id}/"),
            "html_url": p.url_path,
            "slug": p.slug,
            "depth": p.depth,
            "parent_id": p.parent_id,
            "alias_of": p.alias_of,
            "first_published_at": p.published_at,
            "updated_at": p.updated_at.get().copied(),
        }),
    );
    obj.insert("title".to_owned(), serde_json::json!(p.title));
    // The same `url` key tree nodes and child summaries use. It is also
    // at `meta.html_url` for compatibility, but a client should not need
    // a different accessor per endpoint to read one field — and unlike
    // `html_url`, this normalizes the root's empty `url_path` to "/",
    // as the other two shapes already did.
    obj.insert(
        "url".to_owned(),
        serde_json::json!(if p.url_path.is_empty() {
            "/"
        } else {
            p.url_path.as_str()
        }),
    );
    obj.insert("seo_title".to_owned(), serde_json::json!(p.seo_title));
    obj.insert(
        "seo_description".to_owned(),
        serde_json::json!(p.seo_description),
    );
    obj.insert("robots_index".to_owned(), serde_json::json!(p.robots_index));
    obj.insert(
        "sitemap_priority".to_owned(),
        serde_json::json!(p.sitemap_priority),
    );
    obj.insert(
        "show_in_menus".to_owned(),
        serde_json::json!(p.show_in_menus),
    );
    obj
}

/// Build the set of ancestor paths for `path` (excluding `path` itself).
/// Mirrors [`crate::tree::MaterializedPath::ancestors`] without needing a
/// `MaterializedPath` value.
fn ancestor_paths_of(path: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let mut acc = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        acc.push_str(seg);
        acc.push('/');
        if acc != path {
            out.insert(acc.clone());
        }
    }
    out
}

/// Fields `?order=` accepts on this endpoint. Anything else is a `400`
/// — kept next to `compare_pages`, which must handle exactly these.
const PAGE_ORDER_FIELDS: [&str; 8] = [
    "title",
    "slug",
    "url_path",
    "depth",
    "sort_order",
    "published_at",
    "updated_at",
    "created_at",
];

fn compare_pages(a: &Page, b: &Page, specs: &[q::OrderSpec]) -> std::cmp::Ordering {
    for spec in specs {
        let ord = match spec.field.as_str() {
            "title" => a.title.cmp(&b.title),
            "slug" => a.slug.cmp(&b.slug),
            "url_path" => a.url_path.cmp(&b.url_path),
            "depth" => a.depth.cmp(&b.depth),
            "sort_order" => a.sort_order.cmp(&b.sort_order),
            "published_at" => a.published_at.cmp(&b.published_at),
            "updated_at" => a.updated_at.get().cmp(&b.updated_at.get()),
            "created_at" => a.created_at.get().cmp(&b.created_at.get()),
            _ => continue,
        };
        if ord != std::cmp::Ordering::Equal {
            return if spec.descending { ord.reverse() } else { ord };
        }
    }
    // Same reason as the default ordering: a unique last resort, so two
    // rows tied on every requested key still page deterministically.
    a.id.get().cmp(&b.id.get())
}

#[cfg(test)]
mod tests {
    use super::carried_query;

    #[test]
    fn the_resolved_path_is_not_carried_onto_the_detail_url() {
        // `html_path` is what `find/` consumed; repeating it on the
        // detail URL would be noise at best.
        assert_eq!(carried_query(Some("html_path=%2Fabout")), "");
        assert_eq!(carried_query(None), "");
        assert_eq!(carried_query(Some("")), "");
    }

    #[test]
    fn everything_else_survives_the_redirect() {
        // The bug this exists for: a SPA resolving its route with
        // `?locale=fr` followed the 302 and got English back.
        assert_eq!(carried_query(Some("html_path=%2F&locale=fr")), "?locale=fr");
        assert_eq!(
            carried_query(Some("locale=fr&html_path=%2F&fields=title")),
            "?locale=fr&fields=title",
        );
    }

    #[test]
    fn a_parameter_this_endpoint_never_heard_of_is_still_forwarded() {
        // Hard-coding an allowlist here would silently drop whatever the
        // detail endpoint gains next; it ignores names it doesn't know.
        assert_eq!(carried_query(Some("html_path=%2F&whatever=1")), "?whatever=1");
    }

    #[test]
    fn values_are_forwarded_verbatim_not_re_encoded() {
        // Re-encoding risks double-escaping a token that already is.
        assert_eq!(
            carried_query(Some("html_path=%2Fx&preview_token=7.123.ab%2Fcd")),
            "?preview_token=7.123.ab%2Fcd",
        );
    }
}
