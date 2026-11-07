//! A generic admin editor for a host app's own models.
//!
//! `#[derive(CmsAdmin)]` on a `#[derive(Model)]` type implements
//! [`CmsAdmin`] from the struct's `#[cms_admin(...)]` and `#[field(...)]`
//! attributes and registers a [`CmsAdminPage`] for it. The page lives
//! at `/cms-admin/x/<slug>` inside the admin chrome and gives editors:
//!
//! - a list with search, the chosen columns and pagination;
//! - add and edit forms built from the `#[field]` widgets (every widget
//!   the page editor has, media and page choosers included);
//! - delete, with a confirmation.
//!
//! Access runs through the role matrix: the model gets a row with view,
//! add, edit and delete columns (`<resource>.view` … `.delete`), and the
//! dispatcher checks the matching codename on every request.
//!
//! [`CmsAdminOverrides`] holds the hooks a real app needs: fields that
//! cannot be edited, records that cannot be created or deleted,
//! validation, side effects after a save or before a delete, options for
//! a select, and extra widgets for relations the struct has no column
//! for (a many-to-many list).

use std::collections::HashMap;
use std::marker::PhantomData;

use async_trait::async_trait;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rustango::sql::{ExecError, Pool};
use serde_json::{json, Value};

use super::admin_page::{AdminForm, AdminPageCtx, AdminPageHandler, MessageLevel, SidebarSection};
use crate::permissions::Action;
use crate::widget::{Widget, WidgetKind};

/// Static description of one model's editor, built by the derive.
#[derive(Debug, Clone, Copy)]
pub struct CmsAdminMeta {
    /// URL slug: the page is `/cms-admin/x/<slug>`.
    pub slug: &'static str,
    /// Plural label, for the sidebar and the list heading.
    pub label: &'static str,
    /// Singular label, for "Add …" and "Edit …".
    pub singular: &'static str,
    /// Material Symbols icon name for the sidebar.
    pub icon: Option<&'static str>,
    /// Sidebar section.
    pub section: SidebarSection,
    /// Order within the section.
    pub sort_order: i32,
    /// List columns as `(field, header)`. The first links to the form.
    pub list: &'static [(&'static str, &'static str)],
    /// Whether the list shows a search box.
    pub searchable: bool,
    /// Rows per list page.
    pub per_page: i64,
    /// Codename prefix in the role matrix.
    pub permission_resource: &'static str,
}

/// A validation failure for one field, or for the whole form when
/// `field` is `"_form"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

impl FieldError {
    /// An error attached to `field`.
    #[must_use]
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self { field: field.into(), message: message.into() }
    }
}

/// What `#[derive(CmsAdmin)]` implements. Host code rarely calls these
/// directly; [`CmsAdminPage`] does.
#[async_trait]
pub trait CmsAdmin: CmsAdminOverrides + Sized + Send + Sync + 'static {
    /// The editor's static description.
    fn admin_meta() -> &'static CmsAdminMeta;
    /// A blank record for the add form (`Default::default()`).
    fn admin_new() -> Self;
    /// The primary key, once saved.
    fn admin_id(&self) -> Option<i64>;
    /// How a record names itself in headings and messages.
    fn admin_title(&self) -> String;
    /// The text for list column `field`.
    fn admin_cell(&self, field: &str) -> String;
    /// One widget per `#[field]`, filled from `self`.
    fn admin_widgets(&self) -> Vec<Widget>;
    /// Copy the posted values into `self`, leaving the fields in `skip`
    /// alone. Returns a message for each value that does not parse and
    /// each required field left empty.
    fn admin_apply(&mut self, form: &AdminForm, skip: &[&str]) -> Vec<FieldError>;
    /// One page of records matching `search`, and the total match count.
    async fn admin_list(pool: &Pool, search: &str, limit: i64, offset: i64) -> Result<(Vec<Self>, i64), ExecError>;
    /// The record with primary key `id`.
    async fn admin_get(pool: &Pool, id: i64) -> Result<Option<Self>, ExecError>;
    /// Insert or update.
    async fn admin_save(&mut self, pool: &Pool) -> Result<(), ExecError>;
    /// Delete.
    async fn admin_delete(self, pool: &Pool) -> Result<(), ExecError>;
}

