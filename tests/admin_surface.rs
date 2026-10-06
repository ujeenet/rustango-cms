//! Integration tests for the cms-admin static surface — templates
//! parse cleanly, named URLs resolve, no name collisions in the
//! `inventory`-registered routing table.
//!
//! Uses `rustango::test_assertions::*` for the response-shape
//! checks, matching the test style the rest of the framework
//! consumers follow. Stays off the database / tenancy stack on
//! purpose — these are surface-level checks that should run fast
//! and not need docker.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::{Html, IntoResponse};
use rustango::test_assertions::{assert_contains, assert_not_contains, assert_status};
use rustango_cms::admin;
use rustango_cms::urls;
use tera::{Context, Tera};
use tower::ServiceExt as _;

/// Build a fresh Tera instance with every cms-admin template
/// registered (and the helper Tera fns / filters wired up).
fn fresh_tera() -> Tera {
    let mut tera = Tera::default();
    admin::register_templates(&mut tera).expect("register_templates");
    tera
}

/// Minimal render context every admin template expects: the i18n epic
/// (#523) made `LANG`/`LANG_DIR` mandatory (`translate(locale=LANG)` in
/// the chrome), so a bare `Context::new()` no longer renders anything.
fn test_ctx() -> Context {
    let mut ctx = Context::new();
    ctx.insert("LANG", "en");
    ctx.insert("LANG_DIR", "ltr");
    ctx
}

#[test]
fn register_templates_succeeds_on_a_fresh_tera() {
    // Smoke test — if any bundled template's syntax breaks (Tera
    // parse error, missing `{% block %}`, etc.), this lights up.
    let _ = fresh_tera();
}

#[test]
fn named_urls_have_no_duplicate_names() {
    // The `register_url!` calls in src/urls.rs all submit through
    // the framework's `inventory` collector. If two of them
    // register the same name, `rustango::urls::reverse()` would
    // pick one arbitrarily and silently break links. This guard
    // surfaces the collision at test time.
    let dups = urls::duplicate_url_names();
    assert!(
        dups.is_empty(),
        "duplicate URL names registered via `register_url!`: {dups:?}"
    );
}

#[test]
fn cms_admin_pages_edit_reverses_with_id_param() {
    let mut params: HashMap<String, String> = HashMap::new();
    params.insert("id".to_owned(), "42".to_owned());
    let url = rustango::urls::reverse_owned("rcms-admin:pages:edit", &params)
        .expect("rcms-admin:pages:edit must resolve");
    assert_eq!(url, "/cms-admin/pages/42/edit");
}

#[test]
fn public_sitemap_route_is_named() {
    let url = rustango::urls::reverse("rcms:sitemap", &HashMap::new())
        .expect("rcms:sitemap must resolve");
    assert_eq!(url, "/sitemap.xml");
}

#[tokio::test]
async fn settings_template_renders_with_empty_themes() {
    // Render the bundled `settings.html` against a minimal context
    // (no locales, no themes) and confirm the chrome shows up.
    // Wrapping the rendered HTML in a Response lets us drive the
    // assertion through the framework's `assert_contains` /
    // `assert_status` helpers.
    let tera = Arc::new(fresh_tera());
    let mut ctx = test_ctx();
    ctx.insert("active_tab", "settings");
    ctx.insert("brand_name", &"Test CMS");
    ctx.insert("tenant_slug", &"demo");
    // _base.html + form templates pull a handful of chrome
    // variables. Real handlers thread these in via add_chrome +
    // render_with_csrf; tests have to stub them.
    ctx.insert("theme_css", &"");
    ctx.insert("csrf_token", &"test-token");
    ctx.insert("messages", &Vec::<serde_json::Value>::new());
    ctx.insert("locales", &Vec::<serde_json::Value>::new());
    ctx.insert("media_count", &0);
    ctx.insert("org_slug", &"demo");
    ctx.insert("org_display_name", &"Demo");
    ctx.insert("themes", &Vec::<serde_json::Value>::new());
    ctx.insert(
        "theme_swatches",
        &std::collections::HashMap::<String, String>::new(),
    );

    let html = tera
        .render("rcms_admin/settings.html", &ctx)
        .expect("settings.html should render against minimal context");
    let res = Html(html).into_response();

    // The "Admin theme" picker moved to /cms-admin/me (#174) — the
    // settings page now headlines Tenant + Locales + Stats + Branding.
    // Smoke-test that the page still renders with the Tenant heading
    // when no themes are registered.
    assert_status(&res, 200);
    // #325 de-inlined the heading (`margin-top:0` was redundant with
    // `.card h2`; `font-size:16px` → `rcms-text-lg`). Assert the heading
    // renders without pinning the now-utility classes.
    assert_contains(res, ">Tenant</h2>").await;
}

