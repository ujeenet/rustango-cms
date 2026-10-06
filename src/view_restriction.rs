//! `PageViewRestriction` — gate published pages behind a password,
//! a login requirement, or a group membership check (#76).
//!
//! Restrictions inherit down the subtree: a restriction on `/members`
//! covers every descendant unless a descendant declares its own
//! restriction. The public renderer walks the ancestor chain at
//! request time and applies the *nearest* restriction it finds.
//!
//! Four kinds (Wagtail parity + #members):
//!   * `password` — anonymous visitor sees a prompt form, enters
//!     the password, gets a signed cookie that skips the prompt on
//!     subsequent visits.
//!   * `login`    — anonymous visitor redirected to
//!     /members/login?next=<url>. Any authenticated member/admin passes.
//!   * `groups`   — logged-in visitor must be a member of at least
//!     one of the named roles (raw `role_id` match). Anonymous →
//!     /members/login; logged-in but not in any allowed group → 403.
//!   * `permission` — logged-in visitor must hold at least one of the
//!     named permission **codenames**, resolved through the framework
//!     permission engine (`has_any_perm_pool` — superuser + per-user
//!     grant/deny baked in). This is the members-area gate; roles carry
//!     the codenames, admins assign the roles.
//!
//! A page-**type** can also declare a `login`/`permission` restriction
//! for every page of that type (see [`TypeViewRestriction`] +
//! [`effective_or_type_for_page`]).

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestrictionKind {
    Password,
    Login,
    Groups,
    /// Gate by the framework **permission engine**: the viewer must
    /// hold at least one of the restriction's permission codenames
    /// (checked via `rustango::tenancy::has_any_perm_pool`, which bakes
    /// in superuser bypass + per-user grant/deny overrides). Roles like
    /// "Pro" / "Moderator" *carry* those codenames; admins assign the
    /// roles. Distinct from `Groups`, which matches raw `role_id`s.
    Permission,
}

impl RestrictionKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Login => "login",
            Self::Groups => "groups",
            Self::Permission => "permission",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "password" => Self::Password,
            "login" => Self::Login,
            "groups" => Self::Groups,
            "permission" => Self::Permission,
            _ => return None,
        })
    }
}

/// A page-**type**-level view restriction declared by a
/// [`crate::page_type::PageTypeHandler::view_restriction`] hook. Only
/// `Login` and `Permission` kinds make sense at the type level
/// (`password` + `groups` need per-page configuration). Applied only
/// when a page carries no page/subtree restriction of its own.
#[derive(Debug, Clone)]
pub struct TypeViewRestriction {
    pub kind: RestrictionKind,
    pub codenames: Vec<String>,
}

impl TypeViewRestriction {
    /// Require any authenticated member (login-gate the whole type).
    #[must_use]
    pub fn login() -> Self {
        Self {
            kind: RestrictionKind::Login,
            codenames: Vec::new(),
        }
    }

    /// Require at least one of `codenames` via the permission engine.
    #[must_use]
    pub fn permission<I, S>(codenames: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            kind: RestrictionKind::Permission,
            codenames: codenames.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_page_view_restriction",
    app = "cms",
    display = "page_id",
    admin(
        list_display = "page_id, kind, created_at",
        ordering = "-created_at",
        list_filter = "kind",
    )
)]
pub struct PageViewRestriction {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// One restriction per page — application-enforced UNIQUE.
    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    /// `RestrictionKind::as_str()`.
    #[rustango(max_length = 16, index)]
    pub kind: String,

    /// Argon2id PHC hash. Empty string when `kind != password`.
    #[rustango(max_length = 255)]
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

impl PageViewRestriction {
    /// Convenience parser returning the typed kind.
    #[must_use]
    pub fn parsed_kind(&self) -> Option<RestrictionKind> {
        RestrictionKind::parse(&self.kind)
    }