/// Hooks for a model's editor. Every method has a default, so
/// `impl CmsAdminOverrides for MyModel {}` is enough to start.
#[async_trait]
pub trait CmsAdminOverrides: Send + Sync {
    /// Fields shown but not editable on `self`. For a new record `self`
    /// is the blank row.
    async fn read_only_fields(&self, _pool: &Pool) -> Vec<&'static str> {
        Vec::new()
    }

    /// Whether editors may add records. `false` hides "Add" and refuses
    /// the form.
    async fn can_create(_pool: &Pool) -> bool
    where
        Self: Sized,
    {
        true
    }

    /// Whether editors may delete `self`.
    async fn can_delete(&self, _pool: &Pool) -> bool {
        true
    }

    /// Checks beyond "does each value parse", run before saving.
    /// Return one [`FieldError`] per problem; any error stops the save
    /// and re-shows the form.
    async fn validate(&self, _pool: &Pool) -> Vec<FieldError> {
        Vec::new()
    }

    /// Options for a `Select`, `Radio`, `Checkboxes` or `MultiSelect`
    /// field, as `(value, label)`, replacing any declared in
    /// `#[field(options(...))]`. Use it for a foreign key:
    /// list the target table's rows. `None` keeps the declared options.
    async fn field_options(_pool: &Pool, _field: &str) -> Option<Vec<(String, String)>>
    where
        Self: Sized,
    {
        None
    }

    /// Widgets for data that is not a column of this struct, such as a
    /// many-to-many relation. They render after the `#[field]` widgets;
    /// [`Self::save_extra`] stores what they post.
    async fn extra_widgets(&self, _pool: &Pool) -> Vec<Widget> {
        Vec::new()
    }

    /// Store the values [`Self::extra_widgets`] posted. Runs after the
    /// record is saved, so `self` has its primary key.
    async fn save_extra(&self, _pool: &Pool, _form: &AdminForm) -> Result<(), ExecError> {
        Ok(())
    }

    /// Runs after every successful save; `created` is true for an add.
    async fn after_save(&self, _pool: &Pool, _created: bool) -> Result<(), ExecError> {
        Ok(())
    }

    /// Runs before the delete; an error cancels it. Remove dependent
    /// rows here.
    async fn before_delete(&self, _pool: &Pool) -> Result<(), ExecError> {
        Ok(())
    }

    /// The cells of list column `column` for `rows`, one per row in
    /// order, overriding the field's own value. Lets a column show
    /// something computed, such as a count from another table (one query
    /// for the page, not one per row), or name a column that is not a
    /// field at all. `None` keeps the default.
    async fn list_column(_pool: &Pool, _rows: &[Self], _column: &str) -> Option<Vec<String>>
    where
        Self: Sized,
    {
        None
    }
}

/// Parse a `section` attribute value: `content`, `site` and `settings`
/// are the built-in sections; anything else names a new one.
#[must_use]
pub const fn section_from(name: &'static str) -> SidebarSection {
    match name.as_bytes() {
        b"content" | b"Content" => SidebarSection::Content,
        b"site" | b"Site" => SidebarSection::Site,
        b"settings" | b"Settings" => SidebarSection::Settings,
        _ => SidebarSection::Custom(name),
    }
}

/// The admin page serving model `T`. `#[derive(CmsAdmin)]` registers it;
/// register it by hand with `register_admin_page!(CmsAdminPage<T>)` only
/// when implementing [`CmsAdmin`] yourself.
pub struct CmsAdminPage<T>(PhantomData<fn() -> T>);

