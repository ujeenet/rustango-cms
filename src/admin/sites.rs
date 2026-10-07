//! Sites — which hostname serves which root page.
//!
//! Two halves have to agree for a domain to work, and they live in
//! different databases:
//!
//! * the **registry** decides whether the hostname reaches this tenant at
//!   all ([`rustango::tenancy::OrgHost`], shared across tenants);
//! * the tenant's own [`crate::site::Site`] table decides which page of
//!   the tree it serves as that hostname's root.
//!
//! Editing them on separate screens is how you end up with a domain that
//! resolves to the tenant and then 404s, or a mapping to a root nothing
//! can reach. So this screen owns both: adding a hostname writes the
//! registry row and the mapping in one action, and removing it undoes
//! both.
//!
//! The base host ([`rustango::tenancy::Org::host_pattern`]) has no
//! registry row to delete — see the module docs on `org_host` — so it
//! renders without a remove button. Its *root mapping* is still editable,
//! which is how an operator points the original domain at a different
//! root.
//!
//! A hostname is only as trustworthy as whoever proves they own it, and
//! this screen is a tenant permission. So a tenant puts a hostname live
//! itself only under the operator's own domains (`CMS_SITE_HOST_SUFFIXES`);
//! any other is added disabled and an operator enables it from the
//! operator console once ownership is confirmed.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Extension, Form};
use rustango::core::Column as _;
use rustango::extractors::{SessionUser, Tenant, TenantContext};
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::HostError;
use serde::Deserialize;
use tera::Context;

use super::handlers::{add_admin_theme, add_chrome, render_with_csrf};
use super::AdminError;
use crate::page::Page;
use crate::site::Site;

/// Permission required to bind hostnames to pages.
///
/// Deliberately not `cms_page.*`: this changes what the public internet
/// resolves to, which is closer to deployment than to editing copy.
pub const CODENAME: &str = "cms_site.manage";

async fn ensure_access(
    tenant: &Tenant,
    user: Option<&rustango::tenancy::auth::User>,
) -> Option<Response> {
    super::codename_gate(tenant, user, CODENAME).await
}

/// A rejected hostname is user input, not a fault — only a driver failure
/// deserves a 500.
fn host_err(e: HostError) -> AdminError {
    match e {
        HostError::Driver(e) => AdminError::Exec(e),
        other => AdminError::Validation(other.to_string()),
    }
}

/// Parse the chooser's hidden input.
///
/// A cleared chooser posts `""`, which means "no mapping". Parsing here
/// rather than through serde keeps a malformed value a 400 instead of
/// silently collapsing to `0` and unmapping a live domain.
fn parse_page_id(raw: &str) -> Result<i64, AdminError> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(0);
    }
    s.parse()
        .map_err(|_| AdminError::Validation(format!("`{s}` is not a page id")))
}

/// Resolve the page a form posted.
///
/// `0` means "no mapping" — the hostname falls back to the conventional
/// empty-slug root, which is what every site without this feature does.
///
/// **Any** page qualifies, not only a root: the prefix is just that
/// page's `url_path`, so binding a hostname to `/campaigns/spring`
/// works exactly as binding one to a root does. Existence is still checked, because a mapping
/// to a page that was never there is a typo, not a configuration.
async fn resolve_root(pool: &Pool, root_page_id: i64) -> Result<Option<i64>, AdminError> {
    if root_page_id == 0 {
        return Ok(None);
    }
    let page: Option<Page> = Page::objects()
        .where_(Page::id.eq(root_page_id))
        .first(pool)
        .await?;
    if page.is_some() {
        Ok(Some(root_page_id))
    } else {
        Err(AdminError::Validation(format!(
            "no page with id {root_page_id}"
        )))
    }
}

