//! Form Builder schema.
//!
//! The visual builder stores a whole form as one JSON document in the
//! backing snippet's `data` column (`type_name = "form"`). This module is
//! the pure, IO-free contract for that document: serde types + validation
//! + small traversal helpers. Everything downstream — the builder editor,
//! the public renderer, the submit handler, the submissions admin — reads
//! and writes through these types.
//!
//! Shape (mirrors the user's hierarchy):
//!
//! ```text
//! Form
//!  ├─ settings (submit label, success message/redirect, notify emails)
//!  └─ pages[]            ← wizard steps (next/prev)
//!      └─ sections[]     ← logical groupings within a page
//!          └─ rows[]     ← a 12-column grid row
//!              └─ columns[] (width 1..=12)
//!                  └─ fields[]   ← the typed inputs
//! ```
//!
//! IDs are client-minted UUID strings, stable across reorder (the builder
//! JS reuses them); validation never depends on array position.

use serde::{Deserialize, Serialize};

/// One built form. Serialized as the snippet `data` JSON.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Form {
    #[serde(default)]
    pub settings: FormSettings,
    #[serde(default)]
    pub pages: Vec<Page>,
}

/// Form-level defaults. An embed block may override the
/// success redirect / message per placement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormSettings {
    #[serde(default)]
    pub submit_label: String,
    #[serde(default)]
    pub success_message: String,
    #[serde(default)]
    pub redirect_url: String,
    /// Comma-separated recipient list for submit notifications.
    #[serde(default)]
    pub notify_emails: String,
    /// Auto-link the bundled `/forms/forms.css`. Turn off to fully replace
    /// the styling with your own stylesheet. Default `true`.
    #[serde(default = "default_true")]
    pub builtin_css: bool,
    /// The multi-page buttons and progress line; empty → the English
    /// default ([`FormSettings::next_text`] and friends). Translatable.
    #[serde(default)]
    pub next_label: String,
    #[serde(default)]
    pub back_label: String,
    /// `{n}` and `{total}` are replaced, e.g. `Step {n} of {total}`.
    #[serde(default)]
    pub progress_label: String,
    /// Shown when the server refuses a submission.
    #[serde(default)]
    pub error_message: String,
}

impl Default for FormSettings {
    fn default() -> Self {
        Self {
            submit_label: String::new(),
            success_message: String::new(),
            redirect_url: String::new(),
            notify_emails: String::new(),
            builtin_css: true,
            next_label: String::new(),
            back_label: String::new(),
            progress_label: String::new(),
            error_message: String::new(),
        }
    }
}

fn or_default<'a>(set: &'a str, default: &'a str) -> &'a str {
    if set.trim().is_empty() {
        default
    } else {
        set
    }
}

impl FormSettings {
    /// The submit button's text.
    #[must_use]
    pub fn submit_text(&self) -> &str {
        or_default(&self.submit_label, "Submit")
    }
    /// The message shown after a successful submission.
    #[must_use]
    pub fn success_text(&self) -> &str {
        or_default(&self.success_message, "Thanks! Your submission was received.")
    }
    /// The "next step" button's text.
    #[must_use]
    pub fn next_text(&self) -> &str {
        or_default(&self.next_label, "Next")
    }
    /// The "previous step" button's text.
    #[must_use]
    pub fn back_text(&self) -> &str {
        or_default(&self.back_label, "Back")
    }
    /// The progress line, with `{n}` / `{total}` placeholders.
    #[must_use]
    pub fn progress_text(&self) -> &str {
        or_default(&self.progress_label, "Step {n} of {total}")
    }
    /// The banner shown when the server refused the submission.
    #[must_use]
    pub fn error_text(&self) -> &str {
        or_default(
            &self.error_message,
            "Your answers could not be sent. Please check the highlighted question and try again.",
        )
    }
}

const fn default_true() -> bool {
    true
}

/// A wizard step.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Page {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub sections: Vec<Section>,
}

/// A labelled grouping of rows within a page.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Section {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub rows: Vec<Row>,
}

/// A 12-column grid row.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Row {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub columns: Vec<Column>,
}

/// A grid cell holding fields. `width` is in 12ths.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    #[serde(default)]
    pub id: String,
    #[serde(default = "default_width")]
    pub width: u8,
    #[serde(default)]
    pub fields: Vec<Field>,
}

impl Default for Column {
    fn default() -> Self {
        Self {
            id: String::new(),
            width: default_width(),
            fields: Vec::new(),
        }
    }
}

const fn default_width() -> u8 {
    12
}

