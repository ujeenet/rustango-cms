//! Third-party custom widget registry.
//!
//! Out-of-tree crates ship new widget types via
//! [`register_widget!`]. Each registration carries the wire-format
//! `name` (used by [`crate::widget::WidgetKind::Custom`] and looked
//! up at render time) and the **Tera template path** the bundled
//! `_widget.html` macro should include for that name.
//!
//! ## Why no JS class
//!
//! Wagtail audit pain #1: their `Adapter` system requires both a
//! Python side AND a JS class with a string-keyed constructor name
//! that fails at runtime if mismatched. The corresponding plumbing
//! here is *just* a Tera template — the same one the server uses to
//! render the widget. Native HTML5 inputs cover most needs
//! (`<input type="color">`, `<input type="date">`, `<input
//! type="range">`); for the rare widget that wants client-side
//! sprinkle (e.g. a map preview reacting to lat/lng inputs), the
//! template can emit a `<script>` tag inline or a `data-*` hook the
//! host app's own bundle reads. No framework-imposed JS class
//! registry.
//!
//! ## Example
//!
//! ```ignore
//! use rustango_cms::register_widget;
//! register_widget!("latlng", "widgets/latlng.html");
//! ```
//!
//! Inside a `PageTypeHandler::extension_fields` impl:
//!
//! ```ignore
//! Widget::new(
//!     WidgetKind::Custom { name: "latlng".to_owned() },
//!     "office_loc",
//!     "Office location",
//! )
//! ```

/// One registered custom widget — wire-format `name` paired with the
/// Tera template that renders it. Submitted via [`register_widget!`]
/// and collected at startup by the [`inventory`] crate.
pub struct CustomWidgetRegistration {
    /// Wire-format name. Matched against
    /// [`crate::widget::WidgetKind::Custom { name }`] at render
    /// time. Must be globally unique across the process — duplicate
    /// registrations panic at boot via [`validate_registry`].
    pub name: &'static str,
    /// Tera template path used by the bundled `_widget.html` macro
    /// via `{% include template %}`. The template receives the
    /// active [`crate::widget::Widget`] as `w`.
    pub template: &'static str,
}

inventory::collect!(CustomWidgetRegistration);

/// Register a custom widget at module-load time. Picks up
/// `rustango_cms::inventory` (re-exported in [`crate`]) so callers
/// don't need their own `inventory` dep.
///
/// ```ignore
/// use rustango_cms::register_widget;
/// register_widget!("latlng", "widgets/latlng.html");
/// ```
#[macro_export]
macro_rules! register_widget {
    ($name:expr, $template:expr) => {
        $crate::inventory::submit! {
            $crate::widget::registry::CustomWidgetRegistration {
                name: $name,
                template: $template,
            }
        }
    };
}

/// Iterator over every registered custom widget. Returned references
/// are `'static` — registrations are process-global.
pub fn registered_widgets() -> impl Iterator<Item = &'static CustomWidgetRegistration> {
    inventory::iter::<CustomWidgetRegistration>()
}

/// Look up the registered template path for a custom widget by
/// name. Returns `None` when the name isn't registered — callers
/// (the `_widget.html` macro path) treat this as an editor-visible
/// error rather than panicking.
#[must_use]
pub fn find_custom_widget(name: &str) -> Option<&'static CustomWidgetRegistration> {
    registered_widgets().find(|w| w.name == name)
}

/// Surface duplicate registrations at boot. Returns the list of
/// duplicate names; the empty Vec means clean. Host apps should
/// call this once at startup and panic / log on non-empty output —
/// silent duplicates would lead to one of two competing templates
/// winning arbitrarily depending on link order.
#[must_use]
pub fn duplicate_widget_names() -> Vec<&'static str> {
    let mut seen = std::collections::HashSet::new();
    let mut dups = Vec::new();
    for w in registered_widgets() {
        if !seen.insert(w.name) {
            dups.push(w.name);
        }
    }
    dups
}

/// Boot-time validation hook: checks for duplicate widget names.
///
/// # Panics
/// On any duplicate widget name — boot-time crash with both
/// occupants named is the right loud failure.
pub fn validate_registry() {
    let dups = duplicate_widget_names();
    assert!(
        dups.is_empty(),
        "rustango_cms: duplicate custom widget registrations: {dups:?}",
    );
}
