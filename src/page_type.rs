//! Page types: the [`PageTypeHandler`] trait every page kind implements,
//! its inventory registry ([`find_handler`], [`registered_handlers`]),
//! and the editor-form types the handler returns ([`ExtensionField`],
//! [`TabSpec`], [`InlinePanelSpec`], [`DisplayField`]).
//!
//! `#[derive(PageType)]` generates the handler and forwards the
//! author-overridable hooks to [`PageTypeOverrides`].

use async_trait::async_trait;
use rustango::sql::{ExecError, Pool};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Form-field metadata returned by [`PageTypeHandler::extension_fields`].
/// The admin's page-edit form renders one input per entry under the
/// "Content" section so authors can write into the type's extension
/// table without leaving the editor.
///
/// Kept stable for back-compat with v0.x handler authors. Internally
/// the admin converts to the richer [`crate::Widget`] before
/// rendering — new handlers can also return `Widget`s directly via
/// [`Self::from_widget`] for access to the full widget set (Date,
/// Color, Choice, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionField {
    /// HTML form `name` — also the key on the form submission. Used
    /// by [`PageTypeHandler::save_extension`] to pull the value back
    /// out of the posted form.
    pub name: String,
    /// Display label for the input.
    pub label: String,
    /// Render hint — picks which widget the admin draws.
    pub kind: ExtensionFieldKind,
    /// Pre-fill value (the current row's value, when editing).
    pub value: String,
    /// Optional helptext shown below the input.
    pub help: String,
    /// Optional max length (string/text inputs only).
    pub max_length: Option<u32>,
}

impl ExtensionField {
    /// Convert into a [`crate::Widget`] — the unified widget
    /// descriptor the admin renderer + the stream-block editor both
    /// consume. `options` is left empty (the admin handler
    /// populates it for `MediaPicker` widgets from the media
    /// library).
    #[must_use]
    pub fn into_widget(self) -> crate::Widget {
        crate::Widget {
            name: self.name,
            kind: self.kind.into(),
            custom_name: String::new(),
            custom_html: String::new(),
            label: self.label,
            value: self.value,
            options: Vec::new(),
            help: self.help,
            placeholder: String::new(),
            required: false,
            min: None,
            max: None,
            step: None,
            max_length: self.max_length,
            min_length: None,
            pattern: String::new(),
            read_only: false,
            allowed: Vec::new(),
            variant: String::new(),
        }
    }

    /// Wrap a [`crate::Widget`] back into the legacy
    /// [`ExtensionField`] shape — useful for handlers that author
    /// against the richer widget set but still want to return the
    /// legacy `Vec<ExtensionField>`.
    ///
    /// `max_length`, `value`, `help`, `label`, `name` are preserved.
    /// Widget kinds outside the legacy set
    /// ([`ExtensionFieldKind`]'s six variants) downgrade to
    /// [`ExtensionFieldKind::Text`] — handlers that need the full
    /// set should switch to returning `Vec<Widget>` (a follow-up
    /// trait method lands with the stream-block slice).
    #[must_use]
    pub fn from_widget(w: crate::Widget) -> Self {
        let kind = ExtensionFieldKind::from_widget_kind(w.kind);
        Self {
            name: w.name,
            label: w.label,
            kind,
            value: w.value,
            help: w.help,
            max_length: w.max_length,
        }
    }
}

/// Which admin widget to draw for an [`ExtensionField`].
///
/// Stable v0.x surface — six variants the original handler authors
/// know. The full [`crate::WidgetKind`] (25+ kinds: Date, Color,
/// Radio, Checkboxes, …) is reachable by returning [`crate::Widget`]
/// instances directly. `ExtensionFieldKind` converts cleanly via
/// `From` impls in both directions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionFieldKind {
    /// Single-line `<input type="text">`.
    Text,
    /// Multi-line `<textarea>` — plain content.
    Textarea,
    /// Multi-line `<textarea>` with a "preview as markdown" affordance.
    /// Rendered through [`crate::markdown::render`] on the public site.
    Markdown,
    /// Media picker — value is the `cms_media.id` as a stringified
    /// integer. Empty = no selection.
    MediaPicker,
    /// `<input type="number">` for integer-shaped fields.
    Number,
    /// `<input type="url">`.
    Url,
}

impl ExtensionFieldKind {
    /// Best-effort downgrade from the richer [`crate::WidgetKind`]
    /// back to the v0.x six-variant surface. Out-of-set widgets
    /// (Date, Color, Choice, …) downgrade to [`Self::Text`]; the
    /// legacy mapping is lossless.
    #[must_use]
    pub fn from_widget_kind(k: crate::WidgetKind) -> Self {
        use crate::WidgetKind as WK;
        match k {
            WK::Text => Self::Text,
            WK::Textarea => Self::Textarea,
            WK::Markdown => Self::Markdown,
            WK::MediaPicker => Self::MediaPicker,
            WK::Number | WK::Integer | WK::Float => Self::Number,
            WK::Url => Self::Url,
            _ => Self::Text,
        }
    }
}

impl From<ExtensionFieldKind> for crate::WidgetKind {
    fn from(k: ExtensionFieldKind) -> Self {
        match k {
            ExtensionFieldKind::Text => Self::Text,
            ExtensionFieldKind::Textarea => Self::Textarea,
            ExtensionFieldKind::Markdown => Self::Markdown,
            ExtensionFieldKind::MediaPicker => Self::MediaPicker,
            ExtensionFieldKind::Number => Self::Number,
            ExtensionFieldKind::Url => Self::Url,
        }
    }
}

