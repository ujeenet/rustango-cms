//! Form submissions: model, boot-time table ensure, and the public submit
//! handler.
//!
//! The submission table is created at boot via `CREATE TABLE IF NOT EXISTS`
//! (the framework's audit_log/cache pattern) because repo-wide
//! `gen_migration` is blocked by pre-existing framework FK-snapshot drift.
//! A fresh table name (`cms_form_entry`) sidesteps the dead legacy
//! `cms_form_submission` table.
//!
//! Provenance: the source page is resolved from the `Referer` at submit;
//! the embedding block's per-placement success override is resolved
//! server-side from that page's stream JSON by the `_embed` block uuid
//! (never trusted from a client field).

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use rustango::core::Column as _;
use rustango::core::Model as _;
use rustango::extractors::Tenant;
use rustango::sql::Auto;
use rustango::sql::FetcherPool as _;
use rustango::Model;
use serde::{Deserialize, Serialize};

use super::render::HONEYPOT_FIELD;
use super::schema;

/// One stored submission. New snippet-backed forms store here.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "cms_form_entry", app = "cms")]
pub struct FormEntry {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// The form snippet (`cms_snippet`) this answers.
    #[rustango(index)]
    pub form_snippet_id: i64,
    /// Snippet revision sequence at submit time (0 = unknown). Lets the
    /// submissions admin resolve column drift across form edits.
    pub form_version: i64,
    /// Page the form was submitted from (provenance). 0 = unknown.
    #[rustango(index)]
    pub source_page_id: i64,
    #[rustango(max_length = 512)]
    pub source_url: String,
    /// Answers keyed by field key. Multi-value fields hold a JSON array.
    pub data_json: serde_json::Value,
    #[rustango(max_length = 16)]
    pub locale: String,
    #[rustango(max_length = 64)]
    pub ip: String,
    #[rustango(auto_now_add)]
    pub submitted_at: Auto<DateTime<Utc>>,
}

/// Create `cms_form_entry` if absent. Idempotent; called per tenant at boot.
///
/// # Errors
/// Driver / DDL failures.
pub async fn ensure_table(pool: &rustango::sql::Pool) -> Result<(), rustango::sql::ExecError> {
    let ddl = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
        pool.dialect(),
        &FormEntry::SCHEMA,
    );
    for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new()).await?;
    }
    Ok(())
}

/// `POST /forms/submit/{form_id}` — validate, store, notify, and redirect.
/// The largest form submission accepted, uploads included. Set on
/// the route itself, so a host that raises or disables axum's default body
/// limit for its own upload routes doesn't let an anonymous visitor buffer
/// an arbitrarily large file into memory here.
pub const MAX_SUBMISSION_BYTES: usize = 10 * 1024 * 1024;