#[tokio::test]
async fn settings_template_with_zero_locales_shows_empty_state() {
    let tera = Arc::new(fresh_tera());
    let mut ctx = test_ctx();
    ctx.insert("active_tab", "settings");
    ctx.insert("brand_name", &"Test CMS");
    ctx.insert("tenant_slug", &"demo");
    // _base.html + form templates pull a handful of chrome
    // variables. Real handlers thread these in via add_chrome +
    // render_with_csrf; tests have to stub them.
    ctx.insert("theme_css", &"");
    ctx.insert("csrf_token", &"test-token");
    ctx.insert("messages", &Vec::<serde_json::Value>::new());
    ctx.insert("locales", &Vec::<serde_json::Value>::new());
    ctx.insert("media_count", &0);
    ctx.insert("org_slug", &"demo");
    ctx.insert("org_display_name", &"Demo");
    ctx.insert("themes", &Vec::<serde_json::Value>::new());
    ctx.insert(
        "theme_swatches",
        &std::collections::HashMap::<String, String>::new(),
    );

    let html = tera
        .render("rcms_admin/settings.html", &ctx)
        .expect("render settings.html");
    let res = Html(html).into_response();

    // The empty-state copy lives in settings.html; if a refactor
    // accidentally drops it, this catches the regression.
    assert_contains(res, "No locales registered").await;
}

#[tokio::test]
async fn styleguide_renders_every_canonical_component() {
    // #326 — the styleguide is the single source of truth for the admin
    // component catalog. Pin that it renders inline-style-free and that
    // every canonical component family is present, so removing or
    // renaming one (in the template or cms.css) trips this surface.
    let tera = Arc::new(fresh_tera());
    let mut ctx = test_ctx();
    ctx.insert("active_tab", "styleguide");
    ctx.insert("brand_name", &"Test CMS");
    ctx.insert("tenant_slug", &"demo");
    ctx.insert("theme_css", &"");
    ctx.insert("csrf_token", &"test-token");
    ctx.insert("messages", &Vec::<serde_json::Value>::new());
    ctx.insert("locales", &Vec::<serde_json::Value>::new());
    ctx.insert("media_count", &0);
    ctx.insert("org_slug", &"demo");
    ctx.insert("org_display_name", &"Demo");
    ctx.insert("themes", &Vec::<serde_json::Value>::new());
    ctx.insert(
        "theme_swatches",
        &std::collections::HashMap::<String, String>::new(),
    );
    // Styleguide-specific registries — empty exercises the "none
    // registered" branches.
    ctx.insert("blocks", &Vec::<serde_json::Value>::new());
    ctx.insert("page_types", &Vec::<serde_json::Value>::new());
    ctx.insert("library_types", &Vec::<serde_json::Value>::new());

    let html = tera
        .render("rcms_admin/styleguide.html", &ctx)
        .expect("styleguide.html should render against a minimal context");

    // #326 acceptance: the styleguide template ITSELF carries no inline
    // styles. (Checked against the source, not the rendered page — the
    // latter inherits _base.html chrome, which keeps a few legitimate
    // dimensional inline styles that are out of scope here.)
    const STYLEGUIDE_SRC: &str = include_str!("../src/admin/templates/styleguide.html");
    assert!(
        !STYLEGUIDE_SRC.contains(" style=\""),
        "styleguide.html template must be inline-style-free"
    );

    // Every canonical component family must be documented — this is the
    // visual-regression surface the issue asks for.
    for marker in [
        "class=\"rcms-btn",            // buttons
        "class=\"rcms-tag draft",      // status chips
        "rcms-checkbox-field--switch", // switch toggle
        "rcms-switch-track",
        "rcms-alert--error",   // persistent banner
        "rcms-flash--success", // one-shot flash
        "rcms-flash--error",
        "rcms-toast rcms-toast--in rcms-toast--success", // transient toast
        "rcms-toast--error",
        "rcms-tab-strip", // tabs
        "rcms-tab-count",
        "rcms-chip-cluster",   // chip cluster
        "rcms-confirm-dialog", // overlay catalog
        // every HTML input type the admin styles
        "type=\"email\"",
        "type=\"url\"",
        "type=\"tel\"",
        "type=\"password\"",
        "type=\"search\"",
        "type=\"number\"",
        "type=\"date\"",
        "type=\"time\"",
        "type=\"datetime-local\"",
        "type=\"month\"",
        "type=\"week\"",
        "type=\"color\"",
        "type=\"range\"",
        "type=\"file\"",
        "type=\"radio\"",
    ] {
        assert!(
            html.contains(marker),
            "styleguide.html is missing the `{marker}` component marker"
        );
    }

    let res = Html(html).into_response();
    assert_status(&res, 200);
}

