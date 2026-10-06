//! Visual themes — design tokens stored as data, rendered as CSS
//! variables. Mirrors the shape of [wagtail-visual-themes](https://github.com/ujeenet/wagtail-visual-themes):
//! a [`Theme`] holds surface / semantic colors + typography + radii +
//! shadows; [`BrandColor`] rows attach N named brand colors per theme,
//! each emitting its own slug-keyed CSS variable triplet (raw / rgb /
//! contrast) plus an auto-derived 50→950 shade scale.
//!
//! The theme is rendered into a `<style>` block by [`emit_css`] —
//! consumers (admin chrome, public page render) drop the block into
//! `<head>` and reference `var(--color-bg)`, `var(--color-primary)`
//! etc. anywhere downstream.
//!
//! #274 — the admin chrome reads Material Design 3 token names
//! (`--md-sys-color-surface`, `--md-sys-color-primary`, …), not the
//! legacy `--color-*` names this module historically emitted. So every
//! block now emits BOTH namespaces: the legacy `--color-*` for any
//! host crate / public template still reading them, plus the MD3
//! bridge so picking a different admin theme actually shifts the
//! chrome palette. Container / on-* variants the schema doesn't carry
//! directly are derived via [`mix_with_neutral`] + [`wcag_foreground`].

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// A named design-token bundle. Editors create one per visual style
/// they want to ship: "Wagtail Teal", "Forest", "Aurora Violet".
///
/// One theme can be marked `is_admin_default` (drives the admin
/// chrome) and one `is_default` (the fallback for public pages with
/// no explicit theme). They can be the same row or different.
///
/// The `_dark` columns are optional — when blank, the light value
/// shows through for dark mode too. The renderer emits both variants
/// behind `[data-theme="light"]` / `[data-theme="dark"]` blocks.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_theme",
    app = "cms",
    display = "name",
    admin(
        list_display = "name, slug, is_admin_default, is_default, default_mode",
        ordering = "name",
        list_filter = "is_default, is_admin_default",
    )
)]
pub struct Theme {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Human display name — "Wagtail Teal", "Forest", etc.
    #[rustango(max_length = 100)]
    pub name: String,

    /// URL-safe identifier — emitted into the body class as
    /// `theme-<slug>` so per-theme template overrides work
    /// (`.theme-wagtail-teal .hero { … }`).
    #[rustango(max_length = 64, unique)]
    pub slug: String,

    /// When true, this theme drives the CMS admin UI chrome. Exactly
    /// one row at a time; the picker enforces it.
    pub is_admin_default: bool,

    /// When true, this is the fallback theme for public pages with
    /// no explicit `page.theme_id`. Exactly one row at a time.
    pub is_default: bool,

    /// Default mode for visitors with no saved preference. Valid
    /// values: `light`, `dark`, `system`.
    #[rustango(max_length = 16)]
    pub default_mode: String,

    // -------- Surface colors (light) --------
    #[rustango(max_length = 32)]
    pub color_bg: String,
    #[rustango(max_length = 32)]
    pub color_surface: String,
    #[rustango(max_length = 32)]
    pub color_text_primary: String,
    #[rustango(max_length = 32)]
    pub color_text_secondary: String,
    #[rustango(max_length = 32)]
    pub color_text_muted: String,
    #[rustango(max_length = 32)]
    pub color_border: String,

    // -------- Surface colors (dark) --------
    #[rustango(max_length = 32)]
    pub color_bg_dark: String,
    #[rustango(max_length = 32)]
    pub color_surface_dark: String,
    #[rustango(max_length = 32)]
    pub color_text_primary_dark: String,
    #[rustango(max_length = 32)]
    pub color_text_secondary_dark: String,
    #[rustango(max_length = 32)]
    pub color_text_muted_dark: String,
    #[rustango(max_length = 32)]
    pub color_border_dark: String,

    // -------- Semantic colors (light only — dark inherits) --------
    #[rustango(max_length = 32)]
    pub color_success: String,
    #[rustango(max_length = 32)]
    pub color_warning: String,
    #[rustango(max_length = 32)]
    pub color_error: String,
    #[rustango(max_length = 32)]
    pub color_info: String,
    #[rustango(max_length = 32)]
    pub color_link: String,
    #[rustango(max_length = 32)]
    pub color_focus_ring: String,

    // -------- Typography --------
    #[rustango(max_length = 255)]
    pub font_heading: String,
    #[rustango(max_length = 255)]
    pub font_body: String,
    /// Optional URL to an external font CSS (e.g. Google Fonts) the
    /// renderer should `<link>` before the variables block.
    #[rustango(max_length = 500)]
    pub font_url: String,

