//! Pluggable storage backend for CMS media.
//!
//! Media originals, renditions and staged (scratch) uploads used to be
//! written and read with `std::fs` against a hardcoded `./var/media`
//! root, in ~13 places. That bound the CMS to a single local disk:
//! there was no way to put media on S3/R2/MinIO, no way to share media
//! between replicas, and every read/write did blocking I/O on an async
//! worker thread.
//!
//! Everything now goes through the framework's [`Storage`](rustango::storage::Storage) trait, so a
//! host app can point media at any backend:
//!
//! ```ignore
//! use rustango::storage::{registry::StorageRegistry, s3::{S3Storage, S3Config}, BoxedStorage};
//! use std::sync::Arc;
//!
//! let s3: BoxedStorage = Arc::new(S3Storage::new(S3Config {
//!     bucket: "media".into(),
//!     region: "us-west-2".into(),
//!     endpoint: Some("https://<ref>.storage.supabase.co/storage/v1/s3".into()),
//!     access_key_id: std::env::var("S3_ACCESS_KEY_ID").unwrap(),
//!     secret_access_key: std::env::var("S3_SECRET_ACCESS_KEY").unwrap(),
//!     path_style: true,
//! }));
//! rustango_cms::media_storage::install(
//!     StorageRegistry::new().set("media", s3).with_default("media"),
//! );
//! ```
//!
//! ## Backwards compatibility
//!
//! Install nothing and the CMS falls back to
//! `LocalStorage::new("./var/media")`. Because [`key`] is
//! `"{tenant_slug}/{storage_key}"` and `LocalStorage` resolves a key as
//! `root.join(key)`, that reproduces the previous on-disk layout
//! **byte-for-byte** — existing installs keep working with no data
//! migration and no config change.
//!
//! ## Keys
//!
//! One flat scheme for every object, matching what the DB already
//! stores in `cms_media.storage_key` / `cms_media_rendition.storage_key`:
//!
//! ```text
//! <tenant_slug>/<storage_key>
//! <tenant_slug>/renditions/<hash>-<spec>.<ext>
//! <tenant_slug>/scratch/<hash>-<filename>
//! ```
//!
//! Keys are tenant-prefixed, so one bucket safely serves every tenant.

use std::path::PathBuf;
use std::sync::OnceLock;

use rustango::storage::registry::StorageRegistry;
use rustango::storage::{BoxedStorage, LocalStorage};

/// Default root for the fallback [`LocalStorage`] — the historical
/// hardcoded media directory.
pub const DEFAULT_MEDIA_ROOT: &str = "./var/media";

/// Registry disk name used by [`install_from_env`]. Exposed so a host
/// building its own [`StorageRegistry`] can attach a CDN to the same
/// disk (`registry.cdn(media_storage::DISK, base)`).
pub const DISK: &str = "media";

static REGISTRY: OnceLock<StorageRegistry> = OnceLock::new();
static FALLBACK: OnceLock<BoxedStorage> = OnceLock::new();

/// Install the storage registry the CMS uses for media. Call once at
/// boot, before serving. The registry's **default disk** is what media
/// reads/writes resolve to.
///
/// Returns `false` (and keeps the first registry) if called more than
/// once — installing twice is a host-wiring bug, not a runtime
/// condition worth panicking over.
pub fn install(registry: StorageRegistry) -> bool {
    REGISTRY.set(registry).is_ok()
}

/// Registry name of `tenant_slug`'s own disk: `tenant:<slug>`.
///
/// Tenant disks live under this prefix so a slug can never select one of
/// the host's other disks — a tenant provisioned with the slug `private`
/// used to have its media read and written through a host disk named
/// `private`.
#[must_use]
pub fn tenant_disk_name(tenant_slug: &str) -> String {
    format!("tenant:{tenant_slug}")
}

