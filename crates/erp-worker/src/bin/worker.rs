//! The worker process.
//!
//! # The composition root
//!
//! The only file that knows both the kernel and the modules. `erp-worker`
//! depends on no module and `modules/ledger` depends on no worker; they meet
//! here, which is what keeps the dependency arrow pointing one way and lets a
//! module be dropped from a build by deleting three lines.

use std::sync::Arc;
use std::time::Duration;

use erp_control::{ClusterRegistry, ControlPlane, PoolConfig, TenantPools};
use erp_eventlog::{Dispatcher, RetryPolicy};
use erp_types::ModuleId;
use erp_worker::{
    Activity, Finding, HealthJob, Invariant, OutboxJob, PlatformOutboxJob, ProjectionJob, Worker,
    WorkerConfig, shutdown_signal,
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

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&control_url)
        .await?;

    // Primary and, if this deployment has one, its read replica.
    let clusters = ClusterRegistry::from_env()?;
    // **Shared state, when this deployment runs more than one of these.**
    //
    // Sessions read from Redis instead of the control database on every
    // request, and a cache invalidation reaches every node rather than only
    // this one. Without `REDIS_URL` both fall back to what this system did
    // before — see `erp_control::shared`.
    let mut control = ControlPlane::new(pool, TenantPools::new(clusters, PoolConfig::from_env()));
    if let Some(shared) = erp_control::shared::Shared::from_env().await? {
        control = control.sharing(shared);
    }
    let control = Arc::new(control);
    let _invalidations = erp_control::shared::apply_invalidations_in_background(&control);

    // States what this process could demand against what the server allows.
    // Nothing wrote either number down before, which is how four processes each
    // holding a 400-permit budget against a 200-connection server went unnoticed.
    control.pools().report_budget("primary").await;

    // Effect handlers come from modules. An empty dispatcher claims nothing —
    // the same behaviour as a worker rolled out before a module's handler
    // exists, and deliberately not an error.
    let dispatcher = Arc::new(Dispatcher::new(RetryPolicy::default()));

    // **The control plane's dispatcher**, which is a different queue in a
    // different database. Email lives here because the things that send it —
    // invitations today, password resets next — are control-plane rows.
    //
    // A longer lease than the default: a slow relay is the normal failure, and
    // a lease that lapses while a message is still in flight sends it twice.
    let mut platform = Dispatcher::new(RetryPolicy {
        lease: Duration::from_mins(2),
        ..RetryPolicy::default()
    });
    for handler in mailer()? {
        platform = platform.register(handler);
    }
    let platform = Arc::new(platform);

    let config = WorkerConfig {
        name: std::env::var("WORKER_NAME")
            .unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or_else(|_| "worker".to_owned())),
        drain_timeout: Duration::from_secs(20),
        ..WorkerConfig::default()
    };

    // The composition root, and the only place that knows both the kernel and
    // the modules. `erp-worker` itself depends on no module, which is what keeps
    // the dependency arrow pointing one way.
    let mut worker = Worker::new(control, config)
        .with_platform_job(Arc::new(PlatformOutboxJob::new(platform, EMAIL_BATCH)))
        .with_job(Arc::new(OutboxJob::new(dispatcher, 64)))
        .with_job(Arc::new(
            HealthJob::every(Duration::from_mins(5))
                .with(Arc::new(TrialBalance))
                .with(Arc::new(NoOverpaidInvoice))
                .with(Arc::new(NoOverpaidBill))
                .with(Arc::new(CertificateExpiry))
                .with(Arc::new(WorkDocumentExpiry)),
        ));
    for job in module_jobs() {
        worker = worker.with_job(job);
    }

    // **The ZATCA sweeps, and only with a sealing key.** They read a tenant's
    // private key to sign with, so without one there is nothing to read and the
    // jobs are not registered at all — which is louder than a job that runs and
    // finds it can do nothing.
    if let Ok(configured) = std::env::var("SEALING_KEY") {
        let sealing = erp_eventlog::SealingKey::parse(&configured)?;
        tracing::info!(
            key = sealing.id(),
            "sealing key loaded; ZATCA sweeps enabled"
        );
        for job in zatca_jobs(&sealing) {
            worker = worker.with_job(job);
        }
    } else {
        tracing::warn!(
            "SEALING_KEY is not set; invoices will be built and chained but never \
             signed or sent to ZATCA"
        );
    }

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
/// It lives here rather than in `erp-worker` because the *kernel* must not know
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
        Some(ledger::module_id())
    }

    async fn check(
        &self,
        db: &erp_control::TenantDb,
    ) -> Result<Vec<Finding>, erp_worker::BoxError> {
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

/// How long before a certificate expires the platform starts saying so.
///
/// Sixty days, because renewing needs a **human**: the taxpayer logs in to the
/// Fatoora portal and reads an OTP off a screen. Nothing here can do that, so
/// the only thing that stops a lapse is telling somebody early enough to act.
const EXPIRY_WARNING: chrono::TimeDelta = chrono::TimeDelta::days(60);

/// **A ZATCA certificate that is running out.**
///
/// When it lapses, every invoice stops being clearable — and the first anyone
/// would know is a customer waiting for one. A five-year certificate is exactly
/// the kind of deadline nobody has a reminder for.
/// **Work documents that have lapsed, or are about to.**
///
/// The producer §9e asks for, in the shape that actually reaches somebody
/// today. The plan asks for an outbox effect on a date; the tenant dispatcher
/// has no handlers registered at all — email is control-plane, because the
/// things that send it are control-plane rows — so an effect enqueued from `hr`
/// would sit in the outbox for ever. A health finding is read.
///
/// **A lapsed document is a separate finding from an expiring one**, and not a
/// louder version of it: one is somebody to remind, the other is somebody who
/// must come off the rota today. Collapsing them into a single severity is how
/// the second gets treated like the first.
struct WorkDocumentExpiry;

#[async_trait::async_trait]
impl Invariant for WorkDocumentExpiry {
    fn name(&self) -> &'static str {
        "work_document_expiry"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(hr::module_id())
    }

    async fn check(
        &self,
        db: &erp_control::TenantDb,
    ) -> Result<Vec<Finding>, erp_worker::BoxError> {
        // The read model, not the aggregates (L7) — and it costs one indexed
        // scan rather than loading every employee the business has ever had.
        let mut conn = db.read().await?;
        let expiring = hr::expiring(&mut conn, DOCUMENT_WARNING_DAYS, 200).await?;
        drop(conn);

        let (lapsed, soon): (Vec<_>, Vec<_>) = expiring.into_iter().partition(|d| d.days_left < 0);

        let mut findings = Vec::new();
        if !lapsed.is_empty() {
            findings.push(Finding::new(
                "work_document_lapsed",
                format!(
                    "{} work {} lapsed and the people holding them cannot be \
                     rostered: {}",
                    lapsed.len(),
                    if lapsed.len() == 1 {
                        "document has"
                    } else {
                        "documents have"
                    },
                    describe(&lapsed),
                ),
            ));
        }
        if !soon.is_empty() {
            findings.push(Finding::new(
                "work_document_expiring",
                format!(
                    "{} work {} within {DOCUMENT_WARNING_DAYS} days: {}",
                    soon.len(),
                    if soon.len() == 1 {
                        "document expires"
                    } else {
                        "documents expire"
                    },
                    describe(&soon),
                ),
            ));
        }
        Ok(findings)
    }
}

