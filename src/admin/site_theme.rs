//! Settings → Site theme: download this site as a theme package, install
//! one, and put the previous look back.
//!
//! The package format and what installing does are in
//! [`crate::starter::package`]. Here the templates go to this site's own
//! template folder ([`crate::tenant_templates`]), so they take effect
//! without a restart and other sites on the server keep their look.
//!
//! ## Who may use it
//!
//! Superusers only. Installing writes templates, which run on the server
//! with every function the host registered, and changes page types and
//! colours for the whole site.
//!
//! ## Flow
//!
//! Upload → preview → install. The upload stores the file in the system
//! temp folder under a random token and redirects to the preview, which
//! shows exactly what will change before anything does; the install form
//! carries the token. (The admin's script submits multipart forms with
//! `fetch` to add the CSRF header, then follows the redirect.)

use std::path::PathBuf;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::Form;
use rustango::core::Column as _;
use rustango::extractors::{SessionUser, Tenant};
use rustango::messages::Level as MsgLevel;
use rustango::sql::FetcherPool as _;
use serde::Deserialize;
use serde_json::json;
use tera::Context;

use super::handlers::{add_admin_theme, add_chrome, redirect_named_with_params_and_message, render_with_csrf};
use super::AdminError;
use crate::page::Page;
use crate::page_type_model::PageType;
use crate::starter::{self, package, Outcome};
use crate::tenant_templates::{self, TenantTemplates};
use crate::theme::Theme;

/// Site setting recording the theme that was the default before the last
/// install, so "Restore previous look" can bring its colours back too.
const PREVIOUS_SCOPE: &str = "starter_previous";

#[derive(Debug, Deserialize)]
pub(crate) struct DownloadQuery {
    #[serde(default)]
    pub pages: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TokenForm {
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TokenQuery {
    #[serde(default)]
    pub token: String,
}

/// Deny anyone but a superuser: anonymous to the login page, everyone
/// else to no-access.
fn ensure_superuser(user: Option<&rustango::tenancy::auth::User>) -> Option<Response> {
    use axum::response::Redirect;
    match user {
        None => Some(Redirect::to("/login").into_response()),
        Some(u) if u.is_superuser => None,
        Some(_) => Some(Redirect::to("/cms-admin/no-access").into_response()),
    }
}

fn store() -> Option<std::sync::Arc<TenantTemplates>> {
    tenant_templates::installed()
}

/// `GET /cms-admin/site-theme`
pub(crate) async fn index(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    SessionUser(user): SessionUser,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_superuser(user.as_ref()) {
        return Ok(r);
    }
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "site-theme", user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    let pool = tenant.pool();
    let applied = crate::site_setting::get(pool, starter::MARKER_SCOPE).await?.map(|s| s.value_json);
    ctx.insert("applied", &applied);
    let colours = Theme::objects().where_(Theme::is_default.eq(true)).first(pool).await?;
    ctx.insert("colours", &colours.map(|t| t.name));
    match store() {
        None => ctx.insert("not_configured", &true),
        Some(tt) => {
            let slug = &tenant.org.slug;
            let customised = tt
                .list_for(slug)
                .iter()
                .filter(|e| e.source == tenant_templates::Source::Override)
                .count();
            ctx.insert("not_configured", &false);
            ctx.insert("customised_count", &customised);
            ctx.insert("can_restore", &tt.has_snapshot(slug));
        }
    }
    render_with_csrf(&state, &headers, "rcms_admin/site_theme.html", &mut ctx)
}

/// `GET /cms-admin/site-theme/download?pages=1` — this site as a package.
pub(crate) async fn download(
    tenant: Tenant,
    SessionUser(user): SessionUser,
    Query(q): Query<DownloadQuery>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_superuser(user.as_ref()) {
        return Ok(r);
    }
    let tt = store().ok_or_else(|| AdminError::Validation("Per-site templates are not configured.".into()))?;
    let slug = tenant.org.slug.clone();
    let with_pages = q.pages.is_some_and(|v| !v.is_empty() && v != "0");
    let seed = package::export(tenant.pool(), &tenant.org.display_name, with_pages)
        .await
        .map_err(|e| AdminError::Validation(e.to_string()))?;
    // Every template this site renders with: its own, and the ones it
    // inherits from the project.
    let templates: Vec<(String, String)> = tt
        .list_for(&slug)
        .into_iter()
        .filter_map(|e| {
            let body = tt.read_override(&slug, &e.name).or_else(|| tt.read_global(&e.name))?;
            Some((e.name, body))
        })
        .collect();
    let bytes = package::pack_bytes(&seed, &templates).map_err(|e| AdminError::Validation(e.to_string()))?;
    let file = format!("{slug}-theme{}.zip", if with_pages { "-with-pages" } else { "" });
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_owned()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{file}\"")),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        bytes,
    )
        .into_response())
}

