//! What a page type serves on its public URL, and how `Accept` picks.
//!
//! Most page types render a Tera template. Some — feeds a CRM consumes,
//! endpoints that only ever answer machines — have no template at all
//! and exist purely to return JSON. Before this module that was
//! unsayable: [`crate::page_type::PageTypeHandler::default_template`]
//! is a required `&'static str`, and `render_inner` hands whatever it
//! finds straight to Tera, so a blank template is a 500.
//!
//! Two enums, deliberately distinct:
//!
//! - [`PageViewMode`] is what the operator *declares* and what the
//!   `cms_page_type.view_mode` column stores. `Auto` is the default and
//!   means "work it out from the template".
//! - [`PageViewKind`] is what [`resolve_kind`] *decides*, and it is the
//!   only thing the renderer branches on.
//!
//! ## `Auto` never opens a JSON view on a page that has a template
//!
//! It would be tempting to make `Auto` mean "negotiate on everything".
//! That would silently publish a JSON representation of every page's
//! `extension` + `builder` values on every existing site the moment
//! this ships — including hosts that deliberately never mounted
//! [`crate::api::router`]. So `Auto` + a template is HTML-only and
//! ignores `Accept` entirely. Since every existing `cms_page_type` row
//! has a non-empty `default_template`, the upgrade is a no-op. Serving
//! JSON from a page that also renders HTML is opt-in via
//! [`PageViewMode::Api`].

use serde::{Deserialize, Serialize};

/// The declared mode, stored in `cms_page_type.view_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageViewMode {
    /// Derive from the template: blank means JSON-only, otherwise HTML.
    #[default]
    Auto,
    /// Force the template path; `Accept` is ignored.
    Html,
    /// Serve JSON at the page's own URL. When the type *also* has a
    /// template, that template still answers `Accept: text/html`.
    Api,
}

impl PageViewMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Html => "html",
            Self::Api => "api",
        }
    }

    /// Lenient by design — anything unrecognised reads as [`Self::Auto`].
    ///
    /// Unlike [`crate::view_restriction::RestrictionKind::parse`] this
    /// returns `Self`, not `Option<Self>`: during a rolling deploy an
    /// older binary can meet a row written by a newer one, and falling
    /// back to the "behave exactly as before" mode is strictly safer
    /// than erroring on a page request.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "html" => Self::Html,
            "api" => Self::Api,
            _ => Self::Auto,
        }
    }
}

/// What a page type can actually serve, once the declared mode and the
/// template have been reconciled. This is what the renderer branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageViewKind {
    /// Template only. No JSON representation exists; `Accept` is ignored.
    Html,
    /// JSON only — there is no usable template. Served whatever the
    /// client asks for, because it is the sole representation.
    ApiOnly,
    /// Both representations exist; negotiate on `Accept`.
    ApiWithTemplate,
    /// `view_mode = "html"` was declared but the template is blank.
    /// Nothing is renderable — a misconfiguration, not a mode.
    Unrenderable,
}

impl PageViewKind {
    /// Whether this kind can produce a JSON body at all.
    #[must_use]
    pub fn serves_json(self) -> bool {
        matches!(self, Self::ApiOnly | Self::ApiWithTemplate)
    }

    /// Whether a Tera template will be rendered for this kind.
    #[must_use]
    pub fn serves_html(self) -> bool {
        matches!(self, Self::Html | Self::ApiWithTemplate)
    }
}

/// Reconcile a declared mode with the template actually on the row.
///
/// Pure and synchronous. It reads the **stored strings**, not the
/// handler, for the same reason `render_inner` renders
/// `pt.default_template` rather than `handler.default_template()`: the
/// column is the runtime source of truth, UI-created page types have no
/// handler at all, and keeping it pure makes the whole decision table
/// unit-testable without a pool.
#[must_use]
pub fn resolve_kind(view_mode: &str, default_template: &str) -> PageViewKind {
    let has_template = !default_template.trim().is_empty();
    match (PageViewMode::parse(view_mode), has_template) {
        (PageViewMode::Auto, true) => PageViewKind::Html,
        // The autodetect: no template and nothing declared ⇒ JSON.
        (PageViewMode::Auto, false) => PageViewKind::ApiOnly,
        (PageViewMode::Api, true) => PageViewKind::ApiWithTemplate,
        (PageViewMode::Api, false) => PageViewKind::ApiOnly,
        (PageViewMode::Html, true) => PageViewKind::Html,
        (PageViewMode::Html, false) => PageViewKind::Unrenderable,
    }
}

