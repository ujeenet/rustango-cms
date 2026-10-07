//! Built-in blocks shipped with rcms. Each one is a single Rust
//! struct plus a Tera template at `blocks/<type_name>.html` (in
//! [`crate::admin::register_templates`]). Host crates register
//! additional blocks via [`crate::register_block!`].
//!
//! These ship with sensible defaults and a minimal HTML template so
//! the editor + render path produce a working page on day one. Hosts
//! can override the template by registering a same-named template
//! before the bundled load — Tera resolves the most-recently-added
//! template, which is the standard precedence rule.

use crate::block::BlockFieldMeta;
use crate::widget::WidgetKind;
use crate::{register_block, Block, BlockField};

/// `heading` — a heading with a level choice. Two fields:
/// `text` (the heading body) and `level` (`2`..`4`, defaulting to
/// H2). Template emits `<h{level}>{text}</h{level}>`.
#[derive(Default)]
pub struct HeadingBlock;

impl Block for HeadingBlock {
    fn type_name(&self) -> &'static str {
        "heading"
    }
    fn verbose_name(&self) -> &'static str {
        "Heading"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("title")
    }
    fn label_format(&self) -> Option<&'static str> {
        Some("{text}")
    }
    fn preview_value(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "text": "Example heading", "level": "2" }))
    }
    // Reads as straight English now: "text is a required char field,
    // level is a choice of H2/H3/H4". Compare with the previous
    // ~24-line match-arm form — same shape, ~75% fewer lines.
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::char("text", "Text").required(),
            BlockField::choice("level", "Level", [("2", "H2"), ("3", "H3"), ("4", "H4")]),
        ]
    }
}

/// `paragraph` — One markdown field
/// rendered through the rcms safe-markdown pipeline on the public
/// side.
#[derive(Default)]
pub struct ParagraphBlock;

impl Block for ParagraphBlock {
    fn type_name(&self) -> &'static str {
        "paragraph"
    }
    fn verbose_name(&self) -> &'static str {
        "Paragraph"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("notes")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "body".to_owned(),
            label: "Body".to_owned(),
            widget: WidgetKind::Markdown,
            options: Vec::new(),
            help: Some("GFM markdown.".to_owned()),
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }
}

/// `image` — Picks a `cms_media` row
/// + optional alt text + optional caption. Template emits a `<figure>`
/// with the image URL resolved server-side.
#[derive(Default)]
pub struct ImageBlock;

impl Block for ImageBlock {
    fn type_name(&self) -> &'static str {
        "image"
    }
    fn verbose_name(&self) -> &'static str {
        "Image"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("image")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::Widget {
                name: "media_id".to_owned(),
                label: "Image".to_owned(),
                widget: WidgetKind::MediaPicker,
                options: Vec::new(),
                help: None,
                required: true,
                meta: BlockFieldMeta::default(),
            },
            BlockField::Widget {
                name: "alt".to_owned(),
                label: "Alt text".to_owned(),
                widget: WidgetKind::Text,
                options: Vec::new(),
                help: Some("Empty if decorative.".to_owned()),
                required: false,
                meta: BlockFieldMeta::default(),
            },
            BlockField::Widget {
                name: "caption".to_owned(),
                label: "Caption".to_owned(),
                widget: WidgetKind::Text,
                options: Vec::new(),
                help: None,
                required: false,
                meta: BlockFieldMeta::default(),
            },
        ]
    }
}

/// `quote` — Textarea body + optional
/// attribution.
#[derive(Default)]
pub struct QuoteBlock;

impl Block for QuoteBlock {
    fn type_name(&self) -> &'static str {
        "quote"
    }
    fn verbose_name(&self) -> &'static str {
        "Quote"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("format_quote")
    }
    fn label_format(&self) -> Option<&'static str> {
        Some("“{text}”")
    }
    fn preview_value(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "text": "The best way to predict the future is to invent it.",
            "attribution": "Alan Kay"
        }))
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::Widget {
                name: "text".to_owned(),
                label: "Quote".to_owned(),
                widget: WidgetKind::Textarea,
                options: Vec::new(),
                help: None,
                required: true,
                meta: BlockFieldMeta::default(),
            },
            BlockField::Widget {
                name: "attribution".to_owned(),
                label: "Attribution".to_owned(),
                widget: WidgetKind::Text,
                options: Vec::new(),
                help: None,
                required: false,
                meta: BlockFieldMeta::default(),
            },
        ]
    }
}

