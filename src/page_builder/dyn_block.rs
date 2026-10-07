//! Dyn blocks — UI-defined field groups that flow through the
//! existing [`Block`] machinery without inventory
//! registration.
//!
//! A [`DynBlockDef`] is compiled from a schema/component
//! [`GroupDef`] and **implements the
//! `Block` trait**, so the admin stream editor (`render_block_inline`), the
//! public renderer (`default_render`), and version migration all treat it
//! exactly like a code block. The trait wants `&'static str` names — dyn
//! defs intern theirs through a leak-once cache (bounded by the distinct
//! slugs/labels edited over a process lifetime; precedent:
//! `block/registry.rs` uses `Box::leak` for diagnostics).
//!
//! Resolution is an explicit **overlay**: a per-request
//! [`DynBlockSet`] (loaded from the tenant's published schema) is checked
//! before the inventory registry. No global state, tenant-safe.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde_json::Value;

use crate::block::{Block, BlockError, BlockField, BlockRenderCtx};
use crate::page_builder::schema::{FieldNode, GroupDef, Node};

/// Intern a string into the process-lifetime pool. Each distinct string
/// leaks exactly once; repeat calls return the cached `&'static str`.
/// Shared with [`crate::page_builder::db_type`] so UI-created page
/// types can hand the trait its required `&'static str`s from DB rows.
pub(crate) fn intern(s: &str) -> &'static str {
    static POOL: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut pool = POOL
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(existing) = pool.get(s) {
        return existing;
    }
    let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
    pool.insert(leaked);
    leaked
}

/// The per-request set of UI-defined block types, keyed by type name
/// (`r_<key>` repeater items, zone-local group keys, `c_<slug>`
/// components). Loaded from the published schema; passed by reference
/// through [`BlockRenderCtx::dyn_blocks`] and the `_with` editor variants.
pub type DynBlockSet = HashMap<String, Arc<DynBlockDef>>;

/// A UI-defined block: shape resolved from the DB at compile time, then
/// behaviorally identical to a code block.
#[derive(Debug)]
pub struct DynBlockDef {
    type_name: &'static str,
    verbose_name: &'static str,
    icon: Option<&'static str>,
    description: Option<&'static str>,
    label_format: Option<&'static str>,
    fields: Vec<BlockField>,
    version: u32,
}

impl DynBlockDef {
    /// Build from a schema/component group definition. `version` is the
    /// owning schema's (zone groups / repeater items) or component's
    /// version — stamped into stored entries for lazy upgrade.
    #[must_use]
    pub fn from_group(type_name: &str, def: &GroupDef, version: u32) -> Self {
        let label = if def.label.is_empty() {
            &def.key
        } else {
            &def.label
        };
        Self {
            type_name: intern(type_name),
            verbose_name: intern(label),
            icon: def.icon.as_deref().map(intern),
            description: def.description.as_deref().map(intern),
            label_format: def.label_format.as_deref().map(intern),
            fields: children_to_block_fields(&def.children),
            version,
        }
    }
}

/// Flatten a group's children (fields + rows) into sequential
/// [`BlockField`]s — the block editor stacks fields vertically, so row
/// layout inside stream items degrades to source order in v1.
pub(crate) fn children_to_block_fields(children: &[Node]) -> Vec<BlockField> {
    let mut out = Vec::new();
    for node in children {
        match node {
            Node::Field(f) => out.push(field_to_block_field(f)),
            Node::Row(row) => {
                for child in &row.children {
                    if let Node::Field(f) = child {
                        out.push(field_to_block_field(f));
                    }
                }
            }
            // validate() rejects anything else inside a group def.
            _ => {}
        }
    }
    out
}

fn field_to_block_field(f: &FieldNode) -> BlockField {
    let mut meta = crate::block::BlockFieldMeta::none();
    meta.min_length = f.min_length;
    meta.max_length = f.max_length;
    if !f.pattern.is_empty() {
        meta.pattern = Some(f.pattern.clone());
    }
    meta.min_value = f.min;
    meta.max_value = f.max;
    if !f.default_value.is_empty() {
        meta.default_value = Some(f.default_value.clone());
    }
    BlockField::Widget {
        name: f.key.clone(),
        label: if f.label.is_empty() {
            f.key.clone()
        } else {
            f.label.clone()
        },
        widget: f.widget,
        options: f
            .options
            .iter()
            .map(|c| (c.value.clone(), c.label.clone()))
            .collect(),
        help: if f.help.is_empty() {
            None
        } else {
            Some(f.help.clone())
        },
        required: f.required,
        meta,
    }
}