/// The disk one tenant's media goes through.
///
/// A disk registered as [`tenant_disk_name`] wins — that is how a tenant
/// gets its own bucket. Otherwise the registry default is used,
/// which is the shared-bucket arrangement where tenants are separated
/// by the `{slug}/` key prefix.
///
/// Bucket-per-tenant is the stronger isolation: a shared bucket has one
/// public/private flag for everybody, and a single key-construction bug
/// crosses tenants. Note the `{slug}/` prefix is applied either way, so
/// a dedicated bucket can still hold non-media data under other
/// prefixes without collision, and moving a tenant between the two
/// arrangements is a copy rather than a rename.
#[must_use]
pub fn disk_for(tenant_slug: &str) -> BoxedStorage {
    if let Some(reg) = REGISTRY.get() {
        if let Some(d) = reg.disk(&tenant_disk_name(tenant_slug)) {
            return d;
        }
        if let Some(d) = reg.default_disk() {
            return d;
        }
    }
    local_fallback()
}

/// Name of the registry disk serving `tenant_slug` — its own if one is
/// registered, else the default. `None` when no registry is installed.
fn disk_name_for(reg: &StorageRegistry, tenant_slug: &str) -> Option<String> {
    let own = tenant_disk_name(tenant_slug);
    if reg.has(&own) {
        return Some(own);
    }
    reg.default_name().map(str::to_owned)
}

fn local_fallback() -> BoxedStorage {
    FALLBACK
        .get_or_init(|| std::sync::Arc::new(LocalStorage::new(PathBuf::from(DEFAULT_MEDIA_ROOT))))
        .clone()
}

/// The registry's default disk, ignoring per-tenant overrides. Prefer
/// [`disk_for`] anywhere a tenant is in scope.
#[must_use]
pub fn disk() -> BoxedStorage {
    if let Some(reg) = REGISTRY.get() {
        if let Some(d) = reg.default_disk() {
            return d;
        }
    }
    local_fallback()
}

/// Storage key for one tenant-scoped object.
///
/// `storage_key` is the value already persisted on the media /
/// rendition row (e.g. `"ab12…-photo.jpg"`, `"renditions/ab12…-w800.webp"`,
/// `"scratch/ab12…-upload.png"`).
#[must_use]
pub fn key(tenant_slug: &str, storage_key: &str) -> String {
    format!("{tenant_slug}/{storage_key}")
}

/// A public URL for a stored object — the CDN base when one is
/// configured, otherwise whatever the backend can produce (an S3 object
/// URL, `LocalStorage::with_base_url`). `None` means the caller should
/// keep serving the bytes through the CMS's own `/__media__/…` route.
///
/// **Do not use this for access-controlled media.** A CDN URL is served
/// by the edge without ever reaching the CMS, so `PageViewRestriction`
/// and collection permissions are not enforced on it. Use
/// [`origin_url`] — or keep proxying through `/__media__/…` — for
/// anything gated.
#[must_use]
pub fn public_url(tenant_slug: &str, storage_key: &str) -> Option<String> {
    let k = key(tenant_slug, storage_key);
    if let Some(reg) = REGISTRY.get() {
        if let Some(name) = disk_name_for(reg, tenant_slug) {
            // Falls back to the backend's own `url(key)` when no CDN
            // base is registered for that disk.
            return reg.cdn_url(&name, &k);
        }
    }
    disk_for(tenant_slug).url(&k)
}

/// The backend's own URL for a stored object, bypassing any configured
/// CDN. Use when the edge must stay out of the loop — gated media,
/// admin-only assets, anything whose freshness matters more than
/// latency.
#[must_use]
pub fn origin_url(tenant_slug: &str, storage_key: &str) -> Option<String> {
    let k = key(tenant_slug, storage_key);
    if let Some(reg) = REGISTRY.get() {
        if let Some(name) = disk_name_for(reg, tenant_slug) {
            return reg.origin_url(&name, &k);
        }
    }
    disk_for(tenant_slug).url(&k)
}

