//! The key pair and the certificate request — "certificate generation".
//!
//! # What is generated, and by whom
//!
//! Not the certificate. A taxpayer never issues their own: this generates an
//! **ECDSA key pair** and a **PKCS#10 certificate signing request**, ZATCA
//! signs the request, and the certificate comes back from
//! [`onboarding`](super::onboarding). The private key never leaves this process
//! and is never sent anywhere — that is the whole point of a CSR.
//!
//! # secp256k1, which is not the usual curve
//!
//! ZATCA specifies **secp256k1** — the Koblitz curve, the one Bitcoin uses —
//! not the secp256r1/P-256 that almost every other X.509 stack defaults to. A
//! CSR on the wrong curve is refused at onboarding, and the two are one
//! character apart in the name. [`generate`] names the curve once and
//! `the_curve_is_the_koblitz_one_and_not_the_usual_one` reads it back out of the
//! encoded key, so a change to the wrong one fails here rather than at ZATCA.
//!
//! # Two extensions, written as exact bytes
//!
//! ZATCA reads the EGS unit's identity out of two X.509 extensions, and both are
//! shapes OpenSSL's config-string builder cannot express through its Rust
//! binding. They are written as DER instead, which also makes them assertable:
//!
//! | OID | what |
//! |---|---|
//! | `1.3.6.1.4.1.311.20.2` | the certificate template name, which is **the environment** |
//! | `2.5.29.17` | `subjectAltName`, a `directoryName` holding the EGS identity |
//!
//! The template name is how ZATCA tells sandbox from production. Getting it
//! wrong does not fail loudly — it onboards you into the wrong environment,
//! which is why [`Environment`] is a required argument and not a default.

use openssl::asn1::{Asn1Object, Asn1OctetString};
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::stack::Stack;
use openssl::x509::{X509Extension, X509NameBuilder, X509ReqBuilder};

use base64::Engine as _;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Which ZATCA this is talking to.
///
/// A required argument everywhere it appears. The only visible difference is a
/// string in one extension and a base URL, and a mistake between them does not
/// fail — it succeeds against the wrong authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Environment {
    /// The developer portal. Nothing here is a real invoice.
    Sandbox,
    /// The simulation environment: production's behaviour, no legal effect.
    Simulation,
    /// The real one.
    Production,
}

impl Environment {
    pub const ALL: [Self; 3] = [Self::Sandbox, Self::Simulation, Self::Production];

    /// The certificate template name that goes in `1.3.6.1.4.1.311.20.2`.
    ///
    /// **Three different values, one per environment.** An earlier version had
    /// sandbox and simulation sharing one, which a working simulation CSR from
    /// another implementation disproved: it carries `PREZATCA-Code-Signing`.
    /// The sandbox does not check — it issues a certificate against any of the
    /// three — so the mistake would have surfaced at the first simulation
    /// onboarding and nowhere before it.
    #[must_use]
    pub const fn template(self) -> &'static str {
        match self {
            Self::Sandbox => "TSTZATCACA-Code-Signing",
            Self::Simulation => "PREZATCA-Code-Signing",
            Self::Production => "ZATCA-Code-Signing",
        }
    }

    /// Where the onboarding and invoicing calls go.
    ///
    /// ponytail: compiled in rather than configured. A tenant does not choose
    /// their tax authority's hostname, and a deployment pointing at a URL
    /// somebody typed is a worse failure than a redeploy.
    #[must_use]
    pub const fn base_url(self) -> &'static str {
        match self {
            Self::Sandbox => "https://gw-fatoora.zatca.gov.sa/e-invoicing/developer-portal",
            Self::Simulation => "https://gw-fatoora.zatca.gov.sa/e-invoicing/simulation",
            Self::Production => "https://gw-fatoora.zatca.gov.sa/e-invoicing/core",
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Simulation => "simulation",
            Self::Production => "production",
        }
    }
}

impl std::str::FromStr for Environment {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|e| e.as_str() == s)
            .ok_or_else(|| format!("unknown ZATCA environment {s:?}"))
    }
}

/// Which documents this unit issues, as the four-character flag ZATCA reads out
/// of the CSR's `title`.
///
/// It decides which compliance checks the unit has to pass before it gets a
/// production certificate, so it is not a description — it is a commitment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Issues {
    /// Standard invoices — the ones cleared before the buyer gets them.
    pub standard: bool,
    /// Simplified invoices — the ones reported within 24 hours.
    pub simplified: bool,
}

