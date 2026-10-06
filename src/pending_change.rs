//! Changes to a live page held for review.
//!
//! When a page type's review workflow has *Re-approval on edit* on, a
//! non-superuser's save of a live page doesn't touch the page: the posted
//! form is kept here, the live page stays as it is, and the review
//! starts. The editor shows the pending version (to the author and the
//! reviewer); the final approval applies it through the normal save path,
//! and cancelling the review discards it. One row per page — a later save
//! replaces it.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "cms_page_pending_change", app = "cms")]
pub struct PendingChange {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_page", on = "id", unique)]
    pub page_id: i64,

    /// The posted editor form (field name → value), exactly as the save
    /// path takes it.
    pub form: serde_json::Value,

    /// Who proposed it.
    pub created_by: Option<i64>,

    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

impl PendingChange {
    /// The form as the save path takes it.
    #[must_use]
    pub fn form_map(&self) -> std::collections::HashMap<String, String> {
        self.form
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_owned)))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The page's held change, if any.
///
/// # Errors
/// Driver / query failures.
pub async fn for_page(
    pool: &rustango::sql::Pool,
    page_id: i64,
) -> Result<Option<PendingChange>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    PendingChange::objects()
        .where_(PendingChange::page_id.eq(page_id))
        .first(pool)
        .await
}

/// Keep `form` as the page's held change, replacing an earlier one.
///
/// # Errors
/// Driver / query failures.
pub async fn hold(
    pool: &rustango::sql::Pool,
    page_id: i64,
    form: &std::collections::HashMap<String, String>,
    by: Option<i64>,
) -> Result<(), rustango::sql::ExecError> {
    let value = serde_json::to_value(form).unwrap_or_else(|_| serde_json::json!({}));
    match for_page(pool, page_id).await? {
        Some(mut row) => {
            row.form = value;
            row.created_by = by;
            row.save_pool(pool).await?;
        }
        None => {
            let mut row = PendingChange {
                id: Auto::Unset,
                page_id,
                form: value,
                created_by: by,
                updated_at: Auto::Unset,
            };
            row.save_pool(pool).await?;
        }
    }
    Ok(())
}

/// Drop the page's held change (applied, discarded, or the page deleted).
///
/// # Errors
/// Driver / query failures.
pub async fn discard(pool: &rustango::sql::Pool, page_id: i64) -> Result<(), rustango::sql::ExecError> {
    if let Some(row) = for_page(pool, page_id).await? {
        row.delete_pool(pool).await?;
    }
    Ok(())
}
