//! Render a resolved [`Page`] through Tera.
//!
//! The flow:
//!
//! 1. Look up the [`PageType`] row via `page.page_type_id`.
//! 2. Find the registered [`PageTypeHandler`](crate::page_type::PageTypeHandler) by `type_name`.
//! 3. Ask the handler for its typed extension as JSON
//!    (`load_extension`).
//! 4. Build a Tera context with `page` / `page_type` / `extension`
//!    / `children` / `ancestors` / `breadcrumbs`.
//! 5. Render the page's own `template_override` when it names a
//!    template that exists, else `page_type.default_template`
//!    (see `pick_template`).

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use rustango::core::Column as _;
use rustango::extractors::Tenant;
use rustango::sql::FetcherPool as _;
use tera::{Context, Tera};

use crate::page::Page;
use crate::page_type::find_handler;
use crate::page_type_model::PageType;

/// Render `page` to an HTTP response. On any failure (missing
/// page-type row, missing handler, template error) returns a
/// 500/404 response with a short diagnostic body.
///
/// `url_prefix` is the mount prefix the public CMS is served under
/// (e.g. `""` for root, `"/p"` for `router_at("/p", _)`). It is
/// injected into the Tera context as `url_prefix`, but prefer the
/// `page_href` helper for links — `{{ child | page_href }}` /
/// `{{ page_href(page=ancestor) }}` — which prepends the prefix to the
/// page's full `url_path`. Do NOT build links as
/// `{{ url_prefix }}/{{ ancestor.slug }}` — the leaf slug alone breaks
/// for any page deeper than one level (a grandchild `/a/b` → `/b`).
pub async fn render(
    t: &Tenant,
    tera: &Tera,
    page: &Page,
    url_prefix: &str,
    // The serving site's root path, stripped from emitted links. Empty
    // for a single-site tenant, which is every existing one.
    site_prefix: &str,
    locale_code: Option<&str>,
    viewer: Option<&rustango::tenancy::auth::User>,
    cache: Option<(&rustango::cache::BoxedCache, std::time::Duration)>,
) -> Response {
    render_negotiated(
        t,
        tera,
        page,
        url_prefix,
        site_prefix,
        locale_code,
        viewer,
        crate::page_view::Accept::Html,
        cache,
    )
    .await
}

/// [`render`], plus the client's media-type preference.
///
/// Only page types that opt into a JSON view read `accept` at all — see
/// [`crate::page_view`]. `render` is the `Accept::Html` case and stays
/// source-compatible for every existing caller, including the admin's
/// own preview route.
#[allow(clippy::too_many_arguments)]
pub async fn render_negotiated(
    t: &Tenant,
    tera: &Tera,
    page: &Page,
    url_prefix: &str,
    // The serving site's root path, stripped from emitted links. Empty
    // for a single-site tenant, which is every existing one.
    site_prefix: &str,
    locale_code: Option<&str>,
    viewer: Option<&rustango::tenancy::auth::User>,
    accept: crate::page_view::Accept,
    cache: Option<(&rustango::cache::BoxedCache, std::time::Duration)>,
) -> Response {
    render_inner(
        t,
        tera,
        page,
        url_prefix,
        site_prefix,
        locale_code,
        viewer,
        None,
        None,
        accept,
        false,
        cache,
    )
    .await
}

/// Render `page` as a routable-page hit. The matched route's
/// name + named capture groups are exposed to the template as
/// `route_name` and `route_captures`; the handler's
/// `route_context` override gets a chance to inject additional ctx
/// before the framework keys are stamped.
#[allow(clippy::too_many_arguments)]
pub async fn render_with_route(
    t: &Tenant,
    tera: &Tera,
    page: &Page,
    url_prefix: &str,
    // The serving site's root path, stripped from emitted links. Empty
    // for a single-site tenant, which is every existing one.
    site_prefix: &str,
    locale_code: Option<&str>,
    viewer: Option<&rustango::tenancy::auth::User>,
    route_match: &crate::routable::RouteMatch,
    accept: crate::page_view::Accept,
    cache: Option<(&rustango::cache::BoxedCache, std::time::Duration)>,
) -> Response {
    render_inner(
        t,
        tera,
        page,
        url_prefix,
        site_prefix,
        locale_code,
        viewer,
        None,
        Some(route_match),
        accept,
        false,
        cache,
    )
    .await
}

