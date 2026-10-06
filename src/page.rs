//! [`Page`] — the tenant-scoped `cms_page` row every page shares,
//! whatever its type: tree position (`path`, `depth`, `parent_id`),
//! slug and `url_path`, [`PageStatus`] lifecycle, SEO fields and the
//! `page_type_id` that selects its [`PageTypeHandler`](crate::PageTypeHandler).
//! Type-specific fields live in the handler's extension table.

use rustango::sql::Auto;
use crate::log_err::LogErr as _;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// Lifecycle state of a page. Stored as a short string column so it survives
/// migrations cleanly and is grep-able in the DB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageStatus {
    Draft,
    Scheduled,
    Published,
    /// Superseded but preserved: served publicly at 200, stays searchable
    /// and in the sitemap (capped priority), frozen against unpublish/
    /// delete. Archiving prevents content from becoming *unavailable* —
    /// it is a preservation lock, not a step toward deletion (#556).
    Archived,
    /// Terminal takedown state reached when `expire_at` passes (#556,
    /// option a). Unlike [`Self::Archived`] it is NOT served — it 404s
    /// exactly as an expired page did before archived became serve-able.
    /// Sweep-set only; not selectable in the editor.
    Expired,
}

impl PageStatus {
    /// Every status, for parsing and choice lists.
    pub const ALL: [Self; 5] = [
        Self::Draft,
        Self::Scheduled,
        Self::Published,
        Self::Archived,
        Self::Expired,
    ];

    /// The statuses [`is_public`](Self::is_public) accepts.
    pub const PUBLIC: [Self; 2] = [Self::Published, Self::Archived];

    /// [`Self::PUBLIC`] as owned strings, for an `is_in` filter on
    /// `Page::status`.
    #[must_use]
    pub fn public_strings() -> [String; 2] {
        Self::PUBLIC.map(|s| s.as_str().to_owned())
    }

    /// The status a stored or submitted string names; `None` for any
    /// other string.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Scheduled => "scheduled",
            Self::Published => "published",
            Self::Archived => "archived",
            Self::Expired => "expired",
        }
    }

    /// Statuses served to anonymous visitors (200). `archived` joins
    /// `published` (#556); `expired` stays excluded so takedown still
    /// 404s.
    #[must_use]
    pub fn is_public(self) -> bool {
        matches!(self, Self::Published | Self::Archived)
    }

    /// [`is_public`](Self::is_public) for a status read off a row.
    ///
    /// `Page::status` is a `String`, so every caller was comparing
    /// against two `as_str()` literals by hand — and the one that
    /// compared against `Published` alone (`api::pages::find`) silently
    /// disagreed with the rest of the API about archived pages. One
    /// place to be wrong is better than four.
    #[must_use]
    pub fn str_is_public(status: &str) -> bool {
        status == Self::Published.as_str() || status == Self::Archived.as_str()
    }

    /// Whether a save moving a page from `from` to `to` puts it live —
    /// now, or on a schedule the sweep will publish. That takes the
    /// publish right whichever status gets there: archived is served like
    /// published, so gating on the `published` literal alone let an
    /// edit-only user make a draft public (#761).
    #[must_use]
    pub fn str_goes_live(from: &str, to: &str) -> bool {
        from != to && (Self::str_is_public(to) || to == Self::Scheduled.as_str())
    }
}

/// The abstract page record. Each row points at a `cms_page_type` registry
/// row via `page_type_id`; per-type extension data lives in a separate
/// table authored by user code (Wagtail/Django multi-table inheritance).
///
/// Tree shape: `path` is a materialized path (e.g. `0001/0003/`), `depth` is
/// the number of segments, `parent_id` is the immediate parent (nullable for
/// roots). Siblings share `parent_id` and order by `sort_order` then `id`.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_page",
    app = "cms",
    display = "title",
    admin(
        list_display = "title, slug, page_type_id, status, depth, published_at",
        search_fields = "title, slug",
        ordering = "path",
        list_filter = "page_type_id, status",
    )
)]
pub struct Page {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// FK to `cms_page_type.id` — the registry row that names the
    /// handler responsible for this page.
    #[rustango(fk = "cms_page_type", on = "id", index)]
    pub page_type_id: i64,

    #[rustango(max_length = 255)]
    pub title: String,

    /// URL slug, meant to be unique per parent. Not enforced by a DB
    /// constraint; page create / copy / alias dedup against siblings.
    #[rustango(max_length = 255)]
    pub slug: String,

    /// Materialized path of zero-padded segment ids, trailing slash.
    /// Example: `0001/0003/0008/`. Indexed for prefix lookups.
    #[rustango(max_length = 510, index)]
    pub path: String,

