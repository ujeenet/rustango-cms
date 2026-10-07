//! Named URL patterns + Tera helper registration.
//!
//! Registered via [`rustango::register_url!`]:
//! every admin route gets a stable name (`rcms-admin:pages:list`,
//! `rcms-admin:pages:edit`, …) so templates resolve URLs through
//! `{{ url(name="…") }}` instead of hard-coding `/cms-admin/...`
//! strings. Mount-prefix changes only need one edit in this file.
//!
//! ## Wire-up
//!
//! Calling [`crate::admin::register_templates`] also calls
//! [`register_tera_helpers`], which registers:
//!
//! - `{{ url(name=…, kwarg=…) }}` — named URL reversal
//!   ([`rustango::urls`]).
//! - `{{ "…" | querystring(…) }}` — URL query-string builder
//!   for pagination + filter links.
//! - `{{ ts | naturaltime }}`, `intcomma`, `naturalsize`,
//!   `ordinal`, etc. — humanize filters
//!   ([`rustango::humanize`]).
//! - `{{ csrf_token | csrf_input | safe }}` — hidden-input shape
//!   of the CSRF token ([`rustango::forms::csrf`]).
//!
//! No-op on the registry-name side: [`rustango::register_url!`]
//! submits each pattern through `inventory`, so the names are live
//! the moment this module is linked into the binary.

use rustango::register_url;

// -- pages tab --
register_url!("rcms-admin:root", "/cms-admin");
// #144 — editor dashboard / home screen.
register_url!("rcms-admin:dashboard", "/cms-admin/dashboard");

// -- auth chrome (logout + password-reset; reachable pre-auth, see admin/mod.rs) --
register_url!("rcms-admin:logout", "/cms-admin/logout");
register_url!("rcms-admin:password-reset", "/cms-admin/password-reset");
register_url!(
    "rcms-admin:password-reset:confirm",
    "/cms-admin/password-reset/confirm"
);

register_url!("rcms-admin:pages:list", "/cms-admin/pages");
register_url!("rcms-admin:pages:new", "/cms-admin/pages/new");
register_url!("rcms-admin:pages:edit", "/cms-admin/pages/{id}/edit");
register_url!(
    "rcms-admin:pages:translate",
    "/cms-admin/pages/{id}/translate"
);
register_url!("rcms-admin:pages:delete", "/cms-admin/pages/{id}/delete");
register_url!("rcms-admin:pages:preview", "/cms-admin/pages/{id}/preview");
register_url!(
    "rcms-admin:pages:revert",
    "/cms-admin/pages/{id}/revert/{rev_id}"
);
register_url!(
    "rcms-admin:pages:history-diff",
    "/cms-admin/pages/{id}/history/diff"
);
// #192 — per-page audit log CSV export.
register_url!("rcms-admin:pages:log-csv", "/cms-admin/pages/{id}/log.csv");
register_url!("rcms-admin:pages:move", "/cms-admin/pages/{id}/move");
register_url!("rcms-admin:pages:clone", "/cms-admin/pages/{id}/clone");
register_url!("rcms-admin:pages:alias", "/cms-admin/pages/{id}/alias");
register_url!("rcms-admin:pages:privacy", "/cms-admin/pages/{id}/privacy");
register_url!("rcms-admin:pages:bulk", "/cms-admin/pages/bulk");
register_url!("rcms-admin:pages:unlock", "/cms-admin/pages/{id}/unlock");
register_url!(
    "rcms-admin:pages:lock-heartbeat",
    "/cms-admin/pages/{id}/lock-heartbeat"
);
register_url!(
    "rcms-admin:pages:lock-release",
    "/cms-admin/pages/{id}/lock-release"
);
// #147 — draft autosave (silent revision capture).
register_url!(
    "rcms-admin:pages:autosave",
    "/cms-admin/pages/{id}/autosave"
);
register_url!(
    "rcms-admin:pages:sessions",
    "/cms-admin/pages/{id}/sessions"
);
// #115 — page subscriptions (notify-on-publish opt-in).
register_url!(
    "rcms-admin:pages:subscribe",
    "/cms-admin/pages/{id}/subscribe"
);
register_url!(
    "rcms-admin:pages:unsubscribe",
    "/cms-admin/pages/{id}/unsubscribe"
);
// #81 PR 1 — inline comments.
register_url!(
    "rcms-admin:pages:comments:new",
    "/cms-admin/pages/{id}/comments/new"
);
register_url!(
    "rcms-admin:pages:comments:reply",
    "/cms-admin/pages/{id}/comments/{comment_id}/reply"
);
register_url!(
    "rcms-admin:pages:comments:resolve",
    "/cms-admin/pages/{id}/comments/{comment_id}/resolve"
);
register_url!(
    "rcms-admin:pages:comments:reopen",
    "/cms-admin/pages/{id}/comments/{comment_id}/reopen"
);

