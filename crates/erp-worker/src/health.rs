//! Asserting a tenant's invariants continuously, rather than at month end.
//!
//! Architecture §7 lists what must hold per tenant. Listing them is not the same
//! as checking them, and a system of record that only finds out at an audit has
//! the worst possible discovery latency — so they run here, on the same visit
//! loop as everything else.
//!
//! # What a finding is
//!
//! Not a user error. Every check here is of a property that *cannot* be false if
//! the code is right: log positions are contiguous by construction, effects are
//! delivered or dead-lettered, debits equal credits because the type says so. A
//! finding means the pipeline is broken, which is why they log at `error` and
//! why the count is worth alerting on directly.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use erp_control::TenantDb;
use erp_types::TenantId;
use tokio::sync::Mutex;

use crate::job::{Activity, BoxError, Job};

/// Something that should not be true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub check: &'static str,
    pub detail: String,
}

impl Finding {
    #[must_use]
    pub fn new(check: &'static str, detail: impl Into<String>) -> Self {
        Self {
            check,
            detail: detail.into(),
        }
    }
}

/// A property a module says must hold.
///
/// The kernel's own invariants are checked directly in [`HealthJob::tick`] —
/// they apply to every tenant and there is nothing to register. This trait is
/// how a *module* adds one, which is what makes the trial balance the ledger's
/// property rather than the platform's.
#[async_trait::async_trait]
pub trait Invariant: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// The module this belongs to. Skipped for tenants without it.
    fn module(&self) -> Option<erp_types::ModuleId> {
        None
    }

    /// Returns what is wrong. Empty is healthy.
    async fn check(&self, db: &TenantDb) -> Result<Vec<Finding>, BoxError>;
}

/// How far behind a projection group may fall before it is a finding.
const MAX_PROJECTION_LAG: i64 = 10_000;
/// How old the outbox's oldest undelivered effect may be.
const MAX_BACKLOG_SECONDS: i64 = 300;

/// Runs the invariants, on an interval.
///
/// # Why it is not run on every visit
///
/// A busy tenant is visited continuously, and `integrity()` counts every event.
/// Checking a property that changes only when something is badly wrong, several
/// times a second, is the kind of load nobody notices until it is the top query
/// in the slow log.
///
/// The interval is kept in memory rather than in the tenant's database. Losing
/// it on a deploy costs one extra check per tenant, which is cheaper than the
/// table it would take to avoid.
pub struct HealthJob {
    interval: Duration,
    module_invariants: Vec<Arc<dyn Invariant>>,
    last_checked: Mutex<HashMap<TenantId, Instant>>,
}

impl std::fmt::Debug for HealthJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthJob")
            .field("interval", &self.interval)
            .field(
                "module_invariants",
                &self
                    .module_invariants
                    .iter()
                    .map(|i| i.name())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl HealthJob {
    #[must_use]
    pub fn every(interval: Duration) -> Self {
        Self {
            interval,
            module_invariants: Vec::new(),
            last_checked: Mutex::new(HashMap::new()),
        }
    }

    /// Adds a module's invariant.
    #[must_use]
    pub fn with(mut self, invariant: Arc<dyn Invariant>) -> Self {
        self.module_invariants.push(invariant);
        self
    }

    /// Whether this tenant is due, marking it checked if so.
    async fn claim_turn(&self, tenant: TenantId) -> bool {
        let mut seen = self.last_checked.lock().await;
        match seen.get(&tenant) {
            Some(last) if last.elapsed() < self.interval => false,
            _ => {
                seen.insert(tenant, Instant::now());
                true
            }
        }
    }

    /// The invariants every tenant has, whatever modules it runs.
    async fn kernel_findings(&self, db: &TenantDb) -> Result<Vec<Finding>, BoxError> {
        let mut findings = Vec::new();
        let mut conn = db.acquire().await?;

        // L1. If this fails, replay can no longer be trusted at all, and every
        // other check below is measuring a corrupt log.
        let integrity = erp_eventlog::integrity(&mut conn).await?;
        if !integrity.is_contiguous() {
            findings.push(Finding::new(
                "log_contiguous",
                format!(
                    "{} events but the highest position is {} (counter at {})",
                    integrity.event_count, integrity.highest_position, integrity.next_position
                ),
            ));
        }

        let outbox = erp_eventlog::outbox_health(&mut conn).await?;
        if outbox.dead > 0 {
            findings.push(Finding::new(
                "no_dead_letters",
                format!("{} effects were given up on", outbox.dead),
            ));
        }
        if let Some(age) = outbox.backlog_age_seconds
            && age > MAX_BACKLOG_SECONDS
        {
            findings.push(Finding::new(
                "outbox_keeping_up",
                format!("the oldest undelivered effect is {age}s old"),
            ));
        }

        // Projection lag, per group. Read from the checkpoints rather than from
        // a list of groups this build knows, so a group belonging to a module
        // that is enabled but not deployed still shows up.
        let lagging = sqlx::query!(
            r#"SELECT c.group_name,
                      (SELECT COALESCE(max(position), 0) FROM event) - c.position AS "lag!"
                 FROM projection_checkpoint c"#
        )
        .fetch_all(&mut *conn)
        .await?;

        for row in lagging {
            if row.lag > MAX_PROJECTION_LAG {
                findings.push(Finding::new(
                    "projection_lag",
                    format!("{} is {} events behind", row.group_name, row.lag),
                ));
            }
        }

        Ok(findings)
    }
}

#[async_trait::async_trait]
impl Job for HealthJob {
    fn name(&self) -> &'static str {
        "health"
    }

    async fn tick(&self, db: &TenantDb) -> Result<Activity, BoxError> {
        if !self.claim_turn(db.tenant()).await {
            return Ok(Activity::Idle);
        }

        let mut findings = self.kernel_findings(db).await?;

        for invariant in &self.module_invariants {
            if invariant.module().is_some_and(|m| !db.has_module(&m)) {
                continue;
            }
            findings.extend(invariant.check(db).await?);
        }

        for finding in &findings {
            tracing::error!(
                tenant = %db.tenant(),
                check = finding.check,
                detail = %finding.detail,
                "invariant violated"
            );
        }

        if findings.is_empty() {
            tracing::debug!(tenant = %db.tenant(), "healthy");
        }

        // Checking is never `Worked`: a healthy tenant would otherwise be
        // revisited immediately, forever.
        Ok(Activity::Idle)
    }
}
