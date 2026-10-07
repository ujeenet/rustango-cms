//! Which role-matrix codename each admin route needs.
//!
//! The matrix in [`super::resources`] used to decide only which sidebar
//! links render: any user who could open the admin could POST to every
//! screen, so a Viewer could delete pages. [`required`] maps a request to
//! the codename the matrix already defines for it, and the admin router
//! enforces that on every matched route.
//!
//! The map is by URL section and verb:
//!
//! - `GET`/`HEAD` → `<resource>.view`
//! - `POST` ending in `/delete` → `.delete`
//! - `POST` that creates (`/new`, `/upload…`, `/clone`, `/import`,
//!   `/categories/new`) → `.add`
//! - `POST` to a page's `/publish` or `/unpublish` → `cms_page.publish`
//! - any other `POST` → `.edit`
//!
//! Sections with their own gate are left to it: users, roles and SSO
//! providers are superuser-only; the page-type builder, templates,
//! notifications and sites check their dedicated codenames. A section
//! that isn't listed needs only admin access.

use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};

use crate::permissions::Action;

/// URL section → role-matrix codename prefix. Longest prefix first:
/// `/cms-admin/media/collections` must win over `/cms-admin/media`.
const SECTIONS: &[(&str, &str)] = &[
    ("/cms-admin/media/collections", "cms_media_collection"),
    ("/cms-admin/media", "cms_media"),
    ("/cms-admin/documents", "cms_document"),
    ("/cms-admin/pages", "cms_page"),
    ("/cms-admin/library", "cms_library"),
    ("/cms-admin/forms", "cms_library"),
    ("/cms-admin/redirects", "cms_redirect"),
    ("/cms-admin/navigation", "cms_navigation_menu"),
    ("/cms-admin/workflows", "cms_workflow"),
    ("/cms-admin/locales", "cms_locale"),
    ("/cms-admin/site-settings", "cms_settings"),
    ("/cms-admin/settings", "cms_settings"),
    ("/cms-admin/history", "cms_history"),
    ("/cms-admin/reports", "cms_history"),
];

/// What a request needs: a codename, or — for a route on one page — a
/// page-level grant of `action` on that page as an alternative, so
/// per-subtree delegation keeps working.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Requirement {
    pub codename: String,
    pub page: Option<(i64, Action)>,
}

/// Router middleware: refuse a matched admin route whose [`required`]
/// codename the user lacks. Anonymous requests pass through to the
/// login bounce layered outside the router.
pub(crate) async fn gate(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let Some(requirement) = required(req.method(), req.uri().path()) else {
        return next.run(req).await;
    };
    let reads = req.method() == Method::GET || req.method() == Method::HEAD;
    let (mut parts, body) = req.into_parts();
    let allowed = match super::request_user_and_pool(&mut parts).await {
        Ok(None) => true,
        Ok(Some((user, pool))) => allows(&pool, &user, &requirement).await,
        Err(()) => false,
    };
    if allowed {
        return next.run(axum::extract::Request::from_parts(parts, body)).await;
    }
    if reads {
        Redirect::to("/cms-admin/no-access").into_response()
    } else {
        (
            StatusCode::FORBIDDEN,
            format!("This action needs the `{}` permission.", requirement.codename),
        )
            .into_response()
    }
}

/// Does `user` meet `req`? Superusers always do; otherwise the codename
/// from their roles, or the page-level grant when the route names a page.
/// A lookup failure denies.
pub(crate) async fn allows(
    pool: &rustango::sql::Pool,
    user: &rustango::tenancy::auth::User,
    req: &Requirement,
) -> bool {
    allows_id(pool, user.id.get().copied().unwrap_or_default(), user.is_superuser, req).await
}

/// [`allows`] for a caller that holds the user's id, not the row.
pub(crate) async fn allows_id(
    pool: &rustango::sql::Pool,
    uid: i64,
    is_superuser: bool,
    req: &Requirement,
) -> bool {
    if is_superuser {
        return true;
    }
    let has_code = crate::permissions::user_codenames(pool, uid)
        .await
        .map(|codes| codes.contains(&req.codename))
        .unwrap_or(false);
    if has_code {
        return true;
    }
    match req.page {
        Some((page_id, action)) => crate::permissions::user_can(pool, uid, page_id, action)
            .await
            .unwrap_or(false),
        None => false,
    }
}

