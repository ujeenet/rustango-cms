//! How the public router resolves which locale a request is asking for.
//!
//! Three modes; query mode is the v0.1/v0.2 default for back-compat.
//! Path mode (Wagtail-shape) puts the locale code as the first URL
//! segment, with the default locale living at the unprefixed root.
//!
//! - `/about`          → default locale (regardless of mode)
//! - `/es/about`       → recognized only when mode includes Path
//! - `/about?lang=es`  → recognized only when mode includes Query
//!
//! In `PathOrQuery` mode, an explicit `?lang=` wins over the path
//! prefix — the query parameter is read as an explicit override
//! (handy for testing) and the path prefix becomes the default
//! external-link shape.
//!
//! When no explicit locale is given (no `?lang=`, no path prefix), the
//! request's `Accept-Language` header is negotiated against the active
//! locales before falling through to the tenant default (#397). So
//! precedence is: explicit query > explicit path > Accept-Language >
//! default.

use rustango::core::Column as _;

use crate::locale::Locale;

/// Locale-resolution strategy for [`crate::router`] builders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocaleMode {
    /// `?lang=es` only. The v0.1 / v0.2 default — no path
    /// recognition, no extra DB hit per request.
    #[default]
    Query,
    /// `/es/...` only. The first URL segment is matched against
    /// `cms_locale.code`; if it hits, it's stripped and used as the
    /// active locale for rendering.
    Path,
    /// Both. Query wins when both are present, so a stale link
    /// `/es/about?lang=fr` renders French. The path prefix is the
    /// canonical external-link shape; the query parameter is the
    /// explicit override.
    PathOrQuery,
}

impl LocaleMode {
    /// Whether the router should inspect URL path segments for a
    /// locale prefix on every request.
    #[must_use]
    pub fn reads_path(self) -> bool {
        matches!(self, Self::Path | Self::PathOrQuery)
    }

    /// Whether the router should inspect `?lang=` on every request.
    #[must_use]
    pub fn reads_query(self) -> bool {
        matches!(self, Self::Query | Self::PathOrQuery)
    }
}

/// Cookie that persists a visitor's chosen locale across navigation so
/// links that don't carry the locale (bare nav paths, in-content links)
/// stay in the selected language. Lower precedence than an explicit
/// `?lang=` / `/<code>/` so a fresh explicit choice always wins.
pub const LOCALE_COOKIE: &str = "rcms_locale";

/// What [`resolve_request_locale`] returns: the active locale code
/// (if any) plus the URL path with the locale segment removed (so
/// downstream slug resolution sees the same `/about` whether the
/// request came in as `/about?lang=es` or `/es/about`).
#[derive(Debug, Clone)]
pub struct RequestLocale {
    /// Locale code resolved from the request, in `mode`'s priority
    /// order. `None` means "fall through to the tenant default".
    pub code: Option<String>,
    /// The request path with any locale prefix stripped.
    pub stripped_path: String,
    /// `true` when `code` came from an EXPLICIT source (`?lang=` or a
    /// `/<code>/` path prefix), as opposed to the persistence cookie or
    /// `Accept-Language`. The router (re)writes the locale cookie only on
    /// an explicit choice, so a plain nav click never churns it.
    pub explicit: bool,
}

