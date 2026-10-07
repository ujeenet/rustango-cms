//! `#[derive(PageType)]` codegen.
//!
//! Two passes:
//!
//! 1. Parse struct-level `#[page_type(...)]` + adjacent `#[rustango(...)]`
//!    attrs into [`StructConfig`]. The Model derive parses `#[rustango]`
//!    independently; we re-read the bits we need (`app`, `table`).
//!
//! 2. Walk fields, parse `#[field(...)]` per-field attrs into
//!    [`FieldConfig`], remember each field's Rust type. Unit structs
//!    (no `{ … }` body) skip this — the derived handler is a
//!    no-extension page type (e.g. `HomePage`).
//!
//! Then emit:
//!
//! - `impl PageTypeHandler for $ty { … }` with the four required
//!   string methods, optional `feed_kind`, the six `<Self as
//!   PageTypeOverrides>::*` delegators, and (for field-bearing
//!   structs) `widgets`, `load_extension`, `save_extension`.
//! - `inventory::submit!` with a `PageTypeHandlerRegistration`.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{
    parse::{Parse, ParseStream},
    spanned::Spanned,
    Data, DeriveInput, Expr, Fields, Lit, Result, Token, Type,
};

/// Struct-level `#[page_type(...)]` + inherited `#[rustango(...)]`.
struct StructConfig {
    app: Option<String>,
    type_name: String,
    verbose_name: String,
    template: String,
    feed_kind: Option<String>,
    /// `#[page_type(workflow = "Editorial review")]` — opt in to a
    /// multi-step approval workflow by name. `None` ⇒ direct-publish.
    workflow: Option<String>,
    /// Extension-table name. `None` ⇒ unit-struct (no extension row).
    /// Inferred from `#[rustango(table = …)]` when the struct has
    /// `Fields::Named` and the user didn't pass `extension_table`
    /// explicitly.
    extension_table: Option<String>,
    /// `#[page_type(allowed_parents(Foo, Bar))]` — string list. `None`
    /// → delegate to `<Self as PageTypeOverrides>::allowed_parent_types`;
    /// `Some(vec)` → emit a literal `&["Foo", "Bar"]` and skip the
    /// override delegator.
    allowed_parents: Option<Vec<String>>,
    /// `#[page_type(allowed_children(Foo, Bar))]` — same shape as
    /// `allowed_parents`.
    allowed_children: Option<Vec<String>>,
    /// `#[page_type(creatable = false)]` — when `Some(false)` the type
    /// is hidden from the admin "Add page" picker.
    creatable: Option<bool>,
    /// `#[page_type(icon = "article")]` — Material-symbols name for
    /// the page-type picker tile.
    icon: Option<String>,
    /// `#[page_type(description = "…")]` — one-line description under
    /// `verbose_name` in the picker.
    description: Option<String>,
    /// `#[page_type(view_mode = "auto" | "html" | "api")]` — which
    /// representation the type serves publicly. `Some("api")` also makes
    /// the otherwise-mandatory `template` attribute optional, since a
    /// JSON-only type has no template to name. `None` → delegate to
    /// `<Self as PageTypeOverrides>::view_mode`.
    view_mode: Option<String>,
    /// `#[page_type(snippet_m2m(categories = "Category", authors = "Author"))]`
    /// — declarative Page↔Snippet many-to-many relations.
    /// Each pair is `(relation_name, snippet_type_name)`: the key is the
    /// editor-facing relation / form field, the value is the
    /// `cms_snippet.type_name` the chooser lists. Declared at the struct
    /// level (not as a `#[field]`) because the relation has no backing
    /// column — its rows live in `cms_page_snippet_m2m` — so it must not
    /// participate in the `#[derive(Model)]` column surface.
    snippet_m2m: Vec<(String, String)>,
}

