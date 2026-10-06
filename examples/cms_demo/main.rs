//! End-to-end demo for rustango-cms.
//!
//! Wires the lib's `router` + `ensure_seeded` into rustango's
//! manage runner. Two page types (HomePage, ArticlePage) are
//! registered via `register_page_type!`. Templates load from
//! `examples/cms_demo/templates/`. Migrations are read from the
//! lib's own `./migrations/` (the demo doesn't ship its own).
//!
//! This is the runnable companion to `docs/getting-started.md` — the same
//! two page types, the same typed extension table, the same templates. If a
//! step in the guide looks off, diff against this.
//!
//! ## Run (SQLite — no database server)
//!
//! ```sh
//! export DATABASE_URL="sqlite:./var/registry.db?mode=rwc"
//! ARGS="--example cms_demo --no-default-features --features sqlite"
//!
//! cargo run $ARGS -- migrate-registry
//! cargo run $ARGS -- create-tenant demo \
//!     --mode database \
//!     --database-url "sqlite:./var/demo.db?mode=rwc" \
//!     --host-pattern demo.localhost
//! cargo run $ARGS -- create-superuser demo admin
//! cargo run $ARGS -- migrate-tenants   # applies this example's 0004
//! cargo run $ARGS -- runserver
//! ```
//!
//! - Sign in: <http://demo.localhost:8080/login>
//! - Admin: <http://demo.localhost:8080/cms-admin/pages>
//! - Public pages: `http://demo.localhost:8080/<slug>/`
//! - Sitemap: <http://demo.localhost:8080/sitemap.xml>
//!
//! `migrations/0004_create_cms_article_page.json` is checked in; the CMS
//! baseline next to it is materialized at boot and gitignored. Regenerate
//! after changing `ArticleBody` with `cargo run $ARGS -- makemigrations`.
//!
//! ## Run (PostgreSQL)
//!
//! Same order, minus the SQLite-specific flags:
//!
//! ```sh
//! createdb rcms_demo
//! export DATABASE_URL=postgres://localhost/rcms_demo
//! export RUSTANGO_APEX_DOMAIN=localtest.me
//! cargo run --example cms_demo -- migrate-registry
//! cargo run --example cms_demo -- create-tenant demo
//! cargo run --example cms_demo -- create-superuser demo admin
//! cargo run --example cms_demo -- migrate-tenants
//! cargo run --example cms_demo -- runserver
//! ```

mod models;

use std::path::Path;
use std::sync::Arc;

