//! Shared query helpers for the v2 API.
//!
//! Pagination caps, sparse-field selection, and the meta envelope.

use serde::{Deserialize, Deserializer, Serialize};

/// Hard cap on `?limit=` — matches Wagtail's `WAGTAILAPI_LIMIT_MAX`.
/// Beyond this, slow clients can DoS the cms with one request.
pub const MAX_LIMIT: usize = 100;
/// Default page size when `?limit=` is omitted.
pub const DEFAULT_LIMIT: usize = 20;

/// Deserialize an optional query parameter, treating an **empty value as
/// absent**.
///
/// `?root=` is what a client sends when it builds a URL from a template
/// and the value happens to be unset — `?root=&depth=&locale=` is an
/// entirely ordinary request. Plain `Option<i64>` rejects it with
/// `400 cannot parse integer from empty string`, which is both wrong and
/// hostile: the parameter is optional, and omitting its value is the
/// clearest way to say so.
///
/// A non-empty value that still doesn't parse remains a 400 — that is a
/// genuine client error, and silently coercing `?depth=abc` to the
/// default would hide it.
///
/// # Errors
/// Propagates the underlying parse failure for non-empty values.
pub fn empty_as_none<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = Option::<String>::deserialize(de)?;
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(s) => s.parse().map(Some).map_err(serde::de::Error::custom),
    }
}

/// Like [`empty_as_none`] for string parameters: `?locale=` means "not
/// specified", never the locale whose code is the empty string.
///
/// # Errors
/// Only the underlying deserializer's own failures.
pub fn empty_string_as_none<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(de)?;
    Ok(raw.filter(|s| !s.trim().is_empty()))
}

/// Common query parameters that work across every list endpoint.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    #[serde(default, deserialize_with = "empty_as_none")]
    pub limit: Option<usize>,
    #[serde(default, deserialize_with = "empty_as_none")]
    pub offset: Option<usize>,
    /// Comma-separated field allowlist. When set, only these fields
    /// appear in each row's content (meta is always preserved).
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub fields: Option<String>,
    /// Comma-separated ordering. Prefix `-` for descending. `random`
    /// shuffles client-side (cheap on bounded list sizes).
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub order: Option<String>,
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub search: Option<String>,
    /// `?updated_since=2026-01-02T15:04:05Z` — only rows touched at or
    /// after this instant.
    ///
    /// Without it a client polling for changes has to re-fetch and diff
    /// the whole collection, which is the only option the API offered.
    /// RFC 3339; anything else is a 400 rather than a silent full result,
    /// because quietly ignoring it would make a client believe nothing
    /// had changed.
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub updated_since: Option<String>,
}

/// Parse `?updated_since=` as RFC 3339.
///
/// # Errors
/// A human-readable message when the value is not a valid timestamp.
pub fn parse_updated_since(
    q: &ListQuery,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
    let Some(raw) = q.updated_since.as_deref() else {
        return Ok(None);
    };
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| Some(dt.with_timezone(&chrono::Utc)))
        .map_err(|e| format!("`updated_since` must be an RFC 3339 timestamp: {e}"))
}

/// The query a *detail* endpoint accepts.
///
/// Sparse selection used to be list-only, so a client fetching one row
/// had no way to trim the payload — and on a page that meant pulling
/// `extension`, `builder` and the whole `children` array to read a title.
#[derive(Debug, Default, Deserialize)]
pub struct DetailFields {
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub fields: Option<String>,
}

impl DetailFields {
    /// The parsed allowlist, or `None` for "everything".
    #[must_use]
    pub fn keep(&self) -> Option<std::collections::HashSet<String>> {
        self.fields.as_ref().map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
    }
}

/// Clamp a requested page size into `1..=MAX_LIMIT`, defaulting when
/// absent. Shared so every list endpoint agrees on the bounds — the
/// menus list previously re-derived them and could have drifted.
#[must_use]
pub fn clamp_limit(requested: Option<usize>) -> usize {
    clamp_page_size(requested, DEFAULT_LIMIT, MAX_LIMIT)
}