/// #316 — the aging-report's prev/next links render through the
/// `querystring` filter: it preserves the current query (min_days /
/// status) and only overrides `page`. Guards both that the filter is
/// wired into the admin Tera and that the in-template usage renders.
#[test]
fn aging_pages_pagination_links_use_querystring_filter() {
    let tera = fresh_tera();
    let mut ctx = test_ctx();
    // `_base.html` chrome.
    ctx.insert("active_tab", "reports");
    ctx.insert("brand_name", &"Test CMS");
    ctx.insert("tenant_slug", &"demo");
    ctx.insert("theme_css", &"");
    ctx.insert("csrf_token", &"t");
    ctx.insert("messages", &Vec::<serde_json::Value>::new());
    ctx.insert("locales", &Vec::<serde_json::Value>::new());
    ctx.insert("media_count", &0);
    ctx.insert("org_slug", &"demo");
    ctx.insert("org_display_name", &"Demo");
    // Report data — one row (so the table+pagination branch renders),
    // page 2 of 3 so both prev + next links show.
    ctx.insert(
        "rows",
        &vec![serde_json::json!({
            "id": 1, "title": "Old page", "url_path": "/old",
            "status": "published", "days_stale": 120,
            "updated_at": serde_json::Value::Null,
            "last_editor": serde_json::Value::Null,
        })],
    );
    ctx.insert("min_days", &90);
    ctx.insert("status_filter", &"published");
    ctx.insert("current_page", &2u32);
    ctx.insert("total", &120);
    ctx.insert("page_size", &50);
    // The report renders the shared pagination partial now, which builds
    // its links from this context exactly as the handler supplies it.
    ctx.insert(
        "pagination",
        &rustango_cms::admin::pagination::context(2, 50, 120),
    );
    ctx.insert("query_string", &"min_days=90&status=published&page=2");

    let html = tera
        .render("rcms_admin/aging_pages.html", &ctx)
        .expect("aging_pages.html should render");
    // Prev → page 1, Next → page 3 — min_days + status carried through,
    // `page` replaced in place (querystring preserves key order).
    assert!(
        html.contains("?min_days=90&status=published&page=1"),
        "prev link should carry min_days+status and set page=1; got:\n{html}"
    );
    assert!(
        html.contains("?min_days=90&status=published&page=3"),
        "next link should carry min_days+status and set page=3"
    );
}

/// #316 — the media collection-rail links use `querystring`: "All" drops
/// the `collection` key (via the `qs_none` null sentinel — Tera has no
/// `null` literal), a collection link sets it in place, and "Clear" drops
/// q/from/to while keeping collection. All preserve the other filters.
#[test]
fn media_collection_rail_links_use_querystring_filter() {
    let tera = fresh_tera();
    let mut ctx = test_ctx();
    // `_base.html` chrome.
    ctx.insert("active_tab", "media");
    ctx.insert("brand_name", &"Test CMS");
    ctx.insert("tenant_slug", &"demo");
    ctx.insert("theme_css", &"");
    ctx.insert("csrf_token", &"t");
    ctx.insert("messages", &Vec::<serde_json::Value>::new());
    ctx.insert("locales", &Vec::<serde_json::Value>::new());
    ctx.insert("media_count", &0);
    ctx.insert("org_slug", &"demo");
    ctx.insert("org_display_name", &"Demo");
    // Media view — viewing collection 5 with an active `q=logo` search.
    ctx.insert("kind_label", &"Media (images)");
    ctx.insert("upload_kind", &"image");
    ctx.insert("self_url", &"/cms-admin/media");
    ctx.insert("collection_filter", &5i64);
    ctx.insert(
        "collections",
        &vec![serde_json::json!({"id": 3, "name": "Logos", "count": 2})],
    );
    ctx.insert("search_q", &"logo");
    ctx.insert("is_filtered", &true);
    ctx.insert("filtered_count", &0);
    ctx.insert("uncategorized_count", &0);
    ctx.insert("items", &Vec::<serde_json::Value>::new());
    // querystring inputs: current query + the null sentinel.
    ctx.insert("query_string", &"collection=5&q=logo");
    ctx.insert("qs_none", &serde_json::Value::Null);

    let html = tera
        .render("rcms_admin/media_list.html", &ctx)
        .expect("media_list.html should render");
    // `self_url`'s slashes are HTML-escaped (`&#x2F;`, pre-existing); the
    // `| safe` querystring suffix stays raw. Assert on the query part.
    // "All" drops collection, keeps q.
    assert!(
        html.contains("media?q=logo\""),
        "All link should drop collection + keep q; got:\n{html}"
    );
    // Collection #3 link sets collection in place, keeps q.
    assert!(
        html.contains("media?collection=3&q=logo\""),
        "collection link should set collection=3 + keep q"
    );
    // "Clear" drops q (+ from/to), keeps the active collection.
    assert!(
        html.contains("media?collection=5\""),
        "Clear link should drop q + keep collection=5"
    );
}