    /// Materialized public URL path, indexed for
    /// single-query lookup. Leading slash, no trailing slash for
    /// non-root pages: `/`, `/about`, `/about/team`. Maintained by
    /// `tree_ops` whenever a page is created or its slug / parent
    /// changes. The resolver short-circuits via this column instead
    /// of walking ancestor-by-ancestor (Wagtail's `Page.route()`
    /// pattern), turning N queries into 1.
    ///
    /// Default `''` so AddColumn on existing tables doesn't need a
    /// backfill — pre-Slice-1 rows fall through to the slug-walk
    /// resolver until their next save populates the column.
    #[rustango(max_length = 510, index, default = "''")]
    pub url_path: String,

    /// Cached number of `/`-separated segments in `path` (root = 1).
    pub depth: i32,

    /// Self-FK. Null means this is a root page.
    #[rustango(fk = "self", on = "id")]
    pub parent_id: Option<i64>,

    /// **DEPRECATED (#275)** — the structural-variant escape hatch was
    /// retired in favor of per-field translations. The supported path
    /// for localizing a page is the `cms_translation` table (one row
    /// per `(page_id, locale_id, field_path)` override); see
    /// [`crate::translation`].
    ///
    /// The column itself is retained so existing tenant rows that
    /// already carried a variant pointer don't 500 the loader. Sitemap
    /// + feed builders still dedup on this column for back-compat;
    /// `api/pages.rs` still filters by it for legacy API consumers.
    /// No new rows should set this field — the admin UI no longer
    /// surfaces the action and the POST handler is gone.
    #[rustango(fk = "self", on = "id")]
    pub locale_variant_of: Option<i64>,

    /// #75 — live alias. When set, this row contributes only the
    /// URL (slug / url_path / sort_order / parent_id); content
    /// fields (title, seo_*, status, page-type extension data) come
    /// from the row this FK points at. Edits via the page editor
    /// proxy to the source; the alias row stays read-only. Deleting
    /// the source demotes every alias into a standalone copy with
    /// the source's last-known content frozen in place.
    ///
    /// Differentiation from neighbouring features:
    ///   * Clone (#29) is a deep copy — divergent edits, separate
    ///     revisions, no link back to the original.
    ///   * Redirect (#64) is a 301 from old path to new path with
    ///     no separate page row at all.
    ///   * Alias is one row, two (or more) URLs, content always in
    ///     sync because there's only one source-of-truth.
    #[rustango(fk = "self", on = "id")]
    pub alias_of: Option<i64>,

    /// Per-page visual theme override. When set, this page (and its
    /// descendants, unless they override too) renders against the
    /// referenced `cms_theme` row's design tokens. NULL = inherit
    /// from ancestors / site default / admin default.
    #[rustango(fk = "cms_theme", on = "id")]
    pub theme_id: Option<i64>,

    /// Ordering among siblings (lowest first).
    pub sort_order: i32,

    /// Lifecycle. Stored as the `PageStatus::as_str()` value.
    #[rustango(max_length = 16, index)]
    pub status: String,

    /// Set when status transitions to `Published`. Null otherwise.
    ///
    /// Wagtail-parity note (#251): semantically this is
    /// `first_published_at` — the renderer + sweep set it once on
    /// the first published transition and never touch it again, so
    /// `order_by('-published_at')` is the canonical chronological
    /// post-card ordering. For "most-recently-edited" timestamps
    /// see [`Self::last_published_at`].
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,

    /// Refreshed every time the page is saved while `status =
    /// published`. Sitemap `<lastmod>` and edge-cache
    /// invalidation use this — `published_at` (the first-publish
    /// timestamp) is too stable for those.
    ///
    /// Initialized to the same value as `published_at` when the
    /// first-publish happens; subsequent edits bump just this
    /// field. Null for never-published rows.
    ///
    /// `#[serde(default)]` so revision snapshots captured before
    /// this column landed deserialize cleanly (the field arrives
    /// as `None`, which matches the column's nullable default).
    #[serde(default)]
    pub last_published_at: Option<chrono::DateTime<chrono::Utc>>,

    /// Optional schedule — when set with `status = scheduled`, a worker flips
    /// status to `published` once `now() >= go_live_at`.
    pub go_live_at: Option<chrono::DateTime<chrono::Utc>>,

    /// Opposite of `go_live_at` — auto-archive at this time.
    pub expire_at: Option<chrono::DateTime<chrono::Utc>>,

    /// SEO metadata.
    #[rustango(max_length = 70)]
    pub seo_title: String,
    #[rustango(max_length = 200)]
    pub seo_description: String,