// #533 FB-04 — dedicated visual Form Builder editor route.
register_url!("rcms-admin:forms:build", "/cms-admin/forms/{id}/build");
register_url!("rcms-admin:forms:preview", "/cms-admin/forms/{id}/preview");
// #548 FB-15 — publish the draft.
register_url!("rcms-admin:forms:publish", "/cms-admin/forms/{id}/publish");
// #525 — persist the admin UI language.
register_url!("rcms-admin:set-language", "/cms-admin/set-language");
// #545 FB-12 — per-form submission history.
register_url!(
    "rcms-admin:forms:submissions",
    "/cms-admin/forms/{id}/submissions"
);
// #73 PR 2 — per-page workflow actions.
register_url!(
    "rcms-admin:pages:workflow-submit",
    "/cms-admin/pages/{id}/workflow/submit"
);
register_url!(
    "rcms-admin:pages:workflow-approve",
    "/cms-admin/pages/{id}/workflow/approve"
);
register_url!(
    "rcms-admin:pages:workflow-reject",
    "/cms-admin/pages/{id}/workflow/reject"
);
register_url!(
    "rcms-admin:pages:workflow-cancel",
    "/cms-admin/pages/{id}/workflow/cancel"
);

// -- library (snippets) tab --
register_url!("rcms-admin:library:list", "/cms-admin/library");
register_url!("rcms-admin:library:new", "/cms-admin/library/new");
register_url!("rcms-admin:library:edit", "/cms-admin/library/{id}/edit");
register_url!(
    "rcms-admin:library:delete",
    "/cms-admin/library/{id}/delete"
);
register_url!(
    "rcms-admin:library:bulk",
    "/cms-admin/library/{type_name}/bulk"
);
// #123 — opt-in snippet revisions.
register_url!(
    "rcms-admin:library:revert",
    "/cms-admin/library/{id}/revert/{seq}"
);
// #186 — bulk import snippets from CSV / TSV.
register_url!(
    "rcms-admin:library:import",
    "/cms-admin/library/{type_name}/import"
);

// -- media / documents tab --
register_url!("rcms-admin:media:list", "/cms-admin/media");
register_url!("rcms-admin:media:upload", "/cms-admin/media/upload");
// #142 — staged-upload scratch storage.
register_url!(
    "rcms-admin:media:upload-staged",
    "/cms-admin/media/upload-staged"
);
register_url!(
    "rcms-admin:media:upload-staged:cancel",
    "/cms-admin/media/upload-staged/{id}/cancel"
);
// #188 — promote staged uploads to cms_media in bulk with per-file metadata.
register_url!(
    "rcms-admin:media:upload-staged:commit",
    "/cms-admin/media/upload-staged/commit"
);
register_url!("rcms-admin:media:edit", "/cms-admin/media/{id}/edit");
register_url!("rcms-admin:media:crop", "/cms-admin/media/{id}/crop");
// #190 — re-upload bytes for an existing media id.
register_url!("rcms-admin:media:replace", "/cms-admin/media/{id}/replace");
register_url!("rcms-admin:media:delete", "/cms-admin/media/{id}/delete");
register_url!(
    "rcms-admin:media:collection-permissions",
    "/cms-admin/media/collections/{id}/permissions"
);
// #195 — collection view restriction (login / groups / password).
register_url!(
    "rcms-admin:media:collection-privacy",
    "/cms-admin/media/collections/{id}/privacy"
);
register_url!(
    "rcms-admin:media:collections",
    "/cms-admin/media/collections"
);
register_url!(
    "rcms-admin:media:collections:new",
    "/cms-admin/media/collections/new"
);
register_url!(
    "rcms-admin:media:collections:edit",
    "/cms-admin/media/collections/{id}/edit"
);
register_url!(
    "rcms-admin:media:collections:delete",
    "/cms-admin/media/collections/{id}/delete"
);
// #557 — categories / taxonomies.
register_url!("rcms-admin:taxonomies:list", "/cms-admin/taxonomies");
register_url!("rcms-admin:taxonomies:tree", "/cms-admin/taxonomies/{slug}");
register_url!(
    "rcms-admin:taxonomies:categories:new",
    "/cms-admin/taxonomies/{slug}/categories/new"
);
register_url!(
    "rcms-admin:taxonomies:categories:edit",
    "/cms-admin/taxonomies/{slug}/categories/{id}/edit"
);
register_url!(
    "rcms-admin:taxonomies:categories:delete",
    "/cms-admin/taxonomies/{slug}/categories/{id}/delete"
);
register_url!(
    "rcms-admin:taxonomies:categories:translate",
    "/cms-admin/taxonomies/{slug}/categories/{id}/translate"
);
register_url!(
    "rcms-admin:media:bulk-move-collection",
    "/cms-admin/media/bulk-move-collection"
);
register_url!("rcms-admin:documents:list", "/cms-admin/documents");

