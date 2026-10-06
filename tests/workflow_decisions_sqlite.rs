//! #707 — a workflow step is decided once, however many requests race.
#![cfg(feature = "sqlite")]

use chrono::Utc;
use rustango::core::{Column as _, Model as _};
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::tenancy::auth::User;
use rustango::tenancy::permissions::Role;

use rustango_cms::page::{Page, PageStatus};
use rustango_cms::page_type_model::PageType;
use rustango_cms::workflow::{
    approve_current, reject_current, submit_for_review, ApproveOutcome, TaskState, Workflow,
    WorkflowState, WorkflowTask,
};

async fn ddl(pool: &Pool, schema: &rustango::core::ModelSchema) {
    let sql =
        rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(pool.dialect(), schema);
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .expect("ddl");
    }
}

async fn user(pool: &Pool, name: &str, superuser: bool) -> i64 {
    let mut u = User {
        id: Auto::Unset,
        username: name.to_owned(),
        password_hash: String::new(),
        email: None,
        is_superuser: superuser,
        active: true,
        created_at: Utc::now(),
        data: serde_json::json!({}),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    u.save_pool(pool).await.expect("user");
    u.id.get().copied().expect("user id")
}

async fn page(pool: &Pool, type_id: i64, slug: &str, path: &str, parent: Option<i64>) -> i64 {
    let mut p = Page {
        id: Auto::Unset,
        page_type_id: type_id,
        title: slug.to_owned(),
        slug: slug.to_owned(),
        path: path.to_owned(),
        url_path: format!("/{slug}"),
        preview_path: String::new(),
        template_override: String::new(),
        depth: path.split('/').filter(|s| !s.is_empty()).count() as i32,
        parent_id: parent,
        locale_variant_of: None,
        alias_of: None,
        theme_id: None,
        sort_order: 0,
        status: PageStatus::Published.as_str().to_owned(),
        published_at: None,
        last_published_at: None,
        go_live_at: None,
        expire_at: None,
        seo_title: String::new(),
        seo_description: String::new(),
        robots_index: true,
        sitemap_priority: 0.5,
        show_in_menus: true,
        og_title: String::new(),
        og_description: String::new(),
        og_image_media_id: None,
        twitter_card: "summary".to_owned(),
        notification_pre_published_sent: false,
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    p.save_pool(pool).await.expect("page");
    p.id.get().copied().expect("page id")
}

async fn setup() -> (Pool, i64, Workflow, Vec<WorkflowTask>) {
    let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
    for schema in [
        &User::SCHEMA,
        &Role::SCHEMA,
        &PageType::SCHEMA,
        &rustango_cms::media::MediaCollection::SCHEMA,
        &rustango_cms::media::Media::SCHEMA,
        &rustango_cms::theme::Theme::SCHEMA,
        &Page::SCHEMA,
        &Workflow::SCHEMA,
        &WorkflowTask::SCHEMA,
        &WorkflowState::SCHEMA,
        &TaskState::SCHEMA,
    ] {
        ddl(&pool, schema).await;
    }
    let reviewer = user(&pool, "reviewer", true).await;
    let mut role = Role { id: Auto::Unset, name: "Review".to_owned(), description: String::new(), data: serde_json::json!({}) };
    role.save_pool(&pool).await.expect("role");
    let role_id = role.id.get().copied().expect("role id");
    let mut pt = PageType {
        id: Auto::Unset,
        app_label: "cms".to_owned(),
        type_name: "TestPage".to_owned(),
        verbose_name: "Test page".to_owned(),
        default_template: "page.html".to_owned(),
        view_mode: "auto".to_owned(),
        is_creatable: true,
        allowed_parent_types: serde_json::json!([]),
        allowed_child_types: serde_json::json!([]),
        workflow: String::new(),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    pt.save_pool(&pool).await.expect("page type");
    let page_id = page(&pool, pt.id.get().copied().expect("type id"), "p", "0001/", None).await;

    let mut wf = Workflow {
        id: Auto::Unset,
        name: "Two step".to_owned(),
        description: String::new(),
        active: true,
        require_reapproval_on_edit: false,
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    wf.save_pool(&pool).await.expect("workflow");
    let wf_id = wf.id.get().copied().expect("wf id");
    for (n, name) in ["Step 1", "Step 2"].iter().enumerate() {
        let mut t = WorkflowTask {
            id: Auto::Unset,
            workflow_id: wf_id,
            name: (*name).to_owned(),
            role_id,
            sort_order: n as i32,
            kind: "group_approval".to_owned(),
            webhook_url: String::new(),
        };
        t.save_pool(&pool).await.expect("task");
    }
    let tasks = rustango_cms::workflow::tasks_for(&pool, wf_id).await.expect("tasks");
    submit_for_review(&pool, page_id, &wf, &tasks, reviewer).await.expect("submit");
    (pool, reviewer, wf, tasks)
}

async fn state(pool: &Pool) -> WorkflowState {
    WorkflowState::objects().first(pool).await.expect("q").expect("state")
}

async fn in_progress(pool: &Pool) -> usize {
    TaskState::objects()
        .where_(TaskState::status.eq("in_progress".to_owned()))
        .fetch(pool)
        .await
        .expect("q")
        .len()
}

#[tokio::test]
async fn two_approvals_of_one_step_advance_it_once() {
    let (pool, reviewer, _wf, tasks) = setup().await;
    // Both requests loaded the workflow while it sat on step 1.
    let (mut a, mut b) = (state(&pool).await, state(&pool).await);
    let first = approve_current(&pool, &mut a, &tasks, reviewer, "ok").await.expect("approve");
    assert!(matches!(first, ApproveOutcome::Advanced { .. }));
    let second = approve_current(&pool, &mut b, &tasks, reviewer, "ok").await.expect("approve");
    assert!(matches!(second, ApproveOutcome::AlreadyDecided), "{second:?}");

    let now = state(&pool).await;
    assert_eq!(now.current_task_id, tasks[1].id.get().copied(), "on step 2, not finished");
    assert_eq!(now.status, "in_progress");
    assert_eq!(in_progress(&pool).await, 1, "exactly one open step");
}

#[tokio::test]
async fn a_reject_racing_an_approve_changes_nothing() {
    let (pool, reviewer, _wf, tasks) = setup().await;
    let (mut a, mut b) = (state(&pool).await, state(&pool).await);
    approve_current(&pool, &mut a, &tasks, reviewer, "ok").await.expect("approve");
    assert!(!reject_current(&pool, &mut b, reviewer, "no").await.expect("reject"));
    let now = state(&pool).await;
    assert_eq!(now.status, "in_progress", "the stale reject did not send it back");
}

/// A rejected page resubmitted and approved is approved: the earlier
/// round's `needs_changes` must not come back as the page's state, and
/// the rejection's reason stays in the history.
#[tokio::test]
async fn approval_after_a_resubmission_is_the_page_state() {
    let (pool, reviewer, wf, tasks) = setup().await;
    let page_id = Page::objects().first(&pool).await.expect("q").expect("page").id.get().copied().expect("id");
    let mut first = state(&pool).await;
    assert!(reject_current(&pool, &mut first, reviewer, "Add the size").await.expect("reject"));

    submit_for_review(&pool, page_id, &wf, &tasks, reviewer).await.expect("resubmit");
    let mut round = rustango_cms::workflow::active_state_for_page(&pool, page_id)
        .await
        .expect("q")
        .expect("the new round is active");
    assert_eq!(round.status, "in_progress");
    approve_current(&pool, &mut round, &tasks, reviewer, "ok").await.expect("step 1");
    approve_current(&pool, &mut round, &tasks, reviewer, "ok").await.expect("step 2");

    assert!(
        rustango_cms::workflow::active_state_for_page(&pool, page_id).await.expect("q").is_none(),
        "nothing left to review"
    );
    let latest = rustango_cms::workflow::latest_state_for_page(&pool, page_id).await.expect("q").expect("state");
    assert_eq!(latest.status, "approved");
    let history = rustango_cms::workflow::history_for_page(&pool, page_id).await.expect("q");
    assert_eq!(history.len(), 3, "the rejection and both approvals");
    assert_eq!(history[0].comment, "Add the size");
}
