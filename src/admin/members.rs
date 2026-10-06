//! Public **members area** authentication (#members).
//!
//! A member is an end-user of the public site — a `rustango_users` row
//! distinguished from an admin only by the cookie that authenticates it
//! (`rustango_member_session` vs the admin `rustango_tenant_session`).
//! Members sign in through **any** method — email/password (this
//! module), or SSO / social (the framework's [`member_sso_router`],
//! mounted by the host at `/members/auth`). Every method mints the same
//! member session via [`member_auth::mint_cookie`], so the page-gating
//! guard ([`crate::view_restriction_guard`]) treats them uniformly.
//!
//! These routes live in [`super::public_router`] (unauthenticated, but
//! merged into the host `api` router so their POSTs are CSRF-protected
//! like every other admin form) and render through the shared admin
//! Tera with [`super::handlers::render_with_csrf`].
//!
//! [`member_sso_router`]: rustango::tenancy::member_auth::member_sso_router
//! [`member_auth::mint_cookie`]: rustango::tenancy::member_auth::mint_cookie

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Form, Query, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Extension;
use tera::Context;

use rustango::extractors::{Tenant, TenantContext};
use rustango::tenancy::auth::User;
use rustango::tenancy::member_auth::{self, CurrentMember};

use super::handlers::{add_branding_urls, render_with_csrf};
use super::{AdminError, AdminState};

/// Member session lifetime — 7 days, matching the framework's
/// [`rustango::tenancy::member_auth::MemberAuthConfig`] default.
pub(crate) const MEMBER_SESSION_TTL: i64 = 7 * 24 * 60 * 60;

/// SSO buttons on the member login list the tenant's enabled providers
/// pointing at the member callback base (where the host mounts
/// [`member_sso_router`](rustango::tenancy::member_auth::member_sso_router)).
const MEMBER_SSO_BASE: &str = "/members/auth";

// ===================================================================
// Login (email/username + password)
// ===================================================================

#[derive(serde::Deserialize)]
pub(crate) struct MemberLoginForm {
    #[serde(default)]
    pub identifier: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub next: String,
}

/// GET `/members/login` — render the member sign-in page (password form
/// + one button per enabled SSO/social provider). An already-signed-in
/// member is bounced straight to the landing / `?next` destination.
pub(crate) async fn member_login_form(
    State(state): State<AdminState>,
    headers: HeaderMap,
    member: CurrentMember,
    tenant: Tenant,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let next = sanitize_next(q.get("next").map(String::as_str).unwrap_or("/"));
    if member.0.is_some() {
        return Ok(Redirect::to(&next).into_response());
    }
    let mut ctx = Context::new();
    ctx.insert("action", "/members/login");
    ctx.insert("signup_url", "/members/signup");
    ctx.insert("next", &next);
    if let Some(err) = q.get("error") {
        ctx.insert("error", err);
    }
    if let Some(err) = q.get("sso_error") {
        ctx.insert("sso_error", err);
    }
    let sso_providers =
        rustango::admin::sso_provider::list_enabled(tenant.pool(), MEMBER_SSO_BASE).await;
    ctx.insert("sso_enabled", &!sso_providers.is_empty());
    ctx.insert("sso_providers", &sso_providers);
    add_branding_urls(&mut ctx, &tenant).await;
    render_with_csrf(&state, &headers, "rcms_admin/member_login.html", &mut ctx)
}

/// POST `/members/login` — verify credentials against `rustango_users`
/// (by username OR email) and mint the member session cookie. Failures
/// bounce back to the form with a generic error (never leaking whether
/// the account exists).
pub(crate) async fn member_login_submit(
    Extension(ctx): Extension<Arc<TenantContext>>,
    tenant: Tenant,
    Form(form): Form<MemberLoginForm>,
) -> Response {
    let next = sanitize_next(&form.next);
    match authenticate_member(tenant.pool(), &form.identifier, &form.password).await {
        Some(user) => {
            let cookie = member_auth::mint_cookie(
                &ctx.session_secret,
                &user,
                &tenant.org.slug,
                MEMBER_SESSION_TTL,
            );
            redirect_with_cookie(&next, &cookie)
        }
        None => Redirect::to(&format!(
            "/members/login?error=invalid_credentials&next={}",
            crate::view_restriction_guard::url_encode_path(&next)
        ))
        .into_response(),
    }
}

// ===================================================================
// Signup (self-service email/password registration)
// ===================================================================

#[derive(serde::Deserialize)]
pub(crate) struct MemberSignupForm {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub password_confirm: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub next: String,
}

/// GET `/members/signup` — render the self-service registration form.
pub(crate) async fn member_signup_form(
    State(state): State<AdminState>,
    headers: HeaderMap,
    member: CurrentMember,
    tenant: Tenant,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let next = sanitize_next(q.get("next").map(String::as_str).unwrap_or("/"));
    if member.0.is_some() {
        return Ok(Redirect::to(&next).into_response());
    }
    let mut ctx = Context::new();
    ctx.insert("action", "/members/signup");
    ctx.insert("login_url", "/members/login");
    ctx.insert("next", &next);
    if let Some(err) = q.get("error") {
        ctx.insert("error", err);
    }
    add_branding_urls(&mut ctx, &tenant).await;
    render_with_csrf(&state, &headers, "rcms_admin/member_signup.html", &mut ctx)
}

