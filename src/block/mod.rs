//! StreamField — composable body blocks.
//!
//! A `Block` is one type of node that can sit in a `WidgetKind::Stream`
//! list. Each block declares its structural shape via [`Block::fields`]
//! — a `Vec<BlockField>` of leaves ([`BlockField::Widget`] /
//! [`BlockField::Computed`]) and nested containers ([`BlockField::Stream`]
//! / [`BlockField::Repeat`]). The same `BlockField` enum covers
//! struct, stream and list blocks
//! in one recursive shape so the validator + renderer + admin editor
//! all reuse a single walker.
//!
//! ## JSON wire format
//!
//! A stream's stored JSON is an array of `{type, id, value}` tuples:
//!
//! ```json
//! [
//!   { "type": "heading", "id": "550e8400-…", "value": { "text": "Hi" } },
//!   { "type": "two_column", "id": "…", "value": {
//!       "left":  [ {"type": "image", "id": "…", "value": { "media_id": 4 } } ],
//!       "right": [ {"type": "paragraph", "id": "…", "value": { "body": "…" } } ]
//!   }}
//! ]
//! ```
//!
//! - Multi-field block: `value` is a flat dict keyed by [`BlockField`] `name`s.
//! - Single-field block: canonical shape is still `{<name>: <scalar>}`,
//!   but a bare scalar at `value` is also accepted on the way in.
//! - `id` is a UUID v4 minted client-side on insert; never re-minted on edit.
//! - `Repeat` (homogeneous) stores `[{type: item_type, id, value}, …]` — same
//!   shape as `Stream`, single allowed type.
//!
//! ## A small trait surface
//!
//! The editor is one Tera macro and a block is one Rust trait, so a
//! block needs only three required
//! methods ([`Block::type_name`], [`Block::verbose_name`],
//! [`Block::fields`]) and four optional ones with defaults
//! ([`Block::icon`], [`Block::group`], [`Block::version`],
//! [`Block::template`]). [`Block::render`] and [`Block::migrate`] have
//! generic default impls.

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::widget::WidgetKind;

pub mod registry;

pub use registry::{find_block, registered_blocks, validate_block_registry, BlockRegistration};

/// Per-block, sync render error. Stays sync because every render
/// step is pure JSON → HTML — no IO during the rendering pass.
#[derive(Debug, Error)]
pub enum BlockError {
    /// The stored JSON didn't match the block's [`Block::fields`]
    /// schema. Carries a path so admin can highlight the offending
    /// block.
    #[error("block `{block_type}` at `{path}`: {reason}")]
    Shape {
        block_type: String,
        path: String,
        reason: String,
    },

    /// `type_name` in the stored JSON doesn't resolve to a registered
    /// block. Surfaces as the yellow schema-drift banner in the editor.
    #[error("unknown block type `{0}` (not registered via `register_block!`)")]
    UnknownType(String),

    /// `register_block!` returned a `Block::fields()` entry referring
    /// to a non-existent block type via `Stream::allowed` or
    /// `Repeat::item_type`. Caught at boot-time via
    /// [`validate_block_registry`].
    #[error("block `{owner}` references unregistered type `{referred}`")]
    DanglingReference {
        owner: &'static str,
        referred: &'static str,
    },

    /// `block.migrate(from, value)` failed to upgrade an older
    /// version. Original JSON is kept on the record; the editor
    /// surfaces the failure in the schema-drift banner.
    #[error("block `{block_type}` migrate {from}→{to} failed: {reason}")]
    MigrateFailed {
        block_type: String,
        from: u32,
        to: u32,
        reason: String,
    },

    /// Tera failure during a block's render. The block's template path
    /// + the underlying Tera message land in the body.
    #[error("block `{block_type}` template `{template}` render failed: {source}")]
    TemplateRender {
        block_type: String,
        template: String,
        #[source]
        source: tera::Error,
    },
}