// -- locales tab --
register_url!("rcms-admin:sso-providers:list", "/cms-admin/sso-providers");
register_url!(
    "rcms-admin:sso-providers:new",
    "/cms-admin/sso-providers/new"
);
register_url!(
    "rcms-admin:sso-providers:edit",
    "/cms-admin/sso-providers/{id}/edit"
);
register_url!(
    "rcms-admin:sso-providers:delete",
    "/cms-admin/sso-providers/{id}/delete"
);
register_url!("rcms-admin:locales:list", "/cms-admin/locales");
register_url!("rcms-admin:locales:new", "/cms-admin/locales/new");
register_url!("rcms-admin:locales:edit", "/cms-admin/locales/{id}/edit");
register_url!(
    "rcms-admin:locales:delete",
    "/cms-admin/locales/{id}/delete"
);

// -- redirects tab (editor-managed 301/302) --
register_url!("rcms-admin:redirects:list", "/cms-admin/redirects");
register_url!("rcms-admin:redirects:new", "/cms-admin/redirects/new");
register_url!(
    "rcms-admin:redirects:edit",
    "/cms-admin/redirects/{id}/edit"
);
register_url!(
    "rcms-admin:redirects:delete",
    "/cms-admin/redirects/{id}/delete"
);
// #145 — bulk import from CSV / TSV.
register_url!("rcms-admin:redirects:import", "/cms-admin/redirects/import");
register_url!(
    "rcms-admin:redirects:export",
    "/cms-admin/redirects/export.csv"
);

// -- navigation menus (#22) --
register_url!("rcms-admin:navigation:list", "/cms-admin/navigation");
register_url!("rcms-admin:navigation:new", "/cms-admin/navigation/new");
register_url!(
    "rcms-admin:navigation:edit",
    "/cms-admin/navigation/{id}/edit"
);
register_url!(
    "rcms-admin:navigation:clone",
    "/cms-admin/navigation/{id}/clone"
);
register_url!(
    "rcms-admin:navigation:save-tree",
    "/cms-admin/navigation/{id}/save-tree"
);
register_url!(
    "rcms-admin:navigation:seed-from-pages",
    "/cms-admin/navigation/{id}/seed-from-pages"
);
register_url!(
    "rcms-admin:navigation:delete",
    "/cms-admin/navigation/{id}/delete"
);
register_url!(
    "rcms-admin:navigation:translate",
    "/cms-admin/navigation/{id}/translate"
);
register_url!(
    "rcms-admin:navigation:items",
    "/cms-admin/navigation/{id}/items"
);
register_url!(
    "rcms-admin:navigation:item-delete",
    "/cms-admin/navigation/{menu_id}/items/{id}/delete"
);

// -- workflows (#73 PR 1 — multi-step approvals) --
register_url!("rcms-admin:workflows:list", "/cms-admin/workflows");
register_url!("rcms-admin:workflows:new", "/cms-admin/workflows/new");
register_url!(
    "rcms-admin:workflows:edit",
    "/cms-admin/workflows/{id}/edit"
);
register_url!(
    "rcms-admin:workflows:delete",
    "/cms-admin/workflows/{id}/delete"
);
register_url!(
    "rcms-admin:workflows:task-add",
    "/cms-admin/workflows/{id}/tasks/add"
);
register_url!(
    "rcms-admin:workflows:task-delete",
    "/cms-admin/workflows/{id}/tasks/{task_id}/delete"
);

