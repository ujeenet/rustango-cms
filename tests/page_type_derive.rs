//! Tests for `#[derive(PageType)]`. Two fixtures cover the surface:
//!
//! - `TestSimpleHomePage` — unit struct, no extension table. Verifies
//!   the four required string methods + the inventory registration
//!   round-trip via `find_handler`.
//! - `TestArticlePageOverrides` — confirms the `PageTypeOverrides`
//!   delegation: a custom impl wins, an empty impl gets the defaults.
//!
//! The field-bearing case lands in a separate integration test once
//! tang-cms is converted (live DB needed).

use rustango_cms::{find_handler, PageType, PageTypeOverrides};

// ---- Fixture A: minimal unit-struct page type ---------------------

#[derive(PageType, Default)]
#[page_type(
    app = "test",
    type_name = "TestSimpleHomePage",
    verbose_name = "Test home",
    template = "test_home.html"
)]
pub struct TestSimpleHomePage;

impl PageTypeOverrides for TestSimpleHomePage {}

#[test]
fn unit_struct_round_trips_through_registry() {
    let found = find_handler("TestSimpleHomePage");
    assert!(found.is_some(), "TestSimpleHomePage should be registered");
    let h = found.unwrap();
    assert_eq!(h.app_label(), "test");
    assert_eq!(h.type_name(), "TestSimpleHomePage");
    assert_eq!(h.verbose_name(), "Test home");
    assert_eq!(h.default_template(), "test_home.html");
    assert_eq!(h.feed_kind(), None);
    assert!(h.is_creatable());
    assert!(h.allowed_parent_types().is_empty());
    assert!(h.allowed_child_types().is_empty());
}

// ---- Fixture B: unit-struct + overrides --------------------------

#[derive(PageType, Default)]
#[page_type(
    app = "test",
    type_name = "TestArticlePageOverrides",
    verbose_name = "Test article",
    template = "test_article.html",
    feed_kind = "test_feed"
)]
pub struct TestArticlePageOverrides;

impl PageTypeOverrides for TestArticlePageOverrides {
    fn allowed_child_types(&self) -> &'static [&'static str] {
        &["NewsItem", "Author"]
    }
    fn is_creatable(&self) -> bool {
        false
    }
}

#[test]
fn override_trait_wins_over_defaults() {
    let h = find_handler("TestArticlePageOverrides").expect("registered");
    // `feed_kind` came from the `#[page_type(feed_kind = …)]` attr —
    // the derive prefers that over the override-trait default.
    assert_eq!(h.feed_kind(), Some("test_feed"));
    // `allowed_child_types` came from `PageTypeOverrides` impl.
    assert_eq!(h.allowed_child_types(), &["NewsItem", "Author"]);
    // `is_creatable` also from `PageTypeOverrides`.
    assert!(!h.is_creatable());
    // `allowed_parent_types` not overridden — falls back to default empty.
    assert!(h.allowed_parent_types().is_empty());
}

// ---- Fixture C: ensure the derive doesn't conflict with manual impls

// ---- Fixture D: topology attrs win, override impl ignored ---------

#[derive(PageType, Default)]
#[page_type(
    app = "test",
    type_name = "TestAttrTopology",
    verbose_name = "Test attr topology",
    template = "x.html",
    allowed_parents(HomePage, BlogIndexPage),
    allowed_children(BlogPostPage),
    creatable = false
)]
pub struct TestAttrTopology;

impl PageTypeOverrides for TestAttrTopology {
    // These overrides would fire if the attrs were absent — they're
    // here to prove the macro emits the attr literals instead.
    fn allowed_parent_types(&self) -> &'static [&'static str] {
        &["WrongAnswer"]
    }
    fn allowed_child_types(&self) -> &'static [&'static str] {
        &["WrongAnswer"]
    }
    fn is_creatable(&self) -> bool {
        true
    }
}

#[test]
fn struct_attrs_take_precedence_over_overrides() {
    let h = find_handler("TestAttrTopology").expect("registered");
    assert_eq!(h.allowed_parent_types(), &["HomePage", "BlogIndexPage"]);
    assert_eq!(h.allowed_child_types(), &["BlogPostPage"]);
    assert!(!h.is_creatable());
}

#[test]
fn registry_contains_both_test_handlers() {
    let names: std::collections::HashSet<&'static str> = rustango_cms::registered_handlers()
        .map(|h| h.type_name())
        .collect();
    assert!(names.contains("TestSimpleHomePage"));
    assert!(names.contains("TestArticlePageOverrides"));
}

// ---- Fixture C: #246 public_context override --------------------
//
// Verifies the macro emits the `public_context` forwarder by
// type-checking an override that returns a populated map. The
// runtime behaviour (map keys land in the Tera ctx) is exercised
// through the render pipeline — a DB-backed integration test would
// be needed to drive it end-to-end, but the trait dispatch
// contract is what the derive owns.

#[derive(PageType, Default)]
#[page_type(
    app = "test",
    type_name = "TestPublicCtxPage",
    verbose_name = "Test ctx",
    template = "test_ctx.html"
)]
pub struct TestPublicCtxPage;

#[async_trait::async_trait]
impl PageTypeOverrides for TestPublicCtxPage {
    async fn public_context(
        &self,
        _pool: &rustango::sql::Pool,
        _page: &rustango_cms::Page,
    ) -> Result<serde_json::Map<String, serde_json::Value>, rustango::sql::ExecError> {
        let mut m = serde_json::Map::new();
        m.insert(
            "latest_posts".to_owned(),
            serde_json::json!([{"id": 1, "title": "Hi"}, {"id": 2, "title": "Bye"}]),
        );
        m.insert("featured_count".to_owned(), serde_json::json!(7));
        Ok(m)
    }
}