/// Per-field validators threaded into the rendered widget's HTML5
/// attributes (`minlength`, `maxlength`, `pattern`, `min`, `max`)
/// + the block-field-side initial value. Default value is "no
/// constraints", so existing call sites get backwards-compatible
/// behaviour after the migration to the sub-struct.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BlockFieldMeta {
    /// HTML5 `minlength` on string-shaped widgets (Text, Textarea,
    /// Email, URL, Password, Tel).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u32>,
    /// HTML5 `maxlength` on string-shaped widgets. Mirrors
    /// `Widget::max_length` already on the rendered widget — the
    /// meta entry wins when both are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u32>,
    /// HTML5 `pattern` regex on string-shaped widgets. Browser
    /// validates on form submit; the server should re-validate
    /// (TODO: server-side enforcement is a follow-up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// HTML5 `min` on numeric / date widgets. Distinct from
    /// `Widget::min` already on the rendered widget — the meta entry
    /// wins when both are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_value: Option<f64>,
    /// HTML5 `max` on numeric / date widgets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_value: Option<f64>,
    /// Initial value when a fresh instance of this widget is
    /// inserted. Serialised through the widget's `value` field so
    /// the editor shows the default in the empty-block template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
    /// Narrows a snippet chooser to one library type (its `type_name`),
    /// e.g. `"form"` so the picker lists forms only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chooser_filter: Option<String>,
}

impl BlockFieldMeta {
    /// Empty meta — sensible default for the 19 built-in blocks
    /// that don't declare any per-field constraints.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// `true` when every option is `None` — used by `serde` to skip
    /// the `meta` slot on the wire when no validators are set.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.min_length.is_none()
            && self.max_length.is_none()
            && self.pattern.is_none()
            && self.min_value.is_none()
            && self.max_value.is_none()
            && self.default_value.is_none()
            && self.chooser_filter.is_none()
    }

    /// Fluent: set HTML5 `minlength`.
    #[must_use]
    pub fn min_length(mut self, n: u32) -> Self {
        self.min_length = Some(n);
        self
    }
    /// Fluent: set HTML5 `maxlength`.
    #[must_use]
    pub fn max_length(mut self, n: u32) -> Self {
        self.max_length = Some(n);
        self
    }
    /// Fluent: set HTML5 `pattern` regex.
    #[must_use]
    pub fn pattern(mut self, p: impl Into<String>) -> Self {
        self.pattern = Some(p.into());
        self
    }
    /// Fluent: set numeric `min`.
    #[must_use]
    pub fn min_value(mut self, v: f64) -> Self {
        self.min_value = Some(v);
        self
    }
    /// Fluent: set numeric `max`.
    #[must_use]
    pub fn max_value(mut self, v: f64) -> Self {
        self.max_value = Some(v);
        self
    }
    /// Fluent: set widget initial value.
    #[must_use]
    pub fn default_value(mut self, v: impl Into<String>) -> Self {
        self.default_value = Some(v.into());
        self
    }
    /// Fluent: narrow a snippet chooser to one library type.
    #[must_use]
    pub fn chooser_filter(mut self, type_name: impl Into<String>) -> Self {
        self.chooser_filter = Some(type_name.into());
        self
    }
}

/// Per-block-type cardinality constraint on a [`BlockField::Stream`]
/// (block counts). Empty (`None` / `None`) means
/// "no constraint on this type". Independent of the stream-wide
/// `min` / `max` already on the variant.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct BlockCountConstraint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
}

