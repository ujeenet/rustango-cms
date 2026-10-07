//! Page-value bridge — turns a compiled schema + a page's
//! stored values into editor HTML, and a posted form back into stored
//! values.
//!
//! Values live in `cms_page_builder_data.data` as one JSON object keyed
//! by field/group/zone key. Scalars coerce per widget kind (shared with
//! the page-type `preview_extension` rules); groups nest; repeaters/flex
//! zones store the stream wire format `[{type,id,value,version}]`.
//! Structure edits never touch stored data — [`load_upgraded`] fills
//! defaults for new keys and parks removed keys under `_orphaned`, lazily,
//! on next open (persisted only on the next save).

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::page_builder::compile::{BodyItem, CompiledSchema, FIELD_PREFIX};
use crate::page_builder::model;
use crate::widget::{Widget, WidgetKind};

/// Coerce one posted form value into typed JSON per widget kind. The only
/// copy of the rule: `PageTypeHandler::preview_extension` calls it too, so
/// a builder field and a page field of the same kind serialize identically.
/// Chooser ids become `i64` (empty = null), as they are stored.
#[must_use]
pub fn coerce(kind: WidgetKind, raw: &str) -> Value {
    match kind {
        WidgetKind::Checkboxes
        | WidgetKind::MultiSelect
        | WidgetKind::Stream
        | WidgetKind::SnippetM2M => {
            serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::Array(Vec::new()))
        }
        WidgetKind::Boolean => Value::Bool(matches!(raw, "on" | "true" | "1" | "yes")),
        WidgetKind::MediaPicker
        | WidgetKind::PageChooser
        | WidgetKind::SnippetChooser
        | WidgetKind::DocumentChooser => {
            if raw.trim().is_empty() {
                Value::Null
            } else {
                raw.parse::<i64>().map(Value::from).unwrap_or(Value::Null)
            }
        }
        WidgetKind::Integer => raw.parse::<i64>().map(Value::from).unwrap_or(Value::Null),
        WidgetKind::Number | WidgetKind::Float | WidgetKind::Range => raw
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        _ => Value::String(raw.to_owned()),
    }
}

/// Turn a stored JSON value back into an input string for prefilling a
/// widget (`value` field). Arrays/objects re-serialize to JSON (stream
/// widgets); scalars stringify.
fn value_to_input(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => {
            if *b {
                "on".to_owned()
            } else {
                String::new()
            }
        }
        Value::Array(_) | Value::Object(_) => serde_json::to_string(v).unwrap_or_default(),
        // Numbers are stored as floats; a whole one shows as `32`, not `32.0`.
        Value::Number(n) if n.as_f64().is_some_and(|f| f.fract() == 0.0 && f.abs() < 1e15) => {
            format!("{}", n.as_f64().unwrap_or_default() as i64)
        }
        other => other.to_string(),
    }
}

/// Component versions referenced by a compiled schema (`slug → version`),
/// for the lazy-upgrade snapshot.
fn component_versions(compiled: &CompiledSchema) -> Value {
    use crate::block::Block as _;
    let mut m = Map::new();
    for (name, def) in &compiled.dyn_blocks {
        if let Some(slug) = name.strip_prefix("c_") {
            m.insert(slug.to_owned(), Value::from(def.version()));
        }
    }
    Value::Object(m)
}

/// Walk the compiled body → a typed value map from a posted form. Only
/// declared keys are kept; groups reassemble to nested objects, zones
/// keep the stream array. Shared by [`save_from_form`] and the unsaved
/// preview path so on-screen edits render identically to saves.
#[must_use]
pub fn build_values(
    compiled: &CompiledSchema,
    form: &HashMap<String, String>,
) -> Map<String, Value> {
    let mut data = Map::new();
    for item in &compiled.body {
        match item {
            BodyItem::Field(w) => {
                data.insert(
                    strip(&w.name),
                    coerce(w.kind, form.get(&w.name).map_or("", String::as_str)),
                );
            }
            BodyItem::Row(cells) => {
                for (w, _) in cells {
                    data.insert(
                        strip(&w.name),
                        coerce(w.kind, form.get(&w.name).map_or("", String::as_str)),
                    );
                }
            }
            BodyItem::Group { key, items, .. } => {
                let mut obj = Map::new();
                collect_group(items, form, &mut obj);
                data.insert(key.clone(), Value::Object(obj));
            }
            BodyItem::Zone { key, widget, .. } => {
                let raw = form.get(&widget.name).map_or("", String::as_str);
                data.insert(
                    key.clone(),
                    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::Array(Vec::new())),
                );
            }
        }
    }
    data
}

/// The inverse of [`build_values`]: seed a form map from a page's stored
/// values.
///
/// [`save_from_form`] rebuilds the whole body from the form, so any
/// declared key the form does not carry is written back **empty** — a
/// zone becomes `[]`. That is right for the admin editor, which always
/// posts every widget, and catastrophic for a partial programmatic edit
/// (the MCP `update_page`), where changing only an SEO title would
/// otherwise delete the page's entire body. Callers that build a form
/// by hand prefill with this first.
pub fn prefill_form(compiled: &CompiledSchema, values: &Value, form: &mut HashMap<String, String>) {
    let Some(obj) = values.as_object() else {
        return;
    };
    for item in &compiled.body {
        match item {
            BodyItem::Field(w) => prefill_one(form, &w.name, obj.get(&strip(&w.name))),
            BodyItem::Row(cells) => {
                for (w, _) in cells {
                    prefill_one(form, &w.name, obj.get(&strip(&w.name)));
                }
            }
            BodyItem::Group { key, items, .. } => {
                if let Some(g) = obj.get(key).and_then(Value::as_object) {
                    prefill_group(items, g, form);
                }
            }
            BodyItem::Zone { key, widget, .. } => {
                prefill_one(form, &widget.name, obj.get(key));
            }
        }
    }
}

fn prefill_one(form: &mut HashMap<String, String>, name: &str, v: Option<&Value>) {
    if let Some(v) = v {
        form.insert(name.to_owned(), value_to_input(v));
    }
}

fn prefill_group(items: &[BodyItem], stored: &Map<String, Value>, form: &mut HashMap<String, String>) {
    for item in items {
        match item {
            BodyItem::Field(w) => prefill_one(form, &w.name, stored.get(&strip_group(&w.name))),
            BodyItem::Row(cells) => {
                for (w, _) in cells {
                    prefill_one(form, &w.name, stored.get(&strip_group(&w.name)));
                }
            }
            BodyItem::Group { key, items, .. } => {
                if let Some(g) = stored.get(key).and_then(Value::as_object) {
                    prefill_group(items, g, form);
                }
            }
            BodyItem::Zone { key, widget, .. } => {
                prefill_one(form, &widget.name, stored.get(key));
            }
        }
    }
}