/// `embed` — A single URL; the public
/// template renders a responsive embed. [`Self::extra_context`] runs
/// the URL through [`crate::embed::resolve`] so a YouTube
/// *watch* or Vimeo *page* URL becomes the provider's embeddable
/// `src` with the right aspect ratio; unrecognised URLs link out
/// rather than iframing an arbitrary site.
#[derive(Default)]
pub struct EmbedBlock;

impl Block for EmbedBlock {
    fn type_name(&self) -> &'static str {
        "embed"
    }
    fn verbose_name(&self) -> &'static str {
        "Embed"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("smart_display")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "url".to_owned(),
            label: "URL".to_owned(),
            widget: WidgetKind::Url,
            options: Vec::new(),
            help: Some("YouTube or Vimeo URL (others link out).".to_owned()),
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }

    fn extra_context(
        &self,
        value: &serde_json::Value,
        _ctx: &crate::block::BlockRenderCtx<'_>,
    ) -> serde_json::Map<String, serde_json::Value> {
        let url = value
            .get("url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        // Always insert the three keys (empty when unrecognised) so the
        // template can branch with a plain `{% if embed_src %}` without
        // tripping Tera's undefined-variable error.
        let (src, provider, aspect) = match crate::embed::resolve(url) {
            Some(t) => (
                t.src,
                t.provider.name().to_owned(),
                t.aspect_ratio.to_owned(),
            ),
            None => (String::new(), String::new(), String::new()),
        };
        let mut m = serde_json::Map::new();
        m.insert("embed_src".to_owned(), serde_json::Value::String(src));
        m.insert("provider".to_owned(), serde_json::Value::String(provider));
        m.insert("aspect_ratio".to_owned(), serde_json::Value::String(aspect));
        m.insert("safe_href".to_owned(), safe_href_value(url));
        m
    }
}

/// An author-typed link target, or `None` when it would run script.
///
/// The url and embed inputs only validate in the browser, so a
/// `javascript:` (or `vbscript:` / `data:`) value reaches the rendered
/// `href` otherwise. Relative targets and the http(s), mailto and
/// tel schemes pass. The scheme is read the way a browser reads it, with
/// tabs, newlines and other control characters removed, so `java\tscript:`
/// is caught too.
#[must_use]
pub fn safe_href(raw: &str) -> Option<&str> {
    let raw = raw.trim();
    let squashed: String = raw.chars().filter(|c| !c.is_ascii_control() && *c != ' ').collect();
    let scheme = match squashed.find([':', '/', '?', '#']) {
        Some(i) if squashed[i..].starts_with(':') => squashed[..i].to_ascii_lowercase(),
        _ => return Some(raw),
    };
    matches!(scheme.as_str(), "http" | "https" | "mailto" | "tel").then_some(raw)
}

fn safe_href_value(raw: &str) -> serde_json::Value {
    serde_json::Value::String(safe_href(raw).unwrap_or_default().to_owned())
}

/// Curated languages for [`CodeBlock`]'s dropdown. Values are the
/// highlighter token emitted in the `language-…` class.
const CODE_LANGUAGES: &[(&str, &str)] = &[
    ("bash", "Bash / shell"),
    ("c", "C"),
    ("cpp", "C++"),
    ("csharp", "C#"),
    ("css", "CSS"),
    ("diff", "Diff"),
    ("dockerfile", "Dockerfile"),
    ("go", "Go"),
    ("html", "HTML"),
    ("java", "Java"),
    ("javascript", "JavaScript"),
    ("json", "JSON"),
    ("kotlin", "Kotlin"),
    ("markdown", "Markdown"),
    ("php", "PHP"),
    ("python", "Python"),
    ("ruby", "Ruby"),
    ("rust", "Rust"),
    ("scss", "SCSS"),
    ("sql", "SQL"),
    ("swift", "Swift"),
    ("toml", "TOML"),
    ("typescript", "TypeScript"),
    ("xml", "XML"),
    ("yaml", "YAML"),
];

/// `code` — a fenced code block with a language hint. Renders
/// the standard `<pre><code class="language-…">` markup that client
/// highlighters (Prism / highlight.js / Shiki) pick up automatically.
/// The body is HTML-escaped by Tera, so it displays verbatim and can't
/// inject markup. Server-side highlighting (syntect) would add a heavy
/// dependency and is better as an opt-in follow-up — this markup is
/// highlighter-agnostic.
#[derive(Default)]
pub struct CodeBlock;

