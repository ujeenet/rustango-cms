//! Categories & taxonomies — hierarchical categorization, built
//! the **same way pages are**: real migration-tracked models, not a JSON
//! blob or a config file.
//!
//! - [`model::Category`] (`cms_category`) is the abstract base row — a
//!   materialized-path tree with typed columns (name, description,
//!   featured image, thumbnail, parent). Every vocabulary shares it.
//! - [`model::Taxonomy`] (`cms_taxonomy`) is the vocabulary registry,
//!   mirroring `cms_page_type`: one row per vocabulary, **synced from the
//!   [`TaxonomyHandler`] inventory at seed** ([`upsert_taxonomy`]). The DB
//!   rows are the runtime source of truth ([`taxonomies`]).
//! - [`assign::PageCategory`] (`cms_page_category`) files pages under
//!   categories.
//!
//! A **custom vocabulary** is registered in code exactly like a custom
//! page type: implement [`TaxonomyHandler`] + `register_taxonomy!(T)`.
//! Extra typed fields (a country's flag + ISO code) live in the handler's
//! own per-type **extension table** (unique `category_id` FK) — the
//! `error_pages`/page-type extension pattern. The built-in `category`
//! vocabulary ships with only the base columns.
//!
//! The tables are created by a **migration** (`0005_categories.json`,
//! generated via `cargo run --example gen_migration`), not runtime
//! `ensure_table` — so they are tracked exactly like `cms_page`.

pub mod assign;
pub mod model;

pub use assign::PageCategory;
pub use model::{Category, CategoryError, CategoryNode, NewCategory, Taxonomy, DEFAULT_MAX_DEPTH};

use rustango::sql::Pool;

/// A code-registered vocabulary. Mirrors
/// [`PageTypeHandler`](crate::page_type::PageTypeHandler): process-global,
/// zero-arg constructible via the [`register_taxonomy!`](crate::register_taxonomy)
/// factory. The definition is synced into a `cms_taxonomy` row at seed;
/// the optional extension hooks back a custom typed-field table.
#[async_trait::async_trait]
pub trait TaxonomyHandler: Send + Sync + 'static {
    /// Stable identifier — the URL/registry key, unique across vocabularies.
    fn slug(&self) -> &'static str;
    /// Human label for the admin nav + headings.
    fn verbose_name(&self) -> &'static str;
    /// `true` allows nested categories; `false` rejects any non-NULL parent.
    fn hierarchical(&self) -> bool {
        true
    }
    /// Nesting cap (create/move enforced).
    fn max_depth(&self) -> i32 {
        DEFAULT_MAX_DEPTH
    }
    /// Material Symbols icon for the nav entry.
    fn icon(&self) -> Option<&'static str> {
        None
    }
    fn description(&self) -> Option<&'static str> {
        None
    }
    /// Per-type extension table name (typed extra fields), or `None` for a
    /// base-columns-only vocabulary like the built-in `category`.
    fn ext_table(&self) -> Option<&'static str> {
        None
    }
    /// Extra field widgets for the category editor (custom vocabularies),
    /// prefilled from the extension row. Rendered through the shared
    /// `_widget.html` macro alongside the base fields.
    async fn ext_widgets(
        &self,
        _pool: &Pool,
        _category_id: i64,
    ) -> Result<Vec<crate::widget::Widget>, rustango::sql::ExecError> {
        Ok(Vec::new())
    }
    /// Persist the extension row from the posted form (custom vocabularies).
    async fn save_ext(
        &self,
        _pool: &Pool,
        _category_id: i64,
        _form: &std::collections::HashMap<String, String>,
    ) -> Result<(), rustango::sql::ExecError> {
        Ok(())
    }
}

/// Inventory registration for a [`TaxonomyHandler`] (mirrors
/// `PageTypeHandlerRegistration`). Populated by
/// [`register_taxonomy!`](crate::register_taxonomy).
pub struct TaxonomyHandlerRegistration {
    pub factory: fn() -> Box<dyn TaxonomyHandler>,
}

inventory::collect!(TaxonomyHandlerRegistration);

/// Every code-registered vocabulary handler in the binary.
pub fn registered_handlers() -> impl Iterator<Item = Box<dyn TaxonomyHandler>> {
    inventory::iter::<TaxonomyHandlerRegistration>
        .into_iter()
        .map(|r| (r.factory)())
}

/// The code handler for `slug` (for the extension hooks).
#[must_use]
pub fn find_handler(slug: &str) -> Option<Box<dyn TaxonomyHandler>> {
    registered_handlers().find(|h| h.slug() == slug)
}