/// The first few, named. **Not all of them**: a finding that lists two hundred
/// people is one nobody reads, and the count above already says how many there
/// are.
fn describe(documents: &[hr::Expiring]) -> String {
    const NAMED: usize = 5;
    let named: Vec<String> = documents
        .iter()
        .take(NAMED)
        .map(|d| format!("{} ({}, {})", d.name, d.kind, d.expires_on))
        .collect();
    if documents.len() > NAMED {
        format!("{}, and {} more", named.join("; "), documents.len() - NAMED)
    } else {
        named.join("; ")
    }
}

/// How far ahead a warning is worth having.
///
/// Sixty days is roughly what an iqama renewal needs — long enough to act on,
/// short enough that it is not permanently on the list.
const DOCUMENT_WARNING_DAYS: i32 = 60;

struct CertificateExpiry;

#[async_trait::async_trait]
impl Invariant for CertificateExpiry {
    fn name(&self) -> &'static str {
        "zatca_certificate_expiry"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(tax_sa::module_id())
    }

    async fn check(
        &self,
        db: &erp_control::TenantDb,
    ) -> Result<Vec<Finding>, erp_worker::BoxError> {
        // The read model, not the aggregate (L7). It also costs one row rather
        // than every certificate ever issued, and a renewal appends.
        let mut conn = db.read().await?;
        let onboarded = tax_sa::onboarding(&mut conn).await?;
        drop(conn);

        // Never onboarded: nothing to expire, and not a finding.
        let Some(onboarded) = onboarded else {
            return Ok(Vec::new());
        };
        let not_after = onboarded.not_after.as_str();
        let Some(expires) = certificate_time(not_after) else {
            return Ok(vec![Finding::new(
                "zatca_certificate_expiry",
                format!("the certificate's expiry date cannot be read: {not_after:?}"),
            )]);
        };

        let left = expires - chrono::Utc::now();
        if left > EXPIRY_WARNING {
            return Ok(Vec::new());
        }

        Ok(vec![Finding::new(
            "zatca_certificate_expiry",
            if left.num_seconds() <= 0 {
                format!(
                    "the ZATCA certificate expired on {not_after}; no invoice can be cleared \
                     or reported until it is renewed"
                )
            } else {
                format!(
                    "the ZATCA certificate expires in {} days ({not_after}); renewing needs \
                     an OTP from the taxpayer's Fatoora portal",
                    left.num_days()
                )
            },
        )])
    }
}

