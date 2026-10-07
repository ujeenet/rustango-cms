//! Public-renderer enforcement for `PageViewRestriction`.
//!
//! Sits between [`resolve_path`] and [`render`] in the public
//! router. When an effective restriction is found on the requested
//! page (direct or inherited from an ancestor), this module decides
//! whether to allow, redirect, or short-circuit with a prompt.
//!
//! Four restriction kinds:
//!   * `login` — anonymous → 303 to `/members/login?next=<url>`.
//!     Logged-in visitor passes regardless of role.
//!   * `groups` — anonymous → 303 to `/members/login`. Logged-in but
//!     not in any allowed role → 403. Logged-in + matching role → pass.
//!   * `permission` — anonymous → 303 to `/members/login`. Logged-in
//!     but holding none of the allowed permission codenames → 403.
//!     Holding any (or superuser) → pass. Uses the framework permission
//!     engine (`has_any_perm_pool`), not raw role matching.
//!   * `password` — request carries a signed cookie
//!     `rcms_view_grant_<page_id>=<issued_at>:<hmac>`. Missing / invalid →
//!     303 to `/__cms-view-password?page=<id>&next=<url>`. Valid →
//!     pass.
//!
//! [`resolve_path`]: crate::resolver::resolve_path
//! [`render`]: crate::render::render

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use rustango::extractors::Tenant;

use crate::page::Page;
use crate::view_restriction::{self, RestrictionKind};

/// Decide whether `viewer` may see `page`. Returns `None` when the
/// request is allowed through (no restriction OR restriction
/// satisfied); returns `Some(response)` when the request must be
/// redirected or rejected.
///
/// Failures during restriction lookup log + allow — restrictions
/// are a *defence-in-depth* feature, not the primary access
/// control. A DB hiccup shouldn't take a public site offline.
/// Why a request was refused, before anyone decides how to say so.
///
/// The HTML path answers a denial with a redirect to a login or password
/// prompt; the JSON API answers with a status code. Both need the same
/// decision, so [`decide`] computes it and each caller renders it — see
/// [`enforce`] for the HTML rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// Anonymous, and signing in would help.
    NeedsLogin,
    /// Signed in, but not in an allowed group / without an allowed
    /// permission. Signing in again will not help.
    Forbidden,
    /// A `password` restriction with no valid grant cookie.
    NeedsPassword { page_id: i64 },
}

/// The access decision, with no opinion about how to report it.
pub async fn decide(
    tenant: &Tenant,
    page: &Page,
    viewer: Option<&rustango::tenancy::auth::User>,
    headers: &HeaderMap,
) -> Option<Denial> {
    let restriction = match view_restriction::effective_or_type_for_page(tenant.pool(), page).await
    {
        Ok(opt) => opt?,
        Err(e) => {
            // Fail closed (#757): a pool timeout under load must not serve a
            // restricted page to anonymous visitors.
            tracing::warn!(
                target: "rustango_cms::view_restriction",
                error = %e,
                page_id = ?page.id.get().copied(),
                "restriction lookup failed; denying request"
            );
            return Some(Denial::Forbidden);
        }
    };
    // An unrecognised kind is a malformed row, not an absent restriction.
    let Some(kind) = restriction.parsed_kind() else {
        return Some(Denial::Forbidden);
    };
    match kind {
        RestrictionKind::Login => {
            if viewer.is_some() {
                None
            } else {
                Some(Denial::NeedsLogin)
            }
        }
        RestrictionKind::Groups => {
            let allowed = restriction.parsed_groups();
            match viewer {
                None => Some(Denial::NeedsLogin),
                Some(u) => {
                    if u.is_superuser {
                        return None;
                    }
                    let uid = u.id.get().copied().unwrap_or_default();
                    if user_in_any_role(tenant, uid, &allowed).await {
                        None
                    } else {
                        Some(Denial::Forbidden)
                    }
                }
            }
        }
        RestrictionKind::Permission => {
            let allowed = restriction.parsed_codenames();
            match viewer {
                None => Some(Denial::NeedsLogin),
                Some(u) => {
                    let uid = u.id.get().copied().unwrap_or_default();
                    // `has_any_perm_pool` bakes in superuser bypass +
                    // per-user grant/deny, so no separate `is_superuser`
                    // short-circuit is needed here (unlike `Groups`).
                    if viewer_has_any_codename(tenant.pool(), uid, &allowed).await {
                        None
                    } else {
                        Some(Denial::Forbidden)
                    }
                }
            }
        }
        RestrictionKind::Password => {
            // The page that carries the password — an ancestor when it is
            // inherited — is the one the prompt unlocks and the grant names.
            let page_id = restriction.page_id;
            if has_valid_password_grant(headers, &tenant.org.slug, &restriction, page_id, now_secs()) {
                None
            } else {
                Some(Denial::NeedsPassword { page_id })
            }
        }
    }
}

