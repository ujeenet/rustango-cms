//! `pageurl(id=N)` Tera helper — resolve a `cms_page` id to its
//! public URL at render time.
//!
//! A `pageurl` lookup handles the common
//! case of a `ForeignKey(Page)` on an extension row (CTA targets,
//! related-page choosers, etc.) — the template asks for the URL,
//! the CMS walks the page tree. Concretely, the public render
//! pipeline scans the extension dict for `*_id` fields whose values
//! resolve to a `cms_page` row, pre-fetches the rows in a single
//! batch query, and exposes both a `_pages_by_id` ctx map AND a
//! `pageurl(id=…)` Tera function (plus a `| pageurl` filter) so
//! templates can write `{{ pageurl(id=extension.hero_cta_page_id) }}`.
//!
//! Tera 1.x doesn't pass the global context into function / filter
//! args, so we stash the resolved map in a thread-local that
//! [`render::render_inner`](mod@crate::render) sets right before
//! `tera.render(…)` runs and clears the moment render returns
//! (RAII via [`PageUrlGuard`]). Tera's `render` is fully sync, so the
//! thread-local survives the whole call without leaking across
//! requests scheduled on the same tokio worker.
//!
//! Pre-fetch is capped at [`MAX_PREFETCH_IDS`] lookups per page
//! render. Above the cap the function still resolves ids that DID
//! make it into the map; the rest fall back to `"#"`. The cap is
//! deliberately small so a malformed extension JSON (or a future
//! N-to-M field) can't fan a single page request out into hundreds
//! of point lookups.
//!
//! Missing / unresolvable / 0 ids return `"#"` so templates can drop
//! the value straight into an `href` attribute without a guard.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use rustango::core::Column as _;
use rustango::sql::FetcherPool as _;
use tera::{Filter, Function, Tera, Value};

use crate::page::Page;

/// Cap on the number of `*_id` fields the prefetch walker resolves
/// per page render. See module docs for the rationale.
pub const MAX_PREFETCH_IDS: usize = 16;

thread_local! {
    /// Per-thread `page_id → url_path` map. Lives only for the
    /// duration of a single `tera.render` call, installed and cleared
    /// via [`PageUrlGuard`].
    static CURRENT_PAGES_BY_ID: RefCell<Option<Arc<HashMap<i64, String>>>>
        = const { RefCell::new(None) };

    /// Per-thread public mount prefix (`""` at root, `"/p"` for a
    /// prefixed mount) — installed by the render pipeline so
    /// [`page_href`](self) can emit fully-qualified links. Cleared via
    /// [`UrlPrefixGuard`].
    static CURRENT_URL_PREFIX: RefCell<String> = const { RefCell::new(String::new()) };
    /// The serving site's root path, stripped from emitted links.
    static CURRENT_SITE_PREFIX: RefCell<String> = const { RefCell::new(String::new()) };
}

/// RAII guard. While in scope the thread-local map is set; on drop
/// it's cleared. Returned by [`install`]; callers MUST keep it alive
/// across `tera.render(…)` and let it drop right after.
#[must_use = "the guard clears the thread-local on drop; bind it to a local"]
pub struct PageUrlGuard {
    // Field exists so the guard isn't ZST and can be `must_use`.
    _priv: (),
}

