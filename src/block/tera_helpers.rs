//! Public-side `stream_render` Tera function.
//!
//! Mirrors the [`crate::snippet`] pattern (`src/snippet.rs:140-183`):
//! the request handler pre-renders every stream value into HTML
//! BEFORE Tera runs (Tera functions are sync; recursive block render
//! is async-clean only after pre-flight), stashes the result into a
//! `slug → html` map under context key `_stream_html`, and this
//! function just reads from that map at template-render time.
//!
//! ## Usage in templates
//!
//! ```text
//! {{ stream_render(name="body_stream") | safe }}
//! ```
//!
//! `name` is the [`crate::widget::Widget::name`] of the stream
//! widget (i.e. the field on the extension row carrying the JSON).
//! Missing name → empty string.

use tera::{Tera, Value};

use super::render;
use super::BlockRenderCtx;

thread_local! {
    /// Per-thread `field/zone name → pre-rendered HTML` map. Set for the
    /// duration of one `tera.render` call via [`install`]. Tera 1.20
    /// functions can't see the render context, so `stream_render` reads
    /// this instead of its args (the args path stays as a fallback for
    /// callers that pass `_stream_html=` explicitly).
    static CURRENT_STREAM_HTML: std::cell::RefCell<Option<std::sync::Arc<std::collections::HashMap<String, String>>>>
        = const { std::cell::RefCell::new(None) };
}

/// RAII guard: while alive the thread-local stream-HTML map is set; on
/// drop it clears. Bind it across `tera.render(…)` and drop right after.
#[must_use = "the guard clears the thread-local on drop; bind it to a local"]
pub struct StreamHtmlGuard {
    _priv: (),
}

impl Drop for StreamHtmlGuard {
    fn drop(&mut self) {
        CURRENT_STREAM_HTML.with(|cell| *cell.borrow_mut() = None);
    }
}

/// Install the `name → HTML` map for the duration of the returned guard,
/// so `stream_render(name="…")` resolves against it during `tera.render`.
pub fn install(stream_html: std::collections::HashMap<String, String>) -> StreamHtmlGuard {
    CURRENT_STREAM_HTML.with(|cell| *cell.borrow_mut() = Some(std::sync::Arc::new(stream_html)));
    StreamHtmlGuard { _priv: () }
}

/// Register `stream_render` on `tera`. Call once at Tera setup.
///
/// Resolves `name` against the thread-local map installed by [`install`]
/// (the public render pipeline installs it just before `tera.render`);
/// falls back to an explicit `_stream_html=` arg for direct callers.
pub fn register_tera_function(tera: &mut Tera) {
    tera.register_function(
        "stream_render",
        move |args: &std::collections::HashMap<String, Value>| {
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| tera::Error::msg("stream_render: `name` is required"))?;
            // Thread-local first (Tera 1.20 functions can't read context).
            if let Some(html) = CURRENT_STREAM_HTML
                .with(|cell| cell.borrow().as_ref().and_then(|m| m.get(name).cloned()))
            {
                return Ok(Value::String(html));
            }
            let html = args
                .get("_stream_html")
                .and_then(Value::as_object)
                .and_then(|m| m.get(name))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            Ok(Value::String(html))
        },
    );
}

/// Parse the canonical stream JSON for a field. Accepts both the
/// stringified-JSON storage shape (the usual extension-column form)
/// and an already-parsed array; an absent or garbled value yields an
/// empty array (renders nothing).
pub(crate) fn canonical_parsed(raw: Option<&serde_json::Value>) -> serde_json::Value {
    match raw {
        Some(serde_json::Value::String(s)) => {
            serde_json::from_str(s).unwrap_or(serde_json::Value::Array(Vec::new()))
        }
        Some(other) => other.clone(),
        None => serde_json::Value::Array(Vec::new()),
    }
}

