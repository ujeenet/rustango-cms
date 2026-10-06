//! Preview a draft in a **decoupled renderer**.
//!
//! `preview_token` mints a signed token and `/api/v2/pages/{id}/` accepts
//! it, so the machinery for reading a draft over the API already worked.
//! What was missing was the hand-off: the mint endpoint lives inside the
//! session-gated admin, so the only way an editor could use it was to
//! copy a token out by hand. In practice that meant decoupled sites had
//! no preview at all.
//!
//! This closes it. A tenant configures where its frontend lives, and the
//! page editor gains a link that carries a fresh token to it. The
//! frontend reads the token, calls the API with `?preview_token=`, and
//! renders the draft with its own templates.
//!
//! ## The contract with the frontend
//!
//! The configured value is a **URL template**. Each placeholder is
//! substituted and percent-encoded:
//!
//! | Placeholder | Value |
//! |---|---|
//! | `{token}` | the signed preview token |
//! | `{path}` | the page's public URL path, e.g. `/docs/intro` — its `/` separators are **kept**, so the placeholder works in the path position (`https://app.example.com{path}`) as well as in a query value |
//! | `{id}` | the page id |
//!
//! ```text
//! https://app.example.com/api/preview?token={token}&path={path}
//! ```
//!
//! A value with **no** placeholder is treated as a base URL and the
//! parameters are appended as a query string — so the simplest possible
//! configuration (`https://app.example.com/preview`) also works, and an
//! editor who pastes a bare URL is not left wondering why nothing
//! happens.
//!
//! ## When one page does not follow the pattern
//!
//! Substituting `{path}` with the CMS's `url_path` assumes the frontend
//! mirrors the CMS's URL structure. Many do not — `/features/ai-engine`
//! here can be `/product/ai-engine` there, or `/p/9`, or a different app
//! altogether. [`Page::preview_path`](crate::page::Page::preview_path)
//! overrides it per page, and [`for_page`] is where the two meet.

use crate::widget::{Widget, WidgetKind};

/// The site-settings scope holding the frontend's preview entry point.
pub const SCOPE: &str = "preview";
/// The field within that scope.
pub const FIELD: &str = "base_url";

crate::register_site_setting!(SCOPE, "Headless preview", || vec![Widget::new(
    WidgetKind::Text,
    FIELD,
    "Frontend preview URL",
)
.with_help(
    "Where the page editor's “Preview on site” link sends an editor, for \
     sites rendered by a separate frontend. Use {token}, {path} and {id} \
     as placeholders — e.g. https://app.example.com/api/preview?token={token}&path={path}. \
     A plain URL with no placeholders gets them appended as query \
     parameters. Leave empty when the CMS renders the site itself.",
)]);

/// Build the URL an editor should be sent to, or `None` when the tenant
/// has not configured a frontend.
///
/// `base` is the raw configured value; blank counts as unconfigured, so
/// an editor who clears the field turns the feature off rather than
/// getting a link to nowhere.
#[must_use]
pub fn build(base: &str, token: &str, page_id: i64, url_path: &str) -> Option<String> {
    let base = base.trim();
    if base.is_empty() {
        return None;
    }
    let path = if url_path.is_empty() { "/" } else { url_path };
    let id = page_id.to_string();

    let has_placeholder =
        base.contains("{token}") || base.contains("{path}") || base.contains("{id}");
    if has_placeholder {
        return Some(
            base.replace("{token}", &encode(token))
                .replace("{path}", &encode_path(path))
                .replace("{id}", &encode(&id)),
        );
    }

    // No template: append. Respect a base that already carries a query.
    let sep = if base.contains('?') { '&' } else { '?' };
    Some(format!(
        "{base}{sep}token={}&path={}&id={}",
        encode(token),
        encode_path(path),
        encode(&id),
    ))
}

