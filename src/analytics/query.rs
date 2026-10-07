//! Dashboard aggregates over `cms_analytics_event`.
//!
//! Built on the ORM's aggregate API (`rustango::core::aggregates`
//! + `QuerySet::{aggregate,values,annotate}`): typed columns, conditional
//! `.filter()` aggregates, and `count_distinct`. Results come back as
//! `Vec<HashMap<String, SqlValue>>` (the shape is dynamic), decoded by alias.
//!
//! Date filtering is typed (`created_at.gte(from)`) — which works across
//! dialects because `created_at` is stored as an RFC3339 timestamp (set at
//! insert, not via SQLite's space-formatted `CURRENT_TIMESTAMP`). The only
//! raw query is the per-day time series, whose `GROUP BY <day-bucket>`
//! expression isn't expressible through the column-based `.values()` builder.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rustango::core::aggregates::{count_all, count_distinct, sum};
use rustango::core::Column as _;
use rustango::core::SqlValue;
use rustango::sql::{ExecError, Pool};
use serde::Serialize;

use super::model::AnalyticsEvent;

/// A `(label, count)` group row (top pages, referrers, countries, devices).
#[derive(Serialize, Default)]
pub struct Bucket {
    pub label: String,
    pub count: i64,
}

/// One point in the per-day time series.
#[derive(Serialize, Default)]
pub struct DayPoint {
    pub day: String,
    pub pageviews: i64,
    pub visitors: i64,
}

/// Everything the dashboard renders for a date range (except `active_now`,
/// which the handler fills from the in-memory presence gauge).
#[derive(Serialize, Default)]
pub struct Metrics {
    pub unique_visitors: i64,
    pub pageviews: i64,
    pub sessions: i64,
    pub bounce_rate: i64, // percent 0-100
    pub avg_seconds: i64,
    pub top_pages: Vec<Bucket>,
    pub top_referrers: Vec<Bucket>,
    pub countries: Vec<Bucket>,
    pub devices: Vec<Bucket>,
    pub series: Vec<DayPoint>,
}

// ---- SqlValue decode helpers (aggregate dicts are dynamically typed) ----

fn as_i64(v: Option<&SqlValue>) -> i64 {
    match v {
        Some(SqlValue::I64(n)) => *n,
        Some(SqlValue::I32(n)) => i64::from(*n),
        Some(SqlValue::I16(n)) => i64::from(*n),
        Some(SqlValue::F64(n)) => *n as i64,
        Some(SqlValue::F32(n)) => f64::from(*n) as i64,
        // SUM over a BIGINT widens to NUMERIC on Postgres and DECIMAL on
        // MySQL, so a duration total arrives as a Decimal there (#716).
        Some(SqlValue::Decimal(d)) => d.trunc().to_string().parse().unwrap_or(0),
        _ => 0,
    }
}