/// [`resolve_kind`] over a loaded registry row.
#[must_use]
pub fn kind_for(row: &crate::page_type_model::PageType) -> PageViewKind {
    resolve_kind(&row.view_mode, &row.default_template)
}

/// The coarse media-type preference distilled from an `Accept` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accept {
    /// The client asked for HTML, or asked for both and tied.
    Html,
    /// The client asked for JSON more strongly than HTML.
    Json,
    /// No usable preference — `*/*`, a missing header, or nothing we
    /// recognise. Each [`PageViewKind`] picks its own canonical answer.
    Any,
}

/// Parse an `Accept` header into a coarse preference.
///
/// Deliberately *not* the `accept.contains("application/json")` check
/// used by the admin's own `fetch()` helpers
/// (`crate::admin::handlers::wants_json_response`). That test is fine
/// for a request the admin JS made itself, but wrong on a public URL:
/// it flips to JSON on `text/html, application/json;q=0.1`, where the
/// client clearly prefers HTML, and it treats `application/json;q=0` —
/// an explicit *refusal* — as a request for JSON.
///
/// Quality values are honoured, `q=0` rejects a type outright, and a
/// more specific range outranks a wildcard at equal `q`. **A tie goes
/// to HTML**, so a browser sending `text/html,…,*/*;q=0.8` can never be
/// handed a JSON body.
#[must_use]
pub fn parse_accept(raw: Option<&str>) -> Accept {
    // (quality, specificity) — specificity 2 = exact type, 1 = subtype
    // wildcard. `*/*` scores neither: it means "anything", which is
    // `Any`, not a vote for either side.
    let mut best_json: Option<(f32, u8)> = None;
    let mut best_html: Option<(f32, u8)> = None;

    for entry in raw.unwrap_or("").split(',') {
        let mut parts = entry.split(';');
        let Some(range) = parts.next() else { continue };
        let range = range.trim().to_ascii_lowercase();
        if range.is_empty() {
            continue;
        }

        let mut q = 1.0_f32;
        for param in parts {
            let param = param.trim();
            if let Some(v) = param.strip_prefix("q=") {
                // A malformed q is not a reason to reject the entry —
                // fall back to the default rather than erroring.
                q = v.trim().parse::<f32>().unwrap_or(1.0).clamp(0.0, 1.0);
            }
        }
        // `q=0` means "I will not accept this".
        if q <= 0.0 {
            continue;
        }

        let scored = match range.as_str() {
            "application/json" => Some((&mut best_json, 2u8)),
            "application/*" => Some((&mut best_json, 1)),
            "text/html" | "application/xhtml+xml" => Some((&mut best_html, 2)),
            "text/*" => Some((&mut best_html, 1)),
            _ => None,
        };
        if let Some((slot, specificity)) = scored {
            let candidate = (q, specificity);
            if slot.is_none_or(|current| candidate > current) {
                *slot = Some(candidate);
            }
        }
    }

    match (best_json, best_html) {
        (None, None) => Accept::Any,
        (Some(_), None) => Accept::Json,
        (None, Some(_)) => Accept::Html,
        // Ties go to HTML.
        (Some(j), Some(h)) => {
            if j > h {
                Accept::Json
            } else {
                Accept::Html
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_round_trips_and_unknown_reads_as_auto() {
        for m in [PageViewMode::Auto, PageViewMode::Html, PageViewMode::Api] {
            assert_eq!(PageViewMode::parse(m.as_str()), m);
        }
        // Forward-compat: a mode written by a newer binary must not
        // break an older one mid-rollout.
        assert_eq!(PageViewMode::parse("both"), PageViewMode::Auto);
        assert_eq!(PageViewMode::parse(""), PageViewMode::Auto);
        assert_eq!(PageViewMode::parse("  api  "), PageViewMode::Api);
    }

    #[test]
    fn auto_derives_the_kind_from_the_template() {
        assert_eq!(resolve_kind("auto", "page.html"), PageViewKind::Html);
        assert_eq!(resolve_kind("auto", ""), PageViewKind::ApiOnly);
        // Whitespace counts as blank — matches `DbSchemaPageType::from_row`.
        assert_eq!(resolve_kind("auto", "   "), PageViewKind::ApiOnly);
    }

    #[test]
    fn an_explicit_mode_overrides_the_autodetect() {
        assert_eq!(
            resolve_kind("api", "page.html"),
            PageViewKind::ApiWithTemplate
        );
        assert_eq!(resolve_kind("api", ""), PageViewKind::ApiOnly);
        assert_eq!(resolve_kind("html", "page.html"), PageViewKind::Html);
        // Declared HTML with nothing to render: a real misconfiguration,
        // and the state that shows "Preview is not available".
        assert_eq!(resolve_kind("html", ""), PageViewKind::Unrenderable);
    }

    #[test]
    fn an_unknown_mode_behaves_exactly_as_before() {
        // The whole point of the lenient parse: an existing row, or one
        // from a newer binary, must keep rendering its template.
        assert_eq!(resolve_kind("", "page.html"), PageViewKind::Html);
        assert_eq!(resolve_kind("someday", "page.html"), PageViewKind::Html);
    }

    #[test]
    fn browsers_always_get_html() {
        // The real strings, verbatim.
        let chrome = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,\
                      image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7";
        assert_eq!(parse_accept(Some(chrome)), Accept::Html);
        let safari = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
        assert_eq!(parse_accept(Some(safari)), Accept::Html);
    }

    #[test]
    fn wildcards_and_absent_headers_express_no_preference() {
        assert_eq!(parse_accept(None), Accept::Any);
        assert_eq!(parse_accept(Some("")), Accept::Any);
        assert_eq!(parse_accept(Some("*/*")), Accept::Any);
    }

    #[test]
    fn json_clients_get_json() {
        assert_eq!(parse_accept(Some("application/json")), Accept::Json);
        // axios' default.
        assert_eq!(
            parse_accept(Some("application/json, text/plain, */*")),
            Accept::Json
        );
        assert_eq!(parse_accept(Some("application/*")), Accept::Json);
    }

    #[test]
    fn quality_values_decide_and_a_tie_goes_to_html() {
        assert_eq!(
            parse_accept(Some("text/html;q=0.5, application/json;q=0.9")),
            Accept::Json
        );
        // The case a `contains("application/json")` check gets wrong.
        assert_eq!(
            parse_accept(Some("application/json;q=0.1, text/html")),
            Accept::Html
        );
        // Equal q: HTML wins, so a browser is never handed JSON.
        assert_eq!(
            parse_accept(Some("application/json, text/html")),
            Accept::Html
        );
    }

    #[test]
    fn q_zero_is_a_refusal_not_a_request() {
        // The second case a `contains()` check gets wrong.
        assert_eq!(parse_accept(Some("application/json;q=0")), Accept::Any);
        assert_eq!(
            parse_accept(Some("application/json;q=0, text/html")),
            Accept::Html
        );
    }

    #[test]
    fn malformed_input_never_panics() {
        for raw in ["))((", "text/html;q=notanumber", ";;;", ",,,", "q=1"] {
            let _ = parse_accept(Some(raw));
        }
        assert_eq!(parse_accept(Some("text/html;q=notanumber")), Accept::Html);
    }

    #[test]
    fn kinds_report_what_they_can_serve() {
        assert!(PageViewKind::ApiOnly.serves_json());
        assert!(!PageViewKind::ApiOnly.serves_html());
        assert!(PageViewKind::ApiWithTemplate.serves_json());
        assert!(PageViewKind::ApiWithTemplate.serves_html());
        assert!(!PageViewKind::Html.serves_json());
        assert!(!PageViewKind::Unrenderable.serves_json());
        assert!(!PageViewKind::Unrenderable.serves_html());
    }
}