/// #325 — role_form.html is fully de-inlined: the permission matrix's
/// section/heading/cell styling moved into `.perm-section` /
/// `.perm-matrix` component rules, the rest onto rcms- utilities. Lock
/// it so a re-introduced inline `style=` trips here.
#[test]
fn role_form_is_inline_style_free() {
    const SRC: &str = include_str!("../src/admin/templates/role_form.html");
    assert!(
        !SRC.contains(" style=\""),
        "role_form.html must stay inline-style-free (#325) — use a \
         component rule or an rcms- utility instead"
    );
}

/// #325 — three more templates de-inlined to rcms- utilities
/// (`rcms-text-2xs`, `rcms-pt-0`, `rcms-maxh-260` + `rcms-overflow-auto`).
/// Lock them so a re-introduced inline `style=` trips here.
#[test]
fn more_admin_templates_are_inline_style_free() {
    for (name, src) in [
        // form_fields.html was replaced by the visual builder in the form
        // rewrite (#533); lock its successor + the Forms list instead.
        (
            "form_builder.html",
            include_str!("../src/admin/templates/form_builder.html"),
        ),
        (
            "forms_list.html",
            include_str!("../src/admin/templates/forms_list.html"),
        ),
        (
            "redirect_list.html",
            include_str!("../src/admin/templates/redirect_list.html"),
        ),
        (
            "search_promotion_edit.html",
            include_str!("../src/admin/templates/search_promotion_edit.html"),
        ),
    ] {
        assert!(
            !src.contains(" style=\""),
            "{name} must stay inline-style-free (#325) — use a component rule or an rcms- utility"
        );
    }
}

/// #325 — page_form.html (the editor, the original top offender) is now
/// fully de-inlined: the comment-thread + workflow chrome moved into
/// `.rcms-comment*` / `.card--error` / `.workflow-bar` rules, the rest
/// onto rcms- utilities. Lock it so a re-introduced inline `style=`
/// trips here. (`data-confirm-style="…"` is a data attribute, not an
/// inline style, and is correctly ignored by the ` style="` check.)
#[test]
fn page_form_is_inline_style_free() {
    const SRC: &str = include_str!("../src/admin/templates/page_form.html");
    assert!(
        !SRC.contains(" style=\""),
        "page_form.html must stay inline-style-free (#325) — use a \
         component rule or an rcms- utility instead"
    );
}

/// #318 — the page editor's save footer offers the three-action set
/// (Django/Wagtail `ModelForm` parity): plain Save, Save-and-continue,
/// and Save-and-add-another. Each is a submit button carrying a distinct
/// `_action` value that `redirect_after_page_save` routes on (list /
/// keep editing / new sibling). Lock all three in so a header refactor
/// can't silently regress the editor back to single-action.
#[tokio::test]
async fn page_form_save_footer_offers_three_actions() {
    let tera = Arc::new(fresh_tera());
    let ctx = page_form_ctx(); // mode = "edit", translation_mode = false
    let html = tera
        .render("rcms_admin/page_form.html", &ctx)
        .expect("render page_form.html");
    for action in ["save", "continue", "addanother"] {
        assert!(
            html.contains(&format!("name=\"_action\" value=\"{action}\"")),
            "page_form save footer is missing the `{action}` submit button (#318)"
        );
    }
}

