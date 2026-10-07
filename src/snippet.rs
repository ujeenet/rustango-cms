//! Library snippets — reusable, non-page content addressable by slug.
//!
//! Snippets are the bridge between Library *types* (the trait-shape
//! registrations from [`crate::library`]) and live pages: editors
//! create a snippet under a given type, give it a slug, and any
//! template can pull it back via `{{ cms_snippet(slug="hero-cta") }}`.
//!
//! Storage uses one generic `cms_snippet` table (rather than per-type
//! extension tables) so adding a new `LibraryTypeHandler` doesn't
//! require migrating tables — typed fields go in the `data` JSON
//! column when the registered type wants more than title + body.
//!
//! The slug is unique tenant-wide so `cms_snippet(slug="hero-cta")`
//! resolves without needing the caller to know the type. Pass
//! `type_name=` when slugs collide across types.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// A single reusable content block. One row per snippet instance.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_snippet",
    app = "cms",
    display = "title",
    admin(
        list_display = "type_name, slug, title, word_count, updated_at",
        ordering = "type_name, slug",
        list_filter = "type_name",
    )
)]
pub struct Snippet {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Which `LibraryTypeHandler::type_name()` this row belongs to.
    /// Indexed for the "list snippets of type X" admin view.
    #[rustango(max_length = 64, index)]
    pub type_name: String,

    /// Folder path — slash-separated, no leading slash, trailing
    /// slash optional. Empty string = root. Used purely for the
    /// admin's tree organization; doesn't affect template lookups
    /// (slug remains the unique key).
    #[rustango(max_length = 255, index, default = "''")]
    pub folder_path: String,

    /// Stable URL-friendly identifier authors reference from
    /// templates: `{{ cms_snippet(slug="hero-cta") }}`. Unique
    /// across the tenant so a slug alone is enough to resolve.
    #[rustango(max_length = 128, unique)]
    pub slug: String,

    /// Human display title for the admin list view.
    #[rustango(max_length = 200)]
    pub title: String,

    /// Markdown body — rendered through the same sanitizing
    /// pipeline as page bodies. Most snippet types only need this.
    pub body_markdown: String,

    /// Extra typed fields (CTA button label + URL, author email,
    /// FAQ answer link, etc.) the registered handler knows how to
    /// interpret. JSON object; empty `{}` when unused.
    #[rustango(default = "'{}'::jsonb")]
    pub data: serde_json::Value,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

// =====================================================================
// #123 — opt-in versioned snapshots for library snippets.
// Mirrors the page-revision shape: full-row JSON, per-snippet
// monotonic `sequence`. Only captured for types whose handler
// returns `revisions_enabled() == true`.
// =====================================================================

#[derive(rustango::Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_snippet_revision",
    app = "cms",
    admin(
        list_display = "snippet_id, sequence, captured_by, captured_at",
        ordering = "-captured_at",
        list_filter = "snippet_id",
    )
)]
pub struct SnippetRevision {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_snippet", on = "id", index)]
    pub snippet_id: i64,

    /// Per-snippet monotonic counter starting at 1.
    pub sequence: i32,

    /// Full snapshot of the snippet row at the time of save.
    pub snapshot: serde_json::Value,

    /// `rustango_users.id` of the editor who triggered the save.
    pub captured_by: Option<i64>,

    #[rustango(auto_now_add)]
    pub captured_at: Auto<DateTime<Utc>>,
}

impl SnippetRevision {
    /// Short label used in the admin's revisions card.
    #[must_use]
    pub fn label(&self) -> String {
        let when = self
            .captured_at
            .get()
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_default();
        format!("#{} · {}", self.sequence, when)
    }
}

