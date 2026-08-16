//! What ZATCA's API says, and what it says back.
//!
//! # Why the wire shapes are here without the transport
//!
//! Because they are the part that can be wrong quietly. Whether an HTTP client
//! reached `gw-fatoora.zatca.gov.sa` is something a deployment finds out in
//! seconds; whether a `202` with warnings was recorded as *cleared* or as
//! *rejected* is something a business finds out at an inspection.
//!
//! So the bodies, the two endpoints, and above all the reading of the answer are
//! modelled here and tested against recorded responses. The socket is one
//! implementation of [`Submitter`], and it needs a certificate this project does
//! not have.
//!
//! # The distinction that matters
//!
//! ZATCA saying *no* and ZATCA not answering are different facts:
//!
//! - **A verdict** — cleared, cleared with warnings, or refused — is about the
//!   document. It is recorded in the log and it is final.
//! - **A failure to ask** — a timeout, an expired certificate, a 503 — is about
//!   us. Nothing is recorded, because nothing was decided; the document stays
//!   pending and the next sweep tries again.
//!
//! Collapsing the two is how a perfectly good invoice ends up permanently marked
//! rejected because a token expired, which is [`Verdict::of`]'s whole job.

use serde::{Deserialize, Serialize};

/// Which call this document needs. The consequence of [`super::Kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// Standard invoices, **before** the buyer gets the document.
    Clearance,
    /// Simplified invoices, within 24 hours of issuing them.
    Reporting,
}

impl Endpoint {
    /// The call a document of this kind needs.
    #[must_use]
    pub const fn of(kind: super::Kind) -> Self {
        match kind {
            super::Kind::Standard => Self::Clearance,
            super::Kind::Simplified => Self::Reporting,
        }
    }

    /// The path, under whichever environment's base URL is configured.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::Clearance => "/invoices/clearance/single",
            Self::Reporting => "/invoices/reporting/single",
        }
    }

    /// The `Clearance-Status` header, which ZATCA uses to tell the two apart
    /// even though the path already does.
    #[must_use]
    pub const fn clearance_status(self) -> &'static str {
        match self {
            Self::Clearance => "1",
            Self::Reporting => "0",
        }
    }
}

/// The body both endpoints take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    /// The base64 SHA-256 of the canonical document.
    #[serde(rename = "invoiceHash")]
    pub invoice_hash: String,
    /// The document's own UUID, not its number.
    pub uuid: String,
    /// The **signed** XML, base64. Unsigned until there is a certificate, which
    /// is the one thing here that a real submission still needs.
    pub invoice: String,
}

/// One thing ZATCA has to say about a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remark {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub message: String,
}

/// Everything it had to say.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationResults {
    #[serde(default, rename = "infoMessages")]
    pub info: Vec<Remark>,
    #[serde(default, rename = "warningMessages")]
    pub warnings: Vec<Remark>,
    #[serde(default, rename = "errorMessages")]
    pub errors: Vec<Remark>,
    #[serde(default)]
    pub status: Option<String>,
}

/// The response body, which is one shape with two names for the outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    #[serde(default, rename = "validationResults")]
    pub validation: ValidationResults,
    /// `CLEARED` or `NOT_CLEARED`, on the clearance endpoint.
    #[serde(default, rename = "clearanceStatus")]
    pub clearance_status: Option<String>,
    /// `REPORTED` or `NOT_REPORTED`, on the reporting endpoint.
    #[serde(default, rename = "reportingStatus")]
    pub reporting_status: Option<String>,
    /// The signed, stamped document — **the one the buyer must be given**, not
    /// the one that was sent. Clearance only.
    #[serde(default, rename = "clearedInvoice")]
    pub cleared_invoice: Option<String>,
}

/// What ZATCA decided about a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Accepted. On a cleared standard invoice, `stamped` is the document the
    /// buyer gets — ZATCA's signature is on that one and not on ours.
    Accepted {
        warnings: Vec<Remark>,
        stamped: Option<String>,
    },
    /// Refused, and the document is what is wrong. Final: retrying identical
    /// bytes gets the same answer.
    Refused { errors: Vec<Remark> },
}

