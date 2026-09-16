//! What a customer holds: the printed document, and a link to it.
//!
//! # What is printed
//!
//! Decided 2026-09-16. Print-ready HTML with the QR as inline SVG and nothing
//! fetched from anywhere, so a till prints it straight from the browser and a
//! phone renders it from a link. An 80 mm receipt for a simplified invoice, an
//! A4 page for a standard one and for a credit note. Bilingual always — Arabic
//! first, English beside it — because Arabic is the invoice's language and the
//! English is what a foreign buyer reads.
//!
//! # When it may be printed
//!
//! A **simplified** invoice's QR carries the stamp (tags 6 to 9 are the hash,
//! the signature, the public key and ZATCA's signature over the certificate),
//! so nothing prints before the document is signed. A **standard** invoice is
//! not a valid invoice until ZATCA has cleared it, so nothing prints before the
//! clearance — and what prints then is the document ZATCA stamped and returned,
//! its QR included, not the bytes we sent. [`deliverable`] is that rule, in one
//! place, and the routes wait on it.
//!
//! # The public link
//!
//! A customer is not a member. The link is the document's number and an HMAC
//! of it under a secret the tenant keeps (`tax_sa.link_secret`, made on first
//! use): unguessable, stateless, and verified in constant time. The document's
//! own UUID would not do — it is a v5 of the VAT number and the number, both of
//! which are printed on the invoice.

use std::fmt::Write as _;

use base64::Engine as _;
use erp_types::Money;

use crate::documents::{Status, Stored};
use crate::zatca::{Document, Kind, TypeCode};

/// Why a document cannot be handed over yet, or at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotDeliverable {
    /// Issued before the business registered; it has no chain position and
    /// no QR, and never will.
    Unregistered,
    /// Its QR does not carry the stamp yet. Wait: the worker signs on the
    /// visit the sale asked for.
    NotYetSigned,
    /// A standard invoice ZATCA has not cleared yet. Wait: the worker submits
    /// it on the same visit.
    AwaitingClearance,
    /// ZATCA refused it. A corrected document is a new document.
    Refused,
}

/// What the customer gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deliverable {
    /// The QR that goes on the print: ours with the stamp on a simplified
    /// invoice, ZATCA's on a cleared standard one.
    pub qr: String,
    /// The document for a buyer's system: the stamped one when there is one,
    /// the signed one otherwise.
    pub xml: String,
}

/// **The one rule** for whether a document may be handed over.
///
/// # Errors
/// [`NotDeliverable`], saying why not.
pub fn deliverable(stored: &Stored) -> Result<Deliverable, NotDeliverable> {
    match stored.status {
        Status::Unregistered => return Err(NotDeliverable::Unregistered),
        Status::Refused => return Err(NotDeliverable::Refused),
        Status::Pending | Status::Cleared | Status::Reported => {}
    }
    match stored.kind {
        Kind::Standard => {
            if stored.status != Status::Cleared {
                return Err(NotDeliverable::AwaitingClearance);
            }
            let stamped = stored
                .stamped_xml
                .as_deref()
                .and_then(decoded)
                .ok_or(NotDeliverable::AwaitingClearance)?;
            // ZATCA's QR when the stamped document carries one, ours otherwise.
            let qr = qr_in(&stamped)
                .or_else(|| stored.qr.clone())
                .ok_or(NotDeliverable::AwaitingClearance)?;
            Ok(Deliverable { qr, xml: stamped })
        }
        Kind::Simplified => {
            let xml = stored
                .signed_xml
                .clone()
                .ok_or(NotDeliverable::NotYetSigned)?;
            let qr = stored
                .qr
                .clone()
                .filter(|_| stored.signature.is_some())
                .ok_or(NotDeliverable::NotYetSigned)?;
            Ok(Deliverable { qr, xml })
        }
    }
}

/// The stamped document as ZATCA returned it is base64; the text inside.
fn decoded(base64_xml: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_xml.trim())
        .ok()?;
    String::from_utf8(bytes).ok()
}

