//! Page ↔ Snippet many-to-many relations.
//!
//! A page type can declare a named relation (e.g. `categories`) to a
//! snippet type. Editors get a multi-select chooser on the page form
//! and templates get an iterable of the related snippets. Behind the
//! scenes the links live in a through-table.
//!
//! This module is the framework-managed through-table. Reads and
//! writes go through plain helpers ([`relate`], [`unrelate`],
//! [`replace_all`], [`related_snippets`]). For the admin chooser,
//! [`crate::widget::Widget::snippet_m2m`] builds the
//! `WidgetKind::SnippetM2M` multi-snippet widget, and the form
//! save-path persists the posted id array via [`replace_all`].
//!
//! Declarative API: a page type declares its relations with the
//! struct-level `#[page_type(snippet_m2m(categories = "Category", …))]`
//! attribute. The `PageType` derive then generates (a) the `widgets()`
//! chooser, (b) the `save_extension` write via [`replace_all`], and
//! (c) a `snippet_m2m_relations()` accessor that the public renderer uses
//! to resolve each relation into the `snippet_relations.<name>` template
//! variable (`{% for c in snippet_relations.categories %}`). The earlier
//! `#[field(widget = SnippetM2M)]` sketch was superseded by the
//! struct-level form, since an M2M relation has no backing extension
//! column to hang a `#[field]` off.
//!
//! ## Schema
//!
//! One row per (page, snippet, relation_name) triple. `relation_name`
//! lets a single page point at the same snippet under multiple
//! semantic names (`categories`, `featured_authors`, etc.) without
//! collision.
//!
//! ```text
//! cms_page_snippet_m2m
//!   id              PK
//!   page_id         FK cms_page
//!   snippet_id      FK cms_snippet
//!   relation_name   varchar(64)   -- editor-facing field name
//!   sort_order      int           -- chooser-set order
//!   created_at      timestamp
//! ```
//!
//! Tenant-scoped: all helpers route through the tenant pool, so the
//! same (page_id, snippet_id, relation_name) triple in two tenants
//! is naturally segregated.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// One link between a [`crate::page::Page`] and a
/// [`crate::snippet::Snippet`] under a named relation. Composite
/// `(page_id, snippet_id, relation_name)` is unique per tenant —
/// enforced at the application layer (see [`relate`]); a SQL-side
/// composite unique constraint is dialect-drifty in our ORM today
/// and follows the same convention as `cms_page_tag`.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_page_snippet_m2m",
    app = "cms",
    display = "id",
    admin(
        list_display = "page_id, snippet_id, relation_name, sort_order",
        ordering = "page_id, relation_name, sort_order, id",
        list_filter = "relation_name",
    )
)]
pub struct PageSnippetM2M {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Parent page. Index so `WHERE page_id = ?` (the dominant
    /// read shape — load all relations for the page being rendered)
    /// stays a single index lookup.
    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    /// Linked snippet. Index so reverse lookups (the snippet-edit
    /// view's "where is this used" panel) stay cheap.
    #[rustango(fk = "cms_snippet", on = "id", index)]
    pub snippet_id: i64,

    /// Editor-facing field name — `categories`, `featured_authors`,
    /// `related_pages`, etc. A page can carry multiple relations
    /// under different names so a single `BlogPostPage` can have
    /// both a `categories` chooser and a `tags` chooser without
    /// the rows colliding. Indexed because the per-page read is
    /// scoped by both `page_id` AND `relation_name`.
    #[rustango(max_length = 64, index)]
    pub relation_name: String,

    /// Order within the relation as set by the chooser. Lower
    /// values sort first; ties broken by `id` (insertion order).
    pub sort_order: i32,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Fetch every row for `(page_id, relation_name)`, ordered by
/// `sort_order` then `id` (insertion fallback when the chooser
/// hasn't set explicit ordering).
///
/// Returns the M2M rows themselves — call [`related_snippets`] to
/// resolve them to full [`crate::snippet::Snippet`] rows in one
/// extra query.
///
/// # Errors
/// Driver / query failures.
pub async fn rows_for_relation(
    pool: &rustango::sql::Pool,
    page_id: i64,
    relation_name: &str,
) -> Result<Vec<PageSnippetM2M>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let mut rows: Vec<PageSnippetM2M> = PageSnippetM2M::objects()
        .where_(PageSnippetM2M::page_id.eq(page_id))
        .where_(PageSnippetM2M::relation_name.eq(relation_name.to_owned()))
        .fetch(pool)
        .await?;
    rows.sort_by(|a, b| {
        a.sort_order
            .cmp(&b.sort_order)
            .then_with(|| a.id.get().cmp(&b.id.get()))
    });
    Ok(rows)
}

/// Fetch the linked [`crate::snippet::Snippet`] rows for
/// `(page_id, relation_name)` in chooser order. The dominant
/// template-time read — `{% for c in page.categories %}` in
/// host templates resolves through this.
///
/// Two queries: one for the M2M rows (uses the
/// `(page_id, relation_name)` index), one for the snippet rows
/// (`WHERE id IN (...)`). Snippets are re-ordered to match the
/// M2M row order before returning so the chooser-set order
/// survives the round-trip.
///
/// # Errors
/// Driver / query failures.
pub async fn related_snippets(
    pool: &rustango::sql::Pool,
    page_id: i64,
    relation_name: &str,
) -> Result<Vec<crate::snippet::Snippet>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let m2m = rows_for_relation(pool, page_id, relation_name).await?;
    if m2m.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = m2m.iter().map(|r| r.snippet_id).collect();
    let snippets: Vec<crate::snippet::Snippet> = crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::id.is_in(ids.iter().copied()))
        .fetch(pool)
        .await?;
    // Re-order to match the M2M sort. `Snippet::id` is `Auto<i64>`
    // (Set after fetch) — lookup by the inner i64.
    let by_id: std::collections::HashMap<i64, crate::snippet::Snippet> = snippets
        .into_iter()
        .filter_map(|s| s.id.get().copied().map(|i| (i, s)))
        .collect();
    let mut out = Vec::with_capacity(m2m.len());
    for row in m2m {
        if let Some(s) = by_id.get(&row.snippet_id) {
            out.push(s.clone());
        }
        // Dangling reference (the snippet was deleted but the M2M
        // row stayed via a missing cascade) is silently skipped.
        // The ON DELETE CASCADE on the FK normally prevents this;
        // skip-on-miss makes the read resilient to a missed cascade.
    }
    Ok(out)
}

