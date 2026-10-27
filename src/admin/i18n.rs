//! Admin UI i18n foundation.
//!
//! Wires the framework's message-catalog i18n (`rustango::i18n`) into the
//! admin Tera so templates can localize chrome with the `translate`
//! function/filter:
//!
//! ```jinja
//! {{ "Save" | translate(locale=LANG) }}
//! ```
//!
//! Keys are the **English source strings** (gettext-style); a missing
//! translation falls back to the key, so untranslated strings render in
//! English rather than a blank/raw id. Catalogs for the launch languages
//! (uk, pl, fr, de, zh-Hans, ja — never ru) are embedded from `locales/`.
//!
//! This module owns: the locale set ([`UI_LOCALES`]), per-request resolution
//! ([`negotiate`] — cookie → per-user pref → Accept-Language → tenant default →
//! English), server-string translation ([`tr`]), the embedded
//! [`admin_translator`], the per-tenant override layer behind the Tera
//! bindings ([`register_tera_bindings`]), and the completeness/extraction
//! tooling (tests).
//! See `docs/i18n-admin.md` for the authoring workflow.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use axum::http::{header, HeaderMap};
use rustango::i18n::{Locale, Translator};

/// The UI locales the admin chrome is (being) translated to. English is the
/// source; the rest are the launch set. Russian is intentionally absent.
pub(crate) const UI_LOCALES: &[(&str, &str)] = &[
    ("en", "English"),
    ("uk", "Українська"),
    ("pl", "Polski"),
    ("fr", "Français"),
    ("de", "Deutsch"),
    ("zh-Hans", "中文"),
    ("ja", "日本語"),
];

/// Sticky admin-UI-language cookie (distinct from the public `rcms_locale`
/// content-locale cookie).
pub(crate) const ADMIN_LANG_COOKIE: &str = "rcms_admin_lang";

/// True when `code` is a UI locale we actually ship.
#[must_use]
pub(crate) fn is_ui_locale(code: &str) -> bool {
    UI_LOCALES.iter().any(|(c, _)| *c == code)
}

/// Resolve the active admin UI locale. Precedence:
///   1. sticky `rcms_admin_lang` cookie (explicit, most recent choice)
///   2. durable per-user preference (`rustango_users.data.admin_lang`)
///   3. `Accept-Language` — exact, then base-language (`fr-FR`→`fr`, `zh-*`→`zh-Hans`)
///   4. per-tenant default
///   5. English
/// Only ever returns a shipped locale. `user_pref`/`tenant_default` are `None`
/// in pre-auth / userless contexts (e.g. server-side `tr()` flashes).
#[must_use]
pub(crate) fn negotiate(
    cookie_header: Option<&str>,
    accept_language: Option<&str>,
    user_pref: Option<&str>,
    tenant_default: Option<&str>,
) -> String {
    if let Some(h) = cookie_header {
        for part in h.split(';') {
            if let Some(v) = part.trim().strip_prefix(&format!("{ADMIN_LANG_COOKIE}=")) {
                let v = v.trim();
                if is_ui_locale(v) {
                    return v.to_owned();
                }
            }
        }
    }
    if let Some(p) = user_pref {
        if is_ui_locale(p) {
            return p.to_owned();
        }
    }
    if let Some(al) = accept_language {
        for tag in al.split(',') {
            let code = tag.split(';').next().unwrap_or("").trim();
            if code.is_empty() {
                continue;
            }
            if is_ui_locale(code) {
                return code.to_owned();
            }
            let base = code.split('-').next().unwrap_or("").to_lowercase();
            // Chinese collapses to our Simplified catalog.
            if base == "zh" {
                return "zh-Hans".to_owned();
            }
            if let Some((c, _)) = UI_LOCALES.iter().find(|(c, _)| *c == base) {
                return (*c).to_owned();
            }
        }
    }
    if let Some(t) = tenant_default {
        if is_ui_locale(t) {
            return t.to_owned();
        }
    }
    "en".to_owned()
}

