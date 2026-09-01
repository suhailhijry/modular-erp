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
//! rule from `erp_eventlog::upcast`, asked before the pods go up rather than
//! discovered when a projection stops.
//!
//! `refresh <module>` rebuilds one module's read models across the fleet. It
//! does **not** drop anything first: `erp_projection::rebuild_swap` builds the
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
//! do it is a bad one — see `erp_demo::bootstrap`. It is also the wrong shape:
//! a process that refuses to start until every tenant is reachable turns one
//! unreachable cluster into a total outage. This reports; the pipeline decides.
//!
//! Exits non-zero when the fleet is not uniform afterwards, so `check` is
//! usable as a gate and a run that could not finish is visible to whatever
//! scheduled it.

use std::sync::Arc;

use erp_control::{ClusterRegistry, ControlPlane, PoolConfig, TenantPools};
use erp_projection::{Projection, rebuild_swap};

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

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&control_url)
        .await?;

    // Primary and, if this deployment has one, its read replica.
    let clusters = ClusterRegistry::from_env()?;
    let control = Arc::new(ControlPlane::new(
        pool,
        TenantPools::new(clusters, PoolConfig::default()),
    ));

    // **The control plane's own schema, first.**
    //
    // Only in the apply mode. `check` and `versions` are the pre-deploy gates
    // and are look-only by contract — a gate that writes is a gate you cannot
    // run against production before deciding to deploy.
    //
    // It was missing entirely: nothing but `erp_demo::bootstrap` ever called
    // `ControlPlane::migrate`, so a fresh deployment could only get its control
    // schema by building a demo tenant first. That is backwards for the thing
    // this document calls the deploy step.
    if mode.is_empty() {
        control.migrate().await?;
        register_primary(&control).await?;
    }

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
                     `erp_eventlog::upcast` on deploy ordering."
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
    let setup = erp_api::modules()
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
/// with both, which is why the loop is here rather than in `erp-control`.
///
/// `Purchases`, `Sales` and `Ledger` are matched by name because
/// `rebuild_swap` is generic over the group and a module's group is a type. A
/// module whose name is not here is a module nobody can rebuild, which is what
/// `every_module_can_be_rebuilt` refuses.
async fn rebuild(
    control: &ControlPlane,
    tenant: erp_types::TenantId,
    setup: &erp_control::ModuleSetup,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let pool = control.maintenance_pool(tenant).await?;
    let upcasters = (setup.upcasters)();
    let sql = setup.install_sql;

    let reached = match setup.module.as_str() {
        "booking" => {
            let owned = booking::projections();
            let refs: Vec<&dyn Projection<Group = booking::Booking>> =
                owned.iter().map(std::convert::AsRef::as_ref).collect();
            rebuild_swap::<booking::Booking>(&pool, sql, &refs, upcasters, 500).await?
        }
        "crm" => {
            let owned = crm::projections();
            let refs: Vec<&dyn Projection<Group = crm::Crm>> =
                owned.iter().map(AsRef::as_ref).collect();
            rebuild_swap::<crm::Crm>(&pool, sql, &refs, upcasters, 500).await?
        }
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
        "prepaid" => {
            let owned = prepaid::projections();
            let refs: Vec<&dyn Projection<Group = prepaid::Prepaid>> =
                owned.iter().map(std::convert::AsRef::as_ref).collect();
            rebuild_swap::<prepaid::Prepaid>(&pool, sql, &refs, upcasters, 500).await?
        }
        "branches" => {
            let owned = branches::projections();
            let refs: Vec<&dyn Projection<Group = branches::Branches>> =
                owned.iter().map(std::convert::AsRef::as_ref).collect();
            rebuild_swap::<branches::Branches>(&pool, sql, &refs, upcasters, 500).await?
        }
        "pos" => {
            let owned = pos::projections();
            let refs: Vec<&dyn Projection<Group = pos::Pos>> =
                owned.iter().map(std::convert::AsRef::as_ref).collect();
            rebuild_swap::<pos::Pos>(&pool, sql, &refs, upcasters, 500).await?
        }
        "purchases" => {
            let owned = purchases::projections();
            let refs: Vec<&dyn Projection<Group = purchases::Purchases>> =
                owned.iter().map(AsRef::as_ref).collect();
            rebuild_swap::<purchases::Purchases>(&pool, sql, &refs, upcasters, 500).await?
        }
        "tax_sa" => {
            let owned = tax_sa::projections();
            let refs: Vec<&dyn Projection<Group = tax_sa::TaxSa>> =
                owned.iter().map(AsRef::as_ref).collect();
            rebuild_swap::<tax_sa::TaxSa>(&pool, sql, &refs, upcasters, 500).await?
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
///   Somebody is deploying backwards. `erp_eventlog::upcast` would refuse it
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
    let modules = erp_api::modules();
    let understands = |event: &erp_types::EventName| {
        modules
            .iter()
            .find_map(|(_, setup)| (setup.upcasters)().current_version(event))
    };

    let (fleet, failed) = control.survey_event_versions().await?;

    let mut findings: Vec<(String, Vec<String>)> = Vec::new();
    for tenant in fleet {
        let mut problems = Vec::new();
        for (name, stored) in tenant.highest {
            let Ok(event) = erp_types::EventName::new(&name) else {
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

/// How many tenants and databases the primary cluster is declared to hold.
///
/// **A placeholder, not a measurement.** D13 says capacity is sized from
/// observation, and nothing here has observed anything. It exists so that a
/// fresh deployment can accept its first signup instead of answering
/// `no cluster has capacity (0 at their limit)` — which is what it did, and
/// which tells an operator nothing about what to do next.
const PLACEHOLDER_CAPACITY: i32 = 10_000;

/// Declares the primary cluster if the control plane has never heard of it.
///
/// # Why this is here
///
/// It was nowhere. `register_cluster` was called only by `erp_demo::bootstrap`,
/// so a deployment that never built a demo tenant had migrations applied, an
/// empty `cluster` table, and every signup failing with a 500 that named a
/// capacity problem rather than a missing row. Found by bringing the compose
/// stack up clean and posting a signup.
///
/// Declarative and idempotent, like `register_cluster` itself — re-running the
/// deploy step re-declares the same configuration.
///
/// The **variable names** are stored, never the credentials (D13). What is
/// recorded is "this cluster's DSN comes from `PRIMARY_CLUSTER_URL`", so a
/// control-plane backup carries no passwords.
async fn register_primary(
    control: &ControlPlane,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let capacity = std::env::var("PRIMARY_CLUSTER_CAPACITY")
        .ok()
        .and_then(|raw| raw.trim().parse::<i32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(PLACEHOLDER_CAPACITY);

    let replica_variable = std::env::var("PRIMARY_REPLICA_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .map(|_| "PRIMARY_REPLICA_URL");

    control
        .register_cluster(
            "primary",
            "PRIMARY_CLUSTER_URL",
            replica_variable,
            capacity,
            capacity,
            erp_control::Actor::system(),
        )
        .await?;

    tracing::info!(
        capacity,
        replica = replica_variable.is_some(),
        "cluster `primary` declared. The capacity is a placeholder — size it \
         from measurement and set PRIMARY_CLUSTER_CAPACITY (architecture D13)."
    );
    Ok(())
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
        const REBUILDABLE: &[&str] = &[
            "booking",
            "branches",
            "crm",
            "ledger",
            "prepaid",
            "pos",
            "sales",
            "purchases",
            "tax_sa",
        ];

        for (name, setup) in erp_api::modules() {
            assert!(
                REBUILDABLE.contains(&setup.module.as_str()),
                "{name} has no arm in `rebuild`, so a change to its read models \
                 could never be deployed"
            );
        }
    }
}