/// One input (or presentational element) in a column.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Field {
    #[serde(default)]
    pub id: String,
    /// Submission key — the `name` attribute and the `data_json` key.
    /// Empty for presentational types (`static` / `richtext`).
    #[serde(default)]
    pub key: String,
    #[serde(rename = "type", default)]
    pub field_type: FieldType,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub placeholder: String,
    #[serde(default)]
    pub default: String,
    /// Choices for select / radio / checkboxes / multiselect.
    #[serde(default)]
    pub options: Vec<Choice>,
    /// Validation rules.
    #[serde(default)]
    pub validation: Vec<ValidationRule>,
    /// Conditional show/hide/require rules.
    #[serde(default)]
    pub rules: Vec<ConditionalRule>,
    // ---- per-type config (omitted from JSON when unset) ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    /// Accepted MIME/extension list for `file`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub accept: String,
    /// Number of points for `rating`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<u8>,
    /// Static/richtext body (presentational types carry their content here).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    // ---- text validation (FB-13) ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u32>,
    /// Client-side HTML5 `pattern` (regex). Enforced in the browser; the
    /// server enforces length + type-format checks.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pattern: String,
}

/// Server-side validation of one submitted value against a field's rules.
/// Returns an error message, or `None` when valid. Empty values
/// pass here — "required" is enforced separately by the submit handler.
#[must_use]
pub fn validate_value(field: &Field, value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    let name = if field.label.is_empty() {
        &field.key
    } else {
        &field.label
    };
    let n = v.chars().count() as u32;
    if let Some(min) = field.min_length {
        if n < min {
            return Some(format!("{name} must be at least {min} characters."));
        }
    }
    if let Some(max) = field.max_length {
        if n > max {
            return Some(format!("{name} must be at most {max} characters."));
        }
    }
    match field.field_type {
        FieldType::Email => {
            if !is_email_like(v) {
                return Some(format!("{name} must be a valid email address."));
            }
        }
        FieldType::Url => {
            if !is_url_like(v) {
                return Some(format!("{name} must be a valid URL."));
            }
        }
        FieldType::Number => match v.parse::<f64>() {
            Ok(num) => {
                if let Some(min) = field.min {
                    if num < min {
                        return Some(format!("{name} must be ≥ {min}."));
                    }
                }
                if let Some(max) = field.max {
                    if num > max {
                        return Some(format!("{name} must be ≤ {max}."));
                    }
                }
            }
            Err(_) => return Some(format!("{name} must be a number.")),
        },
        _ => {}
    }
    None
}

fn is_email_like(v: &str) -> bool {
    match v.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        }
        None => false,
    }
}

fn is_url_like(v: &str) -> bool {
    v.starts_with("http://") || v.starts_with("https://") || v.starts_with('/')
}

// ===================================================================== i18n (FB-17)

/// One translatable text leaf in a form, for the per-locale editor.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TransLeaf {
    /// Stable `cms_snippet_translation.field_path` key (id-based).
    pub path: String,
    /// The field the text belongs to (its label, or its key when it has
    /// none); empty for form-wide texts. The editor groups rows by it.
    pub group: String,
    /// Which text of the group this is: `submit`, `success`, `page`,
    /// `section`, `label`, `help`, `ph`, `content` or `opt`. The editor
    /// maps it to a translated row label.
    pub part: &'static str,
    /// 1-based option number for `opt`; 0 otherwise.
    pub n: usize,
    /// Canonical (default-locale) text.
    pub text: String,
}

