//! `Navigation` — tenant-managed named menus with nested items (#22).
//!
//! Two tables:
//! - `cms_menu` — one row per editor-curated menu (`main_nav`,
//!   `footer`, etc.). Per-tenant scoped via the tenancy machinery.
//! - `cms_menu_item` — tree-shaped nodes inside a menu. Each item
//!   either links to a page (`page_id` FK) OR carries a custom URL
//!   (`external_url`) — the form enforces exactly one.
//!
//! The `menu(slug="...")` Tera helper exposes the nested tree to
//! templates; themes render their own markup, so the helper carries
//! no opinions about classes / icons / hover state.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// Named menu container — slug is the stable handle templates pass
/// to the `menu(slug="...")` Tera helper. Unique within a tenant.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_menu",
    app = "cms",
    display = "name",
    admin(list_display = "slug, name, created_at", ordering = "name",)
)]
pub struct Menu {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Stable handle (kebab-case). Templates pass this to the
    /// `menu()` Tera helper.
    #[rustango(max_length = 64, unique)]
    pub slug: String,

    /// Display name shown in the admin list.
    #[rustango(max_length = 100)]
    pub name: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// One node inside a menu. Tree-shaped via `parent_id` self-FK. The
/// link target is exactly one of `page_id` (internal) or
/// `external_url` (custom URL); the admin form enforces this, and
/// the public render resolver picks the live URL from the chosen
/// side at request time so slug renames stay reflected.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_menu_item",
    app = "cms",
    display = "label",
    admin(
        list_display = "menu_id, label, parent_id, sort_order",
        ordering = "menu_id, parent_id, sort_order",
    )
)]
pub struct MenuItem {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_menu", on = "id", index)]
    pub menu_id: i64,

    /// Tree parent — None for top-level items.
    #[rustango(fk = "cms_menu_item", on = "id", index)]
    pub parent_id: Option<i64>,

    /// Sibling order — ascending.
    pub sort_order: i32,

    /// Link text rendered to readers. May be empty when linked to a
    /// page; templates fall back to the page title in that case.
    #[rustango(max_length = 200)]
    pub label: String,

    /// Internal page link. Mutually exclusive with `external_url`.
    /// Nulled (rather than cascade-deleted) when the page is
    /// deleted, so the menu item stays visible for the editor to
    /// re-target.
    #[rustango(fk = "cms_page", on = "id")]
    pub page_id: Option<i64>,

    /// Custom external URL. Mutually exclusive with `page_id`.
    #[rustango(max_length = 500)]
    pub external_url: Option<String>,

    /// Open the link in a new tab (target="_blank" rel="noopener").
    pub open_in_new_tab: bool,
}

// ----------------------------------------------------------------
// Resolution — one core, four thin wrappers.
//
// `{% set nav = menu(slug="main") %}` in a theme, `GET /api/v2/menus/`
// for a headless client, and the public render's whole-menu injection
// all land in [`resolve_menus_with`]. They previously had three
// separate implementations, of which `resolve_all_menus` was `1 + 3N`
// queries — it resolved every menu's full tree and then discarded
// everything but the slug and the name.
//
// The core is a **fixed handful of queries whatever the menu size**:
// menus, items, target pages, then at most two batched translation
// lookups. Nothing in here may query per item; that is the regression
// this shape exists to prevent.
// ----------------------------------------------------------------

/// One node in the resolved menu tree. Themes iterate this shape.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedMenuItem {
    /// `cms_menu_item.id` — lets a headless client correlate a rendered
    /// node with the row an editor edits.
    pub id: Option<i64>,
    pub label: String,
    /// Where the item points. `null` for a grouping header — an item with
    /// neither a page nor an external URL, which is a label, not a link.
    pub url: Option<String>,
    /// `true` when this item targets a CMS page (vs. an external URL).
    pub is_page: bool,
    pub page_id: Option<i64>,
    pub open_in_new_tab: bool,
    /// This item targets the page currently being viewed.
    pub is_active: bool,
    /// This item lies on the path to the current page — it targets an
    /// ancestor of it, or it parents a subtree containing the active
    /// item. The second case is what highlights a top-level section
    /// whose own link is a grouping label or an external URL.
    pub in_active_trail: bool,
    pub children: Vec<ResolvedMenuItem>,
}

