//! Help → Documentation: the Rustango-CMS guides, read inside the admin.
//!
//! The guides are the files `docs/index.toml` lists, bundled at build time
//! (see `build.rs`), so the Help center always matches the installed
//! version. Links between guides stay in the admin; screenshots come from
//! the docs site, which hosts `docs/img/` at the same paths; other links
//! into the repository open on GitHub.
//!
//! Every signed-in admin user may read it: the editors' guides are the
//! point.

use std::sync::OnceLock;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use rustango::extractors::{SessionUser, Tenant};
use serde::Deserialize;
use serde_json::json;
use tera::Context;

use super::handlers::{add_admin_theme, add_chrome, render_with_csrf};
use super::AdminError;

include!(concat!(env!("OUT_DIR"), "/help_docs.rs"));

/// Where the published docs live; it serves `docs/img/` at `/static/img/`.
pub const DOCS_SITE: &str = "https://cms.rustango.com";
/// Repository files a guide links to (examples, source) open here.
const REPO_DOCS: &str = "https://github.com/ujeenet/rustango-cms/blob/main/docs";

#[derive(Debug, Deserialize)]
pub(crate) struct SearchQuery {
    #[serde(default)]
    pub q: String,
}

/// One guide.
struct Guide {
    section: &'static str,
    slug: &'static str,
    title: String,
    markdown: &'static str,
}

fn guides() -> &'static [Guide] {
    static GUIDES: OnceLock<Vec<Guide>> = OnceLock::new();
    GUIDES.get_or_init(|| {
        HELP_SECTIONS
            .iter()
            .flat_map(|(section, _, pages)| {
                pages.iter().map(move |(slug, markdown)| Guide {
                    section,
                    slug,
                    title: title_of(markdown, slug),
                    markdown,
                })
            })
            .collect()
    })
}

/// The guide's first `# ` heading; its file name otherwise.
fn title_of(markdown: &str, slug: &str) -> String {
    markdown
        .lines()
        .find_map(|l| l.strip_prefix("# "))
        .map_or_else(|| slug.replace('-', " "), |t| t.trim().to_owned())
}

/// The sections with their guides, for the index and the side list.
fn contents() -> serde_json::Value {
    let mut sections = Vec::new();
    for (title, slug, pages) in HELP_SECTIONS {
        let items: Vec<_> = pages
            .iter()
            .filter_map(|(page, _)| guides().iter().find(|g| g.slug == *page))
            .map(|g| json!({ "slug": g.slug, "title": g.title }))
            .collect();
        sections.push(json!({ "title": title, "slug": slug, "guides": items }));
    }
    serde_json::Value::Array(sections)
}

