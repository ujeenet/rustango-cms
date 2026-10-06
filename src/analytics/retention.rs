//! Analytics retention — delete events older than N days. Run per
//! tenant at boot (servers restart/redeploy regularly).

use rustango::core::SqlValue;

/// Default retention window when the tenant hasn't configured one.
pub const DEFAULT_RETENTION_DAYS: i64 = 365;

/// Resolve the retention window from the `"analytics"` `SiteSetting`
/// scope (`retention_days`), falling back to [`DEFAULT_RETENTION_DAYS`].
pub async fn retention_days(pool: &rustango::sql::Pool) -> i64 {
    match crate::site_setting::get(pool, super::SETTINGS_SCOPE).await {
        Ok(Some(s)) => s
            .value_json
            .get("retention_days")
            .and_then(serde_json::Value::as_i64)
            .filter(|d| *d > 0)
            .unwrap_or(DEFAULT_RETENTION_DAYS),
        _ => DEFAULT_RETENTION_DAYS,
    }
}

/// Delete events older than `days`. `days <= 0` is a no-op (never wipe
/// the whole table implicitly).
///
/// # Errors
/// Driver / SQL failures from the DELETE.
pub async fn prune(pool: &rustango::sql::Pool, days: i64) -> Result<(), rustango::sql::ExecError> {
    if days <= 0 {
        return Ok(());
    }
    let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
    let d = pool.dialect();
    let sql = format!(
        "DELETE FROM {} WHERE {} < {}",
        d.quote_ident("cms_analytics_event"),
        d.quote_ident("created_at"),
        d.placeholder(1),
    );
    // A typed bind on every dialect (#654): `created_at` is written by the
    // ORM, which SQLite stores as RFC3339 text, so a space-separated
    // cutoff compared lexically kept rows from the cutoff's own day.
    rustango::sql::raw_execute_pool(pool, &sql, vec![SqlValue::DateTime(cutoff)]).await?;
    Ok(())
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::analytics::model::AnalyticsEvent;
    use rustango::sql::{Auto, FetcherPool as _};

    fn event(path: &str, created_at: chrono::DateTime<chrono::Utc>) -> AnalyticsEvent {
        AnalyticsEvent {
            id: Auto::Unset,
            visitor_id: "v".into(),
            session_id: "s".into(),
            event_type: "pageview".into(),
            path: path.into(),
            referrer_host: String::new(),
            country: String::new(),
            device: String::new(),
            browser: String::new(),
            os: String::new(),
            screen_w: 0,
            screen_h: 0,
            duration_ms: 0,
            max_scroll_pct: 0,
            is_bounce: false,
            locale: String::new(),
            created_at,
        }
    }

    /// #654 — an event minutes past the cutoff is deleted on SQLite, even
    /// on the cutoff's own calendar day.
    #[tokio::test]
    async fn prune_deletes_right_up_to_the_cutoff() {
        let pool = rustango::sql::Pool::connect("sqlite::memory:")
            .await
            .expect("pool");
        crate::analytics::model::ensure_table(&pool).await.expect("table");
        let cutoff = chrono::Utc::now() - chrono::Duration::days(1);
        for (path, at) in [
            ("/old", cutoff - chrono::Duration::minutes(5)),
            ("/new", cutoff + chrono::Duration::minutes(5)),
        ] {
            event(path, at).insert_pool(&pool).await.expect("event");
        }
        prune(&pool, 1).await.expect("prune");
        let left: Vec<String> = AnalyticsEvent::objects()
            .fetch(&pool)
            .await
            .expect("fetch")
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(left, ["/new"]);
    }
}
