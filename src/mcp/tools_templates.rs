//! MCP tools for per-tenant templates (#587).
//!
//! An agent can **list, read, create and modify** a tenant's template
//! overrides, and point a page type at a template. It funnels into the
//! same [`crate::tenant_templates`] store the admin editor uses, so the
//! same validation, the same containment checks and the same live
//! reload apply.
//!
//! ## Deliberately no delete
//!
//! There is no tool to remove a template or revert an override, and that
//! is a decision rather than an omission. Creating and modifying are
//! recoverable — the previous body is in the author's git history, and a
//! broken body is refused before it is written. Deleting an override is
//! the one operation whose result an agent cannot inspect first and
//! cannot undo: the tenant silently falls back to the global template
//! and the customisation is gone.
//!
//! Reverting stays in the admin, behind a confirm dialog, where a person
//! decides. If an agent needs a template to stop doing something, it can
//! rewrite the body.
//!
//! The tenant comes from `ctx.agent.tenant` — the token is tenant-pinned,
//! so an agent cannot address another tenant's folder even by name.

use rustango::mcp::{McpContext, McpError};
use rustango::openapi::{OpenApiSchema, Schema};
use serde_json::json;

use super::{require_actor, require_codename};
use crate::admin::TEMPLATE_EDIT_CODENAME;

/// The store, or a legible error when the host has not configured one.
fn store() -> Result<std::sync::Arc<crate::tenant_templates::TenantTemplates>, McpError> {
    crate::tenant_templates::installed().ok_or_else(|| {
        McpError::invalid_params(
            "per-tenant templates are not configured on this deployment — \
             the host application has not enabled a template directory",
        )
    })
}

// ------------------------------------------------------------------ list

#[derive(serde::Deserialize)]
struct ListInput {}

impl OpenApiSchema for ListInput {
    fn openapi_schema() -> Schema {
        Schema::object()
    }
}

rustango::register_mcp_tool!(
    "list_templates",
    "List every template this site renders: the ones customised for this \
     tenant and the ones inherited from the global set. `source` is \
     `override` or `global` — editing an inherited template creates a \
     tenant copy and leaves the shared one alone.",
    ListInput,
    |ctx: McpContext, _input: ListInput| async move {
        let actor = require_actor(&ctx).await?;
        require_codename(&ctx.pool, &actor, TEMPLATE_EDIT_CODENAME).await?;
        let tt = store()?;
        let rows: Vec<_> = tt
            .list_for(&ctx.agent.tenant)
            .into_iter()
            .map(|e| {
                json!({
                    "name": e.name,
                    "source": if e.source == crate::tenant_templates::Source::Override {
                        "override"
                    } else {
                        "global"
                    },
                    "bytes": e.bytes,
                })
            })
            .collect();
        Ok(json!({ "templates": rows, "tenant": ctx.agent.tenant }))
    }
);

// ------------------------------------------------------------------ read

#[derive(serde::Deserialize)]
struct ReadInput {
    name: String,
}

impl OpenApiSchema for ReadInput {
    fn openapi_schema() -> Schema {
        Schema::object().property(
            "name",
            Schema::string().description("template name, e.g. landing.html or blocks/hero.html"),
        )
    }
}

rustango::register_mcp_tool!(
    "read_template",
    "Read a template's source. Returns the tenant's own copy when it has \
     one, otherwise the inherited global body — which is the right \
     starting point for customising it.",
    ReadInput,
    |ctx: McpContext, input: ReadInput| async move {
        let actor = require_actor(&ctx).await?;
        require_codename(&ctx.pool, &actor, TEMPLATE_EDIT_CODENAME).await?;
        let tt = store()?;
        let slug = &ctx.agent.tenant;

        let own = tt.read_override(slug, &input.name);
        let global = tt.read_global(&input.name);
        let Some(body) = own.clone().or_else(|| global.clone()) else {
            return Err(McpError::invalid_params(format!(
                "no template `{}` — list_templates shows the valid names",
                input.name
            )));
        };
        Ok(json!({
            "name": input.name,
            "source": if own.is_some() { "override" } else { "global" },
            "body": body,
        }))
    }
);

// ----------------------------------------------------------------- write

#[derive(serde::Deserialize)]
struct WriteInput {
    name: String,
    body: String,
}

impl OpenApiSchema for WriteInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property(
                "name",
                Schema::string()
                    .description("template name, e.g. landing.html or blocks/hero.html"),
            )
            .property(
                "body",
                Schema::string().description("full Tera source; replaces the file"),
            )
    }
}

