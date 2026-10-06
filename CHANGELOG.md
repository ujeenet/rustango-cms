# Changelog

All notable changes to rustango-cms. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Breaking changes
have an entry in [UPGRADING.md](UPGRADING.md).

## Unreleased

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
