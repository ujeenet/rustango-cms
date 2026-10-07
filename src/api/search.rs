//! `GET /api/v2/search/?q=` — one search box, every content type.
//!
//! Each list endpoint has its own `?search=`, so a site-wide search used
//! to be four parallel requests the client merged and ranked itself —
//! with nothing in the responses to rank *by*, since none of them carries
//! a score. That is a lot of work to hand a frontend for the most
//! ordinary feature a site has.
//!
//! This runs the same per-type searches server-side and returns one
//! ordered list of typed hits. Pages go through the real page search
//! (`crate::search`), so a configured Postgres FTS or Elasticsearch
//! backend applies — and so do editorial search promotions, which were
//! previously invisible to any API client.
//!
//! Results are **viewer-aware**: gated pages and assets in closed
//! collections are dropped before ranking, so search cannot be used to
//! enumerate what the caller may not read.

/// Fold `s` for a case-insensitive substring match — Unicode-aware, so
/// `Новини` matches `новини` and `ÜBER` matches `über`. The one fold every
/// API search fallback uses, matching the admin explorer.
pub(crate) fn fold(s: &str) -> String {
    s.to_lowercase()
}

/// Whether `haystack` contains `folded_needle`, a needle already passed
/// through [`fold`].
pub(crate) fn contains_folded(haystack: &str, folded_needle: &str) -> bool {
    fold(haystack).contains(folded_needle)
}

// Our extractor, not axum's — see `query::Query`.
use crate::api::query::Query;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango::core::Column as _;
use rustango::extractors::{SessionUser, Tenant};
use rustango::sql::FetcherPool as _;
use serde::Deserialize;

use super::query as q;
use crate::media::Media;
use crate::page::{Page, PageStatus};
use crate::snippet::Snippet;

/// Content types the unified search covers.
const ALL_TYPES: [&str; 4] = ["page", "image", "document", "snippet"];

#[derive(Debug, Default, Deserialize)]
pub struct SearchQuery {
    /// The needle. Absent or empty is a 400 — an empty search is a
    /// client bug, and answering it with "everything" is the worst
    /// possible guess.
    #[serde(default, deserialize_with = "q::empty_string_as_none")]
    pub q: Option<String>,
    /// `?type=page,snippet` — restrict to some of the four. Unknown
    /// names are a 400 rather than a silent empty result, since a typo
    /// would otherwise look exactly like "no matches".
    #[serde(default, deserialize_with = "q::empty_string_as_none")]
    pub r#type: Option<String>,
    #[serde(default, deserialize_with = "q::empty_as_none")]
    pub limit: Option<usize>,
    #[serde(default, deserialize_with = "q::empty_as_none")]
    pub offset: Option<usize>,
}

/// `GET /api/v2/search/`
pub async fn search(
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    rustango::tenancy::member_auth::CurrentMember(member): rustango::tenancy::member_auth::CurrentMember,
    crate::api::auth::BearerMember(bearer): crate::api::auth::BearerMember,
    Query(qs): Query<SearchQuery>,
) -> Response {
    let viewer = member.as_ref().or(bearer.as_ref()).or(session_user.as_ref());
    match search_inner(&tenant, viewer, &qs).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, "search failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

pub async fn search_inner(
    tenant: &Tenant,
    viewer: Option<&rustango::tenancy::auth::User>,
    qs: &SearchQuery,
) -> Result<Response, rustango::sql::ExecError> {
    let pool = tenant.pool();
    let Some(needle) = qs.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(
            crate::api::error::ApiError::bad_request("missing `q` query parameter")
                .into_response(),
        );
    };

    let wanted = match requested_types(qs.r#type.as_deref()) {
        Ok(t) => t,
        Err(bad) => {
            return Ok(crate::api::error::ApiError::bad_request(format!(
                "unknown `type` value `{bad}` — expected any of: {}",
                ALL_TYPES.join(", ")
            ))
            .into_response())
        }
    };

    let mut hits: Vec<serde_json::Value> = Vec::new();
    if wanted.contains(&"page") {
        hits.extend(page_hits(pool, viewer, needle).await?);
    }
    if wanted.contains(&"image") || wanted.contains(&"document") {
        hits.extend(media_hits(pool, viewer, needle, &wanted).await?);
    }
    if wanted.contains(&"snippet") {
        hits.extend(snippet_hits(pool, needle).await?);
    }

    // Rank across types by a shared, explainable score, then by title so
    // ties are stable and a client paging through sees each row once.
    hits.sort_by(|a, b| {
        let sa = a["score"].as_f64().unwrap_or(0.0);
        let sb = b["score"].as_f64().unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a["title"].as_str().cmp(&b["title"].as_str()))
            .then_with(|| a["id"].as_i64().cmp(&b["id"].as_i64()))
    });

    let total_count = hits.len();
    let limit = q::clamp_limit(qs.limit);
    let offset = qs.offset.unwrap_or(0);
    crate::search_promotion::log_public_query(pool, needle, offset);
    let items: Vec<serde_json::Value> = hits.into_iter().skip(offset).take(limit).collect();

    let mut meta = serde_json::to_value(q::ListMeta::paged(total_count, limit, offset))
        .unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = meta.as_object_mut() {
        obj.insert("q".to_owned(), serde_json::json!(needle));
        obj.insert("types".to_owned(), serde_json::json!(wanted));
    }
    Ok(Json(serde_json::json!({ "meta": meta, "items": items })).into_response())
}

