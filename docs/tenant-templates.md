# Per-tenant templates

A multisite needs its tenants to look different without differing in
code. Each tenant can override any public template from its own
directory, falling back to the global set for everything it has not
touched.

## Layout

```text
{base_dir}/
  templates/                 # the global set — the existing public glob
    _base.html
    landing.html
    blocks/hero.html
  templates_tenants/         # one directory per tenant slug
    acme/
      _base.html             # wins over templates/_base.html
    globex/
      landing.html           # wins; its _base.html falls back to global
```

Names are relative to the tenant's own directory, so `acme/blocks/hero.html`
registers as `blocks/hero.html` — the same name the global set uses. A
template can therefore be copied out of `templates/` and dropped into a
tenant folder **unchanged**, `{% extends %}` and all.

`templates_tenants/` must sit **outside** the `templates/**/*.html` glob.
Inside it, every tenant's files would load into every tenant's instance.

## Enabling it

Build **one** instance and share it, so the renderer, the admin editor and
the MCP tools all read and write the same cache:

```rust
let overrides = Arc::new(
    TenantTemplates::new(Arc::clone(&tera), format!("{base_dir}/templates_tenants"))
        .with_global_dir(format!("{base_dir}/templates")),
);
tenant_templates::install(Arc::clone(&overrides));   // editor + MCP tools

let public = PublicRouter::new(tera)
    .tenant_templates_shared(Arc::clone(&overrides)) // renderer
    .build();
```

Without that, nothing changes: every tenant renders from the global set
exactly as before.

The two halves fail **separately and quietly**, which is worth knowing
before you debug one:

| Missing | Symptom |
| --- | --- |
| `tenant_templates_shared` (or `tenant_templates`) | overrides are editable but never rendered — the site ignores them |
| `install` | the editor page and every MCP template tool answer *"per-tenant templates are not configured on this deployment"* |
| both, i.e. only `new` | the feature is inert on all three surfaces |

Use `tenant_templates(overrides)` — the non-`Arc` form — only when the
host genuinely wants rendering without the editor. Passing a *separate*
instance to `install` is the one real mistake here: the editor would then
write files a different cache is serving from, and saves would appear to
do nothing until the reload interval on the other instance elapsed.

`with_global_dir` is what lets the editor show the fallback a tenant
would inherit and seed a new override from it; without it the editor
lists only overrides that already exist.

## Deploying a change

Templates are deploy artifacts, not code. Drop a folder and the next
request picks it up:

| | Rebuild | Restart |
| --- | --- | --- |
| A tenant's public template | no | no |
| A global public template | no | yes — the global Tera is built at boot |
| An admin template | **yes** — `include_str!`, baked into the binary | n/a |

Reload policy is per-deployment:

```rust
TenantTemplates::new(tera, root)
    .with_reload(Reload::Always)                            // dev
    .with_reload(Reload::Every(Duration::from_secs(30)))    // default is 5s
    .with_reload(Reload::Never)                             // rebuild-on-deploy only
```

`Every` fingerprints a tenant's directory at most that often — file
names, lengths and mtimes, not contents — so noticing a change costs a
directory walk, not a read.

## Editing from the admin

**Settings → Templates** (superuser only) lists every template the site
renders — the tenant's own overrides and the global names it inherits —
and edits them in CodeMirror with Tera-aware highlighting.

* **Customise** an inherited template: the editor opens seeded with the
  global body, and saving writes a copy for this tenant only.
* **Revert** removes the tenant's copy; the global template is untouched.
  Refused when a page type renders with that template and no global
  version would take over — otherwise every page of that type 500s.
* **New template** creates one by name.

The **Validate** button checks the body without saving, answering in
place so you keep your cursor. Saving validates too — the button is the
feedback loop, not the safety net.

A save is validated before anything is written, against the same
instance the renderer builds — so `{% extends "_base.html" %}` resolves
and a missing parent is caught, not just a syntax slip. A rejected save
re-renders with the parse error and the author's text intact.

Gated on the **`cms_template.edit`** permission, with the usual
superuser bypass — seeded onto **Developer** and **Administrator**, and
backfilled onto both on upgrade. Editor and Viewer deliberately do not
get it: a template is code-adjacent. Tera cannot reach Rust, spawn a
process or read files, but every function and filter the host registered
is callable from one.

Grant it to another role from **Management → Roles** like any other
codename.

**The editor writes to local disk.** With more than one replica behind a
load balancer, a save lands on one box and the others never see it, and
on an ephemeral filesystem it is lost at the next deploy. Use it where
there is a single app server, or where the directory is shared storage —
otherwise keep templates in git and treat the editor as read-only.

## Assigning a template to a page type

`render.rs` renders `page_type.default_template`, so *which* file a type
uses has always been data — but the row was written once at creation and
shown read-only. **Page types → the template cell** now opens a picker.

The choices come from the tenant's own Tera instance rather than a
directory listing, so they include templates the host registered in code
(which have no file to stat) alongside the ones on disk — and it is
exactly the set the renderer will look in. Admin chrome is filtered out.
A name that cannot be resolved is refused rather than saved, because it
would 500 every page of that type.

Builder-created page types work the same way. A type made in the UI
starts on `rcms_admin/schema_page.html` — the generic renderer for
UI-defined schemas, which lives in the admin namespace but *is* a public
page template — and can be pointed at a template of your own from here.
Whatever a type is currently set to always appears in the picker, even
if it would not otherwise be offered, so opening the form and saving
cannot silently change it.

