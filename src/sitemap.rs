//! `/sitemap.xml` for the public CMS — adopts [`rustango::sitemaps`].
//!
//! Walks every published, indexable [`Page`] in the current tenant,
//! turns each row into a [`SitemapEntry`] (using the row's
//! `updated_at` for `<lastmod>` and `sitemap_priority` for
//! `<priority>`), and renders the sitemaps.org `<urlset>` XML.
//!
//! Pages excluded:
//!
//! - `status != "published"` — drafts / scheduled / archived rows.
//! - `robots_index = false` — the per-page noindex toggle on
//!   [`Page::robots_index`].
//! - Locale variants (`locale_variant_of IS NOT NULL`) — the
//!   canonical row covers the URL; including both produces
//!   duplicate `<loc>` for the same path.
//!
//! ## Wire-up
//!
//! The CMS public [`router`] mounts `GET /sitemap.xml` automatically.
//! No extra handler registration is required.
//!
//! [`router`]: crate::router()
//! [`Page`]: crate::Page

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rustango::core::Column as _;
use rustango::extractors::Tenant;
use rustango::sql::FetcherPool as _;

use crate::locale_mode::LocaleMode;
use crate::page::Page;

/// Sitemap shard size — sitemaps.org caps each `<urlset>` at 50 000
/// URLs / 50 MB. Below the cap we emit one flat sitemap.xml; above,
/// we emit a `<sitemapindex>` pointing at numbered shards.
pub const SHARD_SIZE: usize = 50_000;

// ---- #441 — custom (non-page) sitemap sources --------------------

/// One non-page URL contributed to the sitemap by a registered
/// [`SitemapSource`] — e.g. a document, a routable sub-URL, or a
/// host-specific landing page. Appended after the page URLs in the
/// same sharded / indexed `/sitemap.xml`.
#[derive(Debug, Clone)]
pub struct SitemapEntry {
    /// Absolute URL (`https://host/path`). Sources build it from the
    /// `base` they're handed so scheme/host stay consistent with the
    /// page locs.
    pub loc: String,
    pub lastmod: Option<chrono::DateTime<chrono::Utc>>,
    /// `<changefreq>` hint (`daily`, `weekly`, …); emitted verbatim.
    pub changefreq: Option<String>,
    /// `<priority>` 0.0–1.0 (clamped on render).
    pub priority: Option<f32>,
}

impl SitemapEntry {
    #[must_use]
    pub fn new(loc: impl Into<String>) -> Self {
        Self {
            loc: loc.into(),
            lastmod: None,
            changefreq: None,
            priority: None,
        }
    }

    #[must_use]
    pub fn with_lastmod(mut self, ts: chrono::DateTime<chrono::Utc>) -> Self {
        self.lastmod = Some(ts);
        self
    }

    #[must_use]
    pub fn with_changefreq(mut self, cf: impl Into<String>) -> Self {
        self.changefreq = Some(cf.into());
        self
    }

    #[must_use]
    pub fn with_priority(mut self, p: f32) -> Self {
        self.priority = Some(p);
        self
    }

    /// Emit this entry as a `<url>…</url>` block (2-space indent).
    fn push_url(&self, xml: &mut String) {
        xml.push_str("  <url>\n");
        push_lp(xml, "loc", &self.loc, 4);
        if let Some(ts) = self.lastmod {
            push_lp(xml, "lastmod", &format_iso8601(ts), 4);
        }
        if let Some(cf) = &self.changefreq {
            push_lp(xml, "changefreq", cf, 4);
        }
        if let Some(p) = self.priority {
            push_lp(xml, "priority", &format!("{:.1}", p.clamp(0.0, 1.0)), 4);
        }
        xml.push_str("  </url>\n");
    }
}

/// A registered source of non-page sitemap URLs. Implement + register
/// with [`crate::register_sitemap_source!`]; the public `/sitemap.xml`
/// appends every source's entries after the page URLs, inside the same
/// shard / index accounting.
#[async_trait::async_trait]
pub trait SitemapSource: Send + Sync {
    /// The URLs this source contributes. `base` is the scheme+host
    /// prefix (no trailing slash) so entries match the page locs.
    async fn entries(&self, pool: &rustango::sql::Pool, base: &str) -> Vec<SitemapEntry>;
}

/// Inventory registration for a [`SitemapSource`]. Mirrors
/// [`crate::page_type::PageTypeHandlerRegistration`].
pub struct SitemapSourceRegistration {
    pub factory: fn() -> Box<dyn SitemapSource>,
}
inventory::collect!(SitemapSourceRegistration);

/// Gather every registered source's entries, in registration order.
pub async fn collect_custom_entries(pool: &rustango::sql::Pool, base: &str) -> Vec<SitemapEntry> {
    let mut out = Vec::new();
    for reg in inventory::iter::<SitemapSourceRegistration> {
        out.extend((reg.factory)().entries(pool, base).await);
    }
    out
}