/// The page-size rule, for a surface with its own bounds (the MCP tools
/// allow larger pages than the public API): `default` when absent, else
/// clamped into `1..=max`.
#[must_use]
pub fn clamp_page_size(requested: Option<usize>, default: usize, max: usize) -> usize {
    requested.unwrap_or(default).clamp(1, max)
}

/// Clamped `(limit, offset)` from a [`ListQuery`].
#[must_use]
pub fn paginate(q: &ListQuery) -> (usize, usize) {
    (clamp_limit(q.limit), q.offset.unwrap_or(0))
}

/// Parsed sparse-field selector. `None` means "no selection — return
/// every field"; `Some(set)` means "only these top-level keys".
#[must_use]
pub fn field_set(q: &ListQuery) -> Option<std::collections::HashSet<String>> {
    q.fields.as_ref().map(|raw| {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

/// Apply a sparse-field filter in place. `meta` and `id` are always
/// preserved so consumers can still walk the response.
pub fn apply_fields(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    keep: Option<&std::collections::HashSet<String>>,
) {
    let Some(keep) = keep else {
        return;
    };
    obj.retain(|k, _| k == "id" || k == "meta" || keep.contains(k));
}

/// `(direction, field)` parsed from one `order=` segment.
#[derive(Debug, Clone)]
pub struct OrderSpec {
    pub field: String,
    pub descending: bool,
    pub random: bool,
}

/// Parse `?order=` into a vector of [`OrderSpec`]s in declaration
/// order. Empty input → empty vec (caller falls back to its default).
#[must_use]
pub fn parse_order(q: &ListQuery) -> Vec<OrderSpec> {
    let Some(raw) = q.order.as_deref() else {
        return Vec::new();
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|seg| {
            if seg == "random" {
                return OrderSpec {
                    field: String::new(),
                    descending: false,
                    random: true,
                };
            }
            let descending = seg.starts_with('-');
            let field = seg.trim_start_matches('-').to_owned();
            OrderSpec {
                field,
                descending,
                random: false,
            }
        })
        .collect()
}

/// Parse `?order=`, refusing a field this endpoint cannot sort by.
///
/// Dropping an unrecognised field silently was the original behaviour,
/// and it was worse than it sounds. Each comparator answers `continue`
/// for a field it doesn't know, so an `?order=` naming *only* unknown
/// fields compared every pair `Equal`; the sort became a no-op and the
/// endpoint's documented default ordering was discarded along with it. A
/// client that typo'd `?order=titel` got neither its ordering nor the
/// default, with nothing said.
///
/// Refusing is also what this API already does one endpoint over:
/// `/search/?type=bogus` is a `400` "so a typo doesn't look like 'no
/// matches'". The same reasoning applies to a typo that looks like
/// "wrong order".
///
/// `random` is always accepted — it names no field.
///
/// # Errors
/// A message naming the offending field and listing the valid ones.
pub fn parse_order_in(q: &ListQuery, allowed: &[&str]) -> Result<Vec<OrderSpec>, String> {
    let specs = parse_order(q);
    for spec in &specs {
        if !spec.random && !allowed.contains(&spec.field.as_str()) {
            return Err(format!(
                "unknown `order` field `{}` — expected any of: {}, or `random`",
                spec.field,
                allowed.join(", "),
            ));
        }
    }
    Ok(specs)
}

/// Refuse a `?fields=` name the serializer never emits.
///
/// Same defect, different parameter: an unknown name was dropped, so
/// `?fields=titel` returned `{"id":…,"meta":{…}}` — every object stripped
/// to nothing, with no hint that the client had misspelled the one field
/// it asked for.
///
/// Validated against the keys of a **serialized sample** rather than a
/// hand-written list, so it cannot drift from what the endpoint actually
/// returns. `id` and `meta` are always permitted: they are always kept in
/// the output, so naming one is harmless even where a serializer omits
/// it.
///
/// A caller with no rows to sample skips validation — there is nothing to
/// filter, and guessing an allowlist from an empty page would mean
/// inventing the very list this avoids.
///
/// # Errors
/// A message naming the offending field and listing the valid ones.
pub fn validate_fields(
    requested: Option<&std::collections::HashSet<String>>,
    sample: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let Some(requested) = requested else {
        return Ok(());
    };
    let known = |k: &str| k == "id" || k == "meta" || sample.contains_key(k);
    let Some(bad) = requested.iter().find(|k| !known(k)) else {
        return Ok(());
    };
    let mut valid: Vec<&str> = sample.keys().map(String::as_str).collect();
    valid.sort_unstable();
    Err(format!(
        "unknown `fields` name `{bad}` — expected any of: {}",
        valid.join(", "),
    ))
}

/// The standard list envelope: `{ meta: { total_count, limit, offset }, items: [...] }`.
#[derive(Debug, Serialize)]
pub struct ListEnvelope {
    pub meta: ListMeta,
    pub items: Vec<serde_json::Value>,
}

#[derive(Debug, Default, Serialize)]
pub struct ListMeta {
    pub total_count: usize,
    pub limit: usize,
    pub offset: usize,
    /// Whether another page exists after this one. A client used to have
    /// to derive this from `offset + items.len() < total_count`, and get
    /// it wrong at the boundary.
    pub has_more: bool,
    /// Offsets to walk with, rather than fully-qualified URLs: the server
    /// sits behind a proxy often enough that it cannot reliably rebuild
    /// its own public URL, and a wrong link is worse than none. `null` at
    /// either end of the range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_offset: Option<usize>,
    /// #408 — a fuzzy "did you mean?" suggestion, set only by the page
    /// search when it returns no hits but a near-match title exists.
    /// Omitted from the JSON otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub did_you_mean: Option<String>,
}

impl ListMeta {
    /// Build the envelope's meta, deriving the paging cursors once so
    /// every list endpoint reports them identically.
    #[must_use]
    pub fn paged(total_count: usize, limit: usize, offset: usize) -> Self {
        let end = offset.saturating_add(limit);
        let has_more = end < total_count;
        Self {
            total_count,
            limit,
            offset,
            has_more,
            next_offset: has_more.then_some(end),
            previous_offset: (offset > 0).then(|| offset.saturating_sub(limit)),
            did_you_mean: None,
        }
    }
}

/// `axum::extract::Query`, rejecting in JSON.
///
/// axum's own extractor refuses a malformed query string with a
/// `text/plain` body — `Failed to deserialize query string: invalid digit
/// found in string`. Every *handler* on this API answers in the JSON
/// envelope, and `docs/api.md` promises that unconditionally, so a client
/// whose fetch wrapper parses failures as JSON works fine right up until
/// someone types `?limit=abc`, at which point it raises a `SyntaxError`
/// from its own parser instead of reporting the 400.
///
/// A malformed query value is not an exotic case: it is what a SPA sends
/// whenever a route param arrives as the wrong type, and it is the same
/// surface `?root=` was reported on.
///
/// The destructuring pattern is identical to axum's — `Query(q):
/// Query<T>` — so switching a handler over is an import change.
pub struct Query<T>(pub T);

impl<T, S> axum::extract::FromRequestParts<S> for Query<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = crate::api::error::ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(v)) => Ok(Self(v)),
            // serde's text is kept: it names the offending field where it
            // can ("root: invalid digit…"), which is the only clue a
            // client gets about *which* parameter it got wrong.
            Err(e) => Err(crate::api::error::ApiError::bad_request(e.body_text())),
        }
    }
}

