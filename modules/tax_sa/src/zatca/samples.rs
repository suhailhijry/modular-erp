//! The sample documents ZATCA's compliance checks demand.
//!
//! # What step 3 actually is
//!
//! Between the compliance certificate and the production one, ZATCA makes the
//! solution prove it can produce valid documents: **one of every type the CSR
//! declared**, signed with the compliance certificate, submitted to
//! `/compliance/invoices`. Six for a unit that issues both kinds — invoice,
//! credit note and debit note, standard and simplified — three for one.
//!
//! # Why these are invented and not real invoices
//!
//! Because there are none yet. A business onboards before it issues, and the
//! checks are about the *solution*, not about anything that was sold. So these
//! are synthetic: the taxpayer's own registration, one line, one riyal.
//!
//! # And why they have their own chain
//!
//! **The compliance chain starts at one and is thrown away.** It shares nothing
//! with the tenant's real counter, which has not started yet and must start at
//! one when it does. Deriving these from `proj_tax_sa.zatca_document` would
//! either consume six real positions or produce a chain with a gap where the
//! samples were — and a gap is the one thing the chain exists to make
//! impossible.

use ledger::VatCategory;
use spa_types::{CurrencyCode, Money, Timestamp};

use super::csr::{Issues, Unit};
use super::{Band, Buyer, Document, Kind, Line, Link, Reference, Totals, TypeCode, document_uuid};
use crate::taxpayer::Registration;

/// One riyal, which is enough to be a document and little enough to be
/// obviously not a sale.
const NET: i64 = 100;
/// 15% of it.
const TAX: i64 = 15;

/// Every document the declared types require, already chained.
///
/// The order is the chain: each points at the one before, the first at ZATCA's
/// genesis value. Submitting them out of order is submitting a broken chain.
pub fn compliance_documents(
    registration: &Registration,
    unit: &Unit,
    at: Timestamp,
) -> Vec<Document> {
    let mut kinds: Vec<Kind> = Vec::new();
    if unit.issues.standard {
        kinds.push(Kind::Standard);
    }
    if unit.issues.simplified {
        kinds.push(Kind::Simplified);
    }

    let mut documents = Vec::new();
    for kind in kinds {
        for type_code in [TypeCode::Invoice, TypeCode::CreditNote, TypeCode::DebitNote] {
            documents.push(sample(registration, kind, type_code, at, documents.len()));
        }
    }
    documents
}

/// How many documents these checks will produce, without producing them.
#[must_use]
pub const fn expected(issues: Issues) -> usize {
    issues.compliance_documents()
}