// -- history (tenant-wide revision audit) --
register_url!("rcms-admin:history", "/cms-admin/history");

// -- site settings (#108) --
register_url!("rcms-admin:site-settings:list", "/cms-admin/site-settings");
// Per-tenant template overrides — the admin editor.
register_url!("rcms-admin:templates:list", "/cms-admin/templates");
register_url!(
    "rcms-admin:page-types:template",
    "/cms-admin/page-types/{id}/template"
);
register_url!("rcms-admin:templates:edit", "/cms-admin/templates/edit");
register_url!("rcms-admin:templates:new", "/cms-admin/templates/new");
register_url!(
    "rcms-admin:templates:validate",
    "/cms-admin/templates/validate"
);
register_url!("rcms-admin:templates:delete", "/cms-admin/templates/delete");
register_url!(
    "rcms-admin:site-settings:edit",
    "/cms-admin/site-settings/{scope}/edit"
);
register_url!(
    "rcms-admin:site-settings:delete",
    "/cms-admin/site-settings/{scope}/delete"
);
register_url!(
    "rcms-admin:site-settings:translate",
    "/cms-admin/site-settings/{scope}/translate"
);

// -- aging-pages report (#101) --
register_url!("rcms-admin:reports:aging", "/cms-admin/reports/aging");
register_url!(
    "rcms-admin:reports:aging-csv",
    "/cms-admin/reports/aging.csv"
);
register_url!(
    "rcms-admin:reports:locked-pages",
    "/cms-admin/reports/locked-pages"
);
register_url!(
    "rcms-admin:reports:locked-pages-csv",
    "/cms-admin/reports/locked-pages.csv"
);
register_url!(
    "rcms-admin:reports:workflows",
    "/cms-admin/reports/workflows"
);
register_url!(
    "rcms-admin:reports:workflows-csv",
    "/cms-admin/reports/workflows.csv"
);
// #122 — revision storage + prune.
register_url!(
    "rcms-admin:reports:revisions",
    "/cms-admin/reports/revisions"
);
register_url!(
    "rcms-admin:reports:revisions:prune",
    "/cms-admin/reports/revisions/prune"
);
// #141 — scheduled-publish report.
register_url!(
    "rcms-admin:reports:scheduled",
    "/cms-admin/reports/scheduled"
);
register_url!(
    "rcms-admin:reports:scheduled-csv",
    "/cms-admin/reports/scheduled.csv"
);
// #143 — search promotions + query log.
register_url!("rcms-admin:reports:search", "/cms-admin/reports/search");
register_url!(
    "rcms-admin:reports:search:edit",
    "/cms-admin/reports/search/{id}/edit"
);

// -- global search (#77) --
register_url!("rcms-admin:search", "/cms-admin/search");

// -- page types (registry + usage stats) --
register_url!("rcms-admin:page-types", "/cms-admin/page-types");
register_url!("rcms-admin:page-types:new", "/cms-admin/page-types/new");
// #843 — settings of an admin-made type (name, parent/child rules, workflow).
register_url!("rcms-admin:page-types:edit", "/cms-admin/page-types/{id}/edit");
register_url!(
    "rcms-admin:page-types:delete",
    "/cms-admin/page-types/{id}/delete"
);
register_url!(
    "rcms-admin:page-types:build",
    "/cms-admin/page-types/{id}/build"
);
register_url!(
    "rcms-admin:page-types:publish",
    "/cms-admin/page-types/{id}/build/publish"
);

// -- reusable components library (#563) --
register_url!("rcms-admin:components:index", "/cms-admin/components");
register_url!("rcms-admin:components:new", "/cms-admin/components/new");
register_url!(
    "rcms-admin:components:edit",
    "/cms-admin/components/{id}/edit"
);
register_url!(
    "rcms-admin:components:delete",
    "/cms-admin/components/{id}/delete"
);

// -- unused media report (#8) --
register_url!("rcms-admin:media:unused", "/cms-admin/media/unused");
register_url!(
    "rcms-admin:media:unused:delete",
    "/cms-admin/media/unused/delete"
);