/// Register a [`SitemapSource`]. Pair with a `Default` impl.
///
/// ```ignore
/// #[derive(Default)]
/// struct DocsSource;
/// #[async_trait::async_trait]
/// impl rustango_cms::sitemap::SitemapSource for DocsSource {
///     async fn entries(&self, _pool: &rustango::sql::Pool, base: &str)
///         -> Vec<rustango_cms::sitemap::SitemapEntry> {
///         vec![rustango_cms::sitemap::SitemapEntry::new(format!("{base}/docs"))
///             .with_changefreq("weekly")]
///     }
/// }
/// rustango_cms::register_sitemap_source!(DocsSource);
/// ```
#[macro_export]
macro_rules! register_sitemap_source {
    ($ty:ty) => {
        $crate::inventory::submit! {
            $crate::sitemap::SitemapSourceRegistration {
                factory: || ::std::boxed::Box::new(<$ty>::default()),
            }
        }
    };
}

/// `<lastmod>` source for a page: `last_published_at` (set only on
/// public-visible changes) preferred over `updated_at`.
fn page_lastmod(p: &Page) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Some(ts) = p.last_published_at {
        Some(ts)
    } else if let rustango::sql::Auto::Set(ts) = p.updated_at {
        Some(ts)
    } else {
        None
    }
}

/// Ids per `IN (…)` list. SQLite refuses a statement with more than
/// 32,766 binds, and a shard holds up to [`SHARD_SIZE`] pages.
const IN_CHUNK: usize = 10_000;

/// Page-extension loads [`collect_images_for_pages`] runs at once.
const HERO_LOADS: usize = 8;

/// Unified shard window `[start, end)` over `total` URLs for a 1-based
/// shard number, or `None` when the shard is out of range (`0`
/// included).
fn shard_window(total: usize, shard_one_based: usize) -> Option<(usize, usize)> {
    if shard_one_based == 0 {
        return None;
    }
    let start = (shard_one_based - 1) * SHARD_SIZE;
    if start >= total {
        return None;
    }
    Some((start, (start + SHARD_SIZE).min(total)))
}

/// The part of the tree a host's sitemap covers.
///
/// A host mapped to a page (Sites screen) serves only that page's
/// subtree, at paths relative to it, so its sitemap lists the same. A
/// host on the default root lists everything, as before.
#[derive(Default)]
struct SiteScope {
    /// The root's `url_path` without a trailing slash, `""` for the default root.
    prefix: String,
    /// The root's materialized tree path, `None` for the default root.
    tree_path: Option<String>,
}

impl SiteScope {
    async fn for_host(pool: &rustango::sql::Pool, headers: &HeaderMap) -> Self {
        // A failed lookup lists the whole tenant, like the page router's fallback.
        match crate::site::root_for_host(pool, crate::router::host_header(headers)).await {
            Ok(Some(root)) => {
                let prefix = root.url_path.trim_end_matches('/').to_owned();
                if prefix.is_empty() {
                    Self::default()
                } else {
                    Self { prefix, tree_path: Some(root.path) }
                }
            }
            _ => Self::default(),
        }
    }

    /// Narrow a page query to this site's subtree.
    fn narrow(
        &self,
        qs: rustango::query::QuerySet<Page>,
    ) -> rustango::query::QuerySet<Page> {
        match &self.tree_path {
            Some(tree) => qs.where_(Page::path.startswith(tree)),
            None => qs,
        }
    }

    /// Rewrite fetched rows to the paths this host serves them at.
    fn to_public(&self, rows: &mut [Page]) {
        if self.prefix.is_empty() {
            return;
        }
        for p in rows {
            p.url_path = crate::site::to_public_path(&self.prefix, &p.url_path);
        }
    }
}

/// `GET /sitemap.xml` — emits the sitemaps.org XML for the current
/// tenant's published, indexable pages.
///
/// Below [`SHARD_SIZE`] URLs, returns a flat `<urlset>` (the old
/// behavior). At/above, returns a `<sitemapindex>` pointing at
/// `/sitemap/1`, `/sitemap/2`, … (handled by
/// [`handle_sitemap_shard`]).
///
/// Includes `xhtml:link rel="alternate" hreflang=…` clustering for
/// locale variants + an `<image:image>` extension when the page-type
/// handler surfaces a hero media id.
// The public router mounts this automatically and supplies `PublicState`
// (a crate-internal type); consumers never call it directly, so exposing
// the private state in the signature is intentional.
#[allow(private_interfaces)]
pub async fn handle_sitemap(
    axum::extract::State(state): axum::extract::State<crate::router::PublicState>,
    t: Tenant,
    headers: HeaderMap,
) -> Response {
    let base = base_url(&headers);
    let mode = state.locale_mode();

    // Fetch every public+indexable row — both canonicals AND locale
    // variants — so we can emit one `<url>` per URL plus hreflang
    // annotations grouping the variants. #556 — archived pages stay in
    // the sitemap (no link rot) at a capped priority; `expired` (takedown)
    // is excluded.
    let scope = SiteScope::for_host(t.pool(), &headers).await;
    let listed = scope.narrow(
        Page::objects()
            .where_(Page::status.is_in(crate::page::PageStatus::public_strings()))
            .where_(Page::robots_index.eq(true)),
    );
    // #644 — a view-restricted page is not listed: crawlers are anonymous.
    let mut rows: Vec<Page> = match crate::view_restriction::only_anonymous_visible(t.pool(), listed)
        .await
        .order_by(&[("path", false)])
        .fetch(t.pool())
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "sitemap: page fetch failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "sitemap unavailable").into_response();
        }
    };
    // Error pages (404/500 …) serve as substitutes for failures, not
    // destinations — keep them out of the sitemap even when published.
    if let Some(tid) = crate::error_pages::error_page_type_id(t.pool()).await {
        rows.retain(|p| p.page_type_id != tid);
    }
    scope.to_public(&mut rows);

    // #441 — non-page URLs from registered sources, appended after the
    // pages in the same (sharded, indexed) sitemap.
    let custom = collect_custom_entries(t.pool(), &base).await;

    // #187 — split when total URLs exceed the shard cap. Return an
    // index pointing at numbered shards instead of one giant urlset.
    if rows.len() + custom.len() > SHARD_SIZE {
        let mut lastmods: Vec<_> = rows.iter().map(page_lastmod).collect();
        lastmods.extend(custom.iter().map(|e| e.lastmod));
        return build_sitemap_index(&base, &lastmods);
    }

    let xml = build_urlset(t.pool(), &base, &rows, &custom, mode).await;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