/// Render `page` using virtual extension data drawn from a posted
/// form `HashMap` instead of the persisted row. The page row itself
/// is still rendered verbatim, so callers should pre-overlay the
/// form's canonical fields onto the `Page` before calling.
///
/// Powers the "preview unsaved changes" iframe in the editor: the
/// admin's edit form POSTs its current state to a preview endpoint
/// that builds a virtual `Page` + delegates here. Nothing is
/// persisted.
pub async fn render_preview(
    t: &Tenant,
    tera: &Tera,
    page: &Page,
    url_prefix: &str,
    // No site rewriting in preview: the editor works in tenant-absolute
    // paths, and a preview that silently re-based its links would not be
    // showing the page the visitor gets.
    site_prefix: &str,
    locale_code: Option<&str>,
    form: &std::collections::HashMap<String, String>,
    accept: crate::page_view::Accept,
    force_json: bool,
) -> Response {
    // Preview always renders fresh — never serve a cached menu fragment
    // (the editor may be previewing unsaved tree/menu changes). Previews
    // are admin-only, so menus show every item (viewer = None).
    render_inner(
        t,
        tera,
        page,
        url_prefix,
        site_prefix,
        locale_code,
        None,
        Some(form),
        None,
        accept,
        force_json,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn render_inner(
    t: &Tenant,
    tera: &Tera,
    page: &Page,
    url_prefix: &str,
    // The serving site's root path, stripped from emitted links. Empty
    // for a single-site tenant, which is every existing one.
    site_prefix: &str,
    locale_code: Option<&str>,
    viewer: Option<&rustango::tenancy::auth::User>,
    preview_form: Option<&std::collections::HashMap<String, String>>,
    route_match: Option<&crate::routable::RouteMatch>,
    accept: crate::page_view::Accept,
    // Serialize regardless of the type's declared `view_mode`. Set
    // **only** by the admin preview, where an authenticated editor has
    // explicitly asked to see a page's API shape. The public path never
    // sets it: letting `Accept` alone force JSON would give every
    // `auto` page a JSON representation it never opted into.
    force_json: bool,
    cache: Option<(&rustango::cache::BoxedCache, std::time::Duration)>,
) -> Response {
    // #401 — take the language-switcher entries the public handler
    // stashed just before polling this future (see
    // `language_switcher::stash_pending`). Read here, before the first
    // `.await`, so it runs on the caller's thread and becomes a plain
    // local that travels with the future; it's re-installed in the
    // await-free block below so the thread-local lands on the thread
    // that runs `tera.render`, immune to tokio's thread-hops.
    let pending_switcher = crate::language_switcher::take_pending();
    // The request origin the public handler stashed (see
    // `meta_tags::stash_site_origin`) — read here, before the first `.await`.
    let site_origin = crate::meta_tags::take_site_origin().unwrap_or_default();
    // #75 — alias resolution. When the row points at a source page,
    // render the source's content (title, page_type, extension data,
    // SEO, status, theme) while keeping the alias's URL-shape fields
    // (id, slug, path, url_path, parent_id). That way the rendered
    // body looks like the source but the URL stays the alias's.
    //
    // `extension_page_id` is the id to pass to the page-type
    // handler's `load_extension`. For non-aliases it's the page's
    // own id; for aliases it's the source's id (extension data is
    // shared with the source).
    let source_id_for_alias: Option<i64> = page.alias_of;
    // `alias_owned` holds the hybrid Page for the alias path so the
    // `&Page` returned by the if-expression has a stable owner. The
    // initial `None` is overwritten before any read on the alias
    // branch and never touched on the non-alias branch — silence
    // the unused-assignment lint inline (the binding pattern is
    // required for lifetime extension).
    #[allow(unused_assignments)]
    let mut alias_owned: Option<Page> = None;
    // #395 — for an alias the canonical URL points at the SOURCE page's
    // url_path (aliases are duplicate content of the source); captured
    // from the source row below. `None` for non-aliases → self-canonical.
    let mut canonical_source_path: Option<String> = None;
    let page: &Page = if let Some(source_id) = source_id_for_alias {
        use rustango::core::Column as _;
        use rustango::sql::FetcherPool as _;
        match Page::objects()
            .where_(Page::id.eq(source_id))
            .fetch(t.pool())
            .await
        {
            Ok(rows) => match rows.into_iter().next() {
                Some(src) => {
                    // #395 — canonical points at the source page for aliases.
                    canonical_source_path = Some(src.url_path.clone());
                    alias_owned = Some(crate::page::alias_hybrid(page, &src));
                    alias_owned.as_ref().unwrap()
                }
                None => {
                    tracing::warn!(
                        target: "rustango_cms::render",
                        alias_id = ?page.id.get().copied(),
                        source_id,
                        "alias source missing — rendering alias row verbatim"
                    );
                    page
                }
            },
            Err(e) => {
                tracing::warn!(target: "rustango_cms::render", error = %e, "alias source fetch failed");
                page
            }
        }
    } else {
        page
    };
    let extension_page_id: i64 = source_id_for_alias
        .or_else(|| page.id.get().copied())
        .unwrap_or_default();

    // #395 — canonical URL: the source's path for aliases, else this
    // page's own path (locale variants self-canonical; hreflang annotates
    // the alternates). Relative to the public mount, consistent with the
    // other URLs the renderer emits.
    // #640 — on a hostname-mapped site, the site root's prefix is stripped
    // like every other emitted link.
    let canonical_url = format!(
        "{url_prefix}{}",
        crate::site::to_public_path(
            site_prefix,
            canonical_source_path
                .as_deref()
                .unwrap_or(page.url_path.as_str())
        )
    );

    // Step 1: load the registry row for this page's type.
    let pts: Vec<PageType> = match PageType::objects()
        .where_(PageType::id.eq(page.page_type_id))
        .fetch(t.pool())
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "render: failed to fetch PageType row");
            return (StatusCode::INTERNAL_SERVER_ERROR, "page-type lookup failed").into_response();
        }
    };
    let Some(pt) = pts.into_iter().next() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "page_type_id {} has no cms_page_type row",
                page.page_type_id
            ),
        )
            .into_response();
    };

    // Step 2: locate the handler. #566 — a type with no code handler
    // (UI-created, or a code type whose handler drifted out of the
    // binary) falls back to a `DbSchemaPageType`: it renders via the
    // row's `default_template` with an empty typed extension. The
    // authored body still comes from the page-builder value store below,
    // so a schema-only type renders exactly like a code one — never 500.
    let handler = find_handler(&pt.type_name)
        .unwrap_or_else(|| Box::new(crate::page_builder::DbSchemaPageType::from_row(&pt)));

    // Step 3: load the typed extension row as JSON (handler default
    // returns `Value::Null` when there's no extension table). For
    // unsaved-content previews the form overlay produces the
    // extension JSON virtually — nothing hits the persisted row.
    let page_id = page.id.get().copied().unwrap_or_default();
    // Extension data for an alias comes from the source row, not
    // the alias row (the alias has no extension table entry).
    let extension_id = extension_page_id;
    let mut extension = if let Some(form) = preview_form {
        match handler
            .preview_extension(t.pool(), extension_id, form)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "render: preview_extension failed");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "preview extension build failed",
                )
                    .into_response();
            }
        }
    } else {
        match handler.load_extension(t.pool(), extension_id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "render: load_extension failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, "extension lookup failed")
                    .into_response();
            }
        }
    };

    // Step 3b: an API view answers here, before Step 4.
    //
    // Everything JSON needs — the overlaid `Page`, the virtual extension
    // built from unsaved form state — is already in hand, and none of
    // the Step 4 prefetch storm (children, menus, snippets, site
    // settings, breadcrumbs, stream prerender, themes) applies to a JSON
    // body. Short-circuiting here makes the JSON path cheaper than the
    // HTML one, which matters because the editor's preview pane re-runs
    // it on a one-second debounce.
    //
    // Because `render`, `render_with_route` and `render_preview` all
    // funnel through this function, the admin preview endpoint returns
    // the real response for free: the editor and the public URL run
    // literally the same code, so the preview cannot drift from what a
    // client receives.
    let view_kind = crate::page_view::resolve_kind(&pt.view_mode, &pt.default_template);
    let serve_json = force_json
        || (view_kind.serves_json()
            && (accept == crate::page_view::Accept::Json || !view_kind.serves_html()));
    if serve_json {
        // Resolve the locale here rather than inheriting Step 4's — this
        // branch returns long before that runs, which is exactly why the
        // JSON view shipped untranslated: `/fr/…` served canonical text.
        // Default locale resolves to `None`, since canonical content *is*
        // the default-locale content and a lookup would be wasted.
        let json_locale = crate::translation::resolve_locale(t.pool(), locale_code)
            .await
            .ok()
            .flatten();
        let json_locale_id = json_locale
            .as_ref()
            .filter(|l| !l.is_default)
            .and_then(|l| l.id.get().copied());
        let builder = crate::page_builder::values::values_for(
            t.pool(),
            page_id,
            page.page_type_id,
            preview_form,
        )
        .await
        .map(|(values, _)| values);
        let mut obj = crate::api::pages::detail_object_with_type(
            t.pool(),
            page,
            &pt.type_name,
            Some(std::mem::take(&mut extension)),
            builder,
            json_locale_id,
            viewer,
        )
        .await;
        crate::api::pages::echo_locale(&mut obj, json_locale.as_ref());
        let mut resp = axum::Json(serde_json::Value::Object(obj)).into_response();
        // Tell shared caches the body depends on `Accept`. Stamped only
        // here, not on every page response: a blanket `Vary: Accept`
        // would fragment CDN caches site-wide for a feature two page
        // types use.
        resp.headers_mut().insert(
            axum::http::header::VARY,
            axum::http::HeaderValue::from_static("Accept"),
        );
        return resp;
    }

    // Step 4: assemble Tera context with siblings of the rendered page.
    //
    // #249 — children come through the handler hook so index-style
    // page types can filter (status = published) + order (by
    // published_at desc) without forcing every template to re-do the
    // work. The default impl preserves the pre-#249 shape: all
    // immediate children, sort_order then id.
    //
    // #members — the viewer's access oracle, resolved once. Reused to hide
    // children and menu items the viewer can't access AND installed as a
    // thread-local so the `can_view` / `visible` Tera helpers guard
    // hand-written links and lists with the same protection. Bounded,
    // fixed query count; short-circuits on tenants with nothing gated.
    //
    // #748 — the three lookups are independent, so they run together:
    // one round trip of latency instead of three.
    let (children, access, ancestors) = rustango::__private_runtime::tokio::join!(
        handler.children_query(t.pool(), page),
        crate::view_restriction::access_context(t.pool(), viewer),
        page.ancestors(t.pool()),
    );
    // #723 — a lookup that fails still renders the page, without that
    // part, but the response is marked `no-store` so the page cache never
    // keeps the partial page for its whole TTL.
    let mut degraded = children.is_err() || ancestors.is_err();
    let children = children.unwrap_or_default();
    let ancestors = ancestors.unwrap_or_default();
    // #688 — a gated child's title and URL are as private as its body.
    // `children_query` decides which pages are live; this decides which
    // of them this viewer may see listed.
    let children: Vec<crate::page::Page> = if access.is_unrestricted() {
        children
    } else {
        children
            .into_iter()
            .filter(|c| access.can_view(&c.path, c.page_type_id))
            .collect()
    };

    // Resolve the request's locale + materialize the
    // translation overrides for this page AND every page referenced
    // in the context (children + ancestors). The flat `translations`
    // map drives the current page; `translations_by_page[id]` is what
    // templates reach for inside `{% for c in children %}` loops so
    // card titles / descriptions translate too.
    let mut related_ids: Vec<i64> = children
        .iter()
        .chain(ancestors.iter())
        .filter_map(|p| p.id.get().copied())
        .collect();
    // #i18n — also cover *grandchildren* (children of children) so an
    // index-of-index page (e.g. a homepage showing teaser cards for
    // blog posts / portfolio items that live two levels down) can
    // translate those card titles/excerpts via `translations_by_page`.
    // One `IN (…)` query, skipped when there are no children.
    {
        use rustango::core::Column as _;
        use rustango::sql::FetcherPool as _;
        let child_ids: Vec<i64> = children
            .iter()
            .filter_map(|p| p.id.get().copied())
            .collect();
        if !child_ids.is_empty() {
            if let Ok(grandchildren) = Page::objects()
                .where_(Page::parent_id.is_in(child_ids))
                .fetch(t.pool())
                .await
            {
                related_ids.extend(grandchildren.iter().filter_map(|p| p.id.get().copied()));
            }
        }
    }
    let (locale, translations, translations_by_page) =
        match crate::translation::resolve_locale(t.pool(), locale_code).await {
            Ok(Some(l)) => {
                if l.is_default {
                    // Default locale — canonical content IS the
                    // localized content. Skip the lookup entirely.
                    (
                        Some(l),
                        std::collections::HashMap::new(),
                        std::collections::HashMap::<
                            i64,
                            std::collections::HashMap<String, String>,
                        >::new(),
                    )
                } else {
                    let lid = l.id.get().copied().unwrap_or_default();
                    let mut all_ids = related_ids.clone();
                    all_ids.push(page_id);
                    let by_page = crate::translation::fetch_for_pages(t.pool(), &all_ids, lid)
                        .await
                        .unwrap_or_default();
                    let here = by_page.get(&page_id).cloned().unwrap_or_default();
                    (Some(l), here, by_page)
                }
            }
            _ => (
                None,
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            ),
        };
    // #409 — the active non-default locale id, used to substitute
    // per-field snippet translations at prefetch. `None` on the default
    // locale so canonical snippet text renders unchanged.
    let snippet_locale_id = locale
        .as_ref()
        .filter(|l| !l.is_default)
        .and_then(|l| l.id.get().copied());

    // v0.2 Slice T5 — resolve the active theme for this page (walks
    // page → ancestors → site default → admin default). The emitted
    // `<style>` block goes straight into `<head>` via the public
    // template; tokens are consumed via plain `var(--color-*)`.
    let (theme_css, theme_slug, theme_default_mode, theme_font_url) =
        match crate::theme_resolve::resolve_for_page(t.pool(), page).await {
            Ok(Some(rt)) => {
                let css = crate::theme::emit_css(&rt.theme, &rt.brand_colors);
                (
                    css,
                    rt.theme.slug.clone(),
                    rt.theme.default_mode.clone(),
                    rt.theme.font_url.clone(),
                )
            }
            _ => (
                String::new(),
                String::new(),
                "light".to_owned(),
                String::new(),
            ),
        };

    // Tera's index syntax `m[int_key]` doesn't coerce Number → String
    // reliably, and JSON object keys must be strings anyway. Convert
    // the HashMap to string-keyed before inserting into the context
    // so templates can do `translations_by_page[post.id | as_str]`
    // — but to keep templates terse we expose a built-in `tr` filter
    // (registered below) that takes a page id + field and resolves
    // against `translations_by_page` automatically.
    let translations_by_page_str: std::collections::HashMap<
        String,
        std::collections::HashMap<String, String>,
    > = translations_by_page
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    // #252 — pre-fetch every tag attached to the current page + each
    // child via a single `IN (…)` query, group by page_id, inject as
    // a `tags` array on each Page's JSON form. Templates iterate
    // `{% for tag in page.tags %}` / `{% for tag in post.tags %}`
    // without further DB hits.
    let mut tag_ids: Vec<i64> = Vec::with_capacity(children.len() + 1);
    tag_ids.push(page_id);
    tag_ids.extend(children.iter().filter_map(|c| c.id.get().copied()));
    // #842 — each page's categories, the same way.
    // #863 — and each child's builder values + choice labels, localized,
    // so a listing's cards read `child.builder.*` in the visitor's language.
    let child_locale_code = locale.as_ref().filter(|l| !l.is_default).map(|l| l.code.clone());
    let (tags_by_page, categories_by_page, builder_by_child) = rustango::__private_runtime::tokio::join!(
        crate::page_tag::prefetch_for_pages(t.pool(), &tag_ids),
        crate::category::assign::prefetch_for_pages(t.pool(), &tag_ids, snippet_locale_id),
        crate::page_builder::values::children_builder(
            t.pool(),
            &children,
            &translations_by_page,
            child_locale_code.as_deref(),
        ),
    );
    let builder_by_child = builder_by_child.unwrap_or_else(|_| {
        degraded = true;
        Default::default()
    });
    // A child with no share image chosen shows its first photo on cards,
    // as the page itself does in `og:image`: from its builder fields
    // (loaded above), else its code-made fields, looked up concurrently.
    let mut child_photos: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let mut lookups = rustango::__private_runtime::tokio::task::JoinSet::new();
    for child in children.iter().filter(|c| c.og_image_media_id.is_none()) {
        let Some(id) = child.id.get().copied() else { continue };
        match builder_by_child.get(&id) {
            Some((_, _, Some(photo))) => {
                child_photos.insert(id, *photo);
            }
            Some(_) => {}
            None => {
                let (pool, child) = (t.pool().clone(), child.clone());
                lookups.spawn(async move { (id, crate::meta_tags::fallback_share_image(&pool, &child).await) });
            }
        }
    }
    while let Some(done) = lookups.join_next().await {
        if let Ok((id, Some(photo))) = done {
            child_photos.insert(id, photo);
        }
    }
    let tags_by_page = tags_by_page.unwrap_or_else(|_| {
        degraded = true;
        Default::default()
    });
    let categories_by_page = categories_by_page.unwrap_or_else(|_| {
        degraded = true;
        Default::default()
    });
    let attach_tags = |row: &Page| -> serde_json::Value {
        let mut v = serde_json::to_value(row).unwrap_or(serde_json::Value::Null);
        if let Some(obj) = v.as_object_mut() {
            let pid = row.id.get().copied().unwrap_or_default();
            let list = tags_by_page.get(&pid).cloned().unwrap_or_default();
            obj.insert(
                "tags".to_owned(),
                serde_json::to_value(list).unwrap_or_else(|_| serde_json::Value::Array(vec![])),
            );
            let cats = categories_by_page.get(&pid).cloned().unwrap_or_default();
            obj.insert(
                "categories".to_owned(),
                serde_json::to_value(cats).unwrap_or_else(|_| serde_json::Value::Array(vec![])),
            );
            if let Some((values, labels, _)) = builder_by_child.get(&pid) {
                obj.insert("builder".to_owned(), values.clone());
                obj.insert("builder_labels".to_owned(), labels.clone());
            }
            if let Some(photo) = child_photos.get(&pid) {
                obj.insert("og_image_media_id".to_owned(), serde_json::json!(photo));
            }
        }
        v
    };
    let mut page_value = attach_tags(page);
    let mut children_values: Vec<serde_json::Value> = children.iter().map(attach_tags).collect();

    // #246 — let the handler inject template-visible state
    // (`latest_posts`, `featured_projects`, etc.) before the
    // framework's canonical keys are stamped. Stamping
    // handler-supplied keys FIRST means a typo'd "page" key can't
    // shadow the canonical `page`; the framework wins every
    // collision.
    let mut handler_ctx = handler
        .public_context(t.pool(), page)
        .await
        .unwrap_or_else(|_| {
            degraded = true;
            Default::default()
        });

    // #198 — routable_page: if the resolver routed to this page via
    // a regex pattern under the canonical `url_path`, merge the
    // handler's `route_context` on top of `public_context` (same
    // precedence rule — framework keys win all). The match itself
    // surfaces as the framework-owned `route_name` + `route_captures`
    // ctx vars stamped below.
    if let Some(rm) = route_match {
        if let Ok(extra) = handler.route_context(t.pool(), page, rm).await {
            for (k, v) in extra {
                handler_ctx.insert(k, v);
            }
        }
    }

    // #i18n — engine-level localization. Overlay the seeded per-page
    // scalar translations onto every page-representing object in the
    // context (current page, its extension, children, and any handler
    // card/list objects that carry a page id) so templates render
    // localized titles/intros/excerpts with no `t`-filter calls —
    // mirroring the automatic localization StreamField bodies get. No-op
    // on the default locale (empty `translations_by_page`).
    let mut ancestors_value = serde_json::to_value(&ancestors).unwrap_or(serde_json::Value::Null);
    if !translations_by_page.is_empty() {
        crate::translation::overlay_translations(&mut page_value, &translations_by_page);
        crate::translation::overlay_translations(&mut extension, &translations_by_page);
        crate::translation::overlay_translations(&mut ancestors_value, &translations_by_page);
        for cv in &mut children_values {
            crate::translation::overlay_translations(cv, &translations_by_page);
        }
        for v in handler_ctx.values_mut() {
            crate::translation::overlay_translations(v, &translations_by_page);
        }
    }

    let mut ctx = Context::new();
    for (k, v) in handler_ctx {
        ctx.insert(&k, &v);
    }
    ctx.insert("page", &page_value);
    ctx.insert("page_type", &pt);
    ctx.insert("extension", &extension);
    ctx.insert("children", &children_values);
    ctx.insert("ancestors", &ancestors_value);
    // #556 — archive context. A page serves as archived either by its
    // own status or by inheriting from the nearest archived ancestor (an
    // archived subtree). `ancestors` is already loaded (root→parent
    // order) so this adds no query — the "zero per-request cost when no
    // archived roots exist" goal falls out for free. `archived_via` names
    // that ancestor so hosts can show a "see the latest version" banner.
    {
        let archived = crate::page::PageStatus::Archived.as_str();
        let own_archived = page.status == archived;
        let archived_ancestor = if own_archived {
            None
        } else {
            ancestors.iter().rev().find(|a| a.status == archived)
        };
        ctx.insert(
            "is_archived",
            &(own_archived || archived_ancestor.is_some()),
        );
        ctx.insert(
            "archived_via",
            &archived_ancestor
                .map(|a| serde_json::json!({ "id": a.id.get().copied(), "title": a.title })),
        );
    }
    // #243 — resolve declared Page↔Snippet M2M relations into
    // `snippet_relations.<name>` so templates can iterate the chosen
    // snippets, e.g. `{% for c in snippet_relations.categories %}`.
    // Resolve failures degrade to an empty list (same as public_context
    // above) rather than 500-ing the page.
    {
        let page_id = page.id.get().copied().unwrap_or_default();
        let mut snippet_relations = serde_json::Map::new();
        for (relation, _ty) in handler.snippet_m2m_relations() {
            let snippets = crate::page_snippet_m2m::related_snippets(t.pool(), page_id, relation)
                .await
                .unwrap_or_else(|_| {
                    degraded = true;
                    Vec::new()
                });
            snippet_relations.insert(
                relation.to_owned(),
                serde_json::to_value(&snippets).unwrap_or(serde_json::Value::Null),
            );
        }
        ctx.insert("snippet_relations", &snippet_relations);
    }
    // #198 — routable-page surface. `route_name` is the matched
    // RouteSpec.name; `route_captures` is the `{name → value}` map
    // of regex named groups. Both keys are framework-owned and
    // stamp AFTER `public_context` / `route_context` so a typo'd
    // override can't shadow them.
    if let Some(rm) = route_match {
        ctx.insert("route_name", &rm.name);
        ctx.insert("route_captures", &rm.captures);
    } else {
        ctx.insert("route_name", &serde_json::Value::Null);
        ctx.insert(
            "route_captures",
            &std::collections::HashMap::<String, String>::new(),
        );
    }
    ctx.insert("url_prefix", url_prefix);
    ctx.insert("canonical_url", &canonical_url);
    ctx.insert("locale", &locale);
    // #i18n — active locale code as `LANG` so public templates can
    // localize hardcoded chrome (UI labels not stored as page content)
    // via the framework `translate(locale=LANG)` fn/filter, mirroring the
    // admin. Falls back to "en" when no locale resolved.
    let lang_code = locale.as_ref().map_or("en", |l| l.code.as_str());
    ctx.insert("LANG", lang_code);
    // #i18n — writing direction for the active locale so public templates
    // can set `<html dir>`. Core knows the RTL scripts (ar/he/fa/…), so an
    // RTL locale added purely via `cms_locale` renders RTL without extra
    // config. Mirrors the admin's `LANG_DIR`.
    ctx.insert("DIR", rustango::i18n::text_direction(lang_code));
    ctx.insert("translations", &translations);
    ctx.insert("translations_by_page", &translations_by_page_str);
    ctx.insert("theme_css", &theme_css);
    ctx.insert("theme_slug", &theme_slug);
    ctx.insert("theme_default_mode", &theme_default_mode);
    ctx.insert("theme_font_url", &theme_font_url);

    // Prefetch every Library snippet under the tenant's schema so
    // `{{ cms_snippet(slug=…) }}` resolves without the DB: the maps go
    // into the context as `_snippets_html*` and, for the function, into a
    // thread-local installed with the others below (#845).
    let (snippets_html, snippets_html_inline) =
        crate::snippet::prefetch_all(t.pool(), snippet_locale_id)
            .await
            .unwrap_or_else(|_| {
                degraded = true;
                Default::default()
            });
    ctx.insert("_snippets_html", &snippets_html);
    ctx.insert("_snippets_html_inline", &snippets_html_inline);

    // Pre-render every `WidgetKind::Stream` extension field to HTML
    // before Tera fires. The `stream_render(name="…")` Tera function
    // reads from `_stream_html` — mirrors the `cms_snippet` shape so
    // the public template just does `{{ stream_render(name="body_stream") | safe }}`.
    // The widget list says where the body is. Without it there is no body
    // to render, so this fails like `load_extension` (#723).
    let stream_widgets = match handler.widgets(t.pool(), page_id).await {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(error = %e, "render: widgets lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "page fields lookup failed").into_response();
        }
    };
    // Use the async variant so chooser blocks (page_chooser /
    // snippet_chooser / document_chooser / image) get enriched with
    // resolved row data (`_url`, `_title`, etc.) before the sync
    // template walker fires.
    let mut stream_html = match crate::block::tera_helpers::prerender_extension_streams_async_with(
        &stream_widgets,
        &extension,
        tera,
        t.pool(),
        &translations,
        snippet_locale_id, // #550 FB-17 — localize embedded form blocks
        // Host enrichers get the page, so a block can inherit from it,
        // and the tenant, so anything they cache stays per-tenant.
        page.id.get().copied(),
        &t.org.slug,
    )
    .await
    {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(
                target: "rustango_cms::block",
                error = %e,
                "stream pre-render failed; falling back to empty",
            );
            degraded = true;
            std::collections::HashMap::new()
        }
    };

    // #565 — page-builder body. When the page type has a published
    // schema, expose the filled values as `builder.*` (scalars + nested
    // groups) and merge each flexible-content/repeater zone's rendered
    // HTML into `_stream_html` so `{{ stream_render(name="<zone>") }}`
    // resolves. Under preview, values come from the posted form so the
    // iframe reflects unsaved edits. Additive to code handler streams.
    let mut builder_media: Option<i64> = None;
    if let Some(built) = crate::page_builder::values::public_render(
        t.pool(),
        extension_id,
        page.page_type_id,
        preview_form,
        &translations,
        tera,
        snippet_locale_id,
        page.id.get().copied(),
        &t.org.slug,
        locale.as_ref().filter(|l| !l.is_default).map(|l| l.code.as_str()),
    )
    .await
    {
        ctx.insert("builder", &built.values);
        // #863 — choice fields' labels in the visitor's language.
        ctx.insert("builder_labels", &built.labels);
        // Generic pre-rendered body for the fallback template (UI types).
        ctx.insert("builder_html", &built.body_html);
        stream_html.extend(built.zone_html);
        builder_media = built.first_media_id;
    }
    ctx.insert("_stream_html", &stream_html);

    // Share image: when the editor chose none on the Promote tab, the
    // page's first photo stands in — what the Promote tab has always
    // promised, and what `rcms_meta_tags` then emits as `og:image`.
    if page_value.get("og_image_media_id").map_or(true, serde_json::Value::is_null) {
        let fallback = crate::meta_tags::first_image_in_extension(&stream_widgets, &extension).or(builder_media);
        if let (Some(id), Some(obj)) = (fallback, page_value.as_object_mut()) {
            obj.insert("og_image_media_id".to_owned(), serde_json::json!(id));
            ctx.insert("page", &page_value);
        }
    }
    ctx.insert("site_origin", &site_origin);

    // #247 — resolve any `*_id` field on the extension that points
    // at a `cms_page` row, exposing both a `_pages_by_id` ctx map AND
    // the `pageurl(id=…)` Tera function. The function reads from a thread-local that's set
    // here and cleared the moment the render returns, dodging Tera's
    // "functions can't see context" limitation without paying the
    // per-render `Tera::clone()` tax.
    let pages_by_id = crate::page_url::prefetch_for_extension(&extension, t.pool()).await;
    // The string-keyed shape is what `_pages_by_id[id | string]` style
    // template access wants (JSON object keys must be strings; Tera's
    // map index doesn't coerce Number → String reliably).
    let pages_by_id_str: std::collections::HashMap<String, String> = pages_by_id
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    ctx.insert("_pages_by_id", &pages_by_id_str);
    // #244 — pre-fetch every `show_in_menus = true AND
    // status = "published"` page once per render so the
    // `auto_menu(parent_id=…, depth=…)` Tera function can build the
    // nested item tree from the in-memory pool. The pool is bounded
    // by menu-eligible cardinality (typically tens), so this is
    // cheaper than letting each `auto_menu` call re-query.
    // #316 — when the public router runs a page cache, reuse the same
    // backend to cache the menu-eligible tree query (shared across
    // every URL of the tenant). Keyed by tenant slug; TTL mirrors the
    // page cache so menu staleness matches page staleness. Preview /
    // admin renders pass `None` and always query live.
    //
    // #250 / #245 — the snippet and site-setting prefetches (documented
    // below) don't depend on the menu, so all three run together (#748).
    let menu_fut = async {
        match cache {
            Some((c, ttl)) => {
                let key = format!("rcms:menu:{}", t.org.slug);
                crate::auto_menu::prefetch_cached(t.pool(), c, &key, Some(ttl)).await
            }
            None => crate::auto_menu::prefetch(t.pool()).await,
        }
    };
    // #639 — the curated menus `menu(slug=…)` reads, filtered for this
    // viewer, localized and marked against this page. One query when the
    // tenant has no menus.
    let curated_fut = crate::navigation::resolve_menus_with(
        t.pool(),
        None,
        crate::navigation::MenuOptions {
            viewer,
            locale: locale.as_ref(),
            current: Some(page),
        },
    );
    let (menu_rows, snippet_html_map, site_settings, curated_menus) = rustango::__private_runtime::tokio::join!(
        menu_fut,
        crate::snippet_render::prefetch(t.pool(), tera, snippet_locale_id),
        crate::site_setting::prefetch_for_render(t.pool(), child_locale_code.as_deref()),
        curated_fut,
    );
    degraded |= curated_menus.is_err();
    let curated_menus = curated_menus.unwrap_or_default();
    // #i18n — localize menu titles for the active locale so `auto_menu()`
    // nav (sidebars, footers, switchers) shows translated labels, not just
    // the canonical ones. The prefetched rows are canonical (cached
    // per-tenant, locale-agnostic), so translate a per-request copy here —
    // before the await-free install block — using the same per-field
    // `cms_translation` overrides the `t` filter reads. Menu pages live
    // anywhere in the tree (not just the current page's children/
    // ancestors), so this is a separate, bounded lookup over the
    // menu-eligible ids. Default locale / no menu rows → no-op.
    let menu_rows = match &locale {
        Some(l) if !l.is_default && !menu_rows.is_empty() => {
            let lid = l.id.get().copied().unwrap_or_default();
            let menu_ids: Vec<i64> = menu_rows.iter().map(|r| r.id).collect();
            let menu_tr = crate::translation::fetch_for_pages(t.pool(), &menu_ids, lid)
                .await
                .unwrap_or_default();
            menu_rows
                .into_iter()
                .map(|mut r| {
                    if let Some(title) = menu_tr
                        .get(&r.id)
                        .and_then(|m| m.get("title"))
                        .filter(|s| !s.is_empty())
                    {
                        r.title = title.clone();
                    }
                    r
                })
                .collect()
        }
        _ => menu_rows,
    };

    let menu_rows = filter_menu_rows_for_viewer(&access, menu_rows);
    // #640 — the rows are cached tenant-wide with tenant-absolute paths;
    // this request's copy gets the site's public paths.
    let mut menu_rows = menu_rows;
    let mut curated_menus = curated_menus;
    if !site_prefix.is_empty() {
        for row in &mut menu_rows {
            row.url_path = crate::site::to_public_path(site_prefix, &row.url_path);
        }
        for menu in curated_menus.values_mut() {
            crate::navigation::rebase_page_urls(&mut menu.items, site_prefix);
        }
    }

    // #250 — `snippet_html_map` (joined above) pre-renders every snippet
    // whose `LibraryTypeHandler::render_template()` declares a template,
    // into a `(type, id) → html` map for the `snippet_html(type=…, id=N)`
    // Tera function. One render per snippet row; the result is shared
    // across every page that references the same snippet (so a Category
    // chip referenced from N blog cards renders once).
    //
    // #245 — `site_settings` holds every `cms_site_setting` row for the
    // `site_setting(scope=…)` Tera function. Bounded by distinct-scope
    // count (tens typically); one query covers every reference in the
    // rendered template.
    // Auto-inject the analytics beacon on real page renders (not editor
    // previews) unless disabled via the `analytics` site setting. Computed
    // here before `site_settings` is moved into `install()` below.
    let inject_beacon = preview_form.is_none() && crate::analytics::enabled(&site_settings);

    // Install every render-scoped thread-local in ONE await-free block
    // immediately before the synchronous `tera.render`. Each helper
    // (`pageurl`, `children_filtered`, `auto_menu`, `snippet_html`,
    // `site_setting`) stashes its data in a thread-local. If an
    // `.await` ran between an install and the render, tokio's
    // work-stealing scheduler could resume the render on a different
    // worker thread where the thread-local was never set (or still
    // held a prior request's data the guard cleared elsewhere) —
    // surfacing as an intermittently-empty `auto_menu` nav. Doing all
    // prefetch awaits above and every install here, with no await
    // before render, pins all five thread-locals to the rendering
    // thread.
    let _pageurl_guard = crate::page_url::install(pages_by_id); // #247
    let _url_prefix_guard = crate::page_url::install_url_prefix(url_prefix); // page_href
    // Installed HERE, with the other guards and after every await, for the
    // reason the block above documents: a thread hop mid-render drops a
    // thread-local installed earlier, which showed up once as an
    // intermittently-empty nav.
    let _site_prefix_guard = crate::page_url::install_site_prefix(site_prefix);
    let _children_guard = crate::children_filtered::install(page_id, children.clone()); // #249
    let _auto_menu_guard = crate::auto_menu::install(menu_rows); // #244/#316
    let _menus_guard = crate::navigation::install(curated_menus); // #639 — menu(slug=…)
    let _cms_snippets_guard = crate::snippet::install(snippets_html, snippets_html_inline); // #845 — cms_snippet(slug=…)
    let _access_guard = crate::access::install(access); // #members — can_view / visible
    let _snippet_html_guard = crate::snippet_render::install(snippet_html_map); // #250
                                                                                // #565 — `stream_render(name="…")` reads this thread-local (Tera 1.20
                                                                                // functions can't see context); feeds code stream fields + builder zones.
    let _stream_html_guard = crate::block::tera_helpers::install(stream_html);
    let _site_settings_guard = crate::site_setting::install(site_settings); // #245
                                                                            // #401 — install the stashed language-switcher entries on *this*
                                                                            // thread so `language_switcher()` resolves during `tera.render`.
    let _switcher_guard = pending_switcher.map(crate::language_switcher::install);

    // Step 5: render. Tera errors come back with the template name +
    // line, so we surface them verbatim — debugging is much easier
    // than a generic "template error" message.
    //
    // A page may name its own template instead of its type's. An
    // override that doesn't resolve falls back rather than failing:
    // the name is editor-typed and may point at a template that was
    // renamed or never created, and a page going dark over a typo is a
    // worse outcome than one rendering in its type's default layout.
    let template = pick_template(
        &page.template_override,
        &pt.default_template,
        tera,
        page.id.get().copied().unwrap_or_default(),
    );
    let rendered = tera.render(template, &ctx);
    // Every thread-local guard above is only needed for `tera.render`;
    // from here on awaiting is safe.
    match rendered {
        Ok(html) => {
            // Rich-text internal links anywhere in the page — extension
            // fields and stream blocks alike (#682).
            let html = crate::richtext::resolve_internal_links(t.pool(), html).await;
            let html = if inject_beacon {
                crate::analytics::inject_beacon(html)
            } else {
                html
            };
            page_response(html, degraded)
        }
        Err(e) => {
            let mut chain = e.to_string();
            let mut src: Option<&dyn std::error::Error> = std::error::Error::source(&e);
            while let Some(s) = src {
                chain.push_str("\n  caused by: ");
                chain.push_str(&s.to_string());
                src = s.source();
            }
            tracing::error!(template = %pt.default_template, error = %chain, "render: tera failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template `{}` failed:\n{}", pt.default_template, chain),
            )
                .into_response()
        }
    }
}