impl StructConfig {
    fn parse(input: &DeriveInput) -> Result<Self> {
        let mut app: Option<String> = None;
        let mut type_name: Option<String> = None;
        let mut verbose_name: Option<String> = None;
        let mut template: Option<String> = None;
        let mut feed_kind: Option<String> = None;
        let mut workflow: Option<String> = None;
        let mut extension_table: Option<String> = None;
        let mut allowed_parents: Option<Vec<String>> = None;
        let mut allowed_children: Option<Vec<String>> = None;
        let mut creatable: Option<bool> = None;
        let mut icon: Option<String> = None;
        let mut description: Option<String> = None;
        let mut view_mode: Option<String> = None;
        let mut snippet_m2m: Vec<(String, String)> = Vec::new();
        let mut rustango_app: Option<String> = None;
        let mut rustango_table: Option<String> = None;

        for attr in &input.attrs {
            if attr.path().is_ident("page_type") {
                attr.parse_nested_meta(|meta| {
                    let key = meta
                        .path
                        .get_ident()
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    match key.as_str() {
                        "app" => app = Some(string_lit(&meta)?),
                        "type_name" => type_name = Some(string_lit(&meta)?),
                        "verbose_name" => verbose_name = Some(string_lit(&meta)?),
                        "template" => template = Some(string_lit(&meta)?),
                        "feed_kind" => feed_kind = Some(string_lit(&meta)?),
                        "workflow" => workflow = Some(string_lit(&meta)?),
                        "extension_table" => extension_table = Some(string_lit(&meta)?),
                        "allowed_parents" => {
                            let content;
                            syn::parenthesized!(content in meta.input);
                            let names: syn::punctuated::Punctuated<AllowedName, Token![,]> =
                                content.parse_terminated(AllowedName::parse, Token![,])?;
                            allowed_parents = Some(names.into_iter().map(|n| n.0).collect());
                        }
                        "allowed_children" => {
                            let content;
                            syn::parenthesized!(content in meta.input);
                            let names: syn::punctuated::Punctuated<AllowedName, Token![,]> =
                                content.parse_terminated(AllowedName::parse, Token![,])?;
                            allowed_children = Some(names.into_iter().map(|n| n.0).collect());
                        }
                        "creatable" => {
                            let value = meta.value()?;
                            let lit: syn::LitBool = value.parse()?;
                            creatable = Some(lit.value);
                        }
                        "icon" => icon = Some(string_lit(&meta)?),
                        "description" => description = Some(string_lit(&meta)?),
                        "view_mode" => {
                            let s = string_lit(&meta)?;
                            if !matches!(s.as_str(), "auto" | "html" | "api") {
                                return Err(meta.error(
                                    "#[page_type(view_mode = …)] must be one of: \
                                     \"auto\", \"html\", \"api\"",
                                ));
                            }
                            view_mode = Some(s);
                        }
                        "snippet_m2m" => {
                            // snippet_m2m(relation = "SnippetType", …) —
                            // reuse the `k = "v"` OptionPair shape.
                            let content;
                            syn::parenthesized!(content in meta.input);
                            let pairs: syn::punctuated::Punctuated<OptionPair, Token![,]> =
                                content.parse_terminated(OptionPair::parse, Token![,])?;
                            for p in pairs {
                                snippet_m2m.push((p.key, p.value));
                            }
                        }
                        other => {
                            return Err(meta.error(format!(
                                "unknown #[page_type] key `{other}` — \
                                valid: app, type_name, verbose_name, \
                                template, feed_kind, workflow, extension_table, \
                                allowed_parents, allowed_children, creatable, \
                                icon, description, view_mode, snippet_m2m"
                            )));
                        }
                    }
                    Ok(())
                })?;
            } else if attr.path().is_ident("rustango") {
                // Best-effort: pluck `app = "…"` and `table = "…"`
                // out of the Model derive's attrs without re-parsing
                // its full grammar. Unrecognized keys are silently
                // ignored — the Model derive will yell about them if
                // they're invalid.
                let _ = attr.parse_nested_meta(|meta| {
                    let key = meta
                        .path
                        .get_ident()
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    match key.as_str() {
                        "app" => {
                            if let Ok(s) = string_lit(&meta) {
                                rustango_app = Some(s);
                            }
                        }
                        "table" => {
                            if let Ok(s) = string_lit(&meta) {
                                rustango_table = Some(s);
                            }
                        }
                        _ => {
                            // Best-effort: skip any value tail so we
                            // don't choke on nested forms (admin(...)).
                            if meta.input.peek(Token![=]) {
                                let _: Token![=] = meta.input.parse()?;
                                let _: Expr = meta.input.parse()?;
                            }
                        }
                    }
                    Ok(())
                });
            }
        }

        let type_name = type_name.ok_or_else(|| {
            syn::Error::new(
                input.span(),
                "#[derive(PageType)] requires #[page_type(type_name = \"…\")]",
            )
        })?;
        let verbose_name = verbose_name.ok_or_else(|| {
            syn::Error::new(
                input.span(),
                "#[derive(PageType)] requires #[page_type(verbose_name = \"…\")]",
            )
        })?;
        // A JSON-only type has no template to name, so `view_mode = "api"`
        // waives the requirement. It stays a hard error otherwise: without
        // that guardrail a forgotten `template` would stop being a loud
        // compile failure and quietly become a template-less page type.
        let template = match template {
            Some(t) => t,
            None if view_mode.as_deref() == Some("api") => String::new(),
            None => {
                return Err(syn::Error::new(
                    input.span(),
                    "#[derive(PageType)] requires #[page_type(template = \"…\")] — \
                     or #[page_type(view_mode = \"api\")] for a JSON-only type \
                     that has no template",
                ))
            }
        };

        let app = app.or(rustango_app);

        // Only inherit table when this is a field-bearing struct.
        let has_fields = matches!(
            &input.data,
            Data::Struct(s) if matches!(s.fields, Fields::Named(_))
        );
        let extension_table = extension_table.or(if has_fields { rustango_table } else { None });

        let app = app.ok_or_else(|| {
            syn::Error::new(
                input.span(),
                "#[derive(PageType)] requires `app` — either \
                 #[page_type(app = \"…\")] or #[rustango(app = \"…\")]",
            )
        })?;

        Ok(Self {
            app: Some(app),
            type_name,
            verbose_name,
            template,
            feed_kind,
            workflow,
            extension_table,
            allowed_parents,
            allowed_children,
            creatable,
            view_mode,
            icon,
            description,
            snippet_m2m,
        })
    }
}

fn string_lit(meta: &syn::meta::ParseNestedMeta<'_>) -> Result<String> {
    let value = meta.value()?;
    let lit: syn::LitStr = value.parse()?;
    Ok(lit.value())
}

/// One `#[field(...)]`-tagged column.
struct FieldConfig {
    /// Rust field name (Ident).
    name: syn::Ident,
    /// Original Rust type — used to pick the form-parse + value
    /// stringifier.
    ty: Type,
    /// `WidgetKind` variant identifier (e.g. `Markdown`).
    widget: syn::Ident,
    label: String,
    help: Option<String>,
    placeholder: Option<String>,
    required: bool,
    /// `#[field(default = "…")]` — used as the widget's `value` when
    /// no row exists yet.
    default_value: Option<String>,
    min: Option<TokenStream>,
    max: Option<TokenStream>,
    /// `#[field(step = "0.1")]` — kept as a literal string so users
    /// can pass `"any"` for free-form decimals.
    step: Option<String>,
    max_length: Option<u32>,
    /// `#[field(options(k = "v", k2 = "v2"))]` — `(value, label)` pairs.
    options: Vec<(String, String)>,
    /// `#[field(allowed(name1, name2))]` — Stream variant.
    allowed: Vec<String>,
    /// `#[field(custom_name = "latlng")]` — required when `widget = Custom`
    /// to identify which registered custom widget template to use.
    /// Ignored for built-in `WidgetKind` variants.
    custom_name: Option<String>,
}