impl<T> Default for CmsAdminPage<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

const CRUD: &[Action] = &[Action::View, Action::Add, Action::Edit, Action::Delete];

fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

fn forbidden() -> Response {
    StatusCode::FORBIDDEN.into_response()
}

/// The form's widgets. `posted` is the rejected submission being shown
/// again: extra widgets read their values from the database, so they
/// take the posted ones instead, or the editor's changes would be lost.
async fn widgets_for<T: CmsAdmin>(
    row: &T,
    pool: &Pool,
    read_only: &[&str],
    all_read_only: bool,
    posted: Option<&AdminForm>,
) -> Vec<Widget> {
    let mut out = Vec::new();
    for mut w in row.admin_widgets() {
        if matches!(w.kind, WidgetKind::Select | WidgetKind::Radio | WidgetKind::Checkboxes | WidgetKind::MultiSelect) {
            if let Some(options) = T::field_options(pool, &w.name).await {
                w.options = options;
            }
        }
        if all_read_only || read_only.contains(&w.name.as_str()) {
            w.read_only = true;
        }
        out.push(w);
    }
    for mut w in row.extra_widgets(pool).await {
        if let Some(v) = posted.and_then(|f| f.get(&w.name)) {
            w.value = v.to_owned();
        }
        if all_read_only {
            w.read_only = true;
        }
        out.push(w);
    }
    out
}

impl<T: CmsAdmin> CmsAdminPage<T> {
    fn code(action: Action) -> String {
        format!("{}.{}", T::admin_meta().permission_resource, action.as_str())
    }

