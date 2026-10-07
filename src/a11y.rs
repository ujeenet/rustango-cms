//! Server-side accessibility heuristics.
//!
//! Walks the canonical [`Page`] row + the page's rendered extension
//! HTML and flags common accessibility issues. Output is rendered
//! as a side panel on the page editor.
//!
//! Contrast / ARIA checks run client-side instead: the admin serves a
//! bundled axe-core against the preview pane.
//!
//! ## Checks
//!
//! | Code | Severity | What it catches |
//! |---|---|---|
//! | `empty_title` | Error | Page title blank |
//! | `seo_title_too_long` | Warning | SEO title > 70 chars (truncated in SERPs) |
//! | `seo_description_too_long` | Warning | SEO description > 200 chars |
//! | `noindex` | Info | Page is excluded from indexing |
//! | `img_missing_alt` | Error | `<img>` tag with no `alt=` attribute |
//! | `empty_link` | Warning | `<a>...</a>` with no text or only `click here` / `read more` |
//! | `skipped_heading` | Warning | Heading levels jump (h1 → h3) |
//!
//! Heuristics are deliberately forgiving — false negatives are
//! preferable to noisy false positives that train editors to ignore
//! the panel.

use crate::page::Page;

/// One accessibility issue surfaced to the editor.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Violation {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    /// Optional canonical field hint — drives a scroll-to-field link
    /// in the side panel.
    pub field: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    #[must_use]
    pub fn icon(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

/// Run every check against `page` + the optional rendered HTML body
/// the page-type handler exposed. Returns a stable-sorted list with
/// errors first, warnings second, info last.
#[must_use]
pub fn check_page(page: &Page, body_html: Option<&str>) -> Vec<Violation> {
    let mut out = Vec::new();
    check_canonical(page, &mut out);
    if let Some(html) = body_html {
        check_html(html, &mut out);
    }
    out.sort_by_key(|v| match v.severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    });
    out
}

/// Counts grouped by severity. Drives the side-panel tab badge.
#[must_use]
pub fn count_by_severity(violations: &[Violation]) -> (usize, usize, usize) {
    let mut errors = 0;
    let mut warnings = 0;
    let mut infos = 0;
    for v in violations {
        match v.severity {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            Severity::Info => infos += 1,
        }
    }
    (errors, warnings, infos)
}

fn check_canonical(page: &Page, out: &mut Vec<Violation>) {
    if page.title.trim().is_empty() {
        out.push(Violation {
            code: "empty_title",
            severity: Severity::Error,
            message: "Page title is empty — screen readers + browser tabs need a title.".to_owned(),
            field: Some("title"),
        });
    }
    if page.seo_title.chars().count() > 70 {
        out.push(Violation {
            code: "seo_title_too_long",
            severity: Severity::Warning,
            message: format!(
                "SEO title is {} chars — search engines truncate around 70.",
                page.seo_title.chars().count()
            ),
            field: Some("seo_title"),
        });
    }
    if page.seo_description.chars().count() > 200 {
        out.push(Violation {
            code: "seo_description_too_long",
            severity: Severity::Warning,
            message: format!(
                "SEO description is {} chars — search engines truncate around 200.",
                page.seo_description.chars().count()
            ),
            field: Some("seo_description"),
        });
    }
    if !page.robots_index {
        out.push(Violation {
            code: "noindex",
            severity: Severity::Info,
            message: "This page is set to noindex — search engines are asked to skip it."
                .to_owned(),
            field: Some("robots_index"),
        });
    }
}

fn check_html(html: &str, out: &mut Vec<Violation>) {
    scan_images(html, out);
    scan_links(html, out);
    scan_heading_order(html, out);
}

/// Find every `<img …>` tag and report ones without an `alt=…`
/// attribute. Empty `alt=""` counts as decorative + is allowed.
fn scan_images(html: &str, out: &mut Vec<Violation>) {
    let mut missing = 0;
    for tag in iter_tags(html, "img") {
        if !has_attr(tag, "alt") {
            missing += 1;
        }
    }
    if missing > 0 {
        out.push(Violation {
            code: "img_missing_alt",
            severity: Severity::Error,
            message: format!(
                "{missing} image{plural} missing `alt` attribute. Use `alt=\"\"` for purely decorative images.",
                plural = if missing == 1 { " is" } else { "s are" }
            ),
            field: None,
        });
    }
}