    /// Read `group_ids` as `Vec<i64>`. Returns empty when the field
    /// is malformed or missing.
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

/// Look up a direct (non-inherited) restriction on a single page.
///
/// # Errors
/// Driver / query failures.
pub async fn direct_for_page(
    pool: &rustango::sql::Pool,
    page_id: i64,
) -> Result<Option<PageViewRestriction>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<PageViewRestriction> = PageViewRestriction::objects()
        .where_(PageViewRestriction::page_id.eq(page_id))
        .fetch(pool)
        .await?;
    Ok(rows.into_iter().next())
}

/// Walk the page + every ancestor in path order (current page first,
/// then parent, then grand-parent, …) and return the *nearest*
/// restriction found. Used by the public renderer middleware.
///
/// # Errors
/// Driver / query failures.
pub async fn effective_for_page(
    pool: &rustango::sql::Pool,
    page: &crate::page::Page,
) -> Result<Option<PageViewRestriction>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    // Collect the page's own id + every ancestor id from the
    // materialized path. `tree::MaterializedPath::ancestors_of`
    // returns ancestor paths in root-first order; we want
    // nearest-first, so iterate path segments + own id in reverse.
    let own_id = page.id.get().copied().unwrap_or_default();
    let ancestor_ids: Vec<i64> = if let Some(parent_id) = page.parent_id {
        // Pull every ancestor row in one query via materialized-path
        // ancestor list, then collect ids in nearest-first order.
        let ancestor_paths = crate::tree::MaterializedPath::ancestors_of(&page.path);
        let ancestors: Vec<crate::page::Page> = if ancestor_paths.is_empty() {
            Vec::new()
        } else {
            crate::page::Page::objects()
                .where_(crate::page::Page::path.is_in(ancestor_paths.iter().cloned()))
                .fetch(pool)
                .await?
        };
        // Sort by depth desc so the deepest (nearest) ancestor comes
        // first; if no ancestors loaded, fall back to just parent_id.
        let mut by_id: std::collections::HashMap<i64, &crate::page::Page> = ancestors
            .iter()
            .filter_map(|a| a.id.get().copied().map(|id| (id, a)))
            .collect();
        let mut chain: Vec<i64> = Vec::new();
        let mut current_parent = Some(parent_id);
        while let Some(pid) = current_parent {
            chain.push(pid);
            current_parent = by_id.remove(&pid).and_then(|a| a.parent_id);
        }
        chain
    } else {
        Vec::new()
    };

    let mut search: Vec<i64> = Vec::with_capacity(ancestor_ids.len() + 1);
    search.push(own_id);
    search.extend(ancestor_ids);

    // One query for every candidate page, then pick the first hit
    // in our nearest-first order.
    let restrictions: Vec<PageViewRestriction> = if search.is_empty() {
        Vec::new()
    } else {
        PageViewRestriction::objects()
            .where_(PageViewRestriction::page_id.is_in(search.iter().copied()))
            .fetch(pool)
            .await?
    };
    if restrictions.is_empty() {
        return Ok(None);
    }
    let by_page: std::collections::HashMap<i64, PageViewRestriction> =
        restrictions.into_iter().map(|r| (r.page_id, r)).collect();
    for pid in search {
        if let Some(r) = by_page.get(&pid) {
            return Ok(Some(r.clone()));
        }
    }
    Ok(None)
}

/// The page's effective restriction (own + inherited from any
/// ancestor), OR — when none is set anywhere on the branch — the
/// restriction its page **type** declares via
/// [`crate::page_type::PageTypeHandler::view_restriction`]. A concrete
/// page/subtree restriction is more specific and always wins over the
/// type-level one.
///
/// The type-level restriction is returned as a *synthesized*
/// (unsaved) [`PageViewRestriction`] so the enforcement path
/// ([`crate::view_restriction_guard::enforce`]) treats it identically
/// to a stored one — no separate code path.
///
/// # Errors
/// Driver / query failures from the page-restriction lookup.
pub async fn effective_or_type_for_page(
    pool: &rustango::sql::Pool,
    page: &crate::page::Page,
) -> Result<Option<PageViewRestriction>, rustango::sql::ExecError> {
    if let Some(r) = effective_for_page(pool, page).await? {
        return Ok(Some(r));
    }
    Ok(type_restriction_for_page(pool, page)
        .await?
        .map(|tvr| synthesize_type_restriction(page, &tvr)))
}

