//! Tests for the sitemap XML emitter. Uses
//! `rustango::setup_test_data!` to share a single fixture of
//! `SitemapEntry` rows across every test in this file (init runs at
//! most once per test-binary process — a shared class-level
//! fixture).

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{TimeZone, Utc};
use rustango::setup_test_data;
use rustango::sitemaps::{render_sitemap, ChangeFreq, SitemapEntry};
use rustango::test_assertions::{
    assert_contains, assert_content_type, assert_not_contains, assert_status,
};

// Shared fixture — three pages of varying lastmod/changefreq/priority.
// `setup_test_data!` wraps a `OnceLock`; the init body runs at most
// once across every test in this file.
setup_test_data!(
    fn fixture_entries() -> Vec<SitemapEntry> {
        vec![
            SitemapEntry::new("https://demo.localtest.me/")
                .with_lastmod(Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap())
                .with_changefreq(ChangeFreq::Daily)
                .with_priority(1.0),
            SitemapEntry::new("https://demo.localtest.me/about")
                .with_lastmod(Utc.with_ymd_and_hms(2026, 4, 15, 8, 30, 0).unwrap())
                .with_changefreq(ChangeFreq::Weekly)
                .with_priority(0.5),
            SitemapEntry::new("https://demo.localtest.me/blog/hello")
                .with_lastmod(Utc.with_ymd_and_hms(2026, 5, 10, 9, 0, 0).unwrap())
                .with_priority(0.8),
        ]
    }
);

/// Wrap the rendered XML into the same shape the public handler
/// returns so the framework's assert helpers (which take `Response`)
/// apply directly.
fn xml_response(body: String) -> Response {
    let mut res = (StatusCode::OK, body).into_response();
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    res
}

#[tokio::test]
async fn render_sitemap_emits_xml_preamble_and_urlset() {
    let xml = render_sitemap(fixture_entries());
    let res = xml_response(xml);
    assert_status(&res, 200);
    assert_content_type(&res, "application/xml; charset=utf-8");
    assert_contains(res, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>").await;
}

#[tokio::test]
async fn render_sitemap_emits_loc_for_every_entry() {
    let xml = render_sitemap(fixture_entries());
    let res = xml_response(xml);
    assert_contains(res, "<loc>https://demo.localtest.me/</loc>").await;
    // Re-render for the second + third checks (assert_contains
    // consumes the response).
    let res2 = xml_response(render_sitemap(fixture_entries()));
    assert_contains(res2, "<loc>https://demo.localtest.me/about</loc>").await;
    let res3 = xml_response(render_sitemap(fixture_entries()));
    assert_contains(res3, "<loc>https://demo.localtest.me/blog/hello</loc>").await;
}

#[tokio::test]
async fn render_sitemap_emits_priority_with_one_decimal() {
    let xml = render_sitemap(fixture_entries());
    let res = xml_response(xml);
    // The framework clamps to [0.0, 1.0] and formats with 1 decimal.
    // 0.5 + 1.0 + 0.8 all appear; 1.0 renders as "1.0".
    assert_contains(res, "<priority>1.0</priority>").await;
    let res2 = xml_response(render_sitemap(fixture_entries()));
    assert_contains(res2, "<priority>0.5</priority>").await;
    let res3 = xml_response(render_sitemap(fixture_entries()));
    assert_contains(res3, "<priority>0.8</priority>").await;
}

#[tokio::test]
async fn render_sitemap_emits_changefreq_when_set() {
    let xml = render_sitemap(fixture_entries());
    let res = xml_response(xml);
    assert_contains(res, "<changefreq>daily</changefreq>").await;
    let res2 = xml_response(render_sitemap(fixture_entries()));
    assert_contains(res2, "<changefreq>weekly</changefreq>").await;
}

#[tokio::test]
async fn render_sitemap_omits_changefreq_when_unset() {
    // Third entry has no changefreq — confirm there's no orphan tag.
    let one = vec![SitemapEntry::new("https://demo.localtest.me/no-freq").with_priority(0.3)];
    let xml = render_sitemap(&one);
    let res = xml_response(xml);
    assert_not_contains(res, "<changefreq>").await;
}

#[tokio::test]
async fn render_sitemap_empty_entries_still_produces_valid_xml() {
    let empty: Vec<SitemapEntry> = Vec::new();
    let xml = render_sitemap(&empty);
    let res = xml_response(xml);
    assert_status(&res, 200);
    assert_contains(res, "<urlset").await;
}

/// Sanity-check that the fixture itself returns a stable
/// `&'static Vec<...>` across calls — that's the contract
/// `setup_test_data!` exists to provide. If this regresses, every
/// other test in this file silently re-runs the init body.
#[test]
fn fixture_is_stable_across_calls() {
    let first = fixture_entries();
    let second = fixture_entries();
    assert!(
        std::ptr::eq(first, second),
        "setup_test_data!: fixture body re-ran on second call",
    );
    assert_eq!(first.len(), 3);
}
