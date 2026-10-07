//! Public route for serving image renditions.
//!
//! `GET /__media__/<filter_spec>/<media_id>` — looks up the
//! [`Media`] row, looks up or generates the matching
//! [`MediaRendition`], serves the bytes with `Content-Type` +
//! `Cache-Control: public, max-age=31536000, immutable` (renditions
//! are content-addressed by `(content_hash, filter_spec)` so the
//! response is safe to cache forever).
//!
//! Originals and renditions are read/written through the pluggable
//! storage backend ([`crate::media_storage`]) — local disk by default,
//! S3/R2/MinIO when the host installs one.

use crate::log_err::LogErr as _;
use std::sync::Arc;

use axum::extract::{Path, Query};
use axum::http::request::Parts;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rustango::core::Column as _;
use rustango::extractors::TenantContext;
use rustango::sql::{Auto, FetcherPool as _};
use rustango::tenancy::{DefaultTenantDb, OrgResolver as _};

use crate::media::Media;
use crate::rendition::{apply, MediaRendition};

/// Lightweight `(org, pool)` pair for the rendition routes. The
/// framework's [`Tenant`](rustango::extractors::Tenant) extractor EAGERLY acquires a pool
/// connection at extractor time and holds it for the entire
/// handler lifetime (`extractors/tenant.rs:236–240` — the
/// `database_acquire(&org)` call). With `max_connections = 4` on
/// the tenant pool that capped concurrent rendition requests at 4
/// — every 5th request blocked at the extractor until one of the
/// in-flight handlers (which can take 100–500 ms for a cold
/// image resize) returned its connection. The stress test
/// reproduced this: 4 concurrent → all ok; 5+ → every request
/// timed out at the pool acquire window.
///
/// This extractor resolves the same `(org, pool)` pair through
/// the same `TenantContext` path as `Tenant`, but stops short of
/// the `database_acquire` step. Rendition handlers only ever call
/// `tenant.pool()` (every `fetch` / `insert_pool` internally
/// acquires + releases) — they NEVER need a long-lived connection
/// — so dropping the held conn is purely a win.
pub struct TenantLite {
    pub org: rustango::tenancy::Org,
    pool: rustango::sql::Pool,
}

impl<S> axum::extract::FromRequestParts<S> for TenantLite
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // `DefaultTenantDb` is the framework's active-feature alias
        // — `sqlx::Postgres` on PG builds, `sqlx::Sqlite` on sqlite
        // builds, etc. Whichever variant the host's
        // `Builder::serve` inserted into request extensions is the
        // one we look up here.
        let ctx = parts
            .extensions
            .get::<Arc<TenantContext<DefaultTenantDb>>>()
            .cloned()
            .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "tenant context missing"))?;
        let org = ctx
            .resolver
            .resolve(parts, &ctx.pools.registry_pool())
            .await
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "tenant resolve failed"))?
            .ok_or((StatusCode::NOT_FOUND, "tenant not found"))?;
        let pool = ctx
            .pools
            .scoped_pool_dyn(&org)
            .await
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "tenant pool unavailable"))?;
        Ok(TenantLite { org, pool })
    }
}

impl TenantLite {
    pub fn pool(&self) -> &rustango::sql::Pool {
        &self.pool
    }
}

/// Mount on a tenant-aware router:
/// ```ignore
/// .merge(rustango_cms::rendition_route::router())
/// ```
///
/// Mounts two routes:
/// - `/__media__/raw/{media_id}` — serves the **original** bytes for
///   any media kind (image, document, file). Use this URL for
///   document download links and for `<a href>` to the original
///   image. Returns the source `Content-Type` from the media row.
/// - `/__media__/{filter_spec}/{media_id}` — serves an image
///   **rendition** (resize / crop / re-encode). Image-only; rejects
///   non-image media kinds with 400.
///
/// Both routes honour collection-level view restrictions.
pub fn router() -> Router {
    Router::new()
        .route("/__media__/raw/{media_id}", get(serve_raw))
        .route("/__media__/{filter_spec}/{media_id}", get(serve))
}

