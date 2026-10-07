//! Per-tenant template overrides, resolved from disk with fallback to
//! the global set.
//!
//! A multisite needs tenants to differ in appearance without differing
//! in code. The render path already picks its template by name from the
//! page-type row (`render.rs` → `tera.render(&pt.default_template, …)`),
//! so *selection* was already data-driven; what was missing was
//! per-tenant template *content*.
//!
//! ## Layout
//!
//! ```text
//! {base_dir}/
//!   templates/                 <- global set, the existing public glob
//!     base.html
//!     landing_page.html
//!   templates_admin/           <- already outside the glob (host chrome)
//!   templates_tenants/         <- this module
//!     acme/
//!       base.html              <- wins over templates/base.html
//!     globex/
//!       landing_page.html      <- wins; its base.html falls back to global
//! ```
//!
//! Tenant directories sit **outside** the global glob deliberately. The
//! glob would otherwise load every tenant's files into every instance
//! under names like `templates_tenants/acme/base.html` — harmless but
//! wasteful, and the same trap the host's `templates_admin/` already
//! avoids.
//!
//! ## Why one Tera per tenant, and not name prefixes
//!
//! `{% extends %}` and `{% include %}` resolve by template **name**,
//! within one Tera instance, when inheritance chains are built. Prefixing
//! a tenant's templates into the shared instance therefore does *not*
//! give it its own base: `{% extends "base.html" %}` inside
//! `templates_tenants/acme/page.html` would still resolve to the global
//! `base.html`. Making prefixes work means rewriting every inheritance
//! reference at load time — fragile, and it breaks the ability to copy a
//! template from the global set into a tenant folder unchanged.
//!
//! Giving each tenant its own instance makes the override natural:
//! overlay the tenant's files onto a clone of the global set, so a name
//! either resolves to the tenant's copy or falls through to the global
//! one, and inheritance follows the same rule.
//!
//! ## Cost
//!
//! Building a tenant instance is `base.clone()` plus a parse of that
//! tenant's own files. The clone is the measured cost —
//! ~2.4 ms for a full corpus (the same deep-copy that made the page
//! editor spend 423 ms/request before it was memoized), so instances are
//! cached, and the cache is bounded.
//!
//! Hand [`TenantTemplates::new`] a Tera holding only the **public**
//! templates where you can. A clone of an instance that also carries the
//! admin corpus is far larger, and the public render path never extends
//! an admin template.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use tera::Tera;

/// Default directory name, a sibling of `templates/`.
pub const DEFAULT_DIR: &str = "templates_tenants";

/// How eagerly to notice that a tenant's files changed on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reload {
    /// Fingerprint the tenant directory on every lookup. Development:
    /// edit a file, reload the browser, see it.
    Always,
    /// Fingerprint at most this often. Production default — a template
    /// drop is picked up within the interval with no restart, without
    /// paying a directory walk per request.
    Every(Duration),
    /// Never re-check. The instance is built once per process, so a
    /// template change needs a restart. Cheapest, and appropriate when
    /// templates only ever change via a deploy that restarts anyway.
    Never,
}

impl Default for Reload {
    fn default() -> Self {
        Self::Every(Duration::from_secs(5))
    }
}

/// Monotonic counter standing in for a clock in the LRU ordering.
///
/// A logical tick, not a timestamp: eviction only needs *which entry was
/// touched last*, and a counter gives that exactly, with no dependence on
/// clock resolution — two hits inside the same nanosecond still order.
static TICK: AtomicU64 = AtomicU64::new(0);

fn tick() -> u64 {
    TICK.fetch_add(1, Ordering::Relaxed)
}

/// One cached tenant instance.
struct Entry {
    tera: Arc<Tera>,
    /// Fingerprint of the tenant directory when this was built.
    fingerprint: u64,
    /// When the fingerprint was last recomputed (for [`Reload::Every`]).
    checked: Instant,
    /// LRU ordering. Atomic so a cache *hit* can record recency while
    /// holding only a read lock — the reason this is not an `Instant`
    /// field: the hot path must not need the write lock, and an LRU that
    /// cannot observe hits evicts the busiest tenant first.
    used: AtomicU64,
}

/// The file in a tenant's override directory naming its owner.
/// Not `.html`, so it is never loaded as a template or listed as one.
const OWNER_FILE: &str = ".rcms-owner";

/// Where [`TenantTemplates::claim`] moves a directory that belongs to a
/// purged tenant. Dot-prefixed, so no slug can resolve to it.
const ORPHANED_DIR: &str = ".orphaned";

/// What [`TenantTemplates::claim`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// The tenant has no override directory.
    NoOverrides,
    /// The directory is marked for this tenant.
    Owned,
    /// The directory had no marker, and now carries this tenant's.
    Adopted,
    /// The directory belonged to an earlier tenant with the same slug and
    /// was moved to `to`.
    MovedAside { previous_owner: String, to: PathBuf },
}

/// Resolves a Tera instance per tenant, overlaying that tenant's
/// templates onto the global set.
pub struct TenantTemplates {
    base: Arc<Tera>,
    root: PathBuf,
    /// Where the *global* templates live on disk. Only the editor needs
    /// this — the renderer gets the global set from `base`, already
    /// compiled. Supplying it lets the editor show what a name currently
    /// falls back to, and seed a new override from it.
    global_dir: Option<PathBuf>,
    reload: Reload,
    capacity: usize,
    cache: RwLock<HashMap<String, Entry>>,
}

impl TenantTemplates {
    /// `base` is the global set — ideally public templates only, see the
    /// module docs on cost. `root` is the directory holding one
    /// subdirectory per tenant slug.
    #[must_use]
    pub fn new(base: Arc<Tera>, root: impl Into<PathBuf>) -> Self {
        Self {
            base,
            root: root.into(),
            global_dir: None,
            reload: Reload::default(),
            // Mirrors the framework's `max_cached_scoped_pools`: past the
            // cap the long tail rebuilds per lookup rather than the
            // process holding an unbounded number of compiled corpora.
            capacity: 64,
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Point the admin editor at the global template directory, so it
    /// can show the fallback a tenant would inherit and seed new
    /// overrides from it.
    #[must_use]
    pub fn with_global_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.global_dir = Some(dir.into());
        self
    }

    #[must_use]
    pub fn with_reload(mut self, reload: Reload) -> Self {
        self.reload = reload;
        self
    }

    #[must_use]
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity.max(1);
        self
    }

