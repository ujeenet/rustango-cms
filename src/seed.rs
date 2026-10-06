//! Per-tenant `cms_page_type` seeding.
//!
//! At boot time the framework hooks `Cli::seed(rcms::ensure_seeded)`,
//! which fans out across every active `Org` and upserts one row per
//! `PageTypeHandler` registered via [`crate::register_page_type!`]
//! (or by hand via [`crate::PageTypeHandlerRegistration`]).
//!
//! Idempotent — re-running on an already-seeded tenant overwrites
//! the metadata fields (verbose_name, default_template,
//! allowed_parent_types) but never touches the surrogate `id`, so
//! existing FKs from `cms_page.page_type_id` remain valid.

use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, ExecError, FetcherPool as _};
use rustango::tenancy::{Org, TenantPools};

use crate::page_type::{registered_handlers, PageTypeHandler};
use crate::page_type_model::PageType;

/// Errors from `ensure_seeded`. Propagates the underlying tenancy /
/// driver errors verbatim so callers can match on them.
#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    #[error(transparent)]
    Tenancy(#[from] rustango::tenancy::TenancyError),
    #[error(transparent)]
    Db(#[from] ExecError),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),

    /// A seeder's own failure: bad configuration, a missing
    /// registration, seed data it cannot read. Without this a host
    /// seeder can only report driver errors, so anything that is
    /// genuinely wrong with *its* inputs has to be forced into a
    /// database error that misdescribes it.
    #[error("{0}")]
    Seeder(String),
}

/// Walk every active `Org` and ensure the tenant has one
/// `cms_page_type` row per registered `PageTypeHandler`.
///
/// Wire as the `Cli::seed` hook so it runs once per boot after
/// migrations. Cheap — does one SELECT + zero-or-one
/// INSERT/UPDATE per (tenant, handler) pair.
///
/// Accepts a tri-dialect [`rustango::sql::Pool`] so the same hook
/// works across Postgres / SQLite / MySQL registries. Internally it
/// matches the active variant and builds a `TenantPools<DB>` parameterized
/// by the concrete sqlx backend — the seeding loop itself is fully
/// dialect-agnostic because [`TenantPools::scoped_pool_dyn`] and
/// [`upsert_page_type`] both speak the erased [`rustango::sql::Pool`].
///
/// Pre-conditions:
/// - The registry DB is reachable via `registry`.
/// - Tenants' tenant-scope migrations (including `0001_initial`
///   from this crate) have already been applied so `cms_page_type`
///   exists.
///
/// # Errors
/// Driver / tenancy errors from any tenant; the first failure
/// short-circuits the whole fan-out.
pub async fn ensure_seeded(registry: &rustango::sql::Pool) -> Result<(), SeedError> {
    // Queued jobs resolve tenants they weren't dispatched with through it.
    crate::task_queue::set_registry(registry.clone());
    match registry {
        #[cfg(feature = "postgres")]
        rustango::sql::Pool::Postgres(pg) => {
            seed_orgs(TenantPools::<sqlx::Postgres>::new(pg.clone())).await
        }
        #[cfg(feature = "sqlite")]
        rustango::sql::Pool::Sqlite(sq) => {
            seed_orgs(TenantPools::<sqlx::Sqlite>::new(sq.clone())).await
        }
        #[cfg(feature = "mysql")]
        rustango::sql::Pool::Mysql(my) => {
            seed_orgs(TenantPools::<sqlx::MySql>::new(my.clone())).await
        }
    }
}

async fn seed_orgs<DB>(pools: TenantPools<DB>) -> Result<(), SeedError>
where
    DB: sqlx::Database,
    rustango::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry_pool = pools.registry_pool();
    let orgs: Vec<Org> = Org::objects().fetch(&registry_pool).await?;

    // #689 — every tenant is seeded even when one fails: a broken tenant
    // used to leave every tenant after it unseeded until the next boot.
    let mut first_err = None;
    for org in orgs {
        if !org.active {
            continue;
        }
        let seeded = match pools.scoped_pool_dyn(&org).await {
            Ok(tenant_pool) => seed_tenant(&tenant_pool, &org).await,
            Err(e) => Err(e.into()),
        };
        match seeded {
            Ok(()) => mark_seeded(&org.slug),
            Err(e) => {
                tracing::error!(target: "rustango_cms::seed", tenant = %org.slug, error = %e, "tenant seed failed");
                first_err.get_or_insert(e);
            }
        }
    }
    first_err.map_or(Ok(()), Err)
}

/// Tenants seeded by this process — at boot, or lazily on first request.
static SEEDED: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
    std::sync::Mutex::new(None);

fn mark_seeded(slug: &str) {
    let mut g = SEEDED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    g.get_or_insert_with(Default::default).insert(slug.to_owned());
}

fn is_seeded(slug: &str) -> bool {
    let g = SEEDED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    g.as_ref().is_some_and(|set| set.contains(slug))
}

