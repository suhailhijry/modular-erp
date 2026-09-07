//! That a certificate was issued, and which one.
//!
//! # What is in the log and what is not
//!
//! The certificate's **identity** — its subject, its serial, when it is valid,
//! which environment issued it — is a fact with a date, and it is the answer to
//! "which certificate signed this invoice?" asked three years later. That goes
//! in the log.
//!
//! The private key and the CSID secret do not, and could not: the log is
//! immutable and replicated, so a secret written into it can never be rotated
//! out and exists in every copy forever. They are sealed in `module_secret`
//! instead — see [`crate::zatca::onboarding`].
//!
//! The OTP appears in neither. It is the taxpayer's proof of who they are for
//! about an hour, and recording it would be recording a credential.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

use crate::zatca::csr::Environment;
use crate::zatca::onboarding::Stage;

/// The stream a tenant's certificates live in. One EGS unit per tenant, so one.
#[must_use]
pub fn onboarding_id() -> AggregateId {
    AggregateId::new("self")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies AggregateId"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OnboardingEvent {
    /// ZATCA issued a certificate for this tenant's unit.
    ///
    /// Appended for the compliance one and again for the production one, and
    /// again on every renewal — each is a separate certificate with its own
    /// validity, and the history is what makes an old signature explicable.
    CsidIssued {
        stage: Stage,
        environment: Environment,
        /// ZATCA's id for the request. What their support desk asks for.
        request_id: String,
        /// The certificate's subject, as one line.
        subject: String,
        /// Its serial number, in hex.
        serial: String,
        /// As the certificate states them, in its own format — kept as text
        /// rather than parsed, because what matters is what the certificate
        /// says and a parse is a second opinion about it.
        not_before: String,
        not_after: String,
        at: Timestamp,
    },
    /// Every compliance sample ZATCA was shown was accepted — what the
    /// production certificate is asked for on the strength of.
    ChecksPassed {
        /// The compliance certificate the samples were signed with. Checks
        /// passed for one certificate are no evidence for the next.
        certificate_serial: String,
        submitted: usize,
        at: Timestamp,
    },
    /// ZATCA refused a step the worker cannot argue with.
    ///
    /// Recorded rather than retried: a refused sample is this software's fault
    /// and the same samples would be refused again. The build version says
    /// which software was refused, so a new build gets one more try.
    Refused {
        step: Step,
        /// ZATCA's words — or the first refused document and its first error.
        detail: String,
        /// `CARGO_PKG_VERSION` of the build that was refused.
        version: String,
        at: Timestamp,
    },
}

impl OnboardingEvent {
    pub const NAMES: [&'static str; 3] = [
        "tax_sa.zatca.csid_issued",
        "tax_sa.zatca.checks_passed",
        "tax_sa.zatca.step_refused",
    ];
}

impl DomainEvent for OnboardingEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::CsidIssued { .. } => Self::NAMES[0],
            Self::ChecksPassed { .. } => Self::NAMES[1],
            Self::Refused { .. } => Self::NAMES[2],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// The steps the worker takes after the certificate, and can be refused at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// The six sample documents.
    ComplianceChecks,
    /// The production certificate request.
    ProductionCertificate,
}

impl Step {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ComplianceChecks => "compliance_checks",
            Self::ProductionCertificate => "production_certificate",
        }
    }
}

/// What ZATCA last refused, standing against the current certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub step: Step,
    pub detail: String,
    pub version: String,
    pub at: Timestamp,
}

/// What a command needs to know about a tenant's certificates.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Onboarding {
    /// The stage this tenant has reached. `Production` means it can clear real
    /// invoices; `Compliance` means it is half way.
    pub stage: Option<Stage>,
    pub environment: Option<Environment>,
    /// The serial of the certificate currently in force, for telling a repeat
    /// caller that nothing changed.
    pub serial: Option<String>,
    /// When it stops working, **as the certificate states it**. Text rather
    /// than an instant because the certificate is the authority and a parse is
    /// a second opinion about it — see the worker's `CertificateExpiry`.
    pub not_after: Option<String>,
    pub issued_at: Option<Timestamp>,
    /// The compliance certificate whose samples all passed, if any.
    pub checks_passed_for: Option<String>,
    /// What ZATCA refused about the current certificate, if anything.
    pub refused: Option<Refusal>,
}

impl Aggregate for Onboarding {
    type Event = OnboardingEvent;