/// Resolved menu shape returned by `menu(slug=...)`.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedMenu {
    pub slug: String,
    pub name: String,
    pub items: Vec<ResolvedMenuItem>,
}

/// Everything the resolver needs beyond the menu itself.
///
/// A struct rather than three positional `Option`s because every one of
/// them is optional and independently useful, and three bare `None`s at
/// a call site say nothing about which is which.
#[derive(Debug, Clone, Copy, Default)]
pub struct MenuOptions<'a> {
    /// #members — hide items whose target page this viewer may not see.
    /// `None` is an anonymous viewer, which is the safe default: it
    /// hides everything gated rather than revealing it.
    pub viewer: Option<&'a rustango::tenancy::auth::User>,
    /// Localize labels. `None` or the default locale returns canonical
    /// content without issuing the translation queries at all.
    pub locale: Option<&'a crate::locale::Locale>,
    /// The page being rendered, for `is_active` / `in_active_trail`.
    /// Passed as the row rather than an id because the trail test is a
    /// `path` prefix comparison, and the caller already holds it.
    pub current: Option<&'a crate::page::Page>,
}

/// Resolve a menu by slug. Returns `None` when no menu with that
/// slug exists in the tenant.
///
/// # Errors
/// Driver / query failures from any of the lookups.
pub async fn resolve_menu(
    pool: &rustango::sql::Pool,
    slug: &str,
) -> Result<Option<ResolvedMenu>, rustango::sql::ExecError> {
    resolve_menu_with(pool, slug, MenuOptions::default()).await
}

/// Like [`resolve_menu`] but hides items whose target page the
/// `viewer` can't access (#members) — login / groups / permission
/// restrictions, page/subtree or per-type. Anonymous viewers see only
/// public + password-gated items. An item pointing at a denied page is
/// dropped (with any submenu it parents), exactly like a draft target.
///
/// # Errors
/// Driver / query failures from any of the lookups.
pub async fn resolve_menu_for_viewer(
    pool: &rustango::sql::Pool,
    slug: &str,
    viewer: Option<&rustango::tenancy::auth::User>,
) -> Result<Option<ResolvedMenu>, rustango::sql::ExecError> {
    resolve_menu_with(
        pool,
        slug,
        MenuOptions {
            viewer,
            ..Default::default()
        },
    )
    .await
}

/// Resolve one menu with the full option set.
///
/// # Errors
/// Driver / query failures from any of the lookups.
pub async fn resolve_menu_with(
    pool: &rustango::sql::Pool,
    slug: &str,
    opts: MenuOptions<'_>,
) -> Result<Option<ResolvedMenu>, rustango::sql::ExecError> {
    let mut all = resolve_menus_with(pool, Some(&[slug.to_owned()]), opts).await?;
    Ok(all.remove(slug))
}

/// Resolve every menu in the tenant into a `slug → ResolvedMenu`
/// map, with no viewer, locale or current page.
///
/// # Errors
/// Driver / query failures from any underlying fetch.
pub async fn resolve_all_menus(
    pool: &rustango::sql::Pool,
) -> Result<std::collections::HashMap<String, ResolvedMenu>, rustango::sql::ExecError> {
    resolve_menus_with(pool, None, MenuOptions::default()).await
}

