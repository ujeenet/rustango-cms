# Upgrading

What to change in a host application when upgrading rustango-cms. Find
your symptom below; each entry names the change and the fix. The full list
is in [CHANGELOG.md](CHANGELOG.md).

## Unreleased

### rustango 0.60

Bump your `rustango` requirement to `0.60` and run `migrate`. Follow
rustango's own UPGRADING notes from 0.58 on (`atomic!` hands out an `AtomicTx`).

### Admin overrides lost their styling

Every admin CSS class now carries the `rcms-` prefix (#314): `.btn` is
`.rcms-btn`, `.field` is `.rcms-field`, and so on. State classes (`active`,
`open`, `is-*`) and the public-site classes are unchanged. Rename the
classes in any admin template you override (`rcms_admin/*.html`) and in any
host CSS that targets the admin.

### A tenant's own media bucket is no longer used

Tenant disks are registered as `tenant:<slug>` so a slug can't select one
of the host's own disks (#740). If you register a per-tenant disk in code,
name it `media_storage::tenant_disk_name(slug)`. `CMS_MEDIA_TENANT_BUCKETS`
needs no change.

### A new hostname stays "awaiting operator approval"

A hostname added on the Sites screen is held until an operator enables it,
unless it is under one of your own domains (#712). Set
`CMS_SITE_HOST_SUFFIXES` to them, comma-separated (for example
`sites.example.com`), to keep those self-service.

### `render_stream_editor` / `render_body` no longer compile

`block::admin::render_stream_editor`, `render_stream_editor_with` and
`page_builder::values::render_body` lost their `media_options` argument
(#717). Image pickers use the shared chooser; drop the argument.

### Editors can no longer publish a page type that has a workflow

A page type bound to a review workflow goes live only through its approval
(or a superuser's save). Code matching on `PageEditRefusal` needs an arm for
`ReviewRequired`; code building `PageEditOutcome` sets `held_for_review`.
Run the migrations: `0014_page_pending_change` keeps the changes to a live
page that wait for review.

### Code reading `?form_submitted=1` stopped matching

The flags now name the form: `?form_submitted=<form id>` and
`?form_error=<form id>:<field key>`. The bundled runtime handles both;
host scripts or templates that test for `=1` should test for the
parameter instead. `forms::schema::is_visible` takes
`HashMap<String, Vec<String>>` (every answer per field); build
`FormSettings` with `..Default::default()`.

### A language switcher link stopped working

`/cms-admin/set-language` is POST-only, so it goes through CSRF (#738).
Submit a form with the CSRF token instead of linking to it.

### Uploading an `.html` / `.js` / `.xml` file fails

Page-like files are refused at upload (#724). Serve them from a static
directory instead of the media library.

### A struct literal of `ScheduleSweepResult` no longer compiles

It gained `went_live` and `taken_down` (#692). Construct it with
`..Default::default()`.

### A `{:?}` of a form, or `gc_older_than`, no longer compiles

Forms that carry a password or secret dropped `Debug` so they can't leak it
to logs (#734); log the fields you need instead. `gc_older_than` now takes
`(&pool, tenant_slug, cutoff)` (#770).

### A struct literal of `EnrichCtx` no longer compiles

`block::enrich::EnrichCtx` is `#[non_exhaustive]`, so a new field breaks no
one later (#665). A host that drives the enrich walker itself builds it with
`EnrichCtx::new(pool, tenant, page_id)`. Enrichers that only read its fields
need no change.

### A struct literal of `PageType` no longer compiles

`page_type_model::PageType` has a new `workflow` field, the workflow a page
type made in the admin goes through (#843). Add `workflow: String::new()`
to any `PageType { … }` you build, for example in tests. Run the tenant
migrations: `0012_page_type_workflow` adds the column.

### `site_setting::prefetch_for_render` / `children_builder` no longer compile

`site_setting::prefetch_for_render` takes the visitor's locale code (`None`
for the default language) and returns each setting already translated.
`page_builder::values::children_builder` returns `(values, labels,
first_photo)` per child.

### `prefetch_for_pages` / `public_render` no longer compile

`category::assign::prefetch_for_pages` takes a third argument, the locale
id whose category names to use (#862), and
`page_builder::values::public_render` a last one, the locale code for choice
labels (#863). Pass `None` for the default language. Run the tenant
migrations: `0013_category_translation` adds the table.

### Queued purge jobs fail on a worker-only process

A worker that never builds the admin router has no cache invalidator.
Call `task_queue::set_default_invalidator` at startup (#732).
