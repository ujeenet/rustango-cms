//! #940 — `#[derive(CmsAdmin)]`: the generated editor lists, searches,
//! creates, edits and deletes rows, honours the hooks, and maps every
//! request to the right role-matrix codename.
#![cfg(feature = "sqlite")]

use async_trait::async_trait;
use rustango::core::Model as _;
use rustango::sql::{Auto, ExecError, FetcherPool as _, Pool};
use rustango::Model;
use rustango_cms::admin::admin_page::required_codename;
use rustango_cms::{
    delete_record, save_record, AdminForm, CmsAdmin, CmsAdminOverrides, CmsAdminPage,
    FieldError, SidebarSection,
};
use serde::{Deserialize, Serialize};

#[derive(Model, CmsAdmin, Debug, Clone, Default, Serialize, Deserialize)]
#[rustango(table = "test_cause", app = "test", display = "name")]
#[cms_admin(
    label = "Causes",
    singular = "cause",
    slug = "causes",
    icon = "volunteer_activism",
    section = "Giving",
    list = "name, external_id, is_active",
    search = "name, description",
    ordering = "sort_order, name"
)]
pub struct Cause {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    #[field(widget = Text, label = "Name", required)]
    pub name: String,
    #[field(widget = Textarea, label = "Description")]
    pub description: String,
    #[field(widget = Number, label = "Sort order", default = "10")]
    pub sort_order: i32,
    #[field(widget = Boolean, label = "Active")]
    pub is_active: bool,
    #[field(widget = Number, label = "External id")]
    pub external_id: Option<i64>,
    #[field(widget = Select, label = "Parent")]
    pub parent_id: Option<i64>,
}

/// Rows with an external id are owned by another system: their id is
/// read-only and they can't be deleted. A name may not be "forbidden".
/// Deleting one clears the parent link of its children.
#[async_trait]
impl CmsAdminOverrides for Cause {
    async fn read_only_fields(&self, _pool: &Pool) -> Vec<&'static str> {
        if self.external_id.is_some() { vec!["external_id"] } else { Vec::new() }
    }

    async fn can_delete(&self, _pool: &Pool) -> bool {
        self.external_id.is_none()
    }

    async fn validate(&self, _pool: &Pool) -> Vec<FieldError> {
        if self.name.trim() == "forbidden" {
            vec![FieldError::new("name", "Pick another name.")]
        } else {
            Vec::new()
        }
    }

    async fn field_options(pool: &Pool, field: &str) -> Option<Vec<(String, String)>> {
        if field != "parent_id" {
            return None;
        }
        let rows = Cause::objects().fetch(pool).await.ok()?;
        Some(rows.iter().map(|c| (c.admin_id().unwrap_or_default().to_string(), c.name.clone())).collect())
    }

    async fn before_delete(&self, pool: &Pool) -> Result<(), ExecError> {
        use rustango::core::Column as _;
        let id = self.admin_id().unwrap_or_default();
        for mut child in Cause::objects().where_(Cause::parent_id.eq(Some(id))).fetch(pool).await? {
            child.parent_id = None;
            child.save(pool).await?;
        }
        Ok(())
    }

    async fn list_column(_pool: &Pool, rows: &[Self], column: &str) -> Option<Vec<String>> {
        (column == "external_id").then(|| {
            rows.iter().map(|r| r.external_id.map_or_else(|| "local".to_owned(), |id| id.to_string())).collect()
        })
    }
}

async fn pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    let sql = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), Cause::SCHEMA);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(&pool, stmt, Vec::new()).await.expect("ddl");
    }
    pool
}

fn form(body: &str) -> AdminForm {
    AdminForm::parse(body.as_bytes())
}

async fn add(pool: &Pool, body: &str) -> Cause {
    let mut row = Cause::admin_new();
    save_record(pool, &mut row, &form(body)).await.expect("saved");
    row
}

#[test]
fn the_page_registers_with_its_meta_and_crud_codenames() {
    let page = rustango_cms::find_admin_page("causes").expect("derive registered the page");
    assert_eq!(page.label(), "Causes");
    assert_eq!(page.icon(), Some("volunteer_activism"));
    assert_eq!(page.section(), SidebarSection::Custom("Giving"));
    assert_eq!(page.permission_resource(), Some("test_cause"));

    assert_eq!(required_codename("causes", true, None).as_deref(), Some("test_cause.view"));
    assert_eq!(required_codename("causes", true, Some("edit")).as_deref(), Some("test_cause.view"));
    assert_eq!(required_codename("causes", false, Some("new")).as_deref(), Some("test_cause.add"));
    assert_eq!(required_codename("causes", false, Some("edit")).as_deref(), Some("test_cause.edit"));
    assert_eq!(required_codename("causes", false, Some("delete")).as_deref(), Some("test_cause.delete"));
    assert_eq!(required_codename("no-such-page", true, None), None);
    let _ = CmsAdminPage::<Cause>::default();
}