/// `GET /sitemap/{n}` — one shard of [`SHARD_SIZE`] URLs from the
/// full page list, in `path` order. Returns 404 when `n` is past the
/// last shard.
#[allow(private_interfaces)]
pub async fn handle_sitemap_shard(
    axum::extract::State(state): axum::extract::State<crate::router::PublicState>,
    t: Tenant,
    headers: HeaderMap,
    axum::extract::Path(shard_one_based): axum::extract::Path<usize>,
) -> Response {
    let base = base_url(&headers);
    let mode = state.locale_mode();
    // Same filter as `handle_sitemap`, so shard windows line up with the
    // index. Counted, then only this shard's window fetched (#691): a
    // crawl of K shards used to fetch every public page K times.
    use rustango::sql::CounterPool as _;
    let error_tid = crate::error_pages::error_page_type_id(t.pool()).await;
    let scope = SiteScope::for_host(t.pool(), &headers).await;
    let listed = || async {
        let qs = scope.narrow(
            Page::objects()
                .where_(Page::status.is_in(crate::page::PageStatus::public_strings()))
                .where_(Page::robots_index.eq(true)),
        );
        let qs = match error_tid {
            Some(tid) => qs.where_(Page::page_type_id.ne(tid)),
            None => qs,
        };
        crate::view_restriction::only_anonymous_visible(t.pool(), qs).await
    };
    let p = match listed().await.count(t.pool()).await {
        Ok(n) => usize::try_from(n).unwrap_or(0),
        Err(e) => {
            tracing::error!(error = %e, "sitemap shard: page count failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "sitemap unavailable").into_response();
        }
    };
    // #441 — pages occupy the first `p` slots of the unified URL list,
    // custom-source entries the tail; shard windows span both.
    let custom = collect_custom_entries(t.pool(), &base).await;
    let total = p + custom.len();
    let Some((start, end)) = shard_window(total, shard_one_based) else {
        return (StatusCode::NOT_FOUND, "shard out of range").into_response();
    };
    let mut page_slice: Vec<Page> = if start < p {
        match listed()
            .await
            .order_by(&[("path", false), ("id", false)])
            .offset(start as i64)
            .limit((end.min(p) - start) as i64)
            .fetch(t.pool())
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!(error = %e, "sitemap shard: page fetch failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, "sitemap unavailable").into_response();
            }
        }
    } else {
        Vec::new()
    };
    scope.to_public(&mut page_slice);
    let custom_slice: Vec<SitemapEntry> =
        custom[start.saturating_sub(p)..end.saturating_sub(p)].to_vec();
    // Re-use the same renderer the flat path uses. hreflang grouping is
    // scoped to this shard's rows (so a translation in a different
    // shard won't emit a cross-shard link — acceptable, since the
    // sitemaps.org spec doesn't require cross-shard
    // grouping).
    let xml = build_urlset(t.pool(), &base, &page_slice, &custom_slice, mode).await;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

/// Build a `<sitemapindex>` over the unified URL list (pages then
/// custom-source entries), pointing at /sitemap/1, /sitemap/2, …
/// `lastmods[i]` is the `<lastmod>` of the i-th URL; each shard's
/// `<lastmod>` is the max across its window.
fn build_sitemap_index(base: &str, lastmods: &[Option<chrono::DateTime<chrono::Utc>>]) -> Response {
    let total = lastmods.len();
    let shard_count = total.div_ceil(SHARD_SIZE);
    let mut xml = String::with_capacity(256 + shard_count * 128);
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<sitemapindex xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    for shard in 0..shard_count {
        let start = shard * SHARD_SIZE;
        let end = (start + SHARD_SIZE).min(total);
        let max_updated = lastmods[start..end].iter().flatten().copied().max();
        xml.push_str("  <sitemap>\n");
        push_lp(&mut xml, "loc", &format!("{base}/sitemap/{}", shard + 1), 4);
        if let Some(ts) = max_updated {
            push_lp(&mut xml, "lastmod", &format_iso8601(ts), 4);
        }
        xml.push_str("  </sitemap>\n");
    }
    xml.push_str("</sitemapindex>\n");
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

/// Render one `<urlset>` for the given pages. Extracted so both the
/// flat `/sitemap.xml` path AND the per-shard `/sitemap/{n}`
/// handler can call into the same code.
async fn build_urlset(
    pool: &rustango::sql::Pool,
    base: &str,
    rows: &[Page],
    custom: &[SitemapEntry],
    mode: LocaleMode,
) -> String {
    let mut group_key: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for p in rows {
        let Some(pid) = p.id.get().copied() else {
            continue;
        };
        let key = p.locale_variant_of.or(p.alias_of).unwrap_or(pid);
        group_key.insert(pid, key);
    }
    let mut groups: std::collections::HashMap<i64, Vec<&Page>> = std::collections::HashMap::new();
    for p in rows {
        let Some(pid) = p.id.get().copied() else {
            continue;
        };
        let key = *group_key.get(&pid).unwrap_or(&pid);
        groups.entry(key).or_default().push(p);
    }
    let locales: Vec<crate::locale::Locale> = crate::locale::Locale::objects()
        .where_(crate::locale::Locale::active.eq(true))
        .fetch(pool)
        .await
        .unwrap_or_default();
    let default_locale_code = locales
        .iter()
        .find(|l| l.is_default)
        .map(|l| l.code.clone())
        .unwrap_or_else(|| "en".to_owned());
    // Resolve every page's `<image:image>` rows up front in a bounded
    // number of queries (#521). This replaced a per-page N+1 (one
    // PageType + ReferenceIndex + Media lookup *per page*) that, under
    // concurrency, pinned the tenant connection pool long enough to
    // exhaust it. The batched map feeds `render_images` per page below.
    let images_by_page = collect_images_for_pages(pool, rows, base).await;
    // #i18n — override-model localization (one canonical page + per-field
    // `cms_translation` overrides, the current model) has no per-locale
    // sibling pages, so a page can still be served in every active locale
    // via its locale-prefixed URL. When ≥1 active non-default locale
    // exists ("languages set"), emit one `<url>` per locale for each page
    // with a full bidirectional hreflang cluster. Legacy `locale_variant_of`
    // pages keep their existing sibling-grouped behavior below.
    let non_default: Vec<&crate::locale::Locale> =
        locales.iter().filter(|l| !l.is_default).collect();
    let langs_set = !non_default.is_empty();
    let mut xml = String::with_capacity(512 + rows.len() * 256);
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\"\n");
    xml.push_str("        xmlns:xhtml=\"http://www.w3.org/1999/xhtml\"\n");
    xml.push_str("        xmlns:image=\"http://www.google.com/schemas/sitemap-image/1.1\">\n");
    for p in rows {
        let Some(pid) = p.id.get().copied() else {
            continue;
        };
        // #251 — prefer `last_published_at` (set only when public content
        // changed) over `updated_at`; see [`page_lastmod`]. Computed once
        // and shared across a page's per-locale `<url>` entries.
        let lastmod = page_lastmod(p).map(format_iso8601);
        // #556 — archived pages stay indexed but rank below current
        // content: cap their priority at 0.3 (crawlers keep them, prefer
        // the live pages). Non-archived pages keep their configured value.
        let priority_f = f64::from(p.sitemap_priority).clamp(0.0, 1.0);
        let priority_f = if p.status == crate::page::PageStatus::Archived.as_str() {
            priority_f.min(0.3)
        } else {
            priority_f
        };
        let priority = format!("{priority_f:.1}");
        let images_xml = render_images(images_by_page.get(&pid).map(Vec::as_slice).unwrap_or(&[]));
        let group = group_key.get(&pid).copied().unwrap_or(pid);
        let has_variants = groups.get(&group).is_some_and(|g| g.len() > 1);

        if langs_set && !has_variants && p.locale_variant_of.is_none() && p.alias_of.is_none() {
            // Override-model i18n: emit the default-locale URL plus one URL
            // per active non-default locale, each carrying the same full
            // hreflang cluster (every locale + x-default → the default URL),
            // so search engines discover + bidirectionally link all variants.
            let default_url = url_for(base, p);
            let mut links = hreflang_link(&default_locale_code, &default_url);
            for l in &non_default {
                links.push_str(&hreflang_link(&l.code, &locale_url(base, p, &l.code, mode)));
            }
            links.push_str(&hreflang_link("x-default", &default_url));
            emit_url(
                &mut xml,
                &default_url,
                lastmod.as_deref(),
                &priority,
                &links,
                &images_xml,
            );
            for l in &non_default {
                let loc = locale_url(base, p, &l.code, mode);
                emit_url(
                    &mut xml,
                    &loc,
                    lastmod.as_deref(),
                    &priority,
                    &links,
                    &images_xml,
                );
            }
        } else {
            // Single `<url>` — with legacy `locale_variant_of` sibling
            // alternates when the page belongs to a variant group.
            let url = url_for(base, p);
            let mut links = String::new();
            if let Some(group_pages) = groups.get(&group) {
                if group_pages.len() > 1 {
                    for member in group_pages {
                        // Canonical (no variant parent) → default locale.
                        // A locale-variant page carries its language as the
                        // first URL segment (`/de/…`, `/fr/…`) for standalone
                        // per-locale trees, so derive the hreflang from that
                        // rather than emitting an unhelpful `und`.
                        let code = if member.locale_variant_of.is_some() {
                            variant_hreflang(&member.url_path)
                        } else {
                            default_locale_code.clone()
                        };
                        links.push_str(&hreflang_link(&code, &url_for(base, member)));
                    }
                    if let Some(canonical) =
                        group_pages.iter().find(|m| m.locale_variant_of.is_none())
                    {
                        links.push_str(&hreflang_link("x-default", &url_for(base, canonical)));
                    }
                }
            }
            emit_url(
                &mut xml,
                &url,
                lastmod.as_deref(),
                &priority,
                &links,
                &images_xml,
            );
        }
        // (images + `</url>` are emitted inside `emit_url` above, using the
        // batched `images_by_page` map.)
    }
    // #441 — non-page entries from registered sources.
    for entry in custom {
        entry.push_url(&mut xml);
    }
    xml.push_str("</urlset>\n");
    xml
}

/// Build the canonical URL for a page (`base + url_path`).
fn url_for(base: &str, p: &Page) -> String {
    let path = if p.url_path.is_empty() {
        "/".to_owned()
    } else {
        p.url_path.clone()
    };
    format!("{base}{path}")
}

/// Build the URL that serves page `p` in locale `code`, per the router's
/// [`LocaleMode`]: path modes prefix `/{code}` (`…/fr/blog`), query mode
/// appends `?lang={code}` (`…/blog?lang=fr`). Mirrors
/// [`crate::language_switcher::build`] so sitemap links match the switcher.
fn locale_url(base: &str, p: &Page, code: &str, mode: LocaleMode) -> String {
    let path = if p.url_path.is_empty() {
        "/"
    } else {
        p.url_path.as_str()
    };
    if mode.reads_path() {
        format!("{base}/{code}{path}")
    } else {
        format!("{base}{path}?lang={code}")
    }
}

/// Derive the hreflang code for a standalone locale-variant page from the
/// first segment of its `url_path` (`/de/guides/x` → `de`). Accepts a bare
/// two-letter language code or a `ll-RR` language-region tag; anything else
/// (e.g. a version token like `0.51`) falls back to `und`. Used for per-locale
/// page trees whose language is expressed in the URL rather than a `cms_locale`
/// association.
fn variant_hreflang(url_path: &str) -> String {
    let seg = url_path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("");
    let two_alpha = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_lowercase());
    let is_code = two_alpha(seg)
        || (seg.len() == 5
            && seg.as_bytes()[2] == b'-'
            && two_alpha(&seg[..2])
            && two_alpha(&seg[3..]));
    if is_code {
        seg.to_owned()
    } else {
        "und".to_owned()
    }
}