/// BCP-47-ish tag shape gate (ASCII alphanumeric + `-`, ≤16 chars) —
/// keeps a malformed cookie/query value from poisoning the cache key or
/// a `/<code>/` lookup.
#[must_use]
pub fn code_shaped(s: &str) -> bool {
    !s.is_empty() && s.len() <= 16 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Pull the `rcms_locale` value out of a raw `Cookie` header. Returns
/// `None` when absent or malformed.
#[must_use]
pub fn locale_cookie_value(cookie_header: Option<&str>) -> Option<String> {
    let raw = cookie_header?;
    for part in raw.split(';') {
        if let Some(v) = part.trim().strip_prefix("rcms_locale=") {
            let v = v.trim();
            return if code_shaped(v) {
                Some(v.to_owned())
            } else {
                None
            };
        }
    }
    None
}

/// Resolve the request's locale code by inspecting path + query (as
/// `mode` permits) and the `Accept-Language` header.
///
/// Precedence: **explicit `?lang=` > explicit `/<code>/` path prefix >
/// `Accept-Language` negotiation > tenant default** (`None`). An
/// explicit choice always wins; the header is a fallback for
/// first-visit/unlinked requests (#397).
///
/// At most two DB queries, both conditional: the path-segment lookup
/// (only in path mode when the first segment looks like a code) and the
/// active-locale list for `Accept-Language` matching (only when no
/// explicit locale was given AND a header is present).
pub async fn resolve_request_locale(
    pool: &rustango::sql::Pool,
    mode: LocaleMode,
    path: &str,
    query: Option<&str>,
    cookie_code: Option<&str>,
    accept_language: Option<&str>,
) -> RequestLocale {
    let query_code = if mode.reads_query() {
        query.and_then(parse_lang_param)
    } else {
        None
    };

    let (path_code, stripped_path) = if mode.reads_path() {
        // Query wins, so a `/es/about?lang=fr` renders French — but the
        // segment is still stripped from the path either way.
        try_strip_locale_segment(pool, path).await
    } else {
        (None, path.to_owned())
    };

    // Precedence: explicit query > explicit path > persistence cookie >
    // Accept-Language > tenant default. The cookie keeps the chosen
    // language sticky across links that don't carry it; an explicit
    // choice always overrides it.
    let explicit_code = query_code.or(path_code);
    let explicit = explicit_code.is_some();
    let code = if let Some(c) = explicit_code {
        Some(c)
    } else if let Some(c) = cookie_code.filter(|c| code_shaped(c)) {
        Some(c.to_owned())
    } else {
        negotiate_accept_language(pool, accept_language).await
    };

    RequestLocale {
        code,
        stripped_path,
        explicit,
    }
}

/// Negotiate `Accept-Language` against the active `cms_locale.code`s.
/// Returns the highest-quality match (exact, then base-language —
/// `fr-CA` matches an active `fr`), or `None`. One DB query, only when
/// a non-empty header is present.
async fn negotiate_accept_language(
    pool: &rustango::sql::Pool,
    header: Option<&str>,
) -> Option<String> {
    use rustango::sql::FetcherPool as _;
    let prefs = parse_accept_language(header?);
    if prefs.is_empty() {
        return None;
    }
    let active: Vec<String> = Locale::objects()
        .where_(Locale::active.eq(true))
        .fetch(pool)
        .await
        .ok()?
        .into_iter()
        .map(|l| l.code)
        .collect();
    if active.is_empty() {
        return None;
    }
    for pref in &prefs {
        // Exact (case-insensitive) match first.
        if let Some(m) = active.iter().find(|c| c.eq_ignore_ascii_case(pref)) {
            return Some(m.clone());
        }
        // Base-language fallback: `fr-CA` → an active `fr` (or `fr-FR`).
        let pref_base = base_lang(pref);
        if let Some(m) = active
            .iter()
            .find(|c| base_lang(c).eq_ignore_ascii_case(pref_base))
        {
            return Some(m.clone());
        }
    }
    None
}

fn base_lang(tag: &str) -> &str {
    tag.split('-').next().unwrap_or(tag)
}

/// Parse an `Accept-Language` header into BCP-47 tags ordered by
/// quality (descending; ties keep header order). Drops `*`, `q=0`, and
/// malformed tags so a bad value can't poison the cache key.
fn parse_accept_language(header: &str) -> Vec<String> {
    let mut items: Vec<(f32, String)> = Vec::new();
    for part in header.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mut bits = part.split(';');
        let tag = bits.next().unwrap_or("").trim();
        if tag.is_empty()
            || tag == "*"
            || !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            continue;
        }
        let mut q = 1.0f32;
        for b in bits {
            if let Some(v) = b.trim().strip_prefix("q=") {
                q = v.trim().parse().unwrap_or(1.0);
            }
        }
        if q <= 0.0 {
            continue;
        }
        items.push((q, tag.to_owned()));
    }
    // Stable sort by quality desc keeps header order for equal-q tags.
    items.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    items.into_iter().map(|(_, t)| t).collect()
}

