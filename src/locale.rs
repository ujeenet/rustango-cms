//! `Locale` — per-tenant locale registry.
//!
//! Drives the language picker on every page edit form and the
//! locale FK on `cms_translation`.
//! Exactly one row carries `is_default = true`; the admin form
//! enforces that invariant when toggling.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// One row per active locale on the tenant. `code` follows
/// BCP-47 (e.g. `en`, `fr-CA`, `pt-BR`).
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_locale",
    app = "cms",
    display = "name",
    admin(
        list_display = "code, name, is_default, active, sort_order",
        ordering = "sort_order, code",
        list_filter = "is_default, active",
    )
)]
pub struct Locale {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// BCP-47 tag — `en`, `fr-CA`, `pt-BR`, …
    #[rustango(max_length = 16, index)]
    pub code: String,

    /// Display name shown in the admin picker (e.g. `English`,
    /// `Français (Canada)`).
    #[rustango(max_length = 64)]
    pub name: String,

    /// Exactly one row should carry `is_default = true` per tenant —
    /// the admin form enforces this on save.
    pub is_default: bool,

    /// Active locales appear in the public-side picker and in the
    /// page-form sidebar. Setting `active = false` keeps existing
    /// translations addressable via direct URL but removes the
    /// locale from authoring surfaces.
    pub active: bool,

    /// Sort order in the admin picker (low first). Defaults set the
    /// default locale to 0 and other rows to 100+ when seeded.
    pub sort_order: i32,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Validate + normalize a BCP-47-ish locale `code` for the `cms_locale`
/// registry. Content locales may be any well-formed tag — core provides
/// metadata + graceful fallback via [`rustango::i18n::locale_info`] — but
/// the tag must be structurally sane so it matches `Accept-Language`, keys
/// translations predictably, and fits the 16-char column. Normalizes
/// `_`→`-`, language→lowercase, script→`Titlecase`, region→`UPPERCASE`.
/// Returns the normalized code, or a human-readable error message.
///
/// Mirrors [`rustango_cms::translation::validate_field_path`](crate::translation::validate_field_path).
/// Pure; unit-tested.
///
/// # Errors
/// When the code is empty, over 16 chars, has an empty segment, or a
/// segment isn't a valid BCP-47 subtag.
pub fn validate_code(raw: &str) -> Result<String, String> {
    let s = raw.trim().replace('_', "-");
    if s.is_empty() {
        return Err("Locale code is required.".to_owned());
    }
    if s.chars().count() > 16 {
        return Err("Locale code is too long (max 16 characters).".to_owned());
    }
    let mut out: Vec<String> = Vec::new();
    for (i, sub) in s.split('-').enumerate() {
        if sub.is_empty() {
            return Err("Locale code has an empty segment (check the hyphens).".to_owned());
        }
        if i == 0 {
            // Language: 2–3 ASCII letters (`en`, `zh`, `ckb`).
            if (2..=3).contains(&sub.len()) && sub.bytes().all(|b| b.is_ascii_alphabetic()) {
                out.push(sub.to_ascii_lowercase());
            } else {
                return Err(format!(
                    "`{sub}` is not a valid language subtag (2–3 letters, e.g. `en`, `zh`)."
                ));
            }
        } else if sub.len() == 4 && sub.bytes().all(|b| b.is_ascii_alphabetic()) {
            // Script: 4 letters, Titlecase (`Hans`, `Cyrl`).
            let mut chars = sub.chars();
            let first = chars.next().unwrap_or_default().to_ascii_uppercase();
            out.push(format!("{first}{}", chars.as_str().to_ascii_lowercase()));
        } else if (sub.len() == 2 && sub.bytes().all(|b| b.is_ascii_alphabetic()))
            || (sub.len() == 3 && sub.bytes().all(|b| b.is_ascii_digit()))
        {
            // Region: 2 letters (`US`) or 3 digits (`419`), UPPERCASE.
            out.push(sub.to_ascii_uppercase());
        } else if sub.len() <= 8 && sub.bytes().all(|b| b.is_ascii_alphanumeric()) {
            // Variant: up to 8 alphanumerics, lowercase.
            out.push(sub.to_ascii_lowercase());
        } else {
            return Err(format!("`{sub}` is not a valid locale subtag."));
        }
    }
    Ok(out.join("-"))
}

#[cfg(test)]
mod tests {
    use super::validate_code;

    #[test]
    fn accepts_and_normalizes_well_formed_tags() {
        assert_eq!(validate_code("en").unwrap(), "en");
        assert_eq!(validate_code(" fr-CA ").unwrap(), "fr-CA");
        assert_eq!(validate_code("zh-Hans").unwrap(), "zh-Hans");
        assert_eq!(validate_code("pt-BR").unwrap(), "pt-BR");
        assert_eq!(validate_code("ckb").unwrap(), "ckb");
        // Case + underscore normalization.
        assert_eq!(validate_code("EN_us").unwrap(), "en-US");
        assert_eq!(validate_code("ZH-hans").unwrap(), "zh-Hans");
        assert_eq!(validate_code("es-419").unwrap(), "es-419");
    }

    #[test]
    fn rejects_malformed_tags() {
        assert!(validate_code("").is_err());
        assert!(validate_code("   ").is_err());
        assert!(validate_code("has space").is_err());
        assert!(validate_code("123").is_err()); // language must be letters
        assert!(validate_code("e").is_err()); // too short
        assert!(validate_code("en-").is_err()); // empty segment
        assert!(validate_code("en--US").is_err());
        assert!(validate_code(&"a".repeat(17)).is_err()); // over the 16-char column
    }
}