/// `GET /__media__/raw/{media_id}` — original-bytes endpoint. Use
/// from block templates / page templates whenever you need a public
/// URL for a `cms_media.id` regardless of kind.
///
/// Returns the source `Content-Type` from the row and a short cache
/// header (1 hour) since the original bytes are content-addressed
/// by `storage_key` and effectively immutable, but the row's title /
/// permissions can change without changing the bytes — we want
/// browsers to recheck within a reasonable window.
async fn serve_raw(
    tenant: TenantLite,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    rustango::tenancy::member_auth::CurrentMember(member): rustango::tenancy::member_auth::CurrentMember,
    Path(media_id): Path<i64>,
) -> Response {
    let media: Media = match Media::objects()
        .where_(Media::id.eq(media_id))
        .fetch(tenant.pool())
        .await
    {
        Ok(v) => match v.into_iter().next() {
            Some(m) => m,
            None => return (StatusCode::NOT_FOUND, "media not found").into_response(),
        },
        Err(e) => {
            tracing::error!(error = %e, "media lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "media lookup failed").into_response();
        }
    };
    // #members — a member session gates media bytes too (member wins).
    let viewer = member.as_ref().or(session_user.as_ref());
    let restricted = match check_collection_restriction(&tenant, &media, viewer).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let bytes = match crate::media_storage::disk_for(&tenant.org.slug)
        .load(&crate::media_storage::key(
            &tenant.org.slug,
            &media.storage_key,
        ))
        .await
    {
        Ok(b) => b,
        Err(_) => {
            return (StatusCode::NOT_FOUND, "original file missing in storage").into_response()
        }
    };
    let mut resp = bytes.into_response();
    let headers = resp.headers_mut();
    // 1-hour browser cache; intermediaries get the same. Strict
    // immutability isn't claimable here because the row's
    // permissions / collection can change while the storage_key
    // (bytes) stays the same.
    set_media_cache(headers, restricted, "public, max-age=3600");
    harden_media_headers(headers, &media.mime, &media.filename);
    resp
}

/// How an uploaded file may be shown when opened from its URL.
#[derive(Debug, PartialEq, Eq)]
enum MediaDisplay {
    /// Raster images: inline, nothing to execute.
    Raster,
    /// SVG: inline so `<img src>` works, but scripts can't run when the
    /// file is opened directly.
    Svg,
    /// Types a browser shows in a viewer without running page script.
    Viewer,
    /// Everything else — HTML, XHTML, XML, JS, unknown: download only.
    Download,
}

fn media_display(mime: &str) -> MediaDisplay {
    let mime = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if mime == "image/svg+xml" {
        MediaDisplay::Svg
    } else if mime.starts_with("image/") {
        MediaDisplay::Raster
    } else if mime == "application/pdf"
        || mime == "text/plain"
        || mime.starts_with("video/")
        || mime.starts_with("audio/")
    {
        MediaDisplay::Viewer
    } else {
        MediaDisplay::Download
    }
}

/// Media is served from the tenant host that also serves `/cms-admin`, so
/// an uploaded HTML or SVG file opened from its URL would run with the
/// viewer's admin session. Pin the type, forbid sniffing, sandbox
/// anything that isn't a raster image, and make active types download.
fn harden_media_headers(headers: &mut axum::http::HeaderMap, mime: &str, filename: &str) {
    let display = media_display(mime);
    let content_type = match display {
        MediaDisplay::Svg => HeaderValue::from_static("image/svg+xml"),
        MediaDisplay::Download => HeaderValue::from_static("application/octet-stream"),
        MediaDisplay::Raster | MediaDisplay::Viewer => HeaderValue::from_str(mime)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    };
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    match display {
        MediaDisplay::Raster => {}
        MediaDisplay::Svg => {
            headers.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(
                    "default-src 'none'; style-src 'unsafe-inline'; img-src data:; sandbox",
                ),
            );
        }
        MediaDisplay::Viewer | MediaDisplay::Download => {
            headers.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static("sandbox"),
            );
            let disposition = if display == MediaDisplay::Viewer {
                "inline"
            } else {
                "attachment"
            };
            // A filename is a hint; drop what would end the quoted string.
            let name: String = filename
                .chars()
                .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
                .collect();
            if let Ok(v) = HeaderValue::from_str(&format!("{disposition}; filename=\"{name}\"")) {
                headers.insert(header::CONTENT_DISPOSITION, v);
            }
        }
    }
}