/// Point `hostname` at `root_page_id`, or drop the mapping when `None`.
async fn set_mapping(
    pool: &Pool,
    hostname: &str,
    root_page_id: Option<i64>,
) -> Result<(), AdminError> {
    let existing: Option<Site> = Site::objects()
        .where_(Site::hostname.eq(hostname.to_owned()))
        .first(pool)
        .await?;
    match (existing, root_page_id) {
        (Some(mut row), Some(id)) => {
            row.root_page_id = id;
            row.save_pool(pool).await?;
        }
        (Some(row), None) => {
            row.delete_pool(pool).await?;
        }
        (None, Some(id)) => {
            let mut row = Site {
                id: rustango::sql::Auto::Unset,
                hostname: hostname.to_owned(),
                root_page_id: id,
                created_at: rustango::sql::Auto::Unset,
            };
            row.insert_pool(pool).await?;
        }
        (None, None) => {}
    }
    Ok(())
}

/// `GET /cms-admin/sites`
pub(crate) async fn list(
    State(state): State<super::AdminState>,
    Extension(ctx): Extension<Arc<TenantContext>>,
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    SessionUser(session_user): SessionUser,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let registry = ctx.pools.registry_pool();
    let hosts = rustango::tenancy::list_for_org(&registry, &tenant.org.slug)
        .await
        .map_err(host_err)?;
    let mappings = crate::site::all(tenant.pool()).await?;

    // One query for every mapped page, to label the choosers. The
    // foreign key means a mapping normally resolves; a database
    // restored with constraints off is the case that does not, and
    // those rows have to say so rather than render as unmapped.
    let mapped_ids: Vec<i64> = mappings.iter().map(|m| m.root_page_id).collect();
    let mapped_pages: Vec<Page> = if mapped_ids.is_empty() {
        Vec::new()
    } else {
        Page::objects()
            .where_(Page::id.is_in(mapped_ids))
            .fetch(tenant.pool())
            .await?
    };
    let page_label = |id: i64| -> Option<String> {
        mapped_pages
            .iter()
            .find(|p| p.id.get().copied() == Some(id))
            .map(|p| {
                if p.url_path.is_empty() {
                    p.title.clone()
                } else {
                    format!("{} ({})", p.title, p.url_path)
                }
            })
    };

    let host_rows: Vec<serde_json::Value> = hosts
        .iter()
        .map(|h| {
            let mapped = mappings.iter().find(|m| m.hostname == h.hostname);
            let id = mapped.map_or(0, |m| m.root_page_id);
            serde_json::json!({
                "hostname": h.hostname,
                "is_base": h.is_base,
                "enabled": h.enabled,
                // Off, and not ours to turn on: an operator approves it.
                "needs_approval": !h.enabled && !h.is_base && !self_service_host(&h.hostname),
                "root_page_id": id,
                "root_title": page_label(id),
                "missing": id != 0 && page_label(id).is_none(),
            })
        })
        .collect();

    // Mappings for hostnames the registry does not route here — left over
    // from a host removed elsewhere, or added straight to the database.
    // They do nothing, and invisible dead rows are how a "why is this not
    // working" hour starts.
    let orphan_rows: Vec<serde_json::Value> = mappings
        .iter()
        .filter(|m| !hosts.iter().any(|h| h.hostname == m.hostname))
        .map(|m| {
            serde_json::json!({
                "hostname": m.hostname,
                "root_title": page_label(m.root_page_id),
            })
        })
        .collect();

    let mut c = Context::new();
    add_chrome(&mut c, &tenant, "sites", session_user.as_ref()).await;
    add_admin_theme(&mut c, tenant.pool()).await;
    c.insert("hosts", &host_rows);
    c.insert("orphans", &orphan_rows);
    c.insert("base_host", &tenant.org.host_pattern);
    render_with_csrf(&state, &headers, "rcms_admin/sites.html", &mut c)
}

