//! `rcms new <project>` — scaffold a new rustango-cms project.
//!
//! Emits a self-contained,
//! runnable CMS app (Cargo.toml, src/main.rs + models.rs, public
//! templates, the CMS baseline migrations, README, .env.example,
//! .gitignore). The skeleton + migrations are embedded at compile time,
//! so this tool has no runtime dependency on the rustango-cms build tree.
//!
//! ```sh
//! cargo run -p rcms-scaffold -- new myblog
//! # or, installed:  cargo install --path crates/rcms-scaffold && rcms new myblog
//! ```
//!
//! The generated project depends on the published rustango-cms release.
//! `--local` points it at this checkout instead, for working on the CMS
//! and a site together.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

// ---- embedded skeleton assets ----
const CARGO_TMPL: &str = include_str!("../assets/Cargo.toml.tmpl");
const MAIN_RS: &str = include_str!("../assets/main.rs");
const MODELS_RS: &str = include_str!("../assets/models.rs");
const HOME_HTML: &str = include_str!("../assets/home_page.html");
const ARTICLE_HTML: &str = include_str!("../assets/article_page.html");
// Shared starter styles, `{% include %}`d by both page templates so a
// generated site looks finished without an asset pipeline.
const SITE_CSS_HTML: &str = include_str!("../assets/site.css.html");
// blog template variant — ArticlePage gains a typed extension table.
const MODELS_BLOG_RS: &str = include_str!("../assets/models_blog.rs");
const ARTICLE_BLOG_HTML: &str = include_str!("../assets/article_page_blog.html");
const README_TMPL: &str = include_str!("../assets/README.md.tmpl");
const ENV_EXAMPLE: &str = include_str!("../assets/env.example");
const GITIGNORE: &str = include_str!("../assets/gitignore");
const BLOG_NOTE: &str = include_str!("../assets/blog_note.md");