/// Resolve the page-type handler's declared restriction, if any.
///
/// A lookup failure is an error, not "unrestricted": the caller denies
/// on error (#757), where a silent `None` used to serve the page.
async fn type_restriction_for_page(
    pool: &rustango::sql::Pool,
    page: &crate::page::Page,
) -> Result<Option<TypeViewRestriction>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let pts: Vec<crate::PageType> = crate::PageType::objects()
        .where_(crate::PageType::id.eq(page.page_type_id))
        .fetch(pool)
        .await?;
    Ok(pts
        .into_iter()
        .next()
        .and_then(|pt| crate::page_type::find_handler(&pt.type_name)?.view_restriction()))
}

/// Build an unsaved [`PageViewRestriction`] mirroring a type-level
/// descriptor so the guard can enforce it with its existing logic.
fn synthesize_type_restriction(
    page: &crate::page::Page,
    tvr: &TypeViewRestriction,
) -> PageViewRestriction {
    synthesize_restriction(page.id.get().copied().unwrap_or_default(), tvr)
}

/// Build an unsaved [`PageViewRestriction`] for a given page id from a
/// type-level descriptor.
fn synthesize_restriction(page_id: i64, tvr: &TypeViewRestriction) -> PageViewRestriction {
    use rustango::sql::Auto;
    PageViewRestriction {
        id: Auto::Unset,
        page_id,
        kind: tvr.kind.as_str().to_owned(),
        password_hash: String::new(),
        group_ids: serde_json::json!([]),
        codenames: serde_json::json!(tvr.codenames),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    }
}

/// Bulk-resolve the effective page/subtree restriction for many pages
/// at once — for viewer-aware menu filtering, where calling
/// [`effective_for_page`] per item would be an N+1 on every render.
///
/// `pages` are `(page_id, materialized_path)` pairs. Returns
/// `page_id → nearest restriction` (own or inherited from the closest
/// restricted ancestor). Cost: two bounded queries total (all tenant
/// restriction rows + the restricted pages' paths), then in-memory
/// longest-prefix matching over the materialized paths.
///
/// Type-level restrictions are layered on separately by the caller (see
/// [`type_restriction_for_type`]); this covers only stored page/subtree
/// rows.
pub async fn effective_restrictions_for(
    pool: &rustango::sql::Pool,
    pages: &[(i64, String)],
) -> std::collections::HashMap<i64, PageViewRestriction> {
    use rustango::sql::FetcherPool as _;
    let mut out = std::collections::HashMap::new();
    if pages.is_empty() {
        return out;
    }
    let all: Vec<PageViewRestriction> = match PageViewRestriction::objects().fetch(pool).await {
        Ok(rows) => rows,
        Err(e) => return deny_every_page(pages, &e),
    };
    if all.is_empty() {
        return out;
    }
    // Resolve each restricted page's materialized path so inheritance
    // can be evaluated as a path-prefix test.
    let restricted_ids: Vec<i64> = all.iter().map(|r| r.page_id).collect();
    let restricted_pages: Vec<crate::page::Page> = {
        use rustango::core::Column as _;
        match crate::page::Page::objects()
            .where_(crate::page::Page::id.is_in(restricted_ids.iter().copied()))
            .fetch(pool)
            .await
        {
            Ok(rows) => rows,
            Err(e) => return deny_every_page(pages, &e),
        }
    };
    let restriction_by_id: std::collections::HashMap<i64, &PageViewRestriction> =
        all.iter().map(|r| (r.page_id, r)).collect();
    // (path, page_id) for restricted pages, longest path first so the
    // nearest ancestor wins the prefix match.
    let mut restricted: Vec<(String, i64)> = restricted_pages
        .iter()
        .filter_map(|p| p.id.get().copied().map(|id| (p.path.clone(), id)))
        .collect();
    restricted.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
    for (pid, mpath) in pages {
        for (rpath, rid) in &restricted {
            if mpath.starts_with(rpath.as_str()) {
                if let Some(r) = restriction_by_id.get(rid) {
                    out.insert(*pid, (*r).clone());
                }
                break;
            }
        }
    }
    out
}

