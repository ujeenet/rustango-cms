//! Admin UI i18n foundation (#524 / epic #523).
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
//! English, #525/#526), server-string translation ([`tr`], #528), the embedded
//! [`admin_translator`], and the completeness/extraction tooling (#529, tests).
//! See `docs/i18n-admin.md` for the authoring workflow.

use crate::log_err::LogErr as _;
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

/// Resolve the active admin UI locale. Precedence (#525 + #526):
///   1. sticky `rcms_admin_lang` cookie (explicit, most recent choice)
///   2. durable per-user preference (`rustango_users.data.admin_lang`, #526)
///   3. `Accept-Language` — exact, then base-language (`fr-FR`→`fr`, `zh-*`→`zh-Hans`)
///   4. per-tenant default (#526)
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

/// Per-tenant default admin locale (#526). Sourced from
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

/// Parse a plural catalog: `key → (CLDR category → template)` (#528/#1102).
fn parse_plural(json: &str) -> HashMap<String, HashMap<String, String>> {
    serde_json::from_str(json).unwrap_or_default()
}

/// The shipped catalog JSON for `code` (source-embedded, same set as
/// [`admin_translator`]). Returns `"{}"` for unknown codes. Kept here so the
/// completeness tooling (#529) and the translator build read one source.
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

/// The shipped **plural** catalog JSON for `code` (#528/#1102) — count-aware
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
/// except the English source. Drives the completeness check (#529).
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

/// Process-wide cached admin translator — THE shared instance used by both the
/// Tera `translate`/`translate_plural` bindings (via `register_templates`) and
/// the server-string `tr`/`tr_plural` helpers, so a DB override applied to it is
/// visible everywhere. The embedded catalogs are immutable; only the override
/// layer (#532) mutates, via [`maybe_refresh_overrides`].
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
/// overwritten. Only the catalog layer is touched — the DB override layer
/// (#532, `maybe_refresh_overrides`) is independent and keeps working.
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

/// Live admin-translation overrides (#532): reload the editable DB layer
/// (`rustango_translations`) into the shared translator, at most once per TTL.
/// Called from `add_chrome`, so an operator's edit in the admin takes effect
/// within ~10s without a redeploy. Best-effort — a missing table or query error
/// just leaves the shipped file catalogs in force.
///
/// The override map is process-wide (admin chrome is product-global). In a
/// multi-tenant deployment it reflects whichever tenant most recently rendered
/// an admin page; per-tenant admin translations would need a per-tenant
/// translator (future work).
pub(crate) async fn maybe_refresh_overrides(pool: &rustango::sql::Pool) {
    use std::time::{Duration, Instant};
    const REFRESH_TTL: Duration = Duration::from_secs(10);
    static LAST: OnceLock<std::sync::Mutex<Option<Instant>>> = OnceLock::new();
    let slot = LAST.get_or_init(|| std::sync::Mutex::new(None));
    {
        let mut guard = slot.lock().expect("override-refresh clock poisoned");
        if matches!(*guard, Some(t) if t.elapsed() < REFRESH_TTL) {
            return;
        }
        // Claim the slot before the await so concurrent renders don't stampede.
        *guard = Some(Instant::now());
    }
    rustango::i18n::db::refresh_overrides_pool(cached_translator(), pool)
        .await
        .log_warn("admin translation overrides not refreshed");
}

/// Translate a server-emitted English string for the request's admin locale
/// (#528 — flash messages, validation errors, content checks). The English
/// source IS the catalog key (gettext-style); an untranslated string falls back
/// to itself, so wrapping a call site is always safe even before its catalog
/// entry exists. `params` are `{name}`-style interpolation pairs.
#[must_use]
pub(crate) fn tr(headers: &HeaderMap, source: &str, params: &[(&str, &str)]) -> String {
    let lang = negotiate(
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()),
        headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
        None,
        tenant_default_locale().as_deref(),
    );
    cached_translator().translate(&lang, source, params)
}

/// Count-aware server-string translation (#528/#1102) — the plural sibling of
/// [`tr`]. `key` is a count-aware source key; the form is chosen for `n` in the
/// request locale, then `params` interpolate (pass `("count", …)`). Falls back
/// to the scalar source string when no plural entry exists.
#[must_use]
pub(crate) fn tr_plural(headers: &HeaderMap, key: &str, n: i64, params: &[(&str, &str)]) -> String {
    let lang = negotiate(
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()),
        headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
        None,
        tenant_default_locale().as_deref(),
    );
    cached_translator().translate_plural(&lang, key, n, params)
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

    /// Extract every admin-template translation key (#529). Scans both shapes:
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

    /// #529 — every shipped catalog parses and never maps a key to an empty
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

    /// #529 — the CI completeness gate. Every key the admin templates actually
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

    /// #527/#529 — every bundled admin template parses and its inheritance
    /// chain resolves. `add_raw_templates` builds inheritance at the end, so a
    /// broken `{% extends %}`/`{% block %}` or any Tera syntax error introduced
    /// by the string sweep fails here rather than at runtime on a live page.
    #[test]
    fn all_admin_templates_parse() {
        let mut tera = tera::Tera::default();
        crate::admin::register_templates(&mut tera)
            .expect("admin templates parse + inheritance resolves");
    }

    /// #529 — the fallback guarantee: an unknown key renders the source string
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

    /// #528/#1102 — every plural-flash key shipped in `en.plurals.json` has a
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

    /// #1102 — the cms translator selects the correct CLDR form per locale.
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
}