/// Resolve which stream JSON to render for `field`, honoring a
/// per-locale translation override (#i18n body translation).
///
/// `translations` maps `field_path → localized value` for the active
/// locale (the same map the `t` filter reads; empty on the default
/// locale). When it carries a non-empty, JSON-**array** override for
/// this stream field, that localized body wins; otherwise the
/// canonical extension body is used. A malformed override (invalid
/// JSON, or valid-but-not-an-array) logs a warning and falls back to
/// canonical — it never panics, so a bad translation row can only
/// degrade to the source-language body, never break the page.
fn localized_or_canonical(
    field: &str,
    raw: Option<&serde_json::Value>,
    translations: &std::collections::HashMap<String, String>,
) -> serde_json::Value {
    match translations
        .get(field)
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        Some(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(v) if v.is_array() => v,
            Ok(_) => {
                tracing::warn!(
                    target: "rustango_cms::block",
                    field = %field,
                    "stream translation override is not a JSON array; using canonical body",
                );
                canonical_parsed(raw)
            }
            Err(e) => {
                tracing::warn!(
                    target: "rustango_cms::block",
                    field = %field,
                    error = %e,
                    "stream translation override is not valid JSON; using canonical body",
                );
                canonical_parsed(raw)
            }
        },
        None => canonical_parsed(raw),
    }
}

/// Walk the handler-provided list of widgets, find every
/// [`crate::widget::WidgetKind::Stream`], pre-render its JSON value
/// to HTML, and return a `name → html` map ready to stamp on the
/// Tera context as `_stream_html`.
///
/// `extension` is the serialized extension JSON (the same value
/// `handler.load_extension` produces). Stream values can sit anywhere
/// under it; we look them up by the widget's `name`. `translations`
/// carries the active locale's per-field overrides — a stream field
/// with a localized body in that map renders the translated blocks
/// (see `localized_or_canonical`); pass an empty map for the
/// canonical (default-locale) render.
///
/// # Errors
/// Tera failures during nested block rendering bubble up. Unknown
/// block types do NOT short-circuit — they render as a yellow drift
/// banner inline (see [`super::render::prerender_stream`]).
pub fn prerender_extension_streams(
    widgets: &[crate::widget::Widget],
    extension: &serde_json::Value,
    tera: &Tera,
    translations: &std::collections::HashMap<String, String>,
) -> Result<std::collections::HashMap<String, String>, super::BlockError> {
    let mut out = std::collections::HashMap::new();
    let ctx = BlockRenderCtx::new(tera);
    for w in widgets {
        if !matches!(w.kind, crate::widget::WidgetKind::Stream) {
            continue;
        }
        // Base array = canonical, or a legacy whole-body blob override
        // (#i18n back-compat); then layer per-leaf overrides on top so
        // individual block fields — including nested ones — localize.
        let base = localized_or_canonical(&w.name, extension.get(&w.name), translations);
        let parsed = super::translate::apply_translatable_overrides(&w.name, &base, translations);
        let html = render::prerender_stream(&parsed, &ctx)?;
        out.insert(w.name.clone(), html);
    }
    Ok(out)
}

/// Async variant — preferred when the caller is already in an
/// `async fn`. Pre-fetches every chooser reference inside the stream
/// trees and stamps the resolved fields (`_url`, `_title`, etc.) onto
/// the block values before kicking the sync render walker.
///
/// # Errors
/// Same as the sync variant.
pub async fn prerender_extension_streams_async(
    widgets: &[crate::widget::Widget],
    extension: &serde_json::Value,
    tera: &Tera,
    pool: &rustango::sql::Pool,
    translations: &std::collections::HashMap<String, String>,
    // #550 FB-17 — active non-default locale id, used to localize embedded
    // form blocks (None for the default locale).
    form_locale_id: Option<i64>,
) -> Result<std::collections::HashMap<String, String>, super::BlockError> {
    prerender_extension_streams_async_with(
        widgets,
        extension,
        tera,
        pool,
        translations,
        form_locale_id,
        None,
        "",
    )
    .await
}

