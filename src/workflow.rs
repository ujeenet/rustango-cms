//! Multi-step approval workflows.
//!
//! Editors compose a `Workflow` from ordered `WorkflowTask`s. When
//! they submit a page for review, a `WorkflowState` row tracks
//! "where in the workflow this page is right now," and a `TaskState`
//! row records the verdict on each step.
//!
//! This module holds the data model + admin CRUD; the page-editor UI
//! (Submit / Approve / Reject buttons), notifications and
//! reapproval-on-edit build on it.
//!
//! ## Shape
//!
//! ```text
//! Workflow ─< WorkflowTask (ordered by sort_order)
//!     │
//!     └─< WorkflowState (one per page-in-flight)
//!             │
//!             └─< TaskState (one per step decision)
//! ```
//!
//! ## States
//!
//! `WorkflowState.status` is one of: `in_progress`, `approved`,
//! `needs_changes`, `cancelled`. `TaskState.status` is one of:
//! `in_progress`, `approved`, `rejected`, `skipped`. The string
//! representation is stable on disk so future ALTERs don't break
//! existing rows.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------
// Status enums — stored as short strings for grep-ability + cheap
// migrations.
// ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    InProgress,
    Approved,
    NeedsChanges,
    Cancelled,
}

impl WorkflowStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Approved => "approved",
            Self::NeedsChanges => "needs_changes",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "in_progress" => Self::InProgress,
            "approved" => Self::Approved,
            "needs_changes" => Self::NeedsChanges,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    InProgress,
    Approved,
    Rejected,
    Skipped,
}

impl TaskStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Skipped => "skipped",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "in_progress" => Self::InProgress,
            "approved" => Self::Approved,
            "rejected" => Self::Rejected,
            "skipped" => Self::Skipped,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------
// Workflow — the template. Composed of an ordered list of tasks.
// ---------------------------------------------------------------

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_workflow",
    app = "cms",
    display = "name",
    admin(
        list_display = "name, active, created_at",
        ordering = "name",
        list_filter = "active",
    )
)]
pub struct Workflow {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Human-readable label shown in the admin (e.g.
    /// `"Editorial review"`, `"Legal sign-off"`).
    #[rustango(max_length = 100, index)]
    pub name: String,

    /// Optional one-line explanation surfaced in the workflow list.
    #[rustango(max_length = 255)]
    pub description: String,

    /// Inactive workflows stay in the DB so old `WorkflowState` rows
    /// don't lose their parent FK, but new submissions filter them
    /// out.
    pub active: bool,

    /// When true, a content edit on a page in the Approved state of
    /// this workflow restarts the workflow from the first task.
    /// Configured per workflow rather than globally.
    pub require_reapproval_on_edit: bool,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,

    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

// ---------------------------------------------------------------
// WorkflowTask — one ordered step of a Workflow. v1 only ships the
// "group approval" shape (any member of a role can approve). Custom
// task kinds (auto-approve, conditional skip, external webhook)
// surface in a later PR via a `kind` discriminator.
// ---------------------------------------------------------------

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_workflow_task",
    app = "cms",
    display = "name",
    admin(
        list_display = "workflow_id, sort_order, name, role_id",
        ordering = "workflow_id, sort_order",
        list_filter = "workflow_id",
    )
)]
pub struct WorkflowTask {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_workflow", on = "id", index)]
    pub workflow_id: i64,

    /// Display name on the action bar ("Editor review",
    /// "Legal review", "Final publish").
    #[rustango(max_length = 100)]
    pub name: String,

    /// Role whose members can approve / reject this step. v1 keeps
    /// the binding to `rustango_roles`; v2 may widen to "any user"
    /// or "named user."
    #[rustango(fk = "rustango_roles", on = "id", index)]
    pub role_id: i64,

    /// Lower numbers run first. Siblings within the same workflow
    /// share `workflow_id` and order by this column.
    pub sort_order: i32,

    /// Task-kind discriminator. Maps to one of the registered
    /// [`crate::task_kind::TaskKind`] implementations:
    ///   * `"group_approval"` — default; a role member must click
    ///     approve / reject via the existing UI.
    ///   * `"auto_approve"` — completes immediately when assigned,
    ///     no human interaction.
    ///   * `"webhook_notify"` — fires `webhook_url` (best-effort)
    ///     when assigned and then auto-completes; external systems
    ///     can post their decision back through the usual approve
    ///     route.
    ///   * Any string a host registered via `register_task_kind!`.
    #[rustango(max_length = 64, default = "'group_approval'")]
    pub kind: String,

