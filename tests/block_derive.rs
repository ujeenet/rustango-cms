//! Tests for `#[derive(Block)]`. Mirrors `page_type_derive.rs` —
//! verifies that:
//!
//! - the trait impl reads the right `type_name` / `verbose_name`
//!   defaults from the struct name when no `#[block(...)]` attrs
//!   override them
//! - `#[block(icon, group, description)]` attrs are surfaced
//! - `#[field(widget = X, options(...), required)]` round-trips into
//!   the right `BlockField::Widget` variants
//! - the block joins the registry via `find_block`

use rustango_cms::{find_block, Block, BlockField};

// ---- Fixture A: defaults from struct name -------------------------

#[derive(Default, Block)]
pub struct TestPullquote {
    #[field(widget = Textarea, required)]
    pub body: String,
}

#[test]
fn defaults_derive_type_name_and_verbose_name() {
    let b = find_block("test_pullquote")
        .expect("TestPullquote should be registered as `test_pullquote`");
    assert_eq!(b.type_name(), "test_pullquote");
    assert_eq!(b.verbose_name(), "Test Pullquote");
    assert!(b.icon().is_none());
    assert!(b.group().is_none());
    assert!(b.description().is_none());
}

#[test]
fn field_attrs_surface_in_fields_vec() {
    let b = find_block("test_pullquote").unwrap();
    let fields = b.fields();
    assert_eq!(fields.len(), 1);
    match &fields[0] {
        BlockField::Widget {
            name,
            label,
            required,
            ..
        } => {
            assert_eq!(name, "body");
            assert_eq!(label, "Body"); // default-derived from field ident
            assert!(*required);
        }
        _ => panic!("expected Widget variant"),
    }
}

// ---- Fixture B: full attr surface ---------------------------------

#[derive(Default, Block)]
#[block(
    type_name = "test_heading_v2",
    verbose_name = "Heading v2",
    icon = "title",
    group = "Headings",
    description = "Section heading with level picker."
)]
pub struct TestHeadingV2 {
    #[field(widget = Text, label = "Heading text", required)]
    pub text: String,
    #[field(widget = Select, label = "Level", options(h2 = "H2", h3 = "H3", h4 = "H4"))]
    pub level: String,
}

#[test]
fn block_attrs_override_defaults() {
    let b =
        find_block("test_heading_v2").expect("custom type_name should win over snake_case default");
    assert_eq!(b.type_name(), "test_heading_v2");
    assert_eq!(b.verbose_name(), "Heading v2");
    assert_eq!(b.icon(), Some("title"));
    assert_eq!(b.group(), Some("Headings"));
    assert_eq!(b.description(), Some("Section heading with level picker."));
}

#[test]
fn options_attr_round_trips_pairs() {
    let b = find_block("test_heading_v2").unwrap();
    let fields = b.fields();
    let level = fields
        .iter()
        .find(|f| matches!(f, BlockField::Widget { name, .. } if name == "level"))
        .expect("level field");
    match level {
        BlockField::Widget { options, label, .. } => {
            assert_eq!(label, "Level");
            assert_eq!(
                options,
                &vec![
                    ("h2".to_owned(), "H2".to_owned()),
                    ("h3".to_owned(), "H3".to_owned()),
                    ("h4".to_owned(), "H4".to_owned()),
                ]
            );
        }
        _ => unreachable!(),
    }
}
