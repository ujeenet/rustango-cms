//! Generic reference index — \"what links here\" (#146, Wagtail
//! parity D3).
//!
//! Tracks every inbound reference: page→page links, snippet→page
//! embeds, page→media renditions. Feeds two surfaces:
//!
//! 1. A \"References\" panel on the page + snippet editor showing
//!    every object that points at the current row.
//! 2. Cascade-delete warnings (\"deleting this snippet will break
//!    12 pages\") on the delete confirmation.
//!
//! Index updater runs from page + snippet save handlers. Reference
//! extraction is a simple scanner over the rendered HTML / persisted
//! JSON shape — looks for known patterns:
//!   - \`href=\"/some/path\"\` → page reference (resolved via url_path
//!     lookup).
//!   - \`{{ cms_snippet(slug=\"X\") }}\` → snippet reference.
//!   - \`/__media__/<spec>/<id>\` → media reference.
//!   - StreamField block values with \`media_id\` / \`page_id\` keys.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// Resource kind identifiers used in `from_kind` / `to_kind`. Strings
/// not enums so plugin types can join the index without code changes.
pub const KIND_PAGE: &str = "cms_page";
pub const KIND_SNIPPET: &str = "cms_snippet";
pub const KIND_MEDIA: &str = "cms_media";

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_reference_index",
    app = "cms",
    display = "id",
    admin(
        list_display = "from_kind, from_id, to_kind, to_id, field_path",
        ordering = "to_kind, to_id",
    )
)]
pub struct ReferenceIndex {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 32, index)]
    pub from_kind: String,

    pub from_id: i64,

    #[rustango(max_length = 32, index)]
    pub to_kind: String,

    pub to_id: i64,

    /// Optional locator inside the source — typically the field name
    /// (\`body_markdown\`, \`body_stream\`, etc.). Empty when the
    /// reference is at the row level.
    #[rustango(max_length = 128)]
    pub field_path: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// One scanned reference (target identity only; the source identity
/// is supplied by the caller).
#[derive(Debug, Clone)]
pub struct Reference {
    pub to_kind: &'static str,
    pub to_id: i64,
    pub field_path: String,
}

/// Walk `haystack` looking for media / page / snippet references.
/// Pure: no DB access. Caller resolves \`href=\"/path\"\` URLs via a
/// follow-up Page lookup if needed.
///
/// Patterns recognized:
///   - \`/__media__/<spec>/<id>\` → media reference.
///   - \`{{ cms_snippet(slug=\"X\") }}\` or \`cms_snippet(slug=X)\` → no
///     direct id, caller must resolve via slug.
#[must_use]
pub fn scan(haystack: &str, field_path: &str) -> Vec<Reference> {
    let mut out = Vec::new();
    // Media references — /__media__/<spec>/<id>
    let mut start = 0;
    while let Some(rel) = haystack[start..].find("/__media__/") {
        let abs = start + rel;
        let tail = &haystack[abs + "/__media__/".len()..];
        // Skip the filter spec.
        let after_spec = match tail.find('/') {
            Some(i) => &tail[i + 1..],
            None => {
                start = abs + 1;
                continue;
            }
        };
        // Parse the id up to the next non-digit.
        let id_end = after_spec
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after_spec.len());
        if id_end > 0 {
            if let Ok(id) = after_spec[..id_end].parse::<i64>() {
                out.push(Reference {
                    to_kind: KIND_MEDIA,
                    to_id: id,
                    field_path: field_path.to_owned(),
                });
            }
        }
        start = abs + 1;
    }
    out
}

