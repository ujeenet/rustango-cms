//! `GET /api/v2/documents/` + `GET /api/v2/documents/{id}/`.
//!
//! Mirrors [`crate::api::images`] for non-image media. Hosts are
//! responsible for wiring an actual byte-serving route for documents
//! — the API surfaces the metadata + storage_key so the host can
//! generate signed download URLs at the application layer.

// Our extractors, not axum's: they refuse a malformed value in the same
// JSON envelope as everything else. See `query::Query`.
use crate::api::query::{Path, Query};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango::core::Column as _;
use rustango::extractors::{SessionUser, Tenant};
use rustango::sql::FetcherPool as _;
use serde::Deserialize;

use super::images::{compare_media, MEDIA_ORDER_FIELDS};
use super::query as q;
use crate::media::Media;

#[derive(Debug, Default, Deserialize)]
pub struct DocumentListQuery {
    #[serde(flatten)]
    pub list: q::ListQuery,
    #[serde(default, deserialize_with = "crate::api::query::empty_as_none")]
    pub collection: Option<i64>,
}

/// `GET /api/v2/documents/`
pub async fn list(
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    rustango::tenancy::member_auth::CurrentMember(member): rustango::tenancy::member_auth::CurrentMember,
    crate::api::auth::BearerMember(bearer): crate::api::auth::BearerMember,
    Query(qs): Query<DocumentListQuery>,
) -> Response {
    let viewer = member.as_ref().or(bearer.as_ref()).or(session_user.as_ref());
    match list_inner(&tenant, viewer, &qs).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, "documents list failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

async fn list_inner(
    tenant: &Tenant,
    viewer: Option<&rustango::tenancy::auth::User>,
    qs: &DocumentListQuery,
) -> Result<Response, rustango::sql::ExecError> {
    let pool = tenant.pool();
    let (limit, offset) = q::paginate(&qs.list);
    let fields = q::field_set(&qs.list);
    let orderings = match q::parse_order_in(&qs.list, &MEDIA_ORDER_FIELDS) {
        Ok(v) => v,
        Err(msg) => return Ok(crate::api::error::ApiError::bad_request(msg).into_response()),
    };

    // Documents = every media row that ISN'T an image. Wagtail also
    // splits "document" from "image" by content type, but rustango's
    // discriminator is the `kind` column so we follow that.
    let mut rows: Vec<Media> = Media::objects()
        .fetch(pool)
        .await?
        .into_iter()
        .filter(|m| m.kind != "image")
        .collect();

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
        let mut obj = serialize_document(m);
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

/// `GET /api/v2/documents/{id}/`
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
            tracing::error!(target: "rustango_cms::api", error = %e, "document detail failed");
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
        .fetch(tenant.pool())
        .await?
        .into_iter()
        .find(|m| m.kind != "image");
    match row {
        // A gated collection answers exactly as a missing asset does —
        // otherwise this endpoint tells an anonymous caller which ids
        // exist behind the gate.
        Some(m) if !gated_for(tenant, viewer, &m).await => {
            let mut obj = serialize_document(&m);
            crate::api::query::apply_fields(&mut obj, q.keep().as_ref());
            Ok(Json(serde_json::Value::Object(obj)).into_response())
        }
        _ => Ok(crate::api::error::ApiError::not_found("document").into_response()),
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

fn serialize_document(m: &Media) -> serde_json::Map<String, serde_json::Value> {
    let id = m.id.get().copied().unwrap_or_default();
    let mut obj = serde_json::Map::new();
    obj.insert("id".to_owned(), serde_json::json!(id));
    obj.insert(
        "meta".to_owned(),
        serde_json::json!({
            "type": "cms.Document",
            "detail_url": format!("/api/v2/documents/{id}/"),
            "storage_key": m.storage_key,
            "content_hash": m.content_hash,
        }),
    );
    obj.insert("title".to_owned(), serde_json::json!(m.title));
    obj.insert("filename".to_owned(), serde_json::json!(m.filename));
    obj.insert("mime".to_owned(), serde_json::json!(m.mime));
    obj.insert("size".to_owned(), serde_json::json!(m.size));
    obj.insert("kind".to_owned(), serde_json::json!(m.kind));
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
