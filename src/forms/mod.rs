//! Visual Form Builder.
//!
//! Forms are **snippet-backed**: each form is a `Snippet` of `type_name`
//! `"form"` whose `data` column holds a [`schema::Form`] JSON document.
//! That gives reuse, folders, search, revisions (= versioning), and
//! translations for free, and lets a form be embedded into any page via a
//! dedicated `form` block.
//!
//! Module map (filled in per ticket):
//! - [`schema`] — the form JSON contract + validation.
//! - [`library_type`] — `FormLibraryType` snippet registration.
//! - [`block`] — the embeddable `form` StreamField block.
//! - [`render`] — schema → public HTML.

pub mod block;
pub mod library_type;
pub mod render;
pub mod schema;
pub mod submit;
