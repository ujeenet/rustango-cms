//! `cargo run -- theme …`: pack a site into a theme package, install one,
//! or check one.
//!
//! ```text
//! theme pack <file.zip> [--site <slug>] [--no-pages]
//! theme install <file.zip> [--site <slug>]
//! theme check <file.zip | theme.json>
//! ```
//!
//! The site is a tenant: `--site` names it, and it may be left out when the
//! registry has a single active site. Templates are read from and written
//! to `$RCMS_SITE_DIR/templates` (default `./templates`). `install` first
//! copies the current folder to `templates.bak-<timestamp>`, then writes
//! the package's templates over it: files the package does not have stay,
//! so pages of a type the package doesn't know keep their template.

use std::error::Error;
use std::path::{Path, PathBuf};

use rustango::sql::{sqlx, FetcherPool as _, Pool};
use rustango::tenancy::{Org, TenantPools};

use super::package;
use super::Outcome;

pub const COMMAND: &str = "theme";

const USAGE: &str = "usage:
  theme pack <file.zip> [--site <slug>] [--no-pages]
  theme install <file.zip> [--site <slug>]
  theme check <file.zip | theme.json>";

type BoxError = Box<dyn Error>;

/// Run the `theme` command; `args` are the words after `theme`.
///
/// # Errors
/// Bad arguments, an unknown site, an invalid package, or I/O and
/// database errors, each described for the person at the terminal.
pub async fn run(args: &[String]) -> Result<(), BoxError> {
    let (verb, rest) = args.split_first().ok_or(USAGE)?;
    let mut file: Option<PathBuf> = None;
    let mut site: Option<String> = None;
    let mut with_pages = true;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--site" => {
                i += 1;
                site = Some(rest.get(i).ok_or("--site needs a slug")?.clone());
            }
            "--no-pages" => with_pages = false,
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`\n{USAGE}").into()),
            path if file.is_none() => file = Some(PathBuf::from(path)),
            extra => return Err(format!("unexpected `{extra}`\n{USAGE}").into()),
        }
        i += 1;
    }
    let file = file.ok_or(USAGE)?;
    let templates = templates_dir();

    match verb.as_str() {
        "check" => {
            let json = if file.extension().is_some_and(|e| e == "json") {
                std::fs::read_to_string(&file)?
            } else {
                package::read_seed(&file)?
            };
            let seed = super::parse(&json)?;
            println!(
                "{}: valid — {} page type(s), {} menu(s){}",
                seed.name,
                seed.page_types.len(),
                seed.menus.len(),
                if seed.pages.is_empty() { ", look only (no pages)" } else { "" }
            );
        }
        "pack" => {
            let (org, pool) = site_pool(site.as_deref()).await?;
            let seed = package::export(&pool, &org.display_name, with_pages).await?;
            let files = package::pack(&seed, &templates, &file)?;
            println!(
                "packed `{}` into {}: theme.json + {files} template file(s){}",
                org.slug,
                file.display(),
                if with_pages { "" } else { " (look only, no pages)" }
            );
        }
        "install" => {
            // Validate before touching anything on disk.
            let json = package::read_seed(&file)?;
            super::parse(&json)?;
            let (org, pool) = site_pool(site.as_deref()).await?;
            let backup = copy_aside(&templates)?;
            let (_, written) = package::unpack(&file, &templates)?;
            let outcome = package::install(&pool, &json).await?;
            match outcome {
                Outcome::Applied { pages, .. } => {
                    println!("installed on `{}` (an empty site): {pages} page(s), menus and colours", org.slug);
                }
                Outcome::Switched { page_types_added, page_types_retemplated, error_pages_added } => println!(
                    "switched `{}` to the package's look: {written} template file(s), \
                     {page_types_added} page type(s) added, {page_types_retemplated} re-templated, \
                     {error_pages_added} error page(s) added; other pages, menus and settings unchanged",
                    org.slug
                ),
                Outcome::AlreadyApplied | Outcome::SiteNotEmpty => {}
            }
            if let Some(b) = backup {
                println!("previous templates kept in {}", b.display());
            }
            for (type_name, template) in missing_templates(&pool, &templates).await? {
                println!(
                    "warning: pages of type `{type_name}` render with `{template}`, which is not in {}",
                    templates.display()
                );
            }
            println!("restart the server to load the new templates");
        }
        other => return Err(format!("unknown theme command `{other}`\n{USAGE}").into()),
    }
    Ok(())
}

fn templates_dir() -> PathBuf {
    std::env::var_os("RCMS_SITE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("templates")
}

/// Copy `dir` to `dir.bak-<timestamp>`; `None` when it does not exist.
fn copy_aside(dir: &Path) -> Result<Option<PathBuf>, BoxError> {
    if !dir.exists() {
        return Ok(None);
    }
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let name = format!("{}.bak-{stamp}", dir.file_name().and_then(|n| n.to_str()).unwrap_or("templates"));
    let backup = dir.with_file_name(name);
    copy_dir(dir, &backup)?;
    Ok(Some(backup))
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let path = entry?.path();
        let dest = to.join(path.file_name().unwrap_or_default());
        if path.is_dir() {
            copy_dir(&path, &dest)?;
        } else {
            std::fs::copy(&path, &dest)?;
        }
    }
    Ok(())
}

/// Page types with live pages whose template file is not under
/// `templates` (only names that look like a file in that folder; the
/// CMS's own `rcms_admin/…` templates are built in).
async fn missing_templates(pool: &Pool, templates: &Path) -> Result<Vec<(String, String)>, BoxError> {
    use crate::page::Page;
    use crate::page_type_model::PageType;
    let used: std::collections::HashSet<i64> = Page::objects()
        .fetch(pool)
        .await?
        .into_iter()
        .map(|p| p.page_type_id)
        .collect();
    let mut missing = Vec::new();
    for t in PageType::objects().fetch(pool).await? {
        let in_use = t.id.get().is_some_and(|id| used.contains(id));
        let template = t.default_template.clone();
        if in_use
            && !template.is_empty()
            && !template.starts_with("rcms_admin/")
            && !templates.join(&template).exists()
        {
            missing.push((t.type_name, template));
        }
    }
    Ok(missing)
}

/// The tenant pool of the site named `slug`, or of the only active site.
async fn site_pool(slug: Option<&str>) -> Result<(Org, Pool), BoxError> {
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set (see .env)")?;
    let registry = Pool::connect(&url).await?;
    match registry {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => pick(TenantPools::<sqlx::Postgres>::new(pg), slug).await,
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => pick(TenantPools::<sqlx::Sqlite>::new(sq), slug).await,
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => pick(TenantPools::<sqlx::MySql>::new(my), slug).await,
    }
}

async fn pick<DB>(pools: TenantPools<DB>, slug: Option<&str>) -> Result<(Org, Pool), BoxError>
where
    DB: sqlx::Database,
    Pool: From<sqlx::Pool<DB>>,
{
    let orgs: Vec<Org> = Org::objects()
        .fetch(&pools.registry_pool())
        .await?
        .into_iter()
        .filter(|o| o.active)
        .collect();
    let names = || orgs.iter().map(|o| o.slug.as_str()).collect::<Vec<_>>().join(", ");
    let org = match slug {
        Some(s) => orgs
            .iter()
            .find(|o| o.slug == s)
            .ok_or_else(|| format!("no active site `{s}` (sites: {})", names()))?,
        None if orgs.len() == 1 => &orgs[0],
        None => return Err(format!("name the site with --site (sites: {})", names()).into()),
    };
    let pool = pools.scoped_pool_dyn(org).await?;
    Ok((org.clone(), pool))
}
