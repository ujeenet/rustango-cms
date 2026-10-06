//! Image renditions — resized variants of a `Media` row served at
//! `/__media__/<filter_spec>/<media_id>`.
//!
//! Rendition pipeline:
//!
//! 1. Tera template emits `<img src="/__media__/fill-300x200/42">`
//!    via the `rcms_image_url(media_id, filter_spec)` Tera function.
//! 2. The public route handler looks up the [`MediaRendition`] keyed
//!    by `(media.content_hash, filter_spec)`. On hit it streams the
//!    cached bytes from storage; on miss it loads the original,
//!    resizes via the [`image`] crate, writes the result, and
//!    inserts a row.
//! 3. `cms_media_rendition` rows are GC'd by a future
//!    `manage cms gc-renditions --older-than 30d` verb (deferred).
//!
//! Dedup happens at the `(content_hash, filter_spec)` level — two
//! `Media` rows with identical bytes share renditions automatically.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// One row per (media, filter_spec) pair. Multiple `Media` rows
/// sharing the same `content_hash` share renditions implicitly via
/// the `(content_hash, filter_spec)` key — re-uploading the same
/// image doesn't regenerate renditions.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_media_rendition",
    app = "cms",
    admin(
        list_display = "media_id, filter_spec, width, height, last_used_at",
        ordering = "-last_used_at",
        list_filter = "filter_spec",
    )
)]
pub struct MediaRendition {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// FK to `cms_media.id`. The rendition is keyed by
    /// `(content_hash, filter_spec)` though — see module docs.
    #[rustango(fk = "cms_media", on = "id", index)]
    pub media_id: i64,

    /// SHA-256 hex of the SOURCE bytes — same value as
    /// `cms_media.content_hash` for the source row. Indexed for the
    /// rendition lookup keyed on `(content_hash, filter_spec)`.
    #[rustango(max_length = 64, index)]
    pub content_hash: String,

    /// Parsed-then-canonicalized filter spec, e.g. `"fill-300x200"`,
    /// `"max-1200x900"`, `"width-800"`. The route handler accepts
    /// these from URL paths; invalid specs return 400.
    #[rustango(max_length = 64, index)]
    pub filter_spec: String,

    /// Pixel dimensions of the resized output.
    pub width: i32,
    pub height: i32,

    /// MIME type of the rendered bytes. Usually `image/webp` for
    /// re-encoded sources, but PNG / JPEG passthrough when the
    /// filter is just a resize.
    #[rustango(max_length = 128)]
    pub mime: String,

    /// Storage backend key, resolved as `<tenant_slug>/<storage_key>`
    /// by [`crate::media_storage`] — local disk by default, S3/R2/MinIO
    /// when the host installs a backend. Renditions use the
    /// `renditions/<hash>-<spec>.<ext>` shape.
    #[rustango(max_length = 512)]
    pub storage_key: String,

    /// File size in bytes — used by the eventual GC to prioritize
    /// large or stale renditions.
    pub size: i64,

    /// Bumped on every successful serve so a GC pass can prune
    /// renditions that haven't been viewed in N days.
    pub last_used_at: chrono::DateTime<chrono::Utc>,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

// ---------- filter spec ----------

/// Parsed filter spec — what the URL looked like, broken down into a
/// resize operation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterOp {
    /// Resize so the result EXACTLY fits the box, cropping overflow.
    Fill { w: u32, h: u32 },
    /// Resize so the result fits WITHIN the box, preserving aspect.
    Max { w: u32, h: u32 },
    /// Resize to a target width, scale height to preserve aspect.
    Width(u32),
    /// Resize to a target height, scale width to preserve aspect.
    Height(u32),
}

/// Encoding hints — applied after the geometric resize. Filter
/// pipeline spec encodes these as `|format-X|quality-N|bgcolor-XYZ`
/// chained after the resize op. All optional; defaults match the
/// source format (JPEG → JPEG, anything else → PNG).
///
/// `Format::Auto` is reserved for future content-negotiation via
/// the `Accept` header — today it behaves like `Format::Webp`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EncodingOps {
    pub format: Option<EncodeFormat>,
    /// 1..=100 — applied to JPEG / WebP / AVIF encoders. PNG ignores.
    pub quality: Option<u8>,
    /// RGB background painted under transparent pixels before
    /// encoding to a non-alpha format. Hex triplet — e.g.
    /// `bgcolor-fff` or `bgcolor-ffffff` (3 or 6 hex chars).
    pub background: Option<(u8, u8, u8)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EncodeFormat {
    Jpeg,
    Png,
    Webp,
    /// AVIF (#398). Encoding requires the `avif` crate feature; without
    /// it, `encode` returns a clear error. Parsing + the URL spec always
    /// recognize `format-avif`.
    Avif,
    /// Content negotiation — picks the best the request accepts.
    /// v0 behaves like WebP (browsers all support it). Future: read
    /// the `Accept` header to pick AVIF when supported.
    Auto,
}

impl EncodeFormat {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "jpeg" | "jpg" => Self::Jpeg,
            "png" => Self::Png,
            "webp" => Self::Webp,
            "avif" => Self::Avif,
            "auto" => Self::Auto,
            _ => return None,
        })
    }
    fn canonical(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::Auto => "auto",
        }
    }
    fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Avif => "image/avif",
            Self::Webp | Self::Auto => "image/webp",
        }
    }
    fn ext(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Avif => "avif",
            Self::Webp | Self::Auto => "webp",
        }
    }
}

/// One end-to-end filter pipeline: an optional resize + optional
/// encoding hints. Parsed from a pipe-separated spec like
/// `fill-300x200|format-webp|quality-75|bgcolor-fff`. The first
/// segment must be a resize op (`fill`/`max`/`width`/`height`);
/// remaining segments classify into `EncodingOps`.
///
/// Backward compat: bare specs without a `|` (e.g. `fill-300x200`)
/// parse to a pipeline with no encoding hints, mirroring the v0.2
/// behavior.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FilterPipeline {
    pub geometric: FilterOp,
    pub encoding: EncodingOps,
}

#[derive(Debug, thiserror::Error)]
pub enum FilterError {
    #[error("unknown filter spec `{0}` — expected fill-WxH | max-WxH | width-W | height-H")]
    Unknown(String),
    #[error("invalid dimensions in `{0}`")]
    BadDimensions(String),
    #[error("invalid encoding op `{0}` — expected format-X | quality-N | bgcolor-XYZ")]
    BadEncoding(String),
}