/// Capture one revision row for `snippet`. Bumps `sequence` off the
/// current max for that snippet id. Idempotent in the sense that
/// repeated calls produce one row per call — callers should only
/// invoke after a successful save.
///
/// # Errors
/// Driver / query failures or serialization errors.
pub async fn capture_revision(
    pool: &rustango::sql::Pool,
    snippet: &Snippet,
    captured_by: Option<i64>,
) -> Result<SnippetRevision, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::{sqlx, FetcherPool as _};
    let snippet_id = snippet.id.get().copied().unwrap_or_default();
    let recent: Vec<SnippetRevision> = SnippetRevision::objects()
        .where_(SnippetRevision::snippet_id.eq(snippet_id))
        .order_by(&[("sequence", true)])
        .fetch(pool)
        .await?;
    let next_seq = recent.first().map_or(1, |r| r.sequence + 1);
    let snapshot = serde_json::to_value(snippet)
        .map_err(|e| rustango::sql::ExecError::Driver(sqlx::Error::Decode(Box::new(e))))?;
    let mut row = SnippetRevision {
        id: Auto::Unset,
        snippet_id,
        sequence: next_seq,
        snapshot,
        captured_by,
        captured_at: Auto::Unset,
    };
    row.insert_pool(pool).await?;
    Ok(row)
}

/// Every revision row for `snippet_id`, newest first.
///
/// # Errors
/// Driver / query failures.
pub async fn revisions_for(
    pool: &rustango::sql::Pool,
    snippet_id: i64,
) -> Result<Vec<SnippetRevision>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    SnippetRevision::objects()
        .where_(SnippetRevision::snippet_id.eq(snippet_id))
        .order_by(&[("sequence", true)])
        .fetch(pool)
        .await
}

/// Fetch one revision by `(snippet_id, sequence)`. Returns `None`
/// when no matching row exists.
///
/// # Errors
/// Driver / query failures.
pub async fn revision_by_sequence(
    pool: &rustango::sql::Pool,
    snippet_id: i64,
    sequence: i32,
) -> Result<Option<SnippetRevision>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let mut rows: Vec<SnippetRevision> = SnippetRevision::objects()
        .where_(SnippetRevision::snippet_id.eq(snippet_id))
        .where_(SnippetRevision::sequence.eq(sequence))
        .fetch(pool)
        .await?;
    Ok(rows.pop())
}

// =====================================================================
// Tera function — `{{ cms_snippet(slug=...) }}` resolves to rendered
// HTML inside any template the host app registered the function with.
// =====================================================================

/// Write (or clear) an element's own template name in its `data`.
///
/// An empty `name` removes the key rather than storing `""`, so the
/// element falls back to its type's template — "clear the box to go
/// back to the default" is what an editor expects, and an empty
/// string would otherwise be a name that renders nothing.
///
/// A `data` that is not a JSON object (null on a fresh row, or a row
/// written by something else) is replaced with one; the alternative
/// is silently dropping the edit.
pub fn set_element_template(data: &mut serde_json::Value, name: &str) {
    if !data.is_object() {
        if name.is_empty() {
            return;
        }
        *data = serde_json::Value::Object(serde_json::Map::new());
    }
    let Some(obj) = data.as_object_mut() else {
        return;
    };
    if name.is_empty() {
        obj.remove("template");
    } else {
        obj.insert(
            "template".to_owned(),
            serde_json::Value::String(name.to_owned()),
        );
    }
}

/// Normalize a user-typed folder path into the storage form:
/// no leading slash, single trailing slash on non-empty paths,
/// collapsed runs of slashes, and segment trim. Empty input
/// (or `/`) → root (`""`).
///
/// Examples:
/// - `"  /marketing/cta/  "` → `"marketing/cta/"`
/// - `"//foo///bar"` → `"foo/bar/"`
/// - `""` / `"/"` → `""`
#[must_use]
pub fn normalize_folder(raw: &str) -> String {
    let trimmed = raw.trim();
    let segs: Vec<&str> = trimmed
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if segs.is_empty() {
        return String::new();
    }
    let mut out = segs.join("/");
    out.push('/');
    out
}

/// All direct child folders of `parent` — walks every snippet's
/// `folder_path`, keeps only the segment immediately under `parent`,
/// dedupes + sorts. Empty `parent` means root.
///
/// Cheap for the typical Library size (tens to low hundreds of
/// snippets); upgrade to a recursive CTE only if profiling demands.
///
/// # Errors
/// Driver / query failures from the snippet lookup.
pub async fn child_folders(
    pool: &rustango::sql::Pool,
    parent: &str,
) -> Result<Vec<String>, rustango::sql::ExecError> {
    use rustango::sql::FetcherPool as _;
    let rows: Vec<Snippet> = Snippet::objects().fetch(pool).await?;
    let mut out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let parent_norm = normalize_folder(parent);
    for r in rows {
        if !r.folder_path.starts_with(&parent_norm) {
            continue;
        }
        let rest = &r.folder_path[parent_norm.len()..];
        let Some((head, _)) = rest.split_once('/') else {
            continue;
        };
        if head.is_empty() {
            continue;
        }
        out.insert(head.to_owned());
    }
    Ok(out.into_iter().collect())
}