/// The resolver. `slugs = None` resolves every menu in the tenant;
/// `Some(&[…])` restricts to those slugs.
///
/// Query budget, and it does not grow with the number of menus or
/// items: menus · items · target pages · (viewer restrictions) ·
/// (item translations) · (page-title translations). The last two are
/// skipped entirely for the default locale.
///
/// # Errors
/// Driver / query failures from any of the lookups.
pub async fn resolve_menus_with(
    pool: &rustango::sql::Pool,
    slugs: Option<&[String]>,
    opts: MenuOptions<'_>,
) -> Result<std::collections::HashMap<String, ResolvedMenu>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let mut menu_q = Menu::objects();
    if let Some(list) = slugs {
        if list.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        menu_q = menu_q.where_(Menu::slug.is_in(list.iter().cloned()));
    }
    let menus: Vec<Menu> = menu_q.fetch(pool).await?;
    if menus.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let menu_ids: Vec<i64> = menus.iter().filter_map(|m| m.id.get().copied()).collect();

    // Every menu's items in one query, then split by `menu_id` in
    // memory — the whole reason `resolve_all_menus` is no longer N+1.
    let items: Vec<MenuItem> = MenuItem::objects()
        .where_(MenuItem::menu_id.is_in(menu_ids.iter().copied()))
        .order_by(&[("sort_order", false), ("id", false)])
        .fetch(pool)
        .await?;

    // One bulk page lookup for every internal-link item — avoids N+1.
    // #585 — only **public** targets resolve; an item pointing at a
    // draft, scheduled, expired or deleted page is dropped below rather
    // than rendered as a dead link.
    let referenced_page_ids: std::collections::BTreeSet<i64> =
        items.iter().filter_map(|i| i.page_id).collect();
    let pages: Vec<crate::page::Page> = if referenced_page_ids.is_empty() {
        Vec::new()
    } else {
        crate::page::Page::objects()
            .where_(crate::page::Page::id.is_in(referenced_page_ids.iter().copied()))
            // `published` **and** `archived` — `PageStatus::is_public`.
            // Filtering to published alone meant a menu silently lost an
            // item whose target had been archived, while the tree, the
            // sitemap and the renderer all still served that page. A SPA
            // drawing nav from menus and content from the tree then
            // disagreed with itself.
            .where_(crate::page::Page::status.is_in(crate::page::PageStatus::public_strings()))
            .fetch(pool)
            .await?
    };
    let pages_by_id: std::collections::HashMap<i64, &crate::page::Page> = pages
        .iter()
        .filter_map(|p| p.id.get().copied().map(|id| (id, p)))
        .collect();

    // #members — resolve which referenced pages this viewer may not see
    // (login / groups / permission, page/subtree or per-type). Denied
    // targets are dropped alongside draft ones below.
    let page_triples: Vec<(i64, String, i64)> = pages
        .iter()
        .filter_map(|p| {
            p.id.get()
                .copied()
                .map(|id| (id, p.path.clone(), p.page_type_id))
        })
        .collect();
    let denied = crate::view_restriction::denied_page_ids(pool, opts.viewer, &page_triples).await;

    // Two translation sources, both batched, both skipped on the default
    // locale — canonical content already *is* the default-locale content.
    //
    // 1. authored labels (and per-locale external URLs) on the item;
    // 2. the page-title fallback an item with an empty label uses, which
    //    lives in `cms_translation` where the renderer already keeps it.
    let (item_tr, page_tr) = match opts.locale.filter(|l| !l.is_default) {
        Some(loc) => {
            let lid = loc.id.get().copied().unwrap_or_default();
            let item_ids: Vec<i64> = items.iter().filter_map(|i| i.id.get().copied()).collect();
            let page_ids: Vec<i64> = referenced_page_ids.iter().copied().collect();
            (
                crate::menu_item_translation::fetch_for_items(pool, &item_ids, lid)
                    .await
                    .unwrap_or_default(),
                crate::translation::fetch_for_pages(pool, &page_ids, lid)
                    .await
                    .unwrap_or_default(),
            )
        }
        None => Default::default(),
    };

    let ctx = ResolveCtx {
        pages_by_id: &pages_by_id,
        denied: &denied,
        item_tr: &item_tr,
        page_tr: &page_tr,
        current: opts.current,
    };

    let mut out = std::collections::HashMap::new();
    for menu in menus {
        let menu_id = menu.id.get().copied().unwrap_or_default();
        let mut children_of: std::collections::HashMap<Option<i64>, Vec<&MenuItem>> =
            std::collections::HashMap::new();
        for item in items.iter().filter(|i| i.menu_id == menu_id) {
            children_of.entry(item.parent_id).or_default().push(item);
        }
        let tree = build(None, &children_of, &ctx);

        // A `parent_id` cycle, or a parent that lives in a different
        // menu, makes an item unreachable from the root bucket — so it
        // renders nowhere, with no error, while the admin's item count
        // still includes it. Say so: silence here means an editor sees
        // "5 items" and a navbar with 2, and nothing explains the gap.
        //
        // Items legitimately dropped for access or publication reasons
        // are subtracted first, so this only fires on structural damage.
        let rendered = count_nodes(&tree);
        let droppable = items
            .iter()
            .filter(|i| i.menu_id == menu_id)
            .filter(|i| {
                i.page_id
                    .is_some_and(|pid| denied.contains(&pid) || !pages_by_id.contains_key(&pid))
            })
            .count();
        let expected = items.iter().filter(|i| i.menu_id == menu_id).count() - droppable;
        if rendered < expected {
            tracing::warn!(
                target: "rustango_cms::navigation",
                menu = %menu.slug,
                rendered,
                expected,
                "menu items unreachable from the root — a parent_id cycle, \
                 or a parent belonging to another menu",
            );
        }

        out.insert(
            menu.slug.clone(),
            ResolvedMenu {
                slug: menu.slug,
                name: menu.name,
                items: tree,
            },
        );
    }
    Ok(out)
}

