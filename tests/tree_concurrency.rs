//! #321 / #317 — DATABASE_URL-gated concurrency regression tests for the
//! `sort_order` race that #317 fixed across all three tree write paths.
//!
//! The pre-fix code read `MAX(sort_order)+1` on the pool, *outside* the
//! write transaction, so two concurrent inserts/moves under the same
//! parent could read the same max and write a duplicate `sort_order`.
//! The fix takes a `SELECT … FOR UPDATE` lock inside the tx (the parent
//! row for `create_child`/`move_to`, the roots set for `create_root`).
//! Each test hammers a write path from many tasks at once and asserts
//! every row got a distinct `sort_order`.
//!
//! Needs a **real PostgreSQL** tenant — SQLite serializes writers so the
//! race can't reproduce there, and `Tenant::for_test`'s `TenantConn` is
//! only obtainable via the PG-only `TenantPools::acquire`. `#[ignore]`d
//! so the normal suite skips it; run by hand against a migrated tenant:
//!
//! ```sh
//! RCMS_TEST_REGISTRY_URL=postgres://postgres:pw@127.0.0.1:5433/rcms_test_registry \
//! RCMS_TEST_TENANT_URL=postgres://postgres:pw@127.0.0.1:5433/rcms_test_tenant \
//!   cargo test --features test_utils --test tree_concurrency -- --ignored --nocapture
//! ```
//! (One-time setup: migrate a PG tenant + insert a permissive
//! `rcms_race_test` page type — see `assert_distinct` / the README.)
#![cfg(all(feature = "test_utils", feature = "postgres"))]

use std::sync::Arc;

use rustango::core::Column as _;
use rustango::extractors::Tenant;
use rustango::sql::Pool;
use rustango::tenancy::{Org, TenantPools, TenantPoolsConfig};

use rustango_cms::tree_ops::NewPage;
use rustango_cms::{Page, PageType};

const N: usize = 16;
const TEST_TYPE: &str = "rcms_race_test";

/// Shared fixture for the DB-gated tree tests — the #321 `test_data`
/// idea, as a plain async helper. Returns `None` (so the caller skips)
/// when the env isn't configured. Both pools are sized to the fleet:
/// with the `FOR UPDATE` lock every task opens a tx and blocks on the
/// lock at once, so each holds a connection simultaneously.
struct Harness {
    pools: Arc<TenantPools<rustango::sql::sqlx::Postgres>>,
    org: Org,
    tenant_pool: Pool,
    type_id: i64,
}

impl Harness {
    /// A fresh tenant bound to its own acquired connection — one per
    /// concurrent task (each task needs an independent `&Tenant`).
    async fn tenant(&self) -> Tenant {
        let conn = self.pools.acquire(&self.org).await.expect("acquire");
        Tenant::for_test(self.org.clone(), conn, self.tenant_pool.clone())
    }
}

/// Panics without its databases (#669): these tests are `#[ignore]`d, so
/// reaching one means it was asked for, and a missing variable must fail
/// rather than report `ok` having tested nothing.
async fn setup() -> Harness {
    let (Ok(registry_url), Ok(tenant_url)) = (
        std::env::var("RCMS_TEST_REGISTRY_URL"),
        std::env::var("RCMS_TEST_TENANT_URL"),
    ) else {
        panic!("set RCMS_TEST_REGISTRY_URL and RCMS_TEST_TENANT_URL to run the tree concurrency tests");
    };

    let registry = Pool::connect(&registry_url).await.expect("registry pool");
    let pg_tenant = rustango::sql::sqlx::postgres::PgPoolOptions::new()
        .max_connections(N as u32 + 8)
        .connect(&tenant_url)
        .await
        .expect("tenant pgpool");
    let pg_tenant_check = pg_tenant.clone();
    let tenant_pool: Pool = pg_tenant.into();

    // Clean slate. This wipes every page, so refuse anything that isn't
    // plainly a throwaway test database (#730).
    let db: String = rustango::sql::sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pg_tenant_check)
        .await
        .expect("current_database");
    assert!(
        db.contains("test"),
        "RCMS_TEST_TENANT_URL points at `{db}`; this test deletes every page, so it only runs \
         against a database whose name contains `test`"
    );
    rustango::sql::raw_execute_pool(&tenant_pool, "DELETE FROM cms_page", vec![])
        .await
        .expect("clear cms_page");

    let page_type: PageType = PageType::objects()
        .where_(PageType::type_name.eq(TEST_TYPE))
        .first(&tenant_pool)
        .await
        .expect("query page type")
        .expect("seed the `rcms_race_test` page type first (permissive whitelists)");
    let type_id = *page_type.id.get().expect("page type id");

    let org: Org = Org::objects()
        .where_(Org::slug.eq("demo"))
        .first(&registry)
        .await
        .expect("query org")
        .expect("the `demo` tenant must exist in the registry");

    let pg = registry.as_postgres().expect("PG registry pool").clone();
    let pools = Arc::new(TenantPools::new(pg).config(TenantPoolsConfig {
        database_pool_max_connections: N as u32 + 8,
        ..Default::default()
    }));

    Harness {
        pools,
        org,
        tenant_pool,
        type_id,
    }
}