pub(crate) async fn handle_form_submit(
    State(state): State<crate::router::PublicState>,
    tenant: Tenant,
    headers: HeaderMap,
    Path(form_id): Path<i64>,
    req: axum::extract::Request,
) -> Response {
    use axum::extract::FromRequest as _;
    let pool = tenant.pool();

    // Load the form snippet → schema.
    let snippet = match crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::id.eq(form_id))
        .where_(crate::snippet::Snippet::type_name.eq("form".to_owned()))
        .first(pool)
        .await
    {
        Ok(Some(s)) => s,
        _ => return (axum::http::StatusCode::NOT_FOUND, "Unknown form.").into_response(),
    };
    let form = schema::parse(&snippet.data).unwrap_or_default();

    // Parse the body. urlencoded (simple / no-JS forms) → `pairs`. multipart
    // (forms with file uploads, fetch-submitted by the runtime so the
    // X-CSRF-Token header is present) → text fields into `pairs`, files held
    // in `pending_uploads` with their storage key in `file_answers`. Nothing
    // is written until the submission is accepted (#656).
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut file_answers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut pending_uploads: Vec<(String, axum::body::Bytes)> = Vec::new();
    if content_type.starts_with("multipart/form-data") {
        let Ok(mut mp) = axum::extract::Multipart::from_request(req, &()).await else {
            return (axum::http::StatusCode::BAD_REQUEST, "Bad upload.").into_response();
        };
        // A read error — the body over MAX_SUBMISSION_BYTES, or a torn
        // upload — rejects the submission (#666). Read as "no more fields",
        // it saved the rest of the form and dropped the file silently.
        loop {
            let field = match mp.next_field().await {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => return (e.status(), "Upload rejected.").into_response(),
            };
            let name = field.name().unwrap_or("").to_owned();
            if name.is_empty() {
                continue;
            }
            let fname = field
                .file_name()
                .map(str::to_owned)
                .filter(|f| !f.is_empty());
            if let Some(fname) = fname {
                let bytes = match field.bytes().await {
                    Ok(b) => b,
                    Err(e) => return (e.status(), "Upload rejected.").into_response(),
                };
                if !bytes.is_empty() {
                    if let Some(key) = upload_key(&fname, &bytes) {
                        file_answers.insert(name, key.clone());
                        pending_uploads.push((key, bytes));
                    }
                }
            } else {
                match field.text().await {
                    Ok(text) => pairs.push((name, text)),
                    Err(e) => return (e.status(), "Upload rejected.").into_response(),
                }
            }
        }
    } else {
        let bytes = axum::body::to_bytes(req.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap_or_default();
        pairs = serde_urlencoded::from_bytes(&bytes).unwrap_or_default();
    }

    // Honeypot — silently accept (200) without storing so bots get no signal.
    let hp_filled = pairs
        .iter()
        .any(|(k, v)| k == HONEYPOT_FIELD && !v.trim().is_empty());

    // Every value per key, for conditional-logic evaluation (FB-14): a
    // checkbox group answers several. Field keys never start with `_`, so
    // meta fields are excluded.
    let mut vals: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for (k, v) in &pairs {
        if !k.starts_with('_') {
            vals.entry(k.clone()).or_default().push(v.clone());
        }
    }

    // Collect answers keyed by field key, building arrays for multi-value.
    // `refused` is the first question the submission failed on, so the
    // visitor is sent back to it.
    let mut answers = serde_json::Map::new();
    let mut refused: Option<String> = None;
    for field in form.fields() {
        if !field.field_type.collects_value() || field.key.is_empty() {
            continue;
        }
        // FB-14 — fields hidden by their rules are excused from required +
        // validation (their stale value must not block the submit).
        let visible = schema::is_visible(field, &vals);
        // File fields carry their value in `file_answers` (saved to disk),
        // not in the urlencoded pairs.
        if matches!(field.field_type, schema::FieldType::File) {
            match file_answers.get(&field.key) {
                Some(key) => {
                    answers.insert(field.key.clone(), serde_json::Value::String(key.clone()));
                }
                None => {
                    if visible && field.required {
                        refused.get_or_insert_with(|| field.key.clone());
                    }
                }
            }
            continue;
        }
        let values: Vec<&String> = pairs
            .iter()
            .filter(|(k, _)| k == &field.key)
            .map(|(_, v)| v)
            .filter(|v| !v.is_empty())
            .collect();
        if visible && field.required && values.is_empty() {
            refused.get_or_insert_with(|| field.key.clone());
        }
        // FB-13 — server-side format/length enforcement (mirrors the
        // client HTML5 attrs; rejects tampered POSTs).
        if visible && values.iter().any(|v| schema::validate_value(field, v).is_some()) {
            refused.get_or_insert_with(|| field.key.clone());
        }
        if field.field_type.is_multi_value() {
            answers.insert(
                field.key.clone(),
                serde_json::Value::Array(
                    values
                        .into_iter()
                        .map(|v| serde_json::Value::String(v.clone()))
                        .collect(),
                ),
            );
        } else if let Some(v) = values.first() {
            answers.insert(field.key.clone(), serde_json::Value::String((*v).clone()));
        }
    }

    // Source page (provenance) from the Referer.
    let referer = headers
        .get(axum::http::header::REFERER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let source_path = referer_path(&referer);
    // The page's own url_path: no query, no language prefix (`/fr/…`), and
    // relative to this host's site root — the same steps the renderer takes.
    let (path_only, query) = match source_path.split_once('?') {
        Some((p, q)) => (p.to_owned(), Some(q.to_owned())),
        None => (source_path.clone(), None),
    };
    let cookie_header = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok());
    let request_locale = crate::locale_mode::resolve_request_locale(
        pool,
        state.locale_mode(),
        &path_only,
        query.as_deref(),
        crate::locale_mode::locale_cookie_value(cookie_header).as_deref(),
        None,
    )
    .await;
    let site_prefix = crate::site::prefix_for_host(pool, crate::router::host_header(&headers))
        .await
        .unwrap_or_default();
    let lookup_path = if path_only.is_empty() {
        String::new()
    } else {
        crate::site::to_lookup_path(&site_prefix, &request_locale.stripped_path)
    };
    let source_page_id = if lookup_path.is_empty() {
        0
    } else {
        crate::page::Page::objects()
            .where_(crate::page::Page::url_path.eq(lookup_path.clone()))
            .first(pool)
            .await
            .ok()
            .flatten()
            .and_then(|p| p.id.get().copied())
            .unwrap_or(0)
    };

    // Per-embed override (success redirect / message), resolved server-side
    // from the source page's stream by the `_embed` block uuid.
    let embed_id = pairs
        .iter()
        .find(|(k, _)| k == "_embed")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    // (The placement's thanks message is rendered with the form itself.)
    let ovr_redirect = if source_page_id != 0 && !embed_id.is_empty() {
        resolve_embed_redirect(pool, source_page_id, &embed_id).await
    } else {
        None
    };

    // Reject only on honeypot or a refused answer — redirect back with a
    // flag naming the question, so the runtime can return to it.
    if hp_filled || refused.is_some() {
        let back = if source_path.is_empty() {
            "/".to_owned()
        } else {
            strip_form_flags(&source_path)
        };
        let sep = if back.contains('?') { '&' } else { '?' };
        let flag = if hp_filled {
            format!("form_submitted={form_id}")
        } else {
            format!("form_error={form_id}:{}", refused.unwrap_or_default())
        };
        return Redirect::to(&format!("{back}{sep}{flag}")).into_response();
    }

    // Store.
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .unwrap_or("")
        .trim()
        .to_owned();
    // FB-15 — record which version this answered (revision count = version),
    // so the submissions admin can resolve schema drift across edits.
    let form_version = crate::snippet::SnippetRevision::objects()
        .where_(crate::snippet::SnippetRevision::snippet_id.eq(form_id))
        .fetch(pool)
        .await
        .map(|v| v.len() as i64)
        .unwrap_or(0);
    // FB-17 — record the language the form was filled in: the page's
    // (`/fr/…`, `?lang=`), else the visitor's chosen one (cookie).
    let locale = request_locale.code.clone().unwrap_or_default();
    let mut entry = FormEntry {
        id: Auto::Unset,
        form_snippet_id: form_id,
        form_version,
        source_page_id,
        source_url: referer.chars().take(512).collect(),
        data_json: serde_json::Value::Object(answers),
        locale: locale.chars().take(16).collect(),
        ip: ip.chars().take(64).collect(),
        submitted_at: Auto::Unset,
    };
    // Only now, with the submission accepted, do its files reach disk
    // (#656): written earlier, every honeypot hit or missing-required
    // bounce left an upload no entry referred to.
    let mut written: Vec<String> = Vec::new();
    for (key, bytes) in &pending_uploads {
        if write_upload(&tenant.org.slug, key, bytes).await {
            written.push(key.clone());
        }
    }
    if entry.insert_pool(pool).await.is_err() {
        for key in &written {
            remove_upload(&tenant.org.slug, key).await;
        }
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save your submission.",
        )
            .into_response();
    }

    // Notify. Everything goes through the notification outbox now: the
    // recipients from this form's settings, plus any Slack / Teams /
    // Telegram / webhook target subscribed to `form.submitted`.
    //
    // This used to be an email loop written right here, sending inside the
    // request. That made the visitor wait on the mail server and turned a
    // transient failure into a log line nobody reads — with the submission
    // saved and the notification simply gone. Queue, then deliver off the
    // request path, where a failure is a row someone can see and retry.
    {
        let mut msg = crate::notify::Notification::new(
            crate::notify::events::FORM_SUBMITTED,
            format!("New form submission: {}", snippet.title),
        )
        .summary(format!("A visitor submitted \"{}\".", snippet.title));
        // Walk the SCHEMA, not the answers. `data_json` is a sorted map, so
        // reading it directly hands the reader "email, message, name" — an
        // alphabetised jumble of the form they actually filled in. The
        // schema knows the order the questions were asked, and a
        // notification is read like the form, top to bottom.
        if let serde_json::Value::Object(map) = &entry.data_json {
            let mut seen = std::collections::HashSet::new();
            let render = |v: &serde_json::Value| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            for f in form.fields() {
                if let Some(v) = map.get(&f.key) {
                    seen.insert(f.key.clone());
                    let label = if f.label.trim().is_empty() {
                        f.key.clone()
                    } else {
                        f.label.clone()
                    };
                    msg = msg.field(label, render(v));
                }
            }
            // Anything the schema no longer mentions still belongs in the
            // notification — a field removed after this entry was queued is
            // exactly when you want to see the value, not lose it.
            for (k, v) in map {
                if !seen.contains(k) {
                    msg = msg.field(k.clone(), render(v));
                }
            }
        }
        if !entry.source_url.is_empty() {
            msg = msg.url(entry.source_url.clone());
        }
        // A notification failure must never fail the submission — the entry
        // is already stored, and losing the lead is far worse than a late
        // alert.
        match crate::notify::worker::enqueue(pool, &msg, form_id).await {
            Ok(0) => {}
            Ok(_) => crate::notify::worker::spawn_drain(
                pool.clone(),
                tenant.org.slug.clone(),
                state.mailer.clone(),
                state.mailer_from.as_str().to_owned(),
            ),
            Err(e) => tracing::warn!(
                target: "rustango_cms::forms",
                error = %e, form_id, "could not queue submission notifications"
            ),
        }
    }

    // Success precedence: embed redirect → form redirect → thanks on source.
    if let Some(url) = ovr_redirect.filter(|u| is_safe_redirect(u)) {
        return Redirect::to(&url).into_response();
    }
    if !form.settings.redirect_url.is_empty() && is_safe_redirect(&form.settings.redirect_url) {
        return Redirect::to(&form.settings.redirect_url).into_response();
    }
    let back = if source_path_is_usable(&referer) {
        strip_form_flags(&referer_path(&referer))
    } else {
        "/".to_owned()
    };
    let sep = if back.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{back}{sep}form_submitted={form_id}")).into_response()
}

