//! Declarative macro for registering a [`PageTypeHandler`].
//!
//! [`PageTypeHandler`]: crate::PageTypeHandler

/// Submit a [`PageTypeHandlerRegistration`] for `$ty` to the global
/// inventory.
///
/// `$ty` must implement both [`PageTypeHandler`] and [`Default`] —
/// the macro instantiates one through `<$ty>::default()` per call to
/// [`registered_handlers`].
///
/// # Example
///
/// ```ignore
/// use rustango_cms::{register_page_type, PageTypeHandler};
///
/// #[derive(Default)]
/// pub struct ArticlePage;
///
/// #[async_trait::async_trait]
/// impl PageTypeHandler for ArticlePage {
///     fn app_label(&self) -> &'static str { "blog" }
///     fn type_name(&self) -> &'static str { "ArticlePage" }
///     fn verbose_name(&self) -> &'static str { "Article" }
///     fn default_template(&self) -> &'static str { "blog/article.html" }
/// }
///
/// register_page_type!(ArticlePage);
/// ```
///
/// ## Per-type extension tables
///
/// For typed extension data (e.g. an article's body, hero image),
/// author a sibling `#[derive(Model)]` struct with a one-to-one FK
/// back to `cms_page`:
///
/// ```ignore
/// use rustango::sql::Auto;
/// use rustango::Model;
///
/// #[derive(Model)]
/// #[rustango(table = "cms_article_page", app = "blog")]
/// pub struct ArticlePageExt {
///     #[rustango(primary_key, fk = "cms_page", on = "id", unique)]
///     pub page_id: i64,
///
///     pub body_markdown: String,
///     pub hero_image: Option<String>,
/// }
/// ```
///
/// The host crate ships this in its own `migrations/` directory;
/// `discover_migration_dirs` picks it up alongside the framework's.
/// Override `PageTypeHandler::load_extension` to fetch the row and
/// return it as JSON for the renderer's Tera context.
///
/// [`PageTypeHandler`]: crate::PageTypeHandler
/// [`PageTypeHandlerRegistration`]: crate::PageTypeHandlerRegistration
/// [`registered_handlers`]: crate::registered_handlers
#[macro_export]
macro_rules! register_page_type {
    ($ty:ty) => {
        $crate::inventory::submit! {
            $crate::PageTypeHandlerRegistration {
                factory: || ::std::boxed::Box::new(<$ty>::default()),
            }
        }
    };
}

/// Register a custom admin page (#23). The type must implement
/// [`AdminPageHandler`](crate::admin::admin_page::AdminPageHandler)
/// and `Default`. Mirrors the
/// [`register_page_type!`] / [`register_library_type!`] pattern —
/// drop the macro call anywhere in the crate's source and the page
/// shows up at `/cms-admin/x/<slug>` with a sidebar entry in its
/// declared section.
///
/// ```ignore
/// use rustango_cms::register_admin_page;
/// use rustango_cms::admin::admin_page::{AdminPageHandler, AdminPageCtx, SidebarSection};
///
/// #[derive(Default)]
/// pub struct ReportsDashboard;
///
/// #[async_trait::async_trait]
/// impl AdminPageHandler for ReportsDashboard {
///     fn slug(&self) -> &'static str { "reports" }
///     fn label(&self) -> &'static str { "Reports" }
///     fn icon(&self) -> Option<&'static str> { Some("insights") }
///     fn section(&self) -> SidebarSection { SidebarSection::Site }
///     async fn render(&self, _ctx: AdminPageCtx<'_>) -> axum::response::Response {
///         todo!()
///     }
/// }
///
/// register_admin_page!(ReportsDashboard);
/// ```
#[macro_export]
macro_rules! register_admin_page {
    ($ty:ty) => {
        $crate::inventory::submit! {
            $crate::admin::admin_page::AdminPageHandlerRegistration {
                factory: || ::std::boxed::Box::new(<$ty>::default()),
            }
        }
    };
}

/// Register a custom taxonomy / category vocabulary (#557). The type must
/// implement [`TaxonomyHandler`](crate::category::TaxonomyHandler) and
/// `Default`. Mirrors [`register_page_type!`] — drop the call anywhere in
/// the crate's source and the vocabulary is synced into a `cms_taxonomy`
/// row at seed and shows up under the admin **Taxonomies** section.
///
/// Extra typed fields go in the handler's own per-type extension table
/// (unique `category_id` FK), the same way a page type extends `cms_page`.
///
/// ```ignore
/// use rustango_cms::register_taxonomy;
/// use rustango_cms::category::TaxonomyHandler;
///
/// #[derive(Default)]
/// pub struct RegionTaxonomy;
/// impl TaxonomyHandler for RegionTaxonomy {
///     fn slug(&self) -> &'static str { "region" }
///     fn verbose_name(&self) -> &'static str { "Regions" }
/// }
/// register_taxonomy!(RegionTaxonomy);
/// ```
#[macro_export]
macro_rules! register_taxonomy {
    ($ty:ty) => {
        $crate::inventory::submit! {
            $crate::category::TaxonomyHandlerRegistration {
                factory: || ::std::boxed::Box::new(<$ty>::default()),
            }
        }
    };
}