/// Peek the first path segment. If it looks like a BCP-47 tag AND
/// matches an active `cms_locale.code`, return `(Some(code),
/// path_without_segment)`. Otherwise `(None, original_path)`.
async fn try_strip_locale_segment(
    pool: &rustango::sql::Pool,
    path: &str,
) -> (Option<String>, String) {
    use rustango::sql::FetcherPool as _;
    let trimmed = path.trim_start_matches('/');
    let mut iter = trimmed.splitn(2, '/');
    let head = iter.next().unwrap_or("");
    if head.is_empty()
        || head.len() > 16
        || !head.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return (None, path.to_owned());
    }
    let exists = Locale::objects()
        .where_(Locale::code.eq(head.to_owned()))
        .where_(Locale::active.eq(true))
        .fetch(pool)
        .await
        .ok()
        .and_then(|mut v| v.pop())
        .is_some();
    if !exists {
        return (None, path.to_owned());
    }
    let rest = iter.next().unwrap_or("");
    let stripped = if rest.is_empty() {
        "/".to_owned()
    } else {
        format!("/{rest}")
    };
    (Some(head.to_owned()), stripped)
}

/// Pull `?lang=<code>` out of a raw query string. Restricts to ASCII
/// alphanumeric + `-` (BCP-47 tag shape) so a malformed value can't
/// poison the cache key.
fn parse_lang_param(q: &str) -> Option<String> {
    for pair in q.split('&') {
        if let Some(rest) = pair.strip_prefix("lang=") {
            let trimmed = rest.trim();
            if trimmed.is_empty() {
                return None;
            }
            if trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                return Some(trimmed.to_owned());
            }
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lang_param_extracts_value() {
        assert_eq!(parse_lang_param("lang=es"), Some("es".to_owned()));
        assert_eq!(parse_lang_param("lang=es&foo=bar"), Some("es".to_owned()));
        assert_eq!(parse_lang_param("foo=bar&lang=fr"), Some("fr".to_owned()));
    }

    #[test]
    fn parse_lang_param_rejects_garbage() {
        assert_eq!(parse_lang_param(""), None);
        assert_eq!(parse_lang_param("lang="), None);
        assert_eq!(parse_lang_param("lang=../etc/passwd"), None);
        assert_eq!(parse_lang_param("lang=<script>"), None);
        assert_eq!(parse_lang_param("foo=bar"), None);
    }

    #[test]
    fn accept_language_orders_by_quality() {
        // Default q=1 for the unweighted tag; explicit q orders the rest.
        assert_eq!(
            parse_accept_language("de;q=0.7, en;q=0.8, fr"),
            vec!["fr", "en", "de"]
        );
    }

    #[test]
    fn accept_language_keeps_header_order_for_ties() {
        assert_eq!(
            parse_accept_language("en-US, en;q=0.9, fr;q=0.9"),
            vec!["en-US", "en", "fr"]
        );
    }

    #[test]
    fn accept_language_drops_wildcard_zero_q_and_garbage() {
        assert_eq!(
            parse_accept_language("*;q=0.5, xx;q=0, en, ../bad, fr-CA"),
            vec!["en", "fr-CA"]
        );
        assert!(parse_accept_language("").is_empty());
        assert!(parse_accept_language("*").is_empty());
    }

    #[test]
    fn base_lang_strips_region() {
        assert_eq!(base_lang("fr-CA"), "fr");
        assert_eq!(base_lang("en"), "en");
    }
}
