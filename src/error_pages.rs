//! CMS-editable error pages (404/500/…).
//!
//! Editors create a page of the built-in **Error page** type, pick the
//! HTTP status it serves (404/403/500/503) and compose its body from
//! content blocks. The public router substitutes that page — rendered
//! through the full pipeline (navbar via `auto_menu`, footer, theme,
//! `_stream_html`) — for the previously plain-text error responses,
//! keeping the real error status code on the wire.
//!
//! When no published Error page exists for a status (or the error page
//! itself fails to render), `fallback_response` serves a bundled,
//! fully self-contained HTML page — no Tera, no DB — so the error path
//! can never recurse into another error.
//!
//! The extension table is created at boot via `CREATE TABLE IF NOT
//! EXISTS` (same pattern as [`crate::forms::submit::ensure_table`])
//! because repo-wide `gen_migration` is blocked by pre-existing
//! framework FK-snapshot drift.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rustango::core::Column as _;
use rustango::core::Model as _;
use rustango::extractors::Tenant;
use rustango::sql::FetcherPool as _;
use rustango::sql::{Auto, ExecError, Pool};
use rustango::Model;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::page::Page;

/// Registry `type_name` for the built-in Error page type.
pub const TYPE_NAME: &str = "ErrorPage";

/// Typed extension row for an Error page: the HTTP status it serves
/// plus its block-stream body.
#[derive(Model, Debug, Clone, Default, Serialize, Deserialize)]
#[rustango(table = "cms_error_page", app = "cms")]
pub struct ErrorPageExt {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// Owning `cms_page` row — one extension row per page.
    #[rustango(fk = "cms_page", on = "id", unique)]
    pub page_id: i64,
    /// HTTP status this page is served for: `"404"` / `"403"` /
    /// `"500"` / `"503"`. Stored as text to match the Select widget's
    /// form value round-trip.
    #[rustango(max_length = 8, index)]
    pub status_code: String,
    /// Block-stream JSON (array of `{type, value}` objects).
    pub body: Option<String>,
}

/// Create `cms_error_page` if absent. Idempotent; called per tenant at
/// boot from [`crate::seed::ensure_seeded`].
///
/// # Errors
/// Driver / DDL failures.
pub async fn ensure_table(pool: &Pool) -> Result<(), ExecError> {
    let ddl = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
        pool.dialect(),
        &ErrorPageExt::SCHEMA,
    );
    for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new()).await?;
    }
    Ok(())
}

/// The built-in Error page type. Hand-rolled (not `#[derive(PageType)]`
/// — the derive emits `::rustango_cms::` paths that don't resolve from
/// inside this crate).
#[derive(Default)]
pub struct ErrorPage;

