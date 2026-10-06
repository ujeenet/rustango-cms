//! `| richtext` Tera filter — Wagtail
//! `{{ page.body | richtext }}` parity.
//!
//! Wagtail's `expand_db_html` rewrites the storage shape the editor
//! emits — `<a linktype="page" id="42">…</a>` — into the renderable
//! shape — `<a href="/about/team">…</a>` — at template-render time.
//! Storing page-id references in rich-text bodies means anchors keep
//! working when pages move (Wagtail's whole reason for the dance).
//!
//! This filter does the same:
//!   - rewrites `<a linktype="page" id="N">` → `<a href="<page url>">`
//!   - rewrites `<a linktype="media" id="N">` → `<a href="<media url>">`
//!   - marks the output safe (`is_safe = true`) so Tera doesn't
//!     double-escape on top of the editor's already-HTML-escaped
//!     content
//!   - missing / unresolvable ids fall back to `href="#"` — the
//!     anchor still renders, just inert (matches Wagtail's "broken
//!     link" placeholder)
//!
//! Resolution maps:
//!   - **Page urls** — reuses [`crate::page_url`]'s thread-local
//!     (populated by the public-render handler via
//!     [`crate::page_url::install`] in [`crate::render`]). Any
//!     extension-level `*_id` field that pointed at a page is
//!     resolvable through here too.
//!   - **Everything else** — anchors the filter cannot resolve are left
//!     intact, and [`resolve_internal_links`] rewrites them over the
//!     page's final HTML, where stream blocks' links are visible too:
//!     one query for the pages, one for the media (#682).
//!
//! ## Usage
//!
//! ```text
//! {{ extension.hero_body | richtext }}
//! ```
//!
//! ## Implementation note
//!
//! Avoids pulling `html5ever` / `ammonia` in for what's effectively a
//! constrained-shape rewrite. The editor's output for internal links
//! is well-formed (always `<a` followed by whitespace + a single
//! `>`-terminated open tag), so a small string scanner that walks
//! `<a ...>` openings and edits the attribute set in place is
//! sufficient. Anything that doesn't match `linktype="page|media"` +
//! `id="…"` is left verbatim — including arbitrary editor-emitted
//! markup like `<img>` / `<ul>` / `<strong>`.

use std::collections::HashMap;

use tera::{Filter, Tera, Value};

/// Register the `richtext` Tera filter. Called once at Tera setup —
/// wired from [`crate::urls::register_tera_helpers`].
pub fn register_tera_filter(tera: &mut Tera) {
    tera.register_filter("richtext", RichtextFilter);
}

struct RichtextFilter;

