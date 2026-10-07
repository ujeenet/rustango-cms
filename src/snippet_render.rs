//! `snippet_html(type=…, id=N)` Tera helper — per-snippet template
//! rendering.
//!
//! Pairs with [`crate::library::LibraryTypeHandler::render_template`]:
//! handlers that declare a template have their rows pre-rendered
//! through it at public-render time. Templates fetch the rendered
//! HTML via this function keyed on `(type, id)`.
//!
//! A type may instead let each element name its own template, by
//! returning `true` from
//! [`allows_element_template`](crate::library::LibraryTypeHandler::allows_element_template);
//! the name then comes from that row's `data.template` and falls back
//! to the type's own. That is what makes an element the whole unit —
//! its content and its markup travel together — and it needs no
//! migration, because `cms_snippet.data` is already free-form JSON.
//!
//! The name resolves against the Tera instance handed to [`prefetch`],
//! which the public router has already overlaid with the tenant's
//! templates (`router.rs` `tera_for`). So a template written into
//! `templates_tenants/<org>/…` — by the admin editor or the
//! `write_template` MCP tool — is renderable by name here, with no
//! per-render `add_raw_template`.
//!
//! Same thread-local install pattern as [`crate::page_url`] /
//! [`crate::auto_menu`] / [`crate::children_filtered`] / [`crate::richtext`]:
//! the public-render pipeline pre-fetches + pre-renders into a
//! `{(type, id) → html}` map, installs it in a thread-local for the
//! duration of `tera.render`, and clears on guard drop.
//!
//! ## Usage in templates
//!
//! ```text
//! {{ snippet_html(type="Category", id=4) | safe }}
//! ```
//!
//! Missing entries (unknown type, unknown id, type without a
//! `render_template`) → empty string.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use tera::{Function, Tera, Value};

/// Composite key for the per-snippet HTML map.
type Key = (String, i64);

thread_local! {
    static CURRENT_SNIPPETS_HTML: RefCell<Option<Arc<HashMap<Key, String>>>>
        = const { RefCell::new(None) };
}

/// RAII guard. While in scope `snippet_html(...)` resolves against
/// the installed map; on drop the thread-local clears.
#[must_use = "the guard clears the thread-local on drop; bind it to a local"]
pub struct SnippetHtmlGuard {
    _priv: (),
}