/// Shared collection-view-restriction check for both routes.
/// `Ok(())` = proceed; `Err(resp)` = short-circuit the request with
/// that response (401 / 403).
/// Whether a media file may be served to `viewer`, and if so whether it
/// sits behind a view restriction: `Ok(true)` means the viewer passed a
/// restriction, so the response must not reach a shared cache.
async fn check_collection_restriction(
    tenant: &TenantLite,
    media: &Media,
    viewer: Option<&rustango::tenancy::auth::User>,
) -> Result<bool, Response> {
    let Some(collection_id) = media.collection_id else {
        return Ok(false);
    };
    match crate::collection_view_restriction::effective_for_collection(tenant.pool(), collection_id)
        .await
    {
        Ok(Some(r)) => match r.parsed_kind() {
            Some(crate::view_restriction::RestrictionKind::Login)
            | Some(crate::view_restriction::RestrictionKind::Password) => {
                if viewer.is_none() {
                    return Err((
                        StatusCode::UNAUTHORIZED,
                        "media collection requires authentication",
                    )
                        .into_response());
                }
                Ok(true)
            }
            Some(crate::view_restriction::RestrictionKind::Groups) => {
                let Some(user) = viewer else {
                    return Err((
                        StatusCode::UNAUTHORIZED,
                        "media collection requires authentication",
                    )
                        .into_response());
                };
                if !user.is_superuser
                    && !user_in_any_role(tenant.pool(), user, &r.parsed_groups()).await
                {
                    return Err((StatusCode::FORBIDDEN, "forbidden").into_response());
                }
                Ok(true)
            }
            Some(crate::view_restriction::RestrictionKind::Permission) => {
                let Some(user) = viewer else {
                    return Err((
                        StatusCode::UNAUTHORIZED,
                        "media collection requires authentication",
                    )
                        .into_response());
                };
                let uid = user.id.get().copied().unwrap_or_default();
                if !crate::view_restriction_guard::viewer_has_any_codename(
                    tenant.pool(),
                    uid,
                    &r.parsed_codenames(),
                )
                .await
                {
                    return Err((StatusCode::FORBIDDEN, "forbidden").into_response());
                }
                Ok(true)
            }
            None => Err((StatusCode::FORBIDDEN, "forbidden").into_response()),
        },
        Ok(None) => Ok(false),
        Err(e) => {
            // Fail closed (#757): an error is not evidence the media is public.
            tracing::warn!(
                target: "rustango_cms::rendition_route",
                media_id = ?media.id, error = %e,
                "collection view-restriction lookup failed; denying"
            );
            Err((StatusCode::SERVICE_UNAVAILABLE, "try again").into_response())
        }
    }
}

/// Rendition request query: the optional `s=` HMAC signature
/// and the `v=` cache-buster (ignored here — it only varies the URL).
#[derive(serde::Deserialize, Default)]
struct RenditionQuery {
    #[serde(default)]
    s: Option<String>,
}

