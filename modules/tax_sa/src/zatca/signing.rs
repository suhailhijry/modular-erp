//! The `XAdES` signature ZATCA requires on every invoice.
//!
//! # What is signed, and what is not
//!
//! ```text
//!   the invoice as rendered           ──►  SHA-256  ──►  invoice digest
//!     (no UBLExtensions, no Signature,                         │
//!      no QR reference — zatca::ubl)                           │
//!                                                              ▼
//!   xades:SignedProperties            ──►  SHA-256  ──►  properties digest
//!     (signing time, certificate)                             │
//!                                                              ▼
//!                                      ds:SignedInfo, holding both
//!                                                              │
//!                                              ECDSA-SHA256 ───┘
//!                                                              │
//!                                                              ▼
//!                                                     ds:SignatureValue
//! ```
//!
//! The invoice digest is the one [`chain`](super::chain) already computes and
//! puts in the chain, because ZATCA hashes the document with exactly those three
//! things removed — and this build never puts them in the bytes it hashes.
//!
//! # Why signing is not done in the projection
//!
//! **ECDSA is randomised.** Every signature over the same bytes with the same
//! key is different, because a fresh `k` goes into each one. A projection that
//! signed would produce different tables on every rebuild, which is the one
//! thing a projection may not do (L2) — and the difference would be in the
//! column a tax authority holds a copy of.
//!
//! It also needs the private key, and a projection that could read
//! `module_secret` would be a projection that could leak it.
//!
//! So signing happens once, outside, and the result is **recorded as an event**
//! — `tax_sa.zatca.signed`. The projection applies that event, which makes the
//! stored signature a replay of something that happened rather than something
//! recomputed. Same argument as recording what ZATCA said.
//!
//! # Three deviations from the standards, all confirmed against ZATCA
//!
//! Each is one named function, and each was a guess until a real certificate
//! settled it — `modules/tax_sa/tests/sandbox.rs` submits a document built by
//! this code and ZATCA accepts it with no warnings:
//!
//! 1. [`certificate_digest`] — ZATCA hashes the certificate's **base64 text**
//!    rather than its DER, which is not what `XAdES` says.
//! 2. [`Signer::signature_value`] — the ECDSA signature goes in as **DER**,
//!    where XML-DSig specifies the raw `r ‖ s` pair.
//! 3. The exact whitespace inside `xades:SignedProperties`, which is inside the
//!    digest and so cannot be normalised away.

use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::sign::Signer as OpenSslSigner;
use openssl::x509::X509;

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("the stored key or certificate could not be read: {0}")]
    Material(String),
    #[error("signing failed: {0}")]
    OpenSsl(String),
    #[error(transparent)]
    Qr(#[from] super::qr::TooLong),
}

impl From<openssl::error::ErrorStack> for SigningError {
    fn from(error: openssl::error::ErrorStack) -> Self {
        Self::OpenSsl(error.to_string())
    }
}

/// A certificate and the key that goes with it.
///
/// Built once per sweep and used for every document in it — parsing a PEM per
/// invoice would be the most expensive thing in the loop.
pub struct Signer {
    certificate: X509,
    key: PKey<Private>,
    /// The certificate's base64, without the PEM armour. It goes in `KeyInfo`
    /// verbatim, and it is what [`certificate_digest`] hashes.
    certificate_base64: String,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field(
                "certificate",
                &format_args!("{} chars", self.certificate_base64.len()),
            )
            .field("key", &"<withheld>")
            .finish()
    }
}

impl Signer {
    /// From the stored private key and the certificate inside a CSID token.
    pub fn new(private_key_pem: &[u8], certificate: &X509) -> Result<Self, SigningError> {
        let key = openssl::ec::EcKey::private_key_from_pem(private_key_pem)
            .and_then(PKey::from_ec_key)
            .map_err(|e| SigningError::Material(e.to_string()))?;

        let der = certificate
            .to_der()
            .map_err(|e| SigningError::Material(e.to_string()))?;

        Ok(Self {
            certificate: certificate.clone(),
            key,
            certificate_base64: B64.encode(der),
        })
    }

