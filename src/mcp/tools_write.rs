//! MCP write tools (#587): create / update / publish pages, upload media,
//! upsert snippets, write translations. Every tool funnels into the SAME
//! write paths the admin handlers use (`apply_page_edit`, `store_media`,
//! `upsert_page_translations`, the snippet save + revision capture), acts
//! as the key's owner, and enforces the identical permission checks.

use crate::log_err::LogErr as _;
use std::collections::HashMap;

use rustango::core::Column as _;
use rustango::mcp::{McpContext, McpError};
use rustango::openapi::{OpenApiSchema, Schema};
use rustango::sql::FetcherPool as _;
use serde_json::{json, Map, Value};

use crate::page::{Page, PageStatus};
use crate::page_type_model::PageType;
use crate::permissions::Action;

use super::{
    mint_uuid4, require_actor, require_any_codename, require_codename, require_page_action,
    value_to_form_string,
    ToolActor, FORBIDDEN,
};

// ------------------------------------------------------------ shared bits

/// What a page type can actually store, resolved from its handler widgets
/// (extension fields, incl. a `body` stream) + any published builder
/// schema. Used to reject content an agent sends to a field the type
/// doesn't expose — otherwise the save path silently drops it and the
/// tool would misleadingly report success.
struct ContentSchema {
    field_names: std::collections::HashSet<String>,
    builder_keys: std::collections::HashSet<String>,
    has_builder: bool,
    /// Block type names this type's schema defines in the field builder
    /// (zone groups, component refs). They are real block types for a
    /// zone's stream but are not in the `register_block!` registry, so a
    /// registry-only check rejects the page's own blocks.
    dyn_block_names: std::collections::HashSet<String>,
}

impl ContentSchema {
    fn field_list(&self) -> String {
        let mut v: Vec<&str> = self.field_names.iter().map(String::as_str).collect();
        v.sort_unstable();
        if v.is_empty() {
            "(none — this type has no editable body/extension fields)".to_owned()
        } else {
            v.join(", ")
        }
    }
}

async fn content_schema(
    pool: &rustango::sql::Pool,
    page_type: &PageType,
    page_id: i64,
) -> ContentSchema {
    let mut field_names = std::collections::HashSet::new();
    if let Some(handler) = crate::page_type::find_handler(&page_type.type_name) {
        if let Ok(widgets) = handler.widgets(pool, page_id).await {
            for w in widgets {
                field_names.insert(w.name);
            }
        }
    }
    let mut builder_keys = std::collections::HashSet::new();
    let mut dyn_block_names = std::collections::HashSet::new();
    let mut has_builder = false;
    if let Ok(Some(row)) = crate::page_builder::model::published_for(
        pool,
        page_type.id.get().copied().unwrap_or_default(),
    )
    .await
    {
        if let Ok(doc) = crate::page_builder::parse_schema(&row.document) {
            has_builder = true;
            let components = crate::page_builder::model::component_map(pool)
                .await
                .unwrap_or_default();
            let compiled =
                crate::page_builder::compile(&doc, &components, row.version.max(1) as u32);
            for k in compiled.top_level_keys() {
                builder_keys.insert(k);
            }
            for name in compiled.dyn_blocks.keys() {
                dyn_block_names.insert(name.clone());
            }
        }
    }
    ContentSchema {
        field_names,
        builder_keys,
        has_builder,
        dyn_block_names,
    }
}

/// Reject a content payload aimed at fields the page type can't store, so
/// the tool fails loudly instead of the save path dropping it. `body` maps
/// to a `body` extension field OR a builder zone of the same name.
fn validate_content(
    schema: &ContentSchema,
    has_body: bool,
    fields: Option<&Map<String, Value>>,
    builder: Option<&Map<String, Value>>,
) -> Result<(), McpError> {
    if has_body && !schema.field_names.contains("body") && !schema.builder_keys.contains("body") {
        return Err(McpError::invalid_params(format!(
            "this page type has no `body` field, so block content can't be \
             stored on it. Editable fields: [{}]. Put the blocks on a page \
             type that declares a body stream (see list_page_types → \
             has_body), or use `fields`/`builder` keys this type exposes.",
            schema.field_list()
        )));
    }
    if let Some(fields) = fields {
        let unknown: Vec<&str> = fields
            .keys()
            .filter(|k| k.as_str() != "body" && !schema.field_names.contains(k.as_str()))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(McpError::invalid_params(format!(
                "unknown field(s) for this page type: [{}]. Editable fields: [{}].",
                unknown.join(", "),
                schema.field_list()
            )));
        }
    }
    if let Some(builder) = builder {
        if !builder.is_empty() {
            if !schema.has_builder {
                return Err(McpError::invalid_params(
                    "this page type has no published builder schema — `builder` \
                     content can't be stored on it.",
                ));
            }
            let mut known: Vec<&str> = schema.builder_keys.iter().map(String::as_str).collect();
            known.sort_unstable();
            let unknown: Vec<&str> = builder
                .keys()
                .filter(|k| !schema.builder_keys.contains(k.as_str()))
                .map(String::as_str)
                .collect();
            if !unknown.is_empty() {
                return Err(McpError::invalid_params(format!(
                    "unknown builder key(s): [{}]. This type's builder keys: [{}].",
                    unknown.join(", "),
                    known.join(", ")
                )));
            }
        }
    }
    Ok(())
}

/// Validate + normalize an agent-supplied stream body: every item must be
/// `{ "type": <registered block>, "value": {…} }`; missing `id`s are
/// minted here (stable UUIDs — translation paths key off them).
/// `dyn_names` carries the block types a page type's builder schema
/// defines. They are not in the `register_block!` registry — a zone group
/// invented in the field builder has no Rust type — so without them this
/// rejects the very blocks the page's own schema allows.
fn normalize_stream_body(
    body: &mut Value,
    dyn_names: &std::collections::HashSet<String>,
) -> Result<(), McpError> {
    let Some(items) = body.as_array_mut() else {
        return Err(McpError::invalid_params(
            "`body` must be an array of stream blocks: [{type, value, id?}]",
        ));
    };
    for (i, item) in items.iter_mut().enumerate() {
        let Some(obj) = item.as_object_mut() else {
            return Err(McpError::invalid_params(format!(
                "body[{i}] must be an object with `type` and `value`"
            )));
        };
        let type_name = obj
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                McpError::invalid_params(format!("body[{i}] is missing a string `type`"))
            })?;
        if crate::block::registry::find_block(&type_name).is_none()
            && !dyn_names.contains(&type_name)
        {
            return Err(McpError::invalid_params(format!(
                "body[{i}]: unknown block type `{type_name}` — list_page_types \
                 returns the registered block types and their fields"
            )));
        }
        if !obj.contains_key("value") {
            obj.insert("value".into(), json!({}));
        }
        let needs_id = obj
            .get("id")
            .and_then(Value::as_str)
            .map(str::is_empty)
            .unwrap_or(true);
        if needs_id {
            obj.insert("id".into(), json!(mint_uuid4()));
        }
        // Nested streams/repeats inside the value get the same treatment.
        if let Some(value) = obj.get_mut("value").and_then(Value::as_object_mut) {
            for (_k, v) in value.iter_mut() {
                if v.is_array() && looks_like_stream(v) {
                    normalize_stream_body(v, dyn_names)?;
                }
            }
        }
    }
    Ok(())
}

/// Heuristic: an array whose elements are objects carrying `type` is a
/// nested stream envelope (vs. a plain list value).
fn looks_like_stream(v: &Value) -> bool {
    v.as_array()
        .map(|a| {
            !a.is_empty()
                && a.iter()
                    .all(|e| e.as_object().is_some_and(|o| o.contains_key("type")))
        })
        .unwrap_or(false)
}

