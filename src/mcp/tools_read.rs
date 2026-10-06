//! MCP read tools (#587): discover page types + blocks, search and read
//! pages (drafts included — the agent acts as an editor), list locales,
//! collections, media, and snippets, and enumerate a page's translatable
//! field paths. Every tool acts as the key's owner and enforces the same
//! view permissions the admin does.

use rustango::core::Column as _;
use rustango::mcp::{McpContext, McpError};
use rustango::openapi::{OpenApiSchema, Schema};
use rustango::sql::FetcherPool as _;
use serde_json::{json, Value};

use crate::page::Page;
use crate::page_type_model::PageType;
use crate::permissions::Action;

use super::{require_actor, require_codename, require_page_action};

/// Agent tools page larger than the public API (20 / 100).
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

fn clamp_limit(limit: Option<usize>) -> usize {
    crate::api::query::clamp_page_size(limit, DEFAULT_LIMIT, MAX_LIMIT)
}

// ------------------------------------------------------------- page types

#[derive(serde::Deserialize)]
struct EmptyInput {}

impl OpenApiSchema for EmptyInput {
    fn openapi_schema() -> Schema {
        Schema::object()
    }
}

fn block_field_summary(f: &crate::block::BlockField) -> Value {
    use crate::block::BlockField;
    match f {
        BlockField::Widget {
            name,
            label,
            widget,
            options,
            required,
            ..
        } => json!({
            "name": name, "label": label, "kind": "widget",
            "widget": widget, "required": required,
            "options": options.iter().map(|(v, l)| json!({"value": v, "label": l})).collect::<Vec<_>>(),
        }),
        BlockField::Stream { name, allowed, .. } => json!({
            "name": name, "kind": "stream", "allowed": allowed,
        }),
        BlockField::Repeat {
            name, item_type, ..
        } => json!({
            "name": name, "kind": "repeat", "item_type": item_type,
        }),
        other => json!({ "kind": "other", "debug": format!("{other:?}") }),
    }
}