/// The framework-default children listing, backing
/// [`PageTypeHandler::children_query`]'s default: the same live children
/// as [`published_children`].
///
/// It used to return every child, drafts included, into the public
/// template context — so a fresh site listed unpublished titles to
/// anonymous visitors (#688). A type that really wants drafts listed
/// overrides `children_query` with [`all_children`].
///
/// # Errors
/// Driver / query failures from the page lookup.
pub async fn default_children(
    pool: &Pool,
    page: &crate::page::Page,
) -> Result<Vec<crate::page::Page>, ExecError> {
    published_children(pool, page).await
}

/// Every immediate child, ordered `sort_order, id` — drafts, scheduled
/// and expired pages included. For admin-side listings; never the
/// public default.
///
/// # Errors
/// Driver / query failures from the page lookup.
pub async fn all_children(
    pool: &Pool,
    page: &crate::page::Page,
) -> Result<Vec<crate::page::Page>, ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let id = page.id.get().copied().unwrap_or_default();
    crate::page::Page::objects()
        .where_(crate::page::Page::parent_id.eq(id))
        .order_by(&[("sort_order", false), ("id", false)])
        .fetch(pool)
        .await
}

/// Immediate children a visitor can open right now, ordered
/// `sort_order, id`: published and not expired, or scheduled and past
/// go-live — the same rule the resolver serves by
/// ([`crate::resolver::visible_now`]). Error pages are left out: they
/// are served for a status code, not listed as content (as in
/// `auto_menu`). The building block for an index/listing page's
/// [`PageTypeHandler::children_query`] override.
///
/// # Errors
/// Driver / query failures from the page lookup.
pub async fn published_children(
    pool: &Pool,
    page: &crate::page::Page,
) -> Result<Vec<crate::page::Page>, ExecError> {
    use crate::page::PageStatus;
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let id = page.id.get().copied().unwrap_or_default();
    let now = chrono::Utc::now();
    let rows = crate::page::Page::objects()
        .where_(crate::page::Page::parent_id.eq(id))
        .where_(crate::page::Page::status.is_in([
            PageStatus::Published.as_str().to_owned(),
            PageStatus::Scheduled.as_str().to_owned(),
        ]))
        .order_by(&[("sort_order", false), ("id", false)])
        .fetch(pool)
        .await?;
    let error_tid = crate::error_pages::error_page_type_id(pool).await;
    Ok(rows
        .into_iter()
        .filter(|p| crate::resolver::visible_now(p, now))
        .filter(|p| error_tid.map_or(true, |tid| p.page_type_id != tid))
        .collect())
}

