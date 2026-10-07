//! Page ↔ category assignments — the `cms_page_category` M2M,
//! mirroring [`crate::page_snippet_m2m`] / [`crate::page_tag`]. A page can
//! be filed under many categories across many vocabularies; the join row
//! denormalizes `taxonomy_id` so the editor can replace one vocabulary's
//! assignments without touching the others.

use chrono::{DateTime, Utc};
use rustango::core::Column as _;
use rustango::sql::{Auto, FetcherPool as _, FetcherTx as _, Pool, PoolTx};
use rustango::Model;
use serde::{Deserialize, Serialize};

use crate::category::model::Category;

/// One page↔category link. `(page_id, category_id)` is unique at the app
/// layer (composite SQL uniqueness is dialect-drifty in the ORM today).
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_page_category",
    app = "cms",
    display = "category_id",
    admin(
        list_display = "page_id, taxonomy_id, category_id, sort_order",
        ordering = "page_id, taxonomy_id, sort_order",
        list_filter = "taxonomy_id",
    )
)]
pub struct PageCategory {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    #[rustango(fk = "cms_category", on = "id", index)]
    pub category_id: i64,

    /// Denormalized owning vocabulary — lets the editor replace one
    /// taxonomy's assignments without touching the others.
    #[rustango(fk = "cms_taxonomy", on = "id", index)]
    pub taxonomy_id: i64,

