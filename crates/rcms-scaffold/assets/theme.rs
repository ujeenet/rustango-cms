//! The starter theme: `theme.json` creates this site's page types, sample
//! pages, menus and colours the first time a site starts with no pages.
//!
//! After that it never runs again for that site, so edit everything in
//! the admin: pages under Pages, page types (with their fields and content
//! blocks) under Page types, menus under Navigation, the site name under
//! Site settings, and colours and fonts in the Rustango admin at `/admin`
//! → Themes. The templates are yours in `templates/`.

use rustango::sql::Pool;
use rustango::tenancy::Org;
use rustango_cms::seed::{SeedError, SeedFuture};
use rustango_cms::widget::{Widget, WidgetKind};

const THEME_JSON: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/theme.json"));

/// The sample pictures `theme.json` uses (`theme-media/`), built into the
/// binary. They go to the site's media library with the sample pages.
const THEME_MEDIA: &[(&str, &[u8])] = &[
    // __THEME_MEDIA__
];

fn apply_theme<'a>(pool: &'a Pool, org: &'a Org) -> SeedFuture<'a> {
    Box::pin(async move {
        let outcome = rustango_cms::starter::apply_with_media(pool, &org.slug, THEME_JSON, THEME_MEDIA)
            .await
            .map_err(|e| SeedError::Seeder(e.to_string()))?;
        if matches!(outcome, rustango_cms::starter::Outcome::Applied { .. }) {
            tracing::info!(site = %org.slug, "starter theme applied");
        }
        Ok(())
    })
}

rustango_cms::register_tenant_seeder!(apply_theme);

// The tagline, footer text and footer badge the templates show, editable
// in the admin under Site settings → Site. The site's name, logo and icon
// are under Settings → Branding, shared with the admin.
rustango_cms::register_site_setting!("site", "Site", || vec![
    Widget::new(WidgetKind::Text, "tagline", "Tagline"),
    Widget::new(WidgetKind::Textarea, "footer", "Footer text"),
    Widget::new(WidgetKind::MediaPicker, "footer_logo", "Footer badge")
        .with_help("Shown in the footer, under the footer text."),
]);

/// Referenced from `main` so the linker keeps the registration above.
pub fn link() {}