    /// Whether search engines should index this page. `false` makes
    /// the renderer emit `<meta name="robots" content="noindex">`
    /// and excludes the page from the generated [`/sitemap.xml`].
    /// Default `true`.
    ///
    /// [`/sitemap.xml`]: crate::sitemap
    #[rustango(default = "true")]
    pub robots_index: bool,

    /// Sitemap priority hint, 0.0–1.0 per the sitemaps.org spec.
    /// 1.0 = homepage / top of hierarchy; 0.5 = ordinary content
    /// page (the spec's default); 0.0–0.3 = low-value pages
    /// (terms, archive listings). Communicated to crawlers via the
    /// generated [`/sitemap.xml`]'s `<priority>` element.
    ///
    /// [`/sitemap.xml`]: crate::sitemap
    #[rustango(default = "0.5")]
    pub sitemap_priority: f32,

    /// Where this page lives on a **decoupled frontend**, when that
    /// differs from `url_path`.
    ///
    /// The site-wide "Headless preview" template substitutes `{path}`
    /// with `url_path`, which silently assumes the frontend mirrors the
    /// CMS's URL structure. Plenty of them do not: a page at
    /// `/features/ai-engine` here may be `/product/ai-engine` there, or
    /// `/p/9`, or on another host entirely.
    ///
    /// Three behaviours, chosen by what the editor types:
    ///
    /// * **empty** — use `url_path`, i.e. today's behaviour;
    /// * **a path** (`/product/ai-engine`) — substituted for `{path}`
    ///   in the site-wide template, so the token and host stay in one
    ///   place;
    /// * **an absolute URL** (`https://other.example.com/x`) — used as
    ///   the whole preview URL, placeholders and all, for the page that
    ///   a different app renders.
    ///
    /// Default `''` so adding the column needs no backfill: every
    /// existing page keeps resolving through `url_path`.
    ///
    /// Deliberately **not** carried by clone or alias: a copy has its own
    /// `url_path`, and inheriting the original's frontend route would
    /// point two pages at one place.
    #[rustango(max_length = 510, default = "''")]
    pub preview_path: String,

    /// Template this page renders through, overriding its page type's
    /// `default_template`.
    ///
    /// The type's template is the right default: pages of a type
    /// normally look alike, and that is what makes a type worth having.
    /// But a type is a *content* shape, and presentation does not always
    /// follow it — a set of pages under one root can want their own
    /// layout without becoming a separate type with a duplicate field
    /// list. Rather than making authors choose between a type per
    /// variation and no variation at all, the page names the template.
    ///
    /// Default `''` (use the type's template) so adding the column needs
    /// no backfill and every existing page renders exactly as before.
    ///
    /// The name resolves against the tenant's template set, so an
    /// override can point at a file the template editor (or the
    /// `write_template` MCP tool) created without a deploy. A name that
    /// resolves to nothing falls back to the type's template rather than
    /// 500ing — a typo should not take the page down.
    #[rustango(max_length = 255, default = "''")]
    pub template_override: String,

    /// Opt-in flag — the navigation-menu editor (#22) shows this page
    /// in its picker as a suggested entry. Editors flip it in the
    /// Promote tab; defaults to `false` so the suggestion list stays
    /// curated and doesn't surface every page in the tree.
    #[rustango(default = "false")]
    pub show_in_menus: bool,

    /// Social-sharing fields (#183, Wagtail parity). All optional;
    /// the renderer falls back to title / seo_description / first
    /// body image when empty.
    #[rustango(max_length = 120, default = "''")]
    pub og_title: String,
    #[rustango(max_length = 300, default = "''")]
    pub og_description: String,
    #[rustango(fk = "cms_media", on = "id")]
    pub og_image_media_id: Option<i64>,
    /// Twitter card variant. Accepted values: `summary`,
    /// `summary_large_image`. Default `summary_large_image`.
    #[rustango(max_length = 32, default = "'summary_large_image'")]
    pub twitter_card: String,

    /// Pre-publish reminder flag (#207). Set by
    /// [`run_schedule_sweep_with_mailer`] when the sweep sends a
    /// "publishes in N minutes" notification to subscribers; cleared
    /// when an editor moves the `go_live_at`. Prevents duplicate
    /// reminders if the sweep tick rate exceeds 1/hour.
    #[rustango(default = "false")]
    pub notification_pre_published_sent: bool,

    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<chrono::DateTime<chrono::Utc>>,
}

