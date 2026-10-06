//! Request timing instrumentation — opt-in, off by default.
//!
//! Set `CMS_PERF_LOG=1` to get one log line per request with the total
//! wall time plus a breakdown of the phases the handler recorded:
//!
//! ```text
//! PERF GET /getting-started -> 303 in 3512.4ms | tenant=181.2 locale=182.0
//!      resolve=543.9 restriction=362.1 render=0.0
//! ```
//!
//! The point is attribution. A slow page is nearly always slow for one
//! of two reasons — too many sequential round-trips, or one expensive
//! phase — and a single total tells you neither.
//!
//! Phases are recorded through a task-local, so [`mark`] is a no-op
//! outside a request (tests, CLI commands) and costs an atomic load
//! when the feature is off.

use std::cell::RefCell;
use std::time::Instant;

// tokio is not a direct dependency; the framework re-exports it for
// exactly this reason (same path `#[rustango::main]` resolves through).
rustango::__private_runtime::tokio::task_local! {
    /// Phase marks for the request being served by *this task*.
    ///
    /// Deliberately a task-local, not a thread-local: tokio moves a task
    /// between worker threads at any await point, so thread-local marks
    /// get attributed to whichever request happens to be on that thread
    /// afterwards — the breakdown then looks plausible and is wrong.
    /// A `Vec` rather than a map because order is the useful part.
    static MARKS: RefCell<Vec<(&'static str, f64)>>;
}

/// `CMS_PERF_LOG=1` — read once; this is on the hot path.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        crate::config::flag("PERF_LOG")
    })
}

/// Start a timer. Pair with [`mark`], or drop it to record nothing.
#[must_use]
pub fn start() -> Instant {
    Instant::now()
}

/// Record `name` as having taken the time since `t`. No-op unless
/// `CMS_PERF_LOG` is set.
pub fn mark(name: &'static str, t: Instant) {
    if !enabled() {
        return;
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    // `try_with` so a mark outside a request (CLI, tests) is a no-op
    // rather than a panic.
    let _ = MARKS.try_with(|m| m.borrow_mut().push((name, ms)));
}

/// Take and clear the marks recorded for the current request.
fn take_marks() -> Vec<(&'static str, f64)> {
    MARKS
        .try_with(|m| std::mem::take(&mut *m.borrow_mut()))
        .unwrap_or_default()
}

/// Axum middleware: time the whole request and emit the breakdown.
///
/// Mounted automatically by the CMS routers when `CMS_PERF_LOG` is set,
/// so a host needs no wiring to profile a running site.
pub async fn timing_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if !enabled() {
        return next.run(req).await;
    }
    let method = req.method().clone();
    let path = req
        .uri()
        .path_and_query()
        .map(ToString::to_string)
        .unwrap_or_else(|| req.uri().path().to_owned());

    let t0 = Instant::now();
    // Scope the marks to this request's task, so a handler's phases can
    // never be credited to a neighbouring request.
    let (resp, marks) = MARKS
        .scope(RefCell::new(Vec::new()), async move {
            let resp = next.run(req).await;
            (resp, take_marks())
        })
        .await;
    let total = t0.elapsed().as_secs_f64() * 1000.0;

    // `unattributed` is the gap between the total and the phases the
    // handler declared — middleware, extractors, serialisation, and any
    // path with no marks at all.
    let attributed: f64 = marks.iter().map(|(_, ms)| ms).sum();
    let detail = marks
        .iter()
        .map(|(n, ms)| format!("{n}={ms:.1}"))
        .collect::<Vec<_>>()
        .join(" ");

    tracing::info!(
        target: "cms::perf",
        "PERF {method} {path} -> {status} in {total:.1}ms | {detail} unattributed={gap:.1}",
        status = resp.status().as_u16(),
        gap = (total - attributed).max(0.0),
    );
    resp
}

/// Wrap `router` in the timing middleware when profiling is enabled.
#[must_use]
pub fn instrument(router: axum::Router) -> axum::Router {
    if enabled() {
        router.layer(axum::middleware::from_fn(timing_middleware))
    } else {
        router
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marking_outside_a_request_is_a_no_op() {
        // No task-local scope here: `mark` must not panic, and there is
        // nothing to collect.
        mark("phase", start());
        assert!(take_marks().is_empty());
    }

    #[tokio::test]
    async fn marks_are_scoped_to_their_own_task() {
        // Two concurrent tasks must not see each other's marks — the
        // exact failure a thread-local would produce under tokio.
        let one = MARKS.scope(RefCell::new(Vec::new()), async {
            let _ = MARKS.try_with(|m| m.borrow_mut().push(("a", 1.0)));
            tokio::task::yield_now().await;
            take_marks()
        });
        let two = MARKS.scope(RefCell::new(Vec::new()), async {
            let _ = MARKS.try_with(|m| m.borrow_mut().push(("b", 2.0)));
            tokio::task::yield_now().await;
            take_marks()
        });
        let (a, b) = tokio::join!(one, two);
        assert_eq!(a, vec![("a", 1.0)]);
        assert_eq!(b, vec![("b", 2.0)]);
    }
}