/// [`prerender_extension_streams_async`], plus the page the stream
/// belongs to.
///
/// The page id reaches host [`enrichers`](super::enrich) as
/// [`EnrichCtx::page_id`](super::enrich::EnrichCtx::page_id). A block
/// that inherits something from its page — a reporting period set once
/// on a section root, a currency, a locale default — cannot look that up
/// from the pool alone, and a thread-local would not survive the awaits
/// in this function.
///
/// Separate function rather than a changed signature, per the `_with`
/// convention already used for the dyn-block variants: every existing
/// caller keeps working and opts in by name.
///
/// # Errors
/// Same as the sync variant.
#[allow(clippy::too_many_arguments)]
pub async fn prerender_extension_streams_async_with(
    widgets: &[crate::widget::Widget],
    extension: &serde_json::Value,
    tera: &Tera,
    pool: &rustango::sql::Pool,
    translations: &std::collections::HashMap<String, String>,
    form_locale_id: Option<i64>,
    page_id: Option<i64>,
    tenant: &str,
) -> Result<std::collections::HashMap<String, String>, super::BlockError> {
    let mut out = std::collections::HashMap::new();
    let ctx = BlockRenderCtx::new(tera);
    for w in widgets {
        if !matches!(w.kind, crate::widget::WidgetKind::Stream) {
            continue;
        }
        // Per-locale resolution, in order: base array = canonical (or a
        // legacy whole-body blob override for #i18n back-compat), then
        // per-leaf overrides merged on top (recursively, incl. nested
        // blocks). Chooser enrichment runs AFTER the merge so it resolves
        // refs in the localized tree (a translation may reference its own
        // images/snippets).
        let base = localized_or_canonical(&w.name, extension.get(&w.name), translations);
        let mut parsed =
            super::translate::apply_translatable_overrides(&w.name, &base, translations);
        enrich_chooser_refs_async(&mut parsed, pool, form_locale_id).await;
        // Host-registered enrichment, after the built-in chooser pass so
        // a host sees page/image/snippet refs already resolved. This is
        // the last moment anything async can touch the stream — the
        // render below is synchronous.
        super::enrich::fire(
            &mut parsed,
            super::enrich::EnrichCtx {
                pool,
                tenant,
                page_id,
            },
        )
        .await;
        let html = render::prerender_stream(&parsed, &ctx)?;
        out.insert(w.name.clone(), html);
    }
    Ok(out)
}

/// Recursive enrichment. For each block in the stream array, looks
/// at its `type`:
///
/// - `page_chooser`: looks up `cms_page` by `value.page_id`, adds
///   `value._url`, `value._title`, `value._slug`.
/// - `snippet_chooser`: looks up `cms_snippet` by `value.snippet_id`,
///   adds `value._title`, `value._slug`, `value._html` (rendered
///   markdown body).
/// - `document_chooser`: looks up `cms_media` by `value.document_id`,
///   adds `value._url`, `value._title`, `value._filename`,
///   `value._mime`, `value._size`.
/// - `image`: looks up `cms_media` by `value.media_id`, adds
///   `value._url`, `value._title`, `value._alt_text`, `value._width`,
///   `value._height`.
/// - `nested_stream`-style fields (`Stream` BlockFields whose value is
///   an array under any key): recurse.
/// - `typed_table`-style fields (`Repeat` BlockFields whose value is
///   an array): recurse.
///
/// All lookups are best-effort. A missing row leaves the value
/// un-augmented so templates can fall back to their existing
/// placeholder shapes. Resolved fields use the `_` prefix so they
/// never collide with author-defined keys.
pub async fn enrich_chooser_refs_async(
    stream: &mut serde_json::Value,
    pool: &rustango::sql::Pool,
    form_locale_id: Option<i64>,
) {
    // #648 — two passes: collect every referenced id in the (nested)
    // stream, resolve them with one query per table, then stamp the rows
    // in. Per-block lookups made the query count the author's block count:
    // a 40-image gallery was 40 sequential round trips on every render.
    let mut refs = ChooserRefs::default();
    collect_chooser_refs(stream, &mut refs);
    if refs.is_empty() {
        return;
    }
    let rows = ChooserRows::fetch(pool, &refs, form_locale_id).await;
    apply_chooser_refs(stream, &rows);
}

/// Ids and form slugs a stream's chooser blocks reference.
#[derive(Default)]
struct ChooserRefs {
    pages: std::collections::BTreeSet<i64>,
    snippets: std::collections::BTreeSet<i64>,
    media: std::collections::BTreeSet<i64>,
    forms: std::collections::BTreeSet<String>,
}

impl ChooserRefs {
    fn is_empty(&self) -> bool {
        self.pages.is_empty() && self.snippets.is_empty() && self.media.is_empty() && self.forms.is_empty()
    }
}