    /// This tenant's cached instance, if it has one that isn't due a
    /// re-check — no filesystem access. `None` means ask
    /// [`Self::for_tenant`], which may touch the disk.
    #[must_use]
    pub fn cached(&self, slug: &str) -> Option<Arc<Tera>> {
        let cache = self.cache.read().ok()?;
        let e = cache.get(slug)?;
        if self.due(e) {
            return None;
        }
        e.used.store(tick(), Ordering::Relaxed);
        Some(Arc::clone(&e.tera))
    }

    /// The instance to render this tenant's pages with.
    ///
    /// A tenant with no directory gets the global set back directly — no
    /// clone, no cache entry, so the common "tenant hasn't customised
    /// anything" case costs nothing.
    #[must_use]
    pub fn for_tenant(&self, slug: &str) -> Arc<Tera> {
        let Some(dir) = self.tenant_dir(slug) else {
            return Arc::clone(&self.base);
        };

        // Fast path: a cached instance that we are not due to re-check.
        if let Ok(cache) = self.cache.read() {
            if let Some(e) = cache.get(slug) {
                if !self.due(e) {
                    e.used.store(tick(), Ordering::Relaxed);
                    return Arc::clone(&e.tera);
                }
            }
        }

        let fingerprint = fingerprint(&dir);

        // Re-check path: the fingerprint may be unchanged, in which case
        // the cached instance stands and only its timestamps move.
        if let Ok(mut cache) = self.cache.write() {
            if let Some(e) = cache.get_mut(slug) {
                if e.fingerprint == fingerprint {
                    e.checked = Instant::now();
                    e.used.store(tick(), Ordering::Relaxed);
                    return Arc::clone(&e.tera);
                }
            }
        }

        let tera = Arc::new(self.build(&dir));

        if let Ok(mut cache) = self.cache.write() {
            if cache.len() >= self.capacity && !cache.contains_key(slug) {
                Self::evict_lru(&mut cache);
            }
            cache.insert(
                slug.to_owned(),
                Entry {
                    tera: Arc::clone(&tera),
                    fingerprint,
                    checked: Instant::now(),
                    used: AtomicU64::new(tick()),
                },
            );
        }
        tera
    }

    /// Whether this entry is due a fingerprint re-check.
    fn due(&self, e: &Entry) -> bool {
        match self.reload {
            Reload::Always => true,
            Reload::Never => false,
            Reload::Every(d) => e.checked.elapsed() >= d,
        }
    }

    fn evict_lru(cache: &mut HashMap<String, Entry>) {
        // O(n) over a small n, which beats pulling in an LRU crate for
        // a map that holds tens of entries.
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, e)| e.used.load(Ordering::Relaxed))
            .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest);
        }
    }

    /// The directory for a slug, if it exists and the slug is safe.
    ///
    /// Slugs come from the tenant registry rather than a request, but a
    /// path is still built from them, so a slug that could climb out of
    /// `root` is refused rather than trusted.
    fn tenant_dir(&self, slug: &str) -> Option<PathBuf> {
        if slug.is_empty()
            || slug.starts_with('.')
            || slug.contains('/')
            || slug.contains('\\')
        {
            return None;
        }
        let dir = self.root.join(slug);
        dir.is_dir().then_some(dir)
    }

    /// Bind a tenant's override directory to `owner`, the identity of the
    /// tenant database it belongs to.
    ///
    /// Overrides live on disk, keyed by slug, and a purged tenant's slug
    /// can be reused: without this the next tenant given that slug renders
    /// through the old customer's files. The directory carries an owner
    /// marker; a directory marked for another owner is moved aside, not
    /// deleted, and dropped from the cache. An unmarked directory predates
    /// the marker and is adopted.
    ///
    /// # Errors
    /// Filesystem failures reading or writing the marker, or moving a
    /// stale directory aside.
    pub fn claim(&self, slug: &str, owner: &str) -> std::io::Result<Claim> {
        let Some(dir) = self.tenant_dir(slug) else {
            return Ok(Claim::NoOverrides);
        };
        let marker = dir.join(OWNER_FILE);
        match std::fs::read_to_string(&marker) {
            Ok(found) if found.trim() == owner => Ok(Claim::Owned),
            Ok(found) => {
                let aside_root = self.root.join(ORPHANED_DIR);
                std::fs::create_dir_all(&aside_root)?;
                let stamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let aside = aside_root.join(format!("{slug}-{stamp}"));
                std::fs::rename(&dir, &aside)?;
                if let Ok(mut cache) = self.cache.write() {
                    cache.remove(slug);
                }
                Ok(Claim::MovedAside { previous_owner: found.trim().to_owned(), to: aside })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::write(&marker, owner)?;
                Ok(Claim::Adopted)
            }
            Err(e) => Err(e),
        }
    }

    /// Overlay a tenant's files onto a clone of the global set.
    fn build(&self, dir: &Path) -> Tera {
        let mut tera = (*self.base).clone();
        let mut overrides: Vec<(String, String)> = Vec::new();
        collect(dir, dir, &mut overrides);

        // Sorted so a build is deterministic regardless of directory
        // iteration order — two processes given the same files produce
        // the same instance.
        overrides.sort_by(|a, b| a.0.cmp(&b.0));

        for (name, body) in overrides {
            if let Err(e) = tera.add_raw_template(&name, &body) {
                // One malformed template must not cost the tenant every
                // other page: log it and leave the global version in
                // place for that name.
                tracing::warn!(
                    target: "rustango_cms::tenant_templates",
                    template = %name, error = %e,
                    "tenant template failed to parse; falling back to the global one",
                );
            }
        }
        if let Err(e) = tera.build_inheritance_chains() {
            tracing::error!(
                target: "rustango_cms::tenant_templates",
                error = %e,
                "tenant template inheritance is broken; serving the global set",
            );
            return (*self.base).clone();
        }
        tera
    }
}

/// Recursively collect `(template name, body)` for a tenant directory.
///
/// Names are relative to the tenant's own directory and `/`-separated,
/// so `acme/blocks/hero.html` registers as `blocks/hero.html` — the same
/// name the global set uses. That is what lets a template be copied from
/// the global folder into a tenant folder and work unmodified.
fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Same rule as the editor: a path that resolves outside `root`
        // is not this tenant's template, however it got there. Without
        // this a symlink would still be *rendered*, so the write guard
        // alone would close the door and leave a window.
        if !contained(root, &path) {
            continue;
        }
        if path.is_dir() {
            collect(root, &path, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let name = rel
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect::<Vec<_>>()
            .join("/");
        if let Ok(body) = std::fs::read_to_string(&path) {
            out.push((name, body));
        }
    }
}

