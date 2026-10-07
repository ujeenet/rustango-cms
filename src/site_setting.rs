//! Per-tenant site settings.
//!
//! Editor-managed singleton key-value rows. Templates read them via
//! the `{{ site_setting(scope='footer') }}` Tera function; admins
//! manage them under Settings → Site settings.
//!
//! Each `scope` (e.g. "footer", "contact", "social") gets exactly one
//! row with a JSON value column. Two editing modes share that storage:
//!
//! - **Free-form** (default): editors type raw JSON in a textarea.
//!   Covers ad-hoc scopes with zero host boilerplate.
//! - **Typed**: a host crate registers a [`SiteSettingSchema`]
//!   for a scope via [`crate::register_site_setting!`], declaring a
//!   list of [`Widget`]s. The admin then renders a real form (the same
//!   `_widget.html` machinery page-type fields use), validates required
//!   fields, and coerces each input back into the `value_json` object
//!   keyed by field `name`. Templates read both modes identically:
//!   `{{ site_setting(scope='footer').site_name }}`.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

use crate::widget::{Widget, WidgetKind};

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_site_setting",
    app = "cms",
    display = "scope",
    admin(list_display = "scope, updated_at", ordering = "scope",)
)]
pub struct SiteSetting {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Stable identifier for this settings row — e.g. `"footer"`,
    /// `"contact"`, `"social"`. Application-enforced UNIQUE within
    /// the tenant (avoids SQL composite-unique dialect drift).
    #[rustango(max_length = 64, index)]
    pub scope: String,

    /// Free-form JSON object. Tera templates read via
    /// `{{ site_setting(scope='footer').field }}`. Empty object
    /// `{}` is a valid initial state.
    pub value_json: serde_json::Value,

    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

/// Fetch a setting row by scope. Returns `None` when no row exists —
/// templates should treat absence as "settings haven't been
/// configured yet" rather than erroring.
///
/// # Errors
/// Driver / query failures.
pub async fn get(
    pool: &rustango::sql::Pool,
    scope: &str,
) -> Result<Option<SiteSetting>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<SiteSetting> = SiteSetting::objects()
        .where_(SiteSetting::scope.eq(scope.to_owned()))
        .fetch(pool)
        .await?;
    Ok(rows.into_iter().next())
}

/// UPSERT a setting by scope.
///
/// # Errors
/// Driver / query failures.
pub async fn upsert(
    pool: &rustango::sql::Pool,
    scope: &str,
    value: serde_json::Value,
) -> Result<SiteSetting, rustango::sql::ExecError> {
    if let Some(mut row) = get(pool, scope).await? {
        row.value_json = value;
        row.save_pool(pool).await?;
        return Ok(row);
    }
    let mut row = SiteSetting {
        id: Auto::Unset,
        scope: scope.to_owned(),
        value_json: value,
        updated_at: Auto::Unset,
    };
    row.insert_pool(pool).await?;
    Ok(row)
}

/// List every setting row in the tenant, sorted by scope.
///
/// # Errors
/// Driver / query failures.
pub async fn list_all(
    pool: &rustango::sql::Pool,
) -> Result<Vec<SiteSetting>, rustango::sql::ExecError> {
    use rustango::sql::FetcherPool as _;
    SiteSetting::objects()
        .order_by(&[("scope", false)])
        .fetch(pool)
        .await
}

// ---- #406 — typed settings registry ------------------------------

/// A typed schema for one settings `scope`: a human label + the list
/// of typed fields editors fill in (instead of raw JSON). Host crates
/// register one per scope via [`crate::register_site_setting!`].
///
/// `fields` returns a *fresh* `Vec<Widget>` each call so the admin can
/// prefill values into the clones without mutating shared state. Each
/// field `name` becomes a key in the stored `value_json` object, and
/// its [`WidgetKind`] drives both the rendered input and how the
/// submitted string is coerced back into JSON (see
/// [`value_json_from_form`]).
pub struct SiteSettingSchema {
    pub scope: &'static str,
    pub label: &'static str,
    pub fields: fn() -> Vec<Widget>,
}

