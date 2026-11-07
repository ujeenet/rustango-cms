//! #731 — `snippet_html` renders only the rows a page asks for, once each,
//! including snippets a snippet template asks for.
#![cfg(feature = "sqlite")]

use rustango::sql::{Auto, Pool};
use rustango_cms::library::LibraryTypeHandler;
use rustango_cms::snippet::Snippet;
use tera::{Context, Tera};

#[derive(Default)]
struct TestBadge;

impl LibraryTypeHandler for TestBadge {
    fn app_label(&self) -> &'static str {
        "test"
    }
    fn type_name(&self) -> &'static str {
        "TestBadge"
    }
    fn verbose_name(&self) -> &'static str {
        "Badge"
    }
    fn render_template(&self) -> Option<&'static str> {
        Some("badge.html")
    }
}
rustango_cms::register_library_type!(TestBadge);

async fn pool_with(rows: &[(&str, &str)]) -> (Pool, Vec<i64>) {
    let pool = Pool::connect("sqlite::memory:").await.expect("pool");
    let sql = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
        pool.dialect(),
        <Snippet as rustango::core::Model>::SCHEMA,
    );
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(&pool, stmt, Vec::new()).await.expect("ddl");
    }
    let mut ids = Vec::new();
    for (slug, title) in rows {
        let mut s = Snippet {
            id: Auto::Unset,
            type_name: "TestBadge".into(),
            folder_path: String::new(),
            slug: (*slug).into(),
            title: (*title).into(),
            body_markdown: String::new(),
            data: serde_json::json!({}),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        s.save(&pool).await.expect("snippet");
        ids.push(*s.id.get().expect("id"));
    }
    (pool, ids)
}

fn tera_with(badge: &str, page: &str) -> Tera {
    let mut tera = Tera::default();
    rustango_cms::snippet_render::register_tera_function(&mut tera);
    tera.add_raw_template("badge.html", badge).unwrap();
    tera.add_raw_template("page.html", page).unwrap();
    tera
}

async fn render_page(pool: &Pool, tera: &Tera) -> String {
    let rec = rustango_cms::snippet_render::record();
    let html = tera.render("page.html", &Context::new()).unwrap();
    let keys = rec.take();
    rustango_cms::snippet_render::fill(pool, tera, None, html, keys).await
}

#[tokio::test]
async fn a_page_gets_the_snippets_it_names_and_only_those() {
    let (pool, ids) = pool_with(&[("a", "Alpha"), ("b", "Beta"), ("c", "Gamma")]).await;
    let page = format!(
        r#"[{{{{ snippet_html(type="TestBadge", id={a}) }}}}|{{{{ snippet_html(type="TestBadge", id={a}) }}}}|{{{{ snippet_html(type="TestBadge", id=999) }}}}]"#,
        a = ids[0]
    );
    let tera = tera_with("<b>{{ snippet.title }}</b>", &page);
    assert_eq!(render_page(&pool, &tera).await, "[<b>Alpha</b>|<b>Alpha</b>|]");
}

#[tokio::test]
async fn a_snippet_template_can_name_another_snippet() {
    let (pool, ids) = pool_with(&[("outer", "Outer"), ("inner", "Inner")]).await;
    // The outer badge's template pulls in the inner one by id.
    let badge = format!(
        r#"{{{{ snippet.title }}}}{{% if snippet.slug == "outer" %}}({{{{ snippet_html(type="TestBadge", id={inner}) }}}}){{% endif %}}"#,
        inner = ids[1]
    );
    let page = format!(r#"<{{{{ snippet_html(type="TestBadge", id={outer}) }}}}>"#, outer = ids[0]);
    let tera = tera_with(&badge, &page);
    assert_eq!(render_page(&pool, &tera).await, "<Outer(Inner)>");
}

#[tokio::test]
async fn an_unregistered_type_renders_empty() {
    let (pool, _) = pool_with(&[]).await;
    let tera = tera_with("x", r#"[{{ snippet_html(type="NoSuchType", id=1) }}]"#);
    assert_eq!(render_page(&pool, &tera).await, "[]");
}

#[tokio::test]
async fn a_snippet_that_names_itself_does_not_multiply() {
    let (pool, ids) = pool_with(&[("loop", "Loop")]).await;
    // Fifty self-references per render: without cycle handling this
    // expands geometrically.
    let badge = format!(
        r#"[{{{{ snippet.title }}}}{{% for i in range(end=50) %}}{{{{ snippet_html(type="TestBadge", id={id}) }}}}{{% endfor %}}]"#,
        id = ids[0]
    );
    let page = format!(r#"{{{{ snippet_html(type="TestBadge", id={id}) }}}}"#, id = ids[0]);
    let tera = tera_with(&badge, &page);
    assert_eq!(render_page(&pool, &tera).await, "[Loop]");
}

#[tokio::test]
async fn no_marker_survives_in_the_page() {
    let (pool, ids) = pool_with(&[("a", "A")]).await;
    // A marker for a row that doesn't exist, written straight into the page.
    let page = format!(
        r#"x<!--rcms-snippet:TestBadge:999-->{{{{ snippet_html(type="TestBadge", id={id}) }}}}y"#,
        id = ids[0]
    );
    let tera = tera_with("{{ snippet.title }}", &page);
    assert_eq!(render_page(&pool, &tera).await, "xAy");
}