// ---- embedded CMS baseline migrations ----
// The current set is embedded so a freshly-generated app's `migrations/` dir
// is complete on day one. The list is GENERATED from the repo's `migrations/`
// directory by `build.rs`, not written by hand: a hand-written list of
// filenames silently rotted when the migrations were squashed, and the
// scaffolder stopped compiling altogether (#613). Defines `MIGRATIONS:
// &[(&str, &str)]` — (migration name, file contents).
include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    if args.len() < 2 || args[1] == "-h" || args[1] == "--help" {
        print_usage();
        return Ok(());
    }
    if args[1] != "new" {
        return Err(format!("unknown command `{}` (expected `new`)", args[1]));
    }

    // Parse: new <name> [--dir <parent>] [--template <minimal|blog>] [--local]
    let mut name: Option<String> = None;
    let mut local = false;
    let mut dir: Option<String> = None;
    let mut template = String::from("minimal");
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                i += 1;
                dir = Some(args.get(i).ok_or("--dir needs a path")?.clone());
            }
            "--template" => {
                i += 1;
                template = args
                    .get(i)
                    .ok_or("--template needs a value (minimal|blog)")?
                    .clone();
            }
            "--local" => local = true,
            "-h" | "--help" => {
                print_usage();
                return Ok(());
            }
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            positional => {
                if name.is_some() {
                    return Err(format!("unexpected argument `{positional}`"));
                }
                name = Some(positional.to_owned());
            }
        }
        i += 1;
    }
    let name = name.ok_or("missing <project-name>\n\n  usage: rcms new <project-name> [--dir <parent>] [--template <minimal|blog>]")?;
    validate_name(&name)?;

    // Pick the template's page-type models + article template + README note.
    let (models_rs, article_html, template_note): (&str, &str, &str) = match template.as_str() {
        "minimal" => (MODELS_RS, ARTICLE_HTML, ""),
        "blog" => (MODELS_BLOG_RS, ARTICLE_BLOG_HTML, BLOG_NOTE),
        other => {
            return Err(format!(
                "unknown --template `{other}` (expected: minimal, blog)"
            ))
        }
    };

    let rcms_dep = if local {
        // The checkout this scaffolder was built from.
        // CARGO_MANIFEST_DIR = .../rustango-cms/crates/rcms-scaffold
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let rcms_root = manifest
            .parent()
            .and_then(Path::parent)
            .ok_or("can't locate the rustango-cms repo root from CARGO_MANIFEST_DIR")?;
        format!(
            "{{ path = {:?}, default-features = false }}",
            path_str(&canon(rcms_root)?)
        )
    } else {
        format!("{{ version = \"{RCMS_REQ}\", default-features = false }}")
    };

    // Target directory.
    let parent = dir.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let target = parent.join(&name);
    if target.exists() {
        return Err(format!("`{}` already exists", target.display()));
    }

    // Render + write.
    let cargo = CARGO_TMPL
        .replace("__NAME__", &name)
        .replace("__RCMS_DEP__", &rcms_dep)
        // Must match what rustango-cms itself requires, or the project
        // resolves two frameworks (#613).
        .replace("__RUSTANGO_REQ__", RUSTANGO_REQ);

    write(&target.join("Cargo.toml"), &cargo)?;
    write(
        &target.join("src/main.rs"),
        &MAIN_RS.replace("__NAME__", &name),
    )?;
    write(&target.join("src/models.rs"), models_rs)?;
    write(&target.join("templates/_site.css.html"), SITE_CSS_HTML)?;
    write(&target.join("templates/home_page.html"), HOME_HTML)?;
    write(&target.join("templates/article_page.html"), article_html)?;
    for (name, json) in MIGRATIONS {
        write(&target.join(format!("migrations/{name}.json")), json)?;
    }
    write(
        &target.join("README.md"),
        &README_TMPL
            .replace("__NAME__", &name)
            .replace("__TEMPLATE_NOTE__", template_note),
    )?;
    write(
        &target.join(".env.example"),
        &ENV_EXAMPLE.replace("__NAME__", &name),
    )?;
    write(&target.join(".gitignore"), GITIGNORE)?;

    println!(
        "Created rustango-cms project `{name}` ({template} template) at {}\n",
        target.display()
    );
    // Keep these in step with assets/README.md.tmpl — and only print commands
    // that have actually been run. The previous version led with
    // `init-tenancy` (a no-op since rustango 0.47), used a bare
    // `create-tenant` (which fails on sqlite: schema mode needs postgres), and
    // never created a user — so following it left you at a login page you
    // could not get past (#613).
    let sqlite_args = "--no-default-features --features sqlite";
    println!("Next — SQLite, no database server needed:");
    println!("  cd {}", target.display());
    println!("  export DATABASE_URL=\"sqlite:./var/registry.db?mode=rwc\"");
    println!("  cargo run {sqlite_args} -- migrate-registry");
    println!("  cargo run {sqlite_args} -- create-tenant demo \\");
    println!("      --mode database \\");
    println!("      --database-url \"sqlite:./var/demo.db?mode=rwc\" \\");
    println!("      --host-pattern demo.localhost");
    println!("  cargo run {sqlite_args} -- create-superuser demo admin");
    // The blog template's ArticlePage has a typed extension table that no
    // shipped migration creates — it's the app's own model, so the app has to
    // generate its migration. Without these two lines `cms_article_page`
    // doesn't exist and saving an article body fails.
    if template == "blog" {
        println!("  cargo run {sqlite_args} -- makemigrations   # emits cms_article_page");
        println!("  cargo run {sqlite_args} -- migrate-tenants");
    }
    println!("  cargo run {sqlite_args} -- runserver");
    println!("\nThen sign in at http://demo.localhost:8080/login");
    println!("and manage pages at http://demo.localhost:8080/cms-admin/pages");
    println!("\n(Postgres instead: see the generated README.)");
    Ok(())
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("project name is empty".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        || name.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        return Err(format!(
            "`{name}` is not a valid crate name (use letters, digits, `_`, `-`; don't start with a digit)"
        ));
    }
    Ok(())
}

fn write(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(path, contents).map_err(|e| format!("write {}: {e}", path.display()))
}

fn canon(p: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(p).map_err(|e| format!("resolve {}: {e}", p.display()))
}

fn path_str(p: &Path) -> String {
    p.display().to_string()
}

