//! Public language-switcher data for templates (#401).
//!
//! The admin has a locale picker, but visitors had no built-in way to
//! switch language. This exposes a `language_switcher()` Tera function
//! returning the active locales — each with the URL to view the
//! *current* page in that locale, built per the configured
//! [`crate::locale_mode::LocaleMode`]:
//!
//! ```html
//! <nav aria-label="Language">
//!   {% for loc in language_switcher() %}
//!     <a href="{{ loc.url }}"{% if loc.is_current %} aria-current="page"{% endif %}>
//!       {{ loc.name }}</a>
//!   {% endfor %}
//! </nav>
//! ```
//!
//! Like [`crate::page_url`], the per-request data is stashed in a
//! thread-local installed by the public render handler (it owns the
//! pool + the resolved locale + the mount prefix) and cleared by the
//! returned guard when `tera.render` finishes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::locale::Locale;
use crate::locale_mode::LocaleMode;

/// One entry in the switcher: a locale + the URL to the current page in
/// it, and whether it's the locale the request resolved to.
#[derive(Debug, Clone, Serialize)]
pub struct SwitcherEntry {
    pub code: String,
    pub name: String,
    pub url: String,
    pub is_current: bool,
    /// This locale is the tenant default (its switcher URL carries a
    /// `?lang=` cookie-reset query; hreflang uses the clean path instead).
    pub is_default: bool,
}

thread_local! {
    static CURRENT_SWITCHER: RefCell<Option<Arc<Vec<SwitcherEntry>>>> = const { RefCell::new(None) };
    /// Entries handed off by the router *before* the render future is
    /// polled. Read synchronously at the top of `render::render_inner`
    /// (same thread as the caller, before its first `.await`) into a
    /// local, then re-`install`ed inside render's await-free block so
    /// the thread-local is set on the *same* thread that runs
    /// `tera.render` — surviving tokio's thread-hops. See the
    /// [`stash_pending`] / [`take_pending`] pair.
    static PENDING_SWITCHER: RefCell<Option<Vec<SwitcherEntry>>> = const { RefCell::new(None) };
}

/// RAII guard — clears the thread-local on drop. Returned by [`install`].
pub struct SwitcherGuard {
    _priv: (),
}

