//! Page-builder schema document (#559/#560) — the node tree a Developer
//! authors in the visual builder, stored (versioned) per page type.
//!
//! Shape (internally tagged on `kind`):
//!
//! ```json
//! { "nodes": [
//!   { "kind": "field",    "key": "subtitle", "label": "Subtitle", "widget": "text" },
//!   { "kind": "row",      "children": [ …field nodes with "width"… ] },
//!   { "kind": "group",    "key": "hero", "label": "Hero", "children": [ … ] },
//!   { "kind": "component","ref": "cta_banner" },
//!   { "kind": "repeater", "key": "faqs", "label": "FAQs", "max": 10,
//!     "item": { "key": "faq", "label": "FAQ", "children": [ … ] } },
//!   { "kind": "flex",     "key": "sections", "label": "Sections",
//!     "allowed": ["quote_pair", "c_cta_banner", "heading"],
//!     "groups":  [ { "key": "quote_pair", "label": "Quote pair", "children": [ … ] } ] }
//! ] }
//! ```
//!
//! Reuses the form builder's storage shapes ([`Choice`], [`ConditionalRule`],
//! key rules) so the two builders stay one mental model. `widget` values are
//! [`WidgetKind`] in its serde (lowercase) encoding — the same identifiers
//! `_widget.html` dispatches on.
//!
//! Structural rules (enforced by [`validate`], not the type system):
//! rows contain only fields; groups / repeater items / flex-zone groups
//! contain fields + rows. Repeaters/zones never nest inside groups or each
//! other in v1 — bounded depth by construction.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::forms::schema::{is_valid_key, Choice, ConditionalRule};
use crate::widget::WidgetKind;

/// One authored node in the builder canvas.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    Field(FieldNode),
    Row(RowNode),
    Group(GroupNode),
    Component(ComponentNode),
    Repeater(RepeaterNode),
    Flex(FlexNode),
}

/// A scalar input. `widget` is the admin widget kind; validation extras
/// mirror the [`crate::widget::Widget`] surface so compilation is 1:1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldNode {
    /// Builder-minted node id (stable across edits; used by the i18n
    /// leaf model + the builder UI). Backfilled client-side.
    #[serde(default)]
    pub id: String,
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default = "default_widget")]
    pub widget: WidgetKind,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub placeholder: String,
    /// Initial value for new pages (JSON-encoded for multi-value kinds).
    #[serde(default, rename = "default")]
    pub default_value: String,
    #[serde(default)]
    pub required: bool,
    /// 1–12 column width when this field sits inside a row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<Choice>,
    /// Conditional show/hide rules (form-builder storage shape).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<ConditionalRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u32>,
    #[serde(default)]
    pub pattern: String,
}

fn default_widget() -> WidgetKind {
    WidgetKind::Text
}

impl Default for FieldNode {
    fn default() -> Self {
        Self {
            id: String::new(),
            key: String::new(),
            label: String::new(),
            widget: WidgetKind::Text,
            help: String::new(),
            placeholder: String::new(),
            default_value: String::new(),
            required: false,
            width: None,
            options: Vec::new(),
            rules: Vec::new(),
            min: None,
            max: None,
            step: None,
            min_length: None,
            max_length: None,
            pattern: String::new(),
        }
    }
}

/// Side-by-side layout container. Children must be fields.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RowNode {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub children: Vec<Node>,
}

/// A named, always-present struct (ACF group): image + title +
/// description under one key. Children: fields + rows.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GroupNode {
    #[serde(default)]
    pub id: String,
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default)]
    pub children: Vec<Node>,
}

/// A reference to a reusable component from the library (`cms_component`).
/// Values store under `key` (defaults to the ref slug).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ComponentNode {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// N instances of one field group (ACF repeater / Wagtail ListBlock).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepeaterNode {
    #[serde(default)]
    pub id: String,
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
    pub item: GroupDef,
}

/// A flexible-content zone (ACF flexible content / Strapi dynamic zone):
/// the editor freely composes entries from `allowed`. `allowed` entries
/// resolve to zone-local `groups`, library components (`c_<slug>`), or
/// code-registered blocks (`find_block`) — interop bonus.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlexNode {
    #[serde(default)]
    pub id: String,
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
    #[serde(default)]
    pub allowed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<GroupDef>,
}