/// Comprehensive `page_form.html` ctx-stub for the rendering tests.
/// The template touches dozens of context vars across its tabs — the
/// per-test inline stubs in this file drifted out of sync as the
/// form grew (workflow banners, privacy roles, lock state, comments,
/// references, plugin-contributed action menus, …) and individual
/// tests started failing with "Variable `…` not found" before the
/// assertion they cared about ran.
///
/// This helper bundles every var the current template reads, with
/// sensible defaults that exercise the "empty / default" branches.
/// Tests override only the keys they care about — e.g. flip
/// `display_fields` to populate the at-a-glance chip cluster, or
/// flip `restriction_kind` to exercise the Privacy tab's "password"
/// branch — without touching the rest.
fn page_form_ctx() -> Context {
    use serde_json::{json, Value};
    let mut ctx = test_ctx();
    // Chrome from `_base.html`.
    ctx.insert("active_tab", "pages");
    // Breadcrumb ancestors (epic: editor header breadcrumb trail).
    ctx.insert("ancestors", &Vec::<Value>::new());
    ctx.insert("brand_name", &"Test CMS");
    ctx.insert("tenant_slug", &"demo");
    ctx.insert("theme_css", &"");
    ctx.insert("csrf_token", &"test-token");
    ctx.insert("messages", &Vec::<Value>::new());
    // Form mode + the canonical page row.
    ctx.insert("mode", "edit");
    ctx.insert(
        "page",
        &json!({
            "id": 1, "title": "Hello", "slug": "hello", "status": "draft",
            "seo_title": "", "seo_description": "", "robots_index": true,
            "sitemap_priority": 0.5, "page_type_id": 1,
            "og_title": "", "og_description": "", "og_image_media_id": Value::Null,
            "twitter_card": "summary_large_image",
            "show_in_menus": false,
            "theme_id": Value::Null,
            "go_live_at": Value::Null, "expire_at": Value::Null,
            "url_path": "/hello",
        }),
    );
    ctx.insert("parent", &Value::Null);
    ctx.insert(
        "chosen_type",
        &json!({"id": 1, "type_name": "test_page", "verbose_name": "Test page"}),
    );
    // Type-extension surface.
    ctx.insert("extension_fields", &Vec::<Value>::new());
    ctx.insert("media_options", &Vec::<Value>::new());
    ctx.insert("inline_panels", &Vec::<Value>::new());
    ctx.insert("extra_tabs", &Vec::<Value>::new());
    // Locales / translation.
    ctx.insert("locales", &Vec::<Value>::new());
    ctx.insert("editing_locale", &Value::Null);
    ctx.insert("translation_mode", &false);
    ctx.insert("core_translations", &json!({}));
    ctx.insert("extra_translations", &Vec::<Value>::new());
    // Revisions panel.
    ctx.insert("revisions", &Vec::<Value>::new());
    // Theme + at-a-glance.
    ctx.insert("themes_for_picker", &Vec::<Value>::new());
    ctx.insert("display_fields", &Vec::<Value>::new());
    // Privacy tab (#76) — restriction_kind = none keeps the password
    // input + role chooser collapsed.
    ctx.insert("restriction_kind", &"none");
    ctx.insert("restriction_has_password", &false);
    ctx.insert("restriction_role_choices", &Vec::<Value>::new());
    ctx.insert("restriction_groups_str", &"");
    // Comments tab (#81) — no threads, no counters.
    ctx.insert("comment_threads", &Vec::<Value>::new());
    ctx.insert("comment_open_count", &0);
    ctx.insert("comment_resolved_count", &0);
    ctx.insert("comment_field_choices", &Vec::<Value>::new());
    // References tab (#146).
    ctx.insert("inbound_references", &Vec::<Value>::new());
    // Page tags + log.
    ctx.insert("page_tags_csv", &"");
    ctx.insert("page_log", &Vec::<Value>::new());
    ctx.insert("live_url", &"");
    // Lock + workflow banners (#73, #176).
    ctx.insert("lock_banner", &Value::Null);
    ctx.insert("lock_heartbeat_secs", &30);
    ctx.insert("workflow", &Value::Null);
    ctx.insert("workflow_current_task", &Value::Null);
    ctx.insert("workflow_history", &Vec::<Value>::new());
    ctx.insert("workflow_state", &Value::Null);
    ctx.insert("workflow_viewer_can_decide", &false);
    ctx.insert("workflow_slug", &Value::Null);
    ctx.insert("workflow_locked", &false);
    // Subscriptions (#207) + plugin-contributed page-action menu.
    ctx.insert("is_subscribed", &false);
    ctx.insert("plugin_page_action_menu_items", &Vec::<Value>::new());
    // a11y panel (#208).
    ctx.insert("a11y_errors", &0);
    // Page tags (#22) — autocomplete dataset.
    ctx.insert("all_tags", &Vec::<Value>::new());
    ctx
}

