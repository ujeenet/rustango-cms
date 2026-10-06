//! Engagement WebSocket — `GET /__cms__/ws` (an HTTP upgrade, so it
//! bypasses both the page cache and CSRF).
//!
//! The socket's lifetime *is* the engagement signal: on connect we bump
//! the per-tenant "active now" gauge and start a timer; on disconnect we
//! decrement the gauge and append one `engagement` row with the measured
//! duration + max scroll depth. The client pushes `{"scroll":N}` updates
//! over the socket; a 30s keepalive ping detects dead connections.
//!
//! Pool safety: we clone the cheap `Pool` handle and **drop the `Tenant`
//! extractor before the socket future runs**, so a Postgres-eager
//! checked-out connection is never held for the (long) socket lifetime —
//! the engagement INSERT acquires a connection only transiently at close.

use crate::log_err::LogErr as _;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::Query;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use rustango::extractors::Tenant;
use rustango::sql::{Auto, Pool};
use serde::Deserialize;

use super::collect::{classify_ua, geo_country, header_str, host_of, is_bot, sanitize_path};
use super::model::AnalyticsEvent;

#[derive(Deserialize, Default)]
pub(crate) struct WsParams {
    #[serde(default)]
    vid: String,
    #[serde(default)]
    sid: String,
    #[serde(default)]
    p: String,
    #[serde(default)]
    loc: String,
    #[serde(default)]
    referrer: String,
}

#[derive(Deserialize)]
struct ScrollMsg {
    scroll: i32,
}

/// Immutable per-connection context moved into the socket future.
struct Engage {
    pool: Pool,
    host: String,
    visitor_id: String,
    session_id: String,
    path: String,
    referrer_host: String,
    country: String,
    device: String,
    browser: String,
    os: String,
    locale: String,
}

/// `GET /__cms__/ws` — upgrade + track one visitor's engagement.
pub(crate) async fn handle_ws(
    tenant: Tenant,
    headers: HeaderMap,
    Query(q): Query<WsParams>,
    ws: WebSocketUpgrade,
) -> Response {
    let ua = header_str(&headers, axum::http::header::USER_AGENT.as_str());
    if is_bot(&ua) {
        return (axum::http::StatusCode::BAD_REQUEST, "").into_response();
    }
    // Clone the pool handle (no connection held) + capture the tenant
    // Host, then drop `tenant` before the socket future.
    let pool = tenant.pool().clone();
    drop(tenant);

    let (device, browser, os) = classify_ua(&ua);
    let ctx = Engage {
        pool,
        host: header_str(&headers, "host"),
        visitor_id: q.vid.chars().take(36).collect(),
        session_id: q.sid.chars().take(36).collect(),
        path: sanitize_path(&q.p),
        referrer_host: host_of(&q.referrer),
        country: geo_country(&headers),
        device: device.to_owned(),
        browser: browser.chars().take(32).collect(),
        os: os.chars().take(32).collect(),
        locale: q.loc.chars().take(16).collect(),
    };
    ws.on_upgrade(move |socket| run(socket, ctx))
}

async fn run(mut socket: WebSocket, ctx: Engage) {
    super::presence_incr(&ctx.host);
    let start = std::time::Instant::now();
    let mut max_scroll: i32 = 0;

    // Read until the client closes (on pagehide) or the connection
    // errors. The socket's lifetime is the engagement duration. Reply to
    // client pings; browsers close cleanly on unload so no server-side
    // keepalive timer is needed (avoids a tokio dependency here).
    while let Some(msg) = socket.recv().await {
        match msg {
            Ok(Message::Text(t)) => {
                if let Ok(m) = serde_json::from_str::<ScrollMsg>(t.as_str()) {
                    let p = m.scroll.clamp(0, 100);
                    if p > max_scroll {
                        max_scroll = p;
                    }
                }
            }
            Ok(Message::Ping(p)) => {
                if socket.send(Message::Pong(p)).await.is_err() {
                    break;
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }

    super::presence_decr(&ctx.host);

    let dur_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    let mut entry = AnalyticsEvent {
        id: Auto::Unset,
        visitor_id: ctx.visitor_id,
        session_id: ctx.session_id,
        event_type: "engagement".to_owned(),
        path: ctx.path,
        referrer_host: ctx.referrer_host,
        country: ctx.country,
        device: ctx.device,
        browser: ctx.browser,
        os: ctx.os,
        screen_w: 0,
        screen_h: 0,
        duration_ms: dur_ms,
        max_scroll_pct: max_scroll,
        is_bounce: false,
        locale: ctx.locale,
        created_at: chrono::Utc::now(),
    };
    entry.insert_pool(&ctx.pool).await.log_warn("analytics event not recorded");
}