/// Per-tenant default admin locale. Sourced from
/// `RUSTANGO_CMS_DEFAULT_ADMIN_LOCALE` so multi-tenant deployments can pin a
/// regional default without a schema change — mirrors the tenant-default
/// timezone pattern. Returns `None` unless the env var names a shipped locale.
#[must_use]
pub(crate) fn tenant_default_locale() -> Option<String> {
    // Read once: it is process configuration, and this runs per request.
    static DEFAULT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    DEFAULT
        .get_or_init(|| {
            crate::config::var("DEFAULT_ADMIN_LOCALE")
                .map(|s| s.trim().to_owned())
                .filter(|s| is_ui_locale(s))
        })
        .clone()
}

fn parse(json: &str) -> HashMap<String, String> {
    serde_json::from_str(json).unwrap_or_default()
}

/// Parse a plural catalog: `key → (CLDR category → template)`.
fn parse_plural(json: &str) -> HashMap<String, HashMap<String, String>> {
    serde_json::from_str(json).unwrap_or_default()
}

/// The shipped catalog JSON for `code` (source-embedded, same set as
/// [`admin_translator`]). Returns `"{}"` for unknown codes. Kept here so the
/// completeness tooling and the translator build read one source.
fn catalog_json(code: &str) -> &'static str {
    match code {
        "en" => include_str!("locales/en.json"),
        "uk" => include_str!("locales/uk.json"),
        "pl" => include_str!("locales/pl.json"),
        "fr" => include_str!("locales/fr.json"),
        "de" => include_str!("locales/de.json"),
        "zh-Hans" => include_str!("locales/zh-Hans.json"),
        "ja" => include_str!("locales/ja.json"),
        _ => "{}",
    }
}

/// The shipped **plural** catalog JSON for `code` — count-aware
/// flash strings keyed by CLDR category. Returns `"{}"` for unknown codes.
fn plural_catalog_json(code: &str) -> &'static str {
    match code {
        "en" => include_str!("locales/en.plurals.json"),
        "uk" => include_str!("locales/uk.plurals.json"),
        "pl" => include_str!("locales/pl.plurals.json"),
        "fr" => include_str!("locales/fr.plurals.json"),
        "de" => include_str!("locales/de.plurals.json"),
        "zh-Hans" => include_str!("locales/zh-Hans.plurals.json"),
        "ja" => include_str!("locales/ja.plurals.json"),
        _ => "{}",
    }
}

/// The launch locales that must be fully translated — every UI locale we ship
/// except the English source. Drives the completeness check.
#[cfg(test)]
#[must_use]
pub(crate) fn launch_locales() -> Vec<&'static str> {
    UI_LOCALES
        .iter()
        .map(|(c, _)| *c)
        .filter(|c| *c != "en")
        .collect()
}

/// Build the admin `Translator`: English default + fallback, with each
/// launch-language catalog embedded from `locales/<code>.json`.
pub(crate) fn admin_translator() -> Arc<Translator> {
    let mut t = Translator::new(Locale::new("en")).with_fallback_chain(&["en"]);
    for (code, _) in UI_LOCALES {
        t = t.add_locale(Locale::new(*code), parse(catalog_json(code)));
        t = t.add_plural_locale(Locale::new(*code), parse_plural(plural_catalog_json(code)));
    }
    Arc::new(t)
}

/// Process-wide cached admin translator: the embedded catalogs plus any
/// [`extend_ui_catalog`] entries. Identical for every tenant, so it never
/// carries the DB override layer — tenant overrides live in
/// [`tenant_overrides`] and are consulted first by [`translate`].
pub(crate) fn cached_translator() -> &'static Arc<Translator> {
    static T: OnceLock<Arc<Translator>> = OnceLock::new();
    T.get_or_init(admin_translator)
}