/// Render a [`Denial`] the way a browser expects: a redirect to the
/// member login or the password prompt, or a plain 403.
pub async fn enforce(
    tenant: &Tenant,
    page: &Page,
    viewer: Option<&rustango::tenancy::auth::User>,
    headers: &HeaderMap,
    raw_path: &str,
) -> Option<Response> {
    Some(match decide(tenant, page, viewer, headers).await? {
        Denial::NeedsLogin => login_redirect(raw_path),
        Denial::Forbidden => {
            (StatusCode::FORBIDDEN, "You don't have access to this page.").into_response()
        }
        Denial::NeedsPassword { page_id } => password_prompt_redirect(page_id, raw_path),
    })
}

/// True when `user_id` holds at least one of `codenames`, resolved
/// through the framework permission engine
/// ([`rustango::tenancy::has_any_perm_pool`]) — superuser bypass and
/// per-user grant/deny overrides are handled inside the engine. Empty
/// codename list denies (a permission restriction that names nothing
/// admits no one but a superuser).
pub(crate) async fn viewer_has_any_codename(
    pool: &rustango::sql::Pool,
    user_id: i64,
    codenames: &[String],
) -> bool {
    if codenames.is_empty() {
        return false;
    }
    let refs: Vec<&str> = codenames.iter().map(String::as_str).collect();
    rustango::tenancy::has_any_perm_pool(user_id, &refs, pool)
        .await
        .unwrap_or(false)
}

/// Anonymous visitors hitting a `login` / `groups` / `permission`
/// restriction are sent to the **member** login (the public members
/// area), never the admin `/login`. The member login offers password +
/// SSO/social, all minting the member session the guard reads.
fn login_redirect(raw_path: &str) -> Response {
    let encoded = url_encode(raw_path);
    Redirect::to(&format!("/members/login?next={encoded}")).into_response()
}

fn password_prompt_redirect(page_id: i64, raw_path: &str) -> Response {
    let encoded = url_encode(raw_path);
    Redirect::to(&format!(
        "/__cms-view-password?page={page_id}&next={encoded}"
    ))
    .into_response()
}

/// Manual application/x-www-form-urlencoded encoding of a path. We
/// avoid pulling in `url::form_urlencoded` since the framework
/// already vends a CSRF helper that doesn't expose its encoder
/// surface. ASCII-only; non-ASCII bytes get pct-encoded.
pub fn url_encode_path(s: &str) -> String {
    url_encode(s)
}
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

async fn user_in_any_role(tenant: &Tenant, user_id: i64, role_ids: &[i64]) -> bool {
    if role_ids.is_empty() {
        return false;
    }
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<rustango::tenancy::permissions::UserRole> =
        rustango::tenancy::permissions::UserRole::objects()
            .where_(rustango::tenancy::permissions::UserRole::user_id.eq(user_id))
            .where_(
                rustango::tenancy::permissions::UserRole::role_id.is_in(role_ids.iter().copied()),
            )
            .fetch(tenant.pool())
            .await
            .unwrap_or_default();
    !rows.is_empty()
}

