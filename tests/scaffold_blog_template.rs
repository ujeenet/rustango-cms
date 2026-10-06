//! #620 — the scaffold's blog article template renders a published article
//! whose body hasn't been written yet, instead of failing the page.

use tera::{Context, Tera};

#[test]
fn an_article_with_no_body_yet_renders() {
    let mut tera = Tera::default();
    rustango_cms::admin::register_templates(&mut tera).expect("cms filters");
    tera.add_raw_templates(vec![
        ("_site.css.html", ""),
        ("article_page.html", include_str!("../crates/rcms-scaffold/assets/article_page_blog.html")),
    ])
    .expect("template parses");
    let mut ctx = Context::new();
    ctx.insert("page", &serde_json::json!({
        "title": "Hello", "seo_title": "", "seo_description": "", "robots_index": true, "url_path": "/hello"
    }));
    ctx.insert("extension", &serde_json::json!({}));
    ctx.insert("ancestors", &Vec::<serde_json::Value>::new());
    ctx.insert("LANG", "en");
    let html = tera.render("article_page.html", &ctx).unwrap_or_else(|e| panic!("render: {e:?}"));
    assert!(html.contains("Hello"));
}
