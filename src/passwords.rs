//! Password hashing off the async runtime (#693).
//!
//! `rustango::passwords` is synchronous argon2id, slow by design (tens of
//! milliseconds of CPU per call). Called inline in a handler it parks a
//! Tokio worker for that long, so a handful of concurrent anonymous
//! member-login or page-password posts stalled every other request on
//! the process. These wrappers run the same calls on the blocking pool.

use rustango::__private_runtime::tokio::task::spawn_blocking;
use rustango::passwords::PasswordError;

/// [`rustango::passwords::hash`] on the blocking pool.
///
/// # Errors
/// The hasher's error.
pub async fn hash(password: &str) -> Result<String, PasswordError> {
    let password = password.to_owned();
    joined(spawn_blocking(move || rustango::passwords::hash(&password)).await)
}

/// [`rustango::passwords::verify`] on the blocking pool.
///
/// # Errors
/// A stored hash the verifier can't parse.
pub async fn verify(password: &str, stored_hash: &str) -> Result<bool, PasswordError> {
    let (password, stored_hash) = (password.to_owned(), stored_hash.to_owned());
    joined(spawn_blocking(move || rustango::passwords::verify(&password, &stored_hash)).await)
}

/// [`rustango::passwords::verify_dummy`] on the blocking pool — the same
/// cost as a real verify, so an unknown account takes as long to refuse.
pub async fn verify_dummy(password: &str) {
    let password = password.to_owned();
    joined(spawn_blocking(move || rustango::passwords::verify_dummy(&password)).await);
}

/// A panic inside the hasher stays a panic in the caller, as it was when
/// the call ran inline.
fn joined<T>(r: Result<T, rustango::__private_runtime::tokio::task::JoinError>) -> T {
    r.unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use rustango::__private_runtime::tokio;

    /// On a single-threaded runtime an inline argon2 call would freeze
    /// every other task until it returned; off the runtime, a ticker keeps
    /// running while the hash is computed.
    #[tokio::test(flavor = "current_thread")]
    async fn hashing_does_not_block_the_runtime() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let t = Arc::clone(&ticks);
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                t.fetch_add(1, Ordering::Relaxed);
            }
        });
        let hash = super::hash("correct horse battery staple").await.expect("hash");
        let during = ticks.load(Ordering::Relaxed);
        assert!(super::verify("correct horse battery staple", &hash).await.expect("verify"));
        assert!(!super::verify("wrong", &hash).await.expect("verify"));
        ticker.abort();
        assert!(during >= 2, "the runtime stalled while hashing ({during} ticks)");
    }
}
