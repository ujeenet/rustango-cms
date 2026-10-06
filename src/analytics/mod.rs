//! Default first-party web analytics (#analytics).
//!
//! Every rustango-cms site gets privacy-conscious engagement analytics
//! that keeps working when the HTML is served from a CDN and deeply
//! cached. Collection is driven entirely by a client beacon to endpoints
//! that are never cached:
//!
//! - **Pageview** — `POST /__cms__/collect` (CSRF-exempt, `no-store`),
//!   one row per page load. See [`collect`].
//! - **Engagement + "active now"** — a WebSocket at `/__cms__/ws`; the
//!   server times connect→disconnect and writes one engagement row on
//!   close, and keeps an in-memory per-tenant "active now" gauge. See
//!   [`ws`]. If the socket can't open, the beacon falls back to an
//!   unload `POST /__cms__/collect`.
//!
//! The beacon `<script>` is auto-injected into every rendered public
//! page (see `render.rs`), so consumers need no template change. The
//! one required consumer line is `CsrfConfig::exempt_prefix(COLLECT_PATH)`
//! (a sendBeacon POST can't carry an `X-CSRF-Token`, and a CDN-cached
//! page may never have received a CSRF cookie).
//!
//! Data model + boot table creation: [`model`]. Aggregates for the
//! admin dashboard (`/cms-admin/analytics`): [`query`]. Retention:
//! [`retention`].

pub mod collect;
pub mod model;
pub mod query;
pub mod retention;
pub mod ws;

pub use model::{ensure_table, AnalyticsEvent};

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Public collector endpoint (POST via `sendBeacon`). Referenced by the
/// consumer's `CsrfConfig::exempt_prefix` and by the beacon JS.
pub const COLLECT_PATH: &str = "/__cms__/collect";
/// Beacon script endpoint (GET, immutable-cached).
pub const BEACON_JS_PATH: &str = "/__cms__/a.js";
/// Engagement WebSocket endpoint (GET upgrade).
pub const WS_PATH: &str = "/__cms__/ws";
/// `SiteSetting` scope holding the analytics config `{ enabled, retention_days }`.
pub const SETTINGS_SCOPE: &str = "analytics";

const BEACON_JS: &str = include_str!("a.js");

/// FNV-1a fingerprint of the beacon JS, for the `?v=` cache-buster.
#[must_use]
pub fn beacon_asset_version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in BEACON_JS.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{h:x}")
    })
}

/// `GET /__cms__/a.js` — the beacon script, immutable-cached (version
/// busted via the `?v=` the injected tag carries).
pub async fn serve_beacon_js() -> impl axum::response::IntoResponse {
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/javascript; charset=utf-8"),
            ),
            (
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
        ],
        BEACON_JS,
    )
}

/// Whether analytics is enabled for this render, from the prefetched
/// `SiteSetting` scope→value map. Default **on** (inject unless the
/// `"analytics"` row explicitly sets `enabled: false`).
#[must_use]
pub fn enabled(site_settings: &HashMap<String, serde_json::Value>) -> bool {
    site_settings
        .get(SETTINGS_SCOPE)
        .and_then(|v| v.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        != Some(false)
}

/// Insert the beacon `<script>` immediately before the last `</body>`.
/// No-op when the tag is absent (fragments / non-HTML) — never appends
/// blindly. Runs during render, before the page cache stores the
/// response, so cached HTML carries the tag.
#[must_use]
pub fn inject_beacon(html: String) -> String {
    let Some(pos) = html.rfind("</body>") else {
        return html;
    };
    let tag = format!(
        "<script defer src=\"{}?v={}\"></script>",
        BEACON_JS_PATH,
        beacon_asset_version()
    );
    let mut out = String::with_capacity(html.len() + tag.len());
    out.push_str(&html[..pos]);
    out.push_str(&tag);
    out.push_str(&html[pos..]);
    out
}

// ---- "active now" presence (in-memory, per-tenant by Host) ----------

/// Process-global live-connection counts, keyed by tenant `Host`. This
/// is a gauge, not persisted — it resets on restart, which is fine for
/// "who's online right now".
fn presence() -> &'static Mutex<HashMap<String, u64>> {
    static P: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn presence_incr(host: &str) {
    if let Ok(mut m) = presence().lock() {
        *m.entry(host.to_owned()).or_insert(0) += 1;
    }
}

pub(crate) fn presence_decr(host: &str) {
    if let Ok(mut m) = presence().lock() {
        if let Some(c) = m.get_mut(host) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                m.remove(host);
            }
        }
    }
}

/// Current "active now" count for a tenant `Host`.
#[must_use]
pub fn presence_count(host: &str) -> u64 {
    presence()
        .lock()
        .ok()
        .and_then(|m| m.get(host).copied())
        .unwrap_or(0)
}