/// How long a page-password grant stays valid, checked on the server —
/// the cookie's `Max-Age` only binds a well-behaved browser.
pub const GRANT_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// The grant cookie for `page_id`. One per page, so unlocking a second
/// password page doesn't replace the first page's grant.
#[must_use]
pub fn grant_cookie_name(page_id: i64) -> String {
    format!("rcms_view_grant_{page_id}")
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Is there a grant for `page_id` in the request's cookies, signed for
/// this tenant and this password, and issued within [`GRANT_TTL_SECS`]?
/// The value is `<issued_at>:<hex hmac>` — see [`grant_signature`].
fn has_valid_password_grant(
    headers: &HeaderMap,
    tenant_slug: &str,
    restriction: &view_restriction::PageViewRestriction,
    page_id: i64,
    now: i64,
) -> bool {
    let Some(cookie_str) = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let prefix = format!("{}=", grant_cookie_name(page_id));
    cookie_str.split(';').filter_map(|c| c.trim().strip_prefix(&prefix)).any(|value| {
        let Some((issued, hex)) = value.split_once(':') else {
            return false;
        };
        let Ok(issued) = issued.parse::<i64>() else {
            return false;
        };
        let fresh = issued <= now && now - issued <= GRANT_TTL_SECS;
        let expected = grant_signature(tenant_slug, restriction, page_id, issued);
        fresh && crate::signing::constant_time_eq(expected.as_bytes(), hex.as_bytes())
    })
}

/// The signature over a grant. Keyed by the CMS signing secret
/// ([`crate::signing::secret`]), so a leaked database row can't mint
/// grants. The message carries the tenant, the page, a
/// fingerprint of the password hash (changing the password revokes every
/// grant) and the issue time (so a grant expires on the server).
#[must_use]
pub fn grant_signature(
    tenant_slug: &str,
    restriction: &view_restriction::PageViewRestriction,
    page_id: i64,
    issued_at: i64,
) -> String {
    let fingerprint =
        crate::signing::hmac_sha256_hex(b"rcms-grant-fp", restriction.password_hash.as_bytes());
    crate::signing::hmac_sha256_hex(
        crate::signing::secret(),
        format!("view_grant|{tenant_slug}|{page_id}|{fingerprint}|{issued_at}").as_bytes(),
    )
}

/// `Set-Cookie` value granting access to `page_id`, issued now.
#[must_use]
pub fn grant_cookie(
    tenant_slug: &str,
    restriction: &view_restriction::PageViewRestriction,
    page_id: i64,
) -> String {
    let issued = now_secs();
    format!(
        "{}={issued}:{}; Path=/; Max-Age={GRANT_TTL_SECS}; SameSite=Lax; HttpOnly",
        grant_cookie_name(page_id),
        grant_signature(tenant_slug, restriction, page_id, issued),
    )
}

// HMAC-SHA256 + constant-time compare now live in `crate::signing`
// (shared with the signed-rendition-URL path, #425).

#[cfg(test)]
mod grant_tests {
    use super::*;
    use rustango::sql::Auto;

    fn restriction(hash: &str) -> view_restriction::PageViewRestriction {
        view_restriction::PageViewRestriction {
            id: Auto::Unset,
            page_id: 42,
            kind: "password".into(),
            password_hash: hash.into(),
            group_ids: serde_json::json!([]),
            codenames: serde_json::json!([]),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        }
    }

    fn with_cookie(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::COOKIE, value.parse().expect("header"));
        h
    }

    /// A grant is bound to its tenant, page and password, and
    /// expires on the server.
    #[test]
    fn a_grant_is_scoped_and_expires() {
        let r = restriction("$argon2id$v=19$m=19456,t=2,p=1$abc$def");
        let now = 1_800_000_000;
        let sig = grant_signature("acme", &r, 42, now);
        let ok = with_cookie(&format!("other=1; rcms_view_grant_42={now}:{sig}"));
        assert!(has_valid_password_grant(&ok, "acme", &r, 42, now + 60));

        assert!(!has_valid_password_grant(&ok, "globex", &r, 42, now), "another tenant");
        assert!(!has_valid_password_grant(&ok, "acme", &r, 43, now), "another page");
        assert!(
            !has_valid_password_grant(&ok, "acme", &restriction("$argon2id$new"), 42, now),
            "a new password revokes it"
        );
        assert!(
            !has_valid_password_grant(&ok, "acme", &r, 42, now + GRANT_TTL_SECS + 1),
            "expired on the server"
        );
        let forged = with_cookie(&format!("rcms_view_grant_42={}:{sig}", now + 3600));
        assert!(!has_valid_password_grant(&forged, "acme", &r, 42, now + 3600), "issue time is signed");
    }

    #[test]
    fn grants_for_two_pages_coexist() {
        assert_ne!(grant_cookie_name(1), grant_cookie_name(2));
        let c = grant_cookie("acme", &restriction("h"), 7);
        assert!(c.starts_with("rcms_view_grant_7=") && c.contains("HttpOnly"), "{c}");
    }
}