/// A page kind registered by user code (e.g. `BlogPostPage`, `LandingPage`).
///
/// Each impl owns the typed extension table that backs its kind, the
/// template used to render it, its admin form, and the parent/child
/// type whitelist, Wagtail-style.
///
/// **Registering.** Prefer `#[derive(PageType)]`, which emits this impl
/// plus the inventory registration and forwards the overridable hooks
/// to [`PageTypeOverrides`]. A hand-written impl registers with
/// [`register_page_type!`](crate::register_page_type) and needs only the
/// four required methods (`app_label`, `type_name`, `verbose_name`,
/// `default_template`); every other hook has a no-op default.
///
/// **When hooks run.** Render hooks (`load_extension`, `public_context`,
/// `children_query`, `routes` / `route_context`) run on every public
/// request for a page of this type. Editor hooks (`widgets`,
/// `extra_tabs`, `inline_panels`, `display_fields`) run on the admin
/// edit form; `save_extension` / `save_inline_panel` on its POST, after
/// the `cms_page` row is saved; `preview_extension` on live preview.
///
/// Handlers are process-global — instances are constructed via
/// [`PageTypeHandlerRegistration::factory`] and live for the
/// lifetime of the binary. The matching DB row in
/// `cms_page_type` is per-tenant and seeded by
/// [`crate::ensure_seeded`](crate::seed::ensure_seeded).
#[async_trait]
pub trait PageTypeHandler: Send + Sync + 'static {
    /// Logical app this handler belongs to (e.g. `"cms_articles"`).
    /// Lets multiple crates contribute page types without
    /// `type_name` collisions, and groups them in the admin.
    fn app_label(&self) -> &'static str;

    /// Stable identifier — stored in `cms_page_type.type_name`. Must
    /// be globally unique across the process.
    fn type_name(&self) -> &'static str;

    /// Human-readable label for the admin UI.
    fn verbose_name(&self) -> &'static str;

    /// Default Tera template path the renderer should use. A page can
    /// override it per row via [`Page::template_override`](crate::Page::template_override).
    fn default_template(&self) -> &'static str;

    /// Page types that are allowed as a parent for instances of this
    /// type. Empty slice = no restriction (the *child-side* guard).
    ///
    /// Wagtail equivalent: `Page.parent_page_types`.
    fn allowed_parent_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// Page types that are allowed as direct children of instances of
    /// this type. Empty slice = no restriction (the *parent-side*
    /// guard). Symmetric with [`Self::allowed_parent_types`]: both
    /// sides must agree at create / move time.
    ///
    /// Wagtail equivalent: `Page.subpage_types`.
    fn allowed_child_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether this type is a **leaf** — it accepts no child pages at
    /// all. Distinct from `allowed_child_types() == []`, which means "no
    /// restriction" (any type may be a child). Returning `true` is the
    /// first-class way to say "this page can't have children" (Wagtail's
    /// `subpage_types = []`) without the sentinel hack of whitelisting a
    /// non-existent child type. Enforced parent-side at create / move
    /// time. #448
    fn is_leaf(&self) -> bool {
        false
    }

    /// Whether this type may be created from the admin "Add page"
    /// menu. Defaults to `true`; abstract / programmatically-only
    /// types should override to `false`.
    fn is_creatable(&self) -> bool {
        true
    }

    /// Material-symbols icon name for the page-type picker tile.
    /// Defaults to `None` — picker renders a generic glyph. Authors
    /// set it via `#[page_type(icon = "article")]` on the
    /// `#[derive(PageType)]` struct, or by overriding this method on
    /// hand-rolled handlers.
    fn icon(&self) -> Option<&'static str> {
        None
    }

    /// One-line description shown under `verbose_name` in the
    /// page-type picker. Defaults to `None`. Authors set it via
    /// `#[page_type(description = "…")]`.
    fn description(&self) -> Option<&'static str> {
        None
    }

    /// Opt this page type in to a public RSS 2.0 / Atom 1.0 feed
    /// (powered by [`rustango::syndication`]). The returned slug is
    /// the URL path segment under `/feed/<slug>/rss.xml` +
    /// `/feed/<slug>/atom.xml`; pick something URL-friendly and
    /// distinct from other page types' slugs.
    ///
    /// Returning `None` (the default) means no feed is emitted for
    /// this type — its pages still render normally; they just
    /// don't appear in any aggregate feed.
    ///
    /// The feed lists every published, indexable [`crate::Page`]
    /// row of this type in `published_at desc` order. See
    /// [`crate::feed`] for the URL shape and content mapping.
    fn feed_kind(&self) -> Option<&'static str> {
        None
    }

    /// Opt this page type in to a multi-step approval workflow (#73).
    /// Returns the [`Workflow`](crate::workflow::Workflow) `name` of the workflow
    /// to apply to pages of this type, or `None` to leave them on the
    /// direct-publish path.
    ///
    /// When this returns `Some("Editorial review")`, the page editor
    /// for pages of this type renders a Submit-for-review action
    /// that creates a `cms_workflow_state` row and walks the page
    /// through the workflow's tasks. Pages can only finish a
    /// workflow once the final task is approved.
    ///
    /// The workflow with the returned name must exist + be active in
    /// the tenant's `cms_workflow` table — admins create + manage
    /// workflows at `/cms-admin/workflows`. If the named workflow
    /// doesn't exist or is inactive, pages fall back to the
    /// direct-publish path.
    fn workflow_slug(&self) -> Option<&'static str> {
        None
    }

    /// Gate every page of this type behind an access requirement — the
    /// per-**type** analogue of the per-page Privacy tab (#76). Return
    /// `None` (the default) to leave the type public; return
    /// [`crate::view_restriction::TypeViewRestriction::login`] to require
    /// any authenticated member, or
    /// [`crate::view_restriction::TypeViewRestriction::permission`] to
    /// require one of a set of permission codenames (checked through the
    /// framework permission engine).
    ///
    /// A page/subtree restriction set on an individual page (or an
    /// ancestor) is more specific and overrides this type-level default.
    /// The derive forwards to
    /// [`PageTypeOverrides::view_restriction`] automatically.
    fn view_restriction(&self) -> Option<crate::view_restriction::TypeViewRestriction> {
        None
    }

    /// Which representation this type serves on its public URL.
    ///
    /// Defaults to [`crate::page_view::PageViewMode::Auto`], which
    /// derives the answer from [`Self::default_template`]: a blank
    /// template means the type is JSON-only, anything else means HTML.
    /// Return [`crate::page_view::PageViewMode::Api`] to serve JSON at
    /// the page's own URL under `Accept: application/json` even when a
    /// template exists — the template still answers browsers.
    ///
    /// Set it declaratively with `#[page_type(view_mode = "api")]`,
    /// which also makes the otherwise-mandatory `template` attribute
    /// optional. Without the attribute the derive forwards to
    /// [`PageTypeOverrides::view_mode`].
    fn view_mode(&self) -> crate::page_view::PageViewMode {
        crate::page_view::PageViewMode::Auto
    }

    /// Load the typed extension row for a given page id and return
    /// it as JSON for the renderer's Tera context.
    ///
    /// The default implementation returns `Value::Null` — types
    /// with no extension table (e.g. a `HomePage` whose data lives
    /// entirely on the base Page row) get a null `extension` value
    /// in the template context.
    ///
    /// # Errors
    /// Driver / query failures from the extension lookup.
    async fn load_extension(&self, _pool: &Pool, _page_id: i64) -> Result<Value, ExecError> {
        Ok(Value::Null)
    }

    /// URL patterns this page type serves IN ADDITION to its
    /// canonical `url_path`. Wagtail's `routable_page` parity (#198).
    ///
    /// Each [`crate::routable::RouteSpec`] declares a regex pattern
    /// (matched against the suffix BELOW the page's `url_path`) and
    /// a stable name. When the public resolver can't find an exact
    /// `url_path` match it walks ancestor candidates and asks each
    /// candidate's handler whether any of its routes accept the
    /// remaining suffix. On match, the renderer dispatches to
    /// [`Self::route_context`] for handler-injected ctx, and the
    /// matched [`crate::routable::RouteMatch`] is exposed to the
    /// template as `route_name` + `route_captures`.
    ///
    /// Default: empty — page types without routable patterns pay
    /// nothing.
    fn routes(&self) -> Vec<crate::routable::RouteSpec> {
        Vec::new()
    }

    /// Handler-computed ctx injected when a routable pattern matched.
    /// Mirrors [`Self::public_context`] but receives the matched
    /// route's name + named capture groups. Override to load
    /// route-specific data — the year's posts when `archive_year`
    /// matched, the tagged posts when `tag` matched, etc.
    ///
    /// Default returns an empty map; the renderer still threads
    /// `route_name` and `route_captures` into the context so a
    /// handler that only needs the captures (no extra fetches) can
    /// stay on the default.
    ///
    /// # Errors
    /// Driver / query failures from the route-specific computation.
    async fn route_context(
        &self,
        _pool: &Pool,
        _page: &crate::page::Page,
        _matched: &crate::routable::RouteMatch,
    ) -> Result<serde_json::Map<String, Value>, ExecError> {
        Ok(serde_json::Map::new())
    }

    /// Inject handler-computed values into the public render's Tera
    /// context. Wagtail's `Page.get_context()` parity for hand-rolled
    /// data — e.g. a `HomePage` that exposes a `latest_posts` list
    /// to its template without baking the query into the template
    /// layer.
    ///
    /// The default returns an empty map; override in
    /// [`PageTypeOverrides`] (or hand-rolled handlers) to surface
    /// extras. Framework keys (`page`, `page_type`, `extension`,
    /// `children`, `ancestors`, `url_prefix`, `locale`,
    /// `translations`, `theme_*`, `_snippets_html`, `_stream_html`,
    /// `_pages_by_id`) are stamped AFTER the handler's keys, so
    /// `public_context` can't shadow them — pick distinct names
    /// for custom keys.
    ///
    /// # Errors
    /// Driver / query failures from the handler-side computation.
    async fn public_context(
        &self,
        _pool: &Pool,
        _page: &crate::page::Page,
    ) -> Result<serde_json::Map<String, Value>, ExecError> {
        Ok(serde_json::Map::new())
    }

    /// Build the `children` ctx var the public renderer hands to the
    /// page template. Wagtail parity for
    /// `Page.get_context()` overriding `context['posts'] =
    /// page.get_children().live().order_by('-first_published_at')`
    /// — index-style pages (BlogIndexPage, ArchivePage, …) lean on
    /// this to surface only published rows in a meaningful order.
    ///
    /// The default returns every immediate child sorted by
    /// `sort_order, id` (matches the pre-existing `children` shape
    /// before #249 — drafts INCLUDED, ordering NOT chronological).
    /// Overriding handlers typically:
    /// - filter by `status = published`
    /// - filter by `expire_at IS NULL OR expire_at > now`
    /// - order by `published_at DESC` (or `sort_order ASC` for a
    ///   curated tree)
    /// - apply a limit
    ///
    /// The result lands in two places in the template context:
    /// `children` (the typed list) AND a thread-local backing the
    /// `children_filtered(...)` Tera function (see [`crate::children_filtered`]).
    ///
    /// # Errors
    /// Driver / query failures from the page lookup.
    async fn children_query(
        &self,
        pool: &Pool,
        page: &crate::page::Page,
    ) -> Result<Vec<crate::page::Page>, ExecError> {
        default_children(pool, page).await
    }

    /// Describe the extension-row's fields so the admin page-editor
    /// can render a form section for them. The default returns an
    /// empty list — types without an extension table get no extra
    /// inputs in the editor.
    ///
    /// `page_id` is provided so handlers can pre-fill values from
    /// the existing row (the typical impl: load the row, then map
    /// each column into an `ExtensionField` with `value`).
    ///
    /// # Errors
    /// Driver / query failures from the extension lookup.
    async fn extension_fields(
        &self,
        _pool: &Pool,
        _page_id: i64,
    ) -> Result<Vec<ExtensionField>, ExecError> {
        Ok(Vec::new())
    }

    /// Rich-widget alternative to [`Self::extension_fields`].
    /// Returns full [`crate::Widget`] instances so handlers can
    /// expose the entire 26-kind widget catalog (Date, DatetimeTz,
    /// Color, Range, Radio, Select, Checkboxes, MultiSelect,
    /// Custom, …) instead of being limited to the legacy six
    /// [`ExtensionFieldKind`] variants.
    ///
    /// Default impl delegates to [`Self::extension_fields`] +
    /// [`ExtensionField::into_widget`], so handlers that haven't
    /// overridden this method keep working unchanged. Handlers
    /// that want the new kinds override `widgets` instead of
    /// `extension_fields`; the admin form-render path always calls
    /// `widgets`, so a single override surfaces every kind in the
    /// editor at once.
    ///
    /// # Errors
    /// Driver / query failures propagated from the handler-side
    /// computation.
    async fn widgets(&self, pool: &Pool, page_id: i64) -> Result<Vec<crate::Widget>, ExecError> {
        Ok(self
            .extension_fields(pool, page_id)
            .await?
            .into_iter()
            .map(ExtensionField::into_widget)
            .collect())
    }

    /// Declared Page↔Snippet many-to-many relations as
    /// `(relation_name, snippet_type)` pairs (#243). Macro-generated
    /// from the struct-level `#[page_type(snippet_m2m(name = "Type", …))]`
    /// attribute; the default is none. The public renderer resolves each
    /// relation's chosen rows into `snippet_relations.<name>` so templates
    /// can iterate the related snippets (Wagtail's `page.categories`).
    fn snippet_m2m_relations(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }

    /// Build a virtual extension `Value` from a posted form `HashMap`
    /// without touching the persisted row. Powers the "preview
    /// unsaved changes" iframe in the page editor — the renderer
    /// substitutes the result for [`Self::load_extension`] so the
    /// preview reflects the current form state.
    ///
    /// Default impl walks [`Self::widgets`] and maps each widget's
    /// `name` to the form value, with kind-aware coercion (booleans
    /// from `"on"`/`"true"`, multi-value kinds parsed as JSON arrays,
    /// numeric kinds parsed into `serde_json::Number`). Handlers
    /// whose persisted shape differs from the widget surface should
    /// override.
    ///
    /// # Errors
    /// Driver / query failures from the widgets lookup (the
    /// form-to-JSON mapping itself is infallible).
    async fn preview_extension(
        &self,
        pool: &Pool,
        page_id: i64,
        form: &std::collections::HashMap<String, String>,
    ) -> Result<Value, ExecError> {
        let widgets = self.widgets(pool, page_id).await.unwrap_or_default();
        let mut map = serde_json::Map::new();
        for w in widgets {
            let raw = form.get(&w.name).map(String::as_str).unwrap_or_default();
            // One coercion table for page fields and builder fields (#660).
            let val = crate::page_builder::values::coerce(w.kind, raw);
            map.insert(w.name, val);
        }
        Ok(Value::Object(map))
    }

    /// Persist the posted form values into this type's extension
    /// table. Called by the admin's page-edit POST handler after the
    /// canonical `cms_page` row has been saved. Upsert keyed on
    /// `page_id` is the standard pattern.
    ///
    /// `form` is the full form HashMap; handlers should pull out
    /// only the keys whose `name` they returned from
    /// [`Self::extension_fields`].
    ///
    /// # Errors
    /// Driver / query failures from the upsert.
    async fn save_extension(
        &self,
        _pool: &Pool,
        _page_id: i64,
        _form: &std::collections::HashMap<String, String>,
    ) -> Result<(), ExecError> {
        Ok(())
    }

    /// Handler-contributed tabs appended to the page-editor's tab
    /// strip after the built-in `Content` / `Promote` / `Theme` /
    /// `Revisions` ones. Each [`TabSpec`] carries a stable `name`
    /// (used in the URL hash + localStorage for "remember last
    /// active tab"), a `label`, an optional Material-symbols `icon`
    /// + small `badge` chip, and a pre-rendered HTML body the
    /// admin template emits inside the main `<form>`.
    ///
    /// Because custom tab bodies sit inside the canonical form, any
    /// `<input name="…">` inside one shows up in the
    /// `HashMap<String, String>` handed to [`Self::save_extension`]
    /// — handlers can persist tab-scoped fields the same way they
    /// persist [`Self::widgets`] entries.
    ///
    /// Default impl returns an empty list — the built-in tabs are
    /// always present and self-contained, so handlers that don't
    /// need a custom tab pay nothing.
    ///
    /// # Errors
    /// Driver / query failures from the handler-side computation.
    async fn extra_tabs(&self, _pool: &Pool, _page_id: i64) -> Result<Vec<TabSpec>, ExecError> {
        Ok(Vec::new())
    }

    /// Inline panels — 1-N collections of sub-rows that the page
    /// editor renders as a sortable list of cards (#117, Wagtail
    /// parity B14). Each [`InlinePanelSpec`] declares the panel's
    /// stable `name`, the human `label`, the per-row `fields`, and
    /// optional cardinality bounds.
    ///
    /// Default impl returns an empty list — handlers that don't use
    /// inline panels pay nothing. Handlers that do override this
    /// method must also implement [`Self::load_inline_panel`] and
    /// [`Self::save_inline_panel`] for the rows to round-trip.
    fn inline_panels(&self) -> Vec<InlinePanelSpec> {
        Vec::new()
    }

    /// Fetch existing rows for `panel_name` keyed under `page_id`.
    /// Each returned `serde_json::Map` is one row; keys match the
    /// `name` of the [`InlinePanelSpec::fields`] entries.
    ///
    /// Default impl returns an empty list.
    ///
    /// # Errors
    /// Driver / query failures from the row lookup.
    async fn load_inline_panel(
        &self,
        _pool: &Pool,
        _page_id: i64,
        _panel_name: &str,
    ) -> Result<Vec<serde_json::Map<String, Value>>, ExecError> {
        Ok(Vec::new())
    }

    /// Persist the posted rows for `panel_name` under `page_id`. The
    /// admin POST handler parses inline-panel form keys
    /// (`inline__<panel_name>__<row_idx>__<field>`) into row dicts
    /// and calls this with the full set; the handler is responsible
    /// for the upsert / delete-removed semantics. Sort order is the
    /// position in the `rows` slice.
    ///
    /// Default impl is a no-op.
    ///
    /// # Errors
    /// Driver / query failures from the upsert.
    async fn save_inline_panel(
        &self,
        _pool: &Pool,
        _page_id: i64,
        _panel_name: &str,
        _rows: &[serde_json::Map<String, Value>],
    ) -> Result<(), ExecError> {
        Ok(())
    }

    /// Computed / display-only fields shown in an "At a glance"
    /// panel at the top of the page-edit form. Each entry is a
    /// pre-rendered HTML chip (the value is `| safe`d into the
    /// template) authored by the handler from arbitrary data:
    /// row counts, word counts, derived flags, audit timestamps,
    /// linked-snippet previews, etc.
    ///
    /// This is the first-class hook for the Wagtail pain point #2
    /// (`FieldPanel(read_only=True)` reads only model fields, no
    /// computed properties; `HelpPanel` is static-text only).
    /// `display_fields` runs at edit-form GET, so each value is
    /// recomputed every load and reflects the latest DB state.
    ///
    /// Default impl returns an empty list — handlers that don't
    /// need it pay nothing.
    ///
    /// # Errors
    /// Driver / query failures from any computation the handler
    /// runs against the pool.
    async fn display_fields(
        &self,
        _pool: &Pool,
        _page_id: i64,
    ) -> Result<Vec<DisplayField>, ExecError> {
        Ok(Vec::new())
    }
}

