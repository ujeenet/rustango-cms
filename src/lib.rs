//! rustango-cms — a Wagtail-style, multi-tenant CMS built on
//! [rustango](https://docs.rs/rustango).
//!
//! Pages live in one tenant-scoped `cms_page` tree (materialized path,
//! see [`tree`]); each page's kind is a registered [`PageTypeHandler`]
//! with its own extension table, template and admin form. Every read and
//! write goes through the tenant's own pool, and the bundled migrations
//! (see [`migrations`]) are tenant-scoped. The same source runs on
//! PostgreSQL, MySQL 8+ and SQLite.
//!
//! ## Entry points
//!
//! | Surface | Where |
//! |---|---|
//! | Public site (slug resolver + Tera render, feeds, sitemap, forms) | [`PublicRouter`], or the [`router()`](crate::router()) / [`router_at`] shortcuts |
//! | Editor admin at `/cms-admin/` | [`admin::router`], gated by [`admin::with_login_required`]; templates via [`admin::register_templates`] |
//! | Page types | `#[derive(PageType)]` + [`PageTypeOverrides`], or a hand-written [`PageTypeHandler`] + [`register_page_type!`] |
//! | StreamField bodies | [`block`] — the [`block::Block`] trait, `#[derive(Block)]` or [`register_block!`] |
//! | Reusable content (library / snippets) | [`library`] ([`LibraryTypeHandler`], [`register_library_type!`]) and [`snippet`] |
//! | Headless JSON API under `/api/v2/` | [`api::router`] / [`api::router_with`] |
//! | MCP server for AI agents at [`mcp::MCP_PREFIX`] | [`mcp::router`] |
//! | Per-tenant setup — page-type rows, roles, default locale, host seeders (late tenants seed on first request) | [`ensure_seeded`] |
//! | Extension hooks (publish, serve, admin menu, dashboard) | [`hooks`] |
//!
//! ## Minimal wiring
//!
//! ```ignore
//! use std::sync::Arc;
//!
//! let mut tera = tera::Tera::new("templates/**/*.html")?;
//! rustango_cms::admin::register_templates(&mut tera)?;
//! let tera = Arc::new(tera);
//!
//! let cms_admin = rustango_cms::admin::with_login_required(
//!     rustango_cms::admin::router(tera.clone()),
//!     "/login",
//! );
//! let app = cms_admin
//!     .merge(rustango_cms::api::router())
//!     .merge(rustango_cms::mcp::router(&settings.mcp)) // CSRF-exempt MCP_PREFIX
//!     .merge(rustango_cms::PublicRouter::new(tera).build());
//! // Hand `app` to `rustango::manage::Cli::new().tenancy().api(app)`
//! // and call `rustango_cms::ensure_seeded` from its `.seed(..)` hook.
//! ```
//!
//! [`admin::router`] is unauthenticated on its own; always wrap it in
//! [`admin::with_login_required`].
//!
//! ## Cargo features
//!
//! | Feature | Effect |
//! |---|---|
//! | `postgres` (default), `sqlite`, `mysql` | Database backend, forwarded to rustango |
//! | `storage_s3` | S3-compatible media storage for [`media_storage::install_from_env`] |
//! | `cache_cloudflare`, `cache_varnish`, `cache_cloudfront`, `cache_gcp`, `cache_azure` | Front-cache purge backends ([`cache_invalidate`]) |
//! | `search_elasticsearch` | Elasticsearch search backend (`search_elasticsearch` module) |
//! | `avif` | AVIF image renditions |
//! | `test_utils` | rustango's `Tenant::for_test` for integration tests |

pub mod a11y;
pub mod access;
pub mod admin;
pub mod analytics;
pub mod api;
pub mod auto_menu;
pub mod block;
// #193 / #428 — opt-in cache-invalidator backends. Each module is
// gated on its own cargo feature so the default build doesn't pull
// the backend-specific deps. Feature flags: `cache_cloudflare`,
// `cache_varnish`, `cache_gcp`, `cache_azure` (all → reqwest via
// `rustango/http-client`), `cache_cloudfront` (→ aws-config +
// aws-sdk-cloudfront).
pub mod breadcrumbs;
#[cfg(feature = "cache_azure")]
pub mod cache_azure;
#[cfg(feature = "cache_cloudflare")]
pub mod cache_cloudflare;
#[cfg(feature = "cache_cloudfront")]
pub mod cache_cloudfront;
#[cfg(feature = "cache_gcp")]
pub mod cache_gcp;
pub mod cache_invalidate;
#[cfg(feature = "cache_varnish")]
pub mod cache_varnish;
pub mod category;
pub mod category_translation;
pub mod children_filtered;
pub mod collection_view_restriction;
pub mod comment;
pub mod config;
pub mod content_checks;
pub mod date_filters;
pub mod dismissible;
pub mod editing_session;
pub mod embed;
pub mod error_pages;
pub mod feed;
pub mod forms;
pub mod fragment_cache;
pub mod hooks;
pub mod language_switcher;
pub mod library;
pub mod locale;
pub mod locale_mode;
mod log_err;
pub mod lock;
pub mod macros;
pub mod markdown;
pub mod mcp;
pub mod media;
pub mod media_storage;
pub mod menu_item_translation;
pub mod meta_tags;
pub mod metrics;
pub mod migrations;
pub mod mysql_text;
pub mod navigation;
pub mod notification_pref;
pub mod notify;
pub mod page;
pub mod page_builder;
mod page_delete;
pub(crate) mod page_form;
pub mod page_log;
pub mod page_snippet_m2m;
pub mod page_subscription;
pub mod page_tag;
pub mod page_type;
pub mod page_type_model;
pub mod page_url;
pub mod page_view;
pub mod pending_change;
pub(crate) mod passwords;
pub mod perf;
pub mod permissions;
pub mod preview_token;
pub mod preview_url;
pub mod redirect;
pub mod reference_index;
pub mod render;
pub mod rendition;
pub mod rendition_route;
pub mod resolver;
pub mod revision;
pub mod richtext;
pub mod routable;
pub mod router;
pub mod search;
#[cfg(feature = "search_elasticsearch")]
pub mod search_elasticsearch;
pub mod search_promotion;
pub mod schedule;
pub mod seed;
pub mod signing;
pub mod site;
pub mod site_setting;
pub mod robots;
pub mod sitemap;
pub mod snippet;
pub mod snippet_render;
pub mod snippet_translation;
pub mod task_kind;
pub mod task_queue;
pub mod tenant_templates;
pub mod theme;
pub mod theme_resolve;
pub mod theme_seed;
pub mod translation;
pub mod tree;
pub mod tree_build;
pub mod tree_ops;
pub mod uploaded_file;
pub mod urls;
pub mod userbar;
pub mod view_restriction;
pub use view_restriction::{RestrictionKind, TypeViewRestriction};
pub mod view_restriction_guard;
pub mod widget;
pub mod workflow;
pub mod workflow_mail;

