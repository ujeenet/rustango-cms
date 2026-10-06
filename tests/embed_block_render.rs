//! End-to-end render of the built-in `embed` block (#402): drives
//! `EmbedBlock::render` → `extra_context` (provider discovery) →
//! `blocks/embed.html`, asserting a recognised URL becomes the
//! provider's embeddable iframe and an unrecognised URL links out
//! instead of iframing an arbitrary site.

use rustango_cms::admin;
use rustango_cms::block::builtin::EmbedBlock;
use rustango_cms::{Block, BlockRenderCtx};
use tera::Tera;

fn fresh_tera() -> Tera {
    let mut tera = Tera::default();
    admin::register_templates(&mut tera).expect("register_templates");
    tera
}

fn render(url: &str) -> String {
    let tera = fresh_tera();
    let ctx = BlockRenderCtx::new(&tera);
    let value = serde_json::json!({ "url": url });
    EmbedBlock.render(&value, &ctx).expect("render embed block")
}

#[test]
fn youtube_watch_url_becomes_responsive_embed_iframe() {
    let html = render("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    assert!(
        html.contains(r#"src="https://www.youtube.com/embed/dQw4w9WgXcQ""#),
        "watch URL should be rewritten to the embed src; got: {html}"
    );
    assert!(
        html.contains("aspect-ratio:16 / 9"),
        "responsive ratio; got: {html}"
    );
    assert!(
        html.contains("<iframe"),
        "known provider iframes; got: {html}"
    );
    assert!(
        html.contains(r#"title="YouTube embed""#),
        "a11y title; got: {html}"
    );
}

#[test]
fn vimeo_page_url_becomes_player_iframe() {
    let html = render("https://vimeo.com/123456789");
    assert!(
        html.contains(r#"src="https://player.vimeo.com/video/123456789""#),
        "vimeo page URL → player src; got: {html}"
    );
    assert!(html.contains("<iframe"), "got: {html}");
}

#[test]
fn unknown_provider_links_out_not_iframed() {
    let html = render("https://example.com/cool-video");
    assert!(
        !html.contains("<iframe"),
        "an arbitrary URL must NOT be iframed (whitelist); got: {html}"
    );
    // The href is the user's URL so it stays HTML-escaped (Tera turns
    // `/` into `&#x2F;`, which browsers decode) — assert the link
    // shape + host without depending on slash escaping.
    assert!(html.contains("<a "), "should link out; got: {html}");
    assert!(html.contains("rcms-embed-link"), "got: {html}");
    assert!(
        html.contains("example.com"),
        "links to the source host; got: {html}"
    );
}

/// #724 — a script-scheme URL renders no link at all, in either block
/// that takes an author-typed URL.
#[test]
fn script_scheme_urls_render_no_link() {
    use rustango_cms::block::builtin::UrlBlock;
    let tera = fresh_tera();
    let ctx = BlockRenderCtx::new(&tera);
    for bad in ["javascript:alert(1)", " JavaScript:alert(1)", "java\tscript:alert(1)", "data:text/html,<script>x</script>", "vbscript:x"] {
        let embed = EmbedBlock.render(&serde_json::json!({ "url": bad }), &ctx).expect("embed");
        let url = UrlBlock.render(&serde_json::json!({ "value": bad, "label": "click" }), &ctx).expect("url");
        for html in [&embed, &url] {
            assert!(!html.contains("href"), "{bad:?} must not become a link: {html}");
        }
    }
    let ok = UrlBlock
        .render(&serde_json::json!({ "value": "https://example.com/a", "label": "go" }), &ctx)
        .expect("url");
    assert!(ok.contains("href") && ok.contains("example.com"), "{ok}");
    let relative = UrlBlock.render(&serde_json::json!({ "value": "/about", "label": "About" }), &ctx).expect("url");
    assert!(relative.contains("href"), "{relative}");
}