/// One computed display chip rendered in the page-edit form's "At
/// a glance" panel. See [`PageTypeHandler::display_fields`].
///
/// `html` is trusted — emitted through `| safe` in the bundled
/// template. Handlers escape user-controlled content themselves
/// (e.g. via `tera::escape_html` or by passing through
/// [`crate::markdown::render`] which sanitizes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayField {
    /// Short label, e.g. "Word count" or "Last edited".
    pub label: String,
    /// Pre-rendered HTML fragment. Emitted with `| safe`.
    pub html: String,
    /// Optional helptext shown beneath the value.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub help: String,
    /// Optional material-symbols icon name shown next to the label
    /// (e.g. `"format_list_numbered"`, `"schedule"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

impl DisplayField {
    /// Smallest possible chip — just `(label, html)`. Caller adds
    /// `help` / `icon` fluently if needed.
    #[must_use]
    pub fn new(label: impl Into<String>, html: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            html: html.into(),
            help: String::new(),
            icon: None,
        }
    }

    #[must_use]
    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = help.into();
        self
    }
}

/// How a custom tab's body is supplied (#20). Three shapes:
///
/// - `Html(s)` — pre-rendered HTML (backwards-compatible escape
///   hatch; what `TabSpec::new(..., html)` produces).
/// - `Template { path, context }` — a Tera template path + a
///   per-tab context map. The page editor renders the template
///   server-side with the standard admin chrome context PLUS the
///   spec's own `context` map merged in, and drops the resulting
///   HTML into the tab panel.
/// - `Widgets(widgets)` — declarative form fields. The editor
///   renders each [`crate::Widget`] via the same `_widget.html`
///   macro the Content tab's main stack uses, so widget visuals +
///   behavior stay identical wherever they appear.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabBody {
    Html(String),
    Template {
        path: String,
        #[serde(default)]
        context: serde_json::Map<String, serde_json::Value>,
    },
    Widgets(Vec<crate::widget::Widget>),
    /// Wagtail's `MultiFieldPanel` / `FieldRowPanel`. Groups widgets
    /// under a heading; `layout` picks vertical stack vs side-by-side.
    /// Optional `help` renders as a hint paragraph between the
    /// heading and the field grid.
    Group {
        heading: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        help: String,
        #[serde(default)]
        layout: PanelLayout,
        widgets: Vec<crate::widget::Widget>,
    },
    /// Wagtail's `HelpPanel`. Static informational block — no form
    /// fields. The body is HTML emitted via `| safe` after CMS-side
    /// authoring; trust source is the handler author.
    Help {
        content: String,
    },
}