#[test]
fn public_context_override_is_registered_via_derive() {
    let h = rustango_cms::find_handler("TestPublicCtxPage").expect("TestPublicCtxPage registered");
    // The derive forwards `public_context` to `PageTypeOverrides`;
    // the registry sees the override. Confirm the handler is the
    // right type by round-tripping the type_name.
    assert_eq!(h.type_name(), "TestPublicCtxPage");
}

// ---- Fixture E: #243 AC3 — struct-level snippet_m2m relations -------
//
// A pure-M2M page type (no extension table). The derive must emit
// `widgets()` / `save_extension()` / `load_extension()` bodies that
// call into `Snippet` + `page_snippet_m2m` keyed on (page_id, relation)
// — proving the column-less M2M path type-checks against the real APIs.
// The runtime round-trip (chooser options, replace_all persistence)
// needs a live DB and lands in the tang-cms integration test; this
// fixture is the compile-pass + registry contract the derive owns.

#[derive(PageType, Default)]
#[page_type(
    app = "test",
    type_name = "TestSnippetM2MPage",
    verbose_name = "Test M2M",
    template = "test_m2m.html",
    snippet_m2m(categories = "Category", featured_authors = "Author")
)]
pub struct TestSnippetM2MPage;

impl PageTypeOverrides for TestSnippetM2MPage {}

#[test]
fn snippet_m2m_struct_attr_registers_via_derive() {
    let h =
        rustango_cms::find_handler("TestSnippetM2MPage").expect("TestSnippetM2MPage registered");
    assert_eq!(h.type_name(), "TestSnippetM2MPage");
    assert_eq!(h.default_template(), "test_m2m.html");
    // No extension table + no #[field] columns, yet the snippet_m2m
    // relations force real widgets()/save_extension() bodies to be
    // generated (a unit struct with neither would get no-op bodies).

    // #243 — the declared relations surface on the handler (in
    // declaration order) so the public renderer can resolve each into
    // `snippet_relations.<name>`.
    assert_eq!(
        h.snippet_m2m_relations(),
        vec![("categories", "Category"), ("featured_authors", "Author")]
    );
}

// ---- Fixture D: JSON-only page type (no template) ------------------
//
// The whole point of `view_mode = "api"`: `#[page_type(template = …)]`
// is otherwise mandatory, so before this a type that only ever answers
// JSON could not be declared at all.

#[derive(PageType, Default)]
#[page_type(
    app = "test",
    type_name = "TestApiOnlyPage",
    verbose_name = "Test API feed",
    view_mode = "api"
)]
pub struct TestApiOnlyPage;

impl PageTypeOverrides for TestApiOnlyPage {}

#[test]
fn an_api_type_needs_no_template() {
    let h = rustango_cms::find_handler("TestApiOnlyPage").expect("TestApiOnlyPage registered");
    // The empty string is the "no template" sentinel.
    assert_eq!(h.default_template(), "");
    assert_eq!(h.view_mode(), rustango_cms::PageViewMode::Api);
    assert_eq!(
        rustango_cms::page_view::resolve_kind(h.view_mode().as_str(), h.default_template()),
        rustango_cms::PageViewKind::ApiOnly,
    );
}

// ---- Fixture E: API type that ALSO renders HTML --------------------

#[derive(PageType, Default)]
#[page_type(
    app = "test",
    type_name = "TestDualPage",
    verbose_name = "Test dual",
    template = "test_dual.html",
    view_mode = "api"
)]
pub struct TestDualPage;

impl PageTypeOverrides for TestDualPage {}

#[test]
fn an_api_type_may_still_declare_a_template() {
    let h = rustango_cms::find_handler("TestDualPage").expect("TestDualPage registered");
    assert_eq!(h.default_template(), "test_dual.html");
    assert_eq!(h.view_mode(), rustango_cms::PageViewMode::Api);
    // Both representations exist, so `Accept` decides.
    assert_eq!(
        rustango_cms::page_view::resolve_kind(h.view_mode().as_str(), h.default_template()),
        rustango_cms::PageViewKind::ApiWithTemplate,
    );
}

// ---- Fixture F: the mode comes from PageTypeOverrides --------------

#[derive(PageType, Default)]
#[page_type(
    app = "test",
    type_name = "TestOverrideModePage",
    verbose_name = "Test override mode",
    template = "test_override_mode.html"
)]
pub struct TestOverrideModePage;

impl PageTypeOverrides for TestOverrideModePage {
    fn view_mode(&self) -> rustango_cms::PageViewMode {
        rustango_cms::PageViewMode::Api
    }
}

#[test]
fn view_mode_forwards_to_the_overrides_trait() {
    let h = rustango_cms::find_handler("TestOverrideModePage").expect("registered");
    assert_eq!(h.view_mode(), rustango_cms::PageViewMode::Api);
    // ...while a type that overrides nothing keeps the Auto default,
    // which is what makes this change a no-op for every existing type.
    let plain = rustango_cms::find_handler("TestSimpleHomePage").expect("registered");
    assert_eq!(plain.view_mode(), rustango_cms::PageViewMode::Auto);
    assert_eq!(
        rustango_cms::page_view::resolve_kind(plain.view_mode().as_str(), plain.default_template()),
        rustango_cms::PageViewKind::Html,
    );
}