    /// URL to POST when this task is assigned. Used by
    /// `webhook_notify`; other kinds ignore. Empty when unused.
    #[rustango(max_length = 500, default = "''")]
    pub webhook_url: String,
}

// ---------------------------------------------------------------
// WorkflowState — per page, per submission. A page can have at most
// one *active* WorkflowState (status != Approved && != Cancelled);
// historical rows are kept for audit.
// ---------------------------------------------------------------

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_workflow_state",
    app = "cms",
    display = "page_id",
    admin(
        list_display = "page_id, workflow_id, status, current_task_id, requested_by, requested_at",
        ordering = "-requested_at",
        list_filter = "status, workflow_id",
    )
)]
pub struct WorkflowState {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_page", on = "id", index)]
    pub page_id: i64,

    #[rustango(fk = "cms_workflow", on = "id", index)]
    pub workflow_id: i64,

    /// `WorkflowStatus::as_str()`.
    #[rustango(max_length = 16, index)]
    pub status: String,

    /// The task currently waiting on a decision. `None` when the
    /// workflow finished (approved or cancelled).
    pub current_task_id: Option<i64>,

    /// Editor who submitted the page for review.
    #[rustango(fk = "rustango_users", on = "id", index)]
    pub requested_by: i64,

    #[rustango(auto_now_add)]
    pub requested_at: Auto<DateTime<Utc>>,

    /// Filled when the workflow reaches a terminal state
    /// (approved / cancelled). Nullable while in flight.
    pub finished_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------
// TaskState — one row per WorkflowTask decision on a given
// WorkflowState. Carries the reviewer + a free-form comment.
// ---------------------------------------------------------------

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_task_state",
    app = "cms",
    display = "id",
    admin(
        list_display = "workflow_state_id, task_id, status, decided_by, decided_at",
        ordering = "-decided_at",
        list_filter = "status",
    )
)]
pub struct TaskState {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_workflow_state", on = "id", index)]
    pub workflow_state_id: i64,

    #[rustango(fk = "cms_workflow_task", on = "id", index)]
    pub task_id: i64,

    /// `TaskStatus::as_str()`.
    #[rustango(max_length = 16, index)]
    pub status: String,

    /// Reviewer's note shown in the audit panel. Empty string =
    /// "no comment."
    pub comment: String,

    /// Reviewer who set this status. `None` while the task is still
    /// in_progress.
    pub decided_by: Option<i64>,

    pub decided_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------
// Helpers used by the page-editor handlers.
// ---------------------------------------------------------------

/// Look up the active (non-finished, non-cancelled) `WorkflowState`
/// for a page. Returns at most one row — a page is in at most one
/// active workflow at a time. Historical rows (approved / cancelled)
/// are filtered out.
///
/// # Errors
/// Driver / query failures.
pub async fn active_state_for_page(
    pool: &rustango::sql::Pool,
    page_id: i64,
) -> Result<Option<WorkflowState>, rustango::sql::ExecError> {
    // Judged on the latest round only: a resubmission starts a new
    // state and leaves the earlier `needs_changes` one behind, which must
    // not come back once the new round is approved.
    Ok(latest_state_for_page(pool, page_id).await?.filter(|st| {
        st.status == WorkflowStatus::InProgress.as_str() || st.status == WorkflowStatus::NeedsChanges.as_str()
    }))
}

/// The page's most recent review round, whatever its outcome.
///
/// # Errors
/// Driver / query failures.
pub async fn latest_state_for_page(
    pool: &rustango::sql::Pool,
    page_id: i64,
) -> Result<Option<WorkflowState>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let rows: Vec<WorkflowState> = WorkflowState::objects()
        .where_(WorkflowState::page_id.eq(page_id))
        .order_by(&[("requested_at", true), ("id", true)]) // newest first
        .fetch(pool)
        .await?;
    Ok(rows.into_iter().next())
}

/// Every decision on the page across all its review rounds, oldest first
/// — a rejection's reason stays visible after the resubmission.
///
/// # Errors
/// Driver / query failures.
pub async fn history_for_page(
    pool: &rustango::sql::Pool,
    page_id: i64,
) -> Result<Vec<TaskState>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let ids: Vec<i64> = WorkflowState::objects()
        .where_(WorkflowState::page_id.eq(page_id))
        .fetch(pool)
        .await?
        .into_iter()
        .filter_map(|st| st.id.get().copied())
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    TaskState::objects()
        .where_(TaskState::workflow_state_id.is_in(ids))
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
}

