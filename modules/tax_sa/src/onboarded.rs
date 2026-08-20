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

use serde::{Deserialize, Serialize};
use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Timestamp};

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
}

impl OnboardingEvent {
    pub const NAMES: [&'static str; 1] = ["tax_sa.zatca.csid_issued"];
}

impl DomainEvent for OnboardingEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::CsidIssued { .. } => Self::NAMES[0],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
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
                // A renewal of the production certificate must not put the
                // tenant back to `compliance`, and a compliance certificate
                // re-issued after going live must not either.
                if *stage == Stage::Production || self.stage.is_none() {
                    self.stage = Some(*stage);
                }
                self.environment = Some(*environment);
                self.serial = Some(serial.clone());
                self.not_after = Some(not_after.clone());
                self.issued_at = Some(*at);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issued(stage: Stage, serial: &str) -> OnboardingEvent {
        OnboardingEvent::CsidIssued {
            stage,
            environment: Environment::Simulation,
            request_id: "1234".to_owned(),
            subject: "C=SA, CN=EGS1".to_owned(),
            serial: serial.to_owned(),
            not_before: "Jan  1 00:00:00 2026 GMT".to_owned(),
            not_after: "Jan  1 00:00:00 2031 GMT".to_owned(),
            at: Timestamp::UNIX_EPOCH,
        }
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