/// Total nodes in a resolved tree, used to detect items the walk could
/// not reach.
fn count_nodes(items: &[ResolvedMenuItem]) -> usize {
    items.len() + items.iter().map(|i| count_nodes(&i.children)).sum::<usize>()
}

/// The read-only lookups `build` needs, bundled so the recursion
/// carries one reference instead of five.
struct ResolveCtx<'a> {
    pages_by_id: &'a std::collections::HashMap<i64, &'a crate::page::Page>,
    denied: &'a std::collections::HashSet<i64>,
    item_tr: &'a std::collections::HashMap<i64, std::collections::HashMap<String, String>>,
    page_tr: &'a std::collections::HashMap<i64, std::collections::HashMap<String, String>>,
    current: Option<&'a crate::page::Page>,
}

impl ResolveCtx<'_> {
    /// A localized field on an item, if an editor set one for this locale.
    fn item_override(&self, item_id: Option<i64>, field: &str) -> Option<String> {
        self.item_tr
            .get(&item_id?)?
            .get(field)
            .filter(|s| !s.is_empty())
            .cloned()
    }

    /// A target page's localized title, falling back to its canonical one.
    fn page_title(&self, page_id: i64, page: &crate::page::Page) -> String {
        self.page_tr
            .get(&page_id)
            .and_then(|m| m.get("title"))
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| page.title.clone())
    }

    /// Does this item's target page sit above the page being viewed?
    /// A `path` prefix test, so it costs no query — the materialized
    /// path already encodes ancestry.
    fn is_ancestor_of_current(&self, target: &crate::page::Page) -> bool {
        let Some(cur) = self.current else {
            return false;
        };
        cur.path.starts_with(&target.path) && cur.path != target.path
    }

    /// Is `page_id` the page currently being viewed?
    ///
    /// An alias counts as its source: the two are different rows with
    /// different ids, so a nav item pointing at the canonical page would
    /// otherwise never highlight while a reader is on the alias's URL.
    fn is_current_page(&self, page_id: i64) -> bool {
        let Some(cur) = self.current else {
            return false;
        };
        cur.id.get().copied() == Some(page_id) || cur.alias_of == Some(page_id)
    }

    /// Does this custom-link URL point at the page being viewed?
    ///
    /// `is_active` used to be page-id equality set only in the page
    /// branch, so an editor who added "Blog" as a custom link to `/blog`
    /// rather than a page link lost highlighting entirely — and the
    /// client had to re-derive it from URLs, which is exactly what this
    /// endpoint exists to avoid. Compared slash-insensitively, since a
    /// hand-typed URL may or may not carry a trailing one.
    fn external_matches_current(&self, url: &str) -> bool {
        let Some(cur) = self.current else {
            return false;
        };
        // Only same-origin paths can name a page here; an absolute URL to
        // another host never matches.
        if !url.starts_with('/') {
            return false;
        }
        let trim = |s: &str| s.trim_end_matches('/').to_owned();
        !cur.url_path.is_empty() && trim(url) == trim(&cur.url_path)
    }
}