    // -------- Radii + shadows --------
    #[rustango(max_length = 16)]
    pub radius_sm: String,
    #[rustango(max_length = 16)]
    pub radius_md: String,
    #[rustango(max_length = 16)]
    pub radius_lg: String,
    #[rustango(max_length = 255)]
    pub shadow_sm: String,
    #[rustango(max_length = 255)]
    pub shadow_md: String,
    #[rustango(max_length = 255)]
    pub shadow_lg: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

/// A named brand color attached to a [`Theme`]. Each one emits:
///
/// - `--color-<slug>` — the raw value as authored.
/// - `--color-<slug>-rgb` — RGB triplet (only when the value is a
///   solid color, not a gradient). Powers Tailwind opacity.
/// - `--color-<slug>-contrast` — auto-computed via WCAG luminance.
/// - `--color-<slug>-50` … `--color-<slug>-950` — Tailwind-aligned
///   shade scale auto-derived from the base.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_brand_color",
    app = "cms",
    display = "name",
    admin(
        list_display = "theme_id, name, slug, sort_order, color_value",
        ordering = "theme_id, sort_order",
    )
)]
pub struct BrandColor {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(fk = "cms_theme", on = "id", index)]
    pub theme_id: i64,

    /// URL-safe identifier — becomes the `--color-<slug>` variable.
    #[rustango(max_length = 64)]
    pub slug: String,

    /// Display name for the admin picker. "Primary", "Aurora", etc.
    #[rustango(max_length = 100)]
    pub name: String,

    /// Light-mode value. Solid color (`#rrggbb` / `rgb()` / `hsl()`)
    /// or a gradient.
    #[rustango(max_length = 255)]
    pub color_value: String,

    /// Optional dark-mode override. Empty = fall back to
    /// `color_value`.
    #[rustango(max_length = 255)]
    pub color_value_dark: String,

    /// Ordering for the admin picker.
    pub sort_order: i32,
}

// =====================================================================
// CSS emission
// =====================================================================