/// Sync a `cms_taxonomy` row from a registered handler — find-or-create
/// keyed on the natural key `slug`, mirroring `upsert_page_type`
/// (`seed.rs`). Called per-tenant at seed.
///
/// # Errors
/// Propagates query / write failures.
pub async fn upsert_taxonomy(
    pool: &Pool,
    handler: &dyn TaxonomyHandler,
) -> Result<(), rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::{Auto, FetcherPool as _};

    let mut existing: Vec<Taxonomy> = Taxonomy::objects()
        .where_(Taxonomy::slug.eq(handler.slug().to_owned()))
        .fetch(pool)
        .await?;
    if let Some(mut row) = existing.pop() {
        row.verbose_name = handler.verbose_name().to_owned();
        row.hierarchical = handler.hierarchical();
        row.max_depth = handler.max_depth().max(1);
        row.icon = handler.icon().unwrap_or_default().to_owned();
        row.description = handler.description().unwrap_or_default().to_owned();
        row.ext_table = handler.ext_table().unwrap_or_default().to_owned();
        row.save_pool(pool).await?;
    } else {
        let mut row = Taxonomy {
            id: Auto::Unset,
            slug: handler.slug().to_owned(),
            verbose_name: handler.verbose_name().to_owned(),
            hierarchical: handler.hierarchical(),
            max_depth: handler.max_depth().max(1),
            icon: handler.icon().unwrap_or_default().to_owned(),
            description: handler.description().unwrap_or_default().to_owned(),
            ext_table: handler.ext_table().unwrap_or_default().to_owned(),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        row.insert_pool(pool).await?;
    }
    Ok(())
}

/// Every registered vocabulary, from the DB (the runtime source of truth,
/// seeded from handlers), ordered by slug.
///
/// # Errors
/// Propagates query failures.
pub async fn taxonomies(pool: &Pool) -> Result<Vec<Taxonomy>, rustango::sql::ExecError> {
    use rustango::sql::FetcherPool as _;
    Taxonomy::objects()
        .order_by(&[("slug", false)])
        .fetch(pool)
        .await
}

/// The `cms_taxonomy` row for `slug`, if seeded.
///
/// # Errors
/// Propagates query failures.
pub async fn find_taxonomy(
    pool: &Pool,
    slug: &str,
) -> Result<Option<Taxonomy>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    Taxonomy::objects()
        .where_(Taxonomy::slug.eq(slug.to_owned()))
        .first(pool)
        .await
}

/// The built-in hierarchical `category` vocabulary shipped with every
/// tenant. Base columns only — name, description,
/// featured image, thumbnail, parent. The flat [`page_tag`](crate::page_tag)
/// story stays the tags vocabulary.
#[derive(Default)]
pub struct CategoryTaxonomy;

impl TaxonomyHandler for CategoryTaxonomy {
    fn slug(&self) -> &'static str {
        "category"
    }
    fn verbose_name(&self) -> &'static str {
        "Categories"
    }
    fn hierarchical(&self) -> bool {
        true
    }
    fn icon(&self) -> Option<&'static str> {
        Some("category")
    }
    fn description(&self) -> Option<&'static str> {
        Some("Hierarchical content categories.")
    }
}

crate::register_taxonomy!(CategoryTaxonomy);

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use rustango::core::Model as _;
    use rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect;

    #[test]
    fn built_in_category_is_registered() {
        let found = registered_handlers().any(|h| h.slug() == "category");
        assert!(found, "built-in category vocabulary must be registered");
        let h = find_handler("category").expect("handler");
        assert!(h.hierarchical());
        assert_eq!(h.verbose_name(), "Categories");
    }

    #[tokio::test]
    async fn upsert_taxonomy_is_idempotent_find_or_create() {
        let pool = Pool::connect("sqlite::memory:").await.expect("pool");
        let ddl = create_table_if_not_exists_sql_with_dialect(pool.dialect(), &Taxonomy::SCHEMA);
        for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            rustango::sql::raw_execute_pool(&pool, stmt, Vec::new())
                .await
                .expect("ddl");
        }
        let h = CategoryTaxonomy;
        upsert_taxonomy(&pool, &h).await.expect("first");
        upsert_taxonomy(&pool, &h).await.expect("second");
        let all = taxonomies(&pool).await.expect("list");
        assert_eq!(
            all.iter().filter(|t| t.slug == "category").count(),
            1,
            "one row, not duplicated"
        );
        let cat = find_taxonomy(&pool, "category")
            .await
            .expect("q")
            .expect("row");
        assert!(cat.hierarchical);
        assert_eq!(cat.verbose_name, "Categories");
    }
}
