//! Dev tool — regenerates the tenant-scope migration JSON from the
//! current model registry.
//!
//! Usage:
//!
//! ```sh
//! cargo run --example gen_migration -- 0001_initial
//! ```
//!
//! Pass the migration name as the only positional arg. Pass nothing
//! to let the framework auto-name (`<n>_auto.json`).
//!
//! Re-run whenever the Page or PageType structs change to capture
//! the schema delta as a new migration file under `./migrations/`.

use rustango::core::{Model, ModelScope};
use rustango::migrate::{MigrationScope, SchemaSnapshot};
use std::path::Path;

fn main() {
    // Force-link the CMS models into the inventory. Referencing Page +
    // PageType pulls in the rest of the crate's `#[derive(Model)]`
    // registrations transitively.
    let _ = rustango_cms::Page::SCHEMA;
    let _ = rustango_cms::PageType::SCHEMA;
    // #557 — the category/taxonomy models live in their own module, not
    // reachable transitively from Page/PageType; force-link them so the
    // generator's registry snapshot includes cms_taxonomy / cms_category /
    // cms_page_category.
    let _ = rustango_cms::Taxonomy::SCHEMA;
    let _ = rustango_cms::Category::SCHEMA;
    let _ = rustango_cms::PageCategory::SCHEMA;
    // #862 — per-locale category names.
    let _ = rustango_cms::CategoryTranslation::SCHEMA;
    // Changes to a live page held for review.
    let _ = rustango_cms::pending_change::PendingChange::SCHEMA;

    // #338: also force-link the framework's tenant-scoped models so the
    // `from_registry_for_scope(Tenant)` snapshot below includes the
    // `rustango_*` tables. Without them present in `current`, the differ
    // sees every framework table as "removed" and emits DropTable for
    // users / roles / content_types / api_keys — silently dropping the
    // auth schema on a fresh tenant migrate (the 0015 corruption).
    let _ = rustango::tenancy::auth::User::SCHEMA;
    let _ = rustango::tenancy::permissions::Role::SCHEMA;
    let _ = rustango::tenancy::permissions::RolePermission::SCHEMA;
    let _ = rustango::tenancy::permissions::UserRole::SCHEMA;
    let _ = rustango::tenancy::permissions::UserPermission::SCHEMA;
    let _ = rustango::tenancy::auth_backends::ApiKey::SCHEMA;
    let _ = rustango::contenttypes::ContentType::SCHEMA;

    // Usage: gen_migration [name] [--replaces old_a,old_b,...]
    //
    // `--replaces` marks the generated file as a squash that supersedes the
    // named migrations, so it reconciles (fake-applies) against a DB that
    // already ran the old chain instead of re-running CREATE TABLE. Use it
    // when regenerating the tenant migration after deleting a prior chain.
    let mut name: Option<String> = None;
    let mut replaces: Vec<String> = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--replaces" => {
                if let Some(list) = it.next() {
                    replaces = list
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect();
                }
            }
            other if !other.starts_with("--") && name.is_none() => name = Some(other.to_owned()),
            _ => {}
        }
    }
    let dir = Path::new("./migrations");

    // Build the baseline from ALL tenant-scoped models (CMS + framework)
    // and diff via the scope-aware path. `make_migrations_scoped` also
    // folds `rustango_*` tables into the prior snapshot, so the framework
    // schema stays put across the tenant migration chain.
    let snapshot = SchemaSnapshot::from_registry_for_scope(ModelScope::Tenant);
    let result = rustango::migrate::make::make_migrations_scoped(
        dir,
        &snapshot,
        ModelScope::Tenant,
        MigrationScope::Tenant,
        name.as_deref(),
    );

    match result {
        Ok(Some(mut mig)) => {
            if !replaces.is_empty() {
                mig.replaces = replaces;
                rustango::migrate::file::write(&dir.join(format!("{}.json", mig.name)), &mig)
                    .expect("rewrite migration with replaces");
                println!("  replaces: {}", mig.replaces.join(", "));
            }
            println!("wrote {}/{}.json", dir.display(), mig.name);
        }
        Ok(None) => println!("no changes — current schema matches latest snapshot"),
        Err(e) => {
            eprintln!("make_migrations failed: {e}");
            std::process::exit(1);
        }
    }
}
