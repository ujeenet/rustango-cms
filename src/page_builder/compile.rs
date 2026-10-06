//! Schema → runtime compilation (#559/#560). Turns a validated
//! [`Document`](crate::page_builder::schema::Document) into the shapes the
//! existing engines consume:
//!
//! - fixed fields / rows / groups → [`Widget`]s (rendered by the shared
//!   `_widget.html` macro; inputs namespaced `pb__…`),
//! - repeaters / flex zones → one `WidgetKind::Stream` [`Widget`] each +
//!   [`DynBlockDef`]s in a [`DynBlockSet`] overlay,
//! - conditional rules → flat [`RuleBinding`]s for the editor's rules
//!   island + server-side visibility checks.
//!
//! Compilation is cheap (no I/O) — callers compile per request from the
//! published schema row + the component library.

use std::collections::HashMap;
use std::sync::Arc;

use crate::forms::schema::ConditionalRule;
use crate::page_builder::dyn_block::{DynBlockDef, DynBlockSet};
use crate::page_builder::schema::{
    repeater_item_type, ComponentEntry, Document, FieldNode, GroupDef, Node,
};
use crate::widget::Widget;

/// Prefix for every builder-owned form input — keeps UI-defined fields
/// from colliding with base-page fields and code handlers' extension
/// fields.
pub const FIELD_PREFIX: &str = "pb__";

/// One editor-facing layout item, in authored order.
#[derive(Debug, Clone)]
pub enum BodyItem {
    /// A stacked scalar input.
    Field(Widget),
    /// Side-by-side inputs with 1–12 widths.
    Row(Vec<(Widget, u8)>),
    /// A named fieldset of stacked/row inputs (values nest under `key`).
    Group {
        key: String,
        label: String,
        items: Vec<BodyItem>,
    },
    /// A repeater or flex zone — one stream editor.
    Zone {
        key: String,
        label: String,
        widget: Widget,
        min: Option<u32>,
        max: Option<u32>,
    },
}

/// A conditional rule bound to the field it governs.
#[derive(Debug, Clone)]
pub struct RuleBinding {
    /// The governed field's key (top-level namespace).
    pub field: String,
    pub rule: ConditionalRule,
}

/// The compiled, render-ready schema.
#[derive(Debug, Clone, Default)]
pub struct CompiledSchema {
    pub body: Vec<BodyItem>,
    pub dyn_blocks: DynBlockSet,
    pub rules: Vec<RuleBinding>,
    /// The published schema version this was compiled from.
    pub version: u32,
}

impl CompiledSchema {
    /// Every value-bearing top-level key, in authored order — the save
    /// path walks this.
    #[must_use]
    pub fn top_level_keys(&self) -> Vec<String> {
        let mut out = Vec::new();
        for item in &self.body {
            match item {
                BodyItem::Field(w) => out.push(strip_prefix(&w.name)),
                BodyItem::Row(cells) => {
                    out.extend(cells.iter().map(|(w, _)| strip_prefix(&w.name)));
                }
                BodyItem::Group { key, .. } | BodyItem::Zone { key, .. } => out.push(key.clone()),
            }
        }
        out
    }
}

fn strip_prefix(name: &str) -> String {
    name.strip_prefix(FIELD_PREFIX).unwrap_or(name).to_owned()
}