/// The codename `method path` needs, or `None` when admin access is
/// enough.
pub(crate) fn required(method: &Method, path: &str) -> Option<Requirement> {
    let path = path.trim_end_matches('/');
    // Granting per-page permissions is security configuration, not
    // content editing.
    if path.starts_with("/cms-admin/pages/") && path.ends_with("/permissions") {
        return Some(Requirement {
            codename: "rustango_roles.edit".to_owned(),
            page: None,
        });
    }
    let (section, prefix) = SECTIONS
        .iter()
        .find(|(section, _)| path == *section || path.starts_with(&format!("{section}/")))?;
    let reads = method == Method::GET
        || method == Method::HEAD
        // Following a page and previewing unsaved changes read, not write.
        || ["/subscribe", "/unsubscribe", "/preview"].iter().any(|s| path.ends_with(s));
    let action = if reads {
        Action::View
    } else if path.ends_with("/delete") {
        Action::Delete
    } else if *prefix == "cms_page" && (path.ends_with("/publish") || path.ends_with("/unpublish"))
    {
        Action::Publish
    } else if ["/new", "/clone", "/import", "/upload", "/upload-staged", "/upload-staged/commit"]
        .iter()
        .any(|s| path.ends_with(s))
    {
        Action::Add
    } else {
        Action::Edit
    };
    let page = (*prefix == "cms_page")
        .then(|| {
            path[section.len()..]
                .trim_start_matches('/')
                .split('/')
                .next()
                .and_then(|seg| seg.parse::<i64>().ok())
        })
        .flatten()
        .map(|id| (id, action));
    Some(Requirement {
        codename: format!("{prefix}.{}", action.as_str()),
        page,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(method: Method, path: &str) -> Option<String> {
        required(&method, path).map(|r| r.codename)
    }

    #[test]
    fn verbs_map_onto_the_role_matrix() {
        assert_eq!(code(Method::GET, "/cms-admin/pages"), Some("cms_page.view".into()));
        assert_eq!(code(Method::POST, "/cms-admin/pages/7/edit"), Some("cms_page.edit".into()));
        assert_eq!(code(Method::POST, "/cms-admin/pages/7/delete"), Some("cms_page.delete".into()));
        assert_eq!(code(Method::POST, "/cms-admin/pages/new"), Some("cms_page.add".into()));
        assert_eq!(code(Method::POST, "/cms-admin/pages/7/clone"), Some("cms_page.add".into()));
        assert_eq!(code(Method::POST, "/cms-admin/pages/7/publish"), Some("cms_page.publish".into()));
        assert_eq!(
            code(Method::POST, "/cms-admin/pages/7/permissions"),
            Some("rustango_roles.edit".into())
        );
        assert_eq!(
            code(Method::POST, "/cms-admin/media/collections/3/delete"),
            Some("cms_media_collection.delete".into())
        );
        assert_eq!(code(Method::POST, "/cms-admin/media/upload"), Some("cms_media.add".into()));
        assert_eq!(code(Method::POST, "/cms-admin/redirects/import"), Some("cms_redirect.add".into()));
        assert_eq!(code(Method::POST, "/cms-admin/site-settings"), Some("cms_settings.edit".into()));
        assert_eq!(code(Method::POST, "/cms-admin/pages/7/subscribe"), Some("cms_page.view".into()));
        assert_eq!(code(Method::POST, "/cms-admin/pages/7/preview"), Some("cms_page.view".into()));
        assert_eq!(code(Method::GET, "/cms-admin/mediafoo"), None, "a section is a path segment");
        assert_eq!(code(Method::GET, "/cms-admin/dashboard"), None);
    }

    #[test]
    fn a_page_route_names_its_page_for_the_grant_fallback() {
        let r = required(&Method::POST, "/cms-admin/pages/42/edit").expect("gated");
        assert_eq!(r.page, Some((42, Action::Edit)));
        assert_eq!(required(&Method::POST, "/cms-admin/pages/bulk").unwrap().page, None);
    }
}
