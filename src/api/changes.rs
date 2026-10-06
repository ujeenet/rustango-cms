//! `GET /api/v2/changes/` — has anything moved?
//!
//! `?updated_since=` lets a client fetch only what changed *in one
//! collection*, but it still has to ask every collection to find out
//! whether any of them did. A dynamic frontend polling pages, images,
//! documents, snippets and menus pays five full list responses per tick
//! to discover, almost always, that nothing happened.
//!
//! This is the cheap tick: one small response saying, per collection,
//! how many rows the caller can see and when one of them last changed.
//! A client keeps the previous answer and re-fetches only the
//! collections whose entry moved — with `?updated_since=` set to the
//! `latest` it already held.
//!
//! ## Why `count` and not just `latest`
//!
//! A deletion moves no timestamp. Without the count, a client that had
//! cached a page would keep serving it forever after an editor removed
//! it, because `latest` never advanced. The pair — "how many, and how
//! recent" — catches additions, edits and removals between them.
//!
//! It is not a push feed and does not pretend to be: there is no SSE or
//! WebSocket here, and a client still decides its own cadence. What it
//! removes is the cost of asking.
//!
//! ## Cost
//!
//! **Viewer-aware, and therefore not free.** The counts must reflect what
//! *this* caller may see, or the endpoint would report that a member-only
//! page exists to someone who cannot read it — so it runs the same
//! visibility filters the list endpoints do. Roughly the cost of one list
//! request, replacing five.

use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use rustango::core::Column as _;
use rustango::extractors::{SessionUser, Tenant};
use rustango::sql::{CounterPool as _, FetcherPool as _};

use crate::media::Media;
use crate::navigation::{Menu, MenuItem};
use crate::page::{Page, PageStatus};
use crate::snippet::Snippet;

/// `GET /api/v2/changes/`
pub async fn changes(
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    rustango::tenancy::member_auth::CurrentMember(member): rustango::tenancy::member_auth::CurrentMember,
    crate::api::auth::BearerMember(bearer): crate::api::auth::BearerMember,
) -> Response {
    let viewer = member.as_ref().or(bearer.as_ref()).or(session_user.as_ref());
    match changes_inner(tenant.pool(), viewer).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, "changes failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

/// One collection's fingerprint.
fn entry(count: usize, latest: Option<DateTime<Utc>>) -> serde_json::Value {
    serde_json::json!({
        "count": count,
        "latest": latest.map(|t| t.to_rfc3339()),
    })
}

/// The most recent of a set of optional timestamps.
fn newest(times: impl Iterator<Item = Option<DateTime<Utc>>>) -> Option<DateTime<Utc>> {
    times.flatten().max()
}

fn to_usize(n: i64) -> usize {
    usize::try_from(n).unwrap_or(0)
}

/// `(count, newest upload)` of the images (`images = true`) or other
/// media the viewer may see: those outside the `denied` collections.
async fn media_fingerprint(
    pool: &rustango::sql::Pool,
    denied: &[i64],
    images: bool,
) -> Result<(usize, Option<DateTime<Utc>>), rustango::sql::ExecError> {
    let listed = || {
        let qs = Media::objects();
        let qs = if images {
            qs.where_(Media::kind.eq("image".to_owned()))
        } else {
            qs.where_(Media::kind.ne("image".to_owned()))
        };
        if denied.is_empty() {
            qs
        } else {
            qs.where_(
                Media::collection_id
                    .is_null()
                    .or(Media::collection_id.not_in(denied.iter().copied())),
            )
        }
    };
    let count = listed().count(pool).await?;
    let latest = listed()
        .order_by(&[("uploaded_at", true)])
        .first(pool)
        .await?
        .and_then(|m| m.uploaded_at.get().copied());
    Ok((to_usize(count), latest))
}

pub async fn changes_inner(
    pool: &rustango::sql::Pool,
    viewer: Option<&rustango::tenancy::auth::User>,
) -> Result<Response, rustango::sql::ExecError> {
    // ---- pages: the same public set the list endpoint serves ----------
    let mut pages: Vec<Page> = Page::objects()
        .where_(Page::status.is_in([
            PageStatus::Published.as_str().to_owned(),
            PageStatus::Archived.as_str().to_owned(),
        ]))
        .fetch(pool)
        .await?;
    pages.retain(|p| p.locale_variant_of.is_none());
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
    pages.retain(|p| !denied.contains(&p.id.get().copied().unwrap_or_default()));

    // ---- media: gated collections excluded, as in the listings --------
    // Counted in SQL (#647): every row of five tables used to be loaded
    // just to count it, on an endpoint built to be polled anonymously.
    // The collections table is small; the media table need not be read.
    let collection_ids: Vec<i64> = crate::media::MediaCollection::objects()
        .fetch(pool)
        .await?
        .iter()
        .filter_map(|c| c.id.get().copied())
        .collect();
    let denied: Vec<i64> =
        crate::collection_view_restriction::denied_collection_ids(pool, viewer, &collection_ids)
            .await
            .into_iter()
            .collect();
    let (images, documents) = (
        media_fingerprint(pool, &denied, true).await?,
        media_fingerprint(pool, &denied, false).await?,
    );

    let snippets = (
        Snippet::objects().count(pool).await?,
        Snippet::objects()
            .order_by(&[("updated_at", true)])
            .first(pool)
            .await?
            .and_then(|s| s.updated_at.get().copied()),
    );

    // Menus carry no `updated_at`, so their items' absence of one leaves
    // `latest` null — the count is what moves when an editor adds or
    // removes a menu or an item, which is the signal a navbar needs.
    let menus = Menu::objects().count(pool).await? + MenuItem::objects().count(pool).await?;

    Ok(Json(serde_json::json!({
        "meta": {
            "checked_at": Utc::now().to_rfc3339(),
            // Spelled out so a client does not have to infer the contract
            // from behaviour.
            "usage": "Re-fetch a collection when its `count` or `latest` \
                      differs from the values you hold, passing the \
                      `latest` you held as `?updated_since=`.",
        },
        "collections": {
            "pages": entry(
                pages.len(),
                newest(pages.iter().map(|p| p.updated_at.get().copied())),
            ),
            "images": entry(images.0, images.1),
            "documents": entry(documents.0, documents.1),
            "snippets": entry(to_usize(snippets.0), snippets.1),
            "menus": entry(to_usize(menus), None),
        },
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Option<DateTime<Utc>> {
        Some(DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc))
    }

    #[test]
    fn newest_picks_the_latest_and_ignores_missing() {
        let got = newest(
            [t("2026-01-01T00:00:00Z"), None, t("2026-06-01T00:00:00Z")].into_iter(),
        );
        assert_eq!(got, t("2026-06-01T00:00:00Z"));
    }

    #[test]
    fn newest_of_nothing_is_none() {
        assert_eq!(newest(std::iter::empty()), None);
        assert_eq!(newest([None, None].into_iter()), None);
    }

    #[test]
    fn an_entry_carries_both_halves_of_the_signal() {
        // `count` is what catches a deletion, which moves no timestamp.
        let e = entry(3, t("2026-01-01T00:00:00Z"));
        assert_eq!(e["count"], 3);
        assert!(e["latest"].is_string());

        let empty = entry(0, None);
        assert_eq!(empty["count"], 0);
        assert!(empty["latest"].is_null());
    }
}
