//! #617 — our tenant migration snapshots must record the framework's own
//! `rustango_*` tables exactly as the model registry describes them.
//!
//! Our baseline sorts after the framework's bootstrap migration, so whatever
//! it says about a `rustango_*` table is what `makemigrations` treats as the
//! current state. When that copy goes stale — because the baseline was
//! squashed against an older framework — every downstream app sees phantom
//! column changes on its first `makemigrations`, and the ones with no SQLite
//! rendering (`AlterColumnMaxLength`) produce a migration that can't be
//! applied at all.
//!
//! When this fails, refresh the snapshot rather than editing migration JSON:
//!
//! ```sh
//! cargo run --example gen_framework_snapshot_sync --no-default-features --features sqlite
//! cargo run --example gen_framework_snapshot_sync --no-default-features --features sqlite -- --write
//! ```

use rustango::core::{Model, ModelScope};
use rustango::migrate::{file, SchemaSnapshot};
use std::path::Path;

#[test]
fn latest_snapshot_records_framework_tables_as_the_registry_declares_them() {
    // Same force-links the generators use, so the inventory this test sees
    // is the inventory `makemigrations` would see.
    let _ = rustango_cms::Page::SCHEMA;
    let _ = rustango_cms::PageType::SCHEMA;

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let prior = file::list_dir(&dir).expect("list ./migrations");
    let last = prior.last().expect("at least one migration");
    let ours = &last.snapshot;

    let system = SchemaSnapshot::from_registry_system_for_scope(ModelScope::Tenant);
    assert!(
        !system.tables.is_empty(),
        "no framework tenant tables in the registry — the force-links above stopped working"
    );

    let mut problems = Vec::new();
    for expected in &system.tables {
        match ours.tables.iter().find(|t| t.name == expected.name) {
            None => problems.push(format!("{}: absent from our snapshot", expected.name)),
            Some(recorded) => {
                for f in &expected.fields {
                    match recorded.fields.iter().find(|r| r.name == f.name) {
                        None => problems.push(format!("{}.{}: absent", expected.name, f.name)),
                        Some(r) if r != f => problems.push(format!(
                            "{}.{}: recorded {r:?} but the model declares {f:?}",
                            expected.name, f.name
                        )),
                        Some(_) => {}
                    }
                }
            }
        }
    }

    assert!(
        problems.is_empty(),
        "`{}` is out of date with the framework's models ({} problem(s)):\n  {}\n\n\
         Refresh it with the gen_framework_snapshot_sync example (see this file's docs).",
        last.name,
        problems.len(),
        problems.join("\n  ")
    );
}