/// Merge extra UI-catalog entries into the shared admin translator's
/// per-locale **catalogs**, so a host that also wants to localize its own
/// hardcoded template chrome can feed those strings through the SAME
/// `translate(locale=LANG)` binding instead of registering a competing one
/// (which would silently clobber the admin catalog — see rcms-blog).
///
/// `extra` is `{ locale_code -> { english_key -> translated } }`. The shipped
/// admin catalog wins on any shared key, so admin UI strings are never
/// overwritten. Only the catalog layer is touched — the per-tenant DB
/// override layer is independent and keeps working.
///
/// Call once at boot AFTER [`crate::admin::register_templates`] (which
/// registers the binding against this same cached translator).
pub fn extend_ui_catalog(extra: HashMap<String, HashMap<String, String>>) {
    let tr = cached_translator();
    for (code, entries) in extra {
        // Consumer entries first, then the shipped admin catalog overwrites
        // any shared key — admin correctness preserved. For a locale the
        // admin doesn't ship (e.g. a content-only locale), the admin side is
        // empty and only the consumer strings remain.
        let mut merged = entries;
        merged.extend(parse(catalog_json(&code)));
        tr.insert_locale(Locale::new(&code), merged);
    }
}

/// One tenant's editable admin-translation overrides (`rustango_translations`):
/// `locale → key → value`.
type Overrides = HashMap<Locale, HashMap<String, String>>;

rustango::__private_runtime::tokio::task_local! {
    /// The overrides of the tenant whose request this task is serving (#652).
    ///
    /// The framework binds `translate` to one process-wide translator and has
    /// no per-request hook (ujeenet/rustango-private#675), so one tenant's
    /// overrides used to render in every tenant's admin. A task-local rather
    /// than a thread-local: Tera's functions see only call-site kwargs, and
    /// tokio moves a task between threads at every await, so a thread-local
    /// set before a handler's awaits could be read by another tenant's render.
    /// Drop this module's bindings once the framework gains that hook.
    static ACTIVE_OVERRIDES: Arc<Overrides>;
}