inventory::collect!(SiteSettingSchema);

/// The registered schema for `scope`, if any. Unregistered scopes fall
/// back to the free-form JSON editor.
#[must_use]
pub fn schema_for(scope: &str) -> Option<&'static SiteSettingSchema> {
    inventory::iter::<SiteSettingSchema>
        .into_iter()
        .find(|s| s.scope == scope)
}

/// Every registered schema, sorted by scope. The admin list surfaces
/// these even before a row exists, so typed settings are always
/// discoverable in the settings menu.
#[must_use]
pub fn registered_schemas() -> Vec<&'static SiteSettingSchema> {
    let mut v: Vec<_> = inventory::iter::<SiteSettingSchema>.into_iter().collect();
    v.sort_by_key(|s| s.scope);
    v
}

/// Register a typed schema for a site-settings `scope`.
///
/// ```ignore
/// use rustango_cms::widget::{Widget, WidgetKind};
/// rustango_cms::register_site_setting!("footer", "Footer", || vec![
///     Widget::new(WidgetKind::Text, "site_name", "Site name").required(),
///     Widget::new(WidgetKind::Textarea, "copyright", "Copyright notice"),
///     Widget::new(WidgetKind::Boolean, "show_social", "Show social links"),
/// ]);
/// ```
///
/// The admin then renders typed inputs for `footer` instead of a JSON
/// textarea, enforces `required` fields, and stores each value under
/// its field `name` in `value_json`. Templates are unchanged:
/// `{{ site_setting(scope='footer').site_name }}`.
#[macro_export]
macro_rules! register_site_setting {
    ($scope:expr, $label:expr, $fields:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::site_setting::SiteSettingSchema {
                scope: $scope,
                label: $label,
                fields: $fields,
            }
        }
    };
}

/// Clone `fields` and prefill each widget's `value` from the matching
/// key in `stored` (the row's `value_json`). The inverse of
/// [`value_json_from_form`]:
/// - booleans → `"on"` when truthy (so the checkbox renders checked),
/// - multi-value widgets → the JSON-array string the editor expects,
/// - everything else → the scalar without JSON quoting (strings
///   as-is, numbers as their decimal text).
#[must_use]
pub fn prefill_widgets(fields: Vec<Widget>, stored: &serde_json::Value) -> Vec<Widget> {
    fields
        .into_iter()
        .map(|mut w| {
            if let Some(v) = stored.get(&w.name) {
                w.value = json_to_widget_value(w.kind, v);
            }
            w
        })
        .collect()
}

fn json_to_widget_value(kind: WidgetKind, v: &serde_json::Value) -> String {
    if kind == WidgetKind::Boolean {
        let truthy = v.as_bool().unwrap_or(false)
            || v.as_str()
                .is_some_and(|s| s == "on" || s == "true" || s == "1")
            || v.as_i64().is_some_and(|n| n != 0);
        return if truthy {
            "on".to_owned()
        } else {
            String::new()
        };
    }
    if kind.is_multi_value() {
        return match v {
            // Stored as a JSON array → hand the editor the array STRING
            // it round-trips through the hidden multi-value input.
            serde_json::Value::Array(_) => v.to_string(),
            serde_json::Value::String(s) => s.clone(),
            _ => String::new(),
        };
    }
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        // Numbers/bools rendered as their bare text (no JSON quoting).
        other => other.to_string(),
    }
}

