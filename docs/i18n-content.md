# Content localization (public multilingual sites)

How a rustango-cms site serves its **content** — pages, StreamField bodies,
menus, snippets, SEO tags — in multiple languages. This is separate from
[**admin UI** localization](i18n-admin.md) (the admin's own labels), and the
schema/rationale behind the model is recorded in
[`i18n-decision.md`](i18n-decision.md).

| | Content locale (this doc) | Admin UI locale |
| --- | --- | --- |
| What | The pages/snippets visitors read | The admin's labels, buttons, nav |
| Store | `cms_locale` + `cms_translation` DB rows | Embedded JSON catalogs (`src/admin/locales/`) |
| Managed | Admin UI (`/cms-admin/locales`, per-page Translate) | Curated per release (`UI_LOCALES`) |
| Applied | Automatically at render (engine overlay) | `{{ "Save" \| translate(locale=LANG) }}` |

The model is **one canonical page + per-field overrides**: there is exactly one
page row per URL (English, say), and each non-default locale stores
`cms_translation` rows `(page_id, locale_id, field_path, value)`. No duplicate
trees, no per-locale page forests — a translation is an overlay, and any field
without one falls back to the canonical text per-field.

## Locales (`cms_locale`)

Content locales are DB rows managed at **Settings → Locales** in the admin:
`code`, display `name`, `is_default`, `active`, `sort_order`. Exactly one
locale is the default (promoting another demotes it automatically).

- **Codes are validated + normalized on save** (`locale::validate_code`):
  BCP-47 shape, `_` → `-`, language lowercased, script Titlecased, region
  uppercased — `PT_br` is stored as `pt-BR`; `bad code!` is rejected with a
  message. Any well-formed tag is a valid content locale.
- The add/edit form offers a **datalist of core-known locales** (from
  `rustango::i18n::known_locales()`) with a live support hint, but free text
  is allowed — content translation is code-agnostic.
- The locale list shows per-locale **"Core support" badges** driven by
  `rustango::i18n::locale_info`: *admin UI* (chrome translated), *CLDR
  plurals*, *RTL*, or ⚠ *content-only* / *unknown to core*. A content-only
  locale still fully translates content; only admin chrome and
  plural/display metadata fall back to English defaults.

## URLs and locale resolution

`PublicRouter` picks the URL shape via `LocaleMode`:

```rust
let public = rustango_cms::PublicRouter::new(tera.clone())
    .locale_mode(rustango_cms::LocaleMode::PathOrQuery) // /fr/blog + ?lang=fr
    .build();
```

- `Query` — `?lang=fr` only (default).
- `Path` — `/fr/blog`.
- `PathOrQuery` — both; the switcher emits path URLs.

Resolution precedence per request: explicit `?lang=` → explicit path prefix →
the `rcms_locale` **persistence cookie** → `Accept-Language` → the default
locale. The cookie is (re)written only on explicit choices, so a visitor who
picks a language keeps it on bare links — templates never prefix URLs by hand.

## Translations (`cms_translation`)

Each row overrides one field of one page in one locale:

- **Scalar fields** — `field_path` is the field name: `title`, `intro`,
  `hero_title`, `seo_description`, …
- **StreamField body leaves** — `field_path` is a dotted block path:
  `body.<block-uuid>.<leaf>` (e.g. `body.5c0e….text`). The translatable leaves
  of any body are enumerated by
  `rustango_cms::block::translate::collect_translatable_leaves`.

Rows are authored in the admin (per-page **Translate** view) or seeded
programmatically (upsert on `(page_id, locale_id, field_path)`).

### The engine applies them — templates stay plain

On a non-default-locale request the render layer localizes the whole template
context before Tera runs:

- `page`, `extension`, `children`, `ancestors`, and any handler-supplied
  card/list objects that carry a page id get their scalar fields overlaid
  (`translation::overlay_translations`) — including grandchildren, so an
  index-of-index page (home page showing post cards two levels down)
  translates its teasers too.
- StreamField bodies are localized during stream prerender, so
  `{{ _stream_html['body'] | safe }}` is already translated.
- `auto_menu()` items and the page `<title>` localize the same way.
- Snippets get per-field `snippet_translation` overrides at prefetch.

Templates therefore render **plain fields** — `{{ page.title }}`,
`{{ p.excerpt }}` — and receive localized output. No per-field filter calls,
no locale-aware links.

For the rare object the overlay can't identify (no page id), the explicit `t`
filter is still available:

```jinja
{{ page.title | t(field="title", translations=translations) }}
{{ c.title | t(field="title", by_page=translations_by_page, page_id=c.page_id) }}
```

### Template context

Every public render exposes:

| Var | Value |
| --- | --- |
| `locale` | the resolved `cms_locale` row (`locale.code`, `locale.is_default`) |
| `LANG` | the active locale code (`"fr"`); feeds `translate(locale=LANG)` for chrome strings |
| `DIR` | `"ltr"` / `"rtl"` for the active locale — set `<html lang="{{ LANG }}" dir="{{ DIR }}">` |
| `translations` | flat `field → value` map for the current page |
| `translations_by_page` | `page_id → {field → value}` for related pages |

An RTL content locale (`ar`, `he`, `fa`, …) added purely via `cms_locale`
renders `dir="rtl"` with zero extra configuration.

## The language switcher

`language_switcher()` returns the active locales with the URL to the *current*
page in each (shaped per the configured `LocaleMode`), the current one
flagged:

```jinja
{% set langs = language_switcher() %}
{% if langs | length > 1 %}
<nav aria-label="Language">
  {% for l in langs %}
    <a href="{{ l.url }}"{% if l.is_current %} aria-current="true"{% endif %}
       hreflang="{{ l.code }}">{{ l.name }}</a>
  {% endfor %}
</nav>
{% endif %}
```

The default locale's link carries an explicit `?lang=` so clicking it resets a
stale persistence cookie. (Entries are installed per-request by the render
pipeline and survive executor thread-hops — no flakiness.)

## SEO: hreflang + per-locale sitemaps

Search engines and audit tools discover the language versions through two
engine-provided surfaces (both no-ops on a single-language site):

- **`<head>` alternates** — one call in the base template:

  ```jinja
  {{ rcms_hreflang_tags(origin=site_origin) | safe }}
  {# → <link rel="alternate" hreflang="fr" href="https://…/fr/blog"/> … + x-default #}
  ```

- **`/sitemap.xml`** — when ≥1 non-default locale is active, every page emits
  one `<url>` per locale (URL shape per `LocaleMode`), each carrying the full
  bidirectional `xhtml:link` hreflang cluster plus `x-default` → the default
  URL. Sharding (`/sitemap/{n}`) behaves the same.

Together with `<html lang dir>` and the switcher's `hreflang` links, crawlers
see all locales without any consumer wiring beyond the one head tag.

## Seeding translations programmatically

A seeder is an upsert loop over `(page_id, locale_id, field_path, value)`:

```rust
use rustango_cms::block::translate::collect_translatable_leaves;
use rustango_cms::translation::Translation;

// scalar
upsert(pool, page_id, fr_id, "title", "Accueil").await?;

// every body leaf
let body: serde_json::Value = serde_json::from_str(&ext.body)?;
for leaf in collect_translatable_leaves("body", &body) {
    if let Some(tr) = catalog.get(&leaf.canonical_text) {
        upsert(pool, page_id, fr_id, &leaf.path, tr).await?;
    }
}
```

Missing entries are simply skipped — the per-field fallback means a partially
translated page renders canonical text for the gaps, never blanks.

## See also

- [`i18n-admin.md`](i18n-admin.md) — the admin UI's own localization
  (catalogs, `translate` filter, `UI_LOCALES`, the CI completeness gate).
- [`i18n-decision.md`](i18n-decision.md) — why one-canonical-row +
  per-field overrides (and the rejected alternatives).
- Framework guide `rustango/docs/i18n.md` — `Translator`, plurals,
  negotiation, and the `locale_info` / `known_locales` capability registry
  the locale badges are built on.