/// Overlay agent-supplied `fields` / `body` / `builder` onto a form map.
fn extend_with_content(
    form: &mut HashMap<String, String>,
    fields: Option<&Map<String, Value>>,
    body: Option<Value>,
    builder: Option<&Map<String, Value>>,
    dyn_names: &std::collections::HashSet<String>,
) -> Result<(), McpError> {
    if let Some(fields) = fields {
        for (k, v) in fields {
            if let Some(s) = value_to_form_string(v) {
                form.insert(k.clone(), s);
            } else {
                form.remove(k);
            }
        }
    }
    if let Some(mut body) = body {
        normalize_stream_body(&mut body, dyn_names)?;
        form.insert(
            "body".to_owned(),
            serde_json::to_string(&body).map_err(|e| McpError::internal(e.to_string()))?,
        );
    }
    if let Some(builder) = builder {
        for (k, v) in builder {
            match v {
                Value::Array(_) => {
                    let mut zone = v.clone();
                    normalize_stream_body(&mut zone, dyn_names)?;
                    form.insert(
                        format!("pb__{k}"),
                        serde_json::to_string(&zone)
                            .map_err(|e| McpError::internal(e.to_string()))?,
                    );
                }
                Value::Object(group) => {
                    for (f, fv) in group {
                        if let Some(s) = value_to_form_string(fv) {
                            form.insert(format!("pb__{k}__{f}"), s);
                        }
                    }
                }
                scalar => {
                    if let Some(s) = value_to_form_string(scalar) {
                        form.insert(format!("pb__{k}"), s);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Run the shared edit pipeline and shape refusals as tool errors.
async fn run_page_edit(
    ctx: &McpContext,
    actor: &ToolActor,
    page_id: i64,
    form: &HashMap<String, String>,
) -> Result<crate::admin::PageEditOutcome, McpError> {
    // MCP has no admin state of its own, so it purges through the
    // process-wide invalidator the admin router registers (#692) — an
    // agent's publish used to leave the old page cached for the full TTL.
    let invalidator: std::sync::Arc<dyn crate::cache_invalidate::PageCacheInvalidator> =
        crate::task_queue::default_invalidator()
            .unwrap_or_else(|| std::sync::Arc::new(crate::cache_invalidate::Noop));
    let outcome = crate::admin::apply_page_edit(
        &ctx.pool,
        &ctx.agent.tenant,
        &invalidator,
        None,
        "",
        false,
        page_id,
        form,
        Some(&actor.as_page_edit_actor()),
    )
    .await
    .map_err(|e| McpError::internal(e.to_string()))?;
    match outcome {
        Ok(o) => Ok(o),
        Err(crate::admin::PageEditRefusal::WorkflowReview { task_name }) => Err(McpError::new(
            FORBIDDEN,
            format!("page is under review on workflow step “{task_name}”"),
        )),
        Err(crate::admin::PageEditRefusal::LockedBy { holder }) => Err(McpError::new(
            FORBIDDEN,
            format!("page is locked by “{holder}”"),
        )),
        Err(crate::admin::PageEditRefusal::ArchivedUnpublish) => Err(McpError::invalid_params(
            "an archived page can't be unpublished in place — set it back to \
             published to unarchive first",
        )),
        Err(crate::admin::PageEditRefusal::PublishDenied) => Err(McpError::new(
            FORBIDDEN,
            format!("action `publish` on page {page_id} denied: this change would put it live"),
        )),
        Err(crate::admin::PageEditRefusal::ReviewRequired { workflow }) => Err(McpError::new(
            FORBIDDEN,
            format!("page {page_id} goes live through the review workflow “{workflow}”: save it as a draft and submit it for review"),
        )),
    }
}

/// [`page_result`] for a save through [`run_page_edit`]: a save held for
/// review says so, since the page it returns is the unchanged live one.
fn edit_result(outcome: &crate::admin::PageEditOutcome, action: &str) -> Value {
    let mut out = page_result(&outcome.page, action);
    if outcome.held_for_review {
        if let Some(obj) = out.as_object_mut() {
            obj.insert("held_for_review".to_owned(), json!(true));
            obj.insert(
                "note".to_owned(),
                json!("the page is live and its type needs re-approval on edit: the change waits for review, visitors see the live page until it is approved"),
            );
        }
    }
    out
}

fn page_result(page: &Page, action: &str) -> Value {
    json!({
        "ok": true,
        "action": action,
        "page": {
            "id": page.id.get().copied(),
            "title": page.title,
            "slug": page.slug,
            "url_path": page.url_path,
            "status": page.status,
        },
    })
}

/// Apply the two canonical flags an agent can set but the page tools
/// previously had no input for.
///
/// `show_in_menus` is what puts a page in `auto_menu`'s output: an agent
/// could create a page and fill it, but not make the site link to it,
/// which left every agent-built page needing a human to finish the job
/// in the admin. `template_override` is the same story for per-page
/// templates — an agent can write a template but not point a page at it.
///
/// Checkbox semantics: `true` → `"on"`, `false` → key removed, matching
/// what an HTML form posts and what `page_form::canonical` produces.
fn apply_page_flags(
    form: &mut HashMap<String, String>,
    show_in_menus: Option<bool>,
    template_override: Option<String>,
) {
    match show_in_menus {
        Some(true) => {
            form.insert("show_in_menus".into(), "on".into());
        }
        Some(false) => {
            form.remove("show_in_menus");
        }
        None => {}
    }
    if let Some(t) = template_override {
        form.insert("template_override".into(), t);
    }
}

// ------------------------------------------------------------ create_page

#[derive(serde::Deserialize)]
struct CreatePageInput {
    #[serde(default)]
    parent_id: Option<i64>,
    page_type: String,
    title: String,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    seo_title: Option<String>,
    #[serde(default)]
    seo_description: Option<String>,
    #[serde(default)]
    fields: Option<Map<String, Value>>,
    #[serde(default)]
    body: Option<Value>,
    #[serde(default)]
    builder: Option<Map<String, Value>>,
    #[serde(default)]
    show_in_menus: Option<bool>,
    #[serde(default)]
    template_override: Option<String>,
}

impl OpenApiSchema for CreatePageInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property(
                "parent_id",
                Schema::integer().description("parent page; omit to create a root page"),
            )
            .property(
                "page_type",
                Schema::string().description("page-type type_name (see list_page_types)"),
            )
            .property("title", Schema::string())
            .property(
                "slug",
                Schema::string().description("defaults to a slugified title, deduped"),
            )
            .property(
                "status",
                Schema::string().description("draft (default) | published"),
            )
            .property("seo_title", Schema::string())
            .property("seo_description", Schema::string())
            .property(
                "fields",
                Schema::object().description("page-type extension fields, keyed by field name"),
            )
            .property(
                "body",
                Schema::array_of(Schema::object()).description(
                    "stream body for the type's `body` field: [{type, value, id?}] — \
                     ids are minted server-side when omitted",
                ),
            )
            .property(
                "builder",
                Schema::object().description(
                    "UI-defined builder body, keyed by schema key: scalars, group \
                     objects, or zone arrays of {type, value} blocks",
                ),
            )
            .property(
                "show_in_menus",
                Schema::boolean().description("whether this page appears in navigation menus (`auto_menu`). Defaults to false, so a new page is not linked to until you say so"),
            )
            .property(
                "template_override",
                Schema::string().description("render through this template instead of the page type's. A name that does not resolve falls back to the type's template"),
            )
            .required(["page_type", "title"])
    }
}

rustango::register_mcp_tool!(
    "create_page",
    "Create a page (draft by default) under a parent — or at the root — \
     with optional extension fields, stream body, and builder body. \
     Placement rules (allowed parent/child types) are enforced; the slug \
     is derived from the title and deduped when omitted.",
    CreatePageInput,
    |ctx: McpContext, input: CreatePageInput| async move {
        let actor = require_actor(&ctx).await?;

        let pt: PageType = PageType::objects()
            .where_(PageType::type_name.eq(input.page_type.clone()))
            .first(&ctx.pool)
            .await
            .map_err(McpError::from)?
            .ok_or_else(|| {
                McpError::invalid_params(format!(
                    "unknown page_type `{}` — list_page_types shows the valid names",
                    input.page_type
                ))
            })?;
        let pt_id = pt.id.get().copied().unwrap_or_default();
        if !pt.is_creatable {
            return Err(McpError::invalid_params(format!(
                "page type `{}` is not creatable",
                pt.type_name
            )));
        }

        let status = match input.status.as_deref().unwrap_or("draft") {
            "draft" => PageStatus::Draft,
            "published" => PageStatus::Published,
            other => {
                return Err(McpError::invalid_params(format!(
                    "status must be `draft` or `published` at create time, got `{other}`"
                )))
            }
        };

        // Placement + permissions.
        let all_types: Vec<PageType> = PageType::objects()
            .fetch(&ctx.pool)
            .await
            .map_err(McpError::from)?;
        let parent: Option<Page> = match input.parent_id {
            None => {
                require_codename(&ctx.pool, &actor, "cms_page.add").await?;
                let allowed = crate::admin::allowed_root_types(&all_types);
                if !allowed.iter().any(|t| t.type_name == pt.type_name) {
                    return Err(McpError::invalid_params(format!(
                        "`{}` can't be created at the root",
                        pt.type_name
                    )));
                }
                None
            }
            Some(pid) => {
                require_page_action(&ctx.pool, &actor, pid, Action::Add).await?;
                let parent: Page = Page::objects()
                    .where_(Page::id.eq(pid))
                    .first(&ctx.pool)
                    .await
                    .map_err(McpError::from)?
                    .ok_or_else(|| {
                        McpError::invalid_params(format!("parent page {pid} not found"))
                    })?;
                let allowed = crate::admin::allowed_children_for(&parent, &all_types);
                if !allowed.iter().any(|t| t.type_name == pt.type_name) {
                    return Err(McpError::invalid_params(format!(
                        "`{}` is not an allowed child under page {pid} — allowed: {}",
                        pt.type_name,
                        allowed
                            .iter()
                            .map(|t| t.type_name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
                crate::tree_ops::validate_child_placement(&ctx.pool, parent.page_type_id, pt_id)
                    .await
                    .map_err(|e| McpError::invalid_params(e.to_string()))?;
                Some(parent)
            }
        };
        if status == PageStatus::Published {
            match input.parent_id {
                Some(pid) => require_page_action(&ctx.pool, &actor, pid, Action::Publish).await?,
                None => require_codename(&ctx.pool, &actor, "cms_page.publish").await?,
            }
        }

        // Reject content aimed at fields this type can't store BEFORE
        // creating the row, so a bad payload doesn't leave an orphan draft.
        // Kept for the write below, which needs this type's UI-defined
        // block names to accept the blocks its own schema allows.
        let mut dyn_names = std::collections::HashSet::new();
        if input.fields.is_some() || input.body.is_some() || input.builder.is_some() {
            // page_id 0 → the handler reports its field schema without a row.
            let schema = content_schema(&ctx.pool, &pt, 0).await;
            validate_content(
                &schema,
                input.body.is_some(),
                input.fields.as_ref(),
                input.builder.as_ref(),
            )?;
            dyn_names = schema.dyn_block_names;
        }

        // Slug: explicit, or slugified title — deduped under the parent.
        let raw_slug = match input.slug.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => crate::admin::slugify(s),
            None => crate::admin::slugify(&input.title),
        };
        let slug = crate::admin::dedup_slug_under_parent(&ctx.pool, input.parent_id, &raw_slug)
            .await
            .map_err(|e| McpError::internal(e.to_string()))?;

        // With content to apply, the row starts as a draft and takes the
        // requested status in the same edit that writes the content (#658):
        // created published, a failed content write left a live page with
        // no body.
        let has_content = input.fields.is_some()
            || input.body.is_some()
            || input.builder.is_some()
            || input.show_in_menus.is_some()
            || input.template_override.is_some();
        let create_status = if has_content { PageStatus::Draft } else { status };
        let new = crate::tree_ops::NewPage::new(pt_id, &input.title, &slug)
            .with_status(create_status)
            .with_seo(
                input.seo_title.as_deref().unwrap_or(""),
                input.seo_description.as_deref().unwrap_or(""),
            );
        let page = match &parent {
            None => Page::create_root_pool(&ctx.pool, new).await,
            Some(p) => Page::create_child_pool(&ctx.pool, p, new).await,
        }
        .map_err(|e| McpError::internal(format!("create: {e}")))?;
        let page_id = page.id.get().copied().unwrap_or_default();

        crate::page_log::record_or_warn(
            &ctx.pool,
            page_id,
            crate::page_log::ACTION_EDIT,
            Some(actor.id),
            format!("Created “{}” via MCP.", page.title),
            Some(json!({ "source": "mcp", "agent_id": ctx.agent.agent_id })),
        )
        .await;

        // Content lands through the shared edit pipeline so extension /
        // builder persistence, revision capture, hooks, and publish
        // stamping all behave exactly like an admin save.
        if has_content {
            let mut form = crate::page_form::canonical(&page);
            form.insert("status".to_owned(), status.as_str().to_owned());
            apply_page_flags(&mut form, input.show_in_menus, input.template_override);
            extend_with_content(
                &mut form,
                input.fields.as_ref(),
                input.body,
                input.builder.as_ref(),
                &dyn_names,
            )?;
            let outcome = run_page_edit(&ctx, &actor, page_id, &form).await?;
            return Ok::<_, McpError>(page_result(&outcome.page, "created"));
        }
        Ok::<_, McpError>(page_result(&page, "created"))
    },
);

// ------------------------------------------------------------ update_page

#[derive(serde::Deserialize)]
struct UpdatePageInput {
    page_id: i64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    seo_title: Option<String>,
    #[serde(default)]
    seo_description: Option<String>,
    #[serde(default)]
    fields: Option<Map<String, Value>>,
    #[serde(default)]
    body: Option<Value>,
    #[serde(default)]
    builder: Option<Map<String, Value>>,
    #[serde(default)]
    show_in_menus: Option<bool>,
    #[serde(default)]
    template_override: Option<String>,
}

impl OpenApiSchema for UpdatePageInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property("page_id", Schema::integer())
            .property("title", Schema::string())
            .property("slug", Schema::string())
            .property(
                "status",
                Schema::string().description("draft | published | archived"),
            )
            .property("seo_title", Schema::string())
            .property("seo_description", Schema::string())
            .property(
                "fields",
                Schema::object()
                    .description("extension fields to change; unmentioned fields are preserved"),
            )
            .property(
                "body",
                Schema::array_of(Schema::object())
                    .description("full replacement stream body [{type, value, id?}]"),
            )
            .property(
                "builder",
                Schema::object().description("builder-body keys to change"),
            )
            .property(
                "show_in_menus",
                Schema::boolean().description(
                    "whether this page appears in navigation menus (`auto_menu`)",
                ),
            )
            .property(
                "template_override",
                Schema::string().description(
                    "render through this template instead of the page type's; \
                     empty string restores the type's template",
                ),
            )
            .required(["page_id"])
    }
}

rustango::register_mcp_tool!(
    "update_page",
    "Update a page: canonical fields (title/slug/status/SEO), extension \
     fields, the stream body, and/or the builder body. Partial — \
     unmentioned fields are preserved. Runs the full admin save pipeline \
     (revision, hooks, redirects on slug moves, publish stamping).",
    UpdatePageInput,
    |ctx: McpContext, input: UpdatePageInput| async move {
        let actor = require_actor(&ctx).await?;
        require_page_action(&ctx.pool, &actor, input.page_id, Action::Edit).await?;

        let page: Page = Page::objects()
            .where_(Page::id.eq(input.page_id))
            .first(&ctx.pool)
            .await
            .map_err(McpError::from)?
            .ok_or_else(|| McpError::invalid_params(format!("page {} not found", input.page_id)))?;

        if let Some(status) = input.status.as_deref() {
            if !matches!(status, "draft" | "published" | "scheduled" | "archived") {
                return Err(McpError::invalid_params(format!(
                    "unknown status `{status}`"
                )));
            }
            // Going live, by any status, is checked in `apply_page_edit`
            // (#761).
        }

        // Reject content the page type can't store rather than dropping it.
        // `dyn_names` outlives the check: the write below needs this type's
        // UI-defined block names to accept the blocks its own schema allows.
        let mut dyn_names = std::collections::HashSet::new();
        if input.fields.is_some() || input.body.is_some() || input.builder.is_some() {
            let pt: Option<PageType> = PageType::objects()
                .where_(PageType::id.eq(page.page_type_id))
                .first(&ctx.pool)
                .await
                .map_err(McpError::from)?;
            if let Some(pt) = pt {
                let schema = content_schema(&ctx.pool, &pt, input.page_id).await;
                validate_content(
                    &schema,
                    input.body.is_some(),
                    input.fields.as_ref(),
                    input.builder.as_ref(),
                )?;
                dyn_names = schema.dyn_block_names;
            }
        }

        let mut form = crate::page_form::prefill(&ctx.pool, &page).await;
        if let Some(t) = input.title {
            form.insert("title".into(), t);
        }
        if let Some(s) = input.slug {
            form.insert("slug".into(), crate::admin::slugify(&s));
        }
        if let Some(s) = input.status {
            form.insert("status".into(), s);
        }
        if let Some(s) = input.seo_title {
            form.insert("seo_title".into(), s);
        }
        if let Some(s) = input.seo_description {
            form.insert("seo_description".into(), s);
        }
        apply_page_flags(&mut form, input.show_in_menus, input.template_override);
        extend_with_content(
            &mut form,
            input.fields.as_ref(),
            input.body,
            input.builder.as_ref(),
            &dyn_names,
        )?;

        let outcome = run_page_edit(&ctx, &actor, input.page_id, &form).await?;
        Ok::<_, McpError>(edit_result(&outcome, "updated"))
    },
);

// ----------------------------------------------------------- publish_page

#[derive(serde::Deserialize)]
struct PublishPageInput {
    page_id: i64,
}

impl OpenApiSchema for PublishPageInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property("page_id", Schema::integer())
            .required(["page_id"])
    }
}

rustango::register_mcp_tool!(
    "publish_page",
    "Publish a page (status → published, timestamps stamped, publish \
     hooks + page log fired). Requires the publish permission on the page.",
    PublishPageInput,
    |ctx: McpContext, input: PublishPageInput| async move {
        let actor = require_actor(&ctx).await?;
        require_page_action(&ctx.pool, &actor, input.page_id, Action::Publish).await?;

        let page: Page = Page::objects()
            .where_(Page::id.eq(input.page_id))
            .first(&ctx.pool)
            .await
            .map_err(McpError::from)?
            .ok_or_else(|| McpError::invalid_params(format!("page {} not found", input.page_id)))?;

        let mut form = crate::page_form::prefill(&ctx.pool, &page).await;
        form.insert("status".into(), "published".into());
        let outcome = run_page_edit(&ctx, &actor, input.page_id, &form).await?;
        Ok::<_, McpError>(edit_result(&outcome, "published"))
    },
);

// ------------------------------------------------------------ upload_media

#[derive(serde::Deserialize)]
struct UploadMediaInput {
    filename: String,
    content_base64: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    alt_text: Option<String>,
    #[serde(default)]
    collection_id: Option<i64>,
}

impl OpenApiSchema for UploadMediaInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property("filename", Schema::string())
            .property(
                "content_base64",
                Schema::string().description("base64-encoded file bytes (standard alphabet)"),
            )
            .property("title", Schema::string())
            .property("alt_text", Schema::string())
            .property("collection_id", Schema::integer())
            .required(["filename", "content_base64"])
    }
}

rustango::register_mcp_tool!(
    "upload_media",
    "Upload one media file (base64). Images get EXIF-stripped; a \
     byte-identical existing file is returned instead of duplicated. The \
     result carries the public URL and a ready thumbnail URL.",
    UploadMediaInput,
    |ctx: McpContext, input: UploadMediaInput| async move {
        let actor = require_actor(&ctx).await?;
        if let Some(cid) = input.collection_id {
            let allowed =
                crate::permissions::user_can_in_collection(&ctx.pool, actor.id, cid, Action::Add)
                    .await
                    .map_err(|e| McpError::internal(e.to_string()))?;
            if !allowed {
                return Err(McpError::new(
                    FORBIDDEN,
                    format!("adding to collection {cid} denied"),
                ));
            }
        } else {
            require_codename(&ctx.pool, &actor, "cms_media.add").await?;
        }

        let bytes = base64_decode(&input.content_base64).ok_or_else(|| {
            McpError::invalid_params("content_base64 is not valid standard base64")
        })?;
        let media = crate::admin::store_media(
            &ctx.pool,
            &ctx.agent.tenant,
            bytes,
            &input.filename,
            input.title.as_deref(),
            input.alt_text.as_deref().unwrap_or(""),
            input.collection_id,
            Some(actor.id),
            true,
        )
        .await
        .map_err(|e| McpError::invalid_params(e.to_string()))?;

        let id = media.id.get().copied().unwrap_or_default();
        Ok::<_, McpError>(json!({
            "ok": true,
            "media": {
                "id": id,
                "title": media.title,
                "filename": media.filename,
                "kind": media.kind,
                "mime": media.mime,
                "size": media.size,
                "url": media.public_url(),
                "thumb_url": crate::rendition::rendition_url_for(
                    id, "max-640x480", Some(&media.content_hash)),
            },
        }))
    },
);

/// Standard-alphabet base64 decode without a new dependency.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut rev = [255u8; 256];
    for (i, &c) in ALPHA.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let clean: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let clean = match clean.iter().position(|&b| b == b'=') {
        Some(p) => &clean[..p],
        None => &clean[..],
    };
    let mut out = Vec::with_capacity(clean.len() * 3 / 4 + 3);
    for chunk in clean.chunks(4) {
        let mut acc: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            let v = rev[b as usize];
            if v == 255 {
                return None;
            }
            acc |= u32::from(v) << (18 - 6 * i);
        }
        out.push((acc >> 16) as u8);
        if chunk.len() > 2 {
            out.push((acc >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(acc as u8);
        }
    }
    if clean.len() % 4 == 1 {
        return None;
    }
    Some(out)
}

// ------------------------------------------------------------ attach_media

#[derive(serde::Deserialize)]
struct AttachMediaInput {
    page_id: i64,
    media_id: i64,
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    body_index: Option<i64>,
    #[serde(default)]
    alt: Option<String>,
    #[serde(default)]
    caption: Option<String>,
}

impl OpenApiSchema for AttachMediaInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property("page_id", Schema::integer())
            .property(
                "media_id",
                Schema::integer().description("an existing library item (upload_media, list_media)"),
            )
            .property(
                "field",
                Schema::string().description(
                    "name of a chooser field on the page type (widget \
                     `mediapicker` or `documentchooser` in list_page_types)",
                ),
            )
            .property(
                "body_index",
                Schema::integer().description(
                    "insert an `image` block into the stream body at this \
                     position instead; past the end appends",
                ),
            )
            .property(
                "alt",
                Schema::string()
                    .description("body_index only; defaults to the library item's own alt text"),
            )
            .property("caption", Schema::string().description("body_index only"))
            .required(["page_id", "media_id"])
    }
}