/// Persist a page's builder values from the posted form. Walks the
/// compiled body (not the raw form) so only declared keys are stored;
/// groups reassemble to nested objects, zones keep the stream array.
///
/// # Errors
/// Propagates the value upsert's DB failures.
pub async fn save_from_form(
    pool: &rustango::sql::Pool,
    page_id: i64,
    compiled: &CompiledSchema,
    form: &HashMap<String, String>,
) -> Result<(), rustango::sql::ExecError> {
    let mut data = build_values(compiled, form);
    // Carry forward removed-field data. Any stored top-level key the
    // current schema no longer declares is parked under `_orphaned`
    // (never silently dropped) alongside any pre-existing parked bag —
    // re-adding the field later recovers it. Keys the schema still
    // declares were already rebuilt from the form above.
    if let Ok(Some(existing)) = model::data_for_page(pool, page_id).await {
        park_orphans(&mut data, &existing.data, &compiled.top_level_keys());
    }
    model::upsert_data(
        pool,
        page_id,
        compiled.version as i32,
        component_versions(compiled),
        Value::Object(data),
    )
    .await
}

/// Fold removed-field data into `data["_orphaned"]`. Every top-level key
/// in `existing` that the schema no longer declares (`known`) — plus any
/// pre-existing `_orphaned` bag — is parked so it survives the save. Keys
/// the schema still declares are left to the freshly-rebuilt `data`.
fn park_orphans(data: &mut Map<String, Value>, existing: &Value, known: &[String]) {
    let mut orphaned = existing
        .get("_orphaned")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let known: std::collections::HashSet<&str> = known.iter().map(String::as_str).collect();
    if let Some(stored) = existing.as_object() {
        for (k, v) in stored {
            if k == "_orphaned" || known.contains(k.as_str()) {
                continue;
            }
            orphaned.insert(k.clone(), v.clone());
        }
    }
    if !orphaned.is_empty() {
        data.insert("_orphaned".to_owned(), Value::Object(orphaned));
    }
}

fn collect_group(items: &[BodyItem], form: &HashMap<String, String>, out: &mut Map<String, Value>) {
    for item in items {
        match item {
            BodyItem::Field(w) => {
                out.insert(
                    strip_group(&w.name),
                    coerce(w.kind, form.get(&w.name).map_or("", String::as_str)),
                );
            }
            BodyItem::Row(cells) => {
                for (w, _) in cells {
                    out.insert(
                        strip_group(&w.name),
                        coerce(w.kind, form.get(&w.name).map_or("", String::as_str)),
                    );
                }
            }
            _ => {}
        }
    }
}

/// `pb__key` → `key`.
fn strip(name: &str) -> String {
    name.strip_prefix(FIELD_PREFIX).unwrap_or(name).to_owned()
}
/// `pb__group__field` → `field` (last segment).
fn strip_group(name: &str) -> String {
    name.rsplit("__").next().unwrap_or(name).to_owned()
}

/// Whether a lazy upgrade happened (new keys defaulted / removed keys
/// parked) so the editor can show a non-blocking notice.
#[derive(Debug, Default, Clone, Copy)]
pub struct UpgradeReport {
    pub upgraded: bool,
}

/// Load a page's builder values, lazily upgraded against the compiled
/// (published) schema. New top-level keys get their default; keys no
/// longer in the schema move to `data._orphaned` (never deleted). The
/// returned value is NOT persisted — the next save writes it back.
///
/// # Errors
/// Propagates the load query's DB failures.
pub async fn load_upgraded(
    pool: &rustango::sql::Pool,
    page_id: i64,
    compiled: &CompiledSchema,
) -> Result<(Value, UpgradeReport), rustango::sql::ExecError> {
    let stored = model::data_for_page(pool, page_id).await?;
    Ok(upgrade(stored.as_ref(), compiled))
}

/// The [`load_upgraded`] half that needs no query: fill `stored` (a
/// page's saved row, if any) up to `compiled`'s shape.
#[must_use]
pub fn upgrade(stored: Option<&model::PageBuilderData>, compiled: &CompiledSchema) -> (Value, UpgradeReport) {
    let mut data: Map<String, Value> = stored
        .as_ref()
        .and_then(|d| d.data.as_object().cloned())
        .unwrap_or_default();

    let known: Vec<String> = compiled.top_level_keys();
    let mut report = UpgradeReport::default();
    let stale = stored
        .as_ref()
        .map_or(false, |d| d.schema_version != compiled.version as i32);

    // Fill defaults for keys the schema declares but the page lacks.
    for item in &compiled.body {
        let (key, default) = match item {
            BodyItem::Field(w) => (strip(&w.name), Value::String(w.value.clone())),
            BodyItem::Row(_) => continue, // handled per-field below
            BodyItem::Group { key, .. } => (key.clone(), Value::Object(Map::new())),
            BodyItem::Zone { key, .. } => (key.clone(), Value::Array(Vec::new())),
        };
        if !data.contains_key(&key) {
            data.insert(key, default);
            if stored.is_some() {
                report.upgraded = true;
            }
        }
    }
    // Row fields default individually.
    for item in &compiled.body {
        if let BodyItem::Row(cells) = item {
            for (w, _) in cells {
                let key = strip(&w.name);
                if !data.contains_key(&key) {
                    data.insert(key, Value::String(w.value.clone()));
                    if stored.is_some() {
                        report.upgraded = true;
                    }
                }
            }
        }
    }

    // Park keys the schema no longer declares (only when the stored
    // version is stale, so a fresh save's extra `_orphaned` survives).
    if stale {
        let mut orphaned = data
            .get("_orphaned")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let remove: Vec<String> = data
            .keys()
            .filter(|k| *k != "_orphaned" && !known.contains(k))
            .cloned()
            .collect();
        for k in remove {
            if let Some(v) = data.remove(&k) {
                orphaned.insert(k, v);
                report.upgraded = true;
            }
        }
        if !orphaned.is_empty() {
            data.insert("_orphaned".to_owned(), Value::Object(orphaned));
        }
    }

    (Value::Object(data), report)
}

/// The builder values, choice labels and first photo of each child page
/// whose type has a published schema, in the visitor's language — what a
/// listing shows on its cards as `child.builder.*` /
/// `child.builder_labels.*`, and the card image when no share image is
/// chosen.
/// One schema load per page type and one data query for all children,
/// whatever their number. `translations_by_page` holds each child's
/// per-locale overrides (empty on the default language).
///
/// # Errors
/// Driver / query failures.
pub async fn children_builder(
    pool: &rustango::sql::Pool,
    children: &[crate::page::Page],
    translations_by_page: &HashMap<i64, HashMap<String, String>>,
    locale_code: Option<&str>,
) -> Result<HashMap<i64, (Value, Value, Option<i64>)>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let mut out = HashMap::new();
    if children.is_empty() {
        return Ok(out);
    }
    let mut schemas: HashMap<i64, Option<(CompiledSchema, crate::page_builder::Document)>> = HashMap::new();
    let mut components = None;
    for child in children {
        if schemas.contains_key(&child.page_type_id) {
            continue;
        }
        let entry = match model::published_for(pool, child.page_type_id).await? {
            Some(row) => match crate::page_builder::parse_schema(&row.document) {
                Ok(doc) => {
                    if components.is_none() {
                        components = Some(model::component_map(pool).await.unwrap_or_default());
                    }
                    let compiled = crate::page_builder::compile(
                        &doc,
                        components.as_ref().expect("loaded above"),
                        row.version.max(1) as u32,
                    );
                    Some((compiled, doc))
                }
                Err(_) => None,
            },
            None => None,
        };
        schemas.insert(child.page_type_id, entry);
    }
    let ids: Vec<i64> = children
        .iter()
        .filter(|c| schemas.get(&c.page_type_id).is_some_and(Option::is_some))
        .filter_map(|c| c.id.get().copied())
        .collect();
    if ids.is_empty() {
        return Ok(out);
    }
    let rows: HashMap<i64, model::PageBuilderData> = model::PageBuilderData::objects()
        .where_(model::PageBuilderData::page_id.is_in(ids))
        .fetch(pool)
        .await?
        .into_iter()
        .map(|r| (r.page_id, r))
        .collect();
    let none = HashMap::new();
    for child in children {
        let (Some(id), Some(Some((compiled, doc)))) = (child.id.get().copied(), schemas.get(&child.page_type_id)) else {
            continue;
        };
        let (mut values, _) = upgrade(rows.get(&id), compiled);
        if let Some(map) = values.as_object_mut() {
            apply_builder_translations(compiled, map, translations_by_page.get(&id).unwrap_or(&none));
        }
        let labels = choice_labels(doc, &values, locale_code);
        let photo = first_media_id(compiled, &values);
        out.insert(id, (values, labels, photo));
    }
    Ok(out)
}

