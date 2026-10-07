//! `Revision` — versioned snapshots of `cms_page` rows.
//!
//! Captured on every page save in the admin. Each revision stores the
//! full page state as JSON; [`prune`] bounds storage.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// One row per save of a `Page`. `sequence` is a per-page monotonic
/// counter starting at 1.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_revision",
    app = "cms",
    admin(
        list_display = "page_id, sequence, created_by, created_at",
        ordering = "-created_at",
        list_filter = "page_id",
    )
)]
pub struct Revision {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// The page this revision belongs to. FK so revisions get
    /// cascade-deleted with their page.
    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    /// Per-page sequence number, starting at 1 for the first save.
    /// Combined with `page_id` to identify a revision; UNIQUE
    /// (page_id, sequence) through [`ensure_unique_sequence`].
    pub sequence: i32,

    /// Full serialized state of the `cms_page` row at this point in
    /// time.
    pub snapshot: serde_json::Value,

    /// `rustango_users.id` of the editor who triggered this save.
    /// `None` for system / script writes.
    pub created_by: Option<i64>,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

impl Revision {
    /// One-line label used by the admin's `display = "label"` and the
    /// revisions panel. Format: `"#N · YYYY-MM-DD HH:MM UTC"`.
    #[must_use]
    pub fn label(&self) -> String {
        let when = self
            .created_at
            .get()
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_default();
        format!("#{} · {}", self.sequence, when)
    }
}

/// Capture one revision for `page`: a full-state snapshot under the
/// page's next `sequence`.
///
/// Two saves of one page can race for the same sequence. The UNIQUE
/// (page_id, sequence) index from [`ensure_unique_sequence`] refuses the
/// second insert, and it retries with the sequence after the winner's.
///
/// # Errors
/// Driver / query failures or serialization errors.
pub async fn capture(
    pool: &rustango::sql::Pool,
    page: &crate::page::Page,
    created_by: Option<i64>,
) -> Result<Revision, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::{sqlx, Auto};

    // Enough for every writer of one page to lose once.
    const ATTEMPTS: usize = 10;
    let page_id = page.id.get().copied().unwrap_or_default();
    let snapshot = serde_json::to_value(page)
        .map_err(|e| rustango::sql::ExecError::Driver(sqlx::Error::Decode(Box::new(e))))?;