#[test]
fn widgets_follow_the_field_attributes() {
    let blank = Cause::admin_new();
    let widgets = blank.admin_widgets();
    let names: Vec<&str> = widgets.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(names, ["name", "description", "sort_order", "is_active", "external_id", "parent_id"]);
    assert!(widgets[0].required);
    assert_eq!(widgets[2].value, "10", "a new row shows the declared default");
    let saved = Cause { id: Auto::Set(3), sort_order: 4, is_active: true, ..Cause::default() };
    let widgets = saved.admin_widgets();
    assert_eq!(widgets[2].value, "4", "a saved row shows its own value");
    assert_eq!(widgets[3].value, "true");
}

#[tokio::test]
async fn create_edit_search_and_paginate() {
    let pool = pool().await;
    let water = add(&pool, "name=Clean+water&description=Wells&sort_order=2&is_active=on").await;
    add(&pool, "name=Books&description=Library+fund&sort_order=1").await;
    add(&pool, "name=Trees&description=Planting+water-wise+trees&sort_order=3&external_id=77").await;
    assert!(water.admin_id().is_some(), "the insert set the primary key");

    let (rows, total) = Cause::admin_list(&pool, "", 25, 0).await.expect("list");
    assert_eq!(total, 3);
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["Books", "Clean water", "Trees"], "ordered by sort_order");

    let (rows, total) = Cause::admin_list(&pool, "WATER", 25, 0).await.expect("search");
    assert_eq!(total, 2, "search is case-insensitive across name and description");
    assert_eq!(rows.len(), 2);

    let (rows, total) = Cause::admin_list(&pool, "", 1, 1).await.expect("page 2");
    assert_eq!((rows.len(), total), (1, 3));
    assert_eq!(rows[0].name, "Clean water");

    let mut row = Cause::admin_get(&pool, water.admin_id().unwrap()).await.expect("get").expect("row");
    assert!(row.is_active);
    save_record(&pool, &mut row, &form("name=Clean+water+2&description=&sort_order=5")).await.expect("edit");
    let row = Cause::admin_get(&pool, water.admin_id().unwrap()).await.unwrap().unwrap();
    assert_eq!(row.name, "Clean water 2");
    assert!(!row.is_active, "an unchecked box clears the flag");
    assert_eq!(row.sort_order, 5);

    assert_eq!(row.admin_title(), "Clean water 2");
    assert_eq!(row.admin_cell("is_active"), "No");
    let cells = Cause::list_column(&pool, &[row], "external_id").await.expect("computed column");
    assert_eq!(cells, ["local"]);
}

#[tokio::test]
async fn bad_input_is_reported_and_nothing_is_saved() {
    let pool = pool().await;
    let mut row = Cause::admin_new();
    let errors = save_record(&pool, &mut row, &form("name=&sort_order=lots")).await.expect_err("invalid");
    let fields: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();
    assert_eq!(fields, ["name", "sort_order"]);
    assert!(row.admin_id().is_none());

    let errors = save_record(&pool, &mut row, &form("name=forbidden&sort_order=1")).await.expect_err("validate");
    assert_eq!(errors, vec![FieldError::new("name", "Pick another name.")]);
    let (_, total) = Cause::admin_list(&pool, "", 25, 0).await.unwrap();
    assert_eq!(total, 0);
}

#[tokio::test]
async fn read_only_fields_and_can_delete_protect_owned_rows() {
    let pool = pool().await;
    let mut owned = add(&pool, "name=Owned&sort_order=1&external_id=9").await;
    save_record(&pool, &mut owned, &form("name=Renamed&sort_order=1&external_id=1000")).await.expect("edit");
    let owned = Cause::admin_get(&pool, owned.admin_id().unwrap()).await.unwrap().unwrap();
    assert_eq!(owned.name, "Renamed", "editable fields still save");
    assert_eq!(owned.external_id, Some(9), "the read-only field kept its value");

    assert!(!delete_record(&pool, owned.clone()).await.expect("refused"), "can_delete said no");
    assert!(Cause::admin_get(&pool, owned.admin_id().unwrap()).await.unwrap().is_some());
}

#[tokio::test]
async fn delete_runs_before_delete_first() {
    let pool = pool().await;
    let parent = add(&pool, "name=Parent&sort_order=1").await;
    let pid = parent.admin_id().unwrap();
    let child = add(&pool, &format!("name=Child&sort_order=2&parent_id={pid}")).await;
    assert_eq!(child.parent_id, Some(pid));

    let options = Cause::field_options(&pool, "parent_id").await.expect("options");
    assert_eq!(options.len(), 2, "the parent select lists every cause");

    assert!(delete_record(&pool, parent).await.expect("deleted"));
    assert!(Cause::admin_get(&pool, pid).await.unwrap().is_none());
    let child = Cause::admin_get(&pool, child.admin_id().unwrap()).await.unwrap().unwrap();
    assert_eq!(child.parent_id, None, "before_delete cleared the link");
}