impl Block for CodeBlock {
    fn type_name(&self) -> &'static str {
        "code"
    }
    fn verbose_name(&self) -> &'static str {
        "Code"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("code")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::choice("language", "Language", CODE_LANGUAGES.iter().copied()),
            BlockField::text("code", "Code").required(),
        ]
    }
    fn preview_value(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "language": "rust",
            "code": "fn main() {\n    println!(\"hello\");\n}",
        }))
    }
}

/// `text` — Multi-line plain text rendered
/// inside `<p>` with newlines preserved.
#[derive(Default)]
pub struct TextBlock;

impl Block for TextBlock {
    fn type_name(&self) -> &'static str {
        "text"
    }
    fn verbose_name(&self) -> &'static str {
        "Text"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("text_fields")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "body".to_owned(),
            label: "Body".to_owned(),
            widget: WidgetKind::Textarea,
            options: Vec::new(),
            help: None,
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }
}

/// `boolean` — Single checkbox; the template
/// renders nothing by default (host overrides decide what "true"
/// means in context — show a banner, flip a class, …).
#[derive(Default)]
pub struct BooleanBlock;

impl Block for BooleanBlock {
    fn type_name(&self) -> &'static str {
        "boolean"
    }
    fn verbose_name(&self) -> &'static str {
        "Boolean (checkbox)"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("check_box")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "value".to_owned(),
            label: "Checked".to_owned(),
            widget: WidgetKind::Boolean,
            options: Vec::new(),
            help: None,
            required: false,
            meta: BlockFieldMeta::default(),
        }]
    }
}

/// `date` — ISO-8601 date string (`YYYY-MM-DD`).
#[derive(Default)]
pub struct DateBlock;

impl Block for DateBlock {
    fn type_name(&self) -> &'static str {
        "date"
    }
    fn verbose_name(&self) -> &'static str {
        "Date"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("calendar_today")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "value".to_owned(),
            label: "Date".to_owned(),
            widget: WidgetKind::Date,
            options: Vec::new(),
            help: None,
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }
}

/// `datetime` — ISO-8601 datetime string.
#[derive(Default)]
pub struct DateTimeBlock;

impl Block for DateTimeBlock {
    fn type_name(&self) -> &'static str {
        "datetime"
    }
    fn verbose_name(&self) -> &'static str {
        "Date & time"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("event")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "value".to_owned(),
            label: "When".to_owned(),
            widget: WidgetKind::Datetime,
            options: Vec::new(),
            help: None,
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }
}

/// `email` — Browser-validates the shape via
/// `<input type="email">`; the template renders a `mailto:` link.
#[derive(Default)]
pub struct EmailBlock;

impl Block for EmailBlock {
    fn type_name(&self) -> &'static str {
        "email"
    }
    fn verbose_name(&self) -> &'static str {
        "Email"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("mail")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::Widget {
                name: "value".to_owned(),
                label: "Email address".to_owned(),
                widget: WidgetKind::Email,
                options: Vec::new(),
                help: None,
                required: true,
                meta: BlockFieldMeta::default(),
            },
            BlockField::Widget {
                name: "label".to_owned(),
                label: "Display label".to_owned(),
                widget: WidgetKind::Text,
                options: Vec::new(),
                help: Some("Defaults to the email address itself.".to_owned()),
                required: false,
                meta: BlockFieldMeta::default(),
            },
        ]
    }
}

/// `url` — Browser-validates via
/// `<input type="url">`; the template renders an anchor.
#[derive(Default)]
pub struct UrlBlock;

impl Block for UrlBlock {
    fn type_name(&self) -> &'static str {
        "url"
    }
    fn verbose_name(&self) -> &'static str {
        "URL"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("link")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::Widget {
                name: "value".to_owned(),
                label: "URL".to_owned(),
                widget: WidgetKind::Url,
                options: Vec::new(),
                help: None,
                required: true,
                meta: BlockFieldMeta::default(),
            },
            BlockField::Widget {
                name: "label".to_owned(),
                label: "Display label".to_owned(),
                widget: WidgetKind::Text,
                options: Vec::new(),
                help: Some("Defaults to the URL itself.".to_owned()),
                required: false,
                meta: BlockFieldMeta::default(),
            },
        ]
    }

    fn extra_context(
        &self,
        value: &serde_json::Value,
        _ctx: &crate::block::BlockRenderCtx<'_>,
    ) -> serde_json::Map<String, serde_json::Value> {
        let raw = value.get("value").and_then(serde_json::Value::as_str).unwrap_or("");
        let mut m = serde_json::Map::new();
        m.insert("safe_href".to_owned(), safe_href_value(raw));
        m
    }
}

