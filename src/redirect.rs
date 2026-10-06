//! Editor-managed 301/302 redirects — Wagtail "Redirects" / Django
//! `contrib.redirects` shape.
//!
//! One row per redirect: `from_path` (the URL visitors hit) → `to_path`
//! (the canonical destination). `is_permanent = true` emits `301 Moved
//! Permanently`; `false` emits `302 Found`. The match is on the
//! request path **including trailing-slash variance** — `/old` and
//! `/old/` are distinct rows so admins can pick the right shape for
//! their case.
//!
//! The public router consults the table whenever a slug lookup fails
//! ([`crate::resolver::resolve_path`] returns `None`), mirroring
//! Django's "redirects framework hooks into the 404 handler" design.
//! Live page URLs take precedence — a redirect from `/about` is
//! ignored if a published page at `/about` exists.
//!
//! Per-request rendering uses [`rustango::redirects::build_redirect_response`]
//! so the `Location` header preserves any query string the visitor
//! arrived with (`/old?ref=ad → /new?ref=ad`).
//!
//! ## Migration
//!
//! Re-run `cargo run --example cms_demo -- makemigrations` after
//! pulling this module so the framework generates the JSON file for
//! `cms_redirect`. The Model derive registers the table via
//! `inventory`; without the migration file the runtime DB schema
//! stays unaware of the new table.

use chrono::{DateTime, Utc};
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// One redirect entry. Tenant-scoped — every row lives in the same
/// per-tenant `cms_redirect` table as the rest of the CMS data.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(
    table = "cms_redirect",
    app = "cms",
    display = "from_path",
    admin(
        list_display = "from_path, to_path, is_permanent, hit_count, updated_at",
        search_fields = "from_path, to_path",
        ordering = "from_path",
        list_filter = "is_permanent",
    )
)]
pub struct Redirect {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Incoming path with leading slash and no host (`/old-news`).
    /// Indexed + unique so the resolver lookup is one query and so
    /// admins can't accidentally create two rules for the same URL.
    #[rustango(max_length = 510, index, unique)]
    pub from_path: String,

    /// Destination URL. Usually a same-host path (`/news`) but absolute
    /// `https://...` URLs are accepted for cross-site moves; the
    /// framework's [`build_redirect_response`] copies the value
    /// verbatim into the `Location` header.
    ///
    /// [`build_redirect_response`]: rustango::redirects::build_redirect_response
    #[rustango(max_length = 1024)]
    pub to_path: String,

    /// `true` → 301 Moved Permanently (canonical URL change; search
    /// engines update their index). `false` → 302 Found (temporary
    /// move). Default `true` — most editor-driven redirects are
    /// canonical changes.
    #[rustango(default = "true")]
    pub is_permanent: bool,

    /// Optional editor note for the admin list view ("renamed for
    /// SEO", "old marketing campaign", …).
    #[rustango(max_length = 255, default = "''")]
    pub note: String,

    /// Bumped every time the public router serves this redirect.
    /// Useful for editors deciding which old URLs to keep around vs
    /// retire. Best-effort — concurrent updates may lose a count.
    #[rustango(default = "0")]
    pub hit_count: i64,

    /// #553 — serve this rule even when a published page exists at
    /// `from_path`. Default `false`: redirects normally fill in for
    /// retired URLs only (live pages win, Django/Wagtail parity).
    #[rustango(default = "false")]
    pub overrides_live: bool,

    /// #553 — source page whose `url_path` was snapshotted into
    /// `from_path` at save time. Soft reference (no FK constraint —
    /// the column is seed-added via [`ensure_columns`]); used to
    /// auto-disable the rule when the page is deleted/unpublished.
    pub from_page_id: Option<i64>,

    /// #553 — destination page. When set, the serve path resolves the
    /// page's **current** `url_path` (follows moves/renames — Wagtail
    /// `redirect_page` parity); `to_path` holds the save-time snapshot
    /// as display/fallback.
    pub to_page_id: Option<i64>,

    /// #553 — disabled rules never serve. Flipped off automatically
    /// (with [`Self::disabled_reason`]) when a linked page is deleted
    /// or unpublished; re-enabled from the admin list.
    #[rustango(default = "true")]
    pub is_active: bool,

    /// #553 — why `is_active` was switched off ("Linked page … was
    /// deleted"). Empty while active.
    #[rustango(max_length = 255, default = "''")]
    pub disabled_reason: String,

    /// #555 — the most recent time this rule served a redirect. `None`
    /// until the first hit flush. Drives the stale-redirect metric.
    /// Written by the throttled hit-flush ([`flush_tenant`]) — never on
    /// the request path.
    pub last_hit_at: Option<DateTime<Utc>>,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

/// Add the #553 columns to a pre-existing `cms_redirect` table.
/// Idempotent (duplicate-column errors are swallowed); called per
/// tenant at boot from [`crate::seed::ensure_seeded`] — the same
/// mechanism the seed-ensured tables use, since repo-wide
/// `gen_migration` is blocked. Fresh databases get the whole table,
/// these columns included, from the JSON migrations; this upgrades a
/// table created before them.
///
/// No indexes on the page-id columns on purpose: `cms_redirect` is a
/// small table and the only filtered reads are the (rare) disable
/// sweeps.
///
/// # Errors
/// Driver failures other than "column already exists".
pub async fn ensure_columns(pool: &rustango::sql::Pool) -> Result<(), rustango::sql::ExecError> {
    // #555 — nullable; no default (NULL = never hit). The type must be
    // the one the model decodes `DateTime<Utc>` from (#745): a plain
    // `TIMESTAMP` on Postgres is `timestamp without time zone`, which
    // fails to decode once a hit writes a value; on MySQL it is
    // whole-second with the 2038 ceiling.
    let last_hit_at = match pool.dialect().name() {
        "postgres" => "ALTER TABLE cms_redirect ADD COLUMN last_hit_at TIMESTAMPTZ",
        "mysql" => "ALTER TABLE cms_redirect ADD COLUMN last_hit_at DATETIME(6) NULL",
        _ => "ALTER TABLE cms_redirect ADD COLUMN last_hit_at TIMESTAMP",
    };
    let cols: &[&str] = &[
        "ALTER TABLE cms_redirect ADD COLUMN overrides_live BOOLEAN NOT NULL DEFAULT FALSE",
        "ALTER TABLE cms_redirect ADD COLUMN from_page_id BIGINT",
        "ALTER TABLE cms_redirect ADD COLUMN to_page_id BIGINT",
        "ALTER TABLE cms_redirect ADD COLUMN is_active BOOLEAN NOT NULL DEFAULT TRUE",
        "ALTER TABLE cms_redirect ADD COLUMN disabled_reason VARCHAR(255) NOT NULL DEFAULT ''",
        last_hit_at,
    ];
    for stmt in cols {
        if let Err(e) = rustango::sql::raw_execute_pool(pool, stmt, Vec::new()).await {
            let msg = e.to_string().to_ascii_lowercase();
            // sqlite: "duplicate column name"; mysql: "Duplicate column
            // name"; postgres: "already exists".
            if msg.contains("duplicate column") || msg.contains("already exists") {
                continue;
            }
            return Err(e);
        }
    }
    repair_pg_last_hit_at(pool).await
}

/// Databases upgraded by a build before #745 got `last_hit_at` as
/// `timestamp without time zone` on Postgres. Convert it in place (the
/// values were written as UTC). A no-op once converted, and elsewhere.
async fn repair_pg_last_hit_at(pool: &rustango::sql::Pool) -> Result<(), rustango::sql::ExecError> {
    #[cfg(feature = "postgres")]
    if let Some(pg) = pool.as_postgres() {
        let data_type: Option<String> = rustango::sql::sqlx::query_scalar(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = 'cms_redirect' \
             AND column_name = 'last_hit_at'",
        )
        .fetch_optional(pg)
        .await
        .map_err(rustango::sql::ExecError::from)?;
        if data_type.as_deref() == Some("timestamp without time zone") {
            rustango::sql::raw_execute_pool(
                pool,
                "ALTER TABLE cms_redirect ALTER COLUMN last_hit_at TYPE TIMESTAMPTZ \
                 USING last_hit_at AT TIME ZONE 'UTC'",
                Vec::new(),
            )
            .await?;
        }
    }
    #[cfg(not(feature = "postgres"))]
    let _ = pool;
    Ok(())
}

/// Look up a redirect for `path` in the current tenant. Returns
/// `Ok(None)` when no row matches.
///
/// `path` should be the request URI's path component (no query
/// string, no host). The lookup is exact-match — trailing-slash
/// variance is intentional, mirroring Django's `contrib.redirects`.
pub async fn find_for_path(
    pool: &rustango::sql::Pool,
    path: &str,
) -> Result<Option<Redirect>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let rows: Vec<Redirect> = Redirect::objects()
        .where_(Redirect::from_path.eq(path.to_owned()))
        .where_(Redirect::is_active.eq(true))
        .fetch(pool)
        .await?;
    Ok(rows.into_iter().next())
}