/// Reduce a Referer URL to its path (+query), dropping scheme/host so the
/// redirect stays same-origin.
fn referer_path(referer: &str) -> String {
    if referer.is_empty() {
        return String::new();
    }
    if let Some(rest) = referer.splitn(2, "://").nth(1) {
        // strip host
        match rest.find('/') {
            Some(i) => rest[i..].to_owned(),
            None => "/".to_owned(),
        }
    } else if referer.starts_with('/') {
        referer.to_owned()
    } else {
        String::new()
    }
}

/// `path` without an earlier `form_submitted` / `form_error` flag, so a
/// second attempt from the result page doesn't carry both.
fn strip_form_flags(path: &str) -> String {
    let Some((base, query)) = path.split_once('?') else {
        return path.to_owned();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|kv| {
            let k = kv.split('=').next().unwrap_or("");
            !kv.is_empty() && k != "form_submitted" && k != "form_error"
        })
        .collect();
    if kept.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

fn source_path_is_usable(referer: &str) -> bool {
    !referer_path(referer).is_empty()
}

/// Only allow same-origin (path-only) redirects to avoid open-redirects.
fn is_safe_redirect(url: &str) -> bool {
    rustango::auth_decorators::safe_next(url).is_some()
}

/// The success redirect set on the `form` block `embed_id` of the source
/// page, if any. The block can sit in a code page type's stream (its
/// extension) or in the fields of a page type made in the admin (its
/// builder data), nested at any depth.
async fn resolve_embed_redirect(
    pool: &rustango::sql::Pool,
    page_id: i64,
    embed_id: &str,
) -> Option<String> {
    let page = crate::page::Page::objects()
        .where_(crate::page::Page::id.eq(page_id))
        .first(pool)
        .await
        .ok()??;
    let mut documents: Vec<serde_json::Value> = Vec::new();
    let handler = crate::page_type_model::PageType::objects()
        .where_(crate::page_type_model::PageType::id.eq(page.page_type_id))
        .first(pool)
        .await
        .ok()
        .flatten()
        .and_then(|pt| crate::page_type::find_handler(&pt.type_name));
    if let Some(handler) = handler {
        if let Ok(extension) = handler.load_extension(pool, page_id).await {
            documents.push(extension);
        }
    }
    if let Ok(Some(builder)) = crate::page_builder::model::data_for_page(pool, page_id).await {
        documents.push(builder.data);
    }
    documents
        .iter()
        .find_map(|doc| find_embed(doc, embed_id))
        .and_then(|block| block.get("success_redirect_url").and_then(serde_json::Value::as_str))
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
}

/// The value of the `form` block with id `embed_id` anywhere under `value`.
fn find_embed<'a>(value: &'a serde_json::Value, embed_id: &str) -> Option<&'a serde_json::Value> {
    match value {
        serde_json::Value::Object(obj) => {
            if obj.get("id").and_then(serde_json::Value::as_str) == Some(embed_id)
                && obj.get("type").and_then(serde_json::Value::as_str) == Some("form")
            {
                return obj.get("value");
            }
            obj.values().find_map(|v| find_embed(v, embed_id))
        }
        serde_json::Value::Array(items) => items.iter().find_map(|v| find_embed(v, embed_id)),
        _ => None,
    }
}

