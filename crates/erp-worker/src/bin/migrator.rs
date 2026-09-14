//! Applies pending tenant-plane migrations across the fleet, and rebuilds the
//! read models they left on an older shape.
//!
//! ```text
//! CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… cargo run --bin migrator          # apply
//! CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… cargo run --bin migrator -- check # look only
//! CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… cargo run --bin migrator -- refresh sales
//! CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… cargo run --bin migrator -- versions # read only
//! … SEALING_KEY=new,old cargo run --bin migrator -- reseal        # move secrets to the first key
//! … SEALING_KEY=new,old cargo run --bin migrator -- reseal check  # look only: may `old` go?
//! ```
//!
//! **Anything else is refused** with the usage and exit 2. An unknown mode used
//! to fall through to the apply branch, so `reseal check` run on a build from
//! before `reseal` existed would have migrated the fleet — a look-only command,
//! mistyped or run on an old image, doing the one thing that writes.
//!
//! `reseal` moves every sealed value — second factors in the control plane,
//! module secrets in every tenant database, suspended tenants included — onto
//! the first key in `SEALING_KEY`. `reseal check` exits 0 only when nothing is
//! left under any other key and every tenant was reached, which is when an old
//! key may leave the list. See "Rotating the sealing key" in `docs/RUNNING.md`.
//!
//! `versions` is the **other** pre-deploy gate. `check` answers "is the fleet's
//! *schema* at the version this build expects?"; `versions` answers "can this
//! build *read* what is already in the fleet's logs?" — which is the two-deploy
//! rule from `erp_eventlog::upcast`, asked before the pods go up rather than
//! discovered when a projection stops.
//!
//! **The bare form also rebuilds every read model that is not this build's.**
//! Each projection group's checkpoint records the read-model version that built
//! its tables (`ProjectionGroup::VERSION`); after the schema migrations, every
//! group in every tenant — suspended ones, and disabled modules' groups, too —
//! whose recorded version differs from what this build projects is rebuilt, and
//! `check` lists them without touching anything. A group no module in this
//! build declares is reported, like an event nothing can read. Until a group
//! is rebuilt, this build's worker refuses to project into it, and when it is
//! older than this build the routes served from it answer 503 — so a deploy
//! that could not finish is loud, not wrong.
//!
//! A rebuild does **not** drop anything first: `erp_projection::rebuild_swap`
//! builds the new tables in a staging schema beside the live ones, catches them
//! up under the checkpoint lock, and exchanges the two in one transaction —
//! stamping the new version with them. A tenant reads the old shape, then the
//! new one, and never an empty one. That is what a *changed* read model needs:
//! `install.sql` is `IF NOT EXISTS` throughout and will not add a column to a
//! table that already exists.
//!
//! `refresh <module>` is the same rebuild for one module, on every tenant that
//! has it enabled, whatever its recorded version — for an operator who wants
//! one.
//!
//! # Where this goes in a deploy
//!
//! Before the code that needs the migration. `check` answers "is the fleet at
//! the version this build expects?" without writing anything, so a pipeline can
//! gate on it; the bare form does the work.
//!
//! **And the bare form once more after the rollout**, when the deploy changed a
//! read model: a tenant that signed up while the previous build was draining
//! was built by that build, with its read models, and nothing else rebuilds
//! them.
//!
//! # Why the API and the worker do not do this themselves
//!
//! Migrating on start is a deployment decision, and several instances racing to
//! do it is a bad one — see `erp_demo::bootstrap`. It is also the wrong shape:
//! a process that refuses to start until every tenant is reachable turns one
//! unreachable cluster into a total outage. This reports; the pipeline decides.
//!
//! Exits non-zero when the fleet is not uniform afterwards — a tenant behind on
//! migrations, or on a read model — so `check` is usable as a gate and a run
//! that could not finish is visible to whatever scheduled it.

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

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(mode) = mode(&args) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };

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
    if mode == Mode::Apply {
        control.migrate().await?;
        register_primary(&control).await?;
    }

    if mode == Mode::Versions {
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

    if let Mode::Refresh(name) = &mode {
        return refresh_fleet(&control, name).await;
    }

    if let Mode::Reseal { apply } = mode {
        return reseal(&control, apply).await;
    }

    // Only `Apply` and `Check` are left: `mode` refused everything else.
    let check_only = mode == Mode::Check;
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

    // After the migrations, because the column that records a group's version
    // is one of them.
    let read_models_settled = read_models(&control, !check_only).await?;

    if !plan.is_uniform() || !read_models_settled {
        // Non-zero on purpose: `check` is a gate, and a run that left tenants
        // behind is one somebody has to look at.
        std::process::exit(1);
    }

    Ok(())
}

