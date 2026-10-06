//! Widget descriptors — the single primitive both flat extension
//! fields ([`crate::page_type::ExtensionField`]) and stream-block
//! fields ([`crate::block::BlockField::Widget`], landing in a later
//! slice) use to describe one form input.
//!
//! ## Why one shared primitive
//!
//! Wagtail's pain point #1 from the audit: custom inputs require a
//! parallel Python class + JS class + Telepath adapter + template +
//! CSS + media manifest. That's six artifacts per widget, with
//! string-keyed adapter names that fail at runtime.
//!
//! Here a widget is **one Tera template + one `WidgetKind` enum
//! variant** (or one inventory registration for custom widgets — see
//! [`registry::register_widget!`]). Every widget is server-rendered;
//! the only client JS the admin ships is generic add/remove/reorder
//! for stream lists and doesn't know what a widget is.
//!
//! ## Surface
//!
//! - [`WidgetKind`] enumerates every built-in input type. New kinds
//!   only show up here; templates pick them up automatically.
//! - [`Widget`] carries `(kind, name, label, value, options, …)` —
//!   the runtime metadata for one rendered input.
//! - [`registry`] holds the inventory plumbing for **third-party
//!   custom widgets** that ship outside this crate.

pub mod registry;

use serde::{Deserialize, Serialize};