// -- settings tab --
register_url!("rcms-admin:settings", "/cms-admin/settings");
register_url!("rcms-admin:settings:theme", "/cms-admin/settings/theme");
// #199 — per-tenant branding (logo + favicon).
register_url!(
    "rcms-admin:settings:branding",
    "/cms-admin/settings/branding"
);

// -- no-access page (#35) — destination for the cms_admin.access gate --
register_url!("rcms-admin:no-access", "/cms-admin/no-access");

// -- per-user account preferences (#27) --
register_url!("rcms-admin:account", "/cms-admin/me");
register_url!(
    "rcms-admin:account:mcp-key-create",
    "/cms-admin/me/mcp-keys"
);
register_url!(
    "rcms-admin:account:mcp-key-revoke",
    "/cms-admin/me/mcp-keys/{id}/revoke"
);
// #197 — per-user notification preferences (workflow / comments / publishes).
register_url!(
    "rcms-admin:account:notifications",
    "/cms-admin/me/notifications"
);
// JSON endpoint backing the PageChooser widget.
register_url!("rcms-admin:page-chooser", "/cms-admin/__page-chooser");
// JSON endpoints backing the Snippet + Document choosers.
register_url!("rcms-admin:snippet-chooser", "/cms-admin/__snippet-chooser");
register_url!(
    "rcms-admin:document-chooser",
    "/cms-admin/__document-chooser"
);
// #201 — avatar upload + display name change.
register_url!("rcms-admin:account:profile", "/cms-admin/me/profile");
// #526 — per-user admin UI language preference.
register_url!("rcms-admin:account:language", "/cms-admin/me/language");
// #202 — email change (with confirmation flow) + password change.
register_url!("rcms-admin:account:email", "/cms-admin/me/email");
register_url!(
    "rcms-admin:account:email:confirm",
    "/cms-admin/me/email/confirm"
);
register_url!("rcms-admin:account:password", "/cms-admin/me/password");
// #129 — styleguide / component gallery.
register_url!("rcms-admin:styleguide", "/cms-admin/styleguide");
// #139 — banner dismissibles.
register_url!("rcms-admin:dismissible", "/cms-admin/dismissibles/{key}");
// `register_admin_page!`-registered custom admin pages (admin/mod.rs `/cms-admin/x/{slug}`).
register_url!("rcms-admin:custom-page", "/cms-admin/x/{slug}");

// -- users / roles (proxied to framework admin) --
register_url!("rcms-admin:users", "/cms-admin/users");
register_url!("rcms-admin:users:new", "/cms-admin/users/new");
register_url!("rcms-admin:users:edit", "/cms-admin/users/{id}/edit");
register_url!(
    "rcms-admin:users:mcp-key-create",
    "/cms-admin/users/{id}/mcp-keys"
);
register_url!(
    "rcms-admin:users:deactivate",
    "/cms-admin/users/{id}/deactivate"
);
register_url!("rcms-admin:roles", "/cms-admin/roles");
register_url!("rcms-admin:roles:new", "/cms-admin/roles/new");
register_url!("rcms-admin:roles:edit", "/cms-admin/roles/{id}/edit");
register_url!("rcms-admin:roles:delete", "/cms-admin/roles/{id}/delete");

// -- public-side --
register_url!("rcms:sitemap", "/sitemap.xml");
register_url!("rcms:feed:rss", "/feed/{kind}/rss.xml");
register_url!("rcms:feed:atom", "/feed/{kind}/atom.xml");

