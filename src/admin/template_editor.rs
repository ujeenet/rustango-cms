//! Edit a tenant's template overrides from the admin.
//!
//! The filesystem layer ([`crate::tenant_templates`]) already resolves a
//! tenant's overrides and reloads them without a restart; this is the UI
//! on top — list, edit, create, delete.
//!
//! ## Who may use it
//!
//! Gated on the [`CODENAME`] permission, with the usual superuser
//! bypass — the same shape as the page-type builder's
//! `cms_page_type.build`. Seeded onto **Developer** and
//! **Administrator**, deliberately not Editor or Viewer.
//!
//! A template is not content. Tera cannot call arbitrary Rust, spawn a
//! process or read the filesystem, so editing one does not hand out
//! remote code execution — but every function
//! and filter the host registered is callable from a template, and
//! `{% include %}` reaches anything in the instance. That is enough to
//! keep it off the roles that only touch content; turning in-browser
//! file editing off is standard hardening.
//!
//! ## Safety
//!
//! * every name goes through [`crate::tenant_templates::safe_name`], so
//!   a request cannot write outside the tenant's own directory;
//! * a body is parsed **before** it is written, so a syntax error is
//!   reported to the author rather than discovered by the next visitor
//!   (the renderer's per-template fallback stays as a safety net, not as
//!   the place you find out);
//! * deleting an override reverts that name to the global template — it
//!   never deletes a global one.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use rustango::extractors::{SessionUser, Tenant};
use serde::Deserialize;
use tera::Context;

use super::handlers::{self, add_admin_theme, add_chrome, render_with_csrf};
use super::AdminError;
use crate::tenant_templates::{self, EditError, Source};