/// `POST /cms-admin/site-theme/upload` — keep the uploaded package and
/// go to its preview. Nothing changes yet.
pub(crate) async fn upload(
    tenant: Tenant,
    headers: HeaderMap,
    SessionUser(user): SessionUser,
    mut multipart: axum::extract::Multipart,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_superuser(user.as_ref()) {
        return Ok(r);
    }
    let mut bytes = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AdminError::Upload(format!("multipart parse: {e}")))?
    {
        if field.name() == Some("package") {
            bytes = field
                .bytes()
                .await
                .map_err(|e| AdminError::Upload(format!("read bytes: {e}")))?
                .to_vec();
        } else {
            let _ = field.bytes().await;
        }
    }
    if bytes.is_empty() {
        return redirect_named_with_params_and_message(
            "rcms-admin:site-theme",
            &[],
            MsgLevel::Error,
            "Choose a theme package (.zip) to upload.",
            &headers,
        );
    }
    let token = new_token();
    std::fs::write(upload_path(&tenant.org.slug, &token), &bytes).map_err(|e| AdminError::Upload(e.to_string()))?;
    let url = format!(
        "{}?token={token}",
        rustango::urls::reverse_owned("rcms-admin:site-theme:preview", &std::collections::HashMap::new())
            .map_err(|e| AdminError::Validation(e.to_string()))?
    );
    Ok(axum::response::Redirect::to(&url).into_response())
}

/// `GET /cms-admin/site-theme/preview?token=…` — what installing the
/// uploaded package would do.
pub(crate) async fn preview_page(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    SessionUser(user): SessionUser,
    Query(q): Query<TokenQuery>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_superuser(user.as_ref()) {
        return Ok(r);
    }
    let bytes = valid_token(&q.token)
        .then(|| std::fs::read(upload_path(&tenant.org.slug, &q.token)).ok())
        .flatten();
    let Some(bytes) = bytes else {
        return redirect_named_with_params_and_message(
            "rcms-admin:site-theme",
            &[],
            MsgLevel::Error,
            "That upload has expired. Upload the package again.",
            &headers,
        );
    };
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "site-theme", user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    match preview(&tenant, &bytes).await {
        Ok(p) => {
            ctx.insert("token", &q.token);
            ctx.insert("preview", &p);
            ctx.insert("problem", &Option::<String>::None);
        }
        Err(problem) => {
            ctx.insert("problem", &Some(problem));
            ctx.insert("preview", &serde_json::Value::Null);
        }
    }
    render_with_csrf(&state, &headers, "rcms_admin/site_theme_preview.html", &mut ctx)
}

