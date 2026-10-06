//! Per-page permission engine (#18).
//!
//! Three pieces:
//!
//! 1. [`PagePermission`] — `(page_id, role_id, permission)` grants
//!    stored in `cms_page_permission`. Sites grant fine-grained
//!    actions per role per subtree.
//! 2. [`user_can`] — `(user_id, page_id, action) → bool`. Walks the
//!    page's ancestor chain collecting grants; superusers short-circuit
//!    to `true`.
//! 3. [`Action`] — the canonical set of action strings the engine
//!    recognizes. Stored as text so the framework's permission system
//!    can interoperate via codenames if/when we expose them.
//!
//! Permissions **inherit** down the tree: a grant on a page applies
//! to every descendant unless overridden by a closer grant. V1 has
//! no explicit denies — any matching grant in the chain returns
//! `true`.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// Canonical action set the engine recognizes. Stored as text so
/// authors can extend the surface without a schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    View,
    Add,
    Edit,
    Publish,
    Delete,
    /// Top-level "may use this admin surface at all" verb — used by
    /// the `cms_admin.access` codename + `with_cms_admin_access`
    /// middleware (#35) to gate the protected router. Unlike the CRUD
    /// verbs it doesn't pair with a resource model; it pairs with a
    /// router surface.
    Access,
    /// Framework-admin variant of [`Action::Access`]. Maps to the
    /// `auth.access_admin` codename the framework's
    /// `permission_required` middleware (rustango#311) enforces on
    /// `/__admin/` + `/admin/`. Surfaced as a separate variant so the
    /// matrix can present "framework admin access" as a one-cell row
    /// without inventing a new codename layout.
    AccessAdmin,
}

impl Action {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Add => "add",
            Self::Edit => "edit",
            Self::Publish => "publish",
            Self::Delete => "delete",
            Self::Access => "access",
            Self::AccessAdmin => "access_admin",
        }
    }

    /// The canonical V1 set; iteration order is also the order the
    /// admin permission editor renders columns in. `Access` is
    /// deliberately omitted — its column lives in a separate Access
    /// section row, not the CRUD grid.
    #[must_use]
    pub fn all() -> &'static [Action] {
        &[
            Self::View,
            Self::Add,
            Self::Edit,
            Self::Publish,
            Self::Delete,
        ]
    }
}

/// One `(page_id, role_id, permission)` grant. UNIQUE at the SQL
/// layer so a role can't be granted the same action twice on the
/// same page.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_page_permission",
    app = "cms",
    admin(
        list_display = "page_id, role_id, permission",
        ordering = "page_id, role_id, permission",
    )
)]
pub struct PagePermission {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    #[rustango(fk = "rustango_roles", on = "id", index)]
    pub role_id: i64,

    /// One of the strings in [`Action::as_str`]. Stored verbatim
    /// so adding actions later doesn't need a schema migration.
    #[rustango(max_length = 32)]
    pub permission: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Resolve `(user_id, page_id, action) → bool` against the per-page
/// permission table. Walks ancestors via the `cms_page.path`
/// materialized prefix and collects matching grants in one query.
///
/// Short-circuits to `true` for tenant superusers. Returns `false`
/// when the user has no roles assigned (V1 — no per-user override
/// table yet).
///
/// # Errors
/// Propagates driver / query failures from the four underlying
/// fetches (user, page, user-roles, permission grants).
pub async fn user_can(
    pool: &rustango::sql::Pool,
    user_id: i64,
    page_id: i64,
    action: Action,
) -> Result<bool, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    // 1) Superuser short-circuit.
    let user = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.eq(user_id))
        .first(pool)
        .await?;
    let Some(user) = user else {
        return Ok(false);
    };
    if !user.active {
        return Ok(false);
    }
    if user.is_superuser {
        return Ok(true);
    }

    // 2) Resolve the user's role ids.
    let memberships: Vec<rustango::tenancy::permissions::UserRole> =
        rustango::tenancy::permissions::UserRole::objects()
            .where_(rustango::tenancy::permissions::UserRole::user_id.eq(user_id))
            .fetch(pool)
            .await?;
    if memberships.is_empty() {
        return Ok(false);
    }
    let role_ids: Vec<i64> = memberships.iter().map(|m| m.role_id).collect();

    // 3) Walk the target page's ancestor chain. `cms_page.path` is a
    //    materialized prefix in path-encoded form (`{depth}-{id}/...`
    //    when the page is at depth N). We use a simpler approach:
    //    fetch the page, then re-fetch every ancestor by id via the
    //    parent_id chain — bounded by tree depth, fine for the admin.
    let target = crate::page::Page::objects()
        .where_(crate::page::Page::id.eq(page_id))
        .first(pool)
        .await?;
    let Some(mut current) = target else {
        return Ok(false);
    };
    let mut chain_ids: Vec<i64> = Vec::new();
    chain_ids.push(current.id.get().copied().unwrap_or_default());
    while let Some(pid) = current.parent_id {
        let parent = crate::page::Page::objects()
            .where_(crate::page::Page::id.eq(pid))
            .first(pool)
            .await?;
        match parent {
            Some(p) => {
                chain_ids.push(pid);
                current = p;
            }
            None => break,
        }
    }

    // 4) Any matching grant in the chain returns true.
    let grants: Vec<PagePermission> = PagePermission::objects()
        .where_(PagePermission::page_id.is_in(chain_ids.iter().copied()))
        .where_(PagePermission::role_id.is_in(role_ids.iter().copied()))
        .where_(PagePermission::permission.eq(action.as_str().to_owned()))
        .fetch(pool)
        .await?;
    Ok(!grants.is_empty())
}