/// One declared field inside a block's structural shape. The four
/// variants together cover struct, stream, list and static
/// blocks — a single
/// recursive enum, walked once, validated once, rendered once.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockField {
    /// A scalar leaf. Pairs a name with one
    /// rcms [`WidgetKind`] for the admin editor; the JSON value is
    /// whatever shape that widget POSTs.
    Widget {
        name: String,
        label: String,
        widget: WidgetKind,
        /// Choice-widget `(value, label)` pairs. Empty for non-choice
        /// kinds. Matches the existing [`crate::widget::Widget::options`]
        /// shape.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
        #[serde(default)]
        required: bool,
        /// HTML5 / value validators (`min_length`, `max_length`,
        /// `pattern`, `min_value`, `max_value`, `default_value`).
        /// Default = no constraints; threaded into the rendered
        /// `Widget` at editor render time.
        #[serde(default, skip_serializing_if = "BlockFieldMeta::is_default")]
        meta: BlockFieldMeta,
    },

    /// A read-only computed leaf — pure function of the block value.
    /// Renders inline in the admin editor + on the public page. No
    /// JSON column, no form input. Same idea as the
    /// `DisplayField` shape.
    Computed {
        name: String,
        label: String,
        /// Pure function of the block's value. The default render
        /// passes the *whole* block value (the `{name1: val1, …}`
        /// dict) so computed fields can derive from siblings.
        ///
        /// Function pointer (not `Box<dyn Fn>`) so the variant stays
        /// `Clone`-able cheaply and inventory-friendly.
        #[serde(skip)]
        render: fn(&Value, &BlockRenderCtx) -> String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },

    /// Heterogeneous nested list. `allowed`
    /// lists which registered block types may appear here. Boot-time
    /// validation in [`validate_block_registry`] verifies every entry
    /// resolves.
    Stream {
        name: String,
        label: String,
        allowed: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<u32>,
        /// Block counts — per-block-type cardinality
        /// (e.g. `{"heading": BlockCountConstraint { min: Some(1),
        /// max: Some(3) }}`). Independent of the stream-wide
        /// `min` / `max`. Empty map = no per-type constraints.
        #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
        block_counts: std::collections::HashMap<String, BlockCountConstraint>,
    },

    /// Homogeneous nested list. Convenience
    /// shorthand for `Stream { allowed: &[item_type] }` with the
    /// admin editor showing a single "+ Add" button (no picker — only
    /// one type is permitted).
    Repeat {
        name: String,
        label: String,
        item_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<u32>,
    },
}

