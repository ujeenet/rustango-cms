//! Analytics event model + boot-time table/index creation.
//!
//! `cms_analytics_event` is created per tenant at boot via
//! `CREATE TABLE IF NOT EXISTS` (the same pattern as `cms_form_entry`).
//! The create-table shortcut does **not** emit index DDL, so the indexes
//! the dashboard aggregates rely on are created explicitly here.

use chrono::{DateTime, Utc};
use rustango::core::Model as _;
use rustango::sql::Auto;
use rustango::Model;
use serde::{Deserialize, Serialize};

/// One analytics hit — a `pageview` (HTTP beacon on load) or an
/// `engagement` record (written when the visitor's WebSocket
/// disconnects, or via an unload fallback beacon). Append-only. No raw
/// IP is ever stored — only a coarse `country` resolved from a CDN
/// header.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "cms_analytics_event", app = "cms")]
pub struct AnalyticsEvent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// Persistent client id (localStorage UUID).
    #[rustango(max_length = 36)]
    pub visitor_id: String,
    /// Per-tab session id (sessionStorage UUID).
    #[rustango(max_length = 36)]
    pub session_id: String,
    /// `"pageview"` | `"engagement"`.
    #[rustango(max_length = 16)]
    pub event_type: String,
    /// Path only — no scheme/host/query.
    #[rustango(max_length = 512)]
    pub path: String,
    /// Referrer host only (privacy: no full URL / query string).
    #[rustango(max_length = 255)]
    pub referrer_host: String,
    /// ISO-3166 alpha-2 country from a CDN header; `""` = Unknown.
    #[rustango(max_length = 2)]
    pub country: String,
    /// `"desktop"` | `"mobile"` | `"tablet"`.
    #[rustango(max_length = 16)]
    pub device: String,
    #[rustango(max_length = 32)]
    pub browser: String,
    #[rustango(max_length = 32)]
    pub os: String,
    pub screen_w: i32,
    pub screen_h: i32,
    /// Engagement only: time on page (ms). 0 for pageviews.
    pub duration_ms: i64,
    /// Engagement only: max scroll depth 0-100. 0 for pageviews.
    pub max_scroll_pct: i32,
    /// Engagement only: client bounce hint (the authoritative bounce
    /// rate is derived server-side in the dashboard query).
    pub is_bounce: bool,
    #[rustango(max_length = 16)]
    pub locale: String,
    /// Set explicitly at insert (`Utc::now()`) rather than via SQLite's
    /// space-formatted `CURRENT_TIMESTAMP`, so the value is stored as an
    /// RFC3339 timestamp and typed ORM date filters
    /// (`created_at.gte(from)`) compare correctly across dialects.
    pub created_at: DateTime<Utc>,
}

/// Create `cms_analytics_event` (+ its indexes) if absent. Idempotent;
/// called per tenant at boot.
///
/// # Errors
/// Driver / DDL failures on the CREATE TABLE.
pub async fn ensure_table(pool: &rustango::sql::Pool) -> Result<(), rustango::sql::ExecError> {
    let ddl = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
        pool.dialect(),
        &AnalyticsEvent::SCHEMA,
    );
    for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new()).await?;
    }
    // Indexes — hand-emitted because the create-table shortcut skips
    // them. Build the DDL up front so the borrowed `&dyn Dialect` isn't
    // held across the `.await` (which would make this future non-Send);
    // then run each (MySQL-safe via `run_ddl_idempotent`, which forgives a
    // re-run's duplicate-index error). MySQL has no `CREATE INDEX IF NOT
    // EXISTS`, so there the clause is left out; with it every statement
    // was a syntax error and the table got no index at all (#715).
    let index_sql: Vec<String> = {
        let d = pool.dialect();
        let if_not_exists = if d.supports_create_index_if_not_exists() {
            "IF NOT EXISTS "
        } else {
            ""
        };
        let table = d.quote_ident("cms_analytics_event");
        let mut v = Vec::new();
        for (name, col) in [
            ("idx_cms_analytics_created", "created_at"),
            ("idx_cms_analytics_visitor", "visitor_id"),
            ("idx_cms_analytics_session", "session_id"),
            ("idx_cms_analytics_path", "path"),
        ] {
            v.push(format!(
                "CREATE INDEX {if_not_exists}{} ON {} ({})",
                d.quote_ident(name),
                table,
                d.quote_ident(col),
            ));
        }
        v
    };
    for sql in &index_sql {
        // Best-effort: the table works without them, only slower.
        if let Err(e) = rustango::sql::run_ddl_idempotent(pool, sql).await {
            tracing::warn!(target: "rustango_cms::analytics", error = %e, %sql, "analytics index not created");
        }
    }
    Ok(())
}
