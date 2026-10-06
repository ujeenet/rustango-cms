//! Tests for the unified widget renderer — `_widget.html` Tera
//! macro driven by `Widget` + `WidgetKind`. Renders every kind
//! against a stubbed context and asserts the expected HTML element
//! lands in the body, using the framework's
//! `rustango::test_assertions::*` helpers consistently with the
//! rest of the suite.

use axum::response::{Html, IntoResponse, Response};
use rustango::test_assertions::{assert_contains, assert_not_contains};
use rustango_cms::admin;
use rustango_cms::{Widget, WidgetKind};
use tera::{Context, Tera};

fn fresh_tera() -> Tera {
    let mut tera = Tera::default();
    admin::register_templates(&mut tera).expect("register_templates");
    tera
}

/// Render the macro `widget::render(w)` against a stubbed
/// `_test.html` that imports it. Wraps in a `Response` for the
/// framework's assert helpers.
fn render_widget(tera: &Tera, w: &Widget) -> Response {
    let mut t = tera.clone();
    t.add_raw_template(
        "_test.html",
        "{% import \"rcms_admin/_widget.html\" as widget %}\
         {{ widget::render(w=w) }}",
    )
    .expect("add stub template");
    let mut ctx = Context::new();
    ctx.insert("w", w);
    // The i18n epic (#523) made `LANG` mandatory in the widget macro
    // (`translate(locale=LANG)` on chooser placeholders etc.).
    ctx.insert("LANG", "en");
    // Custom-widget branch reads `custom_html` directly off `w`, so
    // no `custom_widgets` context map needed here.
    let html = t.render("_test.html", &ctx).expect("render _test.html");
    Html(html).into_response()
}

#[tokio::test]
async fn text_renders_as_input_type_text() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Text, "title", "Title");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="text""#).await;
}

#[tokio::test]
async fn model_chooser_emits_slug_as_chooser_kind() {
    // #421 pt 2 — the registered chooser slug (custom_name) becomes
    // data-chooser-kind, which cms-ux.js routes to /cms-admin/__chooser/<slug>.
    let tera = fresh_tera();
    let w = Widget::model_chooser("author_id", "Author", "author");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"data-chooser-kind="author""#).await;
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"name="author_id""#).await;
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"data-chooser-input"#).await;
}

#[tokio::test]
async fn model_chooser_with_value_shows_id() {
    let tera = fresh_tera();
    let w = Widget::model_chooser("author_id", "Author", "author").with_value("42");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"value="42""#).await;
    let res = render_widget(&tera, &w);
    assert_contains(res, "#42").await;
}

#[tokio::test]
async fn textarea_renders_as_textarea_element() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Textarea, "body", "Body");
    let res = render_widget(&tera, &w);
    assert_contains(res, "<textarea").await;
}

#[tokio::test]
async fn markdown_renders_with_markdown_data_attr() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Markdown, "body", "Body");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"data-widget-mode="markdown""#).await;
}

#[tokio::test]
async fn email_renders_as_input_type_email() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Email, "addr", "Email");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="email""#).await;
}

#[tokio::test]
async fn url_renders_as_input_type_url() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Url, "site", "Website");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="url""#).await;
}

#[tokio::test]
async fn tel_renders_as_input_type_tel() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Tel, "phone", "Phone");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="tel""#).await;
}

#[tokio::test]
async fn password_renders_as_input_type_password() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Password, "pwd", "Password");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="password""#).await;
}

#[tokio::test]
async fn hidden_renders_as_input_type_hidden_with_no_label() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Hidden, "csrf", "CSRF").with_value("abc");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="hidden""#).await;
    // Hidden widgets MUST NOT render their label — that's the
    // whole point of being hidden.
    let res2 = render_widget(&tera, &w);
    assert_not_contains(res2, "<label").await;
}

#[tokio::test]
async fn number_renders_as_input_type_number_with_step() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Integer, "count", "Count");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="number""#).await;
    let res2 = render_widget(&tera, &w);
    assert_contains(res2, r#"step="1""#).await;
}