/// OpenSSL's `Aug 16 20:28:41 2031 GMT`, as an instant.
///
/// Parsed from what the certificate says rather than from a field this system
/// chose, because the certificate is the authority on when it stops working.
fn certificate_time(text: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    // `%e` is the day with a leading space for single digits, which is what
    // OpenSSL prints and what a `%d` parse would reject on the ninth of a month.
    chrono::NaiveDateTime::parse_from_str(text.trim(), "%b %e %H:%M:%S %Y GMT")
        .ok()
        .map(|naive| naive.and_utc())
}

/// How many documents one sweep handles for one tenant.
///
/// Small on purpose: a visit is meant to be short so the worker gets round
/// every tenant, and a clearance call is a network round trip in front of a
/// person. What is not swept this visit is swept the next.
const ZATCA_BATCH: i64 = 20;

/// How many emails one pass sends.
///
/// Small on purpose. The platform pass runs **inline** in the claim loop, so a
/// batch is time the worker is not visiting tenants — and a relay that answers
/// in 200 ms turns 32 into six seconds. Anything left over goes on the next
/// cycle, which is at most `empty_claim_pause` away.
const EMAIL_BATCH: i64 = 32;

/// The mailer, from the deployment's environment.
///
/// **Registered only when `SMTP_URL` is set**, and the handler is absent
/// otherwise — which is not the same as broken. An effect whose kind has no
/// registered handler is *not claimed* (see `erp_eventlog::outbox`), so on a
/// deployment with no relay an invitation email waits in the outbox as an
/// undelivered promise rather than being attempted and dead-lettered. Configure
/// SMTP later and everything already promised goes out.
///
/// That is the same call `SEALING_KEY` makes below, for the same reason: a job
/// that runs and finds it can do nothing is quieter than one that was never
/// registered.
fn mailer()
-> Result<Vec<Arc<dyn erp_eventlog::EffectHandler>>, Box<dyn std::error::Error + Send + Sync>> {
    let Ok(url) = std::env::var("SMTP_URL") else {
        tracing::warn!(
            "SMTP_URL is not set; invitations are still created and their emails \
             still promised, but nothing sends them — the invitation link in the \
             API response is the only way in until a relay is configured"
        );
        return Ok(Vec::new());
    };
    let from = std::env::var("SMTP_FROM")?;
    let smtp = erp_worker::mail::Smtp::new(&url, &from)?;
    tracing::info!(from = %from, "SMTP configured; email effects will be delivered");
    Ok(vec![Arc::new(erp_worker::mail::EmailHandler::new(
        Arc::new(smtp),
    ))])
}

/// Anything a worker writes is the platform's doing, not a person's.
fn by_the_platform() -> erp_eventlog::Metadata {
    erp_eventlog::Metadata::default()
}

/// **Signs the ZATCA documents that have been built and not yet signed.**
///
/// Separate from submitting because they fail for different reasons, and a
/// document needs this one even when ZATCA is unreachable: a simplified
/// invoice's QR carries the cryptographic stamp, and that receipt goes to the
/// customer at the till.
struct SignZatcaDocuments {
    sealing: erp_eventlog::SealingKey,
}

#[async_trait::async_trait]
impl erp_worker::Job for SignZatcaDocuments {
    fn name(&self) -> &'static str {
        "tax_sa.sign"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(tax_sa::module_id())
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        let signed = tax_sa::sign_pending(
            db,
            &self.sealing,
            chrono::Utc::now(),
            ZATCA_BATCH,
            &by_the_platform(),
        )
        .await?;