async fn serve(
    tenant: TenantLite,
    rustango::extractors::SessionUser(session_user): rustango::extractors::SessionUser,
    rustango::tenancy::member_auth::CurrentMember(member): rustango::tenancy::member_auth::CurrentMember,
    Path((filter_spec, media_id)): Path<(String, i64)>,
    Query(query): Query<RenditionQuery>,
) -> Response {
    // 1) Parse the filter spec — now a pipeline (#79). Bad specs
    //    → 400 (separate from "media doesn't exist" so
    //    misconfigured templates surface cleanly).
    let pipeline = match crate::rendition::FilterPipeline::parse(&filter_spec) {
        Ok(o) => o,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
        }
    };
    let canonical_spec = pipeline.canonical();
    let op = pipeline.geometric;

    // #425 — when signed rendition URLs are enabled, reject forged /
    // unsigned requests BEFORE any DB lookup or rendition work, so
    // enumeration + arbitrary-spec CPU-DoS cost an attacker nothing.
    // No-op (always allows) when signing is disabled.
    if !crate::rendition::verify_rendition_sig(&canonical_spec, media_id, query.s.as_deref()) {
        return (
            StatusCode::FORBIDDEN,
            "invalid or missing rendition signature",
        )
            .into_response();
    }

    // 2) Load the Media row.
    let media: Media = match Media::objects()
        .where_(Media::id.eq(media_id))
        .fetch(tenant.pool())
        .await
    {
        Ok(v) => match v.into_iter().next() {
            Some(m) => m,
            None => return (StatusCode::NOT_FOUND, "media not found").into_response(),
        },
        Err(e) => {
            tracing::error!(error = %e, "media lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "media lookup failed").into_response();
        }
    };
    if media.kind != "image" {
        return (
            StatusCode::BAD_REQUEST,
            "media is not an image — renditions only apply to images",
        )
            .into_response();
    }

    // #286 — SVG pass-through. The raster pipeline (decode →
    // resize → re-encode) doesn't apply to vector sources: the
    // `image` crate's decoders don't speak SVG, and rasterising
    // every request would defeat the point of shipping vector.
    //
    // V1 strategy: serve the original SVG bytes verbatim with
    // `Content-Type: image/svg+xml` and the same immutable cache
    // headers raster renditions use. Width/height attrs on the
    // `<img>` tag at the template level handle layout sizing.
    // No `cms_media_rendition` row is written — SVGs are cheap
    // enough to serve from source that the rendition table would
    // just collect noise.
    //
    // A `format-png` / `format-jpg` / `format-webp` step on an SVG also
    // returns the original SVG: rasterising needs a vector renderer
    // (resvg / usvg), which is not a dependency. A template that needs a
    // raster image (an og:image, say) should use a raster source.
    // #members — member session (member wins) OR admin session.
    let viewer = member.as_ref().or(session_user.as_ref());
    if media.mime == "image/svg+xml" {
        let restricted = match check_collection_restriction(&tenant, &media, viewer).await {
            Ok(r) => r,
            Err(resp) => return resp,
        };
        let bytes = match crate::media_storage::disk_for(&tenant.org.slug)
            .load(&crate::media_storage::key(
                &tenant.org.slug,
                &media.storage_key,
            ))
            .await
        {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(
                    target: "rustango_cms::rendition_route",
                    media_id, error = %e,
                    "svg source bytes unreadable"
                );
                return (StatusCode::INTERNAL_SERVER_ERROR, "svg source unreadable")
                    .into_response();
            }
        };
        // The parsed pipeline (`op` + `canonical_spec`) is unused for
        // vector sources — we don't honour size/crop here. Discard
        // explicitly to silence unused warnings without renaming the
        // bindings (which would diverge from the raster path below).
        let _ = (op, &canonical_spec);
        let mut resp = bytes.into_response();
        let headers = resp.headers_mut();
        set_media_cache(headers, restricted, "public, max-age=31536000, immutable");
        harden_media_headers(headers, "image/svg+xml", &media.filename);
        return resp;
    }

    // #195 — collection view restriction, shared with the raw and SVG
    // paths. Direct-image-URL fetches can't run an interactive password
    // prompt, so password-protected collections behave like
    // login-required here.
    let restricted = match check_collection_restriction(&tenant, &media, viewer).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // 3) Look up an existing rendition by (content_hash, filter_spec).
    let rendition_lookup: Result<Vec<MediaRendition>, _> = MediaRendition::objects()
        .where_(MediaRendition::content_hash.eq(media.content_hash.clone()))
        .where_(MediaRendition::filter_spec.eq(canonical_spec.clone()))
        .fetch(tenant.pool())
        .await;
    let existing = match rendition_lookup {
        Ok(v) => v.into_iter().next(),
        Err(e) => {
            tracing::error!(error = %e, "rendition lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "rendition lookup failed").into_response();
        }
    };

    let (rendition, bytes) = match existing {
        Some(r) => {
            match crate::media_storage::disk_for(&tenant.org.slug)
                .load(&crate::media_storage::key(&tenant.org.slug, &r.storage_key))
                .await
            {
                Ok(b) => (r, b),
                Err(_) => {
                    // The row exists but the bytes are gone — regenerate.
                    match generate_once(&tenant, &media, &pipeline, &canonical_spec).await {
                        Ok(out) => out,
                        Err(e) => {
                            tracing::error!(error = %e, "rendition regenerate failed");
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "rendition regenerate failed",
                            )
                                .into_response();
                        }
                    }
                }
            }
        }
        None => match generate_once(&tenant, &media, &pipeline, &canonical_spec).await {
            Ok(out) => out,
            Err(e) => {
                tracing::error!(error = %e, "rendition generate failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("generate: {e}"))
                    .into_response();
            }
        },
    };

    let rendition_mime = rendition.mime.clone();
    // 4) Bump last_used_at so GC can identify cold renditions.
    {
        let mut r = rendition;
        r.last_used_at = chrono::Utc::now();
        r.save_pool(tenant.pool()).await.log_warn("rendition last_used_at not bumped");
    }

    // 5) Stream bytes with strong caching — renditions are
    //    content-addressed so the response is immutable. Content-Type
    //    comes from the rendition (which reflects the chosen encoding
    //    format), NOT the source media mime — they can differ when
    //    `format-X` is in the pipeline.
    let mut resp = bytes.into_response();
    let headers = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&rendition_mime) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    set_media_cache(headers, restricted, "public, max-age=31536000, immutable");
    resp
}