fn build(
    parent: Option<i64>,
    children_of: &std::collections::HashMap<Option<i64>, Vec<&MenuItem>>,
    ctx: &ResolveCtx<'_>,
) -> Vec<ResolvedMenuItem> {
    children_of
        .get(&parent)
        .map(|group| {
            group
                .iter()
                .filter_map(|it| {
                    let id = it.id.get().copied();
                    let mut is_active = false;
                    let mut in_active_trail = false;
                    let (url, resolved_label, is_page) = if let Some(pid) = it.page_id {
                        // #members — hide items the viewer can't access.
                        if ctx.denied.contains(&pid) {
                            return None;
                        }
                        // #585 — internal link resolves only if the target
                        // page is published; otherwise drop the item (and,
                        // with it, any submenu it parents). No dead links.
                        let p = ctx.pages_by_id.get(&pid)?;
                        let url = if p.url_path.is_empty() {
                            "/".to_owned()
                        } else {
                            p.url_path.clone()
                        };
                        // An authored label wins, then its translation;
                        // only an *empty* label falls through to the page
                        // title — and that title is localized too, which
                        // it never used to be.
                        let label = match ctx.item_override(id, "label") {
                            Some(t) => t,
                            None if !it.label.trim().is_empty() => it.label.clone(),
                            None => ctx.page_title(pid, p),
                        };
                        is_active = ctx.is_current_page(pid);
                        in_active_trail = ctx.is_ancestor_of_current(p);
                        (url, label, true)
                    } else if let Some(ext) = it.external_url.clone() {
                        let url = ctx.item_override(id, "external_url").unwrap_or(ext);
                        is_active = ctx.external_matches_current(&url);
                        (
                            url,
                            ctx.item_override(id, "label").unwrap_or_else(|| it.label.clone()),
                            false,
                        )
                    } else {
                        // Neither target: a grouping header. It used to
                        // render `url: "/"`, which a client cannot
                        // distinguish from a deliberate link to the
                        // homepage — so every dropdown label became a
                        // link. `save-tree` now rejects creating these,
                        // but rows already in the database still render.
                        (
                            String::new(),
                            ctx.item_override(id, "label").unwrap_or_else(|| it.label.clone()),
                            false,
                        )
                    };
                    let children = build(id, children_of, ctx);
                    // A grouping item — a label with no link, or an
                    // external one — is on the trail when something under
                    // it is. Without this, nothing highlights the open
                    // section in the common "dropdown header" nav.
                    in_active_trail |= children.iter().any(|c| c.is_active || c.in_active_trail);
                    Some(ResolvedMenuItem {
                        id,
                        label: resolved_label,
                        url: (!url.is_empty()).then_some(url),
                        is_page,
                        page_id: it.page_id,
                        open_in_new_tab: it.open_in_new_tab,
                        is_active,
                        in_active_trail,
                        children,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Order menu items so a child is always deleted before its parent.
///
/// `cms_menu_item.parent_id` is a self-FK with no `ON DELETE`, so removing
/// a parent while a child still points at it trips the constraint. Sorting
/// on `parent_id.is_none()` is not enough: it puts every non-root in one
/// bucket, so a grandchild and its parent tie and their relative order
/// comes from `HashMap` iteration — the delete then fails, or doesn't,
/// depending on hashing.
///
/// Peels leaves off the set: on each pass, emit every item that nothing
/// *remaining* claims as a parent. Items whose parent lies outside the set
/// are leaves for this purpose, which is what makes partial deletes work.
///
/// A `parent_id` cycle can't be peeled; rather than loop forever, the
/// remainder is emitted as-is and the database rejects it — a loud failure
/// beats a hang.
#[must_use]
pub fn order_deepest_first(items: Vec<MenuItem>) -> Vec<MenuItem> {
    let mut remaining = items;
    let mut out = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let claimed: std::collections::HashSet<i64> =
            remaining.iter().filter_map(|it| it.parent_id).collect();
        let (leaves, rest): (Vec<MenuItem>, Vec<MenuItem>) = remaining
            .into_iter()
            .partition(|it| !it.id.get().copied().is_some_and(|id| claimed.contains(&id)));
        if leaves.is_empty() {
            // Every survivor is claimed by another survivor — a cycle.
            out.extend(rest);
            return out;
        }
        out.extend(leaves);
        remaining = rest;
    }
    out
}

/// Rewrite the page links in a resolved menu tree to a hostname-mapped
/// site's public paths (#640). External URLs are left alone.
pub fn rebase_page_urls(items: &mut [ResolvedMenuItem], site_prefix: &str) {
    for item in items {
        if item.is_page {
            if let Some(url) = item.url.as_mut() {
                *url = crate::site::to_public_path(site_prefix, url);
            }
        }
        rebase_page_urls(&mut item.children, site_prefix);
    }
}

// ----------------------------------------------------------------
// `menu(slug="…")` Tera function (#639). Tera functions can't reach the
// database, so the public render resolves every menu up front and
// installs the map here for the duration of `tera.render`.
// ----------------------------------------------------------------

thread_local! {
    static CURRENT_MENUS: std::cell::RefCell<Option<std::collections::HashMap<String, ResolvedMenu>>> =
        const { std::cell::RefCell::new(None) };
}

/// Clears the installed menus on drop. Returned by [`install`].
#[must_use = "the guard clears the thread-local on drop; bind it to a local"]
pub struct MenusGuard {
    _priv: (),
}

impl Drop for MenusGuard {
    fn drop(&mut self) {
        CURRENT_MENUS.with(|cell| *cell.borrow_mut() = None);
    }
}

/// Install the resolved `slug → menu` map `menu(slug=…)` reads, until
/// the guard drops.
pub fn install(menus: std::collections::HashMap<String, ResolvedMenu>) -> MenusGuard {
    CURRENT_MENUS.with(|cell| *cell.borrow_mut() = Some(menus));
    MenusGuard { _priv: () }
}

/// Register `menu(slug="…")`: the resolved menu (`slug`, `name`, nested
/// `items`), or null when no menu has that slug. Wired from
/// [`crate::urls::register_tera_helpers`].
pub fn register_tera_function(tera: &mut tera::Tera) {
    tera.register_function("menu", MenuFn);
}

struct MenuFn;

impl tera::Function for MenuFn {
    fn call(&self, args: &std::collections::HashMap<String, tera::Value>) -> tera::Result<tera::Value> {
        let Some(slug) = args.get("slug").and_then(tera::Value::as_str) else {
            return Err(tera::Error::msg("menu() needs a `slug` argument"));
        };
        CURRENT_MENUS.with(|cell| match cell.borrow().as_ref().and_then(|m| m.get(slug)) {
            Some(menu) => tera::to_value(menu).map_err(tera::Error::json),
            None => Ok(tera::Value::Null),
        })
    }
}

#[cfg(test)]
mod delete_order_tests {
    use super::*;

    fn item(id: i64, parent: Option<i64>) -> MenuItem {
        MenuItem {
            id: rustango::sql::Auto::Set(id),
            menu_id: 1,
            parent_id: parent,
            sort_order: 0,
            label: String::new(),
            page_id: None,
            external_url: None,
            open_in_new_tab: false,
        }
    }

    fn ids(items: &[MenuItem]) -> Vec<i64> {
        items.iter().filter_map(|i| i.id.get().copied()).collect()
    }

    /// A child must never be emitted after its parent.
    fn assert_children_first(ordered: &[MenuItem]) {
        let order = ids(ordered);
        for it in ordered {
            let Some(parent) = it.parent_id else { continue };
            let Some(id) = it.id.get().copied() else {
                continue;
            };
            let (Some(ci), Some(pi)) = (
                order.iter().position(|&x| x == id),
                order.iter().position(|&x| x == parent),
            ) else {
                continue; // parent outside the set — nothing to order against
            };
            assert!(ci < pi, "child {id} deleted after parent {parent}: {order:?}");
        }
    }

    #[test]
    fn a_three_level_subtree_deletes_leaves_first() {
        // The case that failed nondeterministically: A → B → C all tied
        // under the old `parent_id.is_none()` key.
        let ordered = order_deepest_first(vec![
            item(1, None),
            item(2, Some(1)),
            item(3, Some(2)),
        ]);
        assert_eq!(ids(&ordered), vec![3, 2, 1]);
        assert_children_first(&ordered);
    }

    #[test]
    fn input_order_does_not_matter() {
        for input in [
            vec![item(1, None), item(2, Some(1)), item(3, Some(2))],
            vec![item(3, Some(2)), item(1, None), item(2, Some(1))],
            vec![item(2, Some(1)), item(3, Some(2)), item(1, None)],
        ] {
            assert_children_first(&order_deepest_first(input));
        }
    }

    #[test]
    fn siblings_and_several_roots_are_all_ordered() {
        let ordered = order_deepest_first(vec![
            item(1, None),
            item(2, Some(1)),
            item(3, Some(1)),
            item(4, Some(3)),
            item(5, None),
        ]);
        assert_eq!(ordered.len(), 5);
        assert_children_first(&ordered);
    }

    #[test]
    fn an_item_whose_parent_is_kept_is_a_leaf_here() {
        // Partial delete: 9's parent (7) survives the save, so 9 has
        // nothing to wait for.
        let ordered = order_deepest_first(vec![item(9, Some(7))]);
        assert_eq!(ids(&ordered), vec![9]);
    }

    #[test]
    fn a_cycle_terminates_instead_of_hanging() {
        // Corrupt data must not spin the request forever; the DB rejects
        // the delete instead.
        let ordered = order_deepest_first(vec![item(1, Some(2)), item(2, Some(1))]);
        assert_eq!(ordered.len(), 2);
    }

    #[test]
    fn an_empty_set_is_empty() {
        assert!(order_deepest_first(Vec::new()).is_empty());
    }
}

/// Order menu items in **tree pre-order**: each item immediately
/// followed by its own subtree, siblings by `(sort_order, id)`.
///
/// The menu builder's save reconstructs each item's parent from DOM
/// adjacency — "the most recent card one level shallower" — which is how
/// dragging expresses intent. That contract only holds if the DOM starts
/// in pre-order. The editor loaded items `ORDER BY parent_id, sort_order`
/// instead, which is not pre-order in any dialect, so simply opening a
/// menu and pressing Save rewrote it: on Postgres (NULLs last) a child
/// became a root; on sqlite (NULLs first) it was re-parented to whichever
/// root happened to precede it.
///
/// Items unreachable from the root — an orphan whose parent was deleted,
/// or a `parent_id` cycle — are appended at the end rather than dropped.
/// Dropping them would hide them from the editor *and* delete them on the
/// next save, turning a display bug into data loss.
#[must_use]
pub fn order_pre_order(items: Vec<MenuItem>) -> Vec<MenuItem> {
    use std::collections::{HashMap, HashSet};

    let mut children_of: HashMap<Option<i64>, Vec<usize>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        children_of.entry(it.parent_id).or_default().push(i);
    }
    for group in children_of.values_mut() {
        group.sort_by_key(|&i| (items[i].sort_order, items[i].id.get().copied().unwrap_or(0)));
    }

    let mut out: Vec<usize> = Vec::with_capacity(items.len());
    let mut seen: HashSet<usize> = HashSet::new();
    // Explicit stack rather than recursion: a corrupt `parent_id` cycle
    // must not blow the stack, and `seen` makes re-entry impossible.
    let mut stack: Vec<usize> = children_of
        .get(&None)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .rev()
        .collect();
    while let Some(i) = stack.pop() {
        if !seen.insert(i) {
            continue;
        }
        out.push(i);
        if let Some(kids) = items[i]
            .id
            .get()
            .copied()
            .and_then(|id| children_of.get(&Some(id)))
        {
            for &k in kids.iter().rev() {
                stack.push(k);
            }
        }
    }
    // Whatever the walk could not reach.
    for i in 0..items.len() {
        if !seen.contains(&i) {
            out.push(i);
        }
    }

    let mut slots: Vec<Option<MenuItem>> = items.into_iter().map(Some).collect();
    out.into_iter()
        .filter_map(|i| slots[i].take())
        .collect()
}

#[cfg(test)]
mod pre_order_tests {
    use super::*;

    fn item(id: i64, parent: Option<i64>, sort_order: i32) -> MenuItem {
        MenuItem {
            id: rustango::sql::Auto::Set(id),
            menu_id: 1,
            parent_id: parent,
            sort_order,
            label: format!("item-{id}"),
            page_id: None,
            external_url: None,
            open_in_new_tab: false,
        }
    }

    fn ids(items: &[MenuItem]) -> Vec<i64> {
        items.iter().filter_map(|i| i.id.get().copied()).collect()
    }

    #[test]
    fn a_child_immediately_follows_its_parent() {
        // The property the builder's save depends on.
        let ordered = order_pre_order(vec![
            item(3, Some(1), 0),
            item(1, None, 0),
            item(2, None, 10),
        ]);
        assert_eq!(ids(&ordered), vec![1, 3, 2]);
    }

    #[test]
    fn siblings_come_back_in_sort_order() {
        let ordered = order_pre_order(vec![
            item(1, None, 0),
            item(2, Some(1), 20),
            item(3, Some(1), 10),
        ]);
        assert_eq!(ids(&ordered), vec![1, 3, 2]);
    }

    #[test]
    fn a_deep_subtree_stays_contiguous() {
        let ordered = order_pre_order(vec![
            item(1, None, 0),
            item(2, Some(1), 0),
            item(3, Some(2), 0),
            item(4, None, 10),
        ]);
        assert_eq!(ids(&ordered), vec![1, 2, 3, 4]);
    }

    #[test]
    fn the_result_never_loses_an_item() {
        // The regression that would turn a display bug into data loss:
        // anything missing here is invisible in the editor and deleted by
        // the next save.
        let input = vec![
            item(1, None, 0),
            item(2, Some(1), 0),
            item(9, Some(404), 0), // orphan — parent not in this menu
        ];
        let ordered = order_pre_order(input);
        assert_eq!(ordered.len(), 3);
        assert!(ids(&ordered).contains(&9));
    }

    #[test]
    fn a_cycle_terminates_and_keeps_both_items() {
        let ordered = order_pre_order(vec![
            item(1, None, 0),
            item(2, Some(3), 0),
            item(3, Some(2), 0),
        ]);
        assert_eq!(ordered.len(), 3);
        assert_eq!(ids(&ordered)[0], 1, "the reachable root still comes first");
    }

    #[test]
    fn an_empty_menu_is_empty() {
        assert!(order_pre_order(Vec::new()).is_empty());
    }

    #[test]
    fn input_order_does_not_change_the_result() {
        // The editor previously fed this in `ORDER BY parent_id` order,
        // which differs per dialect on NULL placement.
        let a = order_pre_order(vec![
            item(1, None, 0),
            item(2, Some(1), 0),
            item(3, None, 10),
        ]);
        let b = order_pre_order(vec![
            item(2, Some(1), 0),
            item(3, None, 10),
            item(1, None, 0),
        ]);
        assert_eq!(ids(&a), ids(&b));
    }
}

#[cfg(test)]
mod menu_fn_tests {
    use super::*;

    fn render(src: &str) -> tera::Result<String> {
        let mut tera = tera::Tera::default();
        register_tera_function(&mut tera);
        tera.add_raw_template("t.html", src).unwrap();
        tera.render("t.html", &tera::Context::new())
    }

    /// #639 — `menu(slug=…)` reads the menus the render installed.
    #[test]
    fn menu_returns_the_installed_menu_or_null() {
        let item = ResolvedMenuItem {
            id: Some(1),
            label: "About".into(),
            url: Some("/about".into()),
            is_page: true,
            page_id: Some(7),
            open_in_new_tab: false,
            is_active: false,
            in_active_trail: false,
            children: Vec::new(),
        };
        let menu = ResolvedMenu {
            slug: "main".into(),
            name: "Main".into(),
            items: vec![item],
        };
        let _g = install(std::collections::HashMap::from([("main".to_owned(), menu)]));
        let out = render(
            r#"{% set nav = menu(slug="main") %}{{ nav.name }}:{% for i in nav.items %}{{ i.label }}={{ i.url }}{% endfor %}|{% set gone = menu(slug="nope") %}{% if gone %}x{% else %}none{% endif %}"#,
        )
        .unwrap();
        assert_eq!(out, "Main:About=&#x2F;about|none", "autoescaped like any .html template");
        assert!(render("{{ menu() }}").is_err(), "a missing slug is a template error");
    }

    #[test]
    fn guard_clears_the_menus() {
        drop(install(std::collections::HashMap::new()));
        CURRENT_MENUS.with(|c| assert!(c.borrow().is_none()));
    }

    /// #640 — page links move to the site's public path; external ones stay.
    #[test]
    fn rebase_rewrites_page_links_at_every_depth() {
        let item = |label: &str, url: &str, is_page: bool, children| ResolvedMenuItem {
            id: None,
            label: label.into(),
            url: Some(url.into()),
            is_page,
            page_id: None,
            open_in_new_tab: false,
            is_active: false,
            in_active_trail: false,
            children,
        };
        let mut items = vec![item(
            "Shop",
            "/shop",
            true,
            vec![item("Catalog", "/shop/catalog", true, vec![]), item("Docs", "https://x.test/shop/a", false, vec![])],
        )];
        rebase_page_urls(&mut items, "/shop");
        assert_eq!(items[0].url.as_deref(), Some("/"));
        assert_eq!(items[0].children[0].url.as_deref(), Some("/catalog"));
        assert_eq!(items[0].children[1].url.as_deref(), Some("https://x.test/shop/a"));
    }
}