impl FieldConfig {
    /// Parse one struct field. Returns `Ok(None)` when there's no
    /// `#[field(...)]` attr — that column is data-only (e.g. `id`,
    /// `page_id`, `updated_at`) and doesn't surface in the editor.
    fn parse(field: &syn::Field) -> Result<Option<Self>> {
        let mut found_attr = None;
        for attr in &field.attrs {
            if attr.path().is_ident("field") {
                found_attr = Some(attr);
                break;
            }
        }
        let Some(attr) = found_attr else {
            return Ok(None);
        };
        let name = field
            .ident
            .clone()
            .ok_or_else(|| syn::Error::new(field.span(), "tuple structs not supported"))?;
        let ty = field.ty.clone();

        let mut widget: Option<syn::Ident> = None;
        let mut label: Option<String> = None;
        let mut help: Option<String> = None;
        let mut placeholder: Option<String> = None;
        let mut required = false;
        let mut default_value: Option<String> = None;
        let mut min: Option<TokenStream> = None;
        let mut max: Option<TokenStream> = None;
        let mut step: Option<String> = None;
        let mut max_length: Option<u32> = None;
        let mut options: Vec<(String, String)> = Vec::new();
        let mut allowed: Vec<String> = Vec::new();
        let mut custom_name: Option<String> = None;

        attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default();
            match key.as_str() {
                "widget" => {
                    let value = meta.value()?;
                    let id: syn::Ident = value.parse()?;
                    widget = Some(id);
                }
                "label" => label = Some(string_lit(&meta)?),
                "help" => help = Some(string_lit(&meta)?),
                "placeholder" => placeholder = Some(string_lit(&meta)?),
                "required" => required = true,
                "default" => default_value = Some(string_lit(&meta)?),
                "min" => {
                    let value = meta.value()?;
                    let lit: Lit = value.parse()?;
                    min = Some(quote!(#lit));
                }
                "max" => {
                    let value = meta.value()?;
                    let lit: Lit = value.parse()?;
                    max = Some(quote!(#lit));
                }
                "step" => {
                    let value = meta.value()?;
                    // Accept either a string ("any") or a number (5).
                    if value.peek(syn::LitStr) {
                        let lit: syn::LitStr = value.parse()?;
                        step = Some(lit.value());
                    } else {
                        let lit: Lit = value.parse()?;
                        step = Some(quote!(#lit).to_string());
                    }
                }
                "max_length" => {
                    let value = meta.value()?;
                    let lit: syn::LitInt = value.parse()?;
                    max_length = Some(lit.base10_parse()?);
                }
                "options" => {
                    // options(k1 = "v1", k2 = "v2", …)
                    let content;
                    syn::parenthesized!(content in meta.input);
                    let pairs: syn::punctuated::Punctuated<OptionPair, Token![,]> =
                        content.parse_terminated(OptionPair::parse, Token![,])?;
                    for p in pairs {
                        options.push((p.key, p.value));
                    }
                }
                "allowed" => {
                    // allowed(name1, name2, …) — accept idents OR string literals.
                    let content;
                    syn::parenthesized!(content in meta.input);
                    let names: syn::punctuated::Punctuated<AllowedName, Token![,]> =
                        content.parse_terminated(AllowedName::parse, Token![,])?;
                    for n in names {
                        allowed.push(n.0);
                    }
                }
                "custom_name" => custom_name = Some(string_lit(&meta)?),
                other => {
                    return Err(meta.error(format!(
                        "unknown #[field] key `{other}` — valid: widget, label, \
                         help, placeholder, required, default, min, max, step, \
                         max_length, options, allowed, custom_name"
                    )));
                }
            }
            Ok(())
        })?;

        let widget = widget
            .ok_or_else(|| syn::Error::new(attr.span(), "#[field(widget = …)] is required"))?;
        let label = label
            .ok_or_else(|| syn::Error::new(attr.span(), "#[field(label = \"…\")] is required"))?;

        Ok(Some(Self {
            name,
            ty,
            widget,
            label,
            help,
            placeholder,
            required,
            default_value,
            min,
            max,
            step,
            max_length,
            options,
            allowed,
            custom_name,
        }))
    }
}

/// `options(k = "v")` parser — accepts `k = "v"` per pair.
struct OptionPair {
    key: String,
    value: String,
}

impl Parse for OptionPair {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        // key can be an ident or a string literal
        let key = if input.peek(syn::LitStr) {
            let lit: syn::LitStr = input.parse()?;
            lit.value()
        } else {
            let id: syn::Ident = input.parse()?;
            id.to_string()
        };
        let _: Token![=] = input.parse()?;
        let value: syn::LitStr = input.parse()?;
        Ok(Self {
            key,
            value: value.value(),
        })
    }
}

struct AllowedName(String);
impl Parse for AllowedName {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        if input.peek(syn::LitStr) {
            let lit: syn::LitStr = input.parse()?;
            Ok(Self(lit.value()))
        } else {
            let id: syn::Ident = input.parse()?;
            Ok(Self(id.to_string()))
        }
    }
}

/// Top-level expand.
pub fn expand(input: DeriveInput) -> Result<TokenStream> {
    let cfg = StructConfig::parse(&input)?;
    let ident = &input.ident;

    let fields: Vec<FieldConfig> = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => named
                .named
                .iter()
                .filter_map(|f| FieldConfig::parse(f).transpose())
                .collect::<Result<_>>()?,
            Fields::Unit => Vec::new(),
            Fields::Unnamed(_) => {
                return Err(syn::Error::new(
                    input.span(),
                    "tuple structs are not supported",
                ))
            }
        },
        _ => {
            return Err(syn::Error::new(
                input.span(),
                "#[derive(PageType)] only supports structs",
            ))
        }
    };

