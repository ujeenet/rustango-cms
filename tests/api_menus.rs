//! `GET /api/v2/menus/` + `GET /api/v2/menus/{slug}/` — the envelope,
//! label translation (authored and page-title fallback), active-item
//! marking, and the #members filtering.
//!
//! Gated on `sqlite` like every other DB-backed test in this crate (the
//! default feature set is `postgres`, which has no in-memory mode):
//! `cargo test --features sqlite`.
#![cfg(feature = "sqlite")]

mod common;

use common::{find, fixture, json, labels, seed_menu, translate_item, Fixture};
use rustango_cms::api::menus::{detail_inner, list_inner, MenuQuery};

fn q(locale: Option<&str>, current: Option<i64>) -> MenuQuery {
    MenuQuery {
        locale: locale.map(str::to_owned),
        current,
        limit: None,
        offset: None,
    }
}

async fn menu(f: &Fixture, q: &MenuQuery) -> serde_json::Value {
    json(detail_inner(&f.pool, None, "main", q).await.expect("menu")).await
}

#[tokio::test]
async fn list_returns_the_meta_envelope() {
    let f = fixture().await;
    seed_menu(&f).await;
    let body = json(list_inner(&f.pool, &q(None, None)).await.expect("list")).await;

    assert_eq!(body["meta"]["total_count"], 1);
    assert_eq!(body["items"][0]["slug"], "main");
    assert_eq!(body["items"][0]["name"], "Main navigation");
    assert_eq!(body["items"][0]["detail_url"], "/api/v2/menus/main/");
}