fn print_usage() {
    println!(
        "rcms — scaffold a rustango-cms project\n\n\
         USAGE:\n  \
           rcms new <project-name> [--dir <parent>] [--template <minimal|blog>] [--local]\n\n\
         Creates <parent>/<project-name>/ (default parent: current directory)\n\
         with a runnable CMS app: Cargo.toml, src/, templates/, migrations/,\n\
         README.md, .env.example, .gitignore.\n\n\
         Templates:\n  \
           minimal (default) — HomePage + ArticlePage, no typed extension\n  \
           blog              — ArticlePage with a typed cms_article_page\n                      \
             extension (Markdown body + hero image)\n\n\
         --local depends on this rustango-cms checkout by path instead of the\n\
         crates.io release."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded baseline must match the repo's `migrations/` directory.
    /// A hand-written list drifted when the migrations were squashed and broke
    /// the build outright; the list is generated now, and this asserts
    /// the generation actually saw something real.
    #[test]
    fn embedded_migrations_match_the_repo() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../migrations")
            .canonicalize()
            .expect("repo migrations/ dir exists");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .expect("read migrations/")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| p.file_stem()?.to_str().map(str::to_owned))
            .collect();
        on_disk.sort();

        let mut embedded: Vec<String> = MIGRATIONS.iter().map(|(n, _)| (*n).to_owned()).collect();
        embedded.sort();

        assert_eq!(
            embedded, on_disk,
            "embedded migrations are out of step with migrations/ — regenerate (build.rs reads the dir)"
        );
        assert!(!embedded.is_empty(), "no migrations embedded");
        for (name, body) in MIGRATIONS {
            assert!(!body.trim().is_empty(), "{name} embedded empty");
        }
    }

    /// The generated project's framework requirement must match what
    /// rustango-cms itself requires. When it didn't, `[patch.crates-io]`
    /// stopped applying and a generated project resolved two different
    /// `rustango` crates into one graph.
    #[test]
    fn generated_manifest_pins_the_workspace_framework_version() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let workspace = std::fs::read_to_string(root.join("Cargo.toml")).expect("root Cargo.toml");
        let want = workspace
            .lines()
            .find_map(|l| {
                l.trim_start()
                    .strip_prefix("rustango = { version = \"")?
                    .split('"')
                    .next()
                    .map(str::to_owned)
            })
            .expect("workspace declares a rustango requirement");

        assert_eq!(
            RUSTANGO_REQ, want,
            "scaffolded projects would request rustango {RUSTANGO_REQ} while rustango-cms requires {want}"
        );
        assert!(
            !CARGO_TMPL.contains("version = \"0.44\""),
            "template still hardcodes a framework version instead of __RUSTANGO_REQ__"
        );
    }

    /// A generated project asks crates.io for this checkout's release line,
    /// unless `--local` points it at the checkout.
    #[test]
    fn generated_manifest_depends_on_the_release() {
        assert!(RCMS_REQ.split('.').count() == 2, "RCMS_REQ is major.minor, got {RCMS_REQ}");
        assert!(
            env!("CARGO_PKG_VERSION").starts_with(&format!("{RCMS_REQ}.")),
            "scaffolder version {} is out of step with rustango-cms {RCMS_REQ}",
            env!("CARGO_PKG_VERSION")
        );
        assert!(CARGO_TMPL.contains("rustango-cms = __RCMS_DEP__"));
        assert!(!CARGO_TMPL.contains("__RCMS_PATH__"));
    }

    /// The analytics beacon POSTs via `navigator.sendBeacon`, which
    /// can't carry an `X-CSRF-Token`, so `/__cms__/collect` has to be exempt
    /// from CSRF. Every generated project shipped without the exemption:
    /// beacons 403'd, no pageview was ever recorded, and the only symptom was
    /// an empty analytics dashboard. Nothing else in the build can catch a
    /// missing builder call, so assert on the template text.
    #[test]
    fn generated_main_exempts_the_analytics_collector_from_csrf() {
        assert!(
            MAIN_RS.contains("exempt_prefix(rustango_cms::analytics::COLLECT_PATH)"),
            "generated main.rs must exempt the analytics collector from CSRF, or every \
             beacon in the generated site 403s silently (#619)"
        );
    }
}
