//! The `form` StreamField block (#542 / FB-09).
//!
//! A dedicated block editors drop into any page StreamField — including
//! nested streams, since it's a registered block listed in `allowed` sets.
//! It references a built form (by id from the chooser, or by the slug older
//! pages typed in) and optionally overrides the
//! success redirect / message for that placement.
//!
//! The reference is resolved async in the stream pre-pass
//! ([`crate::block::tera_helpers::enrich_chooser_refs_async`], which injects
//! `_schema` / `_form_id` / `_embed` onto the block value); this block's
//! sync [`Block::render`] then turns the schema into HTML via
//! [`crate::forms::render`].

use crate::block::{Block, BlockError, BlockField, BlockFieldMeta, BlockRenderCtx};
use crate::widget::WidgetKind;

/// `form` — embeds a built form on a page.
#[derive(Default)]
pub struct FormBlock;

impl Block for FormBlock {
    fn type_name(&self) -> &'static str {
        "form"
    }

    fn verbose_name(&self) -> &'static str {
        "Form"
    }

    fn icon(&self) -> Option<&'static str> {
        Some("dynamic_form")
    }

    fn group(&self) -> Option<&'static str> {
        Some("Forms")
    }

    fn description(&self) -> Option<&'static str> {
        Some("Embed a reusable form. Optionally override its success message/redirect.")
    }

    fn fields(&self) -> Vec<BlockField> {
        vec![
            // A chooser narrowed to forms; it stores the form's id. Pages
            // saved before hold the slug typed here — both resolve.
            BlockField::widget("form", "Form", WidgetKind::SnippetChooser)
                .required()
                .with_meta(BlockFieldMeta::none().chooser_filter("form"))
                .with_help("The form to show (made under Forms)."),
            BlockField::widget(
                "success_message",
                "Success message override",
                WidgetKind::Text,
            )
            .with_help("Optional — shown after submit instead of the form's default."),
            // Text, not Url: only a path on this site is followed (open-
            // redirect guard), and a URL input refuses a bare `/thank-you`.
            BlockField::widget(
                "success_redirect_url",
                "Success redirect override",
                WidgetKind::Text,
            )
            .with_help("Optional — a page on this site to open after submit instead of showing a message, e.g. /thank-you."),
        ]
    }

    fn render(
        &self,
        value: &serde_json::Value,
        _ctx: &BlockRenderCtx,
    ) -> Result<String, BlockError> {
        let obj = value.as_object();
        let schema_val = obj.and_then(|o| o.get("_schema"));
        let form_id = obj
            .and_then(|o| o.get("_form_id"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let embed = obj
            .and_then(|o| o.get("_embed"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        match schema_val {
            Some(s) => {
                let mut form = crate::forms::schema::parse(s).unwrap_or_default();
                // This placement's own thanks message wins over the form's.
                if let Some(msg) = obj
                    .and_then(|o| o.get("success_message"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|m| !m.trim().is_empty())
                {
                    form.settings.success_message = msg.to_owned();
                }
                Ok(crate::forms::render::render_form_html(
                    &form, form_id, embed,
                ))
            }
            None => {
                let slug = obj
                    .and_then(|o| o.get("form"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                Ok(format!(
                    "<div class=\"rcms-form-missing\">Form <code>{}</code> not found.</div>",
                    crate::forms::render::html_escape(slug)
                ))
            }
        }
    }
}

crate::register_block!(FormBlock);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_placement_success_message_replaces_the_forms() {
        let tera = tera::Tera::default();
        let ctx = BlockRenderCtx::new(&tera);
        let schema = serde_json::json!({ "settings": { "success_message": "Form default" }, "pages": [] });
        let with = serde_json::json!({ "_schema": schema, "_form_id": 3, "_embed": "b1", "success_message": "Placement thanks" });
        let html = FormBlock.render(&with, &ctx).expect("render");
        assert!(html.contains(">Placement thanks</div>"), "{html}");
        let without = serde_json::json!({ "_schema": schema, "_form_id": 3, "_embed": "b1", "success_message": "  " });
        let html = FormBlock.render(&without, &ctx).expect("render");
        assert!(html.contains(">Form default</div>"), "{html}");
    }
}
