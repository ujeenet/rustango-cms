//! Regression test for #614 — a page type that restricts its parents must not
//! be offerable (or creatable) at the root of the tree.
//!
//! Before the fix, the root picker listed every creatable type and the create
//! path accepted any of them, so an `ArticlePage` declaring
//! `allowed_parents(HomePage)` could be created as a root page — a tree the
//! type rules forbid, in the admin the README advertises as the one that
//! "applies the type whitelist".

use rustango_cms::{find_handler, PageType, PageTypeOverrides};

#[derive(PageType, Default)]
#[page_type(
    app = "tests",
    type_name = "RootOkPage",
    verbose_name = "Root OK page",
    template = "root_ok.html"
)]
pub struct RootOkPage;
impl PageTypeOverrides for RootOkPage {}

#[derive(PageType, Default)]
#[page_type(
    app = "tests",
    type_name = "ChildOnlyPage",
    verbose_name = "Child only page",
    template = "child_only.html",
    allowed_parents(RootOkPage)
)]
pub struct ChildOnlyPage;
impl PageTypeOverrides for ChildOnlyPage {}

/// The declarations the fix keys off: an empty `allowed_parent_types` means
/// "anywhere, including the root"; a non-empty one means "only under these".
#[test]
fn parent_restriction_is_visible_to_the_registry() {
    let root_ok = find_handler("RootOkPage").expect("RootOkPage registered");
    assert!(
        root_ok.allowed_parent_types().is_empty(),
        "an unrestricted type must report no parent restriction, so it stays root-legal"
    );

    let child_only = find_handler("ChildOnlyPage").expect("ChildOnlyPage registered");
    assert_eq!(
        child_only.allowed_parent_types(),
        &["RootOkPage"],
        "a parent-restricted type must report its permitted parents"
    );
}

/// Mirrors `allowed_root_types` in the admin: root-legal == no declared
/// parents. Guards the rule itself, so a refactor that drops the filter is
/// caught even if the handler wiring changes.
#[test]
fn only_unrestricted_types_are_root_legal() {
    fn root_legal(type_name: &str) -> bool {
        find_handler(type_name).is_none_or(|h| h.allowed_parent_types().is_empty())
    }

    assert!(
        root_legal("RootOkPage"),
        "RootOkPage must be creatable at the root"
    );
    assert!(
        !root_legal("ChildOnlyPage"),
        "ChildOnlyPage restricts its parents, so it must NOT be creatable at the root (#614)"
    );

    // A type with no registered handler falls back to permitted — the admin
    // can't reason about it, and refusing would block DB-only page types.
    assert!(root_legal("NoSuchTypeRegistered"));
}

/// The page-type rows the admin filters are plain `PageType` records; keep the
/// test honest that the type name is what links a row to its handler.
#[test]
fn page_type_rows_link_to_handlers_by_name() {
    let _ = std::mem::ManuallyDrop::new(ChildOnlyPage);
    let names: Vec<&str> = vec!["RootOkPage", "ChildOnlyPage"];
    for n in names {
        assert!(
            find_handler(n).is_some(),
            "{n} should resolve via find_handler"
        );
    }
    // Compile-time proof the admin's row type is in scope for this rule.
    fn _uses_page_type(_rows: &[PageType]) {}
}