/// Seed `org` if this process hasn't yet (#689). A tenant provisioned
/// while the server runs — operator console, provisioning webhook,
/// `create-tenant` — gets its tracked migrations but missed the boot-time
/// seed, so it had no page types, roles or locale until a restart. One
/// tenant is seeded at a time, and a second request for it waits rather
/// than seeding again.
///
/// # Errors
/// The first failing seed step; the tenant stays unmarked and the next
/// request retries.
pub async fn ensure_tenant_seeded(pool: &rustango::sql::Pool, org: &Org) -> Result<(), SeedError> {
    if is_seeded(&org.slug) {
        return Ok(());
    }
    static LOCK: rustango::__private_runtime::tokio::sync::Mutex<()> =
        rustango::__private_runtime::tokio::sync::Mutex::const_new(());
    let _held = LOCK.lock().await;
    if is_seeded(&org.slug) {
        return Ok(());
    }
    seed_tenant(pool, org).await?;
    mark_seeded(&org.slug);
    tracing::info!(target: "rustango_cms::seed", tenant = %org.slug, "seeded a tenant provisioned after boot");
    Ok(())
}

/// Middleware running [`ensure_tenant_seeded`] for the request's tenant.
/// Layered onto the admin and public routers; costs one set lookup once a
/// tenant is seeded. A tenant that can't be resolved or seeded falls
/// through — the handler reports that as it did before.
pub(crate) async fn lazy_seed_layer(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use rustango::tenancy::OrgResolver as _;
    let (mut parts, body) = req.into_parts();
    if let Some(ctx) = parts
        .extensions
        .get::<std::sync::Arc<rustango::extractors::TenantContext>>()
        .cloned()
    {
        if let Ok(Some(org)) = ctx.resolver.resolve(&mut parts, &ctx.pools.registry_pool()).await {
            if !is_seeded(&org.slug) {
                match ctx.pools.scoped_pool_dyn(&org).await {
                    Ok(pool) => {
                        if let Err(e) = ensure_tenant_seeded(&pool, &org).await {
                            tracing::error!(target: "rustango_cms::seed", tenant = %org.slug, error = %e, "lazy tenant seed failed");
                        }
                    }
                    Err(e) => tracing::warn!(target: "rustango_cms::seed", tenant = %org.slug, error = %e, "tenant pool unavailable for seeding"),
                }
            }
        }
    }
    next.run(axum::http::Request::from_parts(parts, body)).await
}

