//! A theme's sample pictures: the `media` entries of `theme.json`, stored
//! in the site's media library when the theme is applied, and the
//! `"@media:<key>"` references in page content that point at them.
//!
//! ```json
//! "media": [ { "key": "hills", "file": "hills.jpg", "title": "Rolling hills",
//!              "alt": "Layered hills at dawn" } ],
//! "pages": [ { …, "data": { "cover": "@media:hills" } } ]
//! ```
//!
//! The files come with the call ([`super::apply_with_media`]), usually
//! embedded in the site's binary. Each goes through the same path as an
//! upload in the admin. A reference whose picture was not supplied becomes
//! empty, so the page shows no picture rather than a broken one.

use std::collections::HashMap;

use rustango::sql::Pool;
use serde::Deserialize;
use serde_json::Value;

use super::{StarterError, StarterSeed};

/// How page content points at a theme picture.
pub const REF_PREFIX: &str = "@media:";

/// One `media` entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaSeed {
    /// Name page content uses: `"@media:<key>"`.
    pub key: String,
    /// File name, as supplied to [`super::apply_with_media`].
    pub file: String,
    #[serde(default)]
    pub title: String,
    /// What the picture shows, for people who can't see it.
    #[serde(default)]
    pub alt: String,
}

/// Every string in `value` that references a picture, as its key.
fn refs<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(s) => {
            if let Some(key) = s.strip_prefix(REF_PREFIX) {
                out.push(key);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
        Value::Object(map) => map.values().for_each(|v| refs(v, out)),
        _ => {}
    }
}

fn page_refs<'a>(pages: &'a [super::PageSeed], out: &mut Vec<&'a str>) {
    for page in pages {
        refs(&page.data, out);
        page_refs(&page.children, out);
    }
}

/// Problems with the `media` entries and the references to them.
pub(super) fn problems(seed: &StarterSeed) -> Vec<String> {
    let mut problems = Vec::new();
    let mut keys: Vec<&str> = Vec::new();
    for m in &seed.media {
        if keys.contains(&m.key.as_str()) {
            problems.push(format!("media `{}` is listed twice", m.key));
        }
        keys.push(&m.key);
        let simple = !m.file.is_empty() && !m.file.contains(['/', '\\']) && !m.file.starts_with('.');
        if !simple {
            problems.push(format!("media `{}`: `file` must be a plain file name", m.key));
        }
    }
    let mut used = Vec::new();
    page_refs(&seed.pages, &mut used);
    seed.error_pages.iter().for_each(|e| refs(&e.body, &mut used));
    seed.site.values().for_each(|v| refs(v, &mut used));
    seed.branding.values().for_each(|v| refs(v, &mut used));
    for key in used {
        if !keys.contains(&key) {
            problems.push(format!("`{REF_PREFIX}{key}` names no `media` entry"));
        }
    }
    problems
}

/// Store the supplied pictures; returns key → media id (as the editor
/// stores it: a string).
pub(super) async fn store(
    pool: &Pool,
    org_slug: &str,
    seeds: &[MediaSeed],
    files: &[(&str, &[u8])],
) -> Result<HashMap<String, String>, StarterError> {
    let mut ids = HashMap::new();
    for seed in seeds {
        let Some((_, bytes)) = files.iter().find(|(name, _)| *name == seed.file) else {
            tracing::warn!(target: "rustango_cms::starter", file = %seed.file, "theme picture not supplied; skipped");
            continue;
        };
        let title = (!seed.title.is_empty()).then_some(seed.title.as_str());
        let media = crate::admin::store_media(
            pool,
            org_slug,
            bytes.to_vec(),
            &seed.file,
            title,
            &seed.alt,
            None,
            None,
            true,
        )
        .await
        .map_err(|e| StarterError::Invalid(format!("media `{}`: {e}", seed.key)))?;
        if let Some(id) = media.id.get() {
            ids.insert(seed.key.clone(), id.to_string());
        }
    }
    Ok(ids)
}

/// Replace every picture reference in `value` with its media id, or an
/// empty string when the picture was not stored.
pub(super) fn resolve(value: &mut Value, ids: &HashMap<String, String>) {
    match value {
        Value::String(s) => {
            if let Some(key) = s.strip_prefix(REF_PREFIX) {
                *s = ids.get(key).cloned().unwrap_or_default();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| resolve(v, ids)),
        Value::Object(map) => map.values_mut().for_each(|v| resolve(v, ids)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn references_become_ids_or_empty() {
        let ids = HashMap::from([("hills".to_owned(), "7".to_owned())]);
        let mut data = json!({
            "cover": "@media:hills",
            "body": [{ "type": "gallery", "value": { "images": [{ "value": { "image": "@media:sea" } }] } }],
            "title": "media: not a reference",
        });
        resolve(&mut data, &ids);
        assert_eq!(data["cover"], "7");
        assert_eq!(data["body"][0]["value"]["images"][0]["value"]["image"], "");
        assert_eq!(data["title"], "media: not a reference");
    }

    #[test]
    fn unknown_references_and_odd_files_are_reported() {
        let seed: StarterSeed = serde_json::from_value(json!({
            "name": "t",
            "media": [
                { "key": "a", "file": "a.jpg" },
                { "key": "a", "file": "../b.jpg" }
            ],
            "pages": [{ "key": "home", "type": "Home", "title": "Home", "data": { "cover": "@media:missing" } }]
        }))
        .unwrap();
        let found = problems(&seed).join("; ");
        for expected in ["listed twice", "plain file name", "`@media:missing` names no `media` entry"] {
            assert!(found.contains(expected), "{expected}: {found}");
        }
    }
}