#[tokio::test]
async fn page_form_renders_at_a_glance_panel_when_display_fields_present() {
    use serde_json::json;
    let tera = Arc::new(fresh_tera());
    let mut ctx = page_form_ctx();
    ctx.insert(
        "display_fields",
        &json!([
            {"label": "Word count", "html": "<strong>42</strong>", "help": "", "icon": "format_list_numbered"},
            {"label": "Last edited", "html": "<strong>just now</strong>", "help": "", "icon": "history"}
        ]),
    );

    let html = tera
        .render("rcms_admin/page_form.html", &ctx)
        .expect("render page_form.html");
    let res = Html(html).into_response();

    // Display fields now render as compact chips in the dense topbar
    // (the `display-chip` cluster replaces the old "At a glance"
    // panel). Assert the chip chrome + at least one label make it
    // into the markup. Use the rendered attribute form so we don't
    // match the class name inside the `<style>` block.
    assert_status(&res, 200);
    assert_contains(res, "class=\"rcms-display-chip\"").await;
}

#[tokio::test]
async fn page_form_omits_display_chips_when_display_fields_empty() {
    let tera = Arc::new(fresh_tera());
    let ctx = page_form_ctx();
    // `display_fields` defaults to empty in `page_form_ctx`.

    let html = tera
        .render("rcms_admin/page_form.html", &ctx)
        .expect("render page_form.html");
    let res = Html(html).into_response();

    // Chip cluster has `{% if display_fields and display_fields | length > 0 %}`
    // around the chips, so empty list ⇒ no chip elements (the class
    // name itself appears in `<style>`, so we look for the rendered
    // markup `<span class="display-chip"` rather than just the
    // substring).
    assert_not_contains(res, "class=\"rcms-display-chip\"").await;
}

/// #265 — Privacy tab used to render an empty bordered card below
/// the privacy form. Regression catch: render the form, slice out
/// the privacy `<section data-tab-panel="privacy">`, count `card`-
/// class elements inside it. Exactly one card lives inside that
/// section — the privacy form itself.
#[tokio::test]
async fn privacy_tab_has_no_orphan_card() {
    let tera = Arc::new(fresh_tera());
    let ctx = page_form_ctx();
    let html = tera
        .render("rcms_admin/page_form.html", &ctx)
        .expect("render page_form.html");
    // Find the privacy tab's <section ...>...</section> slice.
    let open_marker = r#"data-tab-panel="privacy""#;
    let open_at = html
        .find(open_marker)
        .expect("privacy tab section must be present");
    // Walk back to the `<section` opener.
    let section_start = html[..open_at]
        .rfind("<section")
        .expect("privacy `<section` opener must be present");
    let close_rel = html[section_start..]
        .find("</section>")
        .expect("privacy section must close");
    let slice = &html[section_start..section_start + close_rel + "</section>".len()];

    // The privacy form itself carries `class="card"`. Anything else
    // with the card class inside the section is the regression. Count
    // both `class="card"` and `class="card ` (multi-class element)
    // and `card"` suffix on attribute fragments.
    let mut card_hits = 0usize;
    let mut cursor = 0usize;
    while let Some(rel) = slice[cursor..].find(r#"class="rcms-card"#) {
        card_hits += 1;
        cursor += rel + r#"class="rcms-card"#.len();
    }
    assert_eq!(
        card_hits, 1,
        "privacy tab section should contain exactly 1 .card element \
         (the privacy form). Found {card_hits}. Section dump:\n{slice}"
    );
}

