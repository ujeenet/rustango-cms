//! `#[derive(Block)]` — generates the [`rustango_cms::Block`] trait
//! impl from `#[block(...)]` + `#[field(...)]` struct attributes plus
//! an `inventory::submit!` registration. Mirrors `#[derive(PageType)]`
//! for blocks instead of page types.
//!
//! ## Author surface
//!
//! ```ignore
//! use rustango_cms::Block;
//!
//! #[derive(Default, Block)]
//! #[block(icon = "title", group = "Headings", description = "Section heading.")]
//! pub struct Heading {
//!     #[field(widget = Text, required)]
//!     pub text: String,
//!     #[field(widget = Select, label = "Level", options(h2 = "H2", h3 = "H3"))]
//!     pub level: String,
//! }
//! ```
//!
//! - `type_name` defaults to the snake_case of the struct name
//!   (`Heading` → `"heading"`); override with `#[block(type_name = "...")]`.
//! - `verbose_name` defaults to the struct name as written; override
//!   with `#[block(verbose_name = "...")]`.
//! - `label` defaults to the field name title-cased.

use proc_macro2::TokenStream;
use quote::{quote, ToTokens};
use syn::parse::{Parse, ParseStream};
use syn::{spanned::Spanned, Data, DeriveInput, Fields, Result, Token};

/// Tiny inline snake_case — splits on uppercase boundaries.
/// `HeadingBlock` → `heading_block`; `BlockQuote` → `block_quote`.
fn to_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Title-case from a snake / camel / pascal name. Insert spaces on
/// `_` / `-` boundaries AND before uppercase letters following a
/// lowercase letter (so `TestPullquote` → `Test Pullquote`). Used to
/// derive default labels from field / struct names when authors
/// don't specify one.
fn to_title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let mut prev_lower = false;
    let mut start_of_word = true;
    for c in s.chars() {
        if c == '_' || c == '-' {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            start_of_word = true;
            prev_lower = false;
            continue;
        }
        // Insert space before an uppercase that follows a lowercase
        // (camelCase / PascalCase boundary).
        if c.is_ascii_uppercase() && prev_lower {
            out.push(' ');
            start_of_word = true;
        }
        if start_of_word {
            out.extend(c.to_uppercase());
            start_of_word = false;
        } else {
            out.push(c);
        }
        prev_lower = c.is_ascii_lowercase();
    }
    out
}

/// Top-level `#[block(...)]` attrs.
struct BlockConfig {
    type_name: String,
    verbose_name: String,
    icon: Option<String>,
    group: Option<String>,
    description: Option<String>,
}

impl BlockConfig {
    fn parse(input: &DeriveInput) -> Result<Self> {
        let mut type_name: Option<String> = None;
        let mut verbose_name: Option<String> = None;
        let mut icon: Option<String> = None;
        let mut group: Option<String> = None;
        let mut description: Option<String> = None;

        for attr in &input.attrs {
            if !attr.path().is_ident("block") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                let key = meta
                    .path
                    .get_ident()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                match key.as_str() {
                    "type_name" => type_name = Some(string_lit(&meta)?),
                    "verbose_name" => verbose_name = Some(string_lit(&meta)?),
                    "icon" => icon = Some(string_lit(&meta)?),
                    "group" => group = Some(string_lit(&meta)?),
                    "description" => description = Some(string_lit(&meta)?),
                    other => {
                        return Err(meta.error(format!(
                            "unknown #[block] key `{other}` — valid: \
                             type_name, verbose_name, icon, group, description"
                        )));
                    }
                }
                Ok(())
            })?;
        }

        let struct_name = input.ident.to_string();
        let type_name = type_name.unwrap_or_else(|| to_snake_case(&struct_name));
        let verbose_name = verbose_name.unwrap_or_else(|| to_title_case(&struct_name));

        Ok(Self {
            type_name,
            verbose_name,
            icon,
            group,
            description,
        })
    }
}

/// One `#[field(...)]`-tagged column on a block struct.
struct FieldConfig {
    widget: syn::Ident,
    name: String,
    label: String,
    help: Option<String>,
    required: bool,
    options: Vec<(String, String)>,
}

impl FieldConfig {
    fn parse(field: &syn::Field) -> Result<Option<Self>> {
        let Some(attr) = field.attrs.iter().find(|a| a.path().is_ident("field")) else {
            return Ok(None);
        };
        let ident = field
            .ident
            .clone()
            .ok_or_else(|| syn::Error::new(field.span(), "tuple structs not supported"))?;

        let mut widget: Option<syn::Ident> = None;
        let mut label: Option<String> = None;
        let mut help: Option<String> = None;
        let mut required = false;
        let mut options: Vec<(String, String)> = Vec::new();

        attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default();
            match key.as_str() {
                "widget" => {
                    let value = meta.value()?;
                    widget = Some(value.parse::<syn::Ident>()?);
                }
                "label" => label = Some(string_lit(&meta)?),
                "help" => help = Some(string_lit(&meta)?),
                "required" => required = true,
                "options" => {
                    let content;
                    syn::parenthesized!(content in meta.input);
                    let pairs: syn::punctuated::Punctuated<OptionPair, Token![,]> =
                        content.parse_terminated(OptionPair::parse, Token![,])?;
                    for p in pairs {
                        options.push((p.key, p.value));
                    }
                }
                other => {
                    return Err(meta.error(format!(
                        "unknown #[field] key `{other}` — valid: \
                         widget, label, help, required, options"
                    )));
                }
            }
            Ok(())
        })?;

        let widget = widget
            .ok_or_else(|| syn::Error::new(attr.span(), "#[field(widget = …)] is required"))?;
        let name = ident.to_string();
        let label = label.unwrap_or_else(|| to_title_case(&name));

        Ok(Some(Self {
            widget,
            name,
            label,
            help,
            required,
            options,
        }))
    }
}