/// One `<xhtml:link rel="alternate" hreflang=…>` line (4-space indent).
fn hreflang_link(code: &str, href: &str) -> String {
    format!(
        "    <xhtml:link rel=\"alternate\" hreflang=\"{}\" href=\"{}\"/>\n",
        xml_escape(code),
        xml_escape(href)
    )
}

/// Emit one `<url>` block: `<loc>`, optional `<lastmod>`, `<priority>`,
/// then the pre-rendered hreflang `links` + `images_xml` (both already
/// indented). Shared by the per-locale and single-URL paths.
fn emit_url(
    xml: &mut String,
    loc: &str,
    lastmod: Option<&str>,
    priority: &str,
    links: &str,
    images_xml: &str,
) {
    xml.push_str("  <url>\n");
    push_lp(xml, "loc", loc, 4);
    if let Some(ts) = lastmod {
        push_lp(xml, "lastmod", ts, 4);
    }
    push_lp(xml, "priority", priority, 4);
    xml.push_str(links);
    xml.push_str(images_xml);
    xml.push_str("  </url>\n");
}

/// Pre-render a page's `<image:image>` blocks (6-space indent) so they can
/// be reused across per-locale `<url>` entries without re-querying.
fn render_images(images: &[ImageRow]) -> String {
    let mut s = String::new();
    for img in images {
        s.push_str("    <image:image>\n");
        push_lp(&mut s, "image:loc", &img.loc, 6);
        if let Some(title) = &img.title {
            push_lp(&mut s, "image:title", title, 6);
        }
        if let Some(caption) = &img.caption {
            push_lp(&mut s, "image:caption", caption, 6);
        }
        s.push_str("    </image:image>\n");
    }
    s
}

