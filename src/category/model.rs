//! Category models — real, migration-tracked tables (not a JSON
//! blob). Two models:
//!
//! - [`Taxonomy`] (`cms_taxonomy`) — the vocabulary registry, mirroring
//!   `cms_page_type`: one row per registered vocabulary (`category`,
//!   `countries`, …), synced from handlers at seed.
//! - [`Category`] (`cms_category`) — the abstract base row shared by every
//!   vocabulary: a materialized-path tree (like `cms_page`) with typed
//!   columns `name` / `description` / `featured_image_id` / `thumbnail_id`
//!   / `parent_id`, scoped to a taxonomy via `taxonomy_id`.
//!
//! Custom vocabularies add extra typed fields in their own per-type
//! extension table (unique `category_id` FK), exactly like a page type's
//! extension — see [`crate::category`].
//!
//! Parenting is cycle-guarded inside the move transaction (materialized-
//! path prefix test) and [`build_tree`] is loop-safe (visited-set + depth
//! cap), so a corrupted `parent_id` can never hang the process.

use chrono::{DateTime, Utc};
use rustango::core::Column as _;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::Model;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::tree::{depth_of, descendants_like, MaterializedPath};

/// Default per-taxonomy nesting cap when a handler doesn't set one.
/// Also the backstop that limits damage from a corrupted tree.
pub const DEFAULT_MAX_DEPTH: i32 = 10;

/// Hard ceiling for any tree walk regardless of a taxonomy's configured
/// `max_depth` — a last-resort backstop against a corrupted DB cycle.
const WALK_DEPTH_CAP: usize = 64;

/// A registered vocabulary. One row per taxonomy, synced from the
/// [`TaxonomyHandler`](crate::category::TaxonomyHandler) inventory at seed
/// (`upsert_taxonomy`). Mirrors `cms_page_type`.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_taxonomy",
    app = "cms",
    display = "verbose_name",
    admin(
        list_display = "slug, verbose_name, hierarchical, max_depth",
        search_fields = "slug, verbose_name",
        ordering = "slug",
    )
)]
pub struct Taxonomy {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Stable identifier — the URL/registry key, unique across vocabularies.
    #[rustango(max_length = 100, unique)]
    pub slug: String,

    /// Human label for the admin nav + headings.
    #[rustango(max_length = 200)]
    pub verbose_name: String,

    /// `true` allows nested categories; `false` rejects any non-NULL parent.
    #[rustango(default = "true")]
    pub hierarchical: bool,

    /// Nesting cap (create/move enforced).
    #[rustango(default = "10")]
    pub max_depth: i32,

    /// Material Symbols icon name for the nav entry.
    #[rustango(max_length = 80, default = "''")]
    pub icon: String,

    #[rustango(max_length = 255, default = "''")]
    pub description: String,

    /// Per-type extension table name for typed extra fields, `''` when the
    /// vocabulary uses only the base [`Category`] columns. Analog of
    /// `LibraryType.extension_table`.
    #[rustango(max_length = 120, default = "''")]
    pub ext_table: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

/// The abstract base category row — shared by every vocabulary, one row
/// per term. Tree shape (`path`/`depth`/`parent_id`) mirrors
/// [`crate::page::Page`]; extra per-vocabulary fields live in a per-type
/// extension table keyed on `id`.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_category",
    app = "cms",
    display = "name",
    admin(
        list_display = "taxonomy_id, name, slug, depth, sort_order",
        search_fields = "name, slug",
        ordering = "taxonomy_id, path",
        list_filter = "taxonomy_id",
    )
)]
pub struct Category {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Owning vocabulary — the discriminator (mirrors `page.page_type_id`).
    #[rustango(fk = "cms_taxonomy", on = "id", index)]
    pub taxonomy_id: i64,

    /// Human-readable name (translatable via `cms_translation`).
    #[rustango(max_length = 200)]
    pub name: String,

    /// URL/identifier slug, unique per (taxonomy, parent).
    #[rustango(max_length = 200, index)]
    pub slug: String,

