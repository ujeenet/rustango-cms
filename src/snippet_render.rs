//! `snippet_html(type=…, id=N)` Tera helper — per-snippet template
//! rendering.
//!
//! Pairs with [`crate::library::LibraryTypeHandler::render_template`]:
//! a page template asks for a row of such a type by `(type, id)` and
//! gets it rendered through that template.
//!
//! A type may instead let each element name its own template, by
//! returning `true` from
//! [`allows_element_template`](crate::library::LibraryTypeHandler::allows_element_template);
//! the name then comes from that row's `data.template` and falls back
//! to the type's own. That is what makes an element the whole unit —
//! its content and its markup travel together — and it needs no
//! migration, because `cms_snippet.data` is already free-form JSON.
//!
//! The name resolves against the Tera instance handed to [`fill`],
//! which the public router has already overlaid with the tenant's
//! templates (`router.rs` `tera_for`). So a template written into
//! `templates_tenants/<org>/…` — by the admin editor or the
//! `write_template` MCP tool — is renderable by name here, with no
//! per-render `add_raw_template`.
//!
//! Rendering is lazy and scoped (#731). While the page renders, each
//! `snippet_html(...)` call is recorded and leaves a marker; afterwards
//! [`fill`] fetches only the recorded rows — one query per type — renders
//! them and puts the HTML in place of the markers. A page pays for the
//! snippets it shows, not for every row of every templated type.
//!
//! ## Usage in templates
//!
//! ```text
//! {{ snippet_html(type="Category", id=4) | safe }}
//! ```
//!
//! Unknown type, unknown id, or a type without a template → empty. Use
//! the call's output as it is: the marker is replaced in the finished
//! page, so a filter applied to it (`| escape`, `| truncate`) mangles the
//! marker instead of the snippet.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use tera::{Function, Tera, Value};

/// Composite key for one snippet: `(type name, id)`.
type Key = (String, i64);

/// How deep a snippet's template may itself call `snippet_html`.
const MAX_NESTING: usize = 4;

/// Ceiling on the HTML snippets may add to one page. Fan-out nesting
/// (a snippet that names 50 snippets that each name 50 more) grows
/// geometrically; past this the remaining markers render empty.
const MAX_FILLED_BYTES: usize = 4 * 1024 * 1024;

thread_local! {
    static RECORDING: RefCell<Option<BTreeSet<Key>>> = const { RefCell::new(None) };
}

/// RAII guard: while in scope, `snippet_html(...)` records its key and
/// returns a marker. [`SnippetRecorder::take`] ends the recording.
#[must_use = "the guard stops recording on drop; bind it to a local"]
pub struct SnippetRecorder {
    _priv: (),
}

impl SnippetRecorder {
    /// The keys recorded so far, ending the recording.
    pub fn take(self) -> BTreeSet<Key> {
        RECORDING.with(|cell| cell.borrow_mut().take()).unwrap_or_default()
    }
}

impl Drop for SnippetRecorder {
    fn drop(&mut self) {
        RECORDING.with(|cell| *cell.borrow_mut() = None);
    }
}

/// Start recording `snippet_html(...)` calls on this thread. Bind it
/// right before `tera.render`, with no `.await` in between.
pub fn record() -> SnippetRecorder {
    RECORDING.with(|cell| *cell.borrow_mut() = Some(BTreeSet::new()));
    SnippetRecorder { _priv: () }
}

/// Register the `snippet_html` Tera function. Wired from
/// [`crate::urls::register_tera_helpers`].
pub fn register_tera_function(tera: &mut Tera) {
    tera.register_function("snippet_html", SnippetHtmlFn);
}

/// The placeholder `snippet_html` leaves for [`fill`].
fn marker(type_name: &str, id: i64) -> String {
    format!("<!--rcms-snippet:{type_name}:{id}-->")
}

/// A type name the marker can carry: anything that cannot close the
/// HTML comment or break the `:<id>` suffix parse.
fn valid_type_name(ty: &str) -> bool {
    !ty.is_empty() && !ty.contains("--") && !ty.contains('>') && !ty.contains('\n')
}

fn marker_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<!--rcms-snippet:([^\n>]+?):([0-9]+)-->").expect("marker regex"))
}