/// Parse `?type=`, defaulting to everything.
fn requested_types(raw: Option<&str>) -> Result<Vec<&'static str>, String> {
    let Some(raw) = raw else {
        return Ok(ALL_TYPES.to_vec());
    };
    let mut out = Vec::new();
    for name in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match ALL_TYPES.iter().find(|t| **t == name) {
            Some(t) => {
                if !out.contains(t) {
                    out.push(*t);
                }
            }
            None => return Err(name.to_owned()),
        }
    }
    if out.is_empty() {
        return Ok(ALL_TYPES.to_vec());
    }
    Ok(out)
}

/// How well `haystack` answers `needle`, in `0.0..=1.0`.
///
/// Deliberately simple and the same for every type, because the point is
/// that hits are *comparable* across types — a per-type relevance number
/// nobody can reconcile is what the client already had. An exact title
/// beats a prefix beats a substring; matching a secondary field (a slug,
/// a filename, body text) scores below any title match.
#[must_use]
pub fn score(needle: &str, title: &str, secondary: &[&str]) -> f64 {
    let n = fold(needle);
    let t = fold(title);
    if t == n {
        return 1.0;
    }
    if t.starts_with(&n) {
        return 0.8;
    }
    if t.contains(&n) {
        return 0.6;
    }
    if secondary.iter().any(|s| contains_folded(s, &n)) {
        return 0.3;
    }
    0.0
}

fn hit(
    kind: &str,
    id: i64,
    title: &str,
    url: Option<String>,
    detail_url: String,
    score: f64,
) -> serde_json::Value {
    serde_json::json!({
        "type": kind,
        "id": id,
        "title": title,
        "url": url,
        "detail_url": detail_url,
        "score": score,
    })
}

async fn page_hits(
    pool: &rustango::sql::Pool,
    viewer: Option<&rustango::tenancy::auth::User>,
    needle: &str,
) -> Result<Vec<serde_json::Value>, rustango::sql::ExecError> {
    let mut pages: Vec<Page> = Page::objects()
        .where_(Page::status.is_in([
            PageStatus::Published.as_str().to_owned(),
            PageStatus::Archived.as_str().to_owned(),
        ]))
        .order_by(&[("id", false)])
        .fetch(pool)
        .await?;
    pages.retain(|p| p.locale_variant_of.is_none());

    // #members — drop gated pages before ranking, so search cannot be
    // used to enumerate a member-only section by guessing words.
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

    // Prefer the configured backend (PG FTS / Elasticsearch) when it has
    // an opinion, so a tenant's real search config applies here too.
    let ranked = crate::search::search_page_ids(pool, needle, 1000).await;
    let rank_of: std::collections::HashMap<i64, usize> = ranked
        .as_ref()
        .map(|ids| ids.iter().enumerate().map(|(i, id)| (*id, i)).collect())
        .unwrap_or_default();

    Ok(pages
        .into_iter()
        .filter(|p| !denied.contains(&p.id.get().copied().unwrap_or_default()))
        .filter_map(|p| {
            let id = p.id.get().copied().unwrap_or_default();
            // A backend hit outranks a substring match, and its position
            // in the hit list breaks ties within that band.
            let s = match rank_of.get(&id) {
                Some(pos) => 0.9 - (*pos as f64 / 10_000.0).min(0.25),
                None => score(needle, &p.title, &[&p.slug, &p.url_path, &p.seo_description]),
            };
            (s > 0.0).then(|| {
                hit(
                    "page",
                    id,
                    &p.title,
                    Some(if p.url_path.is_empty() {
                        "/".to_owned()
                    } else {
                        p.url_path.clone()
                    }),
                    format!("/api/v2/pages/{id}/"),
                    s,
                )
            })
        })
        .collect())
}

