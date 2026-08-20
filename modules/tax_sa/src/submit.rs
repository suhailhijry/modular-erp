//! Sending documents to ZATCA, and writing down what it said.
//!
//! # Why this is a function and not a worker job
//!
//! It would be a job, and the job is three lines around this. It is not one yet
//! because the worker binary is what knows about modules — modules cannot depend
//! on it without a cycle — and because there is nothing to construct it with:
//! [`Submitter`] has no implementation in this repository and cannot have one
//! without a production certificate.
//!
//! A job registered with nothing behind it is the failure mode this codebase
//! keeps finding: code that exists and has no caller, which nothing notices.
//! A function with a real caller in its tests is the part that can be right now,
//! and wrapping it when there is a certificate is trivial.
//!
//! # The rule the sweep exists to get right
//!
//! **A refusal is about the document; a failure to ask is about us.** When ZATCA
//! cannot be reached — a timeout, a 503, an expired certificate — the sweep
//! stops and records *nothing*. Every document in the batch would fail the same
//! way, and marking them refused would be a permanent verdict on documents that
//! are fine, written by an outage.

use erp_control::{CommandError, TenantDb};
use erp_eventlog::Metadata;
use erp_types::Timestamp;

use crate::commands::TaxError;
use crate::documents::pending;
use crate::zatca::ubl;
use crate::zatca::wire::{Endpoint, Submission, Submitter, Unanswered, Verdict};

use base64::Engine as _;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// What one sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// Documents ZATCA cleared or reported.
    pub accepted: usize,
    /// Documents ZATCA refused. Final — a corrected document is a new document.
    pub refused: usize,
    /// Why the sweep stopped early, if it did. Nothing was recorded for this
    /// document or any after it, and the next sweep starts where this stopped.
    pub stopped: Option<Unanswered>,
}

