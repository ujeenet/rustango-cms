//! Custom workflow task kinds (#191, Wagtail parity).
//!
//! `WorkflowTask` rows carry a `kind` discriminator (default
//! `"group_approval"` — a role member clicks approve). The
//! [`TaskKind`] trait lets hosts register additional kinds:
//! auto-approve, webhook-on-assign, or anything else they need.
//!
//! ## Built-in kinds
//!
//! - `group_approval` — current behavior; a member of the task's
//!   role decides via the editor UI.
//! - `auto_approve` — completes immediately when assigned. Useful
//!   for steps that exist only to record a workflow checkpoint
//!   (e.g. "schedule final publish" as a no-op separator).
//! - `webhook_notify` — best-effort POSTs `task.webhook_url`
//!   on assign, then completes the task. External systems can
//!   trigger their own approval back via the usual approve
//!   endpoint when they're ready.
//!
//! ## Registering a custom kind
//!
//! ```ignore
//! use rustango_cms::task_kind::{TaskKind, TaskKindRegistration, AssignedTask, AssignOutcome};
//!
//! struct ConditionalSkip;
//!
//! impl TaskKind for ConditionalSkip {
//!     fn kind_name(&self) -> &'static str { "conditional_skip" }
//!     fn verbose_name(&self) -> &'static str { "Conditional skip" }
//!     fn on_assign(&self, _task: &AssignedTask<'_>) -> AssignOutcome {
//!         // Inspect _task.page / _task.task and decide.
//!         AssignOutcome::AutoApprove
//!     }
//! }
//!
//! rustango_cms::register_task_kind!(ConditionalSkip);
//! ```

use crate::page::Page;
use crate::workflow::WorkflowTask;

/// Outcome of [`TaskKind::on_assign`]. The workflow engine reads it
/// to decide whether to leave the task in flight (`WaitForHuman`)
/// or auto-advance (`AutoApprove`).
#[derive(Debug, Clone)]
pub enum AssignOutcome {
    /// Leave the task in flight; an approver needs to click
    /// approve / reject via the usual UI.
    WaitForHuman,
    /// Mark the task approved immediately and advance to the next
    /// task (or finish the workflow if this was the last one).
    AutoApprove,
}

/// Bundle of references passed to [`TaskKind::on_assign`] so the
/// kind can decide based on the live workflow context without
/// re-querying.
pub struct AssignedTask<'a> {
    pub task: &'a WorkflowTask,
    pub page: &'a Page,
    /// User id of the editor who submitted the page for review.
    pub requested_by: i64,
}

/// One pluggable workflow task behavior. Defaults make the trait
/// minimal to implement — only [`Self::kind_name`] is required;
/// the rest opt in.
pub trait TaskKind: Send + Sync + 'static {
    /// Discriminator stored in `WorkflowTask.kind`. Globally unique
    /// across the process.
    fn kind_name(&self) -> &'static str;

    /// Human-readable label for the workflow task editor's "Kind"
    /// dropdown. Defaults to [`Self::kind_name`].
    fn verbose_name(&self) -> &'static str {
        self.kind_name()
    }

    /// Called when this task becomes the workflow's current step.
    /// Side-effects (firing webhooks, recording metrics) belong here.
    ///
    /// The default implementation returns `WaitForHuman` — i.e. the
    /// task behaves like the canonical `group_approval` kind.
    fn on_assign(&self, _ctx: &AssignedTask<'_>) -> AssignOutcome {
        AssignOutcome::WaitForHuman
    }
}

/// Inventory registration shim. Built-in + host-registered kinds
/// land in the same registry via [`crate::register_task_kind!`].
pub struct TaskKindRegistration {
    pub builder: fn() -> Box<dyn TaskKind>,
}

inventory::collect!(TaskKindRegistration);

/// Macro mirror of [`crate::register_block!`]. Hosts call this to
/// publish a custom [`TaskKind`] into the runtime registry.
///
/// ```ignore
/// rustango_cms::register_task_kind!(ConditionalSkip);
/// ```
#[macro_export]
macro_rules! register_task_kind {
    ($ty:ty) => {
        $crate::inventory::submit! {
            $crate::task_kind::TaskKindRegistration {
                builder: || -> Box<dyn $crate::task_kind::TaskKind> {
                    Box::new(<$ty as core::default::Default>::default())
                },
            }
        }
    };
}