/// Assert a set of concurrently-assigned sort_orders has no duplicates.
fn assert_distinct(mut sort_orders: Vec<i32>, what: &str) {
    sort_orders.sort_unstable();
    let mut unique = sort_orders.clone();
    unique.dedup();
    assert_eq!(
        unique.len(),
        sort_orders.len(),
        "concurrent {what} handed out duplicate sort_orders: {sort_orders:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs RCMS_TEST_REGISTRY_URL + RCMS_TEST_TENANT_URL (a migrated PG tenant)"]
async fn concurrent_create_child_assigns_distinct_sort_orders() {
    let h = setup().await;

    let parent = {
        let t = h.tenant().await;
        Page::create_root(&t, NewPage::new(h.type_id, "race-parent", "race-parent"))
            .await
            .expect("create parent")
    };

    let barrier = Arc::new(tokio::sync::Barrier::new(N));
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let h = &h;
        let parent = parent.clone();
        let barrier = Arc::clone(&barrier);
        // Borrow `h` across the spawn via a scoped clone of its parts.
        let pools = Arc::clone(&h.pools);
        let org = h.org.clone();
        let tenant_pool = h.tenant_pool.clone();
        let type_id = h.type_id;
        handles.push(tokio::spawn(async move {
            let conn = pools.acquire(&org).await.expect("acquire");
            let t = Tenant::for_test(org, conn, tenant_pool);
            barrier.wait().await;
            Page::create_child(
                &t,
                &parent,
                NewPage::new(type_id, format!("c{i}"), format!("c{i}")),
            )
            .await
            .expect("create_child")
            .sort_order
        }));
    }

    let mut orders = Vec::with_capacity(N);
    for hdl in handles {
        orders.push(hdl.await.expect("task panicked"));
    }
    assert_distinct(orders, "create_child");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs RCMS_TEST_REGISTRY_URL + RCMS_TEST_TENANT_URL (a migrated PG tenant)"]
async fn concurrent_create_root_assigns_distinct_sort_orders() {
    let h = setup().await;

    // Pre-create one root so the roots set is non-empty — `create_root`
    // locks the existing roots, so this exercises the lock (the empty
    // "first root" case is a documented narrow gap, not tested here).
    {
        let t = h.tenant().await;
        Page::create_root(&t, NewPage::new(h.type_id, "seed-root", "seed-root"))
            .await
            .expect("seed root");
    }

    let barrier = Arc::new(tokio::sync::Barrier::new(N));
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let pools = Arc::clone(&h.pools);
        let org = h.org.clone();
        let tenant_pool = h.tenant_pool.clone();
        let type_id = h.type_id;
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            let conn = pools.acquire(&org).await.expect("acquire");
            let t = Tenant::for_test(org, conn, tenant_pool);
            barrier.wait().await;
            Page::create_root(&t, NewPage::new(type_id, format!("r{i}"), format!("r{i}")))
                .await
                .expect("create_root")
                .sort_order
        }));
    }

    let mut orders = Vec::with_capacity(N);
    for hdl in handles {
        orders.push(hdl.await.expect("task panicked"));
    }
    assert_distinct(orders, "create_root");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs RCMS_TEST_REGISTRY_URL + RCMS_TEST_TENANT_URL (a migrated PG tenant)"]
async fn concurrent_move_to_same_parent_assigns_distinct_sort_orders() {
    let h = setup().await;

    // A target parent + N source roots created sequentially (no
    // contention), then all moved under the target at once.
    let parent = {
        let t = h.tenant().await;
        Page::create_root(&t, NewPage::new(h.type_id, "move-target", "move-target"))
            .await
            .expect("create target")
    };
    let mut sources = Vec::with_capacity(N);
    for i in 0..N {
        let t = h.tenant().await;
        sources.push(
            Page::create_root(
                &t,
                NewPage::new(h.type_id, format!("src{i}"), format!("src{i}")),
            )
            .await
            .expect("create source"),
        );
    }

    let barrier = Arc::new(tokio::sync::Barrier::new(N));
    let mut handles = Vec::with_capacity(N);
    for mut src in sources {
        let pools = Arc::clone(&h.pools);
        let org = h.org.clone();
        let tenant_pool = h.tenant_pool.clone();
        let parent = parent.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            let conn = pools.acquire(&org).await.expect("acquire");
            let t = Tenant::for_test(org, conn, tenant_pool);
            barrier.wait().await;
            src.move_to(&t, Some(&parent)).await.expect("move_to");
            src.sort_order
        }));
    }

    let mut orders = Vec::with_capacity(N);
    for hdl in handles {
        orders.push(hdl.await.expect("task panicked"));
    }
    assert_distinct(orders, "move_to");
}
