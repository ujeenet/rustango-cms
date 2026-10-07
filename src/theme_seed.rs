//! Bootstrap-time hydration of the six bundled themes + their brand
//! colors into each tenant.
//!
//! Theme rows live in [`fixtures/cms_theme.json`] and are loaded via
//! [`rustango::fixtures::Fixture`] — pure data, easy to diff / edit,
//! reproducible across tenants. Brand colors stay in code because
//! their `theme_id` FK has to be resolved against the just-inserted
//! row, which fixtures don't natively handle.
//!
//! ## Idempotency
//!
//! [`ensure_themes_seeded`] runs on every boot and reconciles by slug:
//! it inserts fixture themes whose slug the tenant lacks (with brand
//! colors) and never updates an existing row, so renames and recolors
//! survive. On a tenant that already has themes, new built-ins land
//! with `is_default` / `is_admin_default` cleared. A deleted bundled
//! preset comes back on the next boot, since its slug is missing again.
//!
//! [`fixtures/cms_theme.json`]: ../../../fixtures/cms_theme.json

use rustango::core::Column as _;
use rustango::fixtures::Fixture;
use rustango::sql::{ExecError, FetcherPool as _, Pool};

use crate::theme::{BrandColor, Theme};

/// Bundled theme JSON — embedded at compile time via `include_str!`
/// so the binary ships fully self-contained.
const THEME_FIXTURE: &str = include_str!("../fixtures/cms_theme.json");

/// Brand colors per theme slug — `(slug, name, light_hex, dark_hex)`
/// quads inserted with `theme_id` resolved against the just-loaded
/// row. The bundled set is small (two colors × six themes) so an
/// inline table beats a parallel JSON fixture with hand-rolled FK
/// resolution.
const BRAND_COLORS: &[(&str, &[(&str, &str, &str, &str)])] = &[
    (
        "editor-classic",
        &[
            ("primary", "Primary", "#1b32e7", "#bdc2ff"),
            ("secondary", "Accent", "#942f00", "#ffb59c"),
        ],
    ),
    (
        "lumina",
        &[
            ("primary", "Primary", "#6366f1", "#a5b4fc"),
            ("secondary", "Accent", "#06b6d4", "#67e8f9"),
        ],
    ),
    (
        "wagtail-teal",
        &[
            ("primary", "Primary", "#00838F", "#26C6DA"),
            ("secondary", "Accent", "#43A047", "#66BB6A"),
        ],
    ),
    (
        "forest",
        &[
            ("primary", "Primary", "#2E7D32", "#66BB6A"),
            ("secondary", "Accent", "#6750A4", "#B39DDB"),
        ],
    ),
    (
        "aurora-violet",
        &[
            ("primary", "Primary", "#6750A4", "#D0BCFF"),
            ("secondary", "Accent", "#00ACC1", "#4DD0E1"),
        ],
    ),
    (
        "slate",
        &[
            ("primary", "Primary", "#475569", "#94A3B8"),
            ("secondary", "Accent", "#0EA5E9", "#38BDF8"),
        ],
    ),
    (
        "sunset",
        &[
            ("primary", "Primary", "#E25822", "#FF8A65"),
            ("secondary", "Accent", "#FFB300", "#FFD54F"),
        ],
    ),
    (
        "ink",
        &[
            ("primary", "Primary", "#111111", "#fafafa"),
            ("secondary", "Accent", "#2563EB", "#60A5FA"),
        ],
    ),
];

/// Which fixture rows this tenant is missing, with their flags adjusted
/// for how the tenant is arriving.
///
/// Split out from [`ensure_themes_seeded`] because it is the whole risk of
/// the reconcile and the only part worth testing without a pool: it now
/// runs on **every** boot for every tenant, so a mistake here duplicates
/// rows or silently restyles a live admin.
fn rows_to_insert(
    raw: &serde_json::Value,
    have: &std::collections::HashSet<String>,
    bootstrapping: bool,
) -> Vec<serde_json::Value> {
    let mut rows: Vec<serde_json::Value> = raw.as_array().cloned().unwrap_or_default();
    rows.retain(|r| {
        r.get("slug")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|slug| !have.contains(slug))
    });
    // Arriving at a site that already has themes, a new built-in must not
    // become the active one — that would restyle a live admin nobody asked
    // to restyle, and would leave two rows claiming to be the default.
    // It lands as an option; an editor chooses it.
    if !bootstrapping {
        for r in &mut rows {
            if let Some(obj) = r.as_object_mut() {
                obj.insert("is_default".to_owned(), serde_json::Value::Bool(false));
                obj.insert("is_admin_default".to_owned(), serde_json::Value::Bool(false));
            }
        }
    }
    rows
}