/// Which media kinds a chooser widget accepts, mirroring the admin
/// chooser endpoint's own filter (`kind == "image"` for the media picker,
/// everything else for the document chooser). Returning the rule from one
/// place keeps the tool from accepting a pairing the picker would refuse —
/// which would store an id the editor then can't see or clear.
fn chooser_accepts(kind: &crate::widget::WidgetKind, media_kind: &str) -> Option<bool> {
    match kind {
        crate::widget::WidgetKind::MediaPicker => Some(media_kind == "image"),
        crate::widget::WidgetKind::DocumentChooser => Some(media_kind != "image"),
        _ => None,
    }
}

rustango::register_mcp_tool!(
    "attach_media",
    "Put an existing library item onto a page — either into one of the \
     page type's chooser fields (`field`), or as an `image` block inserted \
     into the stream body (`body_index`). Exactly one of the two. Runs the \
     same save pipeline as update_page (revision, hooks, page log), and \
     refuses a pairing the admin's own picker would reject (an image into \
     a document chooser, or the reverse), so it can never leave an id the \
     editor cannot see.",
    AttachMediaInput,
    |ctx: McpContext, input: AttachMediaInput| async move {
        let actor = require_actor(&ctx).await?;
        require_page_action(&ctx.pool, &actor, input.page_id, Action::Edit).await?;

        let (field, body_index) = match (input.field.as_deref(), input.body_index) {
            (Some(f), None) => (Some(f), None),
            (None, Some(i)) => {
                if i < 0 {
                    return Err(McpError::invalid_params("body_index cannot be negative"));
                }
                (None, Some(i as usize))
            }
            (Some(_), Some(_)) => {
                return Err(McpError::invalid_params(
                    "give either `field` or `body_index`, not both — they are \
                     two different places to put the item",
                ))
            }
            (None, None) => {
                return Err(McpError::invalid_params(
                    "give `field` (a chooser field on this page type) or \
                     `body_index` (insert an image block into the body)",
                ))
            }
        };
        // alt/caption live on a stream block. Silently ignoring them on the
        // chooser path would look like it set alt text; the library item's
        // own alt_text is shared across every page using it, so this tool
        // deliberately will not rewrite it as a side effect.
        if field.is_some() && (input.alt.is_some() || input.caption.is_some()) {
            return Err(McpError::invalid_params(
                "`alt`/`caption` apply to a body block (body_index) — a \
                 chooser field stores only the id. Edit the item's own alt \
                 text in the media library; it is shared by every page \
                 that uses it.",
            ));
        }

        let page: Page = Page::objects()
            .where_(Page::id.eq(input.page_id))
            .first(&ctx.pool)
            .await
            .map_err(McpError::from)?
            .ok_or_else(|| McpError::invalid_params(format!("page {} not found", input.page_id)))?;

        let media: crate::media::Media = crate::media::Media::objects()
            .where_(crate::media::Media::id.eq(input.media_id))
            .first(&ctx.pool)
            .await
            .map_err(McpError::from)?
            .ok_or_else(|| {
                McpError::invalid_params(format!(
                    "media {} not found — see list_media",
                    input.media_id
                ))
            })?;

        // Reading the item is a permission of its own: a page editor is not
        // automatically entitled to every collection.
        if let Some(cid) = media.collection_id {
            let allowed = crate::permissions::user_can_in_collection(
                &ctx.pool,
                actor.id,
                cid,
                Action::View,
            )
            .await
            .map_err(|e| McpError::internal(e.to_string()))?;
            if !allowed {
                return Err(McpError::new(
                    FORBIDDEN,
                    format!("viewing collection {cid} denied"),
                ));
            }
        } else {
            require_codename(&ctx.pool, &actor, "cms_media.add").await?;
        }

        let pt: PageType = PageType::objects()
            .where_(PageType::id.eq(page.page_type_id))
            .first(&ctx.pool)
            .await
            .map_err(McpError::from)?
            .ok_or_else(|| McpError::internal("page type row missing"))?;
        let handler = crate::page_type::find_handler(&pt.type_name).ok_or_else(|| {
            McpError::invalid_params(format!(
                "page type `{}` has no code handler, so it stores no media fields",
                pt.type_name
            ))
        })?;
        let widgets = handler
            .widgets(&ctx.pool, input.page_id)
            .await
            .map_err(|e| McpError::internal(format!("field schema: {e}")))?;

        let mut fields: Option<Map<String, Value>> = None;
        let mut body: Option<Value> = None;
        let placed;

        if let Some(name) = field {
            let w = widgets.iter().find(|w| w.name == name).ok_or_else(|| {
                McpError::invalid_params(format!(
                    "`{name}` is not a field on `{}`. Chooser fields here: {}",
                    pt.type_name,
                    chooser_field_list(&widgets)
                ))
            })?;
            match chooser_accepts(&w.kind, &media.kind) {
                None => {
                    return Err(McpError::invalid_params(format!(
                        "field `{name}` is a `{}` widget, not a media \
                         chooser. Chooser fields here: {}",
                        w.kind.as_tag(),
                        chooser_field_list(&widgets)
                    )))
                }
                Some(false) => {
                    return Err(McpError::invalid_params(format!(
                        "media {} is kind `{}`, which field `{name}` does not \
                         accept — the media picker takes images and the \
                         document chooser takes non-images",
                        input.media_id, media.kind
                    )))
                }
                Some(true) => {}
            }
            let mut m = Map::new();
            m.insert(name.to_owned(), json!(input.media_id));
            fields = Some(m);
            placed = json!({ "field": name });
        } else {
            let index = body_index.unwrap_or(0);
            let body_w = widgets
                .iter()
                .find(|w| w.name == "body" && w.kind == crate::widget::WidgetKind::Stream)
                .ok_or_else(|| {
                    McpError::invalid_params(format!(
                        "page type `{}` has no stream `body`, so there is \
                         nowhere to insert a block — use `field` instead ({})",
                        pt.type_name,
                        chooser_field_list(&widgets)
                    ))
                })?;
            // A stream declares which block types may sit at its top level,
            // and only the editor UI honours it — the save path stores
            // whatever it is given. Without this check the tool would
            // cheerfully persist an `image` block into a stream whose page
            // type forbids one: a block no editor could have added and none
            // can pick, reported back as success.
            if !body_w.allowed.iter().any(|a| a == "image") {
                return Err(McpError::invalid_params(format!(
                    "the `body` stream on `{}` does not allow `image` blocks \
                     (it allows: {}). Attach to a chooser field instead ({}), \
                     or add `image` to the field's allowed blocks.",
                    pt.type_name,
                    if body_w.allowed.is_empty() {
                        "nothing".to_owned()
                    } else {
                        body_w.allowed.join(", ")
                    },
                    chooser_field_list(&widgets)
                )));
            }
            if media.kind != "image" {
                return Err(McpError::invalid_params(format!(
                    "the `image` block needs an image; media {} is kind `{}`. \
                     Attach a document to a document-chooser field instead.",
                    input.media_id, media.kind
                )));
            }
            let ext = handler
                .load_extension(&ctx.pool, input.page_id)
                .await
                .map_err(|e| McpError::internal(format!("load body: {e}")))?;
            let mut blocks = existing_stream_blocks(ext.get("body"))?;
            let at = index.min(blocks.len());
            blocks.insert(
                at,
                json!({
                    "type": "image",
                    "id": mint_uuid4(),
                    "value": {
                        "media_id": input.media_id.to_string(),
                        "alt": input.alt.clone().unwrap_or_else(|| media.alt_text.clone()),
                        "caption": input.caption.clone().unwrap_or_default(),
                    },
                }),
            );
            body = Some(Value::Array(blocks));
            placed = json!({ "body_index": at });
        }

        // The full save pipeline rebuilds the extension, inline panels and
        // builder body from the form, so start from everything stored.
        let mut form = crate::page_form::prefill(&ctx.pool, &page).await;
        // No `builder` payload here, and an extension `body` stream only
        // ever holds registered blocks, so no dyn names are in play.
        extend_with_content(
            &mut form,
            fields.as_ref(),
            body,
            None,
            &std::collections::HashSet::new(),
        )?;
        let outcome = run_page_edit(&ctx, &actor, input.page_id, &form).await?;

        let mut out = edit_result(&outcome, "updated");
        if let Some(obj) = out.as_object_mut() {
            obj.insert(
                "attached".to_owned(),
                json!({
                    "media_id": input.media_id,
                    "kind": media.kind,
                    "title": media.title,
                    "url": media.public_url(),
                    "at": placed,
                }),
            );
        }
        Ok::<_, McpError>(out)
    },
);