/// The QR a UBL document carries: the `EmbeddedDocumentBinaryObject` of the
/// `AdditionalDocumentReference` whose `ID` is `QR`.
///
/// A string search rather than an XML parser: the shape is fixed by the
/// standard, the two tags are unambiguous, and a parser would be a dependency
/// for one lookup.
#[must_use]
pub fn qr_in(xml: &str) -> Option<String> {
    let at = xml.find("<cbc:ID>QR</cbc:ID>")?;
    let rest = &xml[at..];
    let open = rest.find("<cbc:EmbeddedDocumentBinaryObject")?;
    let rest = &rest[open..];
    let start = rest.find('>')? + 1;
    let end = rest.find("</cbc:EmbeddedDocumentBinaryObject>")?;
    let value = rest.get(start..end)?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

// ---------------------------------------------------------------------------
// The public link
// ---------------------------------------------------------------------------

/// The tenant's link secret. Made once, on the first link asked for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LinkSecret {
    /// 32 bytes, hex.
    pub secret: String,
}

impl LinkSecret {
    /// Where it is kept.
    pub const KEY: &'static str = "tax_sa.link_secret";

    /// The secret, made if there is none. Two first callers at once both try
    /// to make it; the store keeps one and the other reads it back.
    ///
    /// # Errors
    /// The store, or the system's random source.
    pub async fn resolve_or_create(
        conn: &mut sqlx::PgConnection,
    ) -> Result<Self, erp_eventlog::ConfigError> {
        if let Some(configured) = erp_eventlog::configuration::get::<Self>(conn, Self::KEY).await? {
            return Ok(configured.value);
        }
        let mut bytes = [0u8; 32];
        openssl::rand::rand_bytes(&mut bytes).map_err(|e| erp_eventlog::ConfigError::Invalid {
            key: Self::KEY.to_owned(),
            reason: e.to_string(),
        })?;
        let fresh = Self {
            secret: hex::encode(bytes),
        };
        match erp_eventlog::configuration::set(
            conn,
            Self::KEY,
            &fresh,
            Some("module:tax_sa"),
            Some(0),
        )
        .await
        {
            Ok(_) => Ok(fresh),
            Err(erp_eventlog::ConfigError::Conflict { .. }) => {
                erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
                    .await?
                    .map(|configured| configured.value)
                    .ok_or_else(|| erp_eventlog::ConfigError::Invalid {
                        key: Self::KEY.to_owned(),
                        reason: "the secret was set and is gone".to_owned(),
                    })
            }
            Err(e) => Err(e),
        }
    }

    /// The token for a document: `INV-00001.<32 hex of HMAC-SHA256>`.
    #[must_use]
    pub fn token(&self, number: &str) -> String {
        format!("{number}.{}", self.mac(number))
    }

    /// The document a token opens, or `None` for a token that is not one of
    /// this tenant's. Compared in constant time.
    #[must_use]
    pub fn opens(&self, token: &str) -> Option<String> {
        let (number, mac) = token.rsplit_once('.')?;
        if number.is_empty() || mac.len() != 32 {
            return None;
        }
        let expected = self.mac(number);
        openssl::memcmp::eq(expected.as_bytes(), mac.as_bytes()).then(|| number.to_owned())
    }

    fn mac(&self, number: &str) -> String {
        let key = hex::decode(&self.secret).unwrap_or_default();
        let mac = openssl::pkey::PKey::hmac(&key)
            .and_then(|key| {
                let mut signer =
                    openssl::sign::Signer::new(openssl::hash::MessageDigest::sha256(), &key)?;
                signer.update(number.as_bytes())?;
                signer.sign_to_vec()
            })
            .unwrap_or_default();
        hex::encode(mac).chars().take(32).collect()
    }
}

// ---------------------------------------------------------------------------
// The print
// ---------------------------------------------------------------------------