impl Block for DynBlockDef {
    fn type_name(&self) -> &'static str {
        self.type_name
    }
    fn verbose_name(&self) -> &'static str {
        self.verbose_name
    }
    fn fields(&self) -> Vec<BlockField> {
        self.fields.clone()
    }
    fn icon(&self) -> Option<&'static str> {
        self.icon
    }
    fn group(&self) -> Option<&'static str> {
        Some("Page sections")
    }
    fn description(&self) -> Option<&'static str> {
        self.description
    }
    fn label_format(&self) -> Option<&'static str> {
        self.label_format
    }
    fn version(&self) -> u32 {
        self.version
    }

    /// Host `blocks/<type_name>.html` wins when present; otherwise a
    /// generic Rust-side walker renders the group's fields — so a
    /// UI-defined group always renders without any template authoring.
    fn render(&self, value: &Value, ctx: &BlockRenderCtx<'_>) -> Result<String, BlockError> {
        let host_template = format!("blocks/{}.html", self.type_name);
        if ctx.tera.get_template_names().any(|n| n == host_template) {
            return crate::block::render::default_render(self, value, ctx);
        }
        Ok(generic_render(self, value, ctx))
    }
}

/// Template-free fallback render: a `<section>` wrapping one `<div>` per
/// field, class-hooked (`pb-group--<type>`, `pb-field--<name>`) so hosts
/// can style without templates. Nested arrays (from any nested stream
/// values) recurse through [`prerender_with`].
/// A block field as the [`crate::widget::Widget`] the value renderer reads.
fn as_widget(
    name: &str,
    label: &str,
    kind: crate::widget::WidgetKind,
    options: &[(String, String)],
) -> crate::widget::Widget {
    crate::widget::Widget::new(kind, name, label).with_options(options.iter().cloned())
}

fn generic_render(def: &DynBlockDef, value: &Value, ctx: &BlockRenderCtx<'_>) -> String {
    let obj = value.as_object();
    let mut out = format!(
        "<section class=\"pb-group pb-group--{}\">",
        tera::escape_html(def.type_name)
    );
    for field in &def.fields {
        let BlockField::Widget { name, label, widget, options, .. } = field else {
            continue;
        };
        let v = obj
            .and_then(|o| o.get(name.as_str()))
            .unwrap_or(&Value::Null);
        let inner = if let Value::Array(_) = v {
            if matches!(widget, crate::widget::WidgetKind::Checkboxes | crate::widget::WidgetKind::MultiSelect) {
                crate::page_builder::values::public_value_html(&as_widget(name, label, *widget, options), v)
            } else {
                // Nested stream-shaped value — recurse (drift-safe).
                crate::block::render::prerender_stream(v, ctx).ok()
            }
        } else {
            // Same rules as a fixed field (#844): rich text sanitized,
            // markdown rendered, choices by label; chooser ids are left
            // out rather than printed.
            crate::page_builder::values::public_value_html(&as_widget(name, label, *widget, options), v)
        };
        let Some(inner) = inner.filter(|h| !h.is_empty()) else {
            continue;
        };
        out.push_str(&format!(
            "<div class=\"pb-field pb-field--{}\">{inner}</div>",
            tera::escape_html(name)
        ));
    }
    out.push_str("</section>");
    out
}

/// Wrapper so an `Arc<DynBlockDef>` can travel as a `Box<dyn Block>`
/// alongside registry-owned blocks.
struct ArcDynBlock(Arc<DynBlockDef>);

