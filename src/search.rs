//! Database full-text search backend.
//!
//! Admin + API search has been client-side `LIKE` substring matching —
//! no relevance ranking, poor scaling. This adds a **Postgres
//! full-text** path: `to_tsvector` / `ts_rank` with `websearch_to_tsquery`
//! (which safely parses visitor query syntax — quotes, `OR`, `-term`).
//!
//! **Dialect-gated.** [`search_page_ids`] returns `Some(ranked_ids)`
//! only when the pool is Postgres ([`rustango::sql::Pool::as_postgres`]);
//! on SQLite / MySQL (or a PG query error) it returns `None`, and the
//! caller falls back to the existing substring filter. So this is purely
//! additive — non-PG deployments are unchanged.
//!
//! v1 indexes the canonical text (`title` + `seo_title` +
//! `seo_description`) on the fly — no stored column required, correct
//! without a migration. A `GIN` index + extension-body indexing (via
//! the task queue) are follow-ups for scale.

// ---- pluggable backend (Postgres FTS is the built-in; Elasticsearch
//      is an opt-in external backend) -------------------------------

use std::sync::{Arc, OnceLock};

/// One page's searchable text, handed to an external [`SearchBackend`]
/// to (re)index. `page_id` is the document id.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchDoc {
    pub page_id: i64,
    pub title: String,
    pub seo_title: String,
    pub seo_description: String,
    pub url_path: String,
    /// The page's lifecycle status (`published` / `archived`) so a
    /// backend can badge archived hits. Defaults to `published` for docs
    /// indexed before this field existed.
    #[serde(default = "default_published_status")]
    pub status: String,
}

fn default_published_status() -> String {
    crate::page::PageStatus::Published.as_str().to_owned()
}

/// An external full-text backend (Elasticsearch, …). When one is
/// installed via [`set_backend`], it takes priority over the built-in
/// Postgres path: search goes to the backend, and pages are indexed /
/// removed through it. Object-safe so it can live behind `Arc<dyn …>`.
#[async_trait::async_trait]
pub trait SearchBackend: Send + Sync + 'static {
    /// Page ids matching `query` for `tenant_slug`, ranked best-first.
    async fn search(&self, tenant_slug: &str, query: &str, limit: i64) -> Vec<i64>;
    /// Upsert one page document.
    async fn index(&self, tenant_slug: &str, doc: &SearchDoc);
    /// Remove a page document (on unpublish / delete).
    async fn delete(&self, tenant_slug: &str, page_id: i64);
}

static BACKEND: OnceLock<Arc<dyn SearchBackend>> = OnceLock::new();

/// Install an external search backend (e.g. Elasticsearch). Call once
/// at startup. Without it, search uses the built-in Postgres FTS path
/// ([`search_page_ids`]) / substring fallback.
pub fn set_backend(backend: Arc<dyn SearchBackend>) {
    // Set once; a second install would be silently lost otherwise (#697).
    if BACKEND.set(backend).is_err() {
        tracing::error!(target: "rustango_cms", "set_backend: a search backend is already installed; this one is ignored");
    }
}

/// The installed external backend, if any.
#[must_use]
pub fn backend() -> Option<&'static Arc<dyn SearchBackend>> {
    BACKEND.get()
}

/// The `to_tsvector` document expression: the page's canonical text,
/// null-safe. Shared by the live query + the ranking test so they can't
/// drift.
#[cfg(feature = "postgres")]
const DOC_SQL: &str =
    "coalesce(title,'') || ' ' || coalesce(seo_title,'') || ' ' || coalesce(seo_description,'')";

/// Page ids matching `query`, **ranked by relevance** (best first),
/// capped at `limit`. `Some` only on Postgres; `None` on any other
/// dialect or on a query error → the caller uses its substring fallback.
///
/// `websearch_to_tsquery` parses `query` as a user search string, so no
/// escaping is needed and malformed input can't error the parse.
pub async fn search_page_ids(
    pool: &rustango::sql::Pool,
    query: &str,
    limit: i64,
) -> Option<Vec<i64>> {
    fts_page_ids(pool, query, limit, true).await
}