/// Look up a workflow by its (case-sensitive) `name`. The name acts
/// as the slug returned from `PageTypeHandler::workflow_slug()`. Only
/// returns active workflows; inactive ones are treated as missing so
/// editors can disable a workflow without invalidating every
/// page-type binding.
///
/// # Errors
/// Driver / query failures.
pub async fn find_by_name(
    pool: &rustango::sql::Pool,
    name: &str,
) -> Result<Option<Workflow>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let rows: Vec<Workflow> = Workflow::objects()
        .where_(Workflow::name.eq(name.to_owned()))
        .where_(Workflow::active.eq(true))
        .fetch(pool)
        .await?;
    Ok(rows.into_iter().next())
}

/// The active workflow pages of `page_type_id` go through before they
/// are published, if any: the type's handler slug or the one chosen in
/// the admin, resolved to an active workflow with at least one step.
/// Pages of such a type go live only through review — except for
/// superusers (the callers check).
///
/// # Errors
/// Driver / query failures.
pub async fn review_workflow_for_type(
    pool: &rustango::sql::Pool,
    page_type_id: i64,
) -> Result<Option<Workflow>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    
    let Some(pt) = crate::page_type_model::PageType::objects()
        .where_(crate::page_type_model::PageType::id.eq(page_type_id))
        .first(pool)
        .await?
    else {
        return Ok(None);
    };
    let Some(name) = pt.workflow_name() else {
        return Ok(None);
    };
    let Some(wf) = find_by_name(pool, &name).await? else {
        return Ok(None);
    };
    let has_steps = !tasks_for(pool, wf.id.get().copied().unwrap_or_default()).await?.is_empty();
    Ok(has_steps.then_some(wf))
}

/// The page types chosen in the admin to go through the workflow `name`.
///
/// # Errors
/// Driver / query failures.
pub async fn page_types_using(
    pool: &rustango::sql::Pool,
    name: &str,
) -> Result<Vec<crate::page_type_model::PageType>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    crate::page_type_model::PageType::objects()
        .where_(crate::page_type_model::PageType::workflow.eq(name.to_owned()))
        .fetch(pool)
        .await
}

/// Point the page types that use workflow `old` at its new name `new`
/// (page types store the name). Returns how many moved.
///
/// # Errors
/// Driver / query failures.
pub async fn rebind_page_types(
    pool: &rustango::sql::Pool,
    old: &str,
    new: &str,
) -> Result<usize, rustango::sql::ExecError> {
    let types = page_types_using(pool, old).await?;
    let n = types.len();
    for mut pt in types {
        new.clone_into(&mut pt.workflow);
        pt.save_pool(pool).await?;
    }
    Ok(n)
}

/// Fetch the ordered task list for a workflow id.
///
/// # Errors
/// Driver / query failures.
pub async fn tasks_for(
    pool: &rustango::sql::Pool,
    workflow_id: i64,
) -> Result<Vec<WorkflowTask>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    WorkflowTask::objects()
        .where_(WorkflowTask::workflow_id.eq(workflow_id))
        .order_by(&[("sort_order", false), ("id", false)])
        .fetch(pool)
        .await
}

/// Submit a page for review: create a new `WorkflowState` pinned to
/// the first task + an `in_progress` `TaskState`. Caller must have
/// already verified the page is in Draft or NeedsChanges state and
/// the workflow has at least one task.
///
/// # Errors
/// Driver / query failures.
pub async fn submit_for_review(
    pool: &rustango::sql::Pool,
    page_id: i64,
    workflow: &Workflow,
    tasks: &[WorkflowTask],
    requested_by: i64,
) -> Result<WorkflowState, rustango::sql::ExecError> {
    use rustango::sql::Auto;

    let workflow_id = workflow.id.get().copied().unwrap_or_default();
    let first_task_id = tasks.first().and_then(|t| t.id.get().copied());

    let mut state = WorkflowState {
        id: Auto::Unset,
        page_id,
        workflow_id,
        status: WorkflowStatus::InProgress.as_str().to_owned(),
        current_task_id: first_task_id,
        requested_by,
        requested_at: Auto::Unset,
        finished_at: None,
    };
    state.insert_pool(pool).await?;

    if let Some(task_id) = first_task_id {
        let state_id = state.id.get().copied().unwrap_or_default();
        let mut ts = TaskState {
            id: Auto::Unset,
            workflow_state_id: state_id,
            task_id,
            status: TaskStatus::InProgress.as_str().to_owned(),
            comment: String::new(),
            decided_by: None,
            decided_at: None,
        };
        ts.insert_pool(pool).await?;
    }

    // #191 — fire the first task's on_assign hook. If the kind
    // auto-satisfies, advance past it (and through any chained
    // auto-approve tasks) before returning.
    advance_through_auto_approve_chain(pool, &mut state, tasks, page_id, requested_by).await?;
    Ok(state)
}