/// List every translatable text leaf in document order: settings messages,
/// then page labels / section titles / field label+help+placeholder /
/// choice option labels / presentational content. Keys are id-based so they
/// survive reorder. A single-page form's page label is left out: it is
/// never shown.
#[must_use]
pub fn translatable_leaves(form: &Form) -> Vec<TransLeaf> {
    let mut out = Vec::new();
    let mut push = |path: String, group: &str, part: &'static str, n: usize, text: &str| {
        if !text.is_empty() {
            out.push(TransLeaf {
                path,
                group: group.to_owned(),
                part,
                n,
                text: text.to_owned(),
            });
        }
    };
    // The effective texts, defaults included: an untouched form still has a
    // Submit button and a thanks message to translate.
    let st = &form.settings;
    push("settings.submit_label".into(), "", "submit", 0, st.submit_text());
    push("settings.success_message".into(), "", "success", 0, st.success_text());
    push("settings.error_message".into(), "", "error", 0, st.error_text());
    let multi_page = form.pages.len() > 1;
    if multi_page {
        push("settings.next_label".into(), "", "next", 0, st.next_text());
        push("settings.back_label".into(), "", "back", 0, st.back_text());
        push("settings.progress_label".into(), "", "progress", 0, st.progress_text());
    }
    for page in &form.pages {
        if multi_page {
            push(format!("page.{}.label", page.id), "", "page", 0, &page.label);
        }
        for section in &page.sections {
            push(format!("section.{}.title", section.id), "", "section", 0, &section.title);
            for row in &section.rows {
                for col in &row.columns {
                    for f in &col.fields {
                        let name = if f.label.is_empty() {
                            f.key.as_str()
                        } else {
                            f.label.as_str()
                        };
                        push(format!("{}.label", f.id), name, "label", 0, &f.label);
                        push(format!("{}.help", f.id), name, "help", 0, &f.help);
                        push(format!("{}.ph", f.id), name, "ph", 0, &f.placeholder);
                        push(format!("{}.content", f.id), name, "content", 0, &f.content);
                        for (i, opt) in f.options.iter().enumerate() {
                            push(format!("{}.opt.{i}", f.id), name, "opt", i + 1, &opt.label);
                        }
                    }
                }
            }
        }
    }
    out
}

/// Apply per-locale `overrides` (field_path → text) to a form's text leaves,
/// in place. Missing/empty overrides leave the canonical text. Mirror of the
/// keys produced by [`translatable_leaves`].
pub fn apply_translations(form: &mut Form, overrides: &std::collections::HashMap<String, String>) {
    if overrides.is_empty() {
        return;
    }
    let get = |path: &str| overrides.get(path).filter(|v| !v.is_empty()).cloned();
    if let Some(v) = get("settings.submit_label") {
        form.settings.submit_label = v;
    }
    if let Some(v) = get("settings.success_message") {
        form.settings.success_message = v;
    }
    if let Some(v) = get("settings.error_message") {
        form.settings.error_message = v;
    }
    if let Some(v) = get("settings.next_label") {
        form.settings.next_label = v;
    }
    if let Some(v) = get("settings.back_label") {
        form.settings.back_label = v;
    }
    if let Some(v) = get("settings.progress_label") {
        form.settings.progress_label = v;
    }
    for page in &mut form.pages {
        if let Some(v) = get(&format!("page.{}.label", page.id)) {
            page.label = v;
        }
        for section in &mut page.sections {
            if let Some(v) = get(&format!("section.{}.title", section.id)) {
                section.title = v;
            }
            for row in &mut section.rows {
                for col in &mut row.columns {
                    for f in &mut col.fields {
                        if let Some(v) = get(&format!("{}.label", f.id)) {
                            f.label = v;
                        }
                        if let Some(v) = get(&format!("{}.help", f.id)) {
                            f.help = v;
                        }
                        if let Some(v) = get(&format!("{}.ph", f.id)) {
                            f.placeholder = v;
                        }
                        if let Some(v) = get(&format!("{}.content", f.id)) {
                            f.content = v;
                        }
                        for (i, opt) in f.options.iter_mut().enumerate() {
                            if let Some(v) = get(&format!("{}.opt.{i}", f.id)) {
                                opt.label = v;
                            }
                        }
                    }
                }
            }
        }
    }
}

// ===================================================================== conditional logic (FB-14)

/// Whether `field` is visible given the current `answers` (key → value).
/// Rules apply in order: a `show` rule makes visibility equal to its match;
/// a `hide` rule hides when matched. No rules → always visible. The runtime
/// (`form_runtime`) mirrors this logic in JS; the server uses it so hidden
/// fields are excused from "required".
#[must_use]
pub fn is_visible(field: &Field, answers: &std::collections::HashMap<String, Vec<String>>) -> bool {
    let mut visible = true;
    for rule in &field.rules {
        let matched = eval_conditions(rule, answers);
        match rule.action.as_str() {
            "show" => visible = matched,
            "hide" => {
                if matched {
                    visible = false;
                }
            }
            _ => {}
        }
    }
    visible
}

fn eval_conditions(
    rule: &ConditionalRule,
    answers: &std::collections::HashMap<String, Vec<String>>,
) -> bool {
    if rule.conditions.is_empty() {
        return true;
    }
    let any = rule.match_mode == "any";
    if any {
        rule.conditions.iter().any(|c| eval_condition(c, answers))
    } else {
        rule.conditions.iter().all(|c| eval_condition(c, answers))
    }
}

