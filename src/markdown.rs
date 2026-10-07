//! Server-side Markdown → HTML rendering for page bodies.
//!
//! Authors write markdown in the admin (via the extension-field
//! editor). At render time, `pulldown-cmark` converts to HTML; the
//! result is then run through `ammonia` so any embedded `<script>`,
//! event handlers, `javascript:` hrefs, or other XSS vectors are
//! stripped before reaching the browser.
//!
//! Exposed as the Tera filter `markdown`. Use it like:
//!
//! ```tera
//! {{ extension.body_markdown | markdown | safe }}
//! ```
//!
//! The `| safe` is required — the filter returns trusted HTML, and
//! without `safe` Tera would re-escape the angle brackets.

use pulldown_cmark::{html, Options, Parser};

/// Convert a markdown source string to sanitized HTML.
///
/// Enables the common GFM extensions (tables, strikethrough, task
/// lists, autolinks, footnotes) so authors get the syntax they expect
/// from GitHub / Notion / Obsidian. The post-pass through ammonia
/// keeps it XSS-safe: only a curated tag/attribute allowlist
/// survives.
#[must_use]
pub fn render(src: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(src, opts);
    let mut html_out = String::with_capacity(src.len() + src.len() / 4);
    html::push_html(&mut html_out, parser);
    base_sanitizer().clean(&html_out).to_string()
}

/// The shared ammonia allow-list: the default policy PLUS `class` on
/// `<code>` / `<pre>` so fenced code blocks keep their `language-…`
/// hint (`pulldown-cmark` emits `<code class="language-rust">` for a
/// ```rust fence) for client-side highlighters. A class is
/// inert — no script / URL vector — so allowing it is XSS-safe.
fn base_sanitizer() -> ammonia::Builder<'static> {
    let mut b = ammonia::Builder::default();
    b.add_tag_attributes("code", ["class"]);
    b.add_tag_attributes("pre", ["class"]);
    b
}

/// Sanitize an HTML string for richtext storage / preview. Uses the same
/// ammonia allow-list as the markdown renderer, PLUS the move-safe
/// internal-link attributes `<a linktype="page|media" id="N">` that
/// the `| richtext` filter resolves to real URLs at render time. Ammonia's
/// default policy strips unknown `<a>` attributes, which would drop the
/// `linktype`/`id` pair and turn move-safe links into broken ones the next
/// time a page is moved — so they're explicitly allow-listed here. Both are
/// inert (no script/URL vector), so allowing them is XSS-safe. Used by the
/// richtext on-save sanitizer (`sanitized_extension_form`) and the
/// preview pane.
#[must_use]
pub fn sanitize_html(src: &str) -> String {
    let mut builder = base_sanitizer();
    builder.add_tag_attributes("a", ["linktype", "id"]);
    builder.clean(src).to_string()
}

/// Register the `markdown` filter on a Tera instance. Host apps
/// already calling `crate::translation::register_tera_filter` should
/// call this too so `{{ … | markdown }}` resolves.
pub fn register_tera_filter(tera: &mut tera::Tera) {
    tera.register_filter("markdown", markdown_filter);
}