impl FilterPipeline {
    /// Parse a pipe-separated spec into a pipeline. The first
    /// segment must be a resize op; subsequent segments encode
    /// hints.
    pub fn parse(raw: &str) -> Result<Self, FilterError> {
        let mut parts = raw.split('|');
        let head = parts.next().unwrap_or("");
        let geometric = FilterOp::parse(head)?;
        let mut encoding = EncodingOps::default();
        for seg in parts {
            let seg = seg.trim();
            if seg.is_empty() {
                continue;
            }
            if let Some(rest) = seg.strip_prefix("format-") {
                encoding.format = Some(
                    EncodeFormat::parse(rest)
                        .ok_or_else(|| FilterError::BadEncoding(seg.to_owned()))?,
                );
            } else if let Some(rest) = seg.strip_prefix("quality-") {
                let n: u8 = rest
                    .parse()
                    .map_err(|_| FilterError::BadEncoding(seg.to_owned()))?;
                if !(1..=100).contains(&n) {
                    return Err(FilterError::BadEncoding(seg.to_owned()));
                }
                encoding.quality = Some(n);
            } else if let Some(rest) = seg.strip_prefix("bgcolor-") {
                encoding.background = Some(
                    parse_hex_rgb(rest).ok_or_else(|| FilterError::BadEncoding(seg.to_owned()))?,
                );
            } else {
                return Err(FilterError::BadEncoding(seg.to_owned()));
            }
        }
        Ok(Self {
            geometric,
            encoding,
        })
    }

    /// Canonical string form (round-trips through `parse`).
    /// Encoding hints are emitted only when non-default.
    pub fn canonical(&self) -> String {
        let mut out = self.geometric.canonical();
        if let Some(fmt) = self.encoding.format {
            out.push('|');
            out.push_str("format-");
            out.push_str(fmt.canonical());
        }
        if let Some(q) = self.encoding.quality {
            out.push_str(&format!("|quality-{q}"));
        }
        if let Some((r, g, b)) = self.encoding.background {
            out.push_str(&format!("|bgcolor-{r:02x}{g:02x}{b:02x}"));
        }
        out
    }
}

/// Parse `fff` or `ffffff` into `(R, G, B)` bytes. 4-digit alpha
/// shorthand is intentionally rejected — bgcolor flattens, alpha
/// has no place.
fn parse_hex_rgb(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim_start_matches('#');
    let hex = match s.len() {
        3 => {
            // Expand `fff` → `ffffff`
            let mut out = String::with_capacity(6);
            for c in s.chars() {
                out.push(c);
                out.push(c);
            }
            out
        }
        6 => s.to_owned(),
        _ => return None,
    };
    crate::theme::parse_rgb6(&hex)
}

impl FilterOp {
    /// Parse a URL-shape spec: `fill-300x200`, `max-1200x900`,
    /// `width-800`, `height-600`. Hyphens, lowercase only.
    pub fn parse(raw: &str) -> Result<Self, FilterError> {
        let raw = raw.trim();
        // Helper: parse `WxH` after a known prefix.
        let parse_pair = |s: &str| -> Result<(u32, u32), FilterError> {
            let (w, h) = s
                .split_once('x')
                .ok_or_else(|| FilterError::BadDimensions(raw.to_owned()))?;
            let w: u32 = w
                .parse()
                .map_err(|_| FilterError::BadDimensions(raw.to_owned()))?;
            let h: u32 = h
                .parse()
                .map_err(|_| FilterError::BadDimensions(raw.to_owned()))?;
            if w == 0 || h == 0 || w > 8192 || h > 8192 {
                return Err(FilterError::BadDimensions(raw.to_owned()));
            }
            Ok((w, h))
        };
        if let Some(rest) = raw.strip_prefix("fill-") {
            let (w, h) = parse_pair(rest)?;
            return Ok(Self::Fill { w, h });
        }
        if let Some(rest) = raw.strip_prefix("max-") {
            let (w, h) = parse_pair(rest)?;
            return Ok(Self::Max { w, h });
        }
        if let Some(rest) = raw.strip_prefix("width-") {
            let n: u32 = rest
                .parse()
                .map_err(|_| FilterError::BadDimensions(raw.to_owned()))?;
            if n == 0 || n > 8192 {
                return Err(FilterError::BadDimensions(raw.to_owned()));
            }
            return Ok(Self::Width(n));
        }
        if let Some(rest) = raw.strip_prefix("height-") {
            let n: u32 = rest
                .parse()
                .map_err(|_| FilterError::BadDimensions(raw.to_owned()))?;
            if n == 0 || n > 8192 {
                return Err(FilterError::BadDimensions(raw.to_owned()));
            }
            return Ok(Self::Height(n));
        }
        Err(FilterError::Unknown(raw.to_owned()))
    }

    /// Canonical string form (round-trips through `parse`).
    pub fn canonical(&self) -> String {
        match self {
            Self::Fill { w, h } => format!("fill-{w}x{h}"),
            Self::Max { w, h } => format!("max-{w}x{h}"),
            Self::Width(n) => format!("width-{n}"),
            Self::Height(n) => format!("height-{n}"),
        }
    }
}

/// Apply `op` to `original` (decoded image) and return the resized
/// image. Filters are cheap-and-correct (Lanczos3). For `Fill`,
/// `focal` (when `Some`) shifts the crop so the focal point stays
/// inside the target rectangle — passes (0.5, 0.5) for "no focal
/// set, use geometric center" semantics.
pub fn apply(
    original: &image::DynamicImage,
    op: FilterOp,
    focal: Option<(f32, f32)>,
) -> image::DynamicImage {
    use image::imageops::FilterType;
    let (sw, sh) = (original.width(), original.height());
    match op {
        FilterOp::Fill { w, h } => {
            // Resize-to-fill: scale so the smaller side covers the
            // target, then crop around the focal point. The focal
            // point is in source-image coordinates as a 0..1
            // fraction; it rides along with the uniform scale.
            let scale_w = w as f32 / sw as f32;
            let scale_h = h as f32 / sh as f32;
            let scale = scale_w.max(scale_h);
            let new_w = (sw as f32 * scale).round() as u32;
            let new_h = (sh as f32 * scale).round() as u32;
            let resized = original.resize_exact(new_w, new_h, FilterType::Lanczos3);
            let (fx, fy) = focal.unwrap_or((0.5, 0.5));
            let fx = fx.clamp(0.0, 1.0);
            let fy = fy.clamp(0.0, 1.0);
            // Center the crop on the focal point in resized
            // coordinates, then clamp so the crop stays inside.
            let max_x = new_w.saturating_sub(w);
            let max_y = new_h.saturating_sub(h);
            let target_x = (fx * new_w as f32) - (w as f32 / 2.0);
            let target_y = (fy * new_h as f32) - (h as f32 / 2.0);
            let x_off = target_x.clamp(0.0, max_x as f32) as u32;
            let y_off = target_y.clamp(0.0, max_y as f32) as u32;
            resized.crop_imm(x_off, y_off, w.min(new_w), h.min(new_h))
        }
        FilterOp::Max { w, h } => original.resize(w, h, FilterType::Lanczos3),
        FilterOp::Width(n) => {
            let h = (sh as f32 * (n as f32 / sw as f32)).round() as u32;
            original.resize_exact(n, h.max(1), FilterType::Lanczos3)
        }
        FilterOp::Height(n) => {
            let w = (sw as f32 * (n as f32 / sh as f32)).round() as u32;
            original.resize_exact(w.max(1), n, FilterType::Lanczos3)
        }
    }
}

