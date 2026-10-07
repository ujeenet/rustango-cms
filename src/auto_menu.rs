//! `auto_menu(parent_id=…, depth=…)` Tera function — an automatic
//! menu built from the page tree.
//!
//! The function walks the page tree at render time,
//! filters by `show_in_menus = True` + `status = "published"`, and
//! returns a nested list a host template can iterate to emit nav
//! chrome. Hosts don't have
//! to hand-roll the SQL or maintain a `cms_navigation` row when the
//! menu IS the published tree.
//!
//! ## Usage
//!
//! ```text
//! {% set items = auto_menu(parent_id=1, depth=2) %}
//! <nav>
//!   {% for it in items %}
//!     <a href="{{ it.url_path }}">{{ it.title }}</a>
//!     {% if it.children | length > 0 %}
//!       <ul>
//!         {% for c in it.children %}
//!           <li><a href="{{ c.url_path }}">{{ c.title }}</a></li>
//!         {% endfor %}
//!       </ul>
//!     {% endif %}
//!   {% endfor %}
//! </nav>
//! ```
//!
//! ## Arguments
//!
//! - `parent_id` — id of the page whose immediate children seed the
//!   menu. Omit / `0` / `null` for tree roots (`parent_id IS NULL`).
//! - `depth` — how many levels of children to expand. `1` = direct
//!   children only (each `children` list empty). `2` = include
//!   grandchildren. Default `1`.
//!
//! Each emitted item has `page_id`, `title`, `url_path`, and a
//! `children` list of the same shape.
//!
//! ## Implementation
//!
//! Same thread-local install pattern as [`crate::page_url`] /
//! [`crate::children_filtered`] — the public-render handler
//! pre-fetches every `show_in_menus = true` page the resolver would
//! serve (published, archived, or scheduled and past go-live) in one
//! query, installs a slim projection in a thread-local,
//! the function builds the nested tree from the in-memory list at
//! call time. The set is bounded (typically tens; full sites a few
//! hundred), so one fetch per render amortizes across N
//! `auto_menu()` calls in the same template.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use rustango::core::Column as _;
use rustango::sql::FetcherPool as _;
use serde::{Deserialize, Serialize};
use tera::{Function, Tera, Value};

use crate::page::Page;

/// Default depth when the template omits the kwarg — direct children
/// only.
pub const DEFAULT_DEPTH: i64 = 1;

/// Slim projection of a [`Page`] row carrying just the columns the
/// menu builder needs. Drops everything else (theme, og_*, SEO body,
/// etc.) so the per-render thread-local stays small.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MenuRow {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub title: String,
    pub url_path: String,
    pub sort_order: i32,
    /// Materialized tree path — carried so viewer-aware menu filtering
    /// can resolve inherited (subtree) view restrictions in-memory
    /// (#members). Not used by the item tree itself.
    #[serde(default)]
    pub path: String,
    /// Page-type id — carried so viewer-aware filtering can consult a
    /// per-type view restriction (#members).
    #[serde(default)]
    pub page_type_id: i64,
}

impl MenuRow {
    fn from_page(p: Page) -> Option<Self> {
        let id = p.id.get().copied()?;
        Some(Self {
            id,
            parent_id: p.parent_id,
            title: p.title,
            url_path: p.url_path,
            sort_order: p.sort_order,
            path: p.path,
            page_type_id: p.page_type_id,
        })
    }
}

thread_local! {
    /// Per-thread, per-render menu pool. Holds every published page
    /// flagged `show_in_menus = true`. Cleared by [`AutoMenuGuard`]
    /// when the render returns.
    static CURRENT_MENU_ROWS: RefCell<Option<Arc<Vec<MenuRow>>>>
        = const { RefCell::new(None) };
}

/// RAII guard. While in scope `auto_menu(...)` resolves against the
/// installed rows; on drop the thread-local clears so a follow-up
/// render on the same tokio worker doesn't inherit stale data.
#[must_use = "the guard clears the thread-local on drop; bind it to a local"]
pub struct AutoMenuGuard {
    _priv: (),
}