/// `?name=` on the edit screen.
#[derive(Debug, Deserialize)]
pub(crate) struct NameQuery {
    #[serde(default)]
    pub name: String,
    /// Set after a successful save so the form can say so.
    #[serde(default)]
    pub saved: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SaveForm {
    pub name: String,
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct NameForm {
    pub name: String,
}

/// Permission required to read or write templates.
///
/// One codename covers both: someone who can see a template's source can
/// already read every other template it includes, so splitting view from
/// edit would be a distinction the templates themselves do not honour.
pub const CODENAME: &str = "cms_template.edit";

/// Deny unless the user is a superuser or holds [`CODENAME`].
///
/// `Some(redirect)` when denied — anonymous to the login page, everyone
/// else to no-access, so a signed-in user without the permission is not
/// bounced through a login form that would not help them.
async fn ensure_access(
    tenant: &Tenant,
    user: Option<&rustango::tenancy::auth::User>,
) -> Option<Response> {
    super::codename_gate(tenant, user, CODENAME).await
}

/// The configured store, or a rendered "not configured" page.
fn store() -> Result<std::sync::Arc<tenant_templates::TenantTemplates>, ()> {
    tenant_templates::installed().ok_or(())
}

/// `GET /cms-admin/templates`
pub(crate) async fn list(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    SessionUser(session_user): SessionUser,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "templates", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;

    match store() {
        Err(()) => ctx.insert("not_configured", &true),
        Ok(tt) => {
            let rows: Vec<serde_json::Value> = tt
                .list_for(&tenant.org.slug)
                .into_iter()
                .map(|e| {
                    serde_json::json!({
                        "name": e.name,
                        "overridden": e.source == Source::Override,
                        "bytes": e.bytes,
                    })
                })
                .collect();
            let overridden = rows.iter().filter(|r| r["overridden"] == true).count();
            ctx.insert("not_configured", &false);
            ctx.insert("rows", &rows);
            ctx.insert("overridden_count", &overridden);
            ctx.insert("tenant_slug", &tenant.org.slug);
        }
    }
    render_with_csrf(&state, &headers, "rcms_admin/template_list.html", &mut ctx)
}

/// `GET /cms-admin/templates/edit?name=…`
pub(crate) async fn edit_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    SessionUser(session_user): SessionUser,
    Query(q): Query<NameQuery>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let Ok(tt) = store() else {
        return Ok(Redirect::to("/cms-admin/templates").into_response());
    };
    let Ok(name) = tenant_templates::safe_name(&q.name) else {
        return Ok(Redirect::to("/cms-admin/templates").into_response());
    };

    let slug = &tenant.org.slug;
    let own = tt.read_override(slug, &name);
    let global = tt.read_global(&name);
    // Editing an inherited template starts from the global body, so
    // "customise this" is a copy-and-change rather than a blank page.
    let body = own.clone().or_else(|| global.clone()).unwrap_or_default();

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "templates", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("name", &name);
    ctx.insert("body", &body);
    ctx.insert("is_override", &own.is_some());
    ctx.insert("has_global", &global.is_some());
    ctx.insert("saved", &q.saved.is_some());
    ctx.insert("tenant_slug", slug);
    render_with_csrf(&state, &headers, "rcms_admin/template_form.html", &mut ctx)
}

/// `POST /cms-admin/templates/edit`
pub(crate) async fn save(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    SessionUser(session_user): SessionUser,
    Form(form): Form<SaveForm>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let Ok(tt) = store() else {
        return Ok(Redirect::to("/cms-admin/templates").into_response());
    };
    let slug = &tenant.org.slug;

    match tt.write_override(slug, &form.name, &form.body) {
        Ok(()) => Ok(Redirect::to(&format!(
            "/cms-admin/templates/edit?name={}&saved=1",
            urlencode(&form.name)
        ))
        .into_response()),
        // Re-render with the author's text intact and the parse error
        // shown. Redirecting would throw away what they typed.
        Err(e) => {
            let mut ctx = Context::new();
            add_chrome(&mut ctx, &tenant, "templates", session_user.as_ref()).await;
            add_admin_theme(&mut ctx, tenant.pool()).await;
            ctx.insert("name", &form.name);
            ctx.insert("body", &form.body);
            ctx.insert("is_override", &tt.read_override(slug, &form.name).is_some());
            ctx.insert("has_global", &tt.read_global(&form.name).is_some());
            ctx.insert("saved", &false);
            ctx.insert("error", &error_text(&e));
            ctx.insert("tenant_slug", slug);
            render_with_csrf(&state, &headers, "rcms_admin/template_form.html", &mut ctx)
        }
    }
}

/// `POST /cms-admin/templates/validate` — check without saving.
///
/// Saving already refuses a body that will not parse, so this is not the
/// safety net; it is the feedback loop. Finding out at save time means
/// a full round trip and a re-render before you learn you mis-typed a
/// tag, and the button lets an author check as they go.
///
/// Answers JSON so the editor can show the result in place rather than
/// reloading and losing the cursor.
pub(crate) async fn validate(
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    Form(form): Form<SaveForm>,
) -> Response {
    if ensure_access(&tenant, session_user.as_ref()).await.is_some() {
        return (
            axum::http::StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "ok": false, "error": "not permitted" })),
        )
            .into_response();
    }
    let Ok(tt) = store() else {
        return axum::Json(serde_json::json!({
            "ok": false,
            "error": "per-tenant templates are not configured",
        }))
        .into_response();
    };
    match tt.validate(&tenant.org.slug, &form.name, &form.body) {
        Ok(()) => axum::Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => {
            axum::Json(serde_json::json!({ "ok": false, "error": e.to_string() })).into_response()
        }
    }
}

/// `POST /cms-admin/templates/new`
pub(crate) async fn create(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    SessionUser(session_user): SessionUser,
    Form(form): Form<NameForm>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    // Creating is just editing a name that has no override yet — the
    // form seeds from the global body when one exists.
    match tenant_templates::safe_name(&form.name) {
        Ok(name) => Ok(Redirect::to(&format!(
            "/cms-admin/templates/edit?name={}",
            urlencode(&name)
        ))
        .into_response()),
        Err(_) => Ok(Redirect::to("/cms-admin/templates?bad_name=1").into_response()),
    }
}

