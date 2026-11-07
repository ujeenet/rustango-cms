//! Theme packages: a site's look (and optionally its pages) as one `.zip`
//! that another site installs.
//!
//! A package holds `theme.json` (the [`StarterSeed`](super::StarterSeed)
//! format) and the site's `templates/`. [`export`] builds the seed from a
//! live site, [`pack`] / [`unpack`] write and read the archive, and
//! [`install`] applies it:
//!
//! - on an **empty** site, everything: page types, pages, menus, site
//!   settings and colours, exactly like a starter theme;
//! - on a site **with content**, only the look: missing page types are
//!   added, existing ones point at the package's templates, its colours
//!   become the default, and its error pages fill in any status the site
//!   has none for. Other pages, menus and settings are left as they are.

use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

use rustango::core::Column as _;
use rustango::sql::{FetcherPool as _, Pool};
use serde_json::{json, Map, Value};

use super::{create_page_type, create_theme, Outcome, StarterError, MARKER_SCOPE, SITE_SCOPE, UI_APP_LABEL};
use crate::navigation::{Menu, MenuItem};
use crate::page::Page;
use crate::page_builder::model as pb;
use crate::page_type_model::PageType;
use crate::theme::Theme;

/// The archive entry holding the seed.
pub const SEED_ENTRY: &str = "theme.json";
/// The archive folder holding the templates.
pub const TEMPLATES_DIR: &str = "templates";

/// Widgets whose stored value is an id of a row on *this* site (an image,
/// a document, a page, …). Exported content drops them: the ids mean
/// nothing on another site.
const REFERENCE_WIDGETS: &[&str] = &[
    "mediapicker",
    "documentchooser",
    "pagechooser",
    "snippetchooser",
    "modelchooser",
    "file",
];
/// Id keys of the built-in blocks (image, document, page and snippet
/// choosers), dropped for the same reason.
const REFERENCE_BLOCK_KEYS: &[&str] = &["media_id", "document_id", "page_id", "snippet_id"];
/// Theme columns that identify the row rather than describe the look.
const THEME_IDENTITY: &[&str] = &["id", "slug", "name", "is_default", "is_admin_default", "created_at", "updated_at"];

/// The site's look, and optionally its pages, as a `theme.json` value.
///
/// Page types are the UI-defined ones (those a theme can recreate);
/// colours are the site's default theme; the error pages come along. With `with_pages`, the page
/// tree, its content and the menus come too, minus references to images
/// and other rows that only exist on this site.
///
/// # Errors
/// Database errors.
pub async fn export(pool: &Pool, name: &str, with_pages: bool) -> Result<Value, StarterError> {
    let types: Vec<PageType> = PageType::objects().fetch(pool).await?;
    let mut by_id: HashMap<i64, String> = HashMap::new();
    let mut page_types = Vec::new();
    let mut reference_keys: HashMap<String, HashSet<String>> = HashMap::new();
    for t in &types {
        let Some(id) = t.id.get().copied() else { continue };
        by_id.insert(id, t.type_name.clone());
        if t.app_label != UI_APP_LABEL {
            continue;
        }
        let Some(schema) = pb::published_for(pool, id).await? else { continue };
        reference_keys.insert(t.type_name.clone(), reference_fields(&schema.document));
        page_types.push(json!({
            "type_name": t.type_name,
            "verbose_name": t.verbose_name,
            "template": t.default_template,
            "parents": t.allowed_parents(),
            "children": t.allowed_children(),
            "schema": schema.document,
        }));
    }
    let exported: HashSet<&str> = page_types.iter().filter_map(|t| t["type_name"].as_str()).collect();

    let mut seed = Map::new();
    let site = crate::site_setting::get(pool, SITE_SCOPE).await?.map(|s| s.value_json);
    // The package is named after the site as visitors see it (its
    // Branding name), when it has one; `name` otherwise. The name travels
    // too; the logo and icon are this site's media, so they don't.
    let branding = crate::branding::get(pool).await;
    let site_name = branding["site_name"].as_str().map(str::trim).filter(|n| !n.is_empty());
    seed.insert("name".into(), json!(site_name.unwrap_or(name)));
    if let Some(n) = site_name {
        seed.insert("branding".into(), json!({ "site_name": n }));
    }
    if let Some(site) = site {
        seed.insert("site".into(), site);
    }
    if let Some(theme) = default_theme(pool).await? {
        seed.insert("theme".into(), theme_seed(&theme)?);
    }
    seed.insert("page_types".into(), Value::Array(page_types.clone()));
    // The error pages are part of the look: they travel even without the
    // rest of the content.
    let home = Page::objects()
        .where_(Page::depth.eq(1))
        .order_by(&[("sort_order", false), ("id", false)])
        .first(pool)
        .await?;
    if let Some(home) = home {
        let error_pages = super::errors::export(pool, &home).await?;
        if !error_pages.is_empty() {
            seed.insert("error_pages".into(), Value::Array(error_pages));
        }
    }

    if with_pages {
        let pages: Vec<Page> = Page::objects().fetch(pool).await?;
        // Only pages whose type travels with the package; a page of a code
        // type would fail to load on a site without that code.
        let travels = |p: &Page| by_id.get(&p.page_type_id).is_some_and(|t| exported.contains(t.as_str()));
        let mut keys: HashMap<i64, String> = HashMap::new();
        for p in &pages {
            if let Some(id) = p.id.get().copied() {
                keys.insert(id, page_key(p));
            }
        }
        let roots: Vec<&Page> = pages.iter().filter(|p| p.parent_id.is_none() && travels(p)).collect();
        let mut tree = Vec::new();
        for root in roots.into_iter().take(1) {
            tree.push(page_seed(pool, root, &pages, &by_id, &keys, &reference_keys, &travels).await?);
        }
        seed.insert("pages".into(), Value::Array(tree));
        seed.insert("menus".into(), Value::Array(menus(pool, &keys).await?));
    }
    Ok(Value::Object(seed))
}