fn markdown_filter(
    value: &tera::Value,
    _args: &std::collections::HashMap<String, tera::Value>,
) -> tera::Result<tera::Value> {
    let Some(src) = value.as_str() else {
        return Ok(value.clone());
    };
    if src.is_empty() {
        return Ok(tera::Value::String(String::new()));
    }
    Ok(tera::Value::String(render(src)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_basic_markdown() {
        let html = render("# Hello\n\nA *world* of [links](https://example.com).");
        assert!(html.contains("<h1>Hello</h1>"));
        assert!(html.contains("<em>world</em>"));
        assert!(html.contains(r#"<a href="https://example.com""#));
    }

    #[test]
    fn strips_inline_script() {
        let html = render("Hello <script>alert(1)</script> world");
        assert!(!html.contains("<script>"));
        assert!(!html.contains("alert"));
    }

    #[test]
    fn strips_javascript_href() {
        let html = render("[click](javascript:alert(1))");
        // ammonia rewrites unsafe schemes to nothing-or-text — what
        // matters is the JS doesn't survive.
        assert!(!html.contains("javascript:"));
    }

    #[test]
    fn supports_tables() {
        let html = render("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(html.contains("<table>"));
        assert!(html.contains("<th>a</th>"));
    }

    #[test]
    fn empty_input_empty_output() {
        assert_eq!(render(""), "");
    }

    // #263 — `sanitize_html` is the entry point the richtext preview
    // endpoint uses. Pin the same XSS guarantees the markdown path
    // gets: no script tags, no javascript: hrefs.
    #[test]
    fn sanitize_html_strips_script_tags() {
        let out = sanitize_html("<p>Hi <script>alert(1)</script></p>");
        assert!(!out.contains("<script>"), "got `{out}`");
        assert!(!out.contains("alert(1)"), "got `{out}`");
        assert!(out.contains("<p>"), "got `{out}`");
    }

    #[test]
    fn sanitize_html_keeps_safe_richtext_shape() {
        let out = sanitize_html(r#"<h2>Title</h2><p>Body <a href="/about">link</a></p>"#);
        assert!(out.contains("<h2>Title</h2>"));
        assert!(out.contains(r#"<a href="/about""#));
    }

    #[test]
    fn sanitize_html_rejects_javascript_href() {
        let out = sanitize_html(r#"<a href="javascript:alert(1)">x</a>"#);
        assert!(!out.contains("javascript:"), "got `{out}`");
    }

    #[test]
    fn sanitize_html_empty_input_empty_output() {
        assert_eq!(sanitize_html(""), "");
    }

    // #403 — fenced code blocks must keep their `language-…` class so
    // client-side highlighters (Prism / highlight.js) can colour them.
    #[test]
    fn fenced_code_keeps_language_class() {
        let html = render("```rust\nfn main() {}\n```");
        assert!(
            html.contains(r#"class="language-rust""#),
            "language hint must survive the sanitizer; got `{html}`"
        );
        // The body is still escaped (no live markup from code content).
        let escaped = render("```html\n<script>x</script>\n```");
        assert!(!escaped.contains("<script>"), "got `{escaped}`");
        assert!(escaped.contains("&lt;script&gt;"), "got `{escaped}`");
    }

    #[test]
    fn sanitize_html_keeps_code_language_class() {
        let out = sanitize_html(r#"<pre><code class="language-python">x = 1</code></pre>"#);
        assert!(
            out.contains(r#"class="language-python""#),
            "richtext code class must survive; got `{out}`"
        );
    }

    #[test]
    fn sanitize_html_still_strips_event_handlers_on_code() {
        // Allowing `class` on code must not open the door to other
        // attributes — onclick etc. still go.
        let out = sanitize_html(r#"<code class="language-x" onclick="alert(1)">x</code>"#);
        assert!(out.contains(r#"class="language-x""#), "got `{out}`");
        assert!(!out.contains("onclick"), "got `{out}`");
    }

    #[test]
    fn sanitize_html_keeps_mailto_and_anchor_links() {
        // #399 — email + in-page anchor links must survive on-save
        // sanitization (mailto is a default-allowed scheme; `#frag` is a
        // relative URL, passed through like `/about`).
        let mail = sanitize_html(r#"<a href="mailto:hi@example.com">mail</a>"#);
        assert!(
            mail.contains(r#"href="mailto:hi@example.com""#),
            "got `{mail}`"
        );
        let anchor = sanitize_html(r##"<a href="#section-2">jump</a>"##);
        assert!(anchor.contains(r##"href="#section-2""##), "got `{anchor}`");
    }

    #[test]
    fn sanitize_html_preserves_move_safe_linktype_anchors() {
        // #294 — the `<a linktype="page|media" id="N">` storage shape the
        // `| richtext` filter resolves must survive on-save sanitization.
        let out = sanitize_html(r#"<a linktype="page" id="42">Team</a>"#);
        assert!(out.contains(r#"linktype="page""#), "got `{out}`");
        assert!(out.contains(r#"id="42""#), "got `{out}`");
        let media = sanitize_html(r#"<a linktype="media" id="7">Doc</a>"#);
        assert!(
            media.contains(r#"linktype="media""#) && media.contains(r#"id="7""#),
            "got `{media}`"
        );
    }

    #[test]
    fn sanitize_html_preserves_richtext_tables_and_images() {
        // #294 (C) — the TipTap editor emits tables + images; both must
        // survive on-save sanitization (ammonia's default allow-list
        // covers table/thead/tbody/tr/th/td + img).
        // The real editor emits colgroup + per-cell colspan/rowspan + a
        // cosmetic `style` (which ammonia drops — harmless). The table
        // STRUCTURE + cell content + colspan must survive.
        let table = sanitize_html(
            r#"<table style="min-width:50px"><colgroup><col style="min-width:25px"></colgroup><tbody><tr><th colspan="1" rowspan="1"><p>A</p></th></tr><tr><td colspan="1" rowspan="1"><p>1</p></td></tr></tbody></table>"#,
        );
        assert!(
            table.contains("<table")
                && table.contains("<th")
                && table.contains("<td")
                && table.contains(r#"colspan="1""#)
                && table.contains("A")
                && table.contains("1"),
            "table structure stripped: `{table}`"
        );
        // Media serve URLs are id-stable, so a plain <img src> is move-safe.
        let img = sanitize_html(r#"<p><img src="/media/serve/9/x.png" alt="pic"></p>"#);
        assert!(
            img.contains(r#"src="/media/serve/9/x.png""#) && img.contains(r#"alt="pic""#),
            "img stripped: `{img}`"
        );
    }

    #[test]
    fn sanitize_html_still_strips_dangerous_attrs_on_linktype_anchor() {
        // allow-listing linktype/id must NOT open an XSS hole on <a>.
        let out = sanitize_html(
            r#"<a linktype="page" id="1" onclick="alert(1)" href="javascript:alert(2)">x</a>"#,
        );
        assert!(out.contains(r#"linktype="page""#), "got `{out}`");
        assert!(!out.contains("onclick"), "got `{out}`");
        assert!(!out.contains("javascript:"), "got `{out}`");
    }
}
