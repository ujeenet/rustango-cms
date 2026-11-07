//! A site's branding: its name, logo and browser icon, kept in one place
//! (the `branding` site setting, edited under **Settings → Branding**)
//! and used everywhere: the admin sidebar and sign-in screens, and the
//! public site's layout.
//!
//! The logo and icon are pictures in the media library, stored as media
//! ids (`logo`, `favicon`). Sites set up before that kept uploaded files
//! instead (`logo_storage_key`, `favicon_storage_key`, under the tenant's
//! `branding/` folder); those still serve, and [`migrate_uploads`] moves
//! them into the media library.
//!
//! Both pictures are served from stable addresses, `/__cms-branding/logo`
//! and `/__cms-branding/favicon`, so pages never need the media id.

use rustango::core::Column as _;
use rustango::sql::Pool;
use rustango::tenancy::Org;
use serde_json::Value;

use crate::media::Media;

/// Site-setting scope holding the branding.
pub const SCOPE: &str = "branding";

/// One of the two branding pictures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picture {
    Logo,
    Favicon,
}

impl Picture {
    /// The setting key holding the media id.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Logo => "logo",
            Self::Favicon => "favicon",
        }
    }

    /// The older key holding an uploaded file's name.
    fn upload_key(self) -> &'static str {
        match self {
            Self::Logo => "logo_storage_key",
            Self::Favicon => "favicon_storage_key",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Logo => "Logo",
            Self::Favicon => "Site icon",
        }
    }

    /// The address the picture is served from.
    #[must_use]
    pub fn url(self) -> &'static str {
        match self {
            Self::Logo => "/__cms-branding/logo",
            Self::Favicon => "/__cms-branding/favicon",
        }
    }
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// The branding setting's value (`{}` when unset).
pub async fn get(pool: &Pool) -> Value {
    crate::site_setting::get(pool, SCOPE)
        .await
        .ok()
        .flatten()
        .map_or_else(|| Value::Object(serde_json::Map::new()), |r| r.value_json)
}

/// Whether `branding` has this picture, as a media id or an uploaded file.
#[must_use]
pub fn has(branding: &Value, picture: Picture) -> bool {
    text(branding, picture.key()).is_some() || text(branding, picture.upload_key()).is_some()
}

/// The site's name: the Branding site name, else the operator's brand
/// name for the tenant, else its display name.
#[must_use]
pub fn site_name(branding: &Value, org: &Org) -> String {
    name_from(branding, org.brand_name.as_deref(), &org.display_name)
}

fn name_from(branding: &Value, brand_name: Option<&str>, display_name: &str) -> String {
    text(branding, "site_name")
        .or_else(|| brand_name.map(str::trim).filter(|n| !n.is_empty()))
        .unwrap_or(display_name)
        .to_owned()
}

/// The picture's bytes and mime type, if set and stored.
pub async fn load(pool: &Pool, org_slug: &str, branding: &Value, picture: Picture) -> Option<(String, Vec<u8>)> {
    let disk = crate::media_storage::disk_for(org_slug);
    if let Some(id) = text(branding, picture.key()).and_then(|s| s.parse::<i64>().ok()) {
        let media = Media::objects().where_(Media::id.eq(id)).first(pool).await.ok().flatten()?;
        let bytes = disk.load(&crate::media_storage::key(org_slug, &media.storage_key)).await.ok()?;
        return Some((media.mime, bytes));
    }
    let name = text(branding, picture.upload_key())?;
    // The name comes from a setting the CMS wrote; still never let it
    // leave the branding folder.
    if name.contains('/') || name.contains("..") {
        return None;
    }
    let bytes = disk.load(&crate::media_storage::key(org_slug, &format!("branding/{name}"))).await.ok()?;
    let mime = mime_guess::from_path(name).first_or_octet_stream().essence_str().to_owned();
    Some((mime, bytes))
}

/// Move pictures uploaded before branding used the media library into it,
/// so the Branding form can show and change them. Idempotent; a picture
/// that can't be read is left as it is.
pub async fn migrate_uploads(pool: &Pool, org_slug: &str) {
    let mut branding = get(pool).await;
    let mut changed = false;
    for picture in [Picture::Logo, Picture::Favicon] {
        if text(&branding, picture.key()).is_some() {
            continue;
        }
        let Some(name) = text(&branding, picture.upload_key()).map(str::to_owned) else { continue };
        let Some((_, bytes)) = load(pool, org_slug, &branding, picture).await else { continue };
        match crate::admin::store_media(pool, org_slug, bytes, &name, Some(picture.title()), "", None, None, true).await {
            Ok(media) => {
                if let (Some(id), Some(map)) = (media.id.get(), branding.as_object_mut()) {
                    map.insert(picture.key().to_owned(), Value::String(id.to_string()));
                    map.remove(picture.upload_key());
                    changed = true;
                }
            }
            Err(e) => tracing::warn!(target: "rustango_cms::branding", error = %e, "could not move a branding upload into the media library"),
        }
    }
    if changed {
        if let Err(e) = crate::site_setting::upsert(pool, SCOPE, branding).await {
            tracing::warn!(target: "rustango_cms::branding", error = %e, "could not save branding");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_site_name_comes_from_branding_first() {
        assert_eq!(name_from(&json!({ "site_name": " Studio " }), Some("Acme"), "demo"), "Studio");
        assert_eq!(name_from(&json!({ "site_name": "" }), Some("Acme"), "demo"), "Acme");
        assert_eq!(name_from(&json!({}), None, "demo"), "demo");
    }

    #[test]
    fn a_picture_counts_as_a_media_id_or_an_old_upload() {
        assert!(has(&json!({ "logo": "7" }), Picture::Logo));
        assert!(has(&json!({ "logo_storage_key": "logo.png" }), Picture::Logo));
        assert!(!has(&json!({ "logo": "" }), Picture::Logo));
        assert!(!has(&json!({ "logo": "7" }), Picture::Favicon));
    }
}