/// The most entries and the most unpacked bytes a package may hold. A theme
/// is a few dozen small text files; anything far beyond that is refused
/// before it is unpacked, so a hostile archive cannot fill the disk.
pub const MAX_ENTRIES: usize = 500;
pub const MAX_UNPACKED_BYTES: u64 = 8 * 1024 * 1024;

/// A package read into memory: its `theme.json` and its templates, each as
/// (path under `templates/`, body).
#[derive(Debug)]
pub struct Package {
    pub seed: String,
    pub templates: Vec<(String, String)>,
}

/// Build a package in memory.
///
/// # Errors
/// Serialisation or archive errors.
pub fn pack_bytes(seed: &Value, templates: &[(String, String)]) -> Result<Vec<u8>, StarterError> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file(SEED_ENTRY, opts).map_err(zip_err)?;
    zip.write_all(serde_json::to_string_pretty(seed)?.as_bytes()).map_err(io)?;
    for (name, body) in templates {
        let rel = safe_relative(name)
            .ok_or_else(|| StarterError::Invalid(format!("template name `{name}` is not a relative path")))?;
        let rel = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
        zip.start_file(format!("{TEMPLATES_DIR}/{rel}"), opts).map_err(zip_err)?;
        zip.write_all(body.as_bytes()).map_err(io)?;
    }
    Ok(zip.finish().map_err(zip_err)?.into_inner())
}