/// A rendered page as a response. A `degraded` page — one rendered with a
/// failed lookup left empty — is marked `no-store`, which the page cache
/// and any shared cache skip, so the gap lasts one request rather than a
/// whole TTL.
fn page_response(html: String, degraded: bool) -> Response {
    let mut resp = Html(html).into_response();
    if degraded {
        resp.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
    }
    resp
}

/// Which template a page renders through: its own override when it
/// names one that exists, otherwise its page type's.
///
/// The existence check is what makes the override safe to expose to
/// editors and agents. An override is a free-typed name — it can point
/// at a template that was renamed, deleted, or never created — and a
/// page going dark over a typo is a worse failure than one rendering in
/// its type's layout. The mismatch is logged rather than swallowed, so
/// the reason is findable.
fn pick_template<'a>(
    override_name: &'a str,
    default_template: &'a str,
    tera: &Tera,
    page_id: i64,
) -> &'a str {
    let over = override_name.trim();
    if over.is_empty() {
        return default_template;
    }
    if tera.get_template_names().any(|n| n == over) {
        return over;
    }
    tracing::warn!(
        target: "rustango_cms::render",
        page_id,
        template = %over,
        "page template_override does not resolve; using the page type's template",
    );
    default_template
}

// ===================================================================
// #members — viewer-aware menu filtering.
// ===================================================================