impl BlockField {
    /// The wire-format `name` used as the dict key inside a block's
    /// `value` object.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            BlockField::Widget { name, .. } => name,
            BlockField::Computed { name, .. } => name,
            BlockField::Stream { name, .. } => name,
            BlockField::Repeat { name, .. } => name,
        }
    }

    /// Fluent constructor for a single-widget field.
    #[must_use]
    pub fn widget(name: impl Into<String>, label: impl Into<String>, widget: WidgetKind) -> Self {
        BlockField::Widget {
            name: name.into(),
            label: label.into(),
            widget,
            options: Vec::new(),
            help: None,
            required: false,
            meta: BlockFieldMeta::default(),
        }
    }

    /// Fluent constructor for a stream field.
    #[must_use]
    pub fn stream<S: Into<String>>(
        name: impl Into<String>,
        label: impl Into<String>,
        allowed: impl IntoIterator<Item = S>,
    ) -> Self {
        BlockField::Stream {
            name: name.into(),
            label: label.into(),
            allowed: allowed.into_iter().map(Into::into).collect(),
            min: None,
            max: None,
            block_counts: std::collections::HashMap::new(),
        }
    }

    /// Fluent constructor for a repeat (single-type list) field.
    #[must_use]
    pub fn repeat(
        name: impl Into<String>,
        label: impl Into<String>,
        item_type: impl Into<String>,
    ) -> Self {
        BlockField::Repeat {
            name: name.into(),
            label: label.into(),
            item_type: item_type.into(),
            min: None,
            max: None,
        }
    }

    /// Attach a validator pack ([`BlockFieldMeta`]) to a Widget
    /// variant. No-op on the other variants. Built so authors can
    /// chain: `BlockField::widget(...).with_meta(meta)`.
    #[must_use]
    pub fn with_meta(mut self, meta: BlockFieldMeta) -> Self {
        if let BlockField::Widget {
            meta: ref mut existing,
            ..
        } = self
        {
            *existing = meta;
        }
        self
    }

    /// Attach a per-block-type cardinality map to a Stream variant.
    /// No-op on other variants.
    #[must_use]
    pub fn with_block_counts(
        mut self,
        counts: std::collections::HashMap<String, BlockCountConstraint>,
    ) -> Self {
        if let BlockField::Stream {
            block_counts: ref mut existing,
            ..
        } = self
        {
            *existing = counts;
        }
        self
    }

    // ---- Fluent builder helpers — collapse the verbose match-arm
    // syntax to one-liners for the common cases. All chain. ----

    /// Mark a Widget field required (no-op on other variants).
    #[must_use]
    pub fn required(mut self) -> Self {
        if let BlockField::Widget { required, .. } = &mut self {
            *required = true;
        }
        self
    }

    /// Set helptext on a Widget field (no-op on other variants).
    #[must_use]
    pub fn with_help(mut self, h: impl Into<String>) -> Self {
        if let BlockField::Widget { help, .. } = &mut self {
            *help = Some(h.into());
        }
        self
    }

    /// Replace the choice-options on a Widget field
    /// (no-op on other variants). Pairs are `(value, label)`.
    #[must_use]
    pub fn with_options<K, V, I>(mut self, opts: I) -> Self
    where
        K: Into<String>,
        V: Into<String>,
        I: IntoIterator<Item = (K, V)>,
    {
        if let BlockField::Widget { options, .. } = &mut self {
            *options = opts
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect();
        }
        self
    }

    /// Set min cardinality on Stream / Repeat fields (no-op on Widget /
    /// Computed). Cardinality below this is a validation error.
    #[must_use]
    pub fn with_min(mut self, n: u32) -> Self {
        match &mut self {
            BlockField::Stream { min, .. } | BlockField::Repeat { min, .. } => {
                *min = Some(n);
            }
            _ => {}
        }
        self
    }

    /// Set max cardinality on Stream / Repeat fields (no-op on Widget /
    /// Computed). Cardinality above this is a validation error.
    #[must_use]
    pub fn with_max(mut self, n: u32) -> Self {
        match &mut self {
            BlockField::Stream { max, .. } | BlockField::Repeat { max, .. } => {
                *max = Some(n);
            }
            _ => {}
        }
        self
    }

    // ---- Per-WidgetKind shortcuts — for hand-rolled blocks the
    // common cases collapse to one line. Defaults are intentionally
    // minimal (not required, no validators); chain `.required()` /
    // `.with_help(...)` / `.with_options(...)` to layer on detail.

    /// Single-line text input.
    #[must_use]
    pub fn char(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::Text)
    }

    /// Multi-line text input.
    #[must_use]
    pub fn text(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::Textarea)
    }

    /// Markdown-source editor with the bundled toolbar.
    #[must_use]
    pub fn markdown(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::Markdown)
    }

    /// HTML rich-text editor.
    #[must_use]
    pub fn richtext(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::RichText)
    }

    /// `<input type="email">`.
    #[must_use]
    pub fn email(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::Email)
    }

    /// `<input type="url">`.
    #[must_use]
    pub fn url(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::Url)
    }

    /// `<input type="number" step="1">`.
    #[must_use]
    pub fn int(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::Integer)
    }

    /// `<input type="number" step="any">` (free-form decimal).
    #[must_use]
    pub fn float(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::Float)
    }

    /// `<input type="checkbox">`.
    #[must_use]
    pub fn boolean(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::Boolean)
    }

    /// `<input type="date">`.
    #[must_use]
    pub fn date(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::Date)
    }

    /// `<input type="datetime-local">`.
    #[must_use]
    pub fn datetime(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::Datetime)
    }

    /// `<select>` dropdown with the given `(value, label)` choices.
    /// Convenience around [`Self::widget`] + [`Self::with_options`].
    #[must_use]
    pub fn choice<K, V, I>(name: impl Into<String>, label: impl Into<String>, choices: I) -> Self
    where
        K: Into<String>,
        V: Into<String>,
        I: IntoIterator<Item = (K, V)>,
    {
        Self::widget(name, label, crate::widget::WidgetKind::Select).with_options(choices)
    }

    /// `MediaPicker` — pick an existing `cms_media` row.
    #[must_use]
    pub fn media(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::MediaPicker)
    }

    /// `PageChooser` — pick a `cms_page` row via modal.
    #[must_use]
    pub fn page(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::PageChooser)
    }

    /// `SnippetChooser` — pick a `cms_snippet` row via modal.
    #[must_use]
    pub fn snippet(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::SnippetChooser)
    }

    /// `DocumentChooser` — pick a non-image `cms_media` row via modal.
    #[must_use]
    pub fn document(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self::widget(name, label, crate::widget::WidgetKind::DocumentChooser)
    }
}