/// Encode `image` per the supplied [`EncodingOps`], applying
/// background-color flattening when targeting a non-alpha format
/// (JPEG). Returns `(bytes, mime, extension)`. When `encoding.format`
/// is `None`, falls back to `source_mime` (JPEG passthrough, anything
/// else → PNG) to preserve the v0.2 behavior.
///
/// # Errors
/// Encoder failures from the [`image`] crate.
pub fn encode(
    image: &image::DynamicImage,
    encoding: &EncodingOps,
    source_mime: &str,
) -> Result<(Vec<u8>, String, String), String> {
    use image::codecs::jpeg::JpegEncoder;
    let format = encoding.format.unwrap_or_else(|| {
        if source_mime == "image/jpeg" || source_mime == "image/jpg" {
            EncodeFormat::Jpeg
        } else if source_mime == "image/webp" {
            EncodeFormat::Webp
        } else {
            EncodeFormat::Png
        }
    });

    // Background-color flatten when targeting a non-alpha format.
    // PNG + WebP both honour alpha; JPEG doesn't.
    let needs_flatten = matches!(format, EncodeFormat::Jpeg) || encoding.background.is_some();
    let prepared: image::DynamicImage = if needs_flatten && encoding.background.is_some() {
        let (r, g, b) = encoding.background.unwrap();
        flatten_onto_background(image, r, g, b)
    } else if matches!(format, EncodeFormat::Jpeg) && image.color().has_alpha() {
        // No explicit bgcolor but JPEG demands opaque — flatten on
        // white by default. Matches Wagtail's behavior.
        flatten_onto_background(image, 255, 255, 255)
    } else {
        image.clone()
    };

    let mut out: Vec<u8> = Vec::new();
    match format {
        EncodeFormat::Jpeg => {
            let quality = encoding.quality.unwrap_or(85);
            let mut cur = std::io::Cursor::new(&mut out);
            let mut enc = JpegEncoder::new_with_quality(&mut cur, quality);
            enc.encode_image(&prepared)
                .map_err(|e| format!("encode jpeg: {e}"))?;
        }
        EncodeFormat::Png => {
            // PNG is lossless; `quality` doesn't apply.
            let mut cur = std::io::Cursor::new(&mut out);
            prepared
                .write_to(&mut cur, image::ImageFormat::Png)
                .map_err(|e| format!("encode png: {e}"))?;
        }
        EncodeFormat::Webp | EncodeFormat::Auto => {
            // The `image` crate's WebP encoder is lossless by
            // default. The `quality` knob is accepted for API
            // symmetry but ignored — swapping in a lossy WebP
            // encoder is a feature-flag follow-up.
            let mut cur = std::io::Cursor::new(&mut out);
            prepared
                .write_to(&mut cur, image::ImageFormat::WebP)
                .map_err(|e| format!("encode webp: {e}"))?;
        }
        EncodeFormat::Avif => {
            // #398 — AVIF, gated behind the `avif` feature (pulls the
            // ravif/rav1e encoder). AVIF supports alpha, so no flatten.
            #[cfg(feature = "avif")]
            {
                // Quality 1..=100; default 61 (Wagtail 7.3's AVIF default).
                // Speed 1..=10 (higher = faster/larger); 4 balances both.
                let quality = encoding.quality.unwrap_or(61);
                let mut cur = std::io::Cursor::new(&mut out);
                let enc =
                    image::codecs::avif::AvifEncoder::new_with_speed_quality(&mut cur, 4, quality);
                prepared
                    .write_with_encoder(enc)
                    .map_err(|e| format!("encode avif: {e}"))?;
            }
            #[cfg(not(feature = "avif"))]
            {
                let _ = &prepared;
                return Err(
                    "AVIF output requires building rustango-cms with the `avif` feature".to_owned(),
                );
            }
        }
    }
    Ok((out, format.mime().to_owned(), format.ext().to_owned()))
}

/// Composite `src` onto a solid (r, g, b) background. Used by the
/// JPEG path (no alpha) + the explicit `bgcolor-XYZ` op.
fn flatten_onto_background(src: &image::DynamicImage, r: u8, g: u8, b: u8) -> image::DynamicImage {
    use image::GenericImageView;
    let (w, h) = src.dimensions();
    let mut buf = image::RgbaImage::from_pixel(w, h, image::Rgba([r, g, b, 255]));
    let rgba = src.to_rgba8();
    image::imageops::overlay(&mut buf, &rgba, 0, 0);
    image::DynamicImage::ImageRgba8(buf)
}

// ---------- Hard crop (#24) ----------

/// [`crop_image`] on the blocking pool (#694): a full decode + encode
/// is hundreds of milliseconds of CPU for a large photo, and inline it
/// parked a Tokio worker for all of it.
///
/// # Errors
/// As [`crop_image`].
pub async fn crop_image_off_runtime(
    bytes: Vec<u8>,
    source_mime: String,
    (x, y, w, h): (u32, u32, u32, u32),
) -> Result<(Vec<u8>, String), String> {
    rustango::__private_runtime::tokio::task::spawn_blocking(move || {
        crop_image(&bytes, &source_mime, x, y, w, h)
    })
    .await
    .map_err(|e| format!("crop worker failed: {e}"))?
}

/// [`strip_exif_if_image`] on the blocking pool (#694). Returns the bytes
/// to store — the re-encoded ones when EXIF was stripped, else the input —
/// and the decoded dimensions.
pub async fn strip_exif_off_runtime(bytes: Vec<u8>, mime: String) -> (Vec<u8>, Option<(u32, u32)>) {
    if !mime.starts_with("image/") || mime == "image/svg+xml" {
        return (bytes, None);
    }
    rustango::__private_runtime::tokio::task::spawn_blocking(move || {
        let (stripped, dims) = strip_exif_if_image(&bytes, &mime);
        (stripped.unwrap_or(bytes), dims)
    })
    .await
    .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
}

/// Decode `bytes`, crop to `(x, y, w, h)` in source-pixel coords,
/// and re-encode in the same format. Returns `(cropped_bytes, mime)`.
/// SVG inputs are rejected — vector crop is a different surface.
///
/// # Errors
/// * Image-decode failure (corrupt or unsupported format)
/// * Crop rectangle outside the source bounds
/// * Re-encode failure
pub fn crop_image(
    bytes: &[u8],
    source_mime: &str,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<(Vec<u8>, String), String> {
    if source_mime == "image/svg+xml" {
        return Err(
            "Cropping SVG sources isn't supported — clone first, edit the markup directly."
                .to_owned(),
        );
    }
    let img = image::load_from_memory(bytes).map_err(|e| format!("decode source: {e}"))?;
    let (sw, sh) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return Err("Crop rectangle must have non-zero width + height.".to_owned());
    }
    if x.saturating_add(w) > sw || y.saturating_add(h) > sh {
        return Err(format!(
            "Crop rectangle ({x},{y},{w},{h}) is outside the source ({sw}×{sh})."
        ));
    }
    let cropped = img.crop_imm(x, y, w, h);
    let mut out: Vec<u8> = Vec::new();
    let out_mime = if source_mime == "image/jpeg" || source_mime == "image/jpg" {
        let mut cur = std::io::Cursor::new(&mut out);
        cropped
            .write_to(&mut cur, image::ImageFormat::Jpeg)
            .map_err(|e| format!("encode jpeg: {e}"))?;
        "image/jpeg".to_owned()
    } else if source_mime == "image/webp" {
        let mut cur = std::io::Cursor::new(&mut out);
        cropped
            .write_to(&mut cur, image::ImageFormat::WebP)
            .map_err(|e| format!("encode webp: {e}"))?;
        "image/webp".to_owned()
    } else {
        // Default to PNG (lossless) for everything else — PNG, GIF,
        // BMP all funnel into a PNG output. Editors who want a
        // specific format can re-upload.
        let mut cur = std::io::Cursor::new(&mut out);
        cropped
            .write_to(&mut cur, image::ImageFormat::Png)
            .map_err(|e| format!("encode png: {e}"))?;
        "image/png".to_owned()
    };
    Ok((out, out_mime))
}

