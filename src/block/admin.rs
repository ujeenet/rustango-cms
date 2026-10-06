//! Admin-side editor rendering for `WidgetKind::Stream` widgets.
//!
//! The page-edit form pre-renders the entire stream editor HTML in
//! Rust before passing it through Tera — mirrors the existing
//! `Widget::custom_html` stash pattern in `src/admin/handlers.rs`.
//! The output is one big string of `<div data-stream-block>` nodes
//! plus `<template>` clones the client-side JS uses on insert.
//!
//! ## Walk shape
//!
//! Top-level entrypoint: [`render_stream_editor`].
//!
//! For each existing block in the stored JSON:
//! 1. Look up the [`crate::Block`] via the registry.
//! 2. Render its header (icon + verbose_name + reorder/remove buttons).
//! 3. Render its body — one wrapper per [`crate::BlockField`]:
//!    - `Widget`: build a one-off [`crate::Widget`] with `name = ""`
//!      so it doesn't accidentally submit, then call the existing
//!      `widgets::render(w)` Tera macro.
//!    - `Stream` / `Repeat`: recurse.
//!    - `Computed`: render inline via the field's render fn.
//!
//! For each registered block type, render an *empty-block* clone
//! into a `<template data-block-template="X">` element — the JS
//! clones these on insert so new blocks land with the right shape
//! without an admin round-trip.
//!
//! ## Form submission
//!
//! Every nested `<input>` / `<select>` / `<textarea>` is rendered with
//! `name=""` so the browser skips it on submit per HTML5
//! § 4.10.18.3 (5.5). Only the single hidden `<input>` carrying the
//! serialized JSON tree (`name="{stream_name}"`) participates in the
//! POST. The JS rewrites this hidden value on every `input` event.

use serde_json::Value;
use tera::{Context, Tera};

use crate::block::{self, BlockField};
use crate::widget::{Widget, WidgetKind};

/// Top-level entry: render the entire editor HTML for one
/// `WidgetKind::Stream` widget.
///
/// `widget.value` is the JSON-string the handler loaded; allowed
/// types live in `widget.allowed`. Returns the editor body — the
/// caller stamps it onto `widget.custom_html` so `_widget.html`'s
/// stream arm can emit it via `{{ w.custom_html | safe }}`.
#[must_use]
pub fn render_stream_editor(
    widget: &Widget,
    tera: &Tera,
) -> String {
    render_stream_editor_with(widget, tera, None)
}