/// Which form input to draw. Templates dispatch on this variant via
/// the bundled `rcms_admin/_widget.html` Tera macro.
///
/// Serialized as a flat lowercase string (`"text"`, `"datetimetz"`,
/// `"multiselect"`, …) so `{% if w.kind == "datetimetz" %}` matches
/// every template author intuitively.
///
/// Custom widgets keep the kind as `"custom"`; the third-party
/// widget's wire name lives in [`Widget::custom_name`] so the macro
/// can dispatch to the right registered template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WidgetKind {
    // ---- text-shaped ------------------------------------------------
    /// Single-line `<input type="text">`.
    Text,
    /// Multi-line `<textarea>` — plain content.
    Textarea,
    /// Multi-line `<textarea>` with a "preview as markdown" affordance.
    /// Rendered through [`crate::markdown::render`] on the public site.
    Markdown,
    /// Rich-text WYSIWYG widget (v1: same shape as Markdown — a
    /// `<textarea>` with `data-widget="richtext"` to which host apps
    /// can attach their own enhancer, e.g. EasyMDE / TipTap).
    RichText,
    /// `<input type="email">` — browser validates the shape.
    Email,
    /// `<input type="url">`.
    Url,
    /// `<input type="tel">`.
    Tel,
    /// `<input type="password">`. Rare in a CMS surface but
    /// completes the set (e.g. for storing API keys on snippets).
    Password,
    /// `<input type="hidden">` — passes values through without
    /// drawing an input.
    Hidden,

    // ---- numeric ----------------------------------------------------
    /// `<input type="number">`. No min/max bound by default.
    Number,
    /// `<input type="number">` constrained to integers.
    Integer,
    /// `<input type="number" step="any">` for free-form decimals.
    Float,
    /// `<input type="range">` slider — pair with `min`/`max`/`step`.
    Range,

    // ---- boolean / checkbox ----------------------------------------
    /// Single `<input type="checkbox">` — present-or-absent.
    Boolean,

    // ---- date / time -----------------------------------------------
    /// `<input type="date">`. Stored as ISO-8601 `YYYY-MM-DD`.
    Date,
    /// `<input type="time">`. Stored as `HH:MM[:SS]`.
    Time,
    /// `<input type="datetime-local">`. Stored as
    /// `YYYY-MM-DDTHH:MM[:SS]` (no timezone).
    Datetime,
    /// Pair of `<input type="datetime-local">` + a timezone
    /// `<select>` (IANA names). Value is the RFC 3339 string the
    /// public-side renderer can parse with `chrono::DateTime`.
    DatetimeTz,

    // ---- color -----------------------------------------------------
    /// `<input type="color">` — hex `#RRGGBB`.
    Color,

    // ---- file / media ----------------------------------------------
    /// `<input type="file">` — raw upload, no library indirection.
    /// Distinct from [`Self::MediaPicker`] (which picks an existing
    /// `cms_media` row by id).
    File,
    /// Media library picker — value is the `cms_media.id` as a
    /// stringified integer. Empty = no selection.
    MediaPicker,
    /// Page chooser — modal `<dialog>` overlay listing pages by
    /// title; value is the `cms_page.id` as a stringified integer.
    /// Empty = no selection. Backs the Wagtail-parity
    /// `PageChooserBlock` + any FK-to-page extension field.
    PageChooser,
    /// Snippet chooser — modal `<dialog>` overlay listing snippets
    /// (optionally filtered to one `type_name` via
    /// [`Widget::custom_name`]); value is the `cms_snippet.id` as a
    /// stringified integer. Backs the Wagtail-parity
    /// `SnippetChooserBlock`.
    SnippetChooser,
    /// Snippet many-to-many chooser — multi-select chooser listing
    /// snippets of one `type_name` (in [`Widget::custom_name`]);
    /// value is a JSON-array string of selected `cms_snippet.id`
    /// values in chooser order. Backs the Wagtail-parity
    /// `ParentalManyToManyField` (#243). Storage lives in the
    /// `cms_page_snippet_m2m` through-table — the widget's
    /// [`Widget::name`] is the M2M `relation_name`, [`Widget::value`]
    /// is the JSON array the editor submits. Hosts call
    /// [`crate::page_snippet_m2m::replace_all`] from their
    /// `save_extension` override and
    /// [`crate::page_snippet_m2m::related_snippets`] from
    /// `load_extension` (or `public_context`) until the derive-macro
    /// hook lands to do that wiring automatically.
    SnippetM2M,
    /// Document chooser — modal `<dialog>` overlay listing media
    /// rows of `kind = "document"`; value is the `cms_media.id` as
    /// a stringified integer. Distinct from [`Self::MediaPicker`]
    /// (which filters to images today). Backs the Wagtail-parity
    /// `DocumentChooserBlock`.
    DocumentChooser,
    /// Generic model chooser (#421) — modal overlay over any
    /// `register_chooser!`ed model. The registered chooser **slug**
    /// lives in [`Widget::custom_name`] and becomes the
    /// `data-chooser-kind`, which the cms-ux.js overlay resolves to
    /// `/cms-admin/__chooser/<slug>`. Value is the chosen row's id as
    /// a stringified integer. Build with [`Widget::model_chooser`].
    ModelChooser,

    // ---- choice (flat — variant is first-class) -------------------
    /// Stacked `<input type="radio">` group. Single-choice. Reads
    /// `(value, label)` pairs from [`Widget::options`].
    Radio,
    /// `<select>` dropdown. Single-choice. Reads from
    /// [`Widget::options`].
    Select,
    /// Stacked `<input type="checkbox">` group. Multi-choice; value
    /// is a JSON array string. Reads from [`Widget::options`].
    Checkboxes,
    /// `<select multiple>`. Multi-choice; value is a JSON array
    /// string. Reads from [`Widget::options`].
    MultiSelect,

    // ---- streamfield -----------------------------------------------
    /// Wagtail-style StreamField — a list of composable blocks
    /// (Heading / Paragraph / Image / Quote / nested two-column / …).
    /// The list of *allowed* block types lives in [`Widget::allowed`];
    /// every entry must resolve to a registered [`crate::Block`].
    ///
    /// Storage shape: a JSON-array string of
    /// `{type, id, value}` envelopes — see [`crate::block`] for the
    /// full wire-format spec. Round-tripped through the editor as one
    /// hidden form field; the admin macro renders the recursive
    /// stream editor and the server deserializes the JSON on submit.
    Stream,

    // ---- escape hatch ----------------------------------------------
    /// Third-party-registered widget. The wire name lives in
    /// [`Widget::custom_name`]; look it up via
    /// [`registry::find_custom_widget`] and render its template.
    Custom,
}