rustango::register_mcp_tool!(
    "list_page_types",
    "List the CMS page types (code-defined and UI-defined) with their \
     creatability + placement rules, plus every registered stream-block \
     type and its field schema — everything needed to author a valid \
     `create_page` / `update_page` body.",
    EmptyInput,
    |ctx: McpContext, _input: EmptyInput| async move {
        let actor = require_actor(&ctx).await?;
        require_codename(&ctx.pool, &actor, "cms_page.view").await?;

        let rows: Vec<PageType> = PageType::objects()
            .fetch(&ctx.pool)
            .await
            .map_err(McpError::from)?;
        let mut types = Vec::with_capacity(rows.len());
        for pt in &rows {
            let handler = crate::page_type::find_handler(&pt.type_name);
            let builder_schema = crate::page_builder::model::published_for(
                &ctx.pool,
                pt.id.get().copied().unwrap_or_default(),
            )
            .await
            .ok()
            .flatten();
            // The type's editable extension fields (name + widget kind).
            // page_id 0 → the handler reports its schema without a row.
            let mut fields: Vec<Value> = Vec::new();
            if let Some(h) = &handler {
                if let Ok(widgets) = h.widgets(&ctx.pool, 0).await {
                    for w in widgets {
                        fields.push(json!({ "name": w.name, "widget": w.kind }));
                    }
                }
            }
            let has_body = fields.iter().any(|f| f["name"] == "body");
            types.push(json!({
                "id": pt.id.get().copied(),
                "app_label": pt.app_label,
                "type_name": pt.type_name,
                "verbose_name": pt.verbose_name,
                "is_creatable": pt.is_creatable,
                "has_code_handler": handler.is_some(),
                "has_builder_schema": builder_schema.is_some(),
                // Does this type accept a stream `body` argument? When false,
                // create_page/update_page reject a `body` payload.
                "has_body": has_body,
                "fields": fields,
                "allowed_parent_types": pt.allowed_parent_types,
                "allowed_child_types": pt.allowed_child_types,
            }));
        }
        let blocks: Vec<Value> = crate::block::registry::registered_blocks()
            .map(|b| {
                json!({
                    "type_name": b.type_name(),
                    "verbose_name": b.verbose_name(),
                    "fields": b.fields().iter().map(block_field_summary).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok::<_, McpError>(json!({ "types": types, "blocks": blocks }))
    },
);

// ------------------------------------------------------------- search

#[derive(serde::Deserialize)]
struct SearchPagesInput {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    page_type: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    parent_id: Option<i64>,
    #[serde(default)]
    limit: Option<usize>,
}

impl OpenApiSchema for SearchPagesInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property(
                "query",
                Schema::string().description("substring match on the title"),
            )
            .property(
                "page_type",
                Schema::string().description("filter by page-type type_name"),
            )
            .property(
                "status",
                Schema::string().description("draft | published | scheduled | archived"),
            )
            .property(
                "parent_id",
                Schema::integer().description("direct children of this page"),
            )
            .property("limit", Schema::integer())
    }
}

fn page_summary(p: &Page) -> Value {
    json!({
        "id": p.id.get().copied(),
        "title": p.title,
        "slug": p.slug,
        "url_path": p.url_path,
        "status": p.status,
        "page_type_id": p.page_type_id,
        "parent_id": p.parent_id,
        "published_at": p.published_at.map(|t| t.to_rfc3339()),
    })
}

rustango::register_mcp_tool!(
    "search_pages",
    "Search pages across every status (drafts included). Filter by title \
     substring, page type, status, or parent; returns id/title/slug/\
     url_path/status summaries.",
    SearchPagesInput,
    |ctx: McpContext, input: SearchPagesInput| async move {
        let actor = require_actor(&ctx).await?;
        require_codename(&ctx.pool, &actor, "cms_page.view").await?;

        let mut qs = Page::objects();
        if let Some(q) = input.query.as_deref().filter(|q| !q.is_empty()) {
            qs = qs.where_(Page::title.contains(q.to_owned()));
        }
        if let Some(status) = input.status.as_deref().filter(|s| !s.is_empty()) {
            qs = qs.where_(Page::status.eq(status.to_owned()));
        }
        if let Some(parent) = input.parent_id {
            qs = qs.where_(Page::parent_id.eq(parent));
        }
        if let Some(tn) = input.page_type.as_deref().filter(|s| !s.is_empty()) {
            let pt: Option<PageType> = PageType::objects()
                .where_(PageType::type_name.eq(tn.to_owned()))
                .first(&ctx.pool)
                .await
                .map_err(McpError::from)?;
            let Some(pt) = pt else {
                return Err(McpError::invalid_params(format!(
                    "unknown page_type `{tn}` — list_page_types shows the valid names"
                )));
            };
            qs = qs.where_(Page::page_type_id.eq(pt.id.get().copied().unwrap_or_default()));
        }
        // A LIMIT needs an order, or which rows come back is arbitrary (#657).
        let mut rows: Vec<Page> = qs
            .order_by(&[("path", false), ("id", false)])
            .limit(clamp_limit(input.limit) as i64)
            .fetch(&ctx.pool)
            .await
            .map_err(McpError::from)?;
        // The codename opens the tool; each row still needs the per-page
        // View grant get_page checks, or the list discloses what the row
        // refuses (#759).
        if !actor.is_superuser {
            let ids: Vec<(i64, Option<i64>)> = rows
                .iter()
                .filter_map(|p| p.id.get().copied().map(|id| (id, p.parent_id)))
                .collect();
            let viewable = crate::permissions::viewable_page_ids(&ctx.pool, actor.id, &ids)
                .await
                .map_err(McpError::from)?;
            rows.retain(|p| p.id.get().is_some_and(|id| viewable.contains(id)));
        }
        Ok::<_, McpError>(json!({ "pages": rows.iter().map(page_summary).collect::<Vec<_>>() }))
    },
);

// ------------------------------------------------------------- get_page

#[derive(serde::Deserialize)]
struct GetPageInput {
    page_id: i64,
    #[serde(default)]
    include_body: Option<bool>,
}

impl OpenApiSchema for GetPageInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property("page_id", Schema::integer())
            .property(
                "include_body",
                Schema::boolean()
                    .description("include extension fields + builder body (default true)"),
            )
            .required(["page_id"])
    }
}