/// Public-render context for a page whose type has a published schema.
/// Returns `(builder_values, zone_html)` where `builder_values`
/// is the whole value tree (scalars + nested groups + zone arrays) for
/// `ctx.builder.*`, and `zone_html` maps each flexible-content/repeater
/// zone key → pre-rendered HTML (through the stream pipeline with the
/// dyn-block overlay) for `{{ stream_render(name="<zone>") | safe }}`.
///
/// `preview_form` (when present) builds the values from the posted edit
/// form instead of the stored row, so the preview iframe reflects
/// on-screen edits before save — identical coercion to [`save_from_form`].
/// Returns `None` when the type has no published schema.
/// Compile the published schema and fill it — the value half of
/// [`public_render_ctx`], without the zone-HTML prerender.
///
/// JSON consumers want values only (they render themselves), and the
/// prerender is the expensive part. Factored out so the HTML renderer,
/// the v2 API and the `Accept`-negotiated JSON view all compile the
/// schema through one path instead of three near-copies.
///
/// Returns `(values, compiled)` so a caller that also needs the zones
/// doesn't compile twice. `None` when the type has no published schema.
pub async fn values_for(
    pool: &rustango::sql::Pool,
    page_id: i64,
    page_type_id: i64,
    preview_form: Option<&HashMap<String, String>>,
) -> Option<(Value, CompiledSchema)> {
    values_for_doc(pool, page_id, page_type_id, preview_form)
        .await
        .map(|(values, compiled, _)| (values, compiled))
}

/// [`values_for`], plus the published schema document it compiled.
async fn values_for_doc(
    pool: &rustango::sql::Pool,
    page_id: i64,
    page_type_id: i64,
    preview_form: Option<&HashMap<String, String>>,
) -> Option<(Value, CompiledSchema, crate::page_builder::Document)> {
    let row = model::published_for(pool, page_type_id).await.ok()??;
    let doc = crate::page_builder::parse_schema(&row.document).ok()?;
    let components = model::component_map(pool).await.unwrap_or_default();
    let compiled = crate::page_builder::compile(&doc, &components, row.version.max(1) as u32);
    let values = match preview_form {
        Some(form) => Value::Object(build_values(&compiled, form)),
        None => load_upgraded(pool, page_id, &compiled).await.ok()?.0,
    };
    Some((values, compiled, doc))
}