/// Re-encode an image's bytes to strip EXIF metadata (#184). Returns
/// `(Some(stripped_bytes), Some((width, height)))` on success; for
/// non-image MIMEs or SVGs returns `(None, None)`; for decode errors
/// returns `(None, None)` so the caller can keep the original bytes.
///
/// `image::DynamicImage::write_to` never emits EXIF, so the decode +
/// re-encode pass is pixel-equivalent without GPS / camera serial /
/// timestamp metadata leaking through.
#[must_use]
pub fn strip_exif_if_image(bytes: &[u8], mime: &str) -> (Option<Vec<u8>>, Option<(u32, u32)>) {
    if !mime.starts_with("image/") || mime == "image/svg+xml" {
        return (None, None);
    }
    let img = match image::load_from_memory(bytes) {
        Ok(img) => img,
        Err(e) => {
            tracing::warn!(
                target: "rustango_cms::rendition",
                mime, error = %e,
                "EXIF strip: decode failed, keeping original bytes"
            );
            return (None, None);
        }
    };
    let dims = (img.width(), img.height());
    let format = match mime {
        "image/jpeg" | "image/jpg" => image::ImageFormat::Jpeg,
        "image/png" => image::ImageFormat::Png,
        "image/webp" => image::ImageFormat::WebP,
        "image/gif" => image::ImageFormat::Gif,
        "image/avif" => image::ImageFormat::Avif,
        // Fall back to PNG (lossless) for less-common inputs.
        _ => image::ImageFormat::Png,
    };
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    {
        let mut cur = std::io::Cursor::new(&mut out);
        if let Err(e) = img.write_to(&mut cur, format) {
            tracing::warn!(
                target: "rustango_cms::rendition",
                mime, error = %e,
                "EXIF strip: re-encode failed, keeping original bytes"
            );
            return (None, Some(dims));
        }
    }
    (Some(out), Some(dims))
}

// ---------- Tera integration ----------

/// Register `rcms_image_url(media_id, filter)` on the user's Tera
/// instance. Templates write
/// `<img src="{{ rcms_image_url(media_id=42, filter="fill-300x200") }}">`
/// and the URL gets resolved lazily by the
/// [`crate::rendition_route::router`] route. The function only
/// validates the filter spec; it does NOT touch the DB, so it stays
/// sync + cheap.
///
/// Call once during Tera setup, alongside
/// `rustango_cms::admin::register_templates(&mut tera)`.
pub fn register_tera_function(tera: &mut tera::Tera) {
    tera.register_function("rcms_image_url", RcmsImageUrlFn);
    // #426 — responsive helpers. `srcset` emits the `srcset` value;
    // `picture` emits a full multi-format `<picture>`.
    tera.register_function("rcms_image_srcset", RcmsImageSrcsetFn);
    tera.register_function("rcms_image_srcset_dpi", RcmsImageSrcsetDpiFn);
    tera.register_function("rcms_picture", RcmsPictureFn);
}

/// `is_safe = true` so Tera autoescape doesn't mangle `/` into
/// `&#x2F;` in the rendered URL. The output is a static-shape
/// URL (`/__media__/<spec>/<id>[?v=<hash>]`) under CMS control —
/// no user input flows into it.
struct RcmsImageUrlFn;

impl tera::Function for RcmsImageUrlFn {
    fn call(
        &self,
        args: &std::collections::HashMap<String, tera::Value>,
    ) -> tera::Result<tera::Value> {
        rcms_image_url(args)
    }
    fn is_safe(&self) -> bool {
        true
    }
}

fn rcms_image_url(
    args: &std::collections::HashMap<String, tera::Value>,
) -> tera::Result<tera::Value> {
    let media_id = parse_media_id_arg(args)
        .ok_or_else(|| tera::Error::msg("rcms_image_url: `media_id` (i64) is required"))?;
    let filter = args
        .get("filter")
        .and_then(tera::Value::as_str)
        .ok_or_else(|| tera::Error::msg("rcms_image_url: `filter` (string) is required"))?;
    let op =
        FilterOp::parse(filter).map_err(|e| tera::Error::msg(format!("rcms_image_url: {e}")))?;
    // Optional content-hash cache-buster. The route serves renditions
    // with `Cache-Control: immutable`, so when the underlying bytes
    // change (e.g. an in-place crop) the URL must change too. Callers
    // pass `v=<media.content_hash>`; `rendition_url` truncates to 12
    // hex chars. `rendition_url` also appends the `s=` signature when
    // signed URLs are enabled (#425).
    let v = args.get("v").and_then(tera::Value::as_str);
    Ok(tera::Value::String(rendition_url(
        media_id,
        &op.canonical(),
        v,
    )))
}

// ---- #426 — responsive image helpers (srcset + <picture>) --------

impl FilterOp {
    /// Nominal target width for a `srcset` `w` descriptor. Height-only
    /// specs have no width to advertise → `None` (skipped from srcset).
    fn nominal_width(self) -> Option<u32> {
        match self {
            Self::Fill { w, .. } | Self::Max { w, .. } | Self::Width(w) => Some(w),
            Self::Height(_) => None,
        }
    }

    /// Multiply every pixel dimension by `factor` — the rendition for a
    /// `factor`× pixel-density (retina) variant (#427). Dimensions
    /// saturate at the 8192 cap the parser enforces.
    fn scaled(self, factor: u32) -> Self {
        let cap = |n: u32| n.saturating_mul(factor).min(8192);
        match self {
            Self::Fill { w, h } => Self::Fill {
                w: cap(w),
                h: cap(h),
            },
            Self::Max { w, h } => Self::Max {
                w: cap(w),
                h: cap(h),
            },
            Self::Width(w) => Self::Width(cap(w)),
            Self::Height(h) => Self::Height(cap(h)),
        }
    }
}

/// `media_id` arg shared by every image helper — accepts an `i64` or a
/// trimmed non-empty stringified integer (the live-preview shape).
fn parse_media_id_arg(args: &std::collections::HashMap<String, tera::Value>) -> Option<i64> {
    args.get("media_id").and_then(|v| {
        v.as_i64().or_else(|| {
            v.as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<i64>().ok())
        })
    })
}

