# Rustango-CMS

**A page-tree content management system for Rust: describe a page type once, and editors get a form for it, a place in the site tree, a live preview, revisions and a public URL.**

**Built on [rustango](https://github.com/ujeenet/rustango), [axum](https://github.com/tokio-rs/axum) and [Tera](https://keats.github.io/tera/).** The CMS is a set of `axum::Router`s and Tera templates you merge into your own rustango app, so the rest of your site stays plain Rust.

It runs multi-tenant from the start — every tenant gets its own page tree, media, users and settings — and the same source boots on **PostgreSQL, MySQL 8+ and SQLite**.

📚 **Docs:** [cms.rustango.com](https://cms.rustango.com) · [in-repo guides](docs/) · [API reference](https://docs.rs/rustango-cms)
🏺 **Tutorial:** [Build a ceramics shop](https://cms.rustango.com/shop-overview) — a whole site, step by step, for editors and developers, with a [runnable example](examples/ceramics_shop/).

---

## Contents

- [Features](#features)
- [Install](#install)
- [Quick start](#quick-start)
- [Try the demo](#try-the-demo)
- [Translations: chrome vs content](#translations-chrome-vs-content)
- [Logging](#logging)
- [Cargo features](#cargo-features)
- [Documentation](#documentation)
- [Contributing](#contributing)
- [License](#license)

## Features

- **Three ways to define content.** Page types as Rust structs (`#[derive(PageType)]`), page types built by editors in the admin with no code or migration, and reusable blocks (a StreamField body of headings, images, quotes, embeds, forms and your own `#[derive(Block)]` types).
- **An editor's admin at `/cms-admin/`.** Page tree with drag-and-drop, live preview, revisions and rollback, scheduled publishing and take-down, locking, approval workflows with email notifications, a media library with focal points and renditions, menus, categories, snippets, redirects, site settings and an accessibility checker.
- **Multi-language content.** Locales per tenant, side-by-side translation screens for pages, snippets, forms and menus, and a localised admin (English, Ukrainian, Polish, French, German, Simplified Chinese and Japanese).
- **A form builder.** Multi-step forms with conditional logic, submissions stored per form, and notification email.
- **Members and permissions.** Roles and codename permissions for editors; private pages and sections for signed-in members, with sign-up, login and single sign-on.
- **Headless when you want it.** A read/write JSON API under `/api/v2/`, locale-aware, and an MCP server so an AI agent can work in the CMS as a user, with its own keys.
- **Production pieces.** SEO fields, sitemap, RSS/Atom feeds, `robots.txt`, search (built-in or Elasticsearch), S3-compatible media storage, front-cache purging (Cloudflare, Varnish, CloudFront, Google Cloud CDN, Azure CDN) and CSRF protection on every admin form.

## Install

```toml
[dependencies]
rustango-cms = { git = "https://github.com/ujeenet/rustango-cms" }
rustango     = { version = "0.60", default-features = false, features = ["admin", "auth_flows", "cache", "cache-page", "config", "email", "forms", "manage", "passwords", "runtime", "signals", "signed_url", "tenancy", "template_views"] }
axum         = { version = "0.8", default-features = false, features = ["tokio", "http1", "json", "form", "query"] }
tera         = { version = "1.20", default-features = false }
serde        = { version = "1", features = ["derive"] }
```

The crate is not on crates.io yet; until the first release, depend on the repository as above. Pick the database with a feature: `postgres` is the default, and `default-features = false, features = ["sqlite"]` (or `"mysql"`) switches it — on both `rustango-cms` and `rustango`.

Use the same `rustango` minor version as the CMS (0.60 today). Two semver-incompatible copies give you two different `Tenant` and `Pool` types that do not unify. [UPGRADING.md](UPGRADING.md) has the notes for each version.

The fastest start is the scaffolder, which writes a ready project for you — see [Getting started](docs/getting-started.md).

## Quick start

A page type is a rustango model with a link to its page and one `#[field]` per box in the editor:

```rust
use rustango::sql::Auto;
use serde::{Deserialize, Serialize};

#[derive(rustango::Model, rustango_cms::PageType, Default, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "blog_article", app = "blog")]
#[page_type(type_name = "ArticlePage", verbose_name = "Article", template = "article.html")]
pub struct ArticlePage {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "cms_page", on = "id", unique)]
    pub page_id: i64,
    #[field(widget = MediaPicker, label = "Photo")]
    pub photo: Option<i64>,
    #[field(widget = Stream, label = "Body", allowed(heading, paragraph, image, quote))]
    pub body: Option<String>,
}

// Required; an empty impl keeps every default (parent/child rules,
// extra template context, …).
impl rustango_cms::PageTypeOverrides for ArticlePage {}
```

Then merge the CMS routers into your `manage` runner:

```rust
use std::path::Path;
use std::sync::Arc;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut tera = tera::Tera::new("templates/**/*.html")?;
    rustango_cms::admin::register_templates(&mut tera)?;
    rustango_cms::block::tera_helpers::register_tera_function(&mut tera);
    let tera = Arc::new(tera);

    // The CMS ships its migrations; this copies them next to yours.
    let migrations = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    rustango_cms::migrations::materialize(&migrations)?;

    // The admin router has no auth of its own — always wrap it.
    let admin = rustango_cms::admin::public_router(tera.clone()).merge(
        rustango_cms::admin::with_login_required(rustango_cms::admin::router(tera.clone()), "/login"),
    );
    let app = admin
        .merge(rustango_cms::api::router())
        .merge(rustango_cms::rendition_route::router())
        .merge(rustango_cms::router(tera)); // public pages: claims every other path

    let csrf = rustango::forms::csrf::CsrfConfig::default()
        .exempt_prefix(rustango_cms::analytics::COLLECT_PATH)
        .exempt_prefix(rustango_cms::api::PREFIX)
        .allow_insecure_for_dev(); // local http only — remove in production

    rustango::manage::Cli::new()
        .tenancy()
        .api(app)
        .migrations_dir(migrations)
        .with_csrf_config(csrf)
        .seed(|registry| {
            let pool = registry.clone();
            async move { rustango_cms::ensure_seeded(&pool).await?; Ok(()) }
        })
        .run()
        .await
}
```

`templates/article.html` receives the page, its fields and the rendered body:

```html
<article>
  <h1>{{ page.title }}</h1>
  {% if extension.photo %}<img src="{{ rcms_image_url(media_id=extension.photo, filter='fill-1200x600') }}" alt="">{% endif %}
  {{ _stream_html['body'] | safe }}
</article>
```

Generate the table with `cargo run -- makemigrations` and apply it with `cargo run -- migrate-tenants`. After that:

| Path | What answers |
|---|---|
| `/`, `/<slug>`, `/<slug>/<child>` | the public site — each page rendered with its type's template |
| `/cms-admin/` | the editor's admin (signed-in staff only) |
| `/api/v2/` | the headless JSON API |
| `/sitemap.xml`, `/robots.txt`, feeds | generated from the published pages |

Page types register themselves at start-up. In a small binary, name each type once (`let _ = std::any::type_name::<ArticlePage>();`) so the linker cannot drop it. The complete, runnable version of this is [`examples/cms_demo`](examples/cms_demo/); the [developer chapters](docs/shop-dev-helper-traits.md) of the tutorial cover the other helpers (menus, library types, taxonomies, site settings).

## Try the demo

The demo runs on SQLite, so there is no database server to install — the registry and each tenant are files:

```sh
export DATABASE_URL="sqlite:./var/registry.db?mode=rwc"
ARGS="--no-default-features --features sqlite --example cms_demo"

cargo run $ARGS -- migrate-registry
cargo run $ARGS -- create-tenant demo \
    --mode database \
    --database-url "sqlite:./var/demo.db?mode=rwc" \
    --host-pattern demo.localhost
cargo run $ARGS -- create-user demo admin --password 'change-me-please' --superuser
RUSTANGO_APEX_DOMAIN=localhost cargo run $ARGS -- runserver
```

Sign in at <http://demo.localhost:8080/login> as `admin`, then open <http://demo.localhost:8080/cms-admin/>. Browsers resolve `*.localhost` to your own machine; for `curl`, pass `-H 'Host: demo.localhost'`. `RUSTANGO_BIND` changes the default `0.0.0.0:8080`.

**PostgreSQL** (the default feature): `createdb rcms_demo`, `DATABASE_URL=postgres://localhost/rcms_demo`, drop `ARGS`'s feature flags and leave out `--mode database --database-url …`. **MySQL 8+**: use the `mysql` feature and `mysql://user:pass@host/db` URLs, and create the databases first.

Create pages in `/cms-admin/`, not in the generic rustango admin at `/__admin/` — only the CMS admin builds the tree paths and checks the page-type rules.

The [ceramics shop](examples/ceramics_shop/) is the larger example: the site the tutorial builds, with product pages, categories, an order form, a members area, a second language and a sale.

## Translations: chrome vs content

Templates have two translation tools. Pick by where the text comes from:

| | `translate` (rustango) | `t` (CMS) |
|---|---|---|
| **For** | text you typed into the template — buttons, headings | text from the database — this page's title, body, lead |
| **Stored in** | a message catalog, keyed by the text itself | `cms_translation` rows, per page, locale and field |
| **Locale** | the request's `LANG` | resolved for the active locale before render |

```html
{# chrome — the same label on every page #}
<button>{{ translate(key="Save changes") }}</button>

{# content — THIS page's title #}
<h1>{{ page.title | t(field="title", translations=translations) }}</h1>

{# content in a loop — each child against its own row #}
{% for c in children %}
  <a href="{{ c.url_path }}">{{ c.title | t(field="title", by_page=translations_by_page, page_id=c.id) }}</a>
{% endfor %}
```

Both fall back to the original text when a translation is missing, so a half-translated site still renders. Register them once when you build Tera:

```rust
rustango::i18n::tera_tags::register(&mut tera, translator); // `translate`
rustango_cms::translation::register_tera_filter(&mut tera); // `t`
```

## Logging

`#[rustango::main]` installs a `tracing` subscriber (`info,sqlx=warn` by default). Change it with `RUST_LOG`:

```sh
RUST_LOG=info,rustango_cms::render=debug cargo run   # one module in detail
RUSTANGO_LOG_FORMAT=json cargo run                   # JSON lines for production
```

[docs/logging.md](docs/logging.md) lists every log target and the OpenTelemetry setup.

## Cargo features

| Feature | Effect |
|---|---|
| `postgres` (default), `sqlite`, `mysql` | Database backend, forwarded to rustango |
| `storage_s3` | S3-compatible media storage (AWS S3, Cloudflare R2, MinIO, …) |
| `cache_cloudflare`, `cache_varnish`, `cache_cloudfront`, `cache_gcp`, `cache_azure` | Purge a front cache when pages are published |
| `search_elasticsearch` | Elasticsearch as the search backend |
| `avif` | AVIF image renditions (pulls an AV1 encoder; slower build) |
| `test_utils` | `Tenant::for_test` for your integration tests |

## Documentation

- **[cms.rustango.com](https://cms.rustango.com)** — everything below, as a website.
- **[What is Rustango-CMS?](docs/cms-overview.md)** and **[Getting started](docs/getting-started.md)** — from an empty folder to a blog in about 20 minutes.
- **For editors** — [find your way](docs/admin-find-your-way.md), [your first page](docs/admin-first-page.md), [live preview](docs/admin-live-preview.md).
- **[Build a ceramics shop](docs/shop-overview.md)** — the tutorial, 15 editor chapters and 2 developer chapters.
- **[Headless API](docs/api.md)**, **[per-tenant templates](docs/tenant-templates.md)**, **[Google sign-in](docs/sso-google-setup.md)**.
- **[API reference on docs.rs](https://docs.rs/rustango-cms)** — every module, trait and type.
- [CHANGELOG.md](CHANGELOG.md) and [UPGRADING.md](UPGRADING.md).

## Contributing

Issues and pull requests are welcome — [CONTRIBUTING.md](CONTRIBUTING.md) explains the local checks a change must pass. Please report security problems privately, as described in [SECURITY.md](SECURITY.md).

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