/// POST `/members/signup` — validate, create a new member
/// (`is_superuser = false`, no tier — an admin assigns roles later),
/// and sign them straight in with a member cookie.
pub(crate) async fn member_signup_submit(
    Extension(ctx): Extension<Arc<TenantContext>>,
    tenant: Tenant,
    Form(form): Form<MemberSignupForm>,
) -> Response {
    let next = sanitize_next(&form.next);
    let email = form.email.trim().to_ascii_lowercase();

    let bounce = |reason: &str| {
        Redirect::to(&format!(
            "/members/signup?error={reason}&next={}",
            crate::view_restriction_guard::url_encode_path(&next)
        ))
        .into_response()
    };

    if email.is_empty() || !email.contains('@') {
        return bounce("invalid_email");
    }
    if form.password.len() < 8 {
        return bounce("weak_password");
    }
    if form.password != form.password_confirm {
        return bounce("password_mismatch");
    }
    match email_taken(tenant.pool(), &email).await {
        Ok(true) => return bounce("email_taken"),
        Ok(false) => {}
        Err(_) => return bounce("server_error"),
    }

    let display_name = if form.display_name.trim().is_empty() {
        email.split('@').next().unwrap_or(&email).to_owned()
    } else {
        form.display_name.trim().to_owned()
    };

    match create_member(tenant.pool(), &email, &form.password, &display_name).await {
        Ok(user) => {
            let cookie = member_auth::mint_cookie(
                &ctx.session_secret,
                &user,
                &tenant.org.slug,
                MEMBER_SESSION_TTL,
            );
            redirect_with_cookie(&next, &cookie)
        }
        Err(e) => {
            tracing::error!(target: "rustango_cms::members", error = %e, "member signup failed");
            bounce("server_error")
        }
    }
}

/// POST `/members/logout` — expire the member session and return to
/// the site root.
pub(crate) async fn member_logout() -> Response {
    redirect_with_cookie("/", &member_auth::clear_cookie())
}

// ===================================================================
// Helpers
// ===================================================================

/// Authenticate a member by username OR (lowercased) email + password.
/// Runs a dummy verify on the unknown-account branch so response timing
/// doesn't reveal whether the identifier exists.
pub(crate) async fn authenticate_member(
    pool: &rustango::sql::Pool,
    identifier: &str,
    password: &str,
) -> Option<User> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let ident = identifier.trim();
    if ident.is_empty() || password.is_empty() {
        return None;
    }
    let mut rows: Vec<User> = User::objects()
        .where_(User::username.eq(ident.to_owned()))
        .limit(1)
        .fetch(pool)
        .await
        .unwrap_or_default();
    if rows.is_empty() {
        rows = User::objects()
            .filter("email", ident.to_ascii_lowercase())
            .limit(1)
            .fetch(pool)
            .await
            .unwrap_or_default();
    }
    let Some(user) = rows.into_iter().next() else {
        crate::passwords::verify_dummy(password).await;
        return None;
    };
    // Verify before checking `active` (#770): returning at once for a
    // disabled account made its response measurably faster than an active
    // one's or an unknown one's (which pays `verify_dummy`), so timing told
    // an attacker the account exists but is disabled.
    let matches = matches!(crate::passwords::verify(password, &user.password_hash).await, Ok(true));
    (matches && user.active).then_some(user)
}

/// True when a `rustango_users` row already carries this (lowercased)
/// email.
async fn email_taken(pool: &rustango::sql::Pool, email: &str) -> Result<bool, AdminError> {
    use rustango::sql::FetcherPool as _;
    let rows: Vec<User> = User::objects()
        .filter("email", email.to_owned())
        .limit(1)
        .fetch(pool)
        .await?;
    Ok(!rows.is_empty())
}

/// Insert a new member row (username = email; unusable-until-set tiers
/// are granted later by an admin). Returns the new user id.
async fn create_member(
    pool: &rustango::sql::Pool,
    email: &str,
    password: &str,
    display_name: &str,
) -> Result<User, AdminError> {
    use rustango::sql::Auto;
    let mut user = User {
        id: Auto::Unset,
        username: email.to_owned(),
        password_hash: crate::passwords::hash(password).await
            .map_err(|e| AdminError::Validation(format!("password hashing failed: {e}")))?,
        email: Some(email.to_owned()),
        is_superuser: false,
        active: true,
        created_at: chrono::Utc::now(),
        data: serde_json::json!({ "display_name": display_name }),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    user.insert_pool(pool).await?;
    if user.id.get().is_none() {
        return Err(AdminError::Validation("member insert returned no id".to_owned()));
    }
    Ok(user)
}

/// Reject open-redirect-shaped `next` values: only a single-slash,
/// same-origin path is honored, everything else falls back to `/`.
fn sanitize_next(next: &str) -> String {
    rustango::auth_decorators::safe_next(next).unwrap_or_else(|| "/".to_owned())
}

/// A 303 redirect that also stamps a `Set-Cookie` header.
fn redirect_with_cookie(location: &str, set_cookie: &str) -> Response {
    let mut resp = Redirect::to(location).into_response();
    if let Ok(v) = HeaderValue::from_str(set_cookie) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::sanitize_next;

    #[test]
    fn next_stays_on_site() {
        assert_eq!(sanitize_next("/members/area?x=1"), "/members/area?x=1");
        for hostile in ["https://evil.test", "//evil.test", "/\\evil.test", "/%5Cevil.test", "/%2F/evil.test", "/\t/evil.test"] {
            assert_eq!(sanitize_next(hostile), "/", "{hostile:?}");
        }
    }
}
