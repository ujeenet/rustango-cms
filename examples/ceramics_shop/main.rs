//! Clay & Kiln — the handmade ceramics shop the documentation tutorial
//! builds, step by step, in the admin.
//!
//! Unlike `cms_demo`, this host wires every feature the tutorial uses:
//! per-tenant templates (so a page type made in the admin can get its own
//! template), a mailer for order-form and workflow emails, `/fr/…` style
//! language URLs, the members area, media renditions, the JSON API and the
//! schedule sweeper. The shop's content — products, categories, menus, the
//! order form — is made in the admin by following the tutorial.
//!
//! ## Run (SQLite — no database server)
//!
//! ```sh
//! export DATABASE_URL="sqlite:./var/registry.db?mode=rwc"
//! export RUSTANGO_SECRET_KEY="change-me"   # encrypts stored secrets; forms need it to send email
//! ARGS="--example ceramics_shop --no-default-features --features sqlite"
//!
//! cargo run $ARGS -- migrate-registry
//! cargo run $ARGS -- create-tenant shop \
//!     --mode database \
//!     --database-url "sqlite:./var/shop.db?mode=rwc" \
//!     --host-pattern shop.localhost
//! cargo run $ARGS -- create-superuser shop owner
//! cargo run $ARGS -- migrate-tenants
//! cargo run $ARGS -- runserver
//! ```
//!
//! Then sign in at <http://shop.localhost:8080/login>. Emails (order-form
//! notifications, workflow steps, password resets) are printed to the
//! console, so the tutorial can show them without a mail server.
//!
//! The headless chapters also need, before `runserver`:
//!
//! ```sh
//! export RCMS_API_CORS_ORIGINS="http://localhost:8250"   # lets headless/ call the API
//! ```
//!
//! and serve the storefront with `python3 -m http.server 8250` from
//! `examples/ceramics_shop/headless/`.

mod models;

use std::path::Path;
use std::sync::Arc;

use rustango_cms::tenant_templates::{self, TenantTemplates};
use rustango_cms::{LocaleMode, PublicRouter};
use tera::Tera;

/// The `From:` address on the shop's emails.
const SHOP_EMAIL: &str = "orders@clayandkiln.test";

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Keep the code-registered types in the binary's inventory.
    let _ = std::any::type_name::<models::HomePage>();
    let _ = std::any::type_name::<models::ShopPage>();
    let _ = std::any::type_name::<models::ContentPage>();
    let _ = std::any::type_name::<models::Workshop>();
    let _ = std::any::type_name::<models::Information>();
    let _ = std::any::type_name::<models::Clay>();
    // The Form Builder's `form` block, so the order form can be placed on pages.
    let _ = std::any::type_name::<rustango_cms::forms::block::FormBlock>();

    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/ceramics_shop");
    let mut tera = Tera::new(&format!("{}/templates/**/*.html", base.display()))?;
    rustango_cms::admin::register_templates(&mut tera)?;
    rustango_cms::block::tera_helpers::register_tera_function(&mut tera);
    // `shop_css_version()` — a hash of the compiled stylesheet, so a rebuilt
    // `static/shop.css` gets a new URL instead of the browser's cached copy.
    let css_version = file_hash(&base.join("static/shop.css"));
    tera.register_function("shop_css_version", move |_: &std::collections::HashMap<String, tera::Value>| {
        Ok(tera::Value::String(css_version.clone()))
    });
    let tera = Arc::new(tera);

    // Per-tenant templates: the admin's Templates screen and the "template"
    // choice of a page type made in the admin both need them. One shared
    // instance for the renderer, the editor and the MCP tools.
    let overrides = Arc::new(
        TenantTemplates::new(Arc::clone(&tera), base.join("var/templates_tenants"))
            .with_global_dir(base.join("templates")),
    );
    tenant_templates::install(Arc::clone(&overrides));

    let migrations_dir = base.join("migrations");
    rustango_cms::migrations::materialize(&migrations_dir)?;
    rustango_cms::media_storage::install_from_env()?;

    // Emails go to the console; swap in an SMTP mailer for a real shop.
    let mailer: Arc<dyn rustango::email::Mailer> = Arc::new(rustango::email::ConsoleMailer);

    let admin = rustango_cms::admin::public_router_with_mailer(tera.clone(), mailer.clone(), SHOP_EMAIL).merge(
        rustango_cms::admin::with_login_required(
            rustango_cms::admin::router_with_mailer(
                tera.clone(),
                rustango_cms::cache_invalidate::noop(),
                mailer.clone(),
                SHOP_EMAIL,
            ),
            "/login",
        ),
    );

    // The shop's stylesheet, compiled by Tailwind (see `tailwind.config.js`).
    let public = PublicRouter::new(tera)
        .static_dir("/static", base.join("static"))
        .with_form_mailer(mailer, SHOP_EMAIL)
        .locale_mode(LocaleMode::PathOrQuery)
        .tenant_templates_shared(overrides)
        .build();

    let members = rustango::tenancy::MemberAuthConfig {
        login_base: "/members/auth".to_owned(),
        landing_url: "/".to_owned(),
        auto_provision: true,
        session_ttl: 7 * 24 * 60 * 60,
    };
    let mcp_settings = rustango::config::McpSettings {
        max_body_bytes: Some(16 * 1024 * 1024),
        ..Default::default()
    };
    let app = admin
        .merge(rustango_cms::mcp::router(&mcp_settings))
        .merge(rustango_cms::api::router_with(&rustango_cms::api::Cors::from_env()))
        .merge(rustango_cms::rendition_route::router())
        .merge(rustango::tenancy::member_sso_router(members))
        .merge(public);

    // The analytics beacon, the MCP endpoint and the read-only API can't
    // carry a CSRF token, so they are exempt (see cms_demo for the detail).
    let csrf = rustango::forms::csrf::CsrfConfig::default()
        .exempt_prefix(rustango_cms::analytics::COLLECT_PATH)
        .exempt_prefix(rustango_cms::mcp::MCP_PREFIX)
        .exempt_prefix(rustango_cms::api::PREFIX)
        .allow_insecure_for_dev();

    rustango::manage::Cli::new()
        .tenancy()
        .api(app)
        .migrations_dir(migrations_dir)
        .with_csrf_config(csrf)
        .seed(|registry| {
            let pool = registry.clone();
            async move {
                rustango_cms::ensure_seeded(&pool).await?;
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

/// A short hex hash of a file's bytes; empty when it can't be read.
fn file_hash(path: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}