#[tokio::test]
async fn float_uses_step_any() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Float, "price", "Price");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"step="any""#).await;
}

#[tokio::test]
async fn range_renders_as_input_type_range_with_min_max() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Range, "vol", "Volume")
        .with_min(0.0)
        .with_max(100.0);
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="range""#).await;
    let res2 = render_widget(
        &tera,
        &Widget::new(WidgetKind::Range, "vol", "Volume")
            .with_min(0.0)
            .with_max(100.0),
    );
    assert_contains(res2, r#"max="100""#).await;
}

#[tokio::test]
async fn boolean_renders_as_checkbox() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Boolean, "active", "Active");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="checkbox""#).await;
}

#[tokio::test]
async fn boolean_with_value_on_renders_checked() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Boolean, "active", "Active").with_value("on");
    let res = render_widget(&tera, &w);
    assert_contains(res, "checked").await;
}

#[tokio::test]
async fn date_renders_as_input_type_date() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Date, "go_live", "Go live");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="date""#).await;
}

#[tokio::test]
async fn time_renders_as_input_type_time() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Time, "open", "Open");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="time""#).await;
}

#[tokio::test]
async fn datetime_renders_as_datetime_local() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Datetime, "starts", "Starts");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="datetime-local""#).await;
}

#[tokio::test]
async fn datetimetz_renders_paired_datetime_and_tz_select() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::DatetimeTz, "starts", "Starts");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="datetime-local""#).await;
    let res2 = render_widget(
        &tera,
        &Widget::new(WidgetKind::DatetimeTz, "starts", "Starts"),
    );
    assert_contains(res2, "America/New_York").await;
}

#[tokio::test]
async fn color_renders_as_input_type_color() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Color, "tint", "Tint").with_value("#ff8800");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="color""#).await;
    let res2 = render_widget(
        &tera,
        &Widget::new(WidgetKind::Color, "tint", "Tint").with_value("#ff8800"),
    );
    assert_contains(res2, "#ff8800").await;
}

#[tokio::test]
async fn file_renders_as_input_type_file() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::File, "doc", "Document");
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="file""#).await;
}

#[tokio::test]
async fn mediapicker_renders_as_media_chooser() {
    // The mediapicker widget is the shared media-picker dialog trigger:
    // a hidden input carrying the media id (FIRST control in the wrapper —
    // stream serialization takes the first input) + a "Choose an image…"
    // button wired via data-chooser-kind="media". Legacy `options` are
    // ignored by this arm (the picker fetches its own data).
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::MediaPicker, "hero", "Hero")
        .with_options([("1", "Banner one"), ("2", "Banner two")]);
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"data-chooser-kind="media""#).await;
    let res2 = render_widget(&tera, &Widget::new(WidgetKind::MediaPicker, "hero", "Hero"));
    assert_contains(res2, r#"name="hero""#).await;
    let res3 = render_widget(&tera, &Widget::new(WidgetKind::MediaPicker, "hero", "Hero"));
    assert_contains(res3, "data-chooser-open").await;
}

#[tokio::test]
async fn radio_renders_one_input_type_radio_per_option() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Radio, "size", "Size").with_options([
        ("s", "Small"),
        ("m", "Medium"),
        ("l", "Large"),
    ]);
    let res = render_widget(&tera, &w);
    assert_contains(res, r#"type="radio""#).await;
    let res2 = render_widget(
        &tera,
        &Widget::new(WidgetKind::Radio, "size", "Size").with_options([
            ("s", "Small"),
            ("m", "Medium"),
            ("l", "Large"),
        ]),
    );
    assert_contains(res2, "Medium").await;
}

#[tokio::test]
async fn select_renders_as_select_dropdown() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Select, "country", "Country")
        .with_options([("us", "United States"), ("uk", "United Kingdom")]);
    let res = render_widget(&tera, &w);
    assert_contains(res, "<select").await;
}

