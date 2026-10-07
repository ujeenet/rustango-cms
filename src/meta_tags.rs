//! Tera helper that emits canonical `<head>` meta tags for a page.
//!
//! Hosts wire one call:
//! `{{ rcms_meta_tags(page=page, canonical=canonical_url, origin=site_origin) | safe }}`
//! and get:
//! - `<link rel="canonical">` — pass `canonical=canonical_url`
//!   (the renderer injects `canonical_url`: the source page's URL for
//!   aliases, else the page's own URL); falls back to `page.url_path`
//!   when the arg is omitted
//! - `<title>` (or skip if the host already emitted one)
//! - `<meta name="description">`
//! - `<meta name="robots">` (when robots_index = false)
//! - Open Graph (`og:title`, `og:description`, `og:image`, `og:type`)
//! - Twitter card (`twitter:card`, `twitter:title`, `twitter:description`, `twitter:image`)
//!
//! Fallback rules:
//! - Title → `page.og_title` ?? `page.seo_title` ?? `page.title`
//! - Description → `page.og_description` ?? `page.seo_description`
//! - Image → resolve `page.og_image_media_id` via `/__media__/fill-1200x630/<id>`
//!   when set; else empty (no tag emitted). The renderer fills
//!   `og_image_media_id` with the page's first photo when the editor chose
//!   none (`first_image_in_extension`, `page_builder::values::first_media_id`).
//!
//! `origin` (the renderer injects `site_origin`, e.g. `https://shop.example`)
//! makes the canonical and image URLs absolute — social networks ignore a
//! relative `og:image`.

use std::collections::HashMap;

use serde_json::Value;

/// A media id from a stored value: a positive number, or its string form
/// (form posts store `"12"`).
pub(crate) fn media_id(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|id| *id > 0)
}

/// The first photo in a code-made page type's fields, in field order: a
/// media-picker field's value, or the first `image` block of a stream
/// field (stored as JSON text or as an array).
pub(crate) fn first_image_in_extension(
    widgets: &[crate::widget::Widget],
    extension: &Value,
) -> Option<i64> {
    use crate::widget::WidgetKind;
    widgets.iter().find_map(|w| {
        let value = extension.get(&w.name)?;
        match w.kind {
            WidgetKind::MediaPicker => media_id(Some(value)),
            WidgetKind::Stream => {
                let parsed;
                let blocks = match value {
                    Value::String(s) => {
                        parsed = serde_json::from_str::<Value>(s).ok()?;
                        &parsed
                    }
                    v => v,
                };
                blocks.as_array()?.iter().find_map(|b| {
                    (b.get("type")?.as_str()? == "image")
                        .then(|| media_id(b.get("value")?.get("media_id")))
                        .flatten()
                })
            }
            _ => None,
        }
    })
}

/// The photo the renderer falls back to as a page's share image — the
/// same lookup, loading what it needs itself (the admin's SEO check uses
/// it; the renderer passes the data it already has).
pub(crate) async fn fallback_share_image(pool: &rustango::sql::Pool, page: &crate::page::Page) -> Option<i64> {
    use rustango::core::Column as _;
    let page_id = page.id.get().copied()?;
    let pt = crate::page_type_model::PageType::objects()
        .where_(crate::page_type_model::PageType::id.eq(page.page_type_id))
        .first(pool)
        .await
        .ok()??;
    if let Some(handler) = crate::page_type::find_handler(&pt.type_name) {
        let widgets = handler.widgets(pool, page_id).await.unwrap_or_default();
        let extension = handler.load_extension(pool, page_id).await.unwrap_or(Value::Null);
        if let Some(id) = first_image_in_extension(&widgets, &extension) {
            return Some(id);
        }
    }
    let (values, compiled) =
        crate::page_builder::values::values_for(pool, page_id, page.page_type_id, None).await?;
    crate::page_builder::values::first_media_id(&compiled, &values)
}