/// Every folder path used by snippets of `type_name`, plus every
/// intermediate folder derived from the leaf paths. Result is sorted
/// and includes the empty (root) folder.
///
/// Example: snippets at `marketing/cta/` and `marketing/banner/`
/// produce `["", "marketing/", "marketing/banner/", "marketing/cta/"]`.
///
/// # Errors
/// Driver / query failures.
pub async fn folder_tree_for_type(
    pool: &rustango::sql::Pool,
    type_name: &str,
) -> Result<Vec<String>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<Snippet> = Snippet::objects()
        .where_(Snippet::type_name.eq(type_name.to_owned()))
        .fetch(pool)
        .await?;
    let mut out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    out.insert(String::new());
    for r in rows {
        if r.folder_path.is_empty() {
            continue;
        }
        let mut acc = String::new();
        for seg in r.folder_path.split('/').filter(|s| !s.is_empty()) {
            acc.push_str(seg);
            acc.push('/');
            out.insert(acc.clone());
        }
    }
    Ok(out.into_iter().collect())
}

thread_local! {
    /// The render's `slug → html` maps (wrapped, body-only), installed by
    /// [`install`] for the duration of `tera.render`.
    static CURRENT_SNIPPETS: std::cell::RefCell<Option<SnippetMaps>> =
        const { std::cell::RefCell::new(None) };
}

type SnippetMaps = (
    std::collections::HashMap<String, String>,
    std::collections::HashMap<String, String>,
);

/// Clears the installed snippet maps on drop. Returned by [`install`].
#[must_use = "the guard clears the thread-local on drop; bind it to a local"]
pub struct SnippetsGuard {
    _priv: (),
}

impl Drop for SnippetsGuard {
    fn drop(&mut self) {
        CURRENT_SNIPPETS.with(|cell| *cell.borrow_mut() = None);
    }
}

/// Install the render's prefetched snippets (from [`prefetch_all`]) for
/// `cms_snippet(slug=…)`, until the guard drops.
pub fn install(
    wrapped: std::collections::HashMap<String, String>,
    inline: std::collections::HashMap<String, String>,
) -> SnippetsGuard {
    CURRENT_SNIPPETS.with(|cell| *cell.borrow_mut() = Some((wrapped, inline)));
    SnippetsGuard { _priv: () }
}

/// Register the `cms_snippet` Tera function. Wired from
/// [`crate::urls::register_tera_helpers`].
///
/// `{{ cms_snippet(slug="hero-cta") }}` is the snippet's rendered body in
/// its wrapper; `body=true` gives the body alone. An unknown slug is an
/// empty string. Tera functions can't read the context, so the maps come
/// from the thread-local the public render installs.
pub fn register_tera_function(tera: &mut tera::Tera) {
    tera.register_function("cms_snippet", CmsSnippetFn);
}

struct CmsSnippetFn;

impl tera::Function for CmsSnippetFn {
    fn call(&self, args: &std::collections::HashMap<String, tera::Value>) -> tera::Result<tera::Value> {
        let slug = args
            .get("slug")
            .and_then(tera::Value::as_str)
            .ok_or_else(|| tera::Error::msg("cms_snippet: `slug` is required"))?;
        let body_only = args.get("body").and_then(tera::Value::as_bool).unwrap_or(false);
        let html = CURRENT_SNIPPETS.with(|cell| {
            cell.borrow()
                .as_ref()
                .and_then(|(wrapped, inline)| if body_only { inline.get(slug) } else { wrapped.get(slug) })
                .cloned()
                .unwrap_or_default()
        });
        Ok(tera::Value::String(html))
    }

    fn is_safe(&self) -> bool {
        true
    }
}