/// Layout direction for [`TabBody::Group`].
///
/// Wagtail parity: `MultiFieldPanel` ≈ Column; `FieldRowPanel` ≈ Row.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelLayout {
    /// Stack widgets vertically (default).
    #[default]
    Column,
    /// Lay widgets out side-by-side (responsive flex; collapses to
    /// column on narrow viewports).
    Row,
}

impl PanelLayout {
    /// CSS class suffix — used by `_panel_group.html` to switch the
    /// flex direction.
    #[must_use]
    pub fn class_suffix(self) -> &'static str {
        match self {
            Self::Column => "column",
            Self::Row => "row",
        }
    }
}

/// One inline panel declaration — a 1-N relation between the page
/// and a child table the handler maintains. See
/// [`PageTypeHandler::inline_panels`]. The admin editor renders one
/// section per spec with the fields stacked vertically; add and remove
/// are plain HTML buttons, with no drag-to-reorder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlinePanelSpec {
    /// Stable identifier — used in form field names
    /// (`inline__<name>__<row>__<field>`). Snake_case conventional.
    pub name: String,
    /// Visible label shown above the panel.
    pub label: String,
    /// Per-row field surface. Each entry renders one input per row.
    pub fields: Vec<ExtensionField>,
    /// Optional minimum row count (inclusive). When set + the editor
    /// submits fewer rows, the save handler logs a warning but
    /// doesn't reject — handlers enforce hard rules themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_num: Option<usize>,
    /// Optional maximum row count (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_num: Option<usize>,
    /// Optional helptext shown beneath the panel header.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub help: String,
    /// Optional material-symbols icon name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

