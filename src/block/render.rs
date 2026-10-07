//! Default render path + recursive stream walker.
//!
//! Templates reach this through the `stream_render` Tera function
//! (`block::tera_helpers`).
//!
//! ## Default render
//!
//! [`default_render`] walks a block's [`crate::Block::fields`] in
//! order, deserializes each field's slice of the block value, and
//! feeds the lot into the block's Tera template
//! (`blocks/<type_name>.html` unless [`crate::Block::template`]
//! overrides). Nested `Stream` / `Repeat` fields recurse via
//! [`prerender_stream`].
//!
//! ## Stream walker
//!
//! [`prerender_stream`] takes a JSON `Value::Array` of
//! `{type, id, value}` entries, looks up each block in the registry,
//! calls [`crate::Block::render`], and concatenates the result. Unknown
//! `type` values surface a minimal yellow drift-banner snippet rather
//! than panicking — admins fix this in the editor by removing the
//! orphan block.

use serde_json::Value;
use tera::Context;

use super::{Block, BlockError, BlockRenderCtx};

/// Walk a block's fields and render its template.
///
/// Template lookup precedence:
/// 1. `block.template()` if it returns `Some(path)`.
/// 2. Otherwise `blocks/<type_name>.html`.
///
/// The rendered template receives the following Tera context:
/// - `value`: the block's whole value dict (mirrors what authors
///   actually want — addressing fields by name as
///   `{{ value.heading }}`).
/// - `block_type`: the wire `type_name`.
/// - Every [`crate::block::BlockField::Computed`] field's `render`
///   output as `computed.<field_name>` so templates can pull
///   derived chips inline.
///
/// # Errors
/// [`BlockError::TemplateRender`] on Tera failure;
/// [`BlockError::Shape`] when the block value isn't an object (the
/// only shape the field walker accepts).
pub fn default_render<B: Block + ?Sized>(
    block: &B,
    value: &Value,
    ctx: &BlockRenderCtx<'_>,
) -> Result<String, BlockError> {
    let template = block.template().map_or_else(
        || format!("blocks/{}.html", block.type_name()),
        str::to_owned,
    );

    let value_object = match value {
        Value::Object(_) => value.clone(),
        Value::Null => Value::Object(serde_json::Map::new()),
        // Single-field blocks may stash a bare scalar at `value`;
        // canonicalize to the {field_name: scalar} shape by looking
        // up the first field. Both shapes are accepted on input.
        scalar => {
            let mut map = serde_json::Map::new();
            if let Some(first) = block.fields().into_iter().next() {
                map.insert(first.name().to_owned(), scalar.clone());
            }
            Value::Object(map)
        }
    };

    // Evaluate every Computed field once, stash as `computed.<name>`.
    let mut computed = serde_json::Map::new();
    for field in block.fields() {
        if let super::BlockField::Computed { name, render, .. } = field {
            computed.insert(name, Value::String(render(&value_object, ctx)));
        }
    }

    // Stamp `Block::extra_context()` first — the framework's
    // canonical keys (`value`, `block_type`, `computed`) are stamped
    // afterwards so they always win, preventing a block from
    // accidentally shadowing them.
    let mut tera_ctx = Context::new();
    let extras = block.extra_context(&value_object, ctx);
    for (k, v) in extras {
        tera_ctx.insert(&k, &v);
    }
    tera_ctx.insert("value", &value_object);
    tera_ctx.insert("block_type", &block.type_name());
    tera_ctx.insert("computed", &Value::Object(computed));

    ctx.tera
        .render(&template, &tera_ctx)
        .map_err(|source| BlockError::TemplateRender {
            block_type: block.type_name().to_owned(),
            template,
            source,
        })
}

/// Walk a JSON array of `{type, id, value}` entries and concatenate
/// each block's [`Block::render`] output.
///
/// Unknown / unregistered `type` values emit a minimal `<div
/// class="rcms-stream-drift">` chrome carrying the unknown type name —
/// authors see exactly what's orphaned + can remove it via the editor.
/// This matches the schema-drift "yellow banner" UX from the plan
/// (`stream_block.py:235-287` filters silently; we render visibly).
///
/// A block that fails to render — a template error, a bad value — leaves a
/// hidden `rcms-stream-error` marker naming its type and is logged; the rest
/// of the stream still renders. It used to fail the whole stream, and
/// the page then rendered with its body missing.
///
/// # Errors
/// Non-array input returns a shape error.
pub fn prerender_stream(value: &Value, ctx: &BlockRenderCtx<'_>) -> Result<String, BlockError> {
    let items = match value {
        Value::Array(items) => items,
        Value::Null => return Ok(String::new()),
        _ => {
            return Err(BlockError::Shape {
                block_type: "<stream>".to_owned(),
                path: String::new(),
                reason: "expected JSON array of block entries".to_owned(),
            })
        }
    };

    let mut out = String::new();
    for (idx, entry) in items.iter().enumerate() {
        let Some(type_name) = entry.get("type").and_then(Value::as_str) else {
            tracing::warn!(target: "rustango_cms::block", index = idx, "stream entry has no `type`; skipped");
            out.push_str(&failed_block_marker("<missing type>"));
            continue;
        };
        let inner_value = entry.get("value").unwrap_or(&Value::Null);
        // #559 — the page-builder dyn-block overlay wins over the
        // inventory registry so UI-defined groups render like code blocks.
        match crate::page_builder::resolve_block(ctx.dyn_blocks, type_name) {
            Some(block) => match block.render(inner_value, ctx) {
                Ok(rendered) => out.push_str(&rendered),
                Err(e) => {
                    tracing::warn!(
                        target: "rustango_cms::block",
                        block_type = type_name, index = idx, error = %e,
                        "block failed to render; the rest of the stream still renders"
                    );
                    out.push_str(&failed_block_marker(type_name));
                }
            },
            None => {
                out.push_str(&format!(
                    "<div class=\"rcms-stream-drift\" data-unknown-type=\"{}\">\
                       Schema drift: unknown block type \"{}\" — \
                       was it removed without a migration?\
                     </div>",
                    tera::escape_html(type_name),
                    tera::escape_html(type_name),
                ));
            }
        }
    }
    Ok(out)
}

/// The stand-in for a block that failed to render: hidden, so visitors see
/// nothing broken, but findable in the page source by its type.
fn failed_block_marker(type_name: &str) -> String {
    format!(
        "<div class=\"rcms-stream-error\" data-block-type=\"{}\" hidden></div>",
        tera::escape_html(type_name)
    )
}
