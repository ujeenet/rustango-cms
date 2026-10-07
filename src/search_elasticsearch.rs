//! Elasticsearch full-text [`SearchBackend`].
//!
//! Opt-in (the `search_elasticsearch` cargo feature pulls the
//! framework's reqwest-based HTTP client). When installed via
//! [`crate::search::set_backend`], search queries + page indexing route
//! to Elasticsearch instead of the built-in Postgres FTS path.
//!
//! One index per tenant (`<prefix><tenant_slug>`); each page is a
//! document keyed by its id. Search uses `multi_match` with field
//! boosting (title ≫ seo_title ≫ description) + `fuzziness: AUTO`, so
//! results are relevance-ranked and typo-tolerant. Indexing / deletion
//! are best-effort (logged on failure) — a search-index hiccup must
//! never block a content save.

use std::sync::Arc;

use rustango::http_client::HttpClient;

use crate::search::{SearchBackend, SearchDoc};

/// Elasticsearch-backed search. Build with [`Self::new`] and install
/// via [`crate::search::set_backend`].
pub struct ElasticsearchBackend {
    http: HttpClient,
    /// Base URL, no trailing slash — e.g. `http://localhost:9200`.
    base_url: String,
    /// Index-name prefix; the per-tenant index is `<prefix><slug>`.
    index_prefix: String,
}

impl ElasticsearchBackend {
    /// Connect to the cluster at `base_url` (trailing slash trimmed),
    /// naming per-tenant indices `<index_prefix><tenant_slug>`.
    ///
    /// # Errors
    /// Fails only if the underlying HTTP client can't be built.
    pub fn new(
        base_url: impl Into<String>,
        index_prefix: impl Into<String>,
    ) -> Result<Self, rustango::http_client::HttpError> {
        let http = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            index_prefix: index_prefix.into(),
        })
    }

    fn index_name(&self, tenant_slug: &str) -> String {
        format!("{}{}", self.index_prefix, sanitize_index(tenant_slug))
    }

    /// Bulk-(re)index every published page for `tenant_slug`. A one-shot
    /// way to populate the index (the auto-on-save path is the
    /// search-index background job).
    ///
    /// # Errors
    /// Driver / query failures fetching the pages.
    pub async fn reindex_all(
        &self,
        pool: &rustango::sql::Pool,
        tenant_slug: &str,
    ) -> Result<usize, rustango::sql::ExecError> {
        use rustango::core::Column as _;
        use rustango::sql::FetcherPool as _;
        // #556 — index public statuses (published + archived); archived
        // pages stay findable. Expired/draft/scheduled are excluded.
        let pages: Vec<crate::page::Page> = crate::page::Page::objects()
            .where_(crate::page::Page::status.is_in([
                crate::page::PageStatus::Published.as_str().to_owned(),
                crate::page::PageStatus::Archived.as_str().to_owned(),
            ]))
            .fetch(pool)
            .await?;
        let mut n = 0;
        for p in &pages {
            let Some(page_id) = p.id.get().copied() else {
                continue;
            };
            self.index(
                tenant_slug,
                &SearchDoc {
                    page_id,
                    title: p.title.clone(),
                    seo_title: p.seo_title.clone(),
                    seo_description: p.seo_description.clone(),
                    url_path: p.url_path.clone(),
                    status: p.status.clone(),
                },
            )
            .await;
            n += 1;
        }
        Ok(n)
    }
}

/// ES index names must be lowercase + can't contain certain chars;
/// tenant slugs are already slug-shaped, but normalise defensively.
fn sanitize_index(slug: &str) -> String {
    slug.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[async_trait::async_trait]
impl SearchBackend for ElasticsearchBackend {
    async fn search(&self, tenant_slug: &str, query: &str, limit: i64) -> Vec<i64> {
        let url = format!("{}/{}/_search", self.base_url, self.index_name(tenant_slug));
        let body = serde_json::json!({
            "size": limit.max(0),
            "_source": false,
            "query": {
                "multi_match": {
                    "query": query,
                    "fields": ["title^3", "seo_title^2", "seo_description"],
                    "fuzziness": "AUTO"
                }
            }
        });
        let resp = match self.http.post(url.as_str()).json(&body) {
            Ok(req) => req.send().await,
            Err(e) => Err(e),
        };
        let json: serde_json::Value = match resp {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(target: "rustango_cms::search", error = %e, "ES search: decode failed");
                    return Vec::new();
                }
            },
            Err(e) => {
                tracing::warn!(target: "rustango_cms::search", error = %e, "ES search request failed");
                return Vec::new();
            }
        };
        parse_hit_ids(&json)
    }

    async fn index(&self, tenant_slug: &str, doc: &SearchDoc) {
        let url = format!(
            "{}/{}/_doc/{}",
            self.base_url,
            self.index_name(tenant_slug),
            doc.page_id
        );
        let send = match self.http.put(url.as_str()).json(doc) {
            Ok(req) => req.send().await,
            Err(e) => Err(e),
        };
        if let Err(e) = send {
            tracing::warn!(target: "rustango_cms::search", page_id = doc.page_id, error = %e, "ES index failed (best-effort)");
        }
    }

    async fn delete(&self, tenant_slug: &str, page_id: i64) {
        let url = format!(
            "{}/{}/_doc/{}",
            self.base_url,
            self.index_name(tenant_slug),
            page_id
        );
        if let Err(e) = self.http.delete(url.as_str()).send().await {
            tracing::warn!(target: "rustango_cms::search", page_id, error = %e, "ES delete failed (best-effort)");
        }
    }
}