/// The rows [`ChooserRefs`] resolve to. A missing row simply has no entry,
/// so its block stays un-augmented, as before.
struct ChooserRows {
    pages: std::collections::HashMap<i64, crate::page::Page>,
    snippets: std::collections::HashMap<i64, crate::snippet::Snippet>,
    media: std::collections::HashMap<i64, crate::media::Media>,
    /// Form reference (slug or id) → (snippet id, schema localized for the
    /// render locale).
    forms: std::collections::HashMap<String, (Option<i64>, serde_json::Value)>,
}

/// Ids per `IN (…)` list, well under every backend's bind limit.
const REF_CHUNK: usize = 1_000;

impl ChooserRows {
    async fn fetch(
        pool: &rustango::sql::Pool,
        refs: &ChooserRefs,
        form_locale_id: Option<i64>,
    ) -> Self {
        use rustango::core::Column as _;
        use rustango::sql::FetcherPool as _;
        let ids = |set: &std::collections::BTreeSet<i64>| set.iter().copied().collect::<Vec<_>>();

        let mut pages = std::collections::HashMap::new();
        for chunk in ids(&refs.pages).chunks(REF_CHUNK) {
            let found = crate::page::Page::objects()
                .where_(crate::page::Page::id.is_in(chunk.iter().copied()))
                .fetch(pool)
                .await
                .unwrap_or_default();
            pages.extend(found.into_iter().filter_map(|p| p.id.get().copied().map(|id| (id, p))));
        }
        let mut snippets = std::collections::HashMap::new();
        for chunk in ids(&refs.snippets).chunks(REF_CHUNK) {
            let found = crate::snippet::Snippet::objects()
                .where_(crate::snippet::Snippet::id.is_in(chunk.iter().copied()))
                .fetch(pool)
                .await
                .unwrap_or_default();
            snippets.extend(found.into_iter().filter_map(|s| s.id.get().copied().map(|id| (id, s))));
        }
        let mut media = std::collections::HashMap::new();
        for chunk in ids(&refs.media).chunks(REF_CHUNK) {
            let found = crate::media::Media::objects()
                .where_(crate::media::Media::id.is_in(chunk.iter().copied()))
                .fetch(pool)
                .await
                .unwrap_or_default();
            media.extend(found.into_iter().filter_map(|m| m.id.get().copied().map(|id| (id, m))));
        }
        // Keyed by the block's reference: a slug (typed in older pages) or
        // the form's id (the chooser). Slugs are looked up first.
        let mut forms = std::collections::HashMap::new();
        if !refs.forms.is_empty() {
            let found = crate::snippet::Snippet::objects()
                .where_(crate::snippet::Snippet::type_name.eq("form".to_owned()))
                .where_(crate::snippet::Snippet::slug.is_in(refs.forms.iter().cloned()))
                .fetch(pool)
                .await
                .unwrap_or_default();
            for s in found {
                if forms.contains_key(&s.slug) {
                    continue; // first match wins, as the per-block lookup did
                }
                let entry = localized_form(pool, &s, form_locale_id).await;
                forms.insert(s.slug.clone(), entry);
            }
            let ids: Vec<i64> = refs
                .forms
                .iter()
                .filter(|r| !forms.contains_key(*r))
                .filter_map(|r| r.parse::<i64>().ok())
                .collect();
            if !ids.is_empty() {
                let found = crate::snippet::Snippet::objects()
                    .where_(crate::snippet::Snippet::type_name.eq("form".to_owned()))
                    .where_(crate::snippet::Snippet::id.is_in(ids))
                    .fetch(pool)
                    .await
                    .unwrap_or_default();
                for s in found {
                    let Some(id) = s.id.get().copied() else { continue };
                    let entry = localized_form(pool, &s, form_locale_id).await;
                    forms.insert(id.to_string(), entry);
                }
            }
        }
        Self { pages, snippets, media, forms }
    }
}

/// A form's (snippet id, schema) — the schema localized for the render
/// locale.
async fn localized_form(
    pool: &rustango::sql::Pool,
    form: &crate::snippet::Snippet,
    form_locale_id: Option<i64>,
) -> (Option<i64>, serde_json::Value) {
    let sid = form.id.get().copied();
    let mut data = form.data.clone();
    if let (Some(lid), Some(sid_)) = (form_locale_id, sid) {
        if let Ok(over) = crate::snippet_translation::fetch_for(pool, sid_, lid).await {
            if !over.is_empty() {
                if let Ok(mut sch) = crate::forms::schema::parse(&data) {
                    crate::forms::schema::apply_translations(&mut sch, &over);
                    data = crate::forms::schema::to_value(&sch);
                }
            }
        }
    }
    (sid, data)
}

