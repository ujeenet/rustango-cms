//! One-shot generator for the #559/#561 page-builder migration.
//!
//! Same targeted approach as `gen_categories_migration`: the general
//! `gen_migration` is blocked by framework/CMS snapshot drift, so this
//! builds `current` = the latest migration's snapshot PLUS only the three
//! new page-builder tables (from the registry) and hands it to the
//! framework differ — which emits exactly three `CreateTable` ops and
//! writes the file. No hand-authored JSON.
//!
//! Usage: `cargo run --example gen_page_builder_migration --no-default-features --features sqlite`

use rustango::core::Model;
use rustango::migrate::{file, make, SchemaSnapshot};
use std::path::Path;

fn main() {
    let _ = rustango_cms::page_builder::PageTypeSchema::SCHEMA;
    let _ = rustango_cms::page_builder::Component::SCHEMA;
    let _ = rustango_cms::page_builder::PageBuilderData::SCHEMA;

    let dir = Path::new("./migrations");
    let prior = file::list_dir(dir).expect("list ./migrations");
    let prev = prior
        .last()
        .expect("at least one prior migration")
        .snapshot
        .clone();
    println!(
        "baseline = {}",
        prior.last().map(|m| m.name.as_str()).unwrap_or("?")
    );

    let cms = SchemaSnapshot::from_registry_for_app("cms");
    let new_names = [
        "cms_page_type_schema",
        "cms_component",
        "cms_page_builder_data",
    ];

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

    match make::make_migrations_from(dir, &current, Some("page_builder")) {
        Ok(Some(m)) => println!("wrote {}/{}.json", dir.display(), m.name),
        Ok(None) => println!("no changes — the three tables are already tracked"),
        Err(e) => {
            eprintln!("make_migrations_from failed: {e}");
            std::process::exit(1);
        }
    }
}
