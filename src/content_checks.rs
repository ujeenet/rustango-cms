//! Extensible content-checks framework.
//!
//! The accessibility panel ([`crate::a11y`]) runs a fixed set of
//! server-side heuristics. This module generalises that idea into a
//! *registerable* check framework:
//! a host implements [`ContentCheck`], registers it with
//! [`register_content_check!`](crate::register_content_check!), and its findings show up in the editor's
//! "Content & SEO" panel alongside the built-ins.
//!
//! Ships built-in SEO checks — most notably the
//! **empty meta-description** check — plus a thin /
//! missing social-image heuristic.
//!
//! Checks are synchronous and run against the saved [`Page`] (the panel
//! is server-rendered on editor open, same as the a11y heuristics). The
//! [`Severity`] levels are shared with the a11y
//! panel so the two read consistently.

use crate::a11y::Severity;
use crate::page::Page;

/// One content/SEO finding surfaced to the editor. Mirrors
/// [`crate::a11y::Violation`] but carries a `category` so the panel can
/// group SEO / content / custom checks.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Finding {
    /// Stable identifier, e.g. `"empty_seo_description"`.
    pub code: &'static str,
    /// Display bucket, e.g. `"SEO"` or `"Content"`.
    pub category: &'static str,
    pub severity: Severity,
    pub message: String,
    /// Optional canonical field hint — drives a scroll-to-field link.
    pub field: Option<&'static str>,
}

/// A registerable content check. Implementors are zero-field structs
/// registered with [`register_content_check!`](crate::register_content_check!).
pub trait ContentCheck: Send + Sync + 'static {
    /// Stable code prefix / identifier for the check.
    fn code(&self) -> &'static str;
    /// Run against the page; return zero or more findings.
    fn run(&self, page: &Page) -> Vec<Finding>;
}

/// Inventory registration — mirrors [`crate::admin::report`] /
/// [`crate::admin::model_admin`].
pub struct ContentCheckRegistration {
    pub factory: fn() -> Box<dyn ContentCheck>,
}

inventory::collect!(ContentCheckRegistration);

/// Register a [`ContentCheck`] implementation.
///
/// ```ignore
/// #[derive(Default)]
/// struct MyCheck;
/// impl rustango_cms::content_checks::ContentCheck for MyCheck { /* … */ }
/// rustango_cms::register_content_check!(MyCheck);
/// ```
#[macro_export]
macro_rules! register_content_check {
    ($ty:ty) => {
        $crate::inventory::submit! {
            $crate::content_checks::ContentCheckRegistration {
                factory: || ::std::boxed::Box::new(<$ty>::default()),
            }
        }
    };
}

/// Every registered content check, sorted by `code` for a stable order.
#[must_use]
pub fn registered_checks() -> Vec<Box<dyn ContentCheck>> {
    let mut v: Vec<Box<dyn ContentCheck>> = inventory::iter::<ContentCheckRegistration>
        .into_iter()
        .map(|r| (r.factory)())
        .collect();
    v.sort_by_key(|c| c.code());
    v
}

/// Run every registered check against `page`, returning a stable-sorted
/// list (errors first, then warnings, then info; ties keep registry
/// order). Drives the editor's "Content & SEO" panel.
#[must_use]
pub fn run_all(page: &Page) -> Vec<Finding> {
    let mut out = Vec::new();
    for check in registered_checks() {
        out.extend(check.run(page));
    }
    out.sort_by_key(|f| match f.severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    });
    out
}

/// Counts grouped by severity — drives the panel badge.
#[must_use]
pub fn count_by_severity(findings: &[Finding]) -> (usize, usize, usize) {
    let mut errors = 0;
    let mut warnings = 0;
    let mut infos = 0;
    for f in findings {
        match f.severity {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            Severity::Info => infos += 1,
        }
    }
    (errors, warnings, infos)
}

// ============================================================ built-in SEO checks

/// Recommended meta-description length floor — shorter than this and
/// search engines have little to work with.
const MIN_SEO_DESCRIPTION_CHARS: usize = 50;

/// **Empty meta description** (SEO check). A page with no
/// `seo_description` lets the search engine synthesise a snippet from
/// the body — usually worse than an authored one. Warning (it's
/// recommended, not required).
#[derive(Default)]
pub struct EmptySeoDescriptionCheck;

impl ContentCheck for EmptySeoDescriptionCheck {
    fn code(&self) -> &'static str {
        "empty_seo_description"
    }
    fn run(&self, page: &Page) -> Vec<Finding> {
        if page.seo_description.trim().is_empty() {
            vec![Finding {
                code: "empty_seo_description",
                category: "SEO",
                severity: Severity::Warning,
                message: "No meta description — search engines will synthesise a snippet \
                          from the page body, which is usually worse than an authored one."
                    .to_owned(),
                field: Some("seo_description"),
            }]
        } else {
            Vec::new()
        }
    }
}

/// **Thin meta description** — present but very short, so it under-uses
/// the SERP snippet. Info (a nudge, not a problem). Skipped when the
/// description is empty (the empty check already fired).
#[derive(Default)]
pub struct ThinSeoDescriptionCheck;