#[tokio::test]
async fn empty_locale_list_renders_empty_state_not_table() {
    let tera = Arc::new(fresh_tera());
    let build_ctx = || {
        let mut c = test_ctx();
        c.insert("active_tab", "locales");
        c.insert("brand_name", &"Test CMS");
        c.insert("tenant_slug", &"demo");
        c.insert("theme_css", &"");
        c.insert("csrf_token", &"test-token");
        c.insert("messages", &Vec::<serde_json::Value>::new());
        c.insert("items", &Vec::<serde_json::Value>::new());
        c
    };

    let ctx = build_ctx();
    let html = tera
        .render("rcms_admin/locale_list.html", &ctx)
        .expect("render locale_list.html");
    let res = Html(html).into_response();

    assert_status(&res, 200);
    assert_contains(res, "Add the first locale").await;

    // Second render — `assert_not_contains` takes ownership of the
    // response, so we re-render against a fresh context.
    let ctx2 = build_ctx();
    let html2 = tera
        .render("rcms_admin/locale_list.html", &ctx2)
        .expect("render locale_list.html");
    let res2 = Html(html2).into_response();
    assert_not_contains(res2, "<table class=\"data\">").await;
}

/// #258 — the non-PG arm of `with_login_required` used to be a
/// silent no-op, leaving `/cms-admin/*` open to anonymous traffic.
/// The current impl is tri-dialect (built on the tri-dialect
/// `SessionUser` extractor) and 302s anonymous requests to the
/// configured login URL with `?next=` preserved.
///
/// This test runs without a `TenantContext` extension, which makes
/// `SessionUser` resolve to `None` regardless of feature flags — the
/// exact same shape as a real anonymous request hitting the gate
/// before the framework's tenant middleware has injected anything.
#[tokio::test]
async fn anonymous_cms_admin_redirects_to_login() {
    let tera = Arc::new(fresh_tera());
    let app = admin::with_login_required(admin::router(tera), "/login");

    let res = app
        .oneshot(
            Request::builder()
                .uri("/cms-admin/pages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");

    assert_eq!(res.status(), StatusCode::FOUND, "expected 302 to /login");
    let loc = res
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .expect("Location header on 302");
    assert_eq!(
        loc, "/login?next=%2Fcms-admin%2Fpages",
        "Location should bounce to the configured login URL with the original path in ?next=",
    );
}

/// Companion to the redirect test — confirm the framework's
/// `redirect_to_login` helper that `with_login_required` reuses is
/// actually re-exported from `rustango::auth_decorators` without a
/// PG cfg gate, so the non-PG arm above keeps compiling on every
/// backend.
#[test]
fn redirect_to_login_helper_is_available_off_pg() {
    let res = rustango::auth_decorators::redirect_to_login("/login", "next", "/cms-admin/pages");
    assert_eq!(res.status(), StatusCode::FOUND);
}

/// #710 — every vendored third-party asset is named in the notices the
/// binary serves, so a newly vendored file can't ship without one.
#[test]
fn every_vendored_asset_has_a_notice() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/admin/static/vendor");
    let notices = std::fs::read_to_string(root.join("THIRD-PARTY-NOTICES.txt")).expect("notices");
    let mut assets: Vec<String> = std::fs::read_dir(&root)
        .expect("vendor dir")
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".js") && !n.ends_with(".entry.js"))
        .collect();
    assets.push("fonts/text-*.woff2".to_owned());
    assets.push("fonts/icons-*.woff2".to_owned());
    for a in assets {
        assert!(notices.contains(&a), "{a} is vendored but has no entry in THIRD-PARTY-NOTICES.txt");
    }
    for licence in ["MIT License", "Mozilla Public License", "SIL OPEN FONT LICENSE", "Apache License"] {
        assert!(notices.to_uppercase().contains(&licence.to_uppercase()), "{licence} text missing");
    }
}

