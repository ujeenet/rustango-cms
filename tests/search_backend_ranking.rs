//! An installed search backend (Elasticsearch) ranks public search. Before,
//! only the pages list asked it; `/api/v2/search/` went straight to the
//! database, so installing Elasticsearch changed nothing there.
//!
//! Its own test binary: the backend is a process-wide, set-once global.
#![cfg(feature = "sqlite")]

mod common;

use std::sync::Arc;

use rustango_cms::search::{ranked_public_page_ids, set_backend, SearchBackend, SearchDoc};

struct Fixed;

#[async_trait::async_trait]
impl SearchBackend for Fixed {
    async fn search(&self, tenant_slug: &str, query: &str, _limit: i64) -> Vec<i64> {
        assert_eq!(tenant_slug, "acme");
        assert_eq!(query, "glaze");
        vec![7, 3]
    }
    async fn index(&self, _tenant_slug: &str, _doc: &SearchDoc) {}
    async fn delete(&self, _tenant_slug: &str, _page_id: i64) {}
}

#[tokio::test]
async fn the_installed_backend_ranks_public_search() {
    let f = common::fixture().await;
    set_backend(Arc::new(Fixed));
    assert_eq!(
        ranked_public_page_ids(&f.pool, "acme", "glaze", 10).await,
        Some(vec![7, 3]),
    );
}