/// Render a `Theme` (plus its `BrandColor` rows) as a `<style>` block
/// ready to drop into `<head>`. Emits:
///
/// - `:root { … }` — surface, semantic, typography, radii, shadow
///   tokens in their light-mode values.
/// - `[data-theme="dark"] { … }` — overrides for dark mode.
/// - One block per brand color with raw / rgb / contrast / 50-950.
///
/// The returned string is plain HTML; the caller is responsible for
/// rendering it into the `<head>` (e.g. via Tera or a context var).
#[must_use]
pub fn emit_css(theme: &Theme, brand_colors: &[BrandColor]) -> String {
    let mut out = String::with_capacity(8192);
    out.push_str("<style data-rcms-theme=\"");
    out.push_str(&html_escape_attr(&theme.slug));
    out.push_str("\">\n");

    // -------- :root (light defaults) --------
    // Specificity matters: cms.css's dark block uses `:root[data-theme="dark"]`
    // (0,2,0), so emit_css's dark/system blocks must do the same to win the
    // cascade — see below. The light block stays `:root` because the cms.css
    // light defaults sit on `:root` too and we want this to override them by
    // source order (theme CSS loads AFTER cms.css in `_base.html`).
    out.push_str(":root {\n");
    // Legacy `--color-*` namespace — kept for back-compat with any host
    // crate / public template still reading them.
    push_var(&mut out, "color-bg", &theme.color_bg);
    push_var(&mut out, "color-surface", &theme.color_surface);
    push_var(&mut out, "color-text-primary", &theme.color_text_primary);
    push_var(
        &mut out,
        "color-text-secondary",
        &theme.color_text_secondary,
    );
    push_var(&mut out, "color-text-muted", &theme.color_text_muted);
    push_var(&mut out, "color-border", &theme.color_border);
    push_var(&mut out, "color-success", &theme.color_success);
    push_var(&mut out, "color-warning", &theme.color_warning);
    push_var(&mut out, "color-error", &theme.color_error);
    push_var(&mut out, "color-info", &theme.color_info);
    push_var(&mut out, "color-link", &theme.color_link);
    push_var(&mut out, "color-focus-ring", &theme.color_focus_ring);
    push_var(&mut out, "font-heading", &theme.font_heading);
    push_var(&mut out, "font-body", &theme.font_body);
    push_var(&mut out, "radius-sm", &theme.radius_sm);
    push_var(&mut out, "radius-md", &theme.radius_md);
    push_var(&mut out, "radius-lg", &theme.radius_lg);
    push_var(&mut out, "shadow-sm", &theme.shadow_sm);
    push_var(&mut out, "shadow-md", &theme.shadow_md);
    push_var(&mut out, "shadow-lg", &theme.shadow_lg);
    // #274 — MD3 bridge: write the same palette into the
    // `--md-sys-color-*` namespace the chrome actually reads.
    emit_md3_bridge(
        &mut out,
        "  ",
        Md3Bridge {
            bg: &theme.color_bg,
            surface: &theme.color_surface,
            text_primary: &theme.color_text_primary,
            text_secondary: &theme.color_text_secondary,
            text_muted: &theme.color_text_muted,
            border: &theme.color_border,
            link: &theme.color_link,
            success: &theme.color_success,
            warning: &theme.color_warning,
            error: &theme.color_error,
            mode: BridgeMode::Light,
        },
    );
    // Brand colors — light variants here.
    for bc in brand_colors {
        emit_brand_color_block(&mut out, &bc.slug, &bc.color_value);
    }
    out.push_str("}\n");

    // -------- :root[data-theme="dark"] --------
    // `:root[…]` matches cms.css's specificity (0,2,0) so our overrides win
    // by source order. Using bare `[data-theme="dark"]` (0,1,0) loses to
    // cms.css's defaults and the theme switch becomes invisible.
    out.push_str(":root[data-theme=\"dark\"] {\n");
    push_var(&mut out, "color-bg", &theme.color_bg_dark);
    push_var(&mut out, "color-surface", &theme.color_surface_dark);
    push_var(
        &mut out,
        "color-text-primary",
        &theme.color_text_primary_dark,
    );
    push_var(
        &mut out,
        "color-text-secondary",
        &theme.color_text_secondary_dark,
    );
    push_var(&mut out, "color-text-muted", &theme.color_text_muted_dark);
    push_var(&mut out, "color-border", &theme.color_border_dark);
    emit_md3_bridge(
        &mut out,
        "  ",
        Md3Bridge {
            bg: pick_with_fallback(&theme.color_bg_dark, &theme.color_bg),
            surface: pick_with_fallback(&theme.color_surface_dark, &theme.color_surface),
            text_primary: pick_with_fallback(
                &theme.color_text_primary_dark,
                &theme.color_text_primary,
            ),
            text_secondary: pick_with_fallback(
                &theme.color_text_secondary_dark,
                &theme.color_text_secondary,
            ),
            text_muted: pick_with_fallback(&theme.color_text_muted_dark, &theme.color_text_muted),
            border: pick_with_fallback(&theme.color_border_dark, &theme.color_border),
            // Semantics + link reuse the light value — schema doesn't carry
            // dark overrides for these. The bridge derives container tints
            // appropriately for the dark surface inside `emit_md3_bridge`.
            link: &theme.color_link,
            success: &theme.color_success,
            warning: &theme.color_warning,
            error: &theme.color_error,
            mode: BridgeMode::Dark,
        },
    );
    // Brand colors with explicit dark overrides.
    for bc in brand_colors {
        let dark_value = if bc.color_value_dark.is_empty() {
            &bc.color_value
        } else {
            &bc.color_value_dark
        };
        emit_brand_color_block(&mut out, &bc.slug, dark_value);
    }
    out.push_str("}\n");

    // -------- @media (prefers-color-scheme: dark) for "system" --------
    // Must match cms.css's own OS-dark selector EXACTLY: `:root:not(
    // [data-theme="light"])`. "System" mode is represented by the ABSENCE of
    // `data-theme` (the admin's no-flash script removes the attribute), so
    // keying on `[data-theme="system"]` never matched — the theme's dark
    // palette (incl. the accent) was silently dropped in System mode and the
    // chrome fell back to cms.css's built-in dark defaults (the violet accent).
    out.push_str("@media (prefers-color-scheme: dark) {\n");
    out.push_str("  :root:not([data-theme=\"light\"]) {\n");
    push_var_indented(&mut out, "color-bg", &theme.color_bg_dark);
    push_var_indented(&mut out, "color-surface", &theme.color_surface_dark);
    push_var_indented(
        &mut out,
        "color-text-primary",
        &theme.color_text_primary_dark,
    );
    push_var_indented(
        &mut out,
        "color-text-secondary",
        &theme.color_text_secondary_dark,
    );
    push_var_indented(&mut out, "color-text-muted", &theme.color_text_muted_dark);
    push_var_indented(&mut out, "color-border", &theme.color_border_dark);
    emit_md3_bridge(
        &mut out,
        "    ",
        Md3Bridge {
            bg: pick_with_fallback(&theme.color_bg_dark, &theme.color_bg),
            surface: pick_with_fallback(&theme.color_surface_dark, &theme.color_surface),
            text_primary: pick_with_fallback(
                &theme.color_text_primary_dark,
                &theme.color_text_primary,
            ),
            text_secondary: pick_with_fallback(
                &theme.color_text_secondary_dark,
                &theme.color_text_secondary,
            ),
            text_muted: pick_with_fallback(&theme.color_text_muted_dark, &theme.color_text_muted),
            border: pick_with_fallback(&theme.color_border_dark, &theme.color_border),
            link: &theme.color_link,
            success: &theme.color_success,
            warning: &theme.color_warning,
            error: &theme.color_error,
            mode: BridgeMode::Dark,
        },
    );
    for bc in brand_colors {
        let dark_value = if bc.color_value_dark.is_empty() {
            &bc.color_value
        } else {
            &bc.color_value_dark
        };
        emit_brand_color_block_indented(&mut out, &bc.slug, dark_value);
    }
    out.push_str("  }\n");
    out.push_str("}\n");

    out.push_str("</style>\n");
    out
}

/// Inputs for the MD3 bridge. Wraps the resolved (light- or dark-variant)
/// colors so [`emit_md3_bridge`] doesn't grow a 12-arg signature.
struct Md3Bridge<'a> {
    bg: &'a str,
    surface: &'a str,
    text_primary: &'a str,
    text_secondary: &'a str,
    text_muted: &'a str,
    border: &'a str,
    link: &'a str,
    success: &'a str,
    warning: &'a str,
    error: &'a str,
    mode: BridgeMode,
}