/// Everything a tenant needs beyond its tracked migrations: registry rows
/// for page, library and taxonomy types, themes, roles, the default
/// locale, the ensure-only tables and columns, MCP skills and the host's
/// seeders. Idempotent. Run for every tenant at boot, and for a tenant
/// provisioned while the server runs on its first request (#689).
///
/// # Errors
/// The first failing step.
pub async fn seed_tenant(tenant_pool: &rustango::sql::Pool, org: &Org) -> Result<(), SeedError> {
    let tenant_pool = tenant_pool.clone();
    for handler in registered_handlers() {
        upsert_page_type(&tenant_pool, handler.as_ref()).await?;
    }
    // Seed `cms_library_type` rows for every
    // `register_library_type!`-registered handler too.
    for handler in crate::library::registered_handlers() {
        crate::library::ensure_library_type_row(&tenant_pool, handler.as_ref()).await?;
    }
    // #557 — seed `cms_taxonomy` rows from the taxonomy handler
    // inventory (built-in `category` + any `register_taxonomy!` ones),
    // mirroring `upsert_page_type`. The `cms_taxonomy`/`cms_category`
    // tables come from the migration (0005_categories), so this is a
    // best-effort sync: on a tenant that hasn't run `migrate-tenants`
    // yet the table is absent — warn, don't fail boot.
    for handler in crate::category::registered_handlers() {
        if let Err(e) = crate::category::upsert_taxonomy(&tenant_pool, handler.as_ref()).await {
            tracing::warn!(
                target: "rustango_cms::seed",
                taxonomy = handler.slug(), error = %e,
                "taxonomy upsert failed — has `migrate-tenants` created cms_taxonomy?"
            );
        }
    }
    // Theme presets — six MUI-aligned rows. Idempotent upsert
    // keyed on `slug`; safe to re-run.
    crate::theme_seed::ensure_themes_seeded(&tenant_pool).await?;
    // Mirror every existing form's `notify_emails` into a notification
    // target. Without this only forms saved *after* the notification
    // service landed would notify anyone — the setting would still be
    // sitting in the form, visibly configured, quietly doing nothing.
    // Idempotent: `sync_form_email` upserts one managed target per form.
    if let Err(e) = reconcile_form_notification_targets(&tenant_pool).await {
        tracing::warn!(
            target: "rustango_cms::seed",
            error = %e,
            "could not reconcile form notification targets"
        );
    }
    // Default "Root" media collection — created once per tenant
    // (#5) so the upload form / media list always has a place
    // to land. Idempotent: no-op when any collection row exists.
    ensure_root_media_collection(&tenant_pool).await?;
    // Default Viewer / Editor / Administrator roles for the
    // permissions matrix (#33). Idempotent: skipped per-role
    // when a row with the same `name` already exists, so
    // edits made by admins through the matrix survive a
    // re-seed.
    ensure_default_roles(&tenant_pool).await?;
    // #452 — seed a default `en` locale so a fresh tenant satisfies
    // the "exactly one is_default" invariant the admin enforces;
    // without it `resolve_locale` returns None on first boot.
    ensure_default_locale(&tenant_pool).await?;
    // #544 FB-11 — ensure the form-submission table exists (created via
    // CREATE TABLE IF NOT EXISTS since gen_migration is blocked).
    crate::forms::submit::ensure_table(&tenant_pool).await?;
    // Error pages — ensure the ErrorPage extension table exists (same
    // CREATE TABLE IF NOT EXISTS pattern). The `cms_page_type` registry
    // row itself comes from `upsert_page_type` via the handler inventory.
    crate::error_pages::ensure_table(&tenant_pool).await?;
    // #762 — UNIQUE (page_id, sequence) on revisions, so concurrent
    // saves can't share a sequence. Warns: without it only that race
    // stays open.
    if let Err(e) = crate::revision::ensure_unique_sequence(&tenant_pool).await {
        tracing::warn!(target: "rustango_cms::seed", error = %e, "revision sequence index not created");
    }
    // #553 — redirect engine v2 columns (overrides_live, page links,
    // is_active/disabled_reason). ALTER-based ensure, idempotent per boot.
    crate::redirect::ensure_columns(&tenant_pool).await?;
    // Default analytics — ensure the event table (+ indexes) exists and
    // prune events past the retention window. Idempotent per boot.
    crate::analytics::ensure_table(&tenant_pool).await?;
    let retain = crate::analytics::retention::retention_days(&tenant_pool).await;
    if let Err(e) = crate::analytics::retention::prune(&tenant_pool, retain).await {
        tracing::warn!(error = %e, "analytics retention prune failed");
    }
    // #729 — the GIN index behind Postgres full-text search. Without it
    // every search re-parses every page (to filter and again to rank).
    // A no-op off Postgres; a role without CREATE privilege only loses
    // the speed-up, so it warns rather than failing the boot.
    if let Err(e) = crate::search::ensure_pg_fts_index(&tenant_pool).await {
        tracing::warn!(error = %e, "full-text search index not created");
    }
    // #532 — editable admin-translation overrides. The model is
    // `managed = false`, so create its table here (idempotent CREATE TABLE
    // IF NOT EXISTS); the framework admin can then browse/edit it and the
    // shared translator picks edits up via `i18n::maybe_refresh_overrides`.
    rustango::i18n::db::ensure_table_pool(&tenant_pool).await?;
    // #587 — MCP skills: bundle the CMS tools and map each bundle onto the
    // existing permission codenames, so a user-owned key's capabilities
    // follow the tenant's RBAC exactly (see `crate::mcp`).
    if let Err(e) = ensure_mcp_skills(&tenant_pool).await {
        tracing::warn!(error = %e, "mcp skill seeding failed");
    }
    // #733 — overrides live on disk by slug, and a purged tenant's
    // slug can be reused. Bind this tenant's directory to its database
    // so a successor never renders through the old customer's files.
    if let Some(templates) = crate::tenant_templates::installed() {
        match crate::tenant_templates::database_birth(&tenant_pool).await {
            Ok(Some(owner)) => match templates.claim(&org.slug, &owner) {
                Ok(crate::tenant_templates::Claim::MovedAside { previous_owner, to }) => {
                    tracing::warn!(
                        target: "rustango_cms::seed",
                        tenant = %org.slug, previous_owner, to = %to.display(),
                        "template overrides belonged to an earlier tenant with this slug; moved aside"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(target: "rustango_cms::seed", tenant = %org.slug, error = %e, "could not claim template overrides"),
            },
            Ok(None) => {}
            Err(e) => tracing::warn!(target: "rustango_cms::seed", tenant = %org.slug, error = %e, "could not read the database's identity"),
        }
    }
    // Host-registered seeding, last: a host's own tables, roles and
    // default rows may depend on anything above, and nothing above
    // depends on them.
    fire_tenant_seeders(&tenant_pool, &org).await?;
    // #683 — after the host seeders, so their page-type tables exist:
    // widen MySQL `TEXT` content columns to `LONGTEXT`. A failure only
    // keeps the 64 KiB cap, so it warns rather than failing the boot.
    match crate::mysql_text::widen_long_text(&tenant_pool).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(target: "rustango_cms::seed", columns = n, "widened TEXT columns to LONGTEXT"),
        Err(e) => tracing::warn!(target: "rustango_cms::seed", error = %e, "could not widen TEXT columns"),
    }
    // #726 — exact, case-sensitive matching on the columns the CMS
    // resolves URLs, slugs and hosts by, as on Postgres and SQLite.
    match crate::mysql_text::exact_identity_columns(&tenant_pool).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(target: "rustango_cms::seed", columns = n, "gave identity columns a binary collation"),
        Err(e) => tracing::warn!(target: "rustango_cms::seed", error = %e, "could not set binary collation on identity columns"),
    }
    Ok(())
}

