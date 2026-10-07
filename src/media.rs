//! `Media` — per-tenant uploaded files (images + documents).
//!
//! Single table with a `kind` discriminator so the admin can show
//! two filtered views ("Media" for images, "Documents" for the
//! rest) over the same storage backend.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// One row per uploaded file. Bytes live behind the per-tenant
/// `Storage` trait, keyed by `storage_key`. Resized variants live in
/// `cms_media_rendition` (see `crate::rendition`); this table is just the
/// upload surface.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_media",
    app = "cms",
    display = "title",
    admin(
        list_display = "title, filename, kind, mime, size, uploaded_at",
        search_fields = "title, filename, alt_text, description",
        ordering = "-uploaded_at",
        list_filter = "kind",
    )
)]
pub struct Media {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Sanitized original filename.
    #[rustango(max_length = 255)]
    pub filename: String,

    /// SHA-256 hex of the file contents. Used for rendition cache
    /// keys + cross-tenant dedup (not implemented in this slice).
    #[rustango(max_length = 64, index)]
    pub content_hash: String,

    /// MIME type detected at upload time.
    #[rustango(max_length = 128)]
    pub mime: String,

    /// Size in bytes.
    pub size: i64,

    /// Discriminator: `"image"`, `"document"`, or `"other"`.
    #[rustango(max_length = 16, index)]
    pub kind: String,

    /// Image-only: pixel width.
    pub width: Option<i32>,

    /// Image-only: pixel height.
    pub height: Option<i32>,

    /// Opaque key inside the tenant's `Storage` backend.
    #[rustango(max_length = 512)]
    pub storage_key: String,

    /// Friendly title shown in the admin picker. Defaults to the
    /// filename without extension.
    #[rustango(max_length = 255)]
    pub title: String,

    /// Alt text — the contextual accessibility string for images.
    /// Optional on non-image kinds. Distinct from [`Self::description`].
    #[rustango(max_length = 1024)]
    pub alt_text: String,

    /// Decorative / editorial description — longer metadata about the
    /// asset (credit, context, usage notes). NOT used as the image
    /// `alt`: `alt_text` carries the accessibility string, this is
    /// human-facing metadata only.
    ///
    /// `default = "''"` so the AddColumn migration can backfill
    /// existing `cms_media` rows — a NOT NULL column added to a table
    /// with data needs a default (SQLite rejects it outright; the
    /// framework's migration guard rejects it on every dialect).
    #[rustango(max_length = 1024, default = "''")]
    pub description: String,

    /// FK to `rustango_users.id` of the operator who uploaded — left
    /// as a plain `Option<i64>` (no compile-time FK) so the cms crate
    /// doesn't have to depend on the tenant-user model shape.
    pub uploaded_by: Option<i64>,

    /// FK to the collection this row belongs to. NULL means
    /// "uncategorized" — render as the root collection in the admin.
    /// The seed pass creates a default "Root" collection per tenant
    /// so most rows land there.
    #[rustango(fk = "cms_media_collection", on = "id", index)]
    pub collection_id: Option<i64>,

    /// Focal point — normalized x coordinate (0.0 = left edge,
    /// 1.0 = right edge). When NULL, `fill-*` renditions crop
    /// around the geometric center.
    pub focal_point_x: Option<f32>,

    /// Focal point — normalized y coordinate (0.0 = top, 1.0 = bottom).
    pub focal_point_y: Option<f32>,

    #[rustango(auto_now_add)]
    pub uploaded_at: Auto<DateTime<Utc>>,
}

// Built-in "media" chooser so `WidgetKind::MediaPicker` renders the shared
// chooser overlay (thumbnails + search + selected-image preview) instead of a
// bare <select>. Backs `/cms-admin/__chooser/media`; rows carry `kind`, so the
// overlay shows image thumbnails.
crate::register_chooser!(crate::media::Media, "media", "Image", "title", "filename");

/// Editor-curated folder for media. Tree-shaped (`parent_id` FK to
/// self) so sites can group assets per team / per campaign / per
/// product, with sub-folders for finer-grain organization.
///
/// One "Root" collection is auto-seeded per tenant on first boot so
/// every upload has somewhere to land. Hosts can rename or delete
/// the root — the `Media.collection_id` FK is nullable, so orphan
/// rows just show under the root view.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_media_collection",
    app = "cms",
    display = "name",
    admin(
        list_display = "name, parent_id, sort_order, created_at",
        ordering = "parent_id, sort_order, name",
    )
)]
pub struct MediaCollection {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Display name. Unique within a parent so the breadcrumb path
    /// reads cleanly.
    #[rustango(max_length = 100)]
    pub name: String,

    /// Tree parent — None for the root collection per tenant.
    #[rustango(fk = "cms_media_collection", on = "id", index)]
    pub parent_id: Option<i64>,

    /// Sibling order — editors drag to reorder; rendered in
    /// ascending sort_order.
    pub sort_order: i32,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

impl Media {
    /// Public download URL — serves the original bytes regardless of
    /// kind (image, document, file). Use in `<img src>`, `<a href>`,
    /// or anywhere a stable URL for the raw file is needed. The
    /// underlying route is `/__media__/raw/{id}`, mounted by
    /// [`crate::rendition_route::router`].
    #[must_use]
    pub fn public_url(&self) -> String {
        match self.id.get() {
            Some(id) => format!("/__media__/raw/{id}"),
            None => String::new(),
        }
    }

    /// Sized-image URL via the rendition pipeline (image-only).
    /// `filter` is a pipeline spec, e.g. `"fill-300x200"`,
    /// `"width-800"`, `"max-1200x800|format-webp"`. Returns empty
    /// for non-image rows so callers can fall back to
    /// [`Self::public_url`].
    #[must_use]
    pub fn image_url(&self, filter: &str) -> String {
        if self.kind != "image" {
            return String::new();
        }
        match self.id.get() {
            Some(id) => format!("/__media__/{filter}/{id}"),
            None => String::new(),
        }
    }

    /// Classify a MIME type into the storage `kind` discriminator.
    pub fn classify_mime(mime: &str) -> &'static str {
        if mime.starts_with("image/") {
            "image"
        } else if mime.starts_with("video/")
            || mime.starts_with("audio/")
            || mime == "application/pdf"
            || mime.starts_with("application/vnd.")
            || mime.starts_with("application/msword")
            || mime.starts_with("application/x-")
            || mime == "text/plain"
            || mime == "text/csv"
        {
            "document"
        } else {
            "other"
        }
    }
}