    async fn list(ctx: &AdminPageCtx<'_>) -> Response {
        let meta = T::admin_meta();
        let pool = ctx.tenant.pool();
        let q = ctx.query_param("q").unwrap_or_default();
        let query = ctx.query();
        let per_page = super::pagination::clamp_per_page(
            query.get("per_page").and_then(|v| v.parse().ok()).or(Some(usize::try_from(meta.per_page).unwrap_or(25))),
        );
        let page = query.get("page").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1).max(1);
        let offset = super::pagination::offset(page, per_page);
        let (rows, total) = match T::admin_list(pool, &q, per_page as i64, offset).await {
            Ok(found) => found,
            Err(e) => return super::AdminError::from(e).into_response(),
        };
        let mut columns: Vec<Vec<String>> = Vec::with_capacity(meta.list.len());
        for (field, _) in meta.list {
            let cells = match T::list_column(pool, &rows, field).await {
                Some(cells) if cells.len() == rows.len() => cells,
                _ => rows.iter().map(|r| r.admin_cell(field)).collect(),
            };
            columns.push(cells);
        }
        let items: Vec<Value> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let cells: Vec<&str> = columns.iter().map(|c| c[i].as_str()).collect();
                json!({ "id": r.admin_id(), "title": r.admin_title(), "cells": cells })
            })
            .collect();
        let mut tctx = tera::Context::new();
        tctx.insert("meta", &meta_json(meta));
        tctx.insert("columns", &meta.list.iter().map(|(_, h)| *h).collect::<Vec<_>>());
        tctx.insert("items", &items);
        tctx.insert("q", &q);
        tctx.insert("total", &total);
        tctx.insert("query_string", ctx.uri.query().unwrap_or_default());
        tctx.insert(
            "pagination",
            &super::pagination::context(page, per_page, usize::try_from(total).unwrap_or_default()),
        );
        tctx.insert("can_add", &(ctx.has_perm(&Self::code(Action::Add)).await && T::can_create(pool).await));
        ctx.render("rcms_admin/cms_admin_list.html", &mut tctx).await
    }

    async fn form(ctx: &AdminPageCtx<'_>, row: &T, errors: &[FieldError], posted: Option<&AdminForm>) -> Response {
        let meta = T::admin_meta();
        let pool = ctx.tenant.pool();
        let id = row.admin_id();
        let may_write = ctx.has_perm(&Self::code(if id.is_some() { Action::Edit } else { Action::Add })).await;
        let read_only = row.read_only_fields(pool).await;
        let widgets = widgets_for(row, pool, &read_only, !may_write, posted).await;
        let can_delete = id.is_some() && ctx.has_perm(&Self::code(Action::Delete)).await && row.can_delete(pool).await;
        let mut by_field: HashMap<&str, Vec<&str>> = HashMap::new();
        for e in errors {
            by_field.entry(e.field.as_str()).or_default().push(e.message.as_str());
        }
        let mut tctx = tera::Context::new();
        tctx.insert("meta", &meta_json(meta));
        tctx.insert("record_id", &id);
        tctx.insert("record_title", &row.admin_title());
        tctx.insert("fields", &widgets);
        tctx.insert("errors", &by_field);
        tctx.insert("can_save", &may_write);
        tctx.insert("can_delete", &can_delete);
        let mut resp = ctx.render("rcms_admin/cms_admin_form.html", &mut tctx).await;
        if !errors.is_empty() {
            *resp.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
        }
        resp
    }

    async fn load(ctx: &AdminPageCtx<'_>, id: Option<i64>) -> Result<Option<T>, Response> {
        let Some(id) = id else { return Ok(None) };
        T::admin_get(ctx.tenant.pool(), id).await.map_err(|e| super::AdminError::from(e).into_response())
    }

    async fn save(ctx: &AdminPageCtx<'_>, form: &AdminForm) -> Response {
        let meta = T::admin_meta();
        let pool = ctx.tenant.pool();
        let mut row = if ctx.action == Some("new") {
            if !T::can_create(pool).await {
                return forbidden();
            }
            T::admin_new()
        } else {
            match Self::load(ctx, form.int("id")).await {
                Ok(Some(row)) => row,
                Ok(None) => return not_found(),
                Err(resp) => return resp,
            }
        };
        if let Err(errors) = save_record(pool, &mut row, form).await {
            return Self::form(ctx, &row, &errors, Some(form)).await;
        }
        let message = format!("Saved {} “{}”.", meta.singular, row.admin_title());
        let back = match (form.get("_continue"), row.admin_id()) {
            (Some(_), Some(id)) => format!("{}/edit?id={id}", ctx.page_url()),
            _ => ctx.page_url(),
        };
        ctx.redirect(&back, MessageLevel::Success, &message)
    }

    async fn delete(ctx: &AdminPageCtx<'_>, form: &AdminForm) -> Response {
        let meta = T::admin_meta();
        let row = match Self::load(ctx, form.int("id")).await {
            Ok(Some(row)) => row,
            Ok(None) => return not_found(),
            Err(resp) => return resp,
        };
        let title = row.admin_title();
        match delete_record(ctx.tenant.pool(), row).await {
            Ok(true) => ctx.redirect(&ctx.page_url(), MessageLevel::Success, &format!("Deleted {} “{title}”.", meta.singular)),
            Ok(false) => forbidden(),
            Err(e) => ctx.redirect(&ctx.page_url(), MessageLevel::Error, &format!("Could not delete “{title}”: {e}")),
        }
    }
}

