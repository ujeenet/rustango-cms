//! Recursive per-leaf translation walker for StreamField content.
//!
//! Wagtail-style StreamField bodies are `[{type, id, value}]` arrays
//! where `value` is a field-name dict (or a bare scalar for a
//! single-field block). Blocks can nest other streams (`Stream` /
//! `Repeat` fields), to any depth. This module walks that tree two ways,
//! both schema-driven via the block registry (so only declared
//! translatable-text leaves are ever touched):
//!
//! * [`collect_translatable_leaves`] — enumerate every translatable text
//!   leaf with a stable dotted path, for the admin translation editor.
//! * [`apply_translatable_overrides`] — produce a localized clone of the
//!   stream with each leaf replaced by its per-locale override.
//!
//! ## Path scheme
//! Leaves are keyed by **block UUID** (client-minted, never re-minted),
//! so paths survive block reordering:
//!
//! * top-level leaf:        `<streamfield>.<uuid>.<fieldname>`
//! * nested stream/repeat:  `<streamfield>.<uuid>.<subfield>.<subuuid>.<fieldname>`
//!
//! Only [`WidgetKind::is_translatable_text`] leaves get paths; structural
//! fields (choosers, numbers, choices, urls) and `Computed` fields are
//! skipped. Unregistered (drift) blocks are skipped entirely — never
//! translated, and passed through unchanged on apply.

use std::collections::HashMap;

use serde_json::Value;

use super::BlockField;
use crate::widget::WidgetKind;

/// One translatable text leaf discovered in a StreamField tree.
#[derive(Debug, Clone)]
pub struct TranslatableLeaf {
    /// Full dotted path (incl. the streamfield prefix) — the
    /// `cms_translation.field_path` key for this leaf.
    pub path: String,
    /// The leaf's widget kind (`Text` / `Textarea` / `Markdown` /
    /// `RichText`) — drives the editor input type.
    pub widget_kind: WidgetKind,
    /// Canonical (default-locale) text; empty when absent/non-string.
    pub canonical_text: String,
    /// Wire type of the owning block (e.g. `"callout"`).
    pub block_type: String,
    /// Human label of the owning block (`verbose_name`), for UI grouping.
    pub block_label: String,
    /// The `BlockField` label, for the editor row.
    pub field_label: String,
    /// Block instance UUID that owns this leaf (for grouping rows).
    pub block_id: String,
    /// Nesting depth: 0 for a leaf in a top-level block, +1 per nested
    /// stream/repeat container — used to indent the editor UI.
    pub depth: usize,
}

/// Collect every translatable text leaf in `canonical_stream` (an
/// already-parsed block array), in document order. `stream_field_name`
/// is the owning StreamField widget name (the path prefix).
#[must_use]
pub fn collect_translatable_leaves(
    stream_field_name: &str,
    canonical_stream: &Value,
) -> Vec<TranslatableLeaf> {
    collect_translatable_leaves_with(stream_field_name, canonical_stream, None)
}

/// [`collect_translatable_leaves`] with a page-builder dyn-block overlay
/// (#567): UI-defined group/repeater/component blocks resolve through
/// `dyn_set` before the inventory registry, so their text leaves are
/// enumerated exactly like code-registered blocks.
#[must_use]
pub fn collect_translatable_leaves_with(
    stream_field_name: &str,
    canonical_stream: &Value,
    dyn_set: Option<&crate::page_builder::DynBlockSet>,
) -> Vec<TranslatableLeaf> {
    let mut out = Vec::new();
    if let Value::Array(arr) = canonical_stream {
        collect_blocks(arr, stream_field_name, 0, dyn_set, &mut out);
    }
    out
}

