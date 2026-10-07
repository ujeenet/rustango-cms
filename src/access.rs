//! Viewer-aware access helpers for public templates (#members).
//!
//! The built-in menus (`auto_menu()`, curated `menu()`/`_menus`) already
//! hide pages the current viewer can't access. These Tera helpers extend
//! the same protection to **hand-written** links and lists:
//!
//!   * `{{ child | can_view }}` / `can_view(page=child)` — a boolean:
//!     may the current viewer see this page? Guard a raw link with it:
//!     `{% if child | can_view %}<a href="{{ child | page_href }}">…</a>{% endif %}`.
//!   * `{{ children | visible }}` — filter a list of page-shaped objects
//!     (or menu items) down to the ones the viewer may access. Recurses
//!     into a nested `children` key, so nested menus / page trees are
//!     filtered at every level in one call.
//!
//! Both read a render-scoped [`AccessContext`] the pipeline installs (via
//! [`install`]) right before `tera.render` — the same await-free
//! thread-local pattern as `pageurl` / `auto_menu` (Tera 1.x can't pass
//! the context into fn/filter args). With no context installed, or on a
//! tenant with nothing gated, both helpers treat every page as visible —
//! the guard ([`crate::view_restriction_guard`]) is still the actual
//! access control; these helpers are only a rendering convenience.
//!
//! Page-shaped values are matched on their `path` (materialized tree
//! path) + `page_type_id` — present on the `children` / `ancestors`
//! context vars and on any serialized [`crate::page::Page`]. A value
//! lacking `path` is treated as visible (the helper can't decide; the
//! guard still protects the target).

use std::cell::RefCell;
use std::sync::Arc;

use tera::{Filter, Function, Tera, Value};

use crate::view_restriction::AccessContext;

thread_local! {
    /// Per-thread access oracle. Lives only for one `tera.render` call,
    /// installed + cleared via [`AccessGuard`].
    static CURRENT_ACCESS: RefCell<Option<Arc<AccessContext>>> = const { RefCell::new(None) };
}

/// RAII guard. While in scope the access context is installed; on drop
/// it's cleared. Bind it across `tera.render(…)` and let it drop after.
#[must_use = "the guard clears the thread-local on drop; bind it to a local"]
pub struct AccessGuard {
    _priv: (),
}

