//! Block registry — inventory-collected `Block` constructors.
//!
//! Mirrors the [`crate::widget::registry`] + [`crate::page_type`]
//! registry shapes: each crate that defines a [`Block`] submits a
//! `BlockRegistration` carrying a `fn() -> Box<dyn Block>` factory,
//! and the framework iterates the global collection at boot to
//! validate uniqueness + cross-references.

use super::{Block, BlockError, BlockField};

/// One registered block — a factory the framework calls to materialize
/// a [`Block`] trait object. Each call returns a fresh box (cheap;
/// `Block`s are stateless descriptors). Submitted via
/// [`crate::register_block!`].
pub struct BlockRegistration {
    /// Constructor function. Convention: `|| Box::new(MyBlock::default())`
    /// — or any callable returning `Box<dyn Block>`.
    pub factory: fn() -> Box<dyn Block>,
}

inventory::collect!(BlockRegistration);

/// Register a [`Block`] at module-load time. Mirrors the
/// `register_widget!` / `register_page_type!` patterns.
///
/// ```ignore
/// pub struct HeadingBlock;
///
/// impl rustango_cms::Block for HeadingBlock {
///     /* … */
/// }
///
/// rustango_cms::register_block!(HeadingBlock);
/// ```
#[macro_export]
macro_rules! register_block {
    ($ty:ty) => {
        $crate::inventory::submit! {
            $crate::block::BlockRegistration {
                factory: || Box::new(<$ty>::default()),
            }
        }
    };
    // Variant for blocks whose constructor isn't `Default::default()`
    // — pass any expression returning a `Box<dyn Block>`.
    ($factory:expr) => {
        $crate::inventory::submit! {
            $crate::block::BlockRegistration {
                factory: $factory,
            }
        }
    };
}

/// Iterator over every registered block. Each call to the factory
/// returns a fresh `Box<dyn Block>` — cheap, blocks are stateless.
pub fn registered_blocks() -> impl Iterator<Item = Box<dyn Block>> {
    inventory::iter::<BlockRegistration>
        .into_iter()
        .map(|r| (r.factory)())
}

/// Look up a registered block by its `type_name`. Returns `None` when
/// the name isn't registered; the editor surfaces this as a
/// schema-drift warning rather than panicking.
#[must_use]
pub fn find_block(type_name: &str) -> Option<Box<dyn Block>> {
    registered_blocks().find(|b| b.type_name() == type_name)
}

/// Surface duplicate `type_name`s — `Vec` is empty when the registry
/// is clean. Two blocks with the same `type_name` would pick whichever
/// one happens to be linked first; loud failure beats silent
/// arbitrariness.
#[must_use]
pub fn duplicate_block_names() -> Vec<&'static str> {
    let mut seen = std::collections::HashSet::new();
    let mut dups = Vec::new();
    for b in registered_blocks() {
        if !seen.insert(b.type_name()) {
            dups.push(b.type_name());
        }
    }
    dups
}

/// Walk every registered block's [`crate::Block::fields`] and verify
/// that every `Stream::allowed` entry + every `Repeat::item_type`
/// resolves to a registered block.
///
/// Returns the first dangling reference found; an empty `Ok(())` means
/// the cross-reference graph is clean.
///
/// # Errors
/// [`BlockError::DanglingReference`] when a block's `fields()`
/// declares a `Stream::allowed` or `Repeat::item_type` value that
/// doesn't match any registered block's `type_name`.
pub fn check_block_references() -> Result<(), BlockError> {
    let names: std::collections::HashSet<&'static str> =
        registered_blocks().map(|b| b.type_name()).collect();
    for block in registered_blocks() {
        let owner = block.type_name();
        for field in block.fields() {
            match field {
                BlockField::Stream { allowed, .. } => {
                    for ref_name in &allowed {
                        if !names.contains(ref_name.as_str()) {
                            // SAFETY: type_name() returns &'static str
                            // from the trait, so leaking ref_name into
                            // the static-ish error type would need an
                            // intern step. Use a Box::leak for the
                            // diagnostic message — this only runs once
                            // at boot and only on misconfiguration.
                            let leaked: &'static str = Box::leak(ref_name.clone().into_boxed_str());
                            return Err(BlockError::DanglingReference {
                                owner,
                                referred: leaked,
                            });
                        }
                    }
                }
                BlockField::Repeat { item_type, .. } => {
                    if !names.contains(item_type.as_str()) {
                        let leaked: &'static str = Box::leak(item_type.clone().into_boxed_str());
                        return Err(BlockError::DanglingReference {
                            owner,
                            referred: leaked,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Boot-time validation hook. Call once during application startup
/// before the admin router goes live. Panics with a loud diagnostic
/// on (a) duplicate `type_name`s, (b) any dangling
/// `Stream::allowed` / `Repeat::item_type` reference.
///
/// Mirrors [`crate::widget::registry::validate_registry`] for
/// widgets.
///
/// # Panics
/// - On duplicate block names.
/// - On dangling cross-references.
pub fn validate_block_registry() {
    let dups = duplicate_block_names();
    assert!(
        dups.is_empty(),
        "rustango_cms: duplicate block registrations: {dups:?}",
    );
    if let Err(e) = check_block_references() {
        panic!("rustango_cms: block registry has a dangling reference: {e}");
    }
}
