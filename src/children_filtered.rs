//! `children_filtered(...)` Tera helper — a page's live children,
//! filtered and ordered.
//!
//! Pairs with [`crate::page_type::PageTypeHandler::children_query`]:
//! the handler hook decides what's in `children` (the typed list the
//! template iterates by default); this template-side function lets a
//! template re-filter, reorder, or truncate that same list at the
//! call site — useful when one template wants two slices of the same
//! tree (e.g. "recent" + "highlighted").
//!
//! Operates purely on the in-memory list installed by the render
//! pipeline; no DB round-trip from the function (same constraint as
//! [`crate::page_url`] — Tera 1.x function args don't see context, so
//! we route the data through a thread-local that's set + cleared
//! around `tera.render`).
//!
//! ## Usage in templates
//!
//! ```text
//! {% set posts = children_filtered(status="published", order_by="-published_at", limit=20) %}
//! {% for p in posts %}
//!   <a href="{{ p.url_path }}">{{ p.title }}</a>
//! {% endfor %}
//! ```
//!
//! ## Arguments
//!
//! - `status` — keep only rows where `Page.status` matches the given
//!   string. Omit to skip the filter.
//! - `order_by` — sort key. `"field"` ascending, `"-field"`
//!   descending. Recognized: `sort_order`, `id`, `title`, `slug`,
//!   `depth`, `published_at`, `created_at`, `updated_at`. Unknown
//!   field → unsorted (with a debug-level warning). Omit to keep
//!   the handler's order.
//! - `limit` — truncate to first N rows after sort. `0` or omitted →
//!   no limit.
//! - `parent_id` — optional sanity check; when supplied, the function
//!   verifies it matches the rendered page's id and returns an empty
//!   list otherwise. The list is always the rendered page's children
//!   — handlers that need a different parent should expose a custom
//!   ctx var instead.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::sync::Arc;

use tera::{Function, Tera, Value};

use crate::page::Page;

thread_local! {
    /// Per-thread `(rendered_page_id, children_list)` pair. Installed
    /// for the duration of one `tera.render` call. Holds owned
    /// `Page` rows so the function can re-serialize them without
    /// keeping a reference to render-local state.
    static CURRENT_CHILDREN: RefCell<Option<Arc<ChildrenSnapshot>>>
        = const { RefCell::new(None) };
}

struct ChildrenSnapshot {
    /// The id of the page being rendered. `parent_id=` kwarg validates
    /// against this so a template can't silently filter the wrong
    /// list.
    rendered_page_id: i64,
    children: Vec<Page>,
}

/// RAII guard. While in scope the rendered page's children are
/// readable via `children_filtered(...)`; on drop the thread-local
/// is cleared so a follow-up render on the same tokio worker
/// doesn't inherit stale data.
#[must_use = "the guard clears the thread-local on drop; bind it to a local"]
pub struct ChildrenGuard {
    _priv: (),
}