/// Cheap content fingerprint for a tenant directory.
///
/// Hashes each file's relative name, length and mtime rather than its
/// bytes — enough to notice an edit, a rename, an addition or a removal
/// without reading the files, which is the point of checking often.
///
/// Same reasoning as `cms_asset_version()`, which fingerprints the
/// bundled assets plus the override's mtime and length.
fn fingerprint(dir: &Path) -> u64 {
    let mut files: Vec<(String, u64, u64)> = Vec::new();
    let mut names: Vec<(String, String)> = Vec::new();
    collect_meta(dir, dir, &mut names);
    for (name, _) in &names {
        let path = dir.join(name);
        let (len, mtime) = std::fs::metadata(&path)
            .map(|m| {
                (
                    m.len(),
                    m.modified()
                        .ok()
                        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_nanos() as u64),
                )
            })
            .unwrap_or((0, 0));
        files.push((name.clone(), len, mtime));
    }
    files.sort();

    // FNV-1a, 64-bit. A validator, not a security primitive.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for (name, len, mtime) in &files {
        eat(name.as_bytes());
        eat(&len.to_le_bytes());
        eat(&mtime.to_le_bytes());
    }
    h
}

/// Like [`collect`] but names only — no file reads.
fn collect_meta(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !contained(root, &path) {
            continue;
        }
        if path.is_dir() {
            collect_meta(root, &path, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            let name = rel
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join("/");
            out.push((name, String::new()));
        }
    }
}

/// Where a template a tenant renders actually came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The tenant has its own copy in `templates_tenants/<slug>/`.
    Override,
    /// Inherited from the global set.
    Global,
}

/// One row in the editor's listing.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TemplateEntry {
    pub name: String,
    pub source: Source,
    pub bytes: u64,
}