/// Like [`search_page_ids`] but spans **every status** (drafts /
/// scheduled / expired too) — for the admin explorer's search, which
/// must surface unpublished pages. Postgres-only (the public index is
/// public-status-only); `None` elsewhere → admin keeps its substring path.
pub async fn search_page_ids_all_statuses(
    pool: &rustango::sql::Pool,
    query: &str,
    limit: i64,
) -> Option<Vec<i64>> {
    fts_page_ids(pool, query, limit, false).await
}

async fn fts_page_ids(
    pool: &rustango::sql::Pool,
    query: &str,
    limit: i64,
    public_only: bool,
) -> Option<Vec<i64>> {
    // Postgres-only path. `as_postgres()` itself only exists under the
    // `postgres` feature, so the whole body is gated — sqlite / mysql
    // builds compile to the `None` fallback (caller uses substring).
    #[cfg(feature = "postgres")]
    {
        let pg = pool.as_postgres()?;
        // #556 — public search returns published + archived (archived
        // content stays findable); expired/draft/scheduled stay out.
        let status_clause = if public_only {
            "status IN ('published','archived') AND "
        } else {
            ""
        };
        let sql = format!(
            "SELECT id FROM cms_page, websearch_to_tsquery('english', $1) AS q \
             WHERE {status_clause}to_tsvector('english', {DOC_SQL}) @@ q \
             ORDER BY ts_rank(to_tsvector('english', {DOC_SQL}), q) DESC, id ASC \
             LIMIT $2"
        );
        match rustango::sql::sqlx::query_scalar::<_, i64>(&sql)
            .bind(query)
            .bind(limit)
            .fetch_all(pg)
            .await
        {
            Ok(ids) => Some(ids),
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::search",
                    error = %e,
                    "postgres FTS query failed; falling back to substring search",
                );
                None
            }
        }
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = (pool, query, limit, public_only);
        None
    }
}

/// Fuzzy "did you mean?" suggestion: the published page title
/// most trigram-similar to `query` above `threshold` (0.0–1.0), or
/// `None`. Postgres + the `pg_trgm` extension only — `None` on any
/// other dialect, when `pg_trgm` isn't installed, on a query error, or
/// when the closest title is just the query itself.
///
/// Callers use it when a search returned no hits, to offer a
/// spelling-correction prompt.
pub async fn did_you_mean(
    pool: &rustango::sql::Pool,
    query: &str,
    threshold: f32,
) -> Option<String> {
    #[cfg(feature = "postgres")]
    {
        let pg = pool.as_postgres()?;
        // #556 — suggest from public pages (published + archived).
        let sql = "SELECT title FROM cms_page \
                   WHERE status IN ('published','archived') AND similarity(title, $1) > $2 \
                   ORDER BY similarity(title, $1) DESC, title ASC \
                   LIMIT 1";
        let title: Option<String> = rustango::sql::sqlx::query_scalar(sql)
            .bind(query)
            .bind(threshold)
            .fetch_optional(pg)
            .await
            .ok()
            .flatten();
        title.filter(|t| !t.eq_ignore_ascii_case(query.trim()))
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = (pool, query, threshold);
        None
    }
}

/// Name of the expression GIN index backing [`search_page_ids`].
#[cfg(feature = "postgres")]
const FTS_INDEX: &str = "cms_page_fts_idx";

