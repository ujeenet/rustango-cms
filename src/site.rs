//! Sites — one tenant, several hostnames, each with its own root page.
//!
//! The framework's registry answers "which tenant owns this hostname"
//! ([`rustango::tenancy::OrgHost`]). This answers the next question: once
//! we are inside the tenant, **which part of the page tree** does that
//! hostname serve?
//!
//! ## How a request finds its page
//!
//! Without a site mapping the resolver anchors on the conventional
//! empty-slug root, so anything else is reachable only under its own
//! path — `/site-b/about`. Mapping a hostname to that page makes it
//! answer at `siteb.com/about` instead.
//!
//! The mapped page need not be a root. The prefix is only ever its
//! `url_path`, so `/campaigns/spring` binds to a hostname exactly as a
//! root does.
//!
//! The translation is done at the edges rather than in the stored data:
//!
//! * on the way **in**, the request path is prefixed with the site root's
//!   `url_path` before lookup;
//! * on the way **out**, that prefix is stripped from generated links.
//!
//! `url_path` therefore stays tenant-absolute and `tree_ops` is untouched.
//! The alternative — storing site-relative paths — means moving a page
//! between sites rewrites every descendant's URL, and two roots would be
//! free to collide on `/about` with nothing to tell them apart.

use chrono::{DateTime, Utc};
use rustango::core::Column as _;
use rustango::sql::{Auto, ExecError, FetcherPool as _, Pool};
use rustango::Model;
use serde::{Deserialize, Serialize};

use crate::page::Page;

/// A hostname bound to a root page within this tenant.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_site",
    app = "cms",
    display = "hostname",
    admin(
        list_display = "hostname, root_page_id, created_at",
        ordering = "hostname"
    )
)]
pub struct Site {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Lowercase bare host, matching what the registry stores. Unique
    /// within the tenant — one hostname cannot serve two roots, or the
    /// request would have no defined answer.
    ///
    /// Not unique *globally*: that is the registry's job, and duplicating
    /// the constraint here would let the two disagree.
    #[rustango(max_length = 255, unique, index)]
    pub hostname: String,