/// The active `overrides_live` rules for this tenant. Consulted by the
/// public router BEFORE page resolution, so it must stay cheap: the
/// set is tiny in practice (usually empty) and the cached variant
/// below amortizes it to one query per TTL.
///
/// # Errors
/// Driver / query failures.
pub async fn override_rules(
    pool: &rustango::sql::Pool,
) -> Result<Vec<Redirect>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    Redirect::objects()
        .where_(Redirect::overrides_live.eq(true))
        .where_(Redirect::is_active.eq(true))
        .fetch(pool)
        .await
}

/// The cache key for one tenant's redirect rule set. Tenants on one host
/// share the cache, so a fixed key served one tenant's rules — including
/// override rules that hijack live pages — on every other host (#681).
fn rules_key(tenant_slug: &str, kind: &str) -> String {
    format!("cms:redirect:{tenant_slug}:{kind}")
}

/// Cached variant of [`override_rules`] (mirrors
/// [`crate::auto_menu::prefetch_cached`]): serves the rule set from
/// `cache` under a per-tenant key, recomputing on miss. Cache failures
/// degrade to a live query; query failures degrade to an empty set
/// (the router then just resolves pages normally).
pub async fn override_rules_cached(
    tenant_slug: &str,
    pool: &rustango::sql::Pool,
    cache: &rustango::cache::BoxedCache,
    ttl: Option<std::time::Duration>,
) -> Vec<Redirect> {
    let json = rustango::cache_fragment::cached_render(
        cache.as_ref(),
        &rules_key(tenant_slug, "overrides"),
        ttl,
        || async {
            let rules = override_rules(pool).await.unwrap_or_default();
            serde_json::to_string(&rules).unwrap_or_else(|_| "[]".to_owned())
        },
    )
    .await;
    serde_json::from_str(&json).unwrap_or_default()
}

/// Resolve the rule's destination at serve time: a `to_page_id` wins
/// (the page's **current** `url_path`, so the rule follows moves and
/// renames) with `to_path` as the fallback when the page is missing
/// or not served — the resolver's rule, so an archived target is still
/// followed (#763).
pub async fn resolve_destination(pool: &rustango::sql::Pool, r: &Redirect) -> String {
    use rustango::core::Column as _;
    if let Some(pid) = r.to_page_id {
        let page = crate::page::Page::objects()
            .where_(crate::page::Page::id.eq(pid))
            .where_(crate::page::Page::status.is_in(crate::resolver::served_statuses()))
            .first(pool)
            .await
            .ok()
            .flatten()
            .filter(|p| crate::resolver::visible_now(p, chrono::Utc::now()));
        if let Some(p) = page {
            return p.url_path;
        }
        tracing::warn!(
            redirect_id = r.id.get().copied().unwrap_or_default(),
            to_page_id = pid,
            "redirect destination page missing/unpublished; using to_path fallback"
        );
    }
    r.to_path.clone()
}

// ---------------------------------------------------------------------
// #554 — wildcard rules (single `*` glob, prefix/suffix capture)
// ---------------------------------------------------------------------

/// A `from_path` is a wildcard PATTERN (vs an exact path) when it
/// carries the single `*` glob.
#[must_use]
pub fn is_wildcard(from_path: &str) -> bool {
    from_path.contains('*')
}

/// Match a wildcard `pattern` (exactly one `*`) against `path`.
/// The pattern splits into a prefix + suffix around the star; a path
/// matches when it starts with the prefix, ends with the suffix, and
/// is long enough to leave a (possibly empty) capture between them.
/// Returns the captured segment (`*`'s expansion) on a match.
///
/// `/old-blog/*` (prefix rule) → matches `/old-blog/a/b`, capture `a/b`.
/// `*/feed` (suffix rule) → matches `/x/y/feed`, capture `/x/y`.
#[must_use]
pub fn match_wildcard(pattern: &str, path: &str) -> Option<String> {
    let star = pattern.find('*')?;
    let prefix = &pattern[..star];
    let suffix = &pattern[star + 1..];
    if path.len() < prefix.len() + suffix.len() {
        return None;
    }
    if !path.starts_with(prefix) || !path.ends_with(suffix) {
        return None;
    }
    Some(path[prefix.len()..path.len() - suffix.len()].to_owned())
}

/// Substitute a capture into a `to_path` template: its single `*` (if
/// any) becomes `capture`; without a `*` the capture is dropped (the
/// whole matched subtree funnels to one URL).
#[must_use]
pub fn apply_capture(to_path: &str, capture: &str) -> String {
    match to_path.find('*') {
        Some(star) => format!("{}{}{}", &to_path[..star], capture, &to_path[star + 1..]),
        None => to_path.to_owned(),
    }
}