/// One `<image:image>` row.
struct ImageRow {
    loc: String,
    title: Option<String>,
    caption: Option<String>,
}

/// Resolve every page's `<image:image>` rows in a bounded number of
/// queries — one `PageType` fetch, one `ReferenceIndex` fetch and one
/// `Media` fetch for the whole page set — instead of the former
/// per-page N+1 (which emitted those three queries *per page*, so a
/// large sitemap fired thousands and, under concurrency, held the
/// tenant pool long enough to deadlock it).
///
/// Hero media still come from the page-type handler's `load_extension`
/// (v0 convention: a `hero_media_id` key in the extension JSON), which
/// has no batch form: one query per page of a type with an extension
/// table. Those run [`HERO_LOADS`] at a time rather than one after
/// another. Id lists are sent in [`IN_CHUNK`]-sized pieces, under
/// SQLite's bind limit. Returns a `page_id -> rows` map; pages with no
/// images are absent.
async fn collect_images_for_pages(
    pool: &rustango::sql::Pool,
    pages: &[Page],
    base: &str,
) -> std::collections::HashMap<i64, Vec<ImageRow>> {
    use crate::page_type::find_handler;
    use crate::page_type_model::PageType;
    use crate::reference_index::ReferenceIndex;

    let mut out: std::collections::HashMap<i64, Vec<ImageRow>> = std::collections::HashMap::new();
    let page_ids: Vec<i64> = pages
        .iter()
        .filter_map(|p| p.id.get().copied())
        .filter(|&id| id != 0)
        .collect();
    if page_ids.is_empty() {
        return out;
    }

    // 1 query — page types keyed by id, so each page resolves its
    // handler from memory rather than via a per-page lookup.
    let mut type_ids: Vec<i64> = pages.iter().map(|p| p.page_type_id).collect();
    type_ids.sort_unstable();
    type_ids.dedup();
    let type_name_by_id: std::collections::HashMap<i64, String> = PageType::objects()
        .where_(PageType::id.is_in(type_ids))
        .fetch(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|pt| pt.id.get().copied().map(|id| (id, pt.type_name)))
        .collect();

    // 1 query — every page->media reference, grouped by page. Ordered
    // by `to_id` to preserve the former per-page query's ordering.
    let mut indexed_by_page: std::collections::HashMap<i64, Vec<i64>> =
        std::collections::HashMap::new();
    for chunk in page_ids.chunks(IN_CHUNK) {
        let refs = ReferenceIndex::objects()
            .where_(ReferenceIndex::from_kind.eq(crate::reference_index::KIND_PAGE))
            .where_(ReferenceIndex::from_id.is_in(chunk.iter().copied()))
            .where_(ReferenceIndex::to_kind.eq(crate::reference_index::KIND_MEDIA))
            .order_by(&[("to_id", false)])
            .fetch(pool)
            .await
            .unwrap_or_default();
        for r in refs {
            indexed_by_page.entry(r.from_id).or_default().push(r.to_id);
        }
    }

    // Per page: hero id from the handler's extension (no-op for types
    // without one), then the indexed references. Accumulate the union
    // of media ids for a single bulk fetch below.
    let mut heroes: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    {
        let permits = std::sync::Arc::new(rustango::__private_runtime::tokio::sync::Semaphore::new(HERO_LOADS));
        let mut loads = rustango::__private_runtime::tokio::task::JoinSet::new();
        for p in pages {
            let Some(pid) = p.id.get().copied().filter(|&id| id != 0) else {
                continue;
            };
            let Some(handler) = type_name_by_id
                .get(&p.page_type_id)
                .and_then(|name| find_handler(name))
            else {
                continue;
            };
            let (pool, permits) = (pool.clone(), std::sync::Arc::clone(&permits));
            loads.spawn(async move {
                let _permit = permits.acquire_owned().await.ok()?;
                let hero = handler
                    .load_extension(&pool, pid)
                    .await
                    .ok()?
                    .get("hero_media_id")
                    .and_then(serde_json::Value::as_i64)?;
                Some((pid, hero))
            });
        }
        while let Some(done) = loads.join_next().await {
            if let Ok(Some((pid, hero))) = done {
                heroes.insert(pid, hero);
            }
        }
    }

    let mut ids_by_page: Vec<(i64, Vec<i64>)> = Vec::with_capacity(pages.len());
    let mut all_media: Vec<i64> = Vec::new();
    for p in pages {
        let Some(pid) = p.id.get().copied().filter(|&id| id != 0) else {
            continue;
        };
        let hero = heroes.get(&pid).copied();
        let indexed = indexed_by_page.remove(&pid).unwrap_or_default();
        let ids = collect_media_ids(hero, indexed);
        if !ids.is_empty() {
            all_media.extend(ids.iter().copied());
            ids_by_page.push((pid, ids));
        }
    }
    if all_media.is_empty() {
        return out;
    }

    // 1 query — all referenced media at once.
    all_media.sort_unstable();
    all_media.dedup();
    let mut by_id: std::collections::HashMap<i64, crate::media::Media> =
        std::collections::HashMap::new();
    for chunk in all_media.chunks(IN_CHUNK) {
        let found = crate::media::Media::objects()
            .where_(crate::media::Media::id.is_in(chunk.iter().copied()))
            .fetch(pool)
            .await
            .unwrap_or_default();
        by_id.extend(found.into_iter().filter_map(|m| m.id.get().copied().map(|id| (id, m))));
    }

    // Emit per page in id-collection order (hero first), images only.
    for (pid, ids) in ids_by_page {
        let rows: Vec<ImageRow> = ids
            .iter()
            .filter_map(|id| {
                let m = by_id.get(id)?;
                if m.kind != "image" {
                    return None;
                }
                Some(ImageRow {
                    loc: format!("{base}/__media__/max-2000x2000/{id}"),
                    title: Some(m.title.clone()).filter(|s| !s.is_empty()),
                    caption: Some(m.alt_text.clone()).filter(|s| !s.is_empty()),
                })
            })
            .collect();
        if !rows.is_empty() {
            out.insert(pid, rows);
        }
    }
    out
}