/// The page's current stream blocks, from whatever shape the extension
/// loader hands back.
///
/// A stream that has never been saved is not always an array: it comes
/// back absent, null, or as the empty string the form layer round-trips,
/// and a stored one can arrive as a JSON *string* rather than parsed.
/// Treating any of those as "not a stream" would make attach_media refuse
/// the very case it exists for — the first image on a fresh page.
fn existing_stream_blocks(raw: Option<&Value>) -> Result<Vec<Value>, McpError> {
    match raw {
        Some(Value::Array(a)) => Ok(a.clone()),
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(Vec::new()),
        Some(Value::String(s)) => serde_json::from_str(s)
            .map_err(|e| McpError::internal(format!("stored body is not a stream: {e}"))),
        Some(other) => Err(McpError::internal(format!(
            "stored body is {other}, not a stream array"
        ))),
    }
}

/// The page type's chooser fields, for an error message that tells the
/// agent where the item *can* go instead of just refusing.
fn chooser_field_list(widgets: &[crate::widget::Widget]) -> String {
    let mut v: Vec<String> = widgets
        .iter()
        .filter(|w| chooser_accepts(&w.kind, "image").is_some())
        .map(|w| {
            let takes = match w.kind {
                crate::widget::WidgetKind::MediaPicker => "images",
                _ => "documents",
            };
            format!("{} ({takes})", w.name)
        })
        .collect();
    v.sort();
    if v.is_empty() {
        "(none on this page type)".to_owned()
    } else {
        v.join(", ")
    }
}