impl Drop for AccessGuard {
    fn drop(&mut self) {
        CURRENT_ACCESS.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

/// Install `ctx` for the duration of the returned guard so `can_view` /
/// `visible` resolve against it on this thread.
pub fn install(ctx: AccessContext) -> AccessGuard {
    CURRENT_ACCESS.with(|cell| {
        *cell.borrow_mut() = Some(Arc::new(ctx));
    });
    AccessGuard { _priv: () }
}

/// Register `can_view` (function + filter) and `visible` (filter) on
/// `tera`. Wired from [`crate::urls::register_tera_helpers`].
pub fn register_tera_function(tera: &mut Tera) {
    tera.register_function("can_view", CanViewFn);
    tera.register_filter("can_view", CanViewFilter);
    tera.register_filter("visible", VisibleFilter);
}

/// Resolve whether the current viewer may see the page described by a
/// Tera value (a page-shaped object with `path` + `page_type_id`).
///
/// Visible when no context is installed or the tenant gates nothing. A
/// value with no `path` key is not a page (an external menu link) and has
/// nothing to gate. A malformed page fails closed: a non-string
/// `path` is hidden, and a missing `page_type_id` is hidden whenever a
/// page type is gated, since the type check can't be answered.
fn value_visible(value: &Value) -> bool {
    CURRENT_ACCESS.with(|cell| match cell.borrow().as_ref() {
        None => true,
        Some(ctx) => {
            if ctx.is_unrestricted() {
                return true;
            }
            let Some(path) = value.get("path") else {
                return true;
            };
            let Some(path) = path.as_str() else {
                return false;
            };
            match value.get("page_type_id").and_then(Value::as_i64) {
                Some(tid) => ctx.can_view(path, tid),
                None => !ctx.gates_by_type() && ctx.can_view(path, 0),
            }
        }
    })
}

struct CanViewFn;

impl Function for CanViewFn {
    fn call(&self, args: &std::collections::HashMap<String, Value>) -> tera::Result<Value> {
        let visible = args.get("page").map(value_visible).unwrap_or(true);
        Ok(Value::Bool(visible))
    }
}

struct CanViewFilter;

impl Filter for CanViewFilter {
    fn filter(
        &self,
        value: &Value,
        _args: &std::collections::HashMap<String, Value>,
    ) -> tera::Result<Value> {
        Ok(Value::Bool(value_visible(value)))
    }
}

struct VisibleFilter;

impl Filter for VisibleFilter {
    fn filter(
        &self,
        value: &Value,
        _args: &std::collections::HashMap<String, Value>,
    ) -> tera::Result<Value> {
        Ok(filter_list(value))
    }
}

/// Filter an array of page-shaped values to the visible ones, recursing
/// into each item's nested `children` array (for nested menus / trees).
/// Non-arrays pass through unchanged.
fn filter_list(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .filter(|it| value_visible(it))
                .map(filter_children)
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Clone a page/menu item, filtering its nested `children` array (if
/// any) so nested structures are pruned at every level.
fn filter_children(item: &Value) -> Value {
    let mut out = item.clone();
    if let Some(obj) = out.as_object_mut() {
        if let Some(Value::Array(kids)) = obj.get("children") {
            let filtered: Vec<Value> = kids
                .iter()
                .filter(|k| value_visible(k))
                .map(filter_children)
                .collect();
            obj.insert("children".to_owned(), Value::Array(filtered));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view_restriction::AccessContext;
    use serde_json::json;
    use tera::{Context, Tera};

    fn tera() -> Tera {
        let mut t = Tera::default();
        register_tera_function(&mut t);
        t
    }

    fn render(expr: &str, ctx: &Context) -> String {
        let mut t = tera();
        t.add_raw_template("t.html", expr).unwrap();
        t.render("t.html", ctx).unwrap()
    }

    #[test]
    fn can_view_filter_reflects_context() {
        let _g = install(AccessContext::deny_prefix_for_test("0001/0002/"));
        let mut ctx = Context::new();
        ctx.insert(
            "gated",
            &json!({ "path": "0001/0002/", "page_type_id": 5, "title": "M" }),
        );
        ctx.insert(
            "open",
            &json!({ "path": "0001/0003/", "page_type_id": 5, "title": "O" }),
        );
        assert_eq!(render("{{ gated | can_view }}", &ctx), "false");
        assert_eq!(render("{{ open | can_view }}", &ctx), "true");
    }

    /// A malformed page fails closed; a pathless menu link stays.
    #[test]
    fn can_view_fails_closed_on_malformed_pages() {
        let _g = install(AccessContext::deny_type_for_test(5));
        let mut ctx = Context::new();
        ctx.insert("no_type", &json!({ "path": "0001/0003/" }));
        ctx.insert("bad_path", &json!({ "path": 7, "page_type_id": 4 }));
        ctx.insert("link", &json!({ "title": "Docs", "url": "https://x.test" }));
        ctx.insert("open", &json!({ "path": "0001/0003/", "page_type_id": 4 }));
        assert_eq!(render("{{ no_type | can_view }}", &ctx), "false");
        assert_eq!(render("{{ bad_path | can_view }}", &ctx), "false");
        assert_eq!(render("{{ link | can_view }}", &ctx), "true");
        assert_eq!(render("{{ open | can_view }}", &ctx), "true");
    }

    #[test]
    fn can_view_defaults_true_without_context() {
        // No context installed (e.g. an ungated render) → everything visible.
        let mut ctx = Context::new();
        ctx.insert("p", &json!({ "path": "0001/0002/", "page_type_id": 5 }));
        assert_eq!(render("{{ p | can_view }}", &ctx), "true");
    }

    #[test]
    fn visible_filter_drops_denied_and_recurses() {
        let _g = install(AccessContext::deny_prefix_for_test("0001/0002/"));
        let mut ctx = Context::new();
        ctx.insert(
            "items",
            &json!([
                { "title": "Open",  "path": "0001/0003/", "page_type_id": 4,
                  "children": [
                      { "title": "OpenKid",  "path": "0001/0003/0009/", "page_type_id": 4 },
                      { "title": "GatedKid", "path": "0001/0002/0008/", "page_type_id": 5 }
                  ] },
                { "title": "Gated", "path": "0001/0002/", "page_type_id": 5 }
            ]),
        );
        // Top level: "Gated" dropped; nested: "GatedKid" dropped, "OpenKid" kept.
        let out = render(
            "{% for i in items | visible %}{{ i.title }}[{% for c in i.children %}{{ c.title }} {% endfor %}]{% endfor %}",
            &ctx,
        );
        assert_eq!(out, "Open[OpenKid ]");
    }

    #[test]
    fn guard_clears_thread_local() {
        {
            let _g = install(AccessContext::deny_prefix_for_test("0001/"));
            let mut ctx = Context::new();
            ctx.insert("p", &json!({ "path": "0001/0002/", "page_type_id": 5 }));
            assert_eq!(render("{{ p | can_view }}", &ctx), "false");
        }
        // After the guard drops, no context → default visible.
        let mut ctx = Context::new();
        ctx.insert("p", &json!({ "path": "0001/0002/", "page_type_id": 5 }));
        assert_eq!(render("{{ p | can_view }}", &ctx), "true");
    }
}