rustango::register_mcp_tool!(
    "get_page",
    "Read one page in full: canonical fields, SEO, and (by default) the \
     page type's extension fields — including stream bodies as JSON — \
     plus the UI-defined builder body when the type has one.",
    GetPageInput,
    |ctx: McpContext, input: GetPageInput| async move {
        let actor = require_actor(&ctx).await?;
        require_page_action(&ctx.pool, &actor, input.page_id, Action::View).await?;

        let page: Page = Page::objects()
            .where_(Page::id.eq(input.page_id))
            .first(&ctx.pool)
            .await
            .map_err(McpError::from)?
            .ok_or_else(|| McpError::invalid_params(format!("page {} not found", input.page_id)))?;

        let pt: Option<PageType> = PageType::objects()
            .where_(PageType::id.eq(page.page_type_id))
            .first(&ctx.pool)
            .await
            .map_err(McpError::from)?;

        let mut out = page_summary(&page);
        let obj = out.as_object_mut().expect("summary is an object");
        obj.insert("seo_title".into(), json!(page.seo_title));
        obj.insert("seo_description".into(), json!(page.seo_description));
        obj.insert("show_in_menus".into(), json!(page.show_in_menus));
        obj.insert(
            "page_type".into(),
            json!(pt.as_ref().map(|t| t.type_name.clone())),
        );

        if input.include_body.unwrap_or(true) {
            if let Some(pt) = &pt {
                if let Some(handler) = crate::page_type::find_handler(&pt.type_name) {
                    let fields = handler
                        .load_extension(&ctx.pool, input.page_id)
                        .await
                        .unwrap_or(Value::Null);
                    obj.insert("fields".into(), fields);
                }
            }
            if let Ok(Some(data)) =
                crate::page_builder::model::data_for_page(&ctx.pool, input.page_id).await
            {
                obj.insert("builder".into(), data.data);
            }
        }
        Ok::<_, McpError>(out)
    },
);

// ------------------------------------------------------------- locales

rustango::register_mcp_tool!(
    "list_locales",
    "List the tenant's active locales (BCP-47 code, display name, default \
     flag). Translations can target any non-default locale.",
    EmptyInput,
    |ctx: McpContext, _input: EmptyInput| async move {
        let actor = require_actor(&ctx).await?;
        require_codename(&ctx.pool, &actor, "cms_page.view").await?;
        let rows: Vec<crate::locale::Locale> = crate::locale::Locale::objects()
            .where_(crate::locale::Locale::active.eq(true))
            .fetch(&ctx.pool)
            .await
            .map_err(McpError::from)?;
        let locales: Vec<Value> = rows
            .iter()
            .map(|l| json!({ "code": l.code, "name": l.name, "is_default": l.is_default }))
            .collect();
        Ok::<_, McpError>(json!({ "locales": locales }))
    },
);

// ------------------------------------------------------------- media

#[derive(serde::Deserialize)]
struct ListMediaInput {
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    collection_id: Option<i64>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

impl OpenApiSchema for ListMediaInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property(
                "search",
                Schema::string().description("match on title / filename / alt text"),
            )
            .property(
                "collection_id",
                Schema::integer().description("0 = uncategorized; omitted = all"),
            )
            .property(
                "kind",
                Schema::string().description("image (default) | document | other | any"),
            )
            .property("limit", Schema::integer())
            .property("offset", Schema::integer())
    }
}

rustango::register_mcp_tool!(
    "list_media",
    "List media (images by default; kind=document/any for the rest) with \
     ready-to-use thumbnail and preview URLs, filterable by collection \
     and search text. Also returns the collection tree.",
    ListMediaInput,
    |ctx: McpContext, input: ListMediaInput| async move {
        let actor = require_actor(&ctx).await?;
        require_codename(&ctx.pool, &actor, "cms_media.view").await?;
        let q = crate::admin::MediaPickerQuery {
            q: input.search,
            collection: input.collection_id,
            kind: input.kind,
            ids: None,
            limit: Some(clamp_limit(input.limit)),
            offset: input.offset,
        };
        crate::admin::media_picker_payload(&ctx.pool, &q)
            .await
            .map_err(|e| McpError::internal(e.to_string()))
    },
);