/// Insert a single (page, snippet, relation) link with the next
/// available `sort_order`. Idempotent — if the triple already
/// exists, returns the existing row instead of duplicating. The
/// application-layer uniqueness check matches the
/// [`crate::page_tag`] convention; SQL-side composite UNIQUE is
/// dialect-drifty.
///
/// # Errors
/// Driver / query failures.
pub async fn relate(
    pool: &rustango::sql::Pool,
    page_id: i64,
    snippet_id: i64,
    relation_name: &str,
) -> Result<PageSnippetM2M, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    // Idempotent check: the same triple already linked?
    let existing: Vec<PageSnippetM2M> = PageSnippetM2M::objects()
        .where_(PageSnippetM2M::page_id.eq(page_id))
        .where_(PageSnippetM2M::snippet_id.eq(snippet_id))
        .where_(PageSnippetM2M::relation_name.eq(relation_name.to_owned()))
        .fetch(pool)
        .await?;
    if let Some(row) = existing.into_iter().next() {
        return Ok(row);
    }
    // Find the next sort_order for `(page_id, relation_name)` so
    // sequential `relate(...)` calls preserve insertion order
    // without the caller having to compute it.
    let siblings = rows_for_relation(pool, page_id, relation_name).await?;
    let next_sort = siblings
        .iter()
        .map(|r| r.sort_order)
        .max()
        .map(|n| n + 1)
        .unwrap_or(0);
    let mut row = PageSnippetM2M {
        id: Auto::Unset,
        page_id,
        snippet_id,
        relation_name: relation_name.to_owned(),
        sort_order: next_sort,
        created_at: Auto::Unset,
    };
    row.insert_pool(pool).await?;
    Ok(row)
}

