//! Applies pending tenant-plane migrations across the fleet.
//!
//! ```text
//! CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… cargo run --bin migrator          # apply
//! CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… cargo run --bin migrator -- check # look only
//! CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… cargo run --bin migrator -- refresh sales
//! CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… cargo run --bin migrator -- versions # read only
//! ```
//!
//! `versions` is the **other** pre-deploy gate. `check` answers "is the fleet's
//! *schema* at the version this build expects?"; `versions` answers "can this
//! build *read* what is already in the fleet's logs?" — which is the two-deploy
//! rule from `spa_eventlog::upcast`, asked before the pods go up rather than
//! discovered when a projection stops.
//!
//! `refresh <module>` rebuilds one module's read models across the fleet. It
//! does **not** drop anything first: `spa_projection::rebuild_swap` builds the
//! new tables in a staging schema beside the live ones, catches them up under
//! the checkpoint lock, and exchanges the two in one transaction. A tenant reads
//! the old shape, then the new one, and never an empty one.
//!
//! That is what a *changed* read model needs — `install.sql` is `IF NOT EXISTS`
//! throughout and will not add a column to a table that already exists.
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
use spa_projection::{Projection, rebuild_swap};

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

    if mode == "versions" {
        return match unreadable_events(&control).await? {
            findings if findings.is_empty() => {
                println!("every event in the fleet is readable by this build");
                Ok(())
            }
            findings => {
                println!(
                    "{} tenants hold events this build cannot read:",
                    findings.len()
                );
                for (slug, problems) in &findings {
                    for problem in problems {
                        println!("  {slug}: {problem}");
                    }
                }
                println!(
                    "\nDeploy the build that can *read* these first. See \
                     `spa_eventlog::upcast` on deploy ordering."
                );
                std::process::exit(1);
            }
        };
    }

    if mode == "refresh" {
        let name = std::env::args()
            .nth(2)
            .ok_or("refresh needs a module name, e.g. `refresh sales`")?;
        return refresh_fleet(&control, &name).await;
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

/// `refresh <module>`: rebuild one module's read models across the fleet.
///
/// Lifted out of `main` because it is a whole mode rather than a branch, and
/// because `main` had grown past the point where the three modes were readable
/// side by side.
async fn refresh_fleet(
    control: &ControlPlane,
    name: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let setup = spa_api::modules()
        .into_iter()
        .find(|(known, _)| *known == name)
        .map(|(_, setup)| setup)
        .ok_or_else(|| format!("{name} is not a module this build offers"))?;

    let tenants = control.tenants_with_module(&setup.module).await?;
    let mut rebuilt = 0;
    let mut failed: Vec<(String, String)> = Vec::new();

    for tenant in &tenants {
        match rebuild(control, tenant.id, &setup).await {
            Ok(position) => {
                rebuilt += 1;
                println!("  {} rebuilt to position {position}", tenant.slug);
            }
            // Collected, not returned: one unreachable cluster must not leave
            // the rest of the fleet on the old shape.
            Err(e) => {
                tracing::error!(tenant = %tenant.id, slug = %tenant.slug, error = %e, "rebuild failed");
                failed.push((tenant.slug.clone(), e.to_string()));
            }
        }
    }

    println!(
        "{} tenants have {name}: {rebuilt} rebuilt, {} failed",
        tenants.len(),
        failed.len()
    );
    for (slug, reason) in &failed {
        println!("  {slug} FAILED: {reason}");
    }
    if !failed.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

/// One tenant's read models, rebuilt beside the live ones and swapped in.
///
/// The projections have to come from somewhere that knows the modules, and the
/// tenant from somewhere that knows the fleet — this binary is the only place
/// with both, which is why the loop is here rather than in `spa-control`.
///
/// `Purchases`, `Sales` and `Ledger` are matched by name because
/// `rebuild_swap` is generic over the group and a module's group is a type. A
/// module whose name is not here is a module nobody can rebuild, which is what
/// `every_module_can_be_rebuilt` refuses.
async fn rebuild(
    control: &ControlPlane,
    tenant: spa_types::TenantId,
    setup: &spa_control::ModuleSetup,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let pool = control.maintenance_pool(tenant).await?;
    let upcasters = (setup.upcasters)();
    let sql = setup.install_sql;

    let reached = match setup.module.as_str() {
        "ledger" => {
            let owned = ledger::projections();
            let refs: Vec<&dyn Projection<Group = ledger::Ledger>> =
                owned.iter().map(AsRef::as_ref).collect();
            rebuild_swap::<ledger::Ledger>(&pool, sql, &refs, upcasters, 500).await?
        }
        "sales" => {
            let owned = sales::projections();
            let refs: Vec<&dyn Projection<Group = sales::Sales>> =
                owned.iter().map(AsRef::as_ref).collect();
            rebuild_swap::<sales::Sales>(&pool, sql, &refs, upcasters, 500).await?
        }
        "purchases" => {
            let owned = purchases::projections();
            let refs: Vec<&dyn Projection<Group = purchases::Purchases>> =
                owned.iter().map(AsRef::as_ref).collect();
            rebuild_swap::<purchases::Purchases>(&pool, sql, &refs, upcasters, 500).await?
        }
        other => return Err(format!("{other} has no rebuild in bin/migrator").into()),
    };

    pool.close().await;
    Ok(reached.get())
}

/// **Every event in the fleet this build would refuse to read.**
///
/// Two findings, and they are different failures:
///
/// - *from the future* — an event at a version higher than this build declares.
///   Somebody is deploying backwards. `spa_eventlog::upcast` would refuse it
///   (L6), but not until a projection reached it, by which time the pods are up
///   and the read models are falling behind.
/// - *unknown* — an event name this build declares nothing for at all. A module
///   was dropped from the build rather than deprecated, and every tenant
///   entitled to it is stranded.
async fn unreadable_events(
    control: &ControlPlane,
) -> Result<Vec<(String, Vec<String>)>, Box<dyn std::error::Error + Send + Sync>> {
    // Asked of every module rather than of one merged set, because the merged
    // set would be a sixth place listing modules — and this gate exists because
    // the fifth one was wrong.
    let modules = spa_api::modules();
    let understands = |event: &spa_types::EventName| {
        modules
            .iter()
            .find_map(|(_, setup)| (setup.upcasters)().current_version(event))
    };

    let (fleet, failed) = control.survey_event_versions().await?;

    let mut findings: Vec<(String, Vec<String>)> = Vec::new();
    for tenant in fleet {
        let mut problems = Vec::new();
        for (name, stored) in tenant.highest {
            let Ok(event) = spa_types::EventName::new(&name) else {
                problems.push(format!("{name} is not a usable event name"));
                continue;
            };
            match understands(&event) {
                Some(current) if current.get() < stored => problems.push(format!(
                    "{name} is at version {stored} and this build understands up to {current} — deploy the build that reads it first"
                )),
                Some(_) => {}
                None => problems.push(format!(
                    "{name} is in the log and this build declares nothing for it — a module was dropped rather than deprecated"
                )),
            }
        }
        if !problems.is_empty() {
            findings.push((tenant.slug, problems));
        }
    }

    // An unreachable tenant is not a clean one. Same rule as the migrator's.
    for (tenant, reason) in failed {
        findings.push((tenant.to_string(), vec![format!("unreachable: {reason}")]));
    }

    Ok(findings)
}

#[cfg(test)]
mod tests {
    /// **Every module this build offers can be rebuilt.**
    ///
    /// `rebuild` matches on the module name because `rebuild_swap` is generic
    /// over the projection group and a group is a type — so the match is the one
    /// place a module can be left out, and leaving one out means a deploy that
    /// changes its read models has no way to apply them.
    ///
    /// Same shape, and the same reason, as `every_module_has_a_projection_job`
    /// in `bin/worker`.
    #[test]
    fn every_module_can_be_rebuilt() {
        const REBUILDABLE: &[&str] = &["ledger", "sales", "purchases"];

        for (name, setup) in spa_api::modules() {
            assert!(
                REBUILDABLE.contains(&setup.module.as_str()),
                "{name} has no arm in `rebuild`, so a change to its read models \
                 could never be deployed"
            );
        }
    }
}
