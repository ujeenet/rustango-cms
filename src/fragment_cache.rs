//! Page-keyed template-fragment caching (#429).
//!
//! ## Why there is no `{% rcmscache %}` Tera tag
//!
//! Wagtail's fragment caching is a Django **block tag**
//! (`{% wagtailcache %}…{% endwagtailcache %}`) that skips rendering
//! the body on a cache hit. **Tera cannot express this**: its grammar
//! is fixed — there is no API to register a custom block statement,
//! and the one block-shaped extension point, `{% filter foo %}…
//! {% endfilter %}`, renders the body *first* and then hands the
//! string to the filter, so it can never *skip* the expensive render.
//! A template-level cache tag is therefore impossible on this engine.
//!
//! ## What you do instead
//!
//! Cache the expensive *computation* in Rust — before it reaches the
//! template — with the framework's
//! [`rustango::cache_fragment::cached_render`] (re-exported here as
//! [`cached_render`]). That's exactly how [`crate::auto_menu`] caches
//! the menu tree. Build the key with [`page_fragment_key`] so it
//! **auto-invalidates** when the page republishes, and **bypass the
//! cache while previewing** (pass the live value instead of calling
//! `cached_render`) so editors always see their unsaved edits.
//!
//! ```ignore
//! use rustango_cms::fragment_cache::{cached_render, page_fragment_key};
//!
//! // In host/app code that holds a `cache: BoxedCache` (the same one
//! // wired into `PublicRouter::cached`) — e.g. a custom route or a
//! // context builder that has the cache in hand:
//! let body = if previewing {
//!     render_related_panel(pool, &page).await           // never cache a preview
//! } else {
//!     let key = page_fragment_key(&tenant.org.slug, &page, "related");
//!     cached_render(cache.as_ref(), &key, Some(ttl), || async {
//!         render_related_panel(pool, &page).await        // computed once per (page, publish)
//!     })
//!     .await
//! };
//! ```

pub use rustango::cache_fragment::cached_render;

use crate::page::Page;

/// Cache key for a page-scoped fragment. Encodes the page id + its
/// publish version stamp, so the key changes — and the old entry is
/// abandoned — every time the page republishes (no explicit purge
/// needed). `suffix` distinguishes fragments on the same page (e.g.
/// `"related"`, `"toc"`, `"sidebar"`).
///
/// `tenant_slug` is part of the key because every tenant numbers its
/// pages from 1: without it, tenants sharing one cache served each
/// other's fragments for the same page id (#681).
#[must_use]
pub fn page_fragment_key(tenant_slug: &str, page: &Page, suffix: &str) -> String {
    let id = page.id.get().copied().unwrap_or(0);
    // `last_published_at` moves only on public-visible republish;
    // fall back to `updated_at`, then 0 for unsaved rows.
    let stamp = page
        .last_published_at
        .or_else(|| page.updated_at.get().copied())
        .map_or(0, |ts| ts.timestamp());
    fragment_key(tenant_slug, id, stamp, suffix)
}

/// Pure key formatter — `cms:frag:<tenant>:<page_id>:<version_stamp>:<suffix>`.
fn fragment_key(tenant_slug: &str, page_id: i64, version_stamp: i64, suffix: &str) -> String {
    format!("cms:frag:{tenant_slug}:{page_id}:{version_stamp}:{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_shape_includes_id_stamp_suffix() {
        assert_eq!(
            fragment_key("acme", 42, 1717545600, "related"),
            "cms:frag:acme:42:1717545600:related"
        );
    }

    #[test]
    fn stamp_change_changes_the_key() {
        // A republish (new stamp) yields a distinct key, so the stale
        // entry is naturally abandoned rather than served.
        let before = fragment_key("acme", 7, 100, "toc");
        let after = fragment_key("acme", 7, 200, "toc");
        assert_ne!(before, after);
    }

    #[test]
    fn tenants_never_share_a_fragment_for_the_same_page_id() {
        assert_ne!(fragment_key("acme", 7, 100, "toc"), fragment_key("globex", 7, 100, "toc"));
    }

    #[test]
    fn suffix_isolates_fragments_on_one_page() {
        assert_ne!(fragment_key("acme", 7, 100, "toc"), fragment_key("acme", 7, 100, "sidebar"));
    }
}
