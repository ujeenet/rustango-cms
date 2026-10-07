//! Per-page-type RSS 2.0 + Atom 1.0 feeds —
//! "syndication on demand", powered by [`rustango::syndication`].
//!
//! ## Opt-in
//!
//! A [`crate::PageTypeHandler`] joins the feed surface by overriding
//! [`crate::PageTypeHandler::feed_kind`] to return `Some("slug")`.
//! The slug is the URL path segment under `/feed/<slug>/...`:
//!
//! ```ignore
//! impl PageTypeHandler for ArticlePage {
//!     // …
//!     fn feed_kind(&self) -> Option<&'static str> { Some("articles") }
//! }
//! ```
//!
//! Then `/feed/articles/rss.xml` and `/feed/articles/atom.xml`
//! enumerate every published, indexable [`crate::Page`] whose
//! `page_type_id` resolves to that handler, newest first.
//!
//! ## Content mapping
//!
//! | Feed field         | CMS source                                              |
//! | ------------------ | ------------------------------------------------------- |
//! | `<title>`          | `page.title`                                            |
//! | `<link>` / `<id>`  | `https?://<host><page.url_path>` (Host-derived; see [`crate::sitemap`] for the scheme/host logic) |
//! | `<description>`    | `page.seo_description` (falls back to empty)            |
//! | `<pubDate>` / `<updated>` | `page.published_at` (falls back to `updated_at`)  |
//!
//! Excludes pages with `robots_index = false` (mirroring the
//! sitemap exclusion) and locale variants (canonical row only).
//!
//! ## Feed metadata
//!
//! `<channel>`-level title is derived from `tenant.org.display_name`;
//! description is a generic "Latest `<handler verbose_name>`" line.
//! Neither is overridable per tenant: the feed does not read
//! `SiteSetting` rows.

use axum::extract::Path;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rustango::core::Column as _;
use rustango::extractors::Tenant;
use rustango::sql::FetcherPool as _;
use rustango::syndication::{render_atom, render_rss, Feed, FeedItem};

use crate::page::Page;

/// Most items a feed carries. Readers poll feeds on a timer, so an
/// uncapped feed made every poll load and serialise the type's whole
/// history; syndication convention is the latest 10-50.
pub const FEED_MAX_ITEMS: i64 = 50;

/// The newest published, indexable, non-variant pages of `type_id` an
/// anonymous reader may see, at most [`FEED_MAX_ITEMS`].
async fn feed_pages(
    pool: &rustango::sql::Pool,
    type_id: i64,
) -> Result<Vec<Page>, rustango::sql::ExecError> {
    let qs = Page::objects()
        .where_(Page::page_type_id.eq(type_id))
        .where_(Page::status.eq("published"))
        .where_(Page::robots_index.eq(true))
        .where_(Page::locale_variant_of.is_null());
    crate::view_restriction::only_anonymous_visible(pool, qs)
        .await
        .order_by(&[("published_at", true)])
        .limit(FEED_MAX_ITEMS)
        .fetch(pool)
        .await
}
use crate::page_type::registered_handlers;
use crate::page_type_model::PageType;

/// `GET /feed/{kind}/rss.xml`
pub async fn handle_feed_rss(t: Tenant, headers: HeaderMap, Path(kind): Path<String>) -> Response {
    match build_feed(&t, &headers, &kind).await {
        Ok(feed) => xml_response(render_rss(&feed), "application/rss+xml; charset=utf-8"),
        Err(FeedError::NotFound) => not_found(&kind),
        Err(FeedError::Db(e)) => {
            tracing::error!(error = %e, kind = %kind, "feed: db error");
            (StatusCode::INTERNAL_SERVER_ERROR, "feed unavailable").into_response()
        }
    }
}