/// Resolve the type-level restriction for a page-type id, if the
/// handler declares one. Used by menu filtering to hide type-gated
/// pages; synthesizes a [`PageViewRestriction`] so callers share one
/// predicate ([`viewer_satisfies`]).
#[must_use]
pub fn type_restriction_for_type(
    page_type_id: i64,
    type_name: &str,
) -> Option<PageViewRestriction> {
    let tvr = crate::page_type::find_handler(type_name)?.view_restriction()?;
    Some(synthesize_restriction(page_type_id, &tvr))
}

/// Decide whether a viewer may *see* a restricted item, using
/// pre-resolved viewer facts (so menu filtering avoids per-item
/// queries). `is_authed` = a member/admin is signed in; `is_super` =
/// superuser; `perm_set` = the viewer's permission codenames (from
/// `user_permissions_pool`); `role_ids` = the viewer's role ids.
///
/// `Password` restrictions are treated as **visible** — password is
/// identity-orthogonal, so the link shows and the guard prompts on
/// click (matching the pre-#members behavior).
#[must_use]
pub fn viewer_satisfies(
    restriction: &PageViewRestriction,
    is_authed: bool,
    is_super: bool,
    perm_set: &std::collections::HashSet<String>,
    role_ids: &std::collections::HashSet<i64>,
) -> bool {
    ViewerFacts {
        is_authed,
        is_super,
        perm_set: perm_set.clone(),
        role_ids: role_ids.clone(),
    }
    .satisfies(
        restriction.parsed_kind(),
        &restriction.parsed_groups(),
        &restriction.parsed_codenames(),
    )
}

/// A viewer's resolved access facts, independent of what is being
/// guarded.
///
/// [`AccessContext`] holds these alongside the *page* restrictions, but
/// media collections gate on exactly the same four things and had no way
/// to reuse them — so `/api/v2/images/` listed assets that `/__media__/`
/// refuses to serve. Splitting the facts out lets both ask the same
/// question of the same viewer.
#[derive(Debug, Clone, Default)]
pub struct ViewerFacts {
    pub is_authed: bool,
    pub is_super: bool,
    pub perm_set: std::collections::HashSet<String>,
    pub role_ids: std::collections::HashSet<i64>,
}

impl ViewerFacts {
    /// Does this viewer satisfy a restriction of `kind`?
    ///
    /// Takes the restriction decomposed rather than by type, because
    /// pages and collections store the same three fields in two
    /// different tables.
    ///
    /// `Password` returns `true`: a password gate is satisfied by a
    /// cookie the guard checks at request time, not by who you are, so
    /// hiding password-protected items from listings would be wrong —
    /// the prompt is the point.
    #[must_use]
    pub fn satisfies(
        &self,
        kind: Option<RestrictionKind>,
        groups: &[i64],
        codenames: &[String],
    ) -> bool {
        if self.is_super {
            return true;
        }
        match kind {
            Some(RestrictionKind::Login) => self.is_authed,
            Some(RestrictionKind::Groups) => groups.iter().any(|g| self.role_ids.contains(g)),
            Some(RestrictionKind::Permission) => {
                codenames.iter().any(|c| self.perm_set.contains(c))
            }
            Some(RestrictionKind::Password) => true,
            // A kind this build can't read denies, as the serve path does.
            None => false,
        }
    }
}

/// Resolve a viewer's permissions and roles.
///
/// `gated` lets the caller skip both lookups when nothing is restricted
/// — the common case, and the reason a public site pays nothing for this
/// machinery.
pub async fn viewer_facts(
    pool: &rustango::sql::Pool,
    viewer: Option<&rustango::tenancy::auth::User>,
    gated: bool,
) -> ViewerFacts {
    let is_super = viewer.is_some_and(|u| u.is_superuser);
    let (perm_set, role_ids) = match viewer {
        Some(u) if gated && !is_super => {
            let uid = u.id.get().copied().unwrap_or_default();
            let perms = rustango::tenancy::user_permissions_pool(uid, pool)
                .await
                .unwrap_or_default();
            (
                perms.into_iter().collect(),
                viewer_role_ids(pool, uid).await,
            )
        }
        _ => (
            std::collections::HashSet::new(),
            std::collections::HashSet::new(),
        ),
    };
    ViewerFacts {
        is_authed: viewer.is_some(),
        is_super,
        perm_set,
        role_ids,
    }
}

