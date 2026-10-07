# Run Rustango-CMS in production

**Goal:** put a Rustango-CMS site on a server: secrets, database, domains, media, caching, search, the scheduler and email — and know what to back up.

**Who this is for:** developers and operators who deploy a site built with Rustango-CMS (for example one made with `rcms new`).

**Time:** about 30 minutes to read; the setup depends on your server.

## Before you start

- The site runs on your computer. If not, start with [Getting started](getting-started.md).
- You have a server with a reverse proxy that can do HTTPS (nginx, Caddy, a cloud load balancer…).
- You chose a database. **PostgreSQL** is the default and the one to use in production: full-text search and schema-per-tenant mode are PostgreSQL-only. MySQL 8+ and SQLite work too.

## 1. Build and copy

Build once, on your computer or in CI:

```sh
cargo build --release                                            # PostgreSQL
cargo build --release --no-default-features --features sqlite   # SQLite
```

On the server, put three things in one folder:

| Copy | Why |
|---|---|
| the binary from `target/release/` | the site |
| `templates/` | your public templates (read at start-up) |
| `migrations/` | the schema history the `migrate` command applies |

A project made with `rcms new` finds the last two through `RCMS_SITE_DIR`. Set it to that folder; without it the binary looks in the folder it was **built** in.

## 2. Secrets

Set these on the server, never in the repository. Each one must stay the same across restarts and be the **same on every server** of the site.

| Variable | What it protects | Without it |
|---|---|---|
| `RUSTANGO_ENV=prod` | turns on secure cookies, and makes a missing session secret stop the boot | cookies work over plain http, which is wrong in production |
| `RUSTANGO_SESSION_SECRET` | the login sessions of editors and members. Base64 of 32 bytes or more: `openssl rand -base64 32` | with `RUSTANGO_ENV=prod` the site does not start; otherwise a key file in `./var/` is used |
| `RUSTANGO_SECRET_KEY` | encrypts secrets stored in the database (form email recipients, single sign-on client secrets) | email notifications can't be saved; the admin says so |
| `RCMS_SECRET_KEY` | preview links, password-reset links, private-page grants, the admin's messages | the CMS generates a key once into `./var/.rustango_cms_signing.key` |
| `RCMS_RENDITION_SIGNING_KEY` | signs image URLs (`?s=…`), so nobody can ask the server for thousands of image sizes | image URLs are not signed |

> **Changing a secret** logs everybody out (`RUSTANGO_SESSION_SECRET`), breaks preview and reset links already sent (`RCMS_SECRET_KEY`), turns signed image URLs in cached pages into errors until the pages are rendered again (`RCMS_RENDITION_SIGNING_KEY`), or makes stored secrets unreadable (`RUSTANGO_SECRET_KEY` — there is no re-encryption). Choose them once.

Check your settings with:

```sh
./yoursite check --deploy
```

## 3. Database and migrations

Point `DATABASE_URL` at the **registry** database — the one that lists your tenants. Each tenant's own database or schema is recorded there.

On **every** deploy, before the new binary serves traffic:

```sh
./yoursite migrate          # the registry first, then every tenant
```

The CMS adds its own new migration files to `migrations/` at start-up, but it never **applies** migrations by itself. Run `makemigrations` on your computer when you change a page type made in code, and commit the files it writes.

**Backups.** There is no backup command. Back up:

- the database(s) with your database's own tool (`pg_dump` for PostgreSQL, a copy of the files for SQLite),
- `./var/` — uploaded media (with the default local storage) and the generated key files.

## 4. Domains and the reverse proxy

| Variable | Meaning | Default |
|---|---|---|
| `RUSTANGO_BIND` | where the site listens | `0.0.0.0:8080` |
| `RUSTANGO_APEX_DOMAIN` | your main domain. It never shows a tenant: it is for the operator console | `localhost` |

Each tenant answers on its **host pattern** (`create-tenant … --host-pattern shop.example.com`). Add more names with `add-host`:

```sh
./yoursite add-host shop www.shop.example.com
```