/// **Every projection group in the fleet not at this build's read model** —
/// rebuilt when `apply`, listed when not. `true` when nothing is left: no
/// group behind or ahead, none undeclared, no tenant unreached.
///
/// ponytail: one rebuild at a time across the fleet, like `refresh`. Move to
/// `buffer_unordered(fleet_concurrency)` when deploy time matters.
async fn read_models(
    control: &ControlPlane,
    apply: bool,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let modules = erp_api::modules();
    let (fleet, failed) = control.survey_read_models().await?;

    let mut rebuilt = 0;
    let mut left: Vec<String> = Vec::new();
    for tenant in &fleet {
        for finding in stale(&tenant.installed, &modules) {
            match finding {
                Stale::Rebuild {
                    setup,
                    group,
                    from,
                    to,
                } => {
                    let said = format!(
                        "{}: {group} at {from}, this build projects {to}",
                        tenant.slug
                    );
                    if !apply {
                        left.push(said);
                        continue;
                    }
                    match rebuild(control, tenant.tenant, setup).await {
                        Ok(_) => {
                            rebuilt += 1;
                            println!("  {said}: rebuilt");
                        }
                        // Collected, not returned: one tenant that will not
                        // rebuild must not leave the rest on the old shape.
                        Err(e) => {
                            tracing::error!(tenant = %tenant.tenant, slug = %tenant.slug, %group, error = %e, "read-model rebuild failed");
                            left.push(format!("{said}: FAILED: {e}"));
                        }
                    }
                }
                Stale::Undeclared { group } => left.push(format!(
                    "{}: {group} is a read model no module in this build declares — a module \
                     was dropped rather than deprecated",
                    tenant.slug
                )),
            }
        }
    }
    // An unreachable tenant is not a current one. Same rule as the migrator's.
    for (tenant, reason) in &failed {
        left.push(format!("{tenant}: unreachable: {reason}"));
    }

    if apply {
        println!("read models: {rebuilt} rebuilt, {} left", left.len());
    } else {
        println!("read models: {} not at this build's version", left.len());
    }
    for line in &left {
        println!("  {line}");
    }
    Ok(left.is_empty())
}

/// What the deploy step does about one tenant's projection group.
#[derive(Debug)]
enum Stale<'a> {
    /// Built for another read model than this build projects.
    Rebuild {
        setup: &'a erp_control::ModuleSetup,
        group: String,
        from: i16,
        to: i16,
    },
    /// No module in this build declares it.
    Undeclared { group: String },
}

/// **Judges one tenant's checkpoints against this build.** Pure, so the
/// decision is tested without a fleet.
///
/// `!=`, not `<`, the same test the runner projects by: every read model is
/// to be this build's. A group ahead of it is one a rolled-back deploy left in
/// a newer shape, which this build's workers refuse to project into, and read
/// models are derived (L2) — rebuilding one down costs a replay and loses
/// nothing, while the `versions` gate is what stops a build deploying over a
/// log it cannot read.
fn stale<'a>(
    installed: &[(String, i16)],
    modules: &'a [(&'static str, erp_control::ModuleSetup)],
) -> Vec<Stale<'a>> {
    installed
        .iter()
        .filter_map(|(group, from)| {
            let declared = modules.iter().find_map(|(_, setup)| {
                setup
                    .groups
                    .iter()
                    .find(|(name, _, _)| name == group)
                    .map(|(_, _, version)| (setup, *version))
            });
            match declared {
                Some((_, to)) if to == *from => None,
                Some((setup, to)) => Some(Stale::Rebuild {
                    setup,
                    group: group.clone(),
                    from: *from,
                    to,
                }),
                None => Some(Stale::Undeclared {
                    group: group.clone(),
                }),
            }
        })
        .collect()
}