// ---------------------------------------------------------------
// TenantSeeder — the host's turn at per-tenant setup
// ---------------------------------------------------------------

/// Boxed future returned by a [`TenantSeeder`].
pub type SeedFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SeedError>> + Send + 'a>>;

/// A host-registered per-tenant seeding pass.
///
/// Everything above in this loop — page types, library types, roles,
/// locales, MCP skills — is seeded per tenant with the *tenant's* pool.
/// A host application had no way into that: `Cli::seed` hands it the
/// **registry** pool, so a host wanting its own permission codename on a
/// role, or a default row in its own table, either wrote to the wrong
/// database or did without.
///
/// This is that hook. It runs once per active tenant per boot, with the
/// tenant's pool, after the CMS's own seeding.
///
/// ```ignore
/// fn seed<'a>(pool: &'a Pool, _org: &'a Org) -> SeedFuture<'a> {
///     Box::pin(async move {
///         rustango_cms::seed::grant_codename(pool, "Developer", "myapp.author").await?;
///         Ok(())
///     })
/// }
/// rustango_cms::register_tenant_seeder!(seed);
/// ```
///
/// **Errors stop the boot.** That is deliberate: a seeder returning
/// `Err` is saying this tenant is not correctly set up, and the failure
/// this hook exists to prevent — a permission that silently reaches
/// nobody — is exactly the kind that nothing downstream detects. A host
/// that wants best-effort behaviour should log and return `Ok(())`
/// itself, where it knows what is safe to skip.
#[derive(Clone, Copy)]
pub struct TenantSeeder(pub for<'a> fn(&'a rustango::sql::Pool, &'a Org) -> SeedFuture<'a>);

inventory::collect!(TenantSeeder);

/// Register a [`TenantSeeder`] at compile time.
///
/// The submission lives in a static initializer, so reference the
/// containing module from `main` (the `blocks::link()` convention) or
/// the linker may drop it.
#[macro_export]
macro_rules! register_tenant_seeder {
    ($f:expr) => {
        ::rustango_cms::inventory::submit! {
            $crate::seed::TenantSeeder($f)
        }
    };
}

/// Run every registered [`TenantSeeder`] against one tenant.
///
/// # Errors
/// The first seeder to fail short-circuits, matching how the rest of
/// this fan-out treats a tenant it could not set up.
async fn fire_tenant_seeders(
    pool: &rustango::sql::Pool,
    org: &Org,
) -> Result<(), SeedError> {
    for seeder in inventory::iter::<TenantSeeder>() {
        (seeder.0)(pool, org).await?;
    }
    Ok(())
}

/// Grant `codename` to the role named `role_name`, if it isn't already.
///
/// The public face of the same backfill the CMS uses for its own
/// codenames on upgrade. A host adding an admin section or an MCP tool
/// needs its codename to reach a role, and reimplementing this means
/// duplicating the role lookup and the already-present check — the two
/// halves that make re-running it safe.
///
/// No-op when the role doesn't exist, so a host can grant to roles it
/// doesn't own without checking first.
///
/// # Errors
/// Driver / query failures.
pub async fn grant_codename(
    pool: &rustango::sql::Pool,
    role_name: &str,
    codename: &str,
) -> Result<(), ExecError> {
    backfill_codename(pool, role_name, codename).await
}

/// The built-in MCP skill set (#587): codename, label, description,
/// prompt instructions, tools, and the permission codenames (any-of)
/// that entitle a user's key to the skill.
type McpSkillSpec = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
);

