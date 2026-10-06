//! `POST /api/v2/auth/login` — a member session a browser SPA can hold.
//!
//! Member auth was cookie-only: `rustango_member_session` is
//! `HttpOnly; SameSite=Lax`, and Lax cookies are simply not sent on
//! cross-site fetches. So a decoupled frontend on its own origin could
//! not authenticate a member *at all* — CORS and `credentials:
//! "include"` do not help, because the browser never attaches the
//! cookie. Gated content was unreachable except from a same-origin page.
//!
//! The token here is deliberately **not a new credential format**: it is
//! the exact same signed value the cookie carries, minted by
//! `member_auth::encode` and verified by `member_auth::decode`. So it
//! inherits the existing expiry, the tenant binding, and the rule that a
//! password change invalidates outstanding sessions — and it cannot
//! drift from the cookie path, because there is only one path.
//!
//! What changes is transport: the SPA holds the value and sends
//! `Authorization: Bearer <token>` instead of relying on a cookie the
//! browser refuses to send.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango::extractors::Tenant;
use rustango::tenancy::auth::User;
use rustango::tenancy::member_auth;
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;

/// Where credentials are posted.
pub const LOGIN_PATH: &str = "/api/v2/auth/login";

/// Credentials. Accepts JSON or form encoding, so a SPA and an HTML form
/// can both post here.
#[derive(Deserialize)]
pub struct LoginForm {
    /// Email or username — whatever the member signed up with.
    pub identifier: String,
    pub password: String,
}

/// `POST /api/v2/auth/login`
///
/// Accepts `application/json` or form encoding, decided by
/// `Content-Type`, so a SPA and a plain HTML form can both post here.
/// Two separate handlers would need two routes for one path.
pub async fn login(
    tenant: Tenant,
    axum::Extension(ctx): axum::Extension<Arc<rustango::extractors::TenantContext>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let is_json = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));

    let parsed: Result<LoginForm, String> = if is_json {
        serde_json::from_slice(&body).map_err(|e| e.to_string())
    } else {
        serde_urlencoded::from_bytes(&body).map_err(|e| e.to_string())
    };
    let Ok(form) = parsed else {
        return crate::api::error::ApiError::bad_request(
            "expected `identifier` and `password`, as JSON or form encoding",
        )
        .into_response();
    };

    login_inner(&tenant, &ctx, &form).await
}

async fn login_inner(
    tenant: &Tenant,
    ctx: &rustango::extractors::TenantContext,
    form: &LoginForm,
) -> Response {
    let Some(user) = crate::admin::members::authenticate_member(
        tenant.pool(),
        &form.identifier,
        &form.password,
    )
    .await
    else {
        // One message for "no such account" and "wrong password" — the
        // difference is an account-enumeration oracle, and a client can
        // do nothing useful with it either way.
        return crate::api::error::ApiError::new(
            crate::api::error::ErrorCode::Unauthenticated,
            "invalid credentials",
        )
        .into_response();
    };

    let uid = user.id.get().copied().unwrap_or_default();
    let ttl = crate::admin::members::MEMBER_SESSION_TTL;
    let token = member_auth::encode(
        &ctx.session_secret,
        &member_auth::MemberSessionPayload::new(
            uid,
            &tenant.org.slug,
            ttl,
            rustango::session::PasswordFingerprint::of(&ctx.session_secret, &user.password_hash),
        ),
    );

    Json(serde_json::json!({
        "token": token,
        "token_type": "Bearer",
        "expires_in": ttl,
        "member": {
            "id": uid,
            "username": user.username,
            "email": user.email,
        },
    }))
    .into_response()
}

/// A member resolved from `Authorization: Bearer <token>`.
///
/// Never rejects — like [`member_auth::CurrentMember`] and
/// `SessionUser`, an absent or invalid token simply means "anonymous",
/// because these endpoints serve anonymous callers by design. A bad
/// token must not turn a public page into a 401.
#[derive(Debug, Clone)]
pub struct BearerMember(pub Option<User>);

impl<S: Send + Sync> FromRequestParts<S> for BearerMember {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let Some(token) = bearer_token(parts) else {
            return Ok(Self(None));
        };
        let Some(ctx) = parts
            .extensions
            .get::<Arc<rustango::extractors::TenantContext>>()
            .cloned()
        else {
            return Ok(Self(None));
        };
        use rustango::tenancy::OrgResolver as _;
        let org = match ctx.resolver.resolve(parts, &ctx.pools.registry_pool()).await {
            Ok(Some(o)) => o,
            _ => return Ok(Self(None)),
        };
        // Same verification the cookie path runs: signature, expiry, and
        // the tenant the token was minted for.
        let Ok(payload) = member_auth::decode(&ctx.session_secret, &org.slug, &token) else {
            return Ok(Self(None));
        };
        let Ok(pool) = ctx.pools.scoped_pool_dyn(&org).await else {
            return Ok(Self(None));
        };

        use rustango::core::Column as _;
        use rustango::sql::FetcherPool as _;
        let user = User::objects()
            .where_(User::id.eq(payload.uid))
            .fetch(&pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .next()
            .filter(|u| u.active)
            // A token minted before the user's last password change is
            // dead, exactly as the cookie is. Rotating a password must
            // log out every device, including SPA ones.
            .filter(|u| match u.password_changed_at {
                Some(changed) => payload.iat >= changed.timestamp(),
                None => true,
            });
        Ok(Self(user))
    }
}

/// Pull the token out of an `Authorization: Bearer …` header.
fn bearer_token(parts: &Parts) -> Option<String> {
    let raw = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, value) = raw.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| value.trim().to_owned())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts_with(header: Option<&str>) -> Parts {
        let mut b = axum::http::Request::builder().uri("/");
        if let Some(h) = header {
            b = b.header(axum::http::header::AUTHORIZATION, h);
        }
        b.body(()).unwrap().into_parts().0
    }

    #[test]
    fn extracts_a_bearer_token() {
        assert_eq!(
            bearer_token(&parts_with(Some("Bearer abc.def"))),
            Some("abc.def".to_owned()),
        );
    }

    #[test]
    fn the_scheme_is_case_insensitive() {
        // RFC 9110 says the scheme is case-insensitive, and clients vary.
        for h in ["bearer abc", "BEARER abc", "BeArEr abc"] {
            assert_eq!(bearer_token(&parts_with(Some(h))), Some("abc".to_owned()));
        }
    }

    #[test]
    fn another_scheme_is_not_a_bearer_token() {
        assert_eq!(bearer_token(&parts_with(Some("Basic abc"))), None);
    }

    #[test]
    fn a_missing_or_empty_header_yields_nothing() {
        assert_eq!(bearer_token(&parts_with(None)), None);
        assert_eq!(bearer_token(&parts_with(Some("Bearer"))), None);
        assert_eq!(bearer_token(&parts_with(Some("Bearer   "))), None);
    }
}
