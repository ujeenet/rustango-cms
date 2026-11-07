//! `cargo run -- quickstart`: a local SQLite site in one command.
//!
//! The first run creates the registry, a `demo` site served at
//! demo.localhost, and an `admin` user (it asks for the password), then
//! starts the server. Later runs skip what already exists and just start
//! the server. Each step is the binary's own command, run as a child
//! process, so it behaves exactly like typing them one by one.

use std::error::Error;
use std::path::Path;
use std::process::Command;

pub const COMMAND: &str = "quickstart";

const TENANT: &str = "demo";
const TENANT_DB_FILE: &str = "var/demo.db";
const TENANT_DB_URL: &str = "sqlite:./var/demo.db?mode=rwc";
const HOST: &str = "demo.localhost";
const ADMIN: &str = "admin";
/// Written once the admin user exists, so a re-run does not ask again.
const DONE_MARKER: &str = "var/.quickstart-admin";

pub fn run() -> Result<(), Box<dyn Error>> {
    let url = std::env::var("DATABASE_URL")
        .map_err(|_| "DATABASE_URL is not set: copy .env.example to .env")?;
    if !url.starts_with("sqlite:") {
        return Err("quickstart sets up a local SQLite site; for PostgreSQL or MySQL \
                    follow the steps in the README"
            .into());
    }
    std::fs::create_dir_all("var")?;

    step(&["migrate-registry"])?;
    if !Path::new(TENANT_DB_FILE).exists() {
        step(&[
            "create-tenant",
            TENANT,
            "--mode",
            "database",
            "--database-url",
            TENANT_DB_URL,
            "--host-pattern",
            HOST,
        ])?;
    }
    if !Path::new(DONE_MARKER).exists() {
        println!("\nCreate the admin user `{ADMIN}` (at least 12 characters):");
        step(&["create-superuser", TENANT, ADMIN])?;
        std::fs::write(DONE_MARKER, "")?;
    }

    println!("\nSign in at http://{HOST}:8080/login as `{ADMIN}`.");
    // Plain localhost is the operator console: a separate login that
    // site users have no account on.
    println!("Use the {HOST} address: plain http://localhost:8080 is the operator console, where `{ADMIN}` can't sign in.");
    println!("Next time, `cargo run -- runserver` is enough.\n");
    step(&["runserver"])
}

/// Run one of this binary's own commands; stop at the first failure.
fn step(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new(std::env::current_exe()?).args(args).status()?;
    if !status.success() {
        return Err(format!("`{}` failed ({status})", args.join(" ")).into());
    }
    Ok(())
}