/// The storage key an upload is saved under in `./var/form-uploads/<slug>/`:
/// a content hash plus the sanitized filename. `None` for a name that
/// sanitizes to nothing. Pure — nothing is written.
fn upload_key(filename: &str, bytes: &[u8]) -> Option<String> {
    use sha2::{Digest, Sha256};
    let safe = sanitize_upload_name(filename);
    if safe.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hex: String = hasher
        .finalize()
        .iter()
        .take(8)
        .fold(String::new(), |mut a, b| {
            use std::fmt::Write as _;
            let _ = write!(a, "{b:02x}");
            a
        });
    Some(format!("{hex}-{safe}"))
}

fn upload_dir(slug: &str) -> std::path::PathBuf {
    std::path::PathBuf::from("./var/form-uploads").join(sanitize_upload_name(slug))
}

/// Write an accepted upload under its [`upload_key`]. Async I/O: this runs
/// inside an anonymous request. A failed write is logged and the
/// submission is kept without the file.
async fn write_upload(slug: &str, key: &str, bytes: &[u8]) -> bool {
    let dir = upload_dir(slug);
    let written = async {
        rustango::__private_runtime::tokio::fs::create_dir_all(&dir).await?;
        rustango::__private_runtime::tokio::fs::write(dir.join(key), bytes).await
    }
    .await;
    if let Err(e) = written {
        tracing::warn!(target: "rustango_cms::forms", error = %e, "form upload not stored; the submission is saved without it");
        return false;
    }
    true
}