/// Middleware that makes the request tenant's overrides visible to the
/// `translate` bindings and [`tr`] for the rest of the request. No tenant
/// resolved → no scope, and only the shipped catalogs apply.
pub(crate) async fn tenant_overrides_layer(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use rustango::tenancy::OrgResolver as _;
    if req.uri().path().starts_with("/cms-admin/static/") {
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    let mut overrides = None;
    if let Some(ctx) = parts
        .extensions
        .get::<Arc<rustango::extractors::TenantContext>>()
        .cloned()
    {
        let registry = ctx.pools.registry_pool();
        if let Ok(Some(org)) = ctx.resolver.resolve(&parts, &registry).await {
            if let Ok(pool) = ctx.pools.scoped_pool_dyn(&org).await {
                overrides = Some(tenant_overrides(&org.slug, &pool).await);
            }
        }
    }
    let req = axum::http::Request::from_parts(parts, body);
    match overrides {
        Some(o) => ACTIVE_OVERRIDES.scope(o, next.run(req)).await,
        None => next.run(req).await,
    }
}

/// `slug`'s overrides, reloaded from its own pool at most once per TTL so an
/// edit in the translations editor shows within ~10s without a query per
/// render. Cached per tenant and never shared. A failed load serves the last
/// good copy (or none), so a missing table just leaves the shipped catalogs.
pub(crate) async fn tenant_overrides(slug: &str, pool: &rustango::sql::Pool) -> Arc<Overrides> {
    use std::time::{Duration, Instant};
    const REFRESH_TTL: Duration = Duration::from_secs(10);
    type Cache = std::sync::Mutex<HashMap<String, (Instant, Arc<Overrides>)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let stale = {
        let mut guard = cache.lock().expect("override cache poisoned");
        match guard.get_mut(slug) {
            Some((at, o)) if at.elapsed() < REFRESH_TTL => return Arc::clone(o),
            Some((at, o)) => {
                // Claim the refresh before the await; concurrent renders keep
                // serving the previous copy instead of stampeding the pool.
                *at = Instant::now();
                Some(Arc::clone(o))
            }
            None => None,
        }
    };
    let fresh = match rustango::i18n::db::all_pool(pool).await {
        Ok(rows) => {
            let mut map = Overrides::new();
            for t in rows {
                map.entry(Locale::new(&t.locale)).or_default().insert(t.key, t.value);
            }
            Arc::new(map)
        }
        Err(e) => {
            tracing::warn!(tenant = slug, error = %e, "admin translation overrides not refreshed");
            stale.unwrap_or_default()
        }
    };
    cache
        .lock()
        .expect("override cache poisoned")
        .insert(slug.to_owned(), (Instant::now(), Arc::clone(&fresh)));
    fresh
}

/// The active tenant's override for `key` in `locale` (exact, then base
/// language — the framework's override precedence).
fn active_override(locale: &str, key: &str) -> Option<String> {
    ACTIVE_OVERRIDES
        .try_with(|ov| {
            let req = Locale::new(locale);
            ov.get(&req)
                .and_then(|c| c.get(key))
                .or_else(|| ov.get(&Locale::new(req.base_language())).and_then(|c| c.get(key)))
                .cloned()
        })
        .ok()
        .flatten()
}

/// Translate `key` for `locale`: the active tenant's override, else the
/// shipped catalogs. An override of a key that carries markup is
/// HTML-escaped — templates render those keys through `| safe`, and an
/// override is tenant-authored text, never trusted markup (#652).
#[must_use]
pub(crate) fn translate(locale: &str, key: &str, params: &[(&str, &str)]) -> String {
    match active_override(locale, key) {
        Some(v) if key.contains('<') => substitute(&tera::escape_html(&v), params),
        Some(v) => substitute(&v, params),
        None => cached_translator().translate(locale, key, params),
    }
}

/// Count-aware [`translate`]. The plural catalogs win, exactly as in the
/// framework; only a key with no plural entry reaches the override layer.
#[must_use]
pub(crate) fn translate_plural(locale: &str, key: &str, n: i64, params: &[(&str, &str)]) -> String {
    static PLURAL_KEYS: OnceLock<std::collections::HashSet<String>> = OnceLock::new();
    let plural = PLURAL_KEYS.get_or_init(|| {
        UI_LOCALES
            .iter()
            .flat_map(|(code, _)| parse_plural(plural_catalog_json(code)).into_keys())
            .collect()
    });
    if plural.contains(key) {
        cached_translator().translate_plural(locale, key, n, params)
    } else {
        translate(locale, key, params)
    }
}

/// `{name}` placeholder substitution in one pass, so an inserted value is
/// never re-scanned — a copy of the framework's private helper.
fn substitute(template: &str, params: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let value = after.find('}').and_then(|close| {
            let name = &after[..close];
            params.iter().rev().find(|(n, _)| *n == name).map(|(_, v)| (*v, close))
        });
        match value {
            Some((v, close)) => {
                out.push_str(v);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Register the tenant-aware `translate` function + filter and
/// `translate_plural` function, replacing the framework's process-wide
/// bindings of the same names (ujeenet/rustango-private#675). Call after
/// `rustango::i18n::tera_tags::register`, which still supplies the locale
/// helpers (`get_text_direction`, `is_rtl`, …).
pub(crate) fn register_tera_bindings(tera: &mut tera::Tera) {
    use tera::Value;
    /// Every string / number / bool arg except the control keys, for
    /// `{name}` interpolation.
    fn interp(args: &HashMap<String, Value>, exclude: &[&str]) -> Vec<(String, String)> {
        args.iter()
            .filter(|(k, _)| !exclude.contains(&k.as_str()))
            .filter_map(|(k, v)| {
                let s = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => return None,
                };
                Some((k.clone(), s))
            })
            .collect()
    }
    fn run(key: &str, args: &HashMap<String, Value>) -> Value {
        let locale = args.get("locale").and_then(Value::as_str).unwrap_or("");
        let owned = interp(args, &["key", "locale"]);
        let refs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        Value::String(translate(locale, key, &refs))
    }
    tera.register_function("translate", |args: &HashMap<String, Value>| {
        let key = args
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| tera::Error::msg("translate(): missing required `key` argument"))?;
        Ok(run(key, args))
    });
    tera.register_filter("translate", |value: &Value, args: &HashMap<String, Value>| {
        let key = value
            .as_str()
            .ok_or_else(|| tera::Error::msg("translate filter: input must be a string (the translation key)"))?;
        Ok(run(key, args))
    });
    tera.register_function("translate_plural", |args: &HashMap<String, Value>| {
        let key = args
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| tera::Error::msg("translate_plural(): missing required `key` argument"))?;
        let n = args
            .get("n")
            .and_then(Value::as_i64)
            .ok_or_else(|| tera::Error::msg("translate_plural(): missing required integer `n` argument"))?;
        let locale = args.get("locale").and_then(Value::as_str).unwrap_or("");
        let owned = interp(args, &["key", "n", "locale"]);
        let refs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        Ok(Value::String(translate_plural(locale, key, n, &refs)))
    });
}

/// The request's admin locale for a server-emitted string.
fn request_locale(headers: &HeaderMap) -> String {
    negotiate(
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()),
        headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
        None,
        tenant_default_locale().as_deref(),
    )
}