impl InlinePanelSpec {
    /// Smallest valid spec — `(name, label, fields)`. Helpers below
    /// add `min` / `max` / `help` / `icon` fluently.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        label: impl Into<String>,
        fields: Vec<ExtensionField>,
    ) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            fields,
            min_num: None,
            max_num: None,
            help: String::new(),
            icon: None,
        }
    }

    #[must_use]
    pub fn with_min(mut self, min: usize) -> Self {
        self.min_num = Some(min);
        self
    }

    #[must_use]
    pub fn with_max(mut self, max: usize) -> Self {
        self.max_num = Some(max);
        self
    }

    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = help.into();
        self
    }

    #[must_use]
    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }
}

/// One handler-contributed tab appended to the page-editor's
/// built-in tab strip (Content / Promote / Theme / Revisions). See
/// [`PageTypeHandler::extra_tabs`].
///
/// The body is one of [`TabBody::Html`] (pre-rendered HTML — the
/// escape hatch), [`TabBody::Template`] (Tera template path + extra
/// context — render with the standard admin context vars in scope),
/// or [`TabBody::Widgets`] (declarative widget list — rendered via
/// `_widget.html` so the form fields look identical to the Content
/// tab's stack).
///
/// Inputs declared inside the rendered HTML land in
/// [`PageTypeHandler::save_extension`]'s form HashMap automatically;
/// every variant lives inside the page-edit `<form>`.
///
/// `name` must be unique within a page's tab set. Mixing with the
/// built-in names (`content`, `promote`, `theme`, `revisions`) is
/// allowed and lets a handler override the BUILT-IN tab body without
/// forking the template; the tab JS keys off `data-tab-trigger="<name>"`
/// so identical names render the same panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabSpec {
    /// Stable identifier — kebab-case is conventional. Used for
    /// `aria-controls`, URL hash links, and the per-page localStorage
    /// "remember last active tab" key.
    pub name: String,
    /// Visible label on the tab button.
    pub label: String,
    /// Optional Material-symbols icon name (e.g. `"workflow"`,
    /// `"insights"`). Rendered to the left of `label`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Optional small chip rendered after `label` (e.g. an unread
    /// count, "new", or a small status indicator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<String>,
    /// What goes inside the tab panel — see [`TabBody`].
    pub body: TabBody,
}