impl WidgetKind {
    /// Lowercase wire-format tag matching the serde rename. Used by
    /// the template macro's `{% if w.kind == "…" %}` arms and tests.
    #[must_use]
    pub fn as_tag(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Textarea => "textarea",
            Self::Markdown => "markdown",
            Self::RichText => "richtext",
            Self::Email => "email",
            Self::Url => "url",
            Self::Tel => "tel",
            Self::Password => "password",
            Self::Hidden => "hidden",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::Float => "float",
            Self::Range => "range",
            Self::Boolean => "boolean",
            Self::Date => "date",
            Self::Time => "time",
            Self::Datetime => "datetime",
            Self::DatetimeTz => "datetimetz",
            Self::Color => "color",
            Self::File => "file",
            Self::MediaPicker => "mediapicker",
            Self::PageChooser => "pagechooser",
            Self::SnippetChooser => "snippetchooser",
            Self::SnippetM2M => "snippetm2m",
            Self::DocumentChooser => "documentchooser",
            Self::ModelChooser => "modelchooser",
            Self::Radio => "radio",
            Self::Select => "select",
            Self::Checkboxes => "checkboxes",
            Self::MultiSelect => "multiselect",
            Self::Stream => "stream",
            Self::Custom => "custom",
        }
    }

    /// `true` for widgets whose wire contract POSTs a JSON-encoded
    /// string in a single form field — `Checkboxes` / `MultiSelect`
    /// (JSON arrays of strings), `Stream` (JSON array of block
    /// envelopes), and `SnippetM2M` (JSON array of i64 snippet ids,
    /// ordered).
    #[must_use]
    pub fn is_multi_value(&self) -> bool {
        matches!(
            self,
            Self::Checkboxes | Self::MultiSelect | Self::Stream | Self::SnippetM2M,
        )
    }

    /// `true` for widgets whose value is author-facing free text worth
    /// translating per locale — single/multi-line text, Markdown source,
    /// and RichText HTML. Everything else is structural (ids, numbers,
    /// dates, choices) or an identifier (`Email`/`Url`/`Tel`) and is
    /// never stored as a translation override. Drives the StreamField
    /// translation walker in [`crate::block::translate`].
    #[must_use]
    pub fn is_translatable_text(&self) -> bool {
        matches!(
            self,
            Self::Text | Self::Textarea | Self::Markdown | Self::RichText,
        )
    }
}

/// Runtime descriptor for one rendered form input. Built by
/// `PageTypeHandler::extension_fields` (and, in a later slice, by
/// `Block::fields` → `BlockField::Widget`) and consumed by the
/// `_widget.html` Tera macro.
///
/// ## Fields
///
/// - `name` — HTML form `name=`. Also the key the POST handler
///   `form_map.get(...)` reads back on submit.
/// - `kind` — picks the input type. See [`WidgetKind`].
/// - `custom_name` — wire name for a [`WidgetKind::Custom`]
///   registration. Looked up at render time via
///   [`registry::find_custom_widget`]. Ignored for built-in kinds.
/// - `label` — display label above / next to the input.
/// - `value` — pre-fill value. For multi-value widgets
///   ([`WidgetKind::Checkboxes`] / [`WidgetKind::MultiSelect`]) the
///   value is the JSON-array string the client POSTs back verbatim.
/// - `options` — `(value, label)` pairs for choice widgets.
/// - `help` — short hint rendered under the input.
/// - `placeholder` — `placeholder=` attribute for text-shaped
///   inputs.
/// - `required` — emits the HTML5 `required` attribute.
/// - `min` / `max` / `step` — clamps for numeric / date / range
///   inputs.
/// - `max_length` — max-length cap for string inputs.
/// - `read_only` — emits HTML5 `readonly`; for fully-non-editable
///   display use the page-level
///   [`crate::page_type::DisplayField`] instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Widget {
    pub name: String,
    pub kind: WidgetKind,
    #[serde(default)]
    pub custom_name: String,
    /// Pre-rendered HTML for [`WidgetKind::Custom`] widgets — the
    /// admin handler walks each widget, looks up the registered
    /// template via [`registry::find_custom_widget`], renders it
    /// once before stamping into Tera context, and stores the
    /// result here. The bundled `_widget.html` macro emits it
    /// `| safe`. Empty for non-Custom widgets.
    #[serde(default)]
    pub custom_html: String,
    pub label: String,
    #[serde(default)]
    pub value: String,
    /// Choice options as `(value, label)` pairs. Always serialized
    /// (even when empty) so the bundled `_widget.html` macro's
    /// `{% for opt in w.options %}` loops don't throw an
    /// undefined-variable error on widgets that don't use options.
    #[serde(default)]
    pub options: Vec<(String, String)>,
    /// String fields below are always serialized (empty = falsy in
    /// Tera) so `{% if w.foo %}` checks in `_widget.html` don't
    /// trip Tera's "Variable not found" error when the slot is
    /// unset. Same rule applies to `variant`, `pattern`, etc.
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub placeholder: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<String>,
    #[serde(default)]
    pub max_length: Option<u32>,
    /// HTML5 `minlength` for string-shaped widgets.
    #[serde(default)]
    pub min_length: Option<u32>,
    /// HTML5 `pattern` regex for string-shaped widgets — browser
    /// validates on form submit.
    #[serde(default)]
    pub pattern: String,
    #[serde(default)]
    pub read_only: bool,
    /// For [`WidgetKind::Stream`] only — the block `type_name`s
    /// permitted at the top level of this stream. Boot-time
    /// validation in [`crate::block::validate_block_registry`]
    /// verifies every entry resolves to a registered
    /// [`crate::Block`]. Empty for non-Stream widgets.
    #[serde(default)]
    pub allowed: Vec<String>,
    /// Style variant — handler opt-in to a visual variant of the
    /// canonical widget. Today only `"switch"` is recognised by the
    /// `_widget.html` macro (on [`WidgetKind::Boolean`], renders the
    /// checkbox as an iOS-style toggle). Empty = use the default.
    ///
    /// Wagtail parity: maps to widget-class swap (`SwitchInput` etc.).
    #[serde(default)]
    pub variant: String,
}