use tera::Tera;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Force-link the page-type registrations into the binary's
    // inventory. Without these references the optimizer is allowed
    // to drop the static-init blocks that submit the handlers.
    let _ = std::any::type_name::<models::HomePage>();
    let _ = std::any::type_name::<models::ArticlePage>();
    let _ = std::any::type_name::<models::SectionedPage>();

    let templates_glob = format!(
        "{}/examples/cms_demo/templates/**/*.html",
        env!("CARGO_MANIFEST_DIR"),
    );
    let mut tera = Tera::new(&templates_glob)?;
    rustango_cms::admin::register_templates(&mut tera)?;
    // `stream_render(name="…")` — renders StreamField / page-builder zone
    // bodies pre-computed into the `_stream_html` context map.
    rustango_cms::block::tera_helpers::register_tera_function(&mut tera);
    let tera = Arc::new(tera);

    // This example owns its migrations, exactly like a generated project:
    // `ArticlePage`'s `cms_article_page` extension table is the *app's*
    // schema, not the library's, so it can't live in rustango-cms's own
    // `migrations/`. `materialize` copies the CMS baseline in here (skipping
    // files that already exist), then `cargo run --example cms_demo --
    // makemigrations` adds the extension table on top — the same two steps
    // the getting-started guide walks through.
    let migrations_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/cms_demo/migrations");
    rustango_cms::migrations::materialize(&migrations_dir)?;

    // Media storage backend (Slice 8.x) — see `install_media_backend`.
    rustango_cms::media_storage::install_from_env()?;

    // rustango 0.31+ mounts the tenant admin via explicit routes
    // for `routes.admin_url` only, so the public CMS can claim
    // site root through its own `.fallback()`. No more
    // `router_at("/p", _)` workaround.
    //
    // `with_login_required` wraps every `/cms-admin/*` route in the
    // framework's `auth_decorators::login_required` middleware:
    // anonymous visitors 302 to `/login?next=<original>` and resume
    // after auth. `/login` is rustango's default RouteConfig value —
    // change it here AND in `RouteConfig::login_url` if your tenant
    // admin uses a non-default login mount.
    // `public_router` carries the routes that must stay reachable
    // *without* auth: the admin stylesheet + JS bundles
    // (`/cms-admin/static/*`), the CMS-branded `/login` GET, the
    // password-reset screens, and the no-access page. It MUST be merged
    // alongside the protected router — otherwise the static assets 404
    // and the admin renders unstyled.
    let cms_admin = rustango_cms::admin::public_router(tera.clone()).merge(
        rustango_cms::admin::with_login_required(
            rustango_cms::admin::router(tera.clone()),
            "/login",
        ),
    );
    // #105 — mount the read-only JSON API v2 (`/api/v2/…`) so a headless
    // frontend can consume pages / menus / snippets / media over HTTP.
    // #587 — the MCP engine (agents create content over JSON-RPC at
    // /cms-admin/mcp, authenticated by user-owned keys minted in the
    // admin). Mounted OUTSIDE `with_login_required` — it carries its own
    // bearer auth. The 16 MiB body cap leaves room for base64 media
    // uploads through the `upload_media` tool.
    let mcp_settings = rustango::config::McpSettings {
        max_body_bytes: Some(16 * 1024 * 1024),
        ..Default::default()
    };
    // #members — SSO / social sign-in for the public members area.
    // Password sign-up / sign-in live in the CMS admin public_router
    // (`/members/login`, `/members/signup`); this adds the OAuth legs at
    // `/members/auth/sso/{slug}`, all minting the same member session so
    // page-gating treats every method uniformly. Providers are the
    // tenant's own `SsoProvider` rows (managed in the admin).
    let member_cfg = rustango::tenancy::MemberAuthConfig {
        login_base: "/members/auth".to_owned(),
        landing_url: "/".to_owned(),
        auto_provision: true,
        session_ttl: 7 * 24 * 60 * 60,
    };
    let api = cms_admin
        .merge(rustango_cms::mcp::router(&mcp_settings))
        // Headless JSON API (#568). CORS is off unless
        // `RCMS_API_CORS_ORIGINS` names the origins allowed to call it
        // (comma-separated, or `*` for reflect-any in development) — an
        // unset environment keeps this same-origin, as it has been.
        .merge(rustango_cms::api::router_with(&rustango_cms::api::Cors::from_env()))
        // /__media__/raw/{id} + /__media__/{filter_spec}/{id} — without this
        // every uploaded image (admin previews, the media picker's thumbs,
        // public pages) 404s. Caught by the E2E suite.
        .merge(rustango_cms::rendition_route::router())
        .merge(rustango::tenancy::member_sso_router(member_cfg))
        .merge(rustango_cms::router(tera));

    // The bundled cms-admin templates emit `{{ csrf_token | csrf_input | safe }}`
    // on every POST form; `Cli::with_csrf` wires the matching
    // `CsrfLayer` middleware so the tokens get *validated* on
    // submit (not just rendered). Production deployments serving
    // HTTPS should drop `allow_insecure_for_dev()` so the cookie
    // re-enables its `Secure` attribute.
    //
    // `exempt_prefix` is required, not optional: the analytics beacon that
    // every public page injects POSTs to `/__cms__/collect` via
    // `navigator.sendBeacon`, which can't attach an `X-CSRF-Token` — and a
    // CDN-cached page may never have received a CSRF cookie at all. Without
    // the exemption every beacon 403s and the analytics dashboard stays empty
    // with no visible error.
    // The MCP endpoint is bearer-authenticated JSON-RPC — it never trusts
    // cookies, so it must be CSRF-exempt or every POST 403s.
    let csrf_cfg = rustango::forms::csrf::CsrfConfig::default()
        .exempt_prefix(rustango_cms::analytics::COLLECT_PATH)
        .exempt_prefix(rustango_cms::mcp::MCP_PREFIX)
        // The v2 API is read-only (`GET` only), so there is nothing for
        // CSRF to protect — and without the exemption an attempted write
        // is rejected as `403 Forbidden` before routing, instead of the
        // `405 Method Not Allowed` that actually describes it.
        .exempt_prefix(rustango_cms::api::PREFIX)
        .allow_insecure_for_dev();

    rustango::manage::Cli::new()
        .tenancy()
        .api(api)
        .migrations_dir(migrations_dir)
        .with_csrf_config(csrf_cfg)
        .seed(|registry| {
            let pool = registry.clone();
            async move {
                rustango_cms::ensure_seeded(&pool).await?;
                // Publish scheduled pages and take expired ones down on
                // time, without waiting for an editor to open the admin.
                rustango_cms::spawn_schedule_sweeper(
                    &pool,
                    std::time::Duration::from_secs(60),
                    rustango_cms::cache_invalidate::noop(),
                );
                Ok(())
            }
        })
        .run()
        .await
}