#[derive(Debug, Deserialize)]
pub(crate) struct SiteForm {
    pub hostname: String,
    /// The page chooser's hidden input — a page id, or `""` for no
    /// mapping (the hostname then serves the conventional root). Taken
    /// as a string because a cleared chooser posts an empty value,
    /// which is not an `i64`; see [`parse_page_id`].
    #[serde(default)]
    pub root_page_id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HostForm {
    pub hostname: String,
}

/// Whether a tenant may put `host` live itself: it is one of the
/// operator's own domains, or a subdomain of one, listed in
/// `CMS_SITE_HOST_SUFFIXES` (comma-separated, e.g. `sites.example.com`).
///
/// Any other hostname could belong to someone else — first claim would
/// route their domain to this tenant — so it is added disabled and
/// waits for an operator to enable it from the operator console.
fn self_service_host(host: &str) -> bool {
    let suffixes = crate::config::var("SITE_HOST_SUFFIXES").unwrap_or_default();
    host_under_suffixes(host, &suffixes)
}

fn host_under_suffixes(host: &str, suffixes: &str) -> bool {
    suffixes
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .any(|s| host == s || host.ends_with(&format!(".{s}")))
}

/// Bind `host` to `org` **disabled**, with [`rustango::tenancy::add_host`]'s
/// checks: refused when any tenant already has it as its base host or an
/// extra one. Disabled from the start, so an unapproved host never routes.
async fn add_pending_host(
    registry: &rustango::sql::Pool,
    org: &rustango::tenancy::Org,
    host: &str,
) -> Result<(), AdminError> {
    use rustango::core::Column as _;
    use rustango::tenancy::{Org, OrgHost};
    let taken = Org::objects()
        .where_(Org::host_pattern.eq(Some(host.to_owned())))
        .first(registry)
        .await?
        .is_some()
        || OrgHost::objects()
            .where_(OrgHost::hostname.eq(host.to_owned()))
            .first(registry)
            .await?
            .is_some();
    if taken {
        return Err(host_err(HostError::Taken(host.to_owned())));
    }
    let mut row = OrgHost {
        id: rustango::sql::Auto::Unset,
        org_id: org.id.get().copied().unwrap_or_default(),
        hostname: host.to_owned(),
        enabled: false,
        created_at: rustango::sql::Auto::Unset,
    };
    // The unique index on `hostname` settles a race with another claim.
    row.insert_pool(registry).await?;
    Ok(())
}

/// `POST /cms-admin/sites/new`
pub(crate) async fn add(
    Extension(ctx): Extension<Arc<TenantContext>>,
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    Form(form): Form<SiteForm>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let host = rustango::tenancy::normalize_hostname(&form.hostname).map_err(host_err)?;
    let root = resolve_root(tenant.pool(), parse_page_id(&form.root_page_id)?).await?;

    let registry = ctx.pools.registry_pool();
    let known = rustango::tenancy::list_for_org(&registry, &tenant.org.slug)
        .await
        .map_err(host_err)?;
    // Already ours — the base host, or one added earlier without a
    // mapping. `add_host` would reject it as taken, which is true but
    // unhelpful: what was asked for is the mapping.
    if !known.iter().any(|h| h.hostname == host) {
        if self_service_host(&host) {
            rustango::tenancy::add_host(&registry, &tenant.org.slug, &host)
                .await
                .map_err(host_err)?;
        } else {
            add_pending_host(&registry, &tenant.org, &host).await?;
        }
    }
    set_mapping(tenant.pool(), &host, root).await?;
    Ok(Redirect::to("/cms-admin/sites").into_response())
}

/// `POST /cms-admin/sites/root`
pub(crate) async fn set_root(
    Extension(ctx): Extension<Arc<TenantContext>>,
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    Form(form): Form<SiteForm>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let host = rustango::tenancy::normalize_hostname(&form.hostname).map_err(host_err)?;

    // Only for hostnames that actually reach this tenant. Without the
    // check one tenant could seed a mapping for another's domain — inert
    // today, but live the moment that domain is ever pointed here.
    let registry = ctx.pools.registry_pool();
    let known = rustango::tenancy::list_for_org(&registry, &tenant.org.slug)
        .await
        .map_err(host_err)?;
    if !known.iter().any(|h| h.hostname == host) {
        return Err(AdminError::Validation(format!(
            "`{host}` is not one of this site's hostnames"
        )));
    }
    let root = resolve_root(tenant.pool(), parse_page_id(&form.root_page_id)?).await?;
    set_mapping(tenant.pool(), &host, root).await?;
    Ok(Redirect::to("/cms-admin/sites").into_response())
}

/// `POST /cms-admin/sites/toggle`
pub(crate) async fn toggle(
    Extension(ctx): Extension<Arc<TenantContext>>,
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    Form(form): Form<HostForm>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let host = rustango::tenancy::normalize_hostname(&form.hostname).map_err(host_err)?;
    let registry = ctx.pools.registry_pool();
    let known = rustango::tenancy::list_for_org(&registry, &tenant.org.slug)
        .await
        .map_err(host_err)?;
    let enabled = known
        .iter()
        .find(|h| h.hostname == host)
        .map(|h| h.enabled)
        .ok_or(HostError::NotFound)
        .map_err(host_err)?;
    // Turning a host off is always the tenant's call; turning one on that
    // is outside the operator's domains is the operator's (#712).
    if !enabled && !self_service_host(&host) {
        return Err(AdminError::Validation(format!(
            "`{host}` is waiting for an operator to confirm this site owns it"
        )));
    }
    rustango::tenancy::set_host_enabled(&registry, &tenant.org.slug, &host, !enabled)
        .await
        .map_err(host_err)?;
    Ok(Redirect::to("/cms-admin/sites").into_response())
}

/// `POST /cms-admin/sites/delete`
pub(crate) async fn remove(
    Extension(ctx): Extension<Arc<TenantContext>>,
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    Form(form): Form<HostForm>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let host = rustango::tenancy::normalize_hostname(&form.hostname).map_err(host_err)?;
    let registry = ctx.pools.registry_pool();
    match rustango::tenancy::remove_host(&registry, &tenant.org.slug, &host).await {
        Ok(()) => {}
        // No registry row: an orphaned mapping. Dropping the local half is
        // exactly what was asked. `IsBaseHost` deliberately still errors —
        // removing the base host would leave the tenant unreachable.
        Err(HostError::NotFound) => {}
        Err(e) => return Err(host_err(e)),
    }
    set_mapping(tenant.pool(), &host, None).await?;
    Ok(Redirect::to("/cms-admin/sites").into_response())
}

#[cfg(test)]
mod tests {
    use super::parse_page_id;

