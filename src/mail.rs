//! The site's outbound mailer, from the framework's `[mail]` settings.
//!
//! Member sign-up confirmations, member and admin password resets, and form
//! notifications all need a mailer, and the CMS cannot send anything until
//! the host hands one over. [`from_env`] builds it the framework's way: the
//! `[mail]` section of `config/*.toml`, or the matching environment variables,
//! which suit a server whose secrets live in the service manager:
//!
//! ```text
//! RUSTANGO__MAIL__BACKEND=smtp
//! RUSTANGO__MAIL__SMTP_HOST=smtp.example.com
//! RUSTANGO__MAIL__SMTP_PORT=587
//! RUSTANGO__MAIL__SMTP_TLS=starttls
//! RUSTANGO__MAIL__SMTP_USERNAME=me@example.com
//! RUSTANGO__MAIL__SMTP_PASSWORD=app-password
//! RUSTANGO__MAIL__FROM_ADDRESS=me@example.com
//! ```
//!
//! SMTP needs the framework's `email-smtp` feature in the host's `rustango`
//! dependency.
//!
//! ```ignore
//! let admin_public = match rustango_cms::mail::from_env() {
//!     Some(mail) => rustango_cms::admin::public_router_with_mailer(tera.clone(), mail.mailer, mail.from),
//!     None => rustango_cms::admin::public_router(tera.clone()),
//! };
//! ```

use std::sync::Arc;

use rustango::email::Mailer;

/// A mailer and the address its mail comes from.
#[derive(Clone)]
pub struct SiteMailer {
    /// Sends the mail.
    pub mailer: Arc<dyn Mailer>,
    /// The `From:` address; `[mail] from_address`, empty when unset.
    pub from: String,
}

/// The mailer named by `[mail] backend`, or `None` when no backend is set.
///
/// `None` is not an error: without a backend the CMS prints what it would
/// have sent to the server log, as it always has. A backend that is set but
/// cannot be built (SMTP without a host, or without the `email-smtp`
/// feature) is logged as an error and also gives `None`, so a broken mail
/// setting never stops the site from starting.
#[must_use]
pub fn from_env() -> Option<SiteMailer> {
    from_config_dir(std::path::Path::new("config"))
}

/// [`from_env`] reading `config_dir` instead of `./config`.
fn from_config_dir(config_dir: &std::path::Path) -> Option<SiteMailer> {
    let tier = rustango::config::Settings::current_env_tier();
    let loaded = match rustango::config::Settings::load_from(config_dir, &tier) {
        // A site with no `config/` folder is common; the `RUSTANGO__MAIL__*`
        // variables still count, read over empty defaults.
        Err(e) if e.is_missing_config() => env_only_settings(),
        other => other,
    };
    match loaded {
        Ok(settings) => from_settings(&settings.mail),
        Err(e) => {
            tracing::error!(target: "rustango_cms::mail", error = %e, "could not load settings; mail is off");
            None
        }
    }
}

/// Settings from `RUSTANGO__*` variables alone, for a site without
/// `config/default.toml`: the framework loader over an empty one.
fn env_only_settings() -> Result<rustango::config::Settings, rustango::config::ConfigError> {
    let dir = std::env::temp_dir().join(format!("rustango-cms-empty-config-{}", std::process::id()));
    let made = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(dir.join("default.toml"), ""));
    if let Err(e) = made {
        tracing::error!(target: "rustango_cms::mail", error = %e, "could not prepare empty settings; mail is off");
    }
    let settings = rustango::config::Settings::load_from(&dir, &rustango::config::Settings::current_env_tier());
    let _ = std::fs::remove_dir_all(&dir);
    settings
}

/// [`from_env`] for settings already loaded.
#[must_use]
pub fn from_settings(mail: &rustango::config::MailSettings) -> Option<SiteMailer> {
    mail.backend.as_ref()?;
    match rustango::email::from_settings(mail) {
        Ok(mailer) => {
            let from = mail.from_address.clone().unwrap_or_default();
            tracing::info!(
                target: "rustango_cms::mail",
                backend = mail.backend.as_deref().unwrap_or(""),
                from = %from,
                "outbound mail configured"
            );
            Some(SiteMailer { mailer, from })
        }
        Err(e) => {
            tracing::error!(target: "rustango_cms::mail", error = %e, "mail is configured but cannot be built; mail is off");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_backend_means_no_mailer() {
        assert!(from_settings(&rustango::config::MailSettings::default()).is_none());
    }

    #[test]
    fn a_memory_backend_is_built_with_its_from_address() {
        let mail = rustango::config::MailSettings {
            backend: Some("memory".into()),
            from_address: Some("site@example.com".into()),
            ..Default::default()
        };
        let built = from_settings(&mail).expect("memory backend builds");
        assert_eq!(built.from, "site@example.com");
    }

    #[test]
    fn mail_variables_count_without_a_config_folder() {
        // A deployed site often has no `config/` folder at all.
        let missing = std::env::temp_dir().join(format!("rcms-mail-test-no-config-{}", std::process::id()));
        std::env::set_var("RUSTANGO__MAIL__BACKEND", "memory");
        std::env::set_var("RUSTANGO__MAIL__FROM_ADDRESS", "env@example.com");
        let built = from_config_dir(&missing);
        std::env::remove_var("RUSTANGO__MAIL__BACKEND");
        std::env::remove_var("RUSTANGO__MAIL__FROM_ADDRESS");
        assert_eq!(built.expect("a mailer from the variables alone").from, "env@example.com");
    }

    #[test]
    fn an_unbuildable_backend_gives_none_instead_of_failing() {
        let mail = rustango::config::MailSettings {
            backend: Some("smtp".into()),
            ..Default::default()
        };
        assert!(from_settings(&mail).is_none());
    }
}