/// A checkbox group or multi-select has several answers: `eq` / `contains`
/// match when any of them does, `ne` when none equals the value.
fn eval_condition(c: &Condition, answers: &std::collections::HashMap<String, Vec<String>>) -> bool {
    let actual: Vec<&str> = answers
        .get(&c.field)
        .map(|v| v.iter().map(String::as_str).filter(|a| !a.is_empty()).collect())
        .unwrap_or_default();
    match c.op.as_str() {
        "eq" => actual.iter().any(|a| *a == c.value),
        "ne" => !actual.iter().any(|a| *a == c.value),
        "contains" => actual.iter().any(|a| a.contains(c.value.as_str())),
        "empty" => actual.is_empty(),
        "not_empty" => !actual.is_empty(),
        _ => true,
    }
}

/// A choice option for selection field types.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub label: String,
    /// The label in other languages, by locale code (`{"fr": "Bleu lune"}`).
    /// Page-type select options use it; forms translate through
    /// `cms_snippet_translation` instead. Empty → the `label` everywhere.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub labels: std::collections::BTreeMap<String, String>,
}

impl Choice {
    /// The label to show in `locale` (a locale code): its translation,
    /// else the label, else the stored value.
    #[must_use]
    pub fn label_in(&self, locale: Option<&str>) -> &str {
        locale
            .and_then(|code| self.labels.get(code))
            .map(String::as_str)
            .filter(|l| !l.is_empty())
            .or(Some(self.label.as_str()).filter(|l| !l.is_empty()))
            .unwrap_or(&self.value)
    }
}

/// A validation rule (this is the storage shape).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ValidationRule {
    /// e.g. `min_length`, `max_length`, `pattern`, `email`, `min`, `max`.
    pub rule: String,
    #[serde(default)]
    pub param: String,
    #[serde(default)]
    pub message: String,
}

/// A conditional rule (this is the storage shape).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConditionalRule {
    /// `all` (default) or `any` — how to combine `conditions`.
    #[serde(default, rename = "match")]
    pub match_mode: String,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// `show`, `hide`, `require`, `optional`, `set_value`.
    #[serde(default)]
    pub action: String,
    /// Argument for `set_value`.
    #[serde(default)]
    pub value: String,
}

/// One predicate of a [`ConditionalRule`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    /// The key of the field this condition reads.
    #[serde(default)]
    pub field: String,
    /// `eq`, `ne`, `contains`, `gt`, `lt`, `empty`, `not_empty`.
    #[serde(default)]
    pub op: String,
    #[serde(default)]
    pub value: String,
}

/// Every field/input type the builder offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    #[default]
    Text,
    Textarea,
    Email,
    Url,
    Tel,
    Number,
    Date,
    Datetime,
    Time,
    Select,
    Radio,
    /// Single boolean checkbox.
    Checkbox,
    /// Multi-select checkbox group.
    Checkboxes,
    Multiselect,
    File,
    Rating,
    Hidden,
    /// Presentational heading / message (no submitted value).
    Static,
    /// Presentational rich-text message (no submitted value).
    Richtext,
}

impl FieldType {
    /// Stable wire string (matches the serde representation).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FieldType::Text => "text",
            FieldType::Textarea => "textarea",
            FieldType::Email => "email",
            FieldType::Url => "url",
            FieldType::Tel => "tel",
            FieldType::Number => "number",
            FieldType::Date => "date",
            FieldType::Datetime => "datetime",
            FieldType::Time => "time",
            FieldType::Select => "select",
            FieldType::Radio => "radio",
            FieldType::Checkbox => "checkbox",
            FieldType::Checkboxes => "checkboxes",
            FieldType::Multiselect => "multiselect",
            FieldType::File => "file",
            FieldType::Rating => "rating",
            FieldType::Hidden => "hidden",
            FieldType::Static => "static",
            FieldType::Richtext => "richtext",
        }
    }

    /// True when the type submits a value (i.e. needs a `key` and shows up
    /// in submissions). Presentational types return false.
    #[must_use]
    pub fn collects_value(self) -> bool {
        !matches!(self, FieldType::Static | FieldType::Richtext)
    }

    /// True for types whose submitted value is a list (checkbox group /
    /// multi-select), which the submit handler collects into a JSON array.
    #[must_use]
    pub fn is_multi_value(self) -> bool {
        matches!(self, FieldType::Checkboxes | FieldType::Multiselect)
    }

    /// True for types that draw from a fixed `options` list.
    #[must_use]
    pub fn has_choices(self) -> bool {
        matches!(
            self,
            FieldType::Select | FieldType::Radio | FieldType::Checkboxes | FieldType::Multiselect
        )
    }
}

// ===================================================================== parse / serialize