impl ContentCheck for ThinSeoDescriptionCheck {
    fn code(&self) -> &'static str {
        "thin_seo_description"
    }
    fn run(&self, page: &Page) -> Vec<Finding> {
        let len = page.seo_description.trim().chars().count();
        if len > 0 && len < MIN_SEO_DESCRIPTION_CHARS {
            vec![Finding {
                code: "thin_seo_description",
                category: "SEO",
                severity: Severity::Info,
                message: format!(
                    "Meta description is only {len} characters — aim for ~50–160 to fill \
                     the search snippet."
                ),
                field: Some("seo_description"),
            }]
        } else {
            Vec::new()
        }
    }
}

/// **Missing social share image** — no `og:image`, so link unfurls on
/// social / chat have no preview image. Info.
#[derive(Default)]
pub struct MissingOgImageCheck;

impl ContentCheck for MissingOgImageCheck {
    fn code(&self) -> &'static str {
        "missing_og_image"
    }
    fn run(&self, page: &Page) -> Vec<Finding> {
        if page.og_image_media_id.is_none() {
            vec![Finding {
                code: "missing_og_image",
                category: "SEO",
                severity: Severity::Info,
                message: "No social share image (og:image) — links to this page won't show \
                          a preview image when shared."
                    .to_owned(),
                field: Some("og_image_media_id"),
            }]
        } else {
            Vec::new()
        }
    }
}

register_content_check!(EmptySeoDescriptionCheck);
register_content_check!(ThinSeoDescriptionCheck);
register_content_check!(MissingOgImageCheck);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PageStatus;
    use rustango::sql::Auto;

    /// Minimal valid page for check fixtures — every field a check
    /// reads is set; the rest get sensible zero values.
    fn sample_page() -> Page {
        Page {
            id: Auto::Unset,
            page_type_id: 1,
            title: "Hello".to_owned(),
            slug: "hello".to_owned(),
            path: "0001".to_owned(),
            url_path: "/hello".to_owned(),
            preview_path: String::new(),
            template_override: String::new(),
            depth: 1,
            parent_id: None,
            locale_variant_of: None,
            alias_of: None,
            theme_id: None,
            sort_order: 0,
            status: PageStatus::Draft.as_str().to_owned(),
            published_at: None,
            last_published_at: None,
            go_live_at: None,
            expire_at: None,
            seo_title: String::new(),
            seo_description: "A good, sufficiently long meta description for this page.".to_owned(),
            robots_index: true,
            sitemap_priority: 0.5,
            show_in_menus: true,
            og_title: String::new(),
            og_description: String::new(),
            og_image_media_id: Some(7),
            twitter_card: "summary".to_owned(),
            notification_pre_published_sent: false,
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        }
    }

    #[test]
    fn three_builtin_checks_register() {
        let codes: Vec<&str> = registered_checks().iter().map(|c| c.code()).collect();
        for code in [
            "empty_seo_description",
            "thin_seo_description",
            "missing_og_image",
        ] {
            assert!(codes.contains(&code), "check `{code}` should register");
        }
    }

    #[test]
    fn clean_page_has_no_seo_findings() {
        let findings = run_all(&sample_page());
        assert!(
            findings.is_empty(),
            "a page with a good description + og image is clean: {findings:?}"
        );
    }

    #[test]
    fn empty_description_flags_warning() {
        let mut p = sample_page();
        p.seo_description = String::new();
        let findings = run_all(&p);
        let f = findings
            .iter()
            .find(|f| f.code == "empty_seo_description")
            .expect("empty description flagged");
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.category, "SEO");
        // Empty (not thin) — the thin check must not double-fire.
        assert!(!findings.iter().any(|f| f.code == "thin_seo_description"));
    }

    #[test]
    fn thin_description_flags_info() {
        let mut p = sample_page();
        p.seo_description = "Too short.".to_owned();
        let findings = run_all(&p);
        assert!(findings
            .iter()
            .any(|f| f.code == "thin_seo_description" && f.severity == Severity::Info));
    }

    #[test]
    fn missing_og_image_flags_info() {
        let mut p = sample_page();
        p.og_image_media_id = None;
        let findings = run_all(&p);
        assert!(findings.iter().any(|f| f.code == "missing_og_image"));
    }

    #[test]
    fn findings_sorted_errors_then_warnings_then_info() {
        let mut p = sample_page();
        p.seo_description = String::new(); // warning
        p.og_image_media_id = None; // info
        let findings = run_all(&p);
        let sevs: Vec<Severity> = findings.iter().map(|f| f.severity).collect();
        let mut sorted = sevs.clone();
        sorted.sort_by_key(|s| match s {
            Severity::Error => 0,
            Severity::Warning => 1,
            Severity::Info => 2,
        });
        assert_eq!(sevs, sorted);
        let (e, w, i) = count_by_severity(&findings);
        assert_eq!((e, w, i), (0, 1, 1));
    }
}