// ------------------------------------------------------ upsert_translations

#[derive(serde::Deserialize)]
struct TranslationUpdate {
    field_path: String,
    value: String,
}

#[derive(serde::Deserialize)]
struct UpsertTranslationsInput {
    page_id: i64,
    locale: String,
    updates: Vec<TranslationUpdate>,
}

impl OpenApiSchema for UpsertTranslationsInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property("page_id", Schema::integer())
            .property(
                "locale",
                Schema::string().description("target locale code (never the default locale)"),
            )
            .property(
                "updates",
                Schema::array_of(
                    Schema::object()
                        .property("field_path", Schema::string())
                        .property(
                            "value",
                            Schema::string().description("empty string deletes the override"),
                        )
                        .required(["field_path", "value"]),
                ),
            )
            .required(["page_id", "locale", "updates"])
    }
}

rustango::register_mcp_tool!(
    "upsert_translations",
    "Write translation overrides for a page in one locale. Field paths \
     come from list_translatable_fields (canonical columns, stream-leaf \
     dotted paths, builder.* paths); an empty value deletes the override \
     so the canonical text shows through. Atomic per call.",
    UpsertTranslationsInput,
    |ctx: McpContext, input: UpsertTranslationsInput| async move {
        let actor = require_actor(&ctx).await?;
        require_page_action(&ctx.pool, &actor, input.page_id, Action::Edit).await?;

        let code = crate::locale::validate_code(&input.locale).map_err(McpError::invalid_params)?;
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
        if locale.is_default {
            return Err(McpError::invalid_params(
                "cannot translate INTO the default locale — canonical content \
                 lives on the page itself (use update_page)",
            ));
        }
        let locale_id = locale.id.get().copied().unwrap_or_default();

        let _page: Page = Page::objects()
            .where_(Page::id.eq(input.page_id))
            .first(&ctx.pool)
            .await
            .map_err(McpError::from)?
            .ok_or_else(|| McpError::invalid_params(format!("page {} not found", input.page_id)))?;

        let mut updates: Vec<(String, String)> = Vec::with_capacity(input.updates.len());
        for u in &input.updates {
            let path = crate::translation::validate_field_path(&u.field_path)
                .map_err(|e| McpError::invalid_params(format!("{}: {e}", u.field_path)))?;
            updates.push((path, u.value.clone()));
        }
        let count = updates.len();
        crate::admin::upsert_page_translations(&ctx.pool, input.page_id, locale_id, updates)
            .await
            .map_err(McpError::from)?;

        crate::page_log::record_or_warn(
            &ctx.pool,
            input.page_id,
            crate::page_log::ACTION_EDIT,
            Some(actor.id),
            format!("Updated {count} translation override(s) for {code} via MCP."),
            Some(json!({ "source": "mcp", "locale": code })),
        )
        .await;
        Ok::<_, McpError>(json!({ "ok": true, "locale": code, "applied": count }))
    },
);