/// Prefetch every snippet for the current tenant and return two
/// `slug → html` maps (wrapped + inline). The public-render flow
/// inserts them into the Tera context so `{{ cms_snippet(slug=…) }}`
/// resolves without re-entering the DB.
///
/// Cheap for typical sites — bounded by `cms_snippet` cardinality
/// (callouts, authors, FAQ entries; tens at most). When sites grow
/// past that, swap this for selective prefetch keyed on the slugs
/// the template actually references.
///
/// # Errors
/// Driver / query failures from the snippet lookup.
pub async fn prefetch_all(
    pool: &rustango::sql::Pool,
    locale_id: Option<i64>,
) -> Result<
    (
        std::collections::HashMap<String, String>,
        std::collections::HashMap<String, String>,
    ),
    rustango::sql::ExecError,
> {
    use rustango::sql::FetcherPool as _;
    let rows: Vec<Snippet> = Snippet::objects().fetch(pool).await?;
    // #409 — per-locale `body_markdown` overrides for the active
    // non-default locale (one batched fetch). Empty on the default
    // locale (`locale_id` None) so the canonical text renders.
    let translations = match locale_id {
        Some(lid) => {
            let ids: Vec<i64> = rows.iter().filter_map(|s| s.id.get().copied()).collect();
            crate::snippet_translation::fetch_for_snippets(pool, &ids, lid)
                .await
                .unwrap_or_default()
        }
        None => std::collections::HashMap::new(),
    };
    let mut wrapped: std::collections::HashMap<String, String> =
        std::collections::HashMap::with_capacity(rows.len());
    let mut inline: std::collections::HashMap<String, String> =
        std::collections::HashMap::with_capacity(rows.len());
    for s in rows {
        let body =
            s.id.get()
                .copied()
                .and_then(|id| translations.get(&id))
                .and_then(|m| m.get("body_markdown"))
                .map_or(s.body_markdown.as_str(), String::as_str);
        inline.insert(s.slug.clone(), crate::markdown::render(body));
        wrapped.insert(s.slug.clone(), render_with_wrapper(&s, body));
    }
    Ok((wrapped, inline))
}

/// Wrap rendered markdown in a class-bearing `<div>` so theme CSS
/// can target `.cms-snippet[data-type="callout"]` etc. `body_markdown`
/// is passed in (rather than read off `s`) so the caller can supply a
/// per-locale translation override.
fn render_with_wrapper(s: &Snippet, body_markdown: &str) -> String {
    let html = crate::markdown::render(body_markdown);
    format!(
        r#"<div class="cms-snippet" data-snippet-slug="{slug}" data-snippet-type="{ty}">{html}</div>"#,
        slug = html_escape(&s.slug),
        ty = html_escape(&s.type_name),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod cms_snippet_fn_tests {
    use super::*;
    use std::collections::HashMap;

    fn render(src: &str) -> String {
        let mut tera = tera::Tera::default();
        register_tera_function(&mut tera);
        tera.add_raw_template("t.html", src).unwrap();
        tera.render("t.html", &tera::Context::new()).unwrap()
    }

    /// `cms_snippet` reads the snippets the render installed.
    #[test]
    fn cms_snippet_returns_the_installed_snippet() {
        let _g = install(
            HashMap::from([("care".to_owned(), r#"<div class="cms-snippet"><p>Hand wash</p></div>"#.to_owned())]),
            HashMap::from([("care".to_owned(), "<p>Hand wash</p>".to_owned())]),
        );
        assert_eq!(
            render(r#"{{ cms_snippet(slug="care") }}"#),
            r#"<div class="cms-snippet"><p>Hand wash</p></div>"#,
            "safe: not escaped"
        );
        assert_eq!(render(r#"{{ cms_snippet(slug="care", body=true) }}"#), "<p>Hand wash</p>");
        assert_eq!(render(r#"[{{ cms_snippet(slug="nope") }}]"#), "[]");
    }

    #[test]
    fn the_guard_clears_the_snippets() {
        drop(install(HashMap::new(), HashMap::new()));
        CURRENT_SNIPPETS.with(|c| assert!(c.borrow().is_none()));
    }
}
