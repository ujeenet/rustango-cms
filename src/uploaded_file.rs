//! Scratch storage for multi-step media uploads (#142, Wagtail
//! parity D9).
//!
//! Mirrors `wagtail.models.media.UploadedFile`: the editor uploads
//! bytes first, then fills in title / alt / collection on a follow-up
//! form, then the row commits to [`crate::media::Media`]. If the
//! editor cancels or closes the tab mid-step, the scratch row is
//! collected by the background sweep — no orphan media leaks into
//! `cms_media`.
//!
//! Existing single-step `media_upload_submit` keeps working unchanged;
//! this primitive is the foundation for future drag-drop / multi-file
//! UIs where bytes arrive faster than the editor can fill metadata.
//!
//! Scratch objects live under the storage key `<tenant>/scratch/<hash>-<filename>`
//! so the commit path can rename in place rather than copying bytes
//! across volumes.

use chrono::{DateTime, Duration, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// How long a scratch upload sticks around before the sweep collects
/// it. Generous — editors sometimes step away mid-upload.
pub const STALE_THRESHOLD_HOURS: i64 = 24;

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_uploaded_file",
    app = "cms",
    display = "original_filename",
    admin(
        list_display = "user_id, original_filename, mime, size, uploaded_at",
        ordering = "-uploaded_at",
    )
)]
pub struct UploadedFile {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Editor who uploaded — for permission gating on commit.
    #[rustango(fk = "rustango_users", on = "id", index)]
    pub user_id: i64,

    /// Opaque key inside the tenant's media storage backend. Living
    /// under a `scratch/` prefix so directory listings make the
    /// distinction obvious.
    #[rustango(max_length = 512)]
    pub storage_key: String,

    #[rustango(max_length = 255)]
    pub original_filename: String,

    #[rustango(max_length = 128)]
    pub mime: String,

    pub size: i64,

    /// SHA-256 hex. Used by commit to dedup against existing
    /// `cms_media` rows.
    #[rustango(max_length = 64, index)]
    pub content_hash: String,

    #[rustango(auto_now_add)]
    pub uploaded_at: Auto<DateTime<Utc>>,
}

/// Insert one scratch row.
///
/// # Errors
/// Driver / query failures.
pub async fn create(
    pool: &rustango::sql::Pool,
    user_id: i64,
    storage_key: String,
    original_filename: String,
    mime: String,
    size: i64,
    content_hash: String,
) -> Result<UploadedFile, rustango::sql::ExecError> {
    let mut row = UploadedFile {
        id: Auto::Unset,
        user_id,
        storage_key,
        original_filename,
        mime,
        size,
        content_hash,
        uploaded_at: Auto::Unset,
    };
    row.insert_pool(pool).await?;
    Ok(row)
}

/// Look up a scratch row by id. Returns `None` when missing — callers
/// should treat that as the editor cancelled / sweep collected.
///
/// # Errors
/// Driver / query failures.
pub async fn get(
    pool: &rustango::sql::Pool,
    id: i64,
) -> Result<Option<UploadedFile>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let mut rows: Vec<UploadedFile> = UploadedFile::objects()
        .where_(UploadedFile::id.eq(id))
        .fetch(pool)
        .await?;
    Ok(rows.pop())
}

/// Delete one scratch row by id. Returns the number of rows removed
/// (0 or 1). Filesystem cleanup is the caller's responsibility — this
/// only touches the DB row.
///
/// # Errors
/// Driver / query failures.
pub async fn delete(
    pool: &rustango::sql::Pool,
    id: i64,
) -> Result<usize, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<UploadedFile> = UploadedFile::objects()
        .where_(UploadedFile::id.eq(id))
        .fetch(pool)
        .await?;
    let mut removed = 0;
    for row in rows {
        row.delete_pool(pool).await?;
        removed += 1;
    }
    Ok(removed)
}

/// Drop every scratch row older than [`STALE_THRESHOLD_HOURS`].
/// Filesystem-side cleanup is the caller's responsibility — the sweep
/// returns the list of `storage_key`s the caller should unlink so the
/// IO happens outside any held DB lock.
///
/// # Errors
/// Driver / query failures.
pub async fn gc_stale(pool: &rustango::sql::Pool) -> Result<Vec<String>, rustango::sql::ExecError> {
    use rustango::sql::FetcherPool as _;
    let cutoff = Utc::now() - Duration::hours(STALE_THRESHOLD_HOURS);
    let all: Vec<UploadedFile> = UploadedFile::objects().fetch(pool).await?;
    let mut unlinked: Vec<String> = Vec::new();
    for row in all {
        let uploaded_at = row.uploaded_at.get().copied();
        let Some(ts) = uploaded_at else {
            continue;
        };
        if ts >= cutoff {
            continue;
        }
        let key = row.storage_key.clone();
        if row.delete_pool(pool).await.is_ok() {
            unlinked.push(key);
        }
    }
    Ok(unlinked)
}