rustango::register_mcp_tool!(
    "list_collections",
    "List the media collection tree (id, name, parent).",
    EmptyInput,
    |ctx: McpContext, _input: EmptyInput| async move {
        let actor = require_actor(&ctx).await?;
        require_codename(&ctx.pool, &actor, "cms_media.view").await?;
        let rows: Vec<crate::media::MediaCollection> = crate::media::MediaCollection::objects()
            .fetch(&ctx.pool)
            .await
            .map_err(McpError::from)?;
        let collections: Vec<Value> = rows
            .iter()
            .map(|c| json!({ "id": c.id.get().copied(), "name": c.name, "parent_id": c.parent_id }))
            .collect();
        Ok::<_, McpError>(json!({ "collections": collections }))
    },
);

// ------------------------------------------------------------- snippets

#[derive(serde::Deserialize)]
struct ListSnippetsInput {
    #[serde(default)]
    type_name: Option<String>,
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

impl OpenApiSchema for ListSnippetsInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property(
                "type_name",
                Schema::string().description("library type (e.g. `form`, `branding`)"),
            )
            .property("search", Schema::string().description("title substring"))
            .property("limit", Schema::integer())
    }
}

rustango::register_mcp_tool!(
    "list_snippets",
    "List library snippets (id, type, slug, title, data), optionally \
     filtered by library type and title text.",
    ListSnippetsInput,
    |ctx: McpContext, input: ListSnippetsInput| async move {
        let actor = require_actor(&ctx).await?;
        require_codename(&ctx.pool, &actor, "cms_library.view").await?;
        let mut qs = crate::snippet::Snippet::objects();
        if let Some(t) = input.type_name.as_deref().filter(|s| !s.is_empty()) {
            qs = qs.where_(crate::snippet::Snippet::type_name.eq(t.to_owned()));
        }
        if let Some(s) = input.search.as_deref().filter(|s| !s.is_empty()) {
            qs = qs.where_(crate::snippet::Snippet::title.contains(s.to_owned()));
        }
        let rows: Vec<crate::snippet::Snippet> = qs
            .order_by(&[("title", false), ("id", false)])
            .limit(clamp_limit(input.limit) as i64)
            .fetch(&ctx.pool)
            .await
            .map_err(McpError::from)?;
        // The type's own row filter, as the admin library list applies it
        // (#759).
        let snippets: Vec<Value> = rows
            .iter()
            .filter(|s| crate::library::find_handler(&s.type_name).map_or(true, |h| h.can_view(s)))
            .map(|s| {
                json!({
                    "id": s.id.get().copied(),
                    "type_name": s.type_name,
                    "slug": s.slug,
                    "folder_path": s.folder_path,
                    "title": s.title,
                    "body_markdown": s.body_markdown,
                    "data": s.data,
                })
            })
            .collect();
        Ok::<_, McpError>(json!({ "snippets": snippets }))
    },
);

// ------------------------------------------- translatable field discovery

#[derive(serde::Deserialize)]
struct ListTranslatableInput {
    page_id: i64,
    #[serde(default)]
    locale: Option<String>,
}

impl OpenApiSchema for ListTranslatableInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property("page_id", Schema::integer())
            .property(
                "locale",
                Schema::string().description(
                    "also report each field's existing override in this locale \
                     (`translated_text` + `translated`), so a run can be \
                     audited, resumed or diffed instead of blindly rewritten",
                ),
            )
            .required(["page_id"])
    }
}

/// Whether an extension field holds prose a translator should see.
///
/// Deliberately a short allow-list rather than "everything that isn't a
/// number": a chooser, id, date, colour or URL field is read structurally
/// by the site, and `upsert_translations` writes whatever it is given with
/// no type check — so listing one here would invite an agent to replace a
/// media id with a translated caption and break the page.
fn is_translatable_text(kind: &crate::widget::WidgetKind) -> bool {
    use crate::widget::WidgetKind as K;
    matches!(kind, K::Text | K::Textarea | K::Markdown | K::RichText)
}

