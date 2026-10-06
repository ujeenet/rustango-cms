//! One-shot generator for the #557 categories migration.
//!
//! The general `gen_migration` (full-registry diff) is blocked in this
//! repo: the framework auth tables and every post-`0004` CMS table
//! (redirect v2 columns, error_pages, forms, analytics …) were added via
//! runtime `ensure_table` / `ensure_columns`, so the live model registry
//! has drifted from the frozen `0002..0004` migration chain — a full diff
//! trips v0.3's "no AlterField" guard.
//!
//! To track the NEW category tables by migration without reconciling all
//! that drift, this builds `current` = the latest migration's snapshot
//! PLUS only the three new category tables (from the registry) and hands
//! it to the framework differ. Since everything else in `current` is
//! byte-identical to the previous snapshot, the differ emits exactly three
//! `CreateTable` ops (+ their indexes) and the framework itself writes the
//! file — no hand-authored JSON.
//!
//! Usage: `cargo run --example gen_categories_migration --no-default-features --features sqlite`

use rustango::core::Model;
use rustango::migrate::{file, make, SchemaSnapshot};
use std::path::Path;

fn main() {
    // Force-link the new models so their `ModelEntry` inventory
    // submissions survive the linker and `from_registry_for_app` sees them.
    let _ = rustango_cms::Taxonomy::SCHEMA;
    let _ = rustango_cms::Category::SCHEMA;
    let _ = rustango_cms::PageCategory::SCHEMA;

    let dir = Path::new("./migrations");
    let prior = file::list_dir(dir).expect("list ./migrations");
    let prev = prior
        .last()
        .expect("at least one prior migration")
        .snapshot
        .clone();
    let prev_name = prior.last().map(|m| m.name.clone()).unwrap_or_default();
    println!("baseline = {prev_name}");

    // Pull ONLY the three new tables (+ their indexes) from the cms-app
    // registry snapshot; everything else comes verbatim from `prev`.
    let cms = SchemaSnapshot::from_registry_for_app("cms");
    let new_names = ["cms_taxonomy", "cms_category", "cms_page_category"];

    let mut current = prev.clone();
    for t in cms
        .tables
        .into_iter()
        .filter(|t| new_names.contains(&t.name.as_str()))
    {
        println!("  + table {}", t.name);
        current.tables.push(t);
    }
    for ix in cms
        .indexes
        .into_iter()
        .filter(|ix| new_names.contains(&ix.table.as_str()))
    {
        current.indexes.push(ix);
    }
    current.tables.sort_by(|a, b| a.name.cmp(&b.name));
    current.indexes.sort_by(|a, b| a.name.cmp(&b.name));

    match make::make_migrations_from(dir, &current, Some("categories")) {
        Ok(Some(m)) => println!("wrote {}/{}.json", dir.display(), m.name),
        Ok(None) => println!("no changes — the three tables are already tracked"),
        Err(e) => {
            eprintln!("make_migrations_from failed: {e}");
            std::process::exit(1);
        }
    }
}