/// Translate a server-emitted English string for the request's admin locale
/// (flash messages, validation errors, content checks). The English
/// source IS the catalog key (gettext-style); an untranslated string falls back
/// to itself, so wrapping a call site is always safe even before its catalog
/// entry exists. `params` are `{name}`-style interpolation pairs.
#[must_use]
pub(crate) fn tr(headers: &HeaderMap, source: &str, params: &[(&str, &str)]) -> String {
    translate(&request_locale(headers), source, params)
}

/// Count-aware server-string translation — the plural sibling of
/// [`tr`]. `key` is a count-aware source key; the form is chosen for `n` in the
/// request locale, then `params` interpolate (pass `("count", …)`). Falls back
/// to the scalar source string when no plural entry exists.
#[must_use]
pub(crate) fn tr_plural(headers: &HeaderMap, key: &str, n: i64, params: &[(&str, &str)]) -> String {
    translate_plural(&request_locale(headers), key, n, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_known_key_and_falls_back_to_source() {
        let t = admin_translator();
        // Known keys → translated.
        assert_eq!(t.translate("fr", "Forms", &[]), "Formulaires");
        assert_eq!(t.translate("de", "Pages", &[]), "Seiten");
        // Unknown key → falls back to the source string (the key itself).
        assert_eq!(
            t.translate("fr", "Totally Untranslated", &[]),
            "Totally Untranslated"
        );
        // English (source) → the key.
        assert_eq!(t.translate("en", "Pages", &[]), "Pages");
    }

    #[test]
    fn negotiate_precedence() {
        // Cookie wins over everything.
        assert_eq!(
            negotiate(
                Some("a=1; rcms_admin_lang=fr; b=2"),
                Some("de"),
                Some("pl"),
                Some("ja")
            ),
            "fr"
        );
        // No cookie → per-user pref beats Accept-Language + tenant default (#526).
        assert_eq!(negotiate(None, Some("de"), Some("pl"), Some("ja")), "pl");
        // Invalid cookie + invalid pref ignored → Accept-Language exact.
        assert_eq!(
            negotiate(
                Some("rcms_admin_lang=ru"),
                Some("de,en;q=0.9"),
                Some("xx"),
                None
            ),
            "de"
        );
        // Base-language match.
        assert_eq!(negotiate(None, Some("fr-FR,fr;q=0.9"), None, None), "fr");
        // Chinese collapses to Simplified.
        assert_eq!(negotiate(None, Some("zh-CN"), None, None), "zh-Hans");
        // No cookie/pref/Accept match → per-tenant default (#526).
        assert_eq!(negotiate(None, Some("ru,es"), None, Some("de")), "de");
        // Nothing matches → English.
        assert_eq!(negotiate(None, Some("ru,es"), Some("xx"), Some("xx")), "en");
        assert_eq!(negotiate(None, None, None, None), "en");
    }

    /// Extract every admin-template translation key. Scans both shapes:
    ///   - filter:   `"Key" | translate(...)`
    ///   - function: `translate(key="Key", ...)`
    /// across every `*.html` under `src/admin/templates`.
    fn extract_template_keys() -> std::collections::BTreeSet<String> {
        let root =
            std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src/admin/templates"));
        // Keys are single-line by convention (a multi-line wrap renders fine via
        // source fallback but isn't a tracked key) — exclude newlines so this
        // matches the grep-based authoring workflow exactly.
        let filter_re = regex::Regex::new(r#""([^"\n]*)"\s*\|\s*translate\b"#).unwrap();
        let fn_re = regex::Regex::new(r#"translate\(\s*key\s*=\s*"([^"\n]*)""#).unwrap();
        let mut keys = std::collections::BTreeSet::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read templates dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("html") {
                    continue;
                }
                let src = std::fs::read_to_string(&path).expect("read template");
                for cap in filter_re.captures_iter(&src) {
                    keys.insert(cap[1].to_string());
                }
                for cap in fn_re.captures_iter(&src) {
                    keys.insert(cap[1].to_string());
                }
            }
        }
        keys
    }

    /// Every shipped catalog parses and never maps a key to an empty
    /// string (an empty value would render blank instead of falling back).
    #[test]
    fn catalogs_parse_with_no_empty_values() {
        for (code, _) in UI_LOCALES {
            let map = parse(catalog_json(code));
            for (k, v) in &map {
                assert!(
                    !v.trim().is_empty(),
                    "[{code}] key {k:?} maps to an empty string"
                );
            }
        }
    }

    /// The CI completeness gate. Every key the admin templates actually
    /// use must have a non-empty translation in every launch locale. English is
    /// the source (its catalog may be empty — missing keys fall back to the key
    /// itself). New swept strings without translations fail here by design.
    #[test]
    fn launch_catalogs_cover_every_template_key() {
        let keys = extract_template_keys();
        assert!(
            keys.len() > 50,
            "expected the admin templates to use many translate() keys, found {} — extraction likely broken",
            keys.len()
        );
        let mut gaps: Vec<String> = Vec::new();
        for code in launch_locales() {
            let map = parse(catalog_json(code));
            for k in &keys {
                if map.get(k).map(|v| v.trim().is_empty()).unwrap_or(true) {
                    gaps.push(format!("[{code}] {k:?}"));
                }
            }
        }
        assert!(
            gaps.is_empty(),
            "{} untranslated admin string(s) across launch locales:\n{}",
            gaps.len(),
            gaps.join("\n")
        );
    }

    /// Editors never see development notes: no admin string points at a
    /// tracker issue or a pull request (#852).
    #[test]
    fn template_keys_carry_no_tracker_references() {
        let dev_note = regex::Regex::new(r"#\d|\bPR\b|follow-up").unwrap();
        let notes: Vec<String> = extract_template_keys()
            .into_iter()
            .filter(|k| dev_note.is_match(k))
            .collect();
        assert!(notes.is_empty(), "development notes in admin strings: {notes:?}");
    }

    /// Every bundled admin template parses and its inheritance
    /// chain resolves. `add_raw_templates` builds inheritance at the end, so a
    /// broken `{% extends %}`/`{% block %}` or any Tera syntax error introduced
    /// by the string sweep fails here rather than at runtime on a live page.
    #[test]
    fn all_admin_templates_parse() {
        let mut tera = tera::Tera::default();
        crate::admin::register_templates(&mut tera)
            .expect("admin templates parse + inheritance resolves");
    }

    /// The fallback guarantee: an unknown key renders the source string
    /// (the key), never a blank or a raw id, in every shipped locale.
    #[test]
    fn unknown_key_falls_back_to_source_in_every_locale() {
        let t = admin_translator();
        for (code, _) in UI_LOCALES {
            assert_eq!(
                t.translate(code, "A Definitely Untranslated String", &[]),
                "A Definitely Untranslated String"
            );
        }
    }

    /// Every plural-flash key shipped in `en.plurals.json` has a
    /// non-empty form set in every launch locale (the plural analogue of the
    /// scalar completeness gate; plural keys live in Rust, not templates).
    #[test]
    fn plural_catalogs_cover_every_key_in_all_locales() {
        let en = parse_plural(plural_catalog_json("en"));
        assert!(!en.is_empty(), "en.plurals.json is empty");
        for (code, _) in UI_LOCALES {
            let cat = parse_plural(plural_catalog_json(code));
            for key in en.keys() {
                let forms = cat
                    .get(key)
                    .unwrap_or_else(|| panic!("[{code}] missing plural key {key:?}"));
                assert!(!forms.is_empty(), "[{code}] no forms for {key:?}");
                for (form, v) in forms {
                    assert!(
                        !v.trim().is_empty(),
                        "[{code}] blank {form} form for {key:?}"
                    );
                }
            }
        }
    }

    /// The cms translator selects the correct CLDR form per locale.
    #[test]
    fn plural_flash_selects_form() {
        let t = admin_translator();
        let key = "Deleted {count} page(s).";
        let tr =
            |code: &str, n: i64| t.translate_plural(code, key, n, &[("count", &n.to_string())]);
        // English one/other.
        assert_eq!(tr("en", 1), "Deleted 1 page.");
        assert_eq!(tr("en", 3), "Deleted 3 pages.");
        // Polish one/few/many.
        assert_eq!(tr("pl", 1), "Usunięto 1 stronę.");
        assert_eq!(tr("pl", 2), "Usunięto 2 strony.");
        assert_eq!(tr("pl", 5), "Usunięto 5 stron.");
    }

    /// Registry reports expose Rust-origin metadata (title, description, column
    /// labels) that surfaces in the sidebar nav (`_base.html`) and the report
    /// page (`report.html`) via `{{ … | translate(locale=LANG) }}` on a
    /// *variable*, not a literal. `launch_catalogs_cover_every_template_key`
    /// only scans template *literals*, so it can't see these keys. Enforce
    /// catalog coverage for every built-in report's title/description/columns
    /// here — the forcing function for the Rust-origin report strings.
    #[test]
    fn report_metadata_covered_in_all_launch_locales() {
        use crate::admin::report::registered_reports;
        let mut keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for r in registered_reports() {
            keys.insert(r.title().to_string());
            if let Some(d) = r.description() {
                keys.insert(d.to_string());
            }
            for col in r.columns() {
                keys.insert(col.label.to_string());
            }
        }
        assert!(
            !keys.is_empty(),
            "no report metadata found — registry empty?"
        );
        let mut gaps: Vec<String> = Vec::new();
        for code in launch_locales() {
            let map = parse(catalog_json(code));
            for k in &keys {
                if map.get(k).map(|v| v.trim().is_empty()).unwrap_or(true) {
                    gaps.push(format!("[{code}] {k:?}"));
                }
            }
        }
        assert!(
            gaps.is_empty(),
            "{} untranslated report-metadata string(s) across launch locales:\n{}",
            gaps.len(),
            gaps.join("\n")
        );
    }

    /// The markup-escaping in [`translate`] keys off `<` in the source key, so
    /// every `translate … | safe` sink must translate a literal key that
    /// carries markup. A plain key (or a variable) piped to `| safe` would let
    /// an override inject markup unescaped (#652).
    #[test]
    fn safe_translate_sinks_use_markup_keys() {
        let root =
            std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src/admin/templates"));
        let expr_re = regex::Regex::new(r"\{\{(.*?)\}\}").unwrap();
        let key_re = regex::Regex::new(r#""([^"\n]*)"\s*\|\s*translate\b|translate(?:_plural)?\(\s*key\s*=\s*"([^"\n]*)""#).unwrap();
        let mut bad = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read templates dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("html") {
                    continue;
                }
                let src = std::fs::read_to_string(&path).expect("read template");
                for cap in expr_re.captures_iter(&src) {
                    let expr = &cap[1];
                    if !expr.contains("translate") || !expr.trim_end().ends_with("safe") {
                        continue;
                    }
                    let markup_key = key_re
                        .captures(expr)
                        .and_then(|k| k.get(1).or_else(|| k.get(2)))
                        .is_some_and(|k| k.as_str().contains('<'));
                    if !markup_key {
                        bad.push(format!("{}: {{{{{expr}}}}}", path.display()));
                    }
                }
            }
        }
        assert!(bad.is_empty(), "`| safe` on a translation without a markup key:\n{}", bad.join("\n"));
    }

    const TIP: &str = "Tip: press <code>/</code> from any admin page to jump straight to the search box.";

    fn overrides(rows: &[(&str, &str, &str)]) -> Arc<Overrides> {
        let mut map = Overrides::new();
        for (locale, key, value) in rows {
            map.entry(Locale::new(*locale))
                .or_default()
                .insert((*key).to_owned(), (*value).to_owned());
        }
        Arc::new(map)
    }

    /// An admin Tera with the real bindings and one template that renders a
    /// markup key through `| safe` and a plain key through autoescape.
    fn admin_tera() -> tera::Tera {
        let mut tera = tera::Tera::default();
        crate::admin::register_templates(&mut tera).expect("admin templates");
        tera.add_raw_template(
            "probe.html",
            r#"{{ "Tip: press <code>/</code> from any admin page to jump straight to the search box." | translate(locale=LANG) | safe }}|{{ translate(key="Pages", locale=LANG) }}"#,
        )
        .expect("probe template");
        tera
    }

    fn render(tera: &tera::Tera, lang: &str) -> String {
        let mut ctx = tera::Context::new();
        ctx.insert("LANG", lang);
        tera.render("probe.html", &ctx).expect("render")
    }

    /// An override reaches only the render scoped to its tenant, its markup
    /// is escaped at a `| safe` sink, and a plain key keeps its text.
    #[test]
    fn overrides_are_scoped_to_their_tenant_and_escaped() {
        let tera = admin_tera();
        let a = overrides(&[
            ("en", TIP, "<img src=x onerror=alert(1)>"),
            ("en", "Pages", "Stories & posts"),
        ]);
        let in_a = ACTIVE_OVERRIDES.sync_scope(a, || render(&tera, "en"));
        assert_eq!(in_a, "&lt;img src=x onerror=alert(1)&gt;|Stories &amp; posts");
        let in_b = ACTIVE_OVERRIDES.sync_scope(overrides(&[]), || render(&tera, "en"));
        assert_eq!(in_b, format!("{TIP}|Pages"));
        assert_eq!(render(&tera, "en"), in_b, "no tenant scope → shipped catalog");
    }

    /// The regression for #652, end to end from each tenant's own database:
    /// tenant A's override never shows in tenant B's admin render, nor in
    /// B's server strings.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn tenant_a_override_does_not_reach_tenant_b() {
        let pool_a = rustango::sql::Pool::connect("sqlite::memory:").await.expect("pool a");
        let pool_b = rustango::sql::Pool::connect("sqlite::memory:").await.expect("pool b");
        for pool in [&pool_a, &pool_b] {
            rustango::i18n::db::ensure_table_pool(pool).await.expect("translations table");
        }
        rustango::i18n::db::upsert_pool(&pool_a, "fr", TIP, "<script>steal()</script>", "a-admin")
            .await
            .expect("override");
        rustango::i18n::db::upsert_pool(&pool_a, "fr", "Pages", "Articles", "a-admin")
            .await
            .expect("override");
        let tera = admin_tera();
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, format!("{ADMIN_LANG_COOKIE}=fr").parse().unwrap());

        let a = tenant_overrides("i18n-652-a", &pool_a).await;
        let (page_a, flash_a) = ACTIVE_OVERRIDES
            .scope(a, async { (render(&tera, "fr"), tr(&headers, "Pages", &[])) })
            .await;
        assert_eq!(page_a, "&lt;script&gt;steal()&lt;&#x2F;script&gt;|Articles");
        assert_eq!(flash_a, "Articles");

        let b = tenant_overrides("i18n-652-b", &pool_b).await;
        let (page_b, flash_b) = ACTIVE_OVERRIDES
            .scope(b, async { (render(&tera, "fr"), tr(&headers, "Pages", &[])) })
            .await;
        let shipped = admin_translator();
        assert_eq!(
            page_b,
            format!("{}|{}", shipped.translate("fr", TIP, &[]), shipped.translate("fr", "Pages", &[]))
        );
        assert_eq!(flash_b, shipped.translate("fr", "Pages", &[]));
        assert!(!page_b.contains("steal"));
    }
}