impl Drop for ChildrenGuard {
    fn drop(&mut self) {
        CURRENT_CHILDREN.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

/// Install the rendered page's children for the duration of the
/// returned guard. `rendered_page_id` is the id `parent_id=` should
/// match — pass the page row's own id.
pub fn install(rendered_page_id: i64, children: Vec<Page>) -> ChildrenGuard {
    CURRENT_CHILDREN.with(|cell| {
        *cell.borrow_mut() = Some(Arc::new(ChildrenSnapshot {
            rendered_page_id,
            children,
        }));
    });
    ChildrenGuard { _priv: () }
}

/// Register the `children_filtered` Tera function. Called once at
/// Tera setup — wired from [`crate::urls::register_tera_helpers`].
pub fn register_tera_function(tera: &mut Tera) {
    tera.register_function("children_filtered", ChildrenFilteredFn);
}

struct ChildrenFilteredFn;

impl Function for ChildrenFilteredFn {
    fn call(&self, args: &std::collections::HashMap<String, Value>) -> tera::Result<Value> {
        let snap = CURRENT_CHILDREN.with(|cell| cell.borrow().clone());
        let Some(snap) = snap else {
            // No render in flight (or the helper is being called from
            // a non-rendered Tera invocation, e.g. macro expansion in
            // an admin form). Return an empty list rather than erroring
            // so the template gracefully renders nothing.
            return Ok(Value::Array(Vec::new()));
        };

        // parent_id sanity check (optional). Mismatch returns empty so
        // templates fail-soft instead of silently filtering the wrong
        // page's children.
        if let Some(pid) = args.get("parent_id").and_then(Value::as_i64) {
            if pid != snap.rendered_page_id {
                return Ok(Value::Array(Vec::new()));
            }
        }

        let mut out: Vec<Page> = snap.children.clone();

        // status filter
        if let Some(want) = args.get("status").and_then(Value::as_str) {
            out.retain(|p| p.status == want);
        }

        // order_by — '-field' desc, 'field' asc. Unknown field leaves
        // the order untouched but emits a debug log so a typo doesn't
        // silently ignore the kwarg.
        if let Some(spec) = args.get("order_by").and_then(Value::as_str) {
            let (desc, field) = match spec.strip_prefix('-') {
                Some(f) => (true, f),
                None => (false, spec),
            };
            if !sort_by_field(&mut out, field, desc) {
                tracing::debug!(
                    target: "rustango_cms::children_filtered",
                    field,
                    "children_filtered: unrecognized order_by field; leaving order unchanged",
                );
            }
        }

        // limit (0 = unlimited; treat negative as unlimited too)
        if let Some(n) = args.get("limit").and_then(Value::as_i64) {
            if n > 0 {
                out.truncate(n as usize);
            }
        }

        serde_json::to_value(&out).map_err(|e| {
            tera::Error::msg(format!(
                "children_filtered: serializing filtered pages failed: {e}"
            ))
        })
    }
}

/// Return `true` if `field` matched a known sort key; `false`
/// means the caller's input didn't match anything and the order
/// was left as-is. Compares via a stable secondary key (`id`) so
/// ties don't reshuffle between renders.
fn sort_by_field(rows: &mut [Page], field: &str, desc: bool) -> bool {
    let cmp: fn(&Page, &Page) -> Ordering = match field {
        "sort_order" => |a, b| a.sort_order.cmp(&b.sort_order).then(cmp_id(a, b)),
        "id" => cmp_id,
        "title" => |a, b| a.title.cmp(&b.title).then(cmp_id(a, b)),
        "slug" => |a, b| a.slug.cmp(&b.slug).then(cmp_id(a, b)),
        "depth" => |a, b| a.depth.cmp(&b.depth).then(cmp_id(a, b)),
        "published_at" => |a, b| a.published_at.cmp(&b.published_at).then(cmp_id(a, b)),
        "last_published_at" => |a, b| {
            a.last_published_at
                .cmp(&b.last_published_at)
                .then(cmp_id(a, b))
        },
        "created_at" => |a, b| {
            a.created_at
                .get()
                .copied()
                .cmp(&b.created_at.get().copied())
                .then(cmp_id(a, b))
        },
        "updated_at" => |a, b| {
            a.updated_at
                .get()
                .copied()
                .cmp(&b.updated_at.get().copied())
                .then(cmp_id(a, b))
        },
        _ => return false,
    };
    if desc {
        rows.sort_by(|a, b| cmp(b, a));
    } else {
        rows.sort_by(cmp);
    }
    true
}

fn cmp_id(a: &Page, b: &Page) -> Ordering {
    a.id.get().copied().cmp(&b.id.get().copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use rustango::sql::Auto;
    use serde_json::json;
    use tera::{Context, Tera};

    fn page(id: i64, slug: &str, status: &str, published_at: Option<i64>) -> Page {
        Page {
            id: Auto::Set(id),
            page_type_id: 1,
            title: format!("Title {id}"),
            slug: slug.to_owned(),
            path: format!("{id:04}/"),
            url_path: format!("/{slug}"),
            preview_path: String::new(),
            template_override: String::new(),
            depth: 1,
            parent_id: Some(99),
            locale_variant_of: None,
            alias_of: None,
            theme_id: None,
            sort_order: id as i32,
            status: status.to_owned(),
            published_at: published_at.map(|t| Utc.timestamp_opt(t, 0).single().unwrap()),
            last_published_at: published_at.map(|t| Utc.timestamp_opt(t, 0).single().unwrap()),
            go_live_at: None,
            expire_at: None,
            seo_title: String::new(),
            seo_description: String::new(),
            robots_index: true,
            sitemap_priority: 0.5,
            show_in_menus: false,
            og_title: String::new(),
            og_description: String::new(),
            og_image_media_id: None,
            twitter_card: String::from("summary_large_image"),
            notification_pre_published_sent: false,
            created_at: Auto::Set(Utc.timestamp_opt(0, 0).single().unwrap()),
            updated_at: Auto::Set(Utc.timestamp_opt(0, 0).single().unwrap()),
        }
    }

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

    #[test]
    fn returns_empty_when_nothing_installed() {
        let tera = fresh_tera();
        let out = render(&tera, r#"{{ children_filtered() | length }}"#);
        assert_eq!(out, "0");
    }

    #[test]
    fn returns_full_list_when_no_filters_applied() {
        let _g = install(
            99,
            vec![page(1, "a", "published", None), page(2, "b", "draft", None)],
        );
        let tera = fresh_tera();
        let out = render(&tera, r#"{{ children_filtered() | length }}"#);
        assert_eq!(out, "2");
    }

    #[test]
    fn filters_by_status() {
        let _g = install(
            99,
            vec![
                page(1, "a", "published", None),
                page(2, "b", "draft", None),
                page(3, "c", "published", None),
            ],
        );
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{% set p = children_filtered(status="published") %}{{ p | length }}|{{ p[0].slug }}"#,
        );
        assert_eq!(out, "2|a");
    }

    #[test]
    fn orders_by_published_at_desc() {
        let _g = install(
            99,
            vec![
                page(1, "a", "published", Some(100)),
                page(2, "b", "published", Some(300)),
                page(3, "c", "published", Some(200)),
            ],
        );
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{% for p in children_filtered(order_by="-published_at") %}{{ p.slug }} {% endfor %}"#,
        );
        assert_eq!(out, "b c a ");
    }

    #[test]
    fn orders_by_title_asc() {
        let _g = install(
            99,
            vec![
                page(3, "c", "published", None),
                page(1, "a", "published", None),
                page(2, "b", "published", None),
            ],
        );
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{% for p in children_filtered(order_by="title") %}{{ p.slug }} {% endfor %}"#,
        );
        // titles "Title 1" < "Title 2" < "Title 3" ascending
        assert_eq!(out, "a b c ");
    }

    #[test]
    fn limit_truncates_after_sort() {
        let _g = install(
            99,
            vec![
                page(1, "a", "published", Some(100)),
                page(2, "b", "published", Some(300)),
                page(3, "c", "published", Some(200)),
            ],
        );
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{% for p in children_filtered(order_by="-published_at", limit=2) %}{{ p.slug }} {% endfor %}"#,
        );
        assert_eq!(out, "b c ");
    }

    #[test]
    fn parent_id_mismatch_returns_empty() {
        let _g = install(99, vec![page(1, "a", "published", None)]);
        let tera = fresh_tera();
        let out = render(&tera, r#"{{ children_filtered(parent_id=42) | length }}"#);
        assert_eq!(out, "0");
    }

    #[test]
    fn parent_id_match_passes() {
        let _g = install(99, vec![page(1, "a", "published", None)]);
        let tera = fresh_tera();
        let out = render(&tera, r#"{{ children_filtered(parent_id=99) | length }}"#);
        assert_eq!(out, "1");
    }

    #[test]
    fn unknown_order_by_leaves_input_order() {
        let _g = install(
            99,
            vec![
                page(3, "c", "published", None),
                page(1, "a", "published", None),
            ],
        );
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{% for p in children_filtered(order_by="bogus_field") %}{{ p.slug }} {% endfor %}"#,
        );
        // Input order preserved (c, a) — unknown field doesn't sort.
        assert_eq!(out, "c a ");
    }

    #[test]
    fn guard_clears_thread_local() {
        {
            let _g = install(99, vec![page(1, "a", "published", None)]);
            let tera = fresh_tera();
            assert_eq!(render(&tera, r#"{{ children_filtered() | length }}"#), "1");
        }
        let tera = fresh_tera();
        assert_eq!(render(&tera, r#"{{ children_filtered() | length }}"#), "0");
    }

    #[test]
    fn serializes_full_page_shape() {
        // Templates iterate the returned objects, so the shape MUST
        // match the canonical Page JSON the public render context
        // exposes for `children`. This pins both surfaces in sync.
        let _g = install(99, vec![page(7, "post", "published", Some(123))]);
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{% set p = children_filtered() %}{{ p[0].id }}-{{ p[0].slug }}-{{ p[0].status }}"#,
        );
        assert_eq!(out, "7-post-published");
        // Pin against the same shape used by other ctx vars.
        let json = serde_json::to_value(&page(7, "post", "published", Some(123))).unwrap();
        assert_eq!(json["id"], json!(7));
        assert_eq!(json["status"], json!("published"));
    }
}