/// Cache policy for served media. A file behind a view restriction was
/// just authorized by the viewer's cookie, which a shared cache or CDN
/// does not key on — a `public` response would be replayed to anyone who
/// asks for the URL next. So restricted media is `private,
/// no-store` and varies on the cookie; everything else keeps `public`.
fn set_media_cache(headers: &mut axum::http::HeaderMap, restricted: bool, public: &'static str) {
    if restricted {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
        headers.insert(header::VARY, HeaderValue::from_static("Cookie"));
    } else {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(public));
    }
}

/// Renditions being generated, keyed by tenant, content hash and spec, so
/// concurrent misses for one rendition wait for the first instead of each
/// decoding the original.
static GENERATING: std::sync::Mutex<
    Option<std::collections::HashMap<String, std::sync::Weak<rustango::__private_runtime::tokio::sync::Mutex<()>>>>,
> = std::sync::Mutex::new(None);

/// The lock for one rendition key, shared by everyone generating it.
fn generation_lock(key: String) -> Arc<rustango::__private_runtime::tokio::sync::Mutex<()>> {
    let mut g = GENERATING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let map = g.get_or_insert_with(Default::default);
    if let Some(lock) = map.get(&key).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    // Drop entries whose generation has finished, so the map stays small.
    map.retain(|_, w| w.strong_count() > 0);
    let lock = Arc::new(rustango::__private_runtime::tokio::sync::Mutex::new(()));
    map.insert(key, Arc::downgrade(&lock));
    lock
}

/// How many renditions may be decoded at once, process-wide. Each decode
/// holds the source file and a full bitmap — tens of MB for a large photo
/// — so an unbounded burst of cold thumbnails could exhaust memory.
fn decode_permits() -> &'static rustango::__private_runtime::tokio::sync::Semaphore {
    static PERMITS: std::sync::OnceLock<rustango::__private_runtime::tokio::sync::Semaphore> =
        std::sync::OnceLock::new();
    PERMITS.get_or_init(|| {
        let n = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
        rustango::__private_runtime::tokio::sync::Semaphore::new(n.max(2))
    })
}