/// Read a package from memory. Refuses an archive with too many entries
/// or too many unpacked bytes, an entry outside `templates/`, a path that
/// would escape its folder, a template that is not UTF-8 text, and an
/// archive without `theme.json`.
///
/// # Errors
/// [`StarterError::Invalid`] naming the problem.
pub fn read_bytes(bytes: &[u8]) -> Result<Package, StarterError> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(zip_err)?;
    if zip.len() > MAX_ENTRIES {
        return Err(StarterError::Invalid(format!(
            "package has {} files; a theme has at most {MAX_ENTRIES}",
            zip.len()
        )));
    }
    let mut seed = None;
    let mut templates = Vec::new();
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let entry = zip.by_index(i).map_err(zip_err)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().map_err(zip_err)?.into_owned();
        // Read at most what is left of the budget, plus one byte to notice
        // an entry that lies about its size.
        let budget = MAX_UNPACKED_BYTES.saturating_sub(total);
        let mut body = Vec::new();
        entry.take(budget + 1).read_to_end(&mut body).map_err(io)?;
        total += body.len() as u64;
        if total > MAX_UNPACKED_BYTES {
            return Err(StarterError::Invalid(format!(
                "package unpacks to more than {} MB",
                MAX_UNPACKED_BYTES / (1024 * 1024)
            )));
        }
        let text = String::from_utf8(body)
            .map_err(|_| StarterError::Invalid(format!("package entry `{name}` is not text")))?;
        if name == SEED_ENTRY {
            seed = Some(text);
            continue;
        }
        let rel = name
            .strip_prefix(&format!("{TEMPLATES_DIR}/"))
            .and_then(safe_relative)
            .ok_or_else(|| StarterError::Invalid(format!("package entry `{name}` is not a template")))?;
        let rel = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
        templates.push((rel, text));
    }
    let seed = seed.ok_or_else(|| StarterError::Invalid(format!("package has no {SEED_ENTRY}")))?;
    Ok(Package { seed, templates })
}

/// Write a package file: `theme.json` plus every file under `templates_dir`.
///
/// # Errors
/// I/O errors reading the templates or writing the archive.
pub fn pack(seed: &Value, templates_dir: &Path, out: &Path) -> Result<usize, StarterError> {
    let mut files = Vec::new();
    collect(templates_dir, &mut files).map_err(io)?;
    files.sort();
    let mut templates = Vec::with_capacity(files.len());
    for path in &files {
        let rel = path.strip_prefix(templates_dir).unwrap_or(path);
        let name = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
        let body = std::fs::read_to_string(path)
            .map_err(|e| StarterError::Invalid(format!("{}: {e}", path.display())))?;
        templates.push((name, body));
    }
    std::fs::write(out, pack_bytes(seed, &templates)?).map_err(io)?;
    Ok(templates.len())
}

/// Read a package file, writing its templates under `templates_dest`
/// (created if missing); returns the `theme.json` and how many templates
/// were written. Same refusals as [`read_bytes`].
///
/// # Errors
/// An invalid package or I/O errors.
pub fn unpack(package: &Path, templates_dest: &Path) -> Result<(String, usize), StarterError> {
    let pkg = read_bytes(&std::fs::read(package).map_err(io)?)?;
    for (name, body) in &pkg.templates {
        let dest = templates_dest.join(name);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        std::fs::write(&dest, body).map_err(io)?;
    }
    Ok((pkg.seed, pkg.templates.len()))
}

/// Read only the `theme.json` of a package file.
///
/// # Errors
/// An invalid package or I/O errors.
pub fn read_seed(package: &Path) -> Result<String, StarterError> {
    Ok(read_bytes(&std::fs::read(package).map_err(io)?)?.seed)
}

/// Apply a package's `theme.json` to a site: everything on an empty site,
/// only the look on a site with content (see the module docs).
///
/// # Errors
/// An invalid seed or database errors.
pub async fn install(pool: &Pool, seed_json: &str) -> Result<Outcome, StarterError> {
    let site_is_empty = crate::site_setting::get(pool, MARKER_SCOPE).await?.is_none()
        && Page::objects().first(pool).await?.is_none();
    if site_is_empty {
        return super::apply(pool, seed_json).await;
    }
    let seed: super::StarterSeed = serde_json::from_str(seed_json)?;
    super::check_types(&seed, &pb::component_map(pool).await?)?;

    let mut added = 0;
    let mut retemplated = 0;
    for pt in &seed.page_types {
        let existing = PageType::objects()
            .where_(PageType::type_name.eq(pt.type_name.clone()))
            .first(pool)
            .await?;
        match existing {
            None => {
                create_page_type(pool, pt).await?;
                added += 1;
            }
            // A type made in the admin (or by a theme): its fields and
            // content stay; it renders with the package's template.
            Some(mut row) if row.app_label == UI_APP_LABEL => {
                if row.default_template != pt.template {
                    row.default_template = pt.template.clone();
                    row.save(pool).await?;
                    retemplated += 1;
                }
            }
            // A type defined in code keeps the template its code names.
            Some(_) => {}
        }
    }
    let roots = Page::objects()
        .where_(Page::depth.eq(1))
        .order_by(&[("sort_order", false), ("id", false)])
        .fetch(pool)
        .await?;
    if let Some(theme) = &seed.theme {
        let theme_id = create_theme(pool, theme).await?;
        for mut root in roots.iter().cloned() {
            root.theme_id = Some(theme_id);
            root.save(pool).await?;
        }
    }
    let error_pages_added = match roots.first() {
        Some(home) => super::errors::add_missing(pool, home, &seed.error_pages).await?,
        None => 0,
    };
    crate::site_setting::upsert(
        pool,
        MARKER_SCOPE,
        json!({ "theme": seed.name, "applied_at": chrono::Utc::now().to_rfc3339() }),
    )
    .await?;
    Ok(Outcome::Switched {
        page_types_added: added,
        page_types_retemplated: retemplated,
        error_pages_added,
    })
}