impl Block for ArcDynBlock {
    fn type_name(&self) -> &'static str {
        self.0.type_name()
    }
    fn verbose_name(&self) -> &'static str {
        self.0.verbose_name()
    }
    fn fields(&self) -> Vec<BlockField> {
        self.0.fields()
    }
    fn icon(&self) -> Option<&'static str> {
        self.0.icon()
    }
    fn group(&self) -> Option<&'static str> {
        self.0.group()
    }
    fn description(&self) -> Option<&'static str> {
        self.0.description()
    }
    fn label_format(&self) -> Option<&'static str> {
        self.0.label_format()
    }
    fn version(&self) -> u32 {
        self.0.version()
    }
    fn render(&self, value: &Value, ctx: &BlockRenderCtx<'_>) -> Result<String, BlockError> {
        self.0.render(value, ctx)
    }
}

/// Resolve a block type through the overlay first, then the inventory
/// registry. THE lookup for every stream consumer that supports dyn
/// blocks.
#[must_use]
pub fn resolve(set: Option<&DynBlockSet>, type_name: &str) -> Option<Box<dyn Block>> {
    if let Some(def) = set.and_then(|s| s.get(type_name)) {
        return Some(Box::new(ArcDynBlock(Arc::clone(def))));
    }
    crate::block::find_block(type_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_builder::schema::FieldNode;
    use crate::widget::WidgetKind;

    fn quote_group() -> GroupDef {
        GroupDef {
            key: "quote_pair".into(),
            label: "Quote pair".into(),
            icon: None,
            description: None,
            label_format: Some("{attribution}".into()),
            children: vec![
                Node::Field(FieldNode {
                    key: "quote".into(),
                    label: "Quote".into(),
                    widget: WidgetKind::Textarea,
                    ..Default::default()
                }),
                Node::Field(FieldNode {
                    key: "attribution".into(),
                    label: "By".into(),
                    widget: WidgetKind::Text,
                    ..Default::default()
                }),
            ],
        }
    }

    #[test]
    fn dyn_block_implements_block_shape() {
        let def = DynBlockDef::from_group("quote_pair", &quote_group(), 7);
        assert_eq!(def.type_name(), "quote_pair");
        assert_eq!(def.verbose_name(), "Quote pair");
        assert_eq!(def.version(), 7);
        assert_eq!(def.label_format(), Some("{attribution}"));
        assert_eq!(def.fields().len(), 2);
    }

    #[test]
    fn intern_dedupes() {
        let a = intern("same-string-x");
        let b = intern("same-string-x");
        assert!(
            std::ptr::eq(a, b),
            "second intern must return the cached ptr"
        );
    }

    #[test]
    fn generic_render_escapes_and_walks_fields() {
        let def = DynBlockDef::from_group("quote_pair", &quote_group(), 1);
        let tera = tera::Tera::default();
        let ctx = BlockRenderCtx::new(&tera);
        let html = def
            .render(
                &serde_json::json!({"quote": "a<b", "attribution": "Ada"}),
                &ctx,
            )
            .expect("render");
        assert!(html.contains("pb-group--quote_pair"), "{html}");
        assert!(html.contains("a&lt;b"), "escaped: {html}");
        assert!(html.contains("Ada"), "{html}");
    }

    #[test]
    fn host_template_override_wins() {
        let def = DynBlockDef::from_group("quote_pair", &quote_group(), 1);
        let mut tera = tera::Tera::default();
        tera.add_raw_template(
            "blocks/quote_pair.html",
            "<blockquote>{{ value.quote }}</blockquote>",
        )
        .unwrap();
        let ctx = BlockRenderCtx::new(&tera);
        let html = def
            .render(&serde_json::json!({"quote": "hi"}), &ctx)
            .expect("render");
        assert_eq!(html, "<blockquote>hi</blockquote>");
    }

    #[test]
    fn resolve_prefers_overlay_then_registry() {
        let mut set: DynBlockSet = HashMap::new();
        set.insert(
            "quote_pair".to_owned(),
            Arc::new(DynBlockDef::from_group("quote_pair", &quote_group(), 1)),
        );
        assert!(resolve(Some(&set), "quote_pair").is_some(), "overlay hit");
        assert!(
            resolve(Some(&set), "heading").is_some(),
            "registry fallthrough"
        );
        assert!(
            resolve(None, "quote_pair").is_none(),
            "no overlay, not registered"
        );
    }
}