const USAGE: &str = "usage: migrator [check | versions | refresh <module> | reseal [check]]";

/// What the migrator was asked to do.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Apply,
    Check,
    Versions,
    Refresh(String),
    Reseal { apply: bool },
}

/// The mode the arguments name exactly, or `None` for anything else — which
/// `main` refuses. See the module doc for what falling through used to do.
fn mode(args: &[String]) -> Option<Mode> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => Some(Mode::Apply),
        ["check"] => Some(Mode::Check),
        ["versions"] => Some(Mode::Versions),
        ["refresh", module] => Some(Mode::Refresh((*module).to_owned())),
        ["reseal"] => Some(Mode::Reseal { apply: true }),
        ["reseal", "check"] => Some(Mode::Reseal { apply: false }),
        _ => None,
    }
}

/// `reseal [check]`: every sealed value onto the first key in `SEALING_KEY`,
/// or only a count of where they are.
///
/// **The exit code is the retirement gate.** Non-zero while anything is under
/// another key, anything would not open, or any tenant was not reached; an old
/// key leaves the list only once `reseal check` exits 0. No control-plane
/// migration and no cluster registration: this is not a deploy step.
async fn reseal(
    control: &ControlPlane,
    apply: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let configured = std::env::var("SEALING_KEY")
        .map_err(|_| "reseal needs SEALING_KEY: the same list the API and the worker have")?;
    let sealing = erp_eventlog::SealingKey::parse(&configured)?;
    let current = sealing.id();

    let plan = control.reseal_fleet(&sealing, apply).await?;

    if apply {
        println!("{} sealed values moved to {current}", plan.census.resealed);
    }
    println!("sealed values by key:");
    for (id, n) in &plan.census.under {
        let marker = if id == current { " (current)" } else { "" };
        println!("  {id}: {n}{marker}");
    }
    for name in &plan.census.unsealable {
        println!("  UNSEALABLE by any key held: {name}");
    }
    for (tenant, reason) in &plan.failed {
        println!("  {tenant} FAILED: {reason}");
    }

    if !plan.is_settled(current) {
        println!("\nKeep every other key in SEALING_KEY until `reseal check` exits 0.");
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
#[expect(
    clippy::too_many_lines,
    reason = "one arm per module's projection group: the list is the point, and \
              a test fails when a module is missing from it"
)]
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
        "payments" => {
            let owned = payments::projections();
            let refs: Vec<&dyn Projection<Group = payments::Payments>> =
                owned.iter().map(AsRef::as_ref).collect();
            rebuild_swap::<payments::Payments>(&pool, sql, &refs, upcasters, 500).await?
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
        "payroll" => {
            let owned = payroll::projections();
            let refs: Vec<&dyn Projection<Group = payroll::Payroll>> =
                owned.iter().map(std::convert::AsRef::as_ref).collect();
            rebuild_swap::<payroll::Payroll>(&pool, sql, &refs, upcasters, 500).await?
        }
        "hr" => {
            let owned = hr::projections();
            let refs: Vec<&dyn Projection<Group = hr::Hr>> =
                owned.iter().map(std::convert::AsRef::as_ref).collect();
            rebuild_swap::<hr::Hr>(&pool, sql, &refs, upcasters, 500).await?
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
        "files" => {
            let owned = files::projections();
            let refs: Vec<&dyn Projection<Group = files::Files>> =
                owned.iter().map(AsRef::as_ref).collect();
            rebuild_swap::<files::Files>(&pool, sql, &refs, upcasters, 500).await?
        }
        "inventory" => {
            let owned = inventory::projections();
            let refs: Vec<&dyn Projection<Group = inventory::Inventory>> =
                owned.iter().map(AsRef::as_ref).collect();
            rebuild_swap::<inventory::Inventory>(&pool, sql, &refs, upcasters, 500).await?
        }
        "conversations" => {
            let owned = conversations::projections();
            let refs: Vec<&dyn Projection<Group = conversations::Conversations>> =
                owned.iter().map(AsRef::as_ref).collect();
            rebuild_swap::<conversations::Conversations>(&pool, sql, &refs, upcasters, 500).await?
        }
        "notifications" => {
            let owned = notifications::projections();
            let refs: Vec<&dyn Projection<Group = notifications::Notifications>> =
                owned.iter().map(AsRef::as_ref).collect();
            rebuild_swap::<notifications::Notifications>(&pool, sql, &refs, upcasters, 500).await?
        }
        "reports" => {
            let owned = reports::projections();
            let refs: Vec<&dyn Projection<Group = reports::Reports>> =
                owned.iter().map(AsRef::as_ref).collect();
            rebuild_swap::<reports::Reports>(&pool, sql, &refs, upcasters, 500).await?
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
///
/// **The capacity is required.** It defaulted to a placeholder of ten thousand
/// until 2026-09-14, so a deployment that never set
/// `PRIMARY_CLUSTER_CAPACITY` — and none did — overfilled its cluster by
/// exactly the guess D13 forbids. Refused now, before anything is applied.
async fn register_primary(
    control: &ControlPlane,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let capacity = erp_control::declared_capacity()?;

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
        "cluster `primary` declared at the capacity PRIMARY_CLUSTER_CAPACITY names"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Mode, Stale, mode, stale};

    /// A module with one group, at `version`.
    fn declaring(group: &'static str, version: i16) -> (&'static str, erp_control::ModuleSetup) {
        fn none() -> &'static erp_eventlog::Upcasters {
            static NONE: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
            NONE.get_or_init(erp_eventlog::Upcasters::new)
        }
        let groups: &'static [(&'static str, &'static str, i16)] =
            Box::leak(Box::new([(group, "proj_toy", version)]));
        (
            group,
            erp_control::ModuleSetup::new(
                erp_types::ModuleId::new(group).expect("a module id"),
                "",
                groups,
                none,
            ),
        )
    }

    /// What `stale` found, as `(group, from, to)`, with `to` -1 for undeclared.
    fn judged(
        installed: &[(&str, i16)],
        build: &[(&'static str, erp_control::ModuleSetup)],
    ) -> Vec<(String, i16, i16)> {
        let installed: Vec<(String, i16)> = installed
            .iter()
            .map(|(g, v)| ((*g).to_owned(), *v))
            .collect();
        stale(&installed, build)
            .into_iter()
            .map(|finding| match finding {
                Stale::Rebuild {
                    group, from, to, ..
                } => (group, from, to),
                Stale::Undeclared { group } => (group, 0, -1),
            })
            .collect()
    }

    /// **The deploy step rebuilds every group not at this build's read model,
    /// in either direction, and leaves a current one alone.** Behind is the
    /// ordinary case; 0 is every tenant from before versions were recorded;
    /// ahead is a rolled-back deploy's leftovers.
    #[test]
    fn a_group_behind_or_ahead_of_this_build_is_rebuilt_and_a_current_one_is_not() {
        let build = [declaring("toy", 2)];
        assert_eq!(judged(&[("toy", 1)], &build), [("toy".to_owned(), 1, 2)]);
        assert_eq!(judged(&[("toy", 0)], &build), [("toy".to_owned(), 0, 2)]);
        assert_eq!(judged(&[("toy", 3)], &build), [("toy".to_owned(), 3, 2)]);
        assert_eq!(judged(&[("toy", 2)], &build), []);
    }

    /// **A read model no module in this build declares is reported**, not
    /// skipped: a module was dropped from the build rather than deprecated, and
    /// the tenants that had it are stranded — the same finding `versions` makes
    /// about their events.
    #[test]
    fn a_group_no_module_declares_is_reported() {
        let build = [declaring("toy", 1)];
        assert_eq!(
            judged(&[("gone", 1), ("toy", 1)], &build),
            [("gone".to_owned(), 0, -1)]
        );
    }

    /// `(group, version, sha256 of the install script)` for every module with
    /// read models, the script with its comments and whitespace squeezed out.
    ///
    /// **Changing a module's `install.sql` changes a hash here, and the test
    /// below fails until its group's `VERSION` moves** — which is what makes
    /// the deploy step rebuild it on every tenant. Without that, the new column
    /// reached only tenants signed up after the deploy, and everyone else got a
    /// 500 the first time a query named it.
    ///
    /// Re-pinning the hash without bumping the version passes. That is a
    /// deliberate act in a diff, like `just baseline`; the version doc says
    /// what else only judgement can catch.
    const READ_MODELS: &[(&str, i16, &str)] = &[
        (
            "booking",
            1,
            "e5f16128efdf8b9761c3900aeb07a51ad4aa4fd0b918de22dc195758b28b96bf",
        ),
        (
            "branches",
            1,
            "30a5dc0637c8a69a4cc3825005bb628b3a2908aa9ea830e7269377fdba6ba0c4",
        ),
        (
            "conversations",
            1,
            "ab09b9a723fb3692a8fec6a3c86e3c8a263a5b9290bae7f62fcba234f94a86e0",
        ),
        (
            "crm",
            1,
            "218c524f519a1bb808abcaa5cdabf4c4a2ac714ab37b67ce25e89b8f31c43ab1",
        ),
        (
            "files",
            1,
            "114994e9b498db69cff8668aa5ddbedbec6902c2789d366902783d3e67e55886",
        ),
        (
            "hr",
            1,
            "063d9abc7de7f0c1e71b1a339483fb16adbda04ad2e7a69fb0b857ad52c06d75",
        ),
        (
            // Version 2 for a serial a count did not find (`missing`, §75), and 3
            // for the `lot` row a return opens for units a count had cleared
            // (§75's review) — a projection that writes rows it did not, with the
            // script's shape unchanged, so the hash stays. The module has not
            // shipped, so no tenant has `proj_inventory` to rebuild and the bump
            // costs nothing but this line. After it ships the same edit is a
            // fleet rebuild. See §71. Version 4 for a return that reopens an
            // emptied lot moving its `recorded_at` (§77's review), the same way.
            "inventory",
            4,
            "e05ca10a686134ce6b6fca60d6239a042c1149f868a32684f9ff228da3ea13e1",
        ),
        (
            "ledger",
            1,
            "44672ee16d291279d7bd588773c4fa304cfa6bf36d8a3dc3f483104039da8edd",
        ),
        (
            "notifications",
            1,
            "6e83346d857ad3b4284096b621b5b62a6e1f2de9129e71436f25e3165fb27c16",
        ),
        (
            "payments",
            1,
            "bbe320e8e524f33cdaaba0e16b11fa1f7d0ee2cf73b5051589bb8b0323dcec16",
        ),
        (
            "payroll",
            1,
            "7ab0250614cddc126ab7300a9cc6b3fa5fb21442130a92acf57549cd5ffda950",
        ),
        (
            "pos",
            1,
            "dfc508a6e46c7c28a5405d9f0a050ed3af84e38f8b0d6626a959d3a97d22d698",
        ),
        (
            "prepaid",
            1,
            "d3cbbb1eadf749f771f709a7fda984ba3389abb0685505255e1b7e66825a1130",
        ),
        (
            "purchases",
            1,
            "ff88b501be248649d3478fee97f2a345e03a38d96773c20e4e692cbb3b9aca77",
        ),
        (
            "reports",
            1,
            "678a6e9730075bcc3851e3e55b6cca84705500f97e8ac8e843b988178ec74323",
        ),
        (
            "sales",
            1,
            "dfcf596a67de087306f3baa7e0decaaca1fa85531493c254bcdc7a3e5cef2c40",
        ),
        (
            "tax_sa",
            1,
            "c3b01fa58e4d616537ffe52c2300e6e60a0f2d3f63f587c8f2758eb8286de121",
        ),
    ];

    /// The install script, without what cannot change its meaning.
    fn normalized(sql: &str) -> String {
        sql.lines()
            .map(|line| line.split_once("--").map_or(line, |(code, _)| code))
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn a_read_model_change_bumps_its_version() {
        use sha2::Digest as _;

        let mut wrong = Vec::new();
        for (name, setup) in erp_api::modules() {
            for (group, _, version) in setup.groups {
                let hash = hex::encode(sha2::Sha256::digest(normalized(setup.install_sql)));
                match READ_MODELS.iter().find(|(pinned, _, _)| pinned == group) {
                    None => wrong.push(format!(
                        "{name}'s read model is not pinned: add (\"{group}\", {version}, \"{hash}\")"
                    )),
                    Some((_, pinned, _)) if pinned != version => wrong.push(format!(
                        "{group} is at version {version} and pinned at {pinned}: update the pin \
                         to (\"{group}\", {version}, \"{hash}\")"
                    )),
                    Some((_, _, pinned)) if *pinned != hash => wrong.push(format!(
                        "{name}'s install.sql changed shape but {group}'s read-model version \
                         is still {version}. Bump its `ProjectionGroup::VERSION` to {} and pin \
                         (\"{group}\", {}, \"{hash}\"); every tenant rebuilds {group} on the \
                         next deploy.",
                        version + 1,
                        version + 1
                    )),
                    Some(_) => {}
                }
            }
        }
        let modules = erp_api::modules();
        for (pinned, _, _) in READ_MODELS {
            if !modules
                .iter()
                .any(|(_, setup)| setup.groups.iter().any(|(group, _, _)| group == pinned))
            {
                wrong.push(format!("{pinned} is pinned and no module declares it"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// The pin is only worth having if a comment does not trip it — or it gets
    /// re-pinned by habit.
    #[test]
    fn a_comment_is_not_a_change_of_shape() {
        assert_eq!(
            normalized("CREATE TABLE t (id INT); -- why\n\n  -- more\nCREATE INDEX i ON t (id);"),
            normalized("CREATE TABLE t (id INT);\nCREATE INDEX i ON t (id); -- a different why")
        );
        assert_ne!(
            normalized("CREATE TABLE t (id INT);"),
            normalized("CREATE TABLE t (id INT, label TEXT);")
        );
    }

    /// **A mode the migrator does not know is refused, not applied.** Every
    /// unknown argument used to reach `migrate_fleet`, so `reseal check` on an
    /// image from before `reseal` would have migrated the fleet it was asked
    /// only to count — and so would `chek`, or `check` with a stray word after
    /// it.
    #[test]
    fn only_the_modes_the_migrator_knows_are_accepted() {
        let parse = |list: &[&str]| mode(&list.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>());

        assert_eq!(parse(&[]), Some(Mode::Apply));
        assert_eq!(parse(&["check"]), Some(Mode::Check));
        assert_eq!(parse(&["versions"]), Some(Mode::Versions));
        assert_eq!(
            parse(&["refresh", "sales"]),
            Some(Mode::Refresh("sales".to_owned()))
        );
        assert_eq!(parse(&["reseal"]), Some(Mode::Reseal { apply: true }));
        assert_eq!(
            parse(&["reseal", "check"]),
            Some(Mode::Reseal { apply: false })
        );

        for unknown in [
            &["chek"][..],
            &[""],
            &["check", "now"],
            &["refresh"],
            &["reseal", "chek"],
            &["reseal", "check", "now"],
            &["rotate"],
        ] {
            assert_eq!(parse(unknown), None, "{unknown:?} was accepted");
        }
    }
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
            "inventory",
            "hr",
            "payroll",
            "pos",
            "sales",
            "purchases",
            "tax_sa",
            "reports",
            "files",
            "conversations",
            "notifications",
            "payments",
        ];

        for (name, setup) in erp_api::modules() {
            // A module with no projection groups has no read models to rebuild.
            // Keyed off the setup rather than an exception list, so the next
            // arithmetic-only module needs no edit here.
            if setup.groups.is_empty() {
                continue;
            }
            assert!(
                REBUILDABLE.contains(&setup.module.as_str()),
                "{name} has no arm in `rebuild`, so a change to its read models \
                 could never be deployed"
            );
        }
    }
}
