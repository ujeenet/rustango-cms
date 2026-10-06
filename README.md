# rustango-cms

A page-tree content management system for Rust, built on top of [rustango](https://crates.io/crates/rustango).

`rustango-cms` provides:

- An abstract `Page` model with parent/child tree traversal (materialized-path) and multi-table inheritance (typed extension tables one-to-one back to `cms_page`).
- A `PageType` registry table + a `PageTypeHandler` trait registered via `inventory` for deep customization.
- A fallback URL handler that resolves slug paths to pages and renders them through rustango's existing Tera-based template layer.

Tenant-aware from day one — every read and write goes through the tenant's own pool (`rustango::extractors::Tenant::pool()`), and migrations register with `MigrationScope::Tenant` so each tenant gets its own `cms_page` and `cms_page_type` tables.

## Status

**v0.1 — pre-alpha.** Public surface stable enough to build on; expect rapid iteration before tagging.

## What's new

The big arc since the 0.39 tenancy refactor ([issue #1](https://github.com/ujeenet/rustango-cms/issues/1)) was bringing the CMS onto the framework's **v0.40–v0.50** surface, replacing code the CMS used to hand-roll. Now on **rustango 0.60**:

- **Tri-dialect** — boots on Postgres, MySQL 8+, and SQLite from one source; admin auth (`with_login_required`, the `SessionUser` save-flow guards) works on all three, not just PG.
- **Framework modules** — public `/sitemap.xml` via `rustango::sitemaps`, table-driven 301/302 `rustango::redirects`, RSS/Atom via `rustango::syndication`, flash messages via `rustango::messages`, named URL reversal + `{% url %}` / `{% csrf_token %}` / `{% querystring %}` template tags, `Paginator` for the list views, and `humanize` filters — replacing the CMS's own copies.
- **ORM adoption** — `QuerySet::first(pool)` for single-row lookups and `QuerySet::iterator(chunk_size)` for memory-bounded full-tree rebuilds, plus the `atomic!` + `on_commit` transaction surface.
- **Auth + security** — CSPRNG sweep (`OsRng` at every token boundary), CSRF on every admin POST, and `@login_required`-equivalent gating on all of `/cms-admin/`.
- **Real WYSIWYG** — the richtext fields now use a vendored [TipTap](https://tiptap.dev) editor (headless, MIT) with a formatting toolbar, tables, images, and **move-safe internal links** (`<a linktype="page|media" id="…">` resolved at render time, so links survive page/media moves). See `src/admin/static/vendor/tiptap.bundle.README.md` to rebuild it.
- **Admin polish** — the page editor gained a three-action save footer (Save / Save & keep editing / Save & add another).

## Quick start

Add to your `Cargo.toml`:

```toml
[dependencies]
rustango     = { version = "0.60", default-features = false, features = ["admin", "auth_flows", "cache", "cache-page", "config", "email", "forms", "manage", "passwords", "runtime", "signals", "signed_url", "tenancy", "template_views"] }
rustango-cms = { git = "https://github.com/ujeenet/rustango-cms" }   # crates.io release coming
axum         = { version = "0.8", default-features = false, features = ["tokio", "http1", "json", "form", "query"] }
tera         = { version = "1.20", default-features = false }
async-trait  = "0.1"
```

> **Heads up:** rustango-cms tracks the current framework line and
> requires `rustango 0.60`. Use the same rustango version as the CMS —
> two semver-incompatible copies give you distinct `Tenant` / `Pool` types
> that do not unify. Tri-dialect support landed back in 0.39
> (dialect-agnostic transactions, tri-dialect `SchemaChange` DDL,
> `SeedFn` lifted to `&Pool`), so the CMS boots on Postgres, MySQL 8+,
> and SQLite from the same source. The framework's v0.40–v0.50 surface
> (sitemaps, redirects, RSS/Atom syndication, `{% url %}` /
> `{% csrf_token %}` / `{% querystring %}` / humanize Tera helpers,
> named URL reversal, `atomic!` + `on_commit`, `Paginator` /
> `CursorPaginator`, `QuerySet::first`/`iterator`, FTS + trigram
> lookups) is now adopted through this crate — see the
> [What's new](#whats-new) section below and
> [issue #1](https://github.com/ujeenet/rustango-cms/issues/1) for the
> per-section breakdown.
>
> `tokio` no longer needs to be a direct dependency of your app —
> `#[rustango::main]` resolves it through rustango's
> `__private_runtime` re-export since 0.31.1.

Register a page type:

```rust
use async_trait::async_trait;
use rustango_cms::{register_page_type, PageTypeHandler};

#[derive(Default)]
pub struct ArticlePage;

#[async_trait]
impl PageTypeHandler for ArticlePage {
    fn app_label(&self) -> &'static str { "blog" }
    fn type_name(&self) -> &'static str { "ArticlePage" }
    fn verbose_name(&self) -> &'static str { "Article" }
    fn default_template(&self) -> &'static str { "article.html" }
}

register_page_type!(ArticlePage);
```

Wire the routers into your `manage` runner:

```rust
use std::sync::Arc;
use tera::Tera;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut tera = Tera::new("templates/**/*.html")?;
    rustango_cms::admin::register_templates(&mut tera)?;
    let tera = Arc::new(tera);

    // CMS admin at /cms-admin/...; public pages + /sitemap.xml at
    // the site root. (rustango 0.31+ mounts the tenant admin via
    // explicit routes for `routes.admin_url` only, so this
    // `.fallback()`-based public router is allowed to claim
    // everything else.)
    // Protect every `/cms-admin/*` route with the framework's
    // `auth_decorators::login_required`. Anonymous visitors are
    // 302'd to `/login?next=<original>` and resumed after auth.
    let cms_admin = rustango_cms::admin::with_login_required(
        rustango_cms::admin::router(tera.clone()),
        "/login",
    );
    let api = cms_admin.merge(rustango_cms::router(tera));

    // Every cms-admin POST form carries a `{{ csrf_token | csrf_input | safe }}`
    // input; the matching `CsrfLayer` middleware on `Cli::with_csrf*`
    // validates it on submit. Drop `allow_insecure_for_dev()` in
    // production (it disables the cookie's `Secure` attribute,
    // which is fine for `http://localhost` but unsafe on the open
    // internet).
    let csrf_cfg = rustango::forms::csrf::CsrfConfig::default().allow_insecure_for_dev();

    rustango::manage::Cli::new()
        .tenancy()
        .api(api)
        // Serve your own static assets (public-template CSS/JS, favicons,
        // …) at /static/. NOTE: the source path is resolved relative to
        // the process's *current working directory* — unlike templates /
        // migrations, which resolve via CARGO_MANIFEST_DIR. Pass a
        // manifest-relative path so it works no matter where the binary
        // is launched from. The cms-admin mounts its bundled assets
        // separately, so this is purely for your host project.
        .with_static("/static", concat!(env!("CARGO_MANIFEST_DIR"), "/static"))
        // Explicit tracing setup. `#[rustango::main]` already installs a
        // basic fmt subscriber via `try_init`, so this is idempotent — it
        // matters when you don't use the macro, or want the Cli to own
        // logging. Tune verbosity with `RUST_LOG` (see Logging below).
        .with_logging()
        // CSRF: `.with_csrf()` is the one-liner; `with_csrf_config` (here)
        // additionally relaxes the cookie's `Secure` flag for local http
        // dev — drop `allow_insecure_for_dev()` in production.
        .with_csrf_config(csrf_cfg)
        .seed(|registry| {
            let pool = registry.clone();
            async move { rustango_cms::ensure_seeded(&pool).await?; Ok(()) }
        })
        .run()
        .await
}
```

After that:

- `/` → CMS root page
- `/<slug>`, `/<slug>/<subslug>` → CMS resolver
- `/sitemap.xml` → sitemaps.org XML of every published,
  indexable page in the current tenant (powered by
  [`rustango::sitemaps`](https://docs.rs/rustango))
- `/admin/*` → rustango tenant admin
- `/cms-admin/pages` → CMS-aware admin (path/depth/sort_order
  computed correctly, type whitelists enforced — distinct from the
  generic rustango admin, which won't compute the tree for you)
- Anything else → CMS `404 — Page not found: /<path>`

## Demo

A working example lives under [`examples/cms_demo/`](examples/cms_demo/) with two page types and matching templates. Run end-to-end:

```sh
createdb rcms_demo
export DATABASE_URL=postgres://localhost/rcms_demo
cargo run --example cms_demo -- migrate-registry          # registry schema (orgs, …)
cargo run --example cms_demo -- create-tenant demo --host-pattern demo.localhost
cargo run --example cms_demo -- create-user demo admin --password 'change-me-please' --superuser
RUSTANGO_APEX_DOMAIN=localhost cargo run --example cms_demo -- runserver
```

Then sign in at `http://demo.localhost:8080/login` as `admin` and manage pages through the R-CMS admin at `http://demo.localhost:8080/cms-admin/pages` (handles root + child creation, applies the type whitelist, computes materialized path / depth / sort order) and visit the rendered pages at `http://demo.localhost:8080/<slug>/`. `create-tenant` applies the tenant migrations itself, and without `--host-pattern` requests cannot be routed to the tenant (the login page answers 404). Browsers resolve `*.localhost` to loopback; for `curl`, pass `-H 'Host: demo.localhost'`. Set `RUSTANGO_BIND` to change the default `0.0.0.0:8080`.

> ⚠️ Don't use the framework admin at `/__admin/cms_page/` to create pages — it goes through a raw INSERT path that leaves `path` empty and skips type validation. Use `/cms-admin/` instead. Read-only viewing of `cms_page` rows in the framework admin is fine.

### Trying it on SQLite (no database server)

The CMS is tri-dialect — the demo runs end-to-end on SQLite with **no DB server to install**: the registry and each tenant are just files. Build with the `sqlite` feature instead of the default `postgres`, and use database-mode tenancy (SQLite has no schemas, so each tenant is its own file):

```sh
export DATABASE_URL="sqlite:./var/registry.db?mode=rwc"
ARGS="--no-default-features --features sqlite --example cms_demo"

cargo run $ARGS -- migrate-registry    # create the registry schema (orgs, …)
cargo run $ARGS -- create-tenant demo \
    --mode database \
    --database-url "sqlite:./var/demo.db?mode=rwc" \
    --host-pattern demo.localhost      # database-mode needs an explicit host
cargo run $ARGS -- create-user demo admin --password 'change-me-please' --superuser
RUSTANGO_APEX_DOMAIN=localhost cargo run $ARGS -- runserver
```

Then the admin is at `http://demo.localhost:8080/cms-admin/pages`, the sitemap at `http://demo.localhost:8080/sitemap.xml`, and pages render at `http://demo.localhost:8080/<slug>/`.

**MySQL 8+** works the same way — swap the feature to `mysql` and use `mysql://user:pass@host/db` URLs (pre-create the registry + tenant databases, since MySQL won't auto-create them the way SQLite creates files). Verified against MySQL 8.0.

### Auth on sqlite / mysql builds

`rustango_cms::admin::with_login_required(...)` is tri-dialect as of
#258 — anonymous traffic to `/cms-admin/*` 302s to the configured
login URL on every backend. A signed-in user also needs the
`cms_admin.access` codename (every seeded role carries it) or to be a
superuser; anyone else — a self-registered member, say — is sent to
`/cms-admin/no-access` (#671). The framework's `SessionUser` extractor
(used by every save-flow guard in the admin) became tri-dialect in
rustango #317, so a logged-in editor's session cookie decodes
correctly under sqlite + mysql just like under postgres.

If a POST save returns a 401 "session expired" page on a non-PG
build despite being logged in, double-check:

1. The login flow itself succeeded (browser carries a
   `rustango_tenant_session` cookie scoped to the tenant slug).
2. The CSRF middleware accepts the request — multipart forms hit
   the JS hijack from #262 to send `X-CSRF-Token` as a header;
   non-multipart admin forms include `{% raw %}{{ csrf_token | csrf_input | safe }}{% endraw %}`.
3. The handler is reachable through `with_login_required`'s gate —
   not bypassed by a custom router layer that drops the layer.

The framework's `auth_decorators::login_required` decorator is
still PG-gated and `unimplemented` on sqlite/mysql, but
`rustango-cms` no longer routes through it — see `with_login_required`
in `src/admin/mod.rs`.

### Developing against a local rustango checkout

The workspace builds against crates.io rustango. To work on the framework
and the CMS together, clone rustango beside this repo and copy
`.cargo/config.toml.example` to `.cargo/config.toml` (gitignored) — it holds
the `[patch.crates-io]` onto `../rustango`. Cargo then rewrites the two
rustango entries in `Cargo.lock` to path sources; don't commit that diff.

## Cookbook — building a typed blog (v0.2)

A typed page in rustango-cms is one of two halves:

1. The `PageTypeHandler` impl + `register_page_type!` registration (covered above).
2. A user-authored **extension table** that holds the typed fields. Multi-table inheritance — `cms_page` has the shared tree-and-status fields; the extension table holds the per-type stuff.

This chapter walks the `ArticlePage` shape — title + canonical metadata in `cms_page`, body Markdown + hero image FK in `cms_article_page`.

### 1. Define the extension model

```rust
use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "cms_article_page", app = "blog")]
pub struct ArticlePageExt {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// 1:1 with `cms_page.id`. Same row's `cms_article_page` carries
    /// the typed body + hero image.
    #[rustango(fk = "cms_page", on = "id", index, unique)]
    pub page_id: i64,
    /// Body content as raw Markdown. Renderer pipes through whichever
    /// MD library the host crate prefers (Tera filter wired in main).
    pub body_markdown: String,
    /// Optional FK to `cms_media` for the lead image. Tera template
    /// emits `{{ rcms_image_url(media_id=ext.hero_media_id, filter='fill-1200x600') }}`.
    pub hero_media_id: Option<i64>,
    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}
```

### 2. Wire `load_extension` on the handler

Override the default `load_extension` to fetch the matching `cms_article_page` row and return it as JSON. Tera templates read it from `extension` in the context.

```rust
use async_trait::async_trait;
use rustango::core::Column as _;
use rustango::sql::Fetcher as _;
use rustango_cms::{register_page_type, PageTypeHandler};

#[derive(Default)]
pub struct ArticlePage;

#[async_trait]
impl PageTypeHandler for ArticlePage {
    fn app_label(&self) -> &'static str { "blog" }
    fn type_name(&self) -> &'static str { "ArticlePage" }
    fn verbose_name(&self) -> &'static str { "Article" }
    fn default_template(&self) -> &'static str { "article.html" }
    fn allowed_parent_types(&self) -> &'static [&'static str] { &["BlogIndexPage"] }

    async fn load_extension(
        &self,
        pool: &rustango::sql::Pool,
        page_id: i64,
    ) -> Result<serde_json::Value, rustango::sql::ExecError> {
        let mut hits: Vec<ArticlePageExt> = ArticlePageExt::objects()
            .where_(ArticlePageExt::page_id.eq(page_id))
            .fetch_pool(pool)
            .await?;
        Ok(hits
            .pop()
            .and_then(|ext| serde_json::to_value(ext).ok())
            .unwrap_or(serde_json::Value::Null))
    }
}

register_page_type!(ArticlePage);
```

### 3. Render the extension in the template

```html
{# templates/article.html #}
<article>
  <h1>{{ page.title | t(field="title", translations=translations) }}</h1>
  {% if extension.hero_media_id %}
    <img src="{{ rcms_image_url(media_id=extension.hero_media_id, filter='fill-1200x600') }}"
         alt="{{ page.title }}">
  {% endif %}
  {{ extension.body_markdown }}
</article>
```

The `t` filter is the Slice 4 translation substitution; `rcms_image_url` is the Slice 5b rendition resolver. Both no-op gracefully if their data is missing.

### 4. Migration

After defining the extension model, `cargo run -- makemigrations` emits the migration. Apply with `cargo run -- migrate`. The CMS-shipped migrations materialize automatically on first boot (see [`migrations.rs`](src/migrations.rs)) — your host project doesn't need to copy them by hand.

### 5. Author content via the admin

1. Upload a hero image: `/cms-admin/media/upload?kind=image`. Note the media id (visible in the grid).
2. Create the page: `/cms-admin/pages/new` → `ArticlePage` type, fill in title + slug + SEO fields.
3. Edit the extension fields **directly in the page editor**: when the handler declares `extension_fields()` / `widgets()` (with `save_extension()` / `preview_extension()`), the generic page-edit form renders those inputs and persists them on save — no framework-admin detour required.

The framework admin at `/admin/cms_article_page/` still works as a raw-DB-CRUD fallback for any fields the handler doesn't surface.

### 6. Verify

- `/your-slug` → public render with hero image + body
- Edit the page in `/cms-admin/pages/<id>/edit` → Revisions panel grows by one per save
- Slug rename → URL changes, descendants cascade, old URL evicts from cache
- Drag a row in the tree view → reorder / reparent + URL re-materialization
- Add a translation row (`/admin/cms_translation/`) for `(page_id, locale_id, field_path='title')` → `?lang=<locale>` request swaps the title

## i18n — chrome (`translate`) vs content (`t`)

rustango-cms templates have **two** translation mechanisms, and they're easy to confuse because both read "translate the text." They operate at different layers — pick by *where the string comes from*:

| | `translate` (framework) | `t` (CMS) |
|---|---|---|
| **For** | UI **chrome** — fixed strings you wrote in the template (button labels, section headings) | page **content** — dynamic per-row values (this page's title, body, lead) |
| **Backed by** | a gettext-shape message catalog, keyed by the string itself | `cms_translation` rows keyed by `(page_id, locale, field_path)` |
| **Shape** | the `translate` function **or** filter, from `rustango::i18n::tera_tags` | the `t` filter, from `rustango_cms::translation` (see syntax below) |
| **Locale** | picked from the request context (`LANG`) | the `translations` map is resolved for the active locale before render |

**Rule of thumb:** a string you typed *into the template* is chrome → `translate`; a value that came *out of the database* is content → `t`.

```html
{# chrome — same label on every page, translated per locale #}
<button>{{ translate(key="Save changes") }}</button>

{# content — THIS page's title, translated via its cms_translation rows #}
<h1>{{ page.title | t(field="title", translations=translations) }}</h1>

{# content, loop form — each child translated against its own row #}
{% for c in children %}
  <a href="{{ c.url_path }}">{{ c.title | t(field="title", by_page=translations_by_page, page_id=c.id) }}</a>
{% endfor %}
```

Register the framework helper once when you build Tera:

```rust
// `translator` is your loaded message catalog (Arc<rustango::i18n::Translator>).
rustango::i18n::tera_tags::register(&mut tera, translator);
rustango_cms::translation::register_tera_filter(&mut tera); // the `t` filter
```

Both forms **fall back to the original / untranslated value** when a catalog entry or `cms_translation` row is missing, so a partially-translated site degrades gracefully rather than rendering blanks.

## Logging

`#[rustango::main]` auto-installs a `tracing_subscriber::fmt`
subscriber with env-filter (`info,sqlx=warn` default). Override with
`RUST_LOG`:

```sh
# Verbose for one module
RUST_LOG=info,rustango_cms::render=debug cargo run

# Quiet upstream noise
RUST_LOG=info,sqlx=warn,hyper=warn cargo run

# Production JSON output (see docs/logging.md for the manual setup)
RUSTANGO_LOG_FORMAT=json cargo run
```

The full target catalog (25 modules — `render`, `rendition_route`,
`page_log`, `workflow_mail`, etc.) plus prod / OTel patterns are
documented in [docs/logging.md](docs/logging.md).

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

The admin embeds third-party assets under their own licences — TipTap,
ProseMirror and CodeMirror (MIT), axe-core (MPL-2.0), the Hanken Grotesk and
Literata fonts (SIL OFL 1.1) and Material Symbols (Apache-2.0). Their notices
are in [THIRD-PARTY-NOTICES.txt](src/admin/static/vendor/THIRD-PARTY-NOTICES.txt),
which is compiled into the binary and served at
`/cms-admin/static/vendor/THIRD-PARTY-NOTICES.txt`.

The example shop (`examples/ceramics_shop`) ships compiled Tailwind CSS
3.4.19 (MIT, notice kept in `static/shop.css`) and photos from Wikimedia
Commons under CC0, CC BY 4.0 and CC BY-SA 4.0 — see
[its photo credits](examples/ceramics_shop/photos/CREDITS.md). The
screenshots in `docs/img/shop` show those photos.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