    fn domain() -> DomainName {
        crate::domain("tax_sa_onboarding")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            OnboardingEvent::CsidIssued {
                stage,
                environment,
                serial,
                not_after,
                at,
                ..
            } => {
                // **Another environment is a fresh start**: what was reached in
                // simulation says nothing about production. Within one, a
                // renewal of the production certificate must not put the tenant
                // back to `compliance`, and a compliance certificate re-issued
                // after going live must not either.
                let moved = self.environment.is_some_and(|had| had != *environment);
                if moved || *stage == Stage::Production || self.stage.is_none() {
                    self.stage = Some(*stage);
                }
                if *stage == Stage::Compliance {
                    // A new certificate has to earn its own passed checks.
                    self.checks_passed_for = None;
                }
                // Any certificate answers whatever was refused about the last.
                self.refused = None;
                self.environment = Some(*environment);
                self.serial = Some(serial.clone());
                self.not_after = Some(not_after.clone());
                self.issued_at = Some(*at);
            }
            OnboardingEvent::ChecksPassed {
                certificate_serial, ..
            } => self.checks_passed_for = Some(certificate_serial.clone()),
            OnboardingEvent::Refused {
                step,
                detail,
                version,
                at,
            } => {
                self.refused = Some(Refusal {
                    step: *step,
                    detail: detail.clone(),
                    version: version.clone(),
                    at: *at,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issued_in(environment: Environment, stage: Stage, serial: &str) -> OnboardingEvent {
        OnboardingEvent::CsidIssued {
            stage,
            environment,
            request_id: "1234".to_owned(),
            subject: "C=SA, CN=EGS1".to_owned(),
            serial: serial.to_owned(),
            not_before: "Jan  1 00:00:00 2026 GMT".to_owned(),
            not_after: "Jan  1 00:00:00 2031 GMT".to_owned(),
            at: Timestamp::UNIX_EPOCH,
        }
    }

    fn issued(stage: Stage, serial: &str) -> OnboardingEvent {
        issued_in(Environment::Simulation, stage, serial)
    }

    /// **Another environment is a fresh start.** Live in simulation says nothing
    /// about production, and reading it as "production" would send real
    /// invoices with simulation's certificate.
    #[test]
    fn a_certificate_for_another_environment_starts_over() {
        let mut onboarding = Onboarding::default();
        onboarding.apply(&issued(Stage::Compliance, "01"));
        onboarding.apply(&issued(Stage::Production, "02"));
        onboarding.apply(&issued_in(Environment::Production, Stage::Compliance, "03"));

        assert_eq!(onboarding.stage, Some(Stage::Compliance));
        assert_eq!(onboarding.environment, Some(Environment::Production));
    }

    /// **Checks and refusals belong to the certificate they were for.** A new
    /// compliance certificate has to earn its own passed checks, and clears
    /// whatever ZATCA refused about the old one.
    #[test]
    fn checks_and_refusals_belong_to_the_certificate_they_were_for() {
        let mut onboarding = Onboarding::default();
        onboarding.apply(&issued(Stage::Compliance, "01"));
        onboarding.apply(&OnboardingEvent::ChecksPassed {
            certificate_serial: "01".to_owned(),
            submitted: 6,
            at: Timestamp::UNIX_EPOCH,
        });
        onboarding.apply(&OnboardingEvent::Refused {
            step: Step::ProductionCertificate,
            detail: "REJECTED: not yet".to_owned(),
            version: "0.1.0".to_owned(),
            at: Timestamp::UNIX_EPOCH,
        });
        assert_eq!(onboarding.checks_passed_for.as_deref(), Some("01"));
        assert_eq!(
            onboarding.refused.as_ref().map(|r| r.step),
            Some(Step::ProductionCertificate)
        );

        onboarding.apply(&issued(Stage::Compliance, "02"));
        assert_eq!(onboarding.checks_passed_for, None);
        assert_eq!(onboarding.refused, None);
    }

    #[test]
    fn a_tenant_reaches_production_through_compliance() {
        let mut onboarding = Onboarding::default();
        assert_eq!(onboarding.stage, None);

        onboarding.apply(&issued(Stage::Compliance, "01"));
        assert_eq!(onboarding.stage, Some(Stage::Compliance));

        onboarding.apply(&issued(Stage::Production, "02"));
        assert_eq!(onboarding.stage, Some(Stage::Production));
        assert_eq!(onboarding.serial.as_deref(), Some("02"));
    }

    /// **A renewal must not demote a tenant.** Re-onboarding for any reason
    /// appends a compliance certificate, and a live business reading its own
    /// status as "not live" would be told to stop invoicing.
    #[test]
    fn a_later_compliance_certificate_does_not_take_production_away() {
        let mut onboarding = Onboarding::default();
        onboarding.apply(&issued(Stage::Compliance, "01"));
        onboarding.apply(&issued(Stage::Production, "02"));
        onboarding.apply(&issued(Stage::Compliance, "03"));

        assert_eq!(onboarding.stage, Some(Stage::Production));
        assert_eq!(
            onboarding.serial.as_deref(),
            Some("03"),
            "the newest certificate is still the one on record"
        );
    }
}