/// Create the GIN index that makes [`search_page_ids`] scale — an
/// expression index over the exact `to_tsvector(...)` the query uses,
/// so the planner can use it instead of a seq scan. Idempotent
/// (`IF NOT EXISTS`); Postgres-only (no-op + `Ok` on other dialects).
/// [`crate::seed::ensure_seeded`] runs it for every tenant at boot.
///
/// # Errors
/// The `CREATE INDEX` failing (e.g. insufficient privileges).
pub async fn ensure_pg_fts_index(
    pool: &rustango::sql::Pool,
) -> Result<(), rustango::sql::sqlx::Error> {
    #[cfg(feature = "postgres")]
    {
        let Some(pg) = pool.as_postgres() else {
            return Ok(());
        };
        // Same expression as the query's `to_tsvector(...)`, so it matches.
        let sql = format!(
            "CREATE INDEX IF NOT EXISTS {FTS_INDEX} ON cms_page \
             USING GIN (to_tsvector('english', {DOC_SQL}))"
        );
        rustango::sql::sqlx::query(&sql)
            .execute(pg)
            .await
            .map(|_| ())
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = pool;
        Ok(())
    }
}

#[cfg(all(test, feature = "postgres"))]
mod tests {
    use rustango::sql::sqlx;

    /// A Postgres schema of its own for one test, holding a
    /// throwaway `cms_page` with the columns the search queries read.
    ///
    /// These tests used to share `RCMS_TEST_TENANT_URL` — the variable
    /// other docs point at a real demo database — and all write one
    /// `cms_page` while running in parallel. They now take their own
    /// `RCMS_TEST_PG_URL`, run only with `--ignored`, and never touch a
    /// table outside the scratch schema.
    struct Scratch {
        pool: rustango::sql::Pool,
        admin: sqlx::PgPool,
        schema: String,
    }

    impl Scratch {
        async fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let url = std::env::var("RCMS_TEST_PG_URL")
                .expect("set RCMS_TEST_PG_URL to a throwaway Postgres database to run these");
            let schema = format!(
                "rcms_test_{}_{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            );
            let admin = sqlx::PgPool::connect(&url).await.expect("connect");
            sqlx::query(&format!("CREATE SCHEMA {schema}"))
                .execute(&admin)
                .await
                .expect("create schema");
            // `public` stays on the path for extensions such as pg_trgm.
            let sep = if url.contains('?') { '&' } else { '?' };
            let scoped = format!("{url}{sep}options=-c%20search_path%3D{schema},public");
            let pool = rustango::sql::Pool::connect(&scoped).await.expect("connect scoped");
            sqlx::query(
                "CREATE TABLE cms_page (\
                 id BIGINT PRIMARY KEY, title TEXT, seo_title TEXT, \
                 seo_description TEXT, status TEXT)",
            )
            .execute(pool.as_postgres().expect("postgres pool"))
            .await
            .expect("create cms_page");
            Self { pool, admin, schema }
        }

        fn pg(&self) -> &sqlx::PgPool {
            self.pool.as_postgres().expect("postgres pool")
        }

        async fn drop(self) {
            sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
                .execute(&self.admin)
                .await
                .expect("drop schema");
        }
    }

    /// Real-Postgres ranking check. Runs with `--ignored` against
    /// `RCMS_TEST_PG_URL`, in a [`Scratch`] schema.
    ///
    /// Verifies the live `search_page_ids` query end-to-end against a
    /// throwaway `cms_page` table: a title-match outranks a
    /// description-only match, and a non-match is excluded.
    #[tokio::test]
    #[ignore = "needs RCMS_TEST_PG_URL"]
    async fn pg_fts_ranks_and_filters() {
        let scratch = Scratch::new().await;
        let (pool, pg) = (&scratch.pool, scratch.pg());

        for (id, title, desc) in [
            (9001_i64, "Rust web framework guide", ""),
            (
                9002,
                "Cooking with cast iron",
                "a rust-free skillet, mostly",
            ),
            (9003, "Knitting patterns", "wool and yarn"),
        ] {
            sqlx::query(
                "INSERT INTO cms_page (id,title,seo_title,seo_description,status) \
                 VALUES ($1,$2,'',$3,'published')",
            )
            .bind(id)
            .bind(title)
            .bind(desc)
            .execute(pg)
            .await
            .expect("insert");
        }

        let ids = super::search_page_ids(pool, "rust", 10)
            .await
            .expect("Some on postgres");

        // Both "rust" docs match; the title hit (9001) outranks the
        // description-only hit (9002); the knitting page (9003) is out.
        scratch.drop().await;
        assert!(ids.contains(&9001), "title match present: {ids:?}");
        assert!(ids.contains(&9002), "description match present: {ids:?}");
        assert!(!ids.contains(&9003), "non-match excluded: {ids:?}");
        let pos = |id| ids.iter().position(|x| *x == id);
        assert!(pos(9001) < pos(9002), "title match ranks first: {ids:?}");
    }