// ------------------------------------------------------------- snippets

#[derive(serde::Deserialize)]
struct UpsertSnippetInput {
    type_name: String,
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    folder_path: Option<String>,
    #[serde(default)]
    body_markdown: Option<String>,
    #[serde(default)]
    data: Option<Value>,
}

impl OpenApiSchema for UpsertSnippetInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property(
                "type_name",
                Schema::string().description("library type (see list_snippets)"),
            )
            .property(
                "id",
                Schema::integer().description("omit to create; set to update"),
            )
            .property("title", Schema::string())
            .property("slug", Schema::string())
            .property("folder_path", Schema::string())
            .property("body_markdown", Schema::string())
            .property(
                "data",
                Schema::object().description(
                    "the snippet's structured data JSON. For a `form` this is \
                     the form schema, saved as its draft: the published form \
                     changes only when it is published in the admin",
                ),
            )
            .required(["type_name"])
    }
}

rustango::register_mcp_tool!(
    "upsert_snippet",
    "Create or update a library snippet (title, slug, folder, markdown \
     body, structured data). Updates capture a revision when the library \
     type has revisions enabled.",
    UpsertSnippetInput,
    |ctx: McpContext, input: UpsertSnippetInput| async move {
        let actor = require_actor(&ctx).await?;
        let handler = crate::library::find_handler(&input.type_name).ok_or_else(|| {
            McpError::invalid_params(format!("unknown library type `{}`", input.type_name))
        })?;
        // The library-wide grant or the per-type one, for the action this
        // call performs (#758) — never `.view`.
        let action = if input.id.is_some() { "edit" } else { "add" };
        require_any_codename(
            &ctx.pool,
            &actor,
            &[
                &format!("cms_library.{action}"),
                &format!("cms_library_item__{}.{action}", input.type_name),
            ],
        )
        .await?;

        let mut snippet = match input.id {
            Some(id) => {
                let existing: crate::snippet::Snippet = crate::snippet::Snippet::objects()
                    .where_(crate::snippet::Snippet::id.eq(id))
                    .first(&ctx.pool)
                    .await
                    .map_err(McpError::from)?
                    .ok_or_else(|| McpError::invalid_params(format!("snippet {id} not found")))?;
                if existing.type_name != input.type_name {
                    return Err(McpError::invalid_params(format!(
                        "snippet {id} is a `{}`, not a `{}`",
                        existing.type_name, input.type_name
                    )));
                }
                if !handler.can_edit(&existing) {
                    return Err(McpError::new(
                        FORBIDDEN,
                        format!(
                            "library type `{}` refuses edits to this snippet",
                            input.type_name
                        ),
                    ));
                }
                existing
            }
            None => crate::snippet::Snippet {
                id: rustango::sql::Auto::Unset,
                type_name: input.type_name.clone(),
                slug: String::new(),
                folder_path: String::new(),
                title: String::new(),
                body_markdown: String::new(),
                data: json!({}),
                created_at: rustango::sql::Auto::Unset,
                updated_at: rustango::sql::Auto::Unset,
            },
        };

        if let Some(t) = input.title {
            snippet.title = t;
        }
        if let Some(s) = input.slug {
            snippet.slug = crate::admin::slugify(&s);
        }
        if snippet.slug.is_empty() {
            snippet.slug = crate::admin::slugify(&snippet.title);
        }
        if let Some(f) = input.folder_path {
            snippet.folder_path = crate::snippet::normalize_folder(&f);
        }
        if let Some(b) = input.body_markdown {
            snippet.body_markdown = b;
        }
        if snippet.title.is_empty() {
            return Err(McpError::invalid_params("a snippet needs a title"));
        }
        // The same cross-field hook the admin save runs (#758), over the
        // fields this call sets.
        let mut fields = std::collections::HashMap::from([
            ("type_name".to_owned(), snippet.type_name.clone()),
            ("title".to_owned(), snippet.title.clone()),
            ("slug".to_owned(), snippet.slug.clone()),
            ("folder_path".to_owned(), snippet.folder_path.clone()),
            ("body_markdown".to_owned(), snippet.body_markdown.clone()),
        ]);
        if let Some(d) = &input.data {
            fields.insert("data".to_owned(), d.to_string());
        }
        if let Err(errors) = handler.validate(&fields) {
            let summary: Vec<String> =
                errors.into_iter().map(|(field, msg)| format!("{field}: {msg}")).collect();
            return Err(McpError::invalid_params(summary.join("; ")));
        }
        // A form's data is its schema, and writing it raw would skip what
        // the form builder does (#758): sanitize rich text, save a draft
        // rather than the live form, and keep the notify target in step.
        let mut form = None;
        if let Some(d) = input.data {
            if snippet.type_name == "form" {
                let mut parsed = crate::forms::schema::parse(&d)
                    .map_err(|e| McpError::invalid_params(format!("invalid form schema: {e}")))?;
                parsed.sanitize_rich_text();
                snippet.data = crate::forms::schema::save_draft_value(&snippet.data, &parsed);
                form = Some(parsed);
            } else {
                snippet.data = d;
            }
        }

        snippet.save_pool(&ctx.pool).await.map_err(McpError::from)?;
        let mut warnings = Vec::new();
        if let Some(parsed) = &form {
            warnings = crate::forms::schema::validate(parsed);
            let id = snippet.id.get().copied().unwrap_or_default();
            if let Err(e) = crate::notify::targets::sync_form_email(
                &ctx.pool,
                id,
                &snippet.title,
                &parsed.settings.notify_emails,
            )
            .await
            {
                tracing::warn!(
                    target: "rustango_cms::notify",
                    error = %e, form_id = id,
                    "could not sync the form's email notification target"
                );
            }
        }
        if handler.revisions_enabled() {
            crate::snippet::capture_revision(&ctx.pool, &snippet, Some(actor.id))
                .await
                .log_warn("revision not captured; the history has a gap");
        }
        Ok::<_, McpError>(json!({
            "ok": true,
            "snippet": {
                "id": snippet.id.get().copied(),
                "type_name": snippet.type_name,
                "slug": snippet.slug,
                "title": snippet.title,
            },
            "warnings": warnings,
        }))
    },
);

