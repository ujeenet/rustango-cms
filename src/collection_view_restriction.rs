//! `CollectionViewRestriction` — gate served media bytes behind a
//! login requirement or group membership check (#195, Wagtail
//! parity).
//!
//! Mirrors [`crate::view_restriction::PageViewRestriction`] but keyed
//! by [`crate::media::MediaCollection`]. Restrictions inherit DOWN
//! the collection tree: a restriction on "Internal docs" covers
//! every child collection unless a child declares its own.
//!
//! The public renderer enforces the restriction in
//! [`crate::rendition_route`] — `<img src="/__media__/.../<id>">`
//! returns 401 / 403 when the request doesn't satisfy the nearest
//! restriction.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

pub use crate::view_restriction::RestrictionKind;

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_collection_view_restriction",
    app = "cms",
    display = "collection_id",
    admin(
        list_display = "collection_id, kind, created_at",
        ordering = "-created_at",
        list_filter = "kind",
    )
)]
pub struct CollectionViewRestriction {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// One restriction per collection — application-enforced UNIQUE.
    #[rustango(fk = "cms_media_collection", on = "id", index)]
    pub collection_id: i64,

    /// `RestrictionKind::as_str()`.
    #[rustango(max_length = 16, index)]
    pub kind: String,

    /// Argon2id PHC hash. Empty string when `kind != password`.
    #[rustango(max_length = 255, default = "''")]
    pub password_hash: String,

    /// JSON array of `rustango_roles.id` integers — admitted groups
    /// when `kind = groups`. Empty array on other kinds.
    pub group_ids: serde_json::Value,

    /// JSON array of permission codename strings — admitted permissions
    /// when `kind = permission` (any one grants access). Empty array on
    /// other kinds. See [`RestrictionKind::Permission`].
    #[rustango(default = "'[]'")]
    pub codenames: serde_json::Value,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,

    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

impl CollectionViewRestriction {
    #[must_use]
    pub fn parsed_kind(&self) -> Option<RestrictionKind> {
        RestrictionKind::parse(&self.kind)
    }

    #[must_use]
    pub fn parsed_groups(&self) -> Vec<i64> {
        self.group_ids
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect::<Vec<_>>())
            .unwrap_or_default()
    }

    /// Read `codenames` as `Vec<String>`. Returns empty when the field
    /// is malformed or missing (a NULL column on a pre-migration row).
    #[must_use]
    pub fn parsed_codenames(&self) -> Vec<String> {
        self.codenames
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }
}

/// Look up a direct (non-inherited) restriction on a single collection.
///
/// # Errors
/// Driver / query failures.
pub async fn direct_for_collection(
    pool: &rustango::sql::Pool,
    collection_id: i64,
) -> Result<Option<CollectionViewRestriction>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<CollectionViewRestriction> = CollectionViewRestriction::objects()
        .where_(CollectionViewRestriction::collection_id.eq(collection_id))
        .fetch(pool)
        .await?;
    Ok(rows.into_iter().next())
}

/// Walk the collection + every ancestor in parent order (current
/// first, then parent, then grand-parent, …) and return the *nearest*
/// restriction found. Mirrors
/// [`crate::view_restriction::effective_for_page`] for collections.
///
/// # Errors
/// Driver / query failures.
pub async fn effective_for_collection(
    pool: &rustango::sql::Pool,
    collection_id: i64,
) -> Result<Option<CollectionViewRestriction>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    // Resolve the ancestor chain by walking `parent_id` up to a max
    // depth — collections are shallow trees (a handful of levels
    // typical), so the N+1 lookup is acceptable. Bail out on cycles.
    let mut chain: Vec<i64> = vec![collection_id];
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    seen.insert(collection_id);
    let mut current_id = collection_id;
    for _ in 0..16 {
        let rows: Vec<crate::media::MediaCollection> = crate::media::MediaCollection::objects()
            .where_(crate::media::MediaCollection::id.eq(current_id))
            .fetch(pool)
            .await?;
        let Some(c) = rows.into_iter().next() else {
            break;
        };
        let Some(pid) = c.parent_id else {
            break;
        };
        if !seen.insert(pid) {
            break;
        }
        chain.push(pid);
        current_id = pid;
    }

    let restrictions: Vec<CollectionViewRestriction> = CollectionViewRestriction::objects()
        .where_(CollectionViewRestriction::collection_id.is_in(chain.clone()))
        .fetch(pool)
        .await?;
    if restrictions.is_empty() {
        return Ok(None);
    }
    let by_collection: std::collections::HashMap<i64, CollectionViewRestriction> = restrictions
        .into_iter()
        .map(|r| (r.collection_id, r))
        .collect();
    for cid in chain {
        if let Some(r) = by_collection.get(&cid) {
            return Ok(Some(r.clone()));
        }
    }
    Ok(None)
}

/// Which of `collection_ids` this viewer may **not** see.
///
/// The batched counterpart to [`effective_for_collection`], which walks
/// the ancestor chain with a query per level — fine for one asset, hopeless
/// for a list of them. This loads every collection and every restriction
/// once, then resolves each chain in memory.
///
/// It exists because the JSON list endpoints did no collection gating at
/// all: `/__media__/{spec}/{id}` refuses an asset in a login-gated
/// collection, while `/api/v2/images/` happily listed its title, filename
/// and dimensions to anyone. The bytes were protected; the catalogue was
/// not.
pub async fn denied_collection_ids(
    pool: &rustango::sql::Pool,
    viewer: Option<&rustango::tenancy::auth::User>,
    collection_ids: &[i64],
) -> std::collections::HashSet<i64> {
    use rustango::sql::FetcherPool as _;
    use std::collections::{HashMap, HashSet};

    let mut denied: HashSet<i64> = HashSet::new();
    if collection_ids.is_empty() {
        return denied;
    }

    let restrictions: Vec<CollectionViewRestriction> =
        match CollectionViewRestriction::objects().fetch(pool).await {
            Ok(r) => r,
            Err(e) => {
                // Fail *closed* would break every gallery on a transient
                // error; fail open matches `access_context`, which logs
                // and allows. The bytes stay gated either way.
                tracing::warn!(
                    target: "rustango_cms::collection_view_restriction",
                    error = %e,
                    "collection restriction lookup failed; treating collections as public",
                );
                return denied;
            }
        };
    if restrictions.is_empty() {
        return denied;
    }
    let by_collection: HashMap<i64, &CollectionViewRestriction> = restrictions
        .iter()
        .map(|r| (r.collection_id, r))
        .collect();

    let parents: HashMap<i64, Option<i64>> = crate::media::MediaCollection::objects()
        .fetch(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|c| c.id.get().copied().map(|id| (id, c.parent_id)))
        .collect();

    let facts = crate::view_restriction::viewer_facts(pool, viewer, true).await;

    for &id in collection_ids {
        // Nearest restriction wins, walking up. Bounded and cycle-safe,
        // like `effective_for_collection`.
        let mut current = Some(id);
        let mut seen: HashSet<i64> = HashSet::new();
        for _ in 0..16 {
            let Some(cid) = current else { break };
            if !seen.insert(cid) {
                break;
            }
            if let Some(r) = by_collection.get(&cid) {
                if !facts.satisfies(r.parsed_kind(), &r.parsed_groups(), &r.parsed_codenames()) {
                    denied.insert(id);
                }
                break;
            }
            current = parents.get(&cid).copied().flatten();
        }
    }
    denied
}