/// Snippet translation mode uses the page editor's side-by-side rows and
/// leaves the original edit form out, so an editor cannot change the
/// English text by mistake while translating.
#[tokio::test]
async fn snippet_translation_mode_shows_rows_not_the_edit_form() {
    let tera = fresh_tera();
    let mut ctx = test_ctx();
    ctx.insert("active_tab", "library");
    ctx.insert("brand_name", &"Test CMS");
    ctx.insert("csrf_token", &"test-token");
    ctx.insert("messages", &Vec::<serde_json::Value>::new());
    ctx.insert("mode", "edit");
    ctx.insert(
        "snippet",
        &serde_json::json!({
            "id": 7, "type_name": "Information", "slug": "shipping",
            "title": "Shipping", "folder_path": "", "body_markdown": "**Free** shipping",
            "data": {},
        }),
    );
    ctx.insert("types", &Vec::<serde_json::Value>::new());
    ctx.insert(
        "locales",
        &serde_json::json!([
            {"code": "en", "name": "English", "is_default": true},
            {"code": "fr", "name": "Français", "is_default": false},
        ]),
    );
    ctx.insert("translation_mode", &true);
    ctx.insert("editing_locale", &serde_json::json!({"code": "fr", "name": "Français"}));
    ctx.insert("core_translations", &serde_json::json!({"title": "Livraison"}));
    ctx.insert("extra_translations", &Vec::<(String, String)>::new());

    let html = tera
        .render("rcms_admin/snippet_form.html", &ctx)
        .expect("snippet_form.html renders in translation mode");
    assert!(!html.contains(r#"name="title""#), "original edit form is hidden");
    assert!(html.contains(r#"name="tr__title" value="Livraison" placeholder="Shipping""#));
    assert!(html.contains(r#"<div class="rcms-canonical-text">**Free** shipping</div>"#));
    assert!(html.contains("/cms-admin/library/7/translate?locale=fr"));
}

/// The Deactivate button submits its own form, which must not sit inside
/// the edit form: the browser drops a nested `<form>` and the button then
/// saved the user instead.
#[tokio::test]
async fn user_form_deactivate_form_is_not_nested() {
    let tera = fresh_tera();
    let mut ctx = test_ctx();
    ctx.insert("active_tab", "users");
    ctx.insert("brand_name", &"Test CMS");
    ctx.insert("csrf_token", &"test-token");
    ctx.insert("messages", &Vec::<serde_json::Value>::new());
    ctx.insert("mode", "edit");
    ctx.insert(
        "user",
        &serde_json::json!({
            "id": 2, "username": "cafe", "email": "hello@cafe.example", "active": true,
            "is_superuser": false, "timezone": "", "created_at": "2026-09-29T00:00:00Z",
            "password_changed_at": null,
        }),
    );
    ctx.insert("role_choices", &Vec::<serde_json::Value>::new());
    let html = tera
        .render("rcms_admin/user_form.html", &ctx)
        .expect("user_form.html renders");
    let edit_end = html.find("</form>").expect("edit form closes");
    let deactivate = html.find(r#"id="user-deactivate-form""#).expect("deactivate form present");
    assert!(deactivate > edit_end, "the deactivate form comes after the edit form closes");
    assert!(html.contains(r#"form="user-deactivate-form""#), "the button targets it");
    assert!(html.contains(r#"value="hello@cafe.example""#));
}

/// A typed site setting in another language: its text fields side by side,
/// posted to the translate route, the English form left out.
#[tokio::test]
async fn site_setting_translation_mode_shows_rows() {
    let tera = fresh_tera();
    let mut ctx = test_ctx();
    ctx.insert("active_tab", "site-settings");
    ctx.insert("brand_name", &"Test CMS");
    ctx.insert("csrf_token", &"test-token");
    ctx.insert("messages", &Vec::<serde_json::Value>::new());
    ctx.insert("scope", "brand");
    ctx.insert("is_new", &false);
    ctx.insert("typed", &true);
    ctx.insert("schema_label", "Brand");
    ctx.insert("widgets", &Vec::<serde_json::Value>::new());
    ctx.insert(
        "locales",
        &serde_json::json!([
            {"code": "en", "name": "English", "is_default": true},
            {"code": "fr", "name": "Français", "is_default": false},
        ]),
    );
    ctx.insert("translation_mode", &true);
    ctx.insert("editing_locale", &serde_json::json!({"code": "fr", "name": "Français"}));
    ctx.insert(
        "tr_rows",
        &serde_json::json!([
            {"name": "footer", "label": "Footer text", "canonical": "Made by hand.", "multiline": true, "value": "Fait main."},
        ]),
    );
    let html = tera
        .render("rcms_admin/site_setting_form.html", &ctx)
        .expect("site_setting_form.html renders in translation mode");
    // Tera escapes `/` in attributes; the browser reads it back.
    let html = html.replace("&#x2F;", "/");
    assert!(html.contains("/cms-admin/site-settings/brand/translate?locale=fr"));
    assert!(html.contains(r#"name="tr__footer""#));
    assert!(html.contains(">Fait main.</textarea>"));
    assert!(!html.contains("data-primary-save"), "the English form is left out");
}