    pub sort_order: i32,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Category ids assigned to `page_id` within one taxonomy, in chooser
/// order.
///
/// # Errors
/// Propagates query failures.
pub async fn category_ids_for_page(
    pool: &Pool,
    page_id: i64,
    taxonomy_id: i64,
) -> Result<Vec<i64>, rustango::sql::ExecError> {
    let rows = PageCategory::objects()
        .where_(PageCategory::page_id.eq(page_id))
        .where_(PageCategory::taxonomy_id.eq(taxonomy_id))
        .order_by(&[("sort_order", false), ("id", false)])
        .fetch(pool)
        .await?;
    Ok(rows.into_iter().map(|r| r.category_id).collect())
}

/// Full [`Category`] rows assigned to `page_id` within one taxonomy, in
/// chooser order (missing rows skipped). The template-time read.
///
/// # Errors
/// Propagates query failures.
pub async fn categories_for_page(
    pool: &Pool,
    page_id: i64,
    taxonomy_id: i64,
) -> Result<Vec<Category>, rustango::sql::ExecError> {
    let ids = category_ids_for_page(pool, page_id, taxonomy_id).await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut cats: Vec<Category> = Category::objects()
        .where_(Category::id.is_in(ids.clone()))
        .fetch(pool)
        .await?;
    let order: std::collections::HashMap<i64, usize> =
        ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    cats.sort_by_key(|c| {
        order
            .get(&c.id.get().copied().unwrap_or_default())
            .copied()
            .unwrap_or(usize::MAX)
    });
    Ok(cats)
}

/// Page ids filed under `category_id`.
///
/// # Errors
/// Propagates query failures.
pub async fn pages_in_category(
    pool: &Pool,
    category_id: i64,
) -> Result<Vec<i64>, rustango::sql::ExecError> {
    let rows = PageCategory::objects()
        .where_(PageCategory::category_id.eq(category_id))
        .fetch(pool)
        .await?;
    Ok(rows.into_iter().map(|r| r.page_id).collect())
}

/// Replace all of a page's assignments **within one taxonomy** with
/// `category_ids` (order preserved), leaving other vocabularies' links
/// intact. Diff-based — mirrors [`crate::page_snippet_m2m::replace_all`].
///
/// # Errors
/// Propagates query / write failures.
pub async fn replace_for_taxonomy(
    pool: &Pool,
    page_id: i64,
    taxonomy_id: i64,
    category_ids: &[i64],
) -> Result<(), rustango::sql::ExecError> {
    let existing: Vec<PageCategory> = PageCategory::objects()
        .where_(PageCategory::page_id.eq(page_id))
        .where_(PageCategory::taxonomy_id.eq(taxonomy_id))
        .fetch(pool)
        .await?;
    let desired: std::collections::HashSet<i64> = category_ids.iter().copied().collect();

    for row in &existing {
        if !desired.contains(&row.category_id) {
            row.clone().delete_pool(pool).await?;
        }
    }
    let have: std::collections::HashMap<i64, PageCategory> =
        existing.into_iter().map(|r| (r.category_id, r)).collect();

    for (idx, cid) in category_ids.iter().enumerate() {
        if let Some(row) = have.get(cid) {
            if row.sort_order != idx as i32 {
                let mut r = row.clone();
                r.sort_order = idx as i32;
                r.save_pool(pool).await?;
            }
        } else {
            let mut r = PageCategory {
                id: Auto::Unset,
                page_id,
                category_id: *cid,
                taxonomy_id,
                sort_order: idx as i32,
                created_at: Auto::Unset,
            };
            r.insert_pool(pool).await?;
        }
    }
    Ok(())
}

/// Delete every assignment referencing `category_id` (category deletion
/// cascade), inside the caller's transaction.
///
/// # Errors
/// Propagates query / delete failures.
pub async fn delete_for_category_tx(
    tx: &mut PoolTx<'_>,
    category_id: i64,
) -> Result<(), rustango::sql::ExecError> {
    let rows: Vec<PageCategory> = PageCategory::objects()
        .where_(PageCategory::category_id.eq(category_id))
        .fetch_tx(tx)
        .await?;
    for row in rows {
        row.delete_tx(tx).await?;
    }
    Ok(())
}

/// Delete every assignment for `page_id` (page deletion cascade).
///
/// # Errors
/// Propagates query / delete failures.
pub async fn delete_for_page(pool: &Pool, page_id: i64) -> Result<(), rustango::sql::ExecError> {
    let rows: Vec<PageCategory> = PageCategory::objects()
        .where_(PageCategory::page_id.eq(page_id))
        .fetch(pool)
        .await?;
    for row in rows {
        row.delete_pool(pool).await?;
    }
    Ok(())
}

/// A page's category as templates see it: enough to show it and
/// to filter or group by it (`{% if "mugs" in p.categories | map(attribute="slug") %}`).
#[derive(Debug, Clone, Serialize)]
pub struct CategoryRef {
    pub id: i64,
    pub name: String,
    pub slug: String,
    /// The vocabulary's slug, e.g. `category`.
    pub taxonomy: String,
}

/// Every category of each page in `page_ids`, in chooser order, grouped by
/// page id. Three queries whatever the page count; the render uses it for
/// the page and its children.
///
/// # Errors
/// Propagates query failures.
pub async fn prefetch_for_pages(
    pool: &Pool,
    page_ids: &[i64],
    locale_id: Option<i64>,
) -> Result<std::collections::HashMap<i64, Vec<CategoryRef>>, rustango::sql::ExecError> {
    use std::collections::HashMap;
    if page_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let links: Vec<PageCategory> = PageCategory::objects()
        .where_(PageCategory::page_id.is_in(page_ids.iter().copied()))
        .order_by(&[("page_id", false), ("sort_order", false), ("id", false)])
        .fetch(pool)
        .await?;
    if links.is_empty() {
        return Ok(HashMap::new());
    }
    let cat_ids: std::collections::BTreeSet<i64> = links.iter().map(|l| l.category_id).collect();
    let cats: HashMap<i64, Category> = Category::objects()
        .where_(Category::id.is_in(cat_ids.into_iter()))
        .fetch(pool)
        .await?
        .into_iter()
        .filter_map(|c| c.id.get().copied().map(|id| (id, c)))
        .collect();
    let tax_ids: std::collections::BTreeSet<i64> = cats.values().map(|c| c.taxonomy_id).collect();
    let tax_slugs: HashMap<i64, String> = crate::category::Taxonomy::objects()
        .where_(crate::category::Taxonomy::id.is_in(tax_ids.into_iter()))
        .fetch(pool)
        .await?
        .into_iter()
        .filter_map(|t| t.id.get().copied().map(|id| (id, t.slug)))
        .collect();
    // #862 — the visitor's language: translated names, one more query.
    let names: HashMap<i64, String> = match locale_id {
        Some(lid) => {
            let ids: Vec<i64> = cats.keys().copied().collect();
            crate::category_translation::fetch_for_categories(pool, &ids, lid)
                .await?
                .into_iter()
                .filter_map(|(id, mut fields)| fields.remove("name").map(|n| (id, n)))
                .collect()
        }
        None => HashMap::new(),
    };
    let mut by_page: HashMap<i64, Vec<CategoryRef>> = HashMap::new();
    for link in links {
        let Some(c) = cats.get(&link.category_id) else {
            continue;
        };
        by_page.entry(link.page_id).or_default().push(CategoryRef {
            id: link.category_id,
            name: names.get(&link.category_id).cloned().unwrap_or_else(|| c.name.clone()),
            slug: c.slug.clone(),
            taxonomy: tax_slugs.get(&c.taxonomy_id).cloned().unwrap_or_default(),
        });
    }
    Ok(by_page)
}

/// Save a page's categories from the editor's `categories` field:
/// a JSON array of category ids across every vocabulary. `None` (the
/// field wasn't posted, e.g. a partial MCP save) leaves the page's
/// categories alone; an empty array clears them. Ids that aren't
/// categories are dropped rather than trusted.
///
/// # Errors
/// Propagates query / write failures.
pub async fn save_from_form(
    pool: &Pool,
    page_id: i64,
    raw: Option<&str>,
) -> Result<(), rustango::sql::ExecError> {
    let Some(raw) = raw else {
        return Ok(());
    };
    let wanted = parse_id_list(raw);
    let cats: Vec<Category> = if wanted.is_empty() {
        Vec::new()
    } else {
        Category::objects()
            .where_(Category::id.is_in(wanted.iter().copied()))
            .fetch(pool)
            .await?
    };
    let taxonomy_of: std::collections::HashMap<i64, i64> = cats
        .iter()
        .filter_map(|c| c.id.get().copied().map(|id| (id, c.taxonomy_id)))
        .collect();
    for tax in crate::category::taxonomies(pool).await? {
        let Some(tax_id) = tax.id.get().copied() else {
            continue;
        };
        let ids: Vec<i64> = wanted
            .iter()
            .copied()
            .filter(|id| taxonomy_of.get(id) == Some(&tax_id))
            .collect();
        replace_for_taxonomy(pool, page_id, tax_id, &ids).await?;
    }
    Ok(())
}

/// One vocabulary's choices in the page editor.
#[derive(Debug, Serialize)]
pub struct EditorGroup {
    pub taxonomy: String,
    pub options: Vec<EditorOption>,
}

/// One category checkbox in the page editor.
#[derive(Debug, Serialize)]
pub struct EditorOption {
    pub id: i64,
    pub name: String,
    /// Nesting level, for indenting child categories.
    pub depth: i32,
    pub checked: bool,
}

/// The page editor's category choices: each vocabulary that has
/// categories, in tree order, with the page's current ones checked
/// (`page_id` is `None` on the new-page form). Also returns the checked
/// ids as the JSON array the form posts back.
///
/// # Errors
/// Propagates query failures.
pub async fn editor_groups(
    pool: &Pool,
    page_id: Option<i64>,
) -> Result<(Vec<EditorGroup>, String), rustango::sql::ExecError> {
    let chosen: std::collections::HashSet<i64> = match page_id {
        Some(pid) => PageCategory::objects()
            .where_(PageCategory::page_id.eq(pid))
            .fetch(pool)
            .await?
            .into_iter()
            .map(|r| r.category_id)
            .collect(),
        None => std::collections::HashSet::new(),
    };
    let all: Vec<Category> = Category::objects()
        .order_by(&[("path", false)])
        .fetch(pool)
        .await?;
    let mut groups = Vec::new();
    for tax in crate::category::taxonomies(pool).await? {
        let Some(tax_id) = tax.id.get().copied() else {
            continue;
        };
        let options: Vec<EditorOption> = all
            .iter()
            .filter(|c| c.taxonomy_id == tax_id)
            .filter_map(|c| {
                let id = c.id.get().copied()?;
                Some(EditorOption {
                    id,
                    name: c.name.clone(),
                    depth: c.depth,
                    checked: chosen.contains(&id),
                })
            })
            .collect();
        if !options.is_empty() {
            groups.push(EditorGroup { taxonomy: tax.verbose_name, options });
        }
    }
    let checked: Vec<String> = groups
        .iter()
        .flat_map(|g| g.options.iter())
        .filter(|o| o.checked)
        .map(|o| o.id.to_string())
        .collect();
    let json = serde_json::to_string(&checked).unwrap_or_else(|_| "[]".to_owned());
    Ok((groups, json))
}

/// The page editor's category field name.
pub const FORM_KEY: &str = "categories";

/// A JSON array of ids, as numbers or strings; anything else is ignored,
/// duplicates keep their first position.
fn parse_id_list(raw: &str) -> Vec<i64> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter_map(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
        .filter(|id| seen.insert(*id))
        .collect()
}

#[cfg(test)]
mod form_tests {
    use super::parse_id_list;