/// `integer` — Numeric input; the template
/// renders the value as-is. Editors needing a unit / formatting
/// override the template.
#[derive(Default)]
pub struct IntegerBlock;

impl Block for IntegerBlock {
    fn type_name(&self) -> &'static str {
        "integer"
    }
    fn verbose_name(&self) -> &'static str {
        "Integer"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("123")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "value".to_owned(),
            label: "Value".to_owned(),
            widget: WidgetKind::Number,
            options: Vec::new(),
            help: None,
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }
}

/// `choice` — Editor picks one entry from a
/// host-supplied list of `(value, label)` pairs. Hosts override
/// [`Block::fields`] by subclassing to set their own option list
/// (the registered default lands as an empty select — actionable
/// only when a host extends).
///
/// Authors who want richer choices should register a per-site
/// `ChoiceBlock` variant via `register_block!` with their own list.
#[derive(Default)]
pub struct ChoiceBlock;

impl Block for ChoiceBlock {
    fn type_name(&self) -> &'static str {
        "choice"
    }
    fn verbose_name(&self) -> &'static str {
        "Choice"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("list_alt")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "value".to_owned(),
            label: "Choice".to_owned(),
            widget: WidgetKind::Select,
            // Empty by default — hosts override the template or
            // register a subclassed block with their own options.
            options: Vec::new(),
            help: Some(
                "Pick a value from the host-configured list. Edit the block to extend choices."
                    .to_owned(),
            ),
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }
}

/// `table` — Editor enters TSV (one row per
/// line, tab-separated cells); the public template renders a
/// `<table>` with the first row as `<thead>`. Cells preserve as
/// plain text. Pasting straight from a spreadsheet works because
/// Excel / Google Sheets / Numbers all serialise selected ranges
/// as TSV on the clipboard.
///
/// TypedTableBlock (schema-driven columns) lives as a separate
/// future block — the data shape differs enough to warrant its
/// own type.
#[derive(Default)]
pub struct TableBlock;

impl Block for TableBlock {
    fn type_name(&self) -> &'static str {
        "table"
    }
    fn verbose_name(&self) -> &'static str {
        "Table"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("table")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::Widget {
                name: "tsv".to_owned(),
                label: "Rows".to_owned(),
                widget: WidgetKind::Textarea,
                options: Vec::new(),
                help: Some(
                    "One row per line, columns separated by tabs. The first row renders as <thead>. Paste from a spreadsheet to fill — Excel / Sheets / Numbers all copy as TSV."
                        .to_owned(),
                ),
                required: true,
                meta: BlockFieldMeta::default(),
            },
            BlockField::Widget {
                name: "caption".to_owned(),
                label: "Caption".to_owned(),
                widget: WidgetKind::Text,
                options: Vec::new(),
                help: None,
                required: false,
                meta: BlockFieldMeta::default(),
            },
        ]
    }
}

/// `float` — Free-form floating-point input.
/// Distinct from `IntegerBlock` (which uses `WidgetKind::Number` with
/// `step="1"`) — this one accepts decimals.
#[derive(Default)]
pub struct FloatBlock;

impl Block for FloatBlock {
    fn type_name(&self) -> &'static str {
        "float"
    }
    fn verbose_name(&self) -> &'static str {
        "Float"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("decimal_increase")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "value".to_owned(),
            label: "Value".to_owned(),
            widget: WidgetKind::Float,
            options: Vec::new(),
            help: None,
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }
}

/// `time` — ISO `HH:MM[:SS]` string.
#[derive(Default)]
pub struct TimeBlock;

impl Block for TimeBlock {
    fn type_name(&self) -> &'static str {
        "time"
    }
    fn verbose_name(&self) -> &'static str {
        "Time"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("schedule")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "value".to_owned(),
            label: "Time".to_owned(),
            widget: WidgetKind::Time,
            options: Vec::new(),
            help: None,
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }
}

/// `multiple_choice` — Distinct from
/// `ChoiceBlock` (single-pick `<select>`) — this one renders stacked
/// checkboxes and stores a JSON-array string. Hosts register a
/// subclassed variant with their own option list (default ships with
/// no options, like `ChoiceBlock`).
#[derive(Default)]
pub struct MultipleChoiceBlock;

