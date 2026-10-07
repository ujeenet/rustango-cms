//! `/robots.txt` for the public CMS.
//!
//! The CMS has served [`/sitemap.xml`](crate::sitemap) for a long time,
//! and a sitemap no crawler is told about is a sitemap nobody reads: the
//! conventional way to advertise one is a `Sitemap:` line in robots.txt.
//! Until this module every rustango-cms site answered **404** there,
//! which is also why crawlers were free to spend their budget on
//! `/cms-admin/` and the login form.
//!
//! ## What is served
//!
//! A default that lets crawlers have the public site and keeps them out
//! of the admin and the auth endpoints, plus an absolute `Sitemap:` line
//! built from the request's own scheme and host — the same
//! [`base_url`](crate::sitemap) the sitemap's `<loc>` entries use, so the
//! two can never disagree about which host they are on.
//!
//! ## Overriding it
//!
//! A tenant that needs something else — a staging site that should
//! disallow everything, say — stores a replacement body in the
//! `seo` site-setting scope under `robots_txt`, and that is served
//! verbatim. Nothing sensitive can leak through this: robots.txt is a
//! public document by definition, and the value is only ever echoed
//! back as `text/plain`.
//!
//! ## Wire-up
//!
//! The CMS public [`router`](crate::router()) mounts `GET /robots.txt`
//! automatically, exactly as it does the sitemap. No registration needed.

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rustango::extractors::Tenant;

/// Site-setting scope and key a tenant can override the body with.
const SETTING_SCOPE: &str = "seo";
const SETTING_KEY: &str = "robots_txt";

/// Paths kept out of the crawl. These are the admin and auth mounts,
/// which are hardcoded here the same way they are in
/// [`crate::admin`] — there is no shared constant to reference yet, so
/// a change to the admin prefix has to be made in both places.
const DISALLOWED: &[&str] = &["/cms-admin/", "/login", "/logout"];

/// `GET /robots.txt`.
///
/// Answers `text/plain`, always 200 — a crawler that gets an error here
/// may treat the whole site as disallowed, so a failure to read the
/// override falls back to the default rather than surfacing a 5xx.
pub async fn handle_robots(t: Tenant, headers: HeaderMap) -> Response {
    let body = match crate::site_setting::get(t.pool(), SETTING_SCOPE).await {
        Ok(Some(setting)) => custom_body(&setting.value_json),
        // No `seo` scope stored: the common case, not a problem.
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                target: "rustango_cms::robots",
                error = %e,
                "could not read the seo site setting; serving the default robots.txt"
            );
            None
        }
    };

    let body = body.unwrap_or_else(|| default_body(&crate::sitemap::base_url(&headers)));

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

/// The stored override, when the scope holds a non-empty `robots_txt`
/// string. A blank value is treated as absent so clearing the field in
/// an admin form restores the default instead of serving an empty file,
/// which a crawler reads as "everything is allowed" — including the
/// admin.
fn custom_body(value: &serde_json::Value) -> Option<String> {
    let text = value.get(SETTING_KEY)?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    Some(format!("{text}\n"))
}

/// The default policy: the public site is crawlable, the admin and auth
/// endpoints are not, and the sitemap is advertised absolutely.
fn default_body(base: &str) -> String {
    let mut out = String::from("User-agent: *\n");
    for path in DISALLOWED {
        out.push_str("Disallow: ");
        out.push_str(path);
        out.push('\n');
    }
    out.push_str("\nSitemap: ");
    out.push_str(base);
    out.push_str("/sitemap.xml\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_advertises_the_sitemap_on_the_requested_host() {
        let body = default_body("https://rustango.com");

        assert!(body.contains("\nSitemap: https://rustango.com/sitemap.xml\n"));
        // Same host the sitemap's own <loc> entries use — the point of
        // sharing `base_url` is that these cannot drift apart.
        assert!(!body.contains("localhost"));
    }

    #[test]
    fn the_default_keeps_crawlers_out_of_the_admin_and_auth() {
        let body = default_body("https://example.org");

        assert!(body.starts_with("User-agent: *\n"));
        assert!(body.contains("\nDisallow: /cms-admin/\n"));
        assert!(body.contains("\nDisallow: /login\n"));
        assert!(body.contains("\nDisallow: /logout\n"));
    }

    #[test]
    fn a_stored_override_is_served_verbatim() {
        let value = serde_json::json!({ "robots_txt": "User-agent: *\nDisallow: /" });

        assert_eq!(
            custom_body(&value).as_deref(),
            Some("User-agent: *\nDisallow: /\n"),
        );
    }

    /// A blank or missing override must fall back, never serve an empty
    /// file — an empty robots.txt allows everything, including the admin
    /// paths the default is there to exclude.
    #[test]
    fn a_blank_or_absent_override_falls_back_to_the_default() {
        assert_eq!(custom_body(&serde_json::json!({ "robots_txt": "" })), None);
        assert_eq!(custom_body(&serde_json::json!({ "robots_txt": "   \n" })), None);
        assert_eq!(custom_body(&serde_json::json!({ "other": "x" })), None);
        assert_eq!(custom_body(&serde_json::json!({})), None);
        // Wrong type, e.g. someone stored a bool through the JSON column.
        assert_eq!(custom_body(&serde_json::json!({ "robots_txt": true })), None);
    }
}
