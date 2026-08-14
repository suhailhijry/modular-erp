//! The worker process.
//!
//! # The composition root
//!
//! The only file that knows both the kernel and the modules. `spa-worker`
//! depends on no module and `modules/ledger` depends on no worker; they meet
//! here, which is what keeps the dependency arrow pointing one way and lets a
//! module be dropped from a build by deleting three lines.

use std::sync::Arc;
use std::time::Duration;

use spa_control::{ClusterRegistry, ControlPlane, PoolConfig, TenantPools};
use spa_eventlog::{Dispatcher, RetryPolicy};
use spa_types::ModuleId;
use spa_worker::{
    Finding, HealthJob, Invariant, OutboxJob, ProjectionJob, Worker, WorkerConfig, shutdown_signal,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let control_url = std::env::var("CONTROL_DATABASE_URL")
        .map_err(|_| "CONTROL_DATABASE_URL is not set; the worker has nothing to connect to")?;

    // Tenant clusters are named in the control plane's `cluster` table, and
    // their credentials come from environment variables named there — never
    // from the table itself (architecture D13).
    let primary_url = std::env::var("PRIMARY_CLUSTER_URL")
        .map_err(|_| "PRIMARY_CLUSTER_URL is not set; no tenant database is reachable")?;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&control_url)
        .await?;

    let clusters = ClusterRegistry::new().with_url("primary", &primary_url)?;
    let control = Arc::new(ControlPlane::new(
        pool,
        TenantPools::new(clusters, PoolConfig::default()),
    ));

    // Effect handlers come from modules. An empty dispatcher claims nothing —
    // the same behaviour as a worker rolled out before a module's handler
    // exists, and deliberately not an error.
    let dispatcher = Arc::new(Dispatcher::new(RetryPolicy::default()));

    let config = WorkerConfig {
        name: std::env::var("WORKER_NAME")
            .unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or_else(|_| "worker".to_owned())),
        drain_timeout: Duration::from_secs(20),
        ..WorkerConfig::default()
    };

    // The composition root, and the only place that knows both the kernel and
    // the modules. `spa-worker` itself depends on no module, which is what keeps
    // the dependency arrow pointing one way.
    let worker = Worker::new(control, config)
        .with_job(Arc::new(OutboxJob::new(dispatcher, 64)))
        .with_job(Arc::new(
            ProjectionJob::<ledger::Ledger>::new(
                ledger::projections(),
                Arc::new(ledger::upcasters().clone()),
                200,
            )
            .for_module(ledger_module()),
        ))
        .with_job(Arc::new(
            ProjectionJob::<sales::Sales>::new(
                sales::projections(),
                Arc::new(sales::upcasters().clone()),
                200,
            )
            .for_module(sales::module_id()),
        ))
        .with_job(Arc::new(
            HealthJob::every(Duration::from_mins(5))
                .with(Arc::new(TrialBalance))
                .with(Arc::new(NoOverpaidInvoice)),
        ));

    let shutdown = worker.run(shutdown_signal()).await;

    if !shutdown.drained {
        // Non-zero, because an orchestrator watching exit codes should be able
        // to tell a clean stop from one that ran out of time.
        tracing::error!("shut down without completing the drain");
        std::process::exit(1);
    }

    Ok(())
}

/// The ledger's trial balance, as an invariant the platform checks.
///
/// It lives here rather than in `spa-worker` because the *kernel* must not know
/// what a trial balance is (D11), and rather than in `modules/ledger` because a
/// module must not depend on the worker. The composition root is where the two
/// meet, and it is three lines.
struct TrialBalance;

#[async_trait::async_trait]
impl Invariant for TrialBalance {
    fn name(&self) -> &'static str {
        "trial_balance"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(ledger_module())
    }

    async fn check(
        &self,
        db: &spa_control::TenantDb,
    ) -> Result<Vec<Finding>, spa_worker::BoxError> {
        let mut conn = db.acquire().await?;
        Ok(ledger::imbalances(&mut conn)
            .await?
            .into_iter()
            .map(|t| {
                Finding::new(
                    "trial_balance",
                    format!(
                        "{} is out by {} ({} debits against {} credits)",
                        t.currency, t.difference, t.debits, t.credits
                    ),
                )
            })
            .collect())
    }
}

fn ledger_module() -> ModuleId {
    ModuleId::new("ledger").unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// No invoice may have taken more money than it asked for.
///
/// Unreachable through `sales::record_payment`, which refuses an overpayment
/// against the invoice's own state — so a finding here means the pipeline is
/// broken, in the same way a non-zero trial balance does.
struct NoOverpaidInvoice;

#[async_trait::async_trait]
impl Invariant for NoOverpaidInvoice {
    fn name(&self) -> &'static str {
        "no_overpaid_invoice"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(sales::module_id())
    }

    async fn check(
        &self,
        db: &spa_control::TenantDb,
    ) -> Result<Vec<Finding>, spa_worker::BoxError> {
        let mut conn = db.acquire().await?;
        Ok(sales::overpaid(&mut conn)
            .await?
            .into_iter()
            .map(|i| {
                Finding::new(
                    "no_overpaid_invoice",
                    format!(
                        "invoice {} is for {} and has taken {}",
                        i.invoice, i.gross, i.paid
                    ),
                )
            })
            .collect())
    }
}