#[cfg(test)]
mod tests {
    use super::*;

    use crate::widget::{Widget, WidgetKind};

    fn w(name: &str, kind: WidgetKind) -> Widget {
        Widget::new(kind, name, name)
    }

    /// The pairing rule mirrors the admin chooser endpoint's own filter
    /// (`kind == "image"` for the picker, everything else for documents).
    /// If it drifts, attach_media starts storing ids the editor cannot see
    /// or clear, so pin both directions.
    #[test]
    fn chooser_accepts_mirrors_the_admin_picker_filter() {
        // media picker: images only
        assert_eq!(
            chooser_accepts(&WidgetKind::MediaPicker, "image"),
            Some(true)
        );
        assert_eq!(
            chooser_accepts(&WidgetKind::MediaPicker, "document"),
            Some(false)
        );
        assert_eq!(
            chooser_accepts(&WidgetKind::MediaPicker, "other"),
            Some(false)
        );
        // document chooser: the complement, not "documents only" — the
        // picker's own filter is `kind != "image"`, so `other` belongs here
        assert_eq!(
            chooser_accepts(&WidgetKind::DocumentChooser, "document"),
            Some(true)
        );
        assert_eq!(
            chooser_accepts(&WidgetKind::DocumentChooser, "other"),
            Some(true)
        );
        assert_eq!(
            chooser_accepts(&WidgetKind::DocumentChooser, "image"),
            Some(false)
        );
        // not a media chooser at all — distinct from "refuses this kind",
        // because the two produce different error messages
        assert_eq!(chooser_accepts(&WidgetKind::Text, "image"), None);
        assert_eq!(chooser_accepts(&WidgetKind::PageChooser, "image"), None);
        assert_eq!(chooser_accepts(&WidgetKind::SnippetChooser, "image"), None);
    }

    #[test]
    fn chooser_field_list_names_only_media_fields_and_what_they_take() {
        let widgets = vec![
            w("title", WidgetKind::Text),
            w("hero", WidgetKind::MediaPicker),
            w("spec_sheet", WidgetKind::DocumentChooser),
            w("related", WidgetKind::PageChooser),
        ];
        let listed = chooser_field_list(&widgets);
        assert_eq!(listed, "hero (images), spec_sheet (documents)");
        // a type with nothing to attach to says so, rather than ""
        assert_eq!(
            chooser_field_list(&[w("title", WidgetKind::Text)]),
            "(none on this page type)"
        );
    }