/// Find `<a>` tags whose visible text is empty or a known-bad
/// placeholder. Match is intentionally narrow — we don't want to
/// flag every "Learn more" link.
fn scan_links(html: &str, out: &mut Vec<Violation>) {
    let mut bad = 0;
    let lower = html.to_ascii_lowercase();
    let mut cursor = 0;
    while let Some(open) = lower[cursor..].find("<a ") {
        let abs_open = cursor + open;
        let Some(close_lt) = lower[abs_open..].find('>') else {
            break;
        };
        let abs_close_lt = abs_open + close_lt + 1;
        let Some(close_a) = lower[abs_close_lt..].find("</a>") else {
            break;
        };
        let abs_close_a = abs_close_lt + close_a;
        let inner = &html[abs_close_lt..abs_close_a];
        let stripped = strip_tags(inner).trim().to_ascii_lowercase();
        if stripped.is_empty()
            || stripped == "click here"
            || stripped == "here"
            || stripped == "read more"
            || stripped == "learn more"
        {
            bad += 1;
        }
        cursor = abs_close_a + 4;
    }
    if bad > 0 {
        out.push(Violation {
            code: "empty_link",
            severity: Severity::Warning,
            message: format!(
                "{bad} link{plural} have empty or generic text (e.g. \"click here\"). Use descriptive link text instead.",
                plural = if bad == 1 { "" } else { "s" }
            ),
            field: None,
        });
    }
}

/// Detect heading-order jumps. The rule: any heading that's more
/// than one level deeper than the previous one is suspicious.
/// (h1 → h2 OK; h2 → h4 NOT OK; h2 → h2 OK; descending always OK.)
fn scan_heading_order(html: &str, out: &mut Vec<Violation>) {
    let mut last_level: Option<u8> = None;
    let mut jumps = 0;
    let lower = html.to_ascii_lowercase();
    let mut cursor = 0;
    while cursor < lower.len() {
        let Some(rel) = lower[cursor..].find("<h") else {
            break;
        };
        let abs = cursor + rel;
        // Need at least one digit after `<h`.
        let after = &lower[abs + 2..];
        let level_char = after.chars().next().unwrap_or(' ');
        if !('1'..='6').contains(&level_char) {
            cursor = abs + 2;
            continue;
        }
        // Confirm next char is `>` or space (a real heading tag).
        let next = after.chars().nth(1).unwrap_or(' ');
        if next != '>' && !next.is_whitespace() {
            cursor = abs + 2;
            continue;
        }
        let level = (level_char as u8) - b'0';
        if let Some(prev) = last_level {
            if level > prev + 1 {
                jumps += 1;
            }
        }
        last_level = Some(level);
        cursor = abs + 2;
    }
    if jumps > 0 {
        out.push(Violation {
            code: "skipped_heading",
            severity: Severity::Warning,
            message: format!(
                "{jumps} heading level jump{plural} detected (e.g. h1 → h3). Use consecutive levels so screen readers can build the outline.",
                plural = if jumps == 1 { "" } else { "s" }
            ),
            field: None,
        });
    }
}

// ---------------------------------------------------------------
// tiny tag iterators (no html5ever dep)
// ---------------------------------------------------------------

/// Iterate over every opening tag in `html` whose name matches
/// `tag_lower` (case-insensitive). Yields the raw text of the
/// opening tag (e.g. `<img src="x" alt="y">`).
fn iter_tags<'a>(html: &'a str, tag_lower: &'a str) -> impl Iterator<Item = &'a str> {
    TagIter {
        html,
        tag_lower,
        cursor: 0,
    }
}

struct TagIter<'a> {
    html: &'a str,
    tag_lower: &'a str,
    cursor: usize,
}

impl<'a> Iterator for TagIter<'a> {
    type Item = &'a str;
    fn next(&mut self) -> Option<&'a str> {
        let lower = self.html.to_ascii_lowercase();
        let needle_lower = format!("<{}", self.tag_lower);
        while self.cursor < lower.len() {
            let rel = lower[self.cursor..].find(&needle_lower)?;
            let abs = self.cursor + rel;
            let after = abs + needle_lower.len();
            // Validate the byte after the tag name is a space or `>`,
            // so `<imgs` doesn't match `<img`.
            let next = lower.as_bytes().get(after).copied().unwrap_or(b' ');
            if next != b'>' && next != b' ' && next != b'/' && next != b'\t' && next != b'\n' {
                self.cursor = after;
                continue;
            }
            let Some(gt) = lower[after..].find('>') else {
                return None;
            };
            let end = after + gt + 1;
            let slice = &self.html[abs..end];
            self.cursor = end;
            return Some(slice);
        }
        None
    }
}

