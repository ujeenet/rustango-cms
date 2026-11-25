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

use rustango::core::Column as _;
use rustango::extractors::{Tenant, TenantContext, TenantRejection};
use rustango::login_throttle::{self, LoginRefused, LoginScope};
use rustango::tenancy::auth::User;
use rustango::tenancy::member_auth::{self, CurrentMember};

use super::handlers::{add_auth_chrome, render_with_csrf};
use crate::security_log::{self, LoginRequest};
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
    ctx.insert("forgot_url", RESET_REQUEST_PATH);
    ctx.insert("password_reset_done", &(q.get("reset").map(String::as_str) == Some("done")));
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
    add_auth_chrome(&mut ctx, &tenant).await;
    render_with_csrf(&state, &headers, "rcms_admin/member_login.html", &mut ctx)
}

/// POST `/members/login` — verify credentials against `rustango_users`
/// (by username OR email) and mint the member session cookie. Failures
/// bounce back to the form with a generic error (never leaking whether
/// the account exists).
pub(crate) async fn member_login_submit(
    Extension(ctx): Extension<Arc<TenantContext>>,
    tenant: Tenant,
    request: LoginRequest,
    Form(form): Form<MemberLoginForm>,
) -> Response {
    let next = sanitize_next(&form.next);
    // The throttle and the lock-out are bound to the site by org id too, so
    // a later site reusing the slug inherits neither.
    let Some(site) = rustango::tenancy::TenantSlug::of(&tenant.org) else {
        return (axum::http::StatusCode::NOT_FOUND, "unknown site").into_response();
    };
    let outcome = authenticate_member(
        tenant.pool(),
        &site,
        &request,
        "member",
        &form.identifier,
        &form.password,
    )
    .await;
    match outcome {
        Ok(user) => signed_in_redirect(&ctx, &user, &tenant.org, &next),
        Err(e) => {
            let reason = match e {
                MemberLoginError::Invalid => "invalid_credentials",
                MemberLoginError::Refused(_) => "throttled",
            };
            Redirect::to(&format!(
                "/members/login?error={reason}&next={}",
                crate::view_restriction_guard::url_encode_path(&next)
            ))
            .into_response()
        }
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
    add_auth_chrome(&mut ctx, &tenant).await;
    render_with_csrf(&state, &headers, "rcms_admin/member_signup.html", &mut ctx)
}

/// POST `/members/signup` — validate, create a new member
/// (`is_superuser = false`, no tier — an admin assigns roles later),
/// and sign them straight in with a member cookie.
pub(crate) async fn member_signup_submit(
    State(state): State<AdminState>,
    headers: HeaderMap,
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
            // The member is signed in at once; the link only proves the
            // address is theirs.
            send_confirmation(&state, &headers, &tenant, &user).await;
            signed_in_redirect(&ctx, &user, &tenant.org, &next)
        }
        Err(e) => {
            tracing::error!(target: "rustango_cms::members", error = %e, "member signup failed");
            bounce("server_error")
        }
    }
}

/// POST `/members/logout` — expire the member session and return to
/// the site root.
pub(crate) async fn member_logout(
    member: CurrentMember,
    tenant: Result<Tenant, TenantRejection>,
    request: LoginRequest,
) -> Response {
    if let (Some(user), Ok(tenant)) = (member.0, tenant) {
        let entry = security_log::Entry::new(security_log::EVENT_LOGOUT, "member", &request.meta)
            .user(user.id.get().copied(), &user.username);
        security_log::record(tenant.pool(), &tenant.org.slug, entry).await;
    }
    redirect_with_cookie("/", &member_auth::clear_cookie())
}

// ===================================================================
// Email confirmation + password reset
// ===================================================================

/// Where a confirmation link lands.
const VERIFY_PATH: &str = "/members/verify";
/// Where a visitor asks for a new confirmation link.
const VERIFY_RESEND_PATH: &str = "/members/verify/resend";
/// Where a member asks for a password-reset link.
const RESET_REQUEST_PATH: &str = "/members/password-reset";
/// Where a password-reset link lands.
const RESET_CONFIRM_PATH: &str = "/members/password-reset/confirm";

