//! Extensible report framework (#439) — a registerable `Report` trait
//! (Wagtail's `ReportView` parity) + the three reports the hardcoded
//! set was missing: **page-types usage**, **unpublished changes**, and
//! **your pages in a workflow**.
//!
//! A report is a slug, a title, a set of typed columns, and an async
//! `rows()` that returns one JSON object per row (keyed by column key,
//! plus an optional `edit_url` the table links the first cell to). The
//! admin serves every registered report through one chrome'd dispatcher
//! at `/cms-admin/report/<slug>` (+ a `.csv` export), and `_base.html`
//! lists them under the Reports sidebar group via the registry — so a
//! host adds a report with [`register_report!`] and it appears in the
//! nav with no routing or template work.
//!
//! The seven pre-existing reports (aging / locked / scheduled /
//! workflow / revisions / search / media-unused) keep their bespoke
//! handlers for now — they carry extra UI (pagination, prune + edit
//! actions, bulk delete) beyond a plain table; migrating them onto this
//! framework is follow-up cleanup, not a prerequisite.

use async_trait::async_trait;
use rustango::sql::Pool;
use serde_json::{json, Value};

/// How a column's cell renders in the shared report table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    /// Plain string.
    Text,
    /// Right-aligned numeric.
    Number,
    /// A status pill (`<span class="rcms-tag {value}">`).
    Status,
    /// An RFC 3339 timestamp rendered as relative + absolute time.
    DateTime,
}

impl ColumnKind {
    /// Stable token handed to the template for per-kind rendering.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ColumnKind::Text => "text",
            ColumnKind::Number => "number",
            ColumnKind::Status => "status",
            ColumnKind::DateTime => "datetime",
        }
    }
}

/// One report column: a `key` into each row object, a header `label`,
/// and a render `kind`.
#[derive(Debug, Clone)]
pub struct ReportColumn {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: ColumnKind,
}

impl ReportColumn {
    #[must_use]
    pub const fn text(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: ColumnKind::Text,
        }
    }
    #[must_use]
    pub const fn number(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: ColumnKind::Number,
        }
    }
    #[must_use]
    pub const fn status(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: ColumnKind::Status,
        }
    }
    #[must_use]
    pub const fn datetime(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: ColumnKind::DateTime,
        }
    }
}

/// A registerable admin report. Implementors are zero-field structs
/// registered with [`register_report!`].
#[async_trait]
pub trait Report: Send + Sync + 'static {
    /// URL slug under `/cms-admin/report/<slug>` + the registry key.
    fn slug(&self) -> &'static str;
    /// Heading + sidebar label.
    fn title(&self) -> &'static str;
    /// Optional one-line blurb shown under the heading.
    fn description(&self) -> Option<&'static str> {
        None
    }
    /// Optional Material Symbols icon name for the sidebar.
    fn icon(&self) -> Option<&'static str> {
        None
    }
    /// The table columns, left to right.
    fn columns(&self) -> Vec<ReportColumn>;
    /// Build the rows. Each row is a JSON object keyed by column key;
    /// an optional `edit_url` key links the first cell. `user_id` is
    /// the signed-in user (for "your …"-style reports); `None` for an
    /// unauthenticated context.
    async fn rows(&self, pool: &Pool, user_id: Option<i64>) -> Vec<Value>;
}

/// Inventory registration — mirrors [`crate::admin::admin_page`].
pub struct ReportRegistration {
    pub factory: fn() -> Box<dyn Report>,
}

inventory::collect!(ReportRegistration);

/// Every registered report, sorted by title.
#[must_use]
pub fn registered_reports() -> Vec<Box<dyn Report>> {
    let mut v: Vec<Box<dyn Report>> = inventory::iter::<ReportRegistration>
        .into_iter()
        .map(|r| (r.factory)())
        .collect();
    v.sort_by(|a, b| a.title().cmp(b.title()));
    v
}

/// The report registered under `slug`, if any.
#[must_use]
pub fn find_report(slug: &str) -> Option<Box<dyn Report>> {
    registered_reports().into_iter().find(|r| r.slug() == slug)
}

/// Register a [`Report`] implementation (#439).
///
/// ```ignore
/// #[derive(Default)]
/// struct MyReport;
/// #[async_trait::async_trait]
/// impl rustango_cms::admin::report::Report for MyReport { /* … */ }
/// rustango_cms::register_report!(MyReport);
/// ```
#[macro_export]
macro_rules! register_report {
    ($ty:ty) => {
        $crate::inventory::submit! {
            $crate::admin::report::ReportRegistration {
                factory: || ::std::boxed::Box::new(<$ty>::default()),
            }
        }
    };
}

// ============================================================ built-in reports

fn page_edit_url(id: i64) -> String {
    format!("/cms-admin/pages/{id}/edit")
}