/// Percent-encode a value for use in a query string.
///
/// Hand-rolled for the same reason `view_restriction_guard` does it: the
/// framework vends no encoder on its public surface, and pulling a crate
/// in for three call sites is not worth it. Unreserved characters per
/// RFC 3986 pass through; everything else, including `/`, is escaped —
/// the path is a *parameter value* here, not a path segment.
/// The preview URL for one page: the site-wide template, the page's own
/// override, or nothing.
///
/// Three cases, in the order an editor would expect:
///
/// 1. the page's `preview_path` is an **absolute URL** — that page is
///    rendered by something else entirely, so it wins outright and works
///    even when no site-wide frontend is configured;
/// 2. the page's `preview_path` is a **path** — the site template still
///    supplies the host and the token, and this replaces `{path}`;
/// 3. it is **empty** — `url_path`, which is the common case.
///
/// Returns `None` when there is nothing to link to: no site template and
/// no absolute override.
#[must_use]
pub fn for_page(
    site_base: Option<&str>,
    page_override: &str,
    token: &str,
    page_id: i64,
    url_path: &str,
) -> Option<String> {
    let over = page_override.trim();
    if is_absolute(over) {
        // `{path}` inside an absolute override still resolves, so a page
        // can point at another host and keep the templating.
        return build(over, token, page_id, url_path);
    }
    let base = site_base?;
    let path = if over.is_empty() { url_path } else { over };
    build(base, token, page_id, path)
}