/// A resolve-once, in-memory oracle for "can this viewer see page X?".
/// Built per render by [`access_context`] and reused for viewer-aware
/// menu hiding **and** the `can_view` / `visible` Tera helpers (so a
/// template can guard any hand-written page link or nested list with
/// the same protection the built-in menus apply). Holds the viewer's
/// resolved facts + every tenant view-restriction (page/subtree and
/// per-type), so [`AccessContext::can_view`] answers with zero further
/// queries.
pub struct AccessContext {
    is_authed: bool,
    is_super: bool,
    perm_set: std::collections::HashSet<String>,
    role_ids: std::collections::HashSet<i64>,
    /// `(materialized_path, restriction)` for every restricted page,
    /// sorted longest-path-first so the nearest ancestor wins the
    /// subtree-inheritance prefix match.
    restricted: Vec<(String, PageViewRestriction)>,
    /// `page_type_id → restriction` for every type whose handler
    /// declares one (empty when nothing gates by type).
    type_restrictions: std::collections::HashMap<i64, PageViewRestriction>,
}

impl AccessContext {
    /// True when nothing is gated at all — callers can short-circuit to
    /// "everything visible" without inspecting each page.
    #[must_use]
    pub fn is_unrestricted(&self) -> bool {
        self.restricted.is_empty() && self.type_restrictions.is_empty()
    }

    /// True when some page type carries a view restriction.
    #[must_use]
    pub fn gates_by_type(&self) -> bool {
        !self.type_restrictions.is_empty()
    }

    /// Can the viewer see the page with this materialized `path` +
    /// `page_type_id`? A page/subtree restriction (nearest ancestor by
    /// longest path prefix) wins; otherwise the page-type restriction
    /// applies; otherwise the page is public.
    #[must_use]
    pub fn can_view(&self, path: &str, page_type_id: i64) -> bool {
        let restriction = self
            .restricted
            .iter()
            .find(|(rpath, _)| path.starts_with(rpath.as_str()))
            .map(|(_, r)| r)
            .or_else(|| self.type_restrictions.get(&page_type_id));
        match restriction {
            None => true,
            Some(r) => viewer_satisfies(
                r,
                self.is_authed,
                self.is_super,
                &self.perm_set,
                &self.role_ids,
            ),
        }
    }
}

impl AccessContext {
    /// What this viewer may not see, as SQL exclusions: the subtree path
    /// prefixes and page type ids whose restriction it fails. Over-hides
    /// rather than under-hides — a page the viewer could open under a
    /// nested restriction it satisfies is still excluded with its denied
    /// ancestor.
    fn denied_scopes(&self) -> (Vec<String>, Vec<i64>) {
        let satisfies =
            |r| viewer_satisfies(r, self.is_authed, self.is_super, &self.perm_set, &self.role_ids);
        let prefixes = self
            .restricted
            .iter()
            .filter(|(_, r)| !satisfies(r))
            .map(|(path, _)| path.clone())
            .collect();
        let types = self
            .type_restrictions
            .iter()
            .filter(|(_, r)| !satisfies(r))
            .map(|(tid, _)| *tid)
            .collect();
        (prefixes, types)
    }
}

/// Narrow a page query to what an anonymous visitor may see (#644).
///
/// For crawler-facing listings (feed, sitemap), which are cached and read
/// without a session. The filter is in SQL so paging and counts stay
/// right. Materialized paths hold only alphanumerics and `/`, so a
/// prefix needs no LIKE escaping.
pub async fn only_anonymous_visible(
    pool: &rustango::sql::Pool,
    mut qs: rustango::query::QuerySet<crate::page::Page>,
) -> rustango::query::QuerySet<crate::page::Page> {
    use rustango::core::Column as _;
    let (prefixes, types) = access_context(pool, None).await.denied_scopes();
    for prefix in prefixes {
        qs = qs.where_(crate::page::Page::path.not_like(format!("{prefix}%")));
    }
    if !types.is_empty() {
        qs = qs.where_(crate::page::Page::page_type_id.not_in(types));
    }
    qs
}

