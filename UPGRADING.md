# Upgrading

What to change in a host application when upgrading rustango-cms. Find
your symptom below; each entry names the change and the fix. The full list
is in [CHANGELOG.md](CHANGELOG.md).

## 0.4.0

### `inline_panels` no longer compiles

`PageTypeHandler::inline_panels(&self)` is now
`async fn inline_panels(&self, pool: &Pool, page_id: i64) -> Result<Vec<InlinePanelSpec>, ExecError>`.
Wrap the old list in `Ok(…)`. With `#[derive(PageType)]`, implement it on
`PageTypeOverrides` instead. An `InlinePanelSpec { … }` literal needs the new
fields; use `InlinePanelSpec::new` or `with_widgets`.

### "clashes with the page editor's own field"

A `#[field]` named `status`, `title`, `slug`, `tags`, `categories` or another
page-editor field is now refused. Rename the Rust field and keep the column:
`#[rustango(column = "status")] pub campaign_status: String`. Add
`#[serde(rename = "status")]` if templates read the old name.

### rustango 0.65

Bump `rustango` to `0.65` and `rustango-cms` to `0.4`, then follow
rustango's own 0.62.0 to 0.65.0 UPGRADING notes. The one most hosts hit: `Org` is `#[non_exhaustive]`, so an
`Org { … }` literal (usually in tests) becomes
`let mut org = Org::new(slug); org.field = …;`.

### Your page-type hooks loaded their own row

`public_context` and `route_context` now run on the page's stored row:
`self.<field>` is the page's value. Code that loaded the row by `page_id`
inside these hooks can use `self` instead. A hook that relied on `self`
being `Default::default()` now sees the real values.

### A `ModelAdmin { … }` literal no longer compiles

`ModelAdmin::list_rows` is now `fn(Pool, i64, i64)` (limit, offset) and
returns `(rows, total)`, so the generic model admin can page. Register
models with `register_model_admin!`, which builds it for you.

### A `BlockRegistration { … }` literal no longer compiles

`BlockRegistration` gained `origin`. Use `register_block!` (it fills it in),
or add `origin: module_path!()`.

### A block with the name of a built-in block now replaces it

Before, which of two same-named blocks won depended on link order. If your
block was meant to sit next to the built-in one, give it another name; the
log line "a site block replaces the built-in block" names the block.

### Editors lost a custom admin page

`/cms-admin/x/<slug>` now checks `<permission_resource>.view` (and `.edit`
for a form sent to it). Tick the page's row in **Management → Roles** for
the roles that use it. Pages without `permission_resource` are unchanged.

### `AdminPageCtx { … }` in a test no longer compiles

It gained `uri` and `action`. Pass the request URI and `None`.

### `verify_rendition_sig` no longer compiles

Use `rendition::check_rendition_sig(spec, id, sig)` and refuse only
`RenditionSig::Rejected`. Image URLs the CMS renders now end in `?s=…`
even without `RCMS_RENDITION_SIGNING_KEY`; a test comparing whole URLs
should drop that parameter.

### Your publish, serve, search or dashboard hook no longer compiles

The four behavioural hooks are async and take a `HookCtx` first:

```rust
use rustango_cms::hooks::{HookCtx, HookFuture};
use rustango_cms::page::Page;

fn purge_cdn<'a>(ctx: HookCtx<'a>, page: &'a Page) -> HookFuture<'a, ()> {
    Box::pin(async move {
        // ctx.tenant, ctx.pool, ctx.user_id; await freely
    })
}
rustango_cms::register_after_publish_page!(purge_cdn);
```

`BeforeServePage` takes `(ctx, page, headers)` and returns
`HookFuture<Option<Response>>`; `AdminSearchArea::search` takes
`(ctx, query)`; a `DashboardWidget` gives a `render: fn(HookCtx) ->
HookFuture<String>` instead of a static `html`.

### Everyone signs in again

From rustango 0.63 on, member cookies, member API tokens,
open password-reset links and preview tokens end once: users sign in again.

### A custom `PageCacheInvalidator` no longer compiles

Its methods take `tenant: &rustango::tenancy::TenantSlug` instead of a slug;
read `tenant.slug`. Callers of `purge_urls`, `invalidate_urls_on_commit` and
`run_schedule_sweep_and_purge` pass `TenantSlug::of(&org)`. A `CachePurgeJob`
literal needs `org_id`; jobs queued before the upgrade purge nothing.

### `issue_password_reset_link` expects an `Org`

Load the tenant's `rustango::tenancy::Org` from the registry and pass `&org`
instead of its slug.

### `authenticate_member` and `security_log` expect a `TenantSlug`

`admin::members::authenticate_member`, `security_log::is_locked` and
`security_log::unlock` take the site's `rustango::tenancy::TenantSlug`
(`TenantSlug::of(&org)`) instead of its slug, so a locked-out name stays
bound to that site even if a new site later reuses the slug.

### New in 0.4.0, nothing to change

Starter themes, theme packages, Branding as the one place for the site's
name, logo and icon (uploaded logos move into the media library on their
own), the admin bar and the template inspector need no changes in an
existing project. A project made with an older `rcms new` keeps working; to
get the new blocks' styles and the admin bar's links, compare its
`templates/` with a newly generated project.

### Sites under a path (optional)

To serve a tenant under a path on the main domain (`create-tenant acme
--path-prefix /acme`), wrap the router in `main.rs`:

```rust
rustango::manage::Cli::new()
    .tenancy()
    .tenant_header(rustango_cms::path_prefix::tenant_header())
    .api(rustango_cms::path_prefix::serve(api))
```

Sites on their own host are not affected either way.

## 0.3.0

### Nothing to change in an existing site

0.3.0 changes only what `rcms new` generates. Bump `rustango-cms` to
`0.3` and keep `rustango` at `0.61.14` or later.

To give a project made earlier the one-command start, copy
`crates/rcms-scaffold/assets/quickstart.rs` to `src/quickstart.rs`, add
`dotenvy = "0.15"` to its dependencies, and add at the top of `main`:

```rust
let _ = dotenvy::dotenv();
if std::env::args().nth(1).as_deref() == Some(quickstart::COMMAND) {
    return quickstart::run();
}
```

with `mod quickstart;` next to `mod models;`. Put
`DATABASE_URL=sqlite:./var/registry.db?mode=rwc` in `.env`.

## 0.2.0

### rustango 0.61

Bump your `rustango` requirement to `0.61` (0.61.6 or later) and
`rustango-cms` to `0.2`, then run `migrate`. Follow rustango's own
UPGRADING notes from 0.60.12 on.

### `no such table: cms_security_event`

Run `migrate` (or `migrate-tenants`): migration 0016 adds the security log.

### An API client parses `extension.<stream field>` with `JSON.parse`

Page detail now returns StreamField extension columns as arrays. Drop the
parse step — read `extension.body` as the list of blocks. A client that
must work against both versions can parse only when the value is a string.

## 0.1.0

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