/// Install a media backend from the environment — the wiring every
/// host app needs, in one place.
///
/// `CMS_MEDIA_BACKEND` selects it:
///
/// | value | backend |
/// |---|---|
/// | unset / `local` | `./var/media` on local disk — the original layout, byte-identical |
/// | `memory` | [`InMemoryStorage`](rustango::storage::InMemoryStorage) — uploads and renditions round-trip with the filesystem never touched |
/// | `s3` | any S3-compatible service (AWS, R2, MinIO, Supabase Storage); needs the `storage_s3` feature |
///
/// `s3` reads `CMS_S3_BUCKET`, `CMS_S3_REGION`, `CMS_S3_ACCESS_KEY_ID`,
/// `CMS_S3_SECRET_ACCESS_KEY`, plus optional `CMS_S3_ENDPOINT` (unset =
/// AWS) and `CMS_S3_PATH_STYLE`. Path-style defaults to **on**, which is
/// what Supabase / MinIO / R2 want; set `false` for AWS virtual-hosted.
///
/// Returns `Err` only for a misconfigured environment (unknown backend,
/// missing S3 credentials) — call it at boot and let it fail loudly
/// rather than silently writing media to the wrong place.
pub fn install_from_env() -> Result<(), String> {
    let backend = crate::config::var("MEDIA_BACKEND").unwrap_or_default();
    let disk: BoxedStorage = match backend.as_str() {
        "" | "local" => return Ok(()),
        "memory" => {
            tracing::warn!("media storage backend = in-memory (nothing hits disk)");
            std::sync::Arc::new(rustango::storage::InMemoryStorage::default())
        }
        #[cfg(feature = "storage_s3")]
        "s3" => {
            use rustango::storage::s3::{S3Config, S3Storage};
            let req = |k: &str| {
                crate::config::var(k).ok_or_else(|| format!("RCMS_MEDIA_BACKEND=s3 requires RCMS_{k}"))
            };
            let cfg = S3Config {
                bucket: req("S3_BUCKET")?,
                region: req("S3_REGION")?,
                endpoint: crate::config::var("S3_ENDPOINT"),
                access_key_id: req("S3_ACCESS_KEY_ID")?,
                secret_access_key: req("S3_SECRET_ACCESS_KEY")?,
                path_style: crate::config::var("S3_PATH_STYLE").as_deref() != Some("false"),
            };
            tracing::info!(
                bucket = %cfg.bucket, region = %cfg.region, endpoint = ?cfg.endpoint,
                "media storage backend = s3",
            );
            std::sync::Arc::new(S3Storage::new(cfg))
        }
        other => {
            return Err(format!(
                "unknown CMS_MEDIA_BACKEND={other:?} (expected local | memory | s3; \
                 `s3` also needs the rustango-cms `storage_s3` feature)",
            ))
        }
    };

    let mut registry = StorageRegistry::new().set(DISK, disk).with_default(DISK);

    // `CMS_MEDIA_CDN_BASE` — serve media straight from the edge instead
    // of proxying every byte through the CMS. For Supabase Storage that
    // is the public-object base of a **public** bucket:
    //
    //   https://<ref>.supabase.co/storage/v1/object/public/<bucket>
    //
    // The S3 endpoint itself is not a CDN origin (it answers
    // `cf-cache-status: DYNAMIC`), so pointing this at the S3 URL buys
    // nothing. Note a public bucket is world-readable: see
    // [`public_url`] before using this for gated media.
    if let Some(base) = crate::config::var("MEDIA_CDN_BASE") {
        let base = base.trim_end_matches('/').to_owned();
        if !base.is_empty() {
            tracing::info!(cdn = %base, disk = DISK, "media CDN base configured");
            registry = registry.cdn(DISK, base);
        }
    }

    // `CMS_MEDIA_TENANT_BUCKETS` — bucket-per-tenant, the stronger
    // isolation. `slug=bucket,slug=bucket`; each entry becomes a disk
    // registered as `tenant:<slug>`, sharing the S3 credentials above (N
    // buckets in one account need one key pair — only cross-account
    // separation would need more). Tenants not listed keep using the
    // default disk. Buckets must already exist: neither the `Storage`
    // trait nor `S3Storage` can create one.
    //
    // `CMS_MEDIA_TENANT_CDNS` gives those tenants their own CDN base,
    // same `slug=value` shape — a white-label domain per tenant.
    #[cfg(feature = "storage_s3")]
    if backend == "s3" {
        use rustango::storage::s3::{S3Config, S3Storage};
        let cdns = parse_pairs(&crate::config::var("MEDIA_TENANT_CDNS").unwrap_or_default());
        for (slug, bucket) in
            parse_pairs(&crate::config::var("MEDIA_TENANT_BUCKETS").unwrap_or_default())
        {
            let cfg = S3Config {
                bucket: bucket.clone(),
                region: crate::config::var("S3_REGION").unwrap_or_default(),
                endpoint: crate::config::var("S3_ENDPOINT"),
                access_key_id: crate::config::var("S3_ACCESS_KEY_ID").unwrap_or_default(),
                secret_access_key: crate::config::var("S3_SECRET_ACCESS_KEY").unwrap_or_default(),
                path_style: crate::config::var("S3_PATH_STYLE").as_deref() != Some("false"),
            };
            let tenant_disk: BoxedStorage = std::sync::Arc::new(S3Storage::new(cfg));
            registry = registry.set(tenant_disk_name(&slug), tenant_disk);
            if let Some(base) = cdns.iter().find(|(s, _)| *s == slug).map(|(_, b)| b) {
                registry = registry.cdn(tenant_disk_name(&slug), base.trim_end_matches('/').to_owned());
            }
            tracing::info!(tenant = %slug, %bucket, "media: dedicated bucket");
        }
    }

    // Promised to fail loudly: a registry installed earlier would keep
    // serving while the one built from the env was thrown away (#697).
    if install(registry) {
        Ok(())
    } else {
        Err("a media storage registry was already installed; CMS_MEDIA_BACKEND was not applied".to_owned())
    }
}