/// Remove an upload written for a submission that then failed to save.
async fn remove_upload(slug: &str, key: &str) {
    let _ = rustango::__private_runtime::tokio::fs::remove_file(upload_dir(slug).join(key)).await;
}

/// Reduce an untrusted filename to a safe basename (no path separators,
/// only `[A-Za-z0-9._-]`, capped length). Used for both the filename and
/// the tenant-slug path segment to keep uploads inside the upload dir.
fn sanitize_upload_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    cleaned.trim_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_upload_name_strips_paths() {
        assert_eq!(sanitize_upload_name("../../etc/passwd"), "passwd");
        assert_eq!(
            sanitize_upload_name("my report (1).pdf"),
            "my_report__1_.pdf"
        );
        assert_eq!(sanitize_upload_name("..\\..\\win.ini"), "win.ini");
    }

    #[test]
    fn finds_the_embed_in_a_stream_or_builder_fields() {
        let builder = serde_json::json!({
            "price": 32.0,
            "order": [{ "id": "b1", "type": "form", "value": { "form": "3", "success_redirect_url": "/thanks" } }]
        });
        assert_eq!(find_embed(&builder, "b1").and_then(|v| v["success_redirect_url"].as_str()), Some("/thanks"));
        let nested = serde_json::json!({ "body": [{ "id": "x", "type": "section", "value": { "inner": [
            { "id": "b2", "type": "form", "value": { "success_redirect_url": "/deep" } } ] } }] });
        assert_eq!(find_embed(&nested, "b2").and_then(|v| v["success_redirect_url"].as_str()), Some("/deep"));
        assert!(find_embed(&nested, "missing").is_none());
        // Another block type with the same id is not the embed.
        let other = serde_json::json!([{ "id": "b3", "type": "text", "value": {} }]);
        assert!(find_embed(&other, "b3").is_none());
    }

    #[test]
    fn earlier_result_flags_are_dropped() {
        assert_eq!(strip_form_flags("/c?form_error=name"), "/c");
        assert_eq!(strip_form_flags("/c?lang=fr&form_submitted=1"), "/c?lang=fr");
        assert_eq!(strip_form_flags("/c?form_error=1&form_error=2&x=1"), "/c?x=1");
        assert_eq!(strip_form_flags("/c"), "/c");
    }

    #[test]
    fn referer_path_strips_host() {
        assert_eq!(referer_path("https://x.test/a/b?c=1"), "/a/b?c=1");
        assert_eq!(referer_path("http://x.test"), "/");
        assert_eq!(referer_path("/already/path"), "/already/path");
        assert_eq!(referer_path(""), "");
    }

    #[test]
    fn safe_redirect_blocks_offsite() {
        assert!(is_safe_redirect("/thanks"));
        assert!(!is_safe_redirect("//evil.test"));
        assert!(!is_safe_redirect("https://evil.test"));
        // Browsers read `/\host` and its encoded forms as `//host` (#728).
        assert!(!is_safe_redirect("/\\evil.test"));
        assert!(!is_safe_redirect("/%5Cevil.test"));
        assert!(!is_safe_redirect("/%2Fevil.test"));
    }
}