/// Auto-approve helper: when the current task's kind reports
/// `AutoApprove`, mark it approved and advance to the next task,
/// repeating until we hit a `WaitForHuman` or the workflow
/// finishes. Called from [`submit_for_review`] after the first
/// task is assigned and from [`approve_current`] after a human
/// approves and the next task gets assigned.
async fn advance_through_auto_approve_chain(
    pool: &rustango::sql::Pool,
    state: &mut WorkflowState,
    tasks: &[WorkflowTask],
    page_id: i64,
    requested_by: i64,
) -> Result<(), rustango::sql::ExecError> {
    use crate::page::Page;
    use rustango::core::Column as _;
    use rustango::sql::Auto;

    // Resolve the live Page row once — every kind's on_assign hook
    // wants page context.
    let page: Page = match Page::objects()
        .where_(Page::id.eq(page_id))
        .first(pool)
        .await?
    {
        Some(p) => p,
        None => return Ok(()),
    };

    loop {
        let Some(current_task_id) = state.current_task_id else {
            return Ok(());
        };
        let Some(current_task) = tasks
            .iter()
            .find(|t| t.id.get().copied() == Some(current_task_id))
        else {
            return Ok(());
        };
        let kind = crate::task_kind::find(&current_task.kind)
            .unwrap_or_else(|| Box::new(crate::task_kind::GroupApprovalKind));
        let ctx = crate::task_kind::AssignedTask {
            task: current_task,
            page: &page,
            requested_by,
        };
        match kind.on_assign(&ctx) {
            crate::task_kind::AssignOutcome::WaitForHuman => return Ok(()),
            crate::task_kind::AssignOutcome::AutoApprove => {
                // Mark the in-flight TaskState approved with no
                // decided_by (system action).
                let state_id = state.id.get().copied().unwrap_or_default();
                let now = Utc::now();
                if let Some(mut ts) = TaskState::objects()
                    .where_(TaskState::workflow_state_id.eq(state_id))
                    .where_(TaskState::task_id.eq(current_task_id))
                    .where_(TaskState::status.eq(TaskStatus::InProgress.as_str().to_owned()))
                    .first(pool)
                    .await?
                {
                    ts.status = TaskStatus::Approved.as_str().to_owned();
                    ts.decided_by = None;
                    ts.decided_at = Some(now);
                    ts.comment = format!("auto-approved by `{}` task kind", current_task.kind);
                    ts.save_pool(pool).await?;
                }

                let current_sort = current_task.sort_order;
                let next = tasks
                    .iter()
                    .filter(|t| t.sort_order > current_sort)
                    .min_by_key(|t| t.sort_order);
                if let Some(next_task) = next {
                    let next_id = next_task.id.get().copied().unwrap_or_default();
                    state.current_task_id = Some(next_id);
                    // #73 — save_partial (not save_pool) so the unchanged
                    // `requested_by -> rustango_users` FK is not re-bound/re-validated;
                    // a full-row UPDATE 787s on SQLite when the original submitter's
                    // user row is gone (deleted / deactivated-and-purged).
                    state
                        .save_partial(&["status", "current_task_id", "finished_at"], pool)
                        .await?;
                    let mut ts = TaskState {
                        id: Auto::Unset,
                        workflow_state_id: state_id,
                        task_id: next_id,
                        status: TaskStatus::InProgress.as_str().to_owned(),
                        comment: String::new(),
                        decided_by: None,
                        decided_at: None,
                    };
                    ts.insert_pool(pool).await?;
                    // Loop continues — the next task might also be
                    // an auto-approve, in which case we chain
                    // through.
                } else {
                    state.current_task_id = None;
                    state.status = WorkflowStatus::Approved.as_str().to_owned();
                    state.finished_at = Some(now);
                    // #73 — save_partial (not save_pool) so the unchanged
                    // `requested_by -> rustango_users` FK is not re-bound/re-validated;
                    // a full-row UPDATE 787s on SQLite when the original submitter's
                    // user row is gone (deleted / deactivated-and-purged).
                    state
                        .save_partial(&["status", "current_task_id", "finished_at"], pool)
                        .await?;
                    return Ok(());
                }
            }
        }
    }
}