    /// A fresh page's stream is not an array. If these collapse, the tool
    /// refuses the first image on a new page — the main thing it is for.
    #[test]
    fn existing_stream_blocks_reads_a_never_saved_body_as_empty() {
        assert!(existing_stream_blocks(None).unwrap().is_empty());
        assert!(existing_stream_blocks(Some(&Value::Null)).unwrap().is_empty());
        assert!(existing_stream_blocks(Some(&json!(""))).unwrap().is_empty());
        assert!(existing_stream_blocks(Some(&json!("   "))).unwrap().is_empty());
    }

    #[test]
    fn existing_stream_blocks_accepts_arrays_and_json_strings_alike() {
        let blocks = json!([{ "type": "heading", "id": "a", "value": "Hi" }]);
        assert_eq!(existing_stream_blocks(Some(&blocks)).unwrap().len(), 1);
        // the same stream arriving as a stored JSON string
        let as_str = json!(r#"[{"type":"heading","id":"a","value":"Hi"}]"#);
        let parsed = existing_stream_blocks(Some(&as_str)).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["type"], "heading");
    }

    #[test]
    fn existing_stream_blocks_refuses_a_body_that_is_not_a_stream() {
        // a number or object is neither a stream nor an unsaved one; failing
        // loudly beats silently discarding whatever was stored
        assert!(existing_stream_blocks(Some(&json!(42))).is_err());
        assert!(existing_stream_blocks(Some(&json!({ "a": 1 }))).is_err());
        assert!(existing_stream_blocks(Some(&json!("not json"))).is_err());
    }

    #[test]
    fn base64_decode_round_trips_and_rejects_garbage() {
        // "hello" → aGVsbG8=
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        // no padding is accepted
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello");
        // whitespace (line wraps) is ignored
        assert_eq!(base64_decode("aGVs\nbG8=").unwrap(), b"hello");
        // a non-alphabet byte is rejected
        assert!(base64_decode("aGVsbG8*").is_none());
        // a dangling quantum (len % 4 == 1) is invalid base64
        assert!(base64_decode("aGVsb").is_none());
    }

    #[test]
    fn value_to_form_string_matches_checkbox_semantics() {
        assert_eq!(value_to_form_string(&json!(true)).as_deref(), Some("on"));
        assert_eq!(value_to_form_string(&json!(false)), None);
        assert_eq!(value_to_form_string(&Value::Null), None);
        assert_eq!(value_to_form_string(&json!("hi")).as_deref(), Some("hi"));
        assert_eq!(value_to_form_string(&json!(42)).as_deref(), Some("42"));
        assert_eq!(
            value_to_form_string(&json!(["a", "b"])).as_deref(),
            Some(r#"["a","b"]"#)
        );
    }

    #[test]
    fn mint_uuid4_is_well_formed_v4() {
        let u = mint_uuid4();
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_eq!(&parts[2][..1], "4", "version nibble");
        assert!(
            matches!(&parts[3][..1], "8" | "9" | "a" | "b"),
            "variant nibble"
        );
        assert_ne!(mint_uuid4(), mint_uuid4());
    }

    #[test]
    fn normalize_stream_body_mints_ids_and_validates() {
        // Unknown block type is rejected.
        let mut bad = json!([{ "type": "definitely_not_a_block", "value": {} }]);
        assert!(normalize_stream_body(&mut bad, &std::collections::HashSet::new()).is_err());

        // Non-array is rejected.
        let mut not_arr = json!({ "type": "x" });
        assert!(normalize_stream_body(&mut not_arr, &std::collections::HashSet::new()).is_err());

        // An item missing `type` is rejected.
        let mut no_type = json!([{ "value": {} }]);
        assert!(normalize_stream_body(&mut no_type, &std::collections::HashSet::new()).is_err());
    }

    #[test]
    fn looks_like_stream_distinguishes_envelopes_from_lists() {
        assert!(looks_like_stream(&json!([{ "type": "x", "value": {} }])));
        assert!(!looks_like_stream(&json!(["a", "b"])));
        assert!(!looks_like_stream(&json!([])));
        assert!(!looks_like_stream(&json!("scalar")));
    }

    // ---- apply_page_flags ----

    fn form_with_menus() -> HashMap<String, String> {
        let mut f = HashMap::new();
        f.insert("show_in_menus".into(), "on".into());
        f
    }

    #[test]
    fn omitting_a_flag_leaves_the_pages_current_value_alone() {
        // `page_form::canonical` has already put the page's state in the
        // form; an update that doesn't mention a flag must not reset it.
        let mut f = form_with_menus();
        apply_page_flags(&mut f, None, None);
        assert_eq!(f.get("show_in_menus").map(String::as_str), Some("on"));

        let mut empty: HashMap<String, String> = HashMap::new();
        apply_page_flags(&mut empty, None, None);
        assert!(!empty.contains_key("show_in_menus"));
    }

    #[test]
    fn setting_show_in_menus_true_posts_the_checkbox_value() {
        let mut f: HashMap<String, String> = HashMap::new();
        apply_page_flags(&mut f, Some(true), None);
        assert_eq!(f.get("show_in_menus").map(String::as_str), Some("on"));
    }

    #[test]
    fn setting_show_in_menus_false_removes_the_key_rather_than_blanking_it() {
        // An unchecked HTML checkbox posts nothing at all; `"off"` or
        // `""` would be read as truthy-present by the save pipeline.
        let mut f = form_with_menus();
        apply_page_flags(&mut f, Some(false), None);
        assert!(!f.contains_key("show_in_menus"));
    }

    #[test]
    fn a_partial_update_keeps_the_pages_template_override() {
        // The prefill is what makes `update_page` partial. Leaving
        // `template_override` out of it meant every update that didn't
        // mention the template silently reset the page to its type's —
        // a one-field edit quietly undoing a layout choice.
        let page = Page {
            id: rustango::sql::Auto::Set(1),
            page_type_id: 1,
            title: "Reports".to_owned(),
            slug: String::new(),
            path: "0001/".to_owned(),
            url_path: "/".to_owned(),
            preview_path: String::new(),
            template_override: "report_index.html".to_owned(),
            depth: 1,
            parent_id: None,
            locale_variant_of: None,
            alias_of: None,
            theme_id: None,
            sort_order: 0,
            status: crate::page::PageStatus::Published.as_str().to_owned(),
            published_at: None,
            last_published_at: None,
            go_live_at: None,
            expire_at: None,
            seo_title: String::new(),
            seo_description: String::new(),
            robots_index: true,
            sitemap_priority: 0.5,
            show_in_menus: false,
            og_title: String::new(),
            og_description: String::new(),
            og_image_media_id: None,
            twitter_card: "summary_large_image".to_owned(),
            notification_pre_published_sent: false,
            created_at: rustango::sql::Auto::Unset,
            updated_at: rustango::sql::Auto::Unset,
        };
        let f = crate::page_form::canonical(&page);
        assert_eq!(
            f.get("template_override").map(String::as_str),
            Some("report_index.html")
        );
    }

    #[test]
    fn template_override_is_set_verbatim() {
        let mut f: HashMap<String, String> = HashMap::new();
        apply_page_flags(&mut f, None, Some("fy2027/report.html".into()));
        assert_eq!(
            f.get("template_override").map(String::as_str),
            Some("fy2027/report.html")
        );
    }

    #[test]
    fn an_empty_template_override_restores_the_types_template() {
        // Empty is the documented way back to the page type's own
        // template, so it must be written, not skipped.
        let mut f: HashMap<String, String> = HashMap::new();
        f.insert("template_override".into(), "old.html".into());
        apply_page_flags(&mut f, None, Some(String::new()));
        assert_eq!(f.get("template_override").map(String::as_str), Some(""));
    }
}
