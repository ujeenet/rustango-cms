//! #321 — end-to-end coverage of the `login_required` admin gate
//! (Section E, #319) through the framework's in-process [`TestClient`].
//!
//! Unlike the tree-concurrency test, this needs **no database**: the gate
//! is pure session-cookie middleware (`admin::with_login_required`), so an
//! anonymous request is bounced to the login URL *before* it reaches any
//! handler or touches a tenant pool. That makes it a normal, CI-runnable
//! test — no `#[ignore]`, no `DATABASE_URL`.

use std::sync::Arc;

use rustango::test_client::TestClient;
use tera::Tera;

/// The real cms-admin router, wrapped in the `login_required` gate the
/// demo + production mounts use. `admin::router` is self-contained (it
/// generates its own signing key + a no-op cache), so no DB/state setup
/// is required to build it.
fn gated_admin_router() -> axum::Router {
    let mut tera = Tera::default();
    rustango_cms::admin::register_templates(&mut tera).expect("register admin templates");
    rustango_cms::admin::with_login_required(rustango_cms::admin::router(Arc::new(tera)), "/login")
}

#[tokio::test]
async fn anonymous_admin_requests_redirect_to_login_carrying_next() {
    let client = TestClient::new(gated_admin_router());

    // A representative spread: a list view, the create form, and an
    // id-parameterized edit URL. All sit behind the gate.
    for path in [
        "/cms-admin/pages",
        "/cms-admin/pages/new",
        "/cms-admin/pages/42/edit",
    ] {
        let resp = client.get(path).send().await;
        assert_eq!(
            resp.status, 302,
            "anonymous GET {path} should redirect, got HTTP {}",
            resp.status
        );
        let location = resp
            .header("location")
            .unwrap_or_else(|| panic!("{path}: 302 with no Location header"));
        assert!(
            location.starts_with("/login"),
            "{path}: expected a /login redirect, got `{location}`"
        );
        // The gate preserves the original URL in `?next=` so the user
        // resumes where they were after signing in.
        assert!(
            location.contains("next="),
            "{path}: redirect should carry `?next=`, got `{location}`"
        );
    }
}