    /// Signs one invoice.
    ///
    /// `canonical` is what [`ubl::render`](super::ubl::render) produced and what
    /// `invoice_hash` was taken over — the two must be the same bytes, or the
    /// digest in the signature is not the digest of the document.
    pub fn sign(
        &self,
        canonical: &str,
        invoice_hash: &str,
        signed_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<Signature, SigningError> {
        debug_assert_eq!(
            invoice_hash,
            super::chain::invoice_hash(canonical),
            "the hash and the bytes disagree"
        );

        let properties = signed_properties(
            &self.certificate_base64,
            &issuer_name(&self.certificate),
            &serial_number(&self.certificate),
            signed_at,
        );
        let properties_digest = digest(&properties);

        let signed_info = signed_info(invoice_hash, &properties_digest);
        let value = self.signature_value(&signed_info)?;

        Ok(Signature {
            extensions: ubl_extensions(&signed_info, &value, &self.certificate_base64, &properties),
            value,
            properties_digest,
            public_key: self.public_key_der()?,
            certificate_signature: self.certificate.signature().as_slice().to_vec(),
            signed_at,
        })
    }

    /// ECDSA-SHA256 over the canonical `ds:SignedInfo`, base64.
    ///
    /// OpenSSL's DER encoding — an ASN.1 `SEQUENCE` of `r` and `s`. XML-DSig
    /// specifies the raw `r ‖ s` pair instead, and **ZATCA wants DER**:
    /// a document signed this way is accepted with no warnings, which is the
    /// only authority there is on the question.
    fn signature_value(&self, signed_info: &str) -> Result<String, SigningError> {
        let mut signer = OpenSslSigner::new(MessageDigest::sha256(), &self.key)?;
        signer.update(signed_info.as_bytes())?;
        Ok(B64.encode(signer.sign_to_vec()?))
    }

    /// The `SubjectPublicKeyInfo`, for QR tag 8.
    fn public_key_der(&self) -> Result<Vec<u8>, SigningError> {
        Ok(self.certificate.public_key()?.public_key_to_der()?)
    }
}

/// Everything one signature produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// The whole `ext:UBLExtensions` block, ready to go at the top of the
    /// document.
    pub extensions: String,
    /// `ds:SignatureValue`, base64. QR tag 7.
    pub value: String,
    /// The digest of the signed properties, kept so a reader can check the
    /// chain of digests without recomputing the signature.
    pub properties_digest: String,
    /// The `SubjectPublicKeyInfo`. QR tag 8.
    pub public_key: Vec<u8>,
    /// ZATCA's signature over the certificate. QR tag 9, and **the reason a
    /// customer's phone can tell a real stamp from an invented one**.
    pub certificate_signature: Vec<u8>,
    pub signed_at: chrono::DateTime<chrono::Utc>,
}

impl Signature {
    /// The phase-two QR: the five plain fields, plus the stamp.
    pub fn qr(
        &self,
        seller: &str,
        vat_number: &str,
        issued_at: &str,
        total: &str,
        tax: &str,
        invoice_hash: &str,
    ) -> Result<String, SigningError> {
        Ok(super::qr::Qr {
            seller,
            vat_number,
            issued_at,
            total,
            tax,
            invoice_hash: Some(invoice_hash),
            signature: Some(&self.value),
            public_key: Some(&self.public_key),
            // Simplified invoices are the ones a customer scans, and the ones
            // ZATCA requires this on.
            certificate_signature: Some(&self.certificate_signature),
        }
        .encode()?)
    }
}

// ---------------------------------------------------------------------------
// The pieces
// ---------------------------------------------------------------------------

/// SHA-256, base64. The digest every part of this uses.
#[must_use]
pub fn digest(text: &str) -> String {
    B64.encode(Sha256::digest(text.as_bytes()))
}

/// The digest of the signing certificate, for `xades:CertDigest`.
///
/// `XAdES` says this is the digest of the certificate's DER. ZATCA hashes the
/// certificate's **base64 text** — the characters, not the bytes they encode —
/// and that is what this does, because a signature ZATCA cannot verify is worth
/// less than one that deviates from the standard in the same direction ZATCA
/// does. **Confirmed**: a document signed this way is accepted.
///
/// It is the same class of quirk as the genesis PIH in
/// [`chain::genesis`](super::chain::genesis), which is also base64 over text.
#[must_use]
pub fn certificate_digest(certificate_base64: &str) -> String {
    B64.encode(Sha256::digest(certificate_base64.as_bytes()))
}