impl Issues {
    /// Both, which is what an ERP that sells to businesses and consumers does.
    #[must_use]
    pub const fn both() -> Self {
        Self {
            standard: true,
            simplified: true,
        }
    }

    /// `1100`, `1000`, `0100`. The last two positions are reserved and zero.
    #[must_use]
    pub fn title(self) -> String {
        format!("{}{}00", u8::from(self.standard), u8::from(self.simplified))
    }

    /// How many sample documents the compliance checks will demand: an invoice,
    /// a credit note and a debit note for each kind that is declared.
    #[must_use]
    pub const fn compliance_documents(self) -> usize {
        (self.standard as usize + self.simplified as usize) * 3
    }
}

/// Everything ZATCA wants to know about the unit issuing the invoices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// The taxpayer's 15-digit VAT registration number.
    pub vat_number: String,
    /// The legal name, as registered.
    pub organization: String,
    /// The branch. For a VAT group member, their own 10-digit TIN.
    pub branch: String,
    /// A name for this unit, unique among the taxpayer's units.
    pub common_name: String,
    /// This software's name, as registered with ZATCA.
    pub solution: String,
    /// Its version.
    pub version: String,
    /// This unit's serial number, unique per taxpayer.
    pub serial: String,
    /// Where the unit is, as free text — a national address rendered short.
    pub address: String,
    /// The taxpayer's industry.
    pub industry: String,
    pub issues: Issues,
}

