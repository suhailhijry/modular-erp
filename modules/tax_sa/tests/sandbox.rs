//! **Against ZATCA itself**, with a real certificate.
//!
//! Ignored by default: it needs a compliance CSID and the private key it was
//! issued against, which no CI job has and no test should invent. Point
//! `ZATCA_CREDENTIALS` at a directory holding
//!
//! ```text
//!   key.pem     the private key, PEM
//!   cert.pem    the certificate ZATCA issued, PEM
//!   csid.json   {"request_id": …, "certificate": …, "secret": …}
//! ```
//!
//! and run:
//!
//! ```sh
//! ZATCA_CREDENTIALS=/path/to/credentials \
//!   cargo test -p tax_sa --test sandbox -- --ignored --nocapture
//! ```
//!
//! # Why this exists
//!
//! Three things in `zatca::signing` are reconstructed from ZATCA's
//! specification and deviate from the standards they are built on — the
//! certificate digest, the signature encoding, and the whitespace inside
//! `xades:SignedProperties`. No unit test can settle them, because the only
//! authority on what ZATCA accepts is ZATCA. This is the test that asks.
//!
//! It sends **one document, built by the ordinary renderer and signed by the
//! ordinary signer**. Anything special-cased for the sake of passing here would
//! defeat the point.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::print_stdout)]

use base64::Engine as _;
use ledger::VatCategory;
use spa_types::{CurrencyCode, Money, Timestamp};
use tax_sa::zatca::csr::Environment;
use tax_sa::zatca::onboarding::{Csid, Registrar};
use tax_sa::zatca::{Band, Buyer, Document, Kind, Line, Link, Totals, TypeCode, document_uuid};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// The taxpayer the credentials belong to. A document under any other VAT
/// number is one the certificate does not cover.
const VAT_NUMBER: &str = "300649169100003";

struct Credentials {
    key: Vec<u8>,
    certificate: openssl::x509::X509,
    csid: Csid,
}

/// The credentials, or `None` when this run has none — which is the normal case
/// and is why every test here is `#[ignore]`.
fn credentials() -> Option<Credentials> {
    let directory = std::env::var("ZATCA_CREDENTIALS").ok()?;
    let directory = std::path::Path::new(&directory);

    let key = std::fs::read(directory.join("key.pem")).expect("key.pem is readable");
    let certificate = openssl::x509::X509::from_pem(
        &std::fs::read(directory.join("cert.pem")).expect("cert.pem is readable"),
    )
    .expect("cert.pem is a certificate");

    let stored: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.join("csid.json")).expect("csid.json is readable"),
    )
    .expect("csid.json is JSON");

    // **The username ZATCA expects is the token as it returned it**, which is
    // base64 of the certificate's base64 — one more layer than the certificate
    // itself. A store that decoded it once has to put that layer back.
    let certificate_base64 = stored["certificate"]
        .as_str()
        .expect("a certificate")
        .trim();
    let token = B64.encode(certificate_base64);

    Some(Credentials {
        key,
        certificate,
        csid: Csid {
            token,
            secret: stored["secret"].as_str().expect("a secret").to_owned(),
            request_id: stored["request_id"].as_str().unwrap_or_default().to_owned(),
        },
    })
}

fn sar() -> CurrencyCode {
    CurrencyCode::new("SAR").expect("valid")
}

fn registration() -> tax_sa::Registration {
    tax_sa::Registration {
        vat_number: VAT_NUMBER.to_owned(),
        name: "مركز أقدامي الرياضي".to_owned(),
        name_latin: Some("Aqdamy Sports Center".to_owned()),
        scheme: tax_sa::taxpayer::IdScheme::Crn,
        identifier: "1010101010".to_owned(),
        address: tax_sa::taxpayer::Address {
            street: "طريق الملك فهد".to_owned(),
            building: "1422".to_owned(),
            additional: Some("6000".to_owned()),
            district: "العليا".to_owned(),
            city: "الرياض".to_owned(),
            postal_code: "12211".to_owned(),
            country: "SA".to_owned(),
        },
    }
}