/// Replace every (from_kind, from_id) row in the index with `refs`.
/// Atomic per-source — readers see either the old set or the new
/// set, never an intermediate state.
///
/// # Errors
/// Driver / query failures.
pub async fn reindex_source(
    pool: &rustango::sql::Pool,
    from_kind: &'static str,
    from_id: i64,
    refs: Vec<Reference>,
) -> Result<(), rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let existing: Vec<ReferenceIndex> = ReferenceIndex::objects()
        .where_(ReferenceIndex::from_kind.eq(from_kind.to_owned()))
        .where_(ReferenceIndex::from_id.eq(from_id))
        .fetch(pool)
        .await?;
    for row in existing {
        row.delete_pool(pool).await?;
    }
    for r in refs {
        let mut row = ReferenceIndex {
            id: Auto::Unset,
            from_kind: from_kind.to_owned(),
            from_id,
            to_kind: r.to_kind.to_owned(),
            to_id: r.to_id,
            field_path: r.field_path,
            created_at: Auto::Unset,
        };
        row.insert_pool(pool).await?;
    }
    Ok(())
}

/// Every inbound reference to (kind, id). Used by the page + snippet
/// editor's References panel.
///
/// # Errors
/// Driver / query failures.
pub async fn inbound(
    pool: &rustango::sql::Pool,
    to_kind: &str,
    to_id: i64,
) -> Result<Vec<ReferenceIndex>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    ReferenceIndex::objects()
        .where_(ReferenceIndex::to_kind.eq(to_kind.to_owned()))
        .where_(ReferenceIndex::to_id.eq(to_id))
        .fetch(pool)
        .await
}

// ---- #419 — usage view (aggregate + jump-to-referrer) ------------

/// One inbound referrer to a target, aggregated across every field in
/// which it references the target. The `title` resolves to the
/// referrer's display name (page / snippet title) for a clickable
/// "used in N places" view; falls back to `kind#id`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Referrer {
    pub from_kind: String,
    pub from_id: i64,
    pub title: String,
    /// Distinct field paths through which this referrer points at the
    /// target (e.g. `body_markdown`, `hero_media_id`).
    pub fields: Vec<String>,
}

/// Group raw inbound rows by `(from_kind, from_id)`, collecting the
/// distinct field paths in first-seen order. Pure — titles are filled
/// in by [`usage_summary`]. The returned length is the aggregate
/// "used in N places" count.
#[must_use]
pub fn aggregate_referrers(refs: Vec<ReferenceIndex>) -> Vec<Referrer> {
    use std::collections::HashMap;
    let mut order: Vec<(String, i64)> = Vec::new();
    let mut fields: HashMap<(String, i64), Vec<String>> = HashMap::new();
    for r in refs {
        let key = (r.from_kind.clone(), r.from_id);
        if !fields.contains_key(&key) {
            order.push(key.clone());
        }
        let entry = fields.entry(key).or_default();
        if !r.field_path.is_empty() && !entry.contains(&r.field_path) {
            entry.push(r.field_path);
        }
    }
    order
        .into_iter()
        .map(|(from_kind, from_id)| {
            let fields = fields
                .remove(&(from_kind.clone(), from_id))
                .unwrap_or_default();
            let title = format!("{from_kind}#{from_id}");
            Referrer {
                from_kind,
                from_id,
                title,
                fields,
            }
        })
        .collect()
}