thread_local! {
    /// The request's scheme + host, stashed by the public handler right
    /// before it calls the renderer; see [`stash_site_origin`].
    static PENDING_ORIGIN: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Clears a stashed origin on drop.
#[must_use = "the guard clears the stash on drop; bind it to a local"]
pub struct OriginGuard {
    _priv: (),
}

impl Drop for OriginGuard {
    fn drop(&mut self) {
        PENDING_ORIGIN.with(|c| *c.borrow_mut() = None);
    }
}

/// Stash the request origin for the render about to start. Call it
/// immediately before the render future is awaited — no `.await` in
/// between — because [`take_site_origin`] reads it on the same thread,
/// before the render's first `.await`.
pub(crate) fn stash_site_origin(origin: String) -> OriginGuard {
    PENDING_ORIGIN.with(|c| *c.borrow_mut() = Some(origin));
    OriginGuard { _priv: () }
}

/// Take the origin [`stash_site_origin`] left, if any.
pub(crate) fn take_site_origin() -> Option<String> {
    PENDING_ORIGIN.with(|c| c.borrow_mut().take())
}

/// Register the `rcms_meta_tags(page=...)` Tera function. Call from
/// the host's tera setup alongside the existing helpers.
pub fn register_tera_function(tera: &mut tera::Tera) {
    tera.register_function("rcms_meta_tags", rcms_meta_tags);
}

fn rcms_meta_tags(args: &HashMap<String, tera::Value>) -> tera::Result<tera::Value> {
    let Some(page) = args.get("page") else {
        return Ok(tera::Value::String(String::new()));
    };
    let page = match page {
        tera::Value::Object(m) => m,
        _ => return Ok(tera::Value::String(String::new())),
    };
    let get_str = |key: &str| -> String {
        page.get(key)
            .and_then(tera::Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let get_i64 = |key: &str| -> Option<i64> { page.get(key).and_then(tera::Value::as_i64) };
    let get_bool =
        |key: &str| -> bool { page.get(key).and_then(tera::Value::as_bool).unwrap_or(true) };

    let title = get_str("title");
    let seo_title = get_str("seo_title");
    let seo_description = get_str("seo_description");
    let og_title = get_str("og_title");
    let og_description = get_str("og_description");
    let og_image_id = get_i64("og_image_media_id");
    let twitter_card = {
        let raw = get_str("twitter_card");
        if matches!(raw.as_str(), "summary" | "summary_large_image") {
            raw
        } else {
            "summary_large_image".to_owned()
        }
    };
    let robots_index = get_bool("robots_index");

    // #395 — canonical URL: explicit `canonical` arg wins (the renderer
    // injects the alias-aware `canonical_url`); otherwise fall back to
    // the page's own `url_path` (self-canonical) so the tag is still
    // emitted when a host calls `rcms_meta_tags(page=page)` alone.
    let canonical = args
        .get("canonical")
        .and_then(tera::Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| get_str("url_path"));

    // Absolute-URL support: when an `origin` arg is passed (e.g.
    // "https://example.com"), root-relative canonical / og:image /
    // twitter:image URLs are prefixed with it so off-site scrapers and
    // canonical resolution work on the production domain. Omitted → relative
    // (backward compatible with existing callers).
    let origin = args
        .get("origin")
        .and_then(tera::Value::as_str)
        .unwrap_or("")
        .trim_end_matches('/')
        .to_owned();
    let abs = |url: &str| -> String {
        if !origin.is_empty() && url.starts_with('/') {
            format!("{origin}{url}")
        } else {
            url.to_owned()
        }
    };
    // og:type — "website" (default) or e.g. "article" for blog posts.
    let og_type = {
        let raw = args
            .get("og_type")
            .and_then(tera::Value::as_str)
            .unwrap_or("website");
        if raw.is_empty() { "website" } else { raw }.to_owned()
    };

    let final_title = if !og_title.is_empty() {
        og_title.clone()
    } else if !seo_title.is_empty() {
        seo_title.clone()
    } else {
        title.clone()
    };
    let final_description = if !og_description.is_empty() {
        og_description.clone()
    } else {
        seo_description.clone()
    };

    let mut html = String::with_capacity(512);

    // Canonical link (#395). URL pushed raw (slug-derived url_paths are
    // sanitized) — same treatment as the og:image URL below; escaping
    // would turn the `/`s into `&#x2F;`.
    if !canonical.is_empty() {
        html.push_str(r#"<link rel="canonical" href=""#);
        html.push_str(&abs(&canonical));
        html.push_str("\">\n");
    }

    // SEO description.
    if !final_description.is_empty() {
        html.push_str(r#"<meta name="description" content=""#);
        html.push_str(&tera::escape_html(&final_description));
        html.push_str("\">\n");
    }
    // robots (only emitted when noindex).
    if !robots_index {
        html.push_str(r#"<meta name="robots" content="noindex">\n"#);
        html.push('\n');
    }

    // Open Graph.
    if !final_title.is_empty() {
        html.push_str(r#"<meta property="og:title" content=""#);
        html.push_str(&tera::escape_html(&final_title));
        html.push_str("\">\n");
    }
    if !final_description.is_empty() {
        html.push_str(r#"<meta property="og:description" content=""#);
        html.push_str(&tera::escape_html(&final_description));
        html.push_str("\">\n");
    }
    html.push_str(r#"<meta property="og:type" content=""#);
    html.push_str(&tera::escape_html(&og_type));
    html.push_str("\">\n");
    if let Some(mid) = og_image_id {
        html.push_str(r#"<meta property="og:image" content=""#);
        html.push_str(&abs(&format!("/__media__/fill-1200x630/{mid}")));
        html.push_str("\">\n");
    }

    // Twitter.
    html.push_str(r#"<meta name="twitter:card" content=""#);
    html.push_str(&tera::escape_html(&twitter_card));
    html.push_str("\">\n");
    if !final_title.is_empty() {
        html.push_str(r#"<meta name="twitter:title" content=""#);
        html.push_str(&tera::escape_html(&final_title));
        html.push_str("\">\n");
    }
    if !final_description.is_empty() {
        html.push_str(r#"<meta name="twitter:description" content=""#);
        html.push_str(&tera::escape_html(&final_description));
        html.push_str("\">\n");
    }
    if let Some(mid) = og_image_id {
        html.push_str(r#"<meta name="twitter:image" content=""#);
        html.push_str(&abs(&format!("/__media__/fill-1200x630/{mid}")));
        html.push_str("\">\n");
    }

    Ok(tera::Value::String(html))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn share_image_fallback_takes_the_first_photo_in_field_order() {
        use crate::widget::{Widget, WidgetKind};
        let widgets = vec![
            Widget::new(WidgetKind::Text, "heading", "Heading"),
            Widget::new(WidgetKind::Stream, "body", "Body"),
            Widget::new(WidgetKind::MediaPicker, "photo", "Photo"),
        ];
        // A stream stored as JSON text, with an image block after a heading.
        let body = r#"[{"type":"heading","value":{"text":"Hi"}},{"type":"image","value":{"media_id":"9"}}]"#;
        let ext = json!({ "heading": "x", "body": body, "photo": 4 });
        assert_eq!(first_image_in_extension(&widgets, &ext), Some(9));
        // No image block → the media picker after it.
        let ext = json!({ "body": "[]", "photo": "4" });
        assert_eq!(first_image_in_extension(&widgets, &ext), Some(4));
        // Empty values don't count.
        let ext = json!({ "body": "", "photo": null });
        assert_eq!(first_image_in_extension(&widgets, &ext), None);
        assert_eq!(media_id(Some(&json!(0))), None);
    }

    fn page_value(extra: serde_json::Value) -> tera::Value {
        let base = json!({
            "title": "Hello",
            "seo_title": "",
            "seo_description": "",
            "og_title": "",
            "og_description": "",
            "og_image_media_id": null,
            "twitter_card": "summary_large_image",
            "robots_index": true,
        });
        let mut merged = base.as_object().unwrap().clone();
        if let serde_json::Value::Object(extra_map) = extra {
            for (k, v) in extra_map {
                merged.insert(k, v);
            }
        }
        tera::to_value(merged).unwrap()
    }

    fn render(page_val: tera::Value) -> String {
        let mut args = HashMap::new();
        args.insert("page".to_owned(), page_val);
        rcms_meta_tags(&args).unwrap().as_str().unwrap().to_owned()
    }

    #[test]
    fn falls_back_to_seo_title_when_og_title_empty() {
        let html = render(page_value(json!({ "seo_title": "Hello SEO" })));
        assert!(html.contains(r#"og:title" content="Hello SEO"#));
        assert!(html.contains(r#"twitter:title" content="Hello SEO"#));
    }

    #[test]
    fn falls_back_to_title_when_seo_title_empty() {
        let html = render(page_value(json!({})));
        assert!(html.contains(r#"og:title" content="Hello"#));
    }

    #[test]
    fn og_title_overrides_seo_title() {
        let html = render(page_value(json!({
            "og_title": "OG Title",
            "seo_title": "SEO Title",
        })));
        assert!(html.contains(r#"og:title" content="OG Title"#));
        assert!(!html.contains(r#"og:title" content="SEO Title"#));
    }

    #[test]
    fn emits_og_image_when_id_set() {
        let html = render(page_value(json!({ "og_image_media_id": 42 })));
        assert!(html.contains(r#"og:image" content="/__media__/fill-1200x630/42""#));
        assert!(html.contains(r#"twitter:image" content="/__media__/fill-1200x630/42""#));
    }

    #[test]
    fn skips_image_tags_when_no_id() {
        let html = render(page_value(json!({})));
        assert!(!html.contains("og:image"));
        assert!(!html.contains("twitter:image"));
    }

    #[test]
    fn twitter_card_default_is_summary_large_image() {
        let html = render(page_value(json!({})));
        assert!(html.contains(r#"twitter:card" content="summary_large_image"#));
    }

    #[test]
    fn invalid_twitter_card_falls_back_to_default() {
        let html = render(page_value(json!({ "twitter_card": "bogus" })));
        assert!(html.contains(r#"twitter:card" content="summary_large_image"#));
    }

    fn render_with_canonical(page_val: tera::Value, canonical: &str) -> String {
        let mut args = HashMap::new();
        args.insert("page".to_owned(), page_val);
        args.insert(
            "canonical".to_owned(),
            tera::Value::String(canonical.to_owned()),
        );
        rcms_meta_tags(&args).unwrap().as_str().unwrap().to_owned()
    }

    #[test]
    fn emits_canonical_from_arg() {
        let html = render_with_canonical(page_value(json!({})), "/about");
        assert!(html.contains(r#"<link rel="canonical" href="/about">"#));
    }

    #[test]
    fn canonical_falls_back_to_url_path() {
        let html = render(page_value(json!({ "url_path": "/blog/post" })));
        assert!(html.contains(r#"<link rel="canonical" href="/blog/post">"#));
    }

    #[test]
    fn canonical_arg_overrides_url_path() {
        // Alias case: page's own url_path is the alias URL, the injected
        // canonical is the source URL — the source wins.
        let html = render_with_canonical(page_value(json!({ "url_path": "/alias" })), "/source");
        assert!(html.contains(r#"href="/source""#));
        assert!(!html.contains("/alias"));
    }

    #[test]
    fn no_canonical_when_arg_and_url_path_both_absent() {
        let html = render(page_value(json!({})));
        assert!(!html.contains(r#"rel="canonical""#));
    }

    fn render_with_extra_args(page_val: tera::Value, extra: &[(&str, &str)]) -> String {
        let mut args = HashMap::new();
        args.insert("page".to_owned(), page_val);
        for (k, v) in extra {
            args.insert((*k).to_owned(), tera::Value::String((*v).to_owned()));
        }
        rcms_meta_tags(&args).unwrap().as_str().unwrap().to_owned()
    }

    #[test]
    fn origin_absolutizes_canonical_and_images() {
        let html = render_with_extra_args(
            page_value(json!({ "url_path": "/blog/post", "og_image_media_id": 7 })),
            &[("origin", "https://example.com/")],
        );
        assert!(
            html.contains(r#"<link rel="canonical" href="https://example.com/blog/post">"#)
        );
        assert!(html
            .contains(r#"og:image" content="https://example.com/__media__/fill-1200x630/7""#));
        assert!(html.contains(
            r#"twitter:image" content="https://example.com/__media__/fill-1200x630/7""#
        ));
    }

    #[test]
    fn og_type_defaults_website_and_can_override() {
        let default_html = render(page_value(json!({})));
        assert!(default_html.contains(r#"og:type" content="website""#));
        let article_html = render_with_extra_args(page_value(json!({})), &[("og_type", "article")]);
        assert!(article_html.contains(r#"og:type" content="article""#));
        assert!(!article_html.contains(r#"og:type" content="website""#));
    }
}