/// The display labels of a page's choice fields (select, radio, checkboxes)
/// in `locale` (a locale code; `None` = the default language), with the
/// same nesting as `builder.*`: `builder_labels.glaze` is "Bleu lune" where
/// `builder.glaze` is the stored value "Moon blue". A multi-choice
/// field gives a list. Fields with no value are left out.
#[must_use]
pub fn choice_labels(doc: &crate::page_builder::Document, values: &Value, locale: Option<&str>) -> Value {
    use crate::page_builder::schema::Node;
    fn walk(nodes: &[Node], values: Option<&Map<String, Value>>, locale: Option<&str>, out: &mut Map<String, Value>) {
        for node in nodes {
            match node {
                Node::Field(f) if !f.options.is_empty() => {
                    let label = |v: &str| {
                        f.options
                            .iter()
                            .find(|c| c.value == v)
                            .map_or_else(|| v.to_owned(), |c| c.label_in(locale).to_owned())
                    };
                    match values.and_then(|m| m.get(&f.key)) {
                        Some(Value::String(v)) if !v.is_empty() => {
                            out.insert(f.key.clone(), Value::String(label(v)));
                        }
                        Some(Value::Array(vs)) => {
                            let list = vs.iter().filter_map(Value::as_str).map(|v| Value::String(label(v))).collect();
                            out.insert(f.key.clone(), Value::Array(list));
                        }
                        _ => {}
                    }
                }
                Node::Row(r) => walk(&r.children, values, locale, out),
                Node::Group(g) => {
                    let mut inner = Map::new();
                    let group_values = values.and_then(|m| m.get(&g.key)).and_then(Value::as_object);
                    walk(&g.children, group_values, locale, &mut inner);
                    if !inner.is_empty() {
                        out.insert(g.key.clone(), Value::Object(inner));
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = Map::new();
    walk(&doc.nodes, values.as_object(), locale, &mut out);
    Value::Object(out)
}

/// `form_locale_id`, `enrich_page_id` and `tenant` exist only to feed the
/// enrichment pass, and mirror the arguments the extension-stream path
/// already passes to `prerender_extension_streams_async_with`.
/// `enrich_page_id` is the **page's** id, which is not always `page_id`:
/// that one addresses the builder value store, and an alias reads its
/// source row's values while still being its own page.
#[allow(clippy::too_many_arguments)]
pub async fn public_render_ctx(
    pool: &rustango::sql::Pool,
    page_id: i64,
    page_type_id: i64,
    preview_form: Option<&HashMap<String, String>>,
    translations: &HashMap<String, String>,
    tera: &tera::Tera,
    form_locale_id: Option<i64>,
    enrich_page_id: Option<i64>,
    tenant: &str,
) -> Option<(Value, HashMap<String, String>, String)> {
    public_render(
        pool,
        page_id,
        page_type_id,
        preview_form,
        translations,
        tera,
        form_locale_id,
        enrich_page_id,
        tenant,
        None,
    )
    .await
    .map(|r| (r.values, r.zone_html, r.body_html))
}

/// What [`public_render`] hands the page renderer.
#[derive(Debug, Clone)]
pub struct PublicRender {
    /// The filled values, exposed to templates as `builder.*`.
    pub values: Value,
    /// Each zone's rendered HTML, by zone key.
    pub zone_html: HashMap<String, String>,
    /// The generic body for templates that don't know the schema.
    pub body_html: String,
    /// The first photo a media-picker field holds — the page's share
    /// image when none is chosen on the Promote tab.
    pub first_media_id: Option<i64>,
    /// The choice fields' display labels, exposed as `builder_labels.*`
    /// (see [`choice_labels`]).
    pub labels: Value,
}

/// [`public_render_ctx`], plus the first photo among the fields.
#[allow(clippy::too_many_arguments)]
pub async fn public_render(
    pool: &rustango::sql::Pool,
    page_id: i64,
    page_type_id: i64,
    preview_form: Option<&HashMap<String, String>>,
    translations: &HashMap<String, String>,
    tera: &tera::Tera,
    form_locale_id: Option<i64>,
    enrich_page_id: Option<i64>,
    tenant: &str,
    locale_code: Option<&str>,
) -> Option<PublicRender> {
    let (mut values, compiled, doc) = values_for_doc(pool, page_id, page_type_id, preview_form).await?;
    let first_media_id = first_media_id(&compiled, &values);
    let labels = choice_labels(&doc, &values, locale_code);
    // #567 — overlay the per-locale builder text leaves (fixed / group /
    // zone) before rendering; empty overrides fall back to canonical.
    if let Some(map) = values.as_object_mut() {
        apply_builder_translations(&compiled, map, translations);
    }
    // Prerender each zone's stream with the dyn-block overlay so UI-defined
    // groups resolve as `Block`s exactly like code-registered blocks.
    let ctx = crate::block::BlockRenderCtx::new(tera).with_dyn_blocks(&compiled.dyn_blocks);
    let mut zone_html = HashMap::new();
    for item in &compiled.body {
        let BodyItem::Zone { key, .. } = item else {
            continue;
        };
        let Some(zv) = values.get(key) else {
            continue;
        };
        // Enrich before the sync render, exactly as the extension-stream
        // path does — `Block::render` cannot await, so a block that needs
        // a row, an image URL or a query result has to be handed it here
        // or it renders empty. Without this a chooser in a zone loses its
        // `_url`/`_title` and a host's report block never gets `_rows`:
        // UI-defined blocks could hold values but never carry data.
        //
        // On a clone, so `builder.*` keeps exactly what the author typed
        // and the injected `_` keys stay confined to the render.
        let mut zv = zv.clone();
        crate::block::tera_helpers::enrich_chooser_refs_async(&mut zv, pool, form_locale_id).await;
        crate::block::enrich::fire(
            &mut zv,
            crate::block::enrich::EnrichCtx {
                pool,
                tenant,
                page_id: enrich_page_id,
            },
        )
        .await;
        if let Ok(html) = crate::block::render::prerender_stream(&zv, &ctx) {
            zone_html.insert(key.clone(), html);
        }
    }
    // Generic public body — for UI-created types whose (host-overridable)
    // template doesn't know the schema's keys; code templates ignore it
    // and read `builder.*` / `stream_render` directly.
    let chooser_html = render_chooser_fields(&compiled.body, values.as_object(), pool, form_locale_id, &ctx).await;
    // #863 — the generic body shows choice labels in the visitor's language.
    let localized = locale_code.map(|_| {
        let mut c = compiled.clone();
        localize_choice_options(&mut c.body, &doc, locale_code);
        c
    });
    let body_html = render_public_body(localized.as_ref().unwrap_or(&compiled), values.as_object(), &zone_html, &chooser_html);
    Some(PublicRender {
        values,
        zone_html,
        body_html,
        first_media_id,
        labels,
    })
}

/// Replace the option labels of the compiled choice widgets with their
/// `locale` labels from the schema document. Widget names encode the key
/// path: `pb__glaze`, `pb__details__finish` (group `details`).
fn localize_choice_options(items: &mut [BodyItem], doc: &crate::page_builder::Document, locale: Option<&str>) {
    use crate::page_builder::schema::Node;
    fn collect(nodes: &[Node], prefix: &str, out: &mut HashMap<String, Vec<crate::forms::schema::Choice>>) {
        for node in nodes {
            match node {
                Node::Field(f) if !f.options.is_empty() => {
                    out.insert(format!("{FIELD_PREFIX}{prefix}{}", f.key), f.options.clone());
                }
                Node::Row(r) => collect(&r.children, prefix, out),
                Node::Group(g) => collect(&g.children, &format!("{prefix}{}__", g.key), out),
                _ => {}
            }
        }
    }
    fn apply(items: &mut [BodyItem], choices: &HashMap<String, Vec<crate::forms::schema::Choice>>, locale: Option<&str>) {
        let relabel = |w: &mut Widget| {
            if let Some(opts) = choices.get(&w.name) {
                for (value, label) in &mut w.options {
                    if let Some(c) = opts.iter().find(|c| &c.value == value) {
                        *label = c.label_in(locale).to_owned();
                    }
                }
            }
        };
        for item in items {
            match item {
                BodyItem::Field(w) => relabel(w),
                BodyItem::Row(cells) => cells.iter_mut().for_each(|(w, _)| relabel(w)),
                BodyItem::Group { items, .. } => apply(items, choices, locale),
                BodyItem::Zone { .. } => {}
            }
        }
    }
    let mut choices = HashMap::new();
    collect(&doc.nodes, "", &mut choices);
    apply(items, &choices, locale);
}

/// The id held by the first media-picker field of the body (top level,
/// rows and groups, in order), if any is filled.
#[must_use]
pub fn first_media_id(compiled: &CompiledSchema, values: &Value) -> Option<i64> {
    fn in_items(items: &[BodyItem], values: &Value, group: bool) -> Option<i64> {
        let key = |w: &Widget| if group { strip_group(&w.name) } else { strip(&w.name) };
        for item in items {
            let found = match item {
                BodyItem::Field(w) if w.kind == WidgetKind::MediaPicker => {
                    crate::meta_tags::media_id(values.get(key(w)))
                }
                BodyItem::Row(cells) => cells
                    .iter()
                    .filter(|(w, _)| w.kind == WidgetKind::MediaPicker)
                    .find_map(|(w, _)| crate::meta_tags::media_id(values.get(key(w)))),
                BodyItem::Group { key, items, .. } => values.get(key).and_then(|g| in_items(items, g, true)),
                _ => None,
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }
    in_items(&compiled.body, values, false)
}

/// The built-in block that renders a chooser widget's value, and the key
/// its id goes under.
fn chooser_block(kind: crate::widget::WidgetKind) -> Option<(&'static str, &'static str)> {
    use crate::widget::WidgetKind as K;
    match kind {
        K::MediaPicker => Some(("image", "media_id")),
        K::PageChooser => Some(("page_chooser", "page_id")),
        K::SnippetChooser => Some(("snippet_chooser", "snippet_id")),
        K::DocumentChooser => Some(("document_chooser", "document_id")),
        _ => None,
    }
}

/// Every fixed field (top level or in a group) with its value-path key:
/// `name`, or `group.name` inside a group.
fn fixed_fields<'a>(items: &'a [BodyItem], prefix: &str, out: &mut Vec<(String, &'a Widget)>) {
    for item in items {
        match item {
            BodyItem::Field(w) => out.push((field_path(prefix, w), w)),
            BodyItem::Row(cells) => out.extend(cells.iter().map(|(w, _)| (field_path(prefix, w), w))),
            BodyItem::Group { key, items, .. } => fixed_fields(items, key, out),
            BodyItem::Zone { .. } => {}
        }
    }
}

fn field_path(prefix: &str, w: &Widget) -> String {
    if prefix.is_empty() {
        strip(&w.name)
    } else {
        format!("{prefix}.{}", strip_group(&w.name))
    }
}

fn value_at<'v>(values: Option<&'v Map<String, Value>>, path: &str) -> Option<&'v Value> {
    let mut parts = path.split('.');
    let first = values?.get(parts.next()?)?;
    parts.try_fold(first, |v, key| v.get(key))
}

/// Chooser fields rendered through the built-in chooser blocks, so a photo
/// is an `<img>` and a page is a link exactly as the same block in a zone
/// would be. One enrichment pass looks up every referenced row.
async fn render_chooser_fields(
    body: &[BodyItem],
    values: Option<&Map<String, Value>>,
    pool: &rustango::sql::Pool,
    form_locale_id: Option<i64>,
    ctx: &crate::block::BlockRenderCtx<'_>,
) -> HashMap<String, String> {
    let mut fields = Vec::new();
    fixed_fields(body, "", &mut fields);
    let mut paths = Vec::new();
    let mut blocks = Vec::new();
    for (path, w) in fields {
        let (Some((block_type, id_key)), Some(id)) =
            (chooser_block(w.kind), value_at(values, &path).and_then(Value::as_i64))
        else {
            continue;
        };
        blocks.push(serde_json::json!({ "type": block_type, "id": path, "value": { id_key: id } }));
        paths.push(path);
    }
    let mut out = HashMap::new();
    if blocks.is_empty() {
        return out;
    }
    let mut stream = Value::Array(blocks);
    crate::block::tera_helpers::enrich_chooser_refs_async(&mut stream, pool, form_locale_id).await;
    if let Value::Array(blocks) = stream {
        for (path, block) in paths.into_iter().zip(blocks) {
            if let Ok(html) = crate::block::render::prerender_stream(&Value::Array(vec![block]), ctx) {
                out.insert(path, html);
            }
        }
    }
    out
}

/// A fixed field's value as public HTML, by widget kind: rich text
/// sanitized, markdown rendered, choices by label, links clickable (http,
/// https, mailto and tel only). `None` for an empty value or a kind that is
/// never shown (password, hidden) or rendered elsewhere (choosers).
pub(crate) fn public_value_html(w: &Widget, v: &Value) -> Option<String> {
    use crate::widget::WidgetKind as K;
    let esc = |s: &str| tera::escape_html(s);
    let label_of = |raw: &str| {
        w.options
            .iter()
            .find(|(value, _)| value == raw)
            .map_or_else(|| esc(raw), |(_, label)| esc(label))
    };
    let text = match v {
        Value::Null => return None,
        Value::String(s) if s.trim().is_empty() => return None,
        Value::Array(a) if a.is_empty() => return None,
        Value::String(s) => Some(s.as_str()),
        _ => None,
    };
    let html = match (w.kind, text) {
        (K::Password | K::Hidden, _) => return None,
        (kind, _) if chooser_block(kind).is_some() => return None,
        (K::Markdown, Some(s)) => crate::markdown::render(s),
        (K::RichText, Some(s)) => crate::markdown::sanitize_html(s),
        (K::Textarea, Some(s)) => esc(s).replace('\n', "<br>"),
        (K::Url, Some(s)) if s.starts_with("https://") || s.starts_with("http://") => {
            format!(r#"<a href="{0}">{0}</a>"#, esc(s))
        }
        (K::Email, Some(s)) if s.contains('@') && !s.contains(char::is_whitespace) => {
            format!(r#"<a href="mailto:{0}">{0}</a>"#, esc(s))
        }
        (K::Tel, Some(s)) => format!(r#"<a href="tel:{}">{}</a>"#, esc(&s.replace(' ', "")), esc(s)),
        (K::Select | K::Radio, Some(s)) => label_of(s),
        (K::Checkboxes | K::MultiSelect, _) => {
            let items: Vec<String> = match v {
                Value::Array(a) => a
                    .iter()
                    .map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_owned))
                    .collect(),
                _ => return None,
            };
            items.iter().map(|i| label_of(i)).collect::<Vec<_>>().join(", ")
        }
        (K::Boolean, _) => (if v.as_bool().unwrap_or(false) { "Yes" } else { "No" }).to_owned(),
        (_, Some(s)) => esc(s),
        (_, None) => match v {
            Value::Array(_) | Value::Object(_) => return None,
            // Numbers are stored as floats; 24.0 reads as 24.
            Value::Number(n) if n.as_f64().is_some_and(|f| f.fract() == 0.0 && f.abs() < 1e15) => {
                format!("{}", n.as_f64().unwrap_or_default() as i64)
            }
            other => esc(&other.to_string()),
        },
    };
    Some(html)
}

/// Kinds whose value reads as a short fact ("Price: 24"), shown with its
/// label; the rest (long text, images) stand on their own.
fn labelled(kind: crate::widget::WidgetKind) -> bool {
    use crate::widget::WidgetKind as K;
    !matches!(
        kind,
        K::Markdown | K::RichText | K::Textarea | K::MediaPicker | K::SnippetChooser | K::PageChooser | K::DocumentChooser
    )
}

/// The `cms_translation.field_path` prefix for page-builder leaves.
pub const BUILDER_PATH_PREFIX: &str = "builder";

/// Enumerate translatable text leaves in a page's builder values:
/// fixed text fields + group text members (`builder.<key>` /
/// `builder.<group>.<field>`) and stream text leaves inside repeater/flex
/// zones (`builder.<zone>.<uuid>.<name>`, via the block walker with the
/// dyn-block overlay). Only `is_translatable_text` widget kinds qualify.
/// Reuses [`crate::block::translate::TranslatableLeaf`] so the admin
/// translation surface lists builder leaves alongside stream leaves.
#[must_use]
pub fn builder_translatable_leaves(
    compiled: &CompiledSchema,
    values: &Value,
) -> Vec<crate::block::translate::TranslatableLeaf> {
    let obj = values.as_object();
    let mut out = Vec::new();
    for item in &compiled.body {
        match item {
            BodyItem::Field(w) => push_fixed_leaf(&mut out, &strip(&w.name), &w.label, w, obj),
            BodyItem::Row(cells) => {
                for (w, _) in cells {
                    push_fixed_leaf(&mut out, &strip(&w.name), &w.label, w, obj);
                }
            }
            BodyItem::Group { key, items, .. } => {
                for it in items {
                    match it {
                        BodyItem::Field(w) => {
                            push_fixed_leaf(
                                &mut out,
                                &format!("{key}.{}", strip_group(&w.name)),
                                &w.label,
                                w,
                                obj,
                            );
                        }
                        BodyItem::Row(cells) => {
                            for (w, _) in cells {
                                push_fixed_leaf(
                                    &mut out,
                                    &format!("{key}.{}", strip_group(&w.name)),
                                    &w.label,
                                    w,
                                    obj,
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            BodyItem::Zone { key, .. } => {
                let arr = obj
                    .and_then(|o| o.get(key))
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                out.extend(crate::block::translate::collect_translatable_leaves_with(
                    &format!("{BUILDER_PATH_PREFIX}.{key}"),
                    &arr,
                    Some(&compiled.dyn_blocks),
                ));
            }
        }
    }
    out
}

/// Read a (possibly dotted `group.field`) key out of the top-level value
/// object, descending one group level.
fn leaf_at<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    match key.split_once('.') {
        Some((g, f)) => obj.get(g).and_then(Value::as_object).and_then(|m| m.get(f)),
        None => obj.get(key),
    }
}

/// Push a translatable fixed-field / group-member leaf into `out`. `key`
/// is the value-store key (`<field>` or `<group>.<field>`); the emitted
/// path is `builder.<key>`. No-op for non-text widgets.
fn push_fixed_leaf(
    out: &mut Vec<crate::block::translate::TranslatableLeaf>,
    key: &str,
    label: &str,
    w: &Widget,
    obj: Option<&Map<String, Value>>,
) {
    if !w.kind.is_translatable_text() {
        return;
    }
    out.push(crate::block::translate::TranslatableLeaf {
        path: format!("{BUILDER_PATH_PREFIX}.{key}"),
        widget_kind: w.kind,
        canonical_text: obj
            .and_then(|o| leaf_at(o, key))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        block_type: "builder".to_owned(),
        block_label: "Body".to_owned(),
        field_label: label.to_owned(),
        block_id: "builder".to_owned(),
        depth: 0,
    });
}

/// Apply per-locale overrides to a page's builder values in place.
/// Fixed/group text leaves keyed `builder.<path>` are replaced when the
/// override is non-empty; zone streams delegate to the block overlay with
/// the dyn-block set. Missing/empty overrides leave the canonical value.
pub fn apply_builder_translations(
    compiled: &CompiledSchema,
    values: &mut Map<String, Value>,
    tr: &HashMap<String, String>,
) {
    if tr.is_empty() {
        return;
    }
    let ov = |path: &str| tr.get(path).map(|s| s.trim()).filter(|s| !s.is_empty());
    for item in &compiled.body {
        match item {
            BodyItem::Field(w) => {
                let k = strip(&w.name);
                if w.kind.is_translatable_text() {
                    if let Some(v) = ov(&format!("{BUILDER_PATH_PREFIX}.{k}")) {
                        values.insert(k, Value::String(v.to_owned()));
                    }
                }
            }
            BodyItem::Row(cells) => {
                for (w, _) in cells {
                    let k = strip(&w.name);
                    if w.kind.is_translatable_text() {
                        if let Some(v) = ov(&format!("{BUILDER_PATH_PREFIX}.{k}")) {
                            values.insert(k, Value::String(v.to_owned()));
                        }
                    }
                }
            }
            BodyItem::Group { key, items, .. } => {
                let Some(gobj) = values.get_mut(key).and_then(Value::as_object_mut) else {
                    continue;
                };
                for it in items {
                    let widgets: Vec<&Widget> = match it {
                        BodyItem::Field(w) => vec![w],
                        BodyItem::Row(cells) => cells.iter().map(|(w, _)| w).collect(),
                        _ => Vec::new(),
                    };
                    for w in widgets {
                        let f = strip_group(&w.name);
                        if w.kind.is_translatable_text() {
                            if let Some(v) = ov(&format!("{BUILDER_PATH_PREFIX}.{key}.{f}")) {
                                gobj.insert(f, Value::String(v.to_owned()));
                            }
                        }
                    }
                }
            }
            BodyItem::Zone { key, .. } => {
                if let Some(zv) = values.get(key) {
                    let localized = crate::block::translate::apply_translatable_overrides_with(
                        &format!("{BUILDER_PATH_PREFIX}.{key}"),
                        zv,
                        tr,
                        Some(&compiled.dyn_blocks),
                    );
                    values.insert(key.clone(), localized);
                }
            }
        }
    }
}

/// Render a compiled schema's filled values as generic, semantically
/// classed public HTML (`pb-*`) — the fallback body for UI-created types.
/// Zones use the already-rendered `zone_html`, chooser fields
/// `chooser_html`; other fields are rendered by widget kind. Hosts can
/// style it or replace it with a per-type template.
#[must_use]
fn render_public_body(
    compiled: &CompiledSchema,
    values: Option<&Map<String, Value>>,
    zone_html: &HashMap<String, String>,
    chooser_html: &HashMap<String, String>,
) -> String {
    let mut out = String::new();
    out.push_str(r#"<div class="pb-body">"#);
    public_items(&compiled.body, "", values, zone_html, chooser_html, &mut out);
    out.push_str("</div>");
    out
}

fn public_items(
    items: &[BodyItem],
    prefix: &str,
    values: Option<&Map<String, Value>>,
    zone_html: &HashMap<String, String>,
    chooser_html: &HashMap<String, String>,
    out: &mut String,
) {
    for item in items {
        match item {
            BodyItem::Field(w) => out.push_str(&public_field(w, prefix, values, chooser_html)),
            BodyItem::Row(cells) => {
                out.push_str(r#"<div class="pb-row">"#);
                for (w, _) in cells {
                    out.push_str(&public_field(w, prefix, values, chooser_html));
                }
                out.push_str("</div>");
            }
            BodyItem::Group { key, label, items } => {
                out.push_str(&format!(r#"<section class="pb-group pb-group--{}">"#, tera::escape_html(key)));
                if !label.is_empty() {
                    out.push_str(&format!("<h3>{}</h3>", tera::escape_html(label)));
                }
                public_items(items, key, values, zone_html, chooser_html, out);
                out.push_str("</section>");
            }
            BodyItem::Zone { key, .. } => {
                out.push_str(&format!(
                    r#"<div class="pb-zone pb-zone--{}">{}</div>"#,
                    tera::escape_html(key),
                    zone_html.get(key).map_or("", String::as_str),
                ));
            }
        }
    }
}

fn public_field(
    w: &Widget,
    prefix: &str,
    values: Option<&Map<String, Value>>,
    chooser_html: &HashMap<String, String>,
) -> String {
    let path = field_path(prefix, w);
    let inner = match chooser_html.get(&path) {
        Some(html) => Some(html.clone()),
        None => value_at(values, &path).and_then(|v| public_value_html(w, v)),
    };
    let Some(inner) = inner else {
        return String::new();
    };
    let key = tera::escape_html(path.rsplit('.').next().unwrap_or(&path));
    if labelled(w.kind) && !w.label.is_empty() {
        format!(
            r#"<div class="pb-field pb-field--{key}"><span class="pb-label">{}</span> <span class="pb-value">{inner}</span></div>"#,
            tera::escape_html(&w.label)
        )
    } else {
        format!(r#"<div class="pb-field pb-field--{key}">{inner}</div>"#)
    }
}

/// Pre-render the editor HTML for a compiled schema, prefilled from
/// `values`. Fixed fields/rows/groups render through the shared
/// `_widget.html` macro; repeater/flex zones render through the stream
/// editor with the dyn-block overlay so `stream_editor.js` drives them
/// unchanged.
#[must_use]
pub fn render_body(
    compiled: &CompiledSchema,
    values: &Value,
    tera: &tera::Tera,
) -> String {
    let obj = values.as_object();
    let mut out = String::new();
    for item in &compiled.body {
        match item {
            BodyItem::Field(w) => out.push_str(&render_field(w, obj, tera)),
            BodyItem::Row(cells) => {
                out.push_str(r#"<div class="rcms-field-row rcms-flex rcms-gap-3 rcms-flex-wrap">"#);
                for (w, width) in cells {
                    out.push_str(&format!(
                        r#"<div style="flex:{width} 1 0;min-width:140px;">"#
                    ));
                    out.push_str(&render_field(w, obj, tera));
                    out.push_str("</div>");
                }
                out.push_str("</div>");
            }
            BodyItem::Group { key, label, items } => {
                let group_obj = obj.and_then(|o| o.get(key)).and_then(Value::as_object);
                let legend = if label.is_empty() { key } else { label };
                out.push_str(&format!(
                    r#"<fieldset class="rcms-panel-group"><legend>{}</legend>"#,
                    tera::escape_html(legend)
                ));
                render_group_items(items, group_obj, tera, &mut out);
                out.push_str("</fieldset>");
            }
            BodyItem::Zone {
                key, label, widget, ..
            } => {
                let mut zw = widget.clone();
                zw.value = obj
                    .and_then(|o| o.get(key))
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .unwrap_or_else(|| "[]".to_owned());
                // The stream-list + per-type clone templates sit on
                // `custom_html`; the `_widget.html` stream arm wraps them
                // with the `data-stream-root` hidden input (so the zone
                // round-trips) + the picker dialog (so editors can add
                // entries). `render_stream_editor_with` alone emits neither.
                zw.custom_html = crate::block::admin::render_stream_editor_with(
                    &zw,
                    tera,
                    Some(&compiled.dyn_blocks),
                );
                let picker = zone_picker_options(&zw.allowed, &compiled.dyn_blocks);
                out.push_str(&format!(
                    r#"<fieldset class="rcms-panel-group"><legend>{}</legend>"#,
                    tera::escape_html(label)
                ));
                out.push_str(&crate::block::admin::render_widget_via_macro_with(
                    &zw, tera, &picker,
                ));
                out.push_str("</fieldset>");
            }
        }
    }
    out
}

fn render_field(w: &Widget, values: Option<&Map<String, Value>>, tera: &tera::Tera) -> String {
    let mut w = w.clone();
    let key = strip(&w.name);
    if let Some(v) = values.and_then(|o| o.get(&key)) {
        w.value = value_to_input(v);
    }
    crate::block::admin::render_widget_via_macro(&w, tera)
}

/// Build the block-picker options for a flex zone / repeater from its
/// allowed dyn-block type names. Editors pick these to add entries; the
/// `_widget.html` stream arm renders them into its `<dialog>`.
fn zone_picker_options(
    allowed: &[String],
    dyn_blocks: &crate::page_builder::DynBlockSet,
) -> Vec<crate::block::admin::PickerOption> {
    allowed
        .iter()
        .filter_map(|name| {
            let block = crate::page_builder::resolve_block(Some(dyn_blocks), name)?;
            Some(crate::block::admin::PickerOption {
                type_name: block.type_name().to_owned(),
                verbose_name: block.verbose_name().to_owned(),
                icon: block.icon().map(str::to_owned),
                group: None,
                description: None,
                default_value: None,
                collapsed: false,
                preview_html: String::new(),
            })
        })
        .collect()
}

fn render_group_items(
    items: &[BodyItem],
    group_obj: Option<&Map<String, Value>>,
    tera: &tera::Tera,
    out: &mut String,
) {
    for item in items {
        match item {
            BodyItem::Field(w) => {
                let mut w = w.clone();
                let key = strip_group(&w.name);
                if let Some(v) = group_obj.and_then(|o| o.get(&key)) {
                    w.value = value_to_input(v);
                }
                out.push_str(&crate::block::admin::render_widget_via_macro(&w, tera));
            }
            BodyItem::Row(cells) => {
                out.push_str(r#"<div class="rcms-field-row rcms-flex rcms-gap-3 rcms-flex-wrap">"#);
                for (w, width) in cells {
                    let mut w = w.clone();
                    let key = strip_group(&w.name);
                    if let Some(v) = group_obj.and_then(|o| o.get(&key)) {
                        w.value = value_to_input(v);
                    }
                    out.push_str(&format!(
                        r#"<div style="flex:{width} 1 0;min-width:140px;">"#
                    ));
                    out.push_str(&crate::block::admin::render_widget_via_macro(&w, tera));
                    out.push_str("</div>");
                }
                out.push_str("</div>");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn whole_numbers_prefill_without_a_decimal() {
        assert_eq!(super::value_to_input(&serde_json::json!(32.0)), "32");
        assert_eq!(super::value_to_input(&serde_json::json!(12.5)), "12.5");
        assert_eq!(super::value_to_input(&serde_json::json!("x")), "x");
    }

    use super::*;
    use crate::page_builder::compile;
    use crate::page_builder::schema::{parse, ComponentEntry};
    use std::collections::HashMap;

    fn compiled() -> CompiledSchema {
        let doc = parse(&serde_json::json!({
            "nodes": [
                { "kind": "field", "key": "subtitle", "widget": "text" },
                { "kind": "row", "children": [
                    { "kind": "field", "key": "a", "widget": "integer" },
                    { "kind": "field", "key": "flag", "widget": "boolean" }
                ]},
                { "kind": "group", "key": "hero", "label": "Hero", "children": [
                    { "kind": "field", "key": "heading", "widget": "text" }
                ]},
                { "kind": "repeater", "key": "faqs", "label": "FAQs",
                  "item": { "key": "faq", "children": [ { "kind": "field", "key": "q", "widget": "text" } ] } }
            ]
        }))
        .unwrap();
        compile::compile(&doc, &HashMap::<String, ComponentEntry>::new(), 3)
    }

    #[test]
    fn save_walks_body_into_typed_json() {
        let c = compiled();
        let mut form = HashMap::new();
        form.insert("pb__subtitle".to_owned(), "Hello".to_owned());
        form.insert("pb__a".to_owned(), "42".to_owned());
        form.insert("pb__flag".to_owned(), "on".to_owned());
        form.insert("pb__hero__heading".to_owned(), "Hi".to_owned());
        form.insert(
            "pb__faqs".to_owned(),
            r#"[{"type":"r_faqs","id":"x","value":{"q":"?"}}]"#.to_owned(),
        );

        // The shared pure transform used by both save + unsaved preview.
        let data = build_values(&c, &form);
        assert_eq!(data["subtitle"], serde_json::json!("Hello"));
        assert_eq!(data["a"], serde_json::json!(42));
        assert_eq!(data["flag"], serde_json::json!(true));
        assert_eq!(data["hero"], serde_json::json!({"heading": "Hi"}));
        assert!(data["faqs"].is_array());
    }

    #[test]
    fn builder_leaves_and_overlay_roundtrip() {
        let c = compiled();
        let mut form = HashMap::new();
        form.insert("pb__subtitle".to_owned(), "Hello".to_owned());
        form.insert("pb__a".to_owned(), "42".to_owned());
        form.insert("pb__hero__heading".to_owned(), "Hi".to_owned());
        form.insert(
            "pb__faqs".to_owned(),
            r#"[{"type":"r_faqs","id":"x1","value":{"q":"Question?"}}]"#.to_owned(),
        );
        let values = Value::Object(build_values(&c, &form));

        // Extraction: text leaves only (subtitle, hero.heading, faq q);
        // the integer `a` and boolean `flag` are excluded.
        let leaves = builder_translatable_leaves(&c, &values);
        let paths: Vec<&str> = leaves.iter().map(|l| l.path.as_str()).collect();
        assert!(paths.contains(&"builder.subtitle"), "{paths:?}");
        assert!(paths.contains(&"builder.hero.heading"), "{paths:?}");
        assert!(paths.contains(&"builder.faqs.x1.q"), "{paths:?}");
        assert!(!paths.iter().any(|p| p.ends_with(".a")), "{paths:?}");

        // Overlay: non-empty overrides win; untranslated leaves keep canonical.
        let mut tr = HashMap::new();
        tr.insert("builder.subtitle".to_owned(), "Bonjour".to_owned());
        tr.insert("builder.hero.heading".to_owned(), "Salut".to_owned());
        tr.insert("builder.faqs.x1.q".to_owned(), "Question ?".to_owned());
        let mut map = values.as_object().unwrap().clone();
        apply_builder_translations(&c, &mut map, &tr);
        assert_eq!(map["subtitle"], serde_json::json!("Bonjour"));
        assert_eq!(map["hero"]["heading"], serde_json::json!("Salut"));
        assert_eq!(
            map["faqs"][0]["value"]["q"],
            serde_json::json!("Question ?")
        );
        // `a` (untranslated, non-text) stays canonical.
        assert_eq!(map["a"], serde_json::json!(42));
    }

    #[test]
    fn coerce_kinds() {
        assert_eq!(coerce(WidgetKind::Integer, "7"), serde_json::json!(7));
        assert_eq!(coerce(WidgetKind::Boolean, "on"), serde_json::json!(true));
        assert_eq!(coerce(WidgetKind::Boolean, ""), serde_json::json!(false));
        assert_eq!(coerce(WidgetKind::MediaPicker, ""), Value::Null);
        assert_eq!(coerce(WidgetKind::MediaPicker, "9"), serde_json::json!(9));
        assert_eq!(coerce(WidgetKind::Text, "x"), serde_json::json!("x"));
    }

    #[test]
    fn park_orphans_moves_dropped_keys_and_keeps_prior_bag() {
        // Freshly-rebuilt data for the current schema (declares subtitle + kept).
        let mut data = serde_json::Map::new();
        data.insert("subtitle".to_owned(), serde_json::json!("new"));
        data.insert("kept".to_owned(), serde_json::json!("v"));
        // Stored data still carries a since-removed `hero_caption`, plus an
        // already-parked bag from an earlier upgrade.
        let existing = serde_json::json!({
            "subtitle": "old",
            "kept": "v",
            "hero_caption": "Cover photo",
            "_orphaned": { "ancient": "kept-forever" }
        });
        let known = vec!["subtitle".to_owned(), "kept".to_owned()];
        park_orphans(&mut data, &existing, &known);

        let orphaned = data["_orphaned"].as_object().unwrap();
        // Dropped key parked; prior bag preserved.
        assert_eq!(orphaned["hero_caption"], serde_json::json!("Cover photo"));
        assert_eq!(orphaned["ancient"], serde_json::json!("kept-forever"));
        // Declared keys are NOT parked and keep the fresh values.
        assert!(!orphaned.contains_key("subtitle"));
        assert!(!orphaned.contains_key("kept"));
        assert_eq!(data["subtitle"], serde_json::json!("new"));
    }

    #[test]
    fn park_orphans_noop_when_nothing_dropped() {
        let mut data = serde_json::Map::new();
        data.insert("subtitle".to_owned(), serde_json::json!("x"));
        let existing = serde_json::json!({ "subtitle": "x" });
        park_orphans(&mut data, &existing, &["subtitle".to_owned()]);
        assert!(!data.contains_key("_orphaned"));
    }

    #[test]
    fn a_partial_edit_that_omits_the_builder_keeps_the_body() {
        // `save_from_form` rebuilds the whole body from the form, so a
        // caller that hand-builds one (the MCP `update_page`) and does not
        // prefill writes every zone back empty. That turned "set an SEO
        // title" into "delete the page's content".
        let c = compiled();
        let stored = serde_json::json!({
            "subtitle": "Hello",
            "a": 42,
            "flag": true,
            "hero": { "heading": "Hi" },
            "faqs": [{ "type": "r_faqs", "id": "x", "value": { "q": "?" } }]
        });

        // Round-trip: stored values -> form -> stored values.
        let mut form = HashMap::new();
        prefill_form(&c, &stored, &mut form);
        let rebuilt = Value::Object(build_values(&c, &form));

        assert_eq!(rebuilt["subtitle"], serde_json::json!("Hello"));
        assert_eq!(rebuilt["a"], serde_json::json!(42));
        assert_eq!(rebuilt["flag"], serde_json::json!(true));
        assert_eq!(rebuilt["hero"]["heading"], serde_json::json!("Hi"));
        assert_eq!(
            rebuilt["faqs"].as_array().map(Vec::len),
            Some(1),
            "the repeater zone was emptied by a partial edit: {rebuilt}"
        );
    }
}

#[cfg(test)]
mod public_value_tests {
    use super::public_value_html;
    use crate::widget::{Widget, WidgetKind as K};
    use serde_json::json;

    fn html(kind: K, v: serde_json::Value) -> Option<String> {
        let w = Widget::new(kind, "f", "F").with_options([("blue", "Blue glaze"), ("white", "White")]);
        public_value_html(&w, &v)
    }

    /// Values render by kind instead of as escaped raw text.
    #[test]
    fn values_render_by_widget_kind() {
        assert_eq!(html(K::RichText, json!("<p>Hi<script>x</script></p>")).unwrap(), "<p>Hi</p>");
        assert!(html(K::Markdown, json!("**Hand-made**")).unwrap().contains("<strong>Hand-made</strong>"));
        assert_eq!(html(K::Select, json!("blue")).unwrap(), "Blue glaze");
        assert_eq!(html(K::Checkboxes, json!(["blue", "white"])).unwrap(), "Blue glaze, White");
        assert_eq!(html(K::Boolean, json!(true)).unwrap(), "Yes");
        assert_eq!(html(K::Number, json!(24.5)).unwrap(), "24.5");
        assert_eq!(html(K::Number, json!(24.0)).unwrap(), "24");
        assert_eq!(html(K::Textarea, json!("a\nb<")).unwrap(), "a<br>b&lt;");
        // Escaped like any template output (`/` as `&#x2F;`), still a link.
        let link = html(K::Url, json!("https://x.test/a")).unwrap();
        assert!(link.starts_with(r#"<a href="https:"#) && link.contains("x.test") && link.ends_with("</a>"), "{link}");
    }

    #[test]
    fn unsafe_links_and_hidden_kinds_do_not_render_as_links() {
        assert_eq!(html(K::Url, json!("javascript:alert(1)")).unwrap(), "javascript:alert(1)");
        assert!(html(K::Password, json!("secret")).is_none());
        assert!(html(K::Hidden, json!("x")).is_none());
        assert!(html(K::MediaPicker, json!(7)).is_none(), "choosers render through their block");
        assert!(html(K::Text, json!("")).is_none());
        assert!(html(K::Text, json!(null)).is_none());
    }
}