#[async_trait::async_trait]
impl crate::PageTypeHandler for ErrorPage {
    fn app_label(&self) -> &'static str {
        "cms"
    }
    fn type_name(&self) -> &'static str {
        TYPE_NAME
    }
    fn verbose_name(&self) -> &'static str {
        "Error page"
    }
    fn default_template(&self) -> &'static str {
        "error_page.html"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("error")
    }
    fn description(&self) -> Option<&'static str> {
        Some(
            "Served in place of a plain error when a URL is missing (404) or the \
             server fails (500). Tip: untick 'Allow search indexing' under Promote.",
        )
    }
    fn is_leaf(&self) -> bool {
        true
    }

    async fn widgets(&self, pool: &Pool, page_id: i64) -> Result<Vec<crate::Widget>, ExecError> {
        let row = fetch_ext(pool, page_id).await?;
        let (code, body) = row
            .map(|r| (r.status_code, r.body.unwrap_or_default()))
            .unwrap_or_else(|| ("404".to_owned(), String::new()));
        Ok(vec![
            crate::Widget::new(
                crate::widget::WidgetKind::Select,
                "status_code",
                "Status code",
            )
            .with_options([
                ("404", "404 — Not Found"),
                ("403", "403 — Forbidden"),
                ("500", "500 — Server Error"),
                ("503", "503 — Service Unavailable"),
            ])
            .required()
            .with_value(code)
            .with_help("HTTP status this page is served for."),
            crate::Widget::stream(
                "body",
                "Body",
                [
                    "heading",
                    "paragraph",
                    "image",
                    "quote",
                    "embed",
                    "code",
                    "raw_html",
                ],
            )
            .with_value(body),
        ])
    }

    async fn load_extension(&self, pool: &Pool, page_id: i64) -> Result<Value, ExecError> {
        let row = fetch_ext(pool, page_id).await?;
        Ok(match row {
            // `body` stays a raw JSON string — `canonical_parsed` in the
            // stream prerenderer parses either shape.
            Some(r) => json!({ "status_code": r.status_code, "body": r.body }),
            None => json!({ "status_code": "404", "body": Value::Null }),
        })
    }

    async fn save_extension(
        &self,
        pool: &Pool,
        page_id: i64,
        form: &std::collections::HashMap<String, String>,
    ) -> Result<(), ExecError> {
        let status_code = form
            .get("status_code")
            .map(String::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("404")
            .to_owned();
        let body = form.get("body").filter(|s| !s.trim().is_empty()).cloned();
        match fetch_ext(pool, page_id).await? {
            Some(mut row) => {
                row.status_code = status_code;
                row.body = body;
                row.save_pool(pool).await?;
            }
            None => {
                let mut row = ErrorPageExt {
                    id: Auto::Unset,
                    page_id,
                    status_code,
                    body,
                };
                row.insert_pool(pool).await?;
            }
        }
        Ok(())
    }
}

crate::register_page_type!(ErrorPage);

async fn fetch_ext(pool: &Pool, page_id: i64) -> Result<Option<ErrorPageExt>, ExecError> {
    Ok(ErrorPageExt::objects()
        .where_(ErrorPageExt::page_id.eq(page_id))
        .fetch(pool)
        .await?
        .into_iter()
        .next())
}

/// The tenant's `cms_page_type.id` for the built-in Error page type,
/// if seeded. Used to exclude error pages from surfaces that walk the
/// published tree (sitemap).
pub(crate) async fn error_page_type_id(pool: &Pool) -> Option<i64> {
    crate::page_type_model::PageType::objects()
        .where_(crate::page_type_model::PageType::type_name.eq(TYPE_NAME.to_owned()))
        .first(pool)
        .await
        .ok()
        .flatten()
        .and_then(|pt| pt.id.get().copied())
}

/// Find the published Error page configured for `code`, if any.
/// Lowest `page_id` wins when several published pages claim the same
/// status (deterministic, oldest-first).
///
/// # Errors
/// Driver / query failures.
pub async fn find_for_status(pool: &Pool, code: u16) -> Result<Option<Page>, ExecError> {
    let mut exts: Vec<ErrorPageExt> = ErrorPageExt::objects()
        .where_(ErrorPageExt::status_code.eq(code.to_string()))
        .fetch(pool)
        .await?;
    exts.sort_by_key(|e| e.page_id);
    for ext in exts {
        let page = Page::objects()
            .where_(Page::id.eq(ext.page_id))
            // Served, as the resolver judges it: an archived error page
            // still answers (#763).
            .where_(Page::status.is_in(crate::resolver::served_statuses()))
            .first(pool)
            .await?
            .filter(|p| crate::resolver::visible_now(p, chrono::Utc::now()));
        if page.is_some() {
            return Ok(page);
        }
    }
    Ok(None)
}