Editors can add names themselves under **Sites**. Names under the domains in `RCMS_SITE_HOST_SUFFIXES` (comma-separated) work at once; others wait for an operator to turn them on.

Let the site listen on `127.0.0.1` and put the proxy in front. A minimal nginx site:

```nginx
server {
    listen 443 ssl;
    server_name shop.example.com;
    # ssl_certificate / ssl_certificate_key …

    client_max_body_size 64m;          # media uploads
    add_header Strict-Transport-Security "max-age=31536000" always;
    add_header X-Content-Type-Options nosniff always;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;   # the CMS picks the tenant from it
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_http_version 1.1;        # MCP keeps an event stream open
        proxy_buffering off;
        proxy_read_timeout 1h;
    }
}
```

`Host` must reach the site unchanged: it is how a request finds its tenant. Don't add `X-Frame-Options: DENY` at the proxy: the editor shows its preview in a frame of the same site, and the CMS already sends `SAMEORIGIN` on those pages.

## 5. Media

By default uploads go to `./var/media` on the server's disk. For several servers, or to keep the disk small, use S3-compatible storage (AWS S3, Cloudflare R2, MinIO, …). Build with the `storage_s3` feature, call `rustango_cms::media_storage::install_from_env()?` at start-up (projects made with `rcms new` and the examples do), and set:

```sh
RCMS_MEDIA_BACKEND=s3
RCMS_S3_BUCKET=media
RCMS_S3_REGION=eu-central-1
RCMS_S3_ACCESS_KEY_ID=…
RCMS_S3_SECRET_ACCESS_KEY=…
RCMS_S3_ENDPOINT=https://…          # leave out for AWS
RCMS_S3_PATH_STYLE=false            # path-style is on unless this is "false"
```

One bucket serves every tenant: each tenant's files are under its own prefix. `RCMS_MEDIA_TENANT_BUCKETS=shop=shop-media,blog=blog-media` gives tenants their own buckets instead (they must exist).

Pages always serve media through the site (`/__media__/…`), so private pages' photos stay private. Public files are sent with a one-year `immutable` cache header, so a CDN in front of the site keeps them.

## 6. Page cache and CDN purging

The public router can cache whole pages:

```rust
let cache: rustango::cache::BoxedCache = /* a shared cache, e.g. rustango's RedisCache (feature `cache-redis`) */;
let public = rustango_cms::PublicRouter::new(tera).cached(cache.clone(), std::time::Duration::from_secs(300));
```

Signed-in visitors are never served from the cache. When an editor publishes, the CMS removes the page — give the admin and the scheduler the **same** cache:

```rust
let purge = std::sync::Arc::new(
    rustango_cms::cache_invalidate::BoxedCacheInvalidator::new(cache, "rcms:page")
        .with_tenant_hosts("shop", ["shop.example.com".to_owned()]),
);
let admin = rustango_cms::admin::router_with_invalidator(tera.clone(), purge.clone());
rustango_cms::spawn_schedule_sweeper(&pool, std::time::Duration::from_secs(60), purge);
```

A CDN in front of the site is purged the same way, with one of the optional backends (each is a cargo feature):

| Feature | Constructor | Needs |
|---|---|---|
| `cache_cloudflare` | `CloudflareInvalidator::new(zone_id, api_token)` | a token with *Zone · Cache Purge · Edit* |
| `cache_varnish` | `VarnishInvalidator::new(vec![urls])` | a VCL that accepts `BAN` from the site |
| `cache_cloudfront` | `CloudFrontInvalidator::from_env_distribution(id).await` | AWS credentials with `cloudfront:CreateInvalidation` |
| `cache_gcp` | `GoogleCloudCdnInvalidator::new(project, url_map, tokens)` | an OAuth2 access-token source |
| `cache_azure` | `AzureCdnInvalidator::new(subscription, group, profile, endpoint, tokens)` | an OAuth2 access-token source |

A failed purge is logged and never stops a save.

## 7. Search

On PostgreSQL the site and the API use full-text search with ranking; the CMS creates the index itself. On SQLite and MySQL search matches words inside titles, slugs and descriptions, without ranking.