const MCP_SKILLS: &[McpSkillSpec] = &[
    (
        "cms-read",
        "CMS read",
        "Browse page types, pages, media, snippets, locales, and translatable fields.",
        "You are working inside this site's CMS. Discover structure with \
         list_page_types before writing; read existing content before \
         changing it.",
        &[
            "list_page_types",
            "search_pages",
            "get_page",
            "list_locales",
            "list_media",
            "list_collections",
            "list_snippets",
            "list_translatable_fields",
        ],
        &["cms_page.view"],
    ),
    (
        "cms-write",
        "CMS write",
        "Create and edit pages, upload media, put media onto a page, and \
         manage snippets.",
        "Create drafts unless the user explicitly asks to publish. Reuse \
         existing media via list_media before uploading duplicates. \
         Uploading a file only files it in the library — call attach_media \
         to actually put it on a page, either into a chooser field or as an \
         image block in the body (list_page_types names both).",
        &[
            "create_page",
            "update_page",
            "upload_media",
            "attach_media",
            "upsert_snippet",
        ],
        &["cms_page.edit", "cms_page.add"],
    ),
    (
        "cms-publish",
        "CMS publish",
        "Publish pages to the live site.",
        "Publishing makes content live immediately — confirm intent before \
         calling publish_page.",
        &["publish_page"],
        &["cms_page.publish"],
    ),
    (
        "cms-templates",
        "CMS templates",
        "Read, create and modify this site's templates, and choose which \
         template a page type renders with.",
        "Templates are Tera. Read the inherited body before customising a \
         template — editing one creates a copy for this site and leaves the \
         shared version alone. validate_template checks a body without \
         writing it; prefer it while iterating. There is deliberately no \
         delete tool: reverting a customisation is left to a person in the \
         admin, so if a template should stop doing something, rewrite it.",
        &[
            "list_templates",
            "read_template",
            "write_template",
            "validate_template",
            "set_page_type_template",
        ],
        &[crate::admin::TEMPLATE_EDIT_CODENAME],
    ),
    (
        "cms-translate",
        "CMS translate",
        "Write translation overrides for pages in any non-default locale.",
        "Use list_translatable_fields to get the exact field paths and \
         canonical text; translate faithfully and keep markup intact.",
        &["upsert_translations"],
        &["cms_page.edit"],
    ),
];