/// Drop menu rows the viewer may not access, using the pre-built
/// [`AccessContext`](crate::view_restriction::AccessContext) — in-memory,
/// no queries.
fn filter_menu_rows_for_viewer(
    access: &crate::view_restriction::AccessContext,
    rows: Vec<crate::auto_menu::MenuRow>,
) -> Vec<crate::auto_menu::MenuRow> {
    if rows.is_empty() || access.is_unrestricted() {
        return rows;
    }
    rows.into_iter()
        .filter(|r| access.can_view(&r.path, r.page_type_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::pick_template;

    fn tera_with(names: &[&str]) -> tera::Tera {
        let mut t = tera::Tera::default();
        for n in names {
            t.add_raw_template(n, "x").expect("template");
        }
        t
    }

    #[test]
    fn no_override_uses_the_page_types_template() {
        let t = tera_with(&["type.html"]);
        assert_eq!(pick_template("", "type.html", &t, 1), "type.html");
    }

    #[test]
    fn whitespace_only_override_counts_as_none() {
        let t = tera_with(&["type.html"]);
        assert_eq!(pick_template("   ", "type.html", &t, 1), "type.html");
    }

    #[test]
    fn an_existing_override_wins() {
        let t = tera_with(&["type.html", "fy2027/report.html"]);
        assert_eq!(
            pick_template("fy2027/report.html", "type.html", &t, 1),
            "fy2027/report.html"
        );
    }

    #[test]
    fn override_is_trimmed_before_lookup() {
        let t = tera_with(&["type.html", "fy2027/report.html"]);
        assert_eq!(
            pick_template("  fy2027/report.html\n", "type.html", &t, 1),
            "fy2027/report.html"
        );
    }

    #[test]
    fn a_missing_override_falls_back_instead_of_breaking_the_page() {
        // The whole point of the existence check: a renamed or
        // mistyped template must not take the page down.
        let t = tera_with(&["type.html"]);
        assert_eq!(pick_template("gone.html", "type.html", &t, 1), "type.html");
    }
}

#[cfg(test)]
mod page_response_tests {
    use super::page_response;

    #[test]
    fn a_degraded_page_is_never_stored() {
        let whole = page_response("<p>ok</p>".to_owned(), false);
        assert!(whole.headers().get(axum::http::header::CACHE_CONTROL).is_none());
        let partial = page_response("<p>partial</p>".to_owned(), true);
        assert_eq!(
            partial.headers().get(axum::http::header::CACHE_CONTROL).map(|v| v.to_str().unwrap_or("")),
            Some("no-store")
        );
    }
}