/// `POST /cms-admin/site-theme/install` — install the previewed package.
pub(crate) async fn install(
    tenant: Tenant,
    headers: HeaderMap,
    SessionUser(user): SessionUser,
    Form(form): Form<TokenForm>,
) -> Result<Response, AdminError> {
    if let Some(r) = ensure_superuser(user.as_ref()) {
        return Ok(r);
    }
    let tt = store().ok_or_else(|| AdminError::Validation("Per-site templates are not configured.".into()))?;
    let slug = tenant.org.slug.clone();
    if !valid_token(&form.token) {
        return Err(AdminError::Validation("Unknown upload.".into()));
    }
    let path = upload_path(&slug, &form.token);
    let Ok(bytes) = std::fs::read(&path) else {
        return redirect_named_with_params_and_message(
            "rcms-admin:site-theme",
            &[],
            MsgLevel::Error,
            "That upload has expired. Upload the package again.",
            &headers,
        );
    };
    let pkg = package::read_bytes(&bytes).map_err(|e| AdminError::Validation(e.to_string()))?;
    starter::parse(&pkg.seed).map_err(|e| AdminError::Validation(e.to_string()))?;

    // Remember the current look first: templates and colours.
    let pool = tenant.pool();
    tt.snapshot(&slug).map_err(|e| AdminError::Validation(e.to_string()))?;
    let previous = Theme::objects().where_(Theme::is_default.eq(true)).first(pool).await?;
    if let Some(prev) = previous.and_then(|t| t.id.get().copied()) {
        crate::site_setting::upsert(pool, PREVIOUS_SCOPE, json!({ "theme_id": prev })).await?;
    }

    tt.write_set(&slug, &pkg.templates)
        .map_err(|e| AdminError::Validation(format!("The package's templates were not installed: {e}")))?;
    let outcome = package::install(pool, &pkg.seed)
        .await
        .map_err(|e| AdminError::Validation(e.to_string()))?;
    let _ = std::fs::remove_file(&path);

    let message = match outcome {
        Outcome::Applied { .. } => "Theme installed: pages, menus and colours were created.",
        Outcome::Switched { error_pages_added, .. } if error_pages_added > 0 => {
            "Theme installed, with its error pages. Your other pages, menus and settings are unchanged."
        }
        _ => "Theme installed. Your pages, menus and settings are unchanged.",
    };
    redirect_named_with_params_and_message("rcms-admin:site-theme", &[], MsgLevel::Success, message, &headers)
}

/// `POST /cms-admin/site-theme/restore` — swap back to the templates and
/// colours from before the last install.
pub(crate) async fn restore(tenant: Tenant, headers: HeaderMap, SessionUser(user): SessionUser) -> Result<Response, AdminError> {
    if let Some(r) = ensure_superuser(user.as_ref()) {
        return Ok(r);
    }
    let tt = store().ok_or_else(|| AdminError::Validation("Per-site templates are not configured.".into()))?;
    let slug = tenant.org.slug.clone();
    tt.restore_snapshot(&slug).map_err(|e| AdminError::Validation(e.to_string()))?;

    let pool = tenant.pool();
    let current = Theme::objects().where_(Theme::is_default.eq(true)).first(pool).await?;
    if let Some(prev_id) = crate::site_setting::get(pool, PREVIOUS_SCOPE)
        .await?
        .and_then(|s| s.value_json["theme_id"].as_i64())
    {
        set_default_theme(pool, prev_id).await?;
        // Restoring again swaps back, colours included.
        if let Some(cur) = current.and_then(|t| t.id.get().copied()) {
            crate::site_setting::upsert(pool, PREVIOUS_SCOPE, json!({ "theme_id": cur })).await?;
        }
    }
    redirect_named_with_params_and_message(
        "rcms-admin:site-theme",
        &[],
        MsgLevel::Success,
        "The previous look is back.",
        &headers,
    )
}