/// The document as a page: a receipt for a simplified invoice, an A4 page for
/// the rest. Nothing is fetched; the QR is inline SVG.
#[must_use]
pub fn html(document: &Document, qr: &str) -> String {
    let receipt = document.kind == Kind::Simplified;
    let (title_ar, title_en) = title(document);
    let mut out = String::with_capacity(8_192);
    let _ = writeln!(
        out,
        "<!doctype html>\n<html lang=\"ar\" dir=\"rtl\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{} — {}</title>\n<style>{}{}</style>\n</head>\n<body class=\"{}\">",
        esc(&document.number),
        esc(title_en),
        COMMON_CSS,
        if receipt { RECEIPT_CSS } else { INVOICE_CSS },
        if receipt { "receipt" } else { "invoice" }
    );
    seller(&mut out, document, title_ar, title_en);
    facts(&mut out, document);
    lines(&mut out, document);
    totals(&mut out, document);
    if !document.note.is_empty() {
        let _ = writeln!(out, "<p class=\"note\">{}</p>", esc(&document.note));
    }
    // The QR, which is the whole reason a phone can check this.
    if let Some(svg) = qr_svg(qr) {
        let _ = writeln!(out, "<figure class=\"qr\">{svg}</figure>");
    }
    let _ = writeln!(
        out,
        "<footer>{}</footer>\n</body>\n</html>",
        both("شكراً لتعاملكم معنا", "Thank you")
    );
    out
}

/// The seller, and the title.
fn seller(out: &mut String, document: &Document, title_ar: &str, title_en: &str) {
    let seller = &document.seller;
    let _ = write!(out, "<header>\n<h1>{}", esc(&seller.name));
    if let Some(latin) = &seller.name_latin {
        let _ = write!(out, "<span class=\"en\">{}</span>", esc(latin));
    }
    let _ = writeln!(
        out,
        "</h1>\n<p class=\"vat\">{} {}</p>\n<p class=\"address\">{}</p>\n<h2>{}</h2>\n</header>",
        both("الرقم الضريبي", "VAT no."),
        esc(&seller.vat_number),
        esc(&address(
            &seller.address.street,
            Some(&seller.address.building),
            seller.address.district.as_str(),
            &seller.address.city,
            Some(&seller.address.postal_code),
            &seller.address.country,
        )),
        both(title_ar, title_en)
    );
}

/// The document's own facts, and the buyer's.
fn facts(out: &mut String, document: &Document) {
    let _ = writeln!(
        out,
        "<section class=\"facts\">\n<dl>\n<dt>{}</dt><dd>{}</dd>\n<dt>{}</dt><dd>{}</dd>",
        both("رقم الفاتورة", "Invoice no."),
        esc(&document.number),
        both("التاريخ والوقت", "Date and time"),
        // On the business's clock, which is what the printed time means.
        document.calendar.clock(document.issued_at)
    );
    if let Some(reference) = &document.reference {
        let _ = writeln!(
            out,
            "<dt>{}</dt><dd>{} ({})</dd>",
            both("مرجع الفاتورة الأصلية", "Original invoice"),
            esc(&reference.number),
            document.calendar.day(reference.issued_at)
        );
    }
    if let Some(buyer) = &document.buyer {
        let _ = writeln!(
            out,
            "<dt>{}</dt><dd>{}</dd>",
            both("العميل", "Customer"),
            esc(&buyer.name)
        );
        if let Some(vat) = &buyer.vat_number {
            let _ = writeln!(
                out,
                "<dt>{}</dt><dd>{}</dd>",
                both("الرقم الضريبي للعميل", "Customer VAT no."),
                esc(vat)
            );
        }
        if let Some(at) = &buyer.address {
            let _ = writeln!(
                out,
                "<dt>{}</dt><dd>{}</dd>",
                both("عنوان العميل", "Customer address"),
                esc(&address(
                    &at.street,
                    at.building.as_deref(),
                    at.district.as_deref().unwrap_or_default(),
                    &at.city,
                    at.postal_code.as_deref(),
                    &at.country,
                ))
            );
        }
    }
    out.push_str("</dl>\n</section>\n");
}