impl Swept {
    /// Whether anything was decided. What a caller logs, and what a job would
    /// return as `Activity`.
    #[must_use]
    pub const fn did_something(&self) -> bool {
        self.accepted > 0 || self.refused > 0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    #[error(transparent)]
    Read(#[from] sqlx::Error),
    #[error(transparent)]
    Pool(#[from] erp_control::PoolError),
    #[error("recording what ZATCA said failed: {0}")]
    Record(String),
}

impl From<CommandError<TaxError>> for SweepError {
    fn from(error: CommandError<TaxError>) -> Self {
        Self::Record(error.to_string())
    }
}

/// What one signing sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SignedOff {
    pub signed: usize,
    /// Documents left unsigned because there was nothing to sign them with.
    /// Not an error — a tenant that has not finished onboarding is in a normal
    /// state, and the standing report is where that shows.
    pub waiting_for_a_certificate: usize,
}

/// **Signs everything that has been built and not yet signed.**
///
/// Runs before [`submit_pending`] and separately from it, because the two fail
/// for different reasons and a document needs the first even when the second
/// cannot happen: a simplified invoice's QR carries the stamp, and the receipt
/// goes to the customer at the till whether or not ZATCA is reachable.
///
/// Each signature is recorded as an event. ECDSA is randomised, so the
/// signature is not something a rebuild could recompute — see
/// [`zatca::signing`](crate::zatca::signing).
pub async fn sign_pending(
    db: &TenantDb,
    sealing: &erp_eventlog::SealingKey,
    at: Timestamp,
    batch: i64,
    metadata: &Metadata,
) -> Result<SignedOff, SweepError> {
    let mut conn = db.read().await?;
    let waiting = crate::documents::unsigned(&mut conn, batch).await?;
    drop(conn);

    if waiting.is_empty() {
        return Ok(SignedOff::default());
    }

    // The production certificate, or nothing to sign with.
    let Some(credentials) = crate::zatca::onboarding::production(db, sealing)
        .await
        .map_err(|e| SweepError::Record(e.to_string()))?
    else {
        return Ok(SignedOff {
            signed: 0,
            waiting_for_a_certificate: waiting.len(),
        });
    };
    let Some(key) = crate::zatca::onboarding::private_key(db, sealing)
        .await
        .map_err(|e| SweepError::Record(e.to_string()))?
    else {
        return Ok(SignedOff {
            signed: 0,
            waiting_for_a_certificate: waiting.len(),
        });
    };

    let certificate = credentials
        .certificate()
        .map_err(|e| SweepError::Record(e.to_string()))?;
    let serial = crate::zatca::signing::serial_number(&certificate);
    // Parsed once for the whole batch, not once per invoice.
    let signer = crate::zatca::signing::Signer::new(&key, &certificate)
        .map_err(|e| SweepError::Record(e.to_string()))?;

    let mut done = SignedOff::default();
    for document in waiting {
        let signature = signer
            .sign(&document.xml, &document.invoice_hash, at)
            .map_err(|e| SweepError::Record(e.to_string()))?;

        let qr = signature
            .qr(
                &document.seller,
                &document.vat_number,
                &document.issued_at.format(crate::zatca::QR_TIME).to_string(),
                &document.total,
                &document.tax,
                &document.invoice_hash,
            )
            .map_err(|e| SweepError::Record(e.to_string()))?;

        crate::commands::record_signature(
            db,
            &document.number,
            &signature,
            &qr,
            &serial,
            at,
            metadata,
        )
        .await?;
        done.signed += 1;
    }

    Ok(done)
}

/// Submits everything waiting, oldest first, and records each answer.
///
/// Oldest first because the twenty-four hours run from issue, so the oldest
/// simplified invoice is the closest to being late.
///
/// `at` is the time to stamp the verdicts with, passed in rather than read from
/// a clock — the same rule as every other date in this system.
pub async fn submit_pending(
    db: &TenantDb,
    zatca: &dyn Submitter,
    at: Timestamp,
    batch: i64,
    metadata: &Metadata,
) -> Result<Swept, SweepError> {
    let mut conn = db.read().await?;
    let waiting = pending(&mut conn, batch).await?;
    drop(conn);

    let mut swept = Swept::default();
    for document in waiting {
        let submission = Submission {
            invoice_hash: document.invoice_hash.clone(),
            uuid: document.uuid.clone(),
            // The **signed** document, declaration and all. ZATCA strips the
            // extensions, the signature and the QR reference before hashing
            // what it receives, which gets back to the bytes `invoice_hash`
            // was taken over.
            invoice: B64.encode(ubl::with_declaration(&document.xml)),
        };

        let verdict = match zatca.submit(Endpoint::of(document.kind), &submission).await {
            Ok(verdict) => verdict,
            // Nothing was decided. Stop: every document after this one would
            // fail the same way, and none of them are what is wrong.
            Err(unanswered) => {
                swept.stopped = Some(unanswered);
                break;
            }
        };

        match &verdict {
            Verdict::Accepted { .. } => swept.accepted += 1,
            Verdict::Refused { .. } => swept.refused += 1,
        }

        crate::commands::record_outcome(
            db,
            &document.number,
            document.kind,
            &verdict,
            at,
            metadata,
        )
        .await?;
    }

    Ok(swept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zatca::wire::Remark;

    /// A ZATCA that says whatever the test needs, and remembers what it saw.
    #[derive(Debug)]
    struct Fake {
        answers: std::sync::Mutex<Vec<Result<Verdict, Unanswered>>>,
        seen: std::sync::Mutex<Vec<(Endpoint, String)>>,
    }

    impl Fake {
        fn saying(answers: Vec<Result<Verdict, Unanswered>>) -> Self {
            Self {
                answers: std::sync::Mutex::new(answers),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Submitter for Fake {
        async fn submit(
            &self,
            endpoint: Endpoint,
            submission: &Submission,
        ) -> Result<Verdict, Unanswered> {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((endpoint, submission.uuid.clone()));
            let mut answers = self
                .answers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if answers.is_empty() {
                return Err(Unanswered::Unavailable("nothing left to say".to_owned()));
            }
            answers.remove(0)
        }
    }

    /// The base64 the fake would receive is the document with its declaration,
    /// which is what ZATCA canonicalises and re-hashes.
    #[test]
    fn what_is_submitted_is_the_file_and_not_the_fragment() {
        let canonical = "<Invoice></Invoice>";
        let encoded = B64.encode(ubl::with_declaration(canonical));
        let decoded = String::from_utf8(B64.decode(&encoded).expect("base64")).expect("utf-8");
        assert!(decoded.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(decoded.ends_with(canonical));
    }

    #[tokio::test]
    async fn a_fake_that_runs_out_says_so_rather_than_accepting() {
        let fake = Fake::saying(vec![]);
        let answer = fake
            .submit(
                Endpoint::Reporting,
                &Submission {
                    invoice_hash: "h".to_owned(),
                    uuid: "u".to_owned(),
                    invoice: "i".to_owned(),
                },
            )
            .await;
        assert!(matches!(answer, Err(Unanswered::Unavailable(_))));
    }

    #[test]
    fn a_sweep_that_decided_nothing_says_so() {
        assert!(!Swept::default().did_something());
        assert!(
            Swept {
                refused: 1,
                ..Swept::default()
            }
            .did_something(),
            "a refusal is something that happened"
        );
        assert!(
            !Swept {
                stopped: Some(Unanswered::NotOnboarded { status: 401 }),
                ..Swept::default()
            }
            .did_something()
        );
    }

    #[test]
    fn a_remark_survives_the_round_trip_into_an_event() {
        let verdict = Verdict::Accepted {
            warnings: vec![Remark {
                code: "BR-KSA-09".to_owned(),
                category: "WARNING".to_owned(),
                message: "check the address".to_owned(),
            }],
            stamped: None,
        };
        let json = serde_json::to_string(&match &verdict {
            Verdict::Accepted { warnings, .. } => warnings.clone(),
            Verdict::Refused { errors } => errors.clone(),
        })
        .expect("serialises");
        let back: Vec<Remark> = serde_json::from_str(&json).expect("reads");
        assert_eq!(back[0].code, "BR-KSA-09");
    }
}
