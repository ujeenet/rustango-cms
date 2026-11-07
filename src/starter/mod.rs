//! Starter themes: a site's first page types, pages, menus and look,
//! created from one JSON seed.
//!
//! A theme made by `rcms new --theme <name>` ships a `theme.json` next to
//! its templates. [`apply`] reads it and, on a site that has no pages yet:
//!
//! 1. creates the page types as UI-defined page-builder schemas (no Rust
//!    model), so editors can change their fields and blocks in the admin;
//! 2. creates the page tree, published, with each page's content;
//! 3. creates the menus (`header`, `footer`, …) pointing at those pages;
//! 4. stores the site settings (`site` scope: name, tagline, footer);
//! 5. adds the theme's colours and fonts as a theme row and makes it the
//!    site's default.
//!
//! It runs once per site: a `starter` site setting records that it did,
//! and a site that already has pages is left alone, so an editor's work
//! is never overwritten.
//!
//! [`package`] turns a live site into a theme package (`theme.json` plus
//! its templates, as a `.zip`) and installs one on another site;
//! [`cli`] is the `theme pack|install|check` command around it.
//!
//! ```ignore
//! fn seed_theme<'a>(pool: &'a Pool, _org: &'a Org) -> SeedFuture<'a> {
//!     Box::pin(async move {
//!         rustango_cms::starter::apply(pool, include_str!("../theme.json"))
//!             .await
//!             .map(|_| ())
//!             .map_err(|e| SeedError::Seeder(e.to_string()))
//!     })
//! }
//! rustango_cms::register_tenant_seeder!(seed_theme);
//! ```
//!
//! ## `theme.json`
//!
//! ```json
//! {
//!   "name": "Blog",
//!   "site": { "name": "My blog", "tagline": "…", "footer": "…" },
//!   "theme": { "base": "paper", "slug": "starter-blog", "name": "Blog",
//!              "tokens": { "color_link": "#2f6fdd", "font_heading": "…" } },
//!   "page_types": [
//!     { "type_name": "Post", "verbose_name": "Post", "template": "post.html",
//!       "parents": ["Blog"], "children": [], "schema": { "nodes": [ … ] } }
//!   ],
//!   "pages": [
//!     { "key": "home", "type": "Home", "title": "Home", "slug": "",
//!       "data": { … }, "children": [ { "key": "blog", … } ] }
//!   ],
//!   "menus": [
//!     { "slug": "header", "name": "Header",
//!       "items": [ { "page": "blog" }, { "label": "GitHub", "url": "https://…" } ] }
//!   ],
//!   "error_pages": [
//!     { "status": 404, "title": "Page not found", "slug": "page-not-found",
//!       "body": [ { "type": "paragraph", "value": { "body": "…" } } ] }
//!   ]
//! }
//! ```
//!
//! `schema` is a page-builder [`Document`](crate::page_builder::Document);
//! `data` is the page's builder values, where a flexible zone is a list
//! of `{ "type": …, "value": { … } }` entries (ids are added for you).
//! `tokens` overrides any column of the `base` preset theme.
//! `error_pages` become pages of the built-in Error page type under the
//! home page, served for their status (404, 403, 500 or 503); see
//! [`errors`]. `media` lists sample pictures that page content uses as
//! `"@media:<key>"`; see [`media`].

use std::collections::HashMap;

use rustango::core::Column as _;
use rustango::sql::{Auto, ExecError, FetcherPool as _, Pool};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::navigation::{Menu, MenuItem};
use crate::page::{Page, PageStatus};
use crate::page_builder::{model as pb, parse_schema, validate_schema};
use crate::page_type_model::PageType;
use crate::theme::Theme;
use crate::tree_ops::{NewPage, TreeError};

pub mod cli;
pub mod errors;
pub mod media;
pub mod package;

pub use errors::ErrorPageSeed;
pub use media::MediaSeed;