#[derive(Clone, Copy)]
enum BridgeMode {
    /// Light surfaces — containers step DARKER than `surface`; primary
    /// containers tint LIGHTER toward white.
    Light,
    /// Dark surfaces — containers step LIGHTER than `surface`; primary
    /// containers tint DARKER toward black so they sit visually below
    /// the base primary against a dark page.
    Dark,
}

/// Map the theme's `color_*` columns onto the Material 3 token names
/// the admin chrome reads (`--md-sys-color-*`). Variants the schema
/// doesn't carry directly — surface container tiers, primary
/// container / fixed, on-* contrast pairs — are derived via mixing +
/// WCAG luminance so the resolved palette covers every name listed in
/// the `:root` block of `src/admin/static/cms.css`.
fn emit_md3_bridge(buf: &mut String, indent: &str, b: Md3Bridge<'_>) {
    let push = |buf: &mut String, name: &str, value: &str| push_var_at(buf, indent, name, value);

    // ---- Surfaces ----
    // Container tier steps; signs depend on mode (see [`BridgeMode`]).
    let (cstep_low, cstep, cstep_high, cstep_highest, cstep_dim, cstep_bright) = match b.mode {
        BridgeMode::Light => (0.03_f32, 0.06, 0.10, 0.14, 0.05, -0.05),
        BridgeMode::Dark => (-0.05_f32, -0.10, -0.15, -0.20, 0.05, -0.10),
    };
    push(buf, "md-sys-color-surface", b.bg);
    push(
        buf,
        "md-sys-color-surface-dim",
        &mix_or_same(b.bg, cstep_dim),
    );
    push(
        buf,
        "md-sys-color-surface-bright",
        &mix_or_same(b.bg, cstep_bright),
    );
    push(buf, "md-sys-color-surface-container-lowest", b.surface);
    push(
        buf,
        "md-sys-color-surface-container-low",
        &mix_or_same(b.surface, cstep_low),
    );
    push(
        buf,
        "md-sys-color-surface-container",
        &mix_or_same(b.surface, cstep),
    );
    push(
        buf,
        "md-sys-color-surface-container-high",
        &mix_or_same(b.surface, cstep_high),
    );
    push(
        buf,
        "md-sys-color-surface-container-highest",
        &mix_or_same(b.surface, cstep_highest),
    );
    push(buf, "md-sys-color-surface-variant", b.border);
    push(buf, "md-sys-color-on-surface", b.text_primary);
    push(buf, "md-sys-color-on-surface-variant", b.text_secondary);
    push(buf, "md-sys-color-outline", b.text_muted);
    push(buf, "md-sys-color-outline-variant", b.border);
    push(buf, "md-sys-color-inverse-surface", b.text_primary);
    push(buf, "md-sys-color-inverse-on-surface", b.surface);

    // ---- Primary (driven by color_link — the most visible accent) ----
    // Container/fixed derivations tinted appropriately per mode.
    let (p_container_t, p_fixed_t, p_fixed_dim_t, p_on_fixed_variant_t) = match b.mode {
        BridgeMode::Light => (-0.35_f32, -0.78, -0.55, 0.35),
        BridgeMode::Dark => (0.30_f32, -0.55, -0.30, 0.55),
    };
    let primary_container = mix_or_same(b.link, p_container_t);
    let primary_fixed = mix_or_same(b.link, p_fixed_t);
    let primary_fixed_dim = mix_or_same(b.link, p_fixed_dim_t);
    let on_primary_fixed_variant = mix_or_same(b.link, p_on_fixed_variant_t);
    push(buf, "md-sys-color-primary", b.link);
    push(buf, "md-sys-color-on-primary", wcag_foreground(b.link));
    push(buf, "md-sys-color-primary-container", &primary_container);
    push(
        buf,
        "md-sys-color-on-primary-container",
        wcag_foreground(&primary_container),
    );
    push(buf, "md-sys-color-primary-fixed", &primary_fixed);
    push(buf, "md-sys-color-primary-fixed-dim", &primary_fixed_dim);
    push(buf, "md-sys-color-on-primary-fixed", b.link);
    push(
        buf,
        "md-sys-color-on-primary-fixed-variant",
        &on_primary_fixed_variant,
    );
    push(buf, "md-sys-color-surface-tint", b.link);
    push(buf, "md-sys-color-inverse-primary", &primary_fixed_dim);

    // ---- Secondary (driven by text_muted — neutral chip accent) ----
    let secondary_container = mix_or_same(
        b.text_muted,
        match b.mode {
            BridgeMode::Light => -0.55,
            BridgeMode::Dark => 0.35,
        },
    );
    push(buf, "md-sys-color-secondary", b.text_muted);
    push(
        buf,
        "md-sys-color-on-secondary",
        wcag_foreground(b.text_muted),
    );
    push(
        buf,
        "md-sys-color-secondary-container",
        &secondary_container,
    );
    push(
        buf,
        "md-sys-color-on-secondary-container",
        wcag_foreground(&secondary_container),
    );

    // ---- Tertiary (driven by link too — themes only carry one accent) ----
    let tertiary_container = mix_or_same(
        b.link,
        match b.mode {
            BridgeMode::Light => -0.25,
            BridgeMode::Dark => 0.20,
        },
    );
    let tertiary_fixed_t = match b.mode {
        BridgeMode::Light => -0.65,
        BridgeMode::Dark => -0.40,
    };
    let tertiary_fixed = mix_or_same(b.link, tertiary_fixed_t);
    let tertiary_fixed_dim = mix_or_same(b.link, tertiary_fixed_t - 0.20);
    push(buf, "md-sys-color-tertiary", b.link);
    push(buf, "md-sys-color-on-tertiary", wcag_foreground(b.link));
    push(buf, "md-sys-color-tertiary-container", &tertiary_container);
    push(
        buf,
        "md-sys-color-on-tertiary-container",
        wcag_foreground(&tertiary_container),
    );
    push(buf, "md-sys-color-tertiary-fixed", &tertiary_fixed);
    push(buf, "md-sys-color-tertiary-fixed-dim", &tertiary_fixed_dim);

    // ---- Semantic colors ----
    emit_semantic_pair(buf, indent, "success", b.success, b.mode);
    emit_semantic_pair(buf, indent, "warning", b.warning, b.mode);
    emit_semantic_pair(buf, indent, "error", b.error, b.mode);
}