/// Seed (or reconcile) the built-in MCP skills for one tenant. Idempotent:
/// `create_skill_pool` is create-only, so on `Duplicate` we reconcile by
/// adding any tool rows a newer CMS version introduced (never removing —
/// operator customizations survive); the skill↔permission mappings are
/// idempotent upserts.
pub async fn ensure_mcp_skills(pool: &rustango::sql::Pool) -> Result<(), rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    use rustango::tenancy::{AgentSkill, AgentSkillTool};

    for (codename, name, description, instructions, tools, perms) in MCP_SKILLS {
        let tool_names: Vec<String> = tools.iter().map(|t| (*t).to_owned()).collect();
        match rustango::tenancy::create_skill_pool(
            pool,
            codename,
            name,
            description,
            instructions,
            &tool_names,
        )
        .await
        {
            Ok(_) => {}
            Err(rustango::tenancy::AgentError::Duplicate(_)) => {
                // Reconcile: insert tool rows this CMS version added.
                let skill: Option<AgentSkill> = AgentSkill::objects()
                    .where_(AgentSkill::codename.eq((*codename).to_owned()))
                    .first(pool)
                    .await?;
                let Some(skill) = skill else { continue };
                let skill_id = skill.id.get().copied().unwrap_or_default();
                let existing: Vec<AgentSkillTool> = AgentSkillTool::objects()
                    .where_(AgentSkillTool::skill_id.eq(skill_id))
                    .fetch(pool)
                    .await?;
                for tool in *tools {
                    if !existing.iter().any(|t| t.tool_name == *tool) {
                        let mut row = AgentSkillTool {
                            id: rustango::sql::Auto::Unset,
                            skill_id,
                            tool_name: (*tool).to_owned(),
                        };
                        row.insert_pool(pool).await?;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(skill = codename, error = %e, "mcp skill create failed");
                continue;
            }
        }
        for perm in *perms {
            if let Err(e) =
                rustango::tenancy::map_skill_to_permission_pool(pool, codename, perm).await
            {
                tracing::warn!(skill = codename, perm, error = %e, "skill-permission map failed");
            }
        }
    }
    Ok(())
}

/// Seed three baseline roles per tenant so the permissions matrix is
/// usable without an admin having to hand-craft codenames (#33):
///
/// - **Viewer** — `view` on every resource except Users + Roles.
/// - **Editor** — view/add/edit on content + media + library +
///   redirects + navigation; view-only on locales / page types /
///   settings / history; no Users/Roles access.
/// - **Administrator** — every codename in the matrix.
///
/// Skipped per-role when a row with the same name exists, so
/// admins can rename / retune the codename set without it being
/// overwritten on the next boot.
async fn ensure_default_roles(pool: &rustango::sql::Pool) -> Result<(), ExecError> {
    use crate::admin::resources::{all_resources, codename};
    use crate::permissions::Action;

    let resources = all_resources();

    // Helper: codenames matching a predicate on `(resource, action)`.
    //
    // Deduplicated, because two resources may legitimately share a
    // `codename_prefix` — an app can point several admin pages at one
    // permission, as a host app does to give its database and API
    // connection screens a single verb. Each is
    // its own row in the matrix, so `codename()` yields the same string
    // twice, and `(role_id, codename)` is UNIQUE.
    let collect = |pred: &dyn Fn(&crate::admin::resources::AdminResource, Action) -> bool| {
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in &resources {
            for action in r.actions {
                if pred(r, *action) {
                    let code = codename(r, *action);
                    if seen.insert(code.clone()) {
                        out.push(code);
                    }
                }
            }
        }
        out
    };

    let viewer_codes = collect(&|r, a| {
        // Allow viewing every CRUD resource, plus the `cms_admin`
        // access verb so they can actually open the admin. The
        // `auth.access_admin` framework-admin verb stays off —
        // Viewers shouldn't see the raw-DB-CRUD framework admin.
        (a == Action::View
            && r.codename_prefix != "rustango_users"
            && r.codename_prefix != "rustango_roles")
            || (a == Action::Access && r.codename_prefix == "cms_admin")
    });
    let editor_codes = collect(&|r, a| {
        // No Users/Roles access at all.
        if r.codename_prefix == "rustango_users" || r.codename_prefix == "rustango_roles" {
            return false;
        }
        if r.codename_prefix == "cms_admin" {
            return a == Action::Access;
        }
        // Editors don't get framework-admin access either — content
        // workflow only.
        if r.codename_prefix == "auth" {
            return false;
        }
        match r.codename_prefix.as_str() {
            // Always view-only here even if more actions are declared.
            "cms_locale" | "cms_page_type" | "cms_history" => a == Action::View,
            // Settings: view + edit.
            "cms_settings" => matches!(a, Action::View | Action::Edit),
            // Default: view/add/edit but no delete/publish on content.
            _ => matches!(a, Action::View | Action::Add | Action::Edit),
        }
    });
    // Administrator grants every codename across every section. With
    // the `auth.access_admin` row now in the matrix (#10), this
    // includes framework-admin access — Administrators get to do
    // raw-DB-CRUD via /admin/ + CMS workflow via /cms-admin/.
    let admin_codes = collect(&|_r, _a| true);

    seed_role_with_codenames(
        pool,
        "Viewer",
        "Read-only access across the admin.",
        &viewer_codes,
    )
    .await?;
    seed_role_with_codenames(
        pool,
        "Editor",
        "Create / edit content, media, redirects, navigation. No access to users or roles.",
        &editor_codes,
    )
    .await?;
    seed_role_with_codenames(
        pool,
        "Administrator",
        "Full access across every admin section.",
        &admin_codes,
    )
    .await?;
    // #559/#561 — Developer role: content modelling. Everything a Viewer
    // sees, plus the page-type field builder (`cms_page_type.build`, a
    // free-form codename the matrix preserves as legacy — it hard-gates
    // the builder sub-router server-side).
    let mut dev_codes = viewer_codes.clone();
    dev_codes.push("cms_page_type.build".to_owned());
    // Per-tenant template overrides. Same reasoning as the builder: it is
    // content *modelling*, not content, so Developer gets it and the
    // content roles do not.
    dev_codes.push(crate::admin::TEMPLATE_EDIT_CODENAME.to_owned());
    dev_codes.push(crate::admin::NOTIFICATION_MANAGE_CODENAME.to_owned());
    // Binding a hostname changes what the public internet resolves to —
    // deployment work, so it sits with the Developer/Administrator pair
    // rather than with the content roles.
    dev_codes.push(crate::admin::SITE_MANAGE_CODENAME.to_owned());
    seed_role_with_codenames(
        pool,
        "Developer",
        "Content modelling: build page-type field schemas + reusable components.",
        &dev_codes,
    )
    .await?;
    // Backfill (#35): when a role already exists from a previous seed
    // run, idempotently add the `cms_admin.access` codename so users
    // assigned to the seeded roles aren't suddenly locked out after
    // upgrading to a build that enforces the gate.
    backfill_codename(pool, "Viewer", "cms_admin.access").await?;
    backfill_codename(pool, "Editor", "cms_admin.access").await?;
    backfill_codename(pool, "Administrator", "cms_admin.access").await?;
    // Backfill (#10): give Administrator the framework-admin access
    // codename `auth.access_admin` (rustango#311 enforces this on
    // `/__admin/` + `/admin/`). Viewers + Editors stay out of the
    // raw-DB-CRUD framework admin by design.
    backfill_codename(pool, "Administrator", "auth.access_admin").await?;
    // #561 — existing installs: give Developer + Administrator the
    // builder codename on upgrade.
    backfill_codename(pool, "Developer", "cms_admin.access").await?;
    backfill_codename(pool, "Developer", "cms_page_type.build").await?;
    backfill_codename(pool, "Administrator", "cms_page_type.build").await?;
    // Template editor: same two roles, so an upgrade doesn't leave the
    // section visible to nobody.
    backfill_codename(pool, "Developer", crate::admin::TEMPLATE_EDIT_CODENAME).await?;
    backfill_codename(pool, "Administrator", crate::admin::TEMPLATE_EDIT_CODENAME).await?;
    // Existing sites get it too — a permission that only reaches tenants
    // created after the release is a permission most sites never have.
    backfill_codename(pool, "Developer", crate::admin::NOTIFICATION_MANAGE_CODENAME).await?;
    backfill_codename(pool, "Administrator", crate::admin::NOTIFICATION_MANAGE_CODENAME).await?;
    backfill_codename(pool, "Developer", crate::admin::SITE_MANAGE_CODENAME).await?;
    backfill_codename(pool, "Administrator", crate::admin::SITE_MANAGE_CODENAME).await?;
    Ok(())
}

/// Add `codename` to the role named `role_name` if missing. No-op
/// when the role doesn't exist or the codename is already present.
async fn backfill_codename(
    pool: &rustango::sql::Pool,
    role_name: &str,
    codename: &str,
) -> Result<(), ExecError> {
    use rustango::core::Column as _;
    let role: Option<rustango::tenancy::permissions::Role> =
        rustango::tenancy::permissions::Role::objects()
            .where_(rustango::tenancy::permissions::Role::name.eq(role_name.to_owned()))
            .first(pool)
            .await?;
    let Some(role) = role else {
        return Ok(());
    };
    let role_id = role.id.get().copied().unwrap_or_default();
    let existing: Vec<rustango::tenancy::permissions::RolePermission> =
        rustango::tenancy::permissions::RolePermission::objects()
            .where_(rustango::tenancy::permissions::RolePermission::role_id.eq(role_id))
            .where_(
                rustango::tenancy::permissions::RolePermission::codename.eq(codename.to_owned()),
            )
            .fetch(pool)
            .await?;
    if !existing.is_empty() {
        return Ok(());
    }
    let mut perm = rustango::tenancy::permissions::RolePermission {
        id: Auto::Unset,
        role_id,
        codename: codename.to_owned(),
    };
    perm.insert_pool(pool).await?;
    Ok(())
}

async fn seed_role_with_codenames(
    pool: &rustango::sql::Pool,
    name: &str,
    description: &str,
    codenames: &[String],
) -> Result<(), ExecError> {
    use rustango::core::Column as _;
    let existing: Vec<rustango::tenancy::permissions::Role> =
        rustango::tenancy::permissions::Role::objects()
            .where_(rustango::tenancy::permissions::Role::name.eq(name.to_owned()))
            .fetch(pool)
            .await?;
    if existing.into_iter().next().is_some() {
        return Ok(());
    }
    let mut row = rustango::tenancy::permissions::Role {
        id: Auto::Unset,
        name: name.to_owned(),
        description: description.to_owned(),
        data: serde_json::Value::Object(serde_json::Map::new()),
    };
    row.insert_pool(pool).await?;
    let role_id = row.id.get().copied().unwrap_or_default();
    // A role's permissions are a set. Callers assemble these lists by
    // walking the resource matrix, where one codename can be reached by
    // more than one route, and `(role_id, codename)` is UNIQUE — so a
    // repeat is a constraint violation that aborts the whole seed, and
    // with it the boot. Enforce the set here rather than trusting every
    // caller to have done it.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for codename in codenames {
        if !seen.insert(codename.as_str()) {
            continue;
        }
        let mut perm = rustango::tenancy::permissions::RolePermission {
            id: Auto::Unset,
            role_id,
            codename: codename.clone(),
        };
        perm.insert_pool(pool).await?;
    }
    Ok(())
}