/// The issuer's distinguished name, as `XAdES` wants it: RFC 2253 order, which is
/// the reverse of how X.509 stores it.
#[must_use]
pub fn issuer_name(certificate: &X509) -> String {
    let mut parts: Vec<String> = certificate
        .issuer_name()
        .entries()
        .filter_map(|entry| {
            let name = entry.object().nid().short_name().ok()?;
            let value = entry.data().to_string().ok()?;
            Some(format!("{name}={value}"))
        })
        .collect();
    parts.reverse();
    parts.join(", ")
}

/// The certificate's serial number, in decimal — which is what
/// `ds:X509SerialNumber` is, however it is printed elsewhere.
#[must_use]
pub fn serial_number(certificate: &X509) -> String {
    certificate
        .serial_number()
        .to_bn()
        .and_then(|bn| bn.to_dec_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// `xades:SignedProperties`, canonical.
///
/// **The whitespace here is inside the digest.** `signed_properties_digest`
/// covers exactly these bytes, so an editor reflowing this function changes
/// every signature it produces — which is why it is one string and not a
/// builder.
#[must_use]
pub fn signed_properties(
    certificate_base64: &str,
    issuer: &str,
    serial: &str,
    signed_at: chrono::DateTime<chrono::Utc>,
) -> String {
    format!(
        "<xades:SignedProperties xmlns:xades=\"http://uri.etsi.org/01903/v1.3.2#\" \
Id=\"xadesSignedProperties\">\n\
\x20                                   <xades:SignedSignatureProperties>\n\
\x20                                       <xades:SigningTime>{time}</xades:SigningTime>\n\
\x20                                       <xades:SigningCertificate>\n\
\x20                                           <xades:Cert>\n\
\x20                                               <xades:CertDigest>\n\
\x20                                                   <ds:DigestMethod xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\" Algorithm=\"http://www.w3.org/2001/04/xmlenc#sha256\"/>\n\
\x20                                                   <ds:DigestValue xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\">{certificate}</ds:DigestValue>\n\
\x20                                               </xades:CertDigest>\n\
\x20                                               <xades:IssuerSerial>\n\
\x20                                                   <ds:X509IssuerName xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\">{issuer}</ds:X509IssuerName>\n\
\x20                                                   <ds:X509SerialNumber xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\">{serial}</ds:X509SerialNumber>\n\
\x20                                               </xades:IssuerSerial>\n\
\x20                                           </xades:Cert>\n\
\x20                                       </xades:SigningCertificate>\n\
\x20                                   </xades:SignedSignatureProperties>\n\
\x20                               </xades:SignedProperties>",
        time = signed_at.format("%Y-%m-%dT%H:%M:%SZ"),
        certificate = certificate_digest(certificate_base64),
    )
}

/// `ds:SignedInfo`, canonical. What the ECDSA signature is over.
///
/// The three `XPath` transforms are the ones that remove the extensions, the
/// signature and the QR reference — the same three things this build never
/// renders into the hashed document in the first place. They are stated anyway
/// because ZATCA's verifier applies them to what it receives.
#[must_use]
pub fn signed_info(invoice: &str, properties: &str) -> String {
    format!(
        "<ds:SignedInfo xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\">\n\
\x20   <ds:CanonicalizationMethod Algorithm=\"http://www.w3.org/2006/12/xml-c14n11\"/>\n\
\x20   <ds:SignatureMethod Algorithm=\"http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha256\"/>\n\
\x20   <ds:Reference Id=\"invoiceSignedData\" URI=\"\">\n\
\x20       <ds:Transforms>\n\
\x20           <ds:Transform Algorithm=\"http://www.w3.org/TR/1999/REC-xpath-19991116\">\n\
\x20               <ds:XPath>not(//ancestor-or-self::ext:UBLExtensions)</ds:XPath>\n\
\x20           </ds:Transform>\n\
\x20           <ds:Transform Algorithm=\"http://www.w3.org/TR/1999/REC-xpath-19991116\">\n\
\x20               <ds:XPath>not(//ancestor-or-self::cac:Signature)</ds:XPath>\n\
\x20           </ds:Transform>\n\
\x20           <ds:Transform Algorithm=\"http://www.w3.org/TR/1999/REC-xpath-19991116\">\n\
\x20               <ds:XPath>not(//ancestor-or-self::cac:AdditionalDocumentReference[cbc:ID='QR'])</ds:XPath>\n\
\x20           </ds:Transform>\n\
\x20           <ds:Transform Algorithm=\"http://www.w3.org/2006/12/xml-c14n11\"/>\n\
\x20       </ds:Transforms>\n\
\x20       <ds:DigestMethod Algorithm=\"http://www.w3.org/2001/04/xmlenc#sha256\"/>\n\
\x20       <ds:DigestValue>{invoice}</ds:DigestValue>\n\
\x20   </ds:Reference>\n\
\x20   <ds:Reference Type=\"http://www.w3.org/2000/09/xmldsig#SignatureProperties\" URI=\"#xadesSignedProperties\">\n\
\x20       <ds:DigestMethod Algorithm=\"http://www.w3.org/2001/04/xmlenc#sha256\"/>\n\
\x20       <ds:DigestValue>{properties}</ds:DigestValue>\n\
\x20   </ds:Reference>\n\
</ds:SignedInfo>",
    )
}

/// The whole `ext:UBLExtensions` block that goes at the top of the document.
#[must_use]
pub fn ubl_extensions(
    signed_info: &str,
    value: &str,
    certificate: &str,
    properties: &str,
) -> String {
    format!(
        "  <ext:UBLExtensions>\n\
\x20   <ext:UBLExtension>\n\
\x20     <ext:ExtensionURI>urn:oasis:names:specification:ubl:dsig:enveloped:xades</ext:ExtensionURI>\n\
\x20     <ext:ExtensionContent>\n\
\x20       <sig:UBLDocumentSignatures xmlns:sig=\"urn:oasis:names:specification:ubl:schema:xsd:CommonSignatureComponents-2\" xmlns:sac=\"urn:oasis:names:specification:ubl:schema:xsd:SignatureAggregateComponents-2\" xmlns:sbc=\"urn:oasis:names:specification:ubl:schema:xsd:SignatureBasicComponents-2\">\n\
\x20         <sac:SignatureInformation>\n\
\x20           <cbc:ID>urn:oasis:names:specification:ubl:signature:1</cbc:ID>\n\
\x20           <sbc:ReferencedSignatureID>urn:oasis:names:specification:ubl:signature:Invoice</sbc:ReferencedSignatureID>\n\
\x20           <ds:Signature xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\" Id=\"signature\">\n\
{signed_info}\n\
\x20             <ds:SignatureValue>{value}</ds:SignatureValue>\n\
\x20             <ds:KeyInfo>\n\
\x20               <ds:X509Data>\n\
\x20                 <ds:X509Certificate>{certificate}</ds:X509Certificate>\n\
\x20               </ds:X509Data>\n\
\x20             </ds:KeyInfo>\n\
\x20             <ds:Object>\n\
\x20               <xades:QualifyingProperties xmlns:xades=\"http://uri.etsi.org/01903/v1.3.2#\" Target=\"signature\">\n\
{properties}\n\
\x20               </xades:QualifyingProperties>\n\
\x20             </ds:Object>\n\
\x20           </ds:Signature>\n\
\x20         </sac:SignatureInformation>\n\
\x20       </sig:UBLDocumentSignatures>\n\
\x20     </ext:ExtensionContent>\n\
\x20   </ext:UBLExtension>\n\
\x20 </ext:UBLExtensions>\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-signed certificate and its key, standing in for a CSID.
    fn material() -> (Vec<u8>, X509) {
        let group =
            openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::SECP256K1).expect("secp256k1");
        let key = openssl::ec::EcKey::generate(&group).expect("a key");
        let pem = key.private_key_to_pem().expect("pem");
        let pkey = PKey::from_ec_key(key).expect("a key");

        let mut name = openssl::x509::X509NameBuilder::new().expect("a builder");
        name.append_entry_by_text("C", "SA").expect("c");
        name.append_entry_by_text("O", "ZATCA").expect("o");
        name.append_entry_by_text("CN", "TSZEINVOICE-SubCA-1")
            .expect("cn");
        let name = name.build();

        let mut certificate = openssl::x509::X509::builder().expect("a builder");
        certificate.set_version(2).expect("v3");
        certificate.set_subject_name(&name).expect("subject");
        certificate.set_issuer_name(&name).expect("issuer");
        certificate.set_pubkey(&pkey).expect("public key");
        let start = openssl::asn1::Asn1Time::days_from_now(0).expect("now");
        let end = openssl::asn1::Asn1Time::days_from_now(1826).expect("five years");
        certificate.set_not_before(&start).expect("not before");
        certificate.set_not_after(&end).expect("not after");
        let serial = openssl::bn::BigNum::from_u32(0x0BAD_CAFE).expect("a number");
        let serial = openssl::asn1::Asn1Integer::from_bn(&serial).expect("a serial");
        certificate.set_serial_number(&serial).expect("serial");
        certificate
            .sign(&pkey, MessageDigest::sha256())
            .expect("signs");

        (pem, certificate.build())
    }

    fn at() -> chrono::DateTime<chrono::Utc> {
        "2026-03-01T10:00:00Z".parse().expect("a valid instant")
    }

    #[test]
    fn a_signature_verifies_under_the_certificate_it_names() {
        let (pem, certificate) = material();
        let signer = Signer::new(&pem, &certificate).expect("a signer");

        let canonical = "<Invoice></Invoice>";
        let hash = super::super::chain::invoice_hash(canonical);
        let signature = signer.sign(canonical, &hash, at()).expect("signs");

        // The signature is over the SignedInfo, which is in the extensions.
        let signed_info = signed_info(&hash, &signature.properties_digest);
        let public = certificate.public_key().expect("a key");
        let mut verifier =
            openssl::sign::Verifier::new(MessageDigest::sha256(), &public).expect("a verifier");
        verifier.update(signed_info.as_bytes()).expect("update");
        assert!(
            verifier
                .verify(&B64.decode(&signature.value).expect("base64"))
                .expect("verifies"),
            "the signature does not verify under its own certificate"
        );
    }

    /// **ECDSA is randomised**, which is the whole reason signing cannot happen
    /// in a projection.
    #[test]
    fn signing_the_same_bytes_twice_gives_two_different_signatures() {
        let (pem, certificate) = material();
        let signer = Signer::new(&pem, &certificate).expect("a signer");
        let canonical = "<Invoice></Invoice>";
        let hash = super::super::chain::invoice_hash(canonical);

        let once = signer.sign(canonical, &hash, at()).expect("signs");
        let again = signer.sign(canonical, &hash, at()).expect("signs");

        assert_ne!(
            once.value, again.value,
            "two ECDSA signatures over the same bytes were identical"
        );
        // Everything else about them is the same, which is what makes the
        // signature the only part that has to be recorded.
        assert_eq!(once.properties_digest, again.properties_digest);
        assert_eq!(once.public_key, again.public_key);
    }

    #[test]
    fn the_signed_info_carries_both_digests_and_the_invoice_hash_is_one_of_them() {
        let hash = super::super::chain::invoice_hash("<Invoice></Invoice>");
        let properties = digest("<props/>");
        let info = signed_info(&hash, &properties);

        assert!(info.contains(&format!("<ds:DigestValue>{hash}</ds:DigestValue>")));
        assert!(info.contains(&format!("<ds:DigestValue>{properties}</ds:DigestValue>")));
        // The reference to the properties is by id, and the id has to match the
        // one the properties element carries.
        assert!(info.contains("URI=\"#xadesSignedProperties\""));
        assert!(
            signed_properties("Yw==", "CN=x", "1", at()).contains("Id=\"xadesSignedProperties\"")
        );
        // ECDSA over SHA-256, and C14N 1.1 — not 2.0, and not exclusive.
        assert!(info.contains("ecdsa-sha256"));
        assert!(info.contains("http://www.w3.org/2006/12/xml-c14n11"));
    }

    /// The three transforms name the three things this build never renders into
    /// the hashed document.
    #[test]
    fn the_transforms_remove_exactly_what_the_hash_leaves_out() {
        let info = signed_info("aGFzaA==", "cHJvcHM=");
        for removed in [
            "not(//ancestor-or-self::ext:UBLExtensions)",
            "not(//ancestor-or-self::cac:Signature)",
            "not(//ancestor-or-self::cac:AdditionalDocumentReference[cbc:ID='QR'])",
        ] {
            assert!(info.contains(removed), "{removed} is not a transform");
        }
    }

    #[test]
    fn the_signed_properties_hold_the_certificate_they_were_signed_with() {
        let (_, certificate) = material();
        let der = certificate.to_der().expect("der");
        let base64 = B64.encode(der);

        let properties = signed_properties(
            &base64,
            &issuer_name(&certificate),
            &serial_number(&certificate),
            at(),
        );

        assert!(properties.contains("<xades:SigningTime>2026-03-01T10:00:00Z</xades:SigningTime>"));
        assert!(properties.contains(&certificate_digest(&base64)));
        // The serial in decimal, not the hex a person reads.
        // Each `ds:` element carries its own namespace declaration, because
        // C14N puts one on every element that uses a prefix its ancestors did
        // not declare — and these are inside a digest, so it matters.
        assert!(
            properties.contains(&format!(
                ">{}</ds:X509SerialNumber>",
                serial_number(&certificate)
            )),
            "{properties}"
        );
        assert!(
            properties
                .contains("<ds:X509SerialNumber xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\">")
        );
        // Decimal, not the hex a person reads off a support ticket.
        assert_eq!(serial_number(&certificate), "195939070");
        assert!(properties.contains("CN=TSZEINVOICE-SubCA-1"));
    }

    /// **UNCONFIRMED, and pinned so a change to it is deliberate.** ZATCA hashes
    /// the certificate's base64 *text*, not its DER.
    #[test]
    fn the_certificate_digest_is_over_the_base64_text_and_not_the_der() {
        let (_, certificate) = material();
        let der = certificate.to_der().expect("der");
        let base64 = B64.encode(&der);

        assert_eq!(
            certificate_digest(&base64),
            B64.encode(Sha256::digest(base64.as_bytes()))
        );
        assert_ne!(
            certificate_digest(&base64),
            B64.encode(Sha256::digest(&der)),
            "hashing the DER is what `XAdES` says and not what ZATCA does"
        );
    }

    /// RFC 2253 order, which is the reverse of how X.509 stores it.
    #[test]
    fn the_issuer_name_is_in_the_order_xades_reads_it() {
        let (_, certificate) = material();
        assert_eq!(
            issuer_name(&certificate),
            "CN=TSZEINVOICE-SubCA-1, O=ZATCA, C=SA"
        );
    }

    #[test]
    fn the_extensions_carry_the_signature_the_certificate_and_the_properties() {
        let (pem, certificate) = material();
        let signer = Signer::new(&pem, &certificate).expect("a signer");
        let canonical = "<Invoice></Invoice>";
        let hash = super::super::chain::invoice_hash(canonical);
        let signature = signer.sign(canonical, &hash, at()).expect("signs");

        let extensions = &signature.extensions;
        assert!(extensions.starts_with("  <ext:UBLExtensions>"));
        assert!(extensions.contains("urn:oasis:names:specification:ubl:dsig:enveloped:xades"));
        assert!(extensions.contains(&format!(
            "<ds:SignatureValue>{}</ds:SignatureValue>",
            signature.value
        )));
        assert!(extensions.contains("<ds:X509Certificate>"));
        assert!(extensions.contains("<xades:QualifyingProperties"));
        assert!(extensions.contains("Target=\"signature\""));
        assert!(extensions.trim_end().ends_with("</ext:UBLExtensions>"));
        // The signed properties are inside, byte for byte as they were hashed.
        assert!(extensions.contains(&signed_properties(
            &B64.encode(certificate.to_der().expect("der")),
            &issuer_name(&certificate),
            &serial_number(&certificate),
            at()
        )));
    }

    /// The QR gains the stamp, and tag 9 is what lets a phone tell a real one
    /// from an invented one.
    #[test]
    fn a_signed_qr_carries_the_hash_the_signature_and_the_stamp() {
        let (pem, certificate) = material();
        let signer = Signer::new(&pem, &certificate).expect("a signer");
        let canonical = "<Invoice></Invoice>";
        let hash = super::super::chain::invoice_hash(canonical);
        let signature = signer.sign(canonical, &hash, at()).expect("signs");

        let encoded = signature
            .qr(
                "روابي للاستشارات",
                "310122393500003",
                "2026-03-01T10:00:00Z",
                "115.00",
                "15.00",
                &hash,
            )
            .expect("encodes");

        let fields = super::super::qr::decode(&encoded).expect("well formed");
        assert_eq!(fields.len(), 9, "a signed QR carries all nine tags");
        let value = |tag: u8| {
            fields
                .iter()
                .find(|(t, _)| *t == tag)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(String::from_utf8(value(6)).expect("utf-8"), hash);
        assert_eq!(String::from_utf8(value(7)).expect("utf-8"), signature.value);
        assert_eq!(value(8), signature.public_key);
        assert_eq!(value(9), signature.certificate_signature);
        assert!(
            !value(9).is_empty(),
            "tag 9 is the certificate's own signature"
        );
    }
}
