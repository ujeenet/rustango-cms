//! Which media-library pictures and files are in use, and where.
//!
//! A picture can be referenced from a page's code extension row, from its
//! page-builder data (fields, blocks, and the items of list blocks such as
//! Cards, Slider and Gallery), from a library element, or from a site
//! setting (Branding's logo and icon, a theme's footer badge). Each holds
//! the media id under a field whose name the schema decides (`cover`,
//! `image`, `logo`, …), so this first learns those names from every
//! schema that can hold a picture ([`media_fields`]) and then scans the
//! stored values for them.
//!
//! The **Unused media** report and a picture's **Used on** panel both read
//! from here, so they always agree.

use std::collections::{BTreeSet, HashSet};

use rustango::sql::{ExecError, FetcherPool as _, Pool};
use serde_json::Value;

use crate::block::BlockField;
use crate::page::Page;
use crate::page_builder::model::{PageBuilderData, PageTypeSchema};
use crate::page_type_model::PageType;
use crate::widget::WidgetKind;

/// Where a picture is used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Use {
    /// A page: id, title, URL path, status.
    Page { id: i64, title: String, url_path: String, status: String },
    /// A library element: id, title.
    Library { id: i64, title: String },
    /// A site setting, by scope.
    Setting { scope: String },
}

/// Names of the fields that hold a media id, across every schema that can
/// hold one: built-in and registered blocks, page-builder page types and
/// components, and site-setting forms. Code page types keep the older
/// `media_id` / `*_media_id` convention, matched in [`is_media_field`].
pub async fn media_fields(pool: &Pool) -> Result<HashSet<String>, ExecError> {
    let mut names: HashSet<String> = HashSet::new();
    for block in crate::block::registered_blocks() {
        for field in block.fields() {
            if let BlockField::Widget { name, widget: WidgetKind::MediaPicker, .. } = field {
                names.insert(name);
            }
        }
    }
    for schema in PageTypeSchema::objects().fetch(pool).await? {
        schema_fields(&schema.document, &mut names);
    }
    for component in crate::page_builder::model::component_map(pool).await?.into_values() {
        if let Ok(children) = serde_json::to_value(&component.doc.children) {
            schema_fields(&serde_json::json!({ "nodes": children }), &mut names);
        }
    }
    for schema in crate::site_setting::registered_schemas() {
        for widget in (schema.fields)() {
            if widget.kind == WidgetKind::MediaPicker {
                names.insert(widget.name.clone());
            }
        }
    }
    for picture in [crate::branding::Picture::Logo, crate::branding::Picture::Favicon] {
        names.insert(picture.key().to_owned());
    }
    Ok(names)
}

/// Every `"widget": "mediapicker"` field key in a page-builder document,
/// at any depth (rows, groups, repeater items, flexible-zone groups).
fn schema_fields(value: &Value, names: &mut HashSet<String>) {
    match value {
        Value::Object(map) => {
            if map.get("widget").and_then(Value::as_str) == Some("mediapicker") {
                if let Some(key) = map.get("key").and_then(Value::as_str) {
                    names.insert(key.to_owned());
                }
            }
            map.values().for_each(|v| schema_fields(v, names));
        }
        Value::Array(items) => items.iter().for_each(|v| schema_fields(v, names)),
        _ => {}
    }
}

fn is_media_field(key: &str, names: &HashSet<String>) -> bool {
    names.contains(key) || key == "media_id" || key.ends_with("_media_id")
}

/// The media ids `value` references through `names` fields. A string that
/// holds JSON (stream bodies stored as text) is read too.
pub fn collect(value: &Value, names: &HashSet<String>, out: &mut BTreeSet<i64>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let id = match child {
                    Value::Number(n) if is_media_field(key, names) => n.as_i64(),
                    Value::String(s) if is_media_field(key, names) => s.trim().parse::<i64>().ok(),
                    _ => None,
                };
                match id {
                    Some(id) if id > 0 => {
                        out.insert(id);
                    }
                    _ => collect(child, names, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect(v, names, out)),
        Value::String(s) => {
            let t = s.trim_start();
            if t.starts_with('[') || t.starts_with('{') {
                if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                    collect(&parsed, names, out);
                }
            }
        }
        _ => {}
    }
}