/// One ordinary document, built the way every tenant's is.
fn document(kind: Kind, type_code: TypeCode, link: Link) -> Document {
    let currency = sar();
    let net = Money::from_minor(10_000, currency);
    let tax = Money::from_minor(1_500, currency);
    let number = format!("SANDBOX-{}-1", type_code.code());

    Document {
        kind,
        type_code,
        uuid: document_uuid(VAT_NUMBER, &number),
        number,
        issued_at: "2026-08-17T10:00:00Z".parse::<Timestamp>().expect("valid"),
        currency,
        seller: registration(),
        buyer: match kind {
            Kind::Standard => Some(Buyer {
                name: "شركة الاختبار".to_owned(),
                vat_number: Some("399999999900003".to_owned()),
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
            description: "اشتراك شهري".to_owned(),
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
            gross: Money::from_minor(11_500, currency),
            bands: vec![Band {
                category: VatCategory::Standard,
                rate_bp: 1_500,
                net,
                tax,
            }],
        },
        link,
        reference: None,
        note: String::new(),
    }
}

/// The same document with a discount taken off the whole of it.
fn discounted(mut document: Document) -> Document {
    let currency = sar();
    document.number = format!("{}-DISC", document.number);
    document.uuid = document_uuid(VAT_NUMBER, &document.number);
    document.allowances = vec![tax_sa::zatca::Allowance {
        reason: "خصم ترويجي".to_owned(),
        amount: Money::from_minor(1_500, currency),
        category: VatCategory::Standard,
        rate_bp: 1_500,
    }];
    // 100.00 of lines, 15.00 off, 85.00 taxed at 15% = 12.75.
    document.totals = tax_sa::zatca::Totals {
        net: Money::from_minor(8_500, currency),
        tax: Money::from_minor(1_275, currency),
        gross: Money::from_minor(9_775, currency),
        before_discount: Some(Money::from_minor(10_000, currency)),
        bands: vec![Band {
            category: VatCategory::Standard,
            rate_bp: 1_500,
            net: Money::from_minor(8_500, currency),
            tax: Money::from_minor(1_275, currency),
        }],
    };
    document
}

/// Renders, hashes, signs and submits one document. Prints what ZATCA said.
async fn ask(kind: Kind, type_code: TypeCode) -> Option<tax_sa::zatca::wire::Verdict> {
    ask_for(document(kind, type_code, Link::first())).await
}

async fn ask_for(document: Document) -> Option<tax_sa::zatca::wire::Verdict> {
    let credentials = credentials()?;
    let signer = tax_sa::zatca::signing::Signer::new(&credentials.key, &credentials.certificate)
        .expect("the key and the certificate go together");

    let kind = document.kind;
    let type_code = document.type_code;
    let canonical = tax_sa::zatca::ubl::render(&document).expect("renders");
    let hash = tax_sa::zatca::chain::invoice_hash(&canonical);

    let at = "2026-08-17T10:00:00Z".parse::<Timestamp>().expect("valid");
    let signature = signer.sign(&canonical, &hash, at).expect("signs");
    let qr = signature
        .qr(
            &document.seller.name,
            &document.seller.vat_number,
            // Through the same formatter every other call site uses — a literal
            // here would be testing the literal.
            &document
                .issued_at
                .format(tax_sa::zatca::QR_TIME)
                .to_string(),
            &tax_sa::zatca::amount(document.totals.gross),
            &tax_sa::zatca::amount(document.totals.tax),
            &hash,
        )
        .expect("encodes");

    let submitted = tax_sa::zatca::ubl::signed(
        &document,
        &tax_sa::zatca::ubl::Enveloped {
            extensions: &signature.extensions,
            qr: &qr,
        },
    )
    .expect("renders");
    println!(
        "hashed {} bytes, submitting {} bytes",
        canonical.len(),
        submitted.len()
    );

    // Kept for a person to look at when ZATCA says no.
    if let Ok(directory) = std::env::var("ZATCA_CREDENTIALS") {
        let _ = std::fs::write(
            std::path::Path::new(&directory).join(format!("submitted-{}.xml", document.number)),
            tax_sa::zatca::ubl::with_declaration(&submitted),
        );
    }

    let client = tax_sa::zatca::http::Fatoora::new(Environment::Sandbox).expect("a client");
    let verdict = client
        .check_compliance(
            Environment::Sandbox,
            &credentials.csid,
            &tax_sa::zatca::wire::Submission {
                invoice_hash: hash.clone(),
                uuid: document.uuid.to_string(),
                invoice: B64.encode(tax_sa::zatca::ubl::with_declaration(&submitted)),
            },
        )
        .await;

    println!("\n=== {kind:?} {type_code:?} ===");
    println!("hash {hash}");
    match &verdict {
        Ok(tax_sa::zatca::wire::Verdict::Accepted { warnings, .. }) => {
            println!("ACCEPTED with {} warning(s)", warnings.len());
            for warning in warnings {
                println!(
                    "  warning {} [{}] {}",
                    warning.code, warning.category, warning.message
                );
            }
        }
        Ok(tax_sa::zatca::wire::Verdict::Refused { errors }) => {
            println!("REFUSED with {} error(s)", errors.len());
            for error in errors {
                println!("  {} [{}] {}", error.code, error.category, error.message);
            }
        }
        Err(unanswered) => println!("NO ANSWER: {unanswered}"),
    }
    verdict.ok()
}

/// **The question this file exists to ask.** A simplified invoice is the one a
/// customer scans, and the one whose QR carries the whole stamp.
#[tokio::test]
#[ignore = "needs a real ZATCA compliance certificate; see the module docs"]
async fn zatca_accepts_a_simplified_invoice_this_build_signed() {
    let Some(verdict) = ask(Kind::Simplified, TypeCode::Invoice).await else {
        println!("ZATCA_CREDENTIALS is not set; nothing was asked");
        return;
    };
    assert!(
        matches!(verdict, tax_sa::zatca::wire::Verdict::Accepted { .. }),
        "ZATCA refused a document this build signed"
    );
}

#[tokio::test]
#[ignore = "needs a real ZATCA compliance certificate; see the module docs"]
async fn zatca_accepts_a_standard_invoice_this_build_signed() {
    let Some(verdict) = ask(Kind::Standard, TypeCode::Invoice).await else {
        println!("ZATCA_CREDENTIALS is not set; nothing was asked");
        return;
    };
    assert!(
        matches!(verdict, tax_sa::zatca::wire::Verdict::Accepted { .. }),
        "ZATCA refused a document this build signed"
    );
}

/// The credentials themselves, checked before anything is blamed on the
/// document: the private key has to be the certificate's.
#[tokio::test]
#[ignore = "needs a real ZATCA compliance certificate; see the module docs"]
async fn the_key_and_the_certificate_belong_together() {
    let Some(credentials) = credentials() else {
        println!("ZATCA_CREDENTIALS is not set; nothing was checked");
        return;
    };

    let ours = openssl::ec::EcKey::private_key_from_pem(&credentials.key)
        .and_then(openssl::pkey::PKey::from_ec_key)
        .expect("a private key");
    let theirs = credentials.certificate.public_key().expect("a public key");
    assert!(
        theirs.public_eq(&ours),
        "the private key is not the one this certificate was issued for"
    );

    println!(
        "certificate {} issued by {}",
        tax_sa::zatca::signing::serial_number(&credentials.certificate),
        tax_sa::zatca::signing::issuer_name(&credentials.certificate)
    );
}

/// **Step 3 of onboarding, against ZATCA.** One sample of every document type
/// the unit declared — six for a unit that issues both kinds — built and signed
/// by the ordinary code, chained among themselves, submitted one at a time.
///
/// This is what stands between a compliance certificate and a production one,
/// so a failure here is a business that cannot go live.
#[tokio::test]
#[ignore = "needs a real ZATCA compliance certificate; see the module docs"]
async fn zatca_accepts_every_compliance_document_this_build_generates() {
    let Some(credentials) = credentials() else {
        println!("ZATCA_CREDENTIALS is not set; nothing was asked");
        return;
    };
    let signer = tax_sa::zatca::signing::Signer::new(&credentials.key, &credentials.certificate)
        .expect("the key and the certificate go together");

    let unit = tax_sa::zatca::csr::Unit {
        vat_number: VAT_NUMBER.to_owned(),
        organization: "مركز أقدامي الرياضي".to_owned(),
        branch: "الفرع الرئيسي".to_owned(),
        common_name: "EGS1-DEV002".to_owned(),
        solution: "Aqdamy System".to_owned(),
        version: "Model1".to_owned(),
        serial: "DEV002".to_owned(),
        address: "الرياض 12211".to_owned(),
        industry: "Sports".to_owned(),
        issues: tax_sa::zatca::csr::Issues::both(),
    };
    let at = "2026-08-17T10:00:00Z".parse::<Timestamp>().expect("valid");

    let submissions =
        tax_sa::zatca::onboarding::compliance_submissions(&registration(), &unit, &signer, at)
            .expect("builds and signs");
    assert_eq!(submissions.len(), 6, "both kinds, three documents each");

    let client = tax_sa::zatca::http::Fatoora::new(Environment::Sandbox).expect("a client");
    let mut passed = 0;
    let mut failures = Vec::new();

    for (number, submission) in &submissions {
        // Kept for a person to look at when ZATCA says no.
        if let Ok(directory) = std::env::var("ZATCA_CREDENTIALS")
            && let Ok(xml) = B64.decode(&submission.invoice)
        {
            let _ = std::fs::write(
                std::path::Path::new(&directory).join(format!("compliance-{number}.xml")),
                xml,
            );
        }

        let verdict = client
            .check_compliance(Environment::Sandbox, &credentials.csid, submission)
            .await;

        match verdict {
            Ok(tax_sa::zatca::wire::Verdict::Accepted { warnings, .. }) => {
                passed += 1;
                println!("{number:<22} ACCEPTED, {} warning(s)", warnings.len());
                for warning in warnings {
                    println!("    warning {} {}", warning.code, warning.message);
                }
            }
            Ok(tax_sa::zatca::wire::Verdict::Refused { errors }) => {
                println!("{number:<22} REFUSED, {} error(s)", errors.len());
                for error in &errors {
                    println!(
                        "    error   {} [{}] {}",
                        error.code, error.category, error.message
                    );
                }
                failures.push((number.clone(), errors));
            }
            Err(unanswered) => panic!("{number}: {unanswered}"),
        }
    }

    assert_eq!(
        passed,
        submissions.len(),
        "ZATCA refused {} of {} compliance documents: {:?}",
        failures.len(),
        submissions.len(),
        failures.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
}

/// **A discounted invoice, against ZATCA.** The three monetary totals it
/// produces have to agree with each other and with the tax, and ZATCA checks
/// that they do.
#[tokio::test]
#[ignore = "needs a real ZATCA compliance certificate; see the module docs"]
async fn zatca_accepts_an_invoice_with_a_document_level_discount() {
    let Some(verdict) = ask_for(discounted(document(
        Kind::Simplified,
        TypeCode::Invoice,
        Link::first(),
    )))
    .await
    else {
        println!("ZATCA_CREDENTIALS is not set; nothing was asked");
        return;
    };
    assert!(
        matches!(verdict, tax_sa::zatca::wire::Verdict::Accepted { .. }),
        "ZATCA refused a discounted invoice this build signed"
    );
}