/// Outcome of one [`run_schedule_sweep`] tick.
#[derive(Debug, Clone, Default)]
pub struct ScheduleSweepResult {
    pub published: usize,
    /// #556 — pages taken down this tick (`published → expired` at
    /// `expire_at`). Named `expired` since archiving is now a *manual*,
    /// content-preserving action, distinct from expiry-as-takedown.
    pub expired: usize,
    /// #432 — `url_path`s of every page whose public visibility flipped
    /// this tick (published or expired). The caller purges these from
    /// the frontend cache (see [`run_schedule_sweep_and_purge`]).
    pub changed_urls: Vec<String>,
    /// #692 — the pages that went live this tick, as read before the flip.
    pub went_live: Vec<Page>,
    /// #692 — ids of the pages taken down this tick.
    pub taken_down: Vec<i64>,
}

/// What must happen once a page has gone live, whichever path put it there
/// (#692): the search index learns about it and the host's after-publish
/// hooks run. An editor's save, a bulk publish, a workflow finish and the
/// schedule sweep each did their own subset — the sweep did neither, so a
/// scheduled page never reached an external search index.
pub async fn went_live_effects(tenant_slug: &str, page: &Page) {
    crate::task_queue::sync_page_search(tenant_slug, page).await;
    crate::hooks::fire_after_publish_page(page);
}

/// [`went_live_effects`] for every page a sweep published, and the search
/// removal for every page it took down.
pub async fn apply_sweep_effects(tenant_slug: &str, result: &ScheduleSweepResult) {
    for page in &result.went_live {
        went_live_effects(tenant_slug, page).await;
    }
    for id in &result.taken_down {
        crate::task_queue::remove_page_search(tenant_slug, *id).await;
    }
}

/// Flip the status of any page whose `go_live_at` has passed (draft
/// or scheduled → published) and any page whose `expire_at` has
/// passed (published → **expired**, #556 — a terminal 404 takedown,
/// no longer conflated with the serve-able `archived` state).
/// Idempotent — running it twice in a row produces zero changes on
/// the second call.
///
/// Called opportunistically from the page-list handler so editors
/// always see current state. Production deployments should also wire
/// it into a cron / sweeper task so public traffic sees flipped
/// pages without an admin visit.
///
/// # Errors
/// Propagates driver / query failures. Successfully-flipped rows
/// before a later failure stay flipped (the function does one update
/// per row, not a tx — partial progress is safe and idempotent).
pub async fn run_schedule_sweep(
    pool: &rustango::sql::Pool,
) -> Result<ScheduleSweepResult, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let now = chrono::Utc::now();
    let mut published = 0usize;
    let mut expired = 0usize;
    let mut changed_urls: Vec<String> = Vec::new();
    let mut went_live: Vec<Page> = Vec::new();
    let mut taken_down: Vec<i64> = Vec::new();

    // Only the rows that are due, filtered in SQL — a sweep runs on a timer
    // and on every admin page list, so it must not load every published
    // page to find the few that expired (#680).
    //
    // Each flip is a conditional UPDATE of the status columns alone, keyed
    // on the status the row was read with. An editor who saved the page in
    // between keeps their edit, and a page moved meanwhile keeps its new
    // path — the old full-row save wrote the stale copy back over both.

    // 1) scheduled → published once go_live_at has passed. Drafts are
    //    intentionally NOT flipped — an editor opts in by picking
    //    "scheduled" alongside the go-live datetime.
    let due_publish: Vec<Page> = Page::objects()
        .where_(Page::status.eq(PageStatus::Scheduled.as_str().to_owned()))
        .where_(Page::go_live_at.lte(Some(now)))
        .fetch(pool)
        .await?;
    for p in due_publish {
        let mut set = vec![
            ("status", PageStatus::Published.as_str().into()),
            // #251 — every publish transition bumps last_published_at.
            ("last_published_at", now.into()),
        ];
        if p.published_at.is_none() {
            set.push(("published_at", now.into()));
        }
        if flip(pool, &p, PageStatus::Scheduled, set).await? {
            changed_urls.push(p.url_path.clone());
            published += 1;
            let mut live = p;
            live.status = PageStatus::Published.as_str().to_owned();
            went_live.push(live);
        }
    }

    // 2) published → expired once expire_at has passed (#556). Expiry is
    //    takedown: the page 404s afterward (the resolver serves only
    //    published + archived). Archiving is a separate, manual action.
    let due_expire: Vec<Page> = Page::objects()
        .where_(Page::status.eq(PageStatus::Published.as_str().to_owned()))
        .where_(Page::expire_at.lte(Some(now)))
        .fetch(pool)
        .await?;
    for p in due_expire {
        let set = vec![("status", PageStatus::Expired.as_str().into())];
        if flip(pool, &p, PageStatus::Published, set).await? {
            changed_urls.push(p.url_path.clone());
            expired += 1;
            taken_down.extend(p.id.get().copied());
        }
    }

    Ok(ScheduleSweepResult {
        published,
        expired,
        changed_urls,
        went_live,
        taken_down,
    })
}