/// Build a `value_json` object from a submitted form map, coercing
/// each value by its widget kind:
/// - numeric kinds → a JSON number when parseable (blank = dropped,
///   unparseable = kept as a string so nothing is silently lost),
/// - booleans → `true` only when the checkbox key is present,
/// - multi-value kinds → the posted JSON-array string parsed to an
///   array (falls back to a string on parse failure),
/// - everything else → a JSON string (empty strings are kept so a
///   template can tell "set to empty" from "never set").
///
/// Keys absent from `fields` are ignored, so the CSRF token and any
/// stray inputs never leak into the stored value.
#[must_use]
pub fn value_json_from_form(
    fields: &[Widget],
    form: &std::collections::HashMap<String, String>,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for w in fields {
        if w.kind == WidgetKind::Boolean {
            obj.insert(
                w.name.clone(),
                serde_json::Value::Bool(form.contains_key(&w.name)),
            );
            continue;
        }
        let Some(raw) = form.get(&w.name) else {
            continue;
        };
        let value = match w.kind {
            WidgetKind::Integer => {
                let t = raw.trim();
                if t.is_empty() {
                    continue;
                }
                t.parse::<i64>().map_or_else(
                    |_| serde_json::Value::String(t.to_owned()),
                    |n| serde_json::json!(n),
                )
            }
            WidgetKind::Number | WidgetKind::Float | WidgetKind::Range => {
                let t = raw.trim();
                if t.is_empty() {
                    continue;
                }
                t.parse::<f64>().map_or_else(
                    |_| serde_json::Value::String(t.to_owned()),
                    |n| serde_json::json!(n),
                )
            }
            _ if w.kind.is_multi_value() => serde_json::from_str::<serde_json::Value>(raw)
                .unwrap_or_else(|_| serde_json::Value::String(raw.clone())),
            _ => serde_json::Value::String(raw.clone()),
        };
        obj.insert(w.name.clone(), value);
    }
    serde_json::Value::Object(obj)
}

/// Labels of `required` fields left blank in `form`. The submit
/// handler turns a non-empty result into a validation error. Boolean
/// (checkbox) widgets are skipped — "required checkbox" has no settled
/// UX here and a missing key legitimately means `false`.
#[must_use]
pub fn missing_required(
    fields: &[Widget],
    form: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    fields
        .iter()
        .filter(|w| w.required && w.kind != WidgetKind::Boolean)
        .filter(|w| form.get(&w.name).is_none_or(|s| s.trim().is_empty()))
        .map(|w| w.label.clone())
        .collect()
}

// ---- #245 — `site_setting(scope=…)` Tera helper -------------------

/// Pre-fetch every site-setting row + group them into a
/// `scope → value_json` map. Public-render installs this map in a
/// thread-local that the `site_setting(scope=…)` Tera function
/// reads. Bounded by the count of distinct scopes (typically tens),
/// so one query per render covers every reference.
///
/// # Errors
/// None — failures degrade to an empty map; templates see
/// `Value::Null` for every scope lookup.
pub async fn prefetch_for_render(
    pool: &rustango::sql::Pool,
    locale_code: Option<&str>,
) -> std::collections::HashMap<String, serde_json::Value> {
    let rows = list_all(pool).await.unwrap_or_default();
    rows.into_iter()
        .map(|r| (r.scope, localize(&r.value_json, locale_code)))
        .collect()
}

// ---- per-language values ------------------------------------------

/// Key inside `value_json` holding translations: `{"fr": {"footer": "…"}}`.
/// Kept in the same row so settings need no extra table; templates never
/// see it (see [`localize`]).
pub const I18N_KEY: &str = "_i18n";

/// A setting's value as a template sees it in `locale` (a locale code;
/// `None` = the default language): the translated text fields laid over
/// the original, empty translations ignored, and the `_i18n` bag removed.
#[must_use]
pub fn localize(value: &serde_json::Value, locale: Option<&str>) -> serde_json::Value {
    let Some(obj) = value.as_object() else {
        return value.clone();
    };
    let mut out = obj.clone();
    let bag = out.remove(I18N_KEY);
    if let Some(tr) = locale.and_then(|code| bag.as_ref()?.get(code)?.as_object().cloned()) {
        for (k, v) in tr {
            if v.as_str().is_some_and(|t| !t.trim().is_empty()) {
                out.insert(k, v);
            }
        }
    }
    serde_json::Value::Object(out)
}

