# rustango-cms i18n / l10n design

**Decided 2026-05-11** before implementing v0.2 Slice 4. The decision
locks the migration shape; revisiting requires a new migration chain.

## Choice: canonical row + per-field translation table + variant escape hatch

The default layout for translatable content is:

1. **One canonical row in `cms_page`** per page, in the tenant's
   default locale (the row with `cms_locale.is_default = true`).
2. **`cms_translation`** holds per-field, per-locale overrides keyed
   on `(page_id, locale_id, field_path)`. Empty fields fall back to
   the canonical row's value at render time.
3. **`cms_page.locale_variant_of: Option<i64>` self-FK** as the escape
   hatch for locales that need a structurally-different page (e.g. a
   Japanese homepage with sections the English version lacks). The
   variant is a fully separate `Page` row whose `locale_variant_of`
   points at the canonical id. The resolver checks for a variant
   first; if missing, falls back to canonical + translations.

## Rejected alternatives

| Approach | Rejected because |
| --- | --- |
| **Per-locale tree** | Doubles the page tree per locale. Translators end up out of sync with each other, and tree-walking is per-locale. The `locale_variant_of` escape hatch gives us the same flexibility for the 5% of pages that genuinely need it, without paying the duplication cost on the other 95%. |
| **Field-level i18n** | Storing `{en: "…", fr: "…"}` per field is natural in JSON-shaped DBs but awkward in PG/MySQL where each field is a column. Forces a JSONB blob per page, breaks ordinary SELECT. |
| **Routing prefix only** | Doesn't ship a translation workflow at all — translators have nowhere to enter content. Strictly less than what CMS users expect. |

## URL shape

`/<slug>/?lang=<locale_code>` for v0.2. Per-locale URL prefix
(`/en/<slug>`, `/fr/<slug>`) is **not in v0.2** — adds routing
complexity for marginal SEO win on the kind of small-site
deployments rustango-cms targets in 2026. Re-evaluate when a
production deployment actually needs it (then Slice 4.1 adds the
prefix-routing variant under a Settings flag).

## Schema (v0.2 Slice 4)

```rust
#[derive(Model)]
#[rustango(table = "cms_translation", app = "cms",
    unique_together = "page_id,locale_id,field_path")]
pub struct Translation {
    #[rustango(primary_key)] pub id: Auto<i64>,
    #[rustango(fk = "cms_page",   on = "id", index)] pub page_id:   i64,
    #[rustango(fk = "cms_locale", on = "id", index)] pub locale_id: i64,
    /// Field name on the page (e.g. "title", "seo_description").
    /// Slice 4.1 widens to dotted paths into JSON streamfield blocks.
    #[rustango(max_length = 64, index)] pub field_path: String,
    pub value: String,
    #[rustango(auto_now)] pub updated_at: Auto<DateTime<Utc>>,
}
```

Plus an `AddColumn` migration:

```text
+ AddColumn cms_page.locale_variant_of (Option<i64>, FK cms_page.id)
```

## Render flow

```
resolve_path → Page row
   │
   ├─ if request locale != default:
   │     look up Translation rows for (page.id, locale.id)
   │     into a `HashMap<String, String>` (field_path → value)
   │
   ├─ pass `translations` into Tera context
   │
   └─ template uses `{{ page.title | t(field="title", translations=translations) }}`
```

The `t` Tera filter returns the localized value when present, the
input (canonical) otherwise. This means a partially-translated page
gracefully falls back per-field instead of returning a half-blank
page.

## Admin authoring

A new "Translate" tab on the page edit form: side-by-side canonical
input (read-only) ↔ per-locale value (editable). Saves write
`cms_translation` rows. Empty value = no override = canonical
shows.

## "What's untranslated?" query

A `manage cms missing-translations --locale es` verb that walks
every published page and reports the field-paths that don't yet have
an `es` translation. Wired off the same `cms_translation` table.

## Deferred to Slice 4.1

- Tera filter coverage of dotted paths into streamfield JSON blocks.
- Per-locale URL prefix routing.
- `locale_variant_of` admin UI (the model + render flow ship in 4.0;
  the "Create locale variant" button is Slice 6 territory).