    /// A cleared chooser posts an empty value, and that means "serve the
    /// default root" — not an error.
    #[test]
    fn an_empty_chooser_means_no_mapping() {
        assert_eq!(parse_page_id("").ok(), Some(0));
        assert_eq!(parse_page_id("   ").ok(), Some(0));
    }

    #[test]
    fn a_chosen_page_parses() {
        assert_eq!(parse_page_id("42").ok(), Some(42));
        assert_eq!(parse_page_id(" 42 ").ok(), Some(42));
    }

    /// The hazard this function exists for: a malformed value must not
    /// collapse to `0`, which would silently unmap a live domain.
    #[test]
    fn a_malformed_value_is_refused_rather_than_unmapping() {
        for raw in ["abc", "1.5", "4 2", "--1", "٣"] {
            assert!(
                parse_page_id(raw).is_err(),
                "{raw:?} should not parse as a page id"
            );
        }
    }
}

#[cfg(test)]
mod host_approval_tests {
    use super::host_under_suffixes;

    #[test]
    fn only_the_operators_domains_are_self_service() {
        let ops = "sites.example.com, .cms.example.net";
        assert!(host_under_suffixes("sites.example.com", ops));
        assert!(host_under_suffixes("acme.sites.example.com", ops));
        assert!(host_under_suffixes("a.b.cms.example.net", ops));
        assert!(!host_under_suffixes("shop.c-corp.com", ops), "someone else's domain");
        assert!(!host_under_suffixes("evilsites.example.com", ops), "not a subdomain");
        assert!(!host_under_suffixes("acme.sites.example.com", ""), "nothing is by default");
    }
}