async fn default_theme(pool: &Pool) -> Result<Option<Theme>, StarterError> {
    Ok(Theme::objects().where_(Theme::is_default.eq(true)).first(pool).await?)
}

fn theme_seed(theme: &Theme) -> Result<Value, StarterError> {
    let Value::Object(mut tokens) = serde_json::to_value(theme)? else {
        return Err(StarterError::Invalid("theme did not serialize to an object".into()));
    };
    for key in THEME_IDENTITY {
        tokens.remove(*key);
    }
    Ok(json!({
        "base": "paper",
        "slug": theme.slug,
        "name": theme.name,
        "tokens": tokens,
    }))
}

/// A stable, readable key for a page: its URL path, `home` for the root.
fn page_key(p: &Page) -> String {
    let trimmed = p.url_path.trim_matches('/');
    if trimmed.is_empty() {
        "home".to_owned()
    } else {
        trimmed.replace('/', "-")
    }
}

fn page_seed<'a>(
    pool: &'a Pool,
    page: &'a Page,
    all: &'a [Page],
    type_names: &'a HashMap<i64, String>,
    keys: &'a HashMap<i64, String>,
    reference_keys: &'a HashMap<String, HashSet<String>>,
    travels: &'a (dyn Fn(&Page) -> bool + Sync),
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, StarterError>> + Send + 'a>> {
    Box::pin(async move {
        let id = page.id.get().copied().unwrap_or_default();
        let type_name = type_names.get(&page.page_type_id).cloned().unwrap_or_default();
        let mut seed = json!({
            "key": keys.get(&id).cloned().unwrap_or_default(),
            "type": type_name,
            "title": page.title,
            "slug": page.slug,
        });
        if !page.seo_description.is_empty() {
            seed["seo_description"] = json!(page.seo_description);
        }
        if page.show_in_menus {
            seed["in_menu"] = json!(true);
        }
        if let Some(row) = pb::data_for_page(pool, id).await? {
            let mut data = row.data;
            strip_references(&mut data, reference_keys.get(&type_name));
            seed["data"] = data;
        }
        let mut children: Vec<&Page> = all.iter().filter(|c| c.parent_id == Some(id) && travels(c)).collect();
        children.sort_by_key(|c| (c.sort_order, c.path.clone()));
        let mut out = Vec::new();
        for child in children {
            out.push(page_seed(pool, child, all, type_names, keys, reference_keys, travels).await?);
        }
        if !out.is_empty() {
            seed["children"] = Value::Array(out);
        }
        Ok(seed)
    })
}

async fn menus(pool: &Pool, keys: &HashMap<i64, String>) -> Result<Vec<Value>, StarterError> {
    let mut out = Vec::new();
    for menu in Menu::objects().fetch(pool).await? {
        let Some(menu_id) = menu.id.get().copied() else { continue };
        let items: Vec<MenuItem> = MenuItem::objects()
            .where_(MenuItem::menu_id.eq(menu_id))
            .fetch(pool)
            .await?;
        out.push(json!({
            "slug": menu.slug,
            "name": menu.name,
            "items": menu_items(&items, None, keys),
        }));
    }
    Ok(out)
}