/// Parse a form from a snippet `data` JSON value. Missing keys default,
/// so a partial/empty `{}` yields an empty form rather than an error.
pub fn parse(value: &serde_json::Value) -> Result<Form, serde_json::Error> {
    serde_json::from_value(value.clone())
}

/// Parse from a JSON string (e.g. a hidden builder input).
pub fn parse_str(s: &str) -> Result<Form, serde_json::Error> {
    if s.trim().is_empty() {
        return Ok(Form::default());
    }
    serde_json::from_str(s)
}

// ===================================================================== draft / publish (FB-15)
//
// The snippet `data` top level is the PUBLISHED form (what the public render
// reads — `parse` ignores the extra `_draft` key). The builder edits a DRAFT
// stored under `data._draft`; Publish copies the draft up to the top level.

/// Parse the DRAFT a builder should edit: `data._draft` when present, else
/// the published top level (forms predating draft/publish edit live).
#[must_use]
pub fn parse_draft(data: &serde_json::Value) -> Form {
    if let Some(d) = data.get("_draft") {
        if d.is_object() {
            return parse(d).unwrap_or_default();
        }
    }
    parse(data).unwrap_or_default()
}

/// Produce new `data` that stores `draft` under `_draft` while leaving the
/// published top level untouched. Save-draft path.
#[must_use]
pub fn save_draft_value(current: &serde_json::Value, draft: &Form) -> serde_json::Value {
    let mut out = match current {
        serde_json::Value::Object(_) => current.clone(),
        _ => serde_json::Value::Object(serde_json::Map::new()),
    };
    if let serde_json::Value::Object(map) = &mut out {
        map.insert("_draft".to_owned(), to_value(draft));
    }
    out
}

/// Produce new `data` that publishes `draft`: top level := draft, and
/// `_draft` := the same (so editing continues from the published state).
#[must_use]
pub fn publish_value(draft: &Form) -> serde_json::Value {
    let mut out = to_value(draft);
    if let serde_json::Value::Object(map) = &mut out {
        map.insert("_draft".to_owned(), to_value(draft));
    }
    out
}

/// True when the draft differs from the published top level.
#[must_use]
pub fn has_unpublished_changes(data: &serde_json::Value) -> bool {
    data.get("_draft")
        .is_some_and(|_| parse_draft(data) != parse(data).unwrap_or_default())
}

/// Serialize a form back to a JSON value for storage in `snippet.data`.
///
/// # Panics
/// Never in practice — the schema types are plain data and always serialize.
#[must_use]
pub fn to_value(form: &Form) -> serde_json::Value {
    serde_json::to_value(form).unwrap_or(serde_json::Value::Null)
}

// ===================================================================== traversal