/// Site-setting scope recording that a starter theme was applied.
pub const MARKER_SCOPE: &str = "starter";
/// Site-setting scope holding the theme's site name, tagline and footer.
pub const SITE_SCOPE: &str = "site";
/// `app_label` of the page types a theme creates: the same label the admin
/// gives types made in the page-type builder, so they edit and delete alike.
const UI_APP_LABEL: &str = "cms_ui";

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The theme was applied; the site now has its pages, menus and look.
    Applied {
        page_types: usize,
        pages: usize,
        menus: usize,
        error_pages: usize,
        /// Sample pictures stored in the media library.
        media: usize,
    },
    /// A starter theme was applied to this site before.
    AlreadyApplied,
    /// The site already has pages, so nothing was touched.
    SiteNotEmpty,
    /// A package was installed on a site with content: only its look
    /// changed (see [`package::install`]).
    Switched {
        page_types_added: usize,
        page_types_retemplated: usize,
        /// Error pages added for statuses the site had none for.
        error_pages_added: usize,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum StarterError {
    #[error("theme.json: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("theme.json: {0}")]
    Invalid(String),
    #[error(transparent)]
    Db(#[from] ExecError),
    #[error(transparent)]
    Tree(#[from] TreeError),
}

/// A parsed `theme.json`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StarterSeed {
    pub name: String,
    #[serde(default)]
    pub site: Map<String, Value>,
    /// The site's name, logo and icon (`site_name`, `logo`, `favicon`),
    /// written to Settings → Branding where it has none yet.
    #[serde(default)]
    pub branding: Map<String, Value>,
    #[serde(default)]
    pub theme: Option<ThemeSeed>,
    #[serde(default)]
    pub page_types: Vec<PageTypeSeed>,
    #[serde(default)]
    pub pages: Vec<PageSeed>,
    #[serde(default)]
    pub menus: Vec<MenuSeed>,
    #[serde(default)]
    pub error_pages: Vec<ErrorPageSeed>,
    #[serde(default)]
    pub media: Vec<MediaSeed>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeSeed {
    /// Slug of the preset theme to start from (`paper`, …).
    pub base: String,
    pub slug: String,
    pub name: String,
    /// Columns of [`Theme`] to override, by name.
    #[serde(default)]
    pub tokens: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageTypeSeed {
    pub type_name: String,
    pub verbose_name: String,
    /// The public template, resolved against the site's own templates.
    pub template: String,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub children: Vec<String>,
    pub schema: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageSeed {
    /// Name menus use to point at this page.
    pub key: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub title: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub seo_description: String,
    #[serde(default)]
    pub in_menu: bool,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub children: Vec<PageSeed>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MenuSeed {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub items: Vec<MenuItemSeed>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MenuItemSeed {
    /// Empty: the linked page's title.
    #[serde(default)]
    pub label: String,
    /// Key of a page in `pages`.
    #[serde(default)]
    pub page: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub children: Vec<MenuItemSeed>,
}

/// Parse and check a `theme.json` without touching a database: every
/// schema valid, every page of a declared type, every menu link resolving.
///
/// # Errors
/// [`StarterError::Parse`] or [`StarterError::Invalid`] naming the problem.
pub fn parse(seed_json: &str) -> Result<StarterSeed, StarterError> {
    let seed: StarterSeed = serde_json::from_str(seed_json)?;
    check(&seed, &HashMap::new())?;
    Ok(seed)
}

/// Apply a starter theme to one site (a tenant pool), once. Its sample
/// pictures are left out; see [`apply_with_media`].
///
/// # Errors
/// An invalid `theme.json`, or a database error part-way. The steps are
/// not one transaction: after a failure the site has some pages, so a
/// retry reports [`Outcome::SiteNotEmpty`] instead of duplicating them.
pub async fn apply(pool: &Pool, seed_json: &str) -> Result<Outcome, StarterError> {
    apply_with_media(pool, "", seed_json, &[]).await
}

/// [`apply`], storing the theme's sample pictures in the site's media
/// library first. `files` are (file name, bytes) for the `media` entries;
/// `org_slug` is the site whose media storage they go to.
///
/// # Errors
/// As [`apply`], plus a picture that fails to store.
pub async fn apply_with_media(
    pool: &Pool,
    org_slug: &str,
    seed_json: &str,
    files: &[(&str, &[u8])],
) -> Result<Outcome, StarterError> {
    let mut seed: StarterSeed = serde_json::from_str(seed_json)?;
    if crate::site_setting::get(pool, MARKER_SCOPE).await?.is_some() {
        return Ok(Outcome::AlreadyApplied);
    }
    if Page::objects().first(pool).await?.is_some() {
        return Ok(Outcome::SiteNotEmpty);
    }
    check(&seed, &pb::component_map(pool).await?)?;

    let media_ids = media::store(pool, org_slug, &seed.media, files).await?;
    resolve_media(&mut seed.pages, &media_ids);
    seed.site.values_mut().for_each(|v| media::resolve(v, &media_ids));
    seed.branding.values_mut().for_each(|v| media::resolve(v, &media_ids));
    for error_page in &mut seed.error_pages {
        media::resolve(&mut error_page.body, &media_ids);
    }

    // Page types: type name -> (id, published schema version).
    let mut types: HashMap<String, (i64, i32)> = HashMap::new();
    for pt in &seed.page_types {
        types.insert(pt.type_name.clone(), create_page_type(pool, pt).await?);
    }

    let mut pages: HashMap<String, i64> = HashMap::new();
    let mut root: Option<Page> = None;
    for page in &seed.pages {
        let created = create_page(pool, None, page, &types, &mut pages).await?;
        root.get_or_insert(created);
    }

    for menu in &seed.menus {
        create_menu(pool, menu, &pages).await?;
    }
    let error_pages = match &root {
        Some(root) => errors::create(pool, root, &seed.error_pages).await?,
        None => 0,
    };

    if !seed.site.is_empty() {
        crate::site_setting::upsert(pool, SITE_SCOPE, Value::Object(seed.site.clone())).await?;
    }
    if !seed.branding.is_empty() {
        // Keep anything the site's owner already set.
        let mut branding = crate::branding::get(pool).await.as_object().cloned().unwrap_or_default();
        for (key, value) in &seed.branding {
            let empty = value.as_str().is_some_and(str::is_empty);
            if !empty && branding.get(key).and_then(Value::as_str).is_none_or(str::is_empty) {
                branding.insert(key.clone(), value.clone());
            }
        }
        crate::site_setting::upsert(pool, crate::branding::SCOPE, Value::Object(branding)).await?;
    }
    if let Some(theme) = &seed.theme {
        let theme_id = create_theme(pool, theme).await?;
        if let Some(mut root) = root {
            root.theme_id = Some(theme_id);
            root.save(pool).await?;
        }
    }

    crate::site_setting::upsert(
        pool,
        MARKER_SCOPE,
        json!({ "theme": seed.name, "applied_at": chrono::Utc::now().to_rfc3339() }),
    )
    .await?;
    Ok(Outcome::Applied {
        page_types: seed.page_types.len(),
        pages: pages.len(),
        menus: seed.menus.len(),
        error_pages,
        media: media_ids.len(),
    })
}

fn resolve_media(pages: &mut [PageSeed], ids: &HashMap<String, String>) {
    for page in pages {
        media::resolve(&mut page.data, ids);
        resolve_media(&mut page.children, ids);
    }
}

/// Everything that can be checked before the first write.
fn check(
    seed: &StarterSeed,
    components: &HashMap<String, crate::page_builder::ComponentEntry>,
) -> Result<(), StarterError> {
    let mut problems = type_problems(seed, components);
    let type_names: Vec<&str> = seed.page_types.iter().map(|pt| pt.type_name.as_str()).collect();
    let mut keys = Vec::new();
    collect_pages(&seed.pages, &type_names, &mut keys, &mut problems);
    for menu in &seed.menus {
        check_menu_items(&menu.items, &keys, &menu.slug, &mut problems);
    }
    if seed.pages.len() > 1 {
        problems.push("one root page expected (the home page); nest the others under it".into());
    }
    if !seed.error_pages.is_empty() && seed.pages.is_empty() {
        problems.push("error pages need a home page to live under".into());
    }
    let home_slugs: Vec<&str> = seed.pages.iter().flat_map(|h| &h.children).map(|c| c.slug.as_str()).collect();
    problems.extend(errors::problems(&seed.error_pages, &home_slugs));
    problems.extend(media::problems(seed));
    if problems.is_empty() {
        Ok(())
    } else {
        Err(StarterError::Invalid(problems.join("; ")))
    }
}

/// Only the page types: what installing a package's look needs.
fn check_types(
    seed: &StarterSeed,
    components: &HashMap<String, crate::page_builder::ComponentEntry>,
) -> Result<(), StarterError> {
    let mut problems = type_problems(seed, components);
    problems.extend(errors::problems(&seed.error_pages, &[]));
    if problems.is_empty() {
        Ok(())
    } else {
        Err(StarterError::Invalid(problems.join("; ")))
    }
}

fn type_problems(
    seed: &StarterSeed,
    components: &HashMap<String, crate::page_builder::ComponentEntry>,
) -> Vec<String> {
    let mut problems = Vec::new();
    for pt in &seed.page_types {
        if crate::page_type::find_handler(&pt.type_name).is_some() {
            problems.push(format!(
                "page type `{}` is also defined in code; pick another name",
                pt.type_name
            ));
        }
        match parse_schema(&pt.schema) {
            Ok(doc) => problems.extend(
                validate_schema(&doc, components)
                    .into_iter()
                    .map(|p| format!("page type `{}`: {p}", pt.type_name)),
            ),
            Err(e) => problems.push(format!("page type `{}`: {e}", pt.type_name)),
        }
    }
    problems
}

fn collect_pages<'a>(
    pages: &'a [PageSeed],
    type_names: &[&str],
    keys: &mut Vec<&'a str>,
    problems: &mut Vec<String>,
) {
    for page in pages {
        if !type_names.contains(&page.type_name.as_str()) {
            problems.push(format!("page `{}` uses undeclared type `{}`", page.key, page.type_name));
        }
        if keys.contains(&page.key.as_str()) {
            problems.push(format!("page key `{}` is used twice", page.key));
        }
        keys.push(&page.key);
        collect_pages(&page.children, type_names, keys, problems);
    }
}

fn check_menu_items(items: &[MenuItemSeed], keys: &[&str], menu: &str, problems: &mut Vec<String>) {
    for item in items {
        match (&item.page, &item.url) {
            (Some(p), None) if !keys.contains(&p.as_str()) => {
                problems.push(format!("menu `{menu}` links to unknown page `{p}`"));
            }
            (Some(_), Some(_)) => {
                problems.push(format!("menu `{menu}`: an item has both `page` and `url`"));
            }
            (None, None) if item.label.is_empty() => {
                problems.push(format!("menu `{menu}`: an item has no page, url or label"));
            }
            _ => {}
        }
        check_menu_items(&item.children, keys, menu, problems);
    }
}

async fn create_page_type(pool: &Pool, seed: &PageTypeSeed) -> Result<(i64, i32), StarterError> {
    let mut row = PageType {
        id: Auto::Unset,
        app_label: UI_APP_LABEL.to_owned(),
        type_name: seed.type_name.clone(),
        verbose_name: seed.verbose_name.clone(),
        default_template: seed.template.clone(),
        view_mode: "auto".to_owned(),
        is_creatable: true,
        allowed_parent_types: json!(seed.parents),
        allowed_child_types: json!(seed.children),
        workflow: String::new(),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    row.insert(pool).await?;
    let id = *row
        .id
        .get()
        .ok_or_else(|| StarterError::Invalid(format!("no id for page type `{}`", seed.type_name)))?;
    pb::save_draft(pool, id, seed.schema.clone(), None).await?;
    let published = pb::publish(pool, id)
        .await?
        .ok_or_else(|| StarterError::Invalid(format!("page type `{}` did not publish", seed.type_name)))?;
    Ok((id, published.version))
}

fn create_page<'a>(
    pool: &'a Pool,
    parent: Option<&'a Page>,
    seed: &'a PageSeed,
    types: &'a HashMap<String, (i64, i32)>,
    pages: &'a mut HashMap<String, i64>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Page, StarterError>> + Send + 'a>> {
    Box::pin(async move {
        let (type_id, version) = types[&seed.type_name];
        let mut new = NewPage::new(type_id, &seed.title, &seed.slug).with_status(PageStatus::Published);
        if !seed.seo_description.is_empty() {
            new = new.with_seo(&seed.title, &seed.seo_description);
        }
        let mut page = match parent {
            Some(parent) => Page::create_child_pool(pool, parent, new).await?,
            None => Page::create_root_pool(pool, new).await?,
        };
        if seed.in_menu {
            page.show_in_menus = true;
            page.save(pool).await?;
        }
        let id = *page.id.get().ok_or(TreeError::MissingId)?;
        if !seed.data.is_null() {
            let data = with_block_ids(&seed.data, &seed.key);
            pb::upsert_data(pool, id, version, json!({}), data).await?;
        }
        crate::revision::capture(pool, &page, None).await?;
        pages.insert(seed.key.clone(), id);
        for child in &seed.children {
            create_page(pool, Some(&page), child, types, pages).await?;
        }
        Ok(page)
    })
}

/// Give every entry of a flexible zone (a list of `{type, value}`
/// objects) a stable id unique within the page, as the editor does.
fn with_block_ids(data: &Value, page_key: &str) -> Value {
    let Value::Object(fields) = data else {
        return data.clone();
    };
    let mut out = fields.clone();
    for (field, value) in &mut out {
        if let Value::Array(entries) = value {
            for (n, entry) in entries.iter_mut().enumerate() {
                if let Value::Object(e) = entry {
                    if e.contains_key("type") && !e.contains_key("id") {
                        e.insert("id".into(), json!(format!("{page_key}-{field}-{n}")));
                    }
                }
            }
        }
    }
    Value::Object(out)
}

async fn create_menu(
    pool: &Pool,
    seed: &MenuSeed,
    pages: &HashMap<String, i64>,
) -> Result<(), StarterError> {
    let mut menu = Menu {
        id: Auto::Unset,
        slug: seed.slug.clone(),
        name: seed.name.clone(),
        created_at: Auto::Unset,
    };
    menu.save(pool).await?;
    let menu_id = *menu.id.get().ok_or(TreeError::MissingId)?;
    create_menu_items(pool, menu_id, None, &seed.items, pages).await
}

fn create_menu_items<'a>(
    pool: &'a Pool,
    menu_id: i64,
    parent_id: Option<i64>,
    items: &'a [MenuItemSeed],
    pages: &'a HashMap<String, i64>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StarterError>> + Send + 'a>> {
    Box::pin(async move {
        for (n, seed) in items.iter().enumerate() {
            let mut item = MenuItem {
                id: Auto::Unset,
                menu_id,
                parent_id,
                sort_order: i32::try_from((n + 1) * 10).unwrap_or(i32::MAX),
                label: seed.label.clone(),
                page_id: seed.page.as_ref().and_then(|k| pages.get(k).copied()),
                external_url: seed.url.clone(),
                open_in_new_tab: false,
            };
            item.save(pool).await?;
            if !seed.children.is_empty() {
                let id = *item.id.get().ok_or(TreeError::MissingId)?;
                create_menu_items(pool, menu_id, Some(id), &seed.children, pages).await?;
            }
        }
        Ok(())
    })
}

/// Copy the `base` preset (or update the theme with the same slug, when a
/// package is installed again), apply the token overrides, and make it the
/// site's default public theme.
async fn create_theme(pool: &Pool, seed: &ThemeSeed) -> Result<i64, StarterError> {
    let existing = Theme::objects()
        .where_(Theme::slug.eq(seed.slug.clone()))
        .first(pool)
        .await?;
    let base = match &existing {
        Some(row) => row.clone(),
        None => Theme::objects()
            .where_(Theme::slug.eq(seed.base.clone()))
            .first(pool)
            .await?
            .ok_or_else(|| StarterError::Invalid(format!("no preset theme `{}`", seed.base)))?,
    };
    let mut fields = match serde_json::to_value(&base)? {
        Value::Object(map) => map,
        _ => return Err(StarterError::Invalid("theme did not serialize to an object".into())),
    };
    for (key, value) in &seed.tokens {
        if !fields.contains_key(key) || matches!(key.as_str(), "id" | "slug" | "created_at" | "updated_at") {
            return Err(StarterError::Invalid(format!("theme token `{key}` is not a theme setting")));
        }
        fields.insert(key.clone(), value.clone());
    }
    fields.insert("slug".into(), json!(seed.slug));
    fields.insert("name".into(), json!(seed.name));
    let mut theme: Theme = serde_json::from_value(Value::Object(fields))?;
    theme.is_default = true;
    if existing.is_some() {
        theme.save(pool).await?;
    } else {
        theme.id = Auto::Unset;
        theme.created_at = Auto::Unset;
        theme.updated_at = Auto::Unset;
        theme.is_admin_default = false;
        theme.insert(pool).await?;
    }
    let id = *theme.id.get().ok_or(TreeError::MissingId)?;
    // One public default: clear the flag on every other theme.
    for mut other in Theme::objects().fetch(pool).await? {
        if other.is_default && other.id.get().copied() != Some(id) {
            other.is_default = false;
            other.save(pool).await?;
        }
    }
    Ok(id)
}
