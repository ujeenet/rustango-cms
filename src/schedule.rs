//! Background driver for scheduled publishing and expiry (#680).
//!
//! [`crate::page::run_schedule_sweep`] flips `scheduled` pages live and
//! takes expired ones down, but nothing ran it except an editor opening
//! the admin page list — on a quiet site a page could sit past its go-live
//! time indefinitely. [`spawn_schedule_sweeper`] runs it on a timer for
//! every active tenant.
//!
//! The public resolver no longer depends on this to decide visibility —
//! it checks the dates itself ([`crate::resolver::visible_now`]). The
//! sweeper persists the status change and purges the page cache, so
//! listings, feeds and cached pages catch up too.

use std::sync::Arc;
use std::time::Duration;

use rustango::sql::sqlx;
use rustango::tenancy::{Org, TenantPools};

use crate::cache_invalidate::PageCacheInvalidator;

/// Run the schedule sweep for every active tenant every `interval`.
///
/// Call it once at boot with the registry pool — the same pool the
/// `Cli::seed` hook receives — and the host's page-cache invalidator
/// (`cache_invalidate::noop()` when the host has no page cache). A tenant
/// whose sweep fails is logged and retried next tick; it never stops the
/// others. Returns immediately; the loop runs on the Tokio runtime.
pub fn spawn_schedule_sweeper(
    registry: &rustango::sql::Pool,
    interval: Duration,
    invalidator: Arc<dyn PageCacheInvalidator>,
) {
    let registry = registry.clone();
    rustango::__private_runtime::tokio::spawn(async move {
        let mut tick = rustango::__private_runtime::tokio::time::interval(interval);
        loop {
            tick.tick().await;
            sweep_all(&registry, &invalidator).await;
        }
    });
}

/// One pass over every active tenant.
async fn sweep_all(registry: &rustango::sql::Pool, invalidator: &Arc<dyn PageCacheInvalidator>) {
    match registry {
        #[cfg(feature = "postgres")]
        rustango::sql::Pool::Postgres(pg) => {
            sweep_orgs(TenantPools::<sqlx::Postgres>::new(pg.clone()), invalidator).await;
        }
        #[cfg(feature = "sqlite")]
        rustango::sql::Pool::Sqlite(sq) => {
            sweep_orgs(TenantPools::<sqlx::Sqlite>::new(sq.clone()), invalidator).await;
        }
        #[cfg(feature = "mysql")]
        rustango::sql::Pool::Mysql(my) => {
            sweep_orgs(TenantPools::<sqlx::MySql>::new(my.clone()), invalidator).await;
        }
    }
}

async fn sweep_orgs<DB>(pools: TenantPools<DB>, invalidator: &Arc<dyn PageCacheInvalidator>)
where
    DB: sqlx::Database,
    rustango::sql::Pool: From<sqlx::Pool<DB>>,
{
    use rustango::sql::FetcherPool as _;
    let orgs: Vec<Org> = match Org::objects().fetch(&pools.registry_pool()).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(target: "rustango_cms::schedule", error = %e, "could not list tenants");
            return;
        }
    };
    for org in orgs.into_iter().filter(|o| o.active) {
        let pool = match pools.scoped_pool_dyn(&org).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(target: "rustango_cms::schedule", tenant = %org.slug, error = %e, "no pool");
                continue;
            }
        };
        match crate::page::run_schedule_sweep_and_purge(&pool, &org.slug, Arc::clone(invalidator)).await {
            Ok(r) if r.published + r.expired > 0 => tracing::info!(
                target: "rustango_cms::schedule",
                tenant = %org.slug, published = r.published, expired = r.expired,
                "schedule sweep"
            ),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(target: "rustango_cms::schedule", tenant = %org.slug, error = %e, "sweep failed");
            }
        }
    }
}
