//! Built-in destinations.
//!
//! Each is a small adapter: take a [`Notification`](super::Notification)
//! and speak one service's dialect. They are registered at compile time,
//! so a host app adding its own sits alongside them with no changes here.

mod email;
mod slack;
mod teams;
mod telegram;
mod webhook;

pub use email::EmailChannel;
pub use slack::SlackChannel;
pub use teams::TeamsChannel;
pub use telegram::TelegramChannel;
pub use webhook::WebhookChannel;

/// A client for one delivery to `url`, refusing internal destinations.
///
/// A channel's URL is whatever its target was configured with, so without a
/// guard the server POSTs, on every notification, to any address that URL
/// names — loopback, the private network, a cloud metadata endpoint. The
/// host is resolved here and refused if any address it resolves to is
/// internal; the connection is then pinned to the vetted address, so a
/// second DNS answer can't swap it, and redirects are not followed.
///
/// `RCMS_NOTIFY_ALLOW_PRIVATE=1` lifts the address check, for deployments
/// that deliver to endpoints on their own network.
///
/// # Errors
/// [`DeliverError::Permanent`] for a bad or refused URL; a DNS failure is
/// [`DeliverError::Transient`].
pub(crate) async fn client_for(
    url: &str,
) -> Result<rustango::http_client::HttpClient, super::DeliverError> {
    use super::DeliverError;
    let parsed = reqwest::Url::parse(url.trim())
        .map_err(|e| DeliverError::Permanent(format!("invalid URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(DeliverError::Permanent("only http and https URLs are delivered to".to_owned()));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| DeliverError::Permanent("URL has no host".to_owned()))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs: Vec<std::net::SocketAddr> =
        rustango::__private_runtime::tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| DeliverError::Transient(format!("resolve {host}: {e}")))?
            .collect();
    let allow_private = crate::config::flag("NOTIFY_ALLOW_PRIVATE");
    if !allow_private {
        if let Some(bad) = addrs.iter().find(|a| !is_public(a.ip())) {
            return Err(DeliverError::Permanent(format!(
                "refusing to deliver to {host}: it resolves to the internal address {}",
                bad.ip()
            )));
        }
    }
    let Some(pinned) = addrs.first().copied() else {
        return Err(DeliverError::Transient(format!("{host} resolved to no address")));
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .resolve(&host, pinned)
        .build()
        .map_err(|e| DeliverError::Permanent(format!("HTTP client unavailable: {e}")))?;
    Ok(rustango::http_client::HttpClient::from_reqwest(client))
}

/// Whether `ip` is a public destination: not loopback, private, link-local,
/// carrier-grade NAT, multicast, broadcast, unspecified or documentation
/// space (the metadata endpoints live in link-local).
pub(crate) fn is_public(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xC0) == 64))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// Map an HTTP status onto retry semantics.
///
/// The split matters more than it looks: a 401/404 means the webhook was
/// revoked or the channel deleted, and retrying it five times just delays
/// the failure that tells someone to fix it. A 429 or 5xx is exactly what
/// the outbox exists for.
pub(crate) fn classify(status: u16, body: &str) -> super::DeliverError {
    let msg = format!("HTTP {status}: {}", body.chars().take(200).collect::<String>());
    if status == 408 || status == 429 || status >= 500 {
        super::DeliverError::Transient(msg)
    } else {
        super::DeliverError::Permanent(msg)
    }
}

#[cfg(test)]
mod egress_tests {
    use super::is_public;

    #[test]
    fn internal_addresses_are_refused() {
        for ip in [
            "127.0.0.1", "10.0.0.5", "172.16.1.1", "192.168.1.1", "169.254.169.254", "100.64.0.1",
            "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:127.0.0.1", "::ffff:169.254.169.254",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} must be refused");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip} is public");
        }
    }

    #[tokio::test]
    async fn a_url_naming_an_internal_host_is_refused() {
        for url in ["http://127.0.0.1:9/hook", "http://169.254.169.254/latest/meta-data", "http://[::1]/x", "ftp://example.com/x"] {
            assert!(super::client_for(url).await.is_err(), "{url}");
        }
    }
}
