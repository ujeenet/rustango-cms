//! `GET /api/v2/pages/tree/` — nesting, bounds, localization, and the
//! restriction filtering that keeps a member-only subtree out of an
//! anonymous response.
//!
//! Gated on `sqlite` like every other DB-backed test in this crate (the
//! default feature set is `postgres`, which has no in-memory mode):
//! `cargo test --features sqlite`.
#![cfg(feature = "sqlite")]

mod common;

use common::{find, fixture, json, titles};
use rustango_cms::api::tree::{tree_inner, TreeQuery, DEFAULT_DEPTH, MAX_DEPTH};

/// A plain authenticated, non-superuser viewer — enough to clear a
/// `login` restriction and nothing more.
fn signed_in_user() -> rustango::tenancy::auth::User {
    rustango::tenancy::auth::User {
        id: rustango::sql::Auto::Set(1),
        username: "member".to_owned(),
        password_hash: String::new(),
        email: None,
        is_superuser: false,
        active: true,
        created_at: chrono::Utc::now(),
        data: serde_json::json!({}),
        password_changed_at: None,
        sessions_revoked_at: None,
    }
}

fn q(root: Option<i64>, depth: Option<i64>, locale: Option<&str>) -> TreeQuery {
    TreeQuery {
        root,
        depth,
        locale: locale.map(str::to_owned),
    }
}