#[doc(hidden)]
pub use ::inventory;

pub use category::{
    registered_handlers as registered_taxonomy_handlers, Category, PageCategory, Taxonomy,
    TaxonomyHandler, TaxonomyHandlerRegistration,
};
pub use library::{
    ensure_library_type_row, find_handler as find_library_handler,
    registered_handlers as registered_library_handlers, LibraryType, LibraryTypeHandler,
    LibraryTypeHandlerRegistration, ListColumn, ListColumnKind,
};
pub use locale::Locale;
pub use locale_mode::{resolve_request_locale, LocaleMode, RequestLocale};
pub use media::{Media, MediaCollection};
pub use category_translation::CategoryTranslation;
pub use menu_item_translation::MenuItemTranslation;
pub use navigation::{Menu, MenuItem};
pub use page::{Page, PageStatus};
pub use page_type::{
    all_children, default_children, find_handler, published_children, registered_handlers, DisplayField,
    ExtensionField, ExtensionFieldKind, InlinePanelSpec, PageTypeHandler,
    PageTypeHandlerRegistration, PageTypeOverrides, TabSpec,
};
pub use page_view::{Accept, PageViewKind, PageViewMode};
pub use permissions::{
    user_can, user_can_in_collection, user_can_matrix, Action, CollectionPermission, PagePermission,
};
pub use redirect::{
    bump_hit_count as bump_redirect_hit_count, find_for_path as find_redirect_for_path, Redirect,
};
pub use revision::Revision;
pub use snippet::Snippet;
pub use theme::{emit_css as emit_theme_css, BrandColor, Theme};
pub use theme_resolve::{resolve_for_page as resolve_theme_for_page, ResolvedTheme};
pub use theme_seed::ensure_themes_seeded;
pub use translation::Translation;

/// `#[derive(PageType)]` — declarative page-type definition.
/// See [`crate::page_type::PageTypeOverrides`] for the override trait
/// authors implement alongside.
pub use rustango_cms_macros::PageType;

/// `#[derive(Block)]` — declarative block definition. Walks
/// `#[block(...)]` + `#[field(widget = …)]` attrs to emit the `Block`
/// trait impl + `register_block!` inventory submission in one go.
/// Coexists with the [`Block`](crate::block::Block) trait through
/// Rust's separate macro / type namespaces (same pattern as
/// `#[derive(PageType)]` alongside [`PageType`](crate::page_type_model::PageType)).
pub use rustango_cms_macros::Block;

/// Re-exports for the `#[derive(PageType)]` macro's generated code.
/// Not part of the public API — addressing anything in here from user
/// code is a footgun.
#[doc(hidden)]
pub mod __private {
    pub use async_trait;
    pub use inventory;
    pub use rustango::core::Model;
    pub use rustango::sql::{ExecError, FetcherPool, Pool};
    pub use serde_json;
}
pub use page_type_model::PageType;
pub use render::render;
pub use resolver::resolve_path;
pub use router::{router, router_at, router_at_cached, router_cached, PublicRouter};
pub use schedule::spawn_schedule_sweeper;
pub use seed::{ensure_seeded, SeedError};
// Custom admin pages extension point (#23). Re-export the trait,
// context, and section enum so host crates can import the surface
// they need to implement without reaching into `admin::admin_page`.
pub use admin::admin_page::{
    find_admin_page, registered_admin_pages, AdminPageCtx, AdminPageHandler,
    AdminPageHandlerRegistration, SidebarSection,
};
pub use block::{
    find_block, registered_blocks, validate_block_registry, Block, BlockCountConstraint,
    BlockError, BlockField, BlockFieldMeta, BlockRegistration, BlockRenderCtx,
};
pub use tree::{MaterializedPath, PathError};
pub use tree_ops::{
    cascade_url_path_to_descendants, compute_url_path, rebuild_all_url_paths,
    validate_child_placement, NewPage, TreeError,
};
pub use widget::{Widget, WidgetKind};