/// Serve the error response for `status`: the editor-built Error page
/// rendered through the full public pipeline when one exists and
/// renders cleanly, else the static `fallback_response`.
///
/// The error page render is only accepted when it comes back `200 OK`
/// — anything else (including a failure inside the error page itself)
/// drops to the Tera-free fallback, so this path cannot recurse.
pub(crate) async fn respond(
    tenant: &Tenant,
    state: &crate::router::PublicState,
    status: StatusCode,
    locale_code: Option<&str>,
) -> Response {
    match find_for_status(tenant.pool(), status.as_u16()).await {
        Ok(Some(page)) => {
            // Same stash-and-hold contract as handle_page_request: the
            // guard must live across the render so the language
            // switcher thread-local survives into `tera.render`.
            let _switcher =
                crate::router::install_locale_switcher(tenant, state, &page, locale_code).await;
            let mut resp = crate::render::render(
                tenant,
                &state.tera,
                &page,
                &state.url_prefix,
                // Error pages render from the tenant-absolute tree.
                "",
                locale_code,
                None,
                state.fragment_cache.as_ref().map(|(c, ttl)| (c, *ttl)),
            )
            .await;
            if resp.status() == StatusCode::OK {
                *resp.status_mut() = status;
                resp.headers_mut().insert(
                    axum::http::header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("no-store"),
                );
                return resp;
            }
            tracing::error!(
                status = %status,
                page_id = page.id.get().copied().unwrap_or_default(),
                "error page failed to render; serving static fallback"
            );
        }
        Ok(None) => {}
        Err(e) => {
            tracing::error!(status = %status, error = %e, "error page lookup failed");
        }
    }
    fallback_response(status)
}

