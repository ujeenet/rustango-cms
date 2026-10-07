//! Page-builder storage models — migration-tracked, versioned.
//!
//! No DDL runs when a Developer edits a schema: a structure change writes
//! a new **draft** row, and publishing flips draft→published (version+1)
//! while archiving the prior published row. Page values live in one stable
//! JSON store ([`PageBuilderData`]) and upgrade lazily. Tables are created
//! once by the generated `0006_page_builder` migration, never altered by
//! the builder.

use chrono::{DateTime, Utc};
use rustango::core::Column as _;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::Model;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::page_builder::schema::{ComponentDoc, ComponentEntry};

/// Lifecycle of a schema row. Exactly one `draft` and one `published`
/// per page type at a time (code-enforced); `archived` rows are history.
pub const STATUS_DRAFT: &str = "draft";
pub const STATUS_PUBLISHED: &str = "published";
pub const STATUS_ARCHIVED: &str = "archived";

/// A versioned body-schema document attached to a page type.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_page_type_schema",
    app = "cms",
    display = "status",
    admin(
        list_display = "page_type_id, status, version, updated_at",
        ordering = "page_type_id, version",
        list_filter = "status",
    )
)]
pub struct PageTypeSchema {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_page_type", on = "id", index)]
    pub page_type_id: i64,

    /// `draft` | `published` | `archived`.
    #[rustango(max_length = 16, index)]
    pub status: String,

    /// Monotonic per page type; the published version pages record.
    #[rustango(default = "1")]
    pub version: i32,

    /// The authored node tree ([`crate::page_builder::schema::Document`]).
    #[rustango(default = "'{}'::jsonb")]
    pub document: Value,

    pub created_by: Option<i64>,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

/// A reusable field-group (a component), referenced from
/// schemas by slug (`c_<slug>`).
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_component",
    app = "cms",
    display = "label",
    admin(
        list_display = "slug, label, version, updated_at",
        search_fields = "slug, label",
        ordering = "slug",
    )
)]
pub struct Component {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 100, unique)]
    pub slug: String,

    #[rustango(max_length = 200)]
    pub label: String,

    #[rustango(max_length = 80, default = "''")]
    pub icon: String,

    #[rustango(max_length = 255, default = "''")]
    pub description: String,

    /// Bumped on every save; stamped onto instances for lazy upgrade.
    #[rustango(default = "1")]
    pub version: i32,

    /// [`crate::page_builder::schema::ComponentDoc`].
    #[rustango(default = "'{}'::jsonb")]
    pub document: Value,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

/// The filled builder values for one page (additive to any code page
/// type's own extension table). One row per page.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_page_builder_data",
    app = "cms",
    display = "page_id",
    admin(
        list_display = "page_id, schema_version, updated_at",
        ordering = "page_id"
    )
)]
pub struct PageBuilderData {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_page", on = "id", unique)]
    pub page_id: i64,

    /// Published schema version the data was last saved against.
    #[rustango(default = "0")]
    pub schema_version: i32,

    /// `{ "<component_slug>": <version> }` snapshot for lazy upgrade.
    #[rustango(default = "'{}'::jsonb")]
    pub component_versions: Value,