/// A field-group definition — the shared shape behind repeater items,
/// zone-local groups, and library components. Children: fields + rows.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GroupDef {
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Collapsed-header substitution, e.g. `"{question}"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_format: Option<String>,
    #[serde(default)]
    pub children: Vec<Node>,
}

/// The document stored in `cms_page_type_schema.document`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Document {
    #[serde(default)]
    pub nodes: Vec<Node>,
}

/// The document stored in `cms_component.document`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ComponentDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_format: Option<String>,
    #[serde(default)]
    pub children: Vec<Node>,
}

/// A library component handed to [`validate`] / [`crate::page_builder::compile`].
#[derive(Debug, Clone)]
pub struct ComponentEntry {
    pub doc: ComponentDoc,
    /// `cms_component.version` — stamped into instance envelopes so
    /// lazy upgrade can diff.
    pub version: u32,
}

/// Parse a document from its stored JSON. Lenient like the form
/// builder's `parse`: missing keys default.
///
/// # Errors
/// Serde error on structurally invalid JSON (unknown `kind`/widget).
pub fn parse(value: &serde_json::Value) -> Result<Document, serde_json::Error> {
    serde_json::from_value(value.clone())
}

/// Widget kinds a Developer may pick in v1. Excludes kinds that are
/// meaningless or dangerous as authored content fields (`custom` needs
/// host HTML, `stream` is expressed via repeater/flex nodes, `hidden`/
/// `password` invite abuse, `snippetm2m` is page-relation machinery).
#[must_use]
pub fn widget_allowed(kind: WidgetKind) -> bool {
    !matches!(
        kind,
        WidgetKind::Custom
            | WidgetKind::Stream
            | WidgetKind::Hidden
            | WidgetKind::Password
            | WidgetKind::SnippetM2M
    )
}

/// The dyn-block type name a repeater item compiles to.
#[must_use]
pub fn repeater_item_type(key: &str) -> String {
    format!("r_{key}")
}

/// The dyn-block type name a library component compiles to.
#[must_use]
pub fn component_type(slug: &str) -> String {
    format!("c_{slug}")
}

/// Validate a schema document against structural + referential rules.
/// Returns human-readable problems (empty = valid). Mirrors
/// `forms::schema::validate`'s "collect everything, don't bail early"
/// contract so the builder can show all issues at once.
#[must_use]
pub fn validate(doc: &Document, components: &HashMap<String, ComponentEntry>) -> Vec<String> {
    let mut problems = Vec::new();
    let mut top_keys: HashSet<String> = HashSet::new();
    let mut top_fields: HashSet<String> = HashSet::new();

    let claim = |key: &str, what: &str, problems: &mut Vec<String>, keys: &mut HashSet<String>| {
        if !is_valid_key(key) {
            problems.push(format!(
                "{what} key `{key}` is invalid — use letters/digits/underscores, starting with a letter"
            ));
        }
        if !keys.insert(key.to_owned()) {
            problems.push(format!(
                "duplicate key `{key}` — every top-level key must be unique"
            ));
        }
    };

    for node in &doc.nodes {
        match node {
            Node::Field(f) => {
                claim(&f.key, "field", &mut problems, &mut top_keys);
                top_fields.insert(f.key.clone());
                check_field(f, "field", &mut problems);
            }
            Node::Row(row) => {
                for child in &row.children {
                    match child {
                        Node::Field(f) => {
                            claim(&f.key, "field", &mut problems, &mut top_keys);
                            top_fields.insert(f.key.clone());
                            check_field(f, "row field", &mut problems);
                            if let Some(w) = f.width {
                                if !(1..=12).contains(&w) {
                                    problems.push(format!(
                                        "row field `{}` width {w} out of range 1..=12",
                                        f.key
                                    ));
                                }
                            }
                        }
                        _ => problems.push("rows may contain only fields".to_owned()),
                    }
                }
            }
            Node::Group(g) => {
                claim(&g.key, "group", &mut problems, &mut top_keys);
                check_children(&g.children, &format!("group `{}`", g.key), &mut problems);
            }
            Node::Component(c) => {
                let key = c.key.clone().unwrap_or_else(|| c.reference.clone());
                claim(&key, "component", &mut problems, &mut top_keys);
                if !components.contains_key(&c.reference) {
                    problems.push(format!(
                        "component `{}` references unknown library component `{}`",
                        key, c.reference
                    ));
                }
            }
            Node::Repeater(r) => {
                claim(&r.key, "repeater", &mut problems, &mut top_keys);
                check_group_def(
                    &r.item,
                    &format!("repeater `{}` item", r.key),
                    &mut problems,
                );
                check_dyn_name_free(&repeater_item_type(&r.key), &mut problems);
                if let (Some(min), Some(max)) = (r.min, r.max) {
                    if min > max {
                        problems.push(format!("repeater `{}` has min > max", r.key));
                    }
                }
            }
            Node::Flex(z) => {
                claim(&z.key, "flex zone", &mut problems, &mut top_keys);
                let mut local: HashSet<&str> = HashSet::new();
                for g in &z.groups {
                    if !local.insert(g.key.as_str()) {
                        problems.push(format!(
                            "flex zone `{}` defines group `{}` twice",
                            z.key, g.key
                        ));
                    }
                    check_group_def(
                        g,
                        &format!("zone `{}` group `{}`", z.key, g.key),
                        &mut problems,
                    );
                    check_dyn_name_free(&g.key, &mut problems);
                }
                if z.allowed.is_empty() {
                    problems.push(format!("flex zone `{}` allows no block types", z.key));
                }
                for a in &z.allowed {
                    let resolves = local.contains(a.as_str())
                        || a.strip_prefix("c_")
                            .is_some_and(|slug| components.contains_key(slug))
                        || crate::block::find_block(a).is_some();
                    if !resolves {
                        problems.push(format!(
                            "flex zone `{}` allows `{a}` which is neither a zone group, a \
                             library component (c_<slug>), nor a registered block",
                            z.key
                        ));
                    }
                }
            }
        }
    }

    // Conditional rules may only reference top-level field keys (v1
    // scope; per-item rules validate against item-local keys below).
    for node in &doc.nodes {
        let fields: Vec<&FieldNode> = match node {
            Node::Field(f) => vec![f],
            Node::Row(r) => r
                .children
                .iter()
                .filter_map(|n| match n {
                    Node::Field(f) => Some(f),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        for f in fields {
            for rule in &f.rules {
                for cond in &rule.conditions {
                    if !top_fields.contains(&cond.field) {
                        problems.push(format!(
                            "field `{}` has a rule referencing unknown field `{}`",
                            f.key, cond.field
                        ));
                    }
                }
            }
        }
    }

    problems
}

/// Validate a component's own document (fields + rows only).
#[must_use]
pub fn validate_component(doc: &ComponentDoc) -> Vec<String> {
    let mut problems = Vec::new();
    check_children(&doc.children, "component", &mut problems);
    for node in &doc.children {
        if matches!(
            node,
            Node::Group(_) | Node::Component(_) | Node::Repeater(_) | Node::Flex(_)
        ) {
            problems.push(
                "components may contain only fields and rows (no nested groups/repeaters/zones in v1)"
                    .to_owned(),
            );
        }
    }
    problems
}

/// Whether a page-type schema references a library component by slug —
/// either a [`ComponentNode`] (`ref == slug`) anywhere in the tree, or a
/// flex zone whose `allowed` lists `c_<slug>` (#563 delete guard).
#[must_use]
pub fn references_component(doc: &Document, slug: &str) -> bool {
    let tag = component_type(slug);
    nodes_reference_component(&doc.nodes, slug, &tag)
}

fn nodes_reference_component(nodes: &[Node], slug: &str, tag: &str) -> bool {
    nodes.iter().any(|node| match node {
        Node::Component(c) => c.reference == slug,
        Node::Row(r) => nodes_reference_component(&r.children, slug, tag),
        Node::Group(g) => nodes_reference_component(&g.children, slug, tag),
        Node::Repeater(r) => nodes_reference_component(&r.item.children, slug, tag),
        Node::Flex(f) => {
            f.allowed.iter().any(|a| a == tag)
                || f.groups
                    .iter()
                    .any(|g| nodes_reference_component(&g.children, slug, tag))
        }
        Node::Field(_) => false,
    })
}

fn check_field(f: &FieldNode, what: &str, problems: &mut Vec<String>) {
    if !widget_allowed(f.widget) {
        problems.push(format!(
            "{what} `{}` uses widget kind `{}` which is not allowed in the builder",
            f.key,
            f.widget.as_tag()
        ));
    }
    if matches!(
        f.widget,
        WidgetKind::Select | WidgetKind::Radio | WidgetKind::Checkboxes | WidgetKind::MultiSelect
    ) && f.options.is_empty()
    {
        problems.push(format!(
            "{what} `{}` is a choice widget with no options",
            f.key
        ));
    }
}

/// Children of a group-like container: fields + rows (of fields), with
/// container-local key uniqueness.
fn check_children(children: &[Node], scope: &str, problems: &mut Vec<String>) {
    let mut keys: HashSet<String> = HashSet::new();
    let visit = |f: &FieldNode, problems: &mut Vec<String>, keys: &mut HashSet<String>| {
        if !is_valid_key(&f.key) {
            problems.push(format!("{scope}: field key `{}` is invalid", f.key));
        }
        if !keys.insert(f.key.clone()) {
            problems.push(format!("{scope}: duplicate field key `{}`", f.key));
        }
        check_field(f, scope, problems);
    };
    for node in children {
        match node {
            Node::Field(f) => visit(f, problems, &mut keys),
            Node::Row(row) => {
                for child in &row.children {
                    match child {
                        Node::Field(f) => visit(f, problems, &mut keys),
                        _ => problems.push(format!("{scope}: rows may contain only fields")),
                    }
                }
            }
            _ => problems.push(format!(
                "{scope}: only fields and rows are allowed inside groups/items"
            )),
        }
    }
}

fn check_group_def(g: &GroupDef, scope: &str, problems: &mut Vec<String>) {
    if !is_valid_key(&g.key) {
        problems.push(format!("{scope}: key `{}` is invalid", g.key));
    }
    check_children(&g.children, scope, problems);
    // Per-item conditional rules reference item-local field keys.
    let local: HashSet<&str> = g
        .children
        .iter()
        .flat_map(|n| match n {
            Node::Field(f) => vec![f.key.as_str()],
            Node::Row(r) => r
                .children
                .iter()
                .filter_map(|c| match c {
                    Node::Field(f) => Some(f.key.as_str()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    for node in &g.children {
        if let Node::Field(f) = node {
            for rule in &f.rules {
                for cond in &rule.conditions {
                    if !local.contains(cond.field.as_str()) {
                        problems.push(format!(
                            "{scope}: field `{}` rule references `{}` which is not in the same item",
                            f.key, cond.field
                        ));
                    }
                }
            }
        }
    }
}

/// A UI-defined dyn-block name must not shadow a code-registered block.
fn check_dyn_name_free(name: &str, problems: &mut Vec<String>) {
    if crate::block::find_block(name).is_some() {
        problems.push(format!(
            "`{name}` collides with a code-registered block type — pick a different key"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn components() -> HashMap<String, ComponentEntry> {
        let mut m = HashMap::new();
        m.insert(
            "cta_banner".to_owned(),
            ComponentEntry {
                doc: ComponentDoc {
                    label_format: None,
                    children: vec![
                        Node::Field(FieldNode {
                            key: "text".into(),
                            label: "Text".into(),
                            widget: WidgetKind::Text,
                            ..Default::default()
                        }),
                        Node::Field(FieldNode {
                            key: "url".into(),
                            label: "URL".into(),
                            widget: WidgetKind::Url,
                            ..Default::default()
                        }),
                    ],
                },
                version: 3,
            },
        );
        m
    }

    fn fixture_json() -> serde_json::Value {
        serde_json::json!({
            "nodes": [
                { "kind": "field", "key": "subtitle", "label": "Subtitle",
                  "widget": "text", "required": true },
                { "kind": "row", "children": [
                    { "kind": "field", "key": "layout", "label": "Layout", "widget": "select",
                      "width": 6, "options": [ {"value": "wide", "label": "Wide"},
                                                {"value": "boxed", "label": "Boxed"} ] },
                    { "kind": "field", "key": "accent", "label": "Accent", "widget": "color",
                      "width": 6,
                      "rules": [ { "match": "all", "action": "show",
                                   "conditions": [ {"field": "layout", "op": "eq", "value": "wide"} ] } ] }
                ]},
                { "kind": "group", "key": "hero", "label": "Hero", "children": [
                    { "kind": "field", "key": "image", "label": "Image", "widget": "mediapicker" },
                    { "kind": "field", "key": "heading", "label": "Heading", "widget": "text" }
                ]},
                { "kind": "component", "ref": "cta_banner" },
                { "kind": "repeater", "key": "faqs", "label": "FAQs", "max": 10,
                  "item": { "key": "faq", "label": "FAQ", "label_format": "{question}",
                    "children": [
                      { "kind": "field", "key": "question", "label": "Question",
                        "widget": "text", "required": true },
                      { "kind": "field", "key": "answer", "label": "Answer", "widget": "richtext" }
                    ] } },
                { "kind": "flex", "key": "sections", "label": "Sections",
                  "allowed": ["quote_pair", "c_cta_banner", "heading"],
                  "groups": [
                    { "key": "quote_pair", "label": "Quote pair", "children": [
                        { "kind": "field", "key": "quote", "label": "Quote", "widget": "textarea" },
                        { "kind": "field", "key": "attribution", "label": "By", "widget": "text" }
                    ] }
                  ] }
            ]
        })
    }

    #[test]
    fn fixture_parses_and_validates_clean() {
        let doc = parse(&fixture_json()).expect("parse");
        assert_eq!(doc.nodes.len(), 6);
        let problems = validate(&doc, &components());
        assert!(problems.is_empty(), "unexpected problems: {problems:?}");
    }

    #[test]
    fn duplicate_top_level_key_rejected() {
        let mut doc = parse(&fixture_json()).unwrap();
        doc.nodes.push(Node::Field(FieldNode {
            key: "subtitle".into(),
            ..Default::default()
        }));
        let problems = validate(&doc, &components());
        assert!(
            problems
                .iter()
                .any(|p| p.contains("duplicate key `subtitle`")),
            "{problems:?}"
        );
    }

    #[test]
    fn banned_widget_kind_rejected() {
        let doc = parse(&serde_json::json!({
            "nodes": [ { "kind": "field", "key": "sneaky", "widget": "hidden" } ]
        }))
        .unwrap();
        let problems = validate(&doc, &HashMap::new());
        assert!(
            problems.iter().any(|p| p.contains("not allowed")),
            "{problems:?}"
        );
    }

    #[test]
    fn dangling_component_and_flex_refs_rejected() {
        let doc = parse(&serde_json::json!({
            "nodes": [
                { "kind": "component", "ref": "nope" },
                { "kind": "flex", "key": "z", "label": "Z",
                  "allowed": ["missing_thing"], "groups": [] }
            ]
        }))
        .unwrap();
        let problems = validate(&doc, &HashMap::new());
        assert!(
            problems
                .iter()
                .any(|p| p.contains("unknown library component `nope`")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("missing_thing")),
            "{problems:?}"
        );
    }

    #[test]
    fn code_block_collision_rejected() {
        // `heading` is a registered builtin block.
        let doc = parse(&serde_json::json!({
            "nodes": [
                { "kind": "flex", "key": "z", "label": "Z", "allowed": ["heading2"],
                  "groups": [ { "key": "heading2", "label": "H", "children": [] } ] },
                { "kind": "repeater", "key": "items", "label": "Items",
                  "item": { "key": "item", "children": [] } }
            ]
        }))
        .unwrap();
        // Sanity: zone-local `heading2` doesn't collide; now force one.
        let doc2 = parse(&serde_json::json!({
            "nodes": [
                { "kind": "flex", "key": "z", "label": "Z", "allowed": ["heading"],
                  "groups": [ { "key": "heading", "label": "H", "children": [] } ] }
            ]
        }))
        .unwrap();
        assert!(validate(&doc, &HashMap::new()).is_empty());
        let problems = validate(&doc2, &HashMap::new());
        assert!(
            problems
                .iter()
                .any(|p| p.contains("collides with a code-registered block")),
            "{problems:?}"
        );
    }

    #[test]
    fn rule_referencing_unknown_field_rejected() {
        let doc = parse(&serde_json::json!({
            "nodes": [
                { "kind": "field", "key": "a", "widget": "text",
                  "rules": [ { "conditions": [ {"field": "ghost", "op": "eq", "value": "1"} ] } ] }
            ]
        }))
        .unwrap();
        let problems = validate(&doc, &HashMap::new());
        assert!(
            problems.iter().any(|p| p.contains("unknown field `ghost`")),
            "{problems:?}"
        );
    }

    #[test]
    fn references_component_finds_direct_and_flex_and_nested() {
        // Direct top-level component node.
        let direct = parse(&serde_json::json!({
            "nodes": [ { "kind": "component", "ref": "cta" } ]
        }))
        .unwrap();
        assert!(references_component(&direct, "cta"));
        assert!(!references_component(&direct, "other"));

        // Flex zone allowing the component via `c_<slug>`.
        let flex = parse(&serde_json::json!({
            "nodes": [ { "kind": "flex", "key": "z", "allowed": ["c_cta", "quote"] } ]
        }))
        .unwrap();
        assert!(references_component(&flex, "cta"));
        assert!(!references_component(&flex, "quote"));

        // Nested inside a group.
        let nested = parse(&serde_json::json!({
            "nodes": [ { "kind": "group", "key": "g", "children": [ { "kind": "component", "ref": "cta" } ] } ]
        }))
        .unwrap();
        assert!(references_component(&nested, "cta"));

        // No reference at all.
        let none = parse(&serde_json::json!({
            "nodes": [ { "kind": "field", "key": "a", "widget": "text" } ]
        }))
        .unwrap();
        assert!(!references_component(&none, "cta"));
    }
}