impl TabSpec {
    /// Smallest possible spec — just `(name, label, html)`. The
    /// `html` is treated as pre-rendered + emitted via Tera's
    /// `| safe`. Backwards-compatible with the v0.2 surface.
    #[must_use]
    pub fn new(name: impl Into<String>, label: impl Into<String>, html: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            icon: None,
            badge: None,
            body: TabBody::Html(html.into()),
        }
    }

    /// Tera-template-backed tab. The renderer mounts `path` with the
    /// standard admin context plus the per-tab context map (empty
    /// until filled with [`Self::with_context_value`]). Example:
    /// `TabSpec::template("seo", "SEO", "blog/seo_tab.html")`.
    #[must_use]
    pub fn template(
        name: impl Into<String>,
        label: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            icon: None,
            badge: None,
            body: TabBody::Template {
                path: path.into(),
                context: serde_json::Map::new(),
            },
        }
    }

    /// Widget-group-backed tab. Same rendering pipeline the Content
    /// tab uses — each widget is emitted via `_widget.html`'s macro.
    /// Inputs submit alongside the page-edit form so the handler's
    /// [`PageTypeHandler::save_extension`] picks them up by name.
    #[must_use]
    pub fn widgets(
        name: impl Into<String>,
        label: impl Into<String>,
        widgets: Vec<crate::widget::Widget>,
    ) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            icon: None,
            badge: None,
            body: TabBody::Widgets(widgets),
        }
    }

    /// MultiFieldPanel-shaped tab — heading + (optional help) +
    /// widgets stacked under one section. Use [`Self::with_layout`]
    /// to switch to row layout (FieldRowPanel).
    ///
    /// Wagtail parity: `MultiFieldPanel(heading=..., children=[...])`.
    #[must_use]
    pub fn group(
        name: impl Into<String>,
        label: impl Into<String>,
        heading: impl Into<String>,
        widgets: Vec<crate::widget::Widget>,
    ) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            icon: None,
            badge: None,
            body: TabBody::Group {
                heading: heading.into(),
                help: String::new(),
                layout: PanelLayout::Column,
                widgets,
            },
        }
    }

    /// HelpPanel-shaped tab — static HTML content, no form fields.
    /// The body is emitted via `| safe`, so authors control the markup.
    ///
    /// Wagtail parity: `HelpPanel(content=mark_safe('<p>...</p>'))`.
    #[must_use]
    pub fn help(
        name: impl Into<String>,
        label: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            icon: None,
            badge: None,
            body: TabBody::Help {
                content: content.into(),
            },
        }
    }

    /// Switch a Group-body tab to a different layout. No-op on other
    /// body variants.
    #[must_use]
    pub fn with_layout(mut self, layout: PanelLayout) -> Self {
        if let TabBody::Group {
            layout: ref mut existing,
            ..
        } = self.body
        {
            *existing = layout;
        }
        self
    }

    /// Attach help text to a Group-body tab. No-op on other body
    /// variants.
    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        if let TabBody::Group {
            help: ref mut existing,
            ..
        } = self.body
        {
            *existing = help.into();
        }
        self
    }

    #[must_use]
    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    #[must_use]
    pub fn with_badge(mut self, badge: impl Into<String>) -> Self {
        self.badge = Some(badge.into());
        self
    }

    /// Insert a context entry — only meaningful for the
    /// [`TabBody::Template`] variant; a no-op on `Html` / `Widgets`.
    /// Chain multiple times for multiple values.
    #[must_use]
    pub fn with_context_value(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        if let TabBody::Template { context, .. } = &mut self.body {
            context.insert(key.into(), value);
        }
        self
    }
}

/// Inventory entry — drop one of these next to each
/// `PageTypeHandler` impl (or use [`crate::register_page_type!`])
/// and it becomes discoverable at runtime via [`registered_handlers`].
pub struct PageTypeHandlerRegistration {
    pub factory: fn() -> Box<dyn PageTypeHandler>,
}

inventory::collect!(PageTypeHandlerRegistration);

/// All `PageTypeHandler`s registered in the current binary.
pub fn registered_handlers() -> impl Iterator<Item = Box<dyn PageTypeHandler>> {
    inventory::iter::<PageTypeHandlerRegistration>
        .into_iter()
        .map(|r| (r.factory)())
}

/// Look up a registered handler by `type_name`.
pub fn find_handler(type_name: &str) -> Option<Box<dyn PageTypeHandler>> {
    registered_handlers().find(|p| p.type_name() == type_name)
}

/// `true` when **any** registered handler declares a per-type
/// [`PageTypeHandler::view_restriction`] (#members). Cached — the
/// handler registry is static for the life of the process — so the
/// hot menu-filtering path can skip the per-type restriction lookup
/// entirely on binaries that gate nothing by type.
#[must_use]
pub fn any_type_view_restriction() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| registered_handlers().any(|h| h.view_restriction().is_some()))
}