/// Outcome of approving the current task. `Advanced` means the
/// workflow moved to the next task; `Finished` means every task
/// approved and the workflow ended (the caller flips the page to
/// Published).
#[derive(Debug, Clone)]
pub enum ApproveOutcome {
    Advanced { next_task_id: i64 },
    Finished,
    /// There is no step to decide, so nothing changed: another request
    /// decided it first (a double-click, or two reviewers at once),
    /// or the current task is no longer in the workflow — finished, or
    /// removed — which a fallback position would have restarted.
    AlreadyDecided,
}

/// Decide the in-progress `TaskState` for `task_id` — one conditional
/// UPDATE, so of two concurrent decisions exactly one flips the row.
/// Returns whether this call was the one.
async fn claim_task(
    pool: &rustango::sql::Pool,
    state_id: i64,
    task_id: i64,
    to: TaskStatus,
    decided_by: i64,
    comment: &str,
) -> Result<bool, rustango::sql::ExecError> {
    use rustango::core::{Column as _, SqlValue};
    use rustango::sql::UpdaterPool as _;
    let changed = TaskState::objects()
        .where_(TaskState::workflow_state_id.eq(state_id))
        .where_(TaskState::task_id.eq(task_id))
        .where_(TaskState::status.eq(TaskStatus::InProgress.as_str().to_owned()))
        .update()
        .set("status", SqlValue::from(to.as_str()))
        .set("decided_by", SqlValue::from(decided_by))
        .set("decided_at", SqlValue::from(Utc::now()))
        .set("comment", SqlValue::from(comment.to_owned()))
        .execute_pool(pool)
        .await?;
    Ok(changed > 0)
}

/// Approve the current task on a `WorkflowState`. Marks the in-flight
/// `TaskState` Approved, advances to the next task (or finishes the
/// workflow when no next task exists).
///
/// # Errors
/// Driver / query failures.
pub async fn approve_current(
    pool: &rustango::sql::Pool,
    state: &mut WorkflowState,
    tasks: &[WorkflowTask],
    decided_by: i64,
    comment: &str,
) -> Result<ApproveOutcome, rustango::sql::ExecError> {
    use rustango::sql::Auto;

    let state_id = state.id.get().copied().unwrap_or_default();
    let current_task_id = state.current_task_id.unwrap_or_default();
    let now = Utc::now();

    // A state whose current task isn't in this workflow (finished, or
    // the task was removed) has no step to approve; a fallback position
    // would restart it from the first task (#770).
    let Some(current_sort) = tasks
        .iter()
        .find(|t| t.id.get().copied() == Some(current_task_id))
        .map(|t| t.sort_order)
    else {
        return Ok(ApproveOutcome::AlreadyDecided);
    };

    // Claim the in-flight TaskState. Only the request that flips it goes
    // on to advance; a second approve of the same step (or one racing
    // it) must not advance again and approve the next step unreviewed.
    if !claim_task(pool, state_id, current_task_id, TaskStatus::Approved, decided_by, comment).await? {
        return Ok(ApproveOutcome::AlreadyDecided);
    }

    // Find the next task (sort_order > current).
    let next = tasks
        .iter()
        .filter(|t| t.sort_order > current_sort)
        .min_by_key(|t| t.sort_order);

    if let Some(next_task) = next {
        let next_id = next_task.id.get().copied().unwrap_or_default();
        state.current_task_id = Some(next_id);
        state.status = WorkflowStatus::InProgress.as_str().to_owned();
        // #73 — save_partial (not save_pool) so the unchanged
        // `requested_by -> rustango_users` FK is not re-bound/re-validated;
        // a full-row UPDATE 787s on SQLite when the original submitter's
        // user row is gone (deleted / deactivated-and-purged).
        state
            .save_partial(&["status", "current_task_id", "finished_at"], pool)
            .await?;

        let mut ts = TaskState {
            id: Auto::Unset,
            workflow_state_id: state_id,
            task_id: next_id,
            status: TaskStatus::InProgress.as_str().to_owned(),
            comment: String::new(),
            decided_by: None,
            decided_at: None,
        };
        ts.insert_pool(pool).await?;

        // #191 — if the next task is an auto-approve kind (or chain
        // of them), advance through them right now. The final state
        // is either a human-required task or workflow Finished.
        advance_through_auto_approve_chain(pool, state, tasks, state.page_id, decided_by).await?;
        if state.status == WorkflowStatus::Approved.as_str() {
            Ok(ApproveOutcome::Finished)
        } else if let Some(advanced_id) = state.current_task_id {
            Ok(ApproveOutcome::Advanced {
                next_task_id: advanced_id,
            })
        } else {
            Ok(ApproveOutcome::Finished)
        }
    } else {
        state.current_task_id = None;
        state.status = WorkflowStatus::Approved.as_str().to_owned();
        state.finished_at = Some(now);
        // #73 — save_partial (not save_pool) so the unchanged
        // `requested_by -> rustango_users` FK is not re-bound/re-validated;
        // a full-row UPDATE 787s on SQLite when the original submitter's
        // user row is gone (deleted / deactivated-and-purged).
        state
            .save_partial(&["status", "current_task_id", "finished_at"], pool)
            .await?;
        Ok(ApproveOutcome::Finished)
    }
}