impl Drop for AutoMenuGuard {
    fn drop(&mut self) {
        CURRENT_MENU_ROWS.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

/// Install the menu row pool for the duration of the returned guard.
pub fn install(rows: Vec<MenuRow>) -> AutoMenuGuard {
    CURRENT_MENU_ROWS.with(|cell| {
        *cell.borrow_mut() = Some(Arc::new(rows));
    });
    AutoMenuGuard { _priv: () }
}

/// Fetch every published page with `show_in_menus = true`, project to
/// [`MenuRow`], and return the pool. Bounded by the tenant's
/// menu-eligible cardinality (typically tens; a large site a few
/// hundred). Failures degrade to an empty pool — the menu just
/// renders empty rather than 500'ing the page.
///
/// Built-in Error pages (404/500 …) are excluded regardless of their
/// `show_in_menus` flag — they're failure substitutes, not navigation
/// destinations, and a root-level error page would otherwise duplicate
/// the host chrome's per-root nav blocks (same rationale as the
/// sitemap exclusion).
///
/// # Errors
/// None — driver / query failures are logged and yield an empty
/// pool.
pub async fn prefetch(pool: &rustango::sql::Pool) -> Vec<MenuRow> {
    let error_tid = crate::error_pages::error_page_type_id(pool).await;
    let now = chrono::Utc::now();
    let rows = Page::objects()
        .where_(Page::show_in_menus.eq(true))
        // Served pages, as the resolver judges them — archived ones too,
        // which explicit menus already keep (#763).
        .where_(Page::status.is_in(crate::resolver::served_statuses()))
        .order_by(&[("sort_order", false), ("id", false)])
        .fetch(pool)
        .await;
    match rows {
        Ok(rows) => rows
            .into_iter()
            .filter(|p| error_tid.map_or(true, |tid| p.page_type_id != tid))
            .filter(|p| crate::resolver::visible_now(p, now))
            .filter_map(MenuRow::from_page)
            .collect(),
        Err(e) => {
            tracing::warn!(
                target: "rustango_cms::auto_menu",
                error = %e,
                "auto_menu prefetch failed; templates will see an empty list",
            );
            Vec::new()
        }
    }
}

/// Cached variant of [`prefetch`]. Serves the menu pool from
/// `cache` under `key`, recomputing + storing on miss via
/// [`rustango::cache_fragment::cached_render`]. The pool is a slim
/// projection of the published, menu-eligible tree; it changes only
/// when a page's `show_in_menus` / title / slug / order changes, so a
/// short `ttl` bounds staleness exactly like the public page cache.
///
/// The fragment is stored as the JSON of `Vec<MenuRow>` (the
/// framework cache is string-valued). Cache backend errors degrade to
/// a live query — `cached_render` swallows them and runs the closure.
///
/// # Errors
/// None — query + cache failures both fall back to a (possibly empty)
/// live pool.
pub async fn prefetch_cached(
    pool: &rustango::sql::Pool,
    cache: &rustango::cache::BoxedCache,
    key: &str,
    ttl: Option<std::time::Duration>,
) -> Vec<MenuRow> {
    let json = rustango::cache_fragment::cached_render(cache.as_ref(), key, ttl, || async {
        serde_json::to_string(&prefetch(pool).await).unwrap_or_else(|_| "[]".to_owned())
    })
    .await;
    serde_json::from_str(&json).unwrap_or_default()
}

/// Register the `auto_menu` Tera function. Wired from
/// [`crate::urls::register_tera_helpers`].
pub fn register_tera_function(tera: &mut Tera) {
    tera.register_function("auto_menu", AutoMenuFn);
}

struct AutoMenuFn;

impl Function for AutoMenuFn {
    fn call(&self, args: &HashMap<String, Value>) -> tera::Result<Value> {
        let pool = CURRENT_MENU_ROWS.with(|cell| cell.borrow().clone());
        let Some(pool) = pool else {
            return Ok(Value::Array(Vec::new()));
        };
        // `parent_id` omitted / 0 / null → root menu (parent_id IS NULL).
        let parent: Option<i64> = args.get("parent_id").and_then(|v| {
            if v.is_null() {
                return None;
            }
            v.as_i64().filter(|n| *n > 0)
        });
        let depth: i64 = args
            .get("depth")
            .and_then(Value::as_i64)
            .filter(|n| *n >= 1)
            .unwrap_or(DEFAULT_DEPTH);

        let by_parent = index_by_parent(&pool);
        let items = build_items(&by_parent, parent, depth);
        serde_json::to_value(&items)
            .map_err(|e| tera::Error::msg(format!("auto_menu: serializing items failed: {e}")))
    }
}

/// JSON-friendly shape templates iterate over.
#[derive(Debug, Serialize)]
pub struct MenuItem {
    pub page_id: i64,
    pub title: String,
    pub url_path: String,
    pub children: Vec<MenuItem>,
}

/// Group rows by `parent_id`. Rows arrive sort_order-then-id sorted
/// (per [`prefetch`]); the index preserves that order inside each
/// bucket, so the emitted menu is stable.
fn index_by_parent(rows: &[MenuRow]) -> HashMap<Option<i64>, Vec<&MenuRow>> {
    crate::tree_build::index_by_parent(rows, |r| r.parent_id)
}

/// Recursively materialize the items under `parent` down to `depth`
/// levels. `depth = 1` returns immediate children with empty
/// `children` arrays.
fn build_items(
    by_parent: &HashMap<Option<i64>, Vec<&MenuRow>>,
    parent: Option<i64>,
    depth: i64,
) -> Vec<MenuItem> {
    crate::tree_build::build(
        by_parent,
        parent,
        depth,
        &|r: &MenuRow| r.id,
        &|r: &MenuRow, children| MenuItem {
            page_id: r.id,
            title: r.title.clone(),
            url_path: r.url_path.clone(),
            children,
        },
    )
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

    fn row(id: i64, parent: Option<i64>, title: &str, slug: &str, sort: i32) -> MenuRow {
        MenuRow {
            id,
            parent_id: parent,
            title: title.to_owned(),
            url_path: format!("/{slug}"),
            sort_order: sort,
            path: String::new(),
            page_type_id: 0,
        }
    }

    /// Tree:
    ///   1 Home (root)
    ///   2 About (root)
    ///     3 Team (child of About)
    ///     4 Careers (child of About)
    ///       5 Engineering (child of Careers)
    fn fixture() -> Vec<MenuRow> {
        vec![
            row(1, None, "Home", "", 0),
            row(2, None, "About", "about", 1),
            row(3, Some(2), "Team", "about/team", 0),
            row(4, Some(2), "Careers", "about/careers", 1),
            row(5, Some(4), "Engineering", "about/careers/eng", 0),
        ]
    }

    #[test]
    fn returns_empty_when_nothing_installed() {
        let tera = fresh_tera();
        let out = render(&tera, "{{ auto_menu() | length }}");
        assert_eq!(out, "0");
    }

    /// `prefetch_cached` stores the menu pool as JSON of
    /// `Vec<MenuRow>` (the framework cache is string-valued). Guard the
    /// round-trip so a field rename / drop that would silently corrupt
    /// cached menus fails loudly here instead of in production.
    #[test]
    fn menu_rows_json_round_trip_is_lossless() {
        let rows = fixture();
        let json = serde_json::to_string(&rows).expect("serialize");
        let back: Vec<MenuRow> = serde_json::from_str(&json).expect("deserialize");
        // MenuRow has no PartialEq — canonical JSON is the equality proxy.
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
        assert_eq!(back.len(), 5);
        // Option<i64> parent_id (both None and Some) must survive.
        assert_eq!(back[0].parent_id, None);
        assert_eq!(back[4].parent_id, Some(4));
        assert_eq!(back[3].title, "Careers");
        assert_eq!(back[2].url_path, "/about/team");
    }

    #[test]
    fn root_menu_returns_top_level_items() {
        let _g = install(fixture());
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{% for it in auto_menu() %}{{ it.title }} {% endfor %}",
        );
        assert_eq!(out, "Home About ");
    }

    #[test]
    fn explicit_parent_id_returns_children_of_that_page() {
        let _g = install(fixture());
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{% for it in auto_menu(parent_id=2) %}{{ it.title }} {% endfor %}",
        );
        assert_eq!(out, "Team Careers ");
    }