impl Block for MultipleChoiceBlock {
    fn type_name(&self) -> &'static str {
        "multiple_choice"
    }
    fn verbose_name(&self) -> &'static str {
        "Multiple choice"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("checklist")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "values".to_owned(),
            label: "Choices".to_owned(),
            widget: WidgetKind::Checkboxes,
            options: Vec::new(),
            help: Some(
                "Pick zero or more values from the host-configured list. Stored as a JSON array."
                    .to_owned(),
            ),
            required: false,
            meta: BlockFieldMeta::default(),
        }]
    }
    fn render(
        &self,
        value: &serde_json::Value,
        ctx: &crate::block::BlockRenderCtx<'_>,
    ) -> Result<String, crate::block::BlockError> {
        // Pre-parse the JSON-array string stored by the Checkboxes
        // widget into a real array so the template can iterate
        // without a custom `json_decode` filter. Falls back to an
        // empty array on bad input — better than failing render.
        let mut prepared = value.as_object().cloned().unwrap_or_default();
        let parsed: Vec<serde_json::Value> = prepared
            .get("values")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        prepared.insert("values".to_owned(), serde_json::Value::Array(parsed));
        crate::block::render::default_render(self, &serde_json::Value::Object(prepared), ctx)
    }
}

/// `static` — Marker block with no fields;
/// the public template emits whatever static HTML the host wants
/// (divider, call-out, branded heading rule). Inserting one in a
/// StreamField is the editor's signal "render the host's static
/// content here".
#[derive(Default)]
pub struct StaticBlock;

impl Block for StaticBlock {
    fn type_name(&self) -> &'static str {
        "static"
    }
    fn verbose_name(&self) -> &'static str {
        "Static block"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("horizontal_rule")
    }
    fn fields(&self) -> Vec<BlockField> {
        // Intentionally empty — the host template controls the output.
        Vec::new()
    }
}

/// `raw_html` — Editor types HTML; the
/// public template sanitises through the same ammonia pipeline as
/// the Markdown widget (`crate::markdown::render`'s post-pass) so
/// `<script>` / `<iframe>` / `on*` handlers are stripped.
///
/// Distinct from `ParagraphBlock` (markdown source → rendered HTML)
/// — this one stores HTML directly. Use when content authors already
/// have hand-written HTML they want to drop in.
#[derive(Default)]
pub struct RawHtmlBlock;

impl Block for RawHtmlBlock {
    fn type_name(&self) -> &'static str {
        "raw_html"
    }
    fn verbose_name(&self) -> &'static str {
        "Raw HTML"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("code")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "html".to_owned(),
            label: "HTML".to_owned(),
            widget: WidgetKind::Textarea,
            options: Vec::new(),
            help: Some(
                "Raw HTML. The public template sanitises through ammonia before render — script / iframe / on* attrs are stripped."
                    .to_owned(),
            ),
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }
    fn render(
        &self,
        value: &serde_json::Value,
        ctx: &crate::block::BlockRenderCtx<'_>,
    ) -> Result<String, crate::block::BlockError> {
        // Pre-sanitise the stored HTML through the same ammonia
        // pipeline `markdown::render` uses for its post-pass, then
        // hand the cleaned string to the template marked `| safe`.
        // Editors typing `<script>` get a no-op render rather than
        // an XSS vector.
        let mut prepared = value.as_object().cloned().unwrap_or_default();
        let cleaned = prepared
            .get("html")
            .and_then(serde_json::Value::as_str)
            .map(ammonia::clean)
            .unwrap_or_default();
        prepared.insert("html".to_owned(), serde_json::Value::String(cleaned));
        crate::block::render::default_render(self, &serde_json::Value::Object(prepared), ctx)
    }
}

/// `page_chooser` — Picks a single
/// `cms_page` row via the modal `PageChooser` widget; stores the page
/// id as a stringified integer.
///
/// The public template receives `value.page_id` (string) — host
/// templates resolve the page via Tera helpers (`url_for_page`) or
/// override the template to render an internal link.
#[derive(Default)]
pub struct PageChooserBlock;

impl Block for PageChooserBlock {
    fn type_name(&self) -> &'static str {
        "page_chooser"
    }
    fn verbose_name(&self) -> &'static str {
        "Page link"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("article")
    }
    fn description(&self) -> Option<&'static str> {
        Some("Link to another page in this site.")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::Widget {
                name: "page_id".to_owned(),
                label: "Page".to_owned(),
                widget: WidgetKind::PageChooser,
                options: Vec::new(),
                help: None,
                required: true,
                meta: BlockFieldMeta::default(),
            },
            BlockField::Widget {
                name: "label".to_owned(),
                label: "Display label".to_owned(),
                widget: WidgetKind::Text,
                options: Vec::new(),
                help: Some("Optional override — defaults to the chosen page's title.".to_owned()),
                required: false,
                meta: BlockFieldMeta::default(),
            },
        ]
    }
}