/// [`generate`], once per rendition: a request that finds another already
/// generating the same rendition waits for it and serves its result.
async fn generate_once(
    tenant: &TenantLite,
    media: &Media,
    pipeline: &crate::rendition::FilterPipeline,
    canonical_spec: &str,
) -> Result<(MediaRendition, Vec<u8>), String> {
    let key = format!("{}\u{0}{}\u{0}{canonical_spec}", tenant.org.slug, media.content_hash);
    let lock = generation_lock(key);
    let _held = lock.lock().await;
    // Whoever held the lock before us may have finished this rendition.
    let done: Option<MediaRendition> = MediaRendition::objects()
        .where_(MediaRendition::content_hash.eq(media.content_hash.clone()))
        .where_(MediaRendition::filter_spec.eq(canonical_spec.to_owned()))
        .first(tenant.pool())
        .await
        .map_err(|e| format!("rendition lookup: {e}"))?;
    if let Some(r) = done {
        if let Ok(bytes) = crate::media_storage::disk_for(&tenant.org.slug)
            .load(&crate::media_storage::key(&tenant.org.slug, &r.storage_key))
            .await
        {
            return Ok((r, bytes));
        }
    }
    let _permit = decode_permits()
        .acquire()
        .await
        .map_err(|e| format!("rendition decode permit: {e}"))?;
    generate(tenant, media, pipeline, canonical_spec).await
}

/// Read original bytes, apply filter, persist + index. Returns the
/// freshly-inserted `MediaRendition` row + its bytes (so we don't
/// have to re-read from disk to serve the request that triggered
/// the generate).
async fn generate(
    tenant: &TenantLite,
    media: &Media,
    pipeline: &crate::rendition::FilterPipeline,
    canonical_spec: &str,
) -> Result<(MediaRendition, Vec<u8>), String> {
    // Image decode + resize + re-encode + on-disk read/write are
    // CPU- and I/O-bound work that MUST NOT run on a tokio async
    // worker thread. The `image` crate's `imageops::resize` is a
    // synchronous CPU operation that can take 100–500 ms on a
    // multi-megapixel photo; running it inline blocks every other
    // async task scheduled on that worker, which on a cold-cache
    // media-library visit starves the DB pool (every queued
    // handler — including the editor's page-edit redirect — sits
    // behind the resize). Push it to the blocking thread pool so
    // the async runtime stays responsive.
    // Load the source bytes here (async, via the storage backend) and move
    // them into the blocking task — the backend may be remote (S3), which
    // must never be read from a blocking worker.
    let src_key = crate::media_storage::key(&tenant.org.slug, &media.storage_key);
    let src_bytes = crate::media_storage::disk_for(&tenant.org.slug)
        .load(&src_key)
        .await
        .map_err(|e| format!("read original {src_key}: {e}"))?;
    let pipeline = *pipeline; // `Copy`; move-friendly
    let focal = match (media.focal_point_x, media.focal_point_y) {
        (Some(x), Some(y)) => Some((x, y)),
        _ => None,
    };
    let media_mime = media.mime.clone();
    let content_hash = media.content_hash.clone();
    let canonical_spec_owned = canonical_spec.to_owned();

    // Returns the bytes ready to ship + the metadata needed to
    // persist the row. The DB insert stays on the async runtime
    // because `insert_pool` is itself async (sqlx).
    struct Blocking {
        out_bytes: Vec<u8>,
        out_mime: String,
        storage_key: String,
        width: i32,
        height: i32,
    }
    let blocking: Result<Blocking, String> =
        rustango::__private_runtime::tokio::task::spawn_blocking(move || {
            let img =
                image::load_from_memory(&src_bytes).map_err(|e| format!("decode source: {e}"))?;
            let resized = apply(&img, pipeline.geometric, focal);
            let (w, h) = (resized.width(), resized.height());

            // #79 — honour the pipeline's encoding hints (format /
            // quality / bgcolor). Falls back to the v0.2
            // source-mime-driven default when no `|format-X` segment
            // is present.
            let (out_bytes, out_mime, ext) =
                crate::rendition::encode(&resized, &pipeline.encoding, &media_mime)?;
            // Pipe is legal on POSIX but ugly on disk + risky on
            // Windows; the canonical_spec stays in the DB column
            // verbatim, but the file name swaps `|` for `_` so
            // storage paths are universally safe.
            let safe_spec = canonical_spec_owned.replace('|', "_");
            let storage_key = format!(
                "renditions/{}-{}.{ext}",
                &content_hash[..32.min(content_hash.len())],
                safe_spec
            );

            Ok(Blocking {
                out_bytes,
                out_mime,
                storage_key,
                width: w as i32,
                height: h as i32,
            })
        })
        .await
        .map_err(|e| format!("rendition spawn_blocking joined error: {e}"))?;
    let b = blocking?;
    // Persist the freshly-encoded rendition through the storage backend.
    crate::media_storage::disk_for(&tenant.org.slug)
        .save(
            &crate::media_storage::key(&tenant.org.slug, &b.storage_key),
            &b.out_bytes,
        )
        .await
        .map_err(|e| format!("write rendition: {e}"))?;

    let mut row = MediaRendition {
        id: Auto::Unset,
        media_id: media.id.get().copied().unwrap_or_default(),
        content_hash: media.content_hash.clone(),
        filter_spec: canonical_spec.to_owned(),
        width: b.width,
        height: b.height,
        mime: b.out_mime,
        storage_key: b.storage_key,
        size: b.out_bytes.len() as i64,
        last_used_at: chrono::Utc::now(),
        created_at: Auto::Unset,
    };
    row.insert_pool(tenant.pool())
        .await
        .map_err(|e| format!("insert rendition: {e}"))?;
    Ok((row, b.out_bytes))
}