    #[test]
    fn depth_one_emits_empty_children_arrays() {
        let _g = install(fixture());
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{% for it in auto_menu(parent_id=2, depth=1) %}\
             {{ it.title }}={{ it.children | length }} {% endfor %}",
        );
        assert_eq!(out, "Team=0 Careers=0 ");
    }

    #[test]
    fn depth_two_expands_one_level_of_nesting() {
        let _g = install(fixture());
        let tera = fresh_tera();
        let src = r#"
{%- for it in auto_menu(parent_id=2, depth=2) -%}
{{ it.title }}:{% for c in it.children %}{{ c.title }},{% endfor %};
{%- endfor -%}
"#;
        let out = render(&tera, src);
        assert_eq!(out, "Team:;Careers:Engineering,;");
    }

    #[test]
    fn depth_three_reaches_grandchildren() {
        let _g = install(fixture());
        let tera = fresh_tera();
        let src = r#"
{%- for it in auto_menu(depth=3) -%}
{{ it.title }}({% for c in it.children %}{{ c.title }}[{% for g in c.children %}{{ g.title }},{% endfor %}],{% endfor %});
{%- endfor -%}
"#;
        let out = render(&tera, src);
        // Home has no children; About → Team[], Careers[Engineering,]
        assert_eq!(out, "Home();About(Team[],Careers[Engineering,],);");
    }

    #[test]
    fn parent_id_zero_treated_as_root() {
        let _g = install(fixture());
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{% for it in auto_menu(parent_id=0) %}{{ it.title }} {% endfor %}",
        );
        assert_eq!(out, "Home About ");
    }

    #[test]
    fn parent_id_unknown_returns_empty() {
        let _g = install(fixture());
        let tera = fresh_tera();
        let out = render(&tera, "{{ auto_menu(parent_id=999) | length }}");
        assert_eq!(out, "0");
    }

    #[test]
    fn items_are_ordered_by_input_order() {
        // Input order is `sort_order, id` per `prefetch`; the in-
        // memory tree builder must preserve that.
        let rows = vec![
            row(10, None, "Z-late", "z", 5),
            row(11, None, "A-early", "a", 0),
            row(12, None, "M-mid", "m", 2),
        ];
        let _g = install(rows);
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{% for it in auto_menu() %}{{ it.title }} {% endfor %}",
        );
        // Items emit in the order they were installed (the prefetch
        // sort is what gives them sort_order semantics).
        assert_eq!(out, "Z-late A-early M-mid ");
    }

    #[test]
    fn json_shape_matches_issue_spec() {
        // The issue specifies the per-item shape: page_id, title,
        // url_path, children. Pin against the literal output.
        let _g = install(vec![row(7, None, "Solo", "solo", 0)]);
        let tera = fresh_tera();
        let out = render(
            &tera,
            "{% set m = auto_menu() %}{{ m[0].page_id }}|{{ m[0].title }}|{{ m[0].url_path }}|{{ m[0].children | length }}",
        );
        assert_eq!(out, "7|Solo|&#x2F;solo|0");
    }

    #[test]
    fn guard_clears_thread_local() {
        {
            let _g = install(fixture());
            let tera = fresh_tera();
            assert_eq!(render(&tera, "{{ auto_menu() | length }}"), "2");
        }
        let tera = fresh_tera();
        assert_eq!(render(&tera, "{{ auto_menu() | length }}"), "0");
    }

    #[test]
    fn negative_or_zero_depth_normalizes_to_default() {
        let _g = install(fixture());
        let tera = fresh_tera();
        // depth = 0 isn't useful (would emit nothing). The function
        // floors to the default so a typo doesn't silently break
        // navigation.
        let out0 = render(
            &tera,
            "{% for it in auto_menu(parent_id=2, depth=0) %}{{ it.title }} {% endfor %}",
        );
        let out1 = render(
            &tera,
            "{% for it in auto_menu(parent_id=2, depth=1) %}{{ it.title }} {% endfor %}",
        );
        assert_eq!(out0, out1);
    }
}