rustango::register_mcp_tool!(
    "write_template",
    "Create or replace a template for this tenant. The body is parsed \
     against this tenant's real template set before anything is written, \
     so `{% extends %}` resolves and a missing parent is caught, not just \
     a syntax slip — a rejected write changes nothing. The change is live \
     on the next request. There is no delete tool: reverting an override \
     is left to a person in the admin.",
    WriteInput,
    |ctx: McpContext, input: WriteInput| async move {
        let actor = require_actor(&ctx).await?;
        require_codename(&ctx.pool, &actor, TEMPLATE_EDIT_CODENAME).await?;
        let tt = store()?;
        let slug = &ctx.agent.tenant;
        let existed = tt.read_override(slug, &input.name).is_some();

        tt.write_override(slug, &input.name, &input.body)
            .map_err(|e| McpError::invalid_params(e.to_string()))?;

        Ok(json!({
            "name": input.name,
            "created": !existed,
            "bytes": input.body.len(),
            "note": "live on the next request; no restart needed",
        }))
    }
);

// -------------------------------------------------------------- validate

#[derive(serde::Deserialize)]
struct ValidateInput {
    name: String,
    body: String,
}

impl OpenApiSchema for ValidateInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property("name", Schema::string().description("the name it would be saved as"))
            .property("body", Schema::string().description("Tera source to check"))
    }
}

rustango::register_mcp_tool!(
    "validate_template",
    "Check whether a template body would parse for this tenant, WITHOUT \
     writing it. Same check `write_template` runs. Use it to iterate on a \
     template before committing to it.",
    ValidateInput,
    |ctx: McpContext, input: ValidateInput| async move {
        let actor = require_actor(&ctx).await?;
        require_codename(&ctx.pool, &actor, TEMPLATE_EDIT_CODENAME).await?;
        let tt = store()?;
        match tt.validate(&ctx.agent.tenant, &input.name, &input.body) {
            Ok(()) => Ok(json!({ "ok": true })),
            Err(e) => Ok(json!({ "ok": false, "error": e.to_string() })),
        }
    }
);

// --------------------------------------------- point a page type at one

#[derive(serde::Deserialize)]
struct AssignInput {
    page_type: String,
    template: String,
}

impl OpenApiSchema for AssignInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property(
                "page_type",
                Schema::string().description("page-type type_name (see list_page_types)"),
            )
            .property(
                "template",
                Schema::string().description(
                    "template name to render this type with; empty string means \
                     no HTML representation (JSON only)",
                ),
            )
    }
}

rustango::register_mcp_tool!(
    "set_page_type_template",
    "Point a page type at a template. The name must be one this tenant can \
     actually render (list_templates), or the change is refused — saving an \
     unresolvable name would break every page of that type. An empty \
     template means the type has no HTML page and serves JSON only.",
    AssignInput,
    |ctx: McpContext, input: AssignInput| async move {
        use rustango::core::Column as _;

        let actor = require_actor(&ctx).await?;
        require_codename(&ctx.pool, &actor, TEMPLATE_EDIT_CODENAME).await?;
        let tt = store()?;

        let mut row: crate::page_type_model::PageType =
            crate::page_type_model::PageType::objects()
                .where_(crate::page_type_model::PageType::type_name.eq(input.page_type.clone()))
                .first(&ctx.pool)
                .await
                .map_err(McpError::from)?
                .ok_or_else(|| {
                    McpError::invalid_params(format!(
                        "unknown page_type `{}` — list_page_types shows the valid names",
                        input.page_type
                    ))
                })?;

        let chosen = input.template.trim().to_owned();
        if !chosen.is_empty() {
            // Must be renderable *and* a sensible thing for a page type —
            // the same rule the admin picker applies. Without the second
            // half an agent could point a page type at the CMS's own
            // chrome, which resolves fine and renders nonsense.
            let renderable = tt
                .for_tenant(&ctx.agent.tenant)
                .get_template_names()
                .any(|n| n == chosen);
            if !renderable || !crate::admin::template_offerable(&chosen) {
                return Err(McpError::invalid_params(format!(
                    "`{chosen}` is not a template this tenant can render as a \
                     page — list_templates shows the valid names",
                )));
            }
        }

        let previous = row.default_template.clone();
        row.default_template = chosen.clone();
        row.save_pool(&ctx.pool).await.map_err(McpError::from)?;

        Ok(json!({
            "page_type": row.type_name,
            "template": chosen,
            "previous": previous,
            "serves": if chosen.is_empty() { "json-only" } else { "html" },
        }))
    }
);
