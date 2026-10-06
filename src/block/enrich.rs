//! Host-registrable async stream enrichment.
//!
//! [`Block::render`](super::Block::render) is synchronous, so a block
//! cannot fetch anything while it renders. The CMS already works around
//! that for its own chooser blocks: `enrich_chooser_refs_async` walks a
//! parsed stream **before** the sync render and writes resolved data
//! into each block's `value` under `_`-prefixed keys, so the template
//! only ever reads what is already there.
//!
//! That walker knows a fixed set of block types. This hook opens the
//! same moment to host applications: a block that needs a query result,
//! a third-party API response or anything else async can fill its own
//! value in, then render synchronously like every other block.
//!
//! ```ignore
//! fn enrich<'a>(stream: &'a mut serde_json::Value, pool: &'a Pool)
//!     -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
//! {
//!     Box::pin(async move {
//!         // walk `stream`, fill `value._rows` on blocks you own
//!     })
//! }
//! rustango_cms::register_block_enricher!(enrich);
//! ```
//!
//! ## Contract
//!
//! * **Own your own block types.** Enrichers all see the same stream.
//!   Match on `type` and touch nothing else; the CMS's own chooser
//!   enrichment has already run by this point and its `_` keys are not
//!   yours to rewrite.
//! * **Use the `_` prefix**, as the built-in enrichment does, so
//!   injected keys can never collide with an author-defined field.
//! * **Recurse yourself** if your blocks can nest inside `Stream` /
//!   `Repeat` fields. The fan-out hands each enricher the whole stream
//!   once rather than guessing at a traversal that fits every host.
//! * **Don't fail the page.** Report a problem by writing it into the
//!   value (`_error`) so the template can render it; the enrichment
//!   moment has no error channel, because a report that cannot load is
//!   a broken widget, not a broken page.
//!
//! Enrichers run **sequentially** in registration order, and each one
//! gets `&mut` to the whole stream — concurrency across enrichers would
//! need a lock per call and buy nothing, since the work inside one
//! enricher (which is where the fan-out actually pays) is free to be as
//! concurrent as it likes.

use std::future::Future;
use std::pin::Pin;

use rustango::sql::Pool;

/// Boxed future returned by a [`BlockEnricher`]. Borrows both the
/// stream it mutates and the context it reads from, so an enricher can
/// hold either across an await without cloning.
pub type EnrichFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// What an enricher is told about the render it is part of.
///
/// A struct rather than a bare `&Pool` so this can gain fields without
/// breaking every registered enricher — and because the pool alone is
/// not enough to answer "where am I?", which is the first thing a block
/// that inherits something from its page needs to know.
///
/// `#[non_exhaustive]` so a new field breaks no one (#665): a host that
/// drives the walker builds one with [`EnrichCtx::new`].
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct EnrichCtx<'a> {
    /// The tenant's database.
    pub pool: &'a Pool,
    /// The tenant this render belongs to, or `""` when the caller did
    /// not supply one (the non-`_with` pre-render, or a host driving
    /// the walker itself).
    ///
    /// An enricher that caches **must** include this in its key. The
    /// pool is already tenant-specific, so the query is safe either way
    /// — but a process-wide cache keyed on a row id alone will serve one
    /// tenant another tenant's data, and that is not a failure anything
    /// downstream can detect.
    pub tenant: &'a str,
    /// The page whose stream this is, when there is one. `None` for
    /// renders with no page behind them — a preview of unsaved content,
    /// or a host calling the pre-render directly.
    pub page_id: Option<i64>,
}

impl<'a> EnrichCtx<'a> {
    /// The context for one render: the tenant's pool, its slug, and the
    /// page the stream belongs to, if any.
    #[must_use]
    pub fn new(pool: &'a Pool, tenant: &'a str, page_id: Option<i64>) -> Self {
        Self { pool, tenant, page_id }
    }
}

/// A host-registered async enrichment pass over one parsed stream.
///
/// A plain `fn` pointer rather than a boxed closure: the registration
/// is collected by `inventory` at link time, which needs a value that
/// can be built in a `static` initializer.
#[derive(Clone, Copy)]
pub struct BlockEnricher(
    pub for<'a> fn(&'a mut serde_json::Value, EnrichCtx<'a>) -> EnrichFuture<'a>,
);

inventory::collect!(BlockEnricher);

/// Register a [`BlockEnricher`] at compile time.
///
/// The submission lives in a static initializer, so a binary that never
/// otherwise mentions the containing module can have it stripped by the
/// linker. Reference the module from `main` (the `blocks::link()`
/// convention) to keep it.
#[macro_export]
macro_rules! register_block_enricher {
    ($f:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::block::enrich::BlockEnricher($f)
        }
    };
}

/// Run every registered enricher over `stream`, in registration order.
///
/// Called from the public pre-render right after the built-in chooser
/// enrichment, so hosts see a stream whose `page_chooser` / `image` /
/// `snippet_chooser` blocks are already resolved.
///
/// A panicking enricher is **not** caught here, unlike the sync hooks
/// in [`crate::hooks`]: catching across an await point would need the
/// future to be `UnwindSafe`, which a `&mut` borrow of the stream is
/// not. An enricher that panics takes the request down — so keep the
/// fallible parts inside a `Result` and write the failure into the
/// value.
pub async fn fire(stream: &mut serde_json::Value, ctx: EnrichCtx<'_>) {
    for hook in inventory::iter::<BlockEnricher>() {
        (hook.0)(stream, ctx).await;
    }
}

/// Whether any enricher is registered. Lets a caller skip the walk
/// entirely on the overwhelmingly common path where a deployment has
/// registered none.
#[must_use]
pub fn any_registered() -> bool {
    inventory::iter::<BlockEnricher>().next().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The library registers no enricher of its own, so the hook must
    /// cost nothing in a deployment that doesn't use it. The fan-out
    /// with an enricher actually registered is exercised end to end in
    /// `tests/block_enrich.rs` — a registration is process-wide, so it
    /// can't share a test binary with this assertion.
    #[test]
    fn nothing_is_registered_by_the_library_itself() {
        assert!(!any_registered());
    }
}