    let mut attempt = 0;
    loop {
        attempt += 1;
        // Only the newest row: the history can run to thousands (#762).
        let latest: Option<Revision> = Revision::objects()
            .where_(Revision::page_id.eq(page_id))
            .order_by(&[("sequence", true)])
            .first(pool)
            .await?;
        let mut row = Revision {
            id: Auto::Unset,
            page_id,
            sequence: latest.map_or(1, |r| r.sequence + 1),
            snapshot: snapshot.clone(),
            created_by,
            created_at: Auto::Unset,
        };
        match row.insert_pool(pool).await {
            Ok(()) => return Ok(row),
            Err(e) if attempt < ATTEMPTS && is_unique_violation(&e) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Whether `e` is a UNIQUE-constraint refusal, on any backend.
fn is_unique_violation(e: &rustango::sql::ExecError) -> bool {
    let msg = e.to_string().to_ascii_lowercase();
    // postgres: "duplicate key value violates unique constraint";
    // sqlite: "UNIQUE constraint failed"; mysql: "Duplicate entry".
    msg.contains("unique constraint") || msg.contains("duplicate entry") || msg.contains("duplicate key value")
}

/// Name of the UNIQUE (page_id, sequence) index on `cms_revision`.
const UNIQUE_SEQUENCE_INDEX: &str = "cms_revision_page_sequence_uniq";

/// Give `cms_revision` its UNIQUE (page_id, sequence) index, so a
/// concurrent save can't reuse a sequence. Pages that already hold
/// duplicates are renumbered first, in (sequence, id) order, which keeps
/// their history's order. Idempotent.
///
/// # Errors
/// Driver failures other than "index already exists".
pub async fn ensure_unique_sequence(
    pool: &rustango::sql::Pool,
) -> Result<(), rustango::sql::ExecError> {
    if create_unique_sequence_index(pool).await.is_ok() {
        return Ok(());
    }
    renumber_duplicate_sequences(pool).await?;
    create_unique_sequence_index(pool).await
}

async fn create_unique_sequence_index(
    pool: &rustango::sql::Pool,
) -> Result<(), rustango::sql::ExecError> {
    // MySQL has no CREATE INDEX IF NOT EXISTS; its "already exists" is
    // swallowed below instead.
    let if_not_exists = if pool.dialect().name() == "mysql" { "" } else { "IF NOT EXISTS " };
    let sql = format!(
        "CREATE UNIQUE INDEX {if_not_exists}{UNIQUE_SEQUENCE_INDEX} ON cms_revision (page_id, sequence)"
    );
    match rustango::sql::raw_execute_pool(pool, &sql, Vec::new()).await {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().to_ascii_lowercase().contains("duplicate key name") => Ok(()),
        Err(e) => Err(e),
    }
}

async fn renumber_duplicate_sequences(
    pool: &rustango::sql::Pool,
) -> Result<(), rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let pages: Vec<(i64,)> = rustango::sql::raw_query_pool(
        "SELECT page_id FROM cms_revision GROUP BY page_id, sequence HAVING COUNT(*) > 1",
        Vec::new(),
        pool,
    )
    .await?;
    let mut seen = std::collections::HashSet::new();
    for (page_id,) in pages {
        if !seen.insert(page_id) {
            continue;
        }
        let rows: Vec<Revision> = Revision::objects()
            .where_(Revision::page_id.eq(page_id))
            .order_by(&[("sequence", false), ("id", false)])
            .fetch(pool)
            .await?;
        for (n, mut row) in rows.into_iter().enumerate() {
            let want = i32::try_from(n + 1).unwrap_or(i32::MAX);
            if row.sequence != want {
                row.sequence = want;
                row.save_pool(pool).await?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------
// #122 — revision garbage collection. High-edit-volume pages
// accumulate unbounded history; pruning trims back to a configurable
// cap. The rules:
//   1. Keep the most recent `keep_latest` revisions (default 50).
//   2. When `keep_published` is true, never delete a revision whose
//      snapshot has `status == "published"`.
//   3. Always keep the single most recent published revision (the
//      rollback anchor) regardless of #1's window.
// ---------------------------------------------------------------

/// Default `keep_latest` cap when the caller doesn't override it.
pub const DEFAULT_KEEP_LATEST: usize = 50;

/// Outcome of one [`prune`] sweep against a page.
#[derive(Debug, Clone, Copy, Default)]
pub struct PruneOutcome {
    /// Rows present before the sweep ran.
    pub total: usize,
    /// Rows actually deleted by this sweep.
    pub deleted: usize,
}

/// Prune old revisions for one page. Returns the count of rows
/// deleted (0 when the page is already under the cap).
///
/// # Errors
/// Driver / query failures.
pub async fn prune(
    pool: &rustango::sql::Pool,
    page_id: i64,
    keep_latest: usize,
    keep_published: bool,
) -> Result<PruneOutcome, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let mut rows: Vec<Revision> = Revision::objects()
        .where_(Revision::page_id.eq(page_id))
        .order_by(&[("sequence", true)]) // desc
        .fetch(pool)
        .await?;
    let total = rows.len();
    if total == 0 {
        return Ok(PruneOutcome::default());
    }
    let keep = keep_latest.max(1);
    // Anchor: most recent published-status snapshot. We always keep
    // this regardless of position, so an emergency rollback target is
    // never accidentally GC'd.
    let anchor_id: Option<i64> = rows
        .iter()
        .find(|r| r.snapshot.get("status").and_then(|v| v.as_str()) == Some("published"))
        .and_then(|r| r.id.get().copied());
    let mut deleted = 0;
    // Walk from oldest to newest so the latest `keep` stay. Iterate
    // tail-first by reversing then dropping front entries.
    rows.reverse();
    let cutoff = total.saturating_sub(keep);
    for (idx, row) in rows.into_iter().enumerate() {
        if idx >= cutoff {
            break;
        }
        if Some(row.id.get().copied().unwrap_or_default()) == anchor_id {
            continue;
        }
        if keep_published
            && (row.snapshot.get("status").and_then(|v| v.as_str()) == Some("published"))
        {
            continue;
        }
        row.delete_pool(pool).await?;
        deleted += 1;
    }
    Ok(PruneOutcome { total, deleted })
}

/// Run [`prune`] against every page in the tenant. Best-effort —
/// per-page failures log + continue so one bad page doesn't abort
/// the whole sweep. Returns the aggregated `(pages_scanned, rows_deleted)`.
///
/// # Errors
/// Driver / query failures on the initial page-id lookup. Per-page
/// pruning errors are logged + swallowed.
pub async fn prune_all_pages(
    pool: &rustango::sql::Pool,
    keep_latest: usize,
    keep_published: bool,
) -> Result<(usize, usize), rustango::sql::ExecError> {
    use rustango::sql::FetcherPool as _;
    let pages: Vec<crate::page::Page> = crate::page::Page::objects().fetch(pool).await?;
    let mut total_pages = 0;
    let mut total_deleted = 0;
    for p in pages {
        let Some(pid) = p.id.get().copied() else {
            continue;
        };
        total_pages += 1;
        match prune(pool, pid, keep_latest, keep_published).await {
            Ok(outcome) => total_deleted += outcome.deleted,
            Err(e) => tracing::warn!(
                target: "rustango_cms::revision",
                page_id = pid,
                error = %e,
                "revision prune failed; continuing"
            ),
        }
    }
    Ok((total_pages, total_deleted))
}

// ---------------------------------------------------------------
// #74 — revision diff. Lightweight, side-by-side, field-level JSON
// comparison between two snapshots. Rich-text + StreamField
// awareness deferred to a follow-up; v1 treats those as opaque
// JSON values and just flags "changed."
// ---------------------------------------------------------------

/// One field's diff outcome for the compare view.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FieldDiff {
    /// Same value on both sides — surfaced so the template can
    /// optionally collapse them under a "show all fields" toggle.
    Unchanged { field: String, value: String },
    /// Different value. Both sides rendered for side-by-side.
    Changed { field: String, a: String, b: String },
    /// Only in B (a was null / missing).
    Added { field: String, b: String },
    /// Only in A (b is null / missing).
    Removed { field: String, a: String },
}

impl FieldDiff {
    /// Field name accessor for template grouping.
    #[must_use]
    pub fn field(&self) -> &str {
        match self {
            Self::Unchanged { field, .. }
            | Self::Changed { field, .. }
            | Self::Added { field, .. }
            | Self::Removed { field, .. } => field,
        }
    }

    /// True for the three non-`Unchanged` variants — drives the
    /// "show only changed" filter in the template.
    #[must_use]
    pub fn is_change(&self) -> bool {
        !matches!(self, Self::Unchanged { .. })
    }
}

#[cfg(test)]
mod prune_tests {
    use super::*;

    /// Pure helper that the prune logic uses internally — confirm
    /// the cutoff math is right for a few representative shapes.
    fn cutoff_for(total: usize, keep: usize) -> usize {
        total.saturating_sub(keep.max(1))
    }

    #[test]
    fn no_cutoff_when_under_keep() {
        assert_eq!(cutoff_for(10, 50), 0);
    }

    #[test]
    fn cutoff_drops_oldest_only() {
        // 100 rows, keep 50 → cutoff = 50 (oldest 50 deleted).
        assert_eq!(cutoff_for(100, 50), 50);
    }

    #[test]
    fn keep_zero_treated_as_one() {
        assert_eq!(cutoff_for(10, 0), 9);
    }

    #[test]
    fn default_keep_latest_is_sensible() {
        assert!((10..=200).contains(&DEFAULT_KEEP_LATEST));
    }
}

/// Fields surfaced in the diff view. Tree-shape columns
/// (`id`, `path`, `depth`, `parent_id`, `sort_order`, `url_path`)
/// are intentionally excluded — Revert leaves those alone, so
/// diffing them just adds noise. Audit-stamp columns (`created_at`,
/// `updated_at`) are excluded for the same reason.
const DIFF_FIELDS: &[&str] = &[
    "title",
    "slug",
    "status",
    "page_type_id",
    "seo_title",
    "seo_description",
    "robots_index",
    "sitemap_priority",
    "theme_id",
    "show_in_menus",
    "go_live_at",
    "expire_at",
    "published_at",
    "locale_variant_of",
];

/// Format a `serde_json::Value` for display in the diff cell. Strings
/// render verbatim; numbers / bools use their JSON shape; objects +
/// arrays are pretty-printed (StreamField bodies are JSON arrays —
/// the v1 cell shows the formatted JSON). `Null` renders as `—`.
fn format_value(value: &serde_json::Value) -> String {
    use serde_json::Value;
    match value {
        Value::Null => "—".to_owned(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// Compare two page revision snapshots field-by-field. Returns one
/// `FieldDiff` per declared diff field (see `DIFF_FIELDS`), in
/// declaration order.
///
/// Both arguments are the raw `snapshot` JSON from the
/// [`Revision`] row.
#[must_use]
pub fn diff_snapshots(a: &serde_json::Value, b: &serde_json::Value) -> Vec<FieldDiff> {
    use serde_json::Value;
    DIFF_FIELDS
        .iter()
        .map(|field| {
            let av = a.get(field).cloned().unwrap_or(Value::Null);
            let bv = b.get(field).cloned().unwrap_or(Value::Null);
            let a_null = matches!(av, Value::Null);
            let b_null = matches!(bv, Value::Null);
            let same = av == bv;
            let field = (*field).to_owned();
            if same {
                FieldDiff::Unchanged {
                    field,
                    value: format_value(&av),
                }
            } else if a_null && !b_null {
                FieldDiff::Added {
                    field,
                    b: format_value(&bv),
                }
            } else if b_null && !a_null {
                FieldDiff::Removed {
                    field,
                    a: format_value(&av),
                }
            } else {
                FieldDiff::Changed {
                    field,
                    a: format_value(&av),
                    b: format_value(&bv),
                }
            }
        })
        .collect()
}