impl Drop for SnippetHtmlGuard {
    fn drop(&mut self) {
        CURRENT_SNIPPETS_HTML.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

/// Install a `(type, id) → html` map for the duration of the
/// returned guard. Bound by the public-render pipeline right before
/// `tera.render`.
pub fn install(map: HashMap<Key, String>) -> SnippetHtmlGuard {
    CURRENT_SNIPPETS_HTML.with(|cell| {
        *cell.borrow_mut() = Some(Arc::new(map));
    });
    SnippetHtmlGuard { _priv: () }
}

/// Register the `snippet_html` Tera function. Wired from
/// [`crate::urls::register_tera_helpers`].
pub fn register_tera_function(tera: &mut Tera) {
    tera.register_function("snippet_html", SnippetHtmlFn);
}

/// `is_safe = true` — output is sanitized HTML produced by the
/// snippet's own template (which itself can apply `| safe` on
/// trusted body fields). Autoescape would mangle the rendered
/// tags into entities otherwise.
struct SnippetHtmlFn;

impl Function for SnippetHtmlFn {
    fn call(&self, args: &HashMap<String, Value>) -> tera::Result<Value> {
        let ty = args.get("type").and_then(Value::as_str).unwrap_or("");
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        if ty.is_empty() || id <= 0 {
            return Ok(Value::String(String::new()));
        }
        let html = CURRENT_SNIPPETS_HTML.with(|cell| {
            cell.borrow()
                .as_ref()
                .and_then(|m| m.get(&(ty.to_owned(), id)).cloned())
                .unwrap_or_default()
        });
        Ok(Value::String(html))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

/// Pre-render every snippet whose handler declares a
/// `render_template`. Returns a `(type, id) → html` map ready to
/// `install` for the render duration.
///
/// Failures during a single template render are best-effort: the
/// row's slot is left empty rather than crashing the whole render
/// (the snippet is missing in the output, the rest of the page
/// still ships).
///
/// # Errors
/// None — degrades to an empty map on snippet-fetch failure (the
/// whole call short-circuits, every `snippet_html(...)` returns
/// empty).
pub async fn prefetch(
    pool: &rustango::sql::Pool,
    tera: &Tera,
    locale_id: Option<i64>,
) -> HashMap<Key, String> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let mut out: HashMap<Key, String> = HashMap::new();
    // Walk every registered library handler that declares a
    // template; for each, fetch every row of that type and render
    // it through the template.
    for handler in crate::library::registered_handlers() {
        let type_default = handler.render_template();
        let per_element = handler.allows_element_template();
        // Nothing to render: no type-wide template, and rows of this
        // type aren't allowed to name one either.
        if type_default.is_none() && !per_element {
            continue;
        }
        let type_name = handler.type_name();
        let rows = match crate::snippet::Snippet::objects()
            .where_(crate::snippet::Snippet::type_name.eq(type_name.to_owned()))
            .fetch(pool)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::snippet_render",
                    error = %e,
                    type_name = %type_name,
                    "snippet prefetch failed for type; skipping",
                );
                continue;
            }
        };
        // #409 — per-locale field overrides for this type's rows (one
        // batched fetch). Empty on the default locale.
        let translations = match locale_id {
            Some(lid) => {
                let ids: Vec<i64> = rows.iter().filter_map(|r| r.id.get().copied()).collect();
                crate::snippet_translation::fetch_for_snippets(pool, &ids, lid)
                    .await
                    .unwrap_or_default()
            }
            None => HashMap::new(),
        };
        for row in rows {
            let Some(id) = row.id.get().copied() else {
                continue;
            };
            let mut ctx = tera::Context::new();
            let mut snippet_value = serde_json::to_value(&row).unwrap_or(serde_json::Value::Null);
            if let Some(overrides) = translations.get(&id) {
                apply_overrides(&mut snippet_value, overrides);
            }
            // Resolve after the translation overlay: a locale may
            // override `data.template` to swap markup per language.
            let template = match per_element
                .then(|| element_template(&snippet_value))
                .flatten()
            {
                Some(name) => name,
                None => match type_default {
                    Some(t) => t.to_owned(),
                    // Per-element type, and this row names nothing.
                    // Not an error: rows opt in one at a time.
                    None => continue,
                },
            };
            ctx.insert("snippet", &snippet_value);
            match tera.render(&template, &ctx) {
                Ok(html) => {
                    out.insert((type_name.to_owned(), id), html);
                }
                Err(e) => {
                    tracing::warn!(
                        target: "rustango_cms::snippet_render",
                        error = %e,
                        type_name = %type_name,
                        id,
                        template = %template,
                        "snippet template render failed; leaving slot empty",
                    );
                }
            }
        }
    }
    out
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
    fn returns_empty_when_nothing_installed() {
        let tera = fresh_tera();
        let out = render(&tera, r#"x{{ snippet_html(type="Category", id=1) }}y"#);
        assert_eq!(out, "xy");
    }

    #[test]
    fn resolves_known_type_and_id() {
        let mut m = HashMap::new();
        m.insert(
            ("Category".to_owned(), 4i64),
            "<span>rust</span>".to_owned(),
        );
        let _g = install(m);
        let tera = fresh_tera();
        let out = render(&tera, r#"{{ snippet_html(type="Category", id=4) }}"#);
        assert_eq!(out, "<span>rust</span>");
    }

    #[test]
    fn unknown_type_returns_empty() {
        let mut m = HashMap::new();
        m.insert(
            ("Category".to_owned(), 4i64),
            "<span>rust</span>".to_owned(),
        );
        let _g = install(m);
        let tera = fresh_tera();
        let out = render(&tera, r#"x{{ snippet_html(type="Author", id=4) }}y"#);
        assert_eq!(out, "xy");
    }

    #[test]
    fn unknown_id_returns_empty() {
        let mut m = HashMap::new();
        m.insert(
            ("Category".to_owned(), 4i64),
            "<span>rust</span>".to_owned(),
        );
        let _g = install(m);
        let tera = fresh_tera();
        let out = render(&tera, r#"x{{ snippet_html(type="Category", id=99) }}y"#);
        assert_eq!(out, "xy");
    }

    #[test]
    fn missing_args_return_empty() {
        let _g = install(HashMap::new());
        let tera = fresh_tera();
        let out = render(&tera, r#"x{{ snippet_html(type="Category") }}y"#);
        assert_eq!(out, "xy");
        let out2 = render(&tera, r#"x{{ snippet_html(id=4) }}y"#);
        assert_eq!(out2, "xy");
    }

    #[test]
    fn output_is_marked_safe() {
        // Without is_safe, `<span>` would land as `&lt;span&gt;`.
        let mut m = HashMap::new();
        m.insert(("X".to_owned(), 1i64), "<b>ok</b>".to_owned());
        let _g = install(m);
        let tera = fresh_tera();
        let out = render(&tera, r#"{{ snippet_html(type="X", id=1) }}"#);
        assert!(out.contains("<b>ok</b>"), "got `{out}`");
    }

    #[test]
    fn guard_clears_thread_local() {
        {
            let mut m = HashMap::new();
            m.insert(("X".to_owned(), 1i64), "y".to_owned());
            let _g = install(m);
            let tera = fresh_tera();
            assert_eq!(render(&tera, r#"{{ snippet_html(type="X", id=1) }}"#), "y");
        }
        let tera = fresh_tera();
        assert_eq!(
            render(&tera, r#"x{{ snippet_html(type="X", id=1) }}y"#),
            "xy"
        );
    }
}
