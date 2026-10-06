//! End-to-end render of the built-in `code` block (#403): drives
//! `CodeBlock::render` → `blocks/code.html`, asserting the
//! highlighter-ready `<pre><code class="language-…">` markup, that the
//! body is HTML-escaped (no markup injection), and that an empty
//! language omits the class.

use rustango_cms::admin;
use rustango_cms::block::builtin::CodeBlock;
use rustango_cms::{Block, BlockRenderCtx};
use tera::Tera;

fn fresh_tera() -> Tera {
    let mut tera = Tera::default();
    admin::register_templates(&mut tera).expect("register_templates");
    tera
}

fn render(value: serde_json::Value) -> String {
    let tera = fresh_tera();
    let ctx = BlockRenderCtx::new(&tera);
    CodeBlock.render(&value, &ctx).expect("render code block")
}

#[test]
fn emits_language_class_and_pre_code() {
    let html = render(serde_json::json!({
        "language": "rust",
        "code": "fn main() {}",
    }));
    assert!(
        html.contains(r#"<code class="language-rust""#),
        "got: {html}"
    );
    assert!(html.starts_with("<pre"), "got: {html}");
    assert!(html.contains("fn main() {}"), "got: {html}");
}

#[test]
fn body_is_html_escaped_no_injection() {
    let html = render(serde_json::json!({
        "language": "html",
        "code": "<script>alert(1)</script> & <b>x</b>",
    }));
    assert!(
        !html.contains("<script>"),
        "code body must be escaped, not live markup; got: {html}"
    );
    assert!(html.contains("&lt;script&gt;"), "got: {html}");
    assert!(html.contains("&amp;"), "ampersand escaped; got: {html}");
}

#[test]
fn empty_language_omits_class() {
    let html = render(serde_json::json!({ "code": "plain text" }));
    assert!(
        !html.contains("language-"),
        "no language → no class; got: {html}"
    );
    assert!(html.contains("<code>plain text</code>"), "got: {html}");
}
