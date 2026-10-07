//! Stepwise schema migration for block-stored JSON.
//!
//! When a block's [`crate::Block::version`] is bumped, every load
//! comparing the *stored* version on a JSON entry to the *current*
//! version triggers [`apply_migrations`], which walks each integer
//! version between them calling [`crate::Block::migrate`] once per
//! step. Authors write single-version-step migrators only; the
//! framework chains them.
//!
//! ## Wire-format version
//!
//! The JSON envelope of a block entry stores `version: u32` next to
//! `type` / `id` / `value` (explicit per-item versions beat
//! implicit ones). Missing
//! `version` is treated as `1` (the initial baseline).

use serde_json::Value;

use super::{Block, BlockError};

/// Read the stored version off a `{type, id, value, version?}`
/// envelope. Missing / non-integer falls back to `1` so legacy
/// streams that pre-date version-stamping just sail through unchanged
/// (their `migrate(1, …)` for blocks at v1 is a no-op).
#[must_use]
pub fn read_stored_version(envelope: &Value) -> u32 {
    envelope
        .get("version")
        .and_then(Value::as_u64)
        .map_or(1, |v| u32::try_from(v).unwrap_or(1))
}

/// Apply every step between `stored` and `block.version()` in order.
/// Each step gets the previous step's output. Already-current values
/// (or future values stored by a newer process) short-circuit.
///
/// # Errors
/// First failing [`crate::Block::migrate`] short-circuits with
/// [`BlockError::MigrateFailed`] carrying the offending step.
pub fn apply_migrations<B: Block + ?Sized>(
    block: &B,
    stored: u32,
    mut value: Value,
) -> Result<Value, BlockError> {
    let target = block.version();
    if stored >= target {
        // Already at-or-past current; nothing to do.
        return Ok(value);
    }
    for step in stored..target {
        value = block.migrate(step, value).map_err(|e| {
            // Re-wrap into a step-aware error if the block returned a
            // bare BlockError without context.
            match e {
                BlockError::MigrateFailed { .. } => e,
                other => BlockError::MigrateFailed {
                    block_type: block.type_name().to_owned(),
                    from: step,
                    to: step + 1,
                    reason: other.to_string(),
                },
            }
        })?;
    }
    Ok(value)
}
