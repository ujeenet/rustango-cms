//! A page's stored state as the edit form posts it.
//!
//! `apply_page_edit` rebuilds the extension row, every inline panel and
//! the page-builder body from the form it is given. A caller that saves
//! through it without posting the whole page — MCP `update_page` /
//! `publish_page` / `insert_image`, page clone — must start from
//! [`prefill`], or whatever it didn't mention is written back empty.

use std::collections::HashMap;

use rustango::core::Column as _;

use crate::mcp::value_to_form_string;
use crate::page::Page;
use crate::page_type_model::PageType;

/// The core-row form keys. Checkbox semantics mirror the HTML form:
/// flag set → `"on"`, unset → key absent.
pub(crate) fn canonical(page: &Page) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("title".into(), page.title.clone());
    m.insert("slug".into(), page.slug.clone());
    m.insert("status".into(), page.status.clone());
    m.insert("seo_title".into(), page.seo_title.clone());
    m.insert("seo_description".into(), page.seo_description.clone());
    m.insert("template_override".into(), page.template_override.clone());
    if page.robots_index {
        m.insert("robots_index".into(), "on".into());
    }
    m.insert(
        "sitemap_priority".into(),
        format!("{}", page.sitemap_priority),
    );
    if page.show_in_menus {
        m.insert("show_in_menus".into(), "on".into());
    }
    if let Some(t) = page.theme_id {
        m.insert("theme_id".into(), t.to_string());
    }
    if let Some(t) = page.go_live_at {
        m.insert("go_live_at".into(), t.format("%Y-%m-%dT%H:%M").to_string());
    }
    if let Some(t) = page.expire_at {
        m.insert("expire_at".into(), t.format("%Y-%m-%dT%H:%M").to_string());
    }
    m.insert("og_title".into(), page.og_title.clone());
    m.insert("og_description".into(), page.og_description.clone());
    if let Some(mid) = page.og_image_media_id {
        m.insert("og_image_media_id".into(), mid.to_string());
    }
    m.insert("twitter_card".into(), page.twitter_card.clone());
    m
}

/// Everything stored for `page`: [`canonical`] plus the tags, the
/// extension fields, the inline-panel rows
/// (`inline__<panel>__<idx>__<field>`) and the page-builder values.
pub(crate) async fn prefill(pool: &rustango::sql::Pool, page: &Page) -> HashMap<String, String> {
    let mut form = canonical(page);
    let page_id = page.id.get().copied().unwrap_or_default();

    if let Ok(tags) = crate::page_tag::tags_for_page(pool, page_id).await {
        if !tags.is_empty() {
            let names: Vec<String> = tags.into_iter().map(|t| t.name).collect();
            form.insert("tags".into(), names.join(","));
        }
    }

    let handler = PageType::objects()
        .where_(PageType::id.eq(page.page_type_id))
        .first(pool)
        .await
        .ok()
        .flatten()
        .and_then(|pt| crate::page_type::find_handler(&pt.type_name));
    if let Some(handler) = handler {
        if let Ok(ext) = handler.load_extension(pool, page_id).await {
            if let Some(obj) = ext.as_object() {
                for (k, v) in obj {
                    if let Some(s) = value_to_form_string(v) {
                        form.insert(k.clone(), s);
                    }
                }
            }
        }
        for spec in handler.inline_panels() {
            let Ok(rows) = handler.load_inline_panel(pool, page_id, &spec.name).await else {
                continue;
            };
            for (idx, row) in rows.iter().enumerate() {
                for (field, v) in row {
                    if let Some(s) = value_to_form_string(v) {
                        form.insert(format!("inline__{}__{idx}__{field}", spec.name), s);
                    }
                }
            }
        }
    }

    if let Some((values, compiled)) =
        crate::page_builder::values::values_for(pool, page_id, page.page_type_id, None).await
    {
        crate::page_builder::values::prefill_form(&compiled, &values, &mut form);
    }
    form
}
