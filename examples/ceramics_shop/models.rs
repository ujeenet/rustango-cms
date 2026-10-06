//! The ceramics shop's code-defined pieces.
//!
//! Most of the shop is built in the admin by the tutorial — the Product
//! page type, categories, menus, forms. What lives here is only what the
//! admin cannot create: the home and shop listing page types, the
//! `Workshop` page type the developer chapter walks through, a reusable
//! "Information" library type, a "Clay" vocabulary and the typed "Brand"
//! site setting.
//!
//! Extension tables are generated, never hand-written:
//!     cargo run --example ceramics_shop -- makemigrations
//!     cargo run --example ceramics_shop -- migrate-tenants

use async_trait::async_trait;
use rustango::sql::{Auto, ExecError, Pool};
use rustango_cms::widget::{Widget, WidgetKind};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- Home page

/// The shop's front page: a large photo with a heading, then an
/// introduction composed from blocks. It sets no child rules, so page types
/// made in the admin (the Product type) can go under it.
#[derive(rustango::Model, rustango_cms::PageType, Default, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "shop_home_page", app = "shop")]
#[page_type(type_name = "HomePage", verbose_name = "Home page", template = "home_page.html", icon = "home")]
pub struct HomePage {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "cms_page", on = "id", unique)]
    pub page_id: i64,
    #[field(widget = MediaPicker, label = "Big photo", help = "The large photo at the top of the page.")]
    pub hero_image: Option<i64>,
    #[field(widget = Text, label = "Big heading", help = "A few words over the photo.")]
    pub hero_heading: Option<String>,
    #[field(widget = Textarea, label = "Text under the heading")]
    pub hero_text: Option<String>,
    #[field(
        widget = Stream,
        label = "Introduction",
        help = "Welcome text, a photo, a quote from a customer.",
        allowed(heading, paragraph, image, quote)
    )]
    pub body: Option<String>,
}

#[async_trait]
impl rustango_cms::PageTypeOverrides for HomePage {
    /// The site's sections: content pages, the shop, workshops, error
    /// pages. (Products go under the shop.)
    fn allowed_child_types(&self) -> &'static [&'static str] {
        &["ContentPage", "ShopPage", "Workshop", "ErrorPage"]
    }

    async fn children_query(
        &self,
        pool: &Pool,
        page: &rustango_cms::Page,
    ) -> Result<Option<Vec<rustango_cms::Page>>, ExecError> {
        Ok(Some(rustango_cms::published_children(pool, page).await?))
    }
}

// ---------------------------------------------------------------- Content page

/// A general page — About us, Shipping, Care — with a photo and a body.
#[derive(rustango::Model, rustango_cms::PageType, Default, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "shop_content_page", app = "shop")]
#[page_type(type_name = "ContentPage", verbose_name = "Content page", template = "content_page.html", icon = "article")]
pub struct ContentPage {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "cms_page", on = "id", unique)]
    pub page_id: i64,
    #[field(widget = MediaPicker, label = "Photo")]
    pub photo: Option<i64>,
    #[field(
        widget = Stream,
        label = "Body",
        allowed(heading, paragraph, image, quote, snippet_chooser, form)
    )]
    pub body: Option<String>,
}

impl rustango_cms::PageTypeOverrides for ContentPage {}

// ---------------------------------------------------------------- Shop page

/// A listing page: its published children are the products, shown as
/// cards and grouped by category in `shop_page.html`.
#[derive(rustango::Model, rustango_cms::PageType, Default, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "shop_shop_page", app = "shop")]
#[page_type(type_name = "ShopPage", verbose_name = "Shop page", template = "shop_page.html", icon = "storefront")]
pub struct ShopPage {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "cms_page", on = "id", unique)]
    pub page_id: i64,
    #[field(widget = Markdown, label = "Introduction", help = "A short text above the products.")]
    pub intro: Option<String>,
}

#[async_trait]
impl rustango_cms::PageTypeOverrides for ShopPage {
    /// Only products live under the shop — `product` is the type editors
    /// build in the admin (chapter 4).
    fn allowed_child_types(&self) -> &'static [&'static str] {
        &["product"]
    }

    async fn children_query(
        &self,
        pool: &Pool,
        page: &rustango_cms::Page,
    ) -> Result<Option<Vec<rustango_cms::Page>>, ExecError> {
        Ok(Some(rustango_cms::published_children(pool, page).await?))
    }
}

// ---------------------------------------------------------------- Workshop

/// A pottery class — the page type the developer chapter builds: a date,
/// the number of seats and a description, in its own table.
#[derive(rustango::Model, rustango_cms::PageType, Default, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "shop_workshop", app = "shop")]
#[page_type(type_name = "Workshop", verbose_name = "Workshop", template = "workshop.html", icon = "event")]
pub struct Workshop {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "cms_page", on = "id", unique)]
    pub page_id: i64,
    #[field(widget = Date, label = "Date")]
    pub date: Option<String>,
    #[field(widget = Integer, label = "Seats", help = "How many people can join.")]
    pub seats: Option<i64>,
    #[field(widget = MediaPicker, label = "Photo")]
    pub photo: Option<i64>,
    #[field(widget = Markdown, label = "Description")]
    pub description: Option<String>,
}

impl rustango_cms::PageTypeOverrides for Workshop {}

// ---------------------------------------------------------------- Library

/// Reusable text — shipping information, care instructions — written
/// once in the Library and placed on any page. Title and body only, so it
/// needs no table of its own.
#[derive(Default)]
pub struct Information;

#[async_trait]
impl rustango_cms::library::LibraryTypeHandler for Information {
    fn app_label(&self) -> &'static str {
        "shop"
    }
    fn type_name(&self) -> &'static str {
        "Information"
    }
    fn verbose_name(&self) -> &'static str {
        "Information"
    }
    fn extension_table(&self) -> Option<&'static str> {
        None
    }
}

rustango_cms::register_library_type!(Information);

// ---------------------------------------------------------------- Clay

/// A second vocabulary next to the built-in Categories: the clay a piece is
/// made from. A flat list (stoneware, porcelain…), so no nesting.
#[derive(Default)]
pub struct Clay;

impl rustango_cms::category::TaxonomyHandler for Clay {
    fn slug(&self) -> &'static str {
        "clay"
    }
    fn verbose_name(&self) -> &'static str {
        "Clay"
    }
    fn hierarchical(&self) -> bool {
        false
    }
    fn icon(&self) -> Option<&'static str> {
        Some("texture")
    }
    fn description(&self) -> Option<&'static str> {
        Some("The clay a piece is made from.")
    }
}

rustango_cms::register_taxonomy!(Clay);

// ---------------------------------------------------------------- Brand

rustango_cms::register_site_setting!("brand", "Brand", || vec![
    Widget::new(WidgetKind::Text, "shop_name", "Shop name").required(),
    Widget::new(WidgetKind::Text, "tagline", "Tagline"),
    Widget::new(WidgetKind::Color, "accent", "Accent colour"),
    Widget::new(WidgetKind::Color, "background", "Background colour"),
    Widget::new(WidgetKind::Textarea, "footer", "Footer text"),
]);