    /// Short blurb shown in listings / on the term page.
    #[rustango(max_length = 1024, default = "''")]
    pub description: String,

    /// Featured image — FK into `cms_media`.
    #[rustango(fk = "cms_media", on = "id")]
    pub featured_image_id: Option<i64>,

    /// Thumbnail — FK into `cms_media`.
    #[rustango(fk = "cms_media", on = "id")]
    pub thumbnail_id: Option<i64>,

    /// Tree parent — None for a root category. Siblings share this.
    #[rustango(fk = "cms_category", on = "id", index)]
    pub parent_id: Option<i64>,

    /// Materialized path of zero-padded segment ids, trailing slash
    /// (e.g. `0001/0003/`). Indexed for cheap subtree prefix queries.
    #[rustango(max_length = 510, index)]
    pub path: String,

    /// Cached `/`-separated segment count (root = 1).
    pub depth: i32,

    pub sort_order: i32,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

/// Errors from category tree mutations. `CycleAttempt` mirrors
/// [`crate::tree_ops::TreeError::CycleAttempt`].
#[derive(Debug, thiserror::Error)]
pub enum CategoryError {
    #[error(transparent)]
    Db(#[from] rustango::sql::ExecError),
    #[error(transparent)]
    Sqlx(#[from] rustango::sql::sqlx::Error),
    #[error("category {0} not found")]
    NotFound(i64),
    #[error("unknown taxonomy id {0} — is it registered/seeded?")]
    UnknownTaxonomy(i64),
    #[error("cannot move a category beneath itself or its own descendant")]
    CycleAttempt,
    #[error("taxonomy `{0}` is flat — categories cannot have a parent")]
    FlatNoParent(String),
    #[error("max depth {0} exceeded for taxonomy `{1}`")]
    MaxDepthExceeded(i32, String),
    #[error("a sibling category already uses slug `{0}`")]
    DuplicateSlug(String),
    #[error("category {0} still has {1} child categor(ies) — reparent or delete them first")]
    HasChildren(i64, usize),
    #[error("category path segment overflow (id out of range)")]
    PathOverflow,
}

impl From<crate::tree::PathError> for CategoryError {
    fn from(_: crate::tree::PathError) -> Self {
        CategoryError::PathOverflow
    }
}

/// A nested category for tree rendering — a row plus its (loop-safe)
/// children.
#[derive(Debug, Clone, Serialize)]
pub struct CategoryNode {
    #[serde(flatten)]
    pub category: Category,
    pub children: Vec<CategoryNode>,
}

/// Normalize a category slug like a page slug: lowercase, spaces/
/// underscores → hyphens, strip anything but `[a-z0-9-]`, collapse
/// repeats. Empty input falls back to `category`.
#[must_use]
pub fn normalize_slug(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for ch in raw.trim().chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if matches!(c, ' ' | '-' | '_') && !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "category".to_owned()
    } else {
        out
    }
}

/// Fetch a taxonomy row by id.
async fn taxonomy(pool: &Pool, taxonomy_id: i64) -> Result<Taxonomy, CategoryError> {
    Taxonomy::objects()
        .where_(Taxonomy::id.eq(taxonomy_id))
        .first(pool)
        .await?
        .ok_or(CategoryError::UnknownTaxonomy(taxonomy_id))
}

/// Fetch a category by id, scoped to its taxonomy.
async fn fetch(pool: &Pool, taxonomy_id: i64, id: i64) -> Result<Option<Category>, CategoryError> {
    Ok(Category::objects()
        .where_(Category::id.eq(id))
        .where_(Category::taxonomy_id.eq(taxonomy_id))
        .first(pool)
        .await?)
}

/// Immediate children of `parent` (None = roots) within a taxonomy,
/// ordered by `sort_order` then `id`.
///
/// # Errors
/// Propagates query failures.
pub async fn children_of(
    pool: &Pool,
    taxonomy_id: i64,
    parent: Option<i64>,
) -> Result<Vec<Category>, CategoryError> {
    let qs = Category::objects()
        .where_(Category::taxonomy_id.eq(taxonomy_id))
        .order_by(&[("sort_order", false), ("id", false)]);
    let rows = match parent {
        Some(pid) => qs.where_(Category::parent_id.eq(pid)).fetch(pool).await?,
        None => qs.where_(Category::parent_id.is_null()).fetch(pool).await?,
    };
    Ok(rows)
}

/// Every category in a taxonomy, ordered by `path` (tree pre-order).
///
/// # Errors
/// Propagates query failures.
pub async fn all_in_taxonomy(
    pool: &Pool,
    taxonomy_id: i64,
) -> Result<Vec<Category>, CategoryError> {
    Ok(Category::objects()
        .where_(Category::taxonomy_id.eq(taxonomy_id))
        .order_by(&[("path", false)])
        .fetch(pool)
        .await?)
}

/// True if a sibling (same taxonomy + parent) already uses `slug`,
/// optionally excluding `exclude_id` (for edits).
async fn slug_taken(
    pool: &Pool,
    taxonomy_id: i64,
    parent: Option<i64>,
    slug: &str,
    exclude_id: Option<i64>,
) -> Result<bool, CategoryError> {
    let siblings = children_of(pool, taxonomy_id, parent).await?;
    Ok(siblings
        .iter()
        .any(|c| c.slug == slug && c.id.get().copied() != exclude_id))
}

/// A new category's base fields (extension fields are saved separately by
/// the handler, keyed on the returned id).
#[derive(Debug, Clone, Default)]
pub struct NewCategory {
    pub name: String,
    pub slug: String,
    pub description: String,
    pub featured_image_id: Option<i64>,
    pub thumbnail_id: Option<i64>,
    pub parent_id: Option<i64>,
}

/// Create a category. Validates parent (exists, same taxonomy, allowed by
/// `hierarchical`, within `max_depth`) and slug-uniqueness, then inserts
/// and materializes its `path` in one transaction.
///
/// # Errors
/// [`CategoryError`] for unknown taxonomy, flat-with-parent, depth
/// overflow, duplicate slug, missing parent, or DB failure.
pub async fn create(
    pool: &Pool,
    taxonomy_id: i64,
    new: NewCategory,
) -> Result<Category, CategoryError> {
    let tax = taxonomy(pool, taxonomy_id).await?;
    let base = if new.slug.trim().is_empty() {
        &new.name
    } else {
        &new.slug
    };
    let slug = normalize_slug(base);

    let (parent_path, parent_depth) = match new.parent_id {
        None => (None, 0),
        Some(pid) => {
            if !tax.hierarchical {
                return Err(CategoryError::FlatNoParent(tax.slug.clone()));
            }
            let parent = fetch(pool, taxonomy_id, pid)
                .await?
                .ok_or(CategoryError::NotFound(pid))?;
            (Some(parent.path.clone()), parent.depth)
        }
    };
    let new_depth = parent_depth + 1;
    if new_depth > tax.max_depth.max(1) {
        return Err(CategoryError::MaxDepthExceeded(
            tax.max_depth,
            tax.slug.clone(),
        ));
    }
    if slug_taken(pool, taxonomy_id, new.parent_id, &slug, None).await? {
        return Err(CategoryError::DuplicateSlug(slug));
    }
    let sort_order = children_of(pool, taxonomy_id, new.parent_id).await?.len() as i32;

    // Insert then materialize the path in one transaction (mirrors
    // tree_ops::create_child). A manual tx (not the `atomic!` macro) lets
    // the `MaterializedPath` `PathError` map straight to `CategoryError`
    // via `?` — the macro is fixed to `ExecError`.
    let mut tx = rustango::sql::transaction_pool(pool).await?;
    let mut cat = Category {
        id: Auto::Unset,
        taxonomy_id,
        name: new.name,
        slug,
        description: new.description,
        featured_image_id: new.featured_image_id,
        thumbnail_id: new.thumbnail_id,
        parent_id: new.parent_id,
        path: String::new(),
        depth: new_depth,
        sort_order,
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    cat.insert_tx(&mut tx).await?;
    let id = cat.id.get().copied().unwrap_or_default();
    let path = match &parent_path {
        Some(pp) => MaterializedPath::child_of(pp, id)?,
        None => MaterializedPath::root(id)?,
    };
    cat.depth = path.depth();
    cat.path = path.into_string();
    cat.save_tx(&mut tx).await?;
    tx.commit().await?;
    Ok(cat)
}

/// Update a category's base fields in place (name/slug/description/media).
/// Re-checks slug uniqueness among siblings (excluding self). Does not
/// re-parent — use [`move_to`] for that.
///
/// # Errors
/// [`CategoryError::DuplicateSlug`], `NotFound`, or DB failure.
pub async fn update(
    pool: &Pool,
    taxonomy_id: i64,
    id: i64,
    new: NewCategory,
) -> Result<Category, CategoryError> {
    let mut cat = fetch(pool, taxonomy_id, id)
        .await?
        .ok_or(CategoryError::NotFound(id))?;
    let base = if new.slug.trim().is_empty() {
        &new.name
    } else {
        &new.slug
    };
    let slug = normalize_slug(base);
    if slug != cat.slug && slug_taken(pool, taxonomy_id, cat.parent_id, &slug, Some(id)).await? {
        return Err(CategoryError::DuplicateSlug(slug));
    }
    cat.name = new.name;
    cat.slug = slug;
    cat.description = new.description;
    cat.featured_image_id = new.featured_image_id;
    cat.thumbnail_id = new.thumbnail_id;
    cat.save_pool(pool).await?;
    Ok(cat)
}

/// Re-parent a category (and its whole subtree) under `new_parent`
/// (None = root). Cycle-guarded: the new parent may not be the category
/// itself or any of its descendants. Enforces `hierarchical` + `max_depth`
/// for the deepest moved node.
///
/// # Errors
/// [`CategoryError::CycleAttempt`], `FlatNoParent`, `MaxDepthExceeded`,
/// `NotFound`, or DB failure.
pub async fn move_to(
    pool: &Pool,
    taxonomy_id: i64,
    id: i64,
    new_parent: Option<i64>,
) -> Result<(), CategoryError> {
    let tax = taxonomy(pool, taxonomy_id).await?;
    let cat = fetch(pool, taxonomy_id, id)
        .await?
        .ok_or(CategoryError::NotFound(id))?;

    // Read the subtree BEFORE the tx (avoids single-conn SQLite deadlock).
    let mut subtree: Vec<Category> = Category::objects()
        .where_(Category::taxonomy_id.eq(taxonomy_id))
        .where_(Category::path.like(descendants_like(&cat.path)))
        .where_(Category::path.ne(cat.path.clone()))
        .order_by(&[("path", false)])
        .fetch(pool)
        .await?;

    let (new_path, new_depth) = match new_parent {
        None => (MaterializedPath::root(id)?, 1),
        Some(pid) => {
            if !tax.hierarchical {
                return Err(CategoryError::FlatNoParent(tax.slug.clone()));
            }
            let parent = fetch(pool, taxonomy_id, pid)
                .await?
                .ok_or(CategoryError::NotFound(pid))?;
            if pid == id || parent.path.starts_with(&cat.path) {
                return Err(CategoryError::CycleAttempt);
            }
            (
                MaterializedPath::child_of(&parent.path, id)?,
                parent.depth + 1,
            )
        }
    };

    let height = subtree
        .iter()
        .map(|d| d.depth)
        .max()
        .map_or(0, |m| m - cat.depth);
    if new_depth + height > tax.max_depth.max(1) {
        return Err(CategoryError::MaxDepthExceeded(
            tax.max_depth,
            tax.slug.clone(),
        ));
    }

    let old_prefix = cat.path.clone();
    let new_path = new_path.into_string();
    if new_path == old_prefix {
        return Ok(());
    }

    rustango::atomic!(pool, |tx| {
        let mut guard = tx.lock().await?;
        let tx = &mut *guard;
        let mut moved = cat.clone();
        moved.parent_id = new_parent;
        moved.path = new_path.clone();
        moved.depth = new_depth;
        moved.save_tx(tx).await?;
        for row in &mut subtree {
            if let Some(suffix) = row.path.strip_prefix(&old_prefix) {
                row.path = format!("{new_path}{suffix}");
                row.depth = depth_of(&row.path);
                row.save_tx(tx).await?;
            }
        }
        Ok::<(), rustango::sql::ExecError>(())
    })
    .await?;
    Ok(())
}

/// Delete a category. With children: `reparent = true` promotes them to
/// the category's own parent; otherwise the delete is rejected
/// ([`CategoryError::HasChildren`]). Page↔category assignments are cleaned
/// by [`crate::category::assign::delete_for_category_tx`]. Returns 1.
///
/// # Errors
/// [`CategoryError::HasChildren`] (children + not reparenting), `NotFound`,
/// or DB failure.
pub async fn delete(
    pool: &Pool,
    taxonomy_id: i64,
    id: i64,
    reparent: bool,
) -> Result<usize, CategoryError> {
    let cat = fetch(pool, taxonomy_id, id)
        .await?
        .ok_or(CategoryError::NotFound(id))?;
    let children = children_of(pool, taxonomy_id, Some(id)).await?;
    if !children.is_empty() && !reparent {
        return Err(CategoryError::HasChildren(id, children.len()));
    }
    if reparent {
        for child in &children {
            let cid = child.id.get().copied().unwrap_or_default();
            move_to(pool, taxonomy_id, cid, cat.parent_id).await?;
        }
    }
    rustango::atomic!(pool, |tx| {
        let mut guard = tx.lock().await?;
        let tx = &mut *guard;
        crate::category::assign::delete_for_category_tx(tx, id).await?;
        crate::category_translation::delete_for_category_tx(tx, id).await?;
        let row = cat.clone();
        row.delete_tx(tx).await?;
        Ok::<(), rustango::sql::ExecError>(())
    })
    .await?;
    Ok(1)
}

/// Build a nested [`CategoryNode`] tree from a flat, path-ordered row
/// list. **Loop-safe**: groups by `parent_id`, materializes generation by
/// generation with a visited-set + a hard depth cap (`WALK_DEPTH_CAP`).
/// A corrupted `parent_id` cycle yields a truncated tree + a single
/// `tracing::error!`, never an infinite loop / OOM.
#[must_use]
pub fn build_tree(rows: Vec<Category>) -> Vec<CategoryNode> {
    let mut by_parent: HashMap<Option<i64>, Vec<Category>> = HashMap::new();
    for r in rows {
        by_parent.entry(r.parent_id).or_default().push(r);
    }
    let mut visited: HashSet<i64> = HashSet::new();
    build_level(&by_parent, None, 0, &mut visited)
}

fn build_level(
    by_parent: &HashMap<Option<i64>, Vec<Category>>,
    parent: Option<i64>,
    depth: usize,
    visited: &mut HashSet<i64>,
) -> Vec<CategoryNode> {
    if depth >= WALK_DEPTH_CAP {
        tracing::error!(
            target: "rustango_cms::category",
            depth, "category tree exceeded depth cap — truncating (corrupted parent_id cycle?)"
        );
        return Vec::new();
    }
    let Some(children) = by_parent.get(&parent) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(children.len());
    for cat in children {
        let id = cat.id.get().copied().unwrap_or_default();
        if !visited.insert(id) {
            tracing::error!(
                target: "rustango_cms::category",
                category_id = id, "category already visited — skipping (cycle in parent_id)"
            );
            continue;
        }
        let kids = build_level(by_parent, Some(id), depth + 1, visited);
        out.push(CategoryNode {
            category: cat.clone(),
            children: kids,
        });
    }
    out
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use rustango::core::Model as _;
    use rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect;

    async fn mem_pool() -> Pool {
        let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
        // Create every table our FKs reference so inserts satisfy the
        // constraints (cms_media/cms_page targets), mirroring the redirect
        // tests. `cms_page` pulls in its own FK targets (type/media/theme).
        for schema in [
            &crate::page_type_model::PageType::SCHEMA,
            &crate::media::Media::SCHEMA,
            &crate::theme::Theme::SCHEMA,
            &crate::page::Page::SCHEMA,
            &Taxonomy::SCHEMA,
            &Category::SCHEMA,
            &crate::category::assign::PageCategory::SCHEMA,
            &crate::locale::Locale::SCHEMA,
            &crate::category_translation::CategoryTranslation::SCHEMA,
        ] {
            let ddl = create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
            for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                rustango::sql::raw_execute_pool(&pool, stmt, Vec::new())
                    .await
                    .expect("ddl");
            }
        }
        pool
    }

    async fn mk_taxonomy(pool: &Pool, slug: &str, hierarchical: bool, max_depth: i32) -> i64 {
        let mut t = Taxonomy {
            id: Auto::Unset,
            slug: slug.to_owned(),
            verbose_name: slug.to_owned(),
            hierarchical,
            max_depth,
            icon: String::new(),
            description: String::new(),
            ext_table: String::new(),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        t.insert_pool(pool).await.expect("insert taxonomy");
        t.id.get().copied().unwrap()
    }

    fn nc(name: &str, parent: Option<i64>) -> NewCategory {
        NewCategory {
            name: name.to_owned(),
            parent_id: parent,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn create_materializes_path_and_typed_columns() {
        let pool = mem_pool().await;
        let tax = mk_taxonomy(&pool, "category", true, 10).await;
        let mut europe_nc = nc("Europe", None);
        europe_nc.description = "The continent".to_owned();
        let europe = create(&pool, tax, europe_nc).await.expect("root");
        assert_eq!(europe.path, "0001/");
        assert_eq!(europe.depth, 1);
        assert_eq!(europe.slug, "europe");
        assert_eq!(europe.description, "The continent");
        assert_eq!(europe.featured_image_id, None);
        let france = create(&pool, tax, nc("France", europe.id.get().copied()))
            .await
            .expect("child");
        assert_eq!(france.path, "0001/0002/");
        assert_eq!(france.depth, 2);
    }

    #[tokio::test]
    async fn duplicate_sibling_slug_rejected_across_parents_ok() {
        let pool = mem_pool().await;
        let tax = mk_taxonomy(&pool, "category", true, 10).await;
        let a = create(&pool, tax, nc("A", None)).await.unwrap();
        let b = create(&pool, tax, nc("B", None)).await.unwrap();
        create(&pool, tax, nc("News", a.id.get().copied()))
            .await
            .expect("first");
        let dup = create(&pool, tax, nc("News", a.id.get().copied())).await;
        assert!(
            matches!(dup, Err(CategoryError::DuplicateSlug(_))),
            "got {dup:?}"
        );
        create(&pool, tax, nc("News", b.id.get().copied()))
            .await
            .expect("under B ok");
    }

    #[tokio::test]
    async fn move_into_own_descendant_is_cycle_rejected() {
        let pool = mem_pool().await;
        let tax = mk_taxonomy(&pool, "category", true, 10).await;
        let root = create(&pool, tax, nc("Root", None)).await.unwrap();
        let mid = create(&pool, tax, nc("Mid", root.id.get().copied()))
            .await
            .unwrap();
        let leaf = create(&pool, tax, nc("Leaf", mid.id.get().copied()))
            .await
            .unwrap();
        let res = move_to(
            &pool,
            tax,
            root.id.get().copied().unwrap(),
            leaf.id.get().copied(),
        )
        .await;
        assert!(
            matches!(res, Err(CategoryError::CycleAttempt)),
            "got {res:?}"
        );
    }

    #[tokio::test]
    async fn move_rewrites_subtree_paths() {
        let pool = mem_pool().await;
        let tax = mk_taxonomy(&pool, "category", true, 10).await;
        let a = create(&pool, tax, nc("A", None)).await.unwrap();
        let b = create(&pool, tax, nc("B", None)).await.unwrap();
        let ac = create(&pool, tax, nc("AC", a.id.get().copied()))
            .await
            .unwrap();
        move_to(
            &pool,
            tax,
            a.id.get().copied().unwrap(),
            b.id.get().copied(),
        )
        .await
        .expect("move");
        let moved_a = fetch(&pool, tax, a.id.get().copied().unwrap())
            .await
            .unwrap()
            .unwrap();
        let moved_ac = fetch(&pool, tax, ac.id.get().copied().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(moved_a.path.starts_with(&b.path));
        assert!(moved_ac.path.starts_with(&moved_a.path));
        assert_eq!(moved_ac.depth, moved_a.depth + 1);
    }

    #[tokio::test]
    async fn flat_taxonomy_rejects_parent() {
        let pool = mem_pool().await;
        let tax = mk_taxonomy(&pool, "tags", false, 1).await;
        let a = create(&pool, tax, nc("A", None)).await.unwrap();
        let res = create(&pool, tax, nc("B", a.id.get().copied())).await;
        assert!(
            matches!(res, Err(CategoryError::FlatNoParent(_))),
            "got {res:?}"
        );
    }

    #[tokio::test]
    async fn max_depth_enforced() {
        let pool = mem_pool().await;
        let tax = mk_taxonomy(&pool, "shallow", true, 2).await;
        let a = create(&pool, tax, nc("A", None)).await.unwrap();
        let b = create(&pool, tax, nc("B", a.id.get().copied()))
            .await
            .unwrap();
        let res = create(&pool, tax, nc("C", b.id.get().copied())).await;
        assert!(
            matches!(res, Err(CategoryError::MaxDepthExceeded(2, _))),
            "got {res:?}"
        );
    }

    #[tokio::test]
    async fn delete_blocks_with_children_then_reparents() {
        let pool = mem_pool().await;
        let tax = mk_taxonomy(&pool, "category", true, 10).await;
        let root = create(&pool, tax, nc("Root", None)).await.unwrap();
        let child = create(&pool, tax, nc("Child", root.id.get().copied()))
            .await
            .unwrap();
        let blocked = delete(&pool, tax, root.id.get().copied().unwrap(), false).await;
        assert!(
            matches!(blocked, Err(CategoryError::HasChildren(_, 1))),
            "got {blocked:?}"
        );
        delete(&pool, tax, root.id.get().copied().unwrap(), true)
            .await
            .expect("reparent delete");
        let promoted = fetch(&pool, tax, child.id.get().copied().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(promoted.parent_id, None);
        assert_eq!(promoted.depth, 1);
    }

    #[tokio::test]
    async fn build_tree_survives_corrupted_cycle() {
        let pool = mem_pool().await;
        let tax = mk_taxonomy(&pool, "category", true, 10).await;
        let a = create(&pool, tax, nc("A", None)).await.unwrap();
        let b = create(&pool, tax, nc("B", a.id.get().copied()))
            .await
            .unwrap();
        let aid = a.id.get().copied().unwrap();
        let bid = b.id.get().copied().unwrap();
        rustango::sql::raw_execute_pool(
            &pool,
            &format!("UPDATE cms_category SET parent_id = {bid} WHERE id = {aid}"),
            Vec::new(),
        )
        .await
        .unwrap();
        let rows = Category::objects()
            .where_(Category::taxonomy_id.eq(tax))
            .fetch(&pool)
            .await
            .unwrap();
        let tree = build_tree(rows); // must return, not hang
        assert!(
            tree.is_empty(),
            "cyclic rows have no root → empty but returns"
        );
    }
}