/// Apply a posted form to `row` and store it, running the hooks in
/// order: [`CmsAdmin::admin_apply`] (skipping
/// [`CmsAdminOverrides::read_only_fields`]), [`CmsAdminOverrides::validate`],
/// the save, [`CmsAdminOverrides::save_extra`], then
/// [`CmsAdminOverrides::after_save`]. A row without a primary key is
/// added, and only when [`CmsAdminOverrides::can_create`] allows it.
///
/// # Errors
/// The validation messages, or one `"_form"` error carrying a refused
/// add or a database failure.
pub async fn save_record<T: CmsAdmin>(pool: &Pool, row: &mut T, form: &AdminForm) -> Result<(), Vec<FieldError>> {
    let creating = row.admin_id().is_none();
    if creating && !T::can_create(pool).await {
        return Err(vec![FieldError::new("_form", format!("New {} can't be added here.", T::admin_meta().singular))]);
    }
    let read_only = row.read_only_fields(pool).await;
    let mut errors = row.admin_apply(form, &read_only);
    if errors.is_empty() {
        errors = row.validate(pool).await;
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let stored = async {
        row.admin_save(pool).await?;
        row.save_extra(pool, form).await?;
        row.after_save(pool, creating).await
    }
    .await;
    stored.map_err(|e| vec![FieldError::new("_form", e.to_string())])
}

/// Delete `row` after [`CmsAdminOverrides::can_delete`] and
/// [`CmsAdminOverrides::before_delete`]. `Ok(false)` when `can_delete`
/// refuses.
///
/// # Errors
/// A failure from `before_delete` or the delete itself.
pub async fn delete_record<T: CmsAdmin>(pool: &Pool, row: T) -> Result<bool, ExecError> {
    if !row.can_delete(pool).await {
        return Ok(false);
    }
    row.before_delete(pool).await?;
    row.admin_delete(pool).await?;
    Ok(true)
}

fn meta_json(meta: &CmsAdminMeta) -> Value {
    json!({
        "slug": meta.slug,
        "label": meta.label,
        "singular": meta.singular,
        "icon": meta.icon,
        "searchable": meta.searchable,
        "url": format!("/cms-admin/x/{}", meta.slug),
    })
}

#[async_trait]
impl<T: CmsAdmin> AdminPageHandler for CmsAdminPage<T> {
    fn slug(&self) -> &'static str {
        T::admin_meta().slug
    }

    fn label(&self) -> &'static str {
        T::admin_meta().label
    }

    fn icon(&self) -> Option<&'static str> {
        T::admin_meta().icon
    }

    fn section(&self) -> SidebarSection {
        T::admin_meta().section
    }

    fn sort_order(&self) -> i32 {
        T::admin_meta().sort_order
    }

    fn permission_resource(&self) -> Option<&'static str> {
        Some(T::admin_meta().permission_resource)
    }

    fn permission_actions(&self) -> &'static [Action] {
        CRUD
    }

    async fn render(&self, ctx: AdminPageCtx<'_>) -> Response {
        match ctx.action {
            None => Self::list(&ctx).await,
            Some("new") => {
                if !T::can_create(ctx.tenant.pool()).await {
                    return forbidden();
                }
                Self::form(&ctx, &T::admin_new(), &[], None).await
            }
            Some("edit") => {
                let id = ctx.query_param("id").and_then(|v| v.parse().ok());
                match Self::load(&ctx, id).await {
                    Ok(Some(row)) => Self::form(&ctx, &row, &[], None).await,
                    Ok(None) => not_found(),
                    Err(resp) => resp,
                }
            }
            Some(_) => not_found(),
        }
    }

    async fn post(&self, ctx: AdminPageCtx<'_>, form: AdminForm) -> Response {
        match ctx.action {
            Some("new" | "edit") => Self::save(&ctx, &form).await,
            Some("delete") => Self::delete(&ctx, &form).await,
            _ => not_found(),
        }
    }
}

/// Helpers the derive's generated code calls. Not a stable API.
#[doc(hidden)]
pub mod __derive {
    use super::{AdminForm, FieldError};

    /// The posted text for a `String` field. A required field left
    /// empty records an error.
    pub fn string(form: &AdminForm, name: &str, required: bool, errors: &mut Vec<FieldError>) -> Option<String> {
        let v = form.get(name)?.to_owned();
        if required && v.trim().is_empty() {
            errors.push(FieldError::new(name, "This field is required."));
        }
        Some(v)
    }

