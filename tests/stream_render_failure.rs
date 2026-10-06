//! #638 — one block that fails to render leaves a hidden marker; the rest
//! of the stream still renders instead of the whole body vanishing.

use rustango_cms::BlockRenderCtx;

#[test]
fn a_failing_block_does_not_blank_the_stream() {
    let mut tera = tera::Tera::default();
    rustango_cms::admin::register_templates(&mut tera).expect("templates");
    // A template that parses but fails at render time.
    tera.add_raw_template("blocks/heading.html", "{{ value.text | no_such_filter }}")
        .expect("override");
    let ctx = BlockRenderCtx::new(&tera);
    let stream = serde_json::json!([
        { "type": "heading", "id": "h", "value": { "text": "Broken" } },
        { "type": "paragraph", "id": "p", "value": { "body": "Still here" } },
        { "id": "no-type" }
    ]);
    let html = rustango_cms::block::render::prerender_stream(&stream, &ctx).expect("stream renders");
    assert!(html.contains("Still here"), "the healthy block renders: {html}");
    assert!(html.contains(r#"class="rcms-stream-error" data-block-type="heading" hidden"#), "{html}");
    assert!(html.contains(r#"data-block-type="&lt;missing type&gt;""#), "{html}");
}
