//! Minimal HMAC-SHA256 used across the CMS for short signed tokens
//! (view-restriction grants #294, signed rendition URLs #425).
//!
//! The framework's CSRF module has an `hmac_sha256` but it's
//! `pub(crate)`; rather than pull the `hmac` + `sha2` crates twice, we
//! implement HMAC over the `sha2` crate the workspace already depends
//! on. One shared impl so the two call sites can't drift.

use sha2::{Digest, Sha256};

/// Process-wide CMS secret used to sign short-lived tokens (preview
/// tokens #430, and any future signed CMS artifact). `None` = unset →
/// the dependent feature stays off.
static SECRET: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();

/// Set the CMS signing secret. Call once at startup, before anything is
/// served; the key must be stable across restarts. Sourced from the
/// `RCMS_SECRET_KEY` env var when this isn't called.
///
/// The first read fixes the secret for the life of the process, so a call
/// after it — or a second call with another key — cannot take effect, and
/// is logged as an error rather than dropped (#697).
pub fn set_secret(key: impl Into<Vec<u8>>) {
    let key = key.into();
    if let Err(rejected) = SECRET.set(Some(key)) {
        if SECRET.get() != Some(&rejected) {
            tracing::error!(
                target: "rustango_cms::signing",
                "set_secret called after the secret was already fixed (set earlier, or read \
                 from RCMS_SECRET_KEY); this call has no effect — set it before serving"
            );
        }
    }
}

/// The CMS signing secret, or `None` when unset (feature off). Falls
/// back to the `RCMS_SECRET_KEY` env var on first read.
#[must_use]
pub fn secret() -> Option<&'static [u8]> {
    SECRET
        .get_or_init(|| {
            crate::config::var("SECRET_KEY").map(String::into_bytes)
        })
        .as_deref()
}

/// HMAC-SHA256(`key`, `msg`) as lowercase hex (64 chars). Callers that
/// want a shorter token truncate the result (a prefix of a hex HMAC is
/// itself a valid MAC at reduced bit-strength).
#[must_use]
pub fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    let bytes = hmac_sha256(key, msg);
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Constant-time comparison of two byte slices — use when checking a
/// supplied MAC against the expected one so timing doesn't leak how
/// many leading bytes matched.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;
    let mut key_block = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let hash = Sha256::digest(key);
        key_block[..32].copy_from_slice(&hash);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0u8; BLOCK_SIZE];
    let mut opad = [0u8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad[i] = key_block[i] ^ 0x36;
        opad[i] = key_block[i] ^ 0x5c;
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_known_vector() {
        // RFC 4231 test case 1: key = 0x0b*20, data = "Hi There".
        let key = [0x0b_u8; 20];
        let out = hmac_sha256_hex(&key, b"Hi There");
        assert_eq!(
            out,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn different_key_or_msg_changes_mac() {
        assert_ne!(hmac_sha256_hex(b"k1", b"m"), hmac_sha256_hex(b"k2", b"m"));
        assert_ne!(hmac_sha256_hex(b"k", b"m1"), hmac_sha256_hex(b"k", b"m2"));
    }

    #[test]
    fn constant_time_eq_behaves_like_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