/// Would a rule `from → to` send a visitor back into itself (#708)?
///
/// An exact rule loops when it targets its own path; a wildcard rule
/// loops when its destination for a sample capture matches the pattern
/// again (`/blog/* → /blog/archive/*`). Trailing slashes are ignored, as
/// the resolver ignores them. An off-site `to` never loops here.
#[must_use]
pub fn redirect_loops(from: &str, to: &str) -> bool {
    let norm = |p: &str| {
        let t = p.trim().trim_end_matches('/');
        if t.is_empty() { "/".to_owned() } else { t.to_owned() }
    };
    if !from.contains('*') {
        return norm(from) == norm(to);
    }
    let dest = apply_capture(to, "x");
    match_wildcard(from, &dest).is_some() || match_wildcard(from, &format!("{}/", norm(&dest))).is_some()
}

/// Most-specific wildcard rule matching `path` from a candidate set:
/// the longest pre-`*` prefix wins (so `/docs/0.44/*` beats `/docs/*`);
/// ties break to the lowest id for determinism. Returns the rule + its
/// capture.
fn best_wildcard_match<'a>(rules: &'a [Redirect], path: &str) -> Option<(&'a Redirect, String)> {
    let mut best: Option<(&Redirect, String, usize, i64)> = None;
    for r in rules {
        if !is_wildcard(&r.from_path) {
            continue;
        }
        let Some(cap) = match_wildcard(&r.from_path, path) else {
            continue;
        };
        let prefix_len = r.from_path.find('*').unwrap_or(0);
        let id = r.id.get().copied().unwrap_or(i64::MAX);
        let take = match &best {
            None => true,
            Some((_, _, bp, bid)) => prefix_len > *bp || (prefix_len == *bp && id < *bid),
        };
        if take {
            best = Some((r, cap, prefix_len, id));
        }
    }
    best.map(|(r, cap, _, _)| (r, cap))
}

/// Build a rule's destination given an optional wildcard capture. A
/// `to_page_id` (funnel-to-page) always resolves to the page's current
/// URL and ignores the capture; otherwise the capture is substituted
/// into `to_path`.
async fn destination_for(
    pool: &rustango::sql::Pool,
    rule: &Redirect,
    capture: Option<&str>,
) -> String {
    match (capture, rule.to_page_id) {
        (Some(cap), None) => apply_capture(&rule.to_path, cap),
        _ => resolve_destination(pool, rule).await,
    }
}

/// The active wildcard (`from_path` contains `*`) rules for this
/// tenant. Used on the exact-miss fallthrough.
///
/// # Errors
/// Driver / query failures.
pub async fn wildcard_rules(
    pool: &rustango::sql::Pool,
) -> Result<Vec<Redirect>, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    // Filtered in SQL (#722): this runs on every 404 fall-through, and a
    // migrated site can carry thousands of exact CSV-imported rules next
    // to a handful of wildcards. `*` is not a LIKE metacharacter, and
    // `contains` escapes the ones that are.
    Redirect::objects()
        .where_(Redirect::is_active.eq(true))
        .where_(Redirect::from_path.contains("*"))
        .fetch(pool)
        .await
}

/// Cached variant of [`wildcard_rules`] — same short-TTL fragment
/// cache as the override set, so tenants with no wildcard rules pay
/// nothing per request.
pub async fn wildcard_rules_cached(
    tenant_slug: &str,
    pool: &rustango::sql::Pool,
    cache: &rustango::cache::BoxedCache,
    ttl: Option<std::time::Duration>,
) -> Vec<Redirect> {
    let json = rustango::cache_fragment::cached_render(
        cache.as_ref(),
        &rules_key(tenant_slug, "wildcards"),
        ttl,
        || async {
            let rules = wildcard_rules(pool).await.unwrap_or_default();
            serde_json::to_string(&rules).unwrap_or_else(|_| "[]".to_owned())
        },
    )
    .await;
    serde_json::from_str(&json).unwrap_or_default()
}

fn build_response(to: String, permanent: bool, query: Option<&str>) -> axum::response::Response {
    rustango::redirects::build_redirect_response(
        &rustango::redirects::RedirectRule { to, permanent },
        query,
    )
}

/// Serve an `overrides_live` rule for `path` if one exists (exact match
/// first, then most-specific wildcard). Called by the public router
/// before page resolution; returns `None` on the (overwhelmingly
/// common) no-rule path. Uses the router's fragment cache when
/// available so cache-warm page views cost zero queries.
pub(crate) async fn serve_override(
    tenant: &rustango::extractors::Tenant,
    cache: Option<(&rustango::cache::BoxedCache, std::time::Duration)>,
    path: &str,
    query: Option<&str>,
) -> Option<axum::response::Response> {
    let rules = match cache {
        Some((c, ttl)) => override_rules_cached(&tenant.org.slug, tenant.pool(), c, Some(ttl)).await,
        None => override_rules(tenant.pool()).await.unwrap_or_default(),
    };
    // Exact override beats a pattern override at the same path.
    let (rule, capture): (Redirect, Option<String>) = if let Some(r) = rules
        .iter()
        .find(|r| !is_wildcard(&r.from_path) && r.from_path == path)
    {
        (r.clone(), None)
    } else {
        let (r, cap) = best_wildcard_match(&rules, path)?;
        (r.clone(), Some(cap))
    };
    let to = destination_for(tenant.pool(), &rule, capture.as_deref()).await;
    let resp = build_response(to, rule.is_permanent, query);
    record_hit(
        tenant.pool(),
        &tenant.org.slug,
        rule.id.get().copied().unwrap_or_default(),
    );
    Some(resp)
}

/// Serve a wildcard rule for `path` on the 404 fallthrough (no exact
/// rule, no live page matched). Non-override wildcards behave like
/// exact non-override rules — they only fill in for URLs with no live
/// page. Returns the built response on a match.
pub(crate) async fn serve_wildcard(
    tenant: &rustango::extractors::Tenant,
    cache: Option<(&rustango::cache::BoxedCache, std::time::Duration)>,
    path: &str,
    query: Option<&str>,
) -> Option<axum::response::Response> {
    let rules = match cache {
        Some((c, ttl)) => wildcard_rules_cached(&tenant.org.slug, tenant.pool(), c, Some(ttl)).await,
        None => wildcard_rules(tenant.pool()).await.unwrap_or_default(),
    };
    let (rule, capture) = best_wildcard_match(&rules, path)?;
    let rule = rule.clone();
    let to = destination_for(tenant.pool(), &rule, Some(&capture)).await;
    let resp = build_response(to, rule.is_permanent, query);
    record_hit(
        tenant.pool(),
        &tenant.org.slug,
        rule.id.get().copied().unwrap_or_default(),
    );
    Some(resp)
}