/// Seed a single "Root" collection if the tenant has no
/// `cms_media_collection` rows yet. New uploads default to it; the
/// admin shows orphan media (NULL `collection_id`) under it too.
/// Seed a default `en` locale when the tenant has none, so the
/// "exactly one is_default" invariant holds from first boot (#452).
/// Idempotent: no-op once any locale row exists.
async fn ensure_default_locale(pool: &rustango::sql::Pool) -> Result<(), ExecError> {
    let existing: Vec<crate::locale::Locale> = crate::locale::Locale::objects().fetch(pool).await?;
    if !existing.is_empty() {
        return Ok(());
    }
    let mut row = crate::locale::Locale {
        id: Auto::Unset,
        code: "en".to_owned(),
        name: "English".to_owned(),
        is_default: true,
        active: true,
        sort_order: 0,
        created_at: Auto::Unset,
    };
    row.insert_pool(pool).await?;
    Ok(())
}

async fn ensure_root_media_collection(pool: &rustango::sql::Pool) -> Result<(), ExecError> {
    let existing: Vec<crate::media::MediaCollection> =
        crate::media::MediaCollection::objects().fetch(pool).await?;
    if !existing.is_empty() {
        return Ok(());
    }
    let mut row = crate::media::MediaCollection {
        id: Auto::Unset,
        name: "Root".to_owned(),
        parent_id: None,
        sort_order: 0,
        created_at: Auto::Unset,
    };
    row.insert_pool(pool).await?;
    Ok(())
}

