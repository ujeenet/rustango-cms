//! `#[derive(CmsAdmin)]` codegen: an admin editor for a `Model`.
//!
//! Reads the struct-level `#[cms_admin(...)]`, the Model derive's
//! `#[rustango(table, display, admin(list_display, search_fields,
//! ordering))]` for defaults, and the same per-field `#[field(...)]`
//! attributes `#[derive(PageType)]` uses. Emits
//! `impl rustango_cms::CmsAdmin` and registers
//! `rustango_cms::CmsAdminPage<Self>` as a custom admin page.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{spanned::Spanned, Data, DeriveInput, Expr, Fields, Result, Token, Type};

use crate::page_type::{classify_type, emit_widget_call, humanize_label, string_lit, FieldConfig, TypeCat};

#[derive(Default)]
struct Config {
    label: Option<String>,
    singular: Option<String>,
    slug: Option<String>,
    icon: Option<String>,
    section: Option<String>,
    sort_order: Option<i32>,
    list: Option<String>,
    search: Option<String>,
    ordering: Option<String>,
    per_page: Option<i64>,
    permission: Option<String>,
    title: Option<String>,
    form: Option<String>,
    // From `#[rustango(...)]`.
    table: Option<String>,
    display: Option<String>,
    list_display: Option<String>,
    search_fields: Option<String>,
    model_ordering: Option<String>,
}

fn skip_value(meta: &syn::meta::ParseNestedMeta<'_>) -> Result<()> {
    if meta.input.peek(Token![=]) {
        let _: Token![=] = meta.input.parse()?;
        let _: Expr = meta.input.parse()?;
    } else if meta.input.peek(syn::token::Paren) {
        let _content;
        syn::parenthesized!(_content in meta.input);
        let _: TokenStream = _content.parse()?;
    }
    Ok(())
}

impl Config {
    fn parse(input: &DeriveInput) -> Result<Self> {
        let mut c = Self::default();
        for attr in &input.attrs {
            if attr.path().is_ident("cms_admin") {
                attr.parse_nested_meta(|meta| {
                    let key = meta.path.get_ident().map(ToString::to_string).unwrap_or_default();
                    match key.as_str() {
                        "label" => c.label = Some(string_lit(&meta)?),
                        "singular" => c.singular = Some(string_lit(&meta)?),
                        "slug" => c.slug = Some(string_lit(&meta)?),
                        "icon" => c.icon = Some(string_lit(&meta)?),
                        "section" => c.section = Some(string_lit(&meta)?),
                        "list" => c.list = Some(string_lit(&meta)?),
                        "search" => c.search = Some(string_lit(&meta)?),
                        "ordering" => c.ordering = Some(string_lit(&meta)?),
                        "permission" => c.permission = Some(string_lit(&meta)?),
                        "title" => c.title = Some(string_lit(&meta)?),
                        "form" => c.form = Some(string_lit(&meta)?),
                        "sort_order" => {
                            let lit: syn::LitInt = meta.value()?.parse()?;
                            c.sort_order = Some(lit.base10_parse()?);
                        }
                        "per_page" => {
                            let lit: syn::LitInt = meta.value()?.parse()?;
                            c.per_page = Some(lit.base10_parse()?);
                        }
                        other => {
                            return Err(meta.error(format!(
                                "unknown #[cms_admin] key `{other}` — valid: label, singular, slug, \
                                 icon, section, sort_order, list, search, ordering, per_page, \
                                 permission, title, form"
                            )))
                        }
                    }
                    Ok(())
                })?;
            } else if attr.path().is_ident("rustango") {
                // Best-effort read of the Model derive's attrs; it
                // validates them itself.
                let _ = attr.parse_nested_meta(|meta| {
                    let key = meta.path.get_ident().map(ToString::to_string).unwrap_or_default();
                    match key.as_str() {
                        "table" => c.table = string_lit(&meta).ok(),
                        "display" => c.display = string_lit(&meta).ok(),
                        "admin" => {
                            meta.parse_nested_meta(|inner| {
                                let k = inner.path.get_ident().map(ToString::to_string).unwrap_or_default();
                                match k.as_str() {
                                    "list_display" => c.list_display = string_lit(&inner).ok(),
                                    "search_fields" => c.search_fields = string_lit(&inner).ok(),
                                    "ordering" => c.model_ordering = string_lit(&inner).ok(),
                                    _ => skip_value(&inner)?,
                                }
                                Ok(())
                            })?;
                        }
                        _ => skip_value(&meta)?,
                    }
                    Ok(())
                });
            }
        }
        Ok(c)
    }
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned).collect()
}