/// Parse the `filters` arg (a Tera array of spec strings) into
/// pipelines, surfacing a clear error on a bad spec or wrong shape.
fn parse_filters_arg(
    args: &std::collections::HashMap<String, tera::Value>,
    fn_name: &str,
) -> tera::Result<Vec<FilterPipeline>> {
    let arr = args
        .get("filters")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            tera::Error::msg(format!(
                "{fn_name}: `filters` (array of spec strings) is required"
            ))
        })?;
    arr.iter()
        .map(|v| {
            let s = v.as_str().ok_or_else(|| {
                tera::Error::msg(format!("{fn_name}: each filter must be a string"))
            })?;
            FilterPipeline::parse(s).map_err(|e| tera::Error::msg(format!("{fn_name}: {e}")))
        })
        .collect()
}

/// Build a concrete rendition URL for `spec` — parsed + canonicalized,
/// signed when signed URLs are enabled (#425). Returns `None` for an
/// invalid spec. `content_hash` is the optional `v=` cache-buster.
/// Used by the JSON API (#431) to emit ready-to-use rendition URLs
/// (clients can't sign a `{filter_spec}` template themselves).
#[must_use]
pub fn rendition_url_for(media_id: i64, spec: &str, content_hash: Option<&str>) -> Option<String> {
    let pipeline = FilterPipeline::parse(spec).ok()?;
    Some(rendition_url(media_id, &pipeline.canonical(), content_hash))
}

/// Build the rendition URL `/__media__/<spec>/<id>[?v=…][&s=…]` —
/// signs it when signed URLs are enabled (#425).
fn rendition_url(media_id: i64, canonical_spec: &str, v: Option<&str>) -> String {
    let base = match v {
        Some(v) if !v.is_empty() => format!(
            "/__media__/{canonical_spec}/{media_id}?v={}",
            &v[..12.min(v.len())]
        ),
        _ => format!("/__media__/{canonical_spec}/{media_id}"),
    };
    maybe_sign(base, canonical_spec, media_id)
}

// ---- #425 — signed rendition URLs (enumeration / CPU-DoS guard) ----

/// Process-wide signing key for rendition URLs. `None` = signing off.
static SIGNING_KEY: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();

/// Enable **signed rendition URLs** (#425). Once a key is set, every
/// `rcms_image_url` / `rcms_image_srcset` / `rcms_picture` URL carries
/// an HMAC `s=` over `(canonical_spec, media_id)`, and the rendition
/// route ([`crate::rendition_route`]) rejects requests whose signature
/// is missing or forged — closing the media-id-enumeration + arbitrary-
/// spec CPU-DoS surface.
///
/// **Opt-in + backward compatible:** when no key is set (and the
/// `RCMS_RENDITION_SIGNING_KEY` env var is unset) URLs stay unsigned and
/// the route serves exactly as before.
///
/// Call once at startup, before serving. The key MUST be stable across
/// restarts — URLs are baked into cached HTML, so a per-boot random key
/// would 403 every image after a restart. Source it from your config.
///
/// The first read fixes the key, so a call after it — or a second call
/// with another key — cannot take effect; it is logged as an error rather
/// than dropped (#697), since it can leave rendition signing off.
pub fn set_signing_key(key: impl Into<Vec<u8>>) {
    let key = key.into();
    if let Err(rejected) = SIGNING_KEY.set(Some(key)) {
        if SIGNING_KEY.get() != Some(&rejected) {
            tracing::error!(
                target: "rustango_cms::rendition",
                "set_signing_key called after the key was already fixed (set earlier, or read \
                 from RCMS_RENDITION_SIGNING_KEY); this call has no effect — set it before serving"
            );
        }
    }
}

/// The active key, or `None` when signing is off. Falls back to the
/// `RCMS_RENDITION_SIGNING_KEY` env var the first time it's read.
fn signing_key() -> Option<&'static [u8]> {
    SIGNING_KEY
        .get_or_init(|| {
            crate::config::var("RENDITION_SIGNING_KEY").map(String::into_bytes)
        })
        .as_deref()
}

/// HMAC-SHA256(key, "spec:id") truncated to 16 hex chars (64-bit — a
/// short, URL-friendly MAC; ample against forgery for a DoS guard).
fn rendition_sig(key: &[u8], canonical_spec: &str, media_id: i64) -> String {
    let msg = format!("{canonical_spec}:{media_id}");
    let full = crate::signing::hmac_sha256_hex(key, msg.as_bytes());
    full[..16].to_owned()
}

/// Whether `supplied` is the correct signature for `(spec, id)` under
/// `key`. Constant-time compare so a near-miss leaks nothing.
fn sig_ok(key: &[u8], canonical_spec: &str, media_id: i64, supplied: Option<&str>) -> bool {
    let expected = rendition_sig(key, canonical_spec, media_id);
    supplied.is_some_and(|s| crate::signing::constant_time_eq(s.as_bytes(), expected.as_bytes()))
}

/// Append the `s=` signature when signing is enabled; pass the URL
/// through untouched otherwise.
fn maybe_sign(url: String, canonical_spec: &str, media_id: i64) -> String {
    match signing_key() {
        Some(key) => {
            let sig = rendition_sig(key, canonical_spec, media_id);
            let sep = if url.contains('?') { '&' } else { '?' };
            format!("{url}{sep}s={sig}")
        }
        None => url,
    }
}

/// Verify a rendition request's `s=` signature. Returns `true` when
/// signing is disabled (open contract) OR `sig` matches `(spec, id)`.
/// The route calls this before the DB lookup + rendition work so a
/// forged request costs nothing.
#[must_use]
pub fn verify_rendition_sig(canonical_spec: &str, media_id: i64, sig: Option<&str>) -> bool {
    match signing_key() {
        None => true,
        Some(key) => sig_ok(key, canonical_spec, media_id, sig),
    }
}