impl Widget {
    /// Smallest possible widget — just `(kind, name, label)`. Caller
    /// fluently adds the rest via `.with_*` methods.
    #[must_use]
    pub fn new(kind: WidgetKind, name: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind,
            custom_name: String::new(),
            custom_html: String::new(),
            label: label.into(),
            value: String::new(),
            options: Vec::new(),
            help: String::new(),
            placeholder: String::new(),
            required: false,
            min: None,
            max: None,
            step: None,
            max_length: None,
            min_length: None,
            pattern: String::new(),
            read_only: false,
            allowed: Vec::new(),
            variant: String::new(),
        }
    }

    /// Fluent: set HTML5 `minlength`.
    #[must_use]
    pub fn with_min_length(mut self, n: u32) -> Self {
        self.min_length = Some(n);
        self
    }

    /// Fluent: set HTML5 `pattern` regex.
    #[must_use]
    pub fn with_pattern(mut self, p: impl Into<String>) -> Self {
        self.pattern = p.into();
        self
    }

    /// Style variant opt-in. Currently meaningful for
    /// [`WidgetKind::Boolean`] (`"switch"` flips to iOS-style toggle).
    #[must_use]
    pub fn with_variant(mut self, variant: impl Into<String>) -> Self {
        self.variant = variant.into();
        self
    }

    /// Shortcut for a [`WidgetKind::Stream`] widget — pairs the
    /// `name` / `label` with the list of allowed top-level block
    /// `type_name`s. Every entry must resolve to a registered
    /// [`crate::Block`] (boot-time validation enforces this).
    #[must_use]
    pub fn stream<S: Into<String>>(
        name: impl Into<String>,
        label: impl Into<String>,
        allowed: impl IntoIterator<Item = S>,
    ) -> Self {
        Self {
            allowed: allowed.into_iter().map(Into::into).collect(),
            ..Self::new(WidgetKind::Stream, name, label)
        }
    }

    /// Shortcut for a [`WidgetKind::Custom`] widget — pairs the
    /// `kind` with the third-party registration `custom_name` looked
    /// up at render time through [`registry::find_custom_widget`].
    #[must_use]
    pub fn custom(
        custom_name: impl Into<String>,
        name: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            custom_name: custom_name.into(),
            ..Self::new(WidgetKind::Custom, name, label)
        }
    }

    #[must_use]
    pub fn with_value(mut self, v: impl Into<String>) -> Self {
        self.value = v.into();
        self
    }

    #[must_use]
    pub fn with_help(mut self, h: impl Into<String>) -> Self {
        self.help = h.into();
        self
    }

    #[must_use]
    pub fn with_placeholder(mut self, p: impl Into<String>) -> Self {
        self.placeholder = p.into();
        self
    }

    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    #[must_use]
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    #[must_use]
    pub fn with_options<I, V, L>(mut self, options: I) -> Self
    where
        I: IntoIterator<Item = (V, L)>,
        V: Into<String>,
        L: Into<String>,
    {
        self.options = options
            .into_iter()
            .map(|(v, l)| (v.into(), l.into()))
            .collect();
        self
    }

    /// #243 — convenience constructor for the Page↔Snippet M2M
    /// chooser. The widget's `name` becomes the M2M
    /// `relation_name` (the field on the page that this chooser
    /// populates), and `snippet_type_name` lands in
    /// [`Self::custom_name`] where the template reads it. Hosts
    /// then call [`Self::with_options`] with the eligible
    /// `(snippet_id, snippet_title)` pairs they fetched from
    /// `cms_snippet` for that type — until the derive-macro hook
    /// lands and does the population automatically.
    ///
    /// ```ignore
    /// use rustango_cms::widget::Widget;
    /// // In the handler's `widgets()` impl, after fetching
    /// // `let cats = Snippet::objects().where_(...).fetch(pool).await?;`
    /// vec![Widget::snippet_m2m("categories", "Categories", "Category")
    ///     .with_options(cats.iter().map(|s|
    ///         (s.id.get().copied().unwrap_or_default().to_string(),
    ///          s.title.clone())))]
    /// ```
    #[must_use]
    pub fn snippet_m2m(
        relation_name: impl Into<String>,
        label: impl Into<String>,
        snippet_type_name: impl Into<String>,
    ) -> Self {
        let mut w = Self::new(WidgetKind::SnippetM2M, relation_name, label);
        w.custom_name = snippet_type_name.into();
        w
    }

    /// Build a generic model-chooser widget (#421) for the
    /// `register_chooser!`ed model registered under `chooser_slug`.
    /// The value is the chosen row's id (stringified). The slug rides
    /// in [`Self::custom_name`] → `data-chooser-kind` → the cms-ux.js
    /// overlay's `/cms-admin/__chooser/<slug>` endpoint.
    ///
    /// ```ignore
    /// use rustango_cms::widget::Widget;
    /// // In a PageTypeHandler's `widgets()` impl:
    /// vec![Widget::model_chooser("author_id", "Author", "author")
    ///     .with_value(stored_id)]
    /// ```
    #[must_use]
    pub fn model_chooser(
        field_name: impl Into<String>,
        label: impl Into<String>,
        chooser_slug: impl Into<String>,
    ) -> Self {
        let mut w = Self::new(WidgetKind::ModelChooser, field_name, label);
        w.custom_name = chooser_slug.into();
        w
    }

    #[must_use]
    pub fn with_min(mut self, m: f64) -> Self {
        self.min = Some(m);
        self
    }

    #[must_use]
    pub fn with_max(mut self, m: f64) -> Self {
        self.max = Some(m);
        self
    }

    #[must_use]
    pub fn with_step(mut self, s: impl Into<String>) -> Self {
        self.step = Some(s.into());
        self
    }

    #[must_use]
    pub fn with_max_length(mut self, n: u32) -> Self {
        self.max_length = Some(n);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_m2m_kind_round_trips_as_tag() {
        assert_eq!(WidgetKind::SnippetM2M.as_tag(), "snippetm2m");
    }

    #[test]
    fn snippet_m2m_is_multi_value() {
        assert!(WidgetKind::SnippetM2M.is_multi_value());
        assert!(WidgetKind::Stream.is_multi_value());
        assert!(WidgetKind::MultiSelect.is_multi_value());
        assert!(WidgetKind::Checkboxes.is_multi_value());
        // Sanity: single-value kinds still report false so the
        // boundary between scalar and JSON-array wire shape is
        // explicit.
        assert!(!WidgetKind::Text.is_multi_value());
        assert!(!WidgetKind::SnippetChooser.is_multi_value());
    }

    #[test]
    fn snippet_m2m_constructor_sets_custom_name_to_type() {
        let w = Widget::snippet_m2m("categories", "Categories", "Category");
        assert_eq!(w.kind, WidgetKind::SnippetM2M);
        assert_eq!(w.name, "categories");
        assert_eq!(w.label, "Categories");
        // `custom_name` carries the snippet type the template uses
        // to render the "Snippets of type X" hint + the eventual
        // type filter on the chooser.
        assert_eq!(w.custom_name, "Category");
    }
}