impl AccessContext {
    /// Every path gated behind [`deny_restriction`]: superusers see
    /// everything, everyone else nothing.
    fn deny_all(is_super: bool) -> Self {
        Self {
            is_authed: false,
            is_super,
            perm_set: std::collections::HashSet::new(),
            role_ids: std::collections::HashSet::new(),
            restricted: vec![(String::new(), deny_restriction(0))],
            type_restrictions: std::collections::HashMap::new(),
        }
    }
}

#[cfg(test)]
impl AccessContext {
    /// Test-only: a context that gates every page whose materialized
    /// path starts with `deny_prefix` behind a `login` restriction (so an
    /// anonymous viewer is denied, everything else public).
    pub(crate) fn deny_prefix_for_test(deny_prefix: &str) -> Self {
        use rustango::sql::Auto;
        let r = PageViewRestriction {
            id: Auto::Unset,
            page_id: 0,
            kind: RestrictionKind::Login.as_str().to_owned(),
            password_hash: String::new(),
            group_ids: serde_json::json!([]),
            codenames: serde_json::json!([]),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        Self {
            is_authed: false,
            is_super: false,
            perm_set: std::collections::HashSet::new(),
            role_ids: std::collections::HashSet::new(),
            restricted: vec![(deny_prefix.to_owned(), r)],
            type_restrictions: std::collections::HashMap::new(),
        }
    }

    /// Test-only: a context gating one page type behind a login.
    pub(crate) fn deny_type_for_test(page_type_id: i64) -> Self {
        let mut ctx = Self::deny_prefix_for_test("\u{0}");
        let (_, r) = ctx.restricted.pop().expect("one restriction");
        ctx.type_restrictions.insert(page_type_id, r);
        ctx
    }

    /// Test-only: a context that gates nothing.
    pub(crate) fn unrestricted_for_test() -> Self {
        Self {
            is_authed: false,
            is_super: false,
            perm_set: std::collections::HashSet::new(),
            role_ids: std::collections::HashSet::new(),
            restricted: Vec::new(),
            type_restrictions: std::collections::HashMap::new(),
        }
    }
}

/// Build the per-render [`AccessContext`] for `viewer`: load every
/// tenant restriction (+ the restricted pages' paths) and per-type
/// restriction, and resolve the viewer's codenames + role ids. Bounded,
/// fixed number of queries; the viewer lookups are skipped entirely when
/// nothing is gated or the viewer is a superuser.
///
/// When the restrictions can't be read the context hides every page from
/// everyone but superusers (#757): a menu or search result must not list
/// a gated page's title and URL because the lookup failed.
pub async fn access_context(
    pool: &rustango::sql::Pool,
    viewer: Option<&rustango::tenancy::auth::User>,
) -> AccessContext {
    match try_access_context(pool, viewer).await {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::warn!(
                target: "rustango_cms::view_restriction",
                error = %e,
                "restriction lookup failed; hiding restricted listings"
            );
            AccessContext::deny_all(viewer.is_some_and(|u| u.is_superuser))
        }
    }
}

