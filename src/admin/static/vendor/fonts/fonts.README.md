# Vendored admin fonts (#615)

The admin's typography and its **entire icon set** are served from here, not
from `fonts.googleapis.com`.

## Why they're vendored

Material Symbols is a *ligature* icon font: the markup contains the icon's name
as text and the font turns it into a glyph. When the CDN request failed —
offline, air-gapped, strict CSP, ad blocker — every icon in the chrome rendered
as its own name, so the sidebar read `article`, `error`, `image`. The admin also
sent each editor's IP to a third party on every page load. Same reasoning as the
vendored TipTap bundle (#294).

## What's here

| File | Contents |
|---|---|
| `fonts.css` | `@font-face` rules, served at `/cms-admin/static/fonts.css` |
| `text-*.woff2` | Hanken Grotesk (UI) + Literata (editor body) — SIL Open Font License 1.1 |
| `icons-00.woff2` | Material Symbols Rounded, variable |

Subsets: **latin, latin-ext, cyrillic, cyrillic-ext** — enough for the shipped
admin locales (de/fr/pl/uk). Japanese and Chinese were never covered by these
families and still fall back to the system CJK face.

Total ≈ 656 KB, of which the icon font is ≈ 361 KB.

## Rules

- URLs inside `fonts.css` are **absolute** (`/cms-admin/static/vendor/fonts/…`).
  Relative ones resolve against `/cms-admin/static/` and 404.
- Every `.woff2` must be listed in `ADMIN_FONT_FILES` (`src/admin/mod.rs`) or it
  won't be served. The route 404s unknown names rather than falling through to
  the CMS page router.
- Files are served `immutable`; the stylesheet link carries
  `?v={{ cms_asset_version() }}` so an updated `fonts.css` isn't masked by a
  cached copy. **If you replace a `.woff2`, change its filename** — the bytes are
  cached for a year under the old name.
- Keep the system fallbacks in the CSS font stacks, so a missing file degrades to
  a normal sans-serif instead of naked ligature text.

## Regenerating

1. Fetch the Google Fonts CSS for the families you want, with a modern browser
   User-Agent (otherwise you get TTF instead of woff2).
2. Download every `url(...)` it references, keeping only the subsets listed
   above.
3. Rewrite each `url(...)` to `/cms-admin/static/vendor/fonts/<file>`.
4. Update `ADMIN_FONT_FILES` in `src/admin/mod.rs` to match the directory.

Theme presets in `fixtures/cms_theme.json` deliberately ship an **empty**
`font_url`: they'd otherwise re-request the same families from the CDN. That
field remains the escape hatch for a site that genuinely wants a different
webfont. Note the theme seed skips when any theme row already exists, so
**installs seeded before this change keep their old CDN URL** until the row is
updated.

## Licences

Hanken Grotesk and Literata are under the SIL Open Font License 1.1; Material
Symbols Rounded is under Apache-2.0. Their full notices, with those of every
other vendored admin asset, are in `../THIRD-PARTY-NOTICES.txt`, compiled into
the binary and served at `/cms-admin/static/vendor/THIRD-PARTY-NOTICES.txt`.