fn collect_blocks(
    arr: &[Value],
    prefix: &str,
    depth: usize,
    dyn_set: Option<&crate::page_builder::DynBlockSet>,
    out: &mut Vec<TranslatableLeaf>,
) {
    for env in arr {
        let (Some(btype), Some(bid)) = (
            env.get("type").and_then(Value::as_str),
            env.get("id").and_then(Value::as_str),
        ) else {
            continue; // missing type/id — no stable key
        };
        let Some(block) = crate::page_builder::resolve_block(dyn_set, btype) else {
            continue; // drift / unregistered — skip (renderer shows a banner)
        };
        let fields = block.fields();
        let value_obj = normalize_value(env.get("value"), &fields);
        let block_prefix = format!("{prefix}.{bid}");
        for field in &fields {
            match field {
                BlockField::Widget {
                    name,
                    label,
                    widget,
                    ..
                } if widget.is_translatable_text() => {
                    out.push(TranslatableLeaf {
                        path: format!("{block_prefix}.{name}"),
                        widget_kind: widget.clone(),
                        canonical_text: value_obj
                            .get(name)
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        block_type: btype.to_owned(),
                        block_label: block.verbose_name().to_owned(),
                        field_label: label.clone(),
                        block_id: bid.to_owned(),
                        depth,
                    });
                }
                BlockField::Stream { name, .. } | BlockField::Repeat { name, .. } => {
                    if let Some(Value::Array(sub)) = value_obj.get(name) {
                        collect_blocks(
                            sub,
                            &format!("{block_prefix}.{name}"),
                            depth + 1,
                            dyn_set,
                            out,
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

/// Return a localized clone of `canonical_stream`: every translatable
/// text leaf whose dotted path has a non-empty entry in `translations`
/// is replaced with that override; missing/empty overrides leave the
/// canonical text intact (per-leaf fallback). Structural fields, block
/// types, ids, and ordering are untouched. Unregistered blocks pass
/// through unchanged.
#[must_use]
pub fn apply_translatable_overrides(
    stream_field_name: &str,
    canonical_stream: &Value,
    translations: &HashMap<String, String>,
) -> Value {
    apply_translatable_overrides_with(stream_field_name, canonical_stream, translations, None)
}

/// [`apply_translatable_overrides`] with a page-builder dyn-block overlay
/// (#567) so UI-defined blocks localize like code-registered ones.
#[must_use]
pub fn apply_translatable_overrides_with(
    stream_field_name: &str,
    canonical_stream: &Value,
    translations: &HashMap<String, String>,
    dyn_set: Option<&crate::page_builder::DynBlockSet>,
) -> Value {
    let mut cloned = canonical_stream.clone();
    if let Value::Array(arr) = &mut cloned {
        apply_blocks(arr, stream_field_name, translations, dyn_set);
    }
    cloned
}

fn apply_blocks(
    arr: &mut [Value],
    prefix: &str,
    translations: &HashMap<String, String>,
    dyn_set: Option<&crate::page_builder::DynBlockSet>,
) {
    for env in arr.iter_mut() {
        let btype = env.get("type").and_then(Value::as_str).map(str::to_owned);
        let bid = env.get("id").and_then(Value::as_str).map(str::to_owned);
        let (Some(btype), Some(bid)) = (btype, bid) else {
            continue;
        };
        let Some(block) = crate::page_builder::resolve_block(dyn_set, &btype) else {
            continue; // drift — pass through unchanged
        };
        let fields = block.fields();
        // Normalize a bare-scalar `value` to the object form the
        // renderer expects, so overrides always land as `{field: text}`.
        ensure_object_value(env, &fields);
        let block_prefix = format!("{prefix}.{bid}");
        let Some(value_map) = env.get_mut("value").and_then(Value::as_object_mut) else {
            continue;
        };
        for field in &fields {
            match field {
                BlockField::Widget { name, widget, .. } if widget.is_translatable_text() => {
                    if let Some(ov) = translations
                        .get(&format!("{block_prefix}.{name}"))
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                    {
                        value_map.insert(name.clone(), Value::String(ov.to_owned()));
                    }
                }
                BlockField::Stream { name, .. } | BlockField::Repeat { name, .. } => {
                    if let Some(Value::Array(sub)) = value_map.get_mut(name) {
                        apply_blocks(
                            sub,
                            &format!("{block_prefix}.{name}"),
                            translations,
                            dyn_set,
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

/// Normalize a block's `value` to an object dict. Mirrors the renderer's
/// bare-scalar handling (`block::render::default_render`): a single-field
/// block may store a bare scalar, which becomes `{first_field: scalar}`.
fn normalize_value(value: Option<&Value>, fields: &[BlockField]) -> serde_json::Map<String, Value> {
    match value {
        Some(Value::Object(m)) => m.clone(),
        Some(Value::Null) | None => serde_json::Map::new(),
        Some(scalar) => {
            let mut m = serde_json::Map::new();
            if let Some(first) = fields.first() {
                m.insert(first.name().to_owned(), scalar.clone());
            }
            m
        }
    }
}

/// Ensure `env["value"]` is an object in place, normalizing a bare
/// scalar to `{first_field: scalar}` (and absent/null to `{}`).
fn ensure_object_value(env: &mut Value, fields: &[BlockField]) {
    if matches!(env.get("value"), Some(Value::Object(_))) {
        return;
    }
    let scalar = env.get("value").cloned();
    if let Some(obj) = env.as_object_mut() {
        let mut m = serde_json::Map::new();
        if let (Some(s), Some(first)) = (scalar, fields.first()) {
            if !s.is_null() {
                m.insert(first.name().to_owned(), s);
            }
        }
        obj.insert("value".to_owned(), Value::Object(m));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{register_block, Block};
    use serde_json::json;

    // --- Test blocks (registered into the inventory for the test bin) ---

    #[derive(Default)]
    struct TrCallout;
    impl Block for TrCallout {
        fn type_name(&self) -> &'static str {
            "tr_callout"
        }
        fn verbose_name(&self) -> &'static str {
            "Callout"
        }
        fn fields(&self) -> Vec<BlockField> {
            vec![
                BlockField::widget("tone", "Tone", WidgetKind::Select),
                BlockField::widget("title", "Title", WidgetKind::Text),
                BlockField::widget("body", "Body", WidgetKind::Markdown),
            ]
        }
    }
    register_block!(TrCallout);

    #[derive(Default)]
    struct TrParam;
    impl Block for TrParam {
        fn type_name(&self) -> &'static str {
            "tr_param"
        }
        fn verbose_name(&self) -> &'static str {
            "Param"
        }
        fn fields(&self) -> Vec<BlockField> {
            vec![
                BlockField::widget("name", "Name", WidgetKind::Text),
                BlockField::widget("type_", "Type", WidgetKind::Text),
                BlockField::widget("description", "Description", WidgetKind::Markdown),
            ]
        }
    }
    register_block!(TrParam);

    #[derive(Default)]
    struct TrApiRef;
    impl Block for TrApiRef {
        fn type_name(&self) -> &'static str {
            "tr_api_reference"
        }
        fn verbose_name(&self) -> &'static str {
            "API reference"
        }
        fn fields(&self) -> Vec<BlockField> {
            vec![
                BlockField::widget("symbol", "Symbol", WidgetKind::Text),
                BlockField::widget("page", "Page", WidgetKind::PageChooser),
                BlockField::repeat("params", "Params", "tr_param"),
            ]
        }
    }
    register_block!(TrApiRef);

    #[derive(Default)]
    struct TrMarkdown;
    impl Block for TrMarkdown {
        fn type_name(&self) -> &'static str {
            "tr_markdown"
        }
        fn verbose_name(&self) -> &'static str {
            "Markdown"
        }
        fn fields(&self) -> Vec<BlockField> {
            vec![BlockField::widget("source", "Source", WidgetKind::Markdown)]
        }
    }
    register_block!(TrMarkdown);

    #[test]
    fn collect_flat_skips_structural() {
        let stream = json!([{
            "type": "tr_callout", "id": "c1",
            "value": {"tone": "info", "title": "Hello", "body": "world"}
        }]);
        let leaves = collect_translatable_leaves("body", &stream);
        let paths: Vec<_> = leaves.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths, vec!["body.c1.title", "body.c1.body"]); // tone (Select) excluded
        assert_eq!(leaves[0].canonical_text, "Hello");
        assert_eq!(leaves[0].depth, 0);
    }

    #[test]
    fn collect_nested_repeat() {
        let stream = json!([{
            "type": "tr_api_reference", "id": "a1",
            "value": {
                "symbol": "QuerySet", "page": 7,
                "params": [
                    {"type": "tr_param", "id": "p1", "value": {"name": "limit", "type_": "int", "description": "max rows"}},
                    {"type": "tr_param", "id": "p2", "value": {"name": "offset", "type_": "int", "description": "skip rows"}}
                ]
            }
        }]);
        let leaves = collect_translatable_leaves("body", &stream);
        let paths: Vec<_> = leaves.iter().map(|l| l.path.clone()).collect();
        assert!(paths.contains(&"body.a1.symbol".to_owned()));
        assert!(!paths.iter().any(|p| p.contains("page"))); // PageChooser excluded
        assert!(paths.contains(&"body.a1.params.p1.name".to_owned()));
        assert!(paths.contains(&"body.a1.params.p2.description".to_owned()));
        // depth: top-level symbol=0, nested param fields=1
        let symbol = leaves.iter().find(|l| l.path == "body.a1.symbol").unwrap();
        assert_eq!(symbol.depth, 0);
        let pname = leaves
            .iter()
            .find(|l| l.path == "body.a1.params.p1.name")
            .unwrap();
        assert_eq!(pname.depth, 1);
    }

    #[test]
    fn collect_bare_scalar_single_field() {
        let stream = json!([{"type": "tr_markdown", "id": "m1", "value": "# hi"}]);
        let leaves = collect_translatable_leaves("body", &stream);
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].path, "body.m1.source");
        assert_eq!(leaves[0].canonical_text, "# hi");
    }

    #[test]
    fn collect_drift_block_yields_nothing() {
        let stream = json!([{"type": "totally_unknown", "id": "x1", "value": {"a": "b"}}]);
        assert!(collect_translatable_leaves("body", &stream).is_empty());
    }

    #[test]
    fn apply_round_trip_overrides_every_leaf() {
        let stream = json!([{
            "type": "tr_api_reference", "id": "a1",
            "value": {
                "symbol": "QuerySet", "page": 7,
                "params": [{"type": "tr_param", "id": "p1", "value": {"name": "limit", "type_": "int", "description": "max"}}]
            }
        }]);
        let tr = HashMap::from([
            ("body.a1.symbol".to_owned(), "JeuRequête".to_owned()),
            ("body.a1.params.p1.name".to_owned(), "limite".to_owned()),
            (
                "body.a1.params.p1.description".to_owned(),
                "lignes max".to_owned(),
            ),
        ]);
        let out = apply_translatable_overrides("body", &stream, &tr);
        let block = &out[0]["value"];
        assert_eq!(block["symbol"], "JeuRequête");
        assert_eq!(block["page"], 7); // structural untouched
        let p1 = &block["params"][0]["value"];
        assert_eq!(p1["name"], "limite");
        assert_eq!(p1["type_"], "int"); // not overridden → canonical
        assert_eq!(p1["description"], "lignes max");
        // ids + types preserved through the merge
        assert_eq!(out[0]["id"], "a1");
        assert_eq!(out[0]["value"]["params"][0]["id"], "p1");
        assert_eq!(out[0]["value"]["params"][0]["type"], "tr_param");
    }

    #[test]
    fn apply_partial_and_whitespace_fallback() {
        let stream = json!([{"type": "tr_callout", "id": "c1", "value": {"tone": "info", "title": "Hello", "body": "world"}}]);
        let tr = HashMap::from([
            ("body.c1.title".to_owned(), "Bonjour".to_owned()),
            ("body.c1.body".to_owned(), "   ".to_owned()), // whitespace-only → ignored
        ]);
        let out = apply_translatable_overrides("body", &stream, &tr);
        assert_eq!(out[0]["value"]["title"], "Bonjour");
        assert_eq!(out[0]["value"]["body"], "world"); // fallback to canonical
    }

    #[test]
    fn apply_bare_scalar_written_as_object() {
        let stream = json!([{"type": "tr_markdown", "id": "m1", "value": "# hi"}]);
        let tr = HashMap::from([("body.m1.source".to_owned(), "# bonjour".to_owned())]);
        let out = apply_translatable_overrides("body", &stream, &tr);
        assert_eq!(out[0]["value"]["source"], "# bonjour"); // normalized to object form
    }

    #[test]
    fn apply_stable_under_reorder() {
        let mk = |id: &str, title: &str| json!({"type": "tr_callout", "id": id, "value": {"tone": "info", "title": title, "body": "b"}});
        let tr = HashMap::from([("body.c2.title".to_owned(), "Deux".to_owned())]);
        let a = json!([mk("c1", "One"), mk("c2", "Two")]);
        let b = json!([mk("c2", "Two"), mk("c1", "One")]); // reversed
        let oa = apply_translatable_overrides("body", &a, &tr);
        let ob = apply_translatable_overrides("body", &b, &tr);
        // c2 localized in both regardless of position
        assert_eq!(oa[1]["value"]["title"], "Deux");
        assert_eq!(ob[0]["value"]["title"], "Deux");
    }

    #[test]
    fn apply_drift_passes_through() {
        let stream = json!([{"type": "totally_unknown", "id": "x1", "value": {"a": "b"}}]);
        let out = apply_translatable_overrides("body", &stream, &HashMap::new());
        assert_eq!(out, stream);
    }

    #[test]
    fn is_translatable_text_truth_table() {
        assert!(WidgetKind::Text.is_translatable_text());
        assert!(WidgetKind::Textarea.is_translatable_text());
        assert!(WidgetKind::Markdown.is_translatable_text());
        assert!(WidgetKind::RichText.is_translatable_text());
        assert!(!WidgetKind::Select.is_translatable_text());
        assert!(!WidgetKind::PageChooser.is_translatable_text());
        assert!(!WidgetKind::Url.is_translatable_text());
        assert!(!WidgetKind::Number.is_translatable_text());
    }
}