/// The `locale` translations stored in `value`, by field key.
#[must_use]
pub fn translations(value: &serde_json::Value, locale: &str) -> serde_json::Map<String, serde_json::Value> {
    value
        .get(I18N_KEY)
        .and_then(|b| b.get(locale))
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Replace the `locale` translations in `value` with `fields` (empty
/// strings dropped; no fields left → the language is removed).
pub fn set_translations(
    value: &mut serde_json::Value,
    locale: &str,
    fields: serde_json::Map<String, serde_json::Value>,
) {
    if !value.is_object() {
        *value = serde_json::json!({});
    }
    let obj = value.as_object_mut().expect("object above");
    let bag = obj
        .entry(I18N_KEY)
        .or_insert_with(|| serde_json::json!({}));
    if !bag.is_object() {
        *bag = serde_json::json!({});
    }
    let bag = bag.as_object_mut().expect("object above");
    let kept: serde_json::Map<String, serde_json::Value> = fields
        .into_iter()
        .filter(|(_, v)| v.as_str().is_some_and(|t| !t.trim().is_empty()))
        .collect();
    if kept.is_empty() {
        bag.remove(locale);
    } else {
        bag.insert(locale.to_owned(), serde_json::Value::Object(kept));
    }
    if bag.is_empty() {
        obj.remove(I18N_KEY);
    }
}

/// Carry `old`'s translations over to `new` — the typed form rebuilds the
/// whole value from its fields and must not drop them.
pub fn keep_translations(old: &serde_json::Value, new: &mut serde_json::Value) {
    if let (Some(bag), Some(obj)) = (old.get(I18N_KEY), new.as_object_mut()) {
        obj.insert(I18N_KEY.to_owned(), bag.clone());
    }
}

thread_local! {
    /// Per-render `scope → value_json` map. Installed by
    /// [`install`], cleared on guard drop.
    static CURRENT_SITE_SETTINGS: std::cell::RefCell<
        Option<std::sync::Arc<std::collections::HashMap<String, serde_json::Value>>>,
    > = const { std::cell::RefCell::new(None) };
}

/// RAII guard for the site-settings map. Returned by [`install`];
/// drop clears the thread-local.
#[must_use = "the guard clears the thread-local on drop; bind it to a local"]
pub struct SiteSettingsGuard {
    _priv: (),
}

impl Drop for SiteSettingsGuard {
    fn drop(&mut self) {
        CURRENT_SITE_SETTINGS.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

/// Install a pre-fetched `scope → value_json` map for the duration
/// of the returned guard. The `site_setting(scope=…)` Tera function
/// reads against this map.
pub fn install(map: std::collections::HashMap<String, serde_json::Value>) -> SiteSettingsGuard {
    CURRENT_SITE_SETTINGS.with(|cell| {
        *cell.borrow_mut() = Some(std::sync::Arc::new(map));
    });
    SiteSettingsGuard { _priv: () }
}

/// Register the `site_setting(scope=…)` Tera function. Wired from
/// [`crate::urls::register_tera_helpers`].
pub fn register_tera_function(tera: &mut tera::Tera) {
    tera.register_function("site_setting", SiteSettingFn);
}

struct SiteSettingFn;

impl tera::Function for SiteSettingFn {
    fn call(
        &self,
        args: &std::collections::HashMap<String, tera::Value>,
    ) -> tera::Result<tera::Value> {
        let scope = args
            .get("scope")
            .and_then(tera::Value::as_str)
            .unwrap_or("");
        if scope.is_empty() {
            return Ok(tera::Value::Null);
        }
        let value = CURRENT_SITE_SETTINGS.with(|cell| {
            cell.borrow()
                .as_ref()
                .and_then(|m| m.get(scope).cloned())
                .unwrap_or(tera::Value::Null)
        });
        Ok(value)
    }
}

#[cfg(test)]
mod schema_tests {

    #[test]
    fn localize_overlays_text_and_hides_the_bag() {
        let v = serde_json::json!({
            "shop_name": "Clay & Kiln", "footer": "Made by hand.",
            "_i18n": { "fr": { "footer": "Fait main.", "shop_name": "" } }
        });
        assert_eq!(super::localize(&v, Some("fr")), serde_json::json!({"shop_name": "Clay & Kiln", "footer": "Fait main."}));
        assert_eq!(super::localize(&v, None), serde_json::json!({"shop_name": "Clay & Kiln", "footer": "Made by hand."}));
        assert_eq!(super::localize(&v, Some("de")), super::localize(&v, None));
    }

    #[test]
    fn set_and_keep_translations() {
        let mut v = serde_json::json!({"footer": "Made by hand."});
        let mut f = serde_json::Map::new();
        f.insert("footer".into(), serde_json::json!("Fait main."));
        f.insert("shop_name".into(), serde_json::json!("  "));
        super::set_translations(&mut v, "fr", f);
        assert_eq!(super::translations(&v, "fr"), serde_json::json!({"footer": "Fait main."}).as_object().unwrap().clone());
        let mut saved = serde_json::json!({"footer": "Made by hand, fired with care."});
        super::keep_translations(&v, &mut saved);
        assert_eq!(saved["_i18n"]["fr"]["footer"], "Fait main.");
        super::set_translations(&mut saved, "fr", serde_json::Map::new());
        assert!(saved.get("_i18n").is_none(), "no languages left → no bag");
    }

    use super::*;
    use std::collections::HashMap;

    // A throwaway typed schema registered into the test binary's
    // inventory so `schema_for` / `registered_schemas` have something
    // to find. Real hosts call `register_site_setting!` the same way.
    fn footer_fields() -> Vec<Widget> {
        vec![
            Widget::new(WidgetKind::Text, "site_name", "Site name").required(),
            Widget::new(WidgetKind::Integer, "since", "Founded year"),
            Widget::new(WidgetKind::Float, "rating", "Rating"),
            Widget::new(WidgetKind::Boolean, "show_social", "Show social links"),
            Widget::new(WidgetKind::MultiSelect, "networks", "Networks"),
        ]
    }
    crate::register_site_setting!("__test_footer", "Test footer", footer_fields);

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn schema_for_finds_registered_and_ignores_unknown() {
        let s = schema_for("__test_footer").expect("registered schema");
        assert_eq!(s.label, "Test footer");
        assert_eq!((s.fields)().len(), 5);
        assert!(schema_for("definitely-not-registered").is_none());
        assert!(registered_schemas()
            .iter()
            .any(|s| s.scope == "__test_footer"));
    }

    #[test]
    fn form_coerces_values_by_widget_kind() {
        let fields = footer_fields();
        // `show_social` checkbox present, CSRF + an unknown key present.
        let f = form(&[
            ("site_name", "Eugene Co"),
            ("since", "2014"),
            ("rating", "4.5"),
            ("show_social", "on"),
            ("networks", r#"["x","mastodon"]"#),
            ("csrf_token", "tok"),
            ("stray", "ignored"),
        ]);
        let v = value_json_from_form(&fields, &f);
        assert_eq!(v["site_name"], serde_json::json!("Eugene Co"));
        assert_eq!(v["since"], serde_json::json!(2014)); // integer, not "2014"
        assert_eq!(v["rating"], serde_json::json!(4.5)); // float
        assert_eq!(v["show_social"], serde_json::json!(true)); // checkbox present
        assert_eq!(v["networks"], serde_json::json!(["x", "mastodon"])); // parsed array
                                                                         // Keys outside the schema never leak into storage.
        assert!(v.get("csrf_token").is_none());
        assert!(v.get("stray").is_none());
    }

    #[test]
    fn absent_checkbox_is_false_blank_number_dropped() {
        let fields = footer_fields();
        // No `show_social` key (unchecked) and an empty `since`.
        let v = value_json_from_form(&fields, &form(&[("site_name", "X"), ("since", "")]));
        assert_eq!(v["show_social"], serde_json::json!(false));
        assert!(v.get("since").is_none()); // blank numeric → dropped
    }

    #[test]
    fn unparseable_number_kept_as_string_not_lost() {
        let fields = footer_fields();
        let v = value_json_from_form(&fields, &form(&[("site_name", "X"), ("since", "MMXIV")]));
        assert_eq!(v["since"], serde_json::json!("MMXIV"));
    }

    #[test]
    fn prefill_round_trips_through_widget_value() {
        let stored = serde_json::json!({
            "site_name": "Eugene Co",
            "since": 2014,
            "show_social": true,
            "networks": ["x", "mastodon"],
        });
        let widgets = prefill_widgets(footer_fields(), &stored);
        let by_name = |n: &str| widgets.iter().find(|w| w.name == n).unwrap().value.clone();
        assert_eq!(by_name("site_name"), "Eugene Co");
        assert_eq!(by_name("since"), "2014"); // number → bare text
        assert_eq!(by_name("show_social"), "on"); // truthy → checkbox checked
        assert_eq!(by_name("networks"), r#"["x","mastodon"]"#); // array → JSON string
    }

    #[test]
    fn missing_required_flags_blank_required_text_only() {
        let fields = footer_fields();
        // site_name required + blank → flagged; integer/bool not required.
        let missing = missing_required(&fields, &form(&[("site_name", "  ")]));
        assert_eq!(missing, vec!["Site name".to_owned()]);
        // Provided → no complaint.
        assert!(missing_required(&fields, &form(&[("site_name", "ok")])).is_empty());
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use std::collections::HashMap;
    use tera::{Context, Tera};

    fn fresh_tera() -> Tera {
        let mut tera = Tera::default();
        register_tera_function(&mut tera);
        tera
    }

    fn render(tera: &Tera, src: &str) -> String {
        let mut t = tera.clone();
        t.add_raw_template("t.html", src).unwrap();
        t.render("t.html", &Context::new()).unwrap()
    }

    #[test]
    fn returns_null_when_nothing_installed() {
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{% set v = site_setting(scope="footer") %}{% if v %}has{% else %}none{% endif %}"#,
        );
        assert_eq!(out, "none");
    }

    #[test]
    fn resolves_known_scope_value_object() {
        let mut m = HashMap::new();
        m.insert(
            "footer".to_owned(),
            serde_json::json!({"site_name": "Eugene", "copyright": 2026}),
        );
        let _g = install(m);
        let tera = fresh_tera();
        // Tera only allows field-access on a NAMED variable, not on
        // the result of a function call — assign once, then read.
        let out = render(
            &tera,
            r#"{% set footer = site_setting(scope="footer") %}{{ footer.site_name }}-{{ footer.copyright }}"#,
        );
        assert_eq!(out, "Eugene-2026");
    }

    #[test]
    fn missing_scope_returns_null() {
        let mut m = HashMap::new();
        m.insert("footer".to_owned(), serde_json::json!({"x": 1}));
        let _g = install(m);
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{% set v = site_setting(scope="missing") %}{% if v %}has{% else %}none{% endif %}"#,
        );
        assert_eq!(out, "none");
    }

    #[test]
    fn empty_scope_arg_returns_null() {
        let mut m = HashMap::new();
        m.insert("footer".to_owned(), serde_json::json!({}));
        let _g = install(m);
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{% set v = site_setting(scope="") %}{% if v %}has{% else %}none{% endif %}"#,
        );
        assert_eq!(out, "none");
    }

    #[test]
    fn guard_clears_thread_local() {
        {
            let mut m = HashMap::new();
            m.insert("x".to_owned(), serde_json::json!({"v": 1}));
            let _g = install(m);
            let tera = fresh_tera();
            assert_eq!(
                render(&tera, r#"{% set s = site_setting(scope="x") %}{{ s.v }}"#,),
                "1",
            );
        }
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{% set v = site_setting(scope="x") %}{% if v %}has{% else %}none{% endif %}"#,
        );
        assert_eq!(out, "none");
    }
}