/// Everything that references pictures, each with what it is.
async fn sources(pool: &Pool, names: &HashSet<String>) -> Result<Vec<(Use, BTreeSet<i64>)>, ExecError> {
    let mut out = Vec::new();
    let types: Vec<PageType> = PageType::objects().fetch(pool).await?;
    let builder: Vec<PageBuilderData> = PageBuilderData::objects().fetch(pool).await?;
    for page in Page::objects().fetch(pool).await? {
        let Some(id) = page.id.get().copied() else { continue };
        let mut ids = BTreeSet::new();
        if let Ok(v) = serde_json::to_value(&page) {
            collect(&v, names, &mut ids);
        }
        let handler = types
            .iter()
            .find(|t| t.id.get().copied() == Some(page.page_type_id))
            .and_then(|t| crate::page_type::find_handler(&t.type_name));
        if let Some(handler) = handler {
            if let Ok(ext) = handler.load_extension(pool, id).await {
                collect(&ext, names, &mut ids);
            }
        }
        for row in builder.iter().filter(|b| b.page_id == id) {
            collect(&row.data, names, &mut ids);
        }
        if !ids.is_empty() {
            out.push((Use::Page { id, title: page.title, url_path: page.url_path, status: page.status }, ids));
        }
    }
    for snippet in crate::snippet::Snippet::objects().fetch(pool).await? {
        let mut ids = BTreeSet::new();
        collect(&snippet.data, names, &mut ids);
        if let (Some(id), false) = (snippet.id.get().copied(), ids.is_empty()) {
            out.push((Use::Library { id, title: snippet.title }, ids));
        }
    }
    for setting in crate::site_setting::list_all(pool).await? {
        let mut ids = BTreeSet::new();
        collect(&setting.value_json, names, &mut ids);
        if !ids.is_empty() {
            out.push((Use::Setting { scope: setting.scope }, ids));
        }
    }
    Ok(out)
}

/// Ids of every picture or file in use anywhere.
///
/// # Errors
/// Database errors.
pub async fn used(pool: &Pool) -> Result<BTreeSet<i64>, ExecError> {
    let names = media_fields(pool).await?;
    Ok(sources(pool, &names).await?.into_iter().flat_map(|(_, ids)| ids).collect())
}

/// Where one picture or file is used.
///
/// # Errors
/// Database errors.
pub async fn uses_of(pool: &Pool, media_id: i64) -> Result<Vec<Use>, ExecError> {
    let names = media_fields(pool).await?;
    Ok(sources(pool, &names)
        .await?
        .into_iter()
        .filter(|(_, ids)| ids.contains(&media_id))
        .map(|(u, _)| u)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_are_found_under_schema_named_fields_at_any_depth() {
        let names: HashSet<String> = ["cover", "image"].iter().map(|s| (*s).to_owned()).collect();
        let data = json!({
            "cover": "3",
            "title": "7",
            "body": [
                { "type": "hero", "value": { "image": "4", "heading": "x" } },
                { "type": "gallery", "value": { "images": [
                    { "type": "gallery_image", "value": { "image": 5 } }
                ] } }
            ],
            "legacy": "{\"hero_media_id\": 6}"
        });
        let mut out = BTreeSet::new();
        collect(&data, &names, &mut out);
        assert_eq!(out.into_iter().collect::<Vec<_>>(), vec![3, 4, 5, 6]);
    }

    #[test]
    fn page_builder_schemas_name_their_picture_fields() {
        let doc = json!({ "nodes": [
            { "kind": "field", "key": "cover", "widget": "mediapicker" },
            { "kind": "field", "key": "intro", "widget": "textarea" },
            { "kind": "flex", "key": "body", "groups": [
                { "key": "hero", "children": [
                    { "kind": "row", "children": [ { "kind": "field", "key": "image", "widget": "mediapicker" } ] }
                ] }
            ] }
        ] });
        let mut names = HashSet::new();
        schema_fields(&doc, &mut names);
        assert_eq!(names, ["cover", "image"].iter().map(|s| (*s).to_owned()).collect());
    }
}