/// Apply `set` to `page` only while it still has status `from`. Returns
/// whether this call flipped it (false when someone changed it first).
async fn flip(
    pool: &rustango::sql::Pool,
    page: &Page,
    from: PageStatus,
    set: Vec<(&'static str, rustango::core::SqlValue)>,
) -> Result<bool, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::UpdaterPool as _;
    let id = page.id.get().copied().unwrap_or_default();
    let mut update = Page::objects()
        .where_(Page::id.eq(id))
        .where_(Page::status.eq(from.as_str().to_owned()))
        .update();
    for (column, value) in set {
        update = update.set(column, value);
    }
    Ok(update.execute_pool(pool).await? == 1)
}

/// [`run_schedule_sweep`] + a frontend-cache purge of every page whose
/// visibility flipped this tick (#432). A scheduled page going live (or
/// an expired one going away) must evict its previously-cached
/// 404/draft/live entry; the bare sweep can't, since it has no cache
/// context. The purge routes through [`crate::task_queue::purge_urls`],
/// so it's queued + retried when a task sink is installed, inline
/// otherwise.
///
/// # Errors
/// Propagates the sweep's driver / query failures (the purge itself is
/// best-effort and never errors the call).
pub async fn run_schedule_sweep_and_purge(
    pool: &rustango::sql::Pool,
    tenant_slug: &str,
    invalidator: std::sync::Arc<dyn crate::cache_invalidate::PageCacheInvalidator>,
) -> Result<ScheduleSweepResult, rustango::sql::ExecError> {
    let result = run_schedule_sweep(pool).await?;
    if !result.changed_urls.is_empty() {
        crate::task_queue::purge_urls(
            invalidator,
            tenant_slug.to_owned(),
            result.changed_urls.clone(),
        )
        .await;
    }
    apply_sweep_effects(tenant_slug, &result).await;
    Ok(result)
}

/// Sweep with a mailer attached — adds a pre-publish reminder pass
/// (#207). Every page with `status = scheduled` and `go_live_at` in
/// (now, now + 1h] that hasn't already received the reminder gets a
/// notification to subscribers + the flag set so future ticks within
/// the same window don't duplicate.
///
/// The publish + archive passes are identical to
/// [`run_schedule_sweep`]; this variant just layers the reminder on
/// top so production deployments with a wired mailer get the
/// notification path for free.
///
/// # Errors
/// Driver / query failures.
pub async fn run_schedule_sweep_with_mailer(
    pool: &rustango::sql::Pool,
    mailer: Option<&dyn rustango::email::Mailer>,
    from: &str,
    tenant_slug: &str,
) -> Result<ScheduleSweepResult, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;

    // 1) Pre-publish reminder pass. Runs FIRST so a reminder is sent
    //    before the actual publish flip later in this same call.
    if let Some(m) = mailer {
        let now = chrono::Utc::now();
        let horizon = now + chrono::Duration::hours(1);
        let scheduled: Vec<Page> = Page::objects()
            .where_(Page::status.eq(PageStatus::Scheduled.as_str().to_owned()))
            .where_(Page::notification_pre_published_sent.eq(false))
            .fetch(pool)
            .await?;
        for mut p in scheduled {
            let Some(go_live) = p.go_live_at else {
                continue;
            };
            if go_live <= now || go_live > horizon {
                continue;
            }
            let page_id = p.id.get().copied().unwrap_or(0);
            // Best-effort fan-out; on failure leave the flag unset
            // so a future tick can retry (a retry that succeeds is
            // strictly better than a permanent miss).
            crate::page_subscription::notify_pre_publish(
                Some(m),
                from,
                pool,
                tenant_slug,
                page_id,
                &p.title,
                &p.url_path,
                go_live,
            )
            .await;
            p.notification_pre_published_sent = true;
            p.save_pool(pool)
                .await
                .log_warn("pre-publish notification flag not saved; it may be sent again");
        }
    }

    // 2) — delegate to the standard sweep for the publish + expiry
    //      flips. Idempotent.
    let result = run_schedule_sweep(pool).await?;
    apply_sweep_effects(tenant_slug, &result).await;
    Ok(result)
}

