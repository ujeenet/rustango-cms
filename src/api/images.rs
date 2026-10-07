//! `GET /api/v2/images/` + `GET /api/v2/images/{id}/`.
//!
//! Read-only image metadata for headless consumers. Bytes still
//! flow through [`crate::rendition_route`] — the API exposes
//! `download_url_template` so clients can build the URL for any
//! filter spec.

// Our extractors, not axum's: they refuse a malformed value in the same
// JSON envelope as everything else. See `query::Query`.
use crate::api::query::{Path, Query};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango::core::Column as _;
use rustango::extractors::{SessionUser, Tenant};
use rustango::sql::FetcherPool as _;
use serde::Deserialize;

use super::query as q;
use crate::media::Media;

#[derive(Debug, Default, Deserialize)]
pub struct ImageListQuery {
    #[serde(flatten)]
    pub list: q::ListQuery,
    /// Optional collection scope: `?collection=N`.
    #[serde(default, deserialize_with = "crate::api::query::empty_as_none")]
    pub collection: Option<i64>,
}

/// `GET /api/v2/images/`
pub async fn list(
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    rustango::tenancy::member_auth::CurrentMember(member): rustango::tenancy::member_auth::CurrentMember,
    crate::api::auth::BearerMember(bearer): crate::api::auth::BearerMember,
    Query(qs): Query<ImageListQuery>,
) -> Response {
    let viewer = member.as_ref().or(bearer.as_ref()).or(session_user.as_ref());
    match list_inner(&tenant, viewer, &qs).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, "images list failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

async fn list_inner(
    tenant: &Tenant,
    viewer: Option<&rustango::tenancy::auth::User>,
    qs: &ImageListQuery,
) -> Result<Response, rustango::sql::ExecError> {
    let pool = tenant.pool();
    let (limit, offset) = q::paginate(&qs.list);
    let fields = q::field_set(&qs.list);
    let orderings = match q::parse_order_in(&qs.list, &MEDIA_ORDER_FIELDS) {
        Ok(v) => v,
        Err(msg) => return Ok(crate::api::error::ApiError::bad_request(msg).into_response()),
    };

    let mut rows: Vec<Media> = Media::objects()
        .where_(Media::kind.eq("image".to_owned()))
        .fetch(pool)
        .await?;

    // #members — a collection can be login/group/permission gated, and
    // `/__media__/` refuses to serve bytes from a gated one. This listing
    // did no such check, so the catalogue (titles, filenames, dimensions,
    // alt text) leaked even though the files themselves were protected.
    let collection_ids: Vec<i64> = rows.iter().filter_map(|m| m.collection_id).collect();
    let denied_collections = crate::collection_view_restriction::denied_collection_ids(
        tenant.pool(),
        viewer,
        &collection_ids,
    )
    .await;
    if !denied_collections.is_empty() {
        rows.retain(|m| {
            m.collection_id
                .is_none_or(|c| !denied_collections.contains(&c))
        });
    }

    if let Some(col_id) = qs.collection {
        rows.retain(|m| m.collection_id == Some(col_id));
    }

    if let Some(needle) = qs.list.search.as_deref().filter(|s| !s.trim().is_empty()) {
        let n = crate::api::search::fold(needle);
        rows.retain(|m| {
            crate::api::search::contains_folded(&m.title, &n)
                || crate::api::search::contains_folded(&m.filename, &n)
                || crate::api::search::contains_folded(&m.alt_text, &n)
        });
    }

    if orderings.iter().any(|o| o.random) {
        use rand::seq::SliceRandom as _;
        rows.shuffle(&mut rand::rng());
    } else if !orderings.is_empty() {
        rows.sort_by(|a, b| compare_media(a, b, &orderings));
    } else {
        rows.sort_by(|a, b| b.uploaded_at.get().cmp(&a.uploaded_at.get()));
    }

    // #changes — `?updated_since=` so a polling client can ask for only
    // what moved, instead of re-fetching the collection and diffing it.
    let updated_since = match crate::api::query::parse_updated_since(&qs.list) {
        Ok(v) => v,
        Err(msg) => return Ok(crate::api::error::ApiError::bad_request(msg).into_response()),
    };
    if let Some(since) = updated_since {
        rows.retain(|r| r.uploaded_at.get().copied().is_some_and(|t| t >= since));
    }

    let total_count = rows.len();
    let window: Vec<&Media> = rows.iter().skip(offset).take(limit).collect();

    let mut items = Vec::with_capacity(window.len());
    for m in window {
        let mut obj = serialize_image(m);
        // Validated against the first serialized row rather than a
        // hand-written list, so it cannot drift from what we return.
        if let Err(msg) = q::validate_fields(fields.as_ref(), &obj) {
            return Ok(crate::api::error::ApiError::bad_request(msg).into_response());
        }
        q::apply_fields(&mut obj, fields.as_ref());
        items.push(serde_json::Value::Object(obj));
    }

    Ok(Json(q::ListEnvelope {
        meta: q::ListMeta::paged(total_count, limit, offset),
        items,
    })
    .into_response())
}

/// `GET /api/v2/images/{id}/`
pub async fn detail(
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    rustango::tenancy::member_auth::CurrentMember(member): rustango::tenancy::member_auth::CurrentMember,
    crate::api::auth::BearerMember(bearer): crate::api::auth::BearerMember,
    Path(id): Path<i64>,
    Query(q): Query<crate::api::query::DetailFields>,
) -> Response {
    let viewer = member.as_ref().or(bearer.as_ref()).or(session_user.as_ref());
    match detail_inner(&tenant, viewer, id, &q).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, "image detail failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

async fn detail_inner(
    tenant: &Tenant,
    viewer: Option<&rustango::tenancy::auth::User>,
    id: i64,
    q: &crate::api::query::DetailFields,
) -> Result<Response, rustango::sql::ExecError> {
    let row: Option<Media> = Media::objects()
        .where_(Media::id.eq(id))
        .where_(Media::kind.eq("image".to_owned()))
        .first(tenant.pool())
        .await?;
    match row {
        // A gated collection answers exactly as a missing asset does —
        // otherwise this endpoint tells an anonymous caller which ids
        // exist behind the gate.
        Some(m) if !gated_for(tenant, viewer, &m).await => {
            let mut obj = serialize_image(&m);
            crate::api::query::apply_fields(&mut obj, q.keep().as_ref());
            Ok(Json(serde_json::Value::Object(obj)).into_response())
        }
        _ => Ok(crate::api::error::ApiError::not_found("image").into_response()),
    }
}

/// Is this asset's collection closed to `viewer`?
async fn gated_for(
    tenant: &Tenant,
    viewer: Option<&rustango::tenancy::auth::User>,
    media: &Media,
) -> bool {
    let Some(cid) = media.collection_id else {
        return false;
    };
    !crate::collection_view_restriction::denied_collection_ids(tenant.pool(), viewer, &[cid])
        .await
        .is_empty()
}

pub(super) fn serialize_image(m: &Media) -> serde_json::Map<String, serde_json::Value> {
    let id = m.id.get().copied().unwrap_or_default();
    let hash_v: String = m.content_hash.chars().take(12).collect();
    let mut obj = serde_json::Map::new();
    obj.insert("id".to_owned(), serde_json::json!(id));
    obj.insert(
        "meta".to_owned(),
        serde_json::json!({
            "type": "cms.Image",
            "detail_url": format!("/api/v2/images/{id}/"),
            "download_url_template": format!(
                "/__media__/{{filter_spec}}/{id}?v={hash_v}"
            ),
        }),
    );
    // #431 — concrete rendition URLs for common sizes. Built (and
    // signed, when #425 signing is on) server-side so clients get
    // ready-to-use URLs — unlike `download_url_template`, which a
    // client can't sign.
    let hash = m.content_hash.as_str();
    obj.insert(
        "renditions".to_owned(),
        serde_json::json!({
            "thumbnail": crate::rendition::rendition_url_for(id, "fill-100x100", Some(hash)),
            "medium": crate::rendition::rendition_url_for(id, "width-800", Some(hash)),
            "large": crate::rendition::rendition_url_for(id, "width-1600", Some(hash)),
        }),
    );
    obj.insert("title".to_owned(), serde_json::json!(m.title));
    obj.insert("filename".to_owned(), serde_json::json!(m.filename));
    obj.insert("alt_text".to_owned(), serde_json::json!(m.alt_text));
    obj.insert("description".to_owned(), serde_json::json!(m.description));
    obj.insert("mime".to_owned(), serde_json::json!(m.mime));
    obj.insert("size".to_owned(), serde_json::json!(m.size));
    obj.insert("width".to_owned(), serde_json::json!(m.width));
    obj.insert("height".to_owned(), serde_json::json!(m.height));
    obj.insert(
        "focal_point_x".to_owned(),
        serde_json::json!(m.focal_point_x),
    );
    obj.insert(
        "focal_point_y".to_owned(),
        serde_json::json!(m.focal_point_y),
    );
    obj.insert(
        "collection_id".to_owned(),
        serde_json::json!(m.collection_id),
    );
    obj.insert(
        "uploaded_at".to_owned(),
        serde_json::json!(m.uploaded_at.get().copied()),
    );
    obj
}

/// Fields `?order=` accepts on the media endpoints. Must match the arms
/// of `compare_media` below: a field listed here but not handled there
/// would sort by nothing, which is the bug this list exists to prevent.
/// Shared with `documents`, which sorts the same rows.
pub(super) const MEDIA_ORDER_FIELDS: [&str; 4] = ["title", "filename", "size", "uploaded_at"];

pub(super) fn compare_media(a: &Media, b: &Media, specs: &[q::OrderSpec]) -> std::cmp::Ordering {
    for spec in specs {
        let ord = match spec.field.as_str() {
            "title" => a.title.cmp(&b.title),
            "filename" => a.filename.cmp(&b.filename),
            "size" => a.size.cmp(&b.size),
            "uploaded_at" => a.uploaded_at.get().cmp(&b.uploaded_at.get()),
            _ => continue,
        };
        if ord != std::cmp::Ordering::Equal {
            return if spec.descending { ord.reverse() } else { ord };
        }
    }
    std::cmp::Ordering::Equal
}
