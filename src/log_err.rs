//! Logging for best-effort side effects.
//!
//! A write the caller chooses not to fail on — a revision capture, a blob
//! delete, an analytics row — still has to leave a trace when it fails.
//! `let _ = write.await;` left none; `write.await.log_warn("…")` logs the
//! error with what was lost, and still carries on.

/// Log the error of a best-effort result instead of dropping it.
pub(crate) trait LogErr {
    /// Log a failure at `warn`, naming `lost` — what did not happen.
    fn log_warn(self, lost: &'static str);
}

impl<T, E: std::fmt::Display> LogErr for Result<T, E> {
    fn log_warn(self, lost: &'static str) {
        if let Err(e) = self {
            tracing::warn!(target: "rustango_cms::best_effort", error = %e, "{lost}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LogErr;

    #[test]
    fn a_failure_is_logged_and_carried_past() {
        Ok::<_, String>(1).log_warn("nothing lost");
        Err::<(), _>("disk full".to_owned()).log_warn("blob not deleted");
    }
}