/// Purge renditions whose `last_used_at` is older than `cutoff`: the
/// stored bytes and the row. Returns how many were removed.
///
/// Nothing in the CMS schedules it — call it from a periodic task or a
/// management command in the host app, with the tenant's pool and slug.
///
/// # Errors
/// Driver / query failures, propagated as-is.
pub async fn gc_older_than(
    pool: &rustango::sql::Pool,
    tenant_slug: &str,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<usize, rustango::sql::ExecError> {
    use rustango::core::Column as _;

    let stale: Vec<MediaRendition> = MediaRendition::objects()
        .where_(MediaRendition::last_used_at.lt(cutoff))
        .fetch(pool)
        .await?;
    let mut removed = 0usize;
    for r in stale {
        crate::media_storage::disk_for(tenant_slug)
            .delete(&crate::media_storage::key(tenant_slug, &r.storage_key))
            .await
            .log_warn("stored file not deleted; the blob is orphaned");
        r.delete_pool(pool).await?;
        removed += 1;
    }
    Ok(removed)
}

/// Whether `user` is a member of any of the role ids in
/// `allowed`. Used by the collection view-restriction guard.
async fn user_in_any_role(
    pool: &rustango::sql::Pool,
    user: &rustango::tenancy::auth::User,
    allowed: &[i64],
) -> bool {
    if allowed.is_empty() {
        return false;
    }
    let Some(user_id) = user.id.get().copied() else {
        return false;
    };
    use rustango::sql::FetcherPool as _;
    let memberships: Vec<rustango::tenancy::permissions::UserRole> =
        match rustango::tenancy::permissions::UserRole::objects()
            .where_(rustango::tenancy::permissions::UserRole::user_id.eq(user_id))
            .fetch(pool)
            .await
        {
            Ok(v) => v,
            Err(_) => return false,
        };
    let mine: std::collections::HashSet<i64> = memberships.iter().map(|m| m.role_id).collect();
    allowed.iter().any(|r| mine.contains(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn served(mime: &str, filename: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        harden_media_headers(&mut h, mime, filename);
        h
    }

    fn get<'a>(h: &'a axum::http::HeaderMap, name: header::HeaderName) -> &'a str {
        h.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
    }

    /// Nothing a user uploads runs as a page on the admin origin.
    #[test]
    fn active_uploads_download_sandboxed_and_unsniffed() {
        for mime in ["text/html", "application/xhtml+xml", "text/xml", "application/javascript", "TEXT/HTML; charset=utf-8"] {
            let h = served(mime, "x.html");
            assert_eq!(get(&h, header::CONTENT_TYPE), "application/octet-stream", "{mime}");
            assert!(get(&h, header::CONTENT_DISPOSITION).starts_with("attachment;"), "{mime}");
            assert_eq!(get(&h, header::CONTENT_SECURITY_POLICY), "sandbox", "{mime}");
            assert_eq!(get(&h, header::X_CONTENT_TYPE_OPTIONS), "nosniff", "{mime}");
        }
    }

    #[test]
    fn svg_stays_inline_but_cannot_run_script() {
        let h = served("image/svg+xml", "logo.svg");
        assert_eq!(get(&h, header::CONTENT_TYPE), "image/svg+xml");
        assert!(h.get(header::CONTENT_DISPOSITION).is_none());
        let csp = get(&h, header::CONTENT_SECURITY_POLICY);
        assert!(csp.contains("default-src 'none'") && csp.contains("sandbox"), "{csp}");
    }

    #[test]
    fn raster_and_viewer_types_stay_inline() {
        let h = served("image/png", "a.png");
        assert_eq!(get(&h, header::CONTENT_TYPE), "image/png");
        assert!(h.get(header::CONTENT_SECURITY_POLICY).is_none());
        let h = served("application/pdf", "a \"b\".pdf");
        assert_eq!(get(&h, header::CONTENT_DISPOSITION), "inline; filename=\"a b.pdf\"");
        assert_eq!(get(&h, header::CONTENT_SECURITY_POLICY), "sandbox");
    }
}

#[cfg(test)]
mod single_flight_tests {
    use super::{decode_permits, generation_lock, GENERATING};
    use std::sync::Arc;

    #[test]
    fn one_rendition_key_shares_one_lock_while_in_flight() {
        let a = generation_lock("t\0hash\0fill-10x10".to_owned());
        let b = generation_lock("t\0hash\0fill-10x10".to_owned());
        assert!(Arc::ptr_eq(&a, &b), "concurrent misses wait on one generation");
        let other = generation_lock("t\0hash\0fill-20x20".to_owned());
        assert!(!Arc::ptr_eq(&a, &other), "another spec generates independently");
        drop((a, b, other));
        let _fresh = generation_lock("t\0hash\0unrelated".to_owned());
        let g = GENERATING.lock().expect("lock");
        let map = g.as_ref().expect("map");
        assert!(!map.contains_key("t\0hash\0fill-10x10"), "finished keys are pruned");
    }

    #[test]
    fn decodes_are_capped() {
        let n = decode_permits().available_permits();
        assert!(n >= 2, "at least two decodes may run: {n}");
        assert!(n <= 1024, "bounded well below the blocking pool: {n}");
    }
}

#[cfg(test)]
mod media_cache_tests {
    use super::set_media_cache;
    use axum::http::{header, HeaderMap};

    #[test]
    fn restricted_media_never_reaches_a_shared_cache() {
        let mut h = HeaderMap::new();
        set_media_cache(&mut h, true, "public, max-age=31536000, immutable");
        assert_eq!(h[header::CACHE_CONTROL], "private, no-store");
        assert_eq!(h[header::VARY], "Cookie");

        let mut h = HeaderMap::new();
        set_media_cache(&mut h, false, "public, max-age=3600");
        assert_eq!(h[header::CACHE_CONTROL], "public, max-age=3600");
        assert!(h.get(header::VARY).is_none());
    }
}