async fn media_hits(
    pool: &rustango::sql::Pool,
    viewer: Option<&rustango::tenancy::auth::User>,
    needle: &str,
    wanted: &[&str],
) -> Result<Vec<serde_json::Value>, rustango::sql::ExecError> {
    let rows: Vec<Media> = Media::objects().fetch(pool).await?;

    // #members — the same collection gating the media listings apply.
    let collection_ids: Vec<i64> = rows.iter().filter_map(|m| m.collection_id).collect();
    let denied =
        crate::collection_view_restriction::denied_collection_ids(pool, viewer, &collection_ids)
            .await;

    Ok(rows
        .into_iter()
        .filter(|m| {
            m.collection_id
                .is_none_or(|c| !denied.contains(&c))
        })
        .filter_map(|m| {
            let kind = if m.kind == "image" { "image" } else { "document" };
            if !wanted.contains(&kind) {
                return None;
            }
            let id = m.id.get().copied().unwrap_or_default();
            let s = score(needle, &m.title, &[&m.filename, &m.alt_text]);
            (s > 0.0).then(|| {
                hit(
                    kind,
                    id,
                    &m.title,
                    None,
                    format!("/api/v2/{kind}s/{id}/"),
                    s,
                )
            })
        })
        .collect())
}

async fn snippet_hits(
    pool: &rustango::sql::Pool,
    needle: &str,
) -> Result<Vec<serde_json::Value>, rustango::sql::ExecError> {
    let rows: Vec<Snippet> = Snippet::objects().fetch(pool).await?;
    Ok(rows
        .into_iter()
        .filter_map(|sn| {
            let id = sn.id.get().copied().unwrap_or_default();
            let s = score(needle, &sn.title, &[&sn.slug, &sn.type_name, &sn.body_markdown]);
            (s > 0.0).then(|| {
                hit(
                    "snippet",
                    id,
                    &sn.title,
                    None,
                    format!("/api/v2/snippets/{id}/"),
                    s,
                )
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_title_outranks_a_prefix_outranks_a_substring() {
        assert!(score("docs", "Docs", &[]) > score("docs", "Docs and more", &[]));
        assert!(score("docs", "Docs and more", &[]) > score("docs", "The Docs page", &[]));
        assert!(score("docs", "The Docs page", &[]) > score("docs", "Unrelated", &["docs.pdf"]));
    }

    #[test]
    fn a_secondary_field_still_scores() {
        assert!(score("report", "Q4", &["annual-report.pdf"]) > 0.0);
    }

    #[test]
    fn no_match_scores_zero() {
        assert_eq!(score("zebra", "Docs", &["a.pdf", "body text"]), 0.0);
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(score("DOCS", "docs", &[]), 1.0);
        assert!(score("dOcS", "The Docs", &[]) > 0.0);
    }

    #[test]
    fn no_type_filter_means_every_type() {
        assert_eq!(requested_types(None).unwrap(), ALL_TYPES.to_vec());
        // An all-whitespace list is the same as absent, not "nothing".
        assert_eq!(requested_types(Some(" , ")).unwrap(), ALL_TYPES.to_vec());
    }

    #[test]
    fn a_type_filter_is_parsed_and_deduped() {
        assert_eq!(
            requested_types(Some("page, snippet ,page")).unwrap(),
            vec!["page", "snippet"],
        );
    }

    #[test]
    fn an_unknown_type_is_an_error_not_an_empty_result() {
        // A typo must not look identical to "no matches".
        assert_eq!(requested_types(Some("pages")).unwrap_err(), "pages");
        assert_eq!(requested_types(Some("page,widget")).unwrap_err(), "widget");
    }
}

#[cfg(test)]
mod fold_tests {
    use super::{contains_folded, fold};

    #[test]
    fn non_latin_and_accented_text_folds() {
        assert!(contains_folded("Новини", &fold("новини")));
        assert!(contains_folded("über uns", &fold("ÜBER")));
        assert!(contains_folded("Ελληνικά", &fold("ΕΛΛΗΝΙΚΆ")));
        assert!(!contains_folded("News", &fold("новини")));
    }
}