/// Asking failed. Nothing was decided, so nothing is recorded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unanswered {
    /// The credentials are wrong, expired, or were never onboarded. Every
    /// document fails the same way until somebody fixes it — which is why it is
    /// not a verdict on any of them.
    #[error("ZATCA refused the credentials ({status}); the solution needs onboarding again")]
    NotOnboarded { status: u16 },
    /// ZATCA is unwell, or we could not reach it.
    #[error("ZATCA did not answer ({0})")]
    Unavailable(String),
    /// It answered something this build cannot read, which is a change on their
    /// side and not a document that is wrong.
    #[error("ZATCA's answer could not be read: {0}")]
    Unreadable(String),
}

impl Verdict {
    /// Reads an HTTP status and body as a verdict — or as no verdict at all.
    ///
    /// | status | meaning |
    /// |---|---|
    /// | 200 | accepted |
    /// | 202 | accepted, with warnings to look at |
    /// | 400 | the document is wrong; final |
    /// | 401, 403 | **not** the document — the solution is not onboarded |
    /// | anything else | ZATCA is unwell; try again later |
    pub fn of(status: u16, body: &str) -> Result<Self, Unanswered> {
        match status {
            401 | 403 => return Err(Unanswered::NotOnboarded { status }),
            200 | 202 | 400 => {}
            other => return Err(Unanswered::Unavailable(format!("HTTP {other}: {body}"))),
        }

        // **With the body.** An answer that will not parse is diagnosed from
        // what it said, and a message carrying only the serde error ("expected
        // value at line 1 column 1") describes every empty body ever returned.
        let answer: Answer = serde_json::from_str(body).map_err(|e| {
            Unanswered::Unreadable(format!(
                "{e} — HTTP {status}, {} bytes: {}",
                body.len(),
                body.chars().take(200).collect::<String>()
            ))
        })?;

        // The status line is the authority, not the HTTP code: ZATCA has
        // answered `200` with `NOT_CLEARED` on documents it refused.
        let accepted = match (&answer.clearance_status, &answer.reporting_status) {
            (Some(status), _) => status.eq_ignore_ascii_case("CLEARED"),
            (_, Some(status)) => status.eq_ignore_ascii_case("REPORTED"),
            // No status either way: fall back to the HTTP code, which is all
            // there is. A body with neither field is already a surprise.
            (None, None) => status != 400,
        };

        if accepted {
            Ok(Self::Accepted {
                warnings: answer.validation.warnings,
                stamped: answer.cleared_invoice,
            })
        } else {
            Ok(Self::Refused {
                errors: answer.validation.errors,
            })
        }
    }
}

/// The one thing that talks to ZATCA.
///
/// # Why it is a trait with no implementation in this repository
///
/// Submitting needs a production CSID: a certificate ZATCA issues after
/// onboarding a specific solution for a specific taxpayer, and signing the
/// document with it. There is no way to write that here and have it be real, and
/// a fake one that pretended would be worse than none — the whole point of a
/// clearance record is that it happened.
///
/// So everything up to the socket is here and tested, and this is the seam. An
/// implementation is a signature, an HTTP POST with basic auth, and
/// [`Verdict::of`] on what comes back.
#[async_trait::async_trait]
pub trait Submitter: Send + Sync + std::fmt::Debug {
    /// Submits one document and reports what ZATCA decided.
    ///
    /// `Err` means *nothing was decided* — see [`Unanswered`]. A document ZATCA
    /// refused comes back as `Ok(Verdict::Refused)`.
    async fn submit(
        &self,
        endpoint: Endpoint,
        submission: &Submission,
    ) -> Result<Verdict, Unanswered>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kind_decides_the_endpoint_and_the_header() {
        assert_eq!(
            Endpoint::of(super::super::Kind::Standard),
            Endpoint::Clearance
        );
        assert_eq!(
            Endpoint::of(super::super::Kind::Simplified),
            Endpoint::Reporting
        );
        assert_eq!(Endpoint::Clearance.clearance_status(), "1");
        assert_eq!(Endpoint::Reporting.clearance_status(), "0");
        assert!(Endpoint::Clearance.path().ends_with("/clearance/single"));
        assert!(Endpoint::Reporting.path().ends_with("/reporting/single"));
    }