/// Register every Tera function / filter the bundled admin
/// templates rely on. Called from
/// [`crate::admin::register_templates`]; host apps don't need to
/// call it themselves unless they're building Tera without the
/// admin templates.
pub fn register_tera_helpers(tera: &mut tera::Tera) {
    rustango::urls::register_url_tag(tera);
    rustango::urls::register_querystring_filter(tera);
    rustango::humanize::register_filters(tera);
    rustango::forms::csrf::register_csrf_filter(tera);
    // #247 — `pageurl(id=…)` + `| pageurl`. The function reads from a
    // thread-local map installed by [`crate::render::render_inner`]
    // for the duration of `tera.render`, so registering it once on
    // the shared Tera is safe.
    crate::page_url::register_tera_function(tera);
    // #members — `can_view(page=…)` / `{{ p | can_view }}` (viewer-aware
    // access predicate) + `{{ list | visible }}` (filter a page/menu list,
    // nested-aware). Reads the render-scoped `AccessContext` thread-local
    // installed by `render::render_inner`; used to guard hand-written page
    // links and nested lists with the same protection the built-in menus
    // apply.
    crate::access::register_tera_function(tera);
    // #249 — `children_filtered(status=…, order_by=…, limit=…)`. Same
    // thread-local install pattern as `pageurl`; the rendered page's
    // children get installed by `render::render_inner` and cleared
    // when render returns.
    crate::children_filtered::register_tera_function(tera);
    // #248 — `| richtext`. Walks `<a linktype="page|media" id="N">`
    // in editor-emitted HTML and rewrites `href` against the same
    // `_pages_by_id` thread-local `pageurl` reads; the rest are resolved
    // over the final page by `richtext::resolve_internal_links`. `is_safe = true` so
    // the editor's already-escaped output passes through verbatim.
    crate::richtext::register_tera_filter(tera);
    // #244 — `auto_menu(parent_id=…, depth=…)`, the automatic menu.
    // The render handler pre-fetches every `show_in_menus = true AND
    // status = published` page into a thread-local pool; the function
    // walks that pool at call time to build the nested item tree.
    crate::auto_menu::register_tera_function(tera);
    // #639 — `menu(slug=…)`, the editor-curated menus. Same pattern: the
    // render resolves every menu and installs them in a thread-local.
    crate::navigation::register_tera_function(tera);
    // #845 — `cms_snippet(slug=…)`, a library element by slug. Same
    // pattern again: the render installs the prefetched snippets.
    crate::snippet::register_tera_function(tera);
    // #255 — `| date_parse` + `| format_date`. Fail-soft string-to-
    // date filters for extension fields backed by a `Date` widget
    // (storage shape is `YYYY-MM-DD`); Tera's built-in `| date`
    // errors on empty/null/unparseable inputs which 500s the page.
    crate::date_filters::register_tera_filters(tera);
    // #253 — `rcms_image_url(media_id, filter, v)`. Pure function;
    // already accepts stringified ids (block-storage shape) + bare
    // i64s (loaded-row shape). Wiring it here makes the bundled
    // `blocks/image.html` AND admin templates (media_list, media_edit)
    // actually resolve the call at render time.
    crate::rendition::register_tera_function(tera);
    // #250 — `snippet_html(type=…, id=N)`. Per-snippet template
    // rendering. The render handler pre-renders every snippet whose
    // handler declares a `render_template`; this function pulls the
    // pre-rendered HTML keyed on `(type, id)`.
    crate::snippet_render::register_tera_function(tera);
    // #245 — `site_setting(scope=…)`. Looks up a pre-fetched
    // `cms_site_setting` row's `value_json` by scope; templates
    // access fields via `{{ site_setting(scope="footer").site_name }}`.
    crate::site_setting::register_tera_function(tera);
    // #445/#453 — the remaining public render helpers, wired here so a
    // single `admin::register_templates` call gives a host EVERY public
    // template function/filter (no separate meta_tags / t / markdown
    // registration to hunt for): `rcms_meta_tags(page=…)`, the `| t`
    // per-field translation filter, and the `| markdown` filter.
    crate::meta_tags::register_tera_function(tera);
    crate::translation::register_tera_filter(tera);
    crate::markdown::register_tera_filter(tera);
    // #401 — `language_switcher()` returns the active locales + the URL
    // to the current page in each (data installed per-request by the
    // public render handler).
    crate::language_switcher::register_tera_function(tera);
    // #i18n — `rcms_hreflang_tags(origin=…)` emits the `<head>` hreflang
    // alternates cluster so SEO crawlers / audit tools that read the page
    // head (not just the sitemap) discover the language versions.
    crate::language_switcher::register_hreflang_function(tera);
    // #442 — `routable_url()` reverses a routable page's sub-URL
    // patterns.
    crate::routable::register_tera_function(tera);
}

/// Surface duplicate URL-pattern names at boot. Returns a list of
/// names registered more than once; empty list means clean.
/// Re-exports [`rustango::urls::duplicates`] for callers that don't
/// want to depend on the framework module directly.
#[must_use]
pub fn duplicate_url_names() -> Vec<&'static str> {
    rustango::urls::duplicates()
}