/// Bundled last-resort error page: fully self-contained HTML (no Tera,
/// no DB, no assets) with the status code, a human title, and back /
/// home links. English-only by design — it must not depend on anything
/// that can fail.
pub(crate) fn fallback_response(status: StatusCode) -> Response {
    let code = status.as_u16();
    let title = match code {
        404 => "Page not found",
        403 => "Access denied",
        500 => "Something went wrong",
        503 => "Service unavailable",
        _ => status.canonical_reason().unwrap_or("Error"),
    };
    let detail = match code {
        404 => "The page you're looking for doesn't exist or may have been moved.",
        403 => "You don't have permission to view this page.",
        503 => "The site is briefly down for maintenance. Please try again shortly.",
        _ => "An unexpected error occurred. Please try again shortly.",
    };
    let html = format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>{code} — {title}</title>
<style>
:root{{--accent:#4f46e5;--accent2:#6366f1;--fg:#0b0b0c;--muted:#5d5d68;--bg:#ffffff;--line:#e6e6ea}}
@media (prefers-color-scheme:dark){{:root{{--fg:#ececf2;--muted:#a2a6c2;--bg:#0e1016;--line:#262a40;--accent:#818cf8;--accent2:#a5b4fc}}}}
*{{box-sizing:border-box}}
body{{font:16px/1.65 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;color:var(--fg);background:var(--bg);margin:0;display:grid;min-height:100vh;place-items:center;position:relative;overflow-x:hidden}}
body::before{{content:"";position:fixed;inset:0;z-index:-1;background:radial-gradient(55rem 30rem at 80% -10%,color-mix(in srgb,var(--accent) 12%,transparent),transparent 60%),radial-gradient(45rem 28rem at 0% 100%,color-mix(in srgb,var(--accent2) 9%,transparent),transparent 55%)}}
.box{{max-width:640px;padding:56px 28px;text-align:center}}
.code{{font-family:Georgia,"Times New Roman",serif;font-style:italic;font-weight:600;font-size:clamp(96px,20vw,160px);line-height:.9;letter-spacing:-.04em;margin:0;background:linear-gradient(120deg,var(--accent) 20%,var(--accent2) 60%,color-mix(in srgb,var(--accent2) 55%,var(--fg)) 95%);-webkit-background-clip:text;background-clip:text;color:transparent}}
.rule{{width:72px;height:3px;border:0;margin:26px auto;border-radius:999px;background:linear-gradient(90deg,var(--accent),var(--accent2))}}
h1{{font-family:Georgia,"Times New Roman",serif;font-weight:600;font-size:clamp(26px,4.5vw,34px);letter-spacing:-.015em;margin:0 0 12px}}
p{{margin:0 auto 28px;color:var(--muted);max-width:44ch}}
.actions{{display:flex;gap:12px;justify-content:center;flex-wrap:wrap}}
.actions a{{display:inline-flex;align-items:center;gap:8px;font-weight:600;font-size:15px;padding:12px 22px;border-radius:999px;text-decoration:none;transition:transform .12s ease,box-shadow .15s}}
.actions a:hover{{transform:translateY(-1px)}}
.actions .primary{{background:var(--accent);color:#fff;box-shadow:0 8px 24px color-mix(in srgb,var(--accent) 35%,transparent)}}
.actions .ghost{{border:1px solid color-mix(in srgb,var(--accent) 30%,var(--line));color:var(--fg)}}
.actions .ghost:hover{{border-color:var(--accent);color:var(--accent)}}
</style>
</head>
<body>
<div class="box">
<p class="code">{code}</p>
<hr class="rule">
<h1>{title}</h1>
<p>{detail}</p>
<div class="actions"><a class="primary" href="/">Go to homepage</a><a class="ghost" href="javascript:history.back()">&larr; Go back</a></div>
</div>
</body>
</html>"##
    );
    (
        status,
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_registers_in_the_page_type_registry() {
        let handler = crate::page_type::find_handler(TYPE_NAME)
            .expect("ErrorPage should be inventory-registered");
        assert_eq!(handler.default_template(), "error_page.html");
        assert_eq!(handler.verbose_name(), "Error page");
        assert!(handler.is_leaf(), "error pages must not accept children");
        assert!(handler.is_creatable());
        assert_eq!(handler.icon(), Some("error"));
    }

    async fn body_string(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn fallback_response_is_selfcontained_html_with_links() {
        let resp = fallback_response(StatusCode::NOT_FOUND);
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(ct.starts_with("text/html"), "content-type was {ct}");
        let cc = resp
            .headers()
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert_eq!(cc, "no-store");
        let body = body_string(resp).await;
        assert!(body.contains("404"));
        assert!(body.contains("Page not found"));
        assert!(body.contains("history.back()"), "back link present");
        assert!(body.contains("href=\"/\""), "home link present");
    }

    #[tokio::test]
    async fn fallback_titles_cover_wired_statuses() {
        for (status, needle) in [
            (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong"),
            (StatusCode::FORBIDDEN, "Access denied"),
            (StatusCode::SERVICE_UNAVAILABLE, "Service unavailable"),
        ] {
            let resp = fallback_response(status);
            assert_eq!(resp.status(), status);
            let body = body_string(resp).await;
            assert!(body.contains(needle), "missing {needle:?}");
        }
    }

    #[cfg(feature = "sqlite")]
    mod db {
        use super::super::*;
        use crate::PageTypeHandler as _;

        async fn mem_pool() -> Pool {
            let pool = Pool::connect("sqlite::memory:")
                .await
                .expect("in-memory sqlite");
            // cms_page + its FK parents (cms_page_type, cms_media) +
            // our extension table, straight from each model's SCHEMA —
            // sqlite enforces FK parent tables at insert time.
            for schema in [
                &crate::page_type_model::PageType::SCHEMA,
                &crate::media::Media::SCHEMA,
                &crate::theme::Theme::SCHEMA,
                &Page::SCHEMA,
            ] {
                let ddl = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
                    pool.dialect(),
                    schema,
                );
                for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                    rustango::sql::raw_execute_pool(&pool, stmt, Vec::new())
                        .await
                        .expect("ddl");
                }
            }
            ensure_table(&pool).await.expect("cms_error_page ddl");
            // Satisfy the `page_type_id` FK for test pages.
            let mut pt = crate::page_type_model::PageType {
                id: Auto::Unset,
                app_label: "cms".to_owned(),
                type_name: TYPE_NAME.to_owned(),
                verbose_name: "Error page".to_owned(),
                default_template: "error_page.html".to_owned(),
                view_mode: "auto".to_owned(),
                is_creatable: true,
                allowed_parent_types: serde_json::json!([]),
                allowed_child_types: serde_json::json!([]),
                workflow: String::new(),
                created_at: Auto::Unset,
                updated_at: Auto::Unset,
            };
            pt.insert_pool(&pool).await.expect("insert page type");
            pool
        }

        async fn mk_page(pool: &Pool, slug: &str, status: &str) -> i64 {
            let mut page = Page {
                id: Auto::Unset,
                page_type_id: 1,
                title: format!("Error {slug}"),
                slug: slug.to_owned(),
                path: format!("0001{slug}/"),
                url_path: format!("/{slug}"),
                preview_path: String::new(),
                template_override: String::new(),
                depth: 1,
                parent_id: None,
                locale_variant_of: None,
                alias_of: None,
                theme_id: None,
                sort_order: 0,
                status: status.to_owned(),
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
                created_at: Auto::Unset,
                updated_at: Auto::Unset,
            };
            page.insert_pool(pool).await.expect("insert page");
            page.id.get().copied().expect("page id")
        }

        #[tokio::test]
        async fn find_for_status_returns_published_match_only() {
            let pool = mem_pool().await;
            // Draft 404 page — must NOT be served.
            let draft_id = mk_page(&pool, "draft-404", "draft").await;
            ErrorPageExt {
                id: Auto::Unset,
                page_id: draft_id,
                status_code: "404".into(),
                body: None,
            }
            .insert_pool(&pool)
            .await
            .expect("draft ext");
            assert!(
                find_for_status(&pool, 404).await.expect("query").is_none(),
                "draft error pages must not serve"
            );

            // Published 404 page — served.
            let live_id = mk_page(&pool, "live-404", "published").await;
            ErrorPageExt {
                id: Auto::Unset,
                page_id: live_id,
                status_code: "404".into(),
                body: None,
            }
            .insert_pool(&pool)
            .await
            .expect("live ext");
            let hit = find_for_status(&pool, 404).await.expect("query");
            assert_eq!(
                hit.and_then(|p| p.id.get().copied()),
                Some(live_id),
                "published 404 page should be found"
            );
            // Other statuses still miss.
            assert!(find_for_status(&pool, 500).await.expect("query").is_none());

            // #763 — an archived error page is served like any archived page.
            let kept_id = mk_page(&pool, "kept-410", "archived").await;
            ErrorPageExt {
                id: Auto::Unset,
                page_id: kept_id,
                status_code: "410".into(),
                body: None,
            }
            .insert_pool(&pool)
            .await
            .expect("archived ext");
            let hit = find_for_status(&pool, 410).await.expect("query");
            assert_eq!(hit.and_then(|p| p.id.get().copied()), Some(kept_id));
        }

        #[tokio::test]
        async fn save_extension_roundtrips_through_load_extension() {
            let pool = mem_pool().await;
            let page_id = mk_page(&pool, "e500", "published").await;
            let handler = ErrorPage;
            let mut form = std::collections::HashMap::new();
            form.insert("status_code".to_owned(), "500".to_owned());
            form.insert(
                "body".to_owned(),
                r#"[{"type":"paragraph","value":"Oops."}]"#.to_owned(),
            );
            handler
                .save_extension(&pool, page_id, &form)
                .await
                .expect("save");
            let ext = handler.load_extension(&pool, page_id).await.expect("load");
            assert_eq!(ext["status_code"], "500");
            assert!(ext["body"].as_str().unwrap_or_default().contains("Oops."));

            // Re-save flips the code in place (upsert, not a second row).
            form.insert("status_code".to_owned(), "503".to_owned());
            handler
                .save_extension(&pool, page_id, &form)
                .await
                .expect("re-save");
            let rows: Vec<ErrorPageExt> = ErrorPageExt::objects()
                .where_(ErrorPageExt::page_id.eq(page_id))
                .fetch(&pool)
                .await
                .expect("fetch");
            assert_eq!(rows.len(), 1, "upsert must not duplicate");
            assert_eq!(rows[0].status_code, "503");
        }
    }
}