/// After a rename (a slug change), keep every old address working: for
/// each `(page_id, old_path, new_path)` a permanent rule from the old
/// path to the page itself (`to_page_id`, so it keeps following later
/// moves). A rule that already starts at the old path is re-pointed and
/// re-enabled instead of duplicated. Runs in the rename's transaction.
///
/// The paths are the pages' `url_path`s — the public path on a
/// single-site tenant (a hostname-mapped site strips its root prefix
/// from public URLs, which these rules don't).
///
/// # Errors
/// Driver / query failures.
pub(crate) async fn record_renames_tx(
    tx: &mut rustango::sql::PoolTx<'_>,
    moves: &[(i64, String, String)],
) -> Result<(), rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherTx as _;
    for (page_id, old, new) in moves {
        if old.is_empty() || old == new {
            continue;
        }
        let existing = Redirect::objects()
            .where_(Redirect::from_path.eq(old.clone()))
            .fetch_tx(tx)
            .await?
            .into_iter()
            .next();
        let mut rule = existing.unwrap_or_else(|| Redirect {
            id: Auto::Unset,
            from_path: old.clone(),
            to_path: String::new(),
            is_permanent: true,
            note: "Automatic: the page was renamed.".to_owned(),
            hit_count: 0,
            overrides_live: false,
            from_page_id: None,
            to_page_id: None,
            is_active: true,
            disabled_reason: String::new(),
            last_hit_at: None,
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        });
        rule.to_path = new.clone();
        rule.to_page_id = Some(*page_id);
        rule.is_active = true;
        rule.disabled_reason.clear();
        rule.save_tx(tx).await?;
    }
    Ok(())
}

/// A descendant's address before its ancestor moved from `old_base` to
/// `new_base`: its current `url` with the new base swapped for the old.
pub(crate) fn rebase_path(url: &str, new_base: &str, old_base: &str) -> Option<String> {
    let rest = if new_base == "/" {
        url.strip_prefix('/')?
    } else {
        url.strip_prefix(new_base)?.strip_prefix('/')?
    };
    Some(if old_base == "/" {
        format!("/{rest}")
    } else {
        format!("{}/{rest}", old_base.trim_end_matches('/'))
    })
}

/// Disable every active rule that references `page_id` via
/// `from_page_id` or `to_page_id`, recording `reason`. Called from the
/// admin's page delete / unpublish paths so a rule never silently
/// serves a dead source or points at a vanished destination. Returns
/// how many rules were disabled.
///
/// # Errors
/// Driver / query failures (the caller logs; the page action itself
/// must not fail on redirect bookkeeping).
pub async fn disable_for_page(
    pool: &rustango::sql::Pool,
    page_id: i64,
    reason: &str,
) -> Result<u64, rustango::sql::ExecError> {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    let mut rows: Vec<Redirect> = Redirect::objects()
        .where_(Redirect::from_page_id.eq(Some(page_id)))
        .where_(Redirect::is_active.eq(true))
        .fetch(pool)
        .await?;
    let more: Vec<Redirect> = Redirect::objects()
        .where_(Redirect::to_page_id.eq(Some(page_id)))
        .where_(Redirect::is_active.eq(true))
        .fetch(pool)
        .await?;
    let seen: std::collections::HashSet<i64> =
        rows.iter().filter_map(|r| r.id.get().copied()).collect();
    rows.extend(
        more.into_iter()
            .filter(|r| r.id.get().copied().map_or(true, |id| !seen.contains(&id))),
    );
    let mut disabled = 0u64;
    for mut row in rows {
        row.is_active = false;
        row.disabled_reason = reason.chars().take(255).collect();
        row.save_pool(pool).await?;
        disabled += 1;
    }
    Ok(disabled)
}

/// Convert a [`Redirect`] row into the framework's [`RedirectRule`]
/// shape so [`rustango::redirects::build_redirect_response`] can
/// produce the 301/302.
#[must_use]
pub fn to_rule(r: &Redirect) -> rustango::redirects::RedirectRule {
    rustango::redirects::RedirectRule {
        to: r.to_path.clone(),
        permanent: r.is_permanent,
    }
}

/// Increment `hit_count` for a single redirect row. Failures are
/// logged + swallowed — analytics counters shouldn't break the
/// user-facing redirect.
///
/// Superseded on the request path by [`record_hit`] (#555) — kept for
/// the public re-export and ad-hoc use.
pub async fn bump_hit_count(pool: &rustango::sql::Pool, mut row: Redirect) {
    row.hit_count = row.hit_count.saturating_add(1);
    if let Err(e) = row.save_pool(pool).await {
        tracing::warn!(error = %e, "cms_redirect: failed to bump hit_count");
    }
}

// ---------------------------------------------------------------------
// #555 — hit telemetry without per-request DB writes
// ---------------------------------------------------------------------
//
// Serving a redirect used to UPDATE the row per hit — a write on the
// hot path that still answered no real question (no recency, no trend).
// Instead we accumulate hits in a process-global map and flush the
// deltas on a throttled, detached cadence, using the SERVING request's
// own pool. The map is keyed by (tenant_slug, redirect_id), so each
// tenant flushes its own deltas from its own traffic — no registry
// access, no cross-tenant pools, no host wiring. A crash loses at most
// one interval's counts (fine for a usage metric).

/// Flush cadence — deltas older than this (per tenant) trigger a
/// detached write on the next served redirect.
const FLUSH_INTERVAL_SECS: i64 = 60;

#[derive(Default)]
struct HitAccum {
    /// `(tenant_slug, redirect_id)` → `(pending_delta, last_hit_unix_secs)`.
    pending: std::collections::HashMap<(String, i64), (u64, i64)>,
    /// `tenant_slug` → last flush unix secs (throttle).
    last_flush: std::collections::HashMap<String, i64>,
}

static HITS: std::sync::LazyLock<std::sync::Mutex<HitAccum>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HitAccum::default()));

fn lock_hits() -> std::sync::MutexGuard<'static, HitAccum> {
    HITS.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record a served-redirect hit — **zero DB I/O on the request path**