/// [`render_stream_editor`] with a page-builder dyn-block overlay (#559):
/// UI-defined block types resolve through `dyn_set` before the inventory
/// registry, so their entries + clone-templates render exactly like code
/// blocks — `stream_editor.js` needs no changes.
#[must_use]
pub fn render_stream_editor_with(
    widget: &Widget,
    tera: &Tera,
    dyn_set: Option<&crate::page_builder::DynBlockSet>,
) -> String {
    let parsed: Value = serde_json::from_str(&widget.value).unwrap_or(Value::Array(Vec::new()));
    let blocks_arr = parsed.as_array().cloned().unwrap_or_default();

    let mut out = String::new();

    // List of existing blocks (top-level stream).
    out.push_str(r#"<div class="rcms-stream-list" data-stream-list>"#);
    for entry in &blocks_arr {
        if let Some(html) = render_block_entry(entry, tera, dyn_set) {
            out.push_str(&html);
        }
    }
    out.push_str("</div>");

    // Per-block-type clone templates. JS reads
    // `<template data-block-template="X">` and clones its content on
    // insert. Walk `widget.allowed` transitively — nested Stream /
    // Repeat fields reference other block types whose templates must
    // also be available at the root for the JS to clone from.
    let mut needed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut frontier: Vec<String> = widget.allowed.to_vec();
    while let Some(name) = frontier.pop() {
        if !needed.insert(name.clone()) {
            continue;
        }
        if let Some(block) = crate::page_builder::resolve_block(dyn_set, &name) {
            for field in block.fields() {
                match field {
                    BlockField::Stream { allowed, .. } => {
                        for a in allowed {
                            if !needed.contains(&a) {
                                frontier.push(a);
                            }
                        }
                    }
                    BlockField::Repeat { item_type, .. } => {
                        if !needed.contains(&item_type) {
                            frontier.push(item_type);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // Clone-template bodies are rendered from an EMPTY value, so for a
    // code-registered block the output depends only on the block type —
    // never on page content. Rendering them per request was
    // the single most expensive thing the page editor did (measured: 63ms of
    // a 69ms release-build request, ~91%, on a page with an *empty* stream —
    // 25 `_widget.html` sub-renders). Memoize them process-wide.
    //
    // UI-defined (page-builder) blocks are NOT cached: their schema can
    // change at runtime, so they always render live.
    for ref_name in &needed {
        let is_dyn = dyn_set.map_or(false, |s| s.get(ref_name).is_some());
        if is_dyn {
            if let Some(block) = crate::page_builder::resolve_block(dyn_set, ref_name) {
                let empty_body = render_block_inline(
                    &*block,
                    &Value::Object(Default::default()),
                    None,
                    tera,
                    dyn_set,
                );
                out.push_str(&format!(
                    r#"<template data-block-template="{}">{}</template>"#,
                    tera::escape_html(ref_name),
                    empty_body,
                ));
            }
            continue;
        }
        let key = ref_name.clone();
        if let Some(hit) = template_cache()
            .lock()
            .ok()
            .and_then(|c| c.get(&key).cloned())
        {
            out.push_str(&hit);
            continue;
        }
        if let Some(block) = crate::page_builder::resolve_block(dyn_set, ref_name) {
            let empty_body = render_block_inline(
                &*block,
                &Value::Object(Default::default()),
                None,
                tera,
                dyn_set,
            );
            let rendered = format!(
                r#"<template data-block-template="{}">{}</template>"#,
                tera::escape_html(ref_name),
                empty_body,
            );
            if let Ok(mut c) = template_cache().lock() {
                c.insert(key, rendered.clone());
            }
            out.push_str(&rendered);
        }
    }
    out
}

/// Process-wide memo of rendered clone-templates for **code-registered**
/// blocks, keyed by block type — bounded by the registered block types.
/// Image blocks render the shared chooser, not an option list, so the
/// media library is no part of the key (#717).
type TemplateCache = std::sync::Mutex<std::collections::HashMap<String, String>>;

fn template_cache() -> &'static TemplateCache {
    static CACHE: std::sync::OnceLock<TemplateCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Render one block entry from the stored JSON. Returns `None` for
/// unrecognized `type` — the caller treats this as drift and emits
/// the yellow banner.
fn render_block_entry(
    entry: &Value,
    tera: &Tera,
    dyn_set: Option<&crate::page_builder::DynBlockSet>,
) -> Option<String> {
    let type_name = entry.get("type").and_then(Value::as_str)?;
    let id = entry.get("id").and_then(Value::as_str).map(str::to_owned);
    let value = entry.get("value").unwrap_or(&Value::Null);

    let block = crate::page_builder::resolve_block(dyn_set, type_name)?;
    Some(render_block_inline(
        &*block,
        value,
        id.as_deref(),
        tera,
        dyn_set,
    ))
}

/// Render one block — header + body. `id` is `None` for the empty
/// clone-template (JS mints a fresh UUID on insert).
fn render_block_inline(
    block: &dyn block::Block,
    value: &Value,
    id: Option<&str>,
    tera: &Tera,
    dyn_set: Option<&crate::page_builder::DynBlockSet>,
) -> String {
    let mut html = String::new();
    let id_attr = match id {
        Some(s) => format!(r#" data-id="{}""#, tera::escape_html(s)),
        None => String::new(),
    };
    // Block::label_format() — sprintf-like template ({field}
    // placeholders). When set, stamps the format string as
    // `data-label-format` so the JS live-updates the collapsed header
    // on field edits. Also pre-substitutes the initial value
    // server-side so the first paint shows the right label without a
    // JS round-trip.
    let label_format_attr = match block.label_format() {
        Some(fmt) => format!(r#" data-label-format="{}""#, tera::escape_html(fmt),),
        None => String::new(),
    };
    html.push_str(&format!(
        r#"<div class="rcms-stream-block" data-stream-block data-type="{type}"{id} data-version="{version}"{label_format}>"#,
        type = tera::escape_html(block.type_name()),
        id = id_attr,
        version = block.version(),
        label_format = label_format_attr,
    ));

    // Compute the initial label — substitute the format string
    // against the current block value. Empty / no-format → fall
    // back to verbose_name.
    let initial_label = match block.label_format() {
        Some(fmt) => {
            let rendered = format_label(fmt, value);
            if rendered.trim().is_empty() {
                block.verbose_name().to_owned()
            } else {
                rendered
            }
        }
        None => block.verbose_name().to_owned(),
    };

    // -- header --
    html.push_str(r#"<div class="rcms-stream-block-header">"#);
    if let Some(icon) = block.icon() {
        html.push_str(&format!(
            r#"<span class="material-symbols-rounded sm rcms-stream-block-icon">{}</span>"#,
            tera::escape_html(icon),
        ));
    }
    html.push_str(&format!(
        r#"<strong class="rcms-stream-block-title" data-stream-block-title>{}</strong>"#,
        tera::escape_html(&initial_label),
    ));
    html.push_str(concat!(
        r#"<span class="rcms-stream-block-actions">"#,
        r#"<button type="button" class="rcms-btn" data-action="toggle-collapse" title="Collapse / expand"><span class="material-symbols-rounded sm">unfold_less</span></button>"#,
        r#"<button type="button" class="rcms-btn" data-action="move-up" title="Move up"><span class="material-symbols-rounded sm">arrow_upward</span></button>"#,
        r#"<button type="button" class="rcms-btn" data-action="move-down" title="Move down"><span class="material-symbols-rounded sm">arrow_downward</span></button>"#,
        r#"<button type="button" class="rcms-btn" data-action="duplicate" title="Duplicate"><span class="material-symbols-rounded sm">content_copy</span></button>"#,
        r#"<button type="button" class="rcms-btn" data-action="remove" title="Remove"><span class="material-symbols-rounded sm">close</span></button>"#,
        r#"</span>"#,
    ));
    html.push_str("</div>");

    // -- body --
    html.push_str(r#"<div class="rcms-stream-block-body">"#);
    for field in block.fields() {
        match field {
            BlockField::Widget {
                name,
                label,
                widget,
                options,
                help,
                required,
                meta,
            } => {
                let field_value = value.get(&name).cloned().unwrap_or(Value::Null);
                let widget_value = json_to_widget_value(&field_value, &widget);
                // #155 — uniquify widget name per block instance so
                // duplicate `id="w_…"` attributes don't collide
                // across multiple blocks of the same type. The JSON
                // serializer reads via `data-stream-field` (not by
                // widget name) so the namespaced name is purely a
                // DOM-hygiene fix.
                let instance_name = match id {
                    Some(block_id) => format!("blk_{block_id}__{name}"),
                    None => format!("blk_tpl__{name}"),
                };
                let mut w = Widget::new(widget, instance_name, label);
                w.value = widget_value;
                if let Some(h) = help {
                    w.help = h;
                }
                w.required = required;
                w.options = options
                    .iter()
                    .map(|(v, l)| (v.clone(), l.clone()))
                    .collect();
                // Phase A (Wagtail-parity validators) — thread the
                // BlockFieldMeta sub-struct into the rendered Widget.
                // Meta entries win when both meta + widget-level slot
                // are set; defaults preserve existing behaviour.
                if let Some(n) = meta.min_length {
                    w.min_length = Some(n);
                }
                if let Some(n) = meta.max_length {
                    w.max_length = Some(n);
                }
                if let Some(p) = meta.pattern.clone() {
                    w.pattern = p;
                }
                if let Some(v) = meta.min_value {
                    w.min = Some(v);
                }
                if let Some(v) = meta.max_value {
                    w.max = Some(v);
                }
                // Read by `_widget.html` as `data-chooser-filter`.
                if let Some(f) = meta.chooser_filter.clone() {
                    w.custom_name = f;
                }
                // Default value seeds the editor's initial render
                // when there's no stored row yet (the empty-block
                // template gets the default in its value attr).
                if w.value.is_empty() {
                    if let Some(d) = meta.default_value.clone() {
                        w.value = d;
                    }
                }
                let widget_html = render_widget_via_macro(&w, tera);
                html.push_str(&format!(
                    r#"<div class="rcms-stream-field" data-stream-field="{}">{}</div>"#,
                    tera::escape_html(&name),
                    widget_html,
                ));
            }
            BlockField::Stream {
                name,
                label,
                allowed,
                ..
            } => {
                let nested_value = value
                    .get(&name)
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new()));
                let nested_arr = nested_value.as_array().cloned().unwrap_or_default();
                let allowed_csv = allowed.join(",");
                html.push_str(&format!(
                    r#"<fieldset class="rcms-stream-nested" data-stream-field="{name}"><legend>{label}</legend>"#,
                    name = tera::escape_html(&name),
                    label = tera::escape_html(&label),
                ));
                html.push_str(&format!(
                    r#"<div class="rcms-stream-list" data-stream-list data-allowed="{}">"#,
                    tera::escape_html(&allowed_csv),
                ));
                for entry in &nested_arr {
                    if let Some(inner) = render_block_entry(entry, tera, dyn_set) {
                        html.push_str(&inner);
                    }
                }
                html.push_str("</div>");
                // Nested Add-block trigger — the JS resolves the
                // target list (the previous sibling `[data-stream-list]`)
                // and the templates from the outer `[data-stream-root]`.
                html.push_str(
                    r#"<div class="rcms-stream-picker"><button type="button" class="rcms-btn" data-action="open-picker" data-insert-at="end"><span class="material-symbols-rounded sm">add</span> Add block</button></div>"#,
                );
                html.push_str("</fieldset>");
            }
            BlockField::Repeat {
                name,
                label,
                item_type,
                ..
            } => {
                let nested_value = value
                    .get(&name)
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new()));
                let nested_arr = nested_value.as_array().cloned().unwrap_or_default();
                html.push_str(&format!(
                    r#"<fieldset class="rcms-stream-nested" data-stream-field="{name}"><legend>{label}</legend>"#,
                    name = tera::escape_html(&name),
                    label = tera::escape_html(&label),
                ));
                html.push_str(&format!(
                    r#"<div class="rcms-stream-list" data-stream-list data-allowed="{}" data-repeat="1">"#,
                    tera::escape_html(&item_type),
                ));
                for entry in &nested_arr {
                    if let Some(inner) = render_block_entry(entry, tera, dyn_set) {
                        html.push_str(&inner);
                    }
                }
                html.push_str("</div>");
                // Repeat fields skip the picker (only one type allowed)
                // — the JS recognises `data-repeat="1"` on the list and
                // mints a row of that type directly.
                html.push_str(&format!(
                    r#"<div class="rcms-stream-picker"><button type="button" class="rcms-btn" data-action="open-picker" data-insert-at="end"><span class="material-symbols-rounded sm">add</span> Add {}</button></div>"#,
                    tera::escape_html(&item_type),
                ));
                html.push_str("</fieldset>");
            }
            BlockField::Computed {
                name,
                label,
                render,
                ..
            } => {
                let value_html = render(value, &block::BlockRenderCtx::new(tera));
                html.push_str(&format!(
                    r#"<div class="rcms-stream-field rcms-stream-field-computed" data-stream-field="{name}"><label>{label}</label><div class="rcms-computed-value">{value_html}</div></div>"#,
                    name = tera::escape_html(&name),
                    label = tera::escape_html(&label),
                    value_html = value_html,
                ));
            }
        }
    }
    html.push_str("</div>"); // /stream-block-body
    html.push_str("</div>"); // /stream-block
    html
}

/// Convert a JSON value back to the string form widgets expect.
/// Scalars stringify. Objects / arrays serialize back to JSON for
/// multi-value widgets (`Checkboxes`, `MultiSelect`, `Stream`).
fn json_to_widget_value(value: &Value, kind: &WidgetKind) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => {
            if *b {
                "true".to_owned()
            } else {
                String::new()
            }
        }
        Value::Number(n) => n.to_string(),
        Value::Array(_) | Value::Object(_) => {
            if matches!(
                kind,
                WidgetKind::Checkboxes | WidgetKind::MultiSelect | WidgetKind::Stream
            ) {
                serde_json::to_string(value).unwrap_or_else(|_| "[]".to_owned())
            } else {
                value.to_string()
            }
        }
    }
}

/// Render a single widget by calling the existing `widgets::render(w)`
/// Tera macro — same path the rest of the admin uses. We thread it
/// through a one-shot inline template so we get the exact same HTML.
pub fn render_widget_via_macro(w: &Widget, tera: &Tera) -> String {
    render_widget_via_macro_with(w, tera, &[])
}

/// [`render_widget_via_macro`] with an explicit block picker (#564).
/// A `Stream` widget's `_widget.html` arm reads `picker_options` from
/// context to populate its block-picker dialog; the plain macro path
/// leaves it empty. Page-builder flex zones / repeaters need their
/// allowed dyn blocks in the picker so editors can add entries.
#[must_use]
pub fn render_widget_via_macro_with(
    w: &Widget,
    tera: &Tera,
    picker_options: &[PickerOption],
) -> String {
    let mut ctx = Context::new();
    ctx.insert("w", w);
    ctx.insert("picker_options", picker_options);
    // `_widget.html` chrome strings (chooser/media buttons, "— no selection —")
    // run through `translate(locale=LANG)`; without LANG those branches error
    // and render empty. Block field labels are English source strings, so the
    // English catalog is the right default here.
    ctx.insert("LANG", "en");

    // Fast path: `register_templates` registers the wrapper once, so we can
    // render straight off the shared `&Tera`. This function is called once
    // per widget (~173x on a content-heavy page edit); the old path did
    // `tera.clone()` + `add_raw_template` EVERY time, deep-copying every
    // compiled admin template -- ~2.4ms a call, 423ms a page.
    if let Ok(html) = tera.render(SHARED_WRAPPER, &ctx) {
        return html;
    }

    // Fallback for hosts that never called `register_templates`: register the
    // wrapper on a throwaway clone (the original behaviour).
    let mut t = tera.clone();
    if t.add_raw_template(SHARED_WRAPPER, WRAPPER_SRC).is_err() {
        return String::new();
    }
    t.render(SHARED_WRAPPER, &ctx).unwrap_or_default()
}

/// Name of the one-shot widget wrapper registered by
/// [`crate::admin::register_templates`].
const SHARED_WRAPPER: &str = "rcms_admin/_stream_widget.html";
/// Source for [`SHARED_WRAPPER`], used only by the un-registered fallback.
const WRAPPER_SRC: &str =
    "{% import \"rcms_admin/_widget.html\" as widget %}{{ widget::render(w=w) }}";

/// Picker option metadata stamped into the page-edit Tera context.
/// The block-picker dialog reads from `picker_options` and filters
/// by `data-allowed` at click time.
#[derive(serde::Serialize)]
pub struct PickerOption {
    pub type_name: String,
    pub verbose_name: String,
    pub icon: Option<String>,
    pub group: Option<String>,
    /// Picker-tile tooltip — `Block::description()`.
    pub description: Option<String>,
    /// Initial-value blob, serialized JSON of `Block::default_value()`.
    /// The stream editor merges this into the empty-instance dict
    /// when the editor adds a new block.
    pub default_value: Option<serde_json::Value>,
    /// Whether new instances start collapsed in the editor —
    /// `Block::collapsed()`. The stream-block header reads this to
    /// decide initial expand state.
    pub collapsed: bool,
    /// Pre-rendered preview HTML — `Block::preview_value()` run
    /// through the block's render template (or
    /// `Block::preview_template()` when set). The picker tile renders
    /// this inline so editors can see "what does this look like"
    /// before inserting. Empty for blocks that opt out of previews.
    pub preview_html: String,
}

/// Build the picker-option list from the global registry. Stamped
/// into Tera context as `picker_options` by `page_edit_form`.
///
/// `tera` is used to render each block's preview thumbnail. Pass the
/// admin Tera instance — block templates live in the same namespace
/// as the rest of the admin templates.
#[must_use]
pub fn picker_options(tera: &Tera) -> Vec<PickerOption> {
    let render_ctx = block::BlockRenderCtx::new(tera);
    block::registered_blocks()
        .map(|b| {
            let preview_html = match b.preview_value() {
                Some(sample) => {
                    // Honour Block::preview_template() override —
                    // wrap in a Tera context manually so we render
                    // the chosen template (not the public one).

                    match b.preview_template() {
                        Some(custom_tpl) => {
                            render_with_template(custom_tpl, &sample, b.type_name(), tera)
                        }
                        None => b.render(&sample, &render_ctx).unwrap_or_default(),
                    }
                }
                None => String::new(),
            };
            PickerOption {
                type_name: b.type_name().to_owned(),
                verbose_name: b.verbose_name().to_owned(),
                icon: b.icon().map(str::to_owned),
                group: b.group().map(str::to_owned),
                description: b.description().map(str::to_owned),
                default_value: b.default_value(),
                collapsed: b.collapsed(),
                preview_html,
            }
        })
        .collect()
}

/// Render `sample_value` through `template` for picker preview.
/// Mirrors the default-render Tera context shape so the same
/// `{{ value.foo }}` markup works in both the preview and the public
/// template.
fn render_with_template(
    template: &str,
    sample_value: &Value,
    type_name: &str,
    tera: &Tera,
) -> String {
    let mut ctx = Context::new();
    ctx.insert("value", sample_value);
    ctx.insert("block_type", type_name);
    ctx.insert("computed", &Value::Object(serde_json::Map::new()));
    tera.render(template, &ctx).unwrap_or_default()
}

/// Substitute `{field_name}` tokens in a label-format string against
/// a block's value dict. Missing fields render as empty (matches
/// the JS-side substitution behaviour in stream_editor.js).
///
/// Used by [`render_block_inline`] for the initial server-rendered
/// label and mirrored client-side by the JS for live updates on
/// field input.
fn format_label(fmt: &str, value: &Value) -> String {
    let obj = value.as_object();
    let mut out = String::with_capacity(fmt.len());
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            // Gobble the field name up to the next '}'.
            let mut name = String::new();
            let mut closed = false;
            while let Some(&next) = chars.peek() {
                chars.next();
                if next == '}' {
                    closed = true;
                    break;
                }
                name.push(next);
            }
            if !closed {
                // Unterminated `{…` — emit verbatim.
                out.push('{');
                out.push_str(&name);
                continue;
            }
            let resolved = obj
                .and_then(|m| m.get(name.trim()))
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => other.to_string().trim_matches('"').to_owned(),
                })
                .unwrap_or_default();
            out.push_str(&resolved);
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod label_format_tests {
    use super::*;

    #[test]
    fn substitutes_single_field() {
        let v = serde_json::json!({"text": "Welcome"});
        assert_eq!(format_label("{text}", &v), "Welcome");
    }

    #[test]
    fn substitutes_multiple_fields() {
        let v = serde_json::json!({"text": "Welcome", "level": "2"});
        assert_eq!(format_label("{text} (H{level})", &v), "Welcome (H2)");
    }

    #[test]
    fn empty_for_missing_fields() {
        let v = serde_json::json!({"text": "Welcome"});
        assert_eq!(format_label("{text} — {missing}", &v), "Welcome — ");
    }

    #[test]
    fn unterminated_brace_passes_through() {
        let v = serde_json::json!({});
        assert_eq!(format_label("hello {oops", &v), "hello {oops");
    }

    #[test]
    fn null_field_is_empty() {
        let v = serde_json::json!({"text": null});
        assert_eq!(format_label("[{text}]", &v), "[]");
    }
}