/// `is_safe = true`: the output is a marker that [`fill`] replaces with
/// the snippet template's own HTML; autoescaping would mangle both.
struct SnippetHtmlFn;

impl Function for SnippetHtmlFn {
    fn call(&self, args: &HashMap<String, Value>) -> tera::Result<Value> {
        let ty = args.get("type").and_then(Value::as_str).unwrap_or("");
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        if !valid_type_name(ty) || id <= 0 {
            return Ok(Value::String(String::new()));
        }
        let recorded = RECORDING.with(|cell| {
            cell.borrow_mut()
                .as_mut()
                .map(|keys| keys.insert((ty.to_owned(), id)))
                .is_some()
        });
        Ok(Value::String(if recorded { marker(ty, id) } else { String::new() }))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

/// Render the `keys` a page recorded and put each snippet's HTML in
/// place of its marker in `html`. Snippets whose own templates call
/// `snippet_html` are filled the same way, up to [`MAX_NESTING`] levels.
///
/// Best effort: a failed fetch or template render leaves that snippet
/// empty and logs it; the rest of the page still ships.
pub async fn fill(
    pool: &rustango::sql::Pool,
    tera: &Tera,
    locale_id: Option<i64>,
    html: String,
    mut keys: BTreeSet<Key>,
) -> String {
    let mut rendered: HashMap<Key, String> = HashMap::new();
    for _ in 0..MAX_NESTING {
        keys.retain(|k| !rendered.contains_key(k));
        if keys.is_empty() {
            break;
        }
        let rows = fetch_rows(pool, locale_id, &keys).await;
        // Render with a recorder on: a snippet template may call
        // `snippet_html` itself. No await until `take`.
        let recorder = record();
        for key in std::mem::take(&mut keys) {
            let out = rows
                .get(&key)
                .and_then(|(template, value)| render_one(tera, &key, template, value))
                .unwrap_or_default();
            rendered.insert(key, out);
        }
        keys = recorder.take();
    }
    let mut expanded: HashMap<Key, String> = HashMap::new();
    let mut budget = MAX_FILLED_BYTES;
    let mut stack = Vec::new();
    expand_markers(&html, &rendered, &mut expanded, &mut stack, &mut budget)
}

/// Replace every marker in `html` with its snippet's HTML, itself
/// expanded. A snippet that (indirectly) names itself gets an empty
/// slot for the repeat; markers with no rendered snippet (unknown, or
/// nested past [`MAX_NESTING`]) are removed; past `budget` bytes the
/// rest render empty.
fn expand_markers(
    html: &str,
    rendered: &HashMap<Key, String>,
    expanded: &mut HashMap<Key, String>,
    stack: &mut Vec<Key>,
    budget: &mut usize,
) -> String {
    let re = marker_re();
    if !html.contains("<!--rcms-snippet:") {
        return html.to_owned();
    }
    let mut out = String::with_capacity(html.len());
    let mut last = 0;
    for caps in re.captures_iter(html) {
        let whole = caps.get(0).expect("match");
        out.push_str(&html[last..whole.start()]);
        last = whole.end();
        let Ok(id) = caps[2].parse::<i64>() else { continue };
        let key = (caps[1].to_owned(), id);
        if stack.contains(&key) {
            continue;
        }
        let piece = if let Some(done) = expanded.get(&key) {
            done.clone()
        } else if let Some(raw) = rendered.get(&key) {
            stack.push(key.clone());
            let done = expand_markers(raw, rendered, expanded, stack, budget);
            stack.pop();
            expanded.insert(key, done.clone());
            done
        } else {
            String::new()
        };
        if piece.len() > *budget {
            *budget = 0;
            continue;
        }
        *budget -= piece.len();
        out.push_str(&piece);
    }
    out.push_str(&html[last..]);
    out
}

/// Fetch and prepare the recorded rows: `(template name, snippet JSON)`
/// per key, translations applied. Keys of a type with no template, or of
/// a missing row, are absent.
async fn fetch_rows(
    pool: &rustango::sql::Pool,
    locale_id: Option<i64>,
    keys: &BTreeSet<Key>,
) -> HashMap<Key, (String, Value)> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let mut by_type: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
    for (ty, id) in keys {
        by_type.entry(ty.as_str()).or_default().push(*id);
    }
    let mut out = HashMap::new();
    for (type_name, ids) in by_type {
        let Some(handler) = crate::library::find_handler(type_name) else {
            continue;
        };
        let type_default = handler.render_template();
        let per_element = handler.allows_element_template();
        if type_default.is_none() && !per_element {
            continue;
        }
        let rows = match crate::snippet::Snippet::objects()
            .where_(crate::snippet::Snippet::type_name.eq(type_name.to_owned()))
            .where_(crate::snippet::Snippet::id.is_in(ids.clone()))
            .fetch(pool)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::snippet_render",
                    error = %e,
                    type_name = %type_name,
                    "snippet fetch failed for type; leaving its slots empty",
                );
                continue;
            }
        };
        // #409 — per-locale field overrides, one batched fetch.
        let translations = match locale_id {
            Some(lid) => crate::snippet_translation::fetch_for_snippets(pool, &ids, lid)
                .await
                .unwrap_or_default(),
            None => HashMap::new(),
        };
        for row in rows {
            let Some(id) = row.id.get().copied() else {
                continue;
            };
            let mut value = serde_json::to_value(&row).unwrap_or(Value::Null);
            if let Some(overrides) = translations.get(&id) {
                apply_overrides(&mut value, overrides);
            }
            // After the translation overlay: a locale may override
            // `data.template` to swap markup per language.
            let template = per_element
                .then(|| element_template(&value))
                .flatten()
                .or_else(|| type_default.map(str::to_owned));
            // A per-element type whose row names nothing: not an error.
            if let Some(template) = template {
                out.insert((type_name.to_owned(), id), (template, value));
            }
        }
    }
    out
}