/// `options(k1 = "v1", k2 = "v2")` parser. Reuses the same shape as
/// `#[derive(PageType)]`'s OptionPair so author muscle memory carries.
struct OptionPair {
    key: String,
    value: String,
}

impl Parse for OptionPair {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        // Accept either an ident or a string-literal key, then `= "value"`.
        let key = if input.peek(syn::Ident) {
            let id: syn::Ident = input.parse()?;
            id.to_string()
        } else {
            let lit: syn::LitStr = input.parse()?;
            lit.value()
        };
        let _eq: Token![=] = input.parse()?;
        let value_lit: syn::LitStr = input.parse()?;
        Ok(Self {
            key,
            value: value_lit.value(),
        })
    }
}

fn string_lit(meta: &syn::meta::ParseNestedMeta<'_>) -> Result<String> {
    let value = meta.value()?;
    let lit: syn::LitStr = value.parse()?;
    Ok(lit.value())
}

pub fn expand(input: DeriveInput) -> Result<TokenStream> {
    let struct_ident = input.ident.clone();
    let config = BlockConfig::parse(&input)?;

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return Err(syn::Error::new(
                    input.span(),
                    "#[derive(Block)] requires a struct with named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new(
                input.span(),
                "#[derive(Block)] only works on structs",
            ));
        }
    };

    let mut field_configs: Vec<FieldConfig> = Vec::new();
    for f in fields {
        if let Some(fc) = FieldConfig::parse(f)? {
            field_configs.push(fc);
        }
    }

    let type_name = &config.type_name;
    let verbose_name = &config.verbose_name;
    let icon_arm = match &config.icon {
        Some(i) => quote! { fn icon(&self) -> ::std::option::Option<&'static str> { Some(#i) } },
        None => quote! {},
    };
    let group_arm = match &config.group {
        Some(g) => quote! { fn group(&self) -> ::std::option::Option<&'static str> { Some(#g) } },
        None => quote! {},
    };
    let description_arm = match &config.description {
        Some(d) => {
            quote! { fn description(&self) -> ::std::option::Option<&'static str> { Some(#d) } }
        }
        None => quote! {},
    };

    let field_exprs: Vec<TokenStream> = field_configs.iter().map(|f| build_field_expr(f)).collect();

    Ok(quote! {
        impl ::rustango_cms::Block for #struct_ident {
            fn type_name(&self) -> &'static str { #type_name }
            fn verbose_name(&self) -> &'static str { #verbose_name }
            #icon_arm
            #group_arm
            #description_arm
            fn fields(&self) -> ::std::vec::Vec<::rustango_cms::BlockField> {
                let mut out: ::std::vec::Vec<::rustango_cms::BlockField> = ::std::vec::Vec::new();
                #(out.push(#field_exprs);)*
                out
            }
        }

        ::rustango_cms::__private::inventory::submit! {
            ::rustango_cms::block::BlockRegistration {
                factory: || ::std::boxed::Box::new(<#struct_ident as ::core::default::Default>::default()),
            }
        }
    })
}

fn build_field_expr(f: &FieldConfig) -> TokenStream {
    let widget_ident = &f.widget;
    let name = &f.name;
    let label = &f.label;
    let required = f.required;
    let help_arm: TokenStream = match &f.help {
        Some(h) => quote! { ::std::option::Option::Some(#h.to_owned()) },
        None => quote! { ::std::option::Option::None },
    };
    let options_arm: TokenStream = if f.options.is_empty() {
        quote! { ::std::vec::Vec::new() }
    } else {
        let pairs = f.options.iter().map(|(k, v)| {
            quote! { (#k.to_owned(), #v.to_owned()) }
        });
        quote! { ::std::vec![#(#pairs),*] }
    };

    quote! {
        ::rustango_cms::BlockField::Widget {
            name: #name.to_owned(),
            label: #label.to_owned(),
            widget: ::rustango_cms::widget::WidgetKind::#widget_ident,
            options: #options_arm,
            help: #help_arm,
            required: #required,
            meta: ::rustango_cms::block::BlockFieldMeta::default(),
        }
    }
}

// Stub used elsewhere — keep ToTokens import alive in case future
// fields need it. Removing avoids unused-warning today.
#[allow(dead_code)]
fn _force_use_to_tokens() {
    let _: fn(&syn::Ident) -> TokenStream = |i| i.into_token_stream();
}