/// A planned archive / unarchive of a selection and (optionally) its
/// subtree (#556). Pure read output — the caller applies `to_flip`
/// inside its own transaction, then purges `changed_urls`.
#[derive(Debug, Default)]
pub struct ArchivePlan {
    /// Rows whose `status` must change, with the new status (and any
    /// republish stamps) already applied — save them as-is.
    pub to_flip: Vec<Page>,
    /// Every `url_path` whose *public treatment* changes — the selection
    /// plus every currently-public descendant (they inherit / lose the
    /// archived banner at render time even when their own row is
    /// untouched). The caller purges these from the frontend cache.
    pub changed_urls: Vec<String>,
}

/// Plan an archive (`archive = true`) or unarchive of the already-loaded
/// `selected` rows (#556).
///
/// - **Archive**: the selected pages flip to `archived`. Descendants
///   inherit the archived treatment at render time, so ALL currently-
///   public descendants are added to `changed_urls` for a cache purge;
///   when `cascade`, published descendants additionally flip their own
///   row to `archived` (so the subtree stays archived even if later
///   reparented). Drafts / scheduled / expired descendants are left
///   untouched — they're already hidden.
/// - **Unarchive**: archived selected pages flip back to `published`
///   (republish stamps refreshed); non-archived selections are skipped.
///   When `cascade`, archived descendants flip back to `published`.
///
/// Reads descendants per selected page; the caller applies the plan.
///
/// # Errors
/// Propagates descendant-query failures.
pub async fn plan_archive(
    pool: &rustango::sql::Pool,
    selected: &[Page],
    archive: bool,
    cascade: bool,
) -> Result<ArchivePlan, rustango::sql::ExecError> {
    let now = chrono::Utc::now();
    let archived = PageStatus::Archived.as_str();
    let published = PageStatus::Published.as_str();
    let republish = |p: &mut Page| {
        p.status = published.to_owned();
        if p.published_at.is_none() {
            p.published_at = Some(now);
        }
        p.last_published_at = Some(now);
    };

    let mut plan = ArchivePlan::default();
    for p in selected {
        if archive {
            if p.status != archived {
                let mut np = p.clone();
                np.status = archived.to_owned();
                plan.to_flip.push(np);
            }
            plan.changed_urls.push(p.url_path.clone());
        } else if p.status == archived {
            let mut np = p.clone();
            republish(&mut np);
            plan.to_flip.push(np);
            plan.changed_urls.push(p.url_path.clone());
        }

        // Descendants: fetch by materialized path (excludes self).
        // Best-effort — a failed subtree read degrades to "no cascade /
        // purge for this branch" rather than aborting the whole action.
        let descendants = p.descendants(pool).await.unwrap_or_default();
        for d in descendants {
            let is_public = d.status == published || d.status == archived;
            if archive {
                if cascade && d.status == published {
                    let mut nd = d.clone();
                    nd.status = archived.to_owned();
                    plan.to_flip.push(nd);
                }
            } else if cascade && d.status == archived {
                let mut nd = d.clone();
                republish(&mut nd);
                plan.to_flip.push(nd);
            }
            if is_public {
                plan.changed_urls.push(d.url_path.clone());
            }
        }
    }

    // A parent + child both selected (or a cascade overlap) can enqueue
    // the same row / URL twice — dedupe so the caller's writes + purge
    // each run once.
    plan.to_flip
        .sort_by_key(|p| p.id.get().copied().unwrap_or_default());
    plan.to_flip
        .dedup_by_key(|p| p.id.get().copied().unwrap_or_default());
    plan.changed_urls.sort();
    plan.changed_urls.dedup();
    Ok(plan)
}

#[cfg(all(test, feature = "sqlite"))]
mod archive_tests {
    use super::*;
    use rustango::core::{Column as _, Model as _};
    use rustango::sql::Pool;