impl Form {
    /// Sanitize the HTML of every rich-text field in place, with the same
    /// policy Markdown and rich-text blocks use. Applied to what is stored
    /// and to what the builder is handed, so no admin surface receives raw
    /// author HTML; the public render sanitizes again on its own.
    pub fn sanitize_rich_text(&mut self) {
        for page in &mut self.pages {
            for section in &mut page.sections {
                for row in &mut section.rows {
                    for col in &mut row.columns {
                        for f in &mut col.fields {
                            if matches!(f.field_type, FieldType::Richtext) {
                                f.content = crate::markdown::sanitize_html(&f.content);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Iterate every field across all pages/sections/rows/columns, in
    /// document order.
    pub fn fields(&self) -> impl Iterator<Item = &Field> {
        self.pages.iter().flat_map(|p| {
            p.sections.iter().flat_map(|s| {
                s.rows
                    .iter()
                    .flat_map(|r| r.columns.iter().flat_map(|c| c.fields.iter()))
            })
        })
    }

    /// Ordered list of submission keys for value-collecting fields —
    /// the column order for the submissions table / CSV.
    #[must_use]
    pub fn value_keys(&self) -> Vec<String> {
        self.fields()
            .filter(|f| f.field_type.collects_value() && !f.key.is_empty())
            .map(|f| f.key.clone())
            .collect()
    }

    /// Find a value-collecting field by its key.
    #[must_use]
    pub fn field_by_key(&self, key: &str) -> Option<&Field> {
        self.fields().find(|f| f.key == key)
    }

    #[must_use]
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    #[must_use]
    pub fn field_count(&self) -> usize {
        self.fields()
            .filter(|f| f.field_type.collects_value())
            .count()
    }
}

// ===================================================================== validation

/// Validate a form for save. Returns a list of human-readable problems
/// (empty = valid). Non-fatal in the builder UI; the save handler may
/// surface these as field errors.
#[must_use]
pub fn validate(form: &Form) -> Vec<String> {
    let mut errors = Vec::new();

    if form.pages.is_empty() {
        errors.push("A form needs at least one page.".to_owned());
    }

    // Column widths.
    for (pi, page) in form.pages.iter().enumerate() {
        for section in &page.sections {
            for row in &section.rows {
                for col in &row.columns {
                    if col.width < 1 || col.width > 12 {
                        errors.push(format!(
                            "Page {} has a column with width {} (must be 1–12).",
                            pi + 1,
                            col.width
                        ));
                    }
                }
            }
        }
    }

    // Field keys: value-collecting fields need a unique, valid key.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for field in form.fields() {
        if !field.field_type.collects_value() {
            continue;
        }
        if field.key.is_empty() {
            errors.push(format!(
                "Field \"{}\" ({}) needs a key.",
                if field.label.is_empty() {
                    "untitled"
                } else {
                    &field.label
                },
                field.field_type.as_str()
            ));
            continue;
        }
        if !is_valid_key(&field.key) {
            errors.push(format!(
                "Field key \"{}\" is invalid (use letters, digits, underscores; start with a letter).",
                field.key
            ));
        }
        if !seen.insert(field.key.as_str()) {
            errors.push(format!("Duplicate field key \"{}\".", field.key));
        }
    }

    errors
}

/// A field key must be a simple identifier: `[A-Za-z][A-Za-z0-9_]*`.
#[must_use]
pub fn is_valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Derive a valid field key from a human label (builder auto-slug helper).
/// Lowercases, replaces runs of non-alphanumerics with `_`, and ensures
/// it starts with a letter. Returns `field` when nothing usable remains.
#[must_use]
pub fn slugify_key(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_us = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    let trimmed = out.trim_matches('_');
    let candidate = if trimmed.is_empty() { "field" } else { trimmed };
    if candidate
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
    {
        candidate.to_owned()
    } else {
        format!("f_{candidate}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Form {
        Form {
            settings: FormSettings {
                submit_label: "Send".into(),
                success_message: "Thanks!".into(),
                ..Default::default()
            },
            pages: vec![Page {
                id: "p1".into(),
                label: "Page 1".into(),
                sections: vec![Section {
                    id: "s1".into(),
                    title: "Contact".into(),
                    rows: vec![Row {
                        id: "r1".into(),
                        columns: vec![
                            Column {
                                id: "c1".into(),
                                width: 6,
                                fields: vec![Field {
                                    id: "f1".into(),
                                    key: "name".into(),
                                    field_type: FieldType::Text,
                                    label: "Name".into(),
                                    required: true,
                                    ..Default::default()
                                }],
                            },
                            Column {
                                id: "c2".into(),
                                width: 6,
                                fields: vec![Field {
                                    id: "f2".into(),
                                    key: "email".into(),
                                    field_type: FieldType::Email,
                                    label: "Email".into(),
                                    required: true,
                                    ..Default::default()
                                }],
                            },
                        ],
                    }],
                }],
            }],
        }
    }

    #[test]
    fn round_trip_preserves_form() {
        let form = sample();
        let value = to_value(&form);
        let back = parse(&value).expect("parse");
        assert_eq!(form, back);
    }

    #[test]
    fn empty_and_partial_json_default() {
        assert_eq!(parse(&json!({})).unwrap(), Form::default());
        assert_eq!(parse_str("").unwrap(), Form::default());
        let partial = parse(&json!({"pages": [{"label": "Only"}]})).unwrap();
        assert_eq!(partial.pages.len(), 1);
        assert_eq!(partial.pages[0].label, "Only");
        // Column width defaults to 12 when omitted.
        let c: Column = serde_json::from_value(json!({})).unwrap();
        assert_eq!(c.width, 12);
    }

    #[test]
    fn field_type_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(FieldType::Multiselect).unwrap(),
            json!("multiselect")
        );
        let f: Field = serde_json::from_value(json!({"key": "x", "type": "checkboxes"})).unwrap();
        assert_eq!(f.field_type, FieldType::Checkboxes);
        assert!(f.field_type.is_multi_value());
        assert!(f.field_type.has_choices());
    }

    #[test]
    fn value_keys_skip_presentational_and_order_is_document() {
        let mut form = sample();
        // Insert a static heading before the inputs — must not appear in keys.
        form.pages[0].sections[0].rows[0].columns[0].fields.insert(
            0,
            Field {
                id: "h".into(),
                field_type: FieldType::Static,
                content: "Hi".into(),
                ..Default::default()
            },
        );
        assert_eq!(form.value_keys(), vec!["name", "email"]);
        assert_eq!(form.field_count(), 2);
        assert_eq!(form.page_count(), 1);
    }

    #[test]
    fn validate_accepts_good_form() {
        assert!(validate(&sample()).is_empty());
    }

    #[test]
    fn validate_flags_duplicate_and_bad_keys() {
        let mut form = sample();
        // duplicate key
        form.pages[0].sections[0].rows[0].columns[1].fields[0].key = "name".into();
        let errs = validate(&form);
        assert!(
            errs.iter().any(|e| e.contains("Duplicate field key")),
            "{errs:?}"
        );

        let mut form2 = sample();
        form2.pages[0].sections[0].rows[0].columns[0].fields[0].key = "1bad".into();
        assert!(validate(&form2).iter().any(|e| e.contains("invalid")));
    }

    #[test]
    fn validate_flags_bad_width_and_no_pages() {
        let mut form = sample();
        form.pages[0].sections[0].rows[0].columns[0].width = 0;
        assert!(validate(&form).iter().any(|e| e.contains("width")));
        assert!(validate(&Form::default())
            .iter()
            .any(|e| e.contains("at least one page")));
    }

    #[test]
    fn presentational_field_without_key_is_ok() {
        let mut form = sample();
        form.pages[0].sections[0].rows[0].columns[0]
            .fields
            .push(Field {
                id: "msg".into(),
                field_type: FieldType::Richtext,
                content: "<p>hi</p>".into(),
                ..Default::default()
            });
        assert!(validate(&form).is_empty());
    }

    #[test]
    fn validate_value_rules() {
        let mut f = Field {
            key: "x".into(),
            field_type: FieldType::Text,
            label: "X".into(),
            ..Default::default()
        };
        f.min_length = Some(3);
        f.max_length = Some(5);
        assert!(validate_value(&f, "").is_none()); // empty passes (required handled elsewhere)
        assert!(validate_value(&f, "ab").unwrap().contains("at least 3"));
        assert!(validate_value(&f, "abcdef").unwrap().contains("at most 5"));
        assert!(validate_value(&f, "abcd").is_none());

        let email = Field {
            key: "e".into(),
            field_type: FieldType::Email,
            label: "E".into(),
            ..Default::default()
        };
        assert!(validate_value(&email, "nope").is_some());
        assert!(validate_value(&email, "a@b.com").is_none());

        let mut num = Field {
            key: "n".into(),
            field_type: FieldType::Number,
            label: "N".into(),
            ..Default::default()
        };
        num.min = Some(1.0);
        num.max = Some(10.0);
        assert!(validate_value(&num, "abc").unwrap().contains("number"));
        assert!(validate_value(&num, "0").unwrap().contains("≥"));
        assert!(validate_value(&num, "11").unwrap().contains("≤"));
        assert!(validate_value(&num, "5").is_none());
    }

    #[test]
    fn draft_publish_round_trip() {
        let published = sample();
        let data = to_value(&published);
        // No _draft yet → draft == published; no unpublished changes.
        assert_eq!(parse_draft(&data), published);
        assert!(!has_unpublished_changes(&data));

        // Save a draft with a changed title — published top level untouched.
        let mut draft = published.clone();
        draft.settings.submit_label = "Changed".into();
        let with_draft = save_draft_value(&data, &draft);
        assert_eq!(parse(&with_draft).unwrap().settings.submit_label, "Send"); // published
        assert_eq!(parse_draft(&with_draft).settings.submit_label, "Changed"); // draft
        assert!(has_unpublished_changes(&with_draft));

        // Publish → top level becomes the draft.
        let published_data = publish_value(&draft);
        assert_eq!(
            parse(&published_data).unwrap().settings.submit_label,
            "Changed"
        );
        assert!(!has_unpublished_changes(&published_data));
    }

    #[test]
    fn choice_label_falls_back_translation_label_value() {
        let mut c = Choice {
            value: "moon-blue".into(),
            label: "Moon blue".into(),
            ..Default::default()
        };
        c.labels.insert("fr".into(), "Bleu lune".into());
        c.labels.insert("de".into(), String::new());
        assert_eq!(c.label_in(Some("fr")), "Bleu lune");
        assert_eq!(c.label_in(Some("de")), "Moon blue"); // empty = untranslated
        assert_eq!(c.label_in(None), "Moon blue");
        c.label.clear();
        assert_eq!(c.label_in(None), "moon-blue");
        // Untranslated options keep the old storage shape.
        let plain = Choice { value: "a".into(), label: "A".into(), ..Default::default() };
        assert_eq!(serde_json::to_value(&plain).unwrap(), serde_json::json!({"value": "a", "label": "A"}));
    }

    #[test]
    fn i18n_leaves_and_apply() {
        use std::collections::HashMap;
        let form = sample();
        let leaves = translatable_leaves(&form);
        // field labels present, keyed by field id
        assert!(leaves
            .iter()
            .any(|l| l.path == "f1.label" && l.text == "Name"));
        assert!(leaves
            .iter()
            .any(|l| l.path == "f2.label" && l.text == "Email"));
        // grouped by field, tagged by part; a one-page form has no page row
        assert!(leaves
            .iter()
            .any(|l| l.path == "f1.label" && l.group == "Name" && l.part == "label"));
        assert_eq!(form.pages.len(), 1);
        assert!(leaves.iter().all(|l| l.part != "page"));
        // The defaults are translatable too; paging texts only on multi-page forms.
        assert!(leaves.iter().any(|l| l.part == "submit"));
        let bare = Form { pages: form.pages.clone(), ..Default::default() };
        assert!(translatable_leaves(&bare).iter().any(|l| l.part == "submit" && l.text == "Submit"));
        assert!(leaves.iter().any(|l| l.part == "error"));
        assert!(leaves.iter().all(|l| l.part != "next"));
        let mut paged = form.clone();
        paged.pages.push(Page { id: "p2".into(), label: "Two".into(), ..Default::default() });
        let paged_leaves = translatable_leaves(&paged);
        assert!(paged_leaves.iter().any(|l| l.path == "settings.progress_label" && l.text == "Step {n} of {total}"));

        let mut t = HashMap::new();
        t.insert("f1.label".to_owned(), "Nom".to_owned());
        t.insert("settings.submit_label".to_owned(), "Envoyer".to_owned());
        let mut fr = form.clone();
        apply_translations(&mut fr, &t);
        assert_eq!(fr.field_by_key("name").unwrap().label, "Nom");
        assert_eq!(fr.field_by_key("email").unwrap().label, "Email"); // untouched
        assert_eq!(fr.settings.submit_label, "Envoyer");
        t.insert("settings.next_label".to_owned(), "Suivant".to_owned());
        apply_translations(&mut fr, &t);
        assert_eq!(fr.settings.next_text(), "Suivant");
    }

    #[test]
    fn conditional_visibility() {
        use std::collections::HashMap;
        let mut field = Field {
            key: "extra".into(),
            field_type: FieldType::Text,
            required: true,
            ..Default::default()
        };
        field.rules = vec![ConditionalRule {
            match_mode: "all".into(),
            conditions: vec![Condition {
                field: "kind".into(),
                op: "eq".into(),
                value: "other".into(),
            }],
            action: "show".into(),
            value: String::new(),
        }];
        let mut a = HashMap::new();
        a.insert("kind".to_owned(), vec!["standard".to_owned()]);
        assert!(!is_visible(&field, &a)); // show-rule unmatched → hidden
        a.insert("kind".to_owned(), vec!["other".to_owned()]);
        assert!(is_visible(&field, &a)); // matched → visible
        // A checkbox group matches when any ticked box does.
        a.insert("kind".to_owned(), vec!["standard".to_owned(), "other".to_owned()]);
        assert!(is_visible(&field, &a));
        field.rules[0].conditions[0].op = "ne".into();
        assert!(!is_visible(&field, &a), "ne: none may equal");
        field.rules[0].conditions[0].op = "eq".into();

        // hide rule
        field.rules[0].action = "hide".into();
        assert!(!is_visible(&field, &a)); // matched hide → hidden
        let empty = HashMap::new();
        assert!(is_visible(&field, &empty)); // unmatched hide → visible
    }

    #[test]
    fn is_valid_key_table() {
        assert!(is_valid_key("name"));
        assert!(is_valid_key("first_name2"));
        assert!(!is_valid_key("2name"));
        assert!(!is_valid_key("first-name"));
        assert!(!is_valid_key(""));
        assert!(!is_valid_key("has space"));
    }

    #[test]
    fn slugify_key_table() {
        assert_eq!(slugify_key("First Name"), "first_name");
        assert_eq!(slugify_key("  Email  "), "email");
        assert_eq!(slugify_key("Café déjà"), "caf_d_j"); // non-ascii dropped
        assert_eq!(slugify_key("123"), "f_123");
        assert_eq!(slugify_key("!!!"), "field");
    }
}