/// (two map ops). When the tenant's throttle window has elapsed, spawn
/// a detached flush that writes the accumulated deltas via the serving
/// request's pool.
pub(crate) fn record_hit(pool: &rustango::sql::Pool, tenant_slug: &str, redirect_id: i64) {
    if redirect_id == 0 {
        return; // unsaved row — nothing to attribute the hit to
    }
    let now = chrono::Utc::now().timestamp();
    let due = {
        let mut acc = lock_hits();
        let e = acc
            .pending
            .entry((tenant_slug.to_owned(), redirect_id))
            .or_insert((0, now));
        e.0 = e.0.saturating_add(1);
        e.1 = e.1.max(now);
        let last = acc.last_flush.get(tenant_slug).copied().unwrap_or(0);
        if now - last >= FLUSH_INTERVAL_SECS {
            acc.last_flush.insert(tenant_slug.to_owned(), now);
            true
        } else {
            false
        }
    };
    if due {
        let pool = pool.clone();
        let slug = tenant_slug.to_owned();
        rustango::__private_runtime::tokio::spawn(async move {
            flush_tenant(&pool, &slug).await;
        });
    }
}

/// Drain one tenant's pending hit deltas and persist them with a
/// targeted `UPDATE` — `hit_count += Δ` and `last_hit_at = greatest(…)`
/// only, so the flush never bumps `updated_at` (which must stay the
/// last *edit* time). Additive + idempotent-on-recency, so concurrent
/// flushes from multiple processes can't double-count or regress the
/// timestamp. Best-effort: a failed write drops that batch.
pub(crate) async fn flush_tenant(pool: &rustango::sql::Pool, tenant_slug: &str) {
    let drained: Vec<(i64, u64, i64)> = {
        let mut acc = lock_hits();
        let keys: Vec<(String, i64)> = acc
            .pending
            .keys()
            .filter(|(s, _)| s == tenant_slug)
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|k| acc.pending.remove(&k).map(|(d, ts)| (k.1, d, ts)))
            .collect()
    };
    if drained.is_empty() {
        return;
    }
    let dialect = pool.dialect();
    let sqlite = dialect.name() == "sqlite";
    let sql = format!(
        "UPDATE cms_redirect SET hit_count = hit_count + {}, \
         last_hit_at = CASE WHEN last_hit_at IS NULL OR last_hit_at < {} THEN {} ELSE last_hit_at END \
         WHERE id = {}",
        dialect.placeholder(1),
        dialect.placeholder(2),
        dialect.placeholder(3),
        dialect.placeholder(4),
    );
    for (id, delta, ts) in drained {
        let ts_dt = chrono::DateTime::from_timestamp(ts, 0).unwrap_or_else(chrono::Utc::now);
        // SQLite stores auto timestamps as TEXT "YYYY-MM-DD HH:MM:SS";
        // bind that shape so the lexical CASE comparison matches (same
        // trick as analytics::retention).
        let ts_bind = if sqlite {
            rustango::core::SqlValue::String(ts_dt.format("%Y-%m-%d %H:%M:%S").to_string())
        } else {
            rustango::core::SqlValue::DateTime(ts_dt)
        };
        let binds = vec![
            rustango::core::SqlValue::I64(delta as i64),
            ts_bind.clone(),
            ts_bind,
            rustango::core::SqlValue::I64(id),
        ];
        if let Err(e) = rustango::sql::raw_execute_pool(pool, &sql, binds).await {
            tracing::warn!(redirect_id = id, error = %e, "redirect hit flush failed");
        }
    }
}

/// Flush every tenant's pending deltas immediately (e.g. graceful
/// shutdown). Best-effort. `pools` are the per-tenant pools to write
/// through, matched to the accumulated slugs.
///
/// Most hosts don't need to call this — the throttled per-request
/// flush keeps the map bounded — but it's available for a clean
/// shutdown drain.
pub async fn flush_all(pools: &[(String, rustango::sql::Pool)]) {
    for (slug, pool) in pools {
        flush_tenant(pool, slug).await;
    }
}

// ---------------------------------------------------------------------
// #555 — stale-redirect report
// ---------------------------------------------------------------------

/// A rule with no hit for this many days counts as **stale**; a rule
/// never hit since (roughly) creation counts as **unused**. Editors
/// use the report to prune rules nothing points at anymore.
pub const STALE_AFTER_DAYS: i64 = 90;

/// Classify a rule for the stale metric: `"disabled"` (auto-switched
/// off), `"unused"` (never served), `"stale"` (no hit in
/// [`STALE_AFTER_DAYS`]+), or `"active"`. Shared by the report and the
/// admin list filter so both agree.
#[must_use]
pub(crate) fn staleness(r: &Redirect, now: DateTime<Utc>) -> &'static str {
    let cutoff = now - chrono::Duration::days(STALE_AFTER_DAYS);
    if !r.is_active {
        "disabled"
    } else if r.last_hit_at.is_none() {
        "unused"
    } else if r.last_hit_at.map_or(false, |t| t < cutoff) {
        "stale"
    } else {
        "active"
    }
}

/// Registry report (#439 framework) surfacing which redirects are worth
/// retiring: never-used, long-unused, or auto-disabled. Appears in the
/// Reports nav automatically via [`crate::register_report!`].
#[derive(Default)]
pub struct StaleRedirectsReport;

#[async_trait::async_trait]
impl crate::admin::report::Report for StaleRedirectsReport {
    fn slug(&self) -> &'static str {
        "stale-redirects"
    }
    fn title(&self) -> &'static str {
        "Stale redirects"
    }
    fn description(&self) -> Option<&'static str> {
        Some("Redirects nothing has used recently — candidates to retire. 'unused' never served; 'stale' hasn't served in 90+ days; 'disabled' was switched off when a linked page was removed.")
    }
    fn icon(&self) -> Option<&'static str> {
        Some("link_off")
    }
    fn columns(&self) -> Vec<crate::admin::report::ReportColumn> {
        use crate::admin::report::ReportColumn;
        vec![
            ReportColumn::text("from", "From"),
            ReportColumn::text("to", "To"),
            ReportColumn::status("verdict", "Status"),
            ReportColumn::number("hits", "Hits"),
            ReportColumn::datetime("last_used", "Last used"),
        ]
    }
    async fn rows(
        &self,
        pool: &rustango::sql::Pool,
        _user_id: Option<i64>,
    ) -> Vec<serde_json::Value> {
        use rustango::sql::FetcherPool as _;
        let now = chrono::Utc::now();
        let mut rows: Vec<Redirect> = Redirect::objects().fetch(pool).await.unwrap_or_default();
        // Rank: disabled, then unused, then stale, then active; within a
        // rank, oldest last-used first (most worth reviewing at the top).
        let rank = |v: &str| -> u8 {
            match v {
                "disabled" => 0,
                "unused" => 1,
                "stale" => 2,
                _ => 3,
            }
        };
        rows.sort_by(|a, b| {
            rank(staleness(a, now))
                .cmp(&rank(staleness(b, now)))
                .then_with(|| a.last_hit_at.cmp(&b.last_hit_at))
        });
        rows.into_iter()
            .filter(|r| rank(staleness(r, now)) < 3) // omit healthy "active" rows
            .map(|r| {
                let v = staleness(&r, now);
                let id = r.id.get().copied().unwrap_or_default();
                json!({
                    "from": r.from_path,
                    "to": r.to_path,
                    // The status pill class comes from this value; reuse
                    // the archived/scheduled/draft tokens the report table
                    // already styles.
                    "verdict": match v { "disabled" => "archived", "unused" => "draft", _ => "scheduled" },
                    "hits": r.hit_count,
                    "last_used": r.last_hit_at.map(|t| t.to_rfc3339()),
                    "edit_url": format!("/cms-admin/redirects/{id}/edit"),
                })
            })
            .collect()
    }
}