    #[test]
    fn a_cleared_invoice_comes_back_with_the_document_the_buyer_gets() {
        let body = r#"{
            "validationResults": {"infoMessages": [], "warningMessages": [],
                                  "errorMessages": [], "status": "PASS"},
            "clearanceStatus": "CLEARED",
            "clearedInvoice": "PEludm9pY2U+"
        }"#;
        assert_eq!(
            Verdict::of(200, body),
            Ok(Verdict::Accepted {
                warnings: vec![],
                stamped: Some("PEludm9pY2U+".to_owned())
            })
        );
    }

    #[test]
    fn a_reported_invoice_has_no_stamped_document_and_that_is_normal() {
        let body = r#"{"validationResults": {"status": "PASS"}, "reportingStatus": "REPORTED"}"#;
        assert_eq!(
            Verdict::of(200, body),
            Ok(Verdict::Accepted {
                warnings: vec![],
                stamped: None
            })
        );
    }

    /// Accepted, and there is something to look at. Recorded as accepted, since
    /// it is.
    #[test]
    fn warnings_do_not_make_an_accepted_document_a_rejected_one() {
        let body = r#"{
            "validationResults": {"warningMessages": [
                {"code": "BR-KSA-09", "category": "WARNING", "message": "check the address"}
            ], "status": "PASS"},
            "reportingStatus": "REPORTED"
        }"#;
        let Ok(Verdict::Accepted { warnings, .. }) = Verdict::of(202, body) else {
            panic!("a 202 with warnings is accepted");
        };
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "BR-KSA-09");
    }

    #[test]
    fn a_refused_document_carries_why() {
        let body = r#"{
            "validationResults": {"errorMessages": [
                {"code": "BR-KSA-40", "category": "ERROR", "message": "invalid VAT number"}
            ], "status": "ERROR"},
            "clearanceStatus": "NOT_CLEARED"
        }"#;
        let Ok(Verdict::Refused { errors }) = Verdict::of(400, body) else {
            panic!("NOT_CLEARED is a refusal");
        };
        assert_eq!(errors[0].code, "BR-KSA-40");
    }

    /// ZATCA has answered `200` with `NOT_CLEARED`. The status line wins.
    #[test]
    fn the_status_line_beats_the_http_code() {
        let body = r#"{"clearanceStatus": "NOT_CLEARED",
                       "validationResults": {"errorMessages": [{"code": "X"}]}}"#;
        assert!(matches!(
            Verdict::of(200, body),
            Ok(Verdict::Refused { .. })
        ));
    }

    /// **The distinction the module exists for.** A token that expired is not a
    /// verdict on an invoice, and recording it as one marks a good document bad
    /// forever.
    #[test]
    fn an_expired_certificate_is_not_a_rejected_invoice() {
        for status in [401, 403] {
            assert_eq!(
                Verdict::of(status, "unauthorized"),
                Err(Unanswered::NotOnboarded { status })
            );
        }
        assert!(matches!(
            Verdict::of(503, "maintenance"),
            Err(Unanswered::Unavailable(_))
        ));
        assert!(matches!(
            Verdict::of(200, "not json at all"),
            Err(Unanswered::Unreadable(_))
        ));
    }

    #[test]
    fn a_submission_serialises_to_the_field_names_zatca_reads() {
        let json = serde_json::to_value(Submission {
            invoice_hash: "aGFzaA==".to_owned(),
            uuid: "3cf5ee18-ee25-44ea-a444-2c37ba7f28be".to_owned(),
            invoice: "PEludm9pY2U+".to_owned(),
        })
        .expect("serialises");
        assert_eq!(json["invoiceHash"], "aGFzaA==");
        assert_eq!(json["uuid"], "3cf5ee18-ee25-44ea-a444-2c37ba7f28be");
        assert_eq!(json["invoice"], "PEludm9pY2U+");
    }
}