fn as_string(v: Option<&SqlValue>) -> String {
    match v {
        Some(SqlValue::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Top-N `(label, count)` grouped by `col` over pageviews in the range.
/// `distinct` counts distinct visitors instead of rows (used for countries).
async fn top_n(
    pool: &Pool,
    col: &'static str,
    distinct: Option<&'static str>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    limit: i64,
) -> Vec<Bucket> {
    let agg = match distinct {
        Some(dc) => count_distinct(dc),
        None => count_all(),
    };
    let Ok(q) = AnalyticsEvent::objects()
        .where_(AnalyticsEvent::created_at.gte(from))
        .where_(AnalyticsEvent::created_at.lt(to))
        .where_(AnalyticsEvent::event_type.eq("pageview".to_owned()))
        .values(&[col])
        .annotate("c", agg.into())
        .order_by(&[("c", true)])
        .limit(limit)
        .compile()
    else {
        return Vec::new();
    };
    let rows = rustango::sql::fetch_aggregate_dict(pool, &q)
        .await
        .unwrap_or_default();
    rows.into_iter()
        .map(|r| Bucket {
            label: as_string(r.get(col)),
            count: as_i64(r.get("c")),
        })
        .collect()
}

/// Compute all dashboard metrics for `[from, to)`.
///
/// # Errors
/// Driver / SQL failures from the per-day series query. (The ORM aggregate
/// queries degrade to zeroes/empties on error rather than propagating.)
pub async fn compute(
    pool: &Pool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Metrics, ExecError> {
    // 1. Scalars — one aggregate query, conditional `.filter()` per metric.
    let (unique_visitors, pageviews, sessions, dur_sum, eng_count) = {
        let compiled = AnalyticsEvent::objects()
            .where_(AnalyticsEvent::created_at.gte(from))
            .where_(AnalyticsEvent::created_at.lt(to))
            .aggregate()
            .values(&[])
            .annotate(
                "uv",
                count_distinct("visitor_id")
                    .filter(AnalyticsEvent::event_type.eq("pageview".to_owned()))
                    .into(),
            )
            .annotate(
                "pv",
                count_all()
                    .filter(AnalyticsEvent::event_type.eq("pageview".to_owned()))
                    .into(),
            )
            .annotate(
                "sess",
                count_distinct("session_id")
                    .filter(AnalyticsEvent::event_type.eq("pageview".to_owned()))
                    .into(),
            )
            .annotate(
                "dur",
                sum("duration_ms")
                    .filter(AnalyticsEvent::event_type.eq("engagement".to_owned()))
                    .default(0_i64)
                    .into(),
            )
            .annotate(
                "engn",
                count_all()
                    .filter(AnalyticsEvent::event_type.eq("engagement".to_owned()))
                    .into(),
            )
            .compile();
        let row = match compiled {
            Ok(q) => rustango::sql::fetch_aggregate_dict(pool, &q)
                .await
                .unwrap_or_default()
                .into_iter()
                .next()
                .unwrap_or_default(),
            Err(_) => HashMap::new(),
        };
        (
            as_i64(row.get("uv")),
            as_i64(row.get("pv")),
            as_i64(row.get("sess")),
            as_i64(row.get("dur")),
            as_i64(row.get("engn")),
        )
    };
    let avg_seconds = if eng_count > 0 {
        dur_sum / eng_count / 1000
    } else {
        0
    };

    // 2. Bounce — a bounce is a single-pageview session whose longest
    //    engagement is under 10s. Counted in SQL (#653): grouping by session
    //    and counting in Rust loaded one row per session in the range to
    //    produce one percentage. COUNT, not SUM, so every backend returns a
    //    plain integer (SUM is DECIMAL on MySQL, #716).
    let (bounced, total_sessions) = {
        let d = pool.dialect();
        let t = d.quote_ident("cms_analytics_event");
        let created = d.quote_ident("created_at");
        let session = d.quote_ident("session_id");
        let etype = d.quote_ident("event_type");
        let dur = d.quote_ident("duration_ms");
        let sql = format!(
            "SELECT COUNT(*) AS total, \
             COUNT(CASE WHEN s.pv = 1 AND COALESCE(s.maxdur, 0) < 10000 THEN 1 END) AS bounced \
             FROM (SELECT {session}, \
                   COUNT(CASE WHEN {etype} = 'pageview' THEN 1 END) AS pv, \
                   MAX(CASE WHEN {etype} = 'engagement' THEN {dur} END) AS maxdur \
                   FROM {t} WHERE {created} >= {p1} AND {created} < {p2} \
                   GROUP BY {session}) s",
            p1 = d.placeholder(1),
            p2 = d.placeholder(2),
        );
        rustango::sql::raw_query_pool::<(i64, i64)>(
            &sql,
            vec![SqlValue::DateTime(from), SqlValue::DateTime(to)],
            pool,
        )
        .await
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .map_or((0, 0), |(total, bounced)| (bounced, total))
    };
    let bounce_rate = if total_sessions > 0 {
        bounced * 100 / total_sessions
    } else {
        0
    };

    // 3. Top-N breakdowns.
    let top_pages = top_n(pool, "path", None, from, to, 20).await;
    let mut top_referrers = top_n(pool, "referrer_host", None, from, to, 30).await;
    top_referrers.retain(|b| !b.label.is_empty());
    top_referrers.truncate(20);
    let countries = top_n(pool, "country", Some("visitor_id"), from, to, 30).await;
    let devices = top_n(pool, "device", None, from, to, 10).await;

    // 4. Per-day series — the one raw query. `DATE()`/`strftime`/`DATE_FORMAT`
    //    bucketing isn't expressible via the column-based `.values()` builder.
    //    `created_at` is RFC3339, so the range binds as a plain `DateTime`.
    let series = {
        let d = pool.dialect();
        let t = d.quote_ident("cms_analytics_event");
        let created = d.quote_ident("created_at");
        let visitor = d.quote_ident("visitor_id");
        let etype = d.quote_ident("event_type");
        let day = match d.name() {
            "sqlite" => format!("strftime('%Y-%m-%d', {created})"),
            "mysql" => format!("DATE_FORMAT({created}, '%Y-%m-%d')"),
            // `to_char` on a timestamptz formats in the session TimeZone;
            // pin it to UTC, as MySQL and SQLite already bucket (#744).
            _ => format!("to_char({created} AT TIME ZONE 'UTC', 'YYYY-MM-DD')"),
        };
        let sql = format!(
            "SELECT {day} AS d, COUNT(*) AS pv, COUNT(DISTINCT {visitor}) AS uv \
             FROM {t} WHERE {created} >= {p1} AND {created} < {p2} AND {etype} = 'pageview' \
             GROUP BY {day} ORDER BY d ASC",
            p1 = d.placeholder(1),
            p2 = d.placeholder(2),
        );
        rustango::sql::raw_query_pool::<(String, i64, i64)>(
            &sql,
            vec![SqlValue::DateTime(from), SqlValue::DateTime(to)],
            pool,
        )
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(day, pageviews, visitors)| DayPoint {
            day,
            pageviews,
            visitors,
        })
        .collect()
    };

    Ok(Metrics {
        unique_visitors,
        pageviews,
        sessions,
        bounce_rate,
        avg_seconds,
        top_pages,
        top_referrers,
        countries,
        devices,
        series,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A SUM over BIGINT comes back as a Decimal on Postgres and
    /// MySQL; reading it as 0 made "avg. time on page" 0s there.
    #[test]
    fn a_decimal_sum_reads_as_its_integer_value() {
        let d: rustango::__rust_decimal::Decimal = "3000000.00".parse().expect("decimal");
        assert_eq!(as_i64(Some(&SqlValue::Decimal(d))), 3_000_000);
        assert_eq!(as_i64(Some(&SqlValue::I64(7))), 7);
        assert_eq!(as_i64(None), 0);
    }
}