For Elasticsearch, build with `search_elasticsearch` and install it at start-up:

```rust
use rustango_cms::search_elasticsearch::{boxed, ElasticsearchBackend};

let es = ElasticsearchBackend::new("http://es:9200", "rcms-")?;   // index per tenant: rcms-<slug>
// The first time only, fill each tenant's index, with that tenant's pool:
// es.reindex_all(&tenant_pool, "shop").await?;
rustango_cms::search::set_backend(boxed(es));
```

After the first fill, every save updates the index. The admin's own search stays on the database, because it must also find drafts.

## 8. The scheduler

Scheduled publishing and taking pages down need `spawn_schedule_sweeper` (see section 6; projects made with `rcms new` call it every 60 seconds). Without it, visitors still see the right pages, but listings, feeds, the cache and the search index only catch up when an editor opens the page list. It is safe to run on every server: each change is made once.

## 9. Email

The CMS sends email for form notifications, workflow steps and password resets through the mailer you give it (`admin::router_with_mailer`, `admin::public_router_with_mailer`, `PublicRouter::with_form_mailer`). The examples print emails to the console. For real email, enable rustango's `email-smtp` feature and build a mailer from your settings with `rustango::email::from_settings`, configured in `config/prod_settings.toml` or with variables such as `RUSTANGO__MAIL__SMTP_HOST`, `RUSTANGO__MAIL__SMTP_USERNAME` and `RUSTANGO__MAIL__SMTP_PASSWORD`.

## 10. Logs and health checks

- `RUST_LOG` sets the log level, for example `info,sqlx=warn`. [Logging](logging.md) lists every log target and shows JSON output.
- `RCMS_PERF_LOG=1` writes one line per page with the time of each step.
- `Cli::new()….with_health()` adds `/health` (the process runs) and `/ready` (the database answers) for your load balancer.

## Several servers

Everything above works with more than one server if:

- every server has the **same** secrets (section 2) — or shares `./var/.rustango_cms_signing.key`,
- media is on S3 (section 5), not on one server's disk,
- the page cache is shared (Redis, …), not in memory,
- `migrate` runs once per deploy, not once per server,
- login rate limits are counted per server: limit logins at the proxy too, or give the framework a shared store (`check --deploy` explains how).

## All settings

| Variable | Section |
|---|---|
| `DATABASE_URL` | 3 |
| `RUSTANGO_ENV`, `RUSTANGO_SESSION_SECRET`, `RUSTANGO_SECRET_KEY`, `RCMS_SECRET_KEY`, `RCMS_RENDITION_SIGNING_KEY` | 2 |
| `RUSTANGO_BIND`, `RUSTANGO_APEX_DOMAIN`, `RCMS_SITE_HOST_SUFFIXES` | 4 |
| `RCMS_MEDIA_BACKEND`, `RCMS_S3_*`, `RCMS_MEDIA_TENANT_BUCKETS`, `RCMS_MEDIA_TENANT_CDNS`, `RCMS_MEDIA_CDN_BASE` | 5 |
| `RCMS_API_CORS_ORIGINS` | [Headless JSON API](api.md) |
| `RCMS_DEFAULT_TIMEZONE`, `RCMS_DEFAULT_ADMIN_LOCALE` | the admin's time zone and language for users who chose none |
| `RCMS_NOTIFY_ALLOW_PRIVATE` | `1` lets notification webhooks reach private-network addresses |
| `RUST_LOG`, `RCMS_PERF_LOG` | 10 |
| `RCMS_SITE_DIR` | 1 (projects made with `rcms new`) |

## Check it worked

- `./yoursite check --deploy` lists no errors. Its warning about `RUSTANGO_BIND` on `127.0.0.1` is expected behind a reverse proxy.
- The login page answers over HTTPS, and its cookies carry `Secure`.
- After a restart you are still signed in.
- A page you publish shows up at once, even with the page cache on.
- A scheduled page goes live within a minute of its time.

## Next

- [Headless JSON API](api.md) — if another program shows your content.
- [Logging](logging.md) — log targets, JSON logs and OpenTelemetry.