    /// The page this hostname serves as its root. Any page qualifies,
    /// not only a tree root. Several hostnames may share one page —
    /// that is how an alias domain works.
    #[rustango(fk = "cms_page", on = "id", index)]
    pub root_page_id: i64,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Prefix a public request path with the site root's path.
///
/// Free function rather than a method so the translation can be tested
/// without constructing a `Page` — the prefix is the only input that
/// matters, and a fixture page would just be noise around it.
#[must_use]
pub fn to_lookup_path(prefix: &str, request_path: &str) -> String {
    if prefix.is_empty() {
        return request_path.to_owned();
    }
    // `/` on a prefixed site means the site root itself.
    if request_path == "/" || request_path.is_empty() {
        return prefix.to_owned();
    }
    format!("{prefix}{request_path}")
}

/// Strip the site root's prefix from a tenant-absolute `url_path`.
///
/// A page outside this site keeps its absolute path: it is genuinely not
/// reachable here, and silently rewriting it would produce a link that
/// 404s rather than one that obviously points elsewhere.
#[must_use]
pub fn to_public_path(prefix: &str, url_path: &str) -> String {
    if prefix.is_empty() {
        return url_path.to_owned();
    }
    match url_path.strip_prefix(prefix) {
        Some("") => "/".to_owned(),
        Some(rest) if rest.starts_with('/') => rest.to_owned(),
        // `"/site-bxyz"` starts with `"/site-b"` textually but is a
        // different subtree — a prefix match is only real on a boundary.
        _ => url_path.to_owned(),
    }
}

/// The URL prefix `hostname` serves under.
///
/// `""` — the conventional root, and therefore every request on a site
/// that has never mapped a hostname. That is the whole non-breaking
/// story: no mapping, no prefix, paths pass through exactly as before.
/// Otherwise the mapped root's `url_path` with no trailing slash,
/// ready to concatenate: `"/shop"`.
///
/// The unmapped case costs one indexed point lookup and stops there —
/// the conventional root's own `url_path` is `/`, so there is nothing
/// to learn from fetching it.
///
/// A mapping whose page has been deleted also yields `""`. Falling back
/// beats 500ing: the domain stays up, and the admin flags the row.
///
/// # Errors
/// Driver / query failures.
pub async fn prefix_for_host(pool: &Pool, hostname: &str) -> Result<String, ExecError> {
    Ok(root_for_host(pool, hostname)
        .await?
        .map(|root| root.url_path.trim_end_matches('/').to_owned())
        .unwrap_or_default())
}

/// The page `hostname` is mapped to serve, or `None` when it serves the
/// default root (no mapping, or one whose page has been deleted).
///
/// # Errors
/// Driver / query failures.
pub async fn root_for_host(pool: &Pool, hostname: &str) -> Result<Option<Page>, ExecError> {
    let host = hostname.trim().to_ascii_lowercase();
    if host.is_empty() {
        return Ok(None);
    }
    let Some(site): Option<Site> = Site::objects()
        .where_(Site::hostname.eq(host))
        .first(pool)
        .await?
    else {
        return Ok(None);
    };
    let root: Option<Page> = Page::objects()
        .where_(Page::id.eq(site.root_page_id))
        .first(pool)
        .await?;
    if root.is_none() {
        tracing::warn!(
            target: "rustango_cms::site",
            hostname = %hostname, root_page_id = site.root_page_id,
            "site maps to a missing root page; serving the default root"
        );
    }
    Ok(root)
}

/// The hostnames rooted at `root_page_id`.
///
/// Used to refuse deleting a page a live domain points at. Silently
/// dropping the mapping instead would leave a domain quietly serving a
/// different site, which is the kind of change that is only noticed by
/// visitors.
///
/// # Errors
/// Driver / query failures.
pub async fn hosts_for_root(pool: &Pool, root_page_id: i64) -> Result<Vec<String>, ExecError> {
    let rows: Vec<Site> = Site::objects()
        .where_(Site::root_page_id.eq(root_page_id))
        .order_by(&[("hostname", false)])
        .fetch(pool)
        .await?;
    Ok(rows.into_iter().map(|s| s.hostname).collect())
}

/// Every site mapping, newest hostname order, for the admin.
///
/// # Errors
/// Driver / query failures.
pub async fn all(pool: &Pool) -> Result<Vec<Site>, ExecError> {
    Site::objects()
        .order_by(&[("hostname", false)])
        .fetch(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::{to_lookup_path as lookup, to_public_path as public};

    #[test]
    fn the_default_root_passes_paths_through_untouched() {
        assert_eq!(lookup("", "/about"), "/about");
        assert_eq!(lookup("", "/"), "/");
        assert_eq!(public("", "/about"), "/about");
    }

    #[test]
    fn a_prefixed_site_maps_its_root_to_slash() {
        assert_eq!(lookup("/site-b", "/"), "/site-b");
        assert_eq!(lookup("/site-b", ""), "/site-b");
        assert_eq!(public("/site-b", "/site-b"), "/");
    }

    #[test]
    fn a_prefixed_site_translates_both_ways() {
        assert_eq!(lookup("/site-b", "/about"), "/site-b/about");
        assert_eq!(lookup("/site-b", "/about/team"), "/site-b/about/team");
        assert_eq!(public("/site-b", "/site-b/about"), "/about");
        assert_eq!(public("/site-b", "/site-b/about/team"), "/about/team");
    }

    /// Round-tripping is the property the whole scheme rests on: what a
    /// visitor asks for must come back as the same public link.
    #[test]
    fn lookup_and_public_round_trip() {
        for prefix in ["", "/site-b", "/deep/nested"] {
            for path in ["/", "/about", "/about/team"] {
                let absolute = lookup(prefix, path);
                assert_eq!(
                    public(prefix, &absolute),
                    path,
                    "round trip failed for prefix {prefix:?} path {path:?}"
                );
            }
        }
    }

    /// A textual prefix match is not a subtree match. `/site-bravo` is a
    /// different root that merely starts with the same characters, and
    /// rewriting it to `ravo` would invent a URL that 404s.
    #[test]
    fn a_sibling_root_sharing_a_name_prefix_is_not_rewritten() {
        assert_eq!(public("/site-b", "/site-bravo/about"), "/site-bravo/about");
    }

    /// A page on another site keeps its absolute path — an honest link
    /// somewhere else beats a rewritten one that goes nowhere.
    #[test]
    fn a_page_outside_this_site_keeps_its_absolute_path() {
        assert_eq!(public("/site-b", "/other/page"), "/other/page");
        assert_eq!(public("/site-b", "/"), "/");
    }
}