fn render_one(tera: &Tera, key: &Key, template: &str, value: &Value) -> Option<String> {
    let mut ctx = tera::Context::new();
    ctx.insert("snippet", value);
    match tera.render(template, &ctx) {
        Ok(html) => Some(html),
        Err(e) => {
            tracing::warn!(
                target: "rustango_cms::snippet_render",
                error = %e,
                type_name = %key.0,
                id = key.1,
                template = %template,
                "snippet template render failed; leaving slot empty",
            );
            None
        }
    }
}

/// The template an individual element names in its `data.template`,
/// for types that opted in via
/// [`crate::library::LibraryTypeHandler::allows_element_template`].
///
/// The name must pass [`crate::tenant_templates::safe_name`], the rule for
/// every editor-supplied template name: relative, `.html`, no
/// traversal. Tera resolves names against its registered set, so a bad
/// name could not load a file anyway; refusing it keeps a broken value
/// from looking like a working one.
fn element_template(snippet_value: &Value) -> Option<String> {
    let name = snippet_value.get("data")?.get("template")?.as_str()?;
    crate::tenant_templates::safe_name(name).ok()
}

/// Apply per-locale translation `overrides` onto a snippet's JSON value.
/// Top-level fields (`title`, `body_markdown`, …) are replaced
/// in place; a `data.<key>` path writes into the snippet's `data`
/// object. Non-object values + missing `data` are created as needed.
fn apply_overrides(value: &mut Value, overrides: &HashMap<String, String>) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    for (path, v) in overrides {
        if let Some(key) = path.strip_prefix("data.") {
            let data = obj
                .entry("data")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(map) = data.as_object_mut() {
                map.insert(key.to_owned(), Value::String(v.clone()));
            }
        } else {
            obj.insert(path.clone(), Value::String(v.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tera::{Context, Tera};

    fn fresh_tera() -> Tera {
        let mut tera = Tera::default();
        register_tera_function(&mut tera);
        tera
    }

    fn render(tera: &Tera, src: &str) -> String {
        let mut t = tera.clone();
        t.add_raw_template("t.html", src).unwrap();
        t.render("t.html", &Context::new()).unwrap()
    }

    fn with_template(name: &str) -> Value {
        serde_json::json!({ "data": { "template": name } })
    }

    #[test]
    fn element_template_reads_a_plain_relative_name() {
        assert_eq!(
            element_template(&with_template("widgets/donations.html")),
            Some("widgets/donations.html".to_owned())
        );
    }

    #[test]
    fn element_template_trims_surrounding_whitespace() {
        assert_eq!(
            element_template(&with_template("  widgets/a.html \n")),
            Some("widgets/a.html".to_owned())
        );
    }

    #[test]
    fn element_template_absent_when_unset_or_wrong_shape() {
        assert_eq!(element_template(&serde_json::json!({})), None);
        assert_eq!(element_template(&serde_json::json!({"data": {}})), None);
        // A non-string `template` is a malformed row, not a name.
        assert_eq!(
            element_template(&serde_json::json!({"data": {"template": 7}})),
            None
        );
    }

    #[test]
    fn element_template_refuses_traversal_and_absolute_names() {
        for bad in [
            "",
            "   ",
            "/etc/passwd",
            "../../secrets.html",
            "widgets/../../../x.html",
            "widgets\\win.html",
            // #661 — what the weaker copy let through.
            ".env",
            "a//b.html",
            "a/./b.html",
            "widgets/donations",
        ] {
            assert_eq!(
                element_template(&with_template(bad)),
                None,
                "should have refused {bad:?}"
            );
        }
    }

    #[test]
    fn element_template_allows_dots_inside_a_segment() {
        // `..` is only a traversal as a whole segment — a file called
        // `a..b.html` is an ordinary name and must not be refused.
        assert_eq!(
            element_template(&with_template("widgets/a..b.html")),
            Some("widgets/a..b.html".to_owned())
        );
    }

    #[test]
    fn apply_overrides_replaces_top_level_and_data_paths() {
        // #409 — title/body_markdown replace in place; `data.<key>`
        // writes into the data object.
        let mut v = serde_json::json!({
            "title": "Hi", "body_markdown": "en body", "data": {"cta": "Click"}
        });
        let mut o = HashMap::new();
        o.insert("title".to_owned(), "Bonjour".to_owned());
        o.insert("body_markdown".to_owned(), "corps fr".to_owned());
        o.insert("data.cta".to_owned(), "Cliquez".to_owned());
        super::apply_overrides(&mut v, &o);
        assert_eq!(v["title"], serde_json::json!("Bonjour"));
        assert_eq!(v["body_markdown"], serde_json::json!("corps fr"));
        assert_eq!(v["data"]["cta"], serde_json::json!("Cliquez"));
    }

    #[test]
    fn apply_overrides_creates_missing_data_object() {
        let mut v = serde_json::json!({ "title": "Hi" });
        let mut o = HashMap::new();
        o.insert("data.x".to_owned(), "y".to_owned());
        super::apply_overrides(&mut v, &o);
        assert_eq!(v["data"]["x"], serde_json::json!("y"));
    }

    #[test]
    fn without_a_recorder_the_call_is_empty() {
        let tera = fresh_tera();
        let out = render(&tera, r#"x{{ snippet_html(type="Category", id=1) }}y"#);
        assert_eq!(out, "xy");
    }

    #[test]
    fn a_recorded_call_leaves_a_marker_and_its_key() {
        let rec = record();
        let tera = fresh_tera();
        let out = render(&tera, r#"a{{ snippet_html(type="Category", id=4) }}b{{ snippet_html(type="Category", id=4) }}"#);
        assert_eq!(out, format!("a{m}b{m}", m = marker("Category", 4)));
        assert_eq!(rec.take(), BTreeSet::from([("Category".to_owned(), 4)]));
    }

    #[test]
    fn bad_arguments_record_nothing() {
        let rec = record();
        let tera = fresh_tera();
        for call in [
            r#"{{ snippet_html(type="Category") }}"#,
            r#"{{ snippet_html(id=4) }}"#,
            r#"{{ snippet_html(type="Category", id=0) }}"#,
            r#"{{ snippet_html(type="x-->y", id=1) }}"#,
            r#"{{ snippet_html(type="a>b", id=1) }}"#,
        ] {
            assert_eq!(render(&tera, call), "", "{call}");
        }
        assert!(rec.take().is_empty());
    }

    #[test]
    fn the_marker_is_not_escaped() {
        let _rec = record();
        let tera = fresh_tera();
        let out = render(&tera, r#"{{ snippet_html(type="X", id=1) }}"#);
        assert!(out.starts_with("<!--"), "got `{out}`");
    }

    #[test]
    fn dropping_the_recorder_stops_recording() {
        {
            let _rec = record();
        }
        let tera = fresh_tera();
        assert_eq!(render(&tera, r#"x{{ snippet_html(type="X", id=1) }}y"#), "xy");
    }
}