/// Order-preserving dedup of a page's media ids — hero first, then the
/// indexed stream/body references; drops non-positive ids.
fn collect_media_ids(hero: Option<i64>, indexed: Vec<i64>) -> Vec<i64> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for id in hero.into_iter().chain(indexed) {
        if id > 0 && seen.insert(id) {
            out.push(id);
        }
    }
    out
}

#[cfg(test)]
mod image_id_tests {
    use super::collect_media_ids;

    #[test]
    fn hero_first_then_indexed_deduped() {
        assert_eq!(collect_media_ids(Some(5), vec![3, 5, 7, 3]), vec![5, 3, 7]);
    }

    #[test]
    fn no_hero_keeps_indexed_order() {
        assert_eq!(collect_media_ids(None, vec![9, 2, 9]), vec![9, 2]);
    }

    #[test]
    fn drops_nonpositive_and_empty() {
        assert_eq!(collect_media_ids(Some(0), vec![-1, 4]), vec![4]);
        assert!(collect_media_ids(None, vec![]).is_empty());
    }
}

#[cfg(test)]
mod custom_source_tests {
    use super::{shard_window, SitemapEntry, SitemapSource, SitemapSourceRegistration, SHARD_SIZE};

    #[test]
    fn entry_emits_all_fields_clamped_and_escaped() {
        let ts = chrono::DateTime::parse_from_rfc3339("2026-06-04T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let e = SitemapEntry::new("https://x.test/a?b=1&c=2")
            .with_lastmod(ts)
            .with_changefreq("weekly")
            .with_priority(1.5); // out of range → clamps to 1.0
        let mut xml = String::new();
        e.push_url(&mut xml);
        assert!(
            xml.contains("<loc>https://x.test/a?b=1&amp;c=2</loc>"),
            "{xml}"
        );
        assert!(
            xml.contains("<lastmod>2026-06-04T12:00:00Z</lastmod>"),
            "{xml}"
        );
        assert!(xml.contains("<changefreq>weekly</changefreq>"), "{xml}");
        assert!(xml.contains("<priority>1.0</priority>"), "{xml}");
    }

    #[test]
    fn minimal_entry_emits_only_loc() {
        let mut xml = String::new();
        SitemapEntry::new("https://x.test/").push_url(&mut xml);
        assert!(xml.contains("<loc>https://x.test/</loc>"), "{xml}");
        assert!(!xml.contains("<lastmod>"), "{xml}");
        assert!(!xml.contains("<changefreq>"), "{xml}");
        assert!(!xml.contains("<priority>"), "{xml}");
    }

    #[test]
    fn shard_window_spans_pages_and_custom() {
        // Out-of-range shards (0 and past the end) → None.
        assert_eq!(shard_window(10, 0), None);
        assert_eq!(shard_window(10, 2), None);
        // Single small shard covers everything.
        assert_eq!(shard_window(10, 1), Some((0, 10)));
        // Cross-shard windows at the cap boundary.
        assert_eq!(shard_window(SHARD_SIZE + 5, 1), Some((0, SHARD_SIZE)));
        assert_eq!(
            shard_window(SHARD_SIZE + 5, 2),
            Some((SHARD_SIZE, SHARD_SIZE + 5))
        );
        assert_eq!(shard_window(SHARD_SIZE + 5, 3), None);
        assert_eq!(shard_window(0, 1), None);
    }

    // A registered test source proves the inventory + macro wiring.
    #[derive(Default)]
    struct TestSource;
    #[async_trait::async_trait]
    impl SitemapSource for TestSource {
        async fn entries(&self, _pool: &rustango::sql::Pool, base: &str) -> Vec<SitemapEntry> {
            vec![SitemapEntry::new(format!("{base}/__test_custom__"))]
        }
    }
    crate::register_sitemap_source!(TestSource);

    #[test]
    fn registry_collects_registered_sources() {
        let count = inventory::iter::<SitemapSourceRegistration>
            .into_iter()
            .count();
        assert!(count >= 1, "expected the test source to be registered");
    }
}

/// Indent-prefixed `<tag>text</tag>\n` writer with XML escaping on
/// the value.
fn push_lp(buf: &mut String, tag: &str, value: &str, indent: usize) {
    use std::fmt::Write as _;
    let pad = " ".repeat(indent);
    let _ = writeln!(buf, "{pad}<{tag}>{}</{tag}>", xml_escape(value));
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn format_iso8601(ts: chrono::DateTime<chrono::Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Build the scheme+host prefix (no trailing slash) for the absolute
/// URLs in the sitemap, the feeds and robots.txt. Scheme defaults to
/// `https`, downgrades to `http` only when the request was clearly
/// plaintext (`X-Forwarded-Proto: http`, or no proxy header AND a
/// localhost-shape host).
///
/// The host is the `Host` header — the one the tenant was resolved from.
/// `X-Forwarded-Host` is not read: nothing here knows whether a
/// trusted proxy set it, so a client could get tenant A's sitemap with
/// every URL on a host of its choosing, and a shared cache would keep it.
pub(crate) fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_owned())
        .unwrap_or_else(|| "localhost".to_owned());
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_ascii_lowercase())
        .filter(|s| s == "http" || s == "https")
        .unwrap_or_else(|| {
            if is_localhost_shape(&host) {
                "http".to_owned()
            } else {
                "https".to_owned()
            }
        });
    format!("{scheme}://{host}")
}