/// `GET /feed/{kind}/atom.xml`
pub async fn handle_feed_atom(t: Tenant, headers: HeaderMap, Path(kind): Path<String>) -> Response {
    match build_feed(&t, &headers, &kind).await {
        Ok(feed) => xml_response(render_atom(&feed), "application/atom+xml; charset=utf-8"),
        Err(FeedError::NotFound) => not_found(&kind),
        Err(FeedError::Db(e)) => {
            tracing::error!(error = %e, kind = %kind, "feed: db error");
            (StatusCode::INTERNAL_SERVER_ERROR, "feed unavailable").into_response()
        }
    }
}

#[derive(Debug)]
enum FeedError {
    /// No registered `PageTypeHandler` claims this kind slug.
    NotFound,
    Db(rustango::sql::ExecError),
}

impl From<rustango::sql::ExecError> for FeedError {
    fn from(e: rustango::sql::ExecError) -> Self {
        Self::Db(e)
    }
}

async fn build_feed(t: &Tenant, headers: &HeaderMap, kind: &str) -> Result<Feed, FeedError> {
    // Resolve the slug → handler. Linear scan over the inventory —
    // registration count is small (typically <20).
    let handler = registered_handlers()
        .find(|h| h.feed_kind() == Some(kind))
        .ok_or(FeedError::NotFound)?;

    // Look up the matching `cms_page_type` row to get its id.
    let type_name = handler.type_name();
    let mut types: Vec<PageType> = PageType::objects()
        .where_(PageType::type_name.eq(type_name.to_owned()))
        .fetch(t.pool())
        .await?;
    let Some(pt) = types.pop() else {
        return Err(FeedError::NotFound);
    };
    let Some(type_id) = pt.id.get().copied() else {
        return Err(FeedError::NotFound);
    };

    let pages = feed_pages(t.pool(), type_id).await?;

    let base = crate::sitemap::base_url(headers);
    let items: Vec<FeedItem> = pages.iter().map(|p| page_to_item(&base, p)).collect();

    Ok(Feed {
        title: format!("{} — {}", t.org.display_name, handler.verbose_name()),
        link: format!("{base}/"),
        description: format!("Latest {}.", handler.verbose_name().to_lowercase()),
        language: None,
        last_build_date: items.iter().filter_map(|i| i.pub_date).max(),
        items,
    })
}

fn page_to_item(base: &str, p: &Page) -> FeedItem {
    let path = if p.url_path.is_empty() {
        "/".to_owned()
    } else {
        p.url_path.clone()
    };
    let link = format!("{base}{path}");
    let pub_date = match p.published_at {
        Some(ts) => Some(ts),
        None => match p.updated_at {
            rustango::sql::Auto::Set(ts) => Some(ts),
            _ => None,
        },
    };
    let mut item = FeedItem::new(p.title.clone(), link.clone()).with_guid(link);
    if !p.seo_description.is_empty() {
        item = item.with_description(p.seo_description.clone());
    }
    if let Some(ts) = pub_date {
        item = item.with_pub_date(ts);
    }
    item
}

fn xml_response(xml: String, content_type: &'static str) -> Response {
    (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], xml).into_response()
}

fn not_found(kind: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        format!("no feed registered for kind `{kind}`"),
    )
        .into_response()
}

