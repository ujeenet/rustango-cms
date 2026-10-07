//! Page-type field builder — UI-defined body schemas.
//!
//! A **Developer** authors a page type's body structure in a visual
//! builder (fields, rows, groups, repeaters, flexible-content zones,
//! reusable components); the published schema renders in the normal page
//! editor for **editors** to fill per page. Schemas are **versioned
//! documents** in a migration-tracked table — no DDL ever runs on builder
//! save/delete; page values live in one stable JSON store and upgrade
//! lazily (mirroring `Block::version`/`migrate`).
//!
//! - [`schema`] — the authored node tree + validation
//! - [`compile`](mod@compile) — schema → `Widget`s + [`dyn_block::DynBlockSet`]
//! - [`dyn_block`] — UI-defined groups as `Block`-trait citizens
//!
//! Storage models, the builder UI, the page-editor bridge, public render,
//! UI-created types, and i18n live in the sibling modules.

pub mod choice_i18n;
pub mod compile;
pub mod db_type;
pub mod dyn_block;
pub mod model;
pub mod schema;
pub mod values;

pub use compile::{compile, BodyItem, CompiledSchema, RuleBinding, FIELD_PREFIX};
pub use db_type::{DbSchemaPageType, DEFAULT_SCHEMA_TEMPLATE};
pub use dyn_block::{resolve as resolve_block, DynBlockDef, DynBlockSet};
pub use model::{Component, PageBuilderData, PageTypeSchema};
pub use schema::{parse as parse_schema, validate as validate_schema, ComponentEntry, Document};