impl Drop for PageUrlGuard {
    fn drop(&mut self) {
        CURRENT_PAGES_BY_ID.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

/// Install `pages_by_id` for the duration of the returned guard.
/// Subsequent calls to `pageurl(id=N)` on this thread resolve N
/// against the installed map.
pub fn install(pages_by_id: HashMap<i64, String>) -> PageUrlGuard {
    CURRENT_PAGES_BY_ID.with(|cell| {
        *cell.borrow_mut() = Some(Arc::new(pages_by_id));
    });
    PageUrlGuard { _priv: () }
}

/// Resolve a page id against the currently-installed map. Returns
/// `None` when nothing is installed, when `id <= 0`, or when the
/// page wasn't pre-fetched. Used by [`crate::richtext`] to
/// rewrite `<a linktype="page" id="N">` anchors against the same
/// map the `pageurl(id=…)` Tera function reads.
#[must_use]
pub fn lookup(id: i64) -> Option<String> {
    if id <= 0 {
        return None;
    }
    CURRENT_PAGES_BY_ID.with(|cell| cell.borrow().as_ref().and_then(|m| m.get(&id).cloned()))
}

/// RAII guard for the render-scoped `url_prefix`. Mirrors
/// [`PageUrlGuard`]; clears the prefix on drop.
#[must_use = "the guard clears the thread-local on drop; bind it to a local"]
pub struct UrlPrefixGuard {
    _priv: (),
}

impl Drop for UrlPrefixGuard {
    fn drop(&mut self) {
        CURRENT_URL_PREFIX.with(|cell| cell.borrow_mut().clear());
    }
}

/// RAII guard for the render-scoped site prefix.
#[must_use = "the guard clears the thread-local on drop; bind it to a local"]
pub struct SitePrefixGuard {
    _priv: (),
}
impl Drop for SitePrefixGuard {
    fn drop(&mut self) {
        CURRENT_SITE_PREFIX.with(|cell| cell.borrow_mut().clear());
    }
}

/// Install the serving site's root path, so links to pages inside that
/// site are emitted relative to it.
///
/// The inverse of the mount prefix, and applied first: `url_path` is
/// stored tenant-absolute (`/site-b/about`), while a visitor on
/// `siteb.com` must see `/about`. A page outside the site keeps its
/// absolute path — see [`crate::site::to_public_path`].
pub fn install_site_prefix(prefix: &str) -> SitePrefixGuard {
    CURRENT_SITE_PREFIX.with(|cell| {
        *cell.borrow_mut() = prefix.to_owned();
    });
    SitePrefixGuard { _priv: () }
}

fn current_site_prefix() -> String {
    CURRENT_SITE_PREFIX.with(|cell| cell.borrow().clone())
}

/// Install the public mount `prefix` for the duration of the returned
/// guard so [`page_href`](self) emits `{prefix}{url_path}` links. The
/// render pipeline installs this alongside the `pageurl` map.
pub fn install_url_prefix(prefix: &str) -> UrlPrefixGuard {
    CURRENT_URL_PREFIX.with(|cell| {
        *cell.borrow_mut() = prefix.to_owned();
    });
    UrlPrefixGuard { _priv: () }
}

fn current_url_prefix() -> String {
    CURRENT_URL_PREFIX.with(|cell| cell.borrow().clone())
}

/// Register the `pageurl` Tera function and the `| pageurl` filter
/// on `tera`. Call once at Tera setup — wired from
/// [`crate::urls::register_tera_helpers`] so host apps that use the
/// bundled `register_templates` get it for free.
///
/// Both forms are marked `is_safe = true`: the URL is read from the
/// `cms_page.url_path` column maintained by `tree_ops`, which is
/// CMS-controlled, not user input, so the output can flow into
/// `href=…` unescaped.
pub fn register_tera_function(tera: &mut Tera) {
    tera.register_function("pageurl", PageurlFn);
    tera.register_filter("pageurl", PageurlFilter);
    // #NNN — `page_href`: the one-call, prefix-aware page→link helper.
    // Function form `page_href(page=child)` / `page_href(id=42)`, filter
    // form `{{ child | page_href }}`. Unlike `pageurl` (bare url_path, id
    // only) it accepts a page-shaped object *and* prepends the mount
    // `url_prefix`, so a `children`/`ancestors` loop links correctly for
    // deep trees without the template hand-building `url_prefix ~ slug`.
    tera.register_function("page_href", PageHrefFn);
    tera.register_filter("page_href", PageHrefFilter);
    // #members — `page_link`: emit a ready-made `<a href>title</a>` for a
    // page-shaped object (or id), prefix-aware, title HTML-escaped.
    // Function `page_link(page=child, class="nav")` / filter
    // `{{ child | page_link }}`. Pair with `can_view` to hide protected
    // links: `{% if child | can_view %}{{ child | page_link }}{% endif %}`.
    tera.register_function("page_link", PageLinkFn);
    tera.register_filter("page_link", PageLinkFilter);
}

struct PageLinkFn;

impl Function for PageLinkFn {
    fn call(&self, args: &HashMap<String, Value>) -> tera::Result<Value> {
        let (href, title) = match args.get("page") {
            Some(page) => (href_from_value(page), title_from_value(page)),
            None => {
                let id = args.get("id").and_then(read_id);
                (href_from_id(id), None)
            }
        };
        let class = args.get("class").and_then(Value::as_str);
        Ok(Value::String(anchor(&href, title.as_deref(), class)))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

struct PageLinkFilter;

impl Filter for PageLinkFilter {
    fn filter(&self, value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
        let class = args.get("class").and_then(Value::as_str);
        Ok(Value::String(anchor(
            &href_from_value(value),
            title_from_value(value).as_deref(),
            class,
        )))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

/// Read a page-shaped value's title (falls back to the href text when
/// absent so the anchor is never empty).
fn title_from_value(value: &Value) -> Option<String> {
    value
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Build a safe `<a>` element. `href` is CMS-controlled (`url_path`);
/// `title` is page-authored text, so it's HTML-escaped.
fn anchor(href: &str, title: Option<&str>, class: Option<&str>) -> String {
    let label = escape_html(title.unwrap_or(href));
    match class {
        Some(c) if !c.is_empty() => {
            format!(r#"<a class="{}" href="{href}">{label}</a>"#, escape_html(c))
        }
        _ => format!(r#"<a href="{href}">{label}</a>"#),
    }
}

/// Minimal HTML-attribute/text escaper for the `page_link` label +
/// class (the helper is `is_safe`, so it must escape its own dynamic
/// text). `href` comes from the CMS-maintained `url_path` and is left
/// verbatim, matching `pageurl` / `page_href`.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

struct PageHrefFn;

impl Function for PageHrefFn {
    fn call(&self, args: &HashMap<String, Value>) -> tera::Result<Value> {
        // `page=<object>` wins; else `id=<int>`.
        let href = match args.get("page") {
            Some(page) => href_from_value(page),
            None => href_from_id(args.get("id").and_then(read_id)),
        };
        Ok(Value::String(href))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

struct PageHrefFilter;

impl Filter for PageHrefFilter {
    fn filter(&self, value: &Value, _args: &HashMap<String, Value>) -> tera::Result<Value> {
        Ok(Value::String(href_from_value(value)))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

/// Build a page href from a Tera value that is either a page-shaped
/// object (has a `url_path`) or a bare id (i64 / stringified int).
fn href_from_value(value: &Value) -> String {
    if let Some(url_path) = value.get("url_path").and_then(Value::as_str) {
        return with_prefix(url_path);
    }
    // Not an object with url_path — treat it as an id reference.
    href_from_id(read_id(value))
}

/// Build a page href from a page id resolved against the installed
/// `pageurl` map. Unresolved / missing → `"#"`.
fn href_from_id(id: Option<i64>) -> String {
    match id.and_then(lookup) {
        Some(url_path) => with_prefix(&url_path),
        None => "#".to_owned(),
    }
}

/// Prepend the render-scoped mount prefix to a `url_path`. An empty
/// `url_path` (root/unset) yields `{prefix}/` so the link is never bare.
fn with_prefix(url_path: &str) -> String {
    let prefix = current_url_prefix();
    // Site first, mount second. `url_path` is tenant-absolute, so the
    // site's root is stripped before the mount prefix is prepended —
    // the other order would try to strip `/blog/site-b` from a path that
    // only ever contained `/site-b`.
    let site = current_site_prefix();
    let url_path = crate::site::to_public_path(&site, url_path);
    if url_path.is_empty() {
        format!("{prefix}/")
    } else {
        format!("{prefix}{url_path}")
    }
}

struct PageurlFn;

impl Function for PageurlFn {
    fn call(&self, args: &HashMap<String, Value>) -> tera::Result<Value> {
        let id = args.get("id").and_then(read_id);
        // Same href as `page_href`: mount prefix + the site's public path (#640).
        Ok(Value::String(href_from_id(id)))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

struct PageurlFilter;

impl Filter for PageurlFilter {
    fn filter(&self, value: &Value, _args: &HashMap<String, Value>) -> tera::Result<Value> {
        Ok(Value::String(href_from_id(read_id(value))))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

/// Accept i64 (post-save shape) and stringified ints (preview-form
/// shape — HTML form values come through as strings). `null` and
/// empty strings yield `None`, which the caller turns into `"#"`.
fn read_id(v: &Value) -> Option<i64> {
    if v.is_null() {
        return None;
    }
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<i64>().ok())
}

/// Scan the extension JSON for top-level fields whose name ends with
/// `_id` and value is an i64, batch-fetch the matching `cms_page`
/// rows, and return a `page_id → url_path` map. Bounded at
/// [`MAX_PREFETCH_IDS`] lookups per call — see the module docs.
///
/// Returns an empty map on any error (driver failure, no candidate
/// ids, etc.); the rendered template falls back to `"#"` and the
/// page still renders. The error is logged.
///
/// # Errors
/// None — failures degrade to an empty map.
pub async fn prefetch_for_extension(
    extension: &serde_json::Value,
    pool: &rustango::sql::Pool,
) -> HashMap<i64, String> {
    let mut ids: Vec<i64> = candidate_ids(extension);
    if ids.is_empty() {
        return HashMap::new();
    }
    // Stable dedupe — same id referenced under multiple `*_id` fields
    // still incurs one row in the IN list.
    ids.sort_unstable();
    ids.dedup();
    if ids.len() > MAX_PREFETCH_IDS {
        tracing::debug!(
            target: "rustango_cms::page_url",
            count = ids.len(),
            cap = MAX_PREFETCH_IDS,
            "extension referenced more *_id fields than the prefetch cap; tail truncated",
        );
        ids.truncate(MAX_PREFETCH_IDS);
    }
    match Page::objects()
        .where_(Page::id.is_in(ids))
        .fetch(pool)
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|p| p.id.get().copied().map(|id| (id, p.url_path)))
            .collect(),
        Err(e) => {
            tracing::warn!(
                target: "rustango_cms::page_url",
                error = %e,
                "pageurl prefetch failed; templates will see '#'",
            );
            HashMap::new()
        }
    }
}

/// Pull every `*_id` field off the top-level extension object and
/// return their i64 values (positive only). Stringified ints are
/// accepted alongside raw numbers — extension JSON for an unsaved
/// preview carries form-shape strings.
///
/// The walk is intentionally shallow: nested objects / arrays aren't
/// descended. Nested chooser refs (stream blocks, repeat blocks) are
/// already enriched by
/// [`crate::block::tera_helpers::enrich_chooser_refs_async`] with
/// `value._url`, which is a separate channel.
fn candidate_ids(extension: &serde_json::Value) -> Vec<i64> {
    let Some(obj) = extension.as_object() else {
        return Vec::new();
    };
    obj.iter()
        .filter(|(k, _)| k.ends_with("_id"))
        .filter_map(|(_, v)| read_id(v))
        .filter(|n| *n > 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tera::{Context, Tera};

    fn fresh_tera() -> Tera {
        let mut tera = Tera::default();
        register_tera_function(&mut tera);
        tera
    }

    #[test]
    fn function_resolves_known_id() {
        let mut map = HashMap::new();
        map.insert(42i64, "/about".to_owned());
        let _guard = install(map);

        let tera = fresh_tera();
        let mut t = tera.clone();
        t.add_raw_template("t.html", "{{ pageurl(id=42) }}")
            .unwrap();
        let out = t.render("t.html", &Context::new()).unwrap();
        assert_eq!(out, "/about");
    }

    #[test]
    fn function_returns_hash_for_unknown_id() {
        let _guard = install(HashMap::new());
        let tera = fresh_tera();
        let mut t = tera.clone();
        t.add_raw_template("t.html", "{{ pageurl(id=999) }}")
            .unwrap();
        let out = t.render("t.html", &Context::new()).unwrap();
        assert_eq!(out, "#");
    }

    #[test]
    fn function_returns_hash_for_zero_and_negative() {
        let mut map = HashMap::new();
        map.insert(0i64, "/should-not-leak".to_owned());
        let _guard = install(map);
        let tera = fresh_tera();
        let mut t = tera.clone();
        t.add_raw_template("z.html", "{{ pageurl(id=0) }}").unwrap();
        t.add_raw_template("n.html", "{{ pageurl(id=-3) }}")
            .unwrap();
        assert_eq!(t.render("z.html", &Context::new()).unwrap(), "#");
        assert_eq!(t.render("n.html", &Context::new()).unwrap(), "#");
    }

    #[test]
    fn function_accepts_stringified_id() {
        // Preview-form values come through as JSON strings.
        let mut map = HashMap::new();
        map.insert(7i64, "/team".to_owned());
        let _guard = install(map);
        let tera = fresh_tera();
        let mut t = tera.clone();
        t.add_raw_template("t.html", r#"{{ pageurl(id="7") }}"#)
            .unwrap();
        assert_eq!(t.render("t.html", &Context::new()).unwrap(), "/team");
    }

    #[test]
    fn filter_resolves_known_id() {
        let mut map = HashMap::new();
        map.insert(3i64, "/blog".to_owned());
        let _guard = install(map);
        let tera = fresh_tera();
        let mut t = tera.clone();
        let mut ctx = Context::new();
        ctx.insert("hero_cta_page_id", &3i64);
        t.add_raw_template("t.html", "{{ hero_cta_page_id | pageurl }}")
            .unwrap();
        assert_eq!(t.render("t.html", &ctx).unwrap(), "/blog");
    }

    #[test]
    fn filter_returns_hash_for_null() {
        let _guard = install(HashMap::new());
        let tera = fresh_tera();
        let mut t = tera.clone();
        let mut ctx = Context::new();
        ctx.insert("maybe_id", &serde_json::Value::Null);
        t.add_raw_template("t.html", "{{ maybe_id | pageurl }}")
            .unwrap();
        assert_eq!(t.render("t.html", &ctx).unwrap(), "#");
    }

    #[test]
    fn guard_clears_thread_local() {
        {
            let mut map = HashMap::new();
            map.insert(1i64, "/x".to_owned());
            let _g = install(map);
            assert_eq!(href_from_id(Some(1)), "/x");
        }
        assert_eq!(href_from_id(Some(1)), "#");
    }

    #[test]
    fn candidate_ids_picks_up_underscore_id_fields() {
        let ext = json!({
            "title": "Home",
            "hero_cta_page_id": 4,
            "related_page_id": "9",
            "irrelevant": 1,
            "body_stream": [{"type": "page_chooser", "value": {"page_id": 99}}],
            "expire_at_id": null,
        });
        let mut ids = candidate_ids(&ext);
        ids.sort_unstable();
        assert_eq!(ids, vec![4, 9]);
    }

    #[test]
    fn candidate_ids_ignores_non_object_extension() {
        assert!(candidate_ids(&serde_json::Value::Null).is_empty());
        assert!(candidate_ids(&json!([1, 2, 3])).is_empty());
        assert!(candidate_ids(&json!("string")).is_empty());
    }

    #[test]
    fn candidate_ids_skips_non_positive() {
        let ext = json!({"foo_id": 0, "bar_id": -1, "baz_id": 5});
        let ids = candidate_ids(&ext);
        assert_eq!(ids, vec![5]);
    }

    // ---- page_href ----

    fn render_expr(expr: &str, ctx: &Context) -> String {
        let mut t = fresh_tera();
        t.add_raw_template("t.html", expr).unwrap();
        t.render("t.html", ctx).unwrap()
    }

    #[test]
    fn page_href_filter_uses_full_url_path_not_leaf_slug() {
        // A grandchild at /about/team must link to the FULL path, not /team.
        let _prefix = install_url_prefix("");
        let mut ctx = Context::new();
        ctx.insert(
            "child",
            &json!({ "slug": "team", "url_path": "/about/team", "title": "Team" }),
        );
        assert_eq!(render_expr("{{ child | page_href }}", &ctx), "/about/team");
    }

    #[test]
    fn page_href_prepends_mount_prefix() {
        let _prefix = install_url_prefix("/p");
        let mut ctx = Context::new();
        ctx.insert("child", &json!({ "url_path": "/about" }));
        assert_eq!(render_expr("{{ child | page_href }}", &ctx), "/p/about");
    }

    #[test]
    fn page_href_function_form_and_id_resolution() {
        let mut map = HashMap::new();
        map.insert(42i64, "/about".to_owned());
        let _guard = install(map);
        let _prefix = install_url_prefix("/p");
        // object form
        let mut ctx = Context::new();
        ctx.insert("anc", &json!({ "url_path": "/about/team" }));
        assert_eq!(
            render_expr("{{ page_href(page=anc) }}", &ctx),
            "/p/about/team"
        );
        // id form resolves via the installed map + prefix
        assert_eq!(
            render_expr("{{ page_href(id=42) }}", &Context::new()),
            "/p/about"
        );
    }

    #[test]
    fn page_href_missing_and_empty() {
        let _guard = install(HashMap::new());
        let _prefix = install_url_prefix("");
        // unresolved id → "#"
        assert_eq!(render_expr("{{ page_href(id=999) }}", &Context::new()), "#");
        // object with empty url_path (root) → "{prefix}/"
        let mut ctx = Context::new();
        ctx.insert("root", &json!({ "url_path": "" }));
        assert_eq!(render_expr("{{ root | page_href }}", &ctx), "/");
    }

    #[test]
    fn page_href_prefix_guard_clears() {
        {
            let _p = install_url_prefix("/p");
            assert_eq!(current_url_prefix(), "/p");
        }
        assert_eq!(current_url_prefix(), "");
    }


    /// The composition that matters: a site-relative link, emitted from a
    /// CMS mounted under a path prefix. Site strip happens first.
    #[test]
    fn site_prefix_is_stripped_before_the_mount_prefix_is_added() {
        let _mount = install_url_prefix("/p");
        let _site = install_site_prefix("/site-b");
        let mut ctx = Context::new();
        ctx.insert("child", &json!({ "url_path": "/site-b/about" }));
        assert_eq!(render_expr("{{ child | page_href }}", &ctx), "/p/about");
    }

    #[test]
    fn the_site_root_itself_links_to_the_mount_root() {
        let _mount = install_url_prefix("");
        let _site = install_site_prefix("/site-b");
        let mut ctx = Context::new();
        ctx.insert("child", &json!({ "url_path": "/site-b" }));
        assert_eq!(render_expr("{{ child | page_href }}", &ctx), "/");
    }

    /// A page on a different site must NOT be rewritten — an honest link
    /// elsewhere beats a mangled one that 404s.
    #[test]
    fn a_page_outside_the_site_keeps_its_absolute_path() {
        let _mount = install_url_prefix("");
        let _site = install_site_prefix("/site-b");
        let mut ctx = Context::new();
        ctx.insert("child", &json!({ "url_path": "/other/page" }));
        assert_eq!(render_expr("{{ child | page_href }}", &ctx), "/other/page");
    }

    /// With no site installed the behaviour is exactly what it was, which
    /// is what keeps every existing single-site deployment unchanged.
    #[test]
    fn no_site_prefix_means_unchanged_behaviour() {
        let _mount = install_url_prefix("/p");
        let mut ctx = Context::new();
        ctx.insert("child", &json!({ "url_path": "/about" }));
        assert_eq!(render_expr("{{ child | page_href }}", &ctx), "/p/about");
    }

    /// `pageurl` emits the same href as `page_href`.
    #[test]
    fn pageurl_strips_the_site_and_adds_the_mount_prefix() {
        let _mount = install_url_prefix("/p");
        let _site = install_site_prefix("/site-b");
        let _pages = install(HashMap::from([(4i64, "/site-b/about".to_owned())]));
        let ctx = Context::new();
        assert_eq!(render_expr("{{ pageurl(id=4) }}", &ctx), "/p/about");
        assert_eq!(render_expr("{{ 4 | pageurl }}", &ctx), "/p/about");
    }
}