crate::register_report!(StaleRedirectsReport);

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    #[test]
    fn rule_sets_are_cached_per_tenant() {
        // #681 — one tenant's override rules must never be served on another host.
        assert_ne!(super::rules_key("acme", "overrides"), super::rules_key("globex", "overrides"));
        assert_ne!(super::rules_key("acme", "overrides"), super::rules_key("acme", "wildcards"));
    }

    use rustango::core::Column as _;
    use rustango::core::Model as _;
    use rustango::sql::FetcherPool as _;
    use rustango::sql::Pool;

    async fn mem_pool() -> Pool {
        let pool = Pool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        // cms_page + its FK parents (needed by resolve_destination) and
        // cms_redirect, straight from each model's SCHEMA.
        for schema in [
            &crate::page_type_model::PageType::SCHEMA,
            &crate::media::Media::SCHEMA,
            &crate::theme::Theme::SCHEMA,
            &crate::page::Page::SCHEMA,
            &Redirect::SCHEMA,
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
        pool
    }

    fn mk_redirect(from: &str, to: &str) -> Redirect {
        Redirect {
            id: Auto::Unset,
            from_path: from.to_owned(),
            to_path: to.to_owned(),
            is_permanent: true,
            note: String::new(),
            hit_count: 0,
            overrides_live: false,
            from_page_id: None,
            to_page_id: None,
            is_active: true,
            disabled_reason: String::new(),
            last_hit_at: None,
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        }
    }

    #[test]
    fn staleness_classifies_all_four_verdicts() {
        let now = chrono::Utc::now();
        let mut r = mk_redirect("/a", "/b");
        // Recently served → active.
        r.last_hit_at = Some(now - chrono::Duration::days(1));
        assert_eq!(staleness(&r, now), "active");
        // No hit inside the window → stale.
        r.last_hit_at = Some(now - chrono::Duration::days(STALE_AFTER_DAYS + 5));
        assert_eq!(staleness(&r, now), "stale");
        // Never served → unused.
        r.last_hit_at = None;
        assert_eq!(staleness(&r, now), "unused");
        // Disabled beats any hit history.
        r.is_active = false;
        r.last_hit_at = Some(now);
        assert_eq!(staleness(&r, now), "disabled");
    }

    async fn mk_page(pool: &Pool, slug: &str, status: &str) -> i64 {
        let mut page = crate::page::Page {
            id: Auto::Unset,
            page_type_id: 1,
            title: format!("Page {slug}"),
            slug: slug.to_owned(),
            path: format!("0001{slug}/"),
            url_path: format!("/{slug}"),
            preview_path: String::new(),
            template_override: String::new(),
            depth: 1,
            parent_id: None,
            locale_variant_of: None,
            alias_of: None,
            theme_id: None,
            sort_order: 0,
            status: status.to_owned(),
            published_at: None,
            last_published_at: None,
            go_live_at: None,
            expire_at: None,
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
        // Satisfy the page_type FK.
        let existing = crate::page_type_model::PageType::objects()
            .fetch(pool)
            .await
            .expect("pt fetch");
        if existing.is_empty() {
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
            pt.insert_pool(pool).await.expect("insert page type");
        }
        page.insert_pool(pool).await.expect("insert page");
        page.id.get().copied().expect("page id")
    }

    /// `ensure_columns` must no-op on a table that already has the
    /// #553 columns (fresh SCHEMA) — the duplicate-column error paths
    /// are swallowed on every dialect message shape.
    #[test]
    fn rebase_path_swaps_the_moved_base() {
        assert_eq!(rebase_path("/shop/moon-jar", "/shop", "/store").as_deref(), Some("/store/moon-jar"));
        assert_eq!(rebase_path("/a/b/c", "/a/b", "/x").as_deref(), Some("/x/c"));
        // A home page (base "/") renamed into a slug, and back.
        assert_eq!(rebase_path("/shop", "/", "/home").as_deref(), Some("/home/shop"));
        assert_eq!(rebase_path("/home/shop", "/home", "/").as_deref(), Some("/shop"));
        // Not under the new base, or a sibling that merely shares a prefix.
        assert_eq!(rebase_path("/other", "/shop", "/store"), None);
        assert_eq!(rebase_path("/shopping", "/shop", "/store"), None);
    }

    /// A rename leaves a rule that follows the page; a second rename of
    /// the same old address re-points that rule instead of duplicating it.
    #[tokio::test]
    async fn renames_leave_rules_that_follow_the_page() {
        let pool = mem_pool().await;
        let mut stale = mk_redirect("/old-jar", "/nowhere");
        stale.is_active = false;
        stale.disabled_reason = "Linked page was unpublished".to_owned();
        stale.save_pool(&pool).await.expect("stale rule");

        let moves = vec![
            (7, "/old-jar".to_owned(), "/shop/jar".to_owned()),
            (8, "/shop/vase".to_owned(), "/shop/vase".to_owned()), // unchanged: skipped
            (9, "/shop/bowl".to_owned(), "/store/bowl".to_owned()),
        ];
        let mut tx = rustango::sql::transaction_pool(&pool).await.expect("tx");
        record_renames_tx(&mut tx, &moves).await.expect("record");
        tx.commit().await.expect("commit");

        let rules: Vec<Redirect> = Redirect::objects().fetch(&pool).await.expect("rules");
        assert_eq!(rules.len(), 2, "one per moved address, the stale one reused: {rules:?}");
        let jar = rules.iter().find(|r| r.from_path == "/old-jar").expect("re-pointed");
        assert_eq!((jar.to_path.as_str(), jar.to_page_id, jar.is_active), ("/shop/jar", Some(7), true));
        assert!(jar.disabled_reason.is_empty());
        let bowl = rules.iter().find(|r| r.from_path == "/shop/bowl").expect("created");
        assert_eq!((bowl.to_page_id, bowl.is_permanent), (Some(9), true));
    }

    #[tokio::test]
    async fn ensure_columns_is_idempotent() {
        let pool = mem_pool().await;
        ensure_columns(&pool).await.expect("first ensure");
        ensure_columns(&pool).await.expect("second ensure");
    }

    /// `ensure_columns` upgrades a pre-#553 table shape in place.
    #[tokio::test]
    async fn ensure_columns_upgrades_old_table() {
        let pool = Pool::connect("sqlite::memory:").await.expect("pool");
        rustango::sql::raw_execute_pool(
            &pool,
            "CREATE TABLE cms_redirect (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_path VARCHAR(510) NOT NULL UNIQUE,
                to_path VARCHAR(1024) NOT NULL,
                is_permanent BOOLEAN NOT NULL DEFAULT TRUE,
                note VARCHAR(255) NOT NULL DEFAULT '',
                hit_count BIGINT NOT NULL DEFAULT 0,
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
            Vec::new(),
        )
        .await
        .expect("old-shape table");
        ensure_columns(&pool).await.expect("upgrade");
        // Full-model round-trip decodes the new columns with defaults.
        let mut row = mk_redirect("/old", "/new");
        row.insert_pool(&pool).await.expect("insert");
        let got = find_for_path(&pool, "/old").await.expect("query");
        let got = got.expect("row found");
        assert!(got.is_active);
        assert!(!got.overrides_live);
        assert_eq!(got.disabled_reason, "");
    }

    #[tokio::test]
    async fn find_for_path_skips_disabled_rules() {
        let pool = mem_pool().await;
        let mut row = mk_redirect("/dead", "/alive");
        row.is_active = false;
        row.disabled_reason = "Linked page was deleted".to_owned();
        row.insert_pool(&pool).await.expect("insert");
        assert!(
            find_for_path(&pool, "/dead")
                .await
                .expect("query")
                .is_none(),
            "disabled rules must never serve"
        );
    }

    #[tokio::test]
    async fn override_rules_returns_only_active_overrides() {
        let pool = mem_pool().await;
        let mut normal = mk_redirect("/a", "/b");
        normal.insert_pool(&pool).await.expect("insert normal");
        let mut over = mk_redirect("/live", "/elsewhere");
        over.overrides_live = true;
        over.insert_pool(&pool).await.expect("insert override");
        let mut dead_over = mk_redirect("/live2", "/elsewhere2");
        dead_over.overrides_live = true;
        dead_over.is_active = false;
        dead_over.insert_pool(&pool).await.expect("insert dead");
        let rules = override_rules(&pool).await.expect("query");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].from_path, "/live");
    }

    #[tokio::test]
    async fn resolve_destination_follows_current_page_url() {
        let pool = mem_pool().await;
        let pid = mk_page(&pool, "target", "published").await;
        let mut rule = mk_redirect("/old", "/stale-snapshot");
        rule.to_page_id = Some(pid);
        // Destination follows the page's CURRENT url_path…
        assert_eq!(resolve_destination(&pool, &rule).await, "/target");
        // …including after a move/rename.
        let mut page = crate::page::Page::objects()
            .where_(crate::page::Page::id.eq(pid))
            .first(&pool)
            .await
            .expect("q")
            .expect("page");
        page.url_path = "/moved/target".to_owned();
        page.save_pool(&pool).await.expect("save");
        assert_eq!(resolve_destination(&pool, &rule).await, "/moved/target");
    }

    /// #763 — an archived page is still served, so a rule targeting it
    /// keeps following it instead of falling back to a stale path.
    #[tokio::test]
    async fn resolve_destination_follows_an_archived_page() {
        let pool = mem_pool().await;
        let pid = mk_page(&pool, "kept", "archived").await;
        let mut rule = mk_redirect("/old", "/stale-snapshot");
        rule.to_page_id = Some(pid);
        assert_eq!(resolve_destination(&pool, &rule).await, "/kept");
    }

    #[tokio::test]
    async fn resolve_destination_falls_back_when_page_unpublished() {
        let pool = mem_pool().await;
        let pid = mk_page(&pool, "gone", "draft").await;
        let mut rule = mk_redirect("/old", "/fallback");
        rule.to_page_id = Some(pid);
        assert_eq!(
            resolve_destination(&pool, &rule).await,
            "/fallback",
            "unpublished target page must fall back to to_path"
        );
    }

    // ---- #554 wildcard matcher (pure) ----

    #[test]
    fn wildcard_prefix_match_and_capture() {
        assert_eq!(
            match_wildcard("/old-blog/*", "/old-blog/2024/hi").as_deref(),
            Some("2024/hi")
        );
        assert_eq!(
            match_wildcard("/old-blog/*", "/old-blog/").as_deref(),
            Some("")
        );
        // strict: no remainder / different prefix → no match
        assert_eq!(match_wildcard("/old-blog/*", "/old-blog"), None);
        assert_eq!(match_wildcard("/old-blog/*", "/other/x"), None);
    }

    #[test]
    fn wildcard_suffix_match_and_capture() {
        assert_eq!(
            match_wildcard("*/feed", "/blog/feed").as_deref(),
            Some("/blog")
        );
        assert_eq!(
            match_wildcard("*/feed", "/a/b/feed").as_deref(),
            Some("/a/b")
        );
        assert_eq!(match_wildcard("*/feed", "/feedx"), None);
    }

    #[test]
    fn loops_are_detected_and_ordinary_rules_are_not() {
        assert!(redirect_loops("/a", "/a"));
        assert!(redirect_loops("/a/", "/a"));
        assert!(redirect_loops("/blog/*", "/blog/archive/*"));
        assert!(redirect_loops("/blog/*", "/blog/"));
        assert!(redirect_loops("*/feed", "/new/*/feed"));
        assert!(!redirect_loops("/a", "/b"));
        assert!(!redirect_loops("/old-blog/*", "/blog/*"));
        assert!(!redirect_loops("/blog/*", "https://example.test/blog/*"));
        assert!(!redirect_loops("*/feed", "*/rss"));
    }

    #[test]
    fn apply_capture_substitutes_or_drops() {
        assert_eq!(apply_capture("/blog/*", "2024/hi"), "/blog/2024/hi");
        assert_eq!(apply_capture("/archive", "2024/hi"), "/archive"); // no star → drop
    }

    #[test]
    fn best_wildcard_match_prefers_longest_prefix_then_lowest_id() {
        let mut broad = mk_redirect("/docs/*", "/d/*");
        broad.id = Auto::Set(2);
        let mut specific = mk_redirect("/docs/0.44/*", "/d44/*");
        specific.id = Auto::Set(5);
        let rules = vec![broad, specific];
        let (r, cap) = best_wildcard_match(&rules, "/docs/0.44/intro").expect("match");
        assert_eq!(r.from_path, "/docs/0.44/*");
        assert_eq!(cap, "intro");
        // A path only the broad rule covers falls to the broad rule.
        let (r2, cap2) = best_wildcard_match(&rules, "/docs/other").expect("match");
        assert_eq!(r2.from_path, "/docs/*");
        assert_eq!(cap2, "other");
    }

    #[tokio::test]
    async fn wildcard_rules_query_returns_only_active_patterns() {
        let pool = mem_pool().await;
        mk_redirect("/exact", "/x")
            .insert_pool(&pool)
            .await
            .expect("exact");
        mk_redirect("/glob/*", "/g/*")
            .insert_pool(&pool)
            .await
            .expect("glob");
        let mut dead = mk_redirect("/dead/*", "/d/*");
        dead.is_active = false;
        dead.insert_pool(&pool).await.expect("dead");
        let rules = wildcard_rules(&pool).await.expect("query");
        assert_eq!(rules.len(), 1, "only the active pattern");
        assert_eq!(rules[0].from_path, "/glob/*");
    }

    #[tokio::test]
    async fn disable_for_page_hits_both_link_columns_and_records_reason() {
        let pool = mem_pool().await;
        let mut from_linked = mk_redirect("/f", "/x");
        from_linked.from_page_id = Some(7);
        from_linked.insert_pool(&pool).await.expect("i1");
        let mut to_linked = mk_redirect("/t", "/y");
        to_linked.to_page_id = Some(7);
        to_linked.insert_pool(&pool).await.expect("i2");
        let mut unrelated = mk_redirect("/u", "/z");
        unrelated.from_page_id = Some(8);
        unrelated.insert_pool(&pool).await.expect("i3");

        let n = disable_for_page(&pool, 7, "Linked page “X” was deleted")
            .await
            .expect("disable");
        assert_eq!(n, 2, "both linked rules disabled, unrelated untouched");
        assert!(find_for_path(&pool, "/f").await.expect("q").is_none());
        assert!(find_for_path(&pool, "/t").await.expect("q").is_none());
        assert!(find_for_path(&pool, "/u").await.expect("q").is_some());
        // Reason recorded; second sweep is a no-op.
        let rows: Vec<Redirect> = Redirect::objects()
            .where_(Redirect::from_path.eq("/f".to_owned()))
            .fetch(&pool)
            .await
            .expect("fetch");
        assert_eq!(rows[0].disabled_reason, "Linked page “X” was deleted");
        let again = disable_for_page(&pool, 7, "x").await.expect("again");
        assert_eq!(again, 0);
    }

    #[tokio::test]
    async fn flush_tenant_applies_deltas_additively_and_sets_last_hit() {
        let pool = mem_pool().await;
        let mut r = mk_redirect("/flush", "/dest");
        r.insert_pool(&pool).await.expect("insert");
        let id = Redirect::objects()
            .where_(Redirect::from_path.eq("/flush".to_owned()))
            .fetch(&pool)
            .await
            .expect("fetch")[0]
            .id
            .get()
            .copied()
            .expect("id");

        // Prime the accumulator directly under a unique slug — avoids the
        // throttled detached spawn inside record_hit racing this test.
        let ts = chrono::Utc::now().timestamp();
        {
            let mut acc = lock_hits();
            acc.pending
                .insert(("flushtest_555".to_owned(), id), (3, ts));
        }
        flush_tenant(&pool, "flushtest_555").await;

        let row = Redirect::objects()
            .where_(Redirect::id.eq(id))
            .fetch(&pool)
            .await
            .expect("refetch")
            .remove(0);
        assert_eq!(row.hit_count, 3, "delta applied");
        assert!(row.last_hit_at.is_some(), "last_hit_at stamped");

        // Draining is idempotent: a second flush over an empty queue is a
        // no-op — no double counting.
        flush_tenant(&pool, "flushtest_555").await;
        let row2 = Redirect::objects()
            .where_(Redirect::id.eq(id))
            .fetch(&pool)
            .await
            .expect("refetch2")
            .remove(0);
        assert_eq!(row2.hit_count, 3, "no double count on empty flush");
    }
}

