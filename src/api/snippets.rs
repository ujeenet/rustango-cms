//! `GET /api/v2/snippets/` + `GET /api/v2/snippets/{id}/`.
//!
//! Read-only snippet access for headless consumers, mirroring the
//! pages / images / documents endpoints. Snippets are the reusable
//! content rows authors manage under the admin "Library"
//! ([`crate::snippet::Snippet`]); the API exposes their `title`,
//! `slug`, `body_markdown`, and typed `data`. Filter to one type with
//! `?type=<type_name>`.

// Our extractors, not axum's: they refuse a malformed value in the same
// JSON envelope as everything else. See `query::Query`.
use crate::api::query::{Path, Query};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango::core::Column as _;
use rustango::extractors::Tenant;
use rustango::sql::FetcherPool as _;
use serde::Deserialize;

use super::query as q;
use crate::snippet::Snippet;

#[derive(Debug, Default, Deserialize)]
pub struct SnippetListQuery {
    #[serde(flatten)]
    pub list: q::ListQuery,
    /// Optional `type_name` scope — filters snippets by type.
    #[serde(default, rename = "type")]
    pub type_name: Option<String>,
}

/// `GET /api/v2/snippets/`
pub async fn list(tenant: Tenant, Query(qs): Query<SnippetListQuery>) -> Response {
    match list_inner(&tenant, &qs).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, "snippets list failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

async fn list_inner(
    tenant: &Tenant,
    qs: &SnippetListQuery,
) -> Result<Response, rustango::sql::ExecError> {
    let pool = tenant.pool();
    let (limit, offset) = q::paginate(&qs.list);
    let fields = q::field_set(&qs.list);
    let orderings = match q::parse_order_in(&qs.list, &SNIPPET_ORDER_FIELDS) {
        Ok(v) => v,
        Err(msg) => return Ok(crate::api::error::ApiError::bad_request(msg).into_response()),
    };

    let mut rows: Vec<Snippet> = Snippet::objects().fetch(pool).await?;

    if let Some(t) = qs.type_name.as_deref().filter(|s| !s.is_empty()) {
        rows.retain(|s| s.type_name == t);
    }
    if let Some(needle) = qs.list.search.as_deref().filter(|s| !s.trim().is_empty()) {
        let n = crate::api::search::fold(needle);
        rows.retain(|s| {
            crate::api::search::contains_folded(&s.title, &n)
                || crate::api::search::contains_folded(&s.slug, &n)
                || crate::api::search::contains_folded(&s.type_name, &n)
                || crate::api::search::contains_folded(&s.body_markdown, &n)
        });
    }

    if orderings.iter().any(|o| o.random) {
        use rand::seq::SliceRandom as _;
        rows.shuffle(&mut rand::rng());
    } else if !orderings.is_empty() {
        rows.sort_by(|a, b| compare_snippet(a, b, &orderings));
    } else {
        rows.sort_by(|a, b| b.updated_at.get().cmp(&a.updated_at.get()));
    }

    // #changes — `?updated_since=` so a polling client can ask for only
    // what moved, instead of re-fetching the collection and diffing it.
    let updated_since = match crate::api::query::parse_updated_since(&qs.list) {
        Ok(v) => v,
        Err(msg) => return Ok(crate::api::error::ApiError::bad_request(msg).into_response()),
    };
    if let Some(since) = updated_since {
        rows.retain(|r| r.updated_at.get().copied().is_some_and(|t| t >= since));
    }

    let total_count = rows.len();
    let window: Vec<&Snippet> = rows.iter().skip(offset).take(limit).collect();

    let mut items = Vec::with_capacity(window.len());
    for s in window {
        let mut obj = serialize_snippet(s);
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

/// `GET /api/v2/snippets/{id}/`
pub async fn detail(
    tenant: Tenant,
    Path(id): Path<i64>,
    Query(q): Query<crate::api::query::DetailFields>,
) -> Response {
    match detail_inner(&tenant, id, &q).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, "snippet detail failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

async fn detail_inner(
    tenant: &Tenant,
    id: i64,
    q: &crate::api::query::DetailFields,
) -> Result<Response, rustango::sql::ExecError> {
    let row: Option<Snippet> = Snippet::objects()
        .where_(Snippet::id.eq(id))
        .first(tenant.pool())
        .await?;
    match row {
        Some(row) => {
            let mut obj = serialize_snippet(&row);
            crate::api::query::apply_fields(&mut obj, q.keep().as_ref());
            Ok(Json(serde_json::Value::Object(obj)).into_response())
        }
        None => Ok(crate::api::error::ApiError::not_found("snippet").into_response()),
    }
}

fn serialize_snippet(s: &Snippet) -> serde_json::Map<String, serde_json::Value> {
    let id = s.id.get().copied().unwrap_or_default();
    let mut obj = serde_json::Map::new();
    obj.insert("id".to_owned(), serde_json::json!(id));
    obj.insert(
        "meta".to_owned(),
        serde_json::json!({
            "type": "cms.Snippet",
            "type_name": s.type_name,
            "detail_url": format!("/api/v2/snippets/{id}/"),
            "updated_at": s.updated_at.get().copied(),
        }),
    );
    obj.insert("type_name".to_owned(), serde_json::json!(s.type_name));
    obj.insert("slug".to_owned(), serde_json::json!(s.slug));
    obj.insert("title".to_owned(), serde_json::json!(s.title));
    obj.insert("folder_path".to_owned(), serde_json::json!(s.folder_path));
    obj.insert(
        "body_markdown".to_owned(),
        serde_json::json!(s.body_markdown),
    );
    obj.insert("data".to_owned(), s.data.clone());
    obj
}

/// Fields `?order=` accepts here. Must match the arms of
/// `compare_snippet` below.
const SNIPPET_ORDER_FIELDS: [&str; 6] =
    ["title", "slug", "type_name", "type", "updated_at", "created_at"];

fn compare_snippet(a: &Snippet, b: &Snippet, specs: &[q::OrderSpec]) -> std::cmp::Ordering {
    for spec in specs {
        let ord = match spec.field.as_str() {
            "title" => a.title.cmp(&b.title),
            "slug" => a.slug.cmp(&b.slug),
            "type_name" | "type" => a.type_name.cmp(&b.type_name),
            "updated_at" => a.updated_at.get().cmp(&b.updated_at.get()),
            "created_at" => a.created_at.get().cmp(&b.created_at.get()),
            _ => continue,
        };
        if ord != std::cmp::Ordering::Equal {
            return if spec.descending { ord.reverse() } else { ord };
        }
    }
    std::cmp::Ordering::Equal
}
