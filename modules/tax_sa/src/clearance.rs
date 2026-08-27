//! What ZATCA decided about a document.
//!
//! # Why the verdict is an event
//!
//! Because it is the only part of this that did not come from us. Everything
//! else about a ZATCA document — the XML, the hash, the chain, the QR — is
//! derived from the log and can be rebuilt at any time. What ZATCA said cannot:
//! it happened once, over a network, and if it is not written down it is gone.
//!
//! Which is also why it is not a column somebody updates. A cleared invoice is a
//! legal fact with a date, and "the row says cleared" is a much weaker statement
//! than "here is the event that says so, at this position, with the stamped
//! document ZATCA returned".
//!
//! # What is not recorded
//!
//! A failure to *reach* ZATCA. A timeout, a 503, an expired certificate — none
//! of those are decisions about the document, so none of them are appended. The
//! document stays pending and the next sweep tries again. See
//! [`zatca::wire::Unanswered`](crate::zatca::wire::Unanswered).

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{DomainName, EventName, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

use crate::zatca::wire::Remark;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClearanceEvent {
    /// The document was signed with this tenant's certificate.
    ///
    /// # Why a signature is an event
    ///
    /// **ECDSA is randomised.** Signing the same bytes twice gives two
    /// different signatures, so a projection that signed would produce
    /// different tables on every rebuild — in the column a tax authority holds
    /// a copy of. Recording it makes the stored signature a replay of something
    /// that happened rather than something recomputed.
    ///
    /// The whole `ext:UBLExtensions` block is here, not just the signature
    /// value, because that block **is** the artefact: it carries the signed
    /// properties, the certificate and the digests that were signed over, and
    /// reassembling it from parts would be a second implementation of
    /// `zatca::signing` that has to agree with the first.
    Signed {
        document: String,
        /// `ds:SignatureValue`, base64. Also QR tag 7.
        signature: String,
        /// The `ext:UBLExtensions` block, verbatim.
        extensions: String,
        /// The QR with the stamp in it — tags 1 to 9.
        qr: String,
        /// Which certificate signed it, in hex. The log already says when each
        /// certificate was issued, so this is what joins the two.
        certificate_serial: String,
        at: Timestamp,
    },
    /// ZATCA accepted the document.
    ///
    /// On a standard invoice this is **clearance**, and `stamped` is the
    /// document the buyer has to be given — ZATCA's signature is on that one and
    /// not on the bytes we sent. On a simplified one it is **reporting**, there
    /// is no stamped document, and the obligation is discharged.
    Accepted {
        /// The document's number, which is its identity everywhere here.
        document: String,
        /// Which of the two calls this was the answer to.
        kind: crate::zatca::Kind,
        /// Accepted, and with something to look at. Not a refusal.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<Remark>,
        /// Base64, as ZATCA returned it. Clearance only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stamped: Option<String>,
        at: Timestamp,
    },
    /// ZATCA refused it, and the document is what is wrong.
    ///
    /// Final. Resubmitting identical bytes gets the identical answer, so the fix
    /// is a corrected document — which is a new document with its own number and
    /// its own place in the chain, never an edit to this one.
    Refused {
        document: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        errors: Vec<Remark>,
        at: Timestamp,
    },
}

impl ClearanceEvent {
    pub const NAMES: [&'static str; 3] = [
        "tax_sa.zatca.accepted",
        "tax_sa.zatca.refused",
        "tax_sa.zatca.signed",
    ];

    /// The document this is about.
    #[must_use]
    pub fn document(&self) -> &str {
        match self {
            Self::Accepted { document, .. }
            | Self::Refused { document, .. }
            | Self::Signed { document, .. } => document,
        }
    }
}

impl DomainEvent for ClearanceEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Accepted { .. } => Self::NAMES[0],
            Self::Refused { .. } => Self::NAMES[1],
            Self::Signed { .. } => Self::NAMES[2],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// What a command needs to know before recording a verdict.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Clearance {
    /// Whether ZATCA has already answered about this document. Recording the
    /// same verdict twice is a no-op, which is what makes a submitter that
    /// crashed after the call and before the append safe to re-run.
    pub settled: bool,
    pub accepted: bool,
    /// Whether it has been signed. A document is signed once — signing it again
    /// would be a second signature over the same invoice, and ZATCA holds the
    /// first.
    pub signed: bool,
    pub at: Option<Timestamp>,
}

impl Aggregate for Clearance {
    type Event = ClearanceEvent;

    fn domain() -> DomainName {
        crate::domain("tax_sa_clearance")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            ClearanceEvent::Accepted { at, .. } => {
                self.settled = true;
                self.accepted = true;
                self.at = Some(*at);
            }
            ClearanceEvent::Refused { at, .. } => {
                self.settled = true;
                self.accepted = false;
                self.at = Some(*at);
            }
            ClearanceEvent::Signed { .. } => self.signed = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_is_unsettled_until_zatca_answers() {
        let mut clearance = Clearance::default();
        assert!(!clearance.settled);

        clearance.apply(&ClearanceEvent::Accepted {
            document: "INV-00001".to_owned(),
            kind: crate::zatca::Kind::Standard,
            warnings: vec![],
            stamped: Some("PEludm9pY2U+".to_owned()),
            at: Timestamp::UNIX_EPOCH,
        });

        assert!(clearance.settled);
        assert!(clearance.accepted);
    }

    #[test]
    fn a_refusal_settles_it_too_and_is_not_an_acceptance() {
        let mut clearance = Clearance::default();
        clearance.apply(&ClearanceEvent::Refused {
            document: "INV-00001".to_owned(),
            errors: vec![Remark {
                code: "BR-KSA-40".to_owned(),
                category: "ERROR".to_owned(),
                message: "invalid VAT number".to_owned(),
            }],
            at: Timestamp::UNIX_EPOCH,
        });

        assert!(clearance.settled);
        assert!(!clearance.accepted);
    }

    #[test]
    fn every_event_names_the_document_it_is_about() {
        let accepted = ClearanceEvent::Accepted {
            document: "INV-00001".to_owned(),
            kind: crate::zatca::Kind::Simplified,
            warnings: vec![],
            stamped: None,
            at: Timestamp::UNIX_EPOCH,
        };
        let refused = ClearanceEvent::Refused {
            document: "CN-00001".to_owned(),
            errors: vec![],
            at: Timestamp::UNIX_EPOCH,
        };
        assert_eq!(accepted.document(), "INV-00001");
        assert_eq!(refused.document(), "CN-00001");
        assert_eq!(accepted.event_name().as_str(), "tax_sa.zatca.accepted");
        assert_eq!(refused.event_name().as_str(), "tax_sa.zatca.refused");
    }
}
