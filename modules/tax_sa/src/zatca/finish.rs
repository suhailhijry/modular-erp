//! What the worker does once a tenant holds a compliance certificate.
//!
//! # Why this is the worker's, and the OTP exchange is not
//!
//! The route spends the OTP: it is the taxpayer's proof of who they are for
//! about an hour, and the one call that needs it is answered while they wait.
//! Everything after that — six signed samples and the production request —
//! needs only the compliance certificate, which is sealed here, so nothing
//! about it needs the taxpayer or a request handler holding a connection
//! through seven network calls. The worker runs it, retries what ZATCA did not
//! answer, and records what it refused.
//!
//! # What a pass reads, and what it writes
//!
//! It reads the onboarding read model (law L7: a worker loads no aggregate)
//! and decides from it what is [`due`]. It writes through the commands:
//! `ChecksPassed` once the samples are accepted, `Refused` when ZATCA says no,
//! and `CsidIssued` for the production certificate. All three are idempotent,
//! so a pass that crashes between ZATCA's answer and the write repeats the
//! step and writes nothing the second time.
//!
//! # Why a refusal waits for a new build
//!
//! The samples are generated here. If ZATCA refuses one, the same build would
//! be refused again on the next pass, every pass, for as long as the tenant
//! waits — so the refusal records the build version and the same version does
//! not ask again. A deploy that fixes the samples tries once more without
//! anybody remembering to.

use erp_eventlog::{Metadata, SealingKey};
use erp_tenant::TenantDb;
use erp_types::Timestamp;

use super::csr::{Environment, Issues};
use super::onboarding::{ComplianceChecks, Issued, OnboardError, Onboarder, Registrar, Stage};
use crate::onboarded::Step;

/// The step a tenant at compliance is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// The six samples have not passed for the current certificate.
    Checks,
    /// They have; the production certificate has not been asked for, or was
    /// not answered.
    Production,
}

/// What a tenant needs next, or nothing: live already, never onboarded, or
/// waiting on a refusal this build cannot change.
#[must_use]
pub fn due(onboarded: &crate::Onboarded) -> Option<Due> {
    if onboarded.stage != Stage::Compliance.as_str() {
        return None;
    }
    if onboarded.refused_version.as_deref() == Some(env!("CARGO_PKG_VERSION")) {
        return None;
    }
    if onboarded.checks_serial.as_deref() == Some(onboarded.serial.as_str()) {
        Some(Due::Production)
    } else {
        Some(Due::Checks)
    }
}

/// What one pass did.
#[derive(Debug, Default)]
pub struct Finished {
    /// The samples, when this pass submitted them.
    pub checks: Option<ComplianceChecks>,
    /// The production certificate, when this pass obtained it.
    pub production: Option<Issued>,
    /// The step ZATCA refused, when this pass recorded a refusal.
    pub refused: Option<Step>,
}

impl Finished {
    #[must_use]
    pub const fn did_something(&self) -> bool {
        self.checks.is_some() || self.production.is_some() || self.refused.is_some()
    }
}

/// Takes a tenant from a compliance certificate as far as ZATCA lets it.
///
/// Returns `Err` only for what nobody decided — ZATCA unreachable, the database
/// down — and the caller tries again next pass. A refusal is a decision, and
/// comes back as `Ok` with [`Finished::refused`] set.
pub async fn finish(
    db: &TenantDb,
    sealing: &SealingKey,
    registrar: &dyn Registrar,
    now: Timestamp,
    metadata: &Metadata,
) -> Result<Finished, OnboardError> {
    let mut conn = db.read().await?;
    let row = crate::onboarding(&mut conn).await?;
    let registration = crate::registered(&mut conn).await?;
    drop(conn);

    let Some(row) = row else {
        return Ok(Finished::default());
    };
    let Some(mut step) = due(&row) else {
        return Ok(Finished::default());
    };
    let environment: Environment = row.environment.parse().map_err(OnboardError::Certificate)?;
    let onboarder = Onboarder::new(db, sealing, registrar);
    let version = env!("CARGO_PKG_VERSION");
    let mut finished = Finished::default();

    if step == Due::Checks {
        let registration = registration.ok_or(OnboardError::NotRegistered)?;
        let checks = onboarder
            .pass_compliance_checks(&registration, Issues::both(), environment, now)
            .await?;
        if !checks.all_passed() {
            // **Ours, not the tenant's.** The samples are generated here, so a
            // refusal is a bug in this software: the whole list to the log,
            // the first one to the record.
            tracing::error!(
                submitted = checks.submitted,
                passed = checks.passed,
                failures = ?checks.failures,
                "ZATCA refused a compliance document"
            );
            crate::commands::record_refusal(
                db,
                Step::ComplianceChecks,
                &first_failure(&checks),
                version,
                now,
                metadata,
            )
            .await?;
            finished.checks = Some(checks);
            finished.refused = Some(Step::ComplianceChecks);
            return Ok(finished);
        }
        crate::commands::record_checks_passed(db, &row.serial, checks.submitted, now, metadata)
            .await?;
        finished.checks = Some(checks);
        step = Due::Production;
    }

    debug_assert_eq!(step, Due::Production);
    match onboarder.go_live(environment, now, metadata).await {
        Ok(issued) => finished.production = Some(issued),
        Err(OnboardError::NotIssued {
            disposition,
            detail,
        }) => {
            crate::commands::record_refusal(
                db,
                Step::ProductionCertificate,
                &format!("{disposition}: {detail}"),
                version,
                now,
                metadata,
            )
            .await?;
            finished.refused = Some(Step::ProductionCertificate);
        }
        Err(other) => return Err(other),
    }
    Ok(finished)
}

/// The first refused document and its first error, as one line.
fn first_failure(checks: &ComplianceChecks) -> String {
    checks
        .failures
        .first()
        .map(|(document, errors)| {
            let first = errors
                .first()
                .map_or_else(String::new, |e| format!("{}: {}", e.code, e.message));
            format!("{document} — {first}")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_compliance() -> crate::Onboarded {
        crate::Onboarded {
            stage: "compliance".to_owned(),
            environment: "simulation".to_owned(),
            serial: "01".to_owned(),
            not_after: "Jan  1 00:00:00 2031 GMT".to_owned(),
            issued_at: Timestamp::UNIX_EPOCH,
            checks_serial: None,
            checks_submitted: None,
            checks_passed_at: None,
            refused_step: None,
            refused_detail: None,
            refused_version: None,
            refused_at: None,
        }
    }

    /// **What is due follows the row, and a refusal binds only the build that
    /// was refused.** A new build sees the same row and tries again.
    #[test]
    fn a_refusal_holds_this_build_and_releases_the_next() {
        let mut row = at_compliance();
        assert_eq!(due(&row), Some(Due::Checks));

        row.refused_version = Some(env!("CARGO_PKG_VERSION").to_owned());
        assert_eq!(due(&row), None, "this build was refused");

        row.refused_version = Some("0.0.0-before".to_owned());
        assert_eq!(
            due(&row),
            Some(Due::Checks),
            "a later build tries once more"
        );

        row.refused_version = None;
        row.checks_serial = Some("01".to_owned());
        assert_eq!(due(&row), Some(Due::Production));

        row.checks_serial = Some("00".to_owned());
        assert_eq!(
            due(&row),
            Some(Due::Checks),
            "checks passed for another certificate are no evidence"
        );

        row.stage = "production".to_owned();
        assert_eq!(due(&row), None);
    }
}