#[tokio::test]
async fn an_unknown_menu_is_a_404() {
    let f = fixture().await;
    seed_menu(&f).await;
    let resp = detail_inner(&f.pool, None, "nope", &q(None, None))
        .await
        .expect("menu");
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn detail_nests_items_and_counts_the_whole_tree() {
    let f = fixture().await;
    seed_menu(&f).await;
    let body = menu(&f, &q(None, None)).await;

    assert_eq!(body["meta"]["slug"], "main");
    // Home, Docs (+ its child), Elsewhere — Members is gated away for an
    // anonymous caller, taking its subtree with it.
    assert_eq!(
        labels(&body["items"]),
        vec!["Home", "Docs", "Intro", "Elsewhere"],
    );
    assert_eq!(
        body["meta"]["total_count"], 4,
        "the count spans the nesting, not just the top level",
    );

    let docs = find(&body["items"], "label", "Docs").expect("Docs");
    assert_eq!(docs["url"], "/docs");
    assert_eq!(docs["is_page"], true);
    assert_eq!(docs["children"].as_array().expect("children").len(), 1);
}

#[tokio::test]
async fn an_empty_label_falls_back_to_the_page_title() {
    let f = fixture().await;
    seed_menu(&f).await;
    let body = menu(&f, &q(None, None)).await;
    assert!(
        find(&body["items"], "label", "Intro").is_some(),
        "the item with no label of its own shows the target page's title",
    );
}

#[tokio::test]
async fn an_external_item_passes_its_url_through() {
    let f = fixture().await;
    seed_menu(&f).await;
    let body = menu(&f, &q(None, None)).await;
    let ext = find(&body["items"], "label", "Elsewhere").expect("Elsewhere");
    assert_eq!(ext["url"], "https://example.com");
    assert_eq!(ext["is_page"], false);
}

// ---------------------------------------------------------------------
// Localization — the two separate problems the feature had to solve.
// ---------------------------------------------------------------------

#[tokio::test]
async fn an_authored_label_uses_its_translation() {
    let f = fixture().await;
    let (_menu, items) = seed_menu(&f).await;
    translate_item(&f, items[0], "label", "Accueil").await;

    let body = menu(&f, &q(Some("fr"), None)).await;
    assert_eq!(body["meta"]["locale"], "fr");
    let all = labels(&body["items"]);
    assert!(all.contains(&"Accueil".to_owned()), "got {all:?}");
    assert!(!all.contains(&"Home".to_owned()), "canonical label replaced");
}

#[tokio::test]
async fn an_empty_label_falls_back_to_the_translated_page_title() {
    // The second half of the bug: the page-title fallback was never
    // localized, so a French menu showed an English label for any item
    // an editor had left unlabelled.
    let f = fixture().await;
    seed_menu(&f).await;
    let body = menu(&f, &q(Some("fr"), None)).await;

    let all = labels(&body["items"]);
    assert!(
        all.contains(&"Introduction".to_owned()),
        "expected the fr page title, got {all:?}",
    );
    assert!(!all.contains(&"Intro".to_owned()));
}

#[tokio::test]
async fn an_untranslated_authored_label_stays_canonical() {
    // It must NOT fall through to the page title just because no
    // translation exists — that would silently change what the item says.
    let f = fixture().await;
    seed_menu(&f).await;
    let body = menu(&f, &q(Some("fr"), None)).await;

    let all = labels(&body["items"]);
    assert!(all.contains(&"Home".to_owned()), "got {all:?}");
    assert!(
        !all.contains(&"Documentation".to_owned()),
        "the authored label 'Docs' must not be replaced by the page's fr title",
    );
}

#[tokio::test]
async fn an_external_url_can_be_localized() {
    let f = fixture().await;
    let (_menu, items) = seed_menu(&f).await;
    let ext_item = *items.last().expect("external item");
    translate_item(&f, ext_item, "external_url", "https://example.fr").await;

    let body = menu(&f, &q(Some("fr"), None)).await;
    let ext = find(&body["items"], "label", "Elsewhere").expect("Elsewhere");
    assert_eq!(ext["url"], "https://example.fr");
}

#[tokio::test]
async fn the_default_locale_returns_canonical_labels() {
    let f = fixture().await;
    let (_menu, items) = seed_menu(&f).await;
    translate_item(&f, items[0], "label", "Accueil").await;

    let body = menu(&f, &q(Some("en"), None)).await;
    let all = labels(&body["items"]);
    assert!(all.contains(&"Home".to_owned()), "got {all:?}");
    assert!(!all.contains(&"Accueil".to_owned()));
}

// ---------------------------------------------------------------------
// Active-item marking.
// ---------------------------------------------------------------------

#[tokio::test]
async fn current_marks_exactly_one_item_active() {
    let f = fixture().await;
    seed_menu(&f).await;
    let body = menu(&f, &q(None, Some(f.intro))).await;

    assert_eq!(body["meta"]["current"], f.intro);
    let intro = find(&body["items"], "label", "Intro").expect("Intro");
    assert_eq!(intro["is_active"], true);

    let mut active = 0;
    count_active(&body["items"], &mut active);
    assert_eq!(active, 1, "exactly one item is the current page");
}

#[tokio::test]
async fn an_ancestor_of_the_current_page_is_in_the_active_trail() {
    let f = fixture().await;
    seed_menu(&f).await;
    let body = menu(&f, &q(None, Some(f.intro))).await;

    let docs = find(&body["items"], "label", "Docs").expect("Docs");
    assert_eq!(docs["in_active_trail"], true, "Docs is Intro's parent");
    assert_eq!(docs["is_active"], false, "an ancestor is not itself active");

    let home = find(&body["items"], "label", "Home").expect("Home");
    assert_eq!(
        home["in_active_trail"], true,
        "Home is Intro's grandparent — a path prefix, so no extra query",
    );

    let ext = find(&body["items"], "label", "Elsewhere").expect("Elsewhere");
    assert_eq!(ext["in_active_trail"], false);
}

#[tokio::test]
async fn without_current_nothing_is_marked() {
    let f = fixture().await;
    seed_menu(&f).await;
    let body = menu(&f, &q(None, None)).await;

    let mut active = 0;
    count_active(&body["items"], &mut active);
    assert_eq!(active, 0);
    assert!(body["meta"]["current"].is_null());
}

#[tokio::test]
async fn an_unresolvable_current_is_a_hint_not_an_error() {
    // A stale link must not take the whole navbar down.
    let f = fixture().await;
    seed_menu(&f).await;
    let resp = detail_inner(&f.pool, None, "main", &q(None, Some(9_999)))
        .await
        .expect("menu");
    assert_eq!(resp.status(), 200);
    let body = json(resp).await;
    let mut active = 0;
    count_active(&body["items"], &mut active);
    assert_eq!(active, 0);
}

// ---------------------------------------------------------------------
// #members
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_gated_item_is_hidden_from_an_anonymous_caller() {
    let f = fixture().await;
    seed_menu(&f).await;
    let body = menu(&f, &q(None, None)).await;
    assert!(
        !labels(&body["items"]).contains(&"Members".to_owned()),
        "a menu item pointing at a gated page leaked",
    );
}

#[tokio::test]
async fn a_gated_item_is_visible_to_a_signed_in_viewer() {
    let f = fixture().await;
    seed_menu(&f).await;
    let viewer = rustango::tenancy::auth::User {
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
    };
    let body = json(
        detail_inner(&f.pool, Some(&viewer), "main", &q(None, None))
            .await
            .expect("menu"),
    )
    .await;
    assert!(labels(&body["items"]).contains(&"Members".to_owned()));
}

fn count_active(items: &serde_json::Value, n: &mut usize) {
    let Some(arr) = items.as_array() else { return };
    for node in arr {
        if node["is_active"] == true {
            *n += 1;
        }
        count_active(&node["children"], n);
    }
}