/// One `(collection_id, role_id, permission)` grant — the media
/// collection analogue of [`PagePermission`] (#19). Same UNIQUE
/// shape and same inheritance semantics (parent collection grants
/// flow to children).
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_collection_permission",
    app = "cms",
    admin(
        list_display = "collection_id, role_id, permission",
        ordering = "collection_id, role_id, permission",
    )
)]
pub struct CollectionPermission {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_media_collection", on = "id", index)]
    pub collection_id: i64,

    #[rustango(fk = "rustango_roles", on = "id", index)]
    pub role_id: i64,

    #[rustango(max_length = 32)]
    pub permission: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Resolve `(user_id, collection_id, action) → bool` against the
/// per-collection permission table (#19). Walks the collection's
/// ancestor chain (via `parent_id`) and short-circuits to `true`
/// for superusers.
///
/// # Errors
/// Driver / query failures from the underlying fetches.
pub async fn user_can_in_collection(
    pool: &rustango::sql::Pool,
    user_id: i64,
    collection_id: i64,
    action: Action,
) -> Result<bool, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let user = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.eq(user_id))
        .first(pool)
        .await?;
    let Some(user) = user else {
        return Ok(false);
    };
    if !user.active {
        return Ok(false);
    }
    if user.is_superuser {
        return Ok(true);
    }

    let memberships: Vec<rustango::tenancy::permissions::UserRole> =
        rustango::tenancy::permissions::UserRole::objects()
            .where_(rustango::tenancy::permissions::UserRole::user_id.eq(user_id))
            .fetch(pool)
            .await?;
    if memberships.is_empty() {
        return Ok(false);
    }
    let role_ids: Vec<i64> = memberships.iter().map(|m| m.role_id).collect();

    // Walk the collection's parent chain.
    let target = crate::media::MediaCollection::objects()
        .where_(crate::media::MediaCollection::id.eq(collection_id))
        .first(pool)
        .await?;
    let Some(mut current) = target else {
        return Ok(false);
    };
    let mut chain_ids: Vec<i64> = Vec::new();
    chain_ids.push(current.id.get().copied().unwrap_or_default());
    while let Some(pid) = current.parent_id {
        let parent = crate::media::MediaCollection::objects()
            .where_(crate::media::MediaCollection::id.eq(pid))
            .first(pool)
            .await?;
        match parent {
            Some(p) => {
                chain_ids.push(pid);
                current = p;
            }
            None => break,
        }
    }

    let grants: Vec<CollectionPermission> = CollectionPermission::objects()
        .where_(CollectionPermission::collection_id.is_in(chain_ids.iter().copied()))
        .where_(CollectionPermission::role_id.is_in(role_ids.iter().copied()))
        .where_(CollectionPermission::permission.eq(action.as_str().to_owned()))
        .fetch(pool)
        .await?;
    Ok(!grants.is_empty())
}