fn sample(
    registration: &Registration,
    kind: Kind,
    type_code: TypeCode,
    at: Timestamp,
    index: usize,
) -> Document {
    let currency = CurrencyCode::new("SAR").unwrap_or_else(|_| {
        unreachable!("SAR is a valid currency code");
    });
    let net = Money::from_minor(NET, currency);
    let tax = Money::from_minor(TAX, currency);

    // A number nobody could mistake for a real one, and distinct per document
    // so the UUIDs differ.
    let number = format!("COMPLIANCE-{}-{}", type_code.code(), index + 1);

    Document {
        kind,
        type_code,
        uuid: document_uuid(&registration.vat_number, &number),
        number,
        issued_at: at,
        currency,
        seller: registration.clone(),
        // A standard document needs a buyer with a VAT number — it is what
        // makes it standard — and a simplified one needs none.
        buyer: match kind {
            Kind::Standard => Some(Buyer {
                name: "مشترٍ للاختبار".to_owned(),
                vat_number: Some("399999999900003".to_owned()),
                // ZATCA wants street, city and country on a standard document.
                address: Some(Box::new(sales::Address {
                    street: "طريق العروبة".to_owned(),
                    city: "الرياض".to_owned(),
                    country: "SA".to_owned(),
                    district: Some("الملز".to_owned()),
                    building: Some("4321".to_owned()),
                    postal_code: Some("12611".to_owned()),
                })),
            }),
            Kind::Simplified => None,
        },
        lines: vec![Line {
            description: "بند اختبار".to_owned(),
            net,
            category: VatCategory::Standard,
            rate_bp: 1_500,
            tax,
        }],
        allowances: Vec::new(),
        totals: Totals {
            net,
            tax,
            before_discount: None,
            gross: Money::from_minor(NET + TAX, currency),
            bands: vec![Band {
                category: VatCategory::Standard,
                rate_bp: 1_500,
                net,
                tax,
            }],
        },
        // Filled in by `chain`, which needs the hash of the document before it.
        link: Link::first(),
        // A credit or debit note has to say what it is against, and ZATCA
        // refuses one that does not.
        reference: match type_code {
            TypeCode::Invoice => None,
            TypeCode::CreditNote | TypeCode::DebitNote => Some(Reference {
                number: format!("COMPLIANCE-388-{index}"),
                issued_at: at,
            }),
        },
        note: match type_code {
            TypeCode::Invoice => String::new(),
            TypeCode::CreditNote => "إرجاع للاختبار".to_owned(),
            TypeCode::DebitNote => "تعديل للاختبار".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> Timestamp {
        "2026-01-01T00:00:00Z".parse().expect("a valid instant")
    }

    fn unit(issues: Issues) -> Unit {
        Unit {
            issues,
            ..crate::zatca::csr::tests::unit()
        }
    }

    #[test]
    fn a_unit_that_issues_both_kinds_has_six_documents_to_prove_it() {
        let documents = compliance_documents(
            &crate::taxpayer::tests::registration(),
            &unit(Issues::both()),
            at(),
        );
        assert_eq!(documents.len(), 6);
        assert_eq!(documents.len(), expected(Issues::both()));

        // One of every type, for each kind.
        for kind in [Kind::Standard, Kind::Simplified] {
            for code in [388, 381, 383] {
                assert!(
                    documents
                        .iter()
                        .any(|d| d.kind == kind && d.type_code.code() == code),
                    "no {kind:?} document with type code {code}"
                );
            }
        }
    }

    #[test]
    fn a_unit_that_issues_one_kind_proves_only_that_one() {
        let standard_only = Issues {
            standard: true,
            simplified: false,
        };
        let documents = compliance_documents(
            &crate::taxpayer::tests::registration(),
            &unit(standard_only),
            at(),
        );
        assert_eq!(documents.len(), 3);
        assert!(documents.iter().all(|d| d.kind == Kind::Standard));
        assert_eq!(documents.len(), expected(standard_only));
    }

    /// A credit or debit note has to name what it is against.
    #[test]
    fn every_note_references_an_invoice() {
        let documents = compliance_documents(
            &crate::taxpayer::tests::registration(),
            &unit(Issues::both()),
            at(),
        );
        for document in &documents {
            match document.type_code {
                TypeCode::Invoice => assert!(document.reference.is_none()),
                TypeCode::CreditNote | TypeCode::DebitNote => {
                    assert!(
                        document.reference.is_some(),
                        "{} has nothing to credit",
                        document.number
                    );
                    assert!(
                        !document.note.is_empty(),
                        "{} gives no reason",
                        document.number
                    );
                }
            }
        }
    }

    /// The samples carry the taxpayer's own registration — a compliance check
    /// under somebody else's VAT number proves nothing about this solution.
    #[test]
    fn the_samples_are_issued_by_the_business_being_onboarded() {
        let registration = crate::taxpayer::tests::registration();
        let documents = compliance_documents(&registration, &unit(Issues::both()), at());
        assert!(
            documents
                .iter()
                .all(|d| d.seller.vat_number == registration.vat_number)
        );
        // And they are obviously not sales.
        assert!(
            documents
                .iter()
                .all(|d| d.number.starts_with("COMPLIANCE-"))
        );
    }

    /// **A standard document needs a buyer with a VAT number**, because that is
    /// what makes it standard — and a simplified one needs none.
    #[test]
    fn the_buyer_matches_the_kind_being_proved() {
        let documents = compliance_documents(
            &crate::taxpayer::tests::registration(),
            &unit(Issues::both()),
            at(),
        );
        for document in &documents {
            match document.kind {
                Kind::Standard => assert!(
                    document
                        .buyer
                        .as_ref()
                        .and_then(|b| b.vat_number.as_ref())
                        .is_some(),
                    "a standard sample with no buyer VAT number"
                ),
                Kind::Simplified => assert!(document.buyer.is_none()),
            }
            assert_eq!(
                document.kind,
                Kind::of(document.buyer.as_ref().and_then(|b| b.vat_number.as_ref()))
            );
        }
    }

    #[test]
    fn each_sample_has_its_own_uuid() {
        let documents = compliance_documents(
            &crate::taxpayer::tests::registration(),
            &unit(Issues::both()),
            at(),
        );
        let mut uuids: Vec<_> = documents.iter().map(|d| d.uuid).collect();
        uuids.sort_unstable();
        uuids.dedup();
        assert_eq!(uuids.len(), documents.len(), "two samples share a UUID");
    }
}
