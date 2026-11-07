//! Signed preview tokens for decoupled / headless frontends.
//!
//! The public API serves **published** pages only (404 on drafts). A
//! preview token lets an authorized decoupled frontend fetch a page's
//! **draft** content: the admin mints a short-lived signed token for a
//! page, the frontend appends it to its API request
//! (`/api/v2/pages/{id}/?preview_token=…`), and the API serves the
//! page regardless of status when the token verifies.
//!
//! Token shape: `"{page_id}.{expires_unix}.{sig}"`, where `sig` is the
//! first 16 hex chars of HMAC-SHA256 over
//! `"{org_id}:{tenant_slug}:{page_id}.{expires_unix}"` keyed by
//! [`crate::signing::secret`]. The expiry is signed, so it can't be
//! extended by tampering. The tenant is signed but not carried: every
//! tenant numbers its pages from 1, so a token must verify only on the
//! tenant that minted it, and not on a later tenant reusing its slug.

use rustango::tenancy::TenantSlug;

/// Mint a preview token for `page_id` that expires at `expires_unix`
/// (UNIX seconds).
#[must_use]
pub fn mint(tenant: &TenantSlug, page_id: i64, expires_unix: i64) -> String {
    mint_with_key(crate::signing::secret(), tenant, page_id, expires_unix)
}

/// Verify `token` for `tenant` at `now_unix`; returns the page id
/// when the signature is valid for this tenant AND the token hasn't
/// expired. `None` when the token is malformed/forged, was minted for
/// another tenant, or has expired.
#[must_use]
pub fn verify(tenant: &TenantSlug, token: &str, now_unix: i64) -> Option<i64> {
    verify_with_key(crate::signing::secret(), tenant, token, now_unix)
}

fn mint_with_key(key: &[u8], tenant: &TenantSlug, page_id: i64, expires_unix: i64) -> String {
    let payload = format!("{page_id}.{expires_unix}");
    let sig = signature(key, tenant, &payload);
    format!("{payload}.{sig}")
}

fn verify_with_key(key: &[u8], tenant: &TenantSlug, token: &str, now_unix: i64) -> Option<i64> {
    let mut parts = token.splitn(3, '.');
    let page_id: i64 = parts.next()?.parse().ok()?;
    let expires: i64 = parts.next()?.parse().ok()?;
    let sig = parts.next()?;
    if now_unix > expires {
        return None; // expired
    }
    let payload = format!("{page_id}.{expires}");
    let expected = signature(key, tenant, &payload);
    if crate::signing::constant_time_eq(sig.as_bytes(), expected.as_bytes()) {
        Some(page_id)
    } else {
        None
    }
}

/// 16-hex (64-bit) MAC over the payload — short + URL-friendly, ample
/// against forgery for a short-lived token.
fn signature(key: &[u8], tenant: &TenantSlug, payload: &str) -> String {
    let signed = format!("{}:{}:{payload}", tenant.org_id, tenant.slug);
    crate::signing::hmac_sha256_hex(key, signed.as_bytes())[..16].to_owned()
}

#[cfg(test)]
mod tests {
    use super::{mint_with_key, verify_with_key, TenantSlug};

    const KEY: &[u8] = b"test-secret";

    fn t(slug: &str) -> TenantSlug {
        TenantSlug::new(1, slug)
    }

    #[test]
    fn round_trips_within_expiry() {
        let token = mint_with_key(KEY, &t("acme"), 42, 2_000);
        assert_eq!(verify_with_key(KEY, &t("acme"), &token, 1_000), Some(42));
        // exactly at expiry is still valid (now <= expires).
        assert_eq!(verify_with_key(KEY, &t("acme"), &token, 2_000), Some(42));
    }

    #[test]
    fn rejects_expired() {
        let token = mint_with_key(KEY, &t("acme"), 7, 1_000);
        assert_eq!(verify_with_key(KEY, &t("acme"), &token, 1_001), None);
    }

    #[test]
    fn rejects_wrong_key_and_tamper() {
        let token = mint_with_key(KEY, &t("acme"), 7, 9_999);
        assert_eq!(verify_with_key(b"other-key", &t("acme"), &token, 0), None);
        // Tampering the page id breaks the signature (no privilege
        // escalation to another page).
        let forged = token.replacen("7.", "8.", 1);
        assert_eq!(verify_with_key(KEY, &t("acme"), &forged, 0), None);
        // Extending the expiry breaks the signature.
        let extended = mint_with_key(KEY, &t("acme"), 7, 9_999).replace("9999", "999999");
        assert_eq!(verify_with_key(KEY, &t("acme"), &extended, 50_000), None);
    }

    #[test]
    fn a_token_minted_on_one_tenant_does_not_verify_on_another() {
        // Page ids restart at 1 per tenant, so "page 7" exists on both.
        let token = mint_with_key(KEY, &t("acme"), 7, 9_999);
        assert_eq!(verify_with_key(KEY, &t("acme"), &token, 0), Some(7));
        assert_eq!(verify_with_key(KEY, &t("globex"), &token, 0), None);
    }

    #[test]
    fn a_token_does_not_verify_on_a_new_tenant_reusing_the_slug() {
        let token = mint_with_key(KEY, &t("acme"), 7, 9_999);
        assert_eq!(verify_with_key(KEY, &TenantSlug::new(2, "acme"), &token, 0), None);
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(verify_with_key(KEY, &t("acme"), "garbage", 0), None);
        assert_eq!(verify_with_key(KEY, &t("acme"), "7.notanumber.abc", 0), None);
        assert_eq!(verify_with_key(KEY, &t("acme"), "7.1000", 0), None); // missing sig
    }
}