/// `axum::extract::Path`, rejecting in JSON.
///
/// Same defect as [`Query`]: `/api/v2/pages/abc/` answers `Invalid URL:
/// Cannot parse \`abc\` to a \`i64\`` as `text/plain`.
pub struct Path<T>(pub T);

impl<T, S> axum::extract::FromRequestParts<S> for Path<T>
where
    T: serde::de::DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = crate::api::error::ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(v)) => Ok(Self(v)),
            Err(e) => Err(crate::api::error::ApiError::bad_request(e.body_text())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `?root=&depth=&locale=` — a client rendering a URL template with
    // unset values. Every one of these was a 400 before `empty_as_none`.
    #[test]
    fn an_empty_numeric_param_reads_as_absent() {
        let q: ListQuery = serde_urlencoded::from_str("limit=&offset=").expect("parse");
        assert_eq!(q.limit, None);
        assert_eq!(q.offset, None);
        assert_eq!(paginate(&q), (DEFAULT_LIMIT, 0));
    }

    #[test]
    fn an_empty_string_param_reads_as_absent() {
        let q: ListQuery = serde_urlencoded::from_str("fields=&order=&search=").expect("parse");
        assert_eq!(q.fields, None);
        assert_eq!(q.order, None);
        assert_eq!(q.search, None);
        assert!(field_set(&q).is_none(), "empty ?fields= must not select nothing");
    }

    #[test]
    fn a_whitespace_only_param_reads_as_absent() {
        let q: ListQuery = serde_urlencoded::from_str("limit=%20&search=%20").expect("parse");
        assert_eq!(q.limit, None);
        assert_eq!(q.search, None);
    }

    #[test]
    fn a_real_value_still_parses() {
        let q: ListQuery = serde_urlencoded::from_str("limit=5&offset=10&search=x").expect("parse");
        assert_eq!(q.limit, Some(5));
        assert_eq!(q.offset, Some(10));
        assert_eq!(q.search.as_deref(), Some("x"));
    }

    #[test]
    fn a_non_empty_unparseable_value_is_still_an_error() {
        // Coercing `?limit=abc` to the default would hide a real client
        // bug; only *emptiness* means "absent".
        let r: Result<ListQuery, _> = serde_urlencoded::from_str("limit=abc");
        assert!(r.is_err());
    }

    fn q_with_since(v: &str) -> ListQuery {
        ListQuery {
            updated_since: Some(v.to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn updated_since_parses_rfc3339() {
        let got = parse_updated_since(&q_with_since("2026-01-02T15:04:05Z")).expect("valid");
        assert_eq!(
            got.map(|d| d.to_rfc3339()),
            Some("2026-01-02T15:04:05+00:00".to_owned()),
        );
    }

    #[test]
    fn updated_since_normalizes_an_offset_to_utc() {
        let got = parse_updated_since(&q_with_since("2026-01-02T15:04:05+02:00")).expect("valid");
        assert_eq!(
            got.map(|d| d.to_rfc3339()),
            Some("2026-01-02T13:04:05+00:00".to_owned()),
        );
    }

    #[test]
    fn updated_since_absent_is_none() {
        assert_eq!(parse_updated_since(&ListQuery::default()), Ok(None));
    }

    #[test]
    fn a_malformed_updated_since_is_an_error_not_a_full_result() {
        // Silently ignoring it would tell a polling client that nothing
        // had changed since a timestamp it never actually applied.
        assert!(parse_updated_since(&q_with_since("yesterday")).is_err());
        assert!(parse_updated_since(&q_with_since("2026-13-45")).is_err());
    }

    #[test]
    fn paged_reports_the_next_offset_only_when_there_is_one() {
        let m = ListMeta::paged(50, 20, 0);
        assert!(m.has_more);
        assert_eq!(m.next_offset, Some(20));
        assert_eq!(m.previous_offset, None, "no page before the first");

        let m = ListMeta::paged(50, 20, 40);
        assert!(!m.has_more, "40 + 20 covers all 50");
        assert_eq!(m.next_offset, None);
        assert_eq!(m.previous_offset, Some(20));
    }

    #[test]
    fn paged_handles_the_exact_boundary() {
        // The off-by-one a client would have written by hand: the last
        // full page must not advertise another one.
        let m = ListMeta::paged(40, 20, 20);
        assert!(!m.has_more);
        assert_eq!(m.next_offset, None);
    }

    #[test]
    fn paged_survives_an_offset_past_the_end() {
        let m = ListMeta::paged(10, 20, 999_999);
        assert!(!m.has_more);
        assert_eq!(m.next_offset, None);
        assert_eq!(m.previous_offset, Some(999_979));
    }

    #[test]
    fn paginate_clamps_to_max() {
        let q = ListQuery {
            limit: Some(9999),
            offset: None,
            ..Default::default()
        };
        let (limit, offset) = paginate(&q);
        assert_eq!(limit, MAX_LIMIT);
        assert_eq!(offset, 0);
    }

    #[test]
    fn paginate_default_when_unset() {
        let q = ListQuery::default();
        assert_eq!(paginate(&q), (DEFAULT_LIMIT, 0));
    }

    #[test]
    fn paginate_floor_one_on_zero() {
        let q = ListQuery {
            limit: Some(0),
            ..Default::default()
        };
        assert_eq!(paginate(&q).0, 1);
    }

    #[test]
    fn field_set_parses_csv() {
        let q = ListQuery {
            fields: Some("title, slug,body".to_owned()),
            ..Default::default()
        };
        let set = field_set(&q).expect("set present");
        assert!(set.contains("title"));
        assert!(set.contains("slug"));
        assert!(set.contains("body"));
    }

    #[test]
    fn apply_fields_keeps_meta_and_id() {
        let mut obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"id":1,"meta":{},"title":"a","slug":"b"}"#).unwrap();
        let keep: std::collections::HashSet<String> = ["slug".to_owned()].into_iter().collect();
        apply_fields(&mut obj, Some(&keep));
        assert!(obj.contains_key("id"));
        assert!(obj.contains_key("meta"));
        assert!(obj.contains_key("slug"));
        assert!(!obj.contains_key("title"));
    }

    #[test]
    fn parse_order_descending_prefix() {
        let q = ListQuery {
            order: Some("-published_at,title".to_owned()),
            ..Default::default()
        };
        let specs = parse_order(&q);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].field, "published_at");
        assert!(specs[0].descending);
        assert_eq!(specs[1].field, "title");
        assert!(!specs[1].descending);
    }

    #[test]
    fn parse_order_random() {
        let q = ListQuery {
            order: Some("random".to_owned()),
            ..Default::default()
        };
        let specs = parse_order(&q);
        assert_eq!(specs.len(), 1);
        assert!(specs[0].random);
    }

    #[test]
    fn an_unknown_order_field_is_refused_by_name() {
        // The whole point: a typo must not look like "wrong order".
        let q = ListQuery {
            order: Some("titel".to_owned()),
            ..Default::default()
        };
        let err = parse_order_in(&q, &["title", "slug"]).expect_err("must refuse");
        assert!(err.contains("titel"), "{err}");
        assert!(err.contains("title"), "the valid names must be listed: {err}");
    }

    #[test]
    fn one_bad_field_refuses_the_whole_ordering() {
        // Accepting the good half would silently apply an ordering the
        // client did not ask for.
        let q = ListQuery {
            order: Some("title,-titel".to_owned()),
            ..Default::default()
        };
        assert!(parse_order_in(&q, &["title"]).is_err());
    }

    #[test]
    fn known_fields_and_random_pass_through() {
        let q = ListQuery {
            order: Some("-title,random".to_owned()),
            ..Default::default()
        };
        let specs = parse_order_in(&q, &["title"]).expect("accepted");
        assert_eq!(specs.len(), 2);
        assert!(specs[0].descending);
        assert!(specs[1].random, "`random` names no field, so no allowlist applies");
    }

    #[test]
    fn no_order_at_all_is_not_an_error() {
        let q = ListQuery::default();
        assert!(parse_order_in(&q, &["title"]).expect("accepted").is_empty());
    }

    fn sample() -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert("title".to_owned(), serde_json::json!("T"));
        m.insert("slug".to_owned(), serde_json::json!("t"));
        m
    }

    #[test]
    fn an_unknown_fields_name_is_refused_by_name() {
        // Previously this returned `{id, meta}` — every object stripped
        // to nothing, with no hint the name was misspelled.
        let mut want = std::collections::HashSet::new();
        want.insert("titel".to_owned());
        let err = validate_fields(Some(&want), &sample()).expect_err("must refuse");
        assert!(err.contains("titel"), "{err}");
        assert!(err.contains("slug"), "the valid names must be listed: {err}");
    }

    #[test]
    fn id_and_meta_are_always_nameable() {
        // They are always kept in the output, so asking for one is
        // harmless even where the serializer does not list it.
        let want: std::collections::HashSet<String> =
            ["id", "meta"].iter().map(|s| (*s).to_owned()).collect();
        assert!(validate_fields(Some(&want), &sample()).is_ok());
    }

    #[test]
    fn no_fields_parameter_validates_nothing() {
        assert!(validate_fields(None, &sample()).is_ok());
        assert!(validate_fields(None, &serde_json::Map::new()).is_ok());
    }
}