/// How long a confirmation link works.
const VERIFY_TTL: std::time::Duration = std::time::Duration::from_secs(3 * 24 * 60 * 60);
/// How long a password-reset link works. Each is also bound to the current
/// password hash, so it stops working once the password changes.
const RESET_TTL: std::time::Duration = std::time::Duration::from_secs(60 * 60);
/// What signs confirmation links (reset links use the admin reset key).
const VERIFY_KEY_PURPOSE: &str = "rcms-member-email-verify";

/// `rustango_users.data` keys: the address that was confirmed, and when.
/// Keyed by the address, so changing the email makes it unconfirmed again.
const DATA_VERIFIED: &str = "email_verified";
const DATA_VERIFIED_AT: &str = "email_verified_at";

/// Whether `user` has confirmed the email address on their account.
#[must_use]
pub fn email_is_verified(user: &User) -> bool {
    let email = super::handlers::user_email(user);
    !email.is_empty()
        && user.data.get(DATA_VERIFIED).and_then(serde_json::Value::as_str) == Some(email.as_str())
}

/// Record that `user` controls the address on their account.
fn mark_verified(user: &mut User) {
    let email = super::handlers::user_email(user);
    if !user.data.is_object() {
        user.data = serde_json::json!({});
    }
    user.data[DATA_VERIFIED] = serde_json::Value::String(email);
    user.data[DATA_VERIFIED_AT] = serde_json::Value::String(chrono::Utc::now().to_rfc3339());
}

/// Send one member email in the background, so the response does not wait
/// for the mail server, and its timing does not reveal whether an account
/// exists. With no mailer it goes to the console, as admin mail does.
fn spawn_member_mail(state: &AdminState, to: String, subject: String, body: String) {
    let mailer = state.mailer.clone();
    let from = if state.mailer_from.trim().is_empty() {
        "noreply@cms.local".to_owned()
    } else {
        state.mailer_from.as_str().to_owned()
    };
    rustango::__private_runtime::tokio::spawn(async move {
        let email = rustango::email::Email::new()
            .to(&to)
            .from(&from)
            .subject(&subject)
            .body(body);
        let result = match mailer.as_deref() {
            Some(m) => m.send(&email).await,
            None => {
                tracing::warn!(
                    target: "rustango_cms::members",
                    "no mailer configured: printing the member email to the console instead of sending it"
                );
                let console = rustango::email::ConsoleMailer;
                <rustango::email::ConsoleMailer as rustango::email::Mailer>::send(&console, &email).await
            }
        };
        if let Err(e) = result {
            tracing::error!(target: "rustango_cms::members", error = %e, "member email could not be sent");
        }
    });
}

/// The site's name for email text: Settings → Branding, else the tenant.
async fn site_name(tenant: &Tenant) -> String {
    let branding = crate::branding::get(tenant.pool()).await;
    crate::branding::site_name(&branding, &tenant.org)
}

/// Email `user` a link that confirms their address. Nothing is sent for an
/// account without an email.
async fn send_confirmation(state: &AdminState, headers: &HeaderMap, tenant: &Tenant, user: &User) {
    let email = super::handlers::user_email(user);
    let Some(uid) = user.id.get().copied() else { return };
    if email.is_empty() {
        return;
    }
    let link = rustango::auth_flows::EmailVerification::issue(
        &super::handlers::password_reset_scope(&tenant.org),
        VERIFY_PATH,
        uid,
        &email,
        &crate::signing::derived_key(VERIFY_KEY_PURPOSE),
        VERIFY_TTL,
    );
    let link = format!("{}{link}", crate::sitemap::base_url(headers));
    let site = site_name(tenant).await;
    spawn_member_mail(
        state,
        email,
        format!("Confirm your email for {site}"),
        format!(
            "Welcome to {site}!\n\n\
             Please confirm your email address by opening this link within three days:\n\n\
             {link}\n\n\
             If you did not create an account, you can ignore this email.\n"
        ),
    );
}

