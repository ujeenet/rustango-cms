//! Public analytics collector — `POST /__cms__/collect`.
//!
//! Receives the beacon JSON (via `navigator.sendBeacon`), enriches it
//! server-side (country from a CDN header, device/browser/OS from the
//! User-Agent), and appends one `cms_analytics_event` row. Always
//! returns `204` + `Cache-Control: no-store`. Bots, prefetches, and
//! Do-Not-Track / Global-Privacy-Control requests are silently dropped.
//! No raw IP is stored.

use crate::log_err::LogErr as _;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use rustango::extractors::Tenant;
use rustango::sql::Auto;
use serde::Deserialize;

use super::model::AnalyticsEvent;

/// The beacon payload (all fields optional / defaulted — never trust the client).
#[derive(Deserialize, Default)]
struct Beacon {
    #[serde(default)]
    t: String,
    #[serde(default)]
    vid: String,
    #[serde(default)]
    sid: String,
    #[serde(default)]
    p: String,
    #[serde(default)]
    referrer: String,
    #[serde(default)]
    sw: i32,
    #[serde(default)]
    sh: i32,
    #[serde(default)]
    dur: i64,
    #[serde(default)]
    scroll: i32,
    #[serde(default)]
    bounce: bool,
    #[serde(default)]
    loc: String,
}

/// `POST /__cms__/collect` — record a pageview or (fallback) engagement event.
pub(crate) async fn handle_collect(
    State(_state): State<crate::router::PublicState>,
    tenant: Tenant,
    headers: HeaderMap,
    req: axum::extract::Request,
) -> Response {
    let pool = tenant.pool();
    let ua = header_str(&headers, axum::http::header::USER_AGENT.as_str());

    // Cheap drops before reading the body.
    if is_bot(&ua) || is_prefetch(&headers) || privacy_optout(&headers) {
        return no_content();
    }

    let bytes = axum::body::to_bytes(req.into_body(), 8 * 1024)
        .await
        .unwrap_or_default();
    let Ok(b) = serde_json::from_slice::<Beacon>(&bytes) else {
        return no_content();
    };

    let (device, browser, os) = classify_ua(&ua);
    let mut entry = AnalyticsEvent {
        id: Auto::Unset,
        visitor_id: b.vid.chars().take(36).collect(),
        session_id: b.sid.chars().take(36).collect(),
        event_type: if b.t == "engagement" {
            "engagement".to_owned()
        } else {
            "pageview".to_owned()
        },
        path: sanitize_path(&b.p),
        referrer_host: host_of(&b.referrer),
        country: geo_country(&headers),
        device: device.to_owned(),
        browser: browser.chars().take(32).collect(),
        os: os.chars().take(32).collect(),
        screen_w: b.sw.clamp(0, 100_000),
        screen_h: b.sh.clamp(0, 100_000),
        duration_ms: b.dur.max(0),
        max_scroll_pct: b.scroll.clamp(0, 100),
        is_bounce: b.bounce,
        locale: b.loc.chars().take(16).collect(),
        created_at: chrono::Utc::now(),
    };
    // Best-effort; analytics must never surface an error to the client.
    entry.insert_pool(pool).await.log_warn("analytics event not recorded");
    no_content()
}