/// Point a guide's links where they work from inside the admin.
fn rewrite_links(markdown: &str) -> String {
    static LINK: OnceLock<regex::Regex> = OnceLock::new();
    static SRC: OnceLock<regex::Regex> = OnceLock::new();
    let link = LINK.get_or_init(|| regex::Regex::new(r"\]\(([^)\s]+)\)").expect("valid regex"));
    let src = SRC.get_or_init(|| regex::Regex::new(r#"src="img/"#).expect("valid regex"));
    let out = link.replace_all(markdown, |c: &regex::Captures<'_>| format!("]({})", target(&c[1])));
    src.replace_all(&out, format!(r#"src="{DOCS_SITE}/static/img/"#)).into_owned()
}

fn target(href: &str) -> String {
    if href.starts_with("http://")
        || href.starts_with("https://")
        || href.starts_with("mailto:")
        || href.starts_with('#')
        || href.starts_with('/')
    {
        return href.to_owned();
    }
    if let Some(rest) = href.strip_prefix("img/") {
        return format!("{DOCS_SITE}/static/img/{rest}");
    }
    let (path, fragment) = href.split_once('#').map_or((href, ""), |(p, f)| (p, f));
    if let Some(stem) = path.strip_suffix(".md") {
        if !stem.contains('/') && guides().iter().any(|g| g.slug == stem) {
            let frag = if fragment.is_empty() { String::new() } else { format!("#{fragment}") };
            return format!("/cms-admin/help/{stem}{frag}");
        }
    }
    format!("{REPO_DOCS}/{href}")
}

/// Text around the first match of `needle` (lowercase) in `markdown`,
/// with Markdown punctuation stripped, for search results.
fn snippet(markdown: &str, needle: &str) -> String {
    let flat: String = markdown
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim_start().starts_with("```") && !l.contains("]("))
        .collect::<Vec<_>>()
        .join(" ")
        .replace(['*', '`', '|', '>'], "");
    let lower = flat.to_lowercase();
    let Some(at) = lower.find(needle) else {
        return flat.chars().take(160).collect();
    };
    // About 70 characters of lead-in, starting at a word.
    let start = flat[..at].char_indices().rev().nth(70).map_or(0, |(i, _)| i);
    let start = if start == 0 { 0 } else { flat[start..at].find(' ').map_or(start, |s| start + s + 1) };
    let text: String = flat[start..].chars().take(180).collect();
    format!("{}{}…", if start > 0 { "…" } else { "" }, text.trim())
}

/// `GET /cms-admin/help?q=…` — the contents, or search results.
pub(crate) async fn index(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    SessionUser(user): SessionUser,
    Query(q): Query<SearchQuery>,
) -> Result<Response, AdminError> {
    if user.is_none() {
        return Ok(Redirect::to("/login").into_response());
    }
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "help", user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    ctx.insert("sections", &contents());
    ctx.insert("docs_site", DOCS_SITE);
    let query = q.q.trim().to_lowercase();
    ctx.insert("q", q.q.trim());
    if !query.is_empty() {
        let results: Vec<_> = guides()
            .iter()
            .filter(|g| g.title.to_lowercase().contains(&query) || g.markdown.to_lowercase().contains(&query))
            .map(|g| json!({ "slug": g.slug, "title": g.title, "section": g.section, "snippet": snippet(g.markdown, &query) }))
            .collect();
        ctx.insert("results", &results);
    }
    render_with_csrf(&state, &headers, "rcms_admin/help_index.html", &mut ctx)
}

/// `GET /cms-admin/help/{slug}` — one guide.
pub(crate) async fn guide(
    State(state): State<super::AdminState>,
    tenant: Tenant,
    headers: HeaderMap,
    SessionUser(user): SessionUser,
    Path(slug): Path<String>,
) -> Result<Response, AdminError> {
    if user.is_none() {
        return Ok(Redirect::to("/login").into_response());
    }
    let all = guides();
    let Some(pos) = all.iter().position(|g| g.slug == slug) else {
        return Ok(Redirect::to("/cms-admin/help").into_response());
    };
    let g = &all[pos];
    let mut ctx = Context::new();
    add_chrome(&mut ctx, &tenant, "help", user.as_ref()).await;
    add_admin_theme(&mut ctx, tenant.pool()).await;
    // The guide's own title is the page heading; drop it from the body.
    let body = g.markdown.split_once('\n').filter(|(first, _)| first.starts_with("# ")).map_or(g.markdown, |(_, rest)| rest);
    ctx.insert("title", &g.title);
    ctx.insert("section", g.section);
    ctx.insert("body_html", &crate::markdown::render(&rewrite_links(body)));
    ctx.insert("sections", &contents());
    ctx.insert("current", g.slug);
    ctx.insert("public_url", &format!("{DOCS_SITE}/{}", g.slug));
    let neighbour = |i: Option<usize>| i.and_then(|i| all.get(i)).map(|n| json!({ "slug": n.slug, "title": n.title }));
    ctx.insert("prev", &neighbour(pos.checked_sub(1)));
    ctx.insert("next", &neighbour(pos.checked_add(1)));
    render_with_csrf(&state, &headers, "rcms_admin/help_page.html", &mut ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_guide_is_bundled_with_a_title() {
        assert!(guides().len() >= 10, "{} guides", guides().len());
        for g in guides() {
            assert!(!g.title.is_empty() && !g.markdown.is_empty(), "{}", g.slug);
        }
        assert!(guides().iter().any(|g| g.slug == "themes"), "the themes guide is listed");
    }

    #[test]
    fn links_point_where_they_work_from_the_admin() {
        assert_eq!(target("themes.md"), "/cms-admin/help/themes");
        assert_eq!(target("themes.md#theme-packages"), "/cms-admin/help/themes#theme-packages");
        assert_eq!(target("img/admin-ui/x.png"), format!("{DOCS_SITE}/static/img/admin-ui/x.png"));
        assert_eq!(target("../examples/cms_demo"), format!("{REPO_DOCS}/../examples/cms_demo"));
        assert_eq!(target("https://example.com"), "https://example.com");
        assert_eq!(target("#section"), "#section");
        assert!(rewrite_links("![x](img/a.png) and [t](themes.md)").contains("/cms-admin/help/themes"));
    }

    #[test]
    fn a_snippet_shows_the_match_in_context() {
        let s = snippet("# T\n\nSome text before the **theme package** and after.", "theme package");
        assert!(s.contains("theme package"), "{s}");
        assert!(!s.contains("**"), "{s}");
        let long = format!("{} the theme package here", "word ".repeat(40));
        let s = snippet(&long, "theme package");
        assert!(s.starts_with("…word"), "starts at a word: {s}");
    }
}