/// What installing `bytes` would do, for the preview page; a sentence
/// describing the problem when the package can't be installed.
async fn preview(tenant: &Tenant, bytes: &[u8]) -> Result<serde_json::Value, String> {
    let pkg = package::read_bytes(bytes).map_err(|e| e.to_string())?;
    let seed = starter::parse(&pkg.seed).map_err(|e| e.to_string())?;
    let pool = tenant.pool();
    let site_is_empty = crate::site_setting::get(pool, starter::MARKER_SCOPE)
        .await
        .map_err(|e| e.to_string())?
        .is_none()
        && Page::objects().first(pool).await.map_err(|e| e.to_string())?.is_none();

    let existing: Vec<PageType> = PageType::objects().fetch(pool).await.map_err(|e| e.to_string())?;
    let types: Vec<serde_json::Value> = seed
        .page_types
        .iter()
        .map(|pt| {
            let state = match existing.iter().find(|e| e.type_name == pt.type_name) {
                None => "new",
                Some(e) if e.app_label == "cms_ui" => "restyled",
                Some(_) => "kept",
            };
            json!({ "name": pt.verbose_name, "type_name": pt.type_name, "state": state })
        })
        .collect();

    let slug = &tenant.org.slug;
    let current: Vec<String> = store().map(|tt| tt.list_for(slug).into_iter().map(|e| e.name).collect()).unwrap_or_default();
    let templates: Vec<serde_json::Value> = pkg
        .templates
        .iter()
        .map(|(name, _)| json!({ "name": name, "replaces": current.contains(name) }))
        .collect();

    // Pages already on the site whose template neither the package nor
    // the site will have.
    let used: std::collections::HashSet<i64> = Page::objects()
        .fetch(pool)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|p| p.page_type_id)
        .collect();
    let missing: Vec<String> = existing
        .iter()
        .filter(|t| t.id.get().is_some_and(|id| used.contains(id)))
        .filter(|t| !t.default_template.is_empty() && !t.default_template.starts_with("rcms_admin/"))
        .filter(|t| {
            let incoming = seed
                .page_types
                .iter()
                .find(|p| p.type_name == t.type_name && t.app_label == "cms_ui")
                .map_or(t.default_template.as_str(), |p| p.template.as_str());
            !current.iter().any(|n| n == incoming) && !pkg.templates.iter().any(|(n, _)| n == incoming)
        })
        .map(|t| t.verbose_name.clone())
        .collect();

    Ok(json!({
        "name": seed.name,
        "site_is_empty": site_is_empty,
        "page_count": count_pages(&seed.pages),
        "menu_count": seed.menus.len(),
        "has_colours": seed.theme.is_some(),
        "types": types,
        "templates": templates,
        "missing": missing,
    }))
}

fn count_pages(pages: &[starter::PageSeed]) -> usize {
    pages.iter().map(|p| 1 + count_pages(&p.children)).sum()
}

async fn set_default_theme(pool: &rustango::sql::Pool, id: i64) -> Result<(), AdminError> {
    for mut t in Theme::objects().fetch(pool).await? {
        let is_it = t.id.get().copied() == Some(id);
        if t.is_default != is_it {
            t.is_default = is_it;
            t.save(pool).await?;
        }
    }
    for mut root in Page::objects().where_(Page::depth.eq(1)).fetch(pool).await? {
        root.theme_id = Some(id);
        root.save(pool).await?;
    }
    Ok(())
}

fn new_token() -> String {
    let mut buf = [0u8; 16];
    // A failed read of the OS source leaves zeros; the slug still keeps
    // uploads of different sites apart, and the next upload overwrites.
    let _ = getrandom::fill(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn valid_token(token: &str) -> bool {
    token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

fn upload_path(slug: &str, token: &str) -> PathBuf {
    let safe: String = slug.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    std::env::temp_dir().join(format!("rcms-theme-{safe}-{token}.zip"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_32_hex_and_nothing_else_is_accepted() {
        let t = new_token();
        assert!(valid_token(&t), "{t}");
        for bad in ["", "../../etc/passwd", "zz000000000000000000000000000000", &"a".repeat(33)] {
            assert!(!valid_token(bad), "{bad}");
        }
    }
}