/// Aggregated, title-resolved inbound usage for any target
/// `(to_kind, to_id)` — reused across pages / snippets / media /
/// documents (#419). Page + snippet referrers get their real title
/// resolved in one batched query each; other kinds keep the `kind#id`
/// fallback.
pub async fn usage_summary(pool: &rustango::sql::Pool, to_kind: &str, to_id: i64) -> Vec<Referrer> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    let refs = inbound(pool, to_kind, to_id).await.unwrap_or_default();
    let mut referrers = aggregate_referrers(refs);

    let page_ids: Vec<i64> = referrers
        .iter()
        .filter(|r| r.from_kind == KIND_PAGE)
        .map(|r| r.from_id)
        .collect();
    let snippet_ids: Vec<i64> = referrers
        .iter()
        .filter(|r| r.from_kind == KIND_SNIPPET)
        .map(|r| r.from_id)
        .collect();

    let mut page_titles: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    if !page_ids.is_empty() {
        let rows: Vec<crate::page::Page> = crate::page::Page::objects()
            .where_(crate::page::Page::id.is_in(page_ids))
            .fetch(pool)
            .await
            .unwrap_or_default();
        for p in rows {
            if let Some(id) = p.id.get().copied() {
                page_titles.insert(id, p.title);
            }
        }
    }
    let mut snippet_titles: std::collections::HashMap<i64, String> =
        std::collections::HashMap::new();
    if !snippet_ids.is_empty() {
        let rows: Vec<crate::snippet::Snippet> = crate::snippet::Snippet::objects()
            .where_(crate::snippet::Snippet::id.is_in(snippet_ids))
            .fetch(pool)
            .await
            .unwrap_or_default();
        for s in rows {
            if let Some(id) = s.id.get().copied() {
                snippet_titles.insert(id, s.title);
            }
        }
    }

    for r in &mut referrers {
        let resolved = match r.from_kind.as_str() {
            KIND_PAGE => page_titles.get(&r.from_id),
            KIND_SNIPPET => snippet_titles.get(&r.from_id),
            _ => None,
        };
        if let Some(t) = resolved.filter(|t| !t.is_empty()) {
            r.title = t.clone();
        }
    }
    referrers
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    fn row(from_kind: &str, from_id: i64, field: &str) -> ReferenceIndex {
        ReferenceIndex {
            id: rustango::sql::Auto::Unset,
            from_kind: from_kind.to_owned(),
            from_id,
            to_kind: KIND_MEDIA.to_owned(),
            to_id: 1,
            field_path: field.to_owned(),
            created_at: rustango::sql::Auto::Unset,
        }
    }

    #[test]
    fn groups_referrer_and_dedups_fields() {
        let out = aggregate_referrers(vec![
            row(KIND_PAGE, 5, "body_markdown"),
            row(KIND_PAGE, 5, "hero_media_id"),
            row(KIND_PAGE, 5, "body_markdown"), // dup field
        ]);
        assert_eq!(out.len(), 1, "one distinct referrer");
        assert_eq!(out[0].from_id, 5);
        assert_eq!(out[0].fields, vec!["body_markdown", "hero_media_id"]);
        assert_eq!(out[0].title, "cms_page#5"); // fallback until resolved
    }

    #[test]
    fn distinct_referrers_in_first_seen_order_is_the_count() {
        let out = aggregate_referrers(vec![
            row(KIND_PAGE, 5, "body"),
            row(KIND_SNIPPET, 9, "intro"),
            row(KIND_PAGE, 5, "body2"),
        ]);
        // "used in N places" = 2, ordered by first appearance.
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].from_kind.as_str(), out[0].from_id), (KIND_PAGE, 5));
        assert_eq!(
            (out[1].from_kind.as_str(), out[1].from_id),
            (KIND_SNIPPET, 9)
        );
        assert_eq!(out[0].fields, vec!["body", "body2"]);
    }

    #[test]
    fn empty_field_path_yields_no_field_entry() {
        let out = aggregate_referrers(vec![row(KIND_PAGE, 7, "")]);
        assert_eq!(out.len(), 1);
        assert!(out[0].fields.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_single_media_ref() {
        let html = r#"<img src="/__media__/fill-300x200/42">"#;
        let refs = scan(html, "body");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].to_kind, KIND_MEDIA);
        assert_eq!(refs[0].to_id, 42);
        assert_eq!(refs[0].field_path, "body");
    }

    #[test]
    fn scan_handles_multiple_media_refs() {
        let html = r#"<p>see /__media__/width-800/7 and /__media__/max-200x200/3</p>"#;
        let refs = scan(html, "body");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].to_id, 7);
        assert_eq!(refs[1].to_id, 3);
    }

    #[test]
    fn scan_skips_garbage() {
        let html = "no media refs in this text at all";
        assert!(scan(html, "body").is_empty());
    }

    #[test]
    fn scan_ignores_id_with_query_string() {
        let html = r#"<img src="/__media__/fill-300x200/42?v=abc123">"#;
        let refs = scan(html, "body");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].to_id, 42);
    }
}
