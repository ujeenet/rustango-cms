//! `GET /api/v2/locales/` — which content locales this tenant serves.
//!
//! Without this a headless client has to hard-code the language list,
//! and it cannot tell a typo from a real code: `?locale=` falls back to
//! the tenant default *silently* on anything it doesn't recognise
//! (`translation::resolve_locale`), so `fr-typo` and `fr` both return
//! 200 and only the content differs.
//!
//! Inactive locales are omitted. A locale that exists but is switched
//! off is not something a client should offer a reader.

use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango::core::Column as _;
use rustango::extractors::Tenant;
use rustango::sql::FetcherPool as _;

use crate::locale::Locale;

/// `GET /api/v2/locales/`
pub async fn list(tenant: Tenant) -> Response {
    match list_inner(tenant.pool()).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(target: "rustango_cms::api", error = %e, "locales list failed");
            crate::api::error::ApiError::internal().into_response()
        }
    }
}

pub async fn list_inner(
    pool: &rustango::sql::Pool,
) -> Result<Response, rustango::sql::ExecError> {
    let locales: Vec<Locale> = Locale::objects()
        .where_(Locale::active.eq(true))
        .order_by(&[("sort_order", false), ("code", false)])
        .fetch(pool)
        .await?;

    let items: Vec<serde_json::Value> = locales
        .iter()
        .map(|l| {
            serde_json::json!({
                "code": l.code,
                "name": l.name,
                "is_default": l.is_default,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "meta": {
            "total_count": items.len(),
            // Echoed so a client can pick a fallback without scanning.
            "default": locales.iter().find(|l| l.is_default).map(|l| l.code.clone()),
        },
        "items": items,
    }))
    .into_response())
}
