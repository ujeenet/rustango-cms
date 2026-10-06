//! A machine-readable error body for the v2 API.
//!
//! Every failure used to be a bare `(StatusCode, &'static str)`, which
//! axum renders as `text/plain`. On an API whose success path is JSON
//! that is a trap: a client's `await res.json()` throws a `SyntaxError`
//! on *every* failure, so the handler reports a parse problem instead of
//! the 404 that actually happened. And two 404s were distinguishable
//! only by their prose — `"page not found"` vs `"menu not found"` — so
//! branching meant string-matching English.
//!
//! Errors now carry the same envelope as everything else:
//!
//! ```json
//! { "error": { "code": "not_found", "message": "page not found" } }
//! ```
//!
//! `code` is the stable part. `message` is for humans and may be
//! reworded; clients branch on `code` and on the HTTP status.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

/// A stable, machine-readable error identifier.
///
/// Deliberately small. Each variant maps to exactly one status, so a
/// client can branch on either and get the same answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// The resource does not exist, is not public, or the caller may not
    /// know whether it exists. Always 404 — see the note below.
    NotFound,
    /// The request itself is malformed: a bad query value, a missing
    /// required parameter.
    BadRequest,
    /// The caller is not authenticated and authentication would help.
    Unauthenticated,
    /// The caller is authenticated but not allowed.
    Forbidden,
    /// The caller must supply a page password (`password` restriction).
    PasswordRequired,
    /// The path exists but not with this verb. The API is read-only, so
    /// this is what every write attempt gets.
    MethodNotAllowed,
    /// Something failed server-side. The detail is logged, not returned.
    Internal,
}

impl ErrorCode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::BadRequest => "bad_request",
            Self::Unauthenticated => "unauthenticated",
            Self::Forbidden => "forbidden",
            Self::PasswordRequired => "password_required",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::Internal => "internal_error",
        }
    }

    #[must_use]
    pub fn status(self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Forbidden | Self::PasswordRequired => StatusCode::FORBIDDEN,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// One API failure, rendered as JSON.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}

impl ApiError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Not found, with one message covering "absent", "not public" and
    /// "you may not see it" — distinguishing them would turn the
    /// endpoint into an existence oracle for gated content.
    #[must_use]
    pub fn not_found(what: &str) -> Self {
        Self::new(ErrorCode::NotFound, format!("{what} not found"))
    }

    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, message)
    }

    /// Server-side failure. The caller gets no detail; the cause belongs
    /// in the log, not the response.
    #[must_use]
    pub fn internal() -> Self {
        Self::new(ErrorCode::Internal, "internal error")
    }

    #[must_use]
    pub fn unauthenticated() -> Self {
        Self::new(
            ErrorCode::Unauthenticated,
            "authentication required to view this page",
        )
    }

    #[must_use]
    pub fn forbidden() -> Self {
        Self::new(ErrorCode::Forbidden, "you don't have access to this page")
    }

    #[must_use]
    pub fn password_required() -> Self {
        Self::new(
            ErrorCode::PasswordRequired,
            "this page is password-protected",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.code.status(),
            Json(serde_json::json!({
                "error": { "code": self.code.as_str(), "message": self.message }
            })),
        )
            .into_response()
    }
}

/// Give axum's bodiless `405` the JSON envelope the rest of the API uses.
///
/// The router registers `GET` (and one `POST`, for login), so any other
/// verb is refused by axum's own `MethodRouter` — correctly, and with the
/// `Allow` header RFC 9110 requires. But it refuses with an *empty* body,
/// and `docs/api.md` promises every failure is JSON with a stable code. A
/// client that does `res.json()` on a failure — the normal shape of a
/// fetch wrapper — got a `SyntaxError` pointing at its own parser instead
/// of a 405 pointing at its verb.
///
/// The message does not name the permitted verbs, and cannot: axum
/// attaches `Allow` *outside* `Router::layer`, so this middleware never
/// sees it. That is the right division anyway — `Allow` is where a client
/// is supposed to look, it is per-route (`/pages/` allows `GET,HEAD`,
/// `/auth/login` allows `POST`), and duplicating it into prose from here
/// would mean re-deriving routing knowledge that would eventually lie.
/// Inner headers are carried across so replacing the body never drops
/// one.
pub async fn method_not_allowed_json(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let resp = next.run(req).await;
    if resp.status() != StatusCode::METHOD_NOT_ALLOWED {
        return resp;
    }

    let (parts, _) = resp.into_parts();
    let mut out = ApiError::new(ErrorCode::MethodNotAllowed, "method not allowed").into_response();
    for (name, value) in parts.headers {
        let Some(name) = name else { continue };
        // Ours describe the new body; theirs described the empty one.
        if name == axum::http::header::CONTENT_LENGTH
            || name == axum::http::header::CONTENT_TYPE
        {
            continue;
        }
        out.headers_mut().insert(name, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_code_maps_to_one_status() {
        assert_eq!(ErrorCode::NotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(ErrorCode::BadRequest.status(), StatusCode::BAD_REQUEST);
        assert_eq!(ErrorCode::Unauthenticated.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(ErrorCode::Forbidden.status(), StatusCode::FORBIDDEN);
        assert_eq!(ErrorCode::PasswordRequired.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            ErrorCode::Internal.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }

    #[test]
    fn codes_are_stable_snake_case_identifiers() {
        // Clients branch on these; renaming one is a breaking change.
        for (code, want) in [
            (ErrorCode::NotFound, "not_found"),
            (ErrorCode::BadRequest, "bad_request"),
            (ErrorCode::Unauthenticated, "unauthenticated"),
            (ErrorCode::Forbidden, "forbidden"),
            (ErrorCode::PasswordRequired, "password_required"),
            (ErrorCode::Internal, "internal_error"),
            (ErrorCode::MethodNotAllowed, "method_not_allowed"),
        ] {
            assert_eq!(code.as_str(), want);
        }
    }

    #[test]
    fn not_found_reads_naturally_and_hides_the_reason() {
        // "absent" and "gated" must produce the same body.
        assert_eq!(ApiError::not_found("page").message, "page not found");
        assert_eq!(ApiError::not_found("menu").message, "menu not found");
    }
}