/// Optional-override trait for declarative page types defined via
/// `#[derive(PageType)]`. The derive emits the full
/// [`PageTypeHandler`] impl and delegates every author-overridable
/// method (`allowed_parent_types`, `allowed_child_types`,
/// `is_creatable`, `feed_kind`, `display_fields`, `extra_tabs`) to
/// `<Self as PageTypeOverrides>::*`. Every method has a sensible
/// default; an author that doesn't need overrides writes a one-line
/// `impl PageTypeOverrides for MyPage {}`.
///
/// ## Why a separate trait
///
/// The derive can't conditionally emit a default impl (Rust forbids
/// two impls of the same trait for the same type). Splitting the
/// overridable methods into a sibling trait gives authors a clean
/// opt-in: write the impl, override what you need, accept defaults
/// for the rest. The `PageTypeHandler` impl the derive emits handles
/// the wire-up.
#[async_trait]
pub trait PageTypeOverrides: Send + Sync + 'static {
    /// Allowed parent types — see [`PageTypeHandler::allowed_parent_types`].
    fn allowed_parent_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// Allowed child types — see [`PageTypeHandler::allowed_child_types`].
    fn allowed_child_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether this type may be created from the admin — see
    /// [`PageTypeHandler::is_creatable`].
    fn is_creatable(&self) -> bool {
        true
    }

    /// Public feed slug — see [`PageTypeHandler::feed_kind`].
    /// The derive prefers a struct-level
    /// `#[page_type(feed_kind = "…")]` attr over this default, so
    /// most authors set it via the attribute, not by overriding here.
    fn feed_kind(&self) -> Option<&'static str> {
        None
    }

    /// Workflow opt-in slug — see [`PageTypeHandler::workflow_slug`].
    /// The derive prefers a struct-level
    /// `#[page_type(workflow = "Editorial review")]` attr over this
    /// default. Override here only when the workflow needs to be
    /// computed (e.g. choose between workflows based on tenant
    /// configuration).
    fn workflow_slug(&self) -> Option<&'static str> {
        None
    }

    /// Per-type access gate — see [`PageTypeHandler::view_restriction`].
    /// Override to require a login or a permission codename for every
    /// page of this type. The derive forwards to this automatically.
    fn view_restriction(&self) -> Option<crate::view_restriction::TypeViewRestriction> {
        None
    }

    /// Which representation this type serves — see
    /// [`PageTypeHandler::view_mode`]. Override to return
    /// [`crate::page_view::PageViewMode::Api`] when the type should
    /// answer JSON on its own URL. The derive forwards to this unless
    /// `#[page_type(view_mode = "…")]` is set, which wins.
    fn view_mode(&self) -> crate::page_view::PageViewMode {
        crate::page_view::PageViewMode::Auto
    }

    /// Computed display chips — see [`PageTypeHandler::display_fields`].
    async fn display_fields(
        &self,
        _pool: &Pool,
        _page_id: i64,
    ) -> Result<Vec<DisplayField>, ExecError> {
        Ok(Vec::new())
    }

    /// Handler-injected template ctx — see
    /// [`PageTypeHandler::public_context`]. The derive forwards to
    /// this override automatically; hand-rolled handlers can opt in
    /// via `impl PageTypeOverrides for MyHandler { … }`.
    async fn public_context(
        &self,
        _pool: &Pool,
        _page: &crate::page::Page,
    ) -> Result<serde_json::Map<String, Value>, ExecError> {
        Ok(serde_json::Map::new())
    }

    /// Override the children listing this page exposes to its template —
    /// see [`PageTypeHandler::children_query`]. Return `Ok(None)` (the
    /// default) to keep the framework behavior (all immediate children,
    /// `sort_order, id`, drafts included). Return `Ok(Some(pages))` to
    /// make this type an index that lists e.g. only published children in
    /// a custom order — [`published_children`] is the common building
    /// block:
    ///
    /// ```ignore
    /// async fn children_query(&self, pool, page)
    ///     -> Result<Option<Vec<Page>>, ExecError>
    /// {
    ///     Ok(Some(rustango_cms::published_children(pool, page).await?))
    /// }
    /// ```
    ///
    /// The derive forwards to this override automatically.
    async fn children_query(
        &self,
        _pool: &Pool,
        _page: &crate::page::Page,
    ) -> Result<Option<Vec<crate::page::Page>>, ExecError> {
        Ok(None)
    }

    /// Routable URL patterns — see [`PageTypeHandler::routes`].
    fn routes(&self) -> Vec<crate::routable::RouteSpec> {
        Vec::new()
    }

    /// Route-specific ctx — see [`PageTypeHandler::route_context`].
    async fn route_context(
        &self,
        _pool: &Pool,
        _page: &crate::page::Page,
        _matched: &crate::routable::RouteMatch,
    ) -> Result<serde_json::Map<String, Value>, ExecError> {
        Ok(serde_json::Map::new())
    }

    /// Handler-contributed tabs — see [`PageTypeHandler::extra_tabs`].
    async fn extra_tabs(&self, _pool: &Pool, _page_id: i64) -> Result<Vec<TabSpec>, ExecError> {
        Ok(Vec::new())
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod preview_tests {
    use super::*;
    use crate::widget::{Widget, WidgetKind};

    struct Choosers;

    #[async_trait]
    impl PageTypeHandler for Choosers {
        fn app_label(&self) -> &'static str {
            "cms"
        }
        fn type_name(&self) -> &'static str {
            "Choosers"
        }
        fn verbose_name(&self) -> &'static str {
            "Choosers"
        }
        fn default_template(&self) -> &'static str {
            "c.html"
        }
        async fn widgets(&self, _pool: &Pool, _page_id: i64) -> Result<Vec<Widget>, ExecError> {
            Ok(vec![
                Widget::new(WidgetKind::PageChooser, "related", "Related"),
                Widget::new(WidgetKind::SnippetChooser, "cta", "CTA"),
                Widget::new(WidgetKind::DocumentChooser, "doc", "Doc"),
            ])
        }
    }

    /// #660 — live preview types every chooser id as the saved page does.
    #[tokio::test]
    async fn preview_types_chooser_ids_like_the_saved_value() {
        let pool = Pool::connect("sqlite::memory:").await.expect("pool");
        let form = std::collections::HashMap::from([
            ("related".to_owned(), "7".to_owned()),
            ("cta".to_owned(), "3".to_owned()),
            ("doc".to_owned(), String::new()),
        ]);
        let v = Choosers.preview_extension(&pool, 1, &form).await.expect("preview");
        assert_eq!(v, serde_json::json!({ "related": 7, "cta": 3, "doc": null }));
    }
}