#[tokio::test]
async fn checkboxes_renders_multivalue_hidden_input_and_per_option_checkboxes() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Checkboxes, "tags", "Tags")
        .with_options([("a", "Alpha"), ("b", "Bravo")]);
    let res = render_widget(&tera, &w);
    // The multi-value contract — one hidden field that holds the
    // JSON-array string.
    assert_contains(res, r#"data-widget-multivalue="checkboxes""#).await;
    let res2 = render_widget(
        &tera,
        &Widget::new(WidgetKind::Checkboxes, "tags", "Tags")
            .with_options([("a", "Alpha"), ("b", "Bravo")]),
    );
    assert_contains(res2, r#"type="hidden""#).await;
}

#[tokio::test]
async fn multiselect_renders_native_select_multiple_with_hidden_companion() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::MultiSelect, "tags", "Tags").with_options([
        ("a", "Alpha"),
        ("b", "Bravo"),
        ("c", "Charlie"),
    ]);
    let res = render_widget(&tera, &w);
    assert_contains(res, "<select multiple").await;
}

#[tokio::test]
async fn required_renders_required_attr() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Text, "title", "Title").required();
    let res = render_widget(&tera, &w);
    assert_contains(res, "required").await;
}

#[tokio::test]
async fn read_only_renders_readonly_attr() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Text, "slug", "Slug")
        .with_value("locked")
        .read_only();
    let res = render_widget(&tera, &w);
    assert_contains(res, "readonly").await;
}

#[tokio::test]
async fn help_text_renders_under_input() {
    let tera = fresh_tera();
    let w = Widget::new(WidgetKind::Text, "slug", "Slug").with_help("Lowercase, hyphens only.");
    let res = render_widget(&tera, &w);
    assert_contains(res, "Lowercase, hyphens only.").await;
}

#[tokio::test]
async fn unknown_custom_name_surfaces_inline_error() {
    let tera = fresh_tera();
    let w = Widget::custom("unregistered_name", "loc", "Location");
    let res = render_widget(&tera, &w);
    // The macro's custom-widget branch emits a visible error when
    // `custom_html` is empty (registration missing). Editors see
    // this so misconfiguration is debuggable in-editor.
    assert_contains(res, "has no registered template").await;
}

#[tokio::test]
async fn custom_widget_html_renders_safely() {
    let tera = fresh_tera();
    let mut w = Widget::custom("latlng", "loc", "Location");
    // The admin handler stamps `custom_html` after looking up the
    // registered template; this test injects it directly to assert
    // the macro emits the HTML through `| safe`.
    w.custom_html = "<div class=\"map-preview\">42, -71</div>".to_owned();
    let res = render_widget(&tera, &w);
    assert_contains(res, "map-preview").await;
}

#[test]
fn widget_kind_as_tag_round_trips_via_serde() {
    // The macro's `{% if w.kind == "datetime" %}` checks depend on
    // `WidgetKind` serializing to its `as_tag()` form. Pin that
    // contract here so a refactor of either side surfaces.
    for kind in [
        WidgetKind::Text,
        WidgetKind::Textarea,
        WidgetKind::Markdown,
        WidgetKind::RichText,
        WidgetKind::Email,
        WidgetKind::Url,
        WidgetKind::Tel,
        WidgetKind::Password,
        WidgetKind::Hidden,
        WidgetKind::Number,
        WidgetKind::Integer,
        WidgetKind::Float,
        WidgetKind::Range,
        WidgetKind::Boolean,
        WidgetKind::Date,
        WidgetKind::Time,
        WidgetKind::Datetime,
        WidgetKind::DatetimeTz,
        WidgetKind::Color,
        WidgetKind::File,
        WidgetKind::MediaPicker,
        WidgetKind::Radio,
        WidgetKind::Select,
        WidgetKind::Checkboxes,
        WidgetKind::MultiSelect,
        WidgetKind::ModelChooser,
        WidgetKind::Custom,
    ] {
        let json = serde_json::to_string(&kind).expect("serialize");
        assert_eq!(
            json,
            format!("\"{}\"", kind.as_tag()),
            "WidgetKind serde does not match as_tag() for {kind:?}",
        );
    }
}