/// The signed link a request arrived on: `path` plus its query string.
fn signed_link(path: &str, query: Option<&str>) -> String {
    match query {
        Some(q) if !q.is_empty() => format!("{path}?{q}"),
        _ => path.to_owned(),
    }
}

/// Render one of the member mail pages with the auth chrome.
async fn render_member_page(
    state: &AdminState,
    headers: &HeaderMap,
    tenant: &Tenant,
    template: &str,
    mut ctx: Context,
) -> Result<Response, AdminError> {
    add_auth_chrome(&mut ctx, tenant).await;
    render_with_csrf(state, headers, template, &mut ctx)
}

/// GET `/members/verify?…` — confirm the address a signup link was sent to.
pub(crate) async fn member_verify(
    State(state): State<AdminState>,
    headers: HeaderMap,
    tenant: Tenant,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Result<Response, AdminError> {
    let link = signed_link(VERIFY_PATH, query.as_deref());
    let verified = rustango::auth_flows::EmailVerification::verify(
        &super::handlers::password_reset_scope(&tenant.org),
        &link,
        &crate::signing::derived_key(VERIFY_KEY_PURPOSE),
    );
    let mut outcome = "invalid";
    if let Ok((uid, email)) = verified {
        let user = User::objects()
            .where_(User::id.eq(uid))
            .where_(User::active.eq(true))
            .first(tenant.pool())
            .await
            .ok()
            .flatten();
        // A link for an address the account no longer has proves nothing.
        if let Some(mut user) = user.filter(|u| super::handlers::user_email(u) == email) {
            if email_is_verified(&user) {
                // A second click must not read as a fresh confirmation.
                outcome = "already";
            } else {
                mark_verified(&mut user);
                outcome = if user.save(tenant.pool()).await.is_ok() { "verified" } else { "error" };
            }
        }
    }
    let mut ctx = Context::new();
    ctx.insert("outcome", outcome);
    ctx.insert("resend_url", VERIFY_RESEND_PATH);
    render_member_page(&state, &headers, &tenant, "rcms_admin/member_verify.html", ctx).await
}

#[derive(serde::Deserialize)]
pub(crate) struct MemberEmailForm {
    #[serde(default)]
    pub email: String,
}

/// GET `/members/verify/resend` — ask for a new confirmation link.
pub(crate) async fn member_verify_resend_form(
    State(state): State<AdminState>,
    headers: HeaderMap,
    tenant: Tenant,
) -> Result<Response, AdminError> {
    let mut ctx = Context::new();
    ctx.insert("outcome", "resend");
    ctx.insert("resend_url", VERIFY_RESEND_PATH);
    render_member_page(&state, &headers, &tenant, "rcms_admin/member_verify.html", ctx).await
}

/// POST `/members/verify/resend` — send a new confirmation link to an
/// account that has not confirmed yet. The answer is the same whether or not
/// the address belongs to an account.
pub(crate) async fn member_verify_resend_submit(
    State(state): State<AdminState>,
    headers: HeaderMap,
    tenant: Tenant,
    Form(form): Form<MemberEmailForm>,
) -> Result<Response, AdminError> {
    if let Some(user) = find_member(tenant.pool(), form.email.trim()).await.filter(|u| u.active) {
        if !email_is_verified(&user) {
            send_confirmation(&state, &headers, &tenant, &user).await;
        }
    }
    let mut ctx = Context::new();
    ctx.insert("outcome", "resent");
    ctx.insert("resend_url", VERIFY_RESEND_PATH);
    render_member_page(&state, &headers, &tenant, "rcms_admin/member_verify.html", ctx).await
}

/// GET `/members/password-reset` — the "forgot password" form.
pub(crate) async fn member_reset_request_form(
    State(state): State<AdminState>,
    headers: HeaderMap,
    tenant: Tenant,
) -> Result<Response, AdminError> {
    let mut ctx = Context::new();
    ctx.insert("action", RESET_REQUEST_PATH);
    ctx.insert("login_url", "/members/login");
    render_member_page(&state, &headers, &tenant, "rcms_admin/member_password_reset.html", ctx).await
}

/// POST `/members/password-reset` — email a reset link. The answer is the
/// same whether or not the address belongs to an account.
pub(crate) async fn member_reset_request_submit(
    State(state): State<AdminState>,
    headers: HeaderMap,
    tenant: Tenant,
    request: LoginRequest,
    Form(form): Form<MemberEmailForm>,
) -> Result<Response, AdminError> {
    if let Some(user) = find_member(tenant.pool(), form.email.trim()).await.filter(|u| u.active) {
        let email = super::handlers::user_email(&user);
        if let (Some(uid), false) = (user.id.get().copied(), email.is_empty()) {
            let entry = security_log::Entry::new(
                security_log::EVENT_PASSWORD_RESET_REQUESTED,
                "member",
                &request.meta,
            )
            .user(Some(uid), &user.username);
            security_log::record(tenant.pool(), &tenant.org.slug, entry).await;
            let key = super::handlers::password_reset_key(
                state.signing_secret.as_slice(),
                &tenant.org.slug,
                &user.password_hash,
            );
            let link = rustango::auth_flows::PasswordReset::issue(
                &super::handlers::password_reset_scope(&tenant.org),
                RESET_CONFIRM_PATH,
                uid,
                &key,
                RESET_TTL,
            );
            let link = format!("{}{link}", crate::sitemap::base_url(&headers));
            let site = site_name(&tenant).await;
            spawn_member_mail(
                &state,
                email,
                format!("Reset your password for {site}"),
                format!(
                    "We received a request to reset your password for {site}.\n\n\
                     Open this link within the next hour to choose a new one:\n\n\
                     {link}\n\n\
                     If you did not ask for this, you can ignore this email; your password stays the same.\n"
                ),
            );
        }
    }
    let mut ctx = Context::new();
    ctx.insert("sent", &true);
    ctx.insert("action", RESET_REQUEST_PATH);
    ctx.insert("login_url", "/members/login");
    render_member_page(&state, &headers, &tenant, "rcms_admin/member_password_reset.html", ctx).await
}

/// GET `/members/password-reset/confirm?…` — the "choose a new password"
/// form, or an "expired link" page.
pub(crate) async fn member_reset_confirm_form(
    State(state): State<AdminState>,
    headers: HeaderMap,
    tenant: Tenant,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Result<Response, AdminError> {
    let link = signed_link(RESET_CONFIRM_PATH, query.as_deref());
    let valid = super::handlers::password_reset_user(&tenant, state.signing_secret.as_slice(), &link)
        .await
        .is_some();
    let mut ctx = Context::new();
    ctx.insert("action", RESET_CONFIRM_PATH);
    ctx.insert("forgot_url", RESET_REQUEST_PATH);
    ctx.insert("valid", &valid);
    if valid {
        ctx.insert("signed_token", &link);
    }
    render_member_page(&state, &headers, &tenant, "rcms_admin/member_password_reset_confirm.html", ctx).await
}

#[derive(serde::Deserialize)]
pub(crate) struct MemberResetForm {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub password_confirm: String,
}

/// POST `/members/password-reset/confirm` — set the new password, then send
/// the member to sign in with it.
pub(crate) async fn member_reset_confirm_submit(
    State(state): State<AdminState>,
    headers: HeaderMap,
    tenant: Tenant,
    request: LoginRequest,
    Form(form): Form<MemberResetForm>,
) -> Result<Response, AdminError> {
    let again = |error: &str| {
        let mut ctx = Context::new();
        ctx.insert("action", RESET_CONFIRM_PATH);
        ctx.insert("forgot_url", RESET_REQUEST_PATH);
        ctx.insert("valid", &true);
        ctx.insert("signed_token", &form.token);
        ctx.insert("error", error);
        ctx
    };
    // The same rule as sign-up.
    if form.password.len() < 8 {
        return render_member_page(&state, &headers, &tenant, "rcms_admin/member_password_reset_confirm.html", again("weak_password")).await;
    }
    if form.password != form.password_confirm {
        return render_member_page(&state, &headers, &tenant, "rcms_admin/member_password_reset_confirm.html", again("password_mismatch")).await;
    }
    let Some(mut user) =
        super::handlers::password_reset_user(&tenant, state.signing_secret.as_slice(), &form.token).await
    else {
        let mut ctx = Context::new();
        ctx.insert("action", RESET_CONFIRM_PATH);
        ctx.insert("forgot_url", RESET_REQUEST_PATH);
        ctx.insert("valid", &false);
        return render_member_page(&state, &headers, &tenant, "rcms_admin/member_password_reset_confirm.html", ctx).await;
    };
    let Ok(hash) = crate::passwords::hash(&form.password).await else {
        return render_member_page(&state, &headers, &tenant, "rcms_admin/member_password_reset_confirm.html", again("server_error")).await;
    };
    user.password_hash = hash;
    user.password_changed_at = Some(chrono::Utc::now());
    // The link arrived by email, so the address is the member's.
    mark_verified(&mut user);
    if user.save(tenant.pool()).await.is_err() {
        return render_member_page(&state, &headers, &tenant, "rcms_admin/member_password_reset_confirm.html", again("server_error")).await;
    }
    // Proving control of the account's mail lifts a lock a guesser set.
    if let Some(site) = rustango::tenancy::TenantSlug::of(&tenant.org) {
        security_log::unlock(&site, &user.username).await;
    }
    let entry = security_log::Entry::new(security_log::EVENT_PASSWORD_RESET, "member", &request.meta)
        .user(user.id.get().copied(), &user.username);
    security_log::record(tenant.pool(), &tenant.org.slug, entry).await;
    Ok(Redirect::to("/members/login?reset=done").into_response())
}

// ===================================================================
// Helpers
// ===================================================================

/// Why a member sign-in was refused.
#[derive(Debug)]
pub enum MemberLoginError {
    /// Too many attempts at this account or from this address, or the
    /// password-hashing queue is full. The password was not checked.
    Refused(LoginRefused),
    /// Unknown name, wrong password, or a disabled account — one answer
    /// for all three, so the response tells nothing about the account.
    Invalid,
}

/// Sign a member in by username OR (lowercased) email + password, in
/// `tenant_slug` (the tenant `pool` belongs to).
///
/// The attempt goes through the framework's login gate in the tenant
/// scope, so the account shares one lock with the admin `/login`, and is
/// written to the [`security_log`] as `source`. Runs a dummy verify on
/// the unknown-account branch so response timing doesn't reveal whether
/// the identifier exists.
///
/// # Errors
/// [`MemberLoginError`].
pub async fn authenticate_member(
    pool: &rustango::sql::Pool,
    site: &rustango::tenancy::TenantSlug,
    request: &LoginRequest,
    source: &'static str,
    identifier: &str,
    password: &str,
) -> Result<User, MemberLoginError> {
    let ident = identifier.trim();
    if ident.is_empty() || password.is_empty() {
        return Err(MemberLoginError::Invalid);
    }
    let scope = LoginScope::Tenant(site.clone());
    let mut attempt = login_throttle::shared()
        .begin(&scope, &request.ip, ident)
        .await
        .map_err(MemberLoginError::Refused)?;
    let found = find_member(pool, ident).await;
    if let Some(user) = &found {
        attempt.resolve(&user.username).await.map_err(MemberLoginError::Refused)?;
    }
    // Verify before checking `active` (#770): returning at once for a
    // disabled account made its response measurably faster than an active
    // one's or an unknown one's (which pays `verify_dummy`), so timing told
    // an attacker the account exists but is disabled.
    let password_ok = match &found {
        Some(user) => {
            matches!(crate::passwords::verify(password, &user.password_hash).await, Ok(true))
        }
        None => {
            crate::passwords::verify_dummy(password).await;
            false
        }
    };
    let active = found.as_ref().is_some_and(|u| u.active);
    let user_id = found.as_ref().and_then(|u| u.id.get().copied());
    let username = found.as_ref().map_or(ident, |u| u.username.as_str());
    if password_ok && active {
        attempt.succeeded().await;
        let entry = security_log::Entry::new(security_log::EVENT_LOGIN, source, &request.meta)
            .user(user_id, username);
        security_log::record(pool, &site.slug, entry).await;
        return found.ok_or(MemberLoginError::Invalid);
    }
    attempt.failed().await;
    let reason = if password_ok { "inactive" } else { "invalid_credentials" };
    let entry = security_log::Entry::new(security_log::EVENT_LOGIN_FAILED, source, &request.meta)
        .user(user_id, username)
        .reason(reason);
    security_log::record(pool, &site.slug, entry).await;
    // The gate refuses a locked account before it gets here, so a lock
    // seen now is the one this failure set.
    if security_log::is_locked(site, &[ident, username]).await {
        let entry = security_log::Entry::new(security_log::EVENT_LOCKOUT, source, &request.meta)
            .user(user_id, username);
        security_log::record(pool, &site.slug, entry).await;
    }
    Err(MemberLoginError::Invalid)
}

/// The `rustango_users` row named `ident` (username first, then the
/// lowercased email).
async fn find_member(pool: &rustango::sql::Pool, ident: &str) -> Option<User> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let by_name: Vec<User> = User::objects()
        .where_(User::username.eq(ident.to_owned()))
        .limit(1)
        .fetch(pool)
        .await
        .unwrap_or_default();
    if let Some(user) = by_name.into_iter().next() {
        return Some(user);
    }
    User::objects()
        .filter("email", ident.to_ascii_lowercase())
        .limit(1)
        .fetch(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .next()
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
    user.insert(pool).await?;
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

/// Redirect to `next` with a fresh member cookie bound to `org`; a 500 for
/// an org with no id, which cannot be bound.
fn signed_in_redirect(
    ctx: &TenantContext,
    user: &User,
    org: &rustango::tenancy::Org,
    next: &str,
) -> Response {
    let Some(tenant) = rustango::tenancy::TenantSlug::of(org) else {
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let cookie = member_auth::mint_cookie(&ctx.session_secret, user, &tenant, MEMBER_SESSION_TTL);
    redirect_with_cookie(next, &cookie)
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

    fn member(email: &str) -> super::User {
        super::User {
            id: rustango::sql::Auto::Set(7),
            username: email.to_owned(),
            password_hash: "hash".to_owned(),
            email: Some(email.to_owned()),
            is_superuser: false,
            active: true,
            created_at: chrono::Utc::now(),
            data: serde_json::json!({ "display_name": "Ann" }),
            password_changed_at: None,
            sessions_revoked_at: None,
        }
    }

    #[test]
    fn a_confirmed_address_stops_counting_once_the_email_changes() {
        let mut user = member("ann@example.com");
        assert!(!super::email_is_verified(&user));
        super::mark_verified(&mut user);
        assert!(super::email_is_verified(&user));
        assert_eq!(user.data["display_name"], "Ann", "other data is kept");
        user.email = Some("new@example.com".to_owned());
        assert!(!super::email_is_verified(&user));
    }

    #[test]
    fn an_account_without_email_is_never_confirmed() {
        let mut user = member("ann@example.com");
        user.email = None;
        super::mark_verified(&mut user);
        assert!(!super::email_is_verified(&user));
    }

    #[test]
    fn a_confirmation_link_verifies_from_its_path_and_query() {
        let mut org = rustango::tenancy::Org::new("blog");
        org.id = rustango::sql::Auto::Set(1);
        let scope = crate::admin::handlers::password_reset_scope(&org);
        let key = crate::signing::derived_key(super::VERIFY_KEY_PURPOSE);
        let link = rustango::auth_flows::EmailVerification::issue(
            &scope,
            super::VERIFY_PATH,
            7,
            "ann@example.com",
            &key,
            super::VERIFY_TTL,
        );
        // The handler sees only the query string; it rebuilds the rest.
        let query = link.split_once('?').map(|(_, q)| q);
        let rebuilt = super::signed_link(super::VERIFY_PATH, query);
        let (uid, email) = rustango::auth_flows::EmailVerification::verify(&scope, &rebuilt, &key)
            .expect("the rebuilt link verifies");
        assert_eq!((uid, email.as_str()), (7, "ann@example.com"));

        let mut other = rustango::tenancy::Org::new("shop");
        other.id = rustango::sql::Auto::Set(2);
        assert!(
            rustango::auth_flows::EmailVerification::verify(
                &crate::admin::handlers::password_reset_scope(&other),
                &rebuilt,
                &key,
            )
            .is_err(),
            "a link for one site does not verify on another"
        );
        let tampered = rebuilt.replace("user_id=7", "user_id=8");
        assert!(rustango::auth_flows::EmailVerification::verify(&scope, &tampered, &key).is_err());
    }

    #[test]
    fn the_member_mail_pages_render_in_every_state() {
        let mut tera = tera::Tera::default();
        crate::admin::register_templates(&mut tera).expect("admin templates");
        let base = || {
            let mut ctx = tera::Context::new();
            ctx.insert("LANG", "fr");
            ctx.insert("csrf_token", "t");
            ctx.insert("site_name", "Acme");
            ctx
        };
        for outcome in ["verified", "already", "invalid", "error", "resend", "resent"] {
            let mut ctx = base();
            ctx.insert("outcome", outcome);
            ctx.insert("resend_url", super::VERIFY_RESEND_PATH);
            let html = tera.render("rcms_admin/member_verify.html", &ctx).unwrap_or_else(|e| panic!("{outcome}: {e:?}"));
            assert_eq!(html.contains("name=\"email\""), matches!(outcome, "invalid" | "error" | "resend"), "{outcome}");
        }
        for sent in [false, true] {
            let mut ctx = base();
            ctx.insert("action", super::RESET_REQUEST_PATH);
            ctx.insert("login_url", "/members/login");
            ctx.insert("sent", &sent);
            let html = tera.render("rcms_admin/member_password_reset.html", &ctx).expect("reset request");
            assert_eq!(html.contains("name=\"email\""), !sent);
        }
        for (valid, error) in [(true, None), (true, Some("weak_password")), (true, Some("password_mismatch")), (false, None)] {
            let mut ctx = base();
            ctx.insert("action", super::RESET_CONFIRM_PATH);
            ctx.insert("forgot_url", super::RESET_REQUEST_PATH);
            ctx.insert("valid", &valid);
            ctx.insert("signed_token", "/members/password-reset/confirm?x=1");
            if let Some(e) = error {
                ctx.insert("error", e);
            }
            let html = tera.render("rcms_admin/member_password_reset_confirm.html", &ctx).expect("reset confirm");
            assert_eq!(html.contains("name=\"password\""), valid);
            assert_eq!(html.contains("rcms-error"), error.is_some() || !valid);
        }
    }

    #[test]
    fn next_stays_on_site() {
        assert_eq!(sanitize_next("/members/area?x=1"), "/members/area?x=1");
        for hostile in ["https://evil.test", "//evil.test", "/\\evil.test", "/%5Cevil.test", "/%2F/evil.test", "/\t/evil.test"] {
            assert_eq!(sanitize_next(hostile), "/", "{hostile:?}");
        }
    }

    #[test]
    fn the_sign_in_page_renders_with_and_without_an_error() {
        let mut tera = tera::Tera::default();
        crate::admin::register_templates(&mut tera).expect("admin templates");
        for error in [None, Some("throttled"), Some("invalid")] {
            let mut ctx = tera::Context::new();
            ctx.insert("LANG", "en");
            ctx.insert("csrf_token", "t");
            ctx.insert("action", "/members/login");
            ctx.insert("signup_url", "/members/signup");
            ctx.insert("next", "/");
            ctx.insert("sso_enabled", &false);
            ctx.insert("sso_providers", &Vec::<serde_json::Value>::new());
            ctx.insert("site_name", "Acme");
            if let Some(e) = error {
                ctx.insert("error", e);
            }
            let html = tera
                .render("rcms_admin/member_login.html", &ctx)
                .unwrap_or_else(|e| panic!("{error:?}: {e:?}"));
            assert_eq!(html.contains("rcms-error"), error.is_some(), "{error:?}");
            assert!(!html.contains("/members/password-reset"), "no forgot link unless the handler passes one");
        }
    }

}
