//! The QR code, which is a TLV block in base64.
//!
//! What a customer's phone reads off the bottom of a receipt, and what an
//! inspector scans in a shop. Not a URL: the data *is* the payload, so a phone
//! with no signal still verifies the seller, the tax, and — from phase 2 — the
//! cryptographic stamp.
//!
//! # The encoding
//!
//! Tag-length-value, concatenated, base64 at the end. Each field is one tag
//! byte, one length byte, then that many bytes of UTF-8:
//!
//! ```text
//!   01 10 "روابي للاستشارات"     seller name
//!   02 0F "310122393500003"      VAT number
//!   03 14 "2026-03-01T10:00:00Z" timestamp
//!   04 06 "115.00"               total, including VAT
//!   05 05 "15.00"                the VAT
//! ```
//!
//! Length is a **byte count**, not a character count. An Arabic name is two
//! bytes a letter, which is the mistake this module exists to not make.

use base64::Engine as _;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// One field's value was longer than a single length byte can describe.
///
/// Refused rather than truncated: a QR with half a seller's name in it scans
/// fine and is wrong, which is the worst of the three outcomes.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("the QR field {tag} is {len} bytes; the most that fits in a TLV length byte is 255")]
pub struct TooLong {
    pub tag: u8,
    pub len: usize,
}

/// What the QR says. Tags 1–5 are phase one and are what this build produces;
/// 6–9 carry the cryptographic stamp and arrive with the certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Qr<'a> {
    pub seller: &'a str,
    pub vat_number: &'a str,
    /// ISO 8601, `2026-03-01T10:00:00Z`.
    pub issued_at: &'a str,
    /// Including VAT, as it is printed: `115.00`.
    pub total: &'a str,
    /// The VAT alone: `15.00`.
    pub tax: &'a str,
    /// Tag 6 — the invoice hash. Present from phase two.
    pub invoice_hash: Option<&'a str>,
    /// Tag 7 — the ECDSA signature over the invoice.
    pub signature: Option<&'a str>,
    /// Tag 8 — the public key of the stamp.
    pub public_key: Option<&'a [u8]>,
    /// Tag 9 — ZATCA's signature over that public key. Standard invoices only.
    pub certificate_signature: Option<&'a [u8]>,
}

impl Qr<'_> {
    /// The base64 TLV a printer puts on the document.
    pub fn encode(&self) -> Result<String, TooLong> {
        let mut out: Vec<u8> = Vec::new();
        for (tag, value) in self.fields() {
            push(&mut out, tag, value)?;
        }
        Ok(B64.encode(out))
    }

    /// Every tag that has a value, in tag order — which is the order ZATCA
    /// specifies and not an implementation detail.
    fn fields(&self) -> Vec<(u8, &[u8])> {
        let mut fields: Vec<(u8, &[u8])> = vec![
            (1, self.seller.as_bytes()),
            (2, self.vat_number.as_bytes()),
            (3, self.issued_at.as_bytes()),
            (4, self.total.as_bytes()),
            (5, self.tax.as_bytes()),
        ];
        if let Some(hash) = self.invoice_hash {
            fields.push((6, hash.as_bytes()));
        }
        if let Some(signature) = self.signature {
            fields.push((7, signature.as_bytes()));
        }
        if let Some(key) = self.public_key {
            fields.push((8, key));
        }
        if let Some(signature) = self.certificate_signature {
            fields.push((9, signature));
        }
        fields
    }
}

fn push(out: &mut Vec<u8>, tag: u8, value: &[u8]) -> Result<(), TooLong> {
    let len = u8::try_from(value.len()).map_err(|_| TooLong {
        tag,
        len: value.len(),
    })?;
    out.push(tag);
    out.push(len);
    out.extend_from_slice(value);
    Ok(())
}

