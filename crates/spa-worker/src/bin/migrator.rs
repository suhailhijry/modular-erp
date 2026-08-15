//! Applies pending tenant-plane migrations across the fleet.
//!
//! ```text
//! CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… cargo run --bin migrator          # apply
//! CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… cargo run --bin migrator -- check # look only
//! CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… cargo run --bin migrator -- refresh sales
//! ```
//!
//! `refresh <module>` rebuilds one module's read models across the fleet: drop
//! the schema, install it again, rewind the checkpoint, and let the worker
//! replay. That is what a *changed* read model needs — `install.sql` is
//! `IF NOT EXISTS` throughout and will not add a column to a table that already
//! exists.
//!
//! # Where this goes in a deploy
//!
//! Before the code that needs the migration. `check` answers "is the fleet at
//! the version this build expects?" without writing anything, so a pipeline can
//! gate on it; the bare form does the work.
//!
//! # Why the API and the worker do not do this themselves
//!
//! Migrating on start is a deployment decision, and several instances racing to
//! do it is a bad one — see `spa_demo::bootstrap`. It is also the wrong shape:
//! a process that refuses to start until every tenant is reachable turns one
//! unreachable cluster into a total outage. This reports; the pipeline decides.
//!
//! Exits non-zero when the fleet is not uniform afterwards, so `check` is
//! usable as a gate and a run that could not finish is visible to whatever
//! scheduled it.

use std::sync::Arc;

use spa_control::{ClusterRegistry, ControlPlane, PoolConfig, TenantPools};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let mode = std::env::args().nth(1).unwrap_or_default();
    let check_only = mode == "check";

    let control_url =
        std::env::var("CONTROL_DATABASE_URL").map_err(|_| "CONTROL_DATABASE_URL is not set")?;
    let primary_url =
        std::env::var("PRIMARY_CLUSTER_URL").map_err(|_| "PRIMARY_CLUSTER_URL is not set")?;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&control_url)
        .await?;

    let clusters = ClusterRegistry::new().with_url("primary", &primary_url)?;
    let control = Arc::new(ControlPlane::new(
        pool,
        TenantPools::new(clusters, PoolConfig::default()),
    ));

    if mode == "refresh" {
        let name = std::env::args()
            .nth(2)
            .ok_or("refresh needs a module name, e.g. `refresh sales`")?;
        let setup = spa_api::modules()
            .into_iter()
            .find(|(known, _)| *known == name)
            .map(|(_, setup)| setup)
            .ok_or_else(|| format!("{name} is not a module this build offers"))?;

        let plan = control.refresh_module_fleet(setup).await?;
        println!(
            "{} tenants have {name}: {} rebuilt, {} failed",
            plan.total(),
            plan.behind.len(),
            plan.failed.len()
        );
        for (tenant, reason) in &plan.failed {
            println!("  {tenant} FAILED: {reason}");
        }
        // Only failures are failures. A rebuild that *did* work leaves tenants
        // in `behind` — they were behind, and now they are replaying — so
        // `is_uniform` is the wrong question here.
        if !plan.failed.is_empty() {
            std::process::exit(1);
        }
        return Ok(());
    }

    let plan = if check_only {
        control.survey_fleet().await?
    } else {
        control.migrate_fleet().await?
    };

    println!(
        "{} tenants: {} current, {} {}, {} failed (this build expects version {})",
        plan.total(),
        plan.current.len(),
        plan.behind.len(),
        if check_only { "behind" } else { "migrated" },
        plan.failed.len(),
        ControlPlane::latest_tenant_migration(),
    );
    for tenant in &plan.behind {
        println!(
            "  {} {} was at {:?}",
            tenant.tenant, tenant.slug, tenant.version
        );
    }
    for (tenant, reason) in &plan.failed {
        println!("  {tenant} FAILED: {reason}");
    }

    if !plan.is_uniform() {
        // Non-zero on purpose: `check` is a gate, and a run that left tenants
        // behind is one somebody has to look at.
        std::process::exit(1);
    }

    Ok(())
}