/// Build a single [`FeedItem`] for `page` against `base_url` (e.g.
/// `"https://example.com"`). Exposed for tests that don't want to
/// build a full `Feed`; production callers route through
/// `handle_feed_rss` / `handle_feed_atom`.
#[doc(hidden)]
pub fn _item_from_page(base: &str, page: &Page) -> FeedItem {
    page_to_item(base, page)
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use rustango::core::Model as _;
    use rustango::sql::Auto;

    async fn setup() -> (rustango::sql::Pool, i64) {
        let pool = rustango::sql::Pool::connect("sqlite::memory:")
            .await
            .expect("pool");
        for schema in [
            &PageType::SCHEMA,
            &crate::media::MediaCollection::SCHEMA,
            &crate::media::Media::SCHEMA,
            &crate::theme::Theme::SCHEMA,
            &Page::SCHEMA,
            &crate::view_restriction::PageViewRestriction::SCHEMA,
        ] {
            let sql = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
                pool.dialect(),
                schema,
            );
            for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                rustango::sql::raw_execute_pool(&pool, stmt, Vec::new())
                    .await
                    .expect("ddl");
            }
        }
        let mut pt = PageType {
            id: Auto::Unset,
            app_label: "cms".into(),
            type_name: "Post".into(),
            verbose_name: "Post".into(),
            default_template: "p.html".into(),
            view_mode: "auto".into(),
            is_creatable: true,
            allowed_parent_types: serde_json::json!([]),
            allowed_child_types: serde_json::json!([]),
            workflow: String::new(),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        pt.save_pool(&pool).await.expect("type");
        let type_id = pt.id.get().copied().expect("type id");
        (pool, type_id)
    }

    /// Save a published, indexable page at `path`; returns its id.
    async fn page(
        pool: &rustango::sql::Pool,
        type_id: i64,
        title: &str,
        path: &str,
        published_at: chrono::DateTime<chrono::Utc>,
    ) -> i64 {
        let mut p = Page {
            id: Auto::Unset,
            page_type_id: type_id,
            title: title.to_owned(),
            slug: title.to_owned(),
            path: path.to_owned(),
            url_path: format!("/{title}"),
            preview_path: String::new(),
            template_override: String::new(),
            depth: i32::try_from(path.matches('/').count()).unwrap_or(1),
            parent_id: None,
            locale_variant_of: None,
            alias_of: None,
            theme_id: None,
            sort_order: 0,
            status: "published".into(),
            published_at: Some(published_at),
            last_published_at: None,
            go_live_at: None,
            expire_at: None,
            seo_title: String::new(),
            seo_description: String::new(),
            robots_index: true,
            sitemap_priority: 0.5,
            show_in_menus: true,
            og_title: String::new(),
            og_description: String::new(),
            og_image_media_id: None,
            twitter_card: "summary".into(),
            notification_pre_published_sent: false,
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        p.save_pool(pool).await.expect("page");
        p.id.get().copied().expect("page id")
    }

    /// A type with more pages than the cap yields the newest cap.
    #[tokio::test]
    async fn a_feed_carries_only_the_newest_items() {
        let (pool, type_id) = setup().await;
        let start = chrono::Utc::now() - chrono::Duration::days(1);
        let total = FEED_MAX_ITEMS + 10;
        for n in 0..total {
            let path = format!("{:04x}/", n + 1);
            page(&pool, type_id, &format!("p{n}"), &path, start + chrono::Duration::minutes(n)).await;
        }
        let pages = feed_pages(&pool, type_id).await.expect("feed pages");
        assert_eq!(pages.len() as i64, FEED_MAX_ITEMS);
        assert_eq!(pages[0].title, format!("p{}", total - 1), "newest first");
    }

    /// A members-only page and its subtree stay out of the feed.
    #[tokio::test]
    async fn a_feed_leaves_out_restricted_pages() {
        let (pool, type_id) = setup().await;
        let now = chrono::Utc::now();
        page(&pool, type_id, "open", "0001/", now).await;
        let gated = page(&pool, type_id, "gated", "0002/", now).await;
        page(&pool, type_id, "gated-child", "0002/0001/", now).await;
        let mut r = crate::view_restriction::PageViewRestriction {
            id: Auto::Unset,
            page_id: gated,
            kind: crate::view_restriction::RestrictionKind::Login.as_str().to_owned(),
            password_hash: String::new(),
            group_ids: serde_json::json!([]),
            codenames: serde_json::json!([]),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        r.save_pool(&pool).await.expect("restriction");
        let titles: Vec<String> = feed_pages(&pool, type_id)
            .await
            .expect("feed pages")
            .into_iter()
            .map(|p| p.title)
            .collect();
        assert_eq!(titles, ["open"]);
    }
}