fn is_localhost_shape(host: &str) -> bool {
    let bare = host.split(':').next().unwrap_or(host);
    bare == "localhost"
        || bare.ends_with(".localtest.me")
        || bare.ends_with(".localhost")
        || bare == "127.0.0.1"
        || bare == "::1"
}

#[cfg(test)]
mod base_url_tests {
    use axum::http::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    /// The host is the one the tenant was resolved from.
    #[test]
    fn a_forwarded_host_cannot_move_the_urls() {
        let h = headers(&[("host", "victim.example"), ("x-forwarded-host", "evil.example")]);
        assert_eq!(super::base_url(&h), "https://victim.example");
    }

    #[test]
    fn only_a_real_scheme_is_taken_from_the_proxy() {
        let h = headers(&[("host", "site.example"), ("x-forwarded-proto", "http")]);
        assert_eq!(super::base_url(&h), "http://site.example");
        let h = headers(&[("host", "site.example"), ("x-forwarded-proto", "javascript")]);
        assert_eq!(super::base_url(&h), "https://site.example");
        let h = headers(&[("host", "demo.localhost:8080")]);
        assert_eq!(super::base_url(&h), "http://demo.localhost:8080");
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod site_scope_tests {
    use super::*;
    use rustango::core::Model as _;
    use rustango::sql::FetcherPool as _;

    async fn pool_with_tree() -> rustango::sql::Pool {
        let pool = rustango::sql::Pool::connect("sqlite::memory:").await.expect("pool");
        for schema in [
            &crate::page_type_model::PageType::SCHEMA,
            &crate::media::Media::SCHEMA,
            &crate::theme::Theme::SCHEMA,
            &Page::SCHEMA,
            &crate::site::Site::SCHEMA,
        ] {
            let sql = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
                pool.dialect(),
                schema,
            );
            for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                rustango::sql::raw_execute_pool(&pool, stmt, Vec::new()).await.expect("ddl");
            }
        }
        for sql in [
            "INSERT INTO cms_page_type (id, app_label, type_name, verbose_name, default_template, is_creatable, allowed_parent_types) \
             VALUES (1, 'cms', 'P', 'P', 'p.html', 1, '[]')",
            "INSERT INTO cms_page (id, page_type_id, title, slug, path, url_path, depth, sort_order, status, seo_title, seo_description) VALUES \
             (1, 1, 'Shop', 'shop', '0001/', '/shop', 1, 0, 'published', '', ''), \
             (2, 1, 'Catalog', 'catalog', '0001/0001/', '/shop/catalog', 2, 0, 'published', '', ''), \
             (3, 1, 'Blog', 'blog', '0002/', '/blog', 1, 0, 'published', '', '')",
            "INSERT INTO cms_site (hostname, root_page_id) VALUES ('shop.example', 1)",
        ] {
            rustango::sql::raw_execute_pool(&pool, sql, Vec::new()).await.expect("row");
        }
        pool
    }

    async fn listed(pool: &rustango::sql::Pool, host: &'static str) -> Vec<String> {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().unwrap());
        let scope = SiteScope::for_host(pool, &headers).await;
        let mut rows: Vec<Page> = scope
            .narrow(Page::objects())
            .order_by(&[("path", false)])
            .fetch(pool)
            .await
            .expect("fetch");
        scope.to_public(&mut rows);
        rows.into_iter().map(|p| p.url_path).collect()
    }

    /// A mapped host lists only its subtree, at the paths it serves.
    #[tokio::test]
    async fn a_mapped_host_lists_its_own_subtree_relative_to_its_root() {
        let pool = pool_with_tree().await;
        assert_eq!(listed(&pool, "shop.example:8000").await, ["/", "/catalog"]);
        // An unmapped host still lists the whole tenant, unchanged.
        assert_eq!(listed(&pool, "other.example").await, ["/shop", "/shop/catalog", "/blog"]);
    }
}