fn rfc3339<Tz: chrono::TimeZone>(dt: &chrono::DateTime<Tz>) -> String {
    dt.to_rfc3339()
}

/// **Page-types usage** (Wagtail 6.0) — how many pages exist of each
/// registered page type. Surfaces unused types + content distribution.
#[derive(Default)]
pub struct PageTypesUsageReport;

#[async_trait]
impl Report for PageTypesUsageReport {
    fn slug(&self) -> &'static str {
        "page-types"
    }
    fn title(&self) -> &'static str {
        "Page types usage"
    }
    fn description(&self) -> Option<&'static str> {
        Some("How many pages exist of each registered page type.")
    }
    fn icon(&self) -> Option<&'static str> {
        Some("category")
    }
    fn columns(&self) -> Vec<ReportColumn> {
        vec![
            ReportColumn::text("type", "Type"),
            ReportColumn::text("identifier", "Identifier"),
            ReportColumn::text("app", "App"),
            ReportColumn::number("pages", "Pages"),
        ]
    }
    async fn rows(&self, pool: &Pool, _user_id: Option<i64>) -> Vec<Value> {
        use crate::page::Page;
        use crate::page_type_model::PageType;
        use rustango::sql::FetcherPool as _;

        let pages: Vec<Page> = Page::objects().fetch(pool).await.unwrap_or_default();
        let mut counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        for p in &pages {
            *counts.entry(p.page_type_id).or_insert(0) += 1;
        }
        let mut types: Vec<PageType> = PageType::objects().fetch(pool).await.unwrap_or_default();
        // App, then type, alphabetically — stable + scannable.
        types.sort_by(|a, b| {
            a.app_label
                .cmp(&b.app_label)
                .then_with(|| a.verbose_name.cmp(&b.verbose_name))
        });
        types
            .iter()
            .map(|t| {
                let count =
                    t.id.get()
                        .copied()
                        .and_then(|id| counts.get(&id).copied())
                        .unwrap_or(0);
                json!({
                    "type": t.verbose_name,
                    "identifier": t.type_name,
                    "app": t.app_label,
                    "pages": count,
                })
            })
            .collect()
    }
}

/// **Unpublished changes** — pages that were published once but now sit
/// in draft (the editor saved a draft of a live page). In this CMS the
/// page row's `status` is the editor's chosen status, so a previously
/// published page (`published_at` set) currently at `status = draft`
/// has edits not yet re-published.
#[derive(Default)]
pub struct UnpublishedChangesReport;

#[async_trait]
impl Report for UnpublishedChangesReport {
    fn slug(&self) -> &'static str {
        "unpublished-changes"
    }
    fn title(&self) -> &'static str {
        "Unpublished changes"
    }
    fn description(&self) -> Option<&'static str> {
        Some("Previously published pages with draft edits not yet re-published.")
    }
    fn icon(&self) -> Option<&'static str> {
        Some("edit_note")
    }
    fn columns(&self) -> Vec<ReportColumn> {
        vec![
            ReportColumn::text("title", "Page"),
            ReportColumn::status("status", "Status"),
            ReportColumn::datetime("last_published", "Last published"),
            ReportColumn::datetime("edited", "Draft edited"),
        ]
    }
    async fn rows(&self, pool: &Pool, _user_id: Option<i64>) -> Vec<Value> {
        use crate::page::{Page, PageStatus};
        use rustango::core::Column as _;
        use rustango::sql::FetcherPool as _;

        let mut pages: Vec<Page> = Page::objects()
            .where_(Page::status.eq(PageStatus::Draft.as_str().to_owned()))
            .fetch(pool)
            .await
            .unwrap_or_default();
        // Only drafts of *previously published* pages — a brand-new
        // never-published draft has no live version to diverge from.
        pages.retain(|p| p.published_at.is_some());
        // Most-recently-edited first.
        pages.sort_by(|a, b| {
            b.updated_at
                .get()
                .copied()
                .cmp(&a.updated_at.get().copied())
        });
        pages
            .iter()
            .map(|p| {
                let id = p.id.get().copied().unwrap_or_default();
                json!({
                    "title": p.title,
                    "status": p.status,
                    "last_published": p.last_published_at.as_ref().map(rfc3339),
                    "edited": p.updated_at.get().map(rfc3339),
                    "edit_url": page_edit_url(id),
                })
            })
            .collect()
    }
}

/// **Your pages in a workflow** — pages the signed-in user submitted
/// that are still in flight (in progress or sent back for changes).
#[derive(Default)]
pub struct WorkflowYourPagesReport;