/// Context handed to [`Block::render`] + each computed field. Holds
/// the Tera registry (for nested `block_render` recursion) and the
/// trust-bag of pre-rendered child HTML the walker assembles bottom-up.
///
/// Sync-only — every render path is sync. Async data (translations,
/// media URLs, snippet HTML) gets pre-fetched in the admin / public
/// request handler before we start the render walk; this struct just
/// carries already-loaded references.
#[derive(Clone, Copy)]
pub struct BlockRenderCtx<'a> {
    /// Tera instance used for `tera.render(template, ctx)` calls.
    /// Block templates can recursively call back into Tera via the
    /// registered `block_render` / `stream_render` functions.
    pub tera: &'a tera::Tera,
    /// Per-request overlay of UI-defined block types (page-builder
    /// dyn blocks). Checked before the inventory registry by every stream
    /// walker. `None` = code blocks only (the default for all existing
    /// call sites).
    pub dyn_blocks: Option<&'a crate::page_builder::DynBlockSet>,
}

impl<'a> BlockRenderCtx<'a> {
    #[must_use]
    pub fn new(tera: &'a tera::Tera) -> Self {
        Self {
            tera,
            dyn_blocks: None,
        }
    }

    /// Attach the page-builder dyn-block overlay.
    #[must_use]
    pub fn with_dyn_blocks(mut self, set: &'a crate::page_builder::DynBlockSet) -> Self {
        self.dyn_blocks = Some(set);
        self
    }
}