/// A `form` block's reference to its form: the slug typed into older pages,
/// or the id the chooser stores (a string, or a number from the API).
fn form_ref(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(s) => Some(s.trim()).filter(|s| !s.is_empty()).map(str::to_owned),
        serde_json::Value::Number(n) => n.as_i64().map(|id| id.to_string()),
        _ => None,
    }
}

/// The blocks of `stream` and of every array nested in a block's value
/// (Stream-in-Stream, Repeat-in-Stream, any container), with their type.
fn for_each_block(
    stream: &mut serde_json::Value,
    f: &mut dyn FnMut(&str, &str, &mut serde_json::Map<String, serde_json::Value>),
) {
    let serde_json::Value::Array(items) = stream else {
        return;
    };
    for entry in items.iter_mut() {
        let serde_json::Value::Object(envelope) = entry else {
            continue;
        };
        let type_name = envelope.get("type").and_then(|v| v.as_str()).unwrap_or("").to_owned();
        let block_id = envelope.get("id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
        let Some(serde_json::Value::Object(value_obj)) = envelope.get_mut("value") else {
            continue;
        };
        f(&type_name, &block_id, value_obj);
        for v in value_obj.values_mut() {
            if matches!(v, serde_json::Value::Array(_)) {
                for_each_block(v, f);
            }
        }
    }
}

fn collect_chooser_refs(stream: &mut serde_json::Value, refs: &mut ChooserRefs) {
    for_each_block(stream, &mut |type_name, _id, value| match type_name {
        "form" => {
            if let Some(r) = form_ref(value.get("form")) {
                refs.forms.insert(r);
            }
        }
        "page_chooser" => refs.pages.extend(read_id(value.get("page_id"))),
        "snippet_chooser" => refs.snippets.extend(read_id(value.get("snippet_id"))),
        "document_chooser" => refs.media.extend(read_id(value.get("document_id"))),
        "image" => refs.media.extend(read_id(value.get("media_id"))),
        _ => {}
    });
}

fn apply_chooser_refs(stream: &mut serde_json::Value, rows: &ChooserRows) {
    use serde_json::Value;
    for_each_block(stream, &mut |type_name, block_id, value| match type_name {
        "form" => {
            // #542 FB-09 — the keys the `form` block's render reads.
            let key = form_ref(value.get("form")).unwrap_or_default();
            if let Some((sid, data)) = rows.forms.get(&key) {
                value.insert("_schema".into(), data.clone());
                if let Some(sid) = sid {
                    value.insert("_form_id".into(), serde_json::json!(sid));
                }
            }
            value.insert("_embed".into(), Value::String(block_id.to_owned()));
        }
        "page_chooser" => {
            if let Some(p) = read_id(value.get("page_id")).and_then(|id| rows.pages.get(&id)) {
                value.insert("_url".into(), Value::String(p.url_path.clone()));
                value.insert("_title".into(), Value::String(p.title.clone()));
                value.insert("_slug".into(), Value::String(p.slug.clone()));
            }
        }
        "snippet_chooser" => {
            if let Some(s) = read_id(value.get("snippet_id")).and_then(|id| rows.snippets.get(&id)) {
                value.insert("_title".into(), Value::String(s.title.clone()));
                value.insert("_slug".into(), Value::String(s.slug.clone()));
                value.insert("_html".into(), Value::String(crate::markdown::render(&s.body_markdown)));
            }
        }
        "document_chooser" => {
            if let Some(m) = read_id(value.get("document_id")).and_then(|id| rows.media.get(&id)) {
                value.insert("_url".into(), Value::String(m.public_url()));
                value.insert("_title".into(), Value::String(m.title.clone()));
                value.insert("_filename".into(), Value::String(m.filename.clone()));
                value.insert("_mime".into(), Value::String(m.mime.clone()));
                value.insert("_size".into(), serde_json::json!(m.size));
            }
        }
        "image" => {
            if let Some(m) = read_id(value.get("media_id")).and_then(|id| rows.media.get(&id)) {
                value.insert("_url".into(), Value::String(m.public_url()));
                value.insert("_title".into(), Value::String(m.title.clone()));
                value.insert("_alt_text".into(), Value::String(m.alt_text.clone()));
                if let Some(w) = m.width {
                    value.insert("_width".into(), serde_json::json!(w));
                }
                if let Some(h) = m.height {
                    value.insert("_height".into(), serde_json::json!(h));
                }
            }
        }
        _ => {}
    });
}

/// Accepts both stringified ints (the stream-block storage shape) and
/// raw i64s (when the row was loaded from a typed column).
fn read_id(v: Option<&serde_json::Value>) -> Option<i64> {
    let v = v?;
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    v.as_str().and_then(|s| s.trim().parse::<i64>().ok())
}

#[cfg(test)]
mod tests {
    //! Per-locale StreamField body translation — the substitution
    //! logic (`localized_or_canonical`) is pure, so it's covered here
    //! without a DB or Tera. Confirms: canonical fallback is exact,
    //! a valid array override wins, and every malformed override
    //! degrades to canonical (never panics).
    use super::{canonical_parsed, localized_or_canonical};
    use std::collections::HashMap;

    const CANONICAL: &str = r#"[{"type":"heading","value":{"text":"Hello","level":"2"}}]"#;

    fn raw() -> serde_json::Value {
        serde_json::Value::String(CANONICAL.to_owned())
    }
    fn first_text(v: &serde_json::Value) -> &str {
        v.as_array().unwrap()[0]["value"]["text"].as_str().unwrap()
    }

    #[test]
    fn canonical_parsed_handles_every_shape() {
        // stringified JSON array (the storage shape)
        assert_eq!(first_text(&canonical_parsed(Some(&raw()))), "Hello");
        // already-parsed array passes through unchanged
        let arr = serde_json::json!([{"type":"x","value":{}}]);
        assert_eq!(canonical_parsed(Some(&arr)), arr);
        // absent -> empty array
        assert_eq!(canonical_parsed(None), serde_json::Value::Array(vec![]));
        // garbled string -> empty array, no panic
        let bad = serde_json::Value::String("{not json".to_owned());
        assert_eq!(
            canonical_parsed(Some(&bad)),
            serde_json::Value::Array(vec![])
        );
    }

    #[test]
    fn no_override_uses_canonical() {
        let out = localized_or_canonical("body", Some(&raw()), &HashMap::new());
        assert_eq!(out, canonical_parsed(Some(&raw())));
        assert_eq!(first_text(&out), "Hello");
    }

    #[test]
    fn valid_array_override_wins() {
        let fr = r#"[{"type":"heading","value":{"text":"Bonjour","level":"2"}}]"#;
        let t = HashMap::from([("body".to_owned(), fr.to_owned())]);
        assert_eq!(
            first_text(&localized_or_canonical("body", Some(&raw()), &t)),
            "Bonjour"
        );
    }

    #[test]
    fn empty_or_whitespace_override_falls_back() {
        for v in ["", "   ", "\n\t"] {
            let t = HashMap::from([("body".to_owned(), v.to_owned())]);
            assert_eq!(
                first_text(&localized_or_canonical("body", Some(&raw()), &t)),
                "Hello"
            );
        }
    }

    #[test]
    fn invalid_json_override_falls_back() {
        let t = HashMap::from([("body".to_owned(), "{not valid json".to_owned())]);
        assert_eq!(
            first_text(&localized_or_canonical("body", Some(&raw()), &t)),
            "Hello"
        );
    }

    #[test]
    fn non_array_override_falls_back() {
        let t = HashMap::from([("body".to_owned(), r#"{"text":"oops"}"#.to_owned())]);
        assert_eq!(
            first_text(&localized_or_canonical("body", Some(&raw()), &t)),
            "Hello"
        );
    }

    #[test]
    fn override_keyed_to_other_field_is_ignored() {
        let t = HashMap::from([(
            "some_other_field".to_owned(),
            r#"[{"type":"x","value":{}}]"#.to_owned(),
        )]);
        assert_eq!(
            first_text(&localized_or_canonical("body", Some(&raw()), &t)),
            "Hello"
        );
    }
}