/// `snippet_chooser` — Picks a
/// `cms_snippet` row via the modal `SnippetChooser` widget. The
/// stored value is the snippet id as a stringified integer.
///
/// Hosts that want to narrow the picker to one snippet `type_name`
/// register a subclassed variant + set `Widget::custom_name` on the
/// chooser field at construction (admin reads it via
/// `data-chooser-filter`). The default ships unfiltered so editors
/// can pick any snippet.
#[derive(Default)]
pub struct SnippetChooserBlock;

impl Block for SnippetChooserBlock {
    fn type_name(&self) -> &'static str {
        "snippet_chooser"
    }
    fn verbose_name(&self) -> &'static str {
        "Snippet"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("library_books")
    }
    fn description(&self) -> Option<&'static str> {
        Some("Pick a reusable snippet from the library.")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "snippet_id".to_owned(),
            label: "Snippet".to_owned(),
            widget: WidgetKind::SnippetChooser,
            options: Vec::new(),
            help: None,
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }
}

/// `document_chooser` — Picks a
/// non-image `cms_media` row (e.g. PDF, archive) via the modal
/// `DocumentChooser` widget. Stored value is the media id as a
/// stringified integer.
#[derive(Default)]
pub struct DocumentChooserBlock;

impl Block for DocumentChooserBlock {
    fn type_name(&self) -> &'static str {
        "document_chooser"
    }
    fn verbose_name(&self) -> &'static str {
        "Document link"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("description")
    }
    fn description(&self) -> Option<&'static str> {
        Some("Link to a PDF or other non-image document in the media library.")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::Widget {
                name: "document_id".to_owned(),
                label: "Document".to_owned(),
                widget: WidgetKind::DocumentChooser,
                options: Vec::new(),
                help: None,
                required: true,
                meta: BlockFieldMeta::default(),
            },
            BlockField::Widget {
                name: "label".to_owned(),
                label: "Display label".to_owned(),
                widget: WidgetKind::Text,
                options: Vec::new(),
                help: Some("Optional — defaults to the document's title.".to_owned()),
                required: false,
                meta: BlockFieldMeta::default(),
            },
        ]
    }
}

register_block!(HeadingBlock);
register_block!(ParagraphBlock);
register_block!(ImageBlock);
register_block!(QuoteBlock);
register_block!(EmbedBlock);
register_block!(CodeBlock);
register_block!(TextBlock);
register_block!(BooleanBlock);
register_block!(DateBlock);
register_block!(DateTimeBlock);
register_block!(EmailBlock);
register_block!(UrlBlock);
register_block!(IntegerBlock);
register_block!(ChoiceBlock);
register_block!(TableBlock);
/// `typed_table_row` — one row inside the default TypedTableBlock.
/// Three columns: `label` (text), `value` (number), `notes` (text).
/// This serves as a working baseline; hosts that need a different
/// column schema register their own row block + their own
/// `TypedTable`-style wrapper.
///
/// Hosts subclass by registering a new row block with the columns
/// they want, then registering a wrapper block whose `fields()`
/// returns `BlockField::repeat("rows", "Rows", "<their_row_type>")`.
#[derive(Default)]
pub struct TypedTableRowBlock;

impl Block for TypedTableRowBlock {
    fn type_name(&self) -> &'static str {
        "typed_table_row"
    }
    fn verbose_name(&self) -> &'static str {
        "Table row"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("table_rows")
    }
    fn description(&self) -> Option<&'static str> {
        Some("One row in a typed table. Subclass to define your own columns.")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::Widget {
                name: "label".to_owned(),
                label: "Label".to_owned(),
                widget: WidgetKind::Text,
                options: Vec::new(),
                help: None,
                required: true,
                meta: BlockFieldMeta::default(),
            },
            BlockField::Widget {
                name: "value".to_owned(),
                label: "Value".to_owned(),
                widget: WidgetKind::Number,
                options: Vec::new(),
                help: None,
                required: false,
                meta: BlockFieldMeta::default(),
            },
            BlockField::Widget {
                name: "notes".to_owned(),
                label: "Notes".to_owned(),
                widget: WidgetKind::Text,
                options: Vec::new(),
                help: None,
                required: false,
                meta: BlockFieldMeta::default(),
            },
        ]
    }
}