/// Emit the four-tuple of `--md-sys-color-<name>` / `-on-<name>` /
/// `-<name>-container` / `-on-<name>-container` for a single semantic
/// hue (success / warning / error). Container tinted away from the
/// base in the surface direction; foreground picked via WCAG luminance.
fn emit_semantic_pair(buf: &mut String, indent: &str, name: &str, value: &str, mode: BridgeMode) {
    if value.is_empty() {
        return;
    }
    let container_t = match mode {
        BridgeMode::Light => -0.55_f32,
        BridgeMode::Dark => 0.40,
    };
    let on_container_t = match mode {
        BridgeMode::Light => 0.40_f32,
        BridgeMode::Dark => -0.55,
    };
    // The base hue is authored for light surfaces; on dark it must
    // lighten or text/border uses (field errors, tree badges) fall
    // below readable contrast (MD3 itself ships #ba1a1a → #ffb4ab).
    let base = match mode {
        BridgeMode::Light => value.to_string(),
        BridgeMode::Dark => mix_or_same(value, -0.60),
    };
    let container = mix_or_same(value, container_t);
    let on_container = mix_or_same(value, on_container_t);
    let write = |buf: &mut String, var: &str, val: &str| {
        push_var_at(buf, indent, &format!("md-sys-color-{var}"), val);
    };
    write(buf, name, &base);
    write(buf, &format!("on-{name}"), wcag_foreground(&base));
    write(buf, &format!("{name}-container"), &container);
    write(buf, &format!("on-{name}-container"), &on_container);
}

/// Mix `hex` toward white/black by `t` and return the result — or fall
/// back to the original string when the input isn't 6-digit hex (named
/// colors, gradients, `rgb(…)`). The caller usually wants to emit
/// SOMETHING even on a non-hex value rather than swallowing the var.
fn mix_or_same(hex: &str, t: f32) -> String {
    mix_with_neutral(hex, t).unwrap_or_else(|| hex.to_string())
}

/// Pick `primary` if non-empty, else `fallback`. Used to resolve the
/// dark-mode value for surface columns where the `_dark` companion may
/// be blank (the schema lets editors skip them — light value carries
/// through). Returning &str keeps the call sites borrow-clean.
fn pick_with_fallback<'a>(primary: &'a str, fallback: &'a str) -> &'a str {
    if primary.is_empty() {
        fallback
    } else {
        primary
    }
}

fn push_var(buf: &mut String, name: &str, value: &str) {
    push_var_at(buf, "  ", name, value);
}

fn push_var_indented(buf: &mut String, name: &str, value: &str) {
    push_var_at(buf, "    ", name, value);
}

/// Append `--name: value;` — or nothing, when either half could end the
/// declaration, the rule or the `<style>` element (#739). Theme and brand
/// colour rows are editable by any user with change permission on those
/// models, and this output reaches every public page and the admin chrome
/// with `| safe`, so a value is data and must stay inside its declaration.
fn push_var_at(buf: &mut String, indent: &str, name: &str, value: &str) {
    if value.is_empty() || !is_css_ident(name) || !is_inert_css_value(value) {
        return;
    }
    buf.push_str(indent);
    buf.push_str("--");
    buf.push_str(name);
    buf.push_str(": ");
    buf.push_str(value);
    buf.push_str(";\n");
}