/// Compile a validated document. `schema_version` is the published
/// version (stamped on zone/repeater dyn blocks so stored entries carry
/// it); component dyn blocks carry their own component version.
#[must_use]
pub fn compile(
    doc: &Document,
    components: &HashMap<String, ComponentEntry>,
    schema_version: u32,
) -> CompiledSchema {
    let mut out = CompiledSchema {
        version: schema_version,
        ..Default::default()
    };

    for node in &doc.nodes {
        match node {
            Node::Field(f) => {
                collect_rules(f, &mut out.rules);
                out.body
                    .push(BodyItem::Field(field_to_widget(f, FIELD_PREFIX)));
            }
            Node::Row(row) => {
                let mut cells = Vec::new();
                let fields: Vec<&FieldNode> = row
                    .children
                    .iter()
                    .filter_map(|n| match n {
                        Node::Field(f) => Some(f),
                        _ => None,
                    })
                    .collect();
                let equal = (12 / fields.len().max(1)) as u8;
                for f in fields {
                    collect_rules(f, &mut out.rules);
                    cells.push((field_to_widget(f, FIELD_PREFIX), f.width.unwrap_or(equal)));
                }
                out.body.push(BodyItem::Row(cells));
            }
            Node::Group(g) => {
                // Group members namespace as pb__<group>__<field>.
                let prefix = format!("{FIELD_PREFIX}{}__", g.key);
                let mut items = Vec::new();
                for child in &g.children {
                    match child {
                        Node::Field(f) => items.push(BodyItem::Field(field_to_widget(f, &prefix))),
                        Node::Row(row) => {
                            let fields: Vec<&FieldNode> = row
                                .children
                                .iter()
                                .filter_map(|n| match n {
                                    Node::Field(f) => Some(f),
                                    _ => None,
                                })
                                .collect();
                            let equal = (12 / fields.len().max(1)) as u8;
                            items.push(BodyItem::Row(
                                fields
                                    .iter()
                                    .map(|f| {
                                        (field_to_widget(f, &prefix), f.width.unwrap_or(equal))
                                    })
                                    .collect(),
                            ));
                        }
                        _ => {}
                    }
                }
                out.body.push(BodyItem::Group {
                    key: g.key.clone(),
                    label: if g.label.is_empty() {
                        g.key.clone()
                    } else {
                        g.label.clone()
                    },
                    items,
                });
            }
            Node::Component(c) => {
                let key = c.key.clone().unwrap_or_else(|| c.reference.clone());
                let Some(entry) = components.get(&c.reference) else {
                    continue; // validate() flags this; compile degrades
                };
                // A component instance = a single-item repeater-style
                // zone locked to exactly one entry? No — v1 semantics:
                // an always-present group. Compile it like a group with
                // the component's children.
                let prefix = format!("{FIELD_PREFIX}{key}__");
                let mut items = Vec::new();
                for child in &entry.doc.children {
                    if let Node::Field(f) = child {
                        items.push(BodyItem::Field(field_to_widget(f, &prefix)));
                    } else if let Node::Row(row) = child {
                        let fields: Vec<&FieldNode> = row
                            .children
                            .iter()
                            .filter_map(|n| match n {
                                Node::Field(f) => Some(f),
                                _ => None,
                            })
                            .collect();
                        let equal = (12 / fields.len().max(1)) as u8;
                        items.push(BodyItem::Row(
                            fields
                                .iter()
                                .map(|f| (field_to_widget(f, &prefix), f.width.unwrap_or(equal)))
                                .collect(),
                        ));
                    }
                }
                out.body.push(BodyItem::Group {
                    key: key.clone(),
                    label: key.clone(),
                    items,
                });
            }
            Node::Repeater(r) => {
                let item_type = repeater_item_type(&r.key);
                out.dyn_blocks.insert(
                    item_type.clone(),
                    Arc::new(DynBlockDef::from_group(&item_type, &r.item, schema_version)),
                );
                let name = format!("{FIELD_PREFIX}{}", r.key);
                let widget = Widget::stream(
                    &name,
                    if r.label.is_empty() { &r.key } else { &r.label },
                    [item_type.as_str()],
                );
                out.body.push(BodyItem::Zone {
                    key: r.key.clone(),
                    label: if r.label.is_empty() {
                        r.key.clone()
                    } else {
                        r.label.clone()
                    },
                    widget,
                    min: r.min,
                    max: r.max,
                });
            }
            Node::Flex(z) => {
                // Zone-local groups become dyn blocks under their key.
                for g in &z.groups {
                    out.dyn_blocks.insert(
                        g.key.clone(),
                        Arc::new(DynBlockDef::from_group(&g.key, g, schema_version)),
                    );
                }
                // Component refs in `allowed` (c_<slug>) become dyn
                // blocks compiled from the library entry.
                for a in &z.allowed {
                    if let Some(slug) = a.strip_prefix("c_") {
                        if let Some(entry) = components.get(slug) {
                            let def = GroupDef {
                                key: a.clone(),
                                label: slug.replace('_', " "),
                                icon: None,
                                description: None,
                                label_format: entry.doc.label_format.clone(),
                                children: entry.doc.children.clone(),
                            };
                            out.dyn_blocks.insert(
                                a.clone(),
                                Arc::new(DynBlockDef::from_group(a, &def, entry.version)),
                            );
                        }
                    }
                }
                let name = format!("{FIELD_PREFIX}{}", z.key);
                let allowed: Vec<&str> = z.allowed.iter().map(String::as_str).collect();
                let widget = Widget::stream(
                    &name,
                    if z.label.is_empty() { &z.key } else { &z.label },
                    allowed,
                );
                out.body.push(BodyItem::Zone {
                    key: z.key.clone(),
                    label: if z.label.is_empty() {
                        z.key.clone()
                    } else {
                        z.label.clone()
                    },
                    widget,
                    min: z.min,
                    max: z.max,
                });
            }
        }
    }
    out
}

fn collect_rules(f: &FieldNode, rules: &mut Vec<RuleBinding>) {
    for rule in &f.rules {
        rules.push(RuleBinding {
            field: f.key.clone(),
            rule: rule.clone(),
        });
    }
}