fn has_attr(tag: &str, name: &str) -> bool {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let bare = name.to_owned();
    lower.contains(&needle) || lower.split_whitespace().any(|tok| tok == bare)
}

/// Strip every `<…>` from `inner`. Cheap one-pass scanner.
fn strip_tags(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut depth = 0;
    for c in inner.chars() {
        if c == '<' {
            depth += 1;
        } else if c == '>' && depth > 0 {
            depth -= 1;
        } else if depth == 0 {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PageStatus;
    use rustango::sql::Auto;

    fn sample_page() -> Page {
        Page {
            id: Auto::Set(1),
            page_type_id: 1,
            title: "Hello world".to_owned(),
            slug: "hello".to_owned(),
            path: "0001/".to_owned(),
            url_path: "/hello".to_owned(),
            preview_path: String::new(),
            template_override: String::new(),
            depth: 1,
            parent_id: None,
            locale_variant_of: None,
            alias_of: None,
            theme_id: None,
            sort_order: 0,
            status: PageStatus::Published.as_str().to_owned(),
            published_at: None,
            last_published_at: None,
            go_live_at: None,
            expire_at: None,
            seo_title: String::new(),
            seo_description: String::new(),
            robots_index: true,
            sitemap_priority: 0.5,
            show_in_menus: false,
            og_title: String::new(),
            og_description: String::new(),
            og_image_media_id: None,
            twitter_card: "summary_large_image".to_owned(),
            notification_pre_published_sent: false,
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        }
    }

    #[test]
    fn flags_empty_title() {
        let mut p = sample_page();
        p.title = String::new();
        let v = check_page(&p, None);
        assert!(v.iter().any(|x| x.code == "empty_title"));
    }

    #[test]
    fn passes_normal_page() {
        let p = sample_page();
        let v = check_page(&p, None);
        assert!(v.iter().all(|x| x.severity != Severity::Error));
    }

    #[test]
    fn flags_long_seo_title() {
        let mut p = sample_page();
        p.seo_title = "x".repeat(80);
        let v = check_page(&p, None);
        assert!(v.iter().any(|x| x.code == "seo_title_too_long"));
    }

    #[test]
    fn flags_noindex_as_info() {
        let mut p = sample_page();
        p.robots_index = false;
        let v = check_page(&p, None);
        let row = v.iter().find(|x| x.code == "noindex").expect("noindex");
        assert_eq!(row.severity, Severity::Info);
    }

    #[test]
    fn flags_img_without_alt() {
        let p = sample_page();
        let html = r#"<p><img src="foo.png"></p>"#;
        let v = check_page(&p, Some(html));
        assert!(v.iter().any(|x| x.code == "img_missing_alt"));
    }

    #[test]
    fn passes_img_with_empty_alt() {
        let p = sample_page();
        let html = r#"<img src="x.png" alt="">"#;
        let v = check_page(&p, Some(html));
        assert!(v.iter().all(|x| x.code != "img_missing_alt"));
    }

    #[test]
    fn flags_click_here_link() {
        let p = sample_page();
        let html = r#"<a href="/x">click here</a>"#;
        let v = check_page(&p, Some(html));
        assert!(v.iter().any(|x| x.code == "empty_link"));
    }

    #[test]
    fn flags_skipped_heading_h1_to_h3() {
        let p = sample_page();
        let html = "<h1>Title</h1><h3>Sub</h3>";
        let v = check_page(&p, Some(html));
        assert!(v.iter().any(|x| x.code == "skipped_heading"));
    }

    #[test]
    fn passes_consecutive_headings() {
        let p = sample_page();
        let html = "<h1>A</h1><h2>B</h2><h3>C</h3>";
        let v = check_page(&p, Some(html));
        assert!(v.iter().all(|x| x.code != "skipped_heading"));
    }

    #[test]
    fn errors_come_first_in_sort_order() {
        let mut p = sample_page();
        p.title = String::new();
        p.seo_title = "x".repeat(80);
        let v = check_page(&p, None);
        assert_eq!(v.first().unwrap().severity, Severity::Error);
    }

    #[test]
    fn count_by_severity_buckets() {
        let mut p = sample_page();
        p.title = String::new();
        p.seo_title = "x".repeat(80);
        p.robots_index = false;
        let v = check_page(&p, None);
        let (errors, warnings, infos) = count_by_severity(&v);
        assert_eq!(errors, 1);
        assert_eq!(warnings, 1);
        assert_eq!(infos, 1);
    }
}