async fn try_access_context(
    pool: &rustango::sql::Pool,
    viewer: Option<&rustango::tenancy::auth::User>,
) -> Result<AccessContext, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    use std::collections::HashMap;

    // Page/subtree restrictions + their materialized paths.
    let all: Vec<PageViewRestriction> = PageViewRestriction::objects().fetch(pool).await?;
    let restricted: Vec<(String, PageViewRestriction)> = if all.is_empty() {
        Vec::new()
    } else {
        let ids: Vec<i64> = all.iter().map(|r| r.page_id).collect();
        let pages: Vec<crate::page::Page> = crate::page::Page::objects()
            .where_(crate::page::Page::id.is_in(ids.iter().copied()))
            .fetch(pool)
            .await?;
        let path_by_id: HashMap<i64, String> = pages
            .iter()
            .filter_map(|p| p.id.get().copied().map(|id| (id, p.path.clone())))
            .collect();
        let mut v: Vec<(String, PageViewRestriction)> = all
            .into_iter()
            .filter_map(|r| path_by_id.get(&r.page_id).map(|p| (p.clone(), r)))
            .collect();
        v.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
        v
    };

    // Per-type restrictions (only when some handler gates by type).
    let type_restrictions: HashMap<i64, PageViewRestriction> =
        if crate::page_type::any_type_view_restriction() {
            let type_rows: Vec<crate::PageType> = crate::PageType::objects().fetch(pool).await?;
            type_rows
                .into_iter()
                .filter_map(|pt| {
                    let tid = pt.id.get().copied()?;
                    type_restriction_for_type(tid, &pt.type_name).map(|r| (tid, r))
                })
                .collect()
        } else {
            HashMap::new()
        };

    let gated = !restricted.is_empty() || !type_restrictions.is_empty();
    let facts = viewer_facts(pool, viewer, gated).await;

    Ok(AccessContext {
        is_authed: facts.is_authed,
        is_super: facts.is_super,
        perm_set: facts.perm_set,
        role_ids: facts.role_ids,
        restricted,
        type_restrictions,
    })
}

/// A restriction nobody but a superuser satisfies: `groups` with no
/// groups. Stands in for "unknown" when the real rows can't be read.
fn deny_restriction(page_id: i64) -> PageViewRestriction {
    PageViewRestriction {
        id: Auto::Unset,
        page_id,
        kind: RestrictionKind::Groups.as_str().to_owned(),
        password_hash: String::new(),
        group_ids: serde_json::json!([]),
        codenames: serde_json::json!([]),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    }
}

/// [`effective_restrictions_for`] when the restrictions can't be read:
/// every page counts as restricted (#757).
fn deny_every_page(
    pages: &[(i64, String)],
    e: &rustango::sql::ExecError,
) -> std::collections::HashMap<i64, PageViewRestriction> {
    tracing::warn!(
        target: "rustango_cms::view_restriction",
        error = %e,
        "restriction lookup failed; treating every listed page as restricted"
    );
    pages.iter().map(|(id, _)| (*id, deny_restriction(*id))).collect()
}

/// Given a batch of pages and a viewer, return the subset of page ids
/// the viewer may **not** access — for viewer-aware menu hiding shared
/// by `auto_menu` and the curated navigation resolver. `pages` are
/// `(page_id, materialized_path, page_type_id)` triples. Thin wrapper
/// over [`access_context`] + [`AccessContext::can_view`].
pub async fn denied_page_ids(
    pool: &rustango::sql::Pool,
    viewer: Option<&rustango::tenancy::auth::User>,
    pages: &[(i64, String, i64)],
) -> std::collections::HashSet<i64> {
    let mut denied = std::collections::HashSet::new();
    if pages.is_empty() {
        return denied;
    }
    let ctx = access_context(pool, viewer).await;
    if ctx.is_unrestricted() {
        return denied;
    }
    for (id, path, type_id) in pages {
        if !ctx.can_view(path, *type_id) {
            denied.insert(*id);
        }
    }
    denied
}