#[async_trait]
impl Report for WorkflowYourPagesReport {
    fn slug(&self) -> &'static str {
        "your-workflow-pages"
    }
    fn title(&self) -> &'static str {
        "Your pages in a workflow"
    }
    fn description(&self) -> Option<&'static str> {
        Some("Pages you submitted that are still awaiting a workflow decision.")
    }
    fn icon(&self) -> Option<&'static str> {
        Some("rule")
    }
    fn columns(&self) -> Vec<ReportColumn> {
        vec![
            ReportColumn::text("page", "Page"),
            ReportColumn::text("workflow", "Workflow"),
            ReportColumn::status("status", "Status"),
            ReportColumn::text("current_step", "Current step"),
            ReportColumn::datetime("submitted", "Submitted"),
        ]
    }
    async fn rows(&self, pool: &Pool, user_id: Option<i64>) -> Vec<Value> {
        use crate::page::Page;
        use crate::workflow::{Workflow, WorkflowState, WorkflowStatus, WorkflowTask};
        use rustango::core::Column as _;
        use rustango::sql::FetcherPool as _;

        let Some(uid) = user_id else {
            return Vec::new();
        };
        let states: Vec<WorkflowState> = WorkflowState::objects()
            .where_(WorkflowState::requested_by.eq(uid))
            .where_(WorkflowState::status.is_in([
                WorkflowStatus::InProgress.as_str().to_owned(),
                WorkflowStatus::NeedsChanges.as_str().to_owned(),
            ]))
            .order_by(&[("requested_at", true)]) // newest first
            .fetch(pool)
            .await
            .unwrap_or_default();
        if states.is_empty() {
            return Vec::new();
        }
        // Bulk-resolve the joined names so the report is a few queries,
        // not one-per-row.
        let pages: std::collections::HashMap<i64, Page> = Page::objects()
            .fetch(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| p.id.get().copied().map(|id| (id, p)))
            .collect();
        let workflows: std::collections::HashMap<i64, String> = Workflow::objects()
            .fetch(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|w| w.id.get().copied().map(|id| (id, w.name)))
            .collect();
        let tasks: std::collections::HashMap<i64, String> = WorkflowTask::objects()
            .fetch(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|t| t.id.get().copied().map(|id| (id, t.name)))
            .collect();

        states
            .iter()
            .map(|s| {
                let page = pages.get(&s.page_id);
                let title = page.map_or("(deleted page)", |p| p.title.as_str());
                let edit_url = page.and_then(|p| p.id.get().copied()).map(page_edit_url);
                let workflow = workflows.get(&s.workflow_id).map_or("—", String::as_str);
                let step = s
                    .current_task_id
                    .and_then(|tid| tasks.get(&tid))
                    .map_or("—", String::as_str);
                json!({
                    "page": title,
                    "workflow": workflow,
                    "status": s.status,
                    "current_step": step,
                    "submitted": s.requested_at.get().map(rfc3339),
                    "edit_url": edit_url,
                })
            })
            .collect()
    }
}

register_report!(PageTypesUsageReport);
register_report!(UnpublishedChangesReport);
register_report!(WorkflowYourPagesReport);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_builtin_reports_register() {
        let reports = registered_reports();
        for slug in ["page-types", "unpublished-changes", "your-workflow-pages"] {
            assert!(
                reports.iter().any(|r| r.slug() == slug),
                "report `{slug}` should be registered"
            );
        }
    }

    #[test]
    fn find_resolves_by_slug_and_misses_cleanly() {
        assert!(find_report("page-types").is_some());
        assert_eq!(
            find_report("page-types").unwrap().title(),
            "Page types usage"
        );
        assert!(find_report("no-such-report").is_none());
    }

    #[test]
    fn registered_reports_sorted_by_title() {
        let titles: Vec<&str> = registered_reports().iter().map(|r| r.title()).collect();
        let mut sorted = titles.clone();
        sorted.sort_unstable();
        assert_eq!(titles, sorted, "registry should be title-sorted");
    }

    #[test]
    fn column_kinds_have_stable_tokens() {
        assert_eq!(ColumnKind::Text.as_str(), "text");
        assert_eq!(ColumnKind::Number.as_str(), "number");
        assert_eq!(ColumnKind::Status.as_str(), "status");
        assert_eq!(ColumnKind::DateTime.as_str(), "datetime");
    }

    #[test]
    fn column_builders_set_kind() {
        assert_eq!(ReportColumn::text("a", "A").kind, ColumnKind::Text);
        assert_eq!(ReportColumn::number("a", "A").kind, ColumnKind::Number);
        assert_eq!(ReportColumn::status("a", "A").kind, ColumnKind::Status);
        assert_eq!(ReportColumn::datetime("a", "A").kind, ColumnKind::DateTime);
    }

    #[test]
    fn every_report_declares_columns() {
        for r in registered_reports() {
            assert!(!r.columns().is_empty(), "{} has no columns", r.slug());
        }
    }
}
