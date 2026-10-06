//! `rustango-cms-macros` — proc-macro derive(s) for rustango-cms.
//!
//! Today: `#[derive(PageType)]` only. The derive walks the struct's
//! `#[page_type(...)]` + `#[field(...)]` attributes and emits the
//! full `PageTypeHandler` trait impl + an `inventory::submit!`
//! registration so the type joins the global handler registry at
//! link time.
//!
//! ## Author surface
//!
//! ```ignore
//! use rustango::Model;
//! use rustango_cms::{PageType, PageTypeOverrides};
//!
//! #[derive(Model, PageType, Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
//! #[rustango(table = "blog_post_page_ext", app = "blog")]
//! #[page_type(
//!     type_name    = "BlogPostPage",
//!     verbose_name = "Blog post",
//!     template     = "blog_post.html",
//! )]
//! pub struct BlogPostPage {
//!     #[rustango(primary_key)]
//!     pub id: Auto<i64>,
//!     #[rustango(fk = "cms_page", on = "id", unique)]
//!     pub page_id: i64,
//!     #[field(widget = Markdown, label = "Body", help = "GFM.")]
//!     pub body_markdown: String,
//! }
//!
//! // ONE additional impl block carries every optional override.
//! impl PageTypeOverrides for BlogPostPage {}
//! ```
//!
//! See `rustango_cms::page_type` for the trait, override trait, and
//! the full surface authors interact with at runtime.

mod block;
mod page_type;

use proc_macro::TokenStream;

/// `#[derive(PageType)]` — generates the [`rustango_cms::PageTypeHandler`]
/// trait impl from `#[page_type(...)]` and `#[field(...)]` attributes
/// plus a global inventory registration so the type is reachable from
/// `rustango_cms::find_handler(type_name)`.
///
/// See module docs for the author-facing surface.
#[proc_macro_derive(PageType, attributes(page_type, field))]
pub fn derive_page_type(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as syn::DeriveInput);
    page_type::expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// `#[derive(Block)]` — generates the [`rustango_cms::Block`] trait
/// impl from `#[block(...)]` and `#[field(...)]` attributes plus a
/// global inventory registration so the block joins the registry at
/// link time.
///
/// Author surface:
///
/// ```ignore
/// use rustango_cms::Block;
///
/// #[derive(Default, Block)]
/// #[block(icon = "title", group = "Headings", description = "Section heading.")]
/// pub struct Heading {
///     #[field(widget = Text, required)]
///     pub text: String,
///     #[field(widget = Select, label = "Level", options(h2 = "H2", h3 = "H3"))]
///     pub level: String,
/// }
/// ```
///
/// `type_name` defaults to the snake_case of the struct name;
/// `verbose_name` defaults to the struct name as written. Override
/// either via `#[block(type_name = "…")]` / `#[block(verbose_name = "…")]`.
#[proc_macro_derive(Block, attributes(block, field))]
pub fn derive_block(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as syn::DeriveInput);
    block::expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
