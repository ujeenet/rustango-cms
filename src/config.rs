//! Process configuration from the environment.
//!
//! Every setting the CMS reads is named here, under one prefix, `RCMS_`.
//! Earlier releases used three (`RCMS_`, `CMS_`, `RUSTANGO_CMS_`), so an
//! operator who wrote `RCMS_MEDIA_BACKEND` silently got local disk; the old
//! spellings are still read, after the canonical one.
//!
//! | Setting | Purpose |
//! |---|---|
//! | `RCMS_SECRET_KEY` | Signs preview tokens and other CMS artifacts |
//! | `RCMS_RENDITION_SIGNING_KEY` | Signs image rendition URLs |
//! | `RCMS_API_CORS_ORIGINS` | Origins allowed to call `/api/v2` |
//! | `RCMS_MEDIA_BACKEND` | `local` (default), `memory` or `s3` |
//! | `RCMS_MEDIA_CDN_BASE` | Public base URL media is served from |
//! | `RCMS_MEDIA_TENANT_BUCKETS` / `RCMS_MEDIA_TENANT_CDNS` | Per-tenant S3 buckets / CDNs, `slug=value,…` |
//! | `RCMS_S3_BUCKET`, `_REGION`, `_ENDPOINT`, `_ACCESS_KEY_ID`, `_SECRET_ACCESS_KEY`, `_PATH_STYLE` | S3 media backend |
//! | `RCMS_SITE_HOST_SUFFIXES` | Domains a tenant may add hostnames under without approval |
//! | `RCMS_DEFAULT_TIMEZONE` | Admin timezone when a user has none |
//! | `RCMS_DEFAULT_ADMIN_LOCALE` | Admin language when a user has none |
//! | `RCMS_PERF_LOG` | `1` / `true` logs per-phase render timings |
//! | `RCMS_NOTIFY_ALLOW_PRIVATE` | `1` lets notification channels deliver to private-network addresses |
//!
//! Cache and search backends keep their providers' own variable names
//! (`CLOUDFLARE_API_TOKEN`, …), documented with each backend.
//!
//! The framework's own `RUSTANGO_SECRET_KEY` is needed too: it encrypts
//! stored secrets such as notification-target tokens. Without it no
//! notification target can be saved — the CMS logs a warning and a form's
//! email recipients get nothing.

/// The value of setting `name` — given without its prefix, e.g.
/// `"MEDIA_BACKEND"` — read as `RCMS_<name>`, then the legacy
/// `CMS_<name>` and `RUSTANGO_CMS_<name>`. An empty value counts as unset.
#[must_use]
pub fn var(name: &str) -> Option<String> {
    var_in(name, |key| std::env::var(key).ok())
}

/// [`var`] over any lookup, so the prefix order is testable without
/// touching the process environment.
fn var_in(name: &str, lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["RCMS_", "CMS_", "RUSTANGO_CMS_"]
        .iter()
        .find_map(|prefix| lookup(&format!("{prefix}{name}")).filter(|v| !v.trim().is_empty()))
}

/// `true` when setting `name` is `1` or `true`.
#[must_use]
pub fn flag(name: &str) -> bool {
    matches!(var(name).as_deref().map(str::trim), Some("1" | "true"))
}

#[cfg(test)]
mod tests {
    use super::var_in;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> =
            pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn the_canonical_name_wins_and_old_names_still_work() {
        let both = env(&[("RCMS_MEDIA_BACKEND", "s3"), ("CMS_MEDIA_BACKEND", "local")]);
        assert_eq!(var_in("MEDIA_BACKEND", both).as_deref(), Some("s3"));
        let legacy = env(&[("CMS_MEDIA_BACKEND", "s3")]);
        assert_eq!(var_in("MEDIA_BACKEND", legacy).as_deref(), Some("s3"));
        let oldest = env(&[("RUSTANGO_CMS_DEFAULT_TIMEZONE", "Europe/Kyiv")]);
        assert_eq!(var_in("DEFAULT_TIMEZONE", oldest).as_deref(), Some("Europe/Kyiv"));
    }

    #[test]
    fn an_empty_value_is_unset() {
        let e = env(&[("RCMS_SECRET_KEY", "  "), ("CMS_SECRET_KEY", "real")]);
        assert_eq!(var_in("SECRET_KEY", e).as_deref(), Some("real"));
        assert_eq!(var_in("SECRET_KEY", env(&[])), None);
    }
}