/// `DonationCause` → `donation cause`.
fn words(ident: &str) -> String {
    let mut out = String::new();
    for (i, ch) in ident.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            out.push(' ');
        }
        out.extend(ch.to_lowercase());
    }
    out
}

fn last_segment(ty: &Type) -> Option<String> {
    if let Type::Path(tp) = ty {
        tp.path.segments.last().map(|s| s.ident.to_string())
    } else {
        None
    }
}

fn has_primary_key(field: &syn::Field) -> bool {
    field.attrs.iter().any(|a| {
        a.path().is_ident("rustango") && {
            let mut found = false;
            let _ = a.parse_nested_meta(|m| {
                if m.path.is_ident("primary_key") {
                    found = true;
                }
                skip_value(&m)
            });
            found
        }
    })
}

/// `r.<field>` → the `String` a widget or list cell shows.
fn show(ty: &Type, field: &syn::Ident) -> TokenStream {
    match classify_type(ty) {
        TypeCat::String => quote! { r.#field.clone() },
        TypeCat::OptionString => quote! { r.#field.clone().unwrap_or_default() },
        TypeCat::Bool => quote! {
            if r.#field { ::std::string::String::from("true") } else { ::std::string::String::new() }
        },
        _ => match last_segment(ty).as_deref() {
            Some("Option") => quote! { ::rustango_cms::admin::cms_admin::__derive::show_opt(r.#field.as_ref()) },
            Some("Auto") => quote! { ::rustango_cms::admin::cms_admin::__derive::show_opt(r.#field.get()) },
            _ => quote! { ::std::string::ToString::to_string(&r.#field) },
        },
    }
}

/// Copy one posted `#[field]` into `self`.
fn apply(f: &FieldConfig) -> TokenStream {
    let name = &f.name;
    let key = name.to_string();
    let required = f.required;
    let d = quote! { ::rustango_cms::admin::cms_admin::__derive };
    let body = match classify_type(&f.ty) {
        TypeCat::Bool => quote! { self.#name = #d::boolean(form, #key); },
        TypeCat::String => quote! {
            if let ::core::option::Option::Some(v) = #d::string(form, #key, #required, &mut errors) { self.#name = v; }
        },
        TypeCat::OptionString => quote! {
            if let ::core::option::Option::Some(v) = #d::opt_string(form, #key) {
                if #required && v.is_none() {
                    errors.push(::rustango_cms::FieldError::new(#key, "This field is required."));
                } else {
                    self.#name = v;
                }
            }
        },
        _ if last_segment(&f.ty).as_deref() == Some("Option") => quote! {
            if let ::core::option::Option::Some(v) = #d::opt_parsed(form, #key, #required, &mut errors) { self.#name = v; }
        },
        _ => quote! {
            if let ::core::option::Option::Some(v) = #d::parsed(form, #key, &mut errors) { self.#name = v; }
        },
    };
    quote! {
        if !skip.contains(&#key) {
            #body
        }
    }
}

pub fn expand(input: DeriveInput) -> Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new(input.generics.span(), "#[derive(CmsAdmin)] does not support generic structs"));
    }
    let ident = &input.ident;
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(input.span(), "#[derive(CmsAdmin)] needs a struct"));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new(input.span(), "#[derive(CmsAdmin)] needs a struct with named fields"));
    };
    let c = Config::parse(&input)?;

    let mut fields: Vec<FieldConfig> = Vec::new();
    for f in &named.named {
        if let Some(fc) = FieldConfig::parse(f)? {
            fields.push(fc);
        }
    }
    if fields.is_empty() {
        return Err(syn::Error::new(
            input.span(),
            "#[derive(CmsAdmin)] needs at least one #[field(widget = …, label = \"…\")] to edit",
        ));
    }
    // `form = "a, b"` puts those widgets first, in that order.
    if let Some(order) = c.form.as_deref().map(split_list) {
        for name in &order {
            if !fields.iter().any(|f| f.name == name.as_str()) {
                return Err(syn::Error::new(
                    input.span(),
                    format!("#[cms_admin(form = …)] names `{name}`, which has no #[field(...)]"),
                ));
            }
        }
        fields.sort_by_key(|f| order.iter().position(|n| f.name == n.as_str()).unwrap_or(order.len()));
    }
    let all: Vec<&syn::Field> = named.named.iter().collect();
    let field_by_name = |n: &str| all.iter().copied().find(|f| f.ident.as_ref().is_some_and(|i| i == n));

    // Primary key: the `#[rustango(primary_key)]` field, else `id`.
    let pk = all
        .iter()
        .copied()
        .find(|f| has_primary_key(f))
        .or_else(|| field_by_name("id"))
        .ok_or_else(|| syn::Error::new(input.span(), "#[derive(CmsAdmin)] needs a primary key (`id` or #[rustango(primary_key)])"))?;
    let pk_ident = pk.ident.clone().expect("named field");
    let pk_expr = match last_segment(&pk.ty).as_deref() {
        Some("Auto") => quote! { self.#pk_ident.get().copied().map(::core::convert::Into::into) },
        _ => quote! { ::core::option::Option::Some(::core::convert::Into::into(self.#pk_ident)) },
    };

    let table = c.table.clone().unwrap_or_else(|| words(&ident.to_string()).replace(' ', "_"));
    let label = c
        .label
        .clone()
        .ok_or_else(|| syn::Error::new(input.span(), "#[derive(CmsAdmin)] requires #[cms_admin(label = \"…\")]"))?;
    let singular = c.singular.clone().unwrap_or_else(|| words(&ident.to_string()));
    let slug = c.slug.clone().unwrap_or_else(|| table.replace('_', "-"));
    let permission = c.permission.clone().unwrap_or_else(|| table.clone());
    let icon = match &c.icon {
        Some(i) => quote! { ::core::option::Option::Some(#i) },
        None => quote! { ::core::option::Option::None },
    };
    let section = c.section.clone().unwrap_or_else(|| "Tools".to_owned());
    let sort_order = c.sort_order.unwrap_or(100);
    let per_page = c.per_page.unwrap_or(25);

    // List columns, and the header for each.
    let list = c
        .list
        .as_deref()
        .or(c.list_display.as_deref())
        .map(split_list)
        .unwrap_or_else(|| fields.iter().take(3).map(|f| f.name.to_string()).collect());
    let headers: Vec<String> = list
        .iter()
        .map(|col| {
            fields
                .iter()
                .find(|f| f.name == col.as_str())
                .map_or_else(|| humanize_label(col), |f| f.label.clone())
        })
        .collect();
    let cell_arms: Vec<TokenStream> = list
        .iter()
        .filter_map(|col| field_by_name(col))
        .map(|f| {
            let id = f.ident.as_ref().expect("named field");
            let key = id.to_string();
            let shown = match classify_type(&f.ty) {
                TypeCat::Bool => quote! { ::std::string::String::from(if r.#id { "Yes" } else { "No" }) },
                _ => show(&f.ty, id),
            };
            quote! { #key => #shown, }
        })
        .collect();

    // Search: OR of case-insensitive substring matches.
    let search = c.search.as_deref().or(c.search_fields.as_deref()).map(split_list).unwrap_or_default();
    for s in &search {
        if field_by_name(s).is_none() {
            return Err(syn::Error::new(input.span(), format!("#[cms_admin] search field `{s}` is not a field of {ident}")));
        }
    }
    let searchable = !search.is_empty();
    let search_expr = {
        let mut it = search.iter().map(|s| {
            let col = syn::Ident::new(s, proc_macro2::Span::call_site());
            quote! { #ident::#col.icontains(search) }
        });
        match it.next() {
            None => quote! {},
            Some(first) => {
                let rest: Vec<TokenStream> = it.collect();
                quote! {
                    if !search.is_empty() {
                        qs = qs.where_(#first #( .or(#rest) )*);
                    }
                }
            }
        }
    };

    // Ordering: "sort_order, -name".
    let ordering = c.ordering.as_deref().or(c.model_ordering.as_deref()).map(split_list).unwrap_or_default();
    let order_items: Vec<TokenStream> = ordering
        .iter()
        .map(|o| {
            let (name, desc) = o.strip_prefix('-').map_or((o.as_str(), false), |n| (n, true));
            quote! { (#name, #desc) }
        })
        .collect();
    let order_expr = if order_items.is_empty() {
        let pk_name = pk_ident.to_string();
        quote! { qs = qs.order_by(&[(#pk_name, false)]); }
    } else {
        quote! { qs = qs.order_by(&[#(#order_items),*]); }
    };

    let title_expr = match c.title.as_deref().or(c.display.as_deref()).and_then(field_by_name) {
        Some(f) => {
            let id = f.ident.as_ref().expect("named field");
            let shown = show(&f.ty, id);
            quote! {
                let r = self;
                let t: ::std::string::String = #shown;
                if t.trim().is_empty() {
                    ::std::format!("#{}", ::rustango_cms::admin::cms_admin::__derive::show_opt(<Self as ::rustango_cms::CmsAdmin>::admin_id(self).as_ref()))
                } else {
                    t
                }
            }
        }
        None => quote! {
            ::std::format!("#{}", ::rustango_cms::admin::cms_admin::__derive::show_opt(<Self as ::rustango_cms::CmsAdmin>::admin_id(self).as_ref()))
        },
    };

    let widget_pushes: Vec<TokenStream> = fields
        .iter()
        .map(|f| {
            let current = show(&f.ty, &f.name);
            let value = match &f.default_value {
                Some(d) => quote! {
                    if is_new { ::std::string::String::from(#d) } else { #current }
                },
                None => current,
            };
            emit_widget_call(f, value)
        })
        .collect::<Result<_>>()?;
    let applies: Vec<TokenStream> = fields.iter().map(apply).collect();

    let list_items = list.iter().zip(&headers).map(|(f, h)| quote! { (#f, #h) });

    Ok(quote! {
        #[::rustango_cms::__private::async_trait::async_trait]
        impl ::rustango_cms::CmsAdmin for #ident {
            fn admin_meta() -> &'static ::rustango_cms::CmsAdminMeta {
                static META: ::rustango_cms::CmsAdminMeta = ::rustango_cms::CmsAdminMeta {
                    slug: #slug,
                    label: #label,
                    singular: #singular,
                    icon: #icon,
                    section: ::rustango_cms::admin::cms_admin::section_from(#section),
                    sort_order: #sort_order,
                    list: &[#(#list_items),*],
                    searchable: #searchable,
                    per_page: #per_page,
                    permission_resource: #permission,
                };
                &META
            }

            fn admin_new() -> Self {
                <Self as ::core::default::Default>::default()
            }

            fn admin_id(&self) -> ::core::option::Option<i64> {
                #pk_expr
            }

            fn admin_title(&self) -> ::std::string::String {
                #title_expr
            }

            #[allow(unused_variables)]
            fn admin_cell(&self, field: &str) -> ::std::string::String {
                let r = self;
                match field {
                    #(#cell_arms)*
                    _ => ::std::string::String::new(),
                }
            }

            fn admin_widgets(&self) -> ::std::vec::Vec<::rustango_cms::Widget> {
                let r = self;
                let is_new = <Self as ::rustango_cms::CmsAdmin>::admin_id(self).is_none();
                let _ = is_new;
                let mut widgets: ::std::vec::Vec<::rustango_cms::Widget> = ::std::vec::Vec::new();
                #(#widget_pushes)*
                widgets
            }

            fn admin_apply(
                &mut self,
                form: &::rustango_cms::AdminForm,
                skip: &[&str],
            ) -> ::std::vec::Vec<::rustango_cms::FieldError> {
                let mut errors: ::std::vec::Vec<::rustango_cms::FieldError> = ::std::vec::Vec::new();
                #(#applies)*
                errors
            }

            async fn admin_list(
                pool: &::rustango_cms::__private::Pool,
                search: &str,
                limit: i64,
                offset: i64,
            ) -> ::core::result::Result<(::std::vec::Vec<Self>, i64), ::rustango_cms::__private::ExecError> {
                use ::rustango::core::Column as _;
                use ::rustango::sql::FetcherPool as _;
                use ::rustango::sql::CounterPool as _;
                let _ = search;
                let mut qs = #ident::objects();
                #search_expr
                let total = qs.clone().count(pool).await?;
                #order_expr
                let rows = qs.limit(limit).offset(offset).fetch(pool).await?;
                ::core::result::Result::Ok((rows, total))
            }

            async fn admin_get(
                pool: &::rustango_cms::__private::Pool,
                id: i64,
            ) -> ::core::result::Result<::core::option::Option<Self>, ::rustango_cms::__private::ExecError> {
                use ::rustango::sql::FetcherPool as _;
                #ident::objects().where_key(id).first(pool).await
            }

            async fn admin_save(
                &mut self,
                pool: &::rustango_cms::__private::Pool,
            ) -> ::core::result::Result<(), ::rustango_cms::__private::ExecError> {
                self.save(pool).await?;
                ::core::result::Result::Ok(())
            }

            async fn admin_delete(
                self,
                pool: &::rustango_cms::__private::Pool,
            ) -> ::core::result::Result<(), ::rustango_cms::__private::ExecError> {
                self.delete(pool).await?;
                ::core::result::Result::Ok(())
            }
        }

        ::rustango_cms::register_admin_page!(::rustango_cms::CmsAdminPage<#ident>);
    })
}