/// One type of node that can sit in a `WidgetKind::Stream` list.
///
/// Three required methods, four optional with defaults, and
/// `render`/`migrate` have generic default impls. Implementors mostly
/// just describe their shape and let the defaults do the heavy lifting.
///
/// ## Minimal impl
///
/// ```ignore
/// use rustango_cms::{Block, BlockField};
/// use rustango_cms::widget::WidgetKind;
///
/// pub struct HeadingBlock;
///
/// impl Block for HeadingBlock {
///     fn type_name(&self) -> &'static str { "heading" }
///     fn verbose_name(&self) -> &'static str { "Heading" }
///     fn icon(&self) -> Option<&'static str> { Some("title") }
///     fn fields(&self) -> Vec<BlockField> {
///         vec![BlockField::widget("text", "Text", WidgetKind::Text)]
///     }
/// }
///
/// rustango_cms::register_block!(HeadingBlock);
/// ```
pub trait Block: Send + Sync + 'static {
    /// Wire-format identifier — the `type` key in the JSON envelope
    /// (e.g. `"heading"`). Must be globally unique across the process;
    /// duplicate registrations panic at boot.
    fn type_name(&self) -> &'static str;

    /// Human-readable label shown in the admin picker + per-block
    /// header chip.
    fn verbose_name(&self) -> &'static str;

    /// Structural shape. Each entry is rendered as one form input
    /// (or sub-stream) in the admin editor and one entry in the
    /// block's stored `value` dict.
    fn fields(&self) -> Vec<BlockField>;

    /// Material-symbols icon name shown next to the block's label in
    /// the picker + the per-block header. `None` → generic "widgets"
    /// glyph.
    fn icon(&self) -> Option<&'static str> {
        None
    }

    /// Picker grouping. Same string ⇒ same visual section in the
    /// "+ Add block" popover. `None` ⇒ "Other" bucket.
    fn group(&self) -> Option<&'static str> {
        None
    }

    /// One-line description shown in the picker tile tooltip + the
    /// "what does this block do" hint when the editor hovers over an
    /// entry. `None` ⇒ no tooltip.
    fn description(&self) -> Option<&'static str> {
        None
    }

    /// Whether new instances of this block start collapsed in the
    /// admin editor. Default `false` (expanded). Set `true` for
    /// blocks whose default value already reads well in the
    /// collapsed header so the editor scrolls less when the page
    /// has many of them.
    fn collapsed(&self) -> bool {
        false
    }

    /// Initial value when a fresh instance of this block is inserted
    /// into a stream. `None` ⇒ the editor opens a blank instance
    /// (every field empty). Useful for blocks whose canonical entry
    /// state isn't the all-empty shape (e.g. a Heading with `level=h2`
    /// pre-selected).
    ///
    /// The returned JSON must match the block's [`Self::fields`]
    /// shape — a dict keyed by field name. The admin editor merges
    /// this dict with the empty defaults on insert.
    fn default_value(&self) -> Option<Value> {
        None
    }

    /// Schema version. Bump when this block's `fields()` changes in a
    /// way that requires migrating stored JSON. Versions on disk that
    /// differ from this trigger [`Self::migrate`] on every load.
    fn version(&self) -> u32 {
        1
    }

    /// Optional override of the default render template
    /// (`blocks/<type_name>.html`). Useful for blocks that share a
    /// template with another (e.g. multiple heading-level variants).
    fn template(&self) -> Option<&'static str> {
        None
    }

    /// Collapsed-state header template. When non-None, the admin
    /// editor replaces the default static [`Self::verbose_name`]
    /// label with this string, substituting `{field_name}` tokens
    /// against the current block value. Useful so a Heading block
    /// in its collapsed state shows the actual heading text instead
    /// of just "Heading".
    ///
    /// Example: `Some("{text} (H{level})")` on a heading block
    /// renders as `Welcome (H2)` when `value = {text: "Welcome",
    /// level: "2"}`.
    ///
    /// The format string is also stamped onto the rendered block as
    /// `data-label-format=` so the stream editor JS can live-update
    /// the header when the author edits any field.
    fn label_format(&self) -> Option<&'static str> {
        None
    }

    /// Sample value used to render a preview thumbnail inside the
    /// block-picker tile. When set, the admin pre-renders this block
    /// with the sample value (via [`Self::preview_template`] or the
    /// regular block template) and stashes the resulting HTML on
    /// each [`PickerOption`](crate::block::admin::PickerOption). Editors get a "what does this look
    /// like" glimpse before they insert the block.
    fn preview_value(&self) -> Option<Value> {
        None
    }

    /// Optional override of the template used for [`Self::preview_value`]
    /// rendering. Defaults to [`Self::template`] (the same template
    /// used for the public-side render). Useful when the block's real
    /// template emits markup that needs heavy CSS / context to look
    /// right but a tiny mock preview is enough.
    fn preview_template(&self) -> Option<&'static str> {
        None
    }

    /// Augments the Tera ctx
    /// with derived values (e.g. a layout-string → grid-span map)
    /// before the block's template renders. Default returns an
    /// empty map. Keys cannot shadow the framework's own ctx
    /// entries (`value`, `block_type`, `computed`) — those are
    /// stamped last.
    ///
    /// Example:
    ///
    /// ```ignore
    /// fn extra_context(&self, value: &Value, _: &BlockRenderCtx) -> serde_json::Map<String, Value> {
    ///     let layout = value.get("layout").and_then(Value::as_str).unwrap_or("1");
    ///     let spans = match layout {
    ///         "2-1" => vec!["col-span-2", "col-span-1"],
    ///         "1-2" => vec!["col-span-1", "col-span-2"],
    ///         _ => vec!["col-span-1"],
    ///     };
    ///     serde_json::json!({ "spans": spans }).as_object().unwrap().clone()
    /// }
    /// ```
    fn extra_context(
        &self,
        _value: &Value,
        _ctx: &BlockRenderCtx<'_>,
    ) -> serde_json::Map<String, Value> {
        serde_json::Map::new()
    }

    /// Render this block's `value` to HTML. The default walks
    /// [`Self::fields`], renders each into a Tera context map, and
    /// renders the registered template. Override only for blocks
    /// that bypass the field walker entirely.
    ///
    /// # Errors
    /// Tera failures + shape mismatches surface as
    /// [`BlockError::TemplateRender`] / [`BlockError::Shape`].
    fn render(&self, value: &Value, ctx: &BlockRenderCtx<'_>) -> Result<String, BlockError> {
        crate::block::render::default_render(self, value, ctx)
    }

    /// Migrate stored JSON from an older `version()` to the current
    /// one. The registry calls this *stepwise* — once per integer
    /// version between the stored value's version and `self.version()`
    /// — so authors write single-version-step migrators only.
    ///
    /// The default is a no-op; blocks that change their schema
    /// override this.
    fn migrate(&self, _from: u32, value: Value) -> Result<Value, BlockError> {
        Ok(value)
    }
}