    async fn mem_pool() -> Pool {
        let pool = Pool::connect("sqlite::memory:").await.expect("mem pool");
        for schema in [
            &crate::page_type_model::PageType::SCHEMA,
            &crate::media::Media::SCHEMA,
            &crate::theme::Theme::SCHEMA,
            &Page::SCHEMA,
        ] {
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
        let mut pt = crate::page_type_model::PageType {
            id: Auto::Unset,
            app_label: "cms".to_owned(),
            type_name: "TestPage".to_owned(),
            verbose_name: "Test page".to_owned(),
            default_template: "page.html".to_owned(),
            view_mode: "auto".to_owned(),
            is_creatable: true,
            allowed_parent_types: serde_json::json!([]),
            allowed_child_types: serde_json::json!([]),
            workflow: String::new(),
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        pt.insert_pool(&pool).await.expect("insert pt");
        pool
    }

    async fn mk_page(
        pool: &Pool,
        slug: &str,
        path: &str,
        parent_id: Option<i64>,
        status: &str,
        expire_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Page {
        let mut page = Page {
            id: Auto::Unset,
            page_type_id: 1,
            title: format!("Page {slug}"),
            slug: slug.to_owned(),
            path: path.to_owned(),
            url_path: format!("/{slug}"),
            preview_path: String::new(),
            template_override: String::new(),
            depth: path.matches('/').count() as i32,
            parent_id,
            locale_variant_of: None,
            alias_of: None,
            theme_id: None,
            sort_order: 0,
            status: status.to_owned(),
            published_at: Some(chrono::Utc::now()),
            last_published_at: Some(chrono::Utc::now()),
            go_live_at: None,
            expire_at,
            seo_title: String::new(),
            seo_description: String::new(),
            robots_index: true,
            sitemap_priority: 0.5,
            show_in_menus: false,
            og_title: String::new(),
            og_description: String::new(),
            og_image_media_id: None,
            twitter_card: "summary_large_image".to_owned(),
            notification_pre_published_sent: false,
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        };
        page.insert_pool(pool).await.expect("insert page");
        page
    }

    async fn status_of(pool: &Pool, id: i64) -> String {
        Page::objects()
            .where_(Page::id.eq(id))
            .first(pool)
            .await
            .expect("fetch")
            .expect("row")
            .status
    }

    #[test]
    fn parse_round_trips_every_status_and_rejects_others() {
        for s in PageStatus::ALL {
            assert_eq!(PageStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(PageStatus::parse("Published"), None);
        assert_eq!(PageStatus::parse(""), None);
        assert_eq!(PageStatus::public_strings(), ["published", "archived"]);
        assert!(PageStatus::PUBLIC.iter().all(|s| s.is_public()));
    }

    #[test]
    fn going_live_is_any_change_into_a_public_or_scheduled_status() {
        let goes = PageStatus::str_goes_live;
        assert!(goes("draft", "published"));
        assert!(goes("draft", "archived"), "archived is served (#761)");
        assert!(goes("draft", "scheduled"), "the sweep publishes it");
        assert!(goes("published", "archived"));
        assert!(!goes("draft", "draft"));
        assert!(!goes("published", "published"), "re-saving a live page");
        assert!(!goes("published", "draft"), "unpublishing is not going live");
    }

    #[test]
    fn is_public_covers_published_and_archived_only() {
        assert!(PageStatus::Published.is_public());
        assert!(PageStatus::Archived.is_public());
        assert!(!PageStatus::Draft.is_public());
        assert!(!PageStatus::Scheduled.is_public());
        assert!(!PageStatus::Expired.is_public());
    }

    #[test]
    fn str_is_public_agrees_with_the_typed_form() {
        // The string form is what rows carry, and it drifting from the
        // typed one is exactly how `find` came to 404 an archived page
        // the rest of the API served.
        for st in [
            PageStatus::Draft,
            PageStatus::Scheduled,
            PageStatus::Published,
            PageStatus::Archived,
            PageStatus::Expired,
        ] {
            assert_eq!(
                PageStatus::str_is_public(st.as_str()),
                st.is_public(),
                "{}", st.as_str(),
            );
        }
        assert!(!PageStatus::str_is_public("nonsense"));
    }

    #[tokio::test]
    async fn sweep_expires_not_archives_past_expire_at() {
        let pool = mem_pool().await;
        let past = chrono::Utc::now() - chrono::Duration::hours(1);
        let p = mk_page(&pool, "gone", "0001/", None, "published", Some(past)).await;
        let id = p.id.get().copied().unwrap();
        let res = run_schedule_sweep(&pool).await.expect("sweep");
        assert_eq!(res.expired, 1, "one page taken down");
        assert_eq!(
            status_of(&pool, id).await,
            PageStatus::Expired.as_str(),
            "expire_at flips to expired (takedown), NOT archived (serve-able)"
        );
    }

    #[tokio::test]
    async fn plan_archive_marks_self_and_purges_subtree_without_cascade() {
        let pool = mem_pool().await;
        let parent = mk_page(&pool, "docs", "0001/", None, "published", None).await;
        let child = mk_page(
            &pool,
            "v1",
            "0001/0002/",
            parent.id.get().copied(),
            "published",
            None,
        )
        .await;
        let draft = mk_page(
            &pool,
            "wip",
            "0001/0003/",
            parent.id.get().copied(),
            "draft",
            None,
        )
        .await;

        // No cascade: only the parent's own row flips; the published child
        // is purged (inherits at render) but keeps its own status; the
        // draft is neither flipped nor purged (already hidden).
        let plan = plan_archive(&pool, std::slice::from_ref(&parent), true, false)
            .await
            .expect("plan");
        let flip_ids: Vec<i64> = plan
            .to_flip
            .iter()
            .map(|p| p.id.get().copied().unwrap())
            .collect();
        assert_eq!(
            flip_ids,
            vec![parent.id.get().copied().unwrap()],
            "only parent flips"
        );
        assert!(plan.changed_urls.contains(&"/docs".to_owned()));
        assert!(
            plan.changed_urls.contains(&"/v1".to_owned()),
            "published child purged"
        );
        assert!(
            !plan.changed_urls.contains(&"/wip".to_owned()),
            "draft child not purged"
        );
        let _ = (child, draft);
    }

    #[tokio::test]
    async fn plan_archive_cascade_flips_published_descendants() {
        let pool = mem_pool().await;
        let parent = mk_page(&pool, "docs", "0001/", None, "published", None).await;
        let child = mk_page(
            &pool,
            "v1",
            "0001/0002/",
            parent.id.get().copied(),
            "published",
            None,
        )
        .await;
        let draft = mk_page(
            &pool,
            "wip",
            "0001/0003/",
            parent.id.get().copied(),
            "draft",
            None,
        )
        .await;

        let plan = plan_archive(&pool, std::slice::from_ref(&parent), true, true)
            .await
            .expect("plan");
        let flip_ids: std::collections::HashSet<i64> = plan
            .to_flip
            .iter()
            .map(|p| p.id.get().copied().unwrap())
            .collect();
        assert!(flip_ids.contains(&parent.id.get().copied().unwrap()));
        assert!(
            flip_ids.contains(&child.id.get().copied().unwrap()),
            "published child flips under cascade"
        );
        assert!(
            !flip_ids.contains(&draft.id.get().copied().unwrap()),
            "draft child left untouched"
        );
        assert!(plan
            .to_flip
            .iter()
            .all(|p| p.status == PageStatus::Archived.as_str()));
    }

    #[tokio::test]
    async fn plan_unarchive_symmetric_revives_archived() {
        let pool = mem_pool().await;
        let parent = mk_page(&pool, "docs", "0001/", None, "archived", None).await;
        let child = mk_page(
            &pool,
            "v1",
            "0001/0002/",
            parent.id.get().copied(),
            "archived",
            None,
        )
        .await;

        // Non-archived selection is a no-op; archived selection revives.
        let plan = plan_archive(&pool, std::slice::from_ref(&parent), false, true)
            .await
            .expect("plan");
        assert!(plan
            .to_flip
            .iter()
            .all(|p| p.status == PageStatus::Published.as_str()));
        let ids: std::collections::HashSet<i64> = plan
            .to_flip
            .iter()
            .map(|p| p.id.get().copied().unwrap())
            .collect();
        assert!(ids.contains(&parent.id.get().copied().unwrap()));
        assert!(
            ids.contains(&child.id.get().copied().unwrap()),
            "cascade unarchive revives archived descendant"
        );
    }
}

/// Merge an alias row with its source into the page the site actually
/// serves.
///
/// An alias is a real, distinct URL whose *content* belongs to another
/// page. The row keeps its own identity — id, slug, path, url_path,
/// parent_id — and borrows everything readers see: title, SEO, type,
/// publication dates, theme, and the promote flags. The stored `title`
/// on an alias is a copy taken at creation and goes stale the moment the
/// source is renamed, which is exactly why nothing should read it.
///
/// Shared by the renderer and `GET /api/v2/pages/{id}/` so the two cannot
/// disagree. They did: the tree resolved alias titles from the source
/// while detail returned the stale copy with no `extension` and no
/// `builder`, which quietly falsified the "the editor's JSON preview is
/// the real response" promise for every alias.
#[must_use]
pub fn alias_hybrid(alias: &Page, source: &Page) -> Page {
    let mut hybrid = alias.clone();
    hybrid.title = source.title.clone();
    hybrid.page_type_id = source.page_type_id;
    hybrid.seo_title = source.seo_title.clone();
    hybrid.seo_description = source.seo_description.clone();
    hybrid.status = source.status.clone();
    hybrid.published_at = source.published_at;
    hybrid.last_published_at = source.last_published_at;
    hybrid.go_live_at = source.go_live_at;
    hybrid.expire_at = source.expire_at;
    hybrid.theme_id = source.theme_id;
    hybrid.robots_index = source.robots_index;
    hybrid.sitemap_priority = source.sitemap_priority;
    hybrid.show_in_menus = source.show_in_menus;
    hybrid
}
