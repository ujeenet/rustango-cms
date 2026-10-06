//! Dev tool — refreshes the **framework** table metadata recorded in our
//! tenant migration snapshots (#617).
//!
//! `rustango_*` tables belong to the framework. Its own bootstrap migration
//! (the app's `system/migrations/`) creates them and snapshots them
//! correctly. Our tenant baseline snapshots them too — but the baseline was
//! squashed against an older framework, so its copy of that metadata goes
//! stale whenever the framework's models change.
//!
//! Because our baseline sorts last, its stale copy is what `makemigrations`
//! treats as the current state. Every downstream app then sees phantom
//! column changes on its very first `makemigrations`, and some of them
//! (`AlterColumnMaxLength`) have no SQLite rendering at all — so the
//! generated migration can't be applied.
//!
//! Only the *recorded* state is wrong: the columns in the database were
//! created by the framework's bootstrap migration, which had the right
//! metadata all along. So this writes a **snapshot-only** migration —
//! corrected snapshot, empty `forward`. Nothing to run, nothing to undo.
//!
//! Usage:
//!
//! ```sh
//! cargo run --example gen_framework_snapshot_sync --no-default-features --features sqlite
//! cargo run --example gen_framework_snapshot_sync --no-default-features --features sqlite -- --write
//! ```
//!
//! Without `--write` it only reports the drift. Re-run after a framework
//! bump; a clean run prints "no drift" and writes nothing.

use rustango::core::{Model, ModelScope};
use rustango::migrate::{file, SchemaSnapshot};
use std::path::Path;

fn main() {
    // Force-link the CMS models so the inventory is populated the same way
    // it is for the other generators in this directory. The framework's own
    // `rustango_*` models register unconditionally, but pulling the CMS
    // registrations in keeps this tool's view of the registry identical to
    // `gen_migration`'s.
    let _ = rustango_cms::Page::SCHEMA;
    let _ = rustango_cms::PageType::SCHEMA;

    let write = std::env::args().any(|a| a == "--write");

    let dir = Path::new("./migrations");
    let prior = file::list_dir(dir).expect("list ./migrations");
    let last = prior.last().expect("at least one prior migration").clone();
    println!("baseline = {}", last.name);

    // The authority for framework tables: exactly the set the framework's
    // bootstrap migration creates in a tenant database.
    let system = SchemaSnapshot::from_registry_system_for_scope(ModelScope::Tenant);

    let mut current = last.snapshot.clone();
    let mut drift = 0usize;

    for table in &system.tables {
        match current.tables.iter_mut().find(|t| t.name == table.name) {
            Some(stale) => {
                if stale != table {
                    report_table_drift(stale, table);
                    drift += 1;
                    *stale = table.clone();
                }
            }
            None => {
                println!("  + {} (absent from our snapshot entirely)", table.name);
                drift += 1;
                current.tables.push(table.clone());
            }
        }
    }

    // Indexes are keyed by name, so replace the framework's wholesale rather
    // than trying to pair them up field by field.
    let fw_tables: Vec<&str> = system.tables.iter().map(|t| t.name.as_str()).collect();
    let ours: Vec<_> = current
        .indexes
        .iter()
        .filter(|ix| fw_tables.contains(&ix.table.as_str()))
        .cloned()
        .collect();
    if ours != system.indexes {
        println!(
            "  ~ indexes on framework tables: {} -> {}",
            ours.len(),
            system.indexes.len()
        );
        drift += 1;
        current
            .indexes
            .retain(|ix| !fw_tables.contains(&ix.table.as_str()));
        current.indexes.extend(system.indexes.iter().cloned());
    }

    if drift == 0 {
        println!("no drift — framework tables in our snapshot match the registry");
        return;
    }

    current.tables.sort_by(|a, b| a.name.cmp(&b.name));
    current.indexes.sort_by(|a, b| a.name.cmp(&b.name));

    if !write {
        println!("\n{drift} drifted table(s). Re-run with --write to record the correction.");
        return;
    }

    let index = file::extract_index(&last.name).unwrap_or(0) + 1;
    let name = format!("{index:04}_framework_snapshot_sync");
    let migration = file::Migration {
        name: name.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        prev: Some(last.name.clone()),
        atomic: true,
        scope: file::MigrationScope::Tenant,
        replaces: Vec::new(),
        snapshot: current,
        // Deliberately empty: the database already matches. See the module
        // docs — this migration corrects bookkeeping, not schema.
        forward: Vec::new(),
    };
    let path = dir.join(format!("{name}.json"));
    file::write(&path, &migration).expect("write migration");
    println!("\nwrote {}", path.display());
}

/// Print one line per *attribute* that differs, rather than dumping two whole
/// `FieldSnapshot`s and leaving the reader to spot the change. Serializing to
/// JSON keeps this generic: it names whatever attribute drifted, including
/// ones added to `FieldSnapshot` after this tool was written.
fn report_table_drift(
    stale: &rustango::migrate::TableSnapshot,
    fresh: &rustango::migrate::TableSnapshot,
) {
    for f in &fresh.fields {
        match stale.fields.iter().find(|s| s.name == f.name) {
            Some(s) if s != f => {
                for (attr, was, now) in attr_diff(s, f) {
                    println!("  ~ {}.{}.{attr}: {was} -> {now}", fresh.name, f.name);
                }
            }
            Some(_) => {}
            None => println!("  + {}.{} (new column)", fresh.name, f.name),
        }
    }
    for s in &stale.fields {
        if !fresh.fields.iter().any(|f| f.name == s.name) {
            println!(
                "  - {}.{} (column no longer in the model)",
                stale.name, s.name
            );
        }
    }
}

fn attr_diff(
    stale: &rustango::migrate::FieldSnapshot,
    fresh: &rustango::migrate::FieldSnapshot,
) -> Vec<(String, String, String)> {
    let (a, b) = (to_map(stale), to_map(fresh));
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| {
            let show = |m: &serde_json::Map<String, serde_json::Value>| {
                m.get(k)
                    .map_or_else(|| "unset".to_owned(), ToString::to_string)
            };
            (k.clone(), show(&a), show(&b))
        })
        .collect()
}

fn to_map(f: &rustango::migrate::FieldSnapshot) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::to_value(f) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    }
}