/// Remove a single (page, snippet, relation) link. No-op when the
/// triple doesn't exist (silently absorbs missing targets).
///
/// # Errors
/// Driver / query failures.
pub async fn unrelate(
    pool: &rustango::sql::Pool,
    page_id: i64,
    snippet_id: i64,
    relation_name: &str,
) -> Result<(), rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let existing: Vec<PageSnippetM2M> = PageSnippetM2M::objects()
        .where_(PageSnippetM2M::page_id.eq(page_id))
        .where_(PageSnippetM2M::snippet_id.eq(snippet_id))
        .where_(PageSnippetM2M::relation_name.eq(relation_name.to_owned()))
        .fetch(pool)
        .await?;
    for row in existing {
        row.delete_pool(pool).await?;
    }
    Ok(())
}

/// Replace the entire relation for `(page_id, relation_name)` with
/// `snippet_ids` in the order provided. Used by save handlers that
/// rewrite the M2M set on every save (the typical chooser-form
/// shape — submit the full ordered list, the handler computes the
/// diff). Order in `snippet_ids` becomes `sort_order` 0..N.
///
/// Three-step: load current rows → delete the ones that disappeared
/// → upsert the survivors with their new `sort_order` (and insert
/// the new ones). Avoids the naive `delete then insert all` so
/// stable rows keep their `id` + `created_at` across saves (audit
/// integrity).
///
/// # Errors
/// Driver / query failures.
pub async fn replace_all(
    pool: &rustango::sql::Pool,
    page_id: i64,
    relation_name: &str,
    snippet_ids: &[i64],
) -> Result<(), rustango::sql::ExecError> {
    // `Column` + `FetcherPool` imports aren't needed here — the
    // inner work goes through `rows_for_relation` (already trait-
    // imported in its own body) and `delete_pool` / `save_pool` /
    // `insert_pool` (autoderefed on the model).
    let current = rows_for_relation(pool, page_id, relation_name).await?;
    let desired: std::collections::HashSet<i64> = snippet_ids.iter().copied().collect();
    // Delete rows whose snippet_id is no longer in the desired set.
    for row in &current {
        if !desired.contains(&row.snippet_id) {
            row.clone().delete_pool(pool).await?;
        }
    }
    // Index existing-and-kept rows by snippet_id for the upsert.
    let kept: std::collections::HashMap<i64, PageSnippetM2M> = current
        .into_iter()
        .filter(|r| desired.contains(&r.snippet_id))
        .map(|r| (r.snippet_id, r))
        .collect();
    // Walk the desired ordering: existing → update sort_order;
    // new → insert with sort_order = i.
    for (i, &snippet_id) in snippet_ids.iter().enumerate() {
        let target_sort = i as i32;
        if let Some(mut row) = kept.get(&snippet_id).cloned() {
            if row.sort_order != target_sort {
                row.sort_order = target_sort;
                row.save_pool(pool).await?;
            }
        } else {
            let mut row = PageSnippetM2M {
                id: Auto::Unset,
                page_id,
                snippet_id,
                relation_name: relation_name.to_owned(),
                sort_order: target_sort,
                created_at: Auto::Unset,
            };
            row.insert_pool(pool).await?;
        }
    }
    Ok(())
}

/// Reverse lookup — every page that links to `snippet_id` under
/// any relation. Powers the snippet-edit view's "where is this
/// used" panel. Returns the M2M rows; callers can resolve to
/// [`crate::page::Page`] rows themselves (one extra `WHERE id IN`
/// query) since the typical caller wants just titles + URLs which
/// the Page row supplies.
///
/// # Errors
/// Driver / query failures.
pub async fn rows_for_snippet(
    pool: &rustango::sql::Pool,
    snippet_id: i64,
) -> Result<Vec<PageSnippetM2M>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    PageSnippetM2M::objects()
        .where_(PageSnippetM2M::snippet_id.eq(snippet_id))
        .fetch(pool)
        .await
}