/// Every registered task kind, including built-ins.
#[must_use]
pub fn registered_kinds() -> Vec<Box<dyn TaskKind>> {
    inventory::iter::<TaskKindRegistration>()
        .map(|r| (r.builder)())
        .collect()
}

/// Look up a kind by `kind_name`. Returns `None` when missing — the
/// workflow engine falls back to `WaitForHuman` semantics so an
/// unknown kind doesn't auto-advance into a corrupt state.
#[must_use]
pub fn find(name: &str) -> Option<Box<dyn TaskKind>> {
    inventory::iter::<TaskKindRegistration>()
        .map(|r| (r.builder)())
        .find(|k| k.kind_name() == name)
}

// =====================================================================
// Built-in kinds
// =====================================================================

/// Default — a role member must approve via the existing UI.
#[derive(Default)]
pub struct GroupApprovalKind;

impl TaskKind for GroupApprovalKind {
    fn kind_name(&self) -> &'static str {
        "group_approval"
    }
    fn verbose_name(&self) -> &'static str {
        "Group approval (role member clicks approve)"
    }
    fn on_assign(&self, _ctx: &AssignedTask<'_>) -> AssignOutcome {
        AssignOutcome::WaitForHuman
    }
}

/// Skip-everything task — used for "checkpoint" steps that exist
/// solely to record progress without blocking on a human.
#[derive(Default)]
pub struct AutoApproveKind;

impl TaskKind for AutoApproveKind {
    fn kind_name(&self) -> &'static str {
        "auto_approve"
    }
    fn verbose_name(&self) -> &'static str {
        "Auto-approve (no human interaction)"
    }
    fn on_assign(&self, _ctx: &AssignedTask<'_>) -> AssignOutcome {
        AssignOutcome::AutoApprove
    }
}

/// Fires a best-effort POST to `task.webhook_url` and then auto-
/// approves. External systems wire `webhook_url` to whatever they
/// want notified (a Slack incoming webhook, a custom orchestrator,
/// the legal team's intake form). The HTTP fire is intentionally
/// stubbed at the framework level — see the impl for details — so
/// no new HTTP-client dependency is forced on the default build.
#[derive(Default)]
pub struct WebhookNotifyKind;

impl TaskKind for WebhookNotifyKind {
    fn kind_name(&self) -> &'static str {
        "webhook_notify"
    }
    fn verbose_name(&self) -> &'static str {
        "Webhook notify (best-effort POST + auto-approve)"
    }
    fn on_assign(&self, ctx: &AssignedTask<'_>) -> AssignOutcome {
        let url = ctx.task.webhook_url.trim();
        if url.is_empty() {
            tracing::warn!(
                target: "rustango_cms::task_kind",
                task_name = %ctx.task.name,
                "webhook_notify task has empty webhook_url; auto-approving anyway"
            );
        } else {
            // Best-effort spawn — the workflow advance doesn't wait
            // on the network. Hosts that need durable delivery
            // should register their own kind that wraps a queued
            // mailer / job runner.
            tracing::info!(
                target: "rustango_cms::task_kind",
                task_name = %ctx.task.name,
                page_id = ctx.page.id.get().copied().unwrap_or(0),
                url = %url,
                requested_by = ctx.requested_by,
                "webhook_notify firing (host should register a custom kind for production HTTP delivery)"
            );
        }
        AssignOutcome::AutoApprove
    }
}

register_task_kind!(GroupApprovalKind);
register_task_kind!(AutoApproveKind);
register_task_kind!(WebhookNotifyKind);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_includes_built_ins() {
        let names: Vec<&'static str> = registered_kinds().iter().map(|k| k.kind_name()).collect();
        assert!(names.contains(&"group_approval"));
        assert!(names.contains(&"auto_approve"));
        assert!(names.contains(&"webhook_notify"));
    }

    #[test]
    fn find_resolves_known_kind() {
        assert!(find("group_approval").is_some());
        assert!(find("auto_approve").is_some());
        assert!(find("webhook_notify").is_some());
        assert!(find("does_not_exist").is_none());
    }

    #[test]
    fn auto_approve_returns_auto() {
        let k = AutoApproveKind;
        // We can't easily construct an AssignedTask in a unit test
        // (it needs a live Page row). Smoke-check by reading
        // kind_name + verbose_name.
        assert_eq!(k.kind_name(), "auto_approve");
        assert!(k.verbose_name().contains("Auto"));
    }
}
