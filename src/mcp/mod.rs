//! MCP engine (#587) — operate the CMS from an AI agent.
//!
//! The framework ships the whole protocol layer (`rustango::mcp`: JSON-RPC
//! transport, SSE, agent auth incl. raw-key bearers, OAuth 2.1, skills +
//! fail-closed authorization); this module contributes the **CMS tools** —
//! read/search content, create + edit + publish pages, upload media, write
//! translations, upsert snippets — plus the per-tenant skill seeding
//! (`crate::seed`) and the key-management UI (account preferences / user
//! form).
//!
//! ## Identity model
//!
//! Every tool requires a **user-owned key**: the token's `uid` resolves to a
//! `rustango_users` row, and the tool acts *as that user* — page/collection
//! permissions are enforced with the same `crate::permissions` checks the
//! admin handlers use, revisions and page-log entries are attributed to the
//! user, and the key's tool scope re-resolves from the owner's live RBAC on
//! every request. Machine agents (no `uid`) are refused: CMS writes always
//! have an accountable actor.
//!
//! ## Host wiring
//!
//! ```ignore
//! let api = cms_admin
//!     .merge(rustango_cms::mcp::router(&settings.mcp))
//!     .merge(rustango_cms::api::router());
//! let csrf = CsrfConfig::default().exempt_prefix(rustango_cms::mcp::MCP_PREFIX);
//! ```
//!
//! The CSRF exemption is **required** — the global `CsrfLayer` otherwise
//! rejects every JSON-RPC POST (bearer-authenticated requests carry no CSRF
//! token, and don't need one: the endpoint never trusts cookies).

use rustango::mcp::{McpContext, McpError};

mod tools_read;
mod tools_templates;
mod tools_write;

/// Where the MCP endpoint mounts, on every tenant host. Host apps must
/// CSRF-exempt this prefix (see the module docs).
pub const MCP_PREFIX: &str = "/cms-admin/mcp";

/// The CMS MCP router: the framework's secure tenant router (agent JWT or
/// raw `prefix.secret` bearer, tenant-pinned, fail-closed skills) nested
/// under [`MCP_PREFIX`]. Merge it into the host router OUTSIDE any
/// session-auth layer — it carries its own authentication.
#[must_use]
pub fn router(settings: &rustango::config::McpSettings) -> axum::Router {
    axum::Router::new().nest(
        MCP_PREFIX,
        rustango::mcp::secure_tenant_router_from_settings(settings),
    )
}

/// JSON-RPC error code for an in-tool authorization denial. Matches the
/// dispatcher's `TOOL_FORBIDDEN` so clients render both the skill-level
/// and the per-object denial the same way.
pub(crate) const FORBIDDEN: i64 = -32003;

/// The user a tool call acts as — the key owner, loaded fresh per call.
pub struct ToolActor {
    pub id: i64,
    pub username: String,
    pub is_superuser: bool,
}

impl ToolActor {
    pub(crate) fn as_page_edit_actor(&self) -> crate::admin::PageEditActor {
        crate::admin::PageEditActor {
            id: self.id,
            username: self.username.clone(),
            is_superuser: self.is_superuser,
        }
    }
}

/// Resolve the acting user for a tool call. Refuses machine agents (CMS
/// writes need an accountable user) and keys whose owner is gone or
/// deactivated — fail-closed, mirroring the admin session checks.
///
/// Public so a host application's own tools gate the same way. A host
/// that reimplemented this would have to duplicate the user lookup and
/// the active check, and the first thing to drift would be the
/// fail-closed behaviour.
pub async fn require_actor(ctx: &McpContext) -> Result<ToolActor, McpError> {
    use rustango::core::Column as _;
    use rustango::tenancy::auth::User;

    let Some(uid) = ctx.agent.user_id else {
        return Err(McpError::new(
            FORBIDDEN,
            "CMS tools require a user-owned key (create one under \
             Account → MCP keys); standalone machine agents are not \
             accepted",
        ));
    };
    let user = User::objects()
        .where_(User::id.eq(uid))
        .first(&ctx.pool)
        .await
        .map_err(|e| McpError::internal(format!("actor lookup: {e}")))?
        .filter(|u| u.active)
        .ok_or_else(|| McpError::new(FORBIDDEN, "key owner is missing or deactivated"))?;
    Ok(ToolActor {
        id: uid,
        username: user.username,
        is_superuser: user.is_superuser,
    })
}

/// Require a permission codename (`cms_page.view`, …) — the same
/// role-union check the admin's codename gates run, superuser bypassing.
///
/// Public for host tools, which need their own codenames checked
/// against the same role union — see [`require_actor`].
pub async fn require_codename(
    pool: &rustango::sql::Pool,
    actor: &ToolActor,
    codename: &str,
) -> Result<(), McpError> {
    require_any_codename(pool, actor, &[codename]).await
}

/// [`require_codename`] satisfied by any one of `codenames` — for a
/// resource with a broad grant and a narrower one (`cms_library.edit`
/// or `cms_library_item__<type>.edit`).
pub async fn require_any_codename(
    pool: &rustango::sql::Pool,
    actor: &ToolActor,
    codenames: &[&str],
) -> Result<(), McpError> {
    if actor.is_superuser {
        return Ok(());
    }
    let names = crate::permissions::user_codenames(pool, actor.id)
        .await
        .map_err(|e| McpError::internal(format!("permission lookup: {e}")))?;
    if codenames.iter().any(|c| names.contains(*c)) {
        Ok(())
    } else {
        Err(McpError::new(
            FORBIDDEN,
            format!("permission `{}` required", codenames.join("` or `")),
        ))
    }
}

/// Require a page-scoped action (`crate::permissions::user_can` — walks
/// the page-permission tree, superuser short-circuits inside).
pub(crate) async fn require_page_action(
    pool: &rustango::sql::Pool,
    actor: &ToolActor,
    page_id: i64,
    action: crate::permissions::Action,
) -> Result<(), McpError> {
    let allowed = crate::permissions::user_can(pool, actor.id, page_id, action)
        .await
        .map_err(|e| McpError::internal(format!("permission lookup: {e}")))?;
    if allowed {
        Ok(())
    } else {
        Err(McpError::new(
            FORBIDDEN,
            format!("action `{}` on page {page_id} denied", action.as_str()),
        ))
    }
}

/// Mint a v4 UUID for a stream-block `id` when the agent didn't supply
/// one. Block ids are the stable anchors translation field-paths key off,
/// so they're generated once and never re-minted.
pub(crate) fn mint_uuid4() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = |r: std::ops::Range<usize>| {
        b[r].iter().fold(String::new(), |mut acc, x| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{x:02x}");
            acc
        })
    };
    format!(
        "{}-{}-{}-{}-{}",
        h(0..4),
        h(4..6),
        h(6..8),
        h(8..10),
        h(10..16)
    )
}

/// Render a stored extension/builder value as the form-string the save
/// pipeline expects. Checkbox semantics: `true` → `"on"`, `false`/null →
/// key omitted (`None`).
pub(crate) fn value_to_form_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(true) => Some("on".to_owned()),
        serde_json::Value::Bool(false) => None,
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        arr_or_obj => serde_json::to_string(arr_or_obj).ok(),
    }
}