/// `POST /cms-admin/templates/delete`
pub(crate) async fn delete(
    State(_state): State<super::AdminState>,
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    SessionUser(session_user): SessionUser,
    Form(form): Form<NameForm>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let Ok(tt) = store() else {
        return Ok(Redirect::to("/cms-admin/templates").into_response());
    };

    // Refuse a delete that would leave a page type pointing at nothing.
    //
    // Assigning an unresolvable template is already refused; deleting the
    // template out from under an assignment breaks the same invariant
    // from the other side, and it is worse because it is silent —
    // measured before this guard existed, every page of the affected type
    // answered 500.
    //
    // Only when there is no global fallback: reverting an override that
    // shadows a global template is exactly what Revert is for, and the
    // type keeps rendering with the inherited version.
    if tt.read_global(&form.name).is_none() {
        let dependents = page_types_using(&tenant, &form.name).await?;
        if !dependents.is_empty() {
            let types = dependents.join(", ");
            return handlers::redirect_named_with_params_and_message_args(
                "rcms-admin:templates:list",
                &[],
                rustango::messages::Level::Error,
                "Cannot remove {name} — {types} render with it and nothing would replace it. Point those page types elsewhere first.",
                &[("name", form.name.as_str()), ("types", types.as_str())],
                &headers,
            );
        }
    }

    if let Err(e) = tt.delete_override(&tenant.org.slug, &form.name) {
        tracing::warn!(
            target: "rustango_cms::admin",
            template = %form.name, error = %e,
            "could not remove tenant template override",
        );
    }
    Ok(Redirect::to("/cms-admin/templates").into_response())
}

/// Page types whose `default_template` is `name`, by verbose name.
///
/// Used to refuse a delete that would strand them.
async fn page_types_using(tenant: &Tenant, name: &str) -> Result<Vec<String>, AdminError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<crate::page_type_model::PageType> = crate::page_type_model::PageType::objects()
        .where_(crate::page_type_model::PageType::default_template.eq(name.to_owned()))
        .fetch(tenant.pool())
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            if r.verbose_name.trim().is_empty() {
                r.type_name
            } else {
                r.verbose_name
            }
        })
        .collect())
}

fn error_text(e: &EditError) -> String {
    e.to_string()
}

/// Minimal percent-encoding for a template name in a redirect target.
///
/// Names are already restricted to `[A-Za-z0-9._/-]`-ish by `safe_name`,
/// but `/` still has to travel as `%2F` in a query value.
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ============================================================
// Assigning a template to a page type.
//
// `render.rs` renders `page_type.default_template`, so which file a page
// type uses has always been data — but the row was written once at
// creation and shown read-only, so there was no way to point a type at a
// template you had just written. This is that missing half: create a
// file in the editor above, then assign it here.
// ============================================================

#[derive(Debug, Deserialize)]
pub(crate) struct TemplateAssignForm {
    /// Chosen from the picker, or typed in. Empty means "no HTML
    /// representation" — see the note on `ApiOnly` below.
    #[serde(default)]
    pub template: String,
}

/// `GET /cms-admin/page-types/{id}/template`
pub(crate) async fn assign_form(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    SessionUser(session_user): SessionUser,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let Some(row) = load_type(&tenant, id).await? else {
        return Ok(Redirect::to("/cms-admin/page-types").into_response());
    };

    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "page-types", session_user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("type_name", &row.type_name);
    ctx.insert("verbose_name", &row.verbose_name);
    ctx.insert("type_id", &id);
    ctx.insert("current", &row.default_template);
    ctx.insert("choices", &renderable_templates(&tenant, &row.default_template));
    ctx.insert(
        "is_api_only",
        &matches!(
            crate::page_view::kind_for(&row),
            crate::page_view::PageViewKind::ApiOnly
        ),
    );
    render_with_csrf(
        &state,
        &headers,
        "rcms_admin/page_type_template_form.html",
        &mut ctx,
    )
}