pub mod admin;
pub mod builtin;
pub mod enrich;
pub mod migrate;
pub mod render;
pub mod tera_helpers;
pub mod translate;

#[cfg(test)]
mod helper_tests {
    use super::*;
    use crate::widget::WidgetKind;

    #[test]
    fn char_helper_makes_text_widget() {
        let f = BlockField::char("title", "Title");
        match f {
            BlockField::Widget {
                name,
                label,
                widget,
                required,
                ..
            } => {
                assert_eq!(name, "title");
                assert_eq!(label, "Title");
                assert!(matches!(widget, WidgetKind::Text));
                assert!(!required, "default = optional");
            }
            _ => panic!("expected Widget variant"),
        }
    }

    #[test]
    fn required_chain_flips_flag() {
        let f = BlockField::int("count", "Count").required();
        if let BlockField::Widget { required, .. } = f {
            assert!(required);
        } else {
            panic!("expected Widget");
        }
    }

    #[test]
    fn choice_helper_carries_options() {
        let f = BlockField::choice(
            "level",
            "Level",
            vec![("2", "H2"), ("3", "H3"), ("4", "H4")],
        );
        if let BlockField::Widget {
            widget, options, ..
        } = f
        {
            assert!(matches!(widget, WidgetKind::Select));
            assert_eq!(options.len(), 3);
            assert_eq!(options[0], ("2".to_owned(), "H2".to_owned()));
        } else {
            panic!("expected Widget");
        }
    }

    #[test]
    fn with_help_on_widget_only() {
        let f = BlockField::text("body", "Body").with_help("Markdown supported");
        if let BlockField::Widget { help, .. } = f {
            assert_eq!(help.as_deref(), Some("Markdown supported"));
        }
        // No-op on other variants.
        let s = BlockField::stream("items", "Items", ["heading"]).with_help("hint");
        if let BlockField::Stream { .. } = s {
            // unchanged — no `help` field to set
        }
    }

    #[test]
    fn with_min_max_targets_stream_and_repeat() {
        let s = BlockField::stream("items", "Items", ["heading"])
            .with_min(1)
            .with_max(5);
        if let BlockField::Stream { min, max, .. } = s {
            assert_eq!(min, Some(1));
            assert_eq!(max, Some(5));
        } else {
            panic!("expected Stream");
        }
        let r = BlockField::repeat("rows", "Rows", "row").with_min(2);
        if let BlockField::Repeat { min, .. } = r {
            assert_eq!(min, Some(2));
        } else {
            panic!("expected Repeat");
        }
    }
}