    #[test]
    fn id_lists_accept_numbers_and_strings_and_drop_the_rest() {
        assert_eq!(parse_id_list(r#"["3", 5, "x", null, "3"]"#), vec![3, 5]);
        assert!(parse_id_list("not json").is_empty());
        assert!(parse_id_list("{}").is_empty());
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod prefetch_tests {
    use super::*;
    use rustango::core::Model as _;

    /// The render gets each page's categories, in chooser order,
    /// with their vocabulary; a page with none is absent.
    #[tokio::test]
    async fn prefetch_groups_categories_by_page_in_order() {
        let pool = Pool::connect("sqlite::memory:").await.expect("pool");
        for schema in [
            &crate::page_type_model::PageType::SCHEMA,
            &crate::media::Media::SCHEMA,
            &crate::theme::Theme::SCHEMA,
            &crate::page::Page::SCHEMA,
            &crate::category::Taxonomy::SCHEMA,
            &Category::SCHEMA,
            &PageCategory::SCHEMA,
            &crate::locale::Locale::SCHEMA,
            &crate::category_translation::CategoryTranslation::SCHEMA,
        ] {
            let sql = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
            for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                rustango::sql::raw_execute_pool(&pool, stmt, Vec::new()).await.expect("ddl");
            }
        }
        for sql in [
            "INSERT INTO cms_page_type (id, app_label, type_name, verbose_name, default_template, is_creatable, allowed_parent_types) VALUES (1, 'cms', 'P', 'P', 'p.html', 1, '[]')",
            "INSERT INTO cms_page (id, page_type_id, title, slug, path, depth, sort_order, status, seo_title, seo_description) VALUES (1, 1, 'A', 'a', '0001/', 1, 0, 'published', '', ''), (2, 1, 'B', 'b', '0002/', 1, 0, 'published', '', '')",
        ] {
            rustango::sql::raw_execute_pool(&pool, sql, Vec::new()).await.expect("row");
        }
        crate::category::upsert_taxonomy(&pool, &crate::category::CategoryTaxonomy).await.expect("taxonomy");
        let tax = crate::category::find_taxonomy(&pool, "category").await.expect("q").expect("row");
        let tax_id = tax.id.get().copied().expect("id");
        let mut ids = Vec::new();
        for name in ["Mugs", "Plates"] {
            let c = crate::category::model::create(
                &pool,
                tax_id,
                crate::category::NewCategory {
                    name: name.to_owned(),
                    slug: String::new(),
                    description: String::new(),
                    parent_id: None,
                    featured_image_id: None,
                    thumbnail_id: None,
                },
            )
            .await
            .expect("category");
            ids.push(c.id.get().copied().expect("id"));
        }
        save_from_form(&pool, 1, Some(&format!("[{}, \"{}\"]", ids[1], ids[0]))).await.expect("save");

        let by_page = prefetch_for_pages(&pool, &[1, 2], None).await.expect("prefetch");
        let names: Vec<(&str, &str, &str)> = by_page[&1]
            .iter()
            .map(|c| (c.name.as_str(), c.slug.as_str(), c.taxonomy.as_str()))
            .collect();
        assert_eq!(names, [("Plates", "plates", "category"), ("Mugs", "mugs", "category")]);
        assert!(!by_page.contains_key(&2));

        // #862 — in another language, a translated name replaces the
        // original; an untranslated one keeps it.
        rustango::sql::raw_execute_pool(
            &pool,
            "INSERT INTO cms_locale (id, code, name, is_default, active, sort_order) VALUES (2, 'fr', 'Français', 0, 1, 1)",
            Vec::new(),
        )
        .await
        .expect("locale");
        rustango::sql::raw_execute_pool(
            &pool,
            &format!(
                "INSERT INTO cms_category_translation (category_id, locale_id, field_path, value, updated_at) VALUES ({}, 2, 'name', 'Assiettes', '2026-01-01T00:00:00Z')",
                ids[1]
            ),
            Vec::new(),
        )
        .await
        .expect("translation");
        let fr: Vec<String> = prefetch_for_pages(&pool, &[1], Some(2)).await.expect("q")[&1]
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(fr, ["Assiettes", "Mugs"]);

        // A partial save (no field) keeps them; an empty list clears them.
        save_from_form(&pool, 1, None).await.expect("untouched");
        assert_eq!(prefetch_for_pages(&pool, &[1], None).await.expect("q")[&1].len(), 2);
        save_from_form(&pool, 1, Some("[]")).await.expect("clear");
        assert!(prefetch_for_pages(&pool, &[1], None).await.expect("q").is_empty());
    }
}