/// Whether a per-page value names a whole URL rather than a path.
///
/// Scheme-relative (`//host/x`) is deliberately *not* absolute here: it
/// is far more likely to be a typo'd path than a deliberate
/// protocol-relative URL, and treating it as a whole URL would silently
/// drop the configured host.
fn is_absolute(raw: &str) -> bool {
    let lower = raw.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Percent-encode a **path**, keeping its `/` separators.
///
/// `{path}` is routinely placed in the path position — a frontend
/// configured as `https://app.example.com{path}?token={token}` is the
/// most natural template there is. Escaping the separators turned that
/// into `https://app.example.com%2Fdocs%2Fintro`, whose authority is
/// nonsense and which no browser can follow.
///
/// Keeping `/` is also correct in the query position: RFC 3986 permits
/// `/` unescaped in a query component, so one encoding serves both and
/// there is no need to ask the author which one they meant.
fn encode_path(raw: &str) -> String {
    raw.split('/').map(encode).collect::<Vec<_>>().join("/")
}

fn encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Read the configured frontend URL for this tenant, if any.
///
/// # Errors
/// Driver / query failures.
pub async fn configured_base(
    pool: &rustango::sql::Pool,
) -> Result<Option<String>, rustango::sql::ExecError> {
    Ok(crate::site_setting::get(pool, SCOPE)
        .await?
        .and_then(|row| {
            row.value_json
                .get(FIELD)
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .filter(|s| !s.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unconfigured_tenant_gets_no_link() {
        // Blank must mean "off", not "a link to nowhere".
        assert_eq!(build("", "tok", 7, "/docs"), None);
        assert_eq!(build("   ", "tok", 7, "/docs"), None);
    }

    #[test]
    fn a_template_substitutes_every_placeholder() {
        let got = build(
            "https://app.example.com/api/preview?token={token}&path={path}&id={id}",
            "abc123",
            7,
            "/docs/intro",
        );
        assert_eq!(
            got.unwrap(),
            "https://app.example.com/api/preview?token=abc123&path=/docs/intro&id=7",
        );
    }

    #[test]
    fn a_placeholder_may_appear_in_the_path_not_just_the_query() {
        // Some frontends route previews as `/preview/<id>`.
        let got = build("https://app.example.com/preview/{id}?t={token}", "abc", 42, "/x");
        assert_eq!(got.unwrap(), "https://app.example.com/preview/42?t=abc");
    }

    #[test]
    fn path_in_the_path_position_keeps_its_separators() {
        // The template a decoupled site actually configures. Escaping the
        // separators produced `http://host%2Ffeatures%2Fai-engine`, whose
        // authority is nonsense — and the test above missed it because
        // `{id}` is a bare number that encoding cannot change.
        let got = build(
            "http://localhost:5173{path}?token={token}",
            "9.123.abc",
            9,
            "/features/ai-engine",
        );
        assert_eq!(
            got.unwrap(),
            "http://localhost:5173/features/ai-engine?token=9.123.abc",
        );
    }

    #[test]
    fn a_path_segment_that_would_break_the_url_is_still_escaped() {
        // Only the separators are spared; anything inside a segment that
        // would end the path or start a fragment must not survive raw.
        let got = build("https://x.test{path}", "t", 1, "/a b/c?d#e").unwrap();
        assert_eq!(got, "https://x.test/a%20b/c%3Fd%23e");
    }

    #[test]
    fn a_plain_base_url_gets_the_parameters_appended() {
        // The simplest configuration an editor might paste.
        let got = build("https://app.example.com/preview", "abc", 7, "/docs");
        assert_eq!(
            got.unwrap(),
            "https://app.example.com/preview?token=abc&path=/docs&id=7",
        );
    }

    #[test]
    fn an_existing_query_string_is_preserved() {
        let got = build("https://app.example.com/preview?mode=draft", "abc", 7, "/x");
        assert_eq!(
            got.unwrap(),
            "https://app.example.com/preview?mode=draft&token=abc&path=/x&id=7",
        );
    }

    #[test]
    fn the_root_page_previews_as_slash() {
        // `url_path` is stored empty for the root; sending an empty
        // `path=` would leave the frontend guessing.
        let got = build("https://app.example.com/p", "abc", 1, "");
        assert!(got.unwrap().contains("path=/&"), "root must still send a path");
    }

    #[test]
    fn a_token_with_url_specials_survives_the_round_trip() {
        // Tokens are base64url plus `.`, all of which must arrive intact.
        let got = build("https://x.test/p?t={token}", "a.b-c_d~e", 1, "/x").unwrap();
        assert!(got.ends_with("t=a.b-c_d~e"), "got {got}");
    }

    #[test]
    fn a_page_without_an_override_follows_its_url_path() {
        let got = for_page(Some("https://app.test{path}?t={token}"), "", "tok", 9, "/features/ai");
        assert_eq!(got.unwrap(), "https://app.test/features/ai?t=tok");
    }

    #[test]
    fn a_page_path_override_replaces_only_the_path() {
        // The frontend routes this page differently, but the host and
        // the token still come from the one site-wide template.
        let got = for_page(
            Some("https://app.test{path}?t={token}"),
            "/product/ai-engine",
            "tok",
            9,
            "/features/ai",
        );
        assert_eq!(got.unwrap(), "https://app.test/product/ai-engine?t=tok");
    }

    #[test]
    fn an_absolute_override_wins_outright() {
        // "This page is rendered by a different app."
        let got = for_page(
            Some("https://app.test{path}?t={token}"),
            "https://other.test/x?t={token}",
            "tok",
            9,
            "/features/ai",
        );
        assert_eq!(got.unwrap(), "https://other.test/x?t=tok");
    }

    #[test]
    fn an_absolute_override_works_with_no_site_template_at_all() {
        // The whole point of the case: one page lives elsewhere, and the
        // tenant has configured no site-wide frontend.
        let got = for_page(None, "https://other.test/x?t={token}", "tok", 9, "/a");
        assert_eq!(got.unwrap(), "https://other.test/x?t=tok");
    }

    #[test]
    fn no_template_and_no_override_is_no_link() {
        assert_eq!(for_page(None, "", "tok", 9, "/a"), None);
        assert_eq!(for_page(None, "   ", "tok", 9, "/a"), None);
        // A *path* override cannot stand on its own — it has no host.
        assert_eq!(for_page(None, "/product/x", "tok", 9, "/a"), None);
    }

    #[test]
    fn an_override_is_trimmed_before_it_is_judged() {
        let got = for_page(Some("https://app.test{path}"), "  /p/9  ", "t", 9, "/a");
        assert_eq!(got.unwrap(), "https://app.test/p/9");
    }

    #[test]
    fn scheme_relative_is_treated_as_a_path_not_a_url() {
        // `//host/x` is far more likely a typo'd path than a deliberate
        // protocol-relative URL; treating it as absolute would silently
        // drop the configured host.
        assert!(!is_absolute("//other.test/x"));
        let got = for_page(Some("https://app.test{path}"), "//other.test/x", "t", 9, "/a");
        assert_eq!(got.unwrap(), "https://app.test//other.test/x");
    }

    #[test]
    fn an_absolute_override_is_recognised_case_insensitively() {
        assert!(is_absolute("HTTPS://other.test/x"));
        assert!(is_absolute("Http://other.test/x"));
        assert!(!is_absolute("ftp://other.test/x"));
    }

    #[test]
    fn encoding_escapes_what_would_break_a_query() {
        assert_eq!(encode("/a b&c=d?e#f"), "%2Fa%20b%26c%3Dd%3Fe%23f");
        assert_eq!(encode("plain-Text_1.0~"), "plain-Text_1.0~");
    }
}