        // Not an error: a tenant that has not finished onboarding is in a
        // normal state, and the standing report is where that shows.
        if signed.waiting_for_a_certificate > 0 {
            tracing::debug!(
                tenant = %db.tenant(),
                waiting = signed.waiting_for_a_certificate,
                "documents are built and there is no certificate to sign them with"
            );
        }
        if signed.signed > 0 {
            tracing::info!(tenant = %db.tenant(), signed = signed.signed, "signed");
        }

        Ok(if signed.signed > 0 {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

/// **Sends signed documents to ZATCA and records what it said.**
///
/// Standard invoices are cleared, simplified ones reported, and the sweep stops
/// on the first call that is not answered — every document after it would fail
/// the same way, and none of them is what is wrong.
struct SubmitToZatca {
    sealing: erp_eventlog::SealingKey,
}

#[async_trait::async_trait]
impl erp_worker::Job for SubmitToZatca {
    fn name(&self) -> &'static str {
        "tax_sa.submit"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(tax_sa::module_id())
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        // Nothing to authenticate with, or nowhere to send it: a tenant part
        // way through onboarding, which is not a failure.
        let Some(credentials) = tax_sa::zatca::onboarding::production(db, &self.sealing).await?
        else {
            return Ok(Activity::Idle);
        };
        let Some(environment) = zatca_environment(db).await? else {
            return Ok(Activity::Idle);
        };

        let client = tax_sa::zatca::http::Fatoora::new(environment)?.with_credentials(credentials);
        let swept = tax_sa::submit_pending(
            db,
            &client,
            chrono::Utc::now(),
            ZATCA_BATCH,
            &by_the_platform(),
        )
        .await?;

        // **Loudly.** A tenant whose documents are not reaching ZATCA has 24
        // hours on every simplified invoice, and nothing else in the system
        // will say so.
        if let Some(stopped) = &swept.stopped {
            tracing::warn!(
                tenant = %db.tenant(),
                error = %stopped,
                accepted = swept.accepted,
                "the ZATCA sweep stopped early; the rest stay pending"
            );
        }
        if swept.did_something() {
            tracing::info!(
                tenant = %db.tenant(),
                accepted = swept.accepted,
                refused = swept.refused,
                "submitted to ZATCA"
            );
        }

        Ok(if swept.did_something() {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

/// **The sweeps that talk to ZATCA**, in one list.
///
/// A function rather than two `with_job` calls in `main` for the same reason
/// [`module_jobs`] is one: a test can look at what a deployment would run, and
/// a job that runs for tenants who never bought the module is caught here
/// rather than on a bill.
fn zatca_jobs(sealing: &erp_eventlog::SealingKey) -> Vec<Arc<dyn erp_worker::Job>> {
    vec![
        Arc::new(SignZatcaDocuments {
            sealing: sealing.clone(),
        }),
        Arc::new(SubmitToZatca {
            sealing: sealing.clone(),
        }),
    ]
}

/// Which ZATCA a tenant onboarded into.
///
/// `None` when they have not onboarded at all, which is most tenants most of
/// the time and is why this is not an error.
///
/// Read from the projection rather than the log (L7). The stored value is
/// re-parsed rather than trusted: it was written by this system, but a value
/// that no longer names an environment is a bad migration, and law L6 says that
/// stops rather than degrades.
async fn zatca_environment(
    db: &erp_control::TenantDb,
) -> Result<Option<tax_sa::zatca::csr::Environment>, erp_worker::BoxError> {
    let mut conn = db.read().await?;
    let onboarded = tax_sa::onboarding(&mut conn).await?;
    drop(conn);

    onboarded
        .map(|o| o.environment.parse().map_err(erp_worker::BoxError::from))
        .transpose()
}

/// **Every module's projections, in one list.**
///
/// A function rather than a chain of `with_job` calls so that a test can look at
/// it. A module missing from here is the worst omission this system has: the
/// events still commit, the ledger still balances, and the module's read models
/// stay **permanently empty** — no bill list, no input tax, and a VAT return
/// quietly under-reporting what can be reclaimed. Nothing else in the suite
/// notices, which was checked by removing one and watching everything pass.
fn module_jobs() -> Vec<Arc<dyn erp_worker::Job>> {
    vec![
        Arc::new(
            ProjectionJob::<booking::Booking>::new(
                booking::projections(),
                Arc::new(booking::upcasters().clone()),
                200,
            )
            .for_module(booking::module_id()),
        ),
        Arc::new(
            ProjectionJob::<crm::Crm>::new(
                crm::projections(),
                Arc::new(crm::upcasters().clone()),
                200,
            )
            .for_module(crm::module_id()),
        ),
        Arc::new(
            ProjectionJob::<ledger::Ledger>::new(
                ledger::projections(),
                Arc::new(ledger::upcasters().clone()),
                200,
            )
            .for_module(ledger::module_id()),
        ),
        Arc::new(
            ProjectionJob::<sales::Sales>::new(
                sales::projections(),
                Arc::new(sales::upcasters().clone()),
                200,
            )
            .for_module(sales::module_id()),
        ),
        Arc::new(
            ProjectionJob::<prepaid::Prepaid>::new(
                prepaid::projections(),
                Arc::new(prepaid::upcasters().clone()),
                200,
            )
            .for_module(prepaid::module_id()),
        ),
        Arc::new(
            ProjectionJob::<payroll::Payroll>::new(
                payroll::projections(),
                Arc::new(payroll::upcasters().clone()),
                200,
            )
            .for_module(payroll::module_id()),
        ),
        Arc::new(
            ProjectionJob::<hr::Hr>::new(hr::projections(), Arc::new(hr::upcasters().clone()), 200)
                .for_module(hr::module_id()),
        ),
        Arc::new(
            ProjectionJob::<branches::Branches>::new(
                branches::projections(),
                Arc::new(branches::upcasters().clone()),
                200,
            )
            .for_module(branches::module_id()),
        ),
        Arc::new(
            ProjectionJob::<pos::Pos>::new(
                pos::projections(),
                Arc::new(pos::upcasters().clone()),
                200,
            )
            .for_module(pos::module_id()),
        ),
        Arc::new(
            ProjectionJob::<purchases::Purchases>::new(
                purchases::projections(),
                Arc::new(purchases::upcasters().clone()),
                200,
            )
            .for_module(purchases::module_id()),
        ),
        Arc::new(
            ProjectionJob::<tax_sa::TaxSa>::new(
                tax_sa::projections(),
                Arc::new(tax_sa::upcasters().clone()),
                200,
            )
            .for_module(tax_sa::module_id()),
        ),
    ]
}

/// No bill may have been paid more than it was for.
///
/// The mirror of [`NoOverpaidInvoice`], and unreachable the same way. Two
/// invariants rather than one because a tenant may have either module without
/// the other, and `module()` is what stops a tenant being checked for something
/// they declined.
struct NoOverpaidBill;

#[async_trait::async_trait]
impl Invariant for NoOverpaidBill {
    fn name(&self) -> &'static str {
        "no_overpaid_bill"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(purchases::module_id())
    }

    async fn check(
        &self,
        db: &erp_control::TenantDb,
    ) -> Result<Vec<Finding>, erp_worker::BoxError> {
        let mut conn = db.acquire().await?;
        Ok(purchases::overpaid(&mut conn)
            .await?
            .into_iter()
            .map(|b| {
                Finding::new(
                    "no_overpaid_bill",
                    format!(
                        "bill {} is for {} and has been paid {}",
                        b.bill, b.gross, b.paid
                    ),
                )
            })
            .collect())
    }
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
        db: &erp_control::TenantDb,
    ) -> Result<Vec<Finding>, erp_worker::BoxError> {
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

#[cfg(test)]
mod tests {
    use super::{certificate_time, describe, module_jobs, zatca_jobs};
    use std::collections::BTreeSet;

    fn document(name: &str, days: i32) -> hr::Expiring {
        hr::Expiring {
            employee: name.to_owned(),
            name: name.to_owned(),
            branch: None,
            kind: "identity".to_owned(),
            number: "X".to_owned(),
            expires_on: chrono::NaiveDate::from_ymd_opt(2026, 5, 31).expect("a real date"),
            days_left: days,
        }
    }

    /// **A finding that lists two hundred people is one nobody reads.**
    ///
    /// The count is already in the sentence, so the list is the first few and
    /// then how many more.
    #[test]
    fn a_long_list_of_documents_is_summarised_rather_than_recited() {
        let few: Vec<_> = (0..3).map(|n| document(&format!("p{n}"), 5)).collect();
        let described = describe(&few);
        assert!(described.contains("p0"));
        assert!(described.contains("p2"));
        assert!(
            !described.contains("more"),
            "a short list should be named in full: {described}"
        );

        let many: Vec<_> = (0..20).map(|n| document(&format!("p{n}"), 5)).collect();
        let described = describe(&many);
        assert!(
            described.contains("and 15 more"),
            "a long list was recited in full: {described}"
        );
        assert!(
            !described.contains("p19"),
            "a long list was recited in full: {described}"
        );
    }

    /// **Every module this build offers has a projection job here.**
    ///
    /// The list of modules comes from `erp_api::modules()`, which is the one
    /// place they are enumerated — so a fourth module cannot be added to the
    /// product and left out of the worker.
    ///
    /// This is the omission nothing else catches. A module registered
    /// everywhere *except* here still signs up, still installs its tables, still
    /// accepts writes, and still posts to the ledger correctly — and its read
    /// models never fill. Verified by deleting one and watching the whole
    /// workspace stay green.
    #[test]
    fn every_module_has_a_projection_job() {
        // **A module with no projection groups needs no job**, and `hr_sa` is
        // the first: it is arithmetic and one configuration key, so there is
        // nothing to project. Keyed off the setup rather than a list of names,
        // because a name here is one somebody has to remember to remove.
        let offered: BTreeSet<String> = erp_api::modules()
            .into_iter()
            .filter(|(_, setup)| !setup.groups.is_empty())
            .map(|(_, setup)| setup.module.as_str().to_owned())
            .collect();

        let worked: BTreeSet<String> = module_jobs()
            .iter()
            .filter_map(|job| job.module())
            .map(|module| module.as_str().to_owned())
            .collect();

        assert_eq!(
            offered,
            worked,
            "a module is offered and never projected — its read models would \
             stay empty forever. Missing: {:?}",
            offered.difference(&worked).collect::<Vec<_>>()
        );
    }

    /// And every one of them is scoped to its module.
    ///
    /// A projection job with no `module()` runs for every tenant, including the
    /// ones that declined it — which is the other half of what "modular" has to
    /// mean, and a `for_module` somebody forgot looks identical until the bill
    /// arrives.
    ///
    /// **The ZATCA sweeps are in here too**, because they are the ones that
    /// would cost real money: a submit job with no `module()` opens a
    /// connection to a tax authority for every tenant on the platform.
    #[test]
    fn no_module_job_runs_for_tenants_that_declined_it() {
        let sealing = erp_eventlog::SealingKey::new("test", &[0u8; 32]).expect("32 bytes");
        for job in module_jobs().into_iter().chain(zatca_jobs(&sealing)) {
            assert!(
                job.module().is_some(),
                "{} runs for every tenant, including the ones that did not buy it",
                job.name()
            );
        }
    }

    /// **A document is signed before it is sent**, and both halves have to be
    /// registered for either to matter.
    ///
    /// Everything ZATCA-related was written before there was anything to run
    /// it: for several increments the whole path worked in tests and was
    /// unreachable in production. This is the check that says it is wired in.
    #[test]
    fn a_deployment_with_a_sealing_key_both_signs_and_submits() {
        let sealing = erp_eventlog::SealingKey::new("test", &[0u8; 32]).expect("32 bytes");
        let names: Vec<&str> = zatca_jobs(&sealing).iter().map(|job| job.name()).collect();

        assert!(names.contains(&"tax_sa.sign"), "{names:?}");
        assert!(names.contains(&"tax_sa.submit"), "{names:?}");
        assert!(
            zatca_jobs(&sealing)
                .iter()
                .all(|job| job.module() == Some(tax_sa::module_id()))
        );
    }

    /// OpenSSL prints a single-digit day with a **leading space**, which a
    /// `%d` parse rejects — so this would work for three weeks in four and
    /// report an unreadable certificate on the ninth of the month.
    #[test]
    fn a_certificates_expiry_is_read_the_way_openssl_prints_it() {
        let parsed = certificate_time("Aug 16 20:28:41 2031 GMT").expect("a date");
        assert_eq!(parsed.to_rfc3339(), "2031-08-16T20:28:41+00:00");

        let single_digit = certificate_time("Sep  9 01:02:03 2031 GMT").expect("a date");
        assert_eq!(single_digit.to_rfc3339(), "2031-09-09T01:02:03+00:00");

        assert!(certificate_time("whenever").is_none());
    }
}
