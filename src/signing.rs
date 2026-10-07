//! Minimal HMAC-SHA256 used across the CMS for short signed tokens
//! (view-restriction grants, signed rendition URLs).
//!
//! The framework's CSRF module has an `hmac_sha256` but it's
//! `pub(crate)`; rather than pull the `hmac` + `sha2` crates twice, we
//! implement HMAC over the `sha2` crate the workspace already depends
//! on. One shared impl so the two call sites can't drift.

use sha2::{Digest, Sha256};

/// Process-wide CMS secret used to sign short-lived tokens: preview
/// tokens, view-restriction grants, password-reset links and the admin's
/// flash cookie (the last two through [`derived_key`]).
static SECRET: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

/// Where the CMS keeps the key it generates when `RCMS_SECRET_KEY` is not
/// set, relative to the working directory. Several replicas need either
/// the env var or this file on shared storage, so they sign alike.
pub const GENERATED_KEY_PATH: &str = "./var/.rustango_cms_signing.key";

/// Set the CMS signing secret. Call once at startup, before anything is
/// served; the key must be stable across restarts. Sourced from the
/// `RCMS_SECRET_KEY` env var, then [`GENERATED_KEY_PATH`], when this
/// isn't called.
///
/// The first read fixes the secret for the life of the process, so a call
/// after it — or a second call with another key — cannot take effect, and
/// is logged as an error rather than dropped.
pub fn set_secret(key: impl Into<Vec<u8>>) {
    let key = key.into();
    if let Err(rejected) = SECRET.set(key) {
        if SECRET.get() != Some(&rejected) {
            tracing::error!(
                target: "rustango_cms::signing",
                "set_secret called after the secret was already fixed (set earlier, or read \
                 from RCMS_SECRET_KEY); this call has no effect — set it before serving"
            );
        }
    }
}

/// The CMS signing secret: [`set_secret`]'s key, else `RCMS_SECRET_KEY`,
/// else a 32-byte key generated once and kept at [`GENERATED_KEY_PATH`],
/// so every signed feature works without configuration.
#[must_use]
pub fn secret() -> &'static [u8] {
    SECRET.get_or_init(|| {
        crate::config::var("SECRET_KEY")
            .map(String::into_bytes)
            .unwrap_or_else(|| load_or_generate(std::path::Path::new(GENERATED_KEY_PATH)))
    })
}

/// A key for one purpose, derived from [`secret`], so a value signed for
/// one feature never verifies in another.
#[must_use]
pub fn derived_key(purpose: &str) -> Vec<u8> {
    hmac_sha256(secret(), purpose.as_bytes()).to_vec()
}

/// Read the key at `path`, or generate 32 bytes from the OS CSPRNG and
/// persist them atomically, so signatures survive restarts. If the file
/// can't be written the key lives for this process only.
fn load_or_generate(path: &std::path::Path) -> Vec<u8> {
    if let Ok(bytes) = std::fs::read(path) {
        if bytes.len() >= 32 {
            return bytes;
        }
    }
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS CSPRNG unavailable");
    let buf = buf.to_vec();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &buf).is_ok() && std::fs::rename(&tmp, path).is_ok() {
        tracing::info!(
            target: "rustango_cms::signing",
            path = %path.display(),
            "generated the CMS signing key (set RCMS_SECRET_KEY to choose your own)"
        );
    } else {
        tracing::warn!(
            target: "rustango_cms::signing",
            path = %path.display(),
            "could not persist the generated CMS signing key; signed links \
             (previews, password resets) stop working at the next restart"
        );
    }
    buf
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

    /// With no `RCMS_SECRET_KEY`, the generated key is kept on disk, so a
    /// restart — or a second replica reading the same file — signs alike.
    #[test]
    fn a_generated_key_is_persisted_and_read_back() {
        let dir = std::env::temp_dir().join(format!("rcms-signing-{}", std::process::id()));
        let path = dir.join("var").join("key");
        let first = load_or_generate(&path);
        assert_eq!(first.len(), 32);
        assert_eq!(load_or_generate(&path), first, "a second read returns the stored key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derived_keys_differ_per_purpose() {
        set_secret(b"test-secret".to_vec());
        assert_ne!(derived_key("rcms:flash"), derived_key("rcms:password-reset"));
        assert_ne!(derived_key("rcms:flash"), secret().to_vec());
    }
}