/// Which of `pages` — `(id, parent_id)` pairs — `user_id` may view: the
/// set [`user_can`] with [`Action::View`] would allow, in a bounded number
/// of queries instead of one walk per page. For list views, so a listing
/// never shows a row its detail view refuses.
///
/// Superusers see every page; inactive, unknown and role-less users see
/// none.
///
/// # Errors
/// Driver / query failures.
pub async fn viewable_page_ids(
    pool: &rustango::sql::Pool,
    user_id: i64,
    pages: &[(i64, Option<i64>)],
) -> Result<std::collections::HashSet<i64>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    use std::collections::{HashMap, HashSet};

    let user = rustango::tenancy::auth::User::objects()
        .where_(rustango::tenancy::auth::User::id.eq(user_id))
        .first(pool)
        .await?;
    match user {
        Some(u) if u.active && u.is_superuser => return Ok(pages.iter().map(|p| p.0).collect()),
        Some(u) if u.active => {}
        _ => return Ok(HashSet::new()),
    }
    let role_ids: Vec<i64> = rustango::tenancy::permissions::UserRole::objects()
        .where_(rustango::tenancy::permissions::UserRole::user_id.eq(user_id))
        .fetch(pool)
        .await?
        .into_iter()
        .map(|m| m.role_id)
        .collect();
    if role_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let granted: HashSet<i64> = PagePermission::objects()
        .where_(PagePermission::role_id.is_in(role_ids))
        .where_(PagePermission::permission.eq(Action::View.as_str().to_owned()))
        .fetch(pool)
        .await?
        .into_iter()
        .map(|g| g.page_id)
        .collect();
    if granted.is_empty() {
        return Ok(HashSet::new());
    }

    // Every ancestor's parent, fetched a tree level at a time.
    let mut parent_of: HashMap<i64, Option<i64>> = pages.iter().copied().collect();
    loop {
        let missing: HashSet<i64> = parent_of
            .values()
            .flatten()
            .copied()
            .filter(|id| !parent_of.contains_key(id))
            .collect();
        if missing.is_empty() {
            break;
        }
        let found: Vec<crate::page::Page> = crate::page::Page::objects()
            .where_(crate::page::Page::id.is_in(missing.iter().copied()))
            .fetch(pool)
            .await?;
        for id in missing {
            // A dangling parent ends the chain, as it does in `user_can`.
            let parent = found
                .iter()
                .find(|p| p.id.get().copied() == Some(id))
                .and_then(|p| p.parent_id);
            parent_of.insert(id, parent);
        }
    }

    let viewable = |mut id: i64| {
        // Bounded by the map, so a parent cycle cannot spin forever.
        for _ in 0..=parent_of.len() {
            if granted.contains(&id) {
                return true;
            }
            match parent_of.get(&id).copied().flatten() {
                Some(parent) => id = parent,
                None => return false,
            }
        }
        false
    };
    Ok(pages.iter().map(|p| p.0).filter(|&id| viewable(id)).collect())
}

/// Resolve the full action matrix for a user on a page in one shot.
/// Useful for the admin form-state computation where every action
/// is checked together (so the page editor can disable Publish but
/// allow Edit, etc.). Returns a fresh HashMap with one entry per
/// action — `true` when granted, `false` when not.
///
/// # Errors
/// Propagates driver errors as `user_can` would.
pub async fn user_can_matrix(
    pool: &rustango::sql::Pool,
    user_id: i64,
    page_id: i64,
) -> Result<std::collections::HashMap<&'static str, bool>, rustango::sql::ExecError> {
    let mut out = std::collections::HashMap::new();
    for action in Action::all() {
        let allowed = user_can(pool, user_id, page_id, *action).await?;
        out.insert(action.as_str(), allowed);
    }
    Ok(out)
}

/// Resolve the full set of role codenames a user inherits via their
/// `UserRole` memberships. Used by the admin chrome to gate sidebar
/// items + by handlers to enforce CRUD access against the
/// permissions matrix (#33).
///
/// Superusers and inactive users are caller's concern — this helper
/// just returns the codename union for whatever role memberships
/// exist. Empty set when the user has no roles assigned.
///
/// # Errors
/// Propagates driver / query failures from the three underlying
/// fetches (memberships, role permissions).
pub async fn user_codenames(
    pool: &rustango::sql::Pool,
    user_id: i64,
) -> Result<std::collections::HashSet<String>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let memberships: Vec<rustango::tenancy::permissions::UserRole> =
        rustango::tenancy::permissions::UserRole::objects()
            .where_(rustango::tenancy::permissions::UserRole::user_id.eq(user_id))
            .fetch(pool)
            .await?;
    if memberships.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let role_ids: Vec<i64> = memberships.iter().map(|m| m.role_id).collect();
    let grants: Vec<rustango::tenancy::permissions::RolePermission> =
        rustango::tenancy::permissions::RolePermission::objects()
            .where_(
                rustango::tenancy::permissions::RolePermission::role_id
                    .is_in(role_ids.iter().copied()),
            )
            .fetch(pool)
            .await?;
    Ok(grants.into_iter().map(|g| g.codename).collect())
}