    /// The posted value for an `Option<String>` field: blank is `None`.
    pub fn opt_string(form: &AdminForm, name: &str) -> Option<Option<String>> {
        let v = form.get(name)?;
        Some(Some(v.to_owned()).filter(|s| !s.trim().is_empty()))
    }

    /// A checkbox: absent means unchecked.
    #[must_use]
    pub fn boolean(form: &AdminForm, name: &str) -> bool {
        form.checked(name)
    }

    /// A required value parsed with `FromStr`. `None` leaves the field
    /// unchanged: the value was not posted, or did not parse (recorded).
    pub fn parsed<T: std::str::FromStr>(form: &AdminForm, name: &str, errors: &mut Vec<FieldError>) -> Option<T> {
        let raw = form.get(name)?.trim();
        if raw.is_empty() {
            errors.push(FieldError::new(name, "This field is required."));
            return None;
        }
        raw.parse().map_or_else(
            |_| {
                errors.push(FieldError::new(name, "Enter a valid value."));
                None
            },
            Some,
        )
    }

    /// An optional value parsed with `FromStr`: blank is `None`.
    pub fn opt_parsed<T: std::str::FromStr>(form: &AdminForm, name: &str, required: bool, errors: &mut Vec<FieldError>) -> Option<Option<T>> {
        let raw = form.get(name)?.trim();
        if raw.is_empty() {
            if required {
                errors.push(FieldError::new(name, "This field is required."));
                return None;
            }
            return Some(None);
        }
        match raw.parse() {
            Ok(v) => Some(Some(v)),
            Err(_) => {
                errors.push(FieldError::new(name, "Enter a valid value."));
                None
            }
        }
    }

    /// An `Option<T>` as widget text.
    #[must_use]
    pub fn show_opt<T: std::fmt::Display>(v: Option<&T>) -> String {
        v.map(ToString::to_string).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::__derive::*;
    use super::*;

    fn form(body: &str) -> AdminForm {
        AdminForm::parse(body.as_bytes())
    }

    #[test]
    fn sections_parse_to_built_ins_or_a_new_heading() {
        assert_eq!(section_from("content"), SidebarSection::Content);
        assert_eq!(section_from("Site"), SidebarSection::Site);
        assert_eq!(section_from("Giving Days"), SidebarSection::Custom("Giving Days"));
    }

    #[test]
    fn values_parse_or_record_an_error() {
        let f = form("n=42&bad=x&blank=&s=hi&o=");
        let mut errors = Vec::new();
        assert_eq!(parsed::<i32>(&f, "n", &mut errors), Some(42));
        assert_eq!(parsed::<i32>(&f, "bad", &mut errors), None);
        assert_eq!(parsed::<i32>(&f, "absent", &mut errors), None);
        assert_eq!(opt_parsed::<i64>(&f, "blank", false, &mut errors), Some(None));
        assert_eq!(string(&f, "s", true, &mut errors), Some("hi".into()));
        assert_eq!(opt_string(&f, "o"), Some(None));
        assert_eq!(errors, vec![FieldError::new("bad", "Enter a valid value.")]);
        let mut errors = Vec::new();
        assert_eq!(string(&f, "blank", true, &mut errors), Some(String::new()));
        assert_eq!(opt_parsed::<i64>(&f, "blank", true, &mut errors), None);
        assert_eq!(errors.len(), 2, "both required blanks are reported");
    }

    #[test]
    fn checkbox_absent_is_false() {
        assert!(boolean(&form("a=on"), "a"));
        assert!(!boolean(&form("b=on"), "a"));
    }

    #[test]
    fn form_keeps_repeated_values() {
        let f = form("ids=1&ids=2&ids=x&name=+a+");
        assert_eq!(f.ints("ids"), vec![1, 2]);
        assert_eq!(f.all("ids").len(), 3);
        assert_eq!(f.text("name"), "a");
        assert_eq!(f.get("ids"), Some("x"), "get returns the last value");
    }
}