    let app = cfg.app.as_deref().unwrap();
    let type_name = &cfg.type_name;
    let verbose_name = &cfg.verbose_name;
    let template = &cfg.template;
    let feed_kind_method = match cfg.feed_kind.as_deref() {
        Some(slug) => quote! {
            fn feed_kind(&self) -> ::core::option::Option<&'static str> {
                ::core::option::Option::Some(#slug)
            }
        },
        None => quote! {
            fn feed_kind(&self) -> ::core::option::Option<&'static str> {
                <Self as ::rustango_cms::PageTypeOverrides>::feed_kind(self)
            }
        },
    };
    let view_mode_method = match cfg.view_mode.as_deref() {
        Some(mode) => {
            let variant = syn::Ident::new(
                match mode {
                    "api" => "Api",
                    "html" => "Html",
                    _ => "Auto",
                },
                proc_macro2::Span::call_site(),
            );
            quote! {
                fn view_mode(&self) -> ::rustango_cms::PageViewMode {
                    ::rustango_cms::PageViewMode::#variant
                }
            }
        }
        None => quote! {
            fn view_mode(&self) -> ::rustango_cms::PageViewMode {
                <Self as ::rustango_cms::PageTypeOverrides>::view_mode(self)
            }
        },
    };
    let workflow_method = match cfg.workflow.as_deref() {
        Some(slug) => quote! {
            fn workflow_slug(&self) -> ::core::option::Option<&'static str> {
                ::core::option::Option::Some(#slug)
            }
        },
        None => quote! {
            fn workflow_slug(&self) -> ::core::option::Option<&'static str> {
                <Self as ::rustango_cms::PageTypeOverrides>::workflow_slug(self)
            }
        },
    };

    // Topology + creatable methods: when the struct-level attr is
    // present, emit a literal return. Otherwise delegate to
    // `PageTypeOverrides` so the author can override there. This is
    // the "ergonomic configuration" surface — most types only need
    // these as data, not as method overrides.
    let allowed_parents_method = match &cfg.allowed_parents {
        Some(names) => {
            let lits = names.iter();
            quote! {
                fn allowed_parent_types(&self) -> &'static [&'static str] {
                    &[ #( #lits ),* ]
                }
            }
        }
        None => quote! {
            fn allowed_parent_types(&self) -> &'static [&'static str] {
                <Self as ::rustango_cms::PageTypeOverrides>::allowed_parent_types(self)
            }
        },
    };
    let allowed_children_method = match &cfg.allowed_children {
        Some(names) => {
            let lits = names.iter();
            quote! {
                fn allowed_child_types(&self) -> &'static [&'static str] {
                    &[ #( #lits ),* ]
                }
            }
        }
        None => quote! {
            fn allowed_child_types(&self) -> &'static [&'static str] {
                <Self as ::rustango_cms::PageTypeOverrides>::allowed_child_types(self)
            }
        },
    };
    let is_creatable_method = match cfg.creatable {
        Some(b) => quote! {
            fn is_creatable(&self) -> bool { #b }
        },
        None => quote! {
            fn is_creatable(&self) -> bool {
                <Self as ::rustango_cms::PageTypeOverrides>::is_creatable(self)
            }
        },
    };
    let icon_method = match cfg.icon.as_deref() {
        Some(name) => quote! {
            fn icon(&self) -> ::core::option::Option<&'static str> {
                ::core::option::Option::Some(#name)
            }
        },
        None => quote! {
            fn icon(&self) -> ::core::option::Option<&'static str> {
                ::core::option::Option::None
            }
        },
    };
    let description_method = match cfg.description.as_deref() {
        Some(text) => quote! {
            fn description(&self) -> ::core::option::Option<&'static str> {
                ::core::option::Option::Some(#text)
            }
        },
        None => quote! {
            fn description(&self) -> ::core::option::Option<&'static str> {
                ::core::option::Option::None
            }
        },
    };

    // The widgets / load_extension / save_extension bodies depend on
    // whether this struct has a backing extension table (column-bearing
    // `#[field]`s) and/or declares Page↔Snippet M2M relations. Either is
    // sufficient to emit real bodies; a unit struct with neither stays
    // a no-op. M2M relations are handled independently of the extension
    // row — they persist to `cms_page_snippet_m2m`, not a column — so a
    // pure-M2M page type with no extension table still gets live bodies.
    let has_ext = !fields.is_empty() && cfg.extension_table.is_some();
    let m2m = cfg.snippet_m2m.clone();
    // Reject silent footguns at expand time: a duplicate relation name
    // would emit two widgets / double saves under one form key, and a
    // relation that shadows a `#[field]` column would clobber it in the
    // shared form map + loaded extension JSON.
    {
        let field_names: ::std::collections::HashSet<String> =
            fields.iter().map(|f| f.name.to_string()).collect();
        let mut seen: ::std::collections::HashSet<&str> = ::std::collections::HashSet::new();
        for (rel, _ty) in &m2m {
            if !seen.insert(rel.as_str()) {
                return Err(syn::Error::new(
                    input.span(),
                    format!("duplicate snippet_m2m relation `{rel}`"),
                ));
            }
            if field_names.contains(rel) {
                return Err(syn::Error::new(
                    input.span(),
                    format!(
                        "snippet_m2m relation `{rel}` collides with a #[field] column of the same name"
                    ),
                ));
            }
        }
    }
    let (widgets_body, load_body, save_body) = if !has_ext && m2m.is_empty() {
        let empty_widgets = quote! {
            async fn widgets(
                &self,
                _pool: &::rustango_cms::__private::Pool,
                _page_id: i64,
            ) -> ::core::result::Result<
                ::std::vec::Vec<::rustango_cms::Widget>,
                ::rustango_cms::__private::ExecError,
            > {
                ::core::result::Result::Ok(::std::vec::Vec::new())
            }
        };
        let empty_load = quote! {
            async fn load_extension(
                &self,
                _pool: &::rustango_cms::__private::Pool,
                _page_id: i64,
            ) -> ::core::result::Result<
                ::serde_json::Value,
                ::rustango_cms::__private::ExecError,
            > {
                ::core::result::Result::Ok(::serde_json::Value::Null)
            }
        };
        let empty_save = quote! {
            async fn save_extension(
                &self,
                _pool: &::rustango_cms::__private::Pool,
                _page_id: i64,
                _form: &::std::collections::HashMap<::std::string::String, ::std::string::String>,
            ) -> ::core::result::Result<(), ::rustango_cms::__private::ExecError> {
                ::core::result::Result::Ok(())
            }
        };
        (empty_widgets, empty_load, empty_save)
    } else {
        let widgets = emit_widgets_body(ident, &fields, has_ext, &m2m)?;
        let load = emit_load_body(ident, has_ext, &m2m);
        let save = emit_save_body(ident, &fields, has_ext, &m2m)?;
        (widgets, load, save)
    };

    // #243 — surface the declared M2M relations to the public renderer,
    // which resolves each into `snippet_relations.<name>`. Only emitted
    // when relations are declared; the trait default returns none.
    let m2m_relations_method = if m2m.is_empty() {
        quote! {}
    } else {
        let pairs = m2m.iter().map(|(rel, ty)| quote! { (#rel, #ty) });
        quote! {
            fn snippet_m2m_relations(&self) -> ::std::vec::Vec<(&'static str, &'static str)> {
                ::std::vec![ #(#pairs),* ]
            }
        }
    };

    let registration = quote! {
        ::rustango_cms::__private::inventory::submit! {
            ::rustango_cms::PageTypeHandlerRegistration {
                factory: || ::std::boxed::Box::new(<#ident as ::core::default::Default>::default()),
            }
        }
    };

    Ok(quote! {
        #[::rustango_cms::__private::async_trait::async_trait]
        impl ::rustango_cms::PageTypeHandler for #ident {
            fn app_label(&self) -> &'static str { #app }
            fn type_name(&self) -> &'static str { #type_name }
            fn verbose_name(&self) -> &'static str { #verbose_name }
            fn default_template(&self) -> &'static str { #template }

            #feed_kind_method
            #view_mode_method
            #workflow_method
            #allowed_parents_method
            #allowed_children_method
            #is_creatable_method
            #icon_method
            #description_method

            fn view_restriction(
                &self,
            ) -> ::core::option::Option<::rustango_cms::TypeViewRestriction> {
                <Self as ::rustango_cms::PageTypeOverrides>::view_restriction(self)
            }

            async fn display_fields(
                &self,
                pool: &::rustango_cms::__private::Pool,
                page_id: i64,
            ) -> ::core::result::Result<
                ::std::vec::Vec<::rustango_cms::DisplayField>,
                ::rustango_cms::__private::ExecError,
            > {
                <Self as ::rustango_cms::PageTypeOverrides>::display_fields(self, pool, page_id).await
            }

            async fn extra_tabs(
                &self,
                pool: &::rustango_cms::__private::Pool,
                page_id: i64,
            ) -> ::core::result::Result<
                ::std::vec::Vec<::rustango_cms::TabSpec>,
                ::rustango_cms::__private::ExecError,
            > {
                <Self as ::rustango_cms::PageTypeOverrides>::extra_tabs(self, pool, page_id).await
            }

            // #246 — handler-injected template ctx. Forwards to
            // `PageTypeOverrides::public_context`; default returns
            // an empty map.
            async fn public_context(
                &self,
                pool: &::rustango_cms::__private::Pool,
                page: &::rustango_cms::Page,
            ) -> ::core::result::Result<
                ::serde_json::Map<::std::string::String, ::serde_json::Value>,
                ::rustango_cms::__private::ExecError,
            > {
                <Self as ::rustango_cms::PageTypeOverrides>::public_context(self, pool, page).await
            }

            // Children listing. Forwards to
            // `PageTypeOverrides::children_query`; when that returns
            // `None` (the default) fall back to the framework default
            // (all immediate children, `sort_order, id`, drafts included)
            // so a type that doesn't override behaves exactly as before.
            async fn children_query(
                &self,
                pool: &::rustango_cms::__private::Pool,
                page: &::rustango_cms::Page,
            ) -> ::core::result::Result<
                ::std::vec::Vec<::rustango_cms::Page>,
                ::rustango_cms::__private::ExecError,
            > {
                match <Self as ::rustango_cms::PageTypeOverrides>::children_query(self, pool, page)
                    .await?
                {
                    ::core::option::Option::Some(pages) => ::core::result::Result::Ok(pages),
                    ::core::option::Option::None => {
                        ::rustango_cms::default_children(pool, page).await
                    }
                }
            }

            // #198 — routable_page URL patterns. Forwards to
            // `PageTypeOverrides::routes` / `route_context`; defaults
            // return empty (no routable surface) so existing handlers
            // that don't override stay non-routable.
            fn routes(&self) -> ::std::vec::Vec<::rustango_cms::routable::RouteSpec> {
                <Self as ::rustango_cms::PageTypeOverrides>::routes(self)
            }

            async fn route_context(
                &self,
                pool: &::rustango_cms::__private::Pool,
                page: &::rustango_cms::Page,
                matched: &::rustango_cms::routable::RouteMatch,
            ) -> ::core::result::Result<
                ::serde_json::Map<::std::string::String, ::serde_json::Value>,
                ::rustango_cms::__private::ExecError,
            > {
                <Self as ::rustango_cms::PageTypeOverrides>::route_context(self, pool, page, matched)
                    .await
            }

            #m2m_relations_method
            #widgets_body
            #load_body
            #save_body
        }

        #registration
    })
}

/// Emit `widgets()` — load the extension row (if any) + build one Widget
/// per `#[field]`, then append one SnippetM2M chooser per declared
/// `snippet_m2m` relation (options fetched from `cms_snippet` by type,
/// current selection from `cms_page_snippet_m2m`).
fn emit_widgets_body(
    struct_ident: &syn::Ident,
    fields: &[FieldConfig],
    has_ext: bool,
    m2m: &[(String, String)],
) -> Result<TokenStream> {
    let ext_part = if has_ext {
        let with_row: Vec<TokenStream> = fields
            .iter()
            .map(|f| emit_widget_call(struct_ident, f, true))
            .collect::<Result<_>>()?;
        let no_row: Vec<TokenStream> = fields
            .iter()
            .map(|f| emit_widget_call(struct_ident, f, false))
            .collect::<Result<_>>()?;
        quote! {
            let row: ::core::option::Option<#struct_ident> = #struct_ident::objects()
                .where_(#struct_ident::page_id.eq(page_id))
                .fetch(pool)
                .await?
                .into_iter()
                .next();
            match row {
                ::core::option::Option::Some(r) => {
                    #(#with_row)*
                }
                ::core::option::Option::None => {
                    #(#no_row)*
                }
            }
        }
    } else {
        quote! {}
    };

    let m2m_pushes: Vec<TokenStream> = m2m
        .iter()
        .map(|(relation, ty)| {
            let label = humanize_label(relation);
            quote! {
                {
                    // Eligible candidates: every snippet of this type,
                    // surfaced as (id_string, title) for the chooser, in
                    // a deterministic title order. Rows without a set id
                    // are dropped rather than offered as a fake "0".
                    let __opts: ::std::vec::Vec<(::std::string::String, ::std::string::String)> =
                        ::rustango_cms::Snippet::objects()
                            .where_(::rustango_cms::Snippet::type_name.eq(::std::string::String::from(#ty)))
                            .order_by(&[("title", false)])
                            .fetch(pool)
                            .await?
                            .into_iter()
                            .filter_map(|s| {
                                s.id.get()
                                    .copied()
                                    .map(|id| (id.to_string(), s.title.clone()))
                            })
                            .collect();
                    // Current selection in chooser order → JSON id array.
                    let __rows = ::rustango_cms::page_snippet_m2m::rows_for_relation(
                        pool, page_id, #relation,
                    )
                    .await?;
                    let __ids: ::std::vec::Vec<i64> =
                        __rows.iter().map(|r| r.snippet_id).collect();
                    let __value = ::serde_json::to_string(&__ids)
                        .unwrap_or_else(|_| ::std::string::String::from("[]"));
                    widgets.push(
                        ::rustango_cms::Widget::snippet_m2m(#relation, #label, #ty)
                            .with_options(__opts)
                            .with_value(__value),
                    );
                }
            }
        })
        .collect();

    Ok(quote! {
        async fn widgets(
            &self,
            pool: &::rustango_cms::__private::Pool,
            page_id: i64,
        ) -> ::core::result::Result<
            ::std::vec::Vec<::rustango_cms::Widget>,
            ::rustango_cms::__private::ExecError,
        > {
            use ::rustango::core::Column as _;
            use ::rustango::sql::FetcherPool as _;

            let mut widgets: ::std::vec::Vec<::rustango_cms::Widget> = ::std::vec::Vec::new();
            #ext_part
            #(#m2m_pushes)*
            ::core::result::Result::Ok(widgets)
        }
    })
}

/// Emit one Widget constructor + builder chain.
fn emit_widget_call(
    _struct_ident: &syn::Ident,
    f: &FieldConfig,
    with_row: bool,
) -> Result<TokenStream> {
    let name_str = f.name.to_string();
    let label = &f.label;
    let widget_ident = &f.widget;
    let widget_str = widget_ident.to_string();

    // Pre-compute the per-field value-stringification used by the
    // editor's "pre-fill" path. Differs per Rust type.
    let value_expr = if with_row {
        emit_value_stringify(&f.ty, &f.name)
    } else {
        match &f.default_value {
            Some(d) => quote! { ::std::string::String::from(#d) },
            None => quote! { ::std::string::String::new() },
        }
    };

    let is_stream = widget_str == "Stream";
    let is_custom = widget_str == "Custom";
    // #421 — `widget = ModelChooser` declares a chooser for a
    // `register_chooser!`ed model; the registered slug rides in
    // `custom_name` (→ `Widget::model_chooser`, which sets it as the
    // `data-chooser-kind` the overlay resolves).
    let is_model_chooser = widget_str == "ModelChooser";

    if is_custom && f.custom_name.is_none() {
        return Err(syn::Error::new(
            f.name.span(),
            "#[field(widget = Custom)] requires `custom_name = \"…\"` so the \
             custom-widget registry knows which template to render",
        ));
    }
    if is_model_chooser && f.custom_name.is_none() {
        return Err(syn::Error::new(
            f.name.span(),
            "#[field(widget = ModelChooser)] requires `custom_name = \"<slug>\"` — \
             the `register_chooser!` slug the chooser resolves against",
        ));
    }

    let constructor = if is_stream {
        let allowed = &f.allowed;
        quote! {
            ::rustango_cms::Widget::stream(
                #name_str,
                #label,
                [#( #allowed ),*],
            )
        }
    } else if is_custom {
        // SAFETY: checked above that custom_name is Some.
        let custom_name = f.custom_name.as_deref().unwrap();
        quote! {
            ::rustango_cms::Widget::custom(
                #custom_name,
                #name_str,
                #label,
            )
        }
    } else if is_model_chooser {
        // SAFETY: checked above that custom_name is Some.
        let slug = f.custom_name.as_deref().unwrap();
        quote! {
            ::rustango_cms::Widget::model_chooser(
                #name_str,
                #label,
                #slug,
            )
        }
    } else {
        quote! {
            ::rustango_cms::Widget::new(
                ::rustango_cms::WidgetKind::#widget_ident,
                #name_str,
                #label,
            )
        }
    };

    // Builder chain — only emit setters whose attr is present.
    let mut chain = TokenStream::new();
    // value: always emit (even for empty)
    chain.extend(quote! { .with_value(#value_expr) });
    if let Some(help) = &f.help {
        chain.extend(quote! { .with_help(#help) });
    }
    if let Some(placeholder) = &f.placeholder {
        chain.extend(quote! { .with_placeholder(#placeholder) });
    }
    if f.required {
        chain.extend(quote! { .required() });
    }
    if let Some(min) = &f.min {
        chain.extend(quote! { .with_min(#min as f64) });
    }
    if let Some(max) = &f.max {
        chain.extend(quote! { .with_max(#max as f64) });
    }
    if let Some(step) = &f.step {
        chain.extend(quote! { .with_step(#step) });
    }
    if let Some(ml) = f.max_length {
        chain.extend(quote! { .with_max_length(#ml) });
    }
    if !f.options.is_empty() && !is_stream {
        let pairs = f.options.iter().map(|(k, v)| quote! { (#k, #v) });
        chain.extend(quote! { .with_options([#(#pairs),*]) });
    }

    Ok(quote! {
        widgets.push(#constructor #chain);
    })
}

/// `r.<field>` → owned String for the widget's `value` slot.
fn emit_value_stringify(ty: &Type, field: &syn::Ident) -> TokenStream {
    let cat = classify_type(ty);
    match cat {
        TypeCat::String => quote! { r.#field.clone() },
        TypeCat::OptionString => {
            quote! { r.#field.clone().unwrap_or_default() }
        }
        TypeCat::Int => quote! { r.#field.to_string() },
        TypeCat::OptionInt => {
            quote! { r.#field.map(|v| v.to_string()).unwrap_or_default() }
        }
        TypeCat::Float => quote! { r.#field.to_string() },
        TypeCat::OptionFloat => {
            quote! { r.#field.map(|v| v.to_string()).unwrap_or_default() }
        }
        TypeCat::Bool => {
            quote! { if r.#field { ::std::string::String::from("true") } else { ::std::string::String::new() } }
        }
        // Unknown — fall back to Display via to_string().
        TypeCat::Unknown => quote! { r.#field.to_string() },
    }
}

/// Emit `load_extension()`. With no M2M relations this is the original
/// "fetch row, serialize to JSON (or Null)" behavior. With M2M relations
/// it returns a JSON object merging the extension row's fields (if any)
/// with one `"<relation>": [snippet ids…]` key per relation, so public
/// templates can read `extension.<relation>`.
fn emit_load_body(
    struct_ident: &syn::Ident,
    has_ext: bool,
    m2m: &[(String, String)],
) -> TokenStream {
    if m2m.is_empty() {
        // No M2M — `has_ext` is guaranteed true here (else this body
        // isn't emitted). Preserve the exact original shape.
        return quote! {
            async fn load_extension(
                &self,
                pool: &::rustango_cms::__private::Pool,
                page_id: i64,
            ) -> ::core::result::Result<
                ::serde_json::Value,
                ::rustango_cms::__private::ExecError,
            > {
                use ::rustango::core::Column as _;
                use ::rustango::sql::FetcherPool as _;

                let row: ::core::option::Option<#struct_ident> = #struct_ident::objects()
                    .where_(#struct_ident::page_id.eq(page_id))
                    .fetch(pool)
                    .await?
                    .into_iter()
                    .next();
                ::core::result::Result::Ok(match row {
                    ::core::option::Option::Some(r) => ::serde_json::to_value(r)
                        .unwrap_or(::serde_json::Value::Null),
                    ::core::option::Option::None => ::serde_json::Value::Null,
                })
            }
        };
    }

    let ext_load = if has_ext {
        quote! {
            {
                use ::rustango::core::Column as _;
                use ::rustango::sql::FetcherPool as _;
                let row: ::core::option::Option<#struct_ident> = #struct_ident::objects()
                    .where_(#struct_ident::page_id.eq(page_id))
                    .fetch(pool)
                    .await?
                    .into_iter()
                    .next();
                if let ::core::option::Option::Some(r) = row {
                    if let ::serde_json::Value::Object(m) =
                        ::serde_json::to_value(r).unwrap_or(::serde_json::Value::Null)
                    {
                        for (k, v) in m {
                            __obj.insert(k, v);
                        }
                    }
                }
            }
        }
    } else {
        quote! {}
    };

    let m2m_loads: Vec<TokenStream> = m2m
        .iter()
        .map(|(relation, _ty)| {
            quote! {
                {
                    // Resolve to full Snippet rows (in chooser order) so a
                    // public template reading `extension.<relation>` gets
                    // objects with `.title` / `.slug` / `.body_markdown`,
                    // matching the documented `{% for c in page.<rel> %}`
                    // accessor (#243 AC: "reads back as Vec<SnippetRef>").
                    let __snippets = ::rustango_cms::page_snippet_m2m::related_snippets(
                        pool, page_id, #relation,
                    )
                    .await?;
                    __obj.insert(
                        ::std::string::String::from(#relation),
                        ::serde_json::to_value(__snippets).unwrap_or(::serde_json::Value::Null),
                    );
                }
            }
        })
        .collect();

    quote! {
        async fn load_extension(
            &self,
            pool: &::rustango_cms::__private::Pool,
            page_id: i64,
        ) -> ::core::result::Result<
            ::serde_json::Value,
            ::rustango_cms::__private::ExecError,
        > {
            let mut __obj = ::serde_json::Map::new();
            #ext_load
            #(#m2m_loads)*
            ::core::result::Result::Ok(::serde_json::Value::Object(__obj))
        }
    }
}

/// Emit `save_extension()` — upsert the extension row from the form (when
/// the struct has column-bearing `#[field]`s) and persist each declared
/// `snippet_m2m` relation into `cms_page_snippet_m2m` via `replace_all`.
fn emit_save_body(
    struct_ident: &syn::Ident,
    fields: &[FieldConfig],
    has_ext: bool,
    m2m: &[(String, String)],
) -> Result<TokenStream> {
    // The column-upsert (and its trait imports) is scoped inside its own
    // block so a pure-M2M page type — which never touches the extension
    // row — doesn't emit unused `use` warnings.
    let ext_part = if has_ext {
        let assigns_update: Vec<TokenStream> = fields
            .iter()
            .map(|f| {
                let name = &f.name;
                let key = name.to_string();
                let parse_expr = emit_form_parse(&f.ty, &key);
                quote! { row.#name = #parse_expr; }
            })
            .collect();
        let assigns_insert: Vec<TokenStream> = fields
            .iter()
            .map(|f| {
                let name = &f.name;
                let key = name.to_string();
                let parse_expr = emit_form_parse(&f.ty, &key);
                quote! { #name: #parse_expr, }
            })
            .collect();
        quote! {
            {
                use ::rustango::core::Column as _;
                use ::rustango::sql::FetcherPool as _;
                use ::rustango::Model as _;

                let existing: ::core::option::Option<#struct_ident> = #struct_ident::objects()
                    .where_(#struct_ident::page_id.eq(page_id))
                    .fetch(pool)
                    .await?
                    .into_iter()
                    .next();
                if let ::core::option::Option::Some(mut row) = existing {
                    #(#assigns_update)*
                    row.save_pool(pool).await?;
                } else {
                    let mut row = #struct_ident {
                        page_id,
                        #(#assigns_insert)*
                        ..::core::default::Default::default()
                    };
                    row.save_pool(pool).await?;
                }
            }
        }
    } else {
        quote! {}
    };

    let m2m_saves: Vec<TokenStream> = m2m
        .iter()
        .map(|(relation, _ty)| {
            quote! {
                {
                    // The chooser posts a JSON array of i64 snippet ids
                    // under the relation name (see _widget.html snippetm2m
                    // arm). Only touch the relation when the form actually
                    // carries the key AND it parses: an absent key means
                    // "this form didn't edit the relation", and a malformed
                    // payload must NOT silently wipe the existing links
                    // (replace_all with an empty set deletes everything).
                    if let ::core::option::Option::Some(__raw) = form.get(#relation) {
                        match ::serde_json::from_str::<::std::vec::Vec<i64>>(__raw) {
                            ::core::result::Result::Ok(__ids) => {
                                ::rustango_cms::page_snippet_m2m::replace_all(
                                    pool, page_id, #relation, &__ids,
                                )
                                .await?;
                            }
                            ::core::result::Result::Err(_) => {
                                // Malformed value — leave the relation intact.
                            }
                        }
                    }
                }
            }
        })
        .collect();

    Ok(quote! {
        async fn save_extension(
            &self,
            pool: &::rustango_cms::__private::Pool,
            page_id: i64,
            form: &::std::collections::HashMap<::std::string::String, ::std::string::String>,
        ) -> ::core::result::Result<(), ::rustango_cms::__private::ExecError> {
            #ext_part
            #(#m2m_saves)*
            ::core::result::Result::Ok(())
        }
    })
}

/// `form.get(key)` → typed value for one column.
fn emit_form_parse(ty: &Type, key: &str) -> TokenStream {
    match classify_type(ty) {
        TypeCat::String => quote! {
            form.get(#key).cloned().unwrap_or_default()
        },
        TypeCat::OptionString => quote! {
            form.get(#key).and_then(|s| {
                let t = s.trim();
                if t.is_empty() { ::core::option::Option::None }
                else { ::core::option::Option::Some(s.clone()) }
            })
        },
        TypeCat::Int => quote! {
            form.get(#key).and_then(|s| s.parse::<i64>().ok()).unwrap_or_default()
        },
        TypeCat::OptionInt => quote! {
            form.get(#key).and_then(|s| s.parse::<i64>().ok())
        },
        TypeCat::Float => quote! {
            form.get(#key).and_then(|s| s.parse::<f64>().ok()).unwrap_or_default()
        },
        TypeCat::OptionFloat => quote! {
            form.get(#key).and_then(|s| s.parse::<f64>().ok())
        },
        TypeCat::Bool => quote! {
            ::core::matches!(form.get(#key).map(|s| s.as_str()), ::core::option::Option::Some("on" | "true" | "1" | "yes"))
        },
        TypeCat::Unknown => quote! {
            // The macro doesn't know how to parse this Rust type from
            // a form string. Fall back to attempting `parse()` via
            // `FromStr` — the consumer's row may implement it.
            form.get(#key).and_then(|s| s.parse().ok()).unwrap_or_default()
        },
    }
}

#[derive(Debug, Clone, Copy)]
enum TypeCat {
    String,
    OptionString,
    Int,
    OptionInt,
    Float,
    OptionFloat,
    Bool,
    Unknown,
}

fn classify_type(ty: &Type) -> TypeCat {
    let last = path_last_segment(ty);
    match last.as_deref() {
        Some("String") => TypeCat::String,
        Some("bool") => TypeCat::Bool,
        Some("i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize") => {
            TypeCat::Int
        }
        Some("f32" | "f64") => TypeCat::Float,
        Some("Option") => {
            let inner = option_inner(ty);
            match inner.as_deref() {
                Some("String") => TypeCat::OptionString,
                Some(
                    "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize",
                ) => TypeCat::OptionInt,
                Some("f32" | "f64") => TypeCat::OptionFloat,
                _ => TypeCat::Unknown,
            }
        }
        _ => TypeCat::Unknown,
    }
}

fn path_last_segment(ty: &Type) -> Option<String> {
    if let Type::Path(tp) = ty {
        tp.path.segments.last().map(|s| s.ident.to_string())
    } else {
        None
    }
}

fn option_inner(ty: &Type) -> Option<String> {
    if let Type::Path(tp) = ty {
        let last = tp.path.segments.last()?;
        if last.ident != "Option" {
            return None;
        }
        if let syn::PathArguments::AngleBracketed(args) = &last.arguments {
            for arg in &args.args {
                if let syn::GenericArgument::Type(inner_ty) = arg {
                    return path_last_segment(inner_ty);
                }
            }
        }
    }
    None
}

/// Derive a human display label from a relation name:
/// `"categories"` → `"Categories"`, `"featured_authors"` → `"Featured
/// Authors"`. Used as the SnippetM2M widget label when the author
/// declares the relation at the struct level (no per-field label).
fn humanize_label(s: &str) -> String {
    s.split(['_', '-', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// `format_ident!` is imported but not used directly yet; keep so future
// codegen tweaks don't need a fresh import.
#[allow(dead_code)]
fn _format_ident_unused() {
    let _ = format_ident!("placeholder");
}