fn menu_items(items: &[MenuItem], parent: Option<i64>, keys: &HashMap<i64, String>) -> Vec<Value> {
    let mut level: Vec<&MenuItem> = items.iter().filter(|i| i.parent_id == parent).collect();
    level.sort_by_key(|i| i.sort_order);
    level
        .into_iter()
        .filter_map(|item| {
            let mut v = json!({ "label": item.label });
            match (item.page_id.and_then(|id| keys.get(&id)), &item.external_url) {
                (Some(key), _) => v["page"] = json!(key),
                // A link to a page that does not travel is dropped.
                (None, _) if item.page_id.is_some() => return None,
                (None, Some(url)) => v["url"] = json!(url),
                (None, None) => {}
            }
            let children = menu_items(items, item.id.get().copied(), keys);
            if v.get("page").is_none() && v.get("url").is_none() && item.label.is_empty() {
                return None;
            }
            if !children.is_empty() {
                v["children"] = Value::Array(children);
            }
            Some(v)
        })
        .collect()
}

/// The keys of every field in a schema whose value is an id of a row on
/// this site, at any depth (rows, groups, repeaters, flexible zones).
fn reference_fields(schema: &Value) -> HashSet<String> {
    let mut keys = HashSet::new();
    walk_nodes(schema, &mut keys);
    keys
}

fn walk_nodes(v: &Value, keys: &mut HashSet<String>) {
    match v {
        Value::Object(map) => {
            if map.get("kind").and_then(Value::as_str) == Some("field")
                && map
                    .get("widget")
                    .and_then(Value::as_str)
                    .is_some_and(|w| REFERENCE_WIDGETS.contains(&w))
            {
                if let Some(k) = map.get("key").and_then(Value::as_str) {
                    keys.insert(k.to_owned());
                }
            }
            for child in map.values() {
                walk_nodes(child, keys);
            }
        }
        Value::Array(items) => items.iter().for_each(|i| walk_nodes(i, keys)),
        _ => {}
    }
}

fn strip_references(data: &mut Value, keys: Option<&HashSet<String>>) {
    match data {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if REFERENCE_BLOCK_KEYS.contains(&k.as_str()) || keys.is_some_and(|ks| ks.contains(k)) {
                    *v = Value::Null;
                } else {
                    strip_references(v, keys);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| strip_references(i, keys)),
        _ => {}
    }
}

/// `rel` as a path that stays inside its destination, or `None`.
fn safe_relative(rel: &str) -> Option<PathBuf> {
    let path = Path::new(rel);
    let ok = !rel.is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)));
    ok.then(|| path.to_path_buf())
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

fn io(e: std::io::Error) -> StarterError {
    StarterError::Invalid(e.to_string())
}

fn zip_err(e: zip::result::ZipError) -> StarterError {
    StarterError::Invalid(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_package_paths_are_refused() {
        assert!(safe_relative("post.html").is_some());
        assert!(safe_relative("blocks/cta.html").is_some());
        for bad in ["", "../etc/passwd", "/etc/passwd", "blocks/../../x", "./x"] {
            assert!(safe_relative(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn exported_content_drops_ids_of_this_site() {
        let schema = json!({ "nodes": [
            { "kind": "field", "key": "cover", "widget": "mediapicker" },
            { "kind": "field", "key": "title", "widget": "text" },
            { "kind": "flex", "key": "body", "allowed": ["image", "gallery"], "groups": [
                { "key": "gallery", "children": [ { "kind": "field", "key": "image_1", "widget": "mediapicker" } ] } ] }
        ]});
        let keys = reference_fields(&schema);
        let mut data = json!({
            "cover": 12, "title": "Hi",
            "body": [
                { "type": "image", "value": { "media_id": 3, "caption": "c" } },
                { "type": "gallery", "value": { "image_1": 4 } }
            ]
        });
        strip_references(&mut data, Some(&keys));
        assert_eq!(data["cover"], Value::Null);
        assert_eq!(data["title"], "Hi");
        assert_eq!(data["body"][0]["value"]["media_id"], Value::Null);
        assert_eq!(data["body"][0]["value"]["caption"], "c");
        assert_eq!(data["body"][1]["value"]["image_1"], Value::Null);
    }
}
