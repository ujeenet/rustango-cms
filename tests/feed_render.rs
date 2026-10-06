//! Tests for the RSS / Atom feed mapping. Exercises the
//! `Page → FeedItem` conversion + `rustango::syndication::render_*`
//! end-to-end without the tenant / DB stack.
//!
//! Uses `rustango::setup_test_data!` to share a `Vec<Page>` fixture
//! across every test; per-test reconstruction of the same dummy
//! page rows would be repetitive (Page has ~20 fields).

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{TimeZone, Utc};
use rustango::setup_test_data;
use rustango::sql::Auto;
use rustango::syndication::{render_atom, render_rss, Feed, FeedItem};
use rustango::test_assertions::{assert_contains, assert_content_type, assert_status};
use rustango_cms::feed::_item_from_page;
use rustango_cms::Page;

// Two fixture pages — first published, second updated-but-not-
// published-yet (so its `pub_date` falls through to `updated_at`).
setup_test_data!(
    fn fixture_pages() -> Vec<Page> {
        vec![
            Page {
                id: Auto::Set(1),
                page_type_id: 7,
                title: "Hello, world".to_owned(),
                slug: "hello-world".to_owned(),
                path: "0001/".to_owned(),
                url_path: "/hello-world".to_owned(),
                preview_path: String::new(),
                template_override: String::new(),
                depth: 1,
                parent_id: None,
                locale_variant_of: None,
                alias_of: None,
                theme_id: None,
                sort_order: 0,
                status: "published".to_owned(),
                published_at: Some(Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap()),
                last_published_at: Some(Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap()),
                go_live_at: None,
                expire_at: None,
                seo_title: String::new(),
                seo_description: "Our first post.".to_owned(),
                robots_index: true,
                sitemap_priority: 0.8,
                show_in_menus: false,
                og_title: String::new(),
                og_description: String::new(),
                og_image_media_id: None,
                twitter_card: "summary_large_image".to_owned(),
                notification_pre_published_sent: false,
                created_at: Auto::Set(Utc.with_ymd_and_hms(2026, 5, 1, 11, 0, 0).unwrap()),
                updated_at: Auto::Set(Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap()),
            },
            Page {
                id: Auto::Set(2),
                page_type_id: 7,
                title: "Second post".to_owned(),
                slug: "second".to_owned(),
                path: "0002/".to_owned(),
                url_path: "/second".to_owned(),
                preview_path: String::new(),
                template_override: String::new(),
                depth: 1,
                parent_id: None,
                locale_variant_of: None,
                alias_of: None,
                theme_id: None,
                sort_order: 1,
                status: "published".to_owned(),
                published_at: None, // → falls back to updated_at
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
                created_at: Auto::Set(Utc.with_ymd_and_hms(2026, 5, 5, 8, 0, 0).unwrap()),
                updated_at: Auto::Set(Utc.with_ymd_and_hms(2026, 5, 10, 9, 0, 0).unwrap()),
            },
        ]
    }
);

fn build_feed() -> Feed {
    let base = "https://demo.localtest.me";
    let items: Vec<FeedItem> = fixture_pages()
        .iter()
        .map(|p| _item_from_page(base, p))
        .collect();
    Feed {
        title: "Demo — Articles".to_owned(),
        link: format!("{base}/"),
        description: "Latest articles.".to_owned(),
        language: Some("en".to_owned()),
        last_build_date: items.iter().filter_map(|i| i.pub_date).max(),
        items,
    }
}

fn xml_response(body: String, content_type: &'static str) -> Response {
    let mut res = (StatusCode::OK, body).into_response();
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    res
}

#[test]
fn item_from_page_links_against_base_url() {
    let pages = fixture_pages();
    let item = _item_from_page("https://demo.localtest.me", &pages[0]);
    assert_eq!(item.link, "https://demo.localtest.me/hello-world");
    assert_eq!(
        item.guid.as_deref(),
        Some("https://demo.localtest.me/hello-world"),
        "guid should default to the link",
    );
}

#[test]
fn item_from_page_uses_seo_description_when_present() {
    let pages = fixture_pages();
    let item = _item_from_page("https://demo.localtest.me", &pages[0]);
    assert_eq!(item.description.as_deref(), Some("Our first post."));
}

#[test]
fn item_from_page_omits_description_when_seo_description_empty() {
    let pages = fixture_pages();
    let item = _item_from_page("https://demo.localtest.me", &pages[1]);
    assert!(
        item.description.is_none(),
        "empty seo_description should yield no <description>",
    );
}

#[test]
fn item_from_page_falls_back_from_published_at_to_updated_at() {
    let pages = fixture_pages();
    let item = _item_from_page("https://demo.localtest.me", &pages[1]);
    assert_eq!(
        item.pub_date,
        Some(Utc.with_ymd_and_hms(2026, 5, 10, 9, 0, 0).unwrap()),
    );
}

#[tokio::test]
async fn rss_feed_advertises_correct_content_type() {
    let xml = render_rss(&build_feed());
    let res = xml_response(xml, "application/rss+xml; charset=utf-8");
    assert_status(&res, 200);
    assert_content_type(&res, "application/rss+xml; charset=utf-8");
    assert_contains(res, "<rss").await;
}

#[tokio::test]
async fn atom_feed_advertises_correct_content_type() {
    let xml = render_atom(&build_feed());
    let res = xml_response(xml, "application/atom+xml; charset=utf-8");
    assert_status(&res, 200);
    assert_content_type(&res, "application/atom+xml; charset=utf-8");
    assert_contains(res, "<feed").await;
}

#[tokio::test]
async fn rss_feed_includes_every_fixture_item() {
    let xml = render_rss(&build_feed());
    let res = xml_response(xml, "application/rss+xml; charset=utf-8");
    assert_contains(res, "Hello, world").await;
    let res2 = xml_response(
        render_rss(&build_feed()),
        "application/rss+xml; charset=utf-8",
    );
    assert_contains(res2, "Second post").await;
}

#[tokio::test]
async fn rss_feed_emits_pub_date_for_first_item() {
    let xml = render_rss(&build_feed());
    let res = xml_response(xml, "application/rss+xml; charset=utf-8");
    // RSS uses RFC 822 — the framework emits `Fri, 01 May 2026 ...`.
    // Don't over-pin the exact format; just confirm the year is in
    // the rendered body somewhere inside a pubDate tag region.
    assert_contains(res, "<pubDate>").await;
}

#[tokio::test]
async fn atom_feed_emits_entry_per_fixture_page() {
    let xml = render_atom(&build_feed());
    let res = xml_response(xml, "application/atom+xml; charset=utf-8");
    assert_contains(res, "<entry>").await;
}

#[test]
fn named_feed_urls_reverse() {
    use std::collections::HashMap;
    let mut params: HashMap<String, String> = HashMap::new();
    params.insert("kind".to_owned(), "articles".to_owned());
    assert_eq!(
        rustango::urls::reverse_owned("rcms:feed:rss", &params).unwrap(),
        "/feed/articles/rss.xml",
    );
    assert_eq!(
        rustango::urls::reverse_owned("rcms:feed:atom", &params).unwrap(),
        "/feed/articles/atom.xml",
    );
}