/// `typed_table` — Schema-driven typed
/// grid where each row's cells are individually-typed widgets (unlike
/// `TableBlock` which is TSV-only).
///
/// Wire shape:
/// ```text
/// { "caption": "...", "rows": [
///     { "type": "typed_table_row", "id": "uuid", "value": { "label": ..., "value": ..., "notes": ... }}
/// ]}
/// ```
///
/// Editor UX: today renders as a stacked list of typed input rows
/// (one form-row per data-row) — a real spreadsheet-style grid is a
/// future enhancement. The `Repeat` BlockField underneath gives
/// add / remove / move-up / move-down out of the box from the
/// existing stream editor JS.
///
/// Hosts subclass by registering their own row block + their own
/// wrapper block; the default ships with the three-column row above
/// as a working baseline.
#[derive(Default)]
pub struct TypedTableBlock;

impl Block for TypedTableBlock {
    fn type_name(&self) -> &'static str {
        "typed_table"
    }
    fn verbose_name(&self) -> &'static str {
        "Typed table"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("grid_on")
    }
    fn description(&self) -> Option<&'static str> {
        Some("Typed grid — each column is a properly-typed widget. Subclass to define your own columns.")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::Widget {
                name: "caption".to_owned(),
                label: "Caption".to_owned(),
                widget: WidgetKind::Text,
                options: Vec::new(),
                help: None,
                required: false,
                meta: BlockFieldMeta::default(),
            },
            BlockField::Repeat {
                name: "rows".to_owned(),
                label: "Rows".to_owned(),
                item_type: "typed_table_row".to_owned(),
                min: None,
                max: None,
            },
        ]
    }
}

/// `rich_text` — Editor surface for
/// hosts wiring a WYSIWYG (EasyMDE / TipTap / Draftail) via the
/// `WidgetKind::RichText` widget's `data-widget-mode="richtext"`
/// hook. Without a host enhancer the textarea ships with the
/// existing markdown toolbar — same editor as ParagraphBlock
/// but stores HTML directly.
///
/// Distinct from:
/// - `ParagraphBlock` (markdown source → rendered via the markdown
///   pipeline)
/// - `RawHtmlBlock` (plain `<textarea>` for HTML, no WYSIWYG hook)
///
/// Render path: stored HTML through ammonia sanitisation — script /
/// iframe / on* attrs stripped — then emitted with `| safe`.
#[derive(Default)]
pub struct RichTextBlock;

impl Block for RichTextBlock {
    fn type_name(&self) -> &'static str {
        "rich_text"
    }
    fn verbose_name(&self) -> &'static str {
        "Rich text"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("article")
    }
    fn description(&self) -> Option<&'static str> {
        Some("Formatted text — hosts wire a WYSIWYG via the richtext widget hook.")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "body".to_owned(),
            label: "Body".to_owned(),
            widget: WidgetKind::RichText,
            options: Vec::new(),
            help: Some(
                "Hosts swap in EasyMDE / TipTap / Draftail via the richtext widget hook. Without one, the editor falls back to the markdown toolbar."
                    .to_owned(),
            ),
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }
    fn render(
        &self,
        value: &serde_json::Value,
        ctx: &crate::block::BlockRenderCtx<'_>,
    ) -> Result<String, crate::block::BlockError> {
        // Sanitise with the rich-text policy, which keeps the editor's
        // move-safe `<a linktype="page|media" id="N">` anchors (default
        // ammonia strips `linktype`/`id`, which left every internal link
        // in a block without a target — #682). The page render resolves
        // them afterwards; the template emits `| safe`.
        let mut prepared = value.as_object().cloned().unwrap_or_default();
        let cleaned = prepared
            .get("body")
            .and_then(serde_json::Value::as_str)
            .map(crate::markdown::sanitize_html)
            .unwrap_or_default();
        prepared.insert("body".to_owned(), serde_json::Value::String(cleaned));
        crate::block::render::default_render(self, &serde_json::Value::Object(prepared), ctx)
    }
}

/// `decimal` — Like FloatBlock but the
/// default ships with a stable `step="any"` hint via the Float
/// widget. Hosts that need bounded precision pass a custom
/// `BlockFieldMeta` via the `with_meta` setter (chained at
/// registration time on a subclassed variant). Storage is the
/// decimal as a string so banking-grade precision round-trips
/// through serde without f64 loss.
#[derive(Default)]
pub struct DecimalBlock;