impl Unit {
    /// `1-<solution>|2-<version>|3-<serial>`, which is the only format ZATCA
    /// accepts for the EGS serial number.
    #[must_use]
    pub fn egs_serial(&self) -> String {
        format!("1-{}|2-{}|3-{}", self.solution, self.version, self.serial)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CsrError {
    #[error("generating a key or a request failed: {0}")]
    OpenSsl(String),
    #[error("{field} cannot be empty")]
    Missing { field: &'static str },
    /// `|` separates the three parts of the EGS serial number, so it cannot
    /// appear inside one — and a value that quietly split it would produce a
    /// serial ZATCA reads as a different unit.
    #[error("{field} cannot contain a `|`")]
    Separator { field: &'static str },
}

impl From<openssl::error::ErrorStack> for CsrError {
    fn from(error: openssl::error::ErrorStack) -> Self {
        Self::OpenSsl(error.to_string())
    }
}

/// A generated key pair and the request that goes with it.
pub struct Generated {
    /// **The secret.** Sealed and stored; never sent anywhere, never logged.
    pub private_key_pem: Vec<u8>,
    /// The request, as PEM.
    pub csr_pem: Vec<u8>,
}

impl std::fmt::Debug for Generated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Generated")
            .field("private_key_pem", &"<withheld>")
            .field("csr_pem", &format_args!("{} bytes", self.csr_pem.len()))
            .finish()
    }
}

impl Generated {
    /// The CSR as the onboarding call carries it: base64 of the PEM.
    ///
    /// The **whole** PEM, `-----BEGIN CERTIFICATE REQUEST-----` and all, which
    /// is what ZATCA's own examples decode to. Worth confirming against the
    /// sandbox on the first onboarding of a deployment: some tooling base64s the
    /// DER body instead, and the two are indistinguishable until ZATCA answers.
    #[must_use]
    pub fn csr_for_zatca(&self) -> String {
        B64.encode(&self.csr_pem)
    }
}

/// Generates the key pair and the request for one EGS unit.
pub fn generate(unit: &Unit, environment: Environment) -> Result<Generated, CsrError> {
    check(unit)?;

    // **secp256k1.** Not the P-256 every other X.509 stack defaults to.
    let group = EcGroup::from_curve_name(Nid::SECP256K1)?;
    let key = EcKey::generate(&group)?;
    let private_key_pem = key.private_key_to_pem()?;
    let pkey = PKey::from_ec_key(key)?;

    let csr_pem = request(unit, environment, &pkey)?;

    Ok(Generated {
        private_key_pem,
        csr_pem,
    })
}

/// A request for a key that already exists — a renewal, which keeps the key and
/// asks for a new certificate over it.
pub fn renew(
    unit: &Unit,
    environment: Environment,
    private_key_pem: &[u8],
) -> Result<Generated, CsrError> {
    check(unit)?;
    let key = EcKey::private_key_from_pem(private_key_pem)?;
    let pkey = PKey::from_ec_key(key)?;
    Ok(Generated {
        private_key_pem: private_key_pem.to_vec(),
        csr_pem: request(unit, environment, &pkey)?,
    })
}

fn request(
    unit: &Unit,
    environment: Environment,
    pkey: &PKey<Private>,
) -> Result<Vec<u8>, CsrError> {
    let mut subject = X509NameBuilder::new()?;
    subject.append_entry_by_text("C", "SA")?;
    subject.append_entry_by_text("OU", &unit.branch)?;
    subject.append_entry_by_text("O", &unit.organization)?;
    subject.append_entry_by_text("CN", &unit.common_name)?;
    let subject = subject.build();

    // The EGS identity, which ZATCA reads out of the subject alternative name
    // rather than the subject.
    let mut identity = X509NameBuilder::new()?;
    identity.append_entry_by_text("SN", &unit.egs_serial())?;
    identity.append_entry_by_text("UID", &unit.vat_number)?;
    identity.append_entry_by_text("title", &unit.issues.title())?;
    identity.append_entry_by_text("registeredAddress", &unit.address)?;
    identity.append_entry_by_text("businessCategory", &unit.industry)?;
    let identity = identity.build();

    // GeneralNames ::= SEQUENCE OF GeneralName, and directoryName is `[4]`
    // EXPLICIT because Name is a CHOICE.
    let san = der(0x30, &der(0xA4, &identity.to_der()?));
    let san_value = Asn1OctetString::new_from_bytes(&san)?;
    let san_oid = Asn1Object::from_str("2.5.29.17")?;
    let san_ext = X509Extension::new_from_der(&san_oid, false, &san_value)?;

    // The template name, a UTF8String — and the only thing in the request that
    // says which ZATCA this is for.
    let template = der(0x0C, environment.template().as_bytes());
    let template_value = Asn1OctetString::new_from_bytes(&template)?;
    let template_oid = Asn1Object::from_str("1.3.6.1.4.1.311.20.2")?;
    let template_ext = X509Extension::new_from_der(&template_oid, false, &template_value)?;

    let mut extensions = Stack::new()?;
    extensions.push(template_ext)?;
    extensions.push(san_ext)?;

    let mut request = X509ReqBuilder::new()?;
    request.set_subject_name(&subject)?;
    request.set_pubkey(pkey)?;
    request.add_extensions(&extensions)?;
    request.sign(pkey, MessageDigest::sha256())?;

    Ok(request.build().to_pem()?)
}

fn check(unit: &Unit) -> Result<(), CsrError> {
    for (field, value) in [
        ("vat_number", &unit.vat_number),
        ("organization", &unit.organization),
        ("branch", &unit.branch),
        ("common_name", &unit.common_name),
        ("solution", &unit.solution),
        ("version", &unit.version),
        ("serial", &unit.serial),
        ("address", &unit.address),
        ("industry", &unit.industry),
    ] {
        if value.trim().is_empty() {
            return Err(CsrError::Missing { field });
        }
    }
    // The three fields the EGS serial number is built from.
    for (field, value) in [
        ("solution", &unit.solution),
        ("version", &unit.version),
        ("serial", &unit.serial),
    ] {
        if value.contains('|') {
            return Err(CsrError::Separator { field });
        }
    }
    Ok(())
}

/// `tag ‖ length ‖ value`, with the long form when it is needed.
///
/// A whole DER library for two extensions would be a dependency for sixty
/// bytes. What is encoded here is a tag, a length and some bytes somebody else
/// already encoded, and `a_length_is_encoded_the_way_der_says` covers the
/// boundary where the short form stops.
fn der(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = value.len();
    if len < 0x80 {
        // Short form: one byte, top bit clear.
        out.push(u8::try_from(len).unwrap_or(0x7F));
    } else if len < 0x100 {
        out.extend_from_slice(&[0x81, u8::try_from(len).unwrap_or(0xFF)]);
    } else {
        // Two length bytes reach 65535, and a directoryName that long is not a
        // unit's address — it is a mistake worth truncating loudly at.
        let len = u16::try_from(len).unwrap_or(u16::MAX);
        out.extend_from_slice(&[0x82, (len >> 8) as u8, (len & 0xFF) as u8]);
    }
    out.extend_from_slice(value);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use openssl::x509::X509Req;

    pub(crate) fn unit() -> Unit {
        Unit {
            vat_number: "310122393500003".to_owned(),
            organization: "روابي للاستشارات".to_owned(),
            branch: "الفرع الرئيسي".to_owned(),
            common_name: "EGS1-886431145".to_owned(),
            solution: "Erp".to_owned(),
            version: "1.0".to_owned(),
            serial: "886431145".to_owned(),
            address: "الرياض 12211".to_owned(),
            industry: "Consulting".to_owned(),
            issues: Issues::both(),
        }
    }

    fn parsed(environment: Environment) -> (X509Req, Generated) {
        let generated = generate(&unit(), environment).expect("generates");
        let request = X509Req::from_pem(&generated.csr_pem).expect("parses");
        (request, generated)
    }

    /// **The curve.** One character between the curve ZATCA specifies and the
    /// one every X.509 stack defaults to, and the wrong one is refused at
    /// onboarding rather than at compile time.
    #[test]
    fn the_curve_is_the_koblitz_one_and_not_the_usual_one() {
        let (request, _) = parsed(Environment::Sandbox);
        let public = request.public_key().expect("a public key");
        let ec = public.ec_key().expect("an EC key");
        assert_eq!(
            ec.group().curve_name(),
            Some(Nid::SECP256K1),
            "the CSR is not on secp256k1"
        );
        assert_ne!(ec.group().curve_name(), Some(Nid::X9_62_PRIME256V1));
    }

    /// A request that does not verify under its own key is one ZATCA rejects,
    /// and the signature is the part a hand-written extension could break.
    #[test]
    fn the_request_is_signed_by_the_key_it_carries() {
        let (request, _) = parsed(Environment::Sandbox);
        let public = request.public_key().expect("a public key");
        assert!(request.verify(&public).expect("verifies"));
    }

    #[test]
    fn the_subject_is_the_four_fields_zatca_reads() {
        let (request, _) = parsed(Environment::Production);
        let entries: Vec<String> = request
            .subject_name()
            .entries()
            .map(|e| {
                format!(
                    "{}={}",
                    e.object().nid().short_name().unwrap_or("?"),
                    e.data().to_string().unwrap_or_default()
                )
            })
            .collect();
        assert_eq!(
            entries,
            vec![
                "C=SA".to_owned(),
                "OU=الفرع الرئيسي".to_owned(),
                "O=روابي للاستشارات".to_owned(),
                "CN=EGS1-886431145".to_owned(),
            ]
        );
    }

    /// **The environment lives in the certificate template name**, and getting
    /// it wrong onboards into the wrong ZATCA rather than failing.
    /// Three environments, three template names, all different.
    #[test]
    fn no_two_environments_share_a_template_name() {
        let mut names: Vec<&str> = Environment::ALL.iter().map(|e| e.template()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Environment::ALL.len());
        // Pinned to the literals, because a wrong one onboards into the wrong
        // authority rather than failing.
        assert_eq!(Environment::Sandbox.template(), "TSTZATCACA-Code-Signing");
        assert_eq!(Environment::Simulation.template(), "PREZATCA-Code-Signing");
        assert_eq!(Environment::Production.template(), "ZATCA-Code-Signing");
    }

    #[test]
    fn the_template_name_is_what_says_which_zatca_this_is_for() {
        let (_, sandbox) = parsed(Environment::Sandbox);
        let sandbox = String::from_utf8_lossy(&sandbox.csr_pem).into_owned();
        let der = B64.decode(strip(&sandbox)).expect("the PEM body is base64");
        assert!(
            contains(&der, b"TSTZATCACA-Code-Signing"),
            "the sandbox template name is not in the request"
        );
        assert!(!contains(&der, b"\x0c\x12ZATCA-Code-Signing"));

        let (_, production) = parsed(Environment::Production);
        let production = String::from_utf8_lossy(&production.csr_pem).into_owned();
        let der = B64.decode(strip(&production)).expect("base64");
        assert!(contains(&der, b"ZATCA-Code-Signing"));
        assert!(!contains(&der, b"TSTZATCACA-Code-Signing"));
    }

    /// The EGS identity has to survive into the request, in the exact shape
    /// ZATCA parses — including the pipe-separated serial.
    #[test]
    fn the_egs_identity_is_in_the_subject_alternative_name() {
        let (_, generated) = parsed(Environment::Simulation);
        let pem = String::from_utf8_lossy(&generated.csr_pem).into_owned();
        let der = B64.decode(strip(&pem)).expect("base64");

        for expected in [
            "1-Erp|2-1.0|3-886431145", // SN
            "310122393500003",         // UID
            "1100",                    // title: both kinds
            "Consulting",              // businessCategory
        ] {
            assert!(
                contains(&der, expected.as_bytes()),
                "{expected} is not in the request"
            );
        }
        // And the OID for `UID`, which is the one an X.509 stack is least
        // likely to have got right: 0.9.2342.19200300.100.1.1.
        assert!(contains(&der, &[0x06, 0x0A, 0x09, 0x92, 0x26]));
    }

    #[test]
    fn the_declared_document_types_decide_the_compliance_checks() {
        assert_eq!(Issues::both().title(), "1100");
        assert_eq!(Issues::both().compliance_documents(), 6);

        let standard_only = Issues {
            standard: true,
            simplified: false,
        };
        assert_eq!(standard_only.title(), "1000");
        assert_eq!(standard_only.compliance_documents(), 3);

        let simplified_only = Issues {
            standard: false,
            simplified: true,
        };
        assert_eq!(simplified_only.title(), "0100");
        assert_eq!(simplified_only.compliance_documents(), 3);
    }

    /// A renewal keeps the key. A renewal that generated a new one would
    /// invalidate every signature the old certificate covers.
    #[test]
    fn a_renewal_keeps_the_key_and_asks_again() {
        let first = generate(&unit(), Environment::Production).expect("generates");
        let again =
            renew(&unit(), Environment::Production, &first.private_key_pem).expect("renews");

        assert_eq!(first.private_key_pem, again.private_key_pem);
        assert_ne!(
            first.csr_pem, again.csr_pem,
            "two requests over one key are signed separately, so the bytes differ"
        );

        // Both requests carry the same public key, which is what makes it a
        // renewal rather than a new unit.
        let public = |pem: &[u8]| {
            X509Req::from_pem(pem)
                .expect("parses")
                .public_key()
                .expect("a key")
                .public_key_to_pem()
                .expect("pem")
        };
        assert_eq!(public(&first.csr_pem), public(&again.csr_pem));
    }

    /// A `|` inside one of the three parts silently produces a serial ZATCA
    /// reads as a different unit.
    #[test]
    fn a_pipe_cannot_hide_inside_the_egs_serial() {
        let mut unit = unit();
        unit.serial = "886431145|3-other".to_owned();
        assert!(matches!(
            generate(&unit, Environment::Production),
            Err(CsrError::Separator { field: "serial" })
        ));

        let mut unit = tests::unit();
        unit.solution = "  ".to_owned();
        assert!(matches!(
            generate(&unit, Environment::Production),
            Err(CsrError::Missing { field: "solution" })
        ));
    }

    #[test]
    fn a_length_is_encoded_the_way_der_says() {
        assert_eq!(der(0x0C, b"hi"), vec![0x0C, 0x02, b'h', b'i']);

        // 127 is the last short form; 128 needs `0x81`.
        assert_eq!(der(0x04, &[0u8; 127])[..2], [0x04, 0x7F]);
        assert_eq!(der(0x04, &[0u8; 128])[..3], [0x04, 0x81, 0x80]);
        assert_eq!(der(0x04, &[0u8; 256])[..4], [0x04, 0x82, 0x01, 0x00]);

        // And the value is not touched.
        assert_eq!(&der(0x30, b"body")[2..], b"body");
    }

    #[test]
    fn every_environment_round_trips_and_points_somewhere_different() {
        for environment in Environment::ALL {
            assert_eq!(environment.as_str().parse::<Environment>(), Ok(environment));
            assert!(environment.base_url().starts_with("https://"));
        }
        assert_ne!(
            Environment::Simulation.base_url(),
            Environment::Production.base_url()
        );
        assert!("nonsense".parse::<Environment>().is_err());
    }

    fn strip(pem: &str) -> String {
        pem.lines()
            .filter(|line| !line.starts_with("-----"))
            .collect()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