/// Upsert a single `cms_page_type` row keyed by `type_name`.
///
/// Find-or-create rather than ON CONFLICT because the macro-emitted
/// `upsert_on()` would conflict on the surrogate primary key — we
/// want resolution by the `type_name` natural-key column.
async fn upsert_page_type(
    pool: &rustango::sql::Pool,
    handler: &dyn PageTypeHandler,
) -> Result<(), ExecError> {
    let parents_json = serde_json::to_value(handler.allowed_parent_types())
        .unwrap_or(serde_json::Value::Array(Vec::new()));
    let children_json = serde_json::to_value(handler.allowed_child_types())
        .unwrap_or(serde_json::Value::Array(Vec::new()));

    let mut existing: Vec<PageType> = PageType::objects()
        .where_(PageType::type_name.eq(handler.type_name().to_string()))
        .fetch(pool)
        .await?;

    if let Some(mut row) = existing.pop() {
        row.app_label = handler.app_label().to_owned();
        row.verbose_name = handler.verbose_name().to_owned();
        row.default_template = handler.default_template().to_owned();
        row.view_mode = handler.view_mode().as_str().to_owned();
        row.is_creatable = handler.is_creatable();
        row.allowed_parent_types = parents_json;
        row.allowed_child_types = children_json;
        row.workflow = handler.workflow_slug().unwrap_or_default().to_owned();
        row.save_pool(pool).await?;
    } else {
        let mut row = PageType {
            id: Auto::Unset,
            app_label: handler.app_label().to_owned(),
            type_name: handler.type_name().to_owned(),
            verbose_name: handler.verbose_name().to_owned(),
            default_template: handler.default_template().to_owned(),
            view_mode: handler.view_mode().as_str().to_owned(),
            is_creatable: handler.is_creatable(),
            allowed_parent_types: parents_json,
            allowed_child_types: children_json,
            workflow: handler.workflow_slug().unwrap_or_default().to_owned(),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        row.insert_pool(pool).await?;
    }
    Ok(())
}

/// Bring every form's managed email target up to date with its settings.
///
/// # Errors
/// Driver / query failures.
async fn reconcile_form_notification_targets(
    pool: &rustango::sql::Pool,
) -> Result<(), rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let forms: Vec<crate::snippet::Snippet> = crate::snippet::Snippet::objects()
        .where_(crate::snippet::Snippet::type_name.eq("form".to_owned()))
        .fetch(pool)
        .await?;
    for f in forms {
        let id = f.id.get().copied().unwrap_or_default();
        let parsed = crate::forms::schema::parse_draft(&f.data);
        crate::notify::targets::sync_form_email(
            pool,
            id,
            &f.title,
            &parsed.settings.notify_emails,
        )
        .await?;
    }
    Ok(())
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    // `Column` (for `.eq`) and `FetcherPool` (for `.fetch` / `.first`)
    // arrive through the glob — the module already imports both.
    use super::*;
    use rustango::core::Model as _;
    use rustango::sql::Pool;
    use rustango::tenancy::permissions::{Role, RolePermission};

    async fn mem_pool() -> Pool {
        let pool = Pool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        for schema in [&Role::SCHEMA, &RolePermission::SCHEMA] {
            let ddl = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
                pool.dialect(),
                schema,
            );
            for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                rustango::sql::raw_execute_pool(&pool, stmt, Vec::new())
                    .await
                    .expect("ddl");
            }
        }
        pool
    }

    /// A repeated codename must not reach the database twice.
    ///
    /// The lists these roles are seeded from are assembled by walking the
    /// admin resource matrix, and one codename can be reached by more
    /// than one route: an app may point several admin pages at a single
    /// permission, as a host app does to give its database and API
    /// connection screens one verb.
    /// `(role_id, codename)` is UNIQUE, so before this was deduplicated
    /// the second insert aborted the seed — and on a fresh tenant that
    /// meant the application would not finish booting.
    #[tokio::test]
    async fn a_repeated_codename_is_granted_once() {
        let pool = mem_pool().await;

        seed_role_with_codenames(
            &pool,
            "Administrator",
            "every codename",
            &[
                "rpt_connection.view".to_owned(),
                "cms_page.view".to_owned(),
                // The same permission, reached through a second admin page.
                "rpt_connection.view".to_owned(),
            ],
        )
        .await
        .expect("seeding a role must tolerate a repeated codename");

        let role: Role = Role::objects()
            .where_(Role::name.eq("Administrator".to_owned()))
            .first(&pool)
            .await
            .expect("query")
            .expect("the role was created");
        let role_id = role.id.get().copied().unwrap_or_default();

        let granted: Vec<RolePermission> = RolePermission::objects()
            .where_(RolePermission::role_id.eq(role_id))
            .fetch(&pool)
            .await
            .expect("query");

        assert_eq!(granted.len(), 2, "one row per distinct codename");
        let mut codes: Vec<&str> = granted.iter().map(|p| p.codename.as_str()).collect();
        codes.sort_unstable();
        assert_eq!(codes, ["cms_page.view", "rpt_connection.view"]);
    }
}