impl Block for DecimalBlock {
    fn type_name(&self) -> &'static str {
        "decimal"
    }
    fn verbose_name(&self) -> &'static str {
        "Decimal"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("attach_money")
    }
    fn description(&self) -> Option<&'static str> {
        Some("Decimal number (stored as text for precision).")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "value".to_owned(),
            label: "Value".to_owned(),
            widget: WidgetKind::Float,
            options: Vec::new(),
            help: Some(
                "Stored as a string for exact precision; the editor renders a number input."
                    .to_owned(),
            ),
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }
}

/// `regex` — Text input with a pattern
/// validator. The default ships pattern-less (accepts anything);
/// hosts subclass + override `fields()` to attach a regex via
/// `BlockFieldMeta::default().pattern("^[A-Z]+$")`. The browser
/// validates on submit via HTML5 `pattern`.
#[derive(Default)]
pub struct RegexBlock;

impl Block for RegexBlock {
    fn type_name(&self) -> &'static str {
        "regex"
    }
    fn verbose_name(&self) -> &'static str {
        "Regex-validated text"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("pattern")
    }
    fn description(&self) -> Option<&'static str> {
        Some("Plain text validated against a regex pattern (host-configured).")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![BlockField::Widget {
            name: "value".to_owned(),
            label: "Value".to_owned(),
            widget: WidgetKind::Text,
            options: Vec::new(),
            help: Some(
                "Hosts wire a pattern via BlockField::widget(...).with_meta(\
                BlockFieldMeta::default().pattern(\"...\")). The browser \
                enforces it on form submit."
                    .to_owned(),
            ),
            required: true,
            meta: BlockFieldMeta::default(),
        }]
    }
}

register_block!(FloatBlock);
register_block!(DecimalBlock);
register_block!(RegexBlock);
register_block!(RichTextBlock);
register_block!(TypedTableRowBlock);
register_block!(TypedTableBlock);
register_block!(TimeBlock);
register_block!(MultipleChoiceBlock);
register_block!(StaticBlock);
register_block!(RawHtmlBlock);
register_block!(PageChooserBlock);
register_block!(SnippetChooserBlock);
register_block!(DocumentChooserBlock);

#[cfg(test)]
mod safe_href_tests {
    use super::safe_href;

    #[test]
    fn script_schemes_are_dropped_however_spelled() {
        for bad in [
            "javascript:alert(1)",
            "  JAVASCRIPT:alert(1)",
            "java\tscript:alert(1)",
            "java\nscript:alert(1)",
            "\u{1}javascript:x",
            "vbscript:x",
            "data:text/html;base64,PHNjcmlwdD4=",
        ] {
            assert_eq!(safe_href(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn web_mail_phone_and_relative_targets_pass() {
        for ok in [
            "https://example.com",
            "HTTP://example.com",
            "mailto:a@example.com",
            "tel:+1555",
            "/about",
            "about/team",
            "#top",
            "?q=1",
            "/path:with:colons",
        ] {
            assert_eq!(safe_href(ok), Some(ok), "{ok:?}");
        }
    }
}

#[cfg(test)]
mod raw_html_tests {
    use super::RawHtmlBlock;
    use crate::block::{Block, BlockRenderCtx};

    /// The raw HTML block is on by default for every editor, so its
    /// render is what keeps an editor's markup from running script on the
    /// site: anything executable must be gone before the `| safe` template.
    #[test]
    fn executable_markup_is_stripped_on_render() {
        let mut tera = tera::Tera::default();
        tera.add_raw_template("blocks/raw_html.html", include_str!("../admin/templates/blocks/raw_html.html"))
            .expect("template");
        let ctx = BlockRenderCtx::new(&tera);
        let html = r#"<p>Hello <b>there</b></p><script>alert(1)</script><img src="x.png" onerror="alert(2)"><a href="javascript:alert(3)">x</a><iframe src="https://evil.example"></iframe><svg onload="alert(4)"></svg>"#;
        let out = RawHtmlBlock.render(&serde_json::json!({ "html": html }), &ctx).expect("render");
        assert!(out.contains("<b>there</b>"), "safe markup survives: {out}");
        for bad in ["<script", "alert(1)", "onerror", "javascript:", "<iframe", "onload"] {
            assert!(!out.contains(bad), "{bad} stripped: {out}");
        }
    }
}