fn leaf_json(l: &crate::block::translate::TranslatableLeaf) -> Value {
    json!({
        "field_path": l.path,
        "widget_kind": l.widget_kind,
        "canonical_text": l.canonical_text,
        "block_type": l.block_type,
        "block_label": l.block_label,
        "field_label": l.field_label,
    })
}

rustango::register_mcp_tool!(
    "list_translatable_fields",
    "Enumerate a page's translatable field paths with their canonical \
     (default-locale) text — the exact `field_path` keys \
     `upsert_translations` accepts: canonical columns (title, seo_*), \
     text-shaped extension fields, stream-block leaves (dotted \
     block-UUID paths), and builder-body leaves. Pass `locale` to see \
     what is already translated there. `max_length`, where present, is \
     the canonical column's limit — the override is not truncated for \
     you.",
    ListTranslatableInput,
    |ctx: McpContext, input: ListTranslatableInput| async move {
        let actor = require_actor(&ctx).await?;
        require_page_action(&ctx.pool, &actor, input.page_id, Action::View).await?;

        let page: Page = Page::objects()
            .where_(Page::id.eq(input.page_id))
            .first(&ctx.pool)
            .await
            .map_err(McpError::from)?
            .ok_or_else(|| McpError::invalid_params(format!("page {} not found", input.page_id)))?;

        let mut fields: Vec<Value> = vec![
            json!({ "field_path": "title", "widget_kind": "text", "canonical_text": page.title, "max_length": 255 }),
            json!({ "field_path": "seo_title", "widget_kind": "text", "canonical_text": page.seo_title, "max_length": 70 }),
            json!({ "field_path": "seo_description", "widget_kind": "textarea", "canonical_text": page.seo_description, "max_length": 200 }),
        ];

        // Text-shaped extension fields (excerpt, description, role, …).
        //
        // These were missing, and their absence actively misled: the render
        // overlay localizes ANY scalar field an object already exposes
        // (`translation::overlay_translations`, applied to `extension` in
        // `render.rs`), so they have always been translatable and
        // `upsert_translations` has always stored them. Reporting only the
        // canonical columns told agents a page type's whole body was
        // untranslatable and that fixing it needed a code change — there is
        // no "mark translatable" mechanism to add.
        //
        // Filtered to text-shaped widgets on purpose: an id, number, date
        // or chooser value is not prose, and translating one would write
        // rubbish into a field the site reads structurally.
        if let Some(pt) = PageType::objects()
            .where_(PageType::id.eq(page.page_type_id))
            .first(&ctx.pool)
            .await
            .map_err(McpError::from)?
        {
            if let Some(handler) = crate::page_type::find_handler(&pt.type_name) {
                let widgets = handler
                    .widgets(&ctx.pool, input.page_id)
                    .await
                    .unwrap_or_default();
                let ext = handler
                    .load_extension(&ctx.pool, input.page_id)
                    .await
                    .unwrap_or(Value::Null);
                for w in &widgets {
                    if !is_translatable_text(&w.kind) {
                        continue;
                    }
                    let canonical = ext
                        .get(&w.name)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let mut entry = json!({
                        "field_path": w.name,
                        "widget_kind": w.kind.as_tag(),
                        "canonical_text": canonical,
                    });
                    if let (Some(obj), Some(max)) = (entry.as_object_mut(), w.max_length) {
                        obj.insert("max_length".to_owned(), json!(max));
                    }
                    fields.push(entry);
                }
            }
        }

        // Stream leaves from every array-shaped extension field.
        let pt: Option<PageType> = PageType::objects()
            .where_(PageType::id.eq(page.page_type_id))
            .first(&ctx.pool)
            .await
            .map_err(McpError::from)?;
        if let Some(pt) = &pt {
            if let Some(handler) = crate::page_type::find_handler(&pt.type_name) {
                if let Ok(ext) = handler.load_extension(&ctx.pool, input.page_id).await {
                    if let Some(obj) = ext.as_object() {
                        for (name, value) in obj {
                            // A StreamField comes back as a JSON *string* (the
                            // serialized envelope), not a parsed array — parse
                            // it before walking for leaves, else nested block
                            // content is invisible to translation.
                            let parsed;
                            let stream = if value.is_array() {
                                Some(value)
                            } else if let Some(s) = value.as_str() {
                                parsed = serde_json::from_str::<Value>(s).ok();
                                parsed.as_ref().filter(|v| v.is_array())
                            } else {
                                None
                            };
                            if let Some(stream) = stream {
                                for leaf in crate::block::translate::collect_translatable_leaves(
                                    name, stream,
                                ) {
                                    fields.push(leaf_json(&leaf));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Builder-body leaves via the compiled published schema.
        if let (Some(pt), Ok(Some(data))) = (
            &pt,
            crate::page_builder::model::data_for_page(&ctx.pool, input.page_id).await,
        ) {
            if let Ok(Some(schema_row)) = crate::page_builder::model::published_for(
                &ctx.pool,
                pt.id.get().copied().unwrap_or_default(),
            )
            .await
            {
                if let Ok(doc) = crate::page_builder::parse_schema(&schema_row.document) {
                    let components = crate::page_builder::model::component_map(&ctx.pool)
                        .await
                        .unwrap_or_default();
                    let compiled = crate::page_builder::compile(
                        &doc,
                        &components,
                        schema_row.version.max(1) as u32,
                    );
                    for leaf in crate::page_builder::values::builder_translatable_leaves(
                        &compiled, &data.data,
                    ) {
                        fields.push(leaf_json(&leaf));
                    }
                }
            }
        }

        // With `locale`, fold in what is already stored so a caller can
        // resume a half-finished pass, diff a retranslation, or see what an
        // empty-value delete would remove. Without it there was no read
        // path for translations at all — only the canonical text.
        let mut requested_locale = Value::Null;
        if let Some(code) = input.locale.as_deref() {
            let code = crate::locale::validate_code(code).map_err(McpError::invalid_params)?;
            let locale: crate::locale::Locale = crate::locale::Locale::objects()
                .where_(crate::locale::Locale::code.eq(code.clone()))
                .where_(crate::locale::Locale::active.eq(true))
                .first(&ctx.pool)
                .await
                .map_err(McpError::from)?
                .ok_or_else(|| {
                    McpError::invalid_params(format!(
                        "unknown or inactive locale `{code}` — see list_locales"
                    ))
                })?;
            let existing = crate::translation::fetch_for(
                &ctx.pool,
                input.page_id,
                locale.id.get().copied().unwrap_or_default(),
            )
            .await
            .map_err(McpError::from)?;
            for f in &mut fields {
                let path = f
                    .get("field_path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if let Some(obj) = f.as_object_mut() {
                    let hit = existing.get(&path);
                    obj.insert("translated".to_owned(), json!(hit.is_some()));
                    obj.insert(
                        "translated_text".to_owned(),
                        hit.map_or(Value::Null, |t| json!(t)),
                    );
                }
            }
            requested_locale = json!(code);
        }

        Ok::<_, McpError>(json!({ "fields": fields, "locale": requested_locale }))
    },
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::WidgetKind as K;

    /// Pins the allow-list. The failure this guards is silent: adding a
    /// chooser or numeric kind here would make `list_translatable_fields`
    /// advertise a media id or a date as prose, and `upsert_translations`
    /// would store the translated string over it without complaint.
    #[test]
    fn only_prose_widgets_are_offered_for_translation() {
        for k in [K::Text, K::Textarea, K::Markdown, K::RichText] {
            assert!(is_translatable_text(&k), "{k:?} holds prose");
        }
        for k in [
            // read structurally by the site — a translation would corrupt them
            K::MediaPicker,
            K::DocumentChooser,
            K::PageChooser,
            K::SnippetChooser,
            K::ModelChooser,
            K::Stream,
            // typed values, not prose
            K::Number,
            K::Integer,
            K::Float,
            K::Boolean,
            K::Date,
            K::Datetime,
            K::Color,
            K::Url,
            K::Email,
            // never user-facing text
            K::Hidden,
            K::Password,
            K::File,
        ] {
            assert!(!is_translatable_text(&k), "{k:?} must not be translatable");
        }
    }
}