impl Filter for RichtextFilter {
    fn filter(&self, value: &Value, _args: &HashMap<String, Value>) -> tera::Result<Value> {
        let html = value.as_str().unwrap_or("");
        // Resolve what the render already knows (pages an extension
        // `*_id` field points at); leave every other anchor intact for
        // `resolve_internal_links`, which sees the whole page.
        Ok(Value::String(expand_db_html(html, &|kind, id| match kind {
            LinkKind::Page => crate::page_url::lookup(id),
            LinkKind::Media => None,
        }, false)))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

/// Walk `html` and rewrite every `<a linktype="page|media" id="N">`
/// opening tag in place. Other markup (text, non-`<a>` tags,
/// already-resolved `<a href=…>` without `linktype`) passes through
/// unchanged.
/// Whether an anchor links a page or a media item (a document).
#[derive(Clone, Copy, PartialEq, Eq)]
enum LinkKind {
    Page,
    Media,
}

/// `resolve` maps an anchor to its URL. `fallback` decides what happens
/// to an anchor it cannot resolve: `true` rewrites it to `href="#"`,
/// `false` leaves it verbatim for a later pass.
fn expand_db_html(html: &str, resolve: &dyn Fn(LinkKind, i64) -> Option<String>, fallback: bool) -> String {
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0;
    while cursor < html.len() {
        // Find the next `<a` and confirm the char after it is
        // whitespace (filters out `<address>`, `<area>`, `<aside>`).
        let Some(rel) = html[cursor..].find("<a") else {
            out.push_str(&html[cursor..]);
            break;
        };
        let open = cursor + rel;
        out.push_str(&html[cursor..open]);
        let after_a = open + 2;
        let next_ch = html[after_a..].chars().next();
        if !matches!(next_ch, Some(c) if c.is_ascii_whitespace()) {
            // `<an…>` or `<area>` etc. — keep `<a` verbatim and resume.
            out.push_str("<a");
            cursor = after_a;
            continue;
        }
        // Find the `>` that closes this opening tag. The editor never
        // emits `>` inside an attribute value, so a simple search is
        // safe for the rewrite-target shape.
        let Some(close_rel) = html[after_a..].find('>') else {
            // Malformed tail — copy verbatim and stop.
            out.push_str(&html[open..]);
            break;
        };
        let close = after_a + close_rel;
        let attrs = &html[after_a..close];
        match rewrite_anchor(attrs, resolve, fallback) {
            Some(new_open) => out.push_str(&new_open),
            None => out.push_str(&html[open..=close]),
        }
        cursor = close + 1;
    }
    out
}

/// The `(kind, id)` an internal-link anchor points at, from the attribute
/// slice of its opening tag. `None` for any other anchor.
fn link_target(attrs: &str) -> Option<(LinkKind, i64)> {
    let kind = match attr_value(attrs, "linktype")? {
        "page" => LinkKind::Page,
        "media" => LinkKind::Media,
        _ => return None,
    };
    let id: i64 = attr_value(attrs, "id")?.parse().ok()?;
    (id > 0).then_some((kind, id))
}

/// The attribute slices of every `<a …>` opening tag in `html`.
fn anchor_attrs(html: &str) -> impl Iterator<Item = &str> {
    let mut cursor = 0;
    std::iter::from_fn(move || {
        while let Some(rel) = html.get(cursor..)?.find("<a") {
            let after_a = cursor + rel + 2;
            let is_anchor = html[after_a..].chars().next().is_some_and(|c| c.is_ascii_whitespace());
            let close = html[after_a..].find('>').map(|c| after_a + c);
            cursor = close.map_or(html.len(), |c| c + 1);
            if is_anchor {
                if let Some(close) = close {
                    return Some(&html[after_a..close]);
                }
            }
        }
        None
    })
}

/// Rewrite every internal-link anchor in a rendered page (#682).
///
/// Rich text stores links as `<a linktype="page|media" id="N">` so they
/// survive moves. The `| richtext` filter only knows the pages an
/// extension field points at, and stream blocks are rendered before any
/// lookup is available, so the page's final HTML is where every anchor is
/// visible at once. One query per kind resolves them; an id that no
/// longer exists becomes an inert `href="#"`.
pub async fn resolve_internal_links(pool: &rustango::sql::Pool, html: String) -> String {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    if !html.contains("linktype=") {
        return html;
    }
    let (mut page_ids, mut media_ids) = (Vec::new(), Vec::new());
    for (kind, id) in anchor_attrs(&html).filter_map(link_target) {
        match kind {
            LinkKind::Page => page_ids.push(id),
            LinkKind::Media => media_ids.push(id),
        }
    }
    for ids in [&mut page_ids, &mut media_ids] {
        ids.sort_unstable();
        ids.dedup();
    }
    let mut pages = HashMap::new();
    if !page_ids.is_empty() {
        match crate::page::Page::objects()
            .where_(crate::page::Page::id.is_in(page_ids))
            .fetch(pool)
            .await
        {
            Ok(rows) => pages.extend(rows.into_iter().filter_map(|p| p.id.get().copied().map(|id| (id, p.url_path)))),
            Err(e) => tracing::warn!(target: "rustango_cms::richtext", error = %e, "page link lookup failed"),
        }
    }
    let mut media = HashMap::new();
    if !media_ids.is_empty() {
        match crate::media::Media::objects()
            .where_(crate::media::Media::id.is_in(media_ids))
            .fetch(pool)
            .await
        {
            Ok(rows) => media.extend(rows.into_iter().filter_map(|m| m.id.get().copied().map(|id| (id, m.public_url())))),
            Err(e) => tracing::warn!(target: "rustango_cms::richtext", error = %e, "media link lookup failed"),
        }
    }
    expand_db_html(
        &html,
        &|kind, id| match kind {
            LinkKind::Page => pages.get(&id).cloned(),
            LinkKind::Media => media.get(&id).cloned(),
        },
        true,
    )
}

/// Inspect the attribute slice of a `<a … >` opening tag. If it
/// carries `linktype="page"` + `id="N"` (or `linktype="media"`),
/// return the rewritten opening tag with `href` resolved. Otherwise
/// return `None` so the caller keeps the original.
///
/// Other attributes on the anchor (`class`, `target`, `rel`, etc.)
/// are preserved verbatim; `linktype` and `id` are stripped from the
/// output since they're internal markers, not browser-renderable.
fn rewrite_anchor(
    attrs: &str,
    resolve: &dyn Fn(LinkKind, i64) -> Option<String>,
    fallback: bool,
) -> Option<String> {
    let (kind, id) = link_target(attrs)?;
    let href = match resolve(kind, id) {
        Some(url) => url,
        None if fallback => "#".to_owned(),
        None => return None,
    };
    let extra = remaining_attrs(attrs, &["linktype", "id"]);
    let extra_trim = extra.trim();
    if extra_trim.is_empty() {
        Some(format!(r#"<a href="{}">"#, html_attr_escape(&href)))
    } else {
        Some(format!(
            r#"<a href="{}" {}>"#,
            html_attr_escape(&href),
            extra_trim
        ))
    }
}

/// Read the value of `name="value"` from `attrs`. Accepts double-
/// or single-quoted strings. Returns `None` when the attribute is
/// absent. False-positive risk (`name` appearing inside an unrelated
/// attribute value) is negligible for editor-emitted richtext.
fn attr_value<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    for (open, close) in [("\"", '"'), ("'", '\'')] {
        let pattern = format!("{name}={open}");
        let mut search = attrs;
        let mut absolute_offset = 0usize;
        while let Some(start) = search.find(&pattern) {
            // Confirm `name` is at an attribute boundary — preceded by
            // whitespace or the start of the slice. Without this guard
            // `id="…"` matches inside `data-id="…"`.
            let abs_start = absolute_offset + start;
            if abs_start > 0 {
                let prev = attrs[..abs_start].chars().next_back();
                if !matches!(prev, Some(c) if c.is_ascii_whitespace()) {
                    let step = start + pattern.len();
                    absolute_offset += step;
                    search = &attrs[absolute_offset..];
                    continue;
                }
            }
            let value_start = abs_start + pattern.len();
            let end_rel = attrs[value_start..].find(close)?;
            return Some(&attrs[value_start..value_start + end_rel]);
        }
    }
    None
}

/// Re-emit `attrs` with every attribute in `strip` removed. Used to
/// drop `linktype` and `id` from the rewritten anchor while
/// preserving everything else (`class`, `target`, `rel`, …).
fn remaining_attrs(attrs: &str, strip: &[&str]) -> String {
    let mut out = String::with_capacity(attrs.len());
    let mut chars = attrs.char_indices().peekable();
    'outer: while let Some(&(i, c)) = chars.peek() {
        // Skip leading whitespace
        if c.is_ascii_whitespace() {
            out.push(c);
            chars.next();
            continue;
        }
        // Parse one attribute: name [= "value" | = 'value' | (no value)]
        let name_start = i;
        let mut name_end = name_start;
        while let Some(&(j, ch)) = chars.peek() {
            if ch == '=' || ch.is_ascii_whitespace() {
                break;
            }
            name_end = j + ch.len_utf8();
            chars.next();
        }
        let name = &attrs[name_start..name_end];
        // Skip any spaces between name and `=`
        while let Some(&(_, ch)) = chars.peek() {
            if ch.is_ascii_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
        // Optional `="value"` clause
        let mut value_end = name_end;
        if let Some(&(_, '=')) = chars.peek() {
            chars.next();
            while let Some(&(_, ch)) = chars.peek() {
                if ch.is_ascii_whitespace() {
                    chars.next();
                } else {
                    break;
                }
            }
            let quote = match chars.peek() {
                Some(&(_, '"')) => Some('"'),
                Some(&(_, '\'')) => Some('\''),
                _ => None,
            };
            if let Some(q) = quote {
                chars.next(); // consume opening quote
                while let Some(&(j, ch)) = chars.peek() {
                    chars.next();
                    if ch == q {
                        value_end = j + ch.len_utf8();
                        break;
                    }
                    value_end = j + ch.len_utf8();
                }
            } else {
                // Unquoted value — until whitespace or end
                while let Some(&(j, ch)) = chars.peek() {
                    if ch.is_ascii_whitespace() {
                        break;
                    }
                    value_end = j + ch.len_utf8();
                    chars.next();
                }
            }
        }
        let chunk = &attrs[name_start..value_end];
        if strip.iter().any(|s| name.eq_ignore_ascii_case(s)) {
            // Drop this attribute entirely. Don't emit anything; the
            // surrounding whitespace handling above already pads.
            continue 'outer;
        }
        // Re-emit with a single leading space if `out` isn't empty
        // and doesn't already end in whitespace.
        if !out.is_empty() && !out.ends_with(|c: char| c.is_ascii_whitespace()) {
            out.push(' ');
        }
        out.push_str(chunk);
    }
    out
}

/// Escape `&`, `"`, `<`, `>` for safe insertion into a `href="…"`
/// attribute. Anchor targets come from `cms_page.url_path` (CMS-
/// controlled) or media filenames, but the escape keeps a defense in
/// depth in case either source ever lands with user-supplied chars.
fn html_attr_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tera::{Context, Tera};

    fn fresh_tera() -> Tera {
        let mut tera = Tera::default();
        register_tera_filter(&mut tera);
        tera
    }

    fn render(tera: &Tera, src: &str, body: &str) -> String {
        let mut t = tera.clone();
        t.add_raw_template("t.html", src).unwrap();
        let mut ctx = Context::new();
        ctx.insert("body", body);
        t.render("t.html", &ctx).unwrap()
    }

    fn install_pages(pairs: &[(i64, &str)]) -> crate::page_url::PageUrlGuard {
        let mut m = HashMap::new();
        for (id, url) in pairs {
            m.insert(*id, (*url).to_owned());
        }
        crate::page_url::install(m)
    }

    #[test]
    fn rewrites_page_link() {
        let _g = install_pages(&[(42, "/about/team")]);
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{{ body | richtext }}",
            r#"<p>See <a linktype="page" id="42">the team</a>.</p>"#,
        );
        assert_eq!(out, r#"<p>See <a href="/about/team">the team</a>.</p>"#,);
    }

    #[test]
    fn an_unknown_page_is_left_for_the_page_pass() {
        let _g = install_pages(&[(1, "/known")]);
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{{ body | richtext }}",
            r#"<a linktype="page" id="999">missing</a>"#,
        );
        assert_eq!(out, r#"<a linktype="page" id="999">missing</a>"#);
    }

    #[test]
    fn the_page_pass_rewrites_page_and_media_links_and_inerts_the_rest() {
        let html = r#"<a linktype="page" id="3" class="x">p</a> <a linktype="media" id="7">pdf</a> <a linktype="media" id="8">gone</a>"#;
        let out = super::expand_db_html(
            html,
            &|kind, id| match (kind, id) {
                (super::LinkKind::Page, 3) => Some("/about".to_owned()),
                (super::LinkKind::Media, 7) => Some("/__media__/raw/7".to_owned()),
                _ => None,
            },
            true,
        );
        assert_eq!(
            out,
            r##"<a href="/about" class="x">p</a> <a href="/__media__/raw/7">pdf</a> <a href="#">gone</a>"##
        );
    }

    #[test]
    fn internal_link_targets_are_collected_from_every_anchor() {
        let html = r#"<p><a href="/plain">x</a><a linktype="page" id="4">a</a><abbr>n</abbr><a linktype="media" id="9">b</a><a linktype="other" id="1">c</a></p>"#;
        let found: Vec<_> = super::anchor_attrs(html).filter_map(super::link_target).collect();
        assert!(found == vec![(super::LinkKind::Page, 4), (super::LinkKind::Media, 9)]);
    }

    #[test]
    fn with_no_map_the_filter_leaves_internal_links_intact() {
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{{ body | richtext }}",
            r#"<a linktype="page" id="5">x</a><a linktype="media" id="6">y</a>"#,
        );
        assert_eq!(out, r#"<a linktype="page" id="5">x</a><a linktype="media" id="6">y</a>"#);
    }

    #[test]
    fn preserves_unrelated_anchors() {
        let _g = install_pages(&[]);
        let tera = fresh_tera();
        let html = r#"<a href="https://example.com" class="ext">link</a>"#;
        let out = render(&tera, "{{ body | richtext }}", html);
        // `linktype` absent → tag passes through verbatim.
        assert_eq!(out, html);
    }

    #[test]
    fn preserves_extra_attributes_on_rewritten_anchor() {
        let _g = install_pages(&[(8, "/services")]);
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{{ body | richtext }}",
            r#"<a linktype="page" id="8" class="cta" target="_blank">go</a>"#,
        );
        assert_eq!(
            out,
            r#"<a href="/services" class="cta" target="_blank">go</a>"#,
        );
    }

    #[test]
    fn handles_attribute_order_id_first() {
        let _g = install_pages(&[(3, "/blog")]);
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{{ body | richtext }}",
            r#"<a id="3" linktype="page">blog</a>"#,
        );
        assert_eq!(out, r#"<a href="/blog">blog</a>"#);
    }

    #[test]
    fn handles_multiple_links_in_one_body() {
        let _g = install_pages(&[(1, "/one"), (2, "/two")]);
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{{ body | richtext }}",
            r#"<p>A: <a linktype="page" id="1">one</a> B: <a linktype="page" id="2">two</a></p>"#,
        );
        assert_eq!(
            out,
            r#"<p>A: <a href="/one">one</a> B: <a href="/two">two</a></p>"#,
        );
    }

