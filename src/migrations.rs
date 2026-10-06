//! Embed + materialize the bundled CMS migrations.
//!
//! `rustango-cms` ships its own per-tenant schema (cms_page,
//! cms_page_type, cms_locale, cms_media, …) as JSON migration files
//! under `migrations/`. Downstream apps need those migrations applied
//! against every active tenant, but they live inside the library
//! crate — not the user's project — so the manage CLI's
//! migration-runner has nowhere to find them by default.
//!
//! Solution: `rustango::embed_migrations!` bakes the JSON into the
//! library at compile time as `&'static [(name, json)]`. The
//! [`materialize`] helper writes any missing entries into the user
//! project's `migrations/` dir on first boot. Idempotent — existing
//! files (with matching names) are left alone, so user customizations
//! are not clobbered.
//!
//! ## Wiring
//!
//! ```ignore
//! use rustango::manage::Cli;
//!
//! #[rustango::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let migrations_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
//!         .join("migrations");
//!     // Materialize CMS-bundled migrations before the runner reads
//!     // the directory. Safe to call on every boot.
//!     rustango_cms::migrations::materialize(&migrations_dir)?;
//!
//!     Cli::new()
//!         .tenancy()
//!         .migrations_dir(migrations_dir)
//!         /* … */
//!         .run().await
//! }
//! ```

use std::path::Path;

/// The CMS migrations embedded at compile time. One entry per JSON
/// file in `rustango-cms/migrations/`. The macro also performs static
/// chain validation — a broken `prev` reference fails the build
/// rather than the runner.
///
/// Order is lex-sorted by file stem to match the on-disk apply order.
pub const MIGRATIONS: &[(&str, &str)] = rustango::embed_migrations!("migrations");

/// Write every embedded CMS migration into `target_dir` if a file
/// with the matching name doesn't already exist. Idempotent — safe
/// to call on every boot. Existing files are not overwritten so user
/// edits / squashed-migration overrides survive.
///
/// # Errors
/// I/O errors creating the directory or writing files.
///
/// # Example
///
/// ```no_run
/// let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
/// rustango_cms::migrations::materialize(&dir).expect("materialize CMS migrations");
/// ```
pub fn materialize(target_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target_dir)?;
    let mut wrote = 0usize;
    for (name, json) in MIGRATIONS {
        let path = target_dir.join(format!("{name}.json"));
        if path.exists() {
            continue;
        }
        std::fs::write(&path, json)?;
        wrote += 1;
    }
    if wrote > 0 {
        tracing::info!(
            target: "rustango_cms::migrations",
            dir = %target_dir.display(),
            count = wrote,
            "materialized bundled CMS migrations",
        );
    }
    Ok(())
}