#[cfg(feature = "storage_s3")]
/// Parse a `key=value,key=value` env list. Blank entries and entries
/// without `=` are skipped rather than failing the boot — a stray comma
/// shouldn't take the site down.
fn parse_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            let (k, v) = (k.trim(), v.trim());
            (!k.is_empty() && !v.is_empty()).then(|| (k.to_owned(), v.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_tenant_prefixed() {
        assert_eq!(key("acme", "ab12-photo.jpg"), "acme/ab12-photo.jpg");
        assert_eq!(
            key("acme", "renditions/ab12-w800.webp"),
            "acme/renditions/ab12-w800.webp"
        );
    }

    #[test]
    fn key_matches_the_legacy_on_disk_layout() {
        // The fallback LocalStorage resolves `root.join(key)`, so this
        // key must reproduce `./var/media/<slug>/<storage_key>` — the
        // path the pre-Slice-8 code built by hand.
        let k = key("acme", "scratch/ab12-upload.png");
        let resolved = PathBuf::from(DEFAULT_MEDIA_ROOT).join(&k);
        assert_eq!(
            resolved,
            PathBuf::from("./var/media/acme/scratch/ab12-upload.png")
        );
    }

    #[test]
    fn cdn_url_wins_over_the_backend_url_and_origin_bypasses_it() {
        // Registry-level behaviour, exercised directly: `install` is a
        // process-wide OnceLock, so a unit test can't install its own
        // without fighting the other tests in this binary.
        use rustango::storage::InMemoryStorage;
        let mem: BoxedStorage = std::sync::Arc::new(InMemoryStorage::default());
        let reg = StorageRegistry::new()
            .set(DISK, mem)
            .with_default(DISK)
            .cdn(DISK, "https://cdn.example.com/media");

        let k = key("acme", "renditions/ab12-w800.webp");
        assert_eq!(
            reg.cdn_url(DISK, &k).as_deref(),
            Some("https://cdn.example.com/media/acme/renditions/ab12-w800.webp"),
        );
        // The edge must be skippable for gated media — InMemoryStorage
        // exposes no URL of its own, so bypassing the CDN yields None.
        assert_eq!(reg.origin_url(DISK, &k), None);
    }

    #[test]
    fn a_tenant_disk_wins_over_the_default_and_keeps_the_slug_prefix() {
        use rustango::storage::InMemoryStorage;
        let shared: BoxedStorage = std::sync::Arc::new(InMemoryStorage::default());
        let acme: BoxedStorage = std::sync::Arc::new(InMemoryStorage::default());
        let reg = StorageRegistry::new()
            .set(DISK, shared)
            .with_default(DISK)
            .cdn(DISK, "https://cdn.shared.example/media")
            .set("tenant:acme", acme)
            .cdn("tenant:acme", "https://cdn.acme.example/media");

        // Dedicated bucket: resolved as `tenant:<slug>`, and the `{slug}/`
        // prefix stays so other data can share the bucket.
        assert_eq!(disk_name_for(&reg, "acme").as_deref(), Some("tenant:acme"));
        assert_eq!(
            reg.cdn_url("tenant:acme", &key("acme", "photo.jpg")).as_deref(),
            Some("https://cdn.acme.example/media/acme/photo.jpg"),
        );
        // A tenant with no dedicated bucket falls back to the shared one.
        assert_eq!(disk_name_for(&reg, "globex").as_deref(), Some(DISK));
        assert_eq!(
            reg.cdn_url(DISK, &key("globex", "photo.jpg")).as_deref(),
            Some("https://cdn.shared.example/media/globex/photo.jpg"),
        );
    }

    /// A slug that matches one of the host's own disk names does
    /// not select that disk.
    #[test]
    fn a_slug_cannot_select_a_host_disk() {
        use rustango::storage::InMemoryStorage;
        let shared: BoxedStorage = std::sync::Arc::new(InMemoryStorage::default());
        let private: BoxedStorage = std::sync::Arc::new(InMemoryStorage::default());
        let reg = StorageRegistry::new()
            .set(DISK, shared)
            .with_default(DISK)
            .set("private", private);
        assert_eq!(disk_name_for(&reg, "private").as_deref(), Some(DISK));
    }

    #[cfg(feature = "storage_s3")]
    #[test]
    fn tenant_pair_lists_skip_junk_rather_than_failing_boot() {
        assert_eq!(
            parse_pairs("acme=acme-media, globex = globex-media"),
            vec![
                ("acme".to_owned(), "acme-media".to_owned()),
                ("globex".to_owned(), "globex-media".to_owned()),
            ],
        );
        // A stray comma or a malformed entry must not take the site down.
        assert_eq!(
            parse_pairs("acme=b,,broken,=x,y="),
            vec![("acme".to_owned(), "b".to_owned())]
        );
        assert!(parse_pairs("").is_empty());
    }

    #[test]
    fn no_cdn_configured_falls_back_to_the_backend_url() {
        use rustango::storage::InMemoryStorage;
        let mem: BoxedStorage = std::sync::Arc::new(InMemoryStorage::default());
        let reg = StorageRegistry::new().set(DISK, mem).with_default(DISK);
        assert_eq!(reg.cdn_url(DISK, "acme/x.png"), None);
    }

    #[test]
    fn disk_defaults_to_local_without_install() {
        // No registry installed in this test binary → local fallback,
        // which has no base_url and therefore no public URL.
        assert!(disk().url("acme/x.png").is_none());
    }
}
