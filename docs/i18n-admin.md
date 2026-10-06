# Admin UI localization (i18n)

How the **CMS admin chrome** is translated, and how to add a language or a
new string. This is separate from **content** translation (translating the
pages/snippets editors author) — that uses the `cms_translation` table and
the render-layer overlay, documented in [`i18n-content.md`](i18n-content.md)
(design rationale in [`i18n-decision.md`](i18n-decision.md)).

| | Admin UI locale (this doc) | Content locale |
| --- | --- | --- |
| What | The admin's own labels, buttons, nav | The pages/snippets being edited |
| Tera | `{{ "Save" \| translate(locale=LANG) }}` | `{{ page.title \| t(field="title", …) }}` |
| Store | Embedded JSON catalogs (`src/admin/locales/`) | `cms_translation` DB rows |
| Source | English string is the key | Canonical page row |

The two are intentionally **distinct** notions and resolve independently.

## How it works

The admin Tera is wired to the framework message catalog
(`rustango::i18n::Translator`) in `admin::register_templates`, which registers
the `translate` filter/function. Templates localize a string by passing the
active locale:

```jinja
{{ "Save" | translate(locale=LANG) }}
<button title="{{ "Delete" | translate(locale=LANG) }}">…</button>
```

- The **key is the English source string** (gettext-style). A missing
  translation falls back to the key, so an unwrapped-but-untranslated string
  renders in English rather than a blank or a raw id.
- `LANG` is injected into every admin page by `render_with_csrf` (the single
  render chokepoint) and defaulted by `add_chrome_sync`, so `locale=LANG` is
  always safe in any template that reaches the admin chrome.
- Attribute strings work too — `title="{{ "x" | translate(locale=LANG) }}"`
  renders the translation *before* the browser parses the attribute.

### Locale resolution (negotiation)

`admin::i18n::negotiate` picks the active admin locale per request:

1. The sticky `rcms_admin_lang` cookie (set by the language switcher).
2. `Accept-Language` — exact match, then base language (`fr-FR` → `fr`,
   `zh-*` → `zh-Hans`).
3. English.

It only ever returns a **shipped** locale. The language choice lives on
**Preferences**, which saves it to the user's profile. `POST
/cms-admin/set-language` with `lang=<code>` (CSRF-checked like any admin
form) sets the same cookie and preference and redirects back, for a custom
switcher.

### Server-side strings

Strings emitted from Rust (flash messages, validation errors) go through
`admin::i18n::tr`:

```rust
let msg = super::i18n::tr(&headers, "Draft saved.", &[]);
// with interpolation:
let msg = super::i18n::tr(&headers, "Created page “{title}”.", &[("title", &title)]);
```

The two flash redirect helpers (`redirect_named_with_message` and its
`_with_params` cousin) already route their body through `tr`, so any static
flash whose English text has a catalog entry is localized automatically.

## Shipped languages

Declared once in `admin::i18n::UI_LOCALES`:

`en` (source), `uk`, `pl`, `fr`, `de`, `zh-Hans`, `ja`. **Russian (`ru`) is
intentionally excluded.**

Each non-English locale has a catalog at `src/admin/locales/<code>.json`
mapping English source → translation. `en.json` stays `{}` (the source needs
no catalog). Catalogs are embedded at compile time via `include_str!`.

## Adding a new string

1. Wrap it in the template: `{{ "My new label" | translate(locale=LANG) }}`
   (or call `tr(&headers, "My new label", &[])` from a handler).
2. Add `"My new label": "…"` to **every** `src/admin/locales/<code>.json`.
3. Run the completeness check (below). It fails until every launch locale has
   a non-empty entry.

To list every key the templates currently use:

```sh
grep -rhoE '"[^"]*"[[:space:]]*\|[[:space:]]*translate\(' src/admin/templates/ \
  | sed -E 's/"[[:space:]]*\|.*//; s/^"//' | sort -u
```

## Adding a new language

1. Add `("<code>", "<Native name>")` to `UI_LOCALES` in `src/admin/i18n.rs`.
2. Create `src/admin/locales/<code>.json` with a translation for every key
   (start from the grep above, or copy `fr.json` and translate the values).
3. `admin_translator` and the switcher pick it up automatically from
   `UI_LOCALES`.
4. Run the completeness check. It now requires the new locale to be complete.

## Plurals (count-aware strings)

Count-based strings use CLDR plural rules (framework `translate_plural`, #1102):

```rust
// handler: pick the form for `n` in the request locale + interpolate
super::i18n::tr_plural(&headers, "Deleted {count} page(s).", n, &[("count", &n.to_string())]);
// flash helpers: redirect_named_with_message_plural / _with_params_and_message_plural
```

The plural catalogs live at `src/admin/locales/<code>.plurals.json`, one
per-CLDR-category object per key:

```json
"Deleted {count} page(s).": { "one": "Usunięto {count} stronę.",
                              "few": "Usunięto {count} strony.",
                              "many": "Usunięto {count} stron." }
```

Categories by language: `en`/`de`/`fr` → `one`/`other` (fr: 0 = one); `pl`/`uk`
→ `one`/`few`/`many`; `zh-Hans`/`ja` → `other` only. A missing form falls back to
`other`, then to the scalar source key — so English/untranslated still renders.
For sentences with **two** independent counts, compose: translate each
`{count} noun(s)` phrase with `tr_plural`, then inject them into a scalar
sentence template via `tr`.

## Completeness check (CI gate)

`cargo test -p rustango-cms --lib admin::i18n` runs:

- `launch_catalogs_cover_every_template_key` — scans every admin template for
  `translate()` keys and fails if any launch locale is missing one.
- `plural_catalogs_cover_every_key_in_all_locales` — every plural key has a
  non-empty form set in every locale.
- `catalogs_parse_with_no_empty_values` — no key maps to a blank string.
- `unknown_key_falls_back_to_source_in_every_locale` — the fallback guarantee.
- `all_admin_templates_parse` — every bundled template parses + its
  inheritance chain resolves, so a sweep typo fails the build, not a live page.

## RTL

`LANG_DIR` (from `rustango::i18n::text_direction`) drives `<html dir>` on every
admin page. No RTL language ships today, but the chrome is ready for Arabic /
Hebrew: adding one to `UI_LOCALES` + a catalog is enough for `dir="rtl"` to
take effect.

## Known gaps / follow-ups

- The visual **Form Builder** canvas (palette, inspector) renders its labels
  from `form_builder.js`; JS-side strings are not yet wrapped.
- A few **dynamic `format!` error strings** on rare paths still fall back to
  English (the `tr()` chokepoint renders them gracefully); convert per call
  site to `tr`/`tr_plural` as needed.
- The developer-facing `AdminError` debug dump (raw error + `caused by:` chain)
  is intentionally not localized.
- **Public-form** field validation is the *content* locale, not the admin UI
  locale — see [`i18n-decision.md`](i18n-decision.md).