impl Drop for SwitcherGuard {
    fn drop(&mut self) {
        CURRENT_SWITCHER.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

/// Install the switcher entries for the duration of the returned guard.
#[must_use]
pub fn install(entries: Vec<SwitcherEntry>) -> SwitcherGuard {
    CURRENT_SWITCHER.with(|cell| {
        *cell.borrow_mut() = Some(Arc::new(entries));
    });
    SwitcherGuard { _priv: () }
}

/// RAII guard clearing [`PENDING_SWITCHER`] on drop — the router holds
/// it across the render future so a stashed hand-off never leaks onto a
/// pooled worker thread if the render short-circuits before consuming it.
pub struct PendingGuard {
    _priv: (),
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        PENDING_SWITCHER.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

/// Stash entries for the *upcoming* render on this thread and return a
/// guard that clears them on drop. Called by the public render handler
/// immediately before it polls the render future (no `.await` in
/// between), so the future's first synchronous slice — the top of
/// [`crate::render`]'s `render_inner`, which calls [`take_pending`] —
/// runs on this same thread and sees them.
#[must_use]
pub fn stash_pending(entries: Vec<SwitcherEntry>) -> PendingGuard {
    PENDING_SWITCHER.with(|cell| {
        *cell.borrow_mut() = Some(entries);
    });
    PendingGuard { _priv: () }
}

/// Take (and clear) any entries [`stash_pending`]'d for this render.
/// Called at the very top of `render_inner`, before its first `.await`,
/// so the returned `Vec` becomes a plain local that travels with the
/// future across tokio thread-hops and can be [`install`]ed in render's
/// await-free block right before `tera.render`.
#[must_use]
pub fn take_pending() -> Option<Vec<SwitcherEntry>> {
    PENDING_SWITCHER.with(|cell| cell.borrow_mut().take())
}

/// Register the `language_switcher()` Tera function.
pub fn register_tera_function(tera: &mut tera::Tera) {
    tera.register_function("language_switcher", switcher_fn);
}

fn switcher_fn(_args: &HashMap<String, tera::Value>) -> tera::Result<tera::Value> {
    let entries = CURRENT_SWITCHER.with(|cell| cell.borrow().clone());
    match entries {
        Some(list) => tera::to_value(&*list).map_err(tera::Error::json),
        None => Ok(tera::Value::Array(Vec::new())),
    }
}

/// Register the `rcms_hreflang_tags(origin=…)` Tera function.
pub fn register_hreflang_function(tera: &mut tera::Tera) {
    tera.register_function("rcms_hreflang_tags", hreflang_fn);
}

/// `rcms_hreflang_tags(origin="https://site.tld")` → the `<head>`
/// `<link rel="alternate" hreflang=…>` cluster for the current page across
/// every active locale, plus `x-default` → the default locale. Emitted so
/// SEO crawlers / audit tools that read the rendered page head (not just
/// the sitemap) discover + bidirectionally link the language versions.
///
/// Uses the same per-request switcher entries the render handler installs
/// (built per the configured [`crate::locale_mode::LocaleMode`]); the
/// default-locale entry's `?lang=` cookie-reset query is stripped so the
/// hreflang URL matches the clean canonical. Returns an empty string when
/// fewer than two locales are active (single-language site → no hreflang).
/// `href`s are absolute when `origin` is a non-empty absolute URL.
fn hreflang_fn(args: &HashMap<String, tera::Value>) -> tera::Result<tera::Value> {
    let origin = args
        .get("origin")
        .and_then(tera::Value::as_str)
        .unwrap_or("")
        .trim_end_matches('/');
    let entries = CURRENT_SWITCHER.with(|cell| cell.borrow().clone());
    let Some(entries) = entries else {
        return Ok(tera::Value::String(String::new()));
    };
    if entries.len() < 2 {
        return Ok(tera::Value::String(String::new()));
    }
    let href_for = |e: &SwitcherEntry| -> String {
        let path = if e.is_default {
            e.url.split('?').next().unwrap_or(e.url.as_str())
        } else {
            e.url.as_str()
        };
        format!("{origin}{path}")
    };
    let mut out = String::with_capacity(entries.len() * 96);
    let mut x_default: Option<String> = None;
    for e in entries.iter() {
        let href = href_for(e);
        out.push_str(&format!(
            "<link rel=\"alternate\" hreflang=\"{}\" href=\"{}\"/>\n",
            attr_escape(&e.code),
            attr_escape(&href)
        ));
        if e.is_default {
            x_default = Some(href);
        }
    }
    if let Some(href) = x_default {
        out.push_str(&format!(
            "<link rel=\"alternate\" hreflang=\"x-default\" href=\"{}\"/>\n",
            attr_escape(&href)
        ));
    }
    Ok(tera::Value::String(out))
}

/// Minimal HTML-attribute escape (output is consumed via `| safe`).
fn attr_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Build switcher entries for `url_path` across the given active
/// `locales`. The URL shape follows `mode`:
/// - default locale → `{url_prefix}{url_path}?lang={code}` (explicit so a
///   click resets the persistence cookie — see `locale_mode::LOCALE_COOKIE`),
/// - path modes → `{url_prefix}/{code}{url_path}`,
/// - query mode → `{url_prefix}{url_path}?lang={code}`.
///
/// `current_code` is the locale the request resolved to (`None` = the
/// default); the matching entry is flagged `is_current`.
#[must_use]
pub fn build(
    locales: &[Locale],
    url_prefix: &str,
    url_path: &str,
    mode: LocaleMode,
    current_code: Option<&str>,
) -> Vec<SwitcherEntry> {
    let default_code = locales
        .iter()
        .find(|l| l.is_default)
        .map(|l| l.code.as_str());
    let current = current_code.or(default_code);
    locales
        .iter()
        .map(|l| {
            // The default-locale link carries an explicit `?lang=` so
            // clicking it overrides a stale `rcms_locale` cookie and
            // returns the visitor to the default language. Non-default
            // path-mode links use the `/<code>/` prefix (also explicit).
            let url = if l.is_default {
                format!("{url_prefix}{url_path}?lang={}", l.code)
            } else if mode.reads_path() {
                format!("{url_prefix}/{}{url_path}", l.code)
            } else {
                format!("{url_prefix}{url_path}?lang={}", l.code)
            };
            SwitcherEntry {
                code: l.code.clone(),
                name: l.name.clone(),
                url,
                is_current: Some(l.code.as_str()) == current,
                is_default: l.is_default,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustango::sql::Auto;

    fn loc(code: &str, name: &str, is_default: bool) -> Locale {
        Locale {
            id: Auto::Unset,
            code: code.to_owned(),
            name: name.to_owned(),
            is_default,
            active: true,
            sort_order: 0,
            created_at: Auto::Unset,
        }
    }

    #[test]
    fn query_mode_urls() {
        let locales = [loc("en", "English", true), loc("es", "Español", false)];
        let out = build(&locales, "", "/about", LocaleMode::Query, Some("es"));
        assert_eq!(out[0].url, "/about?lang=en"); // default — explicit (cookie reset)
        assert!(!out[0].is_current);
        assert_eq!(out[1].url, "/about?lang=es");
        assert!(out[1].is_current);
    }

    #[test]
    fn path_mode_urls_and_prefix() {
        let locales = [loc("en", "English", true), loc("fr", "Français", false)];
        let out = build(&locales, "/p", "/blog", LocaleMode::Path, None);
        assert_eq!(out[0].url, "/p/blog?lang=en");
        assert!(out[0].is_current); // None → default is current
        assert_eq!(out[1].url, "/p/fr/blog");
        assert!(!out[1].is_current);
    }

    #[test]
    fn pathorquery_prefers_path_shape() {
        let locales = [loc("en", "English", true), loc("de", "Deutsch", false)];
        let out = build(&locales, "", "/x", LocaleMode::PathOrQuery, Some("de"));
        assert_eq!(out[1].url, "/de/x");
    }
}