#[cfg(all(test, feature = "postgres"))]
mod pg_tests {
    use rustango::sql::sqlx;

    /// #745 — an upgraded Postgres table gets `last_hit_at` as a type the
    /// model decodes, and one added as plain `TIMESTAMP` by an older
    /// build is converted. Runs with `--ignored` against
    /// `RCMS_TEST_PG_URL`, in a schema of its own.
    #[tokio::test]
    #[ignore = "needs RCMS_TEST_PG_URL"]
    async fn last_hit_at_is_timestamptz_after_ensure() {
        let url = std::env::var("RCMS_TEST_PG_URL")
            .expect("set RCMS_TEST_PG_URL to a throwaway Postgres database to run this");
        let schema = format!("rcms_test_redirect_{}", std::process::id());
        let admin = sqlx::PgPool::connect(&url).await.expect("connect");
        sqlx::query(&format!("CREATE SCHEMA {schema}")).execute(&admin).await.expect("schema");
        let sep = if url.contains('?') { '&' } else { '?' };
        let pool = rustango::sql::Pool::connect(&format!(
            "{url}{sep}options=-c%20search_path%3D{schema}"
        ))
        .await
        .expect("scoped");
        let pg = pool.as_postgres().expect("pg");
        // The pre-#555 table, plus the column exactly as the old
        // ensure_columns added it.
        sqlx::query(
            "CREATE TABLE cms_redirect (id BIGSERIAL PRIMARY KEY, \
             from_path VARCHAR(510) NOT NULL UNIQUE, to_path VARCHAR(1024) NOT NULL, \
             is_permanent BOOLEAN NOT NULL DEFAULT TRUE, note VARCHAR(255) NOT NULL DEFAULT '', \
             hit_count BIGINT NOT NULL DEFAULT 0, last_hit_at TIMESTAMP, \
             created_at TIMESTAMPTZ NOT NULL DEFAULT now(), updated_at TIMESTAMPTZ NOT NULL DEFAULT now())",
        )
        .execute(pg)
        .await
        .expect("old table");
        sqlx::query("INSERT INTO cms_redirect (from_path, to_path, last_hit_at) VALUES ('/a', '/b', '2026-01-02 03:04:05')")
            .execute(pg)
            .await
            .expect("hit row");

        super::ensure_columns(&pool).await.expect("ensure");
        super::ensure_columns(&pool).await.expect("idempotent");

        let data_type: String = sqlx::query_scalar(
            "SELECT data_type FROM information_schema.columns WHERE table_schema = $1 \
             AND table_name = 'cms_redirect' AND column_name = 'last_hit_at'",
        )
        .bind(&schema)
        .fetch_one(pg)
        .await
        .expect("type");
        let found = super::find_for_path(&pool, "/a").await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE")).execute(&admin).await.expect("drop");

        assert_eq!(data_type, "timestamp with time zone");
        let row = found.expect("a hit row decodes").expect("row");
        assert_eq!(row.last_hit_at.map(|t| t.to_rfc3339()).as_deref(), Some("2026-01-02T03:04:05+00:00"));
    }
}