/// Hydrate the bundled theme presets + their brand colors into the
/// current tenant. Inserts every built-in theme whose slug the tenant
/// lacks and never updates an existing row (see the module docs).
///
/// Called for each tenant from [`crate::seed::ensure_seeded`].
///
/// # Errors
/// Driver / query failures propagate. A malformed
/// `fixtures/cms_theme.json` file surfaces as a fixture-format
/// error — that would be a compile-time bug in this crate, not a
/// runtime concern (the JSON is checked into the repo).
pub async fn ensure_themes_seeded(pool: &Pool) -> Result<(), ExecError> {
    // Reconcile by slug rather than bailing wholesale.
    //
    // The old guard returned as soon as *any* theme row existed, to avoid
    // overwriting an editor's customizations. It did protect those — and
    // it also meant a theme added to the fixture in a later release never
    // reached an existing tenant, not even as something to pick. The site
    // was stuck with whatever shipped the day it was bootstrapped.
    //
    // Insert missing slugs; never update an existing row. That keeps the
    // original protection (customizations are still never touched) while
    // letting new built-ins arrive.
    let existing: Vec<Theme> = Theme::objects().fetch(pool).await?;
    let bootstrapping = existing.is_empty();
    let have: std::collections::HashSet<String> =
        existing.into_iter().map(|t| t.slug).collect();

    // Load the theme rows. Framework `Fixture::from_value` requires
    // a top-level JSON array; we shipped one in cms_theme.json.
    let raw: serde_json::Value = serde_json::from_str(THEME_FIXTURE)
        .map_err(|e| ExecError::Driver(rustango::sql::sqlx::Error::Decode(Box::new(e))))?;
    let rows = rows_to_insert(&raw, &have, bootstrapping);
    let added: std::collections::HashSet<String> = rows
        .iter()
        .filter_map(|r| {
            r.get("slug")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    let fixture = Fixture::new("cms_theme")
        .from_value(serde_json::Value::Array(rows))
        .map_err(fixture_to_exec_err)?;
    fixture
        .load_into_pool("cms_theme", pool)
        .await
        .map_err(fixture_to_exec_err)?;

    // Look up the just-loaded rows by slug so we can stamp brand
    // colors with the correct `theme_id`. One round-trip per theme;
    // 6 themes total, fine.
    for (slug, colors) in BRAND_COLORS {
        // Only for themes this pass created. Re-stamping an existing one
        // would append a duplicate swatch set on every boot.
        if !added.contains(*slug) {
            continue;
        }
        let theme = Theme::objects()
            .where_(Theme::slug.eq((*slug).to_owned()))
            .first(pool)
            .await?;
        let Some(theme) = theme else {
            // Fixture didn't include this slug — surfacing the gap
            // is more useful than silently dropping the colors.
            tracing::warn!(
                target: "rustango_cms::theme_seed",
                slug = *slug,
                "brand-color group has no matching theme in cms_theme.json",
            );
            continue;
        };
        let theme_id = theme.id.get().copied().unwrap_or_default();
        for (idx, (cslug, cname, light, dark)) in colors.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let mut row = BrandColor {
                id: rustango::sql::Auto::Unset,
                theme_id,
                slug: (*cslug).to_owned(),
                name: (*cname).to_owned(),
                color_value: (*light).to_owned(),
                color_value_dark: (*dark).to_owned(),
                sort_order: idx as i32,
            };
            row.insert_pool(pool).await?;
        }
    }
    Ok(())
}

/// Coerce a [`rustango::fixtures::FixtureError`] into [`ExecError`]
/// so the seeder's `?` chain still works.
fn fixture_to_exec_err(e: rustango::fixtures::FixtureError) -> ExecError {
    ExecError::Driver(rustango::sql::sqlx::Error::Decode(Box::new(e)))
}

#[cfg(test)]
mod reconcile_tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> serde_json::Value {
        json!([
            { "slug": "paper",          "is_default": true,  "is_admin_default": true },
            { "slug": "editor-classic", "is_default": false, "is_admin_default": false },
        ])
    }

    fn slugs(v: &[serde_json::Value]) -> Vec<&str> {
        v.iter().filter_map(|r| r["slug"].as_str()).collect()
    }

    #[test]
    fn a_fresh_tenant_gets_every_theme_with_its_shipped_default() {
        let rows = rows_to_insert(&fixture(), &std::collections::HashSet::new(), true);
        assert_eq!(slugs(&rows), ["paper", "editor-classic"]);
        // the shipped default survives bootstrapping — otherwise a new
        // tenant would come up with no active theme at all
        assert_eq!(rows[0]["is_default"], json!(true));
        assert_eq!(rows[0]["is_admin_default"], json!(true));
    }

    #[test]
    fn an_existing_tenant_gets_only_what_it_lacks() {
        let have = ["editor-classic".to_owned()].into_iter().collect();
        let rows = rows_to_insert(&fixture(), &have, false);
        assert_eq!(slugs(&rows), ["paper"], "must not re-insert a theme it has");
    }

    /// The regression that matters: a new built-in arriving at a live site
    /// must never take over. Shipping `is_default: true` into a tenant that
    /// already has an active theme would restyle its admin unannounced and
    /// leave two rows both claiming to be the default.
    #[test]
    fn a_new_theme_never_activates_itself_on_a_live_site() {
        let have = ["editor-classic".to_owned()].into_iter().collect();
        let rows = rows_to_insert(&fixture(), &have, false);
        assert_eq!(rows[0]["slug"], json!("paper"));
        assert_eq!(rows[0]["is_default"], json!(false));
        assert_eq!(rows[0]["is_admin_default"], json!(false));
    }

    #[test]
    fn a_fully_seeded_tenant_inserts_nothing() {
        let have = ["paper".to_owned(), "editor-classic".to_owned()]
            .into_iter()
            .collect();
        assert!(rows_to_insert(&fixture(), &have, false).is_empty());
    }
}
