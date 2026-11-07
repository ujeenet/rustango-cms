# Changelog

All notable changes to rustango-cms. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Breaking changes
have an entry in [UPGRADING.md](UPGRADING.md).

Issue numbers such as `#258` in entries written before the public release
refer to the project's earlier, private tracker, not to issues in this
repository.

## 0.4.0 — 2026-11-07

Starter themes (a finished-looking site from one command, with no Rust
models), admin editors for your own models, inline panels that edit
hundreds of rows one card at a time, and rustango 0.65.

### Added

- `Page::page_status()`, `Page::set_page_status()` and `Page::is_public()`:
  typed access to a page's status without changing the stored column, so
  host code stops comparing string literals. `is_public()` counts archived
  pages, which `status == "published"` misses (#763).
- Per-row inline panels (`InlinePanelSpec::per_row`), for panels with many
  rows: the editor tab lists only titles, a card's fields load when it is
  opened, each card saves with its own Save button, and add, remove and
  reorder apply at once (the page's Save no longer posts the panel). New
  hooks on `PageTypeHandler` / `PageTypeOverrides` (forwarded by the
  derive): `inline_rows`, `inline_row`, `save_inline_row`,
  `delete_inline_row`, `reorder_inline_rows`; endpoints under
  `/cms-admin/pages/{id}/inline/{panel}/…`, gated as page edits.

- Inline panels are usable editors: rows render real widgets (selects,
  switches, media and document choosers, checkbox lists), can sit in their
  own editor tab (`InlinePanelSpec::in_tab`), can be reordered, and can be
  locked (`allow(add, delete, order)`). Each row posts its `id`, and a
  loaded row's `__title` heads its card. `#[derive(PageType)]` forwards
  `inline_panels`, `load_inline_panel` and `save_inline_panel` to
  `PageTypeOverrides`.

- `rcms new` ends by linking the Rustango and Rustango-CMS repositories and asks for a star on each.

- `#[derive(CmsAdmin)]`: an admin editor for any model of your own, at
  `/cms-admin/x/<slug>` — a list with search and pages, add and edit forms
  built from the same `#[field(widget = …)]` attributes as page types, and
  delete. It adds a view/add/edit/delete row to the role matrix and checks
  it on every request. `CmsAdminOverrides` holds the rules: read-only
  fields, `can_create` / `can_delete`, `validate`, options for a select
  (foreign keys), extra widgets for many-to-many lists, `after_save`,
  `before_delete` and computed list columns. `save_record` and
  `delete_record` run the same steps outside the admin.
- Custom admin pages can take forms: `AdminPageHandler::post` answers a
  `POST` to `/cms-admin/x/<slug>` or `/cms-admin/x/<slug>/<action>`, after
  the login, CSRF and permission checks. `AdminPageCtx` gains the request
  `uri` and `action`, and `render` (the admin chrome, CSRF token and flash
  messages), `redirect` (with a flash message), `query`, `query_param`,
  `has_perm` and `page_url`. `AdminForm` reads the posted form, repeated
  names included. `permission_actions` picks the matrix columns.
- `PageTypeHandler::route_outcome`: after `route_context`, a matched route
  picks its template (`RouteOutcome::Template`) or answers the site's 404
  page with a real 404 status (`RouteOutcome::NotFound`).
- A `page` object returned from `public_context` / `route_context` adds its
  keys to the template's `page` (`page.<key>`) without replacing the page's
  own.
- A site block registered with the name of a built-in block replaces it.
- `published_children_page(pool, page, order, limit, offset)` returns one
  window of a page's live children with their total, ordered by
  `ChildOrder::Position`, `NewestFirst` or `OldestFirst`. Return it from
  `children_query` so an index with hundreds of children renders only
  the ones it shows (#690).
- Docs: "Your own data in the admin", a new developer chapter of the shop
  tutorial, and the shop example's stockists (editor, import screen,
  `/shop/stockists/<city>`).

- Sites served under a path on the main domain (`create-tenant acme
  --path-prefix /acme` → `example.com/acme/`) work as a whole CMS site:
  pages, the admin, sign-in, media, branding, forms, analytics, sitemap and
  feeds. `path_prefix::serve` takes the prefix off each request and puts it
  back on links, redirects and cookies; `path_prefix::tenant_header` lets
  the framework find the site again. New projects are wired for it.
- The scaffolder is published: `cargo install rcms-scaffold`, then
  `rcms new <name>` from any folder, with no clone of this repository.
  `rcms --version` shows the release it creates projects for. Generated
  projects no longer carry a copy of the CMS migrations; they write them
  from their `rustango-cms` dependency when they start.
- `rcms new <name> --theme <blog|news|portfolio|blog-portfolio>`. The
  first `cargo run -- quickstart` creates the theme's page types, sample
  pages, header and footer menus, site name and colours. All of it is
  editable in the admin, and the theme never runs again for that site.
- Theme page types are defined as data (page-builder schemas), each with
  a flexible **Content** field: rich text, image, quote, embed, plus the
  theme's own **Gallery** and **Call to action** blocks.
- **Settings → Branding** is the one place for the site's name, logo and
  icon, used by the admin, the sign-in screens and the public site. The
  logo and icon are pictures from the media library, picked with the
  image chooser; files uploaded there before move into the media library
  the first time the settings page opens. Starter themes fill Branding in
  (the Rustango-CMS logo to start with) and add a footer badge under
  **Site settings → Site**, which keeps only the theme's own text.
- Starter themes come with sample pictures: covers for every post,
  article and project, and pictures in the About page's blocks, a home
  slider (Portfolio themes) or a gallery (Blog, News). They are original
  illustrations drawn by `scripts/theme-media/generate.html`, so no
  licence applies. The first start stores them in the media library.
  `theme.json` takes a `media` list referenced as `"@media:<key>"`, and
  `starter::apply_with_media` stores the supplied files.
- Template inspector for admins: **Preferences → Show templates on the
  site**. While it is on, the public site shows the signed-in admin a
  panel listing the page template, the layout it extends and
  each block template with its source (this site's copy, the project's
  `templates/`, or built in), with links to the Templates editor; hovering
  a block names its template. Visitors never see it, and such pages are
  sent `no-store`. It opens from the admin bar's **Templates** button.
- **View site** in the admin sidebar goes to the public site's home page.
- Admin bar on the public site: signed in to the CMS admin, a small bar in
  the corner of every page links to **Edit this page** and **Admin**, in
  the admin's language; it collapses to its icon. Members and visitors
  never see it, and such pages are not cached.
- **Cards**, **Slider** and **Gallery** blocks, built into Rustango-CMS:
  each holds a list (Add card / slide / picture). Cards: 2–4 per row with
  image, title, text and link. Slider: one slide at a time or a row of
  several, scrolling sideways without JavaScript; themes add previous/next
  buttons. Gallery: any number of pictures, each opening full size; it
  replaces the themes' four-picture gallery. A `safe_href` Tera filter
  keeps their links (and theme buttons) to site paths and web, mail and
  phone addresses.
- Layout blocks for every theme's **Content** field: **Hero image**
  (full-width picture with text over it), **Jumbo box** (one large
  message, accent, soft or dark) and **Media and text** (image left or
  right of a heading and text). Each has an optional button. Theme blocks
  are now defined once, in `themes/_shared/blocks.json`, for all page
  types. Every theme's About page shows them.
- Every starter theme ships themed error pages (404, 403, 500, 503) in its
  own layout and wording: editable **Error page** pages under the home
  page, kept out of menus, the sitemap and search engines. `theme.json`
  takes them as `error_pages`.
- `rustango_cms::starter`: `apply` creates a site from a `theme.json`
  (once, and only on a site with no pages), and `parse` checks one
  without a database. Any site can be turned into a theme this way.
- Theme packages: `cargo run -- theme pack site.zip` turns a site into one
  `.zip` (its `theme.json` and templates, with or without its pages).
  `theme install site.zip` puts it on another site: everything on an empty
  site, only the look (and any error pages it lacks) on a site with
  content. `theme check` validates a
  package. Templates are merged, and the old folder is backed up first.
- **Settings → Site theme** in the admin (superusers): download this site
  as a theme package, upload one, see a preview of what changes, install
  it, and restore the previous look. Templates go to the site's own
  template folder, live, with no restart; other sites keep their look.
  Projects made with `rcms new` now set up per-site templates, which the
  **Templates** editor uses too.
- **Help → Documentation** in the admin: every guide, read inside the
  admin, for every signed-in user. Sections and search, guide-to-guide
  links that stay in the admin, screenshots from cms.rustango.com, and
  "Open on the docs site". The guides are the ones `docs/index.toml`
  lists, bundled at build time, so they match the installed version.

### Changed

- **Breaking:** `PageTypeHandler::inline_panels` is async and takes the
  pool and the page id, so a panel can read its options and locks from
  the database.

- Requires rustango 0.65. `rcms new` creates projects on rustango 0.65.

- **Breaking:** the generic model admin (`register_model_admin!`) shows its
  rows in pages. `ModelAdmin::list_rows` takes a limit and an offset and
  returns the window with the total; the macro builds it for you (#629).

### Fixed

- The page history screen returned a server error for any site with
  revisions: the lookup that flags each page's latest revision used a
  projection the ORM only allows with an aggregate. It now reads just
  `(id, page_id, sequence)` with `values_list` (`revision::page_sequences`).
- Two people saving the same page no longer lose each other's changes
  to its own fields (title, slug, status, schedule, SEO and social
  fields, search-engine and menu switches, template, frontend route).
  The edit form remembers what it loaded: a field you left alone keeps
  what was saved meanwhile, a field only you changed is written, and a
  field you both changed is not overwritten. The form comes back with
  your values, a notice listing each such field with the saved and your
  value, and Save keeps yours. Autosave follows the same rule. Saves
  that don't come from the form (MCP, the API) work as before. Tags,
  categories and the page type's own content are still saved as posted
  (#903).
- `/api/v2/pages/?id_in=` takes at most 200 ids (`400` beyond), so a long
  list can't exceed a backend's bind-parameter limit; an admin list's
  `?page=` can no longer overflow into a negative offset.
- `GET /api/v2/pages/` read every public page on each request. Exact
  filters (type, `child_of`, `descendant_of`, `ancestor_of`, `id_in`,
  `tag`, `locale`, `translation_of`, `updated_since`) now run in the
  database, and without a search, order by a text field or gated pages
  the count and the page do too. Results and their order are unchanged
  (#655).
- The site search (`/api/v2/search/`) leaves locale copies and closed
  media collections out in the database instead of after loading them
  (#655).
- Admin screens no longer load whole tables to count: collections count
  their media, taxonomies their categories, menus their items, roles
  their members and grants, and page types their pages in the database.
  The components list reads the page-type schemas once, not once per
  component. Category and media-collection tables are paged (#629).
- Deleting a UI-built page type loaded every page to count its users and
  treated a failed count as "no pages", so the delete went ahead; it
  counts in SQL and stops on an error (#769, partly).
- A page that went live on schedule got no "Published" page-log entry and
  emailed no subscribers. The sweep now records both; emails go through
  the mailer given to `admin::router_with_mailer`, which is now also
  available as `task_queue::default_mailer` (#945).
- Saving a live page again logged "Published", re-emailed every
  subscriber and re-fired `after_publish_page`; that now happens only when
  a save takes the page live. A page the scheduler published while it was
  open in the editor stays published when the editor saves. Autosave on a
  live or scheduled page said "Auto-saved" although it saves only drafts;
  it now says to use Save (#903, partly).
- Saving a page without changing anything rewrote the whole page, moved
  its "last updated" time, added a revision and a page-log entry, purged
  the cache and, on a page type with re-approval on edit, held the save
  and started a review. Such a save now writes nothing and says "No
  changes to save" (MCP returns `unchanged: true`). It also no longer
  re-runs the reference index, search sync or hooks, so re-saving a page
  unchanged no longer repairs those; change something to re-run them. A
  save that does change something writes only the changed fields and
  never the page's place in the tree (path, parent, order), so a move made
  while the page was open is kept. A schedule time posted with seconds (MCP,
  scripts) is compared to the second; the admin's minute inputs keep the
  stored seconds. Tags and the reference index are updated by difference
  instead of being deleted and re-added (#903).
- The page form, MCP create and MCP update each parsed a page status
  with their own list; they share `PageStatus::parse_editable` now (the
  form still saves anything unknown as a draft, MCP still refuses it).
  The dashboard counted pages, snippets and media by loading every row,
  and loaded the whole page tree to name a few pages; it counts in SQL and
  fetches only the pages it shows (#763).
- Image renditions pick their format: with no `format-…` (the built-in
  image block, `rcms_image_url(filter="width-1600")`) or `format-auto`, an
  opaque image becomes a JPEG and one with transparent pixels keeps
  transparency (PNG, or WebP for a WebP source). Before, a PNG photo stayed
  a lossless PNG — often ten times larger — and `format-auto` meant
  lossless WebP. An explicit `format-png` / `format-webp` still wins.
  Renditions already stored keep their format until they are regenerated.
  Flat graphics (screenshots, line art) that should stay lossless can ask
  for `format-png`.
- `rcms_image_url` accepts a full pipeline (`width-800|format-jpeg|quality-80`),
  like `rcms_image_srcset` and `rcms_picture`; it used to reject anything
  after the resize op and break the render. `rcms_picture` no longer emits
  a `<source>` for `auto`, which is decided per image (#900).
- Bulk actions read (and, for a media move, wrote) one row per selected
  id; the media move is one UPDATE per chunk and page, user and
  unused-media bulk actions load the selection in one query (#631).
- The sign-in, password-reset and member screens now use the tenant's
  admin theme and branding; window titles use the site's brand name
  instead of "CMS Admin"; a wide logo keeps its proportions on the
  sign-in card (#918).
- The SSO providers page's note used a dimmer, larger text style than
  every other table page (#633). `template-editor.js` uses `const`/`let`
  (#664).
- Without `RCMS_RENDITION_SIGNING_KEY`, anyone could make the server
  decode, resize and store any number of sizes of every image (#643).
  Image URLs the CMS renders (`rcms_image_url`, `rcms_image_srcset`,
  `rcms_picture`, `og:image`) are now always signed — with a key derived
  from the CMS secret when none is set — and always served. Unsigned URLs,
  such as the ones API clients build from `download_url_template`, are
  still served, but an anonymous request can create at most 128 sizes of
  one image that way (`429` after; `RCMS_RENDITION_MAX_UNSIGNED` changes
  it). Setting `RCMS_RENDITION_SIGNING_KEY` keeps the strict behaviour:
  every request must be signed.
- On SQLite, page create and move (`tree_ops::atomic_tree`) opened a
  deferred transaction, so two concurrent moves into one parent could
  read the same sort order or fail with `SQLITE_BUSY`; they now take the
  write lock up front (`atomic_immediate`) (#915).
- The workflows report read every workflow state ever recorded; it pages
  now (the CSV still exports all). The search report counts each query's
  promotions in one query instead of one per row (#628).
- An index page also loaded every grandchild row, whatever the locale,
  just to collect ids for translations. Grandchildren are now read as
  ids, bounded, and only for a non-default locale; child listings filter
  for visibility in SQL instead of loading drafts and expired pages to
  drop them (#690, partly).
- `/api/v2/images/`, `/documents/` and `/snippets/` loaded the whole table
  per request. Exact filters (collection, type, `updated_since`, gated
  collections) run in SQL, and without a text search a date/size order is
  counted and paged there too (#655, pages list still in memory).
- Admin lists that grew with use loaded whole tables: the users list,
  form submissions, page history and the revisions report (which read
  every revision's full snapshot to count them) now count and page in
  SQL with the shared page controls (#627).
- The media and documents libraries filter, count and page in SQL
  instead of loading every file; search stays case-insensitive on every
  backend (#632).
- The account page's MCP keys read their grants in one query instead of
  one per key (#630).
- A public page rendered every row of every library type with a
  template, whether the page showed it or not. `snippet_html(...)` now
  renders only the rows a page asks for, fetched with one query per type
  (#731).
- `GET /api/v2/changes/` loaded every public page to count them; with no
  view restriction in force for the caller it now counts in SQL (#647).
- A page edited through MCP purged the page cache with org id 0 when no
  registry was recorded, so the old page stayed cached until its TTL;
  the purge now uses the agent's own org (#930).
- A new site with no published page answered `/` with a bare 404, which
  read as a broken install; it now says the site has no pages yet and
  links to the admin sign-in. `quickstart` and the generated README say to
  use the `demo.localhost` address: plain `localhost` is the operator
  console, where the site's `admin` user cannot sign in (#43).
- Inline-panel fields always rendered as text boxes, whatever their kind.
- Switch widgets (`with_variant("switch")`) showed a checkbox and the
  switch together: the template still used the class name from before
  the `rcms-` prefix.
- `#[derive(PageType)]` accepted a `#[field]` named like one of the page
  editor's own fields; the two were posted together and one overwrote
  the other (a `status` field unpublished the page on every save). Such a
  field is now a compile error that says how to rename it.

- Page-type hooks (`public_context`, `route_context`) ran on the registered
  default value of the type, so `self.<field>` was always empty. They now
  get the page's stored row.
- Two blocks with the same name were picked in link order, so a site could
  not tell which one it got. A site block now wins over a built-in one; two
  site blocks with one name keep the first and log an error.
- `/cms-admin/x/<slug>` did not check the page's `permission_resource`
  codename; the route gate now does (`.view` to open it).
- The ceramics shop example's migrations had branched from the CMS's, so
  `makemigrations` refused to run; they are merged.
- Deleting a media item that had renditions failed with a server error
  (the renditions' foreign key); the renditions are removed first, in the
  library, the unused-media bulk delete and crop/replace.
- The page editor's tab strip wrapped to a second row when tight, so a
  hovered tab whose label expanded jumped rows, lost the hover and
  flickered; the strip now stays one row and scrolls sideways.
- In the page list, a long title overlapped the slug at mid widths; the
  row now drops its buttons to a second line instead.

- Help → Documentation linked to a domain that does not exist; it now
  opens the Help center.
- Projects made with `rcms new` never mounted `/__media__/…`, so every
  uploaded image was a broken link on the public site, in the editor's
  preview and in the image chooser. The generated `main.rs` now mounts
  `rendition_route::router()`.
- Page and delivery statuses showed untranslated ("PUBLISHED") on the page
  list, search, reports, history, media usage, page-type versions and
  notifications.
- **Site settings**: the badge on a row moved about with the length of
  its name, the Ukrainian and other translations of it meant "entered",
  and the starter theme's internal record showed as a setting. The badge
  now reads "Form" beside the key, and the record is hidden.
- The admin sidebar showed the tenant's display name and its slug, and
  ignored the site name set under **Settings → Branding**. It now shows
  one name, the Branding site name (else the tenant's brand or display
  name), beside the Branding logo; the sign-in screens use the same.
- **Unused media** listed pictures used in page-builder fields and blocks
  (covers, hero, media-and-text, cards, slider and gallery items) and in
  settings as unused, and a picture's **Used on** panel missed them: both
  only read code page types' `*_media_id` columns. They now learn every
  picture field from the block, page-builder and settings schemas and scan
  pages, library elements and site settings (`rustango_cms::media_usage`).
- **Help → Documentation**: sections stretched to the tallest one in their
  row and the search box sat in a hand-styled card; sections now flow in
  columns with plain link lists, and the search uses the standard field.
- **Preferences → Profile**: the avatar field accepted SVG, which can
  carry script and was served from the admin's own origin; avatars are
  now PNG, JPEG, WebP or GIF, anything else is refused with a message, and
  the chosen picture shows before saving.
- High contrast: the focus ring on Help's guide links was cut off at the
  sides; it is drawn inside the row now.
- **Templates** was unstyled (a plain input, a table narrower than its
  card) and showed its title twice; it now uses the admin's table panel.
- A list field's add button showed the item's internal name ("Add
  gallery_image"); it now shows its display name ("Add picture").
- The **Call to action** block's button link refused a site path such as
  `/contact` (the browser's URL check), so a page using one could not be
  saved. Theme button links now take a site path or a full link, and a
  link that is neither (such as `javascript:`) is not rendered.

### Docs

- Tests run under cargo-nextest (`.config/nextest.toml`, as in rustango);
  CONTRIBUTING and CI use it, with `cargo test --doc` for doctests.
- [Themes](docs/themes.md): starter themes, theme packages, what
  travels and what doesn't, data vs code page types, a templates
  reference and troubleshooting.
- The daily install check generates every theme, runs `quickstart` and
  loads every page.

### Breaking

- The behavioural hooks are async and get a `hooks::HookCtx` (the
  tenant's pool, its slug, the acting user's id): `AfterPublishPage`,
  `BeforeServePage`, `AdminSearchArea::search` and the new
  `DashboardWidget::render` (replacing the static `html`). They used to be
  plain `fn`s with no way to know which tenant they ran for, reach its
  database or await; `HookCtx` is `#[non_exhaustive]`, so it can grow
  without breaking a hook again (#686). `page::went_live_effects` takes the pool
  and the acting user's id, `page::apply_sweep_effects` the pool. A hook
  that runs longer than `hooks::HOOK_TIMEOUT` (5 s) is abandoned and
  logged, so a hung webhook can't hang a page view or a save.
- `rendition::verify_rendition_sig` is replaced by `check_rendition_sig`,
  which returns `RenditionSig::{Valid, Unsigned, Rejected}`; image URLs
  rendered by the CMS now carry `?s=` even without
  `RCMS_RENDITION_SIGNING_KEY`.
- Requires rustango 0.65 (built and tested against 0.65.0); see UPGRADING.md.
- `PageCacheInvalidator` methods, `purge_urls`, `invalidate_urls_on_commit`
  and `run_schedule_sweep_and_purge` take the request's `TenantSlug` (slug and
  org id), since the page cache keys on both. `CachePurgeJob` gains `org_id`.
- `BoxedCacheInvalidator` purges through `CachePageLayer::invalidate`, which
  drops every variant of a path, for mapped and `with_default_hosts` hosts alike.
- `admin::issue_password_reset_link` takes the tenant's `Org`; reset links are
  bound to the org id, so a reused slug does not accept an old link.
- Preview tokens are signed with the org id too; `preview_token::mint` and
  `verify` take a `TenantSlug`.
- `page_tag::replace_tags`, `category::assign::replace_for_taxonomy` and
  `category::assign::save_from_form` return whether they changed anything
  (`Result<bool, _>`).

## 0.3.0 — 2026-11-02

A first run in two commands, the same on Linux, macOS and Windows (#37).

### Changed

- `rcms new` projects build SQLite by default (PostgreSQL and MySQL stay
  available with `--no-default-features --features postgres` or
  `mysql`), and come with a `.env` holding the SQLite `DATABASE_URL`,
  which `main` loads at startup. No `export`, no `$env:`, no feature
  flags.
- `rcms new` prints two commands instead of seven.

### Added

- `cargo run -- quickstart` in generated projects: creates the registry,
  a `demo` site at demo.localhost and an `admin` user (asking for the
  password) on the first run, then starts the server. Re-running skips
  what exists.

### Docs

- Getting started follows the new path. The README and the generated
  README show PowerShell next to bash where the commands differ.
- The Windows first-run check in CI runs exactly the documented steps.

## 0.2.1 — 2026-11-02

### Fixed

- Builds again on a fresh install. rustango 0.61.14 added an `apex`
  argument to `tenancy::add_host`, so 0.2.0 stopped compiling for anyone
  whose lockfile picked up 0.61.14. Requires rustango 0.61.14 now.
- Adding a site hostname refuses the apex (where the operator console
  lives) on the approval path too, not only through `add_host`.
- Scaffolded projects run on Windows. `rcms new` now writes a
  `.cargo/config.toml` that gives the main thread an 8 MiB stack on
  Windows (the default is 1 MiB, and a debug build stopped with "thread
  'main' has overflowed its stack" on its first command). For a project
  made earlier, copy
  [`crates/rcms-scaffold/assets/cargo_config.toml`](crates/rcms-scaffold/assets/cargo_config.toml)
  to `.cargo/config.toml` in the project and rebuild.

### Security

- Requires pulldown-cmark 0.13.6 (RUSTSEC-2026-0414, RUSTSEC-2026-0433).
  Both affect only the wikilinks extension, which the CMS does not enable.

## 0.2.0 — 2026-10-27

### Breaking

- Requires rustango 0.61 (built and tested against 0.61.6). Types from
  rustango appear in this crate's API, so a host must move to the same
  rustango minor; see UPGRADING.md.

### Added

- A security log at `/cms-admin/security-log` (superusers only, under
  Management): sign-ins, failed attempts, lockouts, sign-outs, password
  resets and changes to user flags, role memberships and role permissions,
  with the account, the client address and who made a change. Stored per
  tenant in `cms_security_event` (migration 0016) and pruned after 90 days
  (`RCMS_SECURITY_LOG_RETENTION_DAYS`). The framework's own admin `/login`
  is recorded from its auth signals while the request log is on.
- The member login (`/members/login`) and `POST /api/v2/auth/login` share
  the admin login's per-account lock and per-IP limits: 5 failures in an
  hour lock the account for 15 minutes (`RCMS_LOGIN_LOCKOUT_ATTEMPTS`,
  `RCMS_LOGIN_LOCKOUT_SECONDS`, `RCMS_LOGIN_LOCKOUT_WINDOW_SECONDS`). The API
  answers a locked login with `429 rate_limited` and `Retry-After`. A
  password reset lifts the lock.

### Changed

- Page detail in the headless API (and a page's JSON form) returns a
  StreamField extension column as the block array, not as the JSON text it
  is stored as — the shape `builder` zones already had. Per-block
  translations now apply to those streams under `?locale=` too.

- `preview_token::mint` returns the token itself (no `Option`): the CMS
  signing secret is always available now. Without `RCMS_SECRET_KEY` the
  CMS generates a key once into `./var/.rustango_cms_signing.key`, so
  headless preview works with no setup. Password-reset links and the
  admin's flash messages use keys derived from the same secret, so they
  also work across replicas that share it.

### Fixed

- A password reset for an account with no email on file did nothing,
  while the page said to look for the link on stdout. With no mailer
  wired, the link is now printed to stdout for the operator; with a
  mailer, a warning says why nothing was sent. The stdout hint shows
  only when there is no mailer. New: `admin::issue_password_reset_link`
  gives an operator a link for any active user.

- Publishing purged nothing from the page cache: the invalidator built
  cache keys without the tenant the framework's `CachePageLayer` now adds,
  so edited pages stayed stale until the TTL ran out.

- With `RCMS_API_CORS_ORIGINS` set, the preflight now allows `POST` and the
  `Authorization` header, so a frontend on another origin can log a member
  in and send the bearer token. Before, the browser refused both.

- A colour field with no value yet rendered `value=""`, which the browser
  rejects as not `#rrggbb`. It now starts at `#000000`.

- The media Collections panel and a few admin tooltips showed leftover
  development notes (issue numbers, "follow-up PR") to editors. They are
  gone.

- The page-type template picker offered partials that cannot render a
  page on their own: files starting with `_` and block templates under
  `blocks/`. They are no longer listed; a type already set to one keeps it.

- A rich-text field showed two toolbars (a markdown one above the editor's
  own) and a Preview toggle that could reveal the raw HTML. The field now
  shows only the rich-text editor.

## 0.1.0 — 2026-10-06

First release on crates.io: `rustango-cms` and `rustango-cms-macros`.

### Changed

- `rcms new` projects depend on the crates.io release of rustango-cms;
  `--local` keeps the old path dependency on a checkout.

### Breaking

- Requires rustango 0.60. Migration `0015_framework_snapshot_sync` records
  the framework's new `sessions_revoked_at`, `allow_email_link` and SSO links.
- `forms::schema::is_visible` takes every answer per field
  (`HashMap<String, Vec<String>>`), so a checkbox group's condition sees all
  ticked boxes. The result flags name the form: `?form_submitted=<form id>`
  and `?form_error=<form id>:<field key>` (was `=1`). `FormSettings` has
  `next_label`, `back_label`, `progress_label` and `error_message`.
- Every admin CSS class is prefixed `rcms-` (#314). Host template overrides
  that target the old names lose their styling.
- A tenant's media disk is registered as `tenant:<slug>` (#740). Use
  `media_storage::tenant_disk_name(slug)`.
- A hostname added on the Sites screen outside `CMS_SITE_HOST_SUFFIXES` waits
  for operator approval (#712).
- `block::admin::render_stream_editor`, `render_stream_editor_with` and
  `page_builder::values::render_body` no longer take a media-options list
  (#717).
- `/cms-admin/set-language` is POST-only (#738).
- HTML, XHTML, JavaScript and XML uploads are refused (#724).
- `ScheduleSweepResult` has `went_live` and `taken_down` fields (#692).
- The request forms that carry a password or secret no longer implement
  `Debug` (`UserForm`, `SsoProviderForm`, `AccountPasswordForm`,
  `PrivacyForm`, `CollectionPrivacyForm`, `TargetForm`, the member and API
  `LoginForm`s, `api::pages::DetailQuery`), and `NotificationTarget`'s
  `Debug` redacts its secret (#734).
- `rendition_route::gc_older_than` takes the tenant's pool and slug instead
  of the request-only `TenantLite`, so a host can call it outside a request
  (#770).
- `block::enrich::EnrichCtx` is `#[non_exhaustive]`; build it with
  `EnrichCtx::new` (#665).
- `PageType` has a `workflow` field (migration `0012_page_type_workflow`);
  a struct literal needs `workflow: String::new()` (#843).
- `category::assign::prefetch_for_pages` takes the locale id to translate
  names into, and `page_builder::values::public_render` the locale code
  for choice labels; pass `None` for the default language (#862, #863).
- `PageEditRefusal` has a `ReviewRequired` variant and `PageEditOutcome` a
  `held_for_review` field. `workflow::active_state_for_page` returns the
  page's latest round when it is in review or needs changes (it skipped a
  newer approval); `workflow::history_for` is `history_for_page`.

### Added

- Multi-step forms: Enter moves to the next step, each step is a browser
  history entry (Back returns to the previous step, a reload stays put),
  and the answers are kept for the visit. A refused submission returns to
  the refused question with everything filled in and the question marked.
- Form settings for the Next / Back labels, the progress line and the error
  message; they and the default Submit / thanks texts are on the form's
  Translate screen.
- Form rules offer the field's own options as values and read in plain
  words ("is", "is not", "is answered").
- Page types made in the admin have a Settings screen: name, allowed parent
  and child types, and a workflow. Their pages can go through that workflow
  (#843).
- `{{ cms_snippet(slug="…") }}`, which the snippet editor advertises, is
  registered and renders the snippet (`body=true` for the body alone) (#845).
- Pages can be filed under categories: a Categories field on the page
  editor's Promote tab, and `page.categories` / `categories` on each child
  in templates, with the category's name, slug and vocabulary (#842).
- `{% set nav = menu(slug="main") %}` renders an editor-curated menu from a
  template, filtered for the viewer and localized (#639).
- Category names can be translated: a language button on the category
  form, and `page.categories` in the visitor's language (migration
  `0013_category_translation`, #862).
- The options of a page type's select, radio and checkboxes fields can be
  translated (Translate on the field builder); templates show them with
  `builder_labels.<key>` (#863).
- A listing's children carry their builder fields: `child.builder` and
  `child.builder_labels`, translated, loaded once per page type (#863).
- A child with no share image chosen gets its first photo in
  `child.og_image_media_id`, so listing cards show a picture (the About
  us card on the example home page was empty).
- Site settings can be translated: a language button on a typed setting's
  form, stored in the setting's `_i18n` bag, and `site_setting()` returns
  the visitor's language.
- The snippet, form, menu and category translation screens share the page
  editor's side-by-side layout.
- Changes to a live page wait for review when its type's workflow asks
  for re-approval on edit: an editor's save is kept (migration
  `0014_page_pending_change`), visitors see the live page, the editor shows
  the proposed version with a notice, the final approval applies it, and
  cancelling the review drops it. MCP edits report `held_for_review`.
- The dashboard lists the pages waiting for your review. Roles can be given
  on the new-user form. Reject asks what should change.

### Changed

- Release preparation: shared crate metadata (`[workspace.package]`),
  licence files in every crate, a package `exclude` list (the docs
  screenshots and example photos stay out of the crate), a `cargo deny`
  licence policy in CI, issue and pull-request templates, dependabot, and
  a contribution-licensing clause.
- `rcms new` no longer needs a rustango checkout next to rustango-cms: the
  framework comes from crates.io, and generated projects carry no
  `[patch.crates-io]`.
- Every setting reads under one prefix, `RCMS_` (`RCMS_MEDIA_BACKEND`,
  `RCMS_S3_BUCKET`, `RCMS_DEFAULT_TIMEZONE`, …), listed in `config`. The old
  `CMS_` and `RUSTANGO_CMS_` names still work (#701).

### Security

- Superuser required for user management and MCP keys; `is_superuser` can't
  be mass-assigned (#642, #646). Tera `get_env` is disabled in authored
  templates (#641).
- The role matrix is enforced on every admin route (#672); a save that puts a
  page live needs the publish right (#761).
- Restricted media is served `private, no-store` (#650); uploaded page-like
  files are refused and script-scheme links dropped (#724).
- Tenant-bound password-reset and preview tokens (#673); page-password
  grants signed and expiring (#737); claimed hostnames need approval (#712).
- The RSS/Atom feeds and the sitemap leave out view-restricted pages (#644).

- Notification channels (Slack, Teams, Telegram, webhook) refuse to deliver
  to loopback, private, link-local or metadata addresses, pin the vetted
  address and follow no redirects (#645). Set `RCMS_NOTIFY_ALLOW_PRIVATE=1`
  to deliver to endpoints on your own network.

### Fixed

- The page schedule (**Go live at** / **Expire at**) stored the typed time
  as if it were UTC, so an editor in Halifax who scheduled 09:00 got 06:00.
  The inputs now show and take the editor's admin timezone (user setting,
  `RCMS_DEFAULT_TIMEZONE`, else the browser's), name it, and store UTC; the
  server accepts `…Z` / offset values. The hints no longer say the times are
  UTC or that expiry archives the page.
- The Scheduled pages report left out the take-down of a page that is still
  scheduled, and described the old archive behaviour.
- A required radio, rating or checkbox group was only checked by the
  server, and its refusal sent the visitor back to step 1 with every answer
  erased.
- The form block's success message override was never shown, and its
  redirect override was ignored on page types made in the admin; the
  redirect field refused the only values the server accepts (`/thank-you`).
- A submission from a translated page (`/fr/…`) or another site's host was
  saved without its source page.
- A checkbox-group condition only looked at the first ticked box.
- The form chooser says "Choose a form…", and chooser dialogs use the
  translated heading.
- A page type with a review workflow goes live only through it: a save or
  new page that would publish is refused or kept as a draft for everyone but
  a superuser, and page creation checks the publish right (#761).
- Renaming a workflow keeps the page types that use it; deleting one in use
  is refused.
- An editor's changes to a live page under re-approval went live at once
  while the review restarted.
- The edit lock heartbeat, autosave, presence banner and comment pins never
  ran on an edit page; autosave wrote the live page directly. A submitter's
  lock is released on submit and when leaving the editor.
- A whole number in a builder field shows as 32, not 32.0.
- The workflow form's review steps and Add a step form sit inside their card.
- A page added to a menu from the page list keeps no label of its own, so
  the item follows the page's title and its translations (it froze the
  title at the time it was added). The page list no longer offers error
  pages.

- A user's email is read from the `email` column that member sign-in,
  sign-up and SSO use (older `data.email` still works): the admin showed
  members without an email, and saving their user form cleared it. The
  account email change writes the column too.
- The user form's **Deactivate** button sat in a form nested inside the
  edit form, so it saved the user instead of deactivating them.
- A signed-in visitor who may not open a private page gets the site's 403
  error page (or the styled fallback) instead of a bare text body.
- The member sign-in and sign-up pages show the site's name (new **Site
  name** under Settings → Branding, else the tenant's name) and link back
  to the site.
- The page Privacy tab shows only the field the chosen rule uses, in plain
  words, without uppercased role names; the confirmation names the rule.
- `published_children` (and so a page's default `children`) leaves out
  error pages, which showed up as cards on listings.
- Developer notes removed from the user form, the users list and the
  Settings screen; the role cards and the permission table fit their card;
  the menu editor's search box is styled and custom links are tagged
  "link", not "external".

- MySQL: long content columns are `LONGTEXT` (#683) and identity lookups are
  exact (#726).
- Page moves update the subtree's URLs in the same transaction (#706);
  revision sequences stay unique under concurrent saves (#762).
- A tenant created after boot is seeded on first use (#689); queued jobs
  resolve their tenant from the registry (#732).
- Every path that puts a page live syncs search and fires the after-publish
  hooks (#692).
- SQLite: analytics retention deletes every event past the cutoff (#654).
- Searches made through the public API (`/api/v2/search/`,
  `/api/v2/pages/?search=`) appear in the admin's search report; only
  admin searches did (#847).
- `admin::public_router_with_mailer` sends password-reset emails;
  `public_router` had no mailer, so the reset link was only printed to the
  console (#846).
- On a hostname-mapped site, `auto_menu` rows, curated menu links,
  `canonical_url` and `pageurl` use the site's public paths, like
  `page_href` (#640).
- Live preview types page, snippet and document chooser ids as numbers, as
  the saved page does (#660).
- The scaffolded blog renders an article whose body is still empty (#620).
- On a hostname-mapped site the sitemap lists only that site's pages, at the
  paths the host serves (#839).
- The default public page of a type made in the admin shows each field by
  kind: photos as images, rich text as HTML, choices by label, links as
  links; it printed image ids and escaped HTML, and has a readable default
  style (#844).
- The allowed parent and child types of a page type made in the admin are
  enforced when creating and moving pages; they were saved but ignored
  (#843).
- Deleting a page no longer fails with a foreign-key error: its revisions,
  tags, logs and other rows that point at it are cleared first, including a
  host's page-type extension rows (#848).
- Tags chosen on the new-page form are saved (they were dropped until the
  first edit), and a new root page with an empty slug becomes the front
  page, as the form says.
- Checkbox and radio options in the admin are no longer shown in capitals.
- The MCP `search_pages` and `list_snippets` tools return a stable order
  (#657); a library element's template name follows the same rule as any
  other editor-supplied template name (#661).

### Performance

- Chooser references resolve with one query per table (#648); renditions are
  generated once, with a concurrency cap (#725); sitemap shards fetch only
  their window (#691); the editor no longer loads the media library (#717).
