//! Tests for the `Block` registry — `register_block!` inventory
//! collection, boot-time validation (dup names, dangling references),
//! and the `find_block(type_name)` lookup.
//!
//! Two stub blocks are registered at module-load via the macro; the
//! framework's process-global inventory picks them up, so the
//! `validate_block_registry()` happy-path test below depends on them
//! being live in the binary.

use rustango_cms::{register_block, widget::WidgetKind, Block, BlockField};

// ---- stub blocks ---------------------------------------------------
//
// Two minimal blocks used as the fixture surface. One scalar
// (`StubHeading`), one container (`StubTwoColumn`) so the
// `Stream::allowed` reference check has something to look at.

#[derive(Default)]
struct StubHeading;
impl Block for StubHeading {
    fn type_name(&self) -> &'static str {
        "test_heading"
    }
    fn verbose_name(&self) -> &'static str {
        "Test heading"
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::widget("text", "Text", WidgetKind::Text)]
    }
}
register_block!(StubHeading);

#[derive(Default)]
struct StubTwoColumn;
impl Block for StubTwoColumn {
    fn type_name(&self) -> &'static str {
        "test_two_column"
    }
    fn verbose_name(&self) -> &'static str {
        "Test two-column"
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::stream("left", "Left", ["test_heading"]),
            BlockField::stream("right", "Right", ["test_heading"]),
        ]
    }
}
register_block!(StubTwoColumn);

// ---- tests ---------------------------------------------------------

#[test]
fn registry_includes_stub_blocks() {
    let count = rustango_cms::registered_blocks().count();
    // At least the two stubs above must be present. Other crates in
    // this binary may register more; we just check the floor.
    assert!(count >= 2, "expected >= 2 registered blocks, got {count}");
}

#[test]
fn find_block_resolves_stub_by_name() {
    let found = rustango_cms::find_block("test_heading");
    assert!(found.is_some(), "find_block should resolve test_heading");
    let b = found.unwrap();
    assert_eq!(b.type_name(), "test_heading");
    assert_eq!(b.verbose_name(), "Test heading");
}

#[test]
fn find_block_returns_none_for_unknown_type() {
    let found = rustango_cms::find_block("never_registered_anywhere");
    assert!(found.is_none());
}

#[test]
fn validate_block_registry_accepts_clean_graph() {
    // The fixtures above all have resolving references — this should
    // not panic. The function itself panics on dup-names or dangling
    // refs; reaching the next line means it passed.
    rustango_cms::validate_block_registry();
}

#[test]
fn duplicate_block_names_returns_empty_on_clean_registry() {
    let dups = rustango_cms::block::registry::duplicate_block_names();
    assert!(
        dups.is_empty(),
        "expected no duplicate block names, found {dups:?}",
    );
}

#[test]
fn check_block_references_resolves_stream_allowed() {
    // StubTwoColumn declares `Stream { allowed: ["test_heading"] }`
    // and StubHeading is registered, so the cross-reference graph is
    // clean. A `Result::Ok(())` is the contract.
    let r = rustango_cms::block::registry::check_block_references();
    assert!(r.is_ok(), "check_block_references reported: {r:?}");
}

#[test]
fn default_version_is_one_for_new_blocks() {
    // Default impl returns 1; bumping requires opting in via override.
    let b = rustango_cms::find_block("test_heading").unwrap();
    assert_eq!(b.version(), 1);
}

#[test]
fn block_field_constructors_round_trip() {
    let w = BlockField::widget("text", "Text", WidgetKind::Text);
    assert_eq!(w.name(), "text");

    let s = BlockField::stream("left", "Left", ["heading", "paragraph"]);
    if let BlockField::Stream { allowed, .. } = &s {
        assert_eq!(allowed, &vec!["heading".to_owned(), "paragraph".to_owned()]);
    } else {
        panic!("expected Stream variant");
    }

    let r = BlockField::repeat("items", "Items", "todo");
    if let BlockField::Repeat { item_type, .. } = &r {
        assert_eq!(item_type, "todo");
    } else {
        panic!("expected Repeat variant");
    }
}