/// Pull `hits.hits[]._id` (the page ids) out of an ES `_search`
/// response, in the order ES returned them (relevance). Pure — unit
/// tested without a cluster.
fn parse_hit_ids(json: &serde_json::Value) -> Vec<i64> {
    json.get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(|h| h.as_array())
        .map(|hits| {
            hits.iter()
                .filter_map(|h| h.get("_id"))
                .filter_map(|id| {
                    id.as_str()
                        .and_then(|s| s.parse::<i64>().ok())
                        .or_else(|| id.as_i64())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Wrap an [`ElasticsearchBackend`] in the `Arc<dyn SearchBackend>` the
/// registry expects. Convenience for `search::set_backend(...)`.
#[must_use]
pub fn boxed(backend: ElasticsearchBackend) -> Arc<dyn SearchBackend> {
    Arc::new(backend)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hit_ids_in_order() {
        let json = serde_json::json!({
            "hits": { "hits": [
                { "_id": "42", "_score": 9.1 },
                { "_id": "7",  "_score": 3.2 }
            ]}
        });
        assert_eq!(parse_hit_ids(&json), vec![42, 7]);
        // Missing/empty hits → empty.
        assert!(parse_hit_ids(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn sanitizes_index_names() {
        assert_eq!(sanitize_index("Acme-Co"), "acme-co");
        assert_eq!(sanitize_index("a.b/c"), "a_b_c");
    }

    /// Real-Elasticsearch round-trip. Gated on `RCMS_TEST_ES_URL`
    /// (e.g. `http://localhost:9201`); skips cleanly otherwise.
    /// Indexes 3 docs, refreshes, searches, asserts ranking + fuzzy
    /// match + filtering, then drops the throwaway index.
    #[tokio::test]
    async fn es_round_trip_ranks_and_filters() {
        let Ok(url) = std::env::var("RCMS_TEST_ES_URL") else {
            eprintln!("skip es_round_trip: set RCMS_TEST_ES_URL");
            return;
        };
        let es = ElasticsearchBackend::new(&url, "rcmstest_").unwrap();
        let slug = "esrt";
        let index = es.index_name(slug);
        // Clean slate.
        let _ = es
            .http
            .delete(format!("{}/{index}", es.base_url).as_str())
            .send()
            .await;

        for (id, title, desc) in [
            (1_i64, "Rust web framework guide", ""),
            (2, "Cooking with cast iron", "a rust-free skillet"),
            (3, "Knitting patterns", "wool and yarn"),
        ] {
            es.index(
                slug,
                &SearchDoc {
                    page_id: id,
                    title: title.to_owned(),
                    seo_title: String::new(),
                    seo_description: desc.to_owned(),
                    url_path: format!("/{id}"),
                    status: "published".to_owned(),
                },
            )
            .await;
        }
        // Make the writes searchable.
        let _ = es
            .http
            .post(format!("{}/{index}/_refresh", es.base_url).as_str())
            .send()
            .await;

        let ids = es.search(slug, "rust", 10).await;
        // Fuzzy typo still matches the title.
        let fuzzy = es.search(slug, "rüst", 10).await;
        es.delete(slug, 1).await;
        let after_delete = {
            let _ = es
                .http
                .post(format!("{}/{index}/_refresh", es.base_url).as_str())
                .send()
                .await;
            es.search(slug, "rust", 10).await
        };
        // Cleanup.
        let _ = es
            .http
            .delete(format!("{}/{index}", es.base_url).as_str())
            .send()
            .await;

        assert!(
            ids.contains(&1) && ids.contains(&2),
            "both rust docs: {ids:?}"
        );
        assert!(!ids.contains(&3), "knitting excluded: {ids:?}");
        assert_eq!(ids.first(), Some(&1), "title match ranks first: {ids:?}");
        assert!(!fuzzy.is_empty(), "fuzzy 'rüst' matches: {fuzzy:?}");
        assert!(
            !after_delete.contains(&1),
            "deleted doc gone: {after_delete:?}"
        );
    }
}