/// Map one authored field into an admin [`Widget`], namespaced by
/// `prefix` (`pb__` or `pb__<group>__`).
#[must_use]
pub fn field_to_widget(f: &FieldNode, prefix: &str) -> Widget {
    let name = format!("{prefix}{}", f.key);
    let label = if f.label.is_empty() { &f.key } else { &f.label };
    let mut w = Widget::new(f.widget, name, label);
    if f.required {
        w = w.required();
    }
    if !f.help.is_empty() {
        w = w.with_help(&f.help);
    }
    if !f.placeholder.is_empty() {
        w = w.with_placeholder(&f.placeholder);
    }
    if !f.default_value.is_empty() {
        w = w.with_value(&f.default_value);
    }
    if !f.options.is_empty() {
        w = w.with_options(f.options.iter().map(|c| (c.value.clone(), c.label.clone())));
    }
    if let Some(min) = f.min {
        w = w.with_min(min);
    }
    if let Some(max) = f.max {
        w = w.with_max(max);
    }
    if let Some(step) = &f.step {
        w = w.with_step(step.clone());
    }
    if let Some(n) = f.max_length {
        w = w.with_max_length(n);
    }
    if let Some(n) = f.min_length {
        w = w.with_min_length(n);
    }
    if !f.pattern.is_empty() {
        w = w.with_pattern(&f.pattern);
    }
    // Choice widgets need `pb` marking for the save path? No — the save
    // path walks CompiledSchema, not the form blindly. Nothing else.
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Block as _;
    use crate::page_builder::schema::{parse, validate, ComponentDoc};
    use crate::widget::WidgetKind;

    fn components() -> HashMap<String, ComponentEntry> {
        let mut m = HashMap::new();
        m.insert(
            "cta_banner".to_owned(),
            ComponentEntry {
                doc: ComponentDoc {
                    label_format: None,
                    children: vec![Node::Field(FieldNode {
                        key: "text".into(),
                        label: "Text".into(),
                        widget: WidgetKind::Text,
                        ..Default::default()
                    })],
                },
                version: 3,
            },
        );
        m
    }

    fn fixture() -> Document {
        parse(&serde_json::json!({
            "nodes": [
                { "kind": "field", "key": "subtitle", "label": "Subtitle", "widget": "text",
                  "rules": [ { "conditions": [ {"field": "subtitle", "op": "not_empty", "value": ""} ] } ] },
                { "kind": "row", "children": [
                    { "kind": "field", "key": "a", "widget": "text" },
                    { "kind": "field", "key": "b", "widget": "text", "width": 4 }
                ]},
                { "kind": "group", "key": "hero", "label": "Hero", "children": [
                    { "kind": "field", "key": "image", "widget": "mediapicker" }
                ]},
                { "kind": "component", "ref": "cta_banner" },
                { "kind": "repeater", "key": "faqs", "label": "FAQs",
                  "item": { "key": "faq", "children": [
                      { "kind": "field", "key": "q", "widget": "text" } ] } },
                { "kind": "flex", "key": "sections", "label": "Sections",
                  "allowed": ["quote_pair", "c_cta_banner", "heading"],
                  "groups": [ { "key": "quote_pair", "label": "Quote", "children": [
                      { "kind": "field", "key": "quote", "widget": "textarea" } ] } ] }
            ]
        }))
        .expect("fixture parses")
    }

    #[test]
    fn compiles_expected_shapes() {
        let doc = fixture();
        assert!(validate(&doc, &components()).is_empty());
        let c = compile(&doc, &components(), 5);

        assert_eq!(c.body.len(), 6);
        assert!(matches!(&c.body[0], BodyItem::Field(w) if w.name == "pb__subtitle"));
        match &c.body[1] {
            BodyItem::Row(cells) => {
                assert_eq!(cells.len(), 2);
                assert_eq!(cells[0].1, 6, "equal split default");
                assert_eq!(cells[1].1, 4, "explicit width kept");
            }
            other => panic!("expected row, got {other:?}"),
        }
        match &c.body[2] {
            BodyItem::Group { key, items, .. } => {
                assert_eq!(key, "hero");
                assert!(matches!(&items[0], BodyItem::Field(w) if w.name == "pb__hero__image"));
            }
            other => panic!("expected group, got {other:?}"),
        }
        match &c.body[4] {
            BodyItem::Zone { widget, .. } => {
                assert_eq!(widget.kind, WidgetKind::Stream);
                assert_eq!(widget.allowed, vec!["r_faqs".to_owned()]);
            }
            other => panic!("expected repeater zone, got {other:?}"),
        }
        match &c.body[5] {
            BodyItem::Zone { widget, .. } => {
                assert_eq!(
                    widget.allowed,
                    vec![
                        "quote_pair".to_owned(),
                        "c_cta_banner".to_owned(),
                        "heading".to_owned()
                    ]
                );
            }
            other => panic!("expected flex zone, got {other:?}"),
        }

        // Dyn blocks: repeater item + zone group + component ref.
        assert!(c.dyn_blocks.contains_key("r_faqs"));
        assert!(c.dyn_blocks.contains_key("quote_pair"));
        assert!(c.dyn_blocks.contains_key("c_cta_banner"));
        assert_eq!(
            c.dyn_blocks["c_cta_banner"].version(),
            3,
            "component keeps its own version"
        );
        assert_eq!(
            c.dyn_blocks["r_faqs"].version(),
            5,
            "schema-local uses schema version"
        );

        // Rules + keys.
        assert_eq!(c.rules.len(), 1);
        assert_eq!(
            c.top_level_keys(),
            vec![
                "subtitle",
                "a",
                "b",
                "hero",
                "cta_banner",
                "faqs",
                "sections"
            ]
        );
    }
}