    /// Real-Postgres `pg_trgm` "did you mean?" check. Same gating
    /// as [`pg_fts_ranks_and_filters`]; also needs the `pg_trgm`
    /// extension (created in the test DB).
    #[tokio::test]
    #[ignore = "needs RCMS_TEST_PG_URL"]
    async fn pg_did_you_mean_suggests_near_title() {
        let scratch = Scratch::new().await;
        let (pool, pg) = (&scratch.pool, scratch.pg());
        sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_trgm")
            .execute(pg)
            .await
            .ok();
        sqlx::query(
            "INSERT INTO cms_page (id,title,seo_title,seo_description,status) \
             VALUES (9101,'Accessibility checklist','','','published')",
        )
        .execute(pg)
        .await
        .expect("insert");

        // Typo "accessibilty" → suggest the real title; nonsense → None.
        let hit = super::did_you_mean(pool, "accessibilty checklist", 0.3).await;
        let none = super::did_you_mean(pool, "zxqw nonsense", 0.3).await;

        scratch.drop().await;
        assert_eq!(
            hit.as_deref(),
            Some("Accessibility checklist"),
            "got {hit:?}"
        );
        assert_eq!(none, None, "no near match → no suggestion");
    }

    /// Real-Postgres GIN index check: the expression index is
    /// created, idempotently.
    #[tokio::test]
    #[ignore = "needs RCMS_TEST_PG_URL"]
    async fn pg_fts_index_created_idempotently() {
        let scratch = Scratch::new().await;
        let (pool, pg) = (&scratch.pool, scratch.pg());

        super::ensure_pg_fts_index(pool)
            .await
            .expect("create index");
        // Second call is a no-op (IF NOT EXISTS) — must not error.
        super::ensure_pg_fts_index(pool).await.expect("idempotent");

        let name: Option<String> = sqlx::query_scalar(
            "SELECT indexname FROM pg_indexes WHERE schemaname = $1 AND indexname = $2",
        )
        .bind(&scratch.schema)
        .bind(super::FTS_INDEX)
        .fetch_optional(pg)
        .await
        .expect("query pg_indexes");
        scratch.drop().await;
        assert_eq!(name.as_deref(), Some(super::FTS_INDEX));
    }

    /// Real-Postgres: the all-status variant surfaces drafts that the
    /// published-only public query excludes (admin search).
    #[tokio::test]
    #[ignore = "needs RCMS_TEST_PG_URL"]
    async fn pg_all_statuses_includes_drafts() {
        let scratch = Scratch::new().await;
        let (pool, pg) = (&scratch.pool, scratch.pg());
        sqlx::query(
            "INSERT INTO cms_page (id,title,seo_title,seo_description,status) VALUES \
             (9201,'Draft rust note','','','draft'),\
             (9202,'Published rust post','','','published')",
        )
        .execute(pg)
        .await
        .expect("insert");

        let published = super::search_page_ids(pool, "rust", 10).await.unwrap();
        let all = super::search_page_ids_all_statuses(pool, "rust", 10)
            .await
            .unwrap();

        scratch.drop().await;
        assert!(
            published.contains(&9202),
            "published query has the published post"
        );
        assert!(
            !published.contains(&9201),
            "published query excludes the draft"
        );
        assert!(
            all.contains(&9201) && all.contains(&9202),
            "all-status has both: {all:?}"
        );
    }
}