/// `POST /cms-admin/page-types/{id}/template`
pub(crate) async fn assign_save(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    SessionUser(session_user): SessionUser,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Form(form): Form<TemplateAssignForm>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_access(&tenant, session_user.as_ref()).await {
        return Ok(r);
    }
    let Some(mut row) = load_type(&tenant, id).await? else {
        return Ok(Redirect::to("/cms-admin/page-types").into_response());
    };
    let chosen = form.template.trim().to_owned();

    // A name the renderer cannot resolve would 500 every page of this
    // type, so refuse it here rather than at the visitor's expense.
    // Checked against the tenant's *Tera instance*, not the filesystem:
    // that also covers templates the host registered in code, which have
    // no file to stat.
    if !chosen.is_empty()
        && !renderable_templates(&tenant, &row.default_template)
            .iter()
            .any(|t| t == &chosen)
    {
        let mut ctx = Context::new();
        add_chrome(&mut ctx, &tenant, "page-types", session_user.as_ref()).await;
        add_admin_theme(&mut ctx, tenant.pool()).await;
        ctx.insert("type_name", &row.type_name);
        ctx.insert("verbose_name", &row.verbose_name);
        ctx.insert("type_id", &id);
        ctx.insert("current", &row.default_template);
        ctx.insert("choices", &renderable_templates(&tenant, &row.default_template));
        ctx.insert("is_api_only", &false);
        ctx.insert("error", &chosen);
        return render_with_csrf(
            &state,
            &headers,
            "rcms_admin/page_type_template_form.html",
            &mut ctx,
        );
    }

    row.default_template = chosen;
    row.save_pool(tenant.pool()).await?;
    Ok(Redirect::to("/cms-admin/page-types").into_response())
}

async fn load_type(
    tenant: &Tenant,
    id: i64,
) -> Result<Option<crate::page_type_model::PageType>, AdminError> {
    use rustango::core::Column as _;
    Ok(crate::page_type_model::PageType::objects()
        .where_(crate::page_type_model::PageType::id.eq(id))
        .first(tenant.pool())
        .await?)
}

/// Every template name this tenant can actually render, as offered by the
/// picker.
///
/// Taken from the tenant's own Tera instance rather than a directory
/// listing, so it includes host-registered templates that exist only in
/// the binary alongside the ones on disk — and it is exactly the set the
/// renderer will look in.
///
/// Admin chrome is filtered out: `rcms_admin/…` renders the CMS UI and
/// assigning one to a page type would produce nonsense. **Except**
/// `schema_page.html`, which lives in that namespace but is a public
/// page template — it is the generic renderer every builder-created page
/// type gets by default.
///
/// `current` is always included even if it would otherwise be filtered.
/// Without that the `<select>` cannot round-trip: a type whose template
/// is absent from the options renders with the *first* option selected —
/// "— none —" — so opening the form and pressing Save silently strips
/// the type's HTML rendering. Whatever a type is set to, the form must
/// be able to show it and hand it back unchanged.
fn renderable_templates(tenant: &Tenant, current: &str) -> Vec<String> {
    let Some(tt) = tenant_templates::installed() else {
        // Even with no store, the form must be able to represent what the
        // row already holds.
        return if current.is_empty() {
            Vec::new()
        } else {
            vec![current.to_owned()]
        };
    };
    let mut names: Vec<String> = tt
        .for_tenant(&tenant.org.slug)
        .get_template_names()
        .filter(|n| offerable(n) || *n == current)
        .map(str::to_owned)
        .collect();
    if !current.is_empty() && !names.iter().any(|n| n == current) {
        names.push(current.to_owned());
    }
    names.sort();
    names.dedup();
    names
}

/// Whether a template is a sensible thing to offer for a page type.
pub(crate) fn offerable(name: &str) -> bool {
    if name == crate::page_builder::DEFAULT_SCHEMA_TEMPLATE {
        return true;
    }
    !name.starts_with("rcms_admin/") && !name.starts_with("rcms/")
}

#[cfg(test)]
mod tests {
    use super::offerable;

    #[test]
    fn admin_chrome_is_not_offered_for_a_page_type() {
        // Assigning the CMS's own UI to a page type would render nonsense.
        assert!(!offerable("rcms_admin/_base.html"));
        assert!(!offerable("rcms_admin/page_form.html"));
        assert!(!offerable("rcms/view_password.html"));
    }

    #[test]
    fn the_builder_default_is_offered_despite_its_namespace() {
        // `schema_page.html` lives under `rcms_admin/` but is a public
        // page template — the generic renderer every builder-created page
        // type gets. Filtering the namespace blindly hid it, so the
        // picker showed "— none —" for those types and a Save would have
        // silently stripped their HTML rendering.
        assert!(offerable(crate::page_builder::DEFAULT_SCHEMA_TEMPLATE));
        assert!(offerable("rcms_admin/schema_page.html"));
    }

    #[test]
    fn ordinary_site_templates_are_offered() {
        assert!(offerable("landing.html"));
        assert!(offerable("blocks/hero.html"));
    }
}