    #[test]
    fn output_is_marked_safe() {
        // If `is_safe` weren't true, Tera autoescape on `.html`
        // templates would convert `<` → `&lt;`. The fact that the
        // rendered output contains a literal `<a` proves the filter's
        // output is treated as raw HTML.
        let _g = install_pages(&[(1, "/x")]);
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{{ body | richtext }}",
            r#"<a linktype="page" id="1">x</a>"#,
        );
        assert!(out.starts_with("<a "), "expected literal `<a`, got `{out}`");
    }

    #[test]
    fn does_not_misfire_on_address_or_area_tags() {
        let _g = install_pages(&[]);
        let tera = fresh_tera();
        let html = r#"<address>123 Main St</address><area shape="rect">"#;
        let out = render(&tera, "{{ body | richtext }}", html);
        assert_eq!(out, html);
    }

    #[test]
    fn skips_anchor_without_linktype() {
        let _g = install_pages(&[(1, "/should-not-resolve")]);
        let tera = fresh_tera();
        let html = r#"<a id="1" class="x">no linktype</a>"#;
        let out = render(&tera, "{{ body | richtext }}", html);
        assert_eq!(out, html);
    }

    #[test]
    fn ignores_id_inside_unrelated_attribute_name() {
        // `data-id="9"` must not be picked up as the anchor's id.
        let _g = install_pages(&[(9, "/wrong"), (5, "/right")]);
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{{ body | richtext }}",
            r#"<a linktype="page" data-id="9" id="5">x</a>"#,
        );
        assert_eq!(out, r#"<a href="/right" data-id="9">x</a>"#,);
    }

    #[test]
    fn empty_body_returns_empty() {
        let tera = fresh_tera();
        let out = render(&tera, "{{ body | richtext }}", "");
        assert_eq!(out, "");
    }

    #[test]
    fn html_attr_escape_handles_quotes() {
        let _g = install_pages(&[(1, r#"/with"quote"#)]);
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{{ body | richtext }}",
            r#"<a linktype="page" id="1">x</a>"#,
        );
        // The href value was escaped — the `"` in the URL becomes
        // `&quot;` so the attribute syntax remains valid.
        assert!(out.contains(r#"href="/with&quot;quote""#), "got `{out}`");
    }
}