#[tokio::test]
async fn nests_children_under_their_parent() {
    let f = fixture().await;
    let resp = tree_inner(&f.pool, None, &q(None, Some(MAX_DEPTH), None))
        .await
        .expect("tree");
    let body = json(resp).await;

    let items = &body["items"];
    assert_eq!(items.as_array().expect("array").len(), 1, "one root: Home");
    assert_eq!(items[0]["title"], "Home");

    let docs = find(items, "title", "Docs").expect("Docs in the tree");
    let kids: Vec<&str> = docs["children"]
        .as_array()
        .expect("children")
        .iter()
        .map(|c| c["title"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(kids, vec!["Intro", "Deep"], "ordered by path");
    assert_eq!(docs["has_children"], true);
    assert_eq!(docs["url"], "/docs");
    assert_eq!(docs["detail_url"], format!("/api/v2/pages/{}/", f.docs));
}

#[tokio::test]
async fn unpublished_pages_are_absent() {
    let f = fixture().await;
    let body = json(
        tree_inner(&f.pool, None, &q(None, Some(MAX_DEPTH), None))
            .await
            .expect("tree"),
    )
    .await;
    assert!(
        !titles(&body["items"]).contains(&"Draft".to_owned()),
        "a draft page must not appear in the public tree",
    );
}

#[tokio::test]
async fn depth_bounds_the_walk() {
    let f = fixture().await;

    let body = json(
        tree_inner(&f.pool, None, &q(None, Some(1), None))
            .await
            .expect("tree"),
    )
    .await;
    assert_eq!(body["meta"]["depth"], 1);
    assert!(
        body["items"][0]["children"]
            .as_array()
            .expect("children")
            .is_empty(),
        "depth 1 returns the root alone",
    );
    assert_eq!(
        body["items"][0]["has_children"], true,
        "has_children still reports the truth the depth cut off",
    );

    let body = json(
        tree_inner(&f.pool, None, &q(None, Some(2), None))
            .await
            .expect("tree"),
    )
    .await;
    assert!(
        find(&body["items"], "title", "Intro").is_none(),
        "depth 2 stops before grandchildren",
    );
}

#[tokio::test]
async fn depth_defaults_and_clamps() {
    let f = fixture().await;

    let body = json(
        tree_inner(&f.pool, None, &q(None, None, None))
            .await
            .expect("tree"),
    )
    .await;
    assert_eq!(body["meta"]["depth"], DEFAULT_DEPTH);

    // An unbounded request must not produce an unbounded walk.
    let body = json(
        tree_inner(&f.pool, None, &q(None, Some(9_999), None))
            .await
            .expect("tree"),
    )
    .await;
    assert_eq!(body["meta"]["depth"], MAX_DEPTH);
}

#[tokio::test]
async fn root_returns_the_subtree_below_that_page() {
    let f = fixture().await;
    let body = json(
        tree_inner(&f.pool, None, &q(Some(f.docs), Some(MAX_DEPTH), None))
            .await
            .expect("tree"),
    )
    .await;

    assert_eq!(body["meta"]["root"], f.docs);
    let top: Vec<String> = body["items"]
        .as_array()
        .expect("array")
        .iter()
        .map(|n| n["title"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        top,
        vec!["Intro", "Deep"],
        "the root's children become the top level; the root itself is excluded",
    );
}

#[tokio::test]
async fn an_unknown_root_is_a_404() {
    let f = fixture().await;
    let resp = tree_inner(&f.pool, None, &q(Some(9_999), None, None))
        .await
        .expect("tree");
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn a_draft_root_is_a_404_not_a_subtree() {
    // Otherwise `?root=` would confirm the existence of unpublished
    // pages, and expose their children.
    let f = fixture().await;
    let resp = tree_inner(&f.pool, None, &q(Some(f.draft), None, None))
        .await
        .expect("tree");
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn locale_translates_titles_and_falls_back_per_page() {
    let f = fixture().await;
    let body = json(
        tree_inner(&f.pool, None, &q(None, Some(MAX_DEPTH), Some("fr")))
            .await
            .expect("tree"),
    )
    .await;

    assert_eq!(body["meta"]["locale"], "fr");
    let all = titles(&body["items"]);
    assert!(all.contains(&"Documentation".to_owned()), "Docs → fr title");
    assert!(all.contains(&"Introduction".to_owned()), "Intro → fr title");
    assert!(
        all.contains(&"About".to_owned()),
        "an untranslated page keeps its canonical title rather than blanking",
    );
    assert!(!all.contains(&"Docs".to_owned()), "canonical title replaced");
}

#[tokio::test]
async fn an_unknown_locale_falls_back_to_the_default() {
    let f = fixture().await;
    let body = json(
        tree_inner(&f.pool, None, &q(None, Some(MAX_DEPTH), Some("xx")))
            .await
            .expect("tree"),
    )
    .await;
    assert_eq!(body["meta"]["locale"], "en");
    assert!(titles(&body["items"]).contains(&"Docs".to_owned()));
}

// ---------------------------------------------------------------------
// #members — the leak test. This is the one that matters most: `list`
// applies no restrictions at all, so a tree that forgot them would
// publish the titles and URLs of every member-only section.
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_gated_subtree_is_absent_for_an_anonymous_caller() {
    let f = fixture().await;
    let body = json(
        tree_inner(&f.pool, None, &q(None, Some(MAX_DEPTH), None))
            .await
            .expect("tree"),
    )
    .await;

    let all = titles(&body["items"]);
    assert!(
        !all.contains(&"Members".to_owned()),
        "the login-gated page leaked: {all:?}",
    );
    assert!(
        !all.contains(&"Secret".to_owned()),
        "a page under a gated ancestor leaked: {all:?}",
    );
}

#[tokio::test]
async fn a_gated_subtree_is_present_for_a_signed_in_viewer() {
    let f = fixture().await;
    let viewer = signed_in_user();
    let body = json(
        tree_inner(&f.pool, Some(&viewer), &q(None, Some(MAX_DEPTH), None))
            .await
            .expect("tree"),
    )
    .await;

    let all = titles(&body["items"]);
    assert!(all.contains(&"Members".to_owned()), "got {all:?}");
    assert!(all.contains(&"Secret".to_owned()), "got {all:?}");
}

#[tokio::test]
async fn a_gated_root_is_a_404_for_an_anonymous_caller() {
    // `?root=` must not become an oracle for pages the tree hides.
    let f = fixture().await;
    let resp = tree_inner(&f.pool, None, &q(Some(f.members), None, None))
        .await
        .expect("tree");
    assert_eq!(resp.status(), 404);
}

// ---------------------------------------------------------------------
// Direct-child summaries on page detail. One level, so a client can walk
// down from any page without pulling the whole tree first.
// ---------------------------------------------------------------------

use rustango_cms::api::pages::detail_object_with_type;
use rustango_cms::page::Page;

async fn detail(f: &common::Fixture, id: i64, viewer: Option<&rustango::tenancy::auth::User>) -> serde_json::Value {
    use rustango::core::Column as _;
    let page: Page = Page::objects()
        .where_(Page::id.eq(id))
        .first(&f.pool)
        .await
        .expect("query")
        .expect("page");
    serde_json::Value::Object(
        detail_object_with_type(&f.pool, &page, "TestPage", None, None, None, viewer).await,
    )
}

fn child_titles(obj: &serde_json::Value) -> Vec<String> {
    obj["children"]
        .as_array()
        .expect("children array")
        .iter()
        .map(|c| c["title"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn detail_lists_direct_children_only() {
    let f = fixture().await;
    let obj = detail(&f, f.home, None).await;
    let kids = child_titles(&obj);

    assert!(kids.contains(&"About".to_owned()), "got {kids:?}");
    assert!(kids.contains(&"Docs".to_owned()), "got {kids:?}");
    assert!(
        !kids.contains(&"Intro".to_owned()),
        "a grandchild must not appear — that is what the tree endpoint is for",
    );
    assert!(
        !kids.contains(&"Draft".to_owned()),
        "an unpublished child must not be advertised",
    );
}

#[tokio::test]
async fn child_summaries_carry_has_children() {
    let f = fixture().await;
    let obj = detail(&f, f.home, None).await;
    let kids = obj["children"].as_array().expect("children");

    let docs = kids
        .iter()
        .find(|c| c["title"] == "Docs")
        .expect("Docs child");
    assert_eq!(docs["has_children"], true, "Docs parents Intro and Deep");
    assert_eq!(docs["url"], "/docs");
    assert_eq!(docs["slug"], "docs");
    assert_eq!(docs["detail_url"], format!("/api/v2/pages/{}/", f.docs));

    let about = kids
        .iter()
        .find(|c| c["title"] == "About")
        .expect("About child");
    assert_eq!(about["has_children"], false);
}

#[tokio::test]
async fn a_leaf_page_reports_an_empty_children_array() {
    // Present-but-empty, not absent: a client shouldn't have to
    // distinguish "no children" from "this endpoint forgot to say".
    let f = fixture().await;
    let obj = detail(&f, f.deep, None).await;
    assert_eq!(obj["children"], serde_json::json!([]));
}

#[tokio::test]
async fn child_summaries_hide_a_gated_child_from_an_anonymous_caller() {
    let f = fixture().await;
    let kids = child_titles(&detail(&f, f.home, None).await);
    assert!(
        !kids.contains(&"Members".to_owned()),
        "a gated child leaked through the detail endpoint: {kids:?}",
    );

    let viewer = signed_in_user();
    let kids = child_titles(&detail(&f, f.home, Some(&viewer)).await);
    assert!(kids.contains(&"Members".to_owned()), "got {kids:?}");
}