/// Reject the current task. Flips the `TaskState` to Rejected and
/// the `WorkflowState` to NeedsChanges. The submitter can edit and
/// re-submit; the workflow restarts from the first task on next
/// submit. Returns `false`, changing nothing, when the step was already
/// decided.
///
/// # Errors
/// Driver / query failures.
pub async fn reject_current(
    pool: &rustango::sql::Pool,
    state: &mut WorkflowState,
    decided_by: i64,
    comment: &str,
) -> Result<bool, rustango::sql::ExecError> {
    let state_id = state.id.get().copied().unwrap_or_default();
    let current_task_id = state.current_task_id.unwrap_or_default();
    let now = Utc::now();

    // #707 — as in approve_current: only the request that decides the
    // step changes the workflow.
    if !claim_task(pool, state_id, current_task_id, TaskStatus::Rejected, decided_by, comment).await? {
        return Ok(false);
    }

    state.status = WorkflowStatus::NeedsChanges.as_str().to_owned();
    state.finished_at = Some(now);
    // #73 — save_partial (not save_pool): see approve_current; avoids
    // re-validating the unchanged `requested_by -> rustango_users` FK.
    state
        .save_partial(&["status", "current_task_id", "finished_at"], pool)
        .await?;
    Ok(true)
}

/// Cancel an in-flight workflow. Marks the `WorkflowState` Cancelled
/// and any in-progress `TaskState` Skipped. Used when an editor
/// pulls the page out of review entirely.
///
/// Returns `false`, changing nothing, when the step was already decided.
///
/// # Errors
/// Driver / query failures.
pub async fn cancel(
    pool: &rustango::sql::Pool,
    state: &mut WorkflowState,
    decided_by: i64,
) -> Result<(), rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let state_id = state.id.get().copied().unwrap_or_default();
    let now = Utc::now();

    let inflight: Vec<TaskState> = TaskState::objects()
        .where_(TaskState::workflow_state_id.eq(state_id))
        .where_(TaskState::status.eq(TaskStatus::InProgress.as_str().to_owned()))
        .fetch(pool)
        .await?;
    for mut ts in inflight {
        ts.status = TaskStatus::Skipped.as_str().to_owned();
        ts.decided_by = Some(decided_by);
        ts.decided_at = Some(now);
        ts.save_pool(pool).await?;
    }

    state.status = WorkflowStatus::Cancelled.as_str().to_owned();
    state.finished_at = Some(now);
    state.current_task_id = None;
    // #73 — save_partial (not save_pool): see approve_current; avoids
    // re-validating the unchanged `requested_by -> rustango_users` FK.
    state
        .save_partial(&["status", "current_task_id", "finished_at"], pool)
        .await?;
    // A change held for this review is dropped with it.
    crate::pending_change::discard(pool, state.page_id).await?;
    Ok(())
}

/// True when this page has been through a workflow that finished
/// approved (i.e. it's a published-via-workflow page). Editors with
/// `Workflow.require_reapproval_on_edit` set use this signal to
/// decide whether the next edit kicks off a new review cycle.
///
/// # Errors
/// Driver / query failures.
pub async fn has_prior_approval(
    pool: &rustango::sql::Pool,
    page_id: i64,
) -> Result<bool, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let rows: Vec<WorkflowState> = WorkflowState::objects()
        .where_(WorkflowState::page_id.eq(page_id))
        .where_(WorkflowState::status.eq(WorkflowStatus::Approved.as_str().to_owned()))
        .fetch(pool)
        .await?;
    Ok(!rows.is_empty())
}