/// Reads a TLV block back. For tests, and for anyone debugging a receipt.
///
/// # Why it is here and not in a test module
///
/// Because a QR nobody can read back is a QR nobody can check. The decoder is
/// what proves the encoder puts the bytes where it says it does, and it is
/// twenty lines.
pub fn decode(encoded: &str) -> Result<Vec<(u8, Vec<u8>)>, String> {
    let bytes = B64.decode(encoded).map_err(|e| e.to_string())?;
    let mut fields = Vec::new();
    let mut rest = bytes.as_slice();
    while !rest.is_empty() {
        let [tag, len, tail @ ..] = rest else {
            return Err(format!("{} trailing byte(s) with no length", rest.len()));
        };
        let len = usize::from(*len);
        if tail.len() < len {
            return Err(format!(
                "tag {tag} says {len} bytes and only {} are left",
                tail.len()
            ));
        }
        fields.push((*tag, tail[..len].to_vec()));
        rest = &tail[len..];
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qr() -> Qr<'static> {
        Qr {
            seller: "روابي للاستشارات",
            vat_number: "310122393500003",
            issued_at: "2026-03-01T10:00:00Z",
            total: "115.00",
            tax: "15.00",
            invoice_hash: None,
            signature: None,
            public_key: None,
            certificate_signature: None,
        }
    }

    fn text(fields: &[(u8, Vec<u8>)], tag: u8) -> String {
        let value = fields
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        String::from_utf8(value).unwrap_or_default()
    }

    #[test]
    fn a_qr_round_trips_through_its_own_encoding() {
        let encoded = qr().encode().expect("short enough");
        let fields = decode(&encoded).expect("well formed");

        assert_eq!(fields.len(), 5);
        assert_eq!(text(&fields, 1), "روابي للاستشارات");
        assert_eq!(text(&fields, 2), "310122393500003");
        assert_eq!(text(&fields, 3), "2026-03-01T10:00:00Z");
        assert_eq!(text(&fields, 4), "115.00");
        assert_eq!(text(&fields, 5), "15.00");
    }

    /// The mistake this module exists to not make: an Arabic name is two bytes
    /// a letter, and a length in characters puts every later tag one field out.
    #[test]
    fn the_length_is_bytes_and_not_characters() {
        let encoded = qr().encode().expect("short enough");
        let raw = B64.decode(&encoded).expect("base64");

        assert_eq!(raw[0], 1, "the first tag");
        assert_eq!(
            usize::from(raw[1]),
            "روابي للاستشارات".len(),
            "the length byte is the UTF-8 byte count"
        );
        assert_ne!(
            usize::from(raw[1]),
            "روابي للاستشارات".chars().count(),
            "and the test would pass by accident if they were the same"
        );

        // The tag after it starts exactly where that length says it does.
        let after = 2 + usize::from(raw[1]);
        assert_eq!(raw[after], 2, "the VAT number's tag");
    }

    #[test]
    fn the_stamp_tags_appear_only_when_there_is_a_stamp() {
        let mut qr = qr();
        assert_eq!(decode(&qr.encode().expect("ok")).expect("ok").len(), 5);

        qr.invoice_hash = Some("qBKmm6aFilTO/bL8OILnzrfWaqHteSViCChy3W7U+SE=");
        qr.signature = Some("MEQCIA==");
        let fields = decode(&qr.encode().expect("ok")).expect("ok");
        assert_eq!(fields.len(), 7);
        assert_eq!(
            text(&fields, 6),
            "qBKmm6aFilTO/bL8OILnzrfWaqHteSViCChy3W7U+SE="
        );
        assert_eq!(text(&fields, 7), "MEQCIA==");
    }

    /// Truncating would scan fine and be wrong, which is worse than not scanning.
    #[test]
    fn a_field_too_long_for_its_length_byte_is_refused() {
        let long = "ا".repeat(200); // 400 bytes.
        let qr = Qr {
            seller: &long,
            ..qr()
        };
        assert_eq!(qr.encode(), Err(TooLong { tag: 1, len: 400 }));
    }

    #[test]
    fn a_truncated_block_is_a_decode_error_and_not_a_short_read() {
        let encoded = qr().encode().expect("ok");
        let mut raw = B64.decode(&encoded).expect("base64");
        raw.truncate(raw.len() - 3);
        assert!(decode(&B64.encode(raw)).is_err());
    }
}