/// `204 No Content` + `Cache-Control: no-store` (never cached anywhere).
pub(crate) fn no_content() -> Response {
    use axum::http::{header, HeaderValue, StatusCode};
    let mut r = Response::new(axum::body::Body::empty());
    *r.status_mut() = StatusCode::NO_CONTENT;
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

// ---- shared enrichment helpers (reused by the WS handler) -----------

pub(crate) fn header_str(h: &HeaderMap, name: &str) -> String {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned()
}

/// Country from common CDN geo headers; `""` when none present/valid.
pub(crate) fn geo_country(h: &HeaderMap) -> String {
    for name in [
        "cf-ipcountry",
        "x-vercel-ip-country",
        "x-country-code",
        "x-appengine-country",
        "fly-region",
    ] {
        if let Some(v) = h.get(name).and_then(|v| v.to_str().ok()) {
            let c = v.trim().to_ascii_uppercase();
            if c.len() == 2 && c != "XX" && c.bytes().all(|b| b.is_ascii_alphabetic()) {
                return c;
            }
        }
    }
    String::new()
}

/// Dependency-free User-Agent classifier → (device, browser, os).
pub(crate) fn classify_ua(ua: &str) -> (&'static str, &'static str, &'static str) {
    let l = ua.to_ascii_lowercase();
    let device = if l.contains("ipad") || (l.contains("tablet") && !l.contains("mobi")) {
        "tablet"
    } else if l.contains("mobi") || l.contains("iphone") || l.contains("android") {
        "mobile"
    } else {
        "desktop"
    };
    let browser = if l.contains("edg/") || l.contains("edga/") || l.contains("edgios/") {
        "Edge"
    } else if l.contains("opr/") || l.contains("opera") {
        "Opera"
    } else if l.contains("firefox") || l.contains("fxios") {
        "Firefox"
    } else if l.contains("chrome") || l.contains("crios") {
        "Chrome"
    } else if l.contains("safari") {
        "Safari"
    } else {
        "Other"
    };
    let os = if l.contains("windows") {
        "Windows"
    } else if l.contains("android") {
        "Android"
    } else if l.contains("iphone") || l.contains("ipad") || l.contains("ios ") {
        "iOS"
    } else if l.contains("mac os") || l.contains("macintosh") {
        "macOS"
    } else if l.contains("linux") {
        "Linux"
    } else {
        "Other"
    };
    (device, browser, os)
}

/// Known crawler / preview / automation agents (and empty UA) → drop.
pub(crate) fn is_bot(ua: &str) -> bool {
    if ua.trim().is_empty() {
        return true;
    }
    let l = ua.to_ascii_lowercase();
    [
        "bot",
        "crawl",
        "spider",
        "slurp",
        "headless",
        "lighthouse",
        "pingdom",
        "preview",
        "facebookexternalhit",
        "python-requests",
        "curl/",
        "wget",
    ]
    .iter()
    .any(|p| l.contains(p))
}

/// Prefetch / prerender navigations (Chrome `Sec-Purpose`, legacy `Purpose`).
pub(crate) fn is_prefetch(h: &HeaderMap) -> bool {
    h.get("sec-purpose")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("prefetch") || v.contains("prerender"))
        || h.get("purpose").and_then(|v| v.to_str().ok()) == Some("prefetch")
}

/// Honor Do-Not-Track (`DNT: 1`) and Global Privacy Control
/// (`Sec-GPC: 1`) by default. The beacon JS also self-suppresses, so
/// this is a server-side backstop for non-JS clients.
pub(crate) fn privacy_optout(h: &HeaderMap) -> bool {
    h.get("dnt").and_then(|v| v.to_str().ok()) == Some("1")
        || h.get("sec-gpc").and_then(|v| v.to_str().ok()) == Some("1")
}

/// Same-origin path only — strip any accidental scheme/host and the query.
pub(crate) fn sanitize_path(p: &str) -> String {
    let no_scheme = p.split("://").last().unwrap_or(p);
    // If a host slipped in, keep from the first '/'.
    let path = match no_scheme.find('/') {
        Some(i) if no_scheme.contains("://") || !p.starts_with('/') => &no_scheme[i..],
        _ => p,
    };
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let path = if path.is_empty() { "/" } else { path };
    path.chars().take(512).collect()
}

/// Referrer → host only (privacy; also keeps the column small).
pub(crate) fn host_of(referrer: &str) -> String {
    if referrer.is_empty() {
        return String::new();
    }
    let after = referrer.split("://").nth(1).unwrap_or(referrer);
    let host = after.split(['/', '?', '#']).next().unwrap_or("");
    // Drop any userinfo@ and :port.
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    host.chars().take(255).collect()
}