Choosing **— none —** is meaningful, not "unset": with no template a page
type has no HTML representation and serves JSON only
(`page_view::resolve_kind`). The form says so in both directions.

## What it costs

Measured on a release build, 60-template global corpus
(`cargo test --features sqlite --lib cost_of -- --ignored --nocapture`):

| Tenant's overrides | Cold build | Cached hit | Fingerprint tick |
| --- | --- | --- | --- |
| none | — | **1.05 µs** | — |
| 1 | 309 µs | **2.4 µs** | 19 µs |
| 5 | 396 µs | **1.5 µs** | 29 µs |
| 20 | 977 µs | **1.3 µs** | 78 µs |

Against a ~4.5 ms page render the cached path is **~0.03 %** — below
measurement noise, and end-to-end timings for tenants with 0, 1 and 2
overrides came out identical. A tenant with no directory costs an `Arc`
clone.

Two things do cost, and both are bounded:

* **The fingerprint tick** — a stat walk of the tenant's directory, at
  most once per `Reload` interval per tenant, not per request. 19–78 µs
  at these sizes. A tenant with hundreds of overrides should raise the
  interval or use `Reload::Never`.
* **A cold build** — 0.3–1 ms, on a tenant's first request and after
  eviction. This is the number that matters at scale: past the 64-entry
  cap the least-recently-used tenant is evicted, so with **more than ~64
  simultaneously-active tenants** the long tail pays a rebuild per
  request. Raise `with_capacity` to match your hot-tenant count, and
  remember each cached instance holds a clone of the corpus.



A tenant instance is a clone of the global Tera plus a parse of that
tenant's own files. The clone dominates (~2.4 ms for a full corpus), so
instances are cached, and the cache is bounded — 64 by default, matching
the framework's `max_cached_scoped_pools`. Past the cap the
least-recently-used tenant is evicted and rebuilt on its next request;
that is a cost, never a behaviour change.

A tenant with **no** directory is handed the global instance itself — no
clone, no cache slot. The common case is free.

Give `TenantTemplates::new` a Tera holding only the *public* templates
where you can. An instance that also carries the admin corpus makes every
clone much larger, and the public render path never extends an admin
template.

## From an agent (MCP)

The `cms-templates` skill carries five tools, entitled by the same
`cms_template.edit` codename:

| Tool | Does |
| --- | --- |
| `list_templates` | overrides and inherited names, with `source` |
| `read_template` | the tenant's copy, else the inherited body |
| `validate_template` | parse a body **without writing it** |
| `write_template` | create or replace an override |
| `set_page_type_template` | point a page type at a template |

An agent can **assign** templates to page types but cannot **create** a
page type — there is no `create_page_type` tool. Types are made in the
UI (Page types → New), including builder-defined ones; MCP then wires
them to a template.

**There is deliberately no delete tool.** Creating and modifying are
recoverable — the previous body is in git, and a broken one is refused
before it is written. Removing an override is the one operation an agent
cannot inspect first and cannot undo: the tenant silently falls back to
the global template and the customisation is gone. Reverting stays in
the admin, behind a confirm dialog, where a person decides. An agent that
needs a template to stop doing something rewrites the body.

The tenant comes from the token (`ctx.agent.tenant`), which is
tenant-pinned, so an agent cannot address another tenant's folder even by
name.

## How tenant folders are gated

Four layers, in the order a request meets them:

1. **The tenant is not a parameter.** The slug comes from
   `tenant.org.slug` — resolved from the request's `Host` against the
   registry — never from a form field or query string. There is no input
   that selects *which* tenant's folder to touch.
2. **The permission**: `cms_template.edit`, superuser bypass.
3. **The name is validated lexically.** `safe_name` rejects `..` in any
   segment, absolute paths, backslashes, dotfiles and anything not
   ending `.html`. A leading `/` is stripped as a convenience, so
   `/etc/passwd.html` becomes an oddly-named template *inside* the
   tenant's own directory rather than an escape.
4. **The resolved path is checked.** Every read, write, delete and
   directory walk requires the path — symlinks resolved — to sit inside
   that tenant's directory.

Layer 4 exists because layers 1–3 are all about the *string*. A symlink
already on disk has an entirely ordinary name: a link at
`templates_tenants/acme/peek.html` pointing at another tenant's file let
`acme` read **and overwrite** it. Templates arrive by rsync or git, where
a symlink is easy to miss in review, so the check has to be on where the
path actually lands. The guard is on the walk as well as the writes —
otherwise the link would still be *rendered*, which closes the door and
leaves a window.

A symlink that stays inside the tenant's own tree is fine: the rule is
containment, not a ban on links.

What this does **not** gate: anything with filesystem access to the root
directory. All tenants live under one root, so an operator, a deploy
pipeline or another process on the box can reach every tenant's
templates. The boundary here is the application, not the filesystem.

## Failure behaviour

- A tenant template that does not parse is skipped with a warning, and
  that name falls back to the global one. One bad file costs one page,
  not the tenant's whole site.
- If a tenant's overrides break inheritance as a set, the tenant is
  served the global templates and the error is logged.
- A slug that could climb out of the root (`..`, `a/b`) is refused.

## The drift trap

An override is a fork. A tenant that copies `_base.html` stops receiving
every later improvement to the global one — the same way the docs admin's
`_base.html` override has silently drifted from the bundled version.

Prefer overriding leaf templates and blocks over base chrome, and when a
tenant does fork a base, record which version it was forked from so the
drift is visible rather than discovered.