/// Why a write was refused.
#[derive(Debug)]
pub enum EditError {
    /// The name is not a template path this may write.
    BadName(String),
    /// The body is not valid Tera. Carries the parse error, so the
    /// editor can show the author what is wrong.
    Invalid(String),
    /// The editor is not configured (no tenant-template root).
    NotConfigured,
    Io(std::io::Error),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName(n) => write!(f, "`{n}` is not a valid template name"),
            Self::Invalid(e) => write!(f, "{e}"),
            Self::NotConfigured => write!(f, "per-tenant templates are not configured"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Validate a template name supplied by an editor.
///
/// Everything here is a request parameter, so nothing is trusted: the
/// name must be relative, must stay inside the tenant's directory, and
/// must be a template rather than an arbitrary file. Without the
/// extension check this endpoint would happily write `../../.ssh/config`.
///
/// # Errors
/// [`EditError::BadName`] with the offending value.
pub fn safe_name(name: &str) -> Result<String, EditError> {
    let n = name.trim().trim_start_matches('/');
    let bad = n.is_empty()
        || !n.ends_with(".html")
        || n.contains('\\')
        || n.starts_with('.')
        || n.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..")
        || std::path::Path::new(n).is_absolute();
    if bad {
        return Err(EditError::BadName(name.to_owned()));
    }
    Ok(n.to_owned())
}

impl TenantTemplates {
    /// The tenant's own override directory, whether or not it exists.
    #[must_use]
    pub fn dir_for(&self, slug: &str) -> Option<PathBuf> {
        if slug.is_empty() || slug.contains('/') || slug.contains('\\') || slug.starts_with('.') {
            return None;
        }
        Some(self.root.join(slug))
    }

    /// Every template this tenant renders: its own overrides, plus the
    /// global names it inherits. Sorted, overrides marked.
    #[must_use]
    pub fn list_for(&self, slug: &str) -> Vec<TemplateEntry> {
        let mut seen: HashMap<String, TemplateEntry> = HashMap::new();

        if let Some(g) = &self.global_dir {
            let mut names = Vec::new();
            collect_meta(g, g, &mut names);
            for (name, _) in names {
                let bytes = std::fs::metadata(g.join(&name)).map(|m| m.len()).unwrap_or(0);
                seen.insert(name.clone(), TemplateEntry { name, source: Source::Global, bytes });
            }
        }
        if let Some(dir) = self.dir_for(slug).filter(|d| d.is_dir()) {
            let mut names = Vec::new();
            collect_meta(&dir, &dir, &mut names);
            for (name, _) in names {
                let bytes = std::fs::metadata(dir.join(&name)).map(|m| m.len()).unwrap_or(0);
                seen.insert(name.clone(), TemplateEntry { name, source: Source::Override, bytes });
            }
        }
        let mut out: Vec<TemplateEntry> = seen.into_values().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// The tenant's override body for `name`, if it has one.
    #[must_use]
    pub fn read_override(&self, slug: &str, name: &str) -> Option<String> {
        let name = safe_name(name).ok()?;
        let dir = self.dir_for(slug)?;
        let path = dir.join(name);
        if !contained(&dir, &path) {
            return None;
        }
        std::fs::read_to_string(path).ok()
    }

    /// The global body for `name` — what the tenant inherits today, and
    /// the sensible starting point for a new override.
    #[must_use]
    pub fn read_global(&self, name: &str) -> Option<String> {
        let name = safe_name(name).ok()?;
        std::fs::read_to_string(self.global_dir.as_ref()?.join(name)).ok()
    }

    /// Create or replace a tenant's override.
    ///
    /// The body is parsed before anything is written, so a syntax error
    /// is reported to the author instead of being discovered by the next
    /// visitor. (The renderer's per-template fallback still exists, but
    /// it is a safety net, not the place to find out.)
    ///
    /// # Errors
    /// [`EditError`] for a rejected name, invalid Tera, or an I/O failure.
    pub fn write_override(&self, slug: &str, name: &str, body: &str) -> Result<(), EditError> {
        let name = safe_name(name)?;
        let dir = self.dir_for(slug).ok_or(EditError::NotConfigured)?;

        self.validate(slug, &name, body)?;

        let path = dir.join(&name);
        // Contained before anything is created (#770): checked after, a
        // symlinked subdirectory let the write create directories outside
        // the tenant's folder even though the file itself was refused. The
        // tenant root is the one directory safe to create first — the check
        // needs it to exist.
        std::fs::create_dir_all(&dir).map_err(EditError::Io)?;
        if !contained(&dir, &path) {
            return Err(EditError::BadName(name));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(EditError::Io)?;
        }
        std::fs::write(&path, body).map_err(EditError::Io)?;
        Ok(())
    }

    /// Check a candidate body the way the renderer will actually load it.
    ///
    /// Parsing it against an empty Tera is not enough — and is actively
    /// wrong. A template that opens `{% extends "_base.html" %}` is
    /// perfectly valid, but in isolation the parent is "not loaded" and
    /// it looks broken. So the probe is a clone of the global set plus
    /// this tenant's other overrides: the same instance `build` makes,
    /// with the candidate swapped in.
    ///
    /// That also makes the check *stronger* than syntax alone — it
    /// catches extending or including a template that does not exist,
    /// which is the mistake an author is most likely to make when
    /// copying a template between tenants.
    ///
    /// # Errors
    /// [`EditError::Invalid`] carrying the parse or inheritance error.
    pub fn validate(&self, slug: &str, name: &str, body: &str) -> Result<(), EditError> {
        let mut probe = (*self.base).clone();

        if let Some(dir) = self.dir_for(slug).filter(|d| d.is_dir()) {
            let mut existing = Vec::new();
            collect(&dir, &dir, &mut existing);
            existing.sort_by(|a, b| a.0.cmp(&b.0));
            for (n, b) in existing {
                // Skip the one being replaced, and ignore failures in the
                // others: they are already live, and reporting a
                // *different* file's problem here would be baffling.
                if n == name {
                    continue;
                }
                let _ = probe.add_raw_template(&n, &b);
            }
        }

        probe
            .add_raw_template(name, body)
            .and_then(|()| probe.build_inheritance_chains())
            .map_err(|e| EditError::Invalid(chain(&e)))
    }

    /// Drop a tenant's override, reverting that name to the global one.
    ///
    /// # Errors
    /// [`EditError`] for a rejected name or an I/O failure. Deleting a
    /// name the tenant never overrode is a no-op, not an error — the
    /// end state the caller asked for is the one they get.
    pub fn delete_override(&self, slug: &str, name: &str) -> Result<(), EditError> {
        let name = safe_name(name)?;
        let dir = self.dir_for(slug).ok_or(EditError::NotConfigured)?;
        let path = dir.join(&name);
        if !path.exists() {
            return Ok(());
        }
        if !contained(&dir, &path) {
            return Err(EditError::BadName(name));
        }
        std::fs::remove_file(&path).map_err(EditError::Io)?;
        // Tidy empty parents left behind by a nested override, stopping
        // at the tenant's own directory.
        let mut p = path.parent().map(std::path::Path::to_path_buf);
        while let Some(cur) = p {
            if cur == dir || !cur.starts_with(&dir) {
                break;
            }
            if std::fs::remove_dir(&cur).is_err() {
                break;
            }
            p = cur.parent().map(std::path::Path::to_path_buf);
        }
        Ok(())
    }
}

/// Whether `path` really lives inside `root`, symlinks resolved.
///
/// [`safe_name`] is lexical — it rejects `..` in the *name*, which is
/// enough to stop a crafted request escaping. It cannot see a symlink
/// already sitting on disk: a link at
/// `templates_tenants/acme/peek.html` pointing at another tenant's file
/// let `acme` both read and **overwrite** it, because every string
/// involved looked perfectly ordinary.
///
/// Templates arrive by rsync or git, and a symlink is easy to miss in a
/// diff, so the check has to be on the resolved path rather than the
/// name. Resolves the deepest existing ancestor (the leaf may not exist
/// yet, on a create) and re-appends the rest.
fn contained(root: &Path, path: &Path) -> bool {
    let Ok(root_real) = root.canonicalize() else {
        return false;
    };
    let mut probe = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = probe.canonicalize() {
            let mut full = real;
            for seg in tail.iter().rev() {
                full.push(seg);
            }
            return full.starts_with(&root_real);
        }
        let Some(name) = probe.file_name().map(std::ffi::OsStr::to_owned) else {
            return false;
        };
        tail.push(name);
        if !probe.pop() {
            return false;
        }
    }
}

/// Flatten a Tera error and its causes into one line.
///
/// Tera puts the actual problem in `source()` — the top-level message is
/// usually just "Failed to parse 'x.html'", which tells an author
/// nothing.
fn chain(e: &tera::Error) -> String {
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = std::error::Error::source(s);
    }
    msg
}

/// The instance the admin editor operates on.
///
/// Registered by the host at boot. A `OnceLock` rather than a field on
/// `AdminState` so wiring the editor does not change the admin router's
/// signature for every existing host.
/// The identity [`TenantTemplates::claim`] binds a directory to: when the
/// tenant's database had its first migration applied. It does not change
/// for the life of the database and is new for any database provisioned
/// later, a purged slug's successor included. `None` before any migration.
///
/// # Errors
/// Driver / query failures.
pub async fn database_birth(
    pool: &rustango::sql::Pool,
) -> Result<Option<String>, rustango::sql::ExecError> {
    // `applied_at` is TIMESTAMPTZ, DATETIME or TEXT by dialect; read it as
    // text on each.
    let as_text = match pool.dialect().name() {
        "postgres" => "CAST(MIN(applied_at) AS TEXT)",
        "mysql" => "CAST(MIN(applied_at) AS CHAR)",
        _ => "MIN(applied_at)",
    };
    // The framework's migration ledger (its name is not re-exported).
    let sql = format!("SELECT {as_text} FROM __rustango_migrations__");
    let rows: Vec<(Option<String>,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool).await?;
    Ok(rows.into_iter().next().and_then(|r| r.0))
}

static INSTALLED: std::sync::OnceLock<Arc<TenantTemplates>> = std::sync::OnceLock::new();

/// Make this instance available to the admin template editor.
pub fn install(tt: Arc<TenantTemplates>) {
    // Set once; a second install would be silently lost otherwise (#697).
    if INSTALLED.set(tt).is_err() {
        tracing::error!(target: "rustango_cms", "install: tenant templates are already installed; this set is ignored");
    }
}

/// The installed instance, if the host configured one.
#[must_use]
pub fn installed() -> Option<Arc<TenantTemplates>> {
    INSTALLED.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    /// A scratch directory that removes itself. Avoids a `tempfile`
    /// dev-dependency for a handful of tests.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "rcms-tt-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir_all(&p).expect("scratch");
            Self(p)
        }
        fn write(&self, rel: &str, body: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The global set: a base with a block, and a page that fills it.
    fn global() -> Arc<Tera> {
        let mut t = Tera::default();
        t.add_raw_templates(vec![
            ("base.html", "GLOBAL-BASE[{% block body %}{% endblock %}]"),
            ("page.html", "{% extends \"base.html\" %}{% block body %}global-page{% endblock %}"),
            ("only_global.html", "only-global"),
        ])
        .unwrap();
        t.build_inheritance_chains().unwrap();
        Arc::new(t)
    }

    fn render(t: &Tera, name: &str) -> String {
        t.render(name, &tera::Context::new()).expect("render")
    }

    #[test]
    fn a_tenant_without_a_directory_gets_the_global_set_itself() {
        // Not a clone: the common case (tenant has customised nothing)
        // must cost neither memory nor a cache slot.
        let s = Scratch::new();
        let base = global();
        let tt = TenantTemplates::new(Arc::clone(&base), &s.0);
        let got = tt.for_tenant("nobody");
        assert!(Arc::ptr_eq(&got, &base), "should hand back the global Arc");
        assert_eq!(tt.cache.read().unwrap().len(), 0, "no cache entry either");
    }

    #[test]
    fn a_tenant_template_wins_over_the_global_one_of_the_same_name() {
        let s = Scratch::new();
        s.write("acme/page.html", "acme-page");
        let tt = TenantTemplates::new(global(), &s.0);
        assert_eq!(render(&tt.for_tenant("acme"), "page.html"), "acme-page");
    }

    #[test]
    fn anything_the_tenant_has_not_overridden_falls_back() {
        let s = Scratch::new();
        s.write("acme/page.html", "acme-page");
        let tt = TenantTemplates::new(global(), &s.0);
        let t = tt.for_tenant("acme");
        assert_eq!(render(&t, "only_global.html"), "only-global");
    }

    #[test]
    fn extends_resolves_to_the_tenants_base_not_the_global_one() {
        // The whole reason this is one instance per tenant rather than
        // name prefixes in a shared one. The tenant's page says
        // `{% extends "base.html" %}` — unmodified, copy-pasteable — and
        // must get the tenant's base.
        let s = Scratch::new();
        s.write("acme/base.html", "ACME-BASE[{% block body %}{% endblock %}]");
        let tt = TenantTemplates::new(global(), &s.0);
        let out = render(&tt.for_tenant("acme"), "page.html");
        assert_eq!(out, "ACME-BASE[global-page]", "got {out}");

        // And a tenant that overrode nothing still gets the global base.
        assert_eq!(render(&tt.for_tenant("other"), "page.html"), "GLOBAL-BASE[global-page]");
    }

    #[test]
    fn nested_directories_keep_the_name_the_global_set_uses() {
        // `acme/blocks/hero.html` must register as `blocks/hero.html`,
        // or a copied template's `{% include %}` would not match.
        let s = Scratch::new();
        s.write("acme/blocks/hero.html", "acme-hero");
        s.write("acme/page.html", "{% include \"blocks/hero.html\" %}");
        let tt = TenantTemplates::new(global(), &s.0);
        assert_eq!(render(&tt.for_tenant("acme"), "page.html"), "acme-hero");
    }

    #[test]
    fn non_html_files_are_ignored() {
        let s = Scratch::new();
        s.write("acme/page.html", "acme-page");
        s.write("acme/README.md", "not a template");
        s.write("acme/style.css", "body{}");
        let tt = TenantTemplates::new(global(), &s.0);
        let t = tt.for_tenant("acme");
        assert_eq!(render(&t, "page.html"), "acme-page");
        assert!(t.get_template_names().all(|n| n.ends_with(".html")));
    }

    #[test]
    fn a_repeat_lookup_reuses_the_built_instance() {
        let s = Scratch::new();
        s.write("acme/page.html", "acme-page");
        let tt = TenantTemplates::new(global(), &s.0).with_reload(Reload::Never);
        let a = tt.for_tenant("acme");
        let b = tt.for_tenant("acme");
        assert!(Arc::ptr_eq(&a, &b), "should not rebuild on every request");
    }

    #[test]
    fn an_edit_is_picked_up_without_a_restart() {
        // The point of the whole design: drop a file, next request sees
        // it — no rebuild, no process restart.
        let s = Scratch::new();
        s.write("acme/page.html", "before");
        let tt = TenantTemplates::new(global(), &s.0).with_reload(Reload::Always);
        assert_eq!(render(&tt.for_tenant("acme"), "page.html"), "before");

        s.write("acme/page.html", "after-and-longer");
        assert_eq!(render(&tt.for_tenant("acme"), "page.html"), "after-and-longer");
    }

    #[test]
    fn a_new_file_is_noticed_not_just_a_changed_one() {
        let s = Scratch::new();
        s.write("acme/page.html", "acme-page");
        let tt = TenantTemplates::new(global(), &s.0).with_reload(Reload::Always);
        let _ = tt.for_tenant("acme");

        s.write("acme/only_global.html", "acme-now-owns-this");
        assert_eq!(
            render(&tt.for_tenant("acme"), "only_global.html"),
            "acme-now-owns-this",
        );
    }

    #[test]
    fn reload_never_does_not_notice_an_edit() {
        // Documents the trade rather than leaving it implied.
        let s = Scratch::new();
        s.write("acme/page.html", "before");
        let tt = TenantTemplates::new(global(), &s.0).with_reload(Reload::Never);
        assert_eq!(render(&tt.for_tenant("acme"), "page.html"), "before");
        s.write("acme/page.html", "after-and-longer");
        assert_eq!(render(&tt.for_tenant("acme"), "page.html"), "before");
    }

    #[test]
    fn a_malformed_tenant_template_falls_back_instead_of_breaking_the_site() {
        // One bad file must cost that page, not every page the tenant has.
        let s = Scratch::new();
        s.write("acme/page.html", "{% for x in %}broken");
        s.write("acme/only_global.html", "acme-fine");
        let tt = TenantTemplates::new(global(), &s.0);
        let t = tt.for_tenant("acme");
        assert_eq!(render(&t, "page.html"), "GLOBAL-BASE[global-page]");
        assert_eq!(render(&t, "only_global.html"), "acme-fine");
    }

    #[test]
    fn a_slug_cannot_climb_out_of_the_root() {
        // Slugs come from the registry, not a request — but a path is
        // still built from them, so they are not trusted blindly.
        let s = Scratch::new();
        s.write("acme/page.html", "acme-page");
        let tt = TenantTemplates::new(global(), &s.0);
        for slug in ["..", ".", "", "../acme", "a/b", "a\\b"] {
            assert!(tt.tenant_dir(slug).is_none(), "slug {slug:?} must be refused");
        }
    }

    // ---- editor CRUD ---------------------------------------------------

    fn store(s: &Scratch) -> TenantTemplates {
        // A global dir the editor can read fallbacks from.
        s.write("_global/base.html", "GLOBAL-BASE[{% block body %}{% endblock %}]");
        s.write("_global/page.html", "{% extends \"base.html\" %}{% block body %}g{% endblock %}");
        TenantTemplates::new(global(), s.0.join("t")).with_global_dir(s.0.join("_global"))
    }

    #[test]
    fn a_template_that_extends_a_global_parent_validates() {
        // The bug this exists for: validating against an empty Tera made
        // `{% extends "base.html" %}` look broken, because the parent was
        // "not loaded" — so every realistic template was rejected.
        let s = Scratch::new();
        let tt = store(&s);
        tt.write_override("acme", "page.html", "{% extends \"base.html\" %}{% block body %}a{% endblock %}")
            .expect("extending a global parent is valid");
        assert!(tt.read_override("acme", "page.html").unwrap().contains("{% block body %}a"));
    }

    #[test]
    fn extending_a_parent_that_does_not_exist_is_refused() {
        // The check is stronger than syntax: a missing parent is the
        // mistake an author makes copying a template between tenants.
        let s = Scratch::new();
        let tt = store(&s);
        let err = tt
            .write_override("acme", "page.html", "{% extends \"nope.html\" %}")
            .expect_err("must refuse");
        assert!(matches!(err, EditError::Invalid(_)), "{err}");
        assert!(err.to_string().contains("nope.html"), "{err}");
        assert!(tt.read_override("acme", "page.html").is_none(), "nothing written");
    }

    #[test]
    fn a_syntax_error_is_refused_with_a_useful_message() {
        let s = Scratch::new();
        let tt = store(&s);
        let err = tt.write_override("acme", "x.html", "{% for a in %}").expect_err("must refuse");
        // The top-level Tera message is just "Failed to parse"; the cause
        // carries the detail, so the chain must be flattened.
        assert!(err.to_string().len() > "Failed to parse 'x.html'".len(), "{err}");
    }

    #[test]
    fn a_tenants_own_base_is_used_when_validating_its_other_templates() {
        // acme overrides base.html; a page extending "base.html" must
        // validate against *acme's* base, not only the global one.
        let s = Scratch::new();
        let tt = store(&s);
        tt.write_override("acme", "base.html", "A[{% block body %}{% endblock %}]").unwrap();
        tt.write_override("acme", "page.html", "{% extends \"base.html\" %}{% block body %}x{% endblock %}")
            .expect("valid against the tenant's own base");
    }

    #[test]
    fn listing_marks_overrides_and_includes_inherited_names() {
        let s = Scratch::new();
        let tt = store(&s);
        tt.write_override("acme", "page.html", "own").unwrap();
        let rows = tt.list_for("acme");
        let by: HashMap<&str, Source> = rows.iter().map(|e| (e.name.as_str(), e.source)).collect();
        assert_eq!(by.get("page.html"), Some(&Source::Override));
        assert_eq!(by.get("base.html"), Some(&Source::Global), "inherited names are listed too");
    }

    #[test]
    fn deleting_an_override_reverts_to_the_global_one() {
        let s = Scratch::new();
        let tt = store(&s);
        tt.write_override("acme", "page.html", "own").unwrap();
        assert!(tt.read_override("acme", "page.html").is_some());
        tt.delete_override("acme", "page.html").unwrap();
        assert!(tt.read_override("acme", "page.html").is_none());
        // The global file is untouched — reverting is not deleting.
        assert!(tt.read_global("page.html").is_some(), "global must survive");
    }

    #[test]
    fn deleting_something_never_overridden_is_not_an_error() {
        let s = Scratch::new();
        let tt = store(&s);
        tt.delete_override("acme", "page.html").expect("no-op, not a failure");
    }

    #[test]
    fn a_name_that_escapes_the_tenant_directory_is_refused() {
        // Without this the editor would write anywhere the process can
        // reach. `..` in any position is the whole game.
        for bad in ["../../evil.html", "a/../../b.html", "..", "../x.html",
                    ".hidden.html", "notatemplate.txt", "", "x.html/"] {
            assert!(safe_name(bad).is_err(), "{bad:?} must be refused");
        }
        for ok in ["page.html", "blocks/hero.html", "a/b/c.html", "/page.html"] {
            assert!(safe_name(ok).is_ok(), "{ok:?} must be allowed");
        }
    }

    #[test]
    fn an_absolute_looking_name_is_confined_not_escaped() {
        // A leading `/` is stripped as a convenience — an author may
        // type `/page.html`. The property that matters is not that
        // scary-looking names are *rejected*, but that whatever survives
        // resolves INSIDE the tenant's own directory.
        let s = Scratch::new();
        let tt = store(&s);
        let name = safe_name("/etc/passwd.html").expect("stripped to a relative name");
        assert_eq!(name, "etc/passwd.html");

        tt.write_override("acme", "/etc/passwd.html", "x").unwrap();
        assert!(
            s.0.join("t/acme/etc/passwd.html").exists(),
            "must land under the tenant, as an oddly-named template",
        );
        assert!(
            !std::path::Path::new("/etc/passwd.html").exists(),
            "and must not have touched the real filesystem root",
        );
    }

    #[test]
    fn every_accepted_name_resolves_inside_the_tenant_directory() {
        // Belt and braces over `safe_name`: assert the *resolved path*,
        // not just the string, for anything the validator lets through.
        let s = Scratch::new();
        let tt = store(&s);
        let dir = tt.dir_for("acme").unwrap();
        for candidate in ["page.html", "/page.html", "a/b/c.html", "/etc/passwd.html"] {
            let name = safe_name(candidate).expect("accepted");
            let path = dir.join(&name);
            assert!(
                path.starts_with(&dir),
                "{candidate:?} resolved to {path:?}, outside {dir:?}",
            );
        }
    }

    #[test]
    fn a_refused_write_leaves_no_file_behind() {
        let s = Scratch::new();
        let tt = store(&s);
        assert!(tt.write_override("acme", "../escape.html", "x").is_err());
        assert!(!s.0.join("escape.html").exists(), "must not write outside");
    }

    #[test]
    fn a_nested_override_and_its_empty_parents_are_cleaned_up() {
        let s = Scratch::new();
        let tt = store(&s);
        tt.write_override("acme", "blocks/deep/hero.html", "h").unwrap();
        assert!(s.0.join("t/acme/blocks/deep/hero.html").exists());
        tt.delete_override("acme", "blocks/deep/hero.html").unwrap();
        assert!(!s.0.join("t/acme/blocks").exists(), "empty dirs tidied");
        assert!(s.0.join("t/acme").exists(), "but not the tenant's own dir");
    }

    #[test]
    fn an_edit_through_the_store_is_visible_to_the_renderer() {
        // Editor and renderer must share one instance, or a save would
        // write files a different cache is serving from.
        let s = Scratch::new();
        let tt = store(&s).with_reload(Reload::Always);
        tt.write_override("acme", "page.html", "{% extends \"base.html\" %}{% block body %}EDITED{% endblock %}")
            .unwrap();
        let out = tt.for_tenant("acme").render("page.html", &tera::Context::new()).unwrap();
        assert_eq!(out, "GLOBAL-BASE[EDITED]");
    }

    #[test]
    fn the_renderable_set_includes_overrides_globals_and_baked_templates() {
        // What the page-type picker offers. It must come from the Tera
        // instance, not a directory listing: a host can register a
        // template in code that has no file to stat, and the renderer
        // will happily resolve it.
        let s = Scratch::new();
        let tt = store(&s);
        tt.write_override("acme", "own.html", "own").unwrap();
        let names: Vec<String> = tt
            .for_tenant("acme")
            .get_template_names()
            .map(str::to_owned)
            .collect();
        assert!(names.iter().any(|n| n == "own.html"), "tenant override: {names:?}");
        assert!(names.iter().any(|n| n == "page.html"), "baked/global: {names:?}");
    }

    #[test]
    fn a_name_the_tenant_cannot_render_is_absent_from_the_set() {
        // The page-type assignment checks membership of this set before
        // saving, so a name missing here is a name that would 500 every
        // page of that type.
        let s = Scratch::new();
        let tt = store(&s);
        let names: Vec<String> = tt
            .for_tenant("acme")
            .get_template_names()
            .map(str::to_owned)
            .collect();
        assert!(!names.iter().any(|n| n == "does_not_exist.html"));
    }

    /// Not an assertion — a measurement, run on demand:
    /// `cargo test --features sqlite --lib cost_of -- --ignored --nocapture`
    #[test]
    #[ignore = "measurement, not a pass/fail check"]
    fn cost_of_the_tenant_template_layer() {
        use std::time::Instant;
        let s = Scratch::new();
        // A corpus with some bulk, closer to a real global set than the
        // three-template fixture.
        let mut base = Tera::default();
        for i in 0..60 {
            base.add_raw_template(&format!("bulk/t{i}.html"), "{% block b %}{% endblock %}x")
                .unwrap();
        }
        base.add_raw_template("base.html", "B[{% block body %}{% endblock %}]").unwrap();
        base.build_inheritance_chains().unwrap();
        let base = Arc::new(base);

        for n in [1usize, 5, 20] {
            let slug = format!("t{n}");
            for i in 0..n {
                s.write(&format!("{slug}/f{i}.html"), "{% extends \"base.html\" %}{% block body %}y{% endblock %}");
            }
            let tt = TenantTemplates::new(Arc::clone(&base), &s.0);

            let t0 = Instant::now();
            let _ = tt.for_tenant(&slug);
            let cold = t0.elapsed();

            let t1 = Instant::now();
            for _ in 0..10_000 { let _ = tt.for_tenant(&slug); }
            let hit = t1.elapsed() / 10_000;

            let dir = tt.dir_for(&slug).unwrap();
            let t2 = Instant::now();
            for _ in 0..1_000 { let _ = fingerprint(&dir); }
            let fp = t2.elapsed() / 1_000;

            println!(
                "  {n:>2} override(s): cold build {:>8.3?} | cached hit {:>8.3?} | fingerprint {:>8.3?}",
                cold, hit, fp
            );
        }

        let tt = TenantTemplates::new(Arc::clone(&base), &s.0);
        let t = Instant::now();
        for _ in 0..10_000 { let _ = tt.for_tenant("no-such-tenant"); }
        println!("   no overrides: {:>8.3?} per call (returns the global Arc)", t.elapsed() / 10_000);
    }

    // ---- cross-tenant isolation ---------------------------------------

    /// Two tenants under one root, with a symlink planted in the first.
    fn two_tenants(s: &Scratch) -> TenantTemplates {
        s.write("_global/base.html", "GLOBAL-BASE[{% block body %}{% endblock %}]");
        s.write("t/acme/own.html", "acme-own");
        s.write("t/other/secret.html", "OTHER-TENANT-SECRET");
        std::os::unix::fs::symlink(
            s.0.join("t/other/secret.html"),
            s.0.join("t/acme/peek.html"),
        )
        .expect("symlink");
        TenantTemplates::new(global(), s.0.join("t")).with_global_dir(s.0.join("_global"))
    }

    #[test]
    fn a_symlink_out_of_the_tenant_directory_cannot_be_read() {
        // Demonstrated against a running server before this guard
        // existed: `safe_name` is lexical, so a link whose *name* is
        // ordinary sailed straight through and leaked another tenant's
        // template.
        let s = Scratch::new();
        let tt = two_tenants(&s);
        assert!(tt.read_override("acme", "peek.html").is_none(), "must not read through the link");
        assert_eq!(tt.read_override("acme", "own.html").as_deref(), Some("acme-own"));
    }

    #[test]
    fn a_symlink_out_of_the_tenant_directory_cannot_be_written_through() {
        // The worse half: the same link allowed an *overwrite* of the
        // other tenant's file.
        let s = Scratch::new();
        let tt = two_tenants(&s);
        let err = tt
            .write_override("acme", "peek.html", "OVERWRITTEN")
            .expect_err("must refuse");
        assert!(matches!(err, EditError::BadName(_)), "{err}");
        assert_eq!(
            std::fs::read_to_string(s.0.join("t/other/secret.html")).unwrap(),
            "OTHER-TENANT-SECRET",
            "the other tenant's file must be untouched",
        );
    }

    #[test]
    fn a_symlink_out_of_the_tenant_directory_cannot_be_deleted_through() {
        let s = Scratch::new();
        let tt = two_tenants(&s);
        assert!(tt.delete_override("acme", "peek.html").is_err());
        assert!(s.0.join("t/other/secret.html").exists(), "target must survive");
    }

    #[test]
    fn an_escaping_symlink_is_neither_listed_nor_rendered() {
        // Guarding the writes alone would close the door and leave a
        // window: the link would still be picked up by the walk, so the
        // tenant would *render* another tenant's template.
        let s = Scratch::new();
        let tt = two_tenants(&s);
        let names: Vec<String> = tt.list_for("acme").into_iter().map(|e| e.name).collect();
        assert!(names.iter().any(|n| n == "own.html"), "{names:?}");
        assert!(!names.iter().any(|n| n == "peek.html"), "escaping link listed: {names:?}");

        let rendered: Vec<String> = tt
            .for_tenant("acme")
            .get_template_names()
            .map(str::to_owned)
            .collect();
        assert!(!rendered.iter().any(|n| n == "peek.html"), "escaping link loaded: {rendered:?}");
    }

    #[test]
    fn a_symlink_that_stays_inside_the_tenant_is_still_fine() {
        // The rule is containment, not "no symlinks" — a link within the
        // tenant's own tree is harmless and refusing it would be
        // surprising.
        let s = Scratch::new();
        let tt = two_tenants(&s);
        std::os::unix::fs::symlink(s.0.join("t/acme/own.html"), s.0.join("t/acme/alias.html"))
            .expect("symlink");
        assert_eq!(tt.read_override("acme", "alias.html").as_deref(), Some("acme-own"));
    }

    #[test]
    fn one_tenant_cannot_reach_another_by_name_alone() {
        // The lexical guard, kept honest alongside the resolved one.
        let s = Scratch::new();
        let tt = two_tenants(&s);
        for name in ["../other/secret.html", "/../other/secret.html", "..%2Fother.html"] {
            assert!(
                tt.read_override("acme", name).is_none(),
                "{name:?} must not resolve",
            );
        }
    }

    #[test]
    fn the_cache_is_bounded_and_evicts_the_least_recently_used() {
        let s = Scratch::new();
        for i in 0..4 {
            s.write(&format!("t{i}/page.html"), &format!("tenant-{i}"));
        }
        let tt = TenantTemplates::new(global(), &s.0)
            .with_reload(Reload::Never)
            .with_capacity(2);
        let _ = tt.for_tenant("t0");
        let _ = tt.for_tenant("t1");
        let _ = tt.for_tenant("t0"); // t0 is now the more recently used
        let _ = tt.for_tenant("t2"); // evicts t1

        let cache = tt.cache.read().unwrap();
        assert_eq!(cache.len(), 2, "cache must stay bounded");
        assert!(cache.contains_key("t0"), "recently used survives");
        assert!(!cache.contains_key("t1"), "least recently used evicted");
    }

    #[test]
    fn an_evicted_tenant_still_renders_correctly() {
        // Eviction is a cost, never a correctness change.
        let s = Scratch::new();
        s.write("t0/page.html", "zero");
        s.write("t1/page.html", "one");
        let tt = TenantTemplates::new(global(), &s.0).with_capacity(1);
        assert_eq!(render(&tt.for_tenant("t0"), "page.html"), "zero");
        assert_eq!(render(&tt.for_tenant("t1"), "page.html"), "one");
        assert_eq!(render(&tt.for_tenant("t0"), "page.html"), "zero");
    }

    /// A successor tenant on a reused slug never renders the
    /// previous owner's overrides.
    #[test]
    fn a_reused_slug_does_not_inherit_the_old_overrides() {
        let s = Scratch::new();
        s.write("acme/page.html", "OLD-CUSTOMER");
        let tt = TenantTemplates::new(global(), &s.0);

        assert_eq!(tt.claim("acme", "db-born-1").expect("claim"), Claim::Adopted);
        assert_eq!(tt.claim("acme", "db-born-1").expect("claim"), Claim::Owned);
        assert_eq!(render(&tt.for_tenant("acme"), "page.html"), "OLD-CUSTOMER");

        // Purged and re-provisioned: same slug, new database.
        match tt.claim("acme", "db-born-2").expect("claim") {
            Claim::MovedAside { previous_owner, to } => {
                assert_eq!(previous_owner, "db-born-1");
                assert!(to.join("page.html").is_file(), "kept, not deleted");
            }
            other => panic!("expected the old directory moved aside, got {other:?}"),
        }
        assert_eq!(
            render(&tt.for_tenant("acme"), "page.html"),
            "GLOBAL-BASE[global-page]",
            "the cached old instance is gone too"
        );
        assert_eq!(tt.claim("acme", "db-born-2").expect("claim"), Claim::NoOverrides);
        assert!(tt.tenant_dir(".orphaned").is_none(), "the aside directory is no tenant's");
    }

    #[test]
    fn the_owner_marker_is_not_a_template() {
        let s = Scratch::new();
        s.write("acme/page.html", "MINE");
        let tt = TenantTemplates::new(global(), &s.0);
        tt.claim("acme", "db").expect("claim");
        assert_eq!(render(&tt.for_tenant("acme"), "page.html"), "MINE");
        assert!(tt.for_tenant("acme").get_template(OWNER_FILE).is_err());
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn database_birth_is_the_first_applied_migration() {
        let pool = rustango::sql::Pool::connect("sqlite::memory:").await.expect("pool");
        rustango::sql::raw_execute_pool(
            &pool,
            "CREATE TABLE __rustango_migrations__ (name TEXT PRIMARY KEY, applied_at TEXT NOT NULL)",
            Vec::new(),
        )
        .await
        .expect("ledger");
        assert_eq!(database_birth(&pool).await.expect("empty"), None);
        for (name, at) in [("0002_b", "2026-02-01T00:00:00+00:00"), ("0001_a", "2026-01-01T00:00:00+00:00")] {
            rustango::sql::raw_execute_pool(
                &pool,
                &format!("INSERT INTO __rustango_migrations__ VALUES ('{name}', '{at}')"),
                Vec::new(),
            )
            .await
            .expect("row");
        }
        assert_eq!(
            database_birth(&pool).await.expect("birth").as_deref(),
            Some("2026-01-01T00:00:00+00:00")
        );
    }

    /// The async render path takes a cached instance without
    /// touching the disk, and falls back to `for_tenant` when one is due.
    #[test]
    fn cached_serves_only_an_instance_not_due_a_check() {
        let s = Scratch::new();
        s.write("acme/page.html", "MINE");
        let never = TenantTemplates::new(global(), &s.0).with_reload(Reload::Never);
        assert!(never.cached("acme").is_none(), "nothing built yet");
        let built = never.for_tenant("acme");
        let hit = never.cached("acme").expect("cached after a build");
        assert!(Arc::ptr_eq(&built, &hit));

        let always = TenantTemplates::new(global(), &s.0).with_reload(Reload::Always);
        let _ = always.for_tenant("acme");
        assert!(always.cached("acme").is_none(), "always due: re-check on the blocking pool");
    }

    /// A symlink out of the tenant's folder can't be used to create
    /// directories outside it, not just files.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_subdirectory_creates_nothing_outside() {
        let s = Scratch::new();
        let outside = Scratch::new();
        std::fs::create_dir_all(s.0.join("acme")).unwrap();
        std::os::unix::fs::symlink(&outside.0, s.0.join("acme/evil")).unwrap();
        let tt = TenantTemplates::new(global(), &s.0);
        assert!(tt.write_override("acme", "evil/new/x.html", "x").is_err());
        assert!(!outside.0.join("new").exists(), "no directory was created outside");
    }
}