fn is_css_ident(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A custom-property value that cannot escape its declaration: no `;` `{`
/// `}` to end it, no `<` to close the `<style>` element, no `\` escapes,
/// no comment opener and no control characters.
fn is_inert_css_value(value: &str) -> bool {
    !value.contains("/*")
        && !value
            .chars()
            .any(|c| matches!(c, ';' | '{' | '}' | '<' | '>' | '\\') || c.is_control())
}

fn html_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn emit_brand_color_block(buf: &mut String, slug: &str, value: &str) {
    push_var(buf, &format!("color-{slug}"), value);
    if let Some(rgb) = hex_to_rgb_triplet(value) {
        push_var(buf, &format!("color-{slug}-rgb"), &rgb);
        push_var(
            buf,
            &format!("color-{slug}-contrast"),
            wcag_foreground(value),
        );
        // 50 → 950 shade scale.
        for (suffix, t) in SHADE_LEVELS {
            if let Some(shade) = mix_with_neutral(value, *t) {
                push_var(buf, &format!("color-{slug}-{suffix}"), &shade);
            }
        }
    }
}

fn emit_brand_color_block_indented(buf: &mut String, slug: &str, value: &str) {
    push_var_indented(buf, &format!("color-{slug}"), value);
    if let Some(rgb) = hex_to_rgb_triplet(value) {
        push_var_indented(buf, &format!("color-{slug}-rgb"), &rgb);
        push_var_indented(
            buf,
            &format!("color-{slug}-contrast"),
            wcag_foreground(value),
        );
        for (suffix, t) in SHADE_LEVELS {
            if let Some(shade) = mix_with_neutral(value, *t) {
                push_var_indented(buf, &format!("color-{slug}-{suffix}"), &shade);
            }
        }
    }
}

// Tailwind-aligned scale. Negative t mixes toward white, positive
// toward black. Tuned to roughly land where Tailwind 50/100/.../950
// sit relative to the 500 base.
const SHADE_LEVELS: &[(&str, f32)] = &[
    ("50", -0.92),
    ("100", -0.82),
    ("200", -0.62),
    ("300", -0.40),
    ("400", -0.20),
    ("500", 0.0),
    ("600", 0.14),
    ("700", 0.30),
    ("800", 0.48),
    ("900", 0.66),
    ("950", 0.82),
];

/// Parse `rrggbb` (an optional leading `#` is dropped) into bytes.
///
/// Checks the digits before slicing: `len()` counts bytes, so a 6-byte
/// non-ASCII string (`"aé€"`) would otherwise pass a length check and
/// panic on a char boundary (#687).
pub(crate) fn parse_rgb6(hex: &str) -> Option<(u8, u8, u8)> {
    let s = hex.trim_start_matches('#');
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

/// Parse `#rrggbb` (with or without `#`, case-insensitive) into a
/// space-separated `"r g b"` triplet suitable for
/// `rgb(var(--…-rgb) / 50%)` Tailwind syntax. Returns `None` for
/// anything that isn't a 6-digit hex (gradients, named colors, etc.)
/// — those get no `-rgb` companion and no shade scale.
fn hex_to_rgb_triplet(hex: &str) -> Option<String> {
    let (r, g, b) = parse_rgb6(hex.trim())?;
    Some(format!("{r} {g} {b}"))
}

/// Mix `hex` with white (t<0) or black (t>0) by ratio `|t|`. Returns
/// the result as `#rrggbb`. Used to synthesize a 50→950 shade scale
/// from a single base color.
fn mix_with_neutral(hex: &str, t: f32) -> Option<String> {
    let (r, g, b) = parse_rgb6(hex.trim())?;
    let (r, g, b) = (r as f32, g as f32, b as f32);
    let (target, ratio) = if t < 0.0 { (255.0, -t) } else { (0.0, t) };
    let mix = |c: f32| (c + (target - c) * ratio).clamp(0.0, 255.0) as u8;
    Some(format!("#{:02x}{:02x}{:02x}", mix(r), mix(g), mix(b)))
}

/// WCAG-style luminance check; returns `#000` for light backgrounds,
/// `#fff` for dark ones. Good enough for body text on a brand color.
fn wcag_foreground(hex: &str) -> &'static str {
    let Some((r, g, b)) = parse_rgb6(hex.trim()) else {
        return "#fff";
    };
    let lum = 0.2126 * (r as f32) + 0.7152 * (g as f32) + 0.0722 * (b as f32);
    if lum > 140.0 {
        "#0a0a0a"
    } else {
        "#ffffff"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_theme_values_never_leave_their_declaration() {
        let mut theme = fixture_theme();
        theme.slug = "x\"><script>alert(1)</script>".into();
        theme.color_link = "red}</style><svg onload=alert(1)>".into();
        theme.font_body = "a; } body { display: none".into();
        theme.color_bg = "#fff /* swallow the rest".into();
        theme.color_surface = "\"Inter\", 'Helvetica Neue', sans-serif".into();
        let brand = BrandColor {
            id: Auto::Unset,
            theme_id: 1,
            slug: "x:root{}".into(),
            name: "Hostile".into(),
            color_value: "#123456".into(),
            color_value_dark: String::new(),
            sort_order: 0,
        };
        let css = emit_css(&theme, &[brand]);
        assert!(!css.contains("</style><") && !css.contains("<svg") && !css.contains("<script"));
        assert!(!css.contains("display: none") && !css.contains("swallow"));
        assert!(!css.contains("x:root"), "a hostile brand slug is not a property name");
        assert!(css.contains("--color-surface: \"Inter\", 'Helvetica Neue', sans-serif;"));
        assert_eq!(css.matches("<style").count(), 1);
        assert_eq!(css.matches("</style>").count(), 1);
    }

    #[test]
    fn six_byte_non_ascii_colours_are_rejected_not_panicked_on() {
        // 1 + 2 + 3 bytes: passes a byte-length check, index 2 is mid-char.
        let bad = "a\u{e9}\u{20ac}";
        assert_eq!(parse_rgb6(bad), None);
        assert_eq!(hex_to_rgb_triplet(bad), None);
        assert_eq!(mix_with_neutral(bad, 0.5), None);
        assert_eq!(wcag_foreground(bad), "#fff");
        assert_eq!(parse_rgb6("#3B82f6"), Some((59, 130, 246)));
        assert_eq!(parse_rgb6("+1+2+3"), None, "from_str_radix alone accepts a sign");
    }

    #[test]
    fn hex_to_rgb_parses_six_digit() {
        assert_eq!(hex_to_rgb_triplet("#3b82f6"), Some("59 130 246".to_owned()));
        assert_eq!(hex_to_rgb_triplet("3b82f6"), Some("59 130 246".to_owned()));
    }

    #[test]
    fn hex_to_rgb_rejects_non_hex() {
        assert_eq!(hex_to_rgb_triplet("linear-gradient(...)"), None);
        assert_eq!(hex_to_rgb_triplet("#fff"), None); // 3-digit not supported yet
        assert_eq!(hex_to_rgb_triplet("rgb(0,0,0)"), None);
    }

    #[test]
    fn mix_toward_white_brightens() {
        let lighter = mix_with_neutral("#2E7D32", -0.5).unwrap();
        let original_r = 0x2e;
        let lighter_r =
            u8::from_str_radix(lighter.trim_start_matches('#').get(0..2).unwrap(), 16).unwrap();
        assert!(lighter_r > original_r);
    }

    #[test]
    fn mix_toward_black_darkens() {
        let darker = mix_with_neutral("#2E7D32", 0.5).unwrap();
        let original_g = 0x7d;
        let darker_g =
            u8::from_str_radix(darker.trim_start_matches('#').get(2..4).unwrap(), 16).unwrap();
        assert!(darker_g < original_g);
    }

    #[test]
    fn foreground_picks_white_on_dark() {
        assert_eq!(wcag_foreground("#1B5E20"), "#ffffff");
    }

    #[test]
    fn foreground_picks_black_on_light() {
        assert_eq!(wcag_foreground("#fafdf6"), "#0a0a0a");
    }

    /// Build a minimal theme suitable for snapshotting the emitter
    /// without depending on a `Default` impl on the rustango `Auto<…>`
    /// columns. Only the columns `emit_css` reads matter.
    fn fixture_theme() -> Theme {
        Theme {
            id: Auto::default(),
            name: "Fixture".into(),
            slug: "fixture".into(),
            is_admin_default: false,
            is_default: false,
            default_mode: "light".into(),
            color_bg: "#ffffff".into(),
            color_surface: "#fafafa".into(),
            color_text_primary: "#111111".into(),
            color_text_secondary: "#444444".into(),
            color_text_muted: "#777777".into(),
            color_border: "#dddddd".into(),
            color_bg_dark: "#0a0a0a".into(),
            color_surface_dark: "#141414".into(),
            color_text_primary_dark: "#f5f5f5".into(),
            color_text_secondary_dark: "#bbbbbb".into(),
            color_text_muted_dark: "#888888".into(),
            color_border_dark: "#222222".into(),
            color_success: "#22a06b".into(),
            color_warning: "#d97706".into(),
            color_error: "#ba1a1a".into(),
            color_info: "#3b82f6".into(),
            color_link: "#6366f1".into(),
            color_focus_ring: "#6366f1".into(),
            font_heading: "system-ui".into(),
            font_body: "system-ui".into(),
            font_url: String::new(),
            radius_sm: "2px".into(),
            radius_md: "4px".into(),
            radius_lg: "8px".into(),
            shadow_sm: "0 1px 2px #0001".into(),
            shadow_md: "0 4px 12px #0001".into(),
            shadow_lg: "0 12px 28px #0001".into(),
            created_at: Auto::default(),
            updated_at: Auto::default(),
        }
    }

    /// #274 — every MD3 token the chrome reads from `cms.css` must be
    /// covered by `emit_css` so picking a theme actually swings the
    /// palette. Cross-check the bridge against the full list of
    /// `--md-sys-color-*` consumers; missing one means switching that
    /// hue is silently a no-op.
    #[test]
    fn emit_css_bridges_md3_namespace() {
        let css = emit_css(&fixture_theme(), &[]);
        // Sanity: legacy namespace still present (back-compat).
        assert!(
            css.contains("--color-bg: #ffffff"),
            "legacy --color-bg missing"
        );
        assert!(css.contains("--color-link: #6366f1"));
        // Surface family — picked-theme bg/surface drive every tier.
        assert!(css.contains("--md-sys-color-surface: #ffffff"));
        assert!(css.contains("--md-sys-color-surface-container-lowest: #fafafa"));
        assert!(css.contains("--md-sys-color-surface-container-low:"));
        assert!(css.contains("--md-sys-color-surface-container:"));
        assert!(css.contains("--md-sys-color-surface-container-high:"));
        assert!(css.contains("--md-sys-color-surface-container-highest:"));
        assert!(css.contains("--md-sys-color-on-surface: #111111"));
        assert!(css.contains("--md-sys-color-on-surface-variant: #444444"));
        assert!(css.contains("--md-sys-color-outline: #777777"));
        assert!(css.contains("--md-sys-color-outline-variant: #dddddd"));
        // Accent — link drives MD3 primary so theme switch shifts the
        // pickup color across buttons / sidebar active / focus rings.
        assert!(css.contains("--md-sys-color-primary: #6366f1"));
        assert!(css.contains("--md-sys-color-primary-container:"));
        assert!(css.contains("--md-sys-color-primary-fixed:"));
        assert!(css.contains("--md-sys-color-on-primary:"));
        // Semantic family.
        assert!(css.contains("--md-sys-color-success: #22a06b"));
        assert!(css.contains("--md-sys-color-on-success:"));
        assert!(css.contains("--md-sys-color-success-container:"));
        assert!(css.contains("--md-sys-color-warning: #d97706"));
        assert!(css.contains("--md-sys-color-error: #ba1a1a"));
        assert!(css.contains("--md-sys-color-error-container:"));
    }

    /// #274 — the dark + system blocks have to match cms.css's
    /// specificity (`:root[data-theme="dark"]`, 0,2,0). A bare
    /// `[data-theme="dark"]` selector loses the cascade against
    /// cms.css's built-in dark block and the theme switch becomes
    /// silently invisible in dark mode.
    #[test]
    fn emit_css_uses_root_qualified_selectors_for_dark_and_system() {
        let css = emit_css(&fixture_theme(), &[]);
        assert!(
            css.contains(":root[data-theme=\"dark\"] {"),
            "dark block must be :root-qualified to match cms.css specificity"
        );
        // "System" is the ABSENCE of `data-theme` — the admin's no-flash
        // script removes the attribute — so the OS-dark block keys on
        // `:root:not([data-theme="light"])`, matching cms.css's own selector.
        // It used to key on `[data-theme="system"]`, which never matched and
        // silently dropped the tenant's dark palette in System mode; this
        // assertion tracked that superseded selector.
        assert!(
            css.contains(":root:not([data-theme=\"light\"]) {"),
            "system block (inside @media) must be :root-qualified and match cms.css's OS-dark selector"
        );
        // And no LOSING-specificity bare attribute selectors left over.
        assert!(
            !css.contains("\n[data-theme=\"dark\"] {"),
            "stray bare [data-theme=\"dark\"] would lose cascade"
        );
        assert!(
            !css.contains("[data-theme=\"system\"]"),
            "`system` is the absence of data-theme, so no selector should name it"
        );
    }

    /// Dark surfaces resolve from `_dark` columns so the bridge picks
    /// up the inverted palette there too.
    #[test]
    fn emit_css_dark_block_uses_dark_columns() {
        let css = emit_css(&fixture_theme(), &[]);
        // Slice down to the dark block to keep the assertion local.
        let dark = css
            .split(":root[data-theme=\"dark\"] {")
            .nth(1)
            .unwrap()
            .split("}\n")
            .next()
            .unwrap();
        assert!(dark.contains("--md-sys-color-surface: #0a0a0a"));
        assert!(dark.contains("--md-sys-color-surface-container-lowest: #141414"));
        assert!(dark.contains("--md-sys-color-on-surface: #f5f5f5"));
    }

    /// Semantic base hues are authored for light surfaces; the dark
    /// blocks must lighten them or error/success/warning text and
    /// borders (field validation, block-tree badges) go unreadable
    /// against dark backgrounds.
    #[test]
    fn emit_css_dark_block_lightens_semantic_bases() {
        let css = emit_css(&fixture_theme(), &[]);
        let dark = css
            .split(":root[data-theme=\"dark\"] {")
            .nth(1)
            .unwrap()
            .split("}\n")
            .next()
            .unwrap();
        assert!(
            !dark.contains("--md-sys-color-error: #ba1a1a"),
            "dark block must not reuse the light error hue verbatim"
        );
        let err_line = dark
            .lines()
            .find(|l| l.contains("--md-sys-color-error:"))
            .expect("dark block emits an error base");
        let hex = err_line
            .split(':')
            .nth(1)
            .unwrap()
            .trim()
            .trim_end_matches(';');
        let r = u8::from_str_radix(&hex[1..3], 16).unwrap();
        assert!(
            r > 0xba,
            "dark error base should be lighter than the light hue, got {hex}"
        );
    }
}