    /// The filled values (scalars + nested groups + stream arrays +
    /// `_orphaned`).
    #[rustango(default = "'{}'::jsonb")]
    pub data: Value,

    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

// ---- schema row helpers -------------------------------------------------

/// The published schema row for a page type, if any.
///
/// # Errors
/// Propagates query failures.
pub async fn published_for(
    pool: &Pool,
    page_type_id: i64,
) -> Result<Option<PageTypeSchema>, rustango::sql::ExecError> {
    PageTypeSchema::objects()
        .where_(PageTypeSchema::page_type_id.eq(page_type_id))
        .where_(PageTypeSchema::status.eq(STATUS_PUBLISHED.to_owned()))
        .first(pool)
        .await
}

/// The draft schema row for a page type, if any.
///
/// # Errors
/// Propagates query failures.
pub async fn draft_for(
    pool: &Pool,
    page_type_id: i64,
) -> Result<Option<PageTypeSchema>, rustango::sql::ExecError> {
    PageTypeSchema::objects()
        .where_(PageTypeSchema::page_type_id.eq(page_type_id))
        .where_(PageTypeSchema::status.eq(STATUS_DRAFT.to_owned()))
        .first(pool)
        .await
}

/// Published + archived rows for a page type, newest version first — the
/// version history.
///
/// # Errors
/// Propagates query failures.
pub async fn list_versions(
    pool: &Pool,
    page_type_id: i64,
) -> Result<Vec<PageTypeSchema>, rustango::sql::ExecError> {
    let mut rows: Vec<PageTypeSchema> = PageTypeSchema::objects()
        .where_(PageTypeSchema::page_type_id.eq(page_type_id))
        .fetch(pool)
        .await?;
    rows.retain(|r| r.status != STATUS_DRAFT);
    rows.sort_by(|a, b| b.version.cmp(&a.version));
    Ok(rows)
}

/// Upsert the single draft for a page type with `document`. Creates the
/// draft (version = published+1) if none exists.
///
/// # Errors
/// Propagates query / write failures.
pub async fn save_draft(
    pool: &Pool,
    page_type_id: i64,
    document: Value,
    created_by: Option<i64>,
) -> Result<PageTypeSchema, rustango::sql::ExecError> {
    if let Some(mut draft) = draft_for(pool, page_type_id).await? {
        draft.document = document;
        draft.save_pool(pool).await?;
        return Ok(draft);
    }
    let next = published_for(pool, page_type_id)
        .await?
        .map_or(1, |p| p.version + 1);
    let mut row = PageTypeSchema {
        id: Auto::Unset,
        page_type_id,
        status: STATUS_DRAFT.to_owned(),
        version: next,
        document,
        created_by,
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    row.insert_pool(pool).await?;
    Ok(row)
}

/// Publish the draft: archive the current published row, flip the draft
/// to published with version = prev_published + 1. Returns the new
/// published row, or `None` when there is no draft to publish.
///
/// # Errors
/// Propagates query / write failures.
pub async fn publish(
    pool: &Pool,
    page_type_id: i64,
) -> Result<Option<PageTypeSchema>, rustango::sql::ExecError> {
    let Some(mut draft) = draft_for(pool, page_type_id).await? else {
        return Ok(None);
    };
    let prev = published_for(pool, page_type_id).await?;
    // Archive and flip in one transaction (#649): committed separately, a
    // failed flip left the old schema archived and no schema published, so
    // every page of the type rendered an empty builder body. Both rows are
    // read above, on the pool, before the transaction takes a connection.
    let mut tx = rustango::sql::transaction_pool(pool).await?;
    let prev_version = match prev {
        Some(mut prev) => {
            prev.status = STATUS_ARCHIVED.to_owned();
            prev.save_tx(&mut tx).await?;
            prev.version
        }
        None => 0,
    };
    draft.status = STATUS_PUBLISHED.to_owned();
    draft.version = prev_version + 1;
    draft.save_tx(&mut tx).await?;
    tx.commit().await?;
    Ok(Some(draft))
}

// ---- component helpers --------------------------------------------------

/// Every reusable component, ordered by slug.
///
/// # Errors
/// Propagates query failures.
pub async fn all_components(pool: &Pool) -> Result<Vec<Component>, rustango::sql::ExecError> {
    Component::objects()
        .order_by(&[("slug", false)])
        .fetch(pool)
        .await
}

/// Component library as a `slug → entry` map for [`crate::page_builder::validate_schema`]
/// / [`crate::page_builder::compile()`]. Malformed component docs are skipped.
///
/// # Errors
/// Propagates query failures.
pub async fn component_map(
    pool: &Pool,
) -> Result<HashMap<String, ComponentEntry>, rustango::sql::ExecError> {
    let rows = all_components(pool).await?;
    let mut map = HashMap::new();
    for c in rows {
        let doc: ComponentDoc = serde_json::from_value(c.document.clone()).unwrap_or_default();
        map.insert(
            c.slug.clone(),
            ComponentEntry {
                doc,
                version: c.version.max(1) as u32,
            },
        );
    }
    Ok(map)
}

/// One component by id.
///
/// # Errors
/// Propagates query failures.
pub async fn component_by_id(
    pool: &Pool,
    id: i64,
) -> Result<Option<Component>, rustango::sql::ExecError> {
    Component::objects()
        .where_(Component::id.eq(id))
        .first(pool)
        .await
}

/// Create a component (version 1). `document` is a [`ComponentDoc`] JSON.
///
/// # Errors
/// Propagates the insert failure (incl. the unique-slug violation).
pub async fn create_component(
    pool: &Pool,
    slug: &str,
    label: &str,
    icon: &str,
    description: &str,
    document: Value,
) -> Result<Component, rustango::sql::ExecError> {
    let mut row = Component {
        id: Auto::Unset,
        slug: slug.to_owned(),
        label: label.to_owned(),
        icon: icon.to_owned(),
        description: description.to_owned(),
        version: 1,
        document,
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    row.insert_pool(pool).await?;
    Ok(row)
}

/// Update a component's metadata + document and bump its version so
/// referencing schemas pick the change up lazily (next open / render).
/// The slug is immutable (it's the reference key).
///
/// # Errors
/// Propagates query / write failures.
pub async fn update_component(
    pool: &Pool,
    id: i64,
    label: &str,
    icon: &str,
    description: &str,
    document: Value,
) -> Result<Option<Component>, rustango::sql::ExecError> {
    let Some(mut row) = component_by_id(pool, id).await? else {
        return Ok(None);
    };
    row.label = label.to_owned();
    row.icon = icon.to_owned();
    row.description = description.to_owned();
    row.document = document;
    row.version += 1;
    row.save_pool(pool).await?;
    Ok(Some(row))
}

/// Delete a component by id (no-op if it's already gone).
///
/// # Errors
/// Propagates the delete failure.
pub async fn delete_component(pool: &Pool, id: i64) -> Result<(), rustango::sql::ExecError> {
    if let Some(row) = component_by_id(pool, id).await? {
        row.delete_pool(pool).await?;
    }
    Ok(())
}

/// Page types whose non-archived (draft or published) schema references
/// a component slug — returns `(page_type_id, status)` pairs. Used to
/// block deletion of an in-use component.
///
/// # Errors
/// Propagates query failures.
pub async fn component_references(
    pool: &Pool,
    slug: &str,
) -> Result<Vec<(i64, String)>, rustango::sql::ExecError> {
    let rows: Vec<PageTypeSchema> = PageTypeSchema::objects().fetch(pool).await?;
    let mut out = Vec::new();
    for row in rows {
        if row.status == STATUS_ARCHIVED {
            continue;
        }
        if let Ok(doc) = crate::page_builder::schema::parse(&row.document) {
            if crate::page_builder::schema::references_component(&doc, slug) {
                out.push((row.page_type_id, row.status));
            }
        }
    }
    Ok(out)
}

// ---- page value helpers -------------------------------------------------

/// The stored builder values for a page, if any.
///
/// # Errors
/// Propagates query failures.
pub async fn data_for_page(
    pool: &Pool,
    page_id: i64,
) -> Result<Option<PageBuilderData>, rustango::sql::ExecError> {
    PageBuilderData::objects()
        .where_(PageBuilderData::page_id.eq(page_id))
        .first(pool)
        .await
}

/// Upsert a page's builder values.
///
/// # Errors
/// Propagates query / write failures.
pub async fn upsert_data(
    pool: &Pool,
    page_id: i64,
    schema_version: i32,
    component_versions: Value,
    data: Value,
) -> Result<(), rustango::sql::ExecError> {
    if let Some(mut row) = data_for_page(pool, page_id).await? {
        row.schema_version = schema_version;
        row.component_versions = component_versions;
        row.data = data;
        row.save_pool(pool).await?;
    } else {
        let mut row = PageBuilderData {
            id: Auto::Unset,
            page_id,
            schema_version,
            component_versions,
            data,
            updated_at: Auto::Unset,
        };
        row.insert_pool(pool).await?;
    }
    Ok(())
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use rustango::core::Model as _;
    use rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect;

    async fn mem_pool() -> Pool {
        let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
        // Only the schema table is needed for these unit tests (no FK
        // enforcement target rows involved — page_type_id is a bare int
        // here; sqlite in-memory has FKs off unless a target exists).
        for schema in [
            &crate::page_type_model::PageType::SCHEMA,
            &PageTypeSchema::SCHEMA,
            &Component::SCHEMA,
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

    async fn mk_page_type(pool: &Pool, type_name: &str) -> i64 {
        let mut pt = crate::page_type_model::PageType {
            id: Auto::Unset,
            app_label: "cms".to_owned(),
            type_name: type_name.to_owned(),
            verbose_name: type_name.to_owned(),
            default_template: "page.html".to_owned(),
            view_mode: "auto".to_owned(),
            is_creatable: true,
            allowed_parent_types: serde_json::json!([]),
            allowed_child_types: serde_json::json!([]),
            workflow: String::new(),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        pt.insert_pool(pool).await.expect("insert page type");
        pt.id.get().copied().unwrap()
    }

    #[tokio::test]
    async fn draft_publish_versions_flow() {
        let pool = mem_pool().await;
        let ptid = mk_page_type(&pool, "Standard").await;
        // First draft → publish → v1.
        save_draft(&pool, ptid, serde_json::json!({"nodes": []}), None)
            .await
            .unwrap();
        assert!(draft_for(&pool, ptid).await.unwrap().is_some());
        let p1 = publish(&pool, ptid).await.unwrap().expect("published");
        assert_eq!(p1.version, 1);
        assert_eq!(p1.status, STATUS_PUBLISHED);
        assert!(
            draft_for(&pool, ptid).await.unwrap().is_none(),
            "draft consumed"
        );

        // Edit → new draft carries v2 preview → publish → v2, v1 archived.
        let d2 = save_draft(&pool, ptid, serde_json::json!({"nodes": [1]}), None)
            .await
            .unwrap();
        assert_eq!(d2.version, 2);
        let p2 = publish(&pool, ptid).await.unwrap().expect("published v2");
        assert_eq!(p2.version, 2);
        let hist = list_versions(&pool, ptid).await.unwrap();
        assert_eq!(hist.len(), 2, "v2 published + v1 archived");
        assert_eq!(hist[0].version, 2);
        assert_eq!(hist[0].status, STATUS_PUBLISHED);
        assert_eq!(hist[1].status, STATUS_ARCHIVED);

        // Publish with no draft → None.
        assert!(publish(&pool, ptid).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn re_saving_draft_updates_in_place() {
        let pool = mem_pool().await;
        let ptid = mk_page_type(&pool, "Standard").await;
        save_draft(&pool, ptid, serde_json::json!({"nodes": [1]}), None)
            .await
            .unwrap();
        save_draft(&pool, ptid, serde_json::json!({"nodes": [1, 2]}), None)
            .await
            .unwrap();
        let drafts: Vec<PageTypeSchema> = PageTypeSchema::objects()
            .where_(PageTypeSchema::page_type_id.eq(ptid))
            .where_(PageTypeSchema::status.eq(STATUS_DRAFT.to_owned()))
            .fetch(&pool)
            .await
            .unwrap();
        assert_eq!(drafts.len(), 1, "one draft, updated in place");
    }
}