/// The set of role ids a member/admin holds (for menu-hiding a legacy
/// `groups` restriction).
async fn viewer_role_ids(pool: &rustango::sql::Pool, uid: i64) -> std::collections::HashSet<i64> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<rustango::tenancy::permissions::UserRole> =
        rustango::tenancy::permissions::UserRole::objects()
            .where_(rustango::tenancy::permissions::UserRole::user_id.eq(uid))
            .fetch(pool)
            .await
            .unwrap_or_default();
    rows.into_iter().map(|r| r.role_id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn restriction(
        kind: RestrictionKind,
        codenames: &[&str],
        groups: &[i64],
    ) -> PageViewRestriction {
        use rustango::sql::Auto;
        PageViewRestriction {
            id: Auto::Unset,
            page_id: 1,
            kind: kind.as_str().to_owned(),
            password_hash: String::new(),
            group_ids: serde_json::json!(groups),
            codenames: serde_json::json!(codenames),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        }
    }

    #[test]
    fn kind_roundtrips_including_permission() {
        for k in [
            RestrictionKind::Password,
            RestrictionKind::Login,
            RestrictionKind::Groups,
            RestrictionKind::Permission,
        ] {
            assert_eq!(RestrictionKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(RestrictionKind::parse("bogus"), None);
        assert_eq!(RestrictionKind::Permission.as_str(), "permission");
    }

    #[test]
    fn parsed_codenames_reads_strings_only() {
        let r = restriction(RestrictionKind::Permission, &["a", "b"], &[]);
        assert_eq!(r.parsed_codenames(), vec!["a".to_owned(), "b".to_owned()]);
        // A groups restriction (ints in group_ids) yields no codenames.
        let g = restriction(RestrictionKind::Groups, &[], &[3, 4]);
        assert!(g.parsed_codenames().is_empty());
        assert_eq!(g.parsed_groups(), vec![3, 4]);
    }

    #[test]
    fn viewer_satisfies_login_needs_auth() {
        let r = restriction(RestrictionKind::Login, &[], &[]);
        let (p, ro) = (HashSet::new(), HashSet::new());
        assert!(!viewer_satisfies(&r, false, false, &p, &ro));
        assert!(viewer_satisfies(&r, true, false, &p, &ro));
    }

    #[test]
    fn viewer_satisfies_permission_needs_codename_or_super() {
        let r = restriction(RestrictionKind::Permission, &["members.pro"], &[]);
        let ro = HashSet::new();
        // authed but lacks the codename → denied
        assert!(!viewer_satisfies(&r, true, false, &HashSet::new(), &ro));
        // holds the codename → allowed
        let mut p = HashSet::new();
        p.insert("members.pro".to_owned());
        assert!(viewer_satisfies(&r, true, false, &p, &ro));
        // superuser bypass, even anonymous perm set
        assert!(viewer_satisfies(&r, false, true, &HashSet::new(), &ro));
    }

    #[test]
    fn viewer_satisfies_groups_needs_role_or_super() {
        let r = restriction(RestrictionKind::Groups, &[], &[7]);
        let p = HashSet::new();
        assert!(!viewer_satisfies(&r, true, false, &p, &HashSet::new()));
        let mut roles = HashSet::new();
        roles.insert(7);
        assert!(viewer_satisfies(&r, true, false, &p, &roles));
    }

    #[test]
    fn viewer_satisfies_password_stays_visible() {
        // Password is identity-orthogonal — menu items stay visible and
        // the guard prompts on click.
        let r = restriction(RestrictionKind::Password, &[], &[]);
        assert!(viewer_satisfies(
            &r,
            false,
            false,
            &HashSet::new(),
            &HashSet::new()
        ));
    }

    #[test]
    fn access_context_can_view_prefix_and_unrestricted() {
        // Subtree gate: the gated page + its descendants are denied to an
        // anonymous viewer; siblings outside the subtree are visible.
        let ctx = AccessContext::deny_prefix_for_test("0001/0002/");
        assert!(!ctx.can_view("0001/0002/", 5)); // the gated page
        assert!(!ctx.can_view("0001/0002/0007/", 5)); // a descendant
        assert!(ctx.can_view("0001/0003/", 5)); // a sibling subtree
        assert!(ctx.can_view("0001/", 4)); // the root
        assert!(!ctx.is_unrestricted());

        let open = AccessContext::unrestricted_for_test();
        assert!(open.is_unrestricted());
        assert!(open.can_view("0001/0002/", 5));
    }

    #[test]
    fn type_view_restriction_constructors() {
        let l = TypeViewRestriction::login();
        assert_eq!(l.kind, RestrictionKind::Login);
        assert!(l.codenames.is_empty());
        let p = TypeViewRestriction::permission(["members.pro", "members.access"]);
        assert_eq!(p.kind, RestrictionKind::Permission);
        assert_eq!(
            p.codenames,
            vec!["members.pro".to_owned(), "members.access".to_owned()]
        );
    }
}