/// The lines.
fn lines(out: &mut String, document: &Document) {
    let _ = writeln!(
        out,
        "<table class=\"lines\">\n<thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th>\
         <th>{}</th><th>{}</th><th>{}</th></tr></thead>\n<tbody>",
        both("البيان", "Description"),
        both("الكمية", "Qty"),
        both("سعر الوحدة", "Unit price"),
        both("المبلغ", "Net"),
        both("الضريبة %", "VAT %"),
        both("الضريبة", "VAT"),
        both("الإجمالي", "Total")
    );
    for line in &document.lines {
        let _ = writeln!(
            out,
            "<tr><td>{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td>\
             <td class=\"n\">{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td></tr>",
            esc(&line.description),
            line.units(),
            line.price().map(amount).unwrap_or_default(),
            amount(line.net),
            percent(line.rate_bp),
            amount(line.tax),
            line.gross().map(amount).unwrap_or_default()
        );
    }
    out.push_str("</tbody>\n</table>\n");
}

/// The totals.
fn totals(out: &mut String, document: &Document) {
    let totals = &document.totals;
    out.push_str("<section class=\"totals\">\n<dl>\n");
    if let Some(before) = totals.before_discount {
        let _ = writeln!(
            out,
            "<dt>{}</dt><dd>{}</dd>\n<dt>{}</dt><dd>{}</dd>",
            both("الإجمالي قبل الخصم", "Before discount"),
            amount(before),
            both("الخصم", "Discount"),
            amount(totals.discount())
        );
    }
    let _ = writeln!(
        out,
        "<dt>{}</dt><dd>{}</dd>",
        both("الإجمالي الخاضع للضريبة", "Total excluding VAT"),
        amount(totals.net)
    );
    for band in &totals.bands {
        let _ = writeln!(
            out,
            "<dt>{} {}</dt><dd>{}</dd>",
            both("ضريبة القيمة المضافة", "VAT"),
            percent(band.rate_bp),
            amount(band.tax)
        );
    }
    let _ = writeln!(
        out,
        "<dt class=\"grand\">{}</dt><dd class=\"grand\">{} {}</dd>",
        both("الإجمالي شامل الضريبة", "Total including VAT"),
        amount(totals.gross),
        document.currency
    );
    if let Some(prepaid) = &document.prepaid {
        let _ = writeln!(
            out,
            "<dt>{}</dt><dd>{} ({})</dd>",
            both("مدفوع مقدماً", "Prepaid"),
            amount(prepaid.gross(document.currency)),
            esc(&prepaid.number)
        );
    }
    out.push_str("</dl>\n</section>\n");
}

/// The QR as inline SVG, or nothing for a payload the encoder refuses — a
/// print with no QR rather than no print, and the document view still carries
/// the text.
pub(crate) fn qr_svg(qr: &str) -> Option<String> {
    let code = qrcode::QrCode::new(qr.as_bytes()).ok()?;
    Some(
        code.render::<qrcode::render::svg::Color<'_>>()
            .min_dimensions(180, 180)
            .quiet_zone(true)
            .build(),
    )
}

pub(crate) fn title(document: &Document) -> (&'static str, &'static str) {
    match (document.type_code, document.kind) {
        (TypeCode::CreditNote, _) => ("إشعار دائن", "Credit note"),
        (TypeCode::DebitNote, _) => ("إشعار مدين", "Debit note"),
        (TypeCode::Prepayment, Kind::Standard) => {
            ("فاتورة ضريبية لدفعة مقدمة", "Prepayment tax invoice")
        }
        (TypeCode::Prepayment, Kind::Simplified) => (
            "فاتورة ضريبية مبسطة لدفعة مقدمة",
            "Simplified prepayment tax invoice",
        ),
        (TypeCode::Invoice, Kind::Standard) => ("فاتورة ضريبية", "Tax invoice"),
        (TypeCode::Invoice, Kind::Simplified) => ("فاتورة ضريبية مبسطة", "Simplified tax invoice"),
    }
}