/// `srcset` value from parsed specs: `"<url> <w>w, …"`. Height-only
/// specs (no nominal width) are skipped.
fn build_srcset(media_id: i64, specs: &[FilterPipeline], v: Option<&str>) -> String {
    specs
        .iter()
        .filter_map(|sp| {
            sp.geometric
                .nominal_width()
                .map(|w| format!("{} {w}w", rendition_url(media_id, &sp.canonical(), v)))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `srcset` with pixel-density (`Nx`) descriptors (#427 retina/DPI): the
/// `base` spec rendered at each density — its dimensions multiplied by
/// the density — e.g. `width-400` → `"…/width-400/42 1x, …/width-800/42 2x"`.
/// Densities are de-duplicated + sorted ascending; non-positive dropped.
fn build_dpi_srcset(
    media_id: i64,
    base: &FilterPipeline,
    densities: &[u32],
    v: Option<&str>,
) -> String {
    let mut ds: Vec<u32> = densities.iter().copied().filter(|d| *d > 0).collect();
    ds.sort_unstable();
    ds.dedup();
    ds.iter()
        .map(|d| {
            let spec = FilterPipeline {
                geometric: base.geometric.scaled(*d),
                encoding: base.encoding,
            };
            format!("{} {d}x", rendition_url(media_id, &spec.canonical(), v))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn attr_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `<source type=…>` MIME for a format token (the `<picture>` source
/// selector). Unknown tokens are skipped by the caller.
fn format_mime(fmt: &str) -> Option<&'static str> {
    Some(match fmt {
        "avif" => "image/avif",
        "webp" | "auto" => "image/webp",
        "jpeg" | "jpg" => "image/jpeg",
        "png" => "image/png",
        _ => return None,
    })
}

/// Build a `<picture>`: one `<source>` per modern format (a
/// `|format-X` variant of `specs`) + an `<img>` fallback (the base
/// specs, widest as `src`). `alt`/`sizes` are HTML-attribute-escaped.
fn build_picture(
    media_id: i64,
    specs: &[FilterPipeline],
    formats: &[String],
    alt: &str,
    sizes: Option<&str>,
    v: Option<&str>,
) -> String {
    let sizes_attr = sizes
        .filter(|s| !s.is_empty())
        .map(|s| format!(" sizes=\"{}\"", attr_escape(s)))
        .unwrap_or_default();
    let mut out = String::from("<picture>");
    for fmt in formats {
        let (Some(mime), Some(parsed)) = (format_mime(fmt), EncodeFormat::parse(fmt)) else {
            continue;
        };
        let variant: Vec<FilterPipeline> = specs
            .iter()
            .map(|sp| {
                let mut s = *sp;
                s.encoding.format = Some(parsed);
                s
            })
            .collect();
        let srcset = build_srcset(media_id, &variant, v);
        if !srcset.is_empty() {
            out.push_str(&format!(
                "<source type=\"{mime}\" srcset=\"{srcset}\"{sizes_attr}>"
            ));
        }
    }
    let base_srcset = build_srcset(media_id, specs, v);
    let fallback = specs
        .iter()
        .max_by_key(|sp| sp.geometric.nominal_width().unwrap_or(0))
        .map(|sp| rendition_url(media_id, &sp.canonical(), v))
        .unwrap_or_default();
    let srcset_attr = if base_srcset.is_empty() {
        String::new()
    } else {
        format!(" srcset=\"{base_srcset}\"")
    };
    out.push_str(&format!(
        "<img src=\"{fallback}\"{srcset_attr}{sizes_attr} alt=\"{}\" loading=\"lazy\" decoding=\"async\"></picture>",
        attr_escape(alt),
    ));
    out
}

/// `rcms_image_srcset(media_id, filters=[…], [v])` → the `srcset`
/// value (no markup). `is_safe` — output is CMS-controlled URLs.
struct RcmsImageSrcsetFn;
impl tera::Function for RcmsImageSrcsetFn {
    fn call(
        &self,
        args: &std::collections::HashMap<String, tera::Value>,
    ) -> tera::Result<tera::Value> {
        let media_id = parse_media_id_arg(args)
            .ok_or_else(|| tera::Error::msg("rcms_image_srcset: `media_id` (i64) is required"))?;
        let specs = parse_filters_arg(args, "rcms_image_srcset")?;
        let v = args.get("v").and_then(tera::Value::as_str);
        Ok(tera::Value::String(build_srcset(media_id, &specs, v)))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

/// `rcms_image_srcset_dpi(media_id, filter="width-400", [densities=[1,2]],
/// [v])` → a pixel-density `srcset` (`Nx` descriptors) of one base spec
/// at each density (#427). `is_safe` — CMS-controlled URLs.
struct RcmsImageSrcsetDpiFn;
impl tera::Function for RcmsImageSrcsetDpiFn {
    fn call(
        &self,
        args: &std::collections::HashMap<String, tera::Value>,
    ) -> tera::Result<tera::Value> {
        let media_id = parse_media_id_arg(args).ok_or_else(|| {
            tera::Error::msg("rcms_image_srcset_dpi: `media_id` (i64) is required")
        })?;
        let filter = args
            .get("filter")
            .and_then(tera::Value::as_str)
            .ok_or_else(|| {
                tera::Error::msg("rcms_image_srcset_dpi: `filter` (string) is required")
            })?;
        let base = FilterPipeline::parse(filter)
            .map_err(|e| tera::Error::msg(format!("rcms_image_srcset_dpi: {e}")))?;
        let densities: Vec<u32> = args
            .get("densities")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_u64().map(|n| n as u32))
                    .collect()
            })
            .unwrap_or_else(|| vec![1, 2]);
        let v = args.get("v").and_then(tera::Value::as_str);
        Ok(tera::Value::String(build_dpi_srcset(
            media_id, &base, &densities, v,
        )))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

/// `rcms_picture(media_id, filters=[…], [formats=["avif","webp"]],
/// [alt], [sizes], [v])` → a full `<picture>`. `is_safe` — URLs are
/// CMS-controlled and `alt`/`sizes` are escaped internally.
struct RcmsPictureFn;
impl tera::Function for RcmsPictureFn {
    fn call(
        &self,
        args: &std::collections::HashMap<String, tera::Value>,
    ) -> tera::Result<tera::Value> {
        let media_id = parse_media_id_arg(args)
            .ok_or_else(|| tera::Error::msg("rcms_picture: `media_id` (i64) is required"))?;
        let specs = parse_filters_arg(args, "rcms_picture")?;
        let formats: Vec<String> = args
            .get("formats")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_else(|| vec!["avif".to_owned(), "webp".to_owned()]);
        let alt = args.get("alt").and_then(tera::Value::as_str).unwrap_or("");
        let sizes = args.get("sizes").and_then(tera::Value::as_str);
        let v = args.get("v").and_then(tera::Value::as_str);
        Ok(tera::Value::String(build_picture(
            media_id, &specs, &formats, alt, sizes, v,
        )))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #694 — a large decode + re-encode must not freeze the runtime.
    #[rustango::__private_runtime::tokio::test(flavor = "current_thread")]
    async fn image_work_runs_off_the_runtime() {
        use rustango::__private_runtime::tokio;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let img = image::RgbImage::from_fn(1200, 900, |x, y| image::Rgb([(x % 251) as u8, (y % 241) as u8, 90]));
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .expect("encode");

        let ticks = std::sync::Arc::new(AtomicUsize::new(0));
        let t = std::sync::Arc::clone(&ticks);
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                t.fetch_add(1, Ordering::Relaxed);
            }
        });
        let (stored, dims) = strip_exif_off_runtime(jpeg, "image/jpeg".into()).await;
        let after_strip = ticks.load(Ordering::Relaxed);
        let (cropped, _) = crop_image_off_runtime(stored, "image/jpeg".into(), (0, 0, 600, 450))
            .await
            .expect("crop");
        let after_crop = ticks.load(Ordering::Relaxed);
        ticker.abort();
        assert_eq!(dims, Some((1200, 900)));
        assert!(!cropped.is_empty());
        assert!(after_strip >= 2, "strip stalled the runtime ({after_strip} ticks)");
        assert!(after_crop >= after_strip + 2, "crop stalled the runtime");
    }

    #[test]
    fn bgcolor_with_non_ascii_bytes_is_an_error_not_a_panic() {
        assert!(FilterPipeline::parse("width-10|bgcolor-a\u{e9}\u{20ac}").is_err());
        assert!(FilterPipeline::parse("width-10|bgcolor-a\u{e9}").is_err());
        assert!(FilterPipeline::parse("width-10|bgcolor-fff").is_ok());
    }

    #[test]
    fn parses_fill() {
        assert_eq!(
            FilterOp::parse("fill-300x200").unwrap(),
            FilterOp::Fill { w: 300, h: 200 }
        );
    }

    #[test]
    fn parses_max() {
        assert_eq!(
            FilterOp::parse("max-1200x900").unwrap(),
            FilterOp::Max { w: 1200, h: 900 }
        );
    }

    #[test]
    fn parses_width() {
        assert_eq!(FilterOp::parse("width-800").unwrap(), FilterOp::Width(800));
    }

    #[test]
    fn parses_height() {
        assert_eq!(
            FilterOp::parse("height-600").unwrap(),
            FilterOp::Height(600)
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(FilterOp::parse("nope").is_err());
        assert!(FilterOp::parse("fill-30000x30000").is_err()); // > 8192
        assert!(FilterOp::parse("fill-0x0").is_err());
        assert!(FilterOp::parse("fill-300").is_err());
    }

    #[test]
    fn canonical_roundtrips() {
        for raw in ["fill-300x200", "max-1200x900", "width-800", "height-600"] {
            let parsed = FilterOp::parse(raw).unwrap();
            assert_eq!(parsed.canonical(), raw);
        }
    }

    // --- #253 — rcms_image_url Tera function registration + arg shape

    fn fresh_tera() -> tera::Tera {
        let mut tera = tera::Tera::default();
        register_tera_function(&mut tera);
        tera
    }

    fn render_one(src: &str, ctx_values: &[(&str, tera::Value)]) -> tera::Result<String> {
        let mut tera = fresh_tera();
        tera.add_raw_template("t.html", src)?;
        let mut ctx = tera::Context::new();
        for (k, v) in ctx_values {
            ctx.insert(*k, v);
        }
        tera.render("t.html", &ctx)
    }

    #[test]
    fn image_url_accepts_int_id() {
        let out = render_one(
            r#"{{ rcms_image_url(media_id=42, filter="width-1600") }}"#,
            &[],
        )
        .unwrap();
        assert_eq!(out, "/__media__/width-1600/42");
    }

    #[test]
    fn image_url_accepts_stringified_id() {
        // Stream-block storage shape — the block JSON carries
        // `media_id` as a string after live-preview / form posts.
        let out = render_one(
            r#"{{ rcms_image_url(media_id=v, filter="width-1600") }}"#,
            &[("v", tera::Value::String("99".to_owned()))],
        )
        .unwrap();
        assert_eq!(out, "/__media__/width-1600/99");
    }

    #[test]
    fn image_url_trims_whitespace_around_string_id() {
        let out = render_one(
            r#"{{ rcms_image_url(media_id=v, filter="max-1200x900") }}"#,
            &[("v", tera::Value::String("  17  ".to_owned()))],
        )
        .unwrap();
        assert_eq!(out, "/__media__/max-1200x900/17");
    }

    #[test]
    fn image_url_empty_string_id_errors() {
        let err = render_one(
            r#"{{ rcms_image_url(media_id=v, filter="width-1600") }}"#,
            &[("v", tera::Value::String("".to_owned()))],
        )
        .unwrap_err();
        assert!(
            format!("{err:?}").contains("media_id"),
            "expected media_id error, got `{err:?}`"
        );
    }

    #[test]
    fn image_url_missing_filter_errors() {
        let err = render_one(r#"{{ rcms_image_url(media_id=42) }}"#, &[]).unwrap_err();
        assert!(
            format!("{err:?}").contains("filter"),
            "expected filter error, got `{err:?}`"
        );
    }

    #[test]
    fn image_url_with_content_hash_buster() {
        let out = render_one(
            r#"{{ rcms_image_url(media_id=42, filter="width-800", v="abc123def456hash") }}"#,
            &[],
        )
        .unwrap();
        assert_eq!(out, "/__media__/width-800/42?v=abc123def456");
    }

    // --- #79 — encoding pipeline -------------------------------

    #[test]
    fn pipeline_parses_bare_resize() {
        let p = FilterPipeline::parse("fill-300x200").unwrap();
        assert_eq!(p.geometric, FilterOp::Fill { w: 300, h: 200 });
        assert_eq!(p.encoding, EncodingOps::default());
    }

    #[test]
    fn pipeline_parses_chained_encoding() {
        let p = FilterPipeline::parse("fill-300x200|format-webp|quality-75|bgcolor-fff").unwrap();
        assert_eq!(p.geometric, FilterOp::Fill { w: 300, h: 200 });
        assert_eq!(p.encoding.format, Some(EncodeFormat::Webp));
        assert_eq!(p.encoding.quality, Some(75));
        assert_eq!(p.encoding.background, Some((255, 255, 255)));
    }

    #[test]
    fn pipeline_canonical_roundtrips() {
        // #398 — AVIF specs now parse (encoding is gated behind the
        // `avif` feature, but the URL spec is always recognized).
        let raw = "max-1200x900|format-avif|quality-80";
        let parsed = FilterPipeline::parse(raw).unwrap();
        assert_eq!(parsed.encoding.format, Some(EncodeFormat::Avif));
        assert_eq!(parsed.canonical(), raw);
        let raw = "fill-300x200|format-webp|quality-75|bgcolor-ffffff";
        let parsed = FilterPipeline::parse(raw).unwrap();
        assert_eq!(parsed.canonical(), raw);
    }

    #[test]
    #[cfg(not(feature = "avif"))]
    fn avif_encode_errors_clearly_without_feature() {
        let img = image::DynamicImage::new_rgb8(2, 2);
        let ops = EncodingOps {
            format: Some(EncodeFormat::Avif),
            quality: None,
            background: None,
        };
        let err = encode(&img, &ops, "image/png").unwrap_err();
        assert!(err.contains("avif"), "got: {err}");
    }

    #[test]
    #[cfg(feature = "avif")]
    fn avif_encode_produces_avif_bytes() {
        let img = image::DynamicImage::new_rgb8(8, 8);
        let ops = EncodingOps {
            format: Some(EncodeFormat::Avif),
            quality: Some(50),
            background: None,
        };
        let (bytes, mime, ext) = encode(&img, &ops, "image/png").unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(mime, "image/avif");
        assert_eq!(ext, "avif");
    }

    #[test]
    fn pipeline_rejects_garbage_segments() {
        assert!(FilterPipeline::parse("fill-300x200|format-jpegxl").is_err());
        assert!(FilterPipeline::parse("fill-300x200|quality-0").is_err());
        assert!(FilterPipeline::parse("fill-300x200|quality-101").is_err());
        assert!(FilterPipeline::parse("fill-300x200|bgcolor-xxxx").is_err());
        assert!(FilterPipeline::parse("fill-300x200|nonsense").is_err());
    }

    #[test]
    fn parse_hex_rgb_three_and_six_digit() {
        assert_eq!(super::parse_hex_rgb("fff"), Some((255, 255, 255)));
        assert_eq!(super::parse_hex_rgb("ff0000"), Some((255, 0, 0)));
        assert_eq!(super::parse_hex_rgb("#abcdef"), Some((171, 205, 239)));
        assert_eq!(super::parse_hex_rgb("ff"), None);
        assert_eq!(super::parse_hex_rgb("ffffff00"), None); // RGBA not supported
    }
}

#[cfg(test)]
mod responsive_tests {
    use super::{build_dpi_srcset, build_picture, build_srcset, FilterPipeline};

    fn specs(raw: &[&str]) -> Vec<FilterPipeline> {
        raw.iter()
            .map(|s| FilterPipeline::parse(s).unwrap())
            .collect()
    }

    #[test]
    fn srcset_emits_url_and_width_descriptor() {
        let out = build_srcset(42, &specs(&["fill-400x300", "fill-800x600"]), None);
        assert_eq!(
            out,
            "/__media__/fill-400x300/42 400w, /__media__/fill-800x600/42 800w"
        );
    }

    #[test]
    fn srcset_skips_height_only_specs() {
        // `height-200` has no nominal width → dropped; `width-600` stays.
        let out = build_srcset(7, &specs(&["height-200", "width-600"]), None);
        assert_eq!(out, "/__media__/width-600/7 600w");
    }

    #[test]
    fn srcset_appends_cache_buster_truncated() {
        let out = build_srcset(1, &specs(&["width-300"]), Some("0123456789abcdef"));
        assert_eq!(out, "/__media__/width-300/1?v=0123456789ab 300w");
    }

    #[test]
    fn dpi_srcset_scales_width_per_density() {
        let base = FilterPipeline::parse("width-400").unwrap();
        assert_eq!(
            build_dpi_srcset(42, &base, &[1, 2], None),
            "/__media__/width-400/42 1x, /__media__/width-800/42 2x"
        );
    }

    #[test]
    fn dpi_srcset_scales_both_fill_dims_and_dedups_densities() {
        let base = FilterPipeline::parse("fill-200x100").unwrap();
        // Densities deduped + sorted + 0 dropped.
        let out = build_dpi_srcset(7, &base, &[2, 1, 2, 0], None);
        assert_eq!(
            out,
            "/__media__/fill-200x100/7 1x, /__media__/fill-400x200/7 2x"
        );
    }

    #[test]
    fn dpi_srcset_caps_at_8192() {
        let base = FilterPipeline::parse("width-5000").unwrap();
        // 5000×2 = 10000 → capped to 8192.
        assert_eq!(
            build_dpi_srcset(1, &base, &[2], None),
            "/__media__/width-8192/1 2x"
        );
    }

    #[test]
    fn rendition_url_for_parses_and_rejects_bad_spec() {
        // #431 — the public API builder: canonicalizes + (with no
        // signing key in tests) emits the plain URL; bad spec → None.
        assert_eq!(
            super::rendition_url_for(42, "width-800", Some("abc123def456ZZ")),
            Some("/__media__/width-800/42?v=abc123def456".to_owned())
        );
        assert_eq!(super::rendition_url_for(42, "not-a-spec", None), None);
    }

    #[test]
    fn picture_has_source_per_format_and_img_fallback() {
        let html = build_picture(
            42,
            &specs(&["width-400", "width-800"]),
            &["avif".to_owned(), "webp".to_owned()],
            "A <photo> & more",
            Some("100vw"),
            None,
        );
        // One <source> per format, with the format folded into the spec.
        assert!(html.contains(r#"<source type="image/avif" srcset="/__media__/width-400|format-avif/42 400w, /__media__/width-800|format-avif/42 800w" sizes="100vw">"#), "{html}");
        assert!(html.contains(r#"<source type="image/webp" srcset="/__media__/width-400|format-webp/42 400w, /__media__/width-800|format-webp/42 800w" sizes="100vw">"#), "{html}");
        // Fallback <img>: widest as src + base srcset, alt escaped.
        assert!(
            html.contains(r#"<img src="/__media__/width-800/42""#),
            "{html}"
        );
        assert!(
            html.contains(r#"alt="A &lt;photo&gt; &amp; more""#),
            "{html}"
        );
        assert!(
            html.starts_with("<picture>") && html.ends_with("</picture>"),
            "{html}"
        );
    }

    #[test]
    fn picture_skips_unknown_format_token() {
        let html = build_picture(
            5,
            &specs(&["width-500"]),
            &["bogus".to_owned(), "webp".to_owned()],
            "",
            None,
            None,
        );
        assert!(!html.contains("bogus"), "{html}");
        assert!(html.contains(r#"type="image/webp""#), "{html}");
    }

    #[test]
    fn tera_functions_render_end_to_end() {
        // Exercises the `filters=[…]` array-arg parsing + registration
        // + the `is_safe` (no slash-escaping) contract.
        let mut tera = tera::Tera::default();
        super::register_tera_function(&mut tera);
        tera.add_raw_template(
            "s.html",
            r#"{{ rcms_image_srcset(media_id=42, filters=["width-400", "width-800"]) }}"#,
        )
        .unwrap();
        assert_eq!(
            tera.render("s.html", &tera::Context::new()).unwrap(),
            "/__media__/width-400/42 400w, /__media__/width-800/42 800w"
        );
        tera.add_raw_template(
            "p.html",
            r#"{{ rcms_picture(media_id=7, filters=["width-600"], formats=["webp"], alt="hi") }}"#,
        )
        .unwrap();
        let p = tera.render("p.html", &tera::Context::new()).unwrap();
        assert!(p.contains(r#"type="image/webp""#), "{p}");
        assert!(p.contains(r#"alt="hi""#), "{p}");
        assert!(p.starts_with("<picture>"), "{p}");
    }
}

#[cfg(test)]
mod sign_tests {
    use super::{maybe_sign, rendition_sig, sig_ok, verify_rendition_sig};

    #[test]
    fn sig_is_deterministic_16_hex_and_input_sensitive() {
        let k = b"secret";
        let a = rendition_sig(k, "fill-400x300", 42);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_eq!(a, rendition_sig(k, "fill-400x300", 42)); // stable
        assert_ne!(a, rendition_sig(k, "fill-400x300", 43)); // media id
        assert_ne!(a, rendition_sig(k, "fill-401x300", 42)); // spec
        assert_ne!(a, rendition_sig(b"other-key", "fill-400x300", 42)); // key
    }

    #[test]
    fn sig_ok_accepts_correct_rejects_forged_or_missing() {
        let k = b"secret";
        let good = rendition_sig(k, "width-800", 7);
        assert!(sig_ok(k, "width-800", 7, Some(&good)));
        assert!(!sig_ok(k, "width-800", 7, Some("deadbeefdeadbeef"))); // forged
        assert!(!sig_ok(k, "width-800", 7, None)); // missing
        assert!(!sig_ok(k, "width-800", 8, Some(&good))); // right sig, wrong id
    }

    #[test]
    fn disabled_signing_is_open_and_unsigned() {
        // No key configured (and RCMS_RENDITION_SIGNING_KEY unset in the
        // test env) → signing off: URLs pass through untouched and every
        // request verifies. This is the default, backward-compatible path.
        assert_eq!(
            maybe_sign("/__media__/width-800/7".to_owned(), "width-800", 7),
            "/__media__/width-800/7"
        );
        assert!(verify_rendition_sig("width-800", 7, None));
    }
}