/// A label in both languages, Arabic first.
fn both(ar: &str, en: &str) -> String {
    format!(
        "<span class=\"ar\">{}</span><span class=\"en\">{}</span>",
        esc(ar),
        esc(en)
    )
}

/// One line of address, the parts that are there.
pub(crate) fn address(
    street: &str,
    building: Option<&str>,
    district: &str,
    city: &str,
    postal_code: Option<&str>,
    country: &str,
) -> String {
    [
        Some(street),
        building.filter(|b| !b.is_empty()),
        Some(district).filter(|d| !d.is_empty()),
        Some(city),
        postal_code.filter(|p| !p.is_empty()),
        Some(country),
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("، ")
}

/// The number without its currency: `115.00`, not `115.00 SAR`.
pub(crate) fn amount(money: Money) -> String {
    let text = money.to_string();
    text.rsplit_once(' ')
        .map_or(text.clone(), |(number, _)| number.to_owned())
}

/// `1500` basis points as `15%`, `250` as `2.5%`.
pub(crate) fn percent(rate_bp: i32) -> String {
    let (whole, hundredths) = (rate_bp / 100, rate_bp % 100);
    if hundredths == 0 {
        format!("{whole}%")
    } else {
        format!("{whole}.{hundredths:02}%")
    }
}

fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const COMMON_CSS: &str = "\
body{font-family:'Noto Naskh Arabic','Segoe UI',Tahoma,sans-serif;color:#111;margin:0;padding:0}\
.en{display:block;font-size:.8em;color:#555;direction:ltr}\
header h1{margin:0;font-size:1.3em}header h1 .en{display:inline;margin-inline-start:.5em}\
header .vat,header .address{margin:.2em 0;font-size:.9em}header h2{margin:.6em 0 0;font-size:1.1em}\
dl{display:grid;grid-template-columns:auto 1fr;gap:.2em .8em;margin:.6em 0}dt{font-weight:600}dd{margin:0}\
table.lines{width:100%;border-collapse:collapse;margin:.6em 0;font-size:.9em}\
table.lines th,table.lines td{border-bottom:1px solid #ccc;padding:.3em;text-align:start;vertical-align:top}\
td.n{text-align:end;direction:ltr;font-variant-numeric:tabular-nums;white-space:nowrap}\
.totals dl{justify-content:end}.totals dd{text-align:end;direction:ltr;font-variant-numeric:tabular-nums}\
.grand{font-size:1.15em;font-weight:700}.note{white-space:pre-wrap;font-size:.9em}\
figure.qr{margin:.8em auto;text-align:center}figure.qr svg{max-width:180px;height:auto}\
footer{text-align:center;margin-top:1em;font-size:.9em}\
@media print{body{-webkit-print-color-adjust:exact;print-color-adjust:exact}}";

const RECEIPT_CSS: &str = "\
@page{size:80mm auto;margin:3mm}body{width:74mm;margin:0 auto;padding:2mm;font-size:12px}\
header{text-align:center}header .en,header h1 .en{display:block;margin:0}\
table.lines th:nth-child(3),table.lines td:nth-child(3){display:none}";

const INVOICE_CSS: &str = "\
@page{size:A4;margin:15mm}body{max-width:180mm;margin:0 auto;padding:10mm;font-size:13px}\
header{display:grid;grid-template-columns:1fr auto;gap:1em}header h2{grid-column:1/-1}";

#[cfg(test)]
mod tests {
    use super::*;
    use erp_types::CurrencyCode;

    fn stored(kind: Kind, status: Status) -> Stored {
        let sar = CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!());
        Stored {
            number: "INV-00001".to_owned(),
            source: "inv-1".to_owned(),
            kind,
            type_code: 388,
            issued_at: erp_types::Timestamp::UNIX_EPOCH,
            currency: sar,
            net: Money::from_minor(100, sar),
            tax: Money::from_minor(15, sar),
            gross: Money::from_minor(115, sar),
            icv: Some(1),
            previous_hash: None,
            invoice_hash: Some("hash".to_owned()),
            xml: Some("<Invoice/>".to_owned()),
            qr: Some("ours".to_owned()),
            signature: None,
            signed_xml: None,
            signed_at: None,
            status,
            stamped_xml: None,
            remarks: Vec::new(),
            settled_at: None,
            document: None,
        }
    }

    /// **The rule, state by state.** Nothing before the signature; a standard
    /// invoice nothing before the clearance, and then ZATCA's document.
    #[test]
    fn nothing_is_handed_over_before_the_stamp_or_the_clearance() {
        let mut receipt = stored(Kind::Simplified, Status::Pending);
        assert_eq!(deliverable(&receipt), Err(NotDeliverable::NotYetSigned));
        receipt.signature = Some("sig".to_owned());
        receipt.signed_xml = Some("<Invoice>signed</Invoice>".to_owned());
        let handed = deliverable(&receipt).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(handed.qr, "ours");
        assert_eq!(handed.xml, "<Invoice>signed</Invoice>");

        let mut invoice = stored(Kind::Standard, Status::Pending);
        invoice.signature = Some("sig".to_owned());
        invoice.signed_xml = Some("<Invoice>signed</Invoice>".to_owned());
        assert_eq!(
            deliverable(&invoice),
            Err(NotDeliverable::AwaitingClearance),
            "signed is not cleared"
        );
        invoice.status = Status::Cleared;
        let stamped = "<Invoice><cac:AdditionalDocumentReference><cbc:ID>QR</cbc:ID>\
                       <cac:Attachment><cbc:EmbeddedDocumentBinaryObject mimeCode=\"text/plain\">\
                       theirs</cbc:EmbeddedDocumentBinaryObject></cac:Attachment>\
                       </cac:AdditionalDocumentReference></Invoice>";
        invoice.stamped_xml = Some(base64::engine::general_purpose::STANDARD.encode(stamped));
        let handed = deliverable(&invoice).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            handed.qr, "theirs",
            "the QR on a cleared invoice is ZATCA's"
        );
        assert_eq!(handed.xml, stamped);

        assert_eq!(
            deliverable(&stored(Kind::Simplified, Status::Refused)),
            Err(NotDeliverable::Refused)
        );
        assert_eq!(
            deliverable(&stored(Kind::Standard, Status::Unregistered)),
            Err(NotDeliverable::Unregistered)
        );
    }

    /// A link is the number and a MAC of it; a byte off opens nothing.
    #[test]
    fn a_link_opens_its_document_and_nothing_else() {
        let secret = LinkSecret {
            secret: "ab".repeat(32),
        };
        let token = secret.token("INV-00001");
        assert!(token.starts_with("INV-00001."));
        assert_eq!(secret.opens(&token).as_deref(), Some("INV-00001"));
        // A different byte, so it is a forgery every run rather than fifteen
        // in sixteen.
        let last = token.chars().last().expect("a token");
        let mut forged = token.clone();
        forged.replace_range(token.len() - 1.., if last == '0' { "1" } else { "0" });
        assert_eq!(secret.opens(&forged), None);
        assert_eq!(
            secret.opens("INV-00002.0000000000000000000000000000000000"),
            None
        );
        assert_eq!(secret.opens("INV-00001"), None);
        let other = LinkSecret {
            secret: "cd".repeat(32),
        };
        assert_eq!(other.opens(&token), None, "another business's secret");
    }

    #[test]
    fn the_qr_is_read_back_out_of_a_document_and_amounts_print_bare() {
        assert_eq!(qr_in("<Invoice><cbc:ID>ICV</cbc:ID></Invoice>"), None);
        let sar = CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!());
        assert_eq!(amount(Money::from_minor(11_500, sar)), "115.00");
        assert_eq!(amount(Money::from_minor(-250, sar)), "-2.50");
        assert_eq!(percent(1_500), "15%");
        assert_eq!(percent(250), "2.50%");
        assert_eq!(esc("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
    }
}
