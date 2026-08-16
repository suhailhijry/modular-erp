//! The invoice as UBL 2.1, rendered already canonical.
//!
//! # Why it is written by hand
//!
//! Because the bytes are the point. ZATCA hashes the *canonicalised* document
//! (C14N 1.1) and the seller signs that hash, so a serialiser that reorders an
//! attribute or collapses an empty element changes the hash and invalidates the
//! signature. The usual pipeline — build a DOM, serialise it, run it through an
//! XSL transform, canonicalise, hash — has four places to go wrong and needs a
//! DOM library, an XSLT engine and a C14N implementation.
//!
//! This writes canonical form directly, so canonicalising the output is the
//! identity function and `hash(bytes) == hash(c14n(bytes))`. The rules that
//! keeps true, all of them enforced by
//! [`the_output_is_already_canonical`](tests::the_output_is_already_canonical):
//!
//! - UTF-8, `\n` endings, no XML declaration in the hashed bytes,
//! - no empty-element tags: `<cbc:Note></cbc:Note>`, never `<cbc:Note/>`,
//! - no comments, no processing instructions,
//! - namespaces declared once on the root, in prefix order,
//! - attributes in order, values with `&`, `<` and `"` escaped,
//! - text with `&`, `<` and `>` escaped, and control characters refused.
//!
//! # What is deliberately not here
//!
//! `ext:UBLExtensions`, `cac:Signature` and the QR's
//! `AdditionalDocumentReference` — the three things ZATCA *removes* before
//! hashing. Not generating them is the same document as generating and stripping
//! them, minus the stripping. They go in when there is a certificate to sign
//! with, wrapped around these bytes rather than mixed into them.

use std::fmt::Write as _;

use super::{Document, TypeCode, amount, category_code, exemption_reason, percent};
use crate::taxpayer::{Address, Registration};

/// A value carried something that cannot survive canonicalisation.
///
/// Refused rather than stripped: a document whose customer name silently lost a
/// character is one whose hash nobody can reproduce from the source data.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{field} contains a character that cannot appear in an XML document: {found:?}")]
pub struct NotRenderable {
    pub field: &'static str,
    pub found: char,
}

/// The three things ZATCA strips before hashing, put back for submission.
///
/// The signature and the QR are computed **from** the hashed document, so they
/// cannot be in it. This is what wraps around those bytes afterwards — see
/// [`signed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enveloped<'a> {
    /// The whole `ext:UBLExtensions` block from
    /// [`signing::Signature`](super::signing::Signature).
    pub extensions: &'a str,
    /// The base64 TLV a phone reads.
    pub qr: &'a str,
}

/// The canonical bytes: what gets hashed, and what the signature is over.
///
/// No XML declaration, because C14N removes one — so a document with a
/// declaration and one without hash the same, and this build hashes what it
/// sends. [`with_declaration`] adds it back for anything that stores or displays
/// the document.
///
/// # This is not a document, it is a canonicalisation artefact
///
/// ZATCA hashes what it receives **after** removing three elements with an XSL
/// transform — and a transform removes *elements*, not the whitespace text
/// nodes around them. Removing `<ext:UBLExtensions>` from
///
/// ```text
///   <Invoice …>\n  <ext:UBLExtensions>…</ext:UBLExtensions>\n  <cbc:ProfileID>
/// ```
///
/// leaves the `"\n  "` before it *and* the `"\n  "` after it, so the result is
/// `<Invoice …>\n  \n  <cbc:ProfileID>` — with a line carrying nothing but
/// indentation. This build never renders those elements, so it has to put their
/// leftovers in deliberately, or its hash is not the hash ZATCA computes.
///
/// **Confirmed against ZATCA**, which is the only way it could have been: with
/// the leftovers, a signed invoice is accepted; without them, the answer is
/// `invalid-invoice-hash`. See `modules/tax_sa/tests/sandbox.rs`.
pub fn render(document: &Document) -> Result<String, NotRenderable> {
    render_with(document, None)
}

/// The document as it is **submitted**: the hashed bytes, plus the signature,
/// the QR and the `cac:Signature` that points at it.
///
/// Rendered rather than spliced into [`render`]'s output. A string insertion at
/// a marker is a second parser that has to agree with the first about where the
/// document's parts are, and the two disagreeing would produce a document whose
/// hash is right and whose shape is wrong.
pub fn signed(document: &Document, enveloped: &Enveloped<'_>) -> Result<String, NotRenderable> {
    render_with(document, Some(enveloped))
}

fn render_with(
    document: &Document,
    enveloped: Option<&Enveloped<'_>>,
) -> Result<String, NotRenderable> {
    let element = document.type_code.element();
    let mut out = String::with_capacity(4096);

    // Namespaces on the root, in prefix order, which is where C14N wants them.
    let _ = writeln!(
        out,
        "<{element} xmlns=\"urn:oasis:names:specification:ubl:schema:xsd:{element}-2\" \
         xmlns:cac=\"urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2\" \
         xmlns:cbc=\"urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2\" \
         xmlns:ext=\"urn:oasis:names:specification:ubl:schema:xsd:CommonExtensionComponents-2\">"
    );

    // **First child of the root**, and the first of the three things the hash
    // leaves out.
    match enveloped {
        Some(enveloped) => out.push_str(enveloped.extensions),
        // The whitespace the element left behind. See `left_behind`.
        None => out.push_str("  \n"),
    }

    // The profile ZATCA registers every document under.
    text(&mut out, 1, "cbc:ProfileID", "reporting:1.0", "profile")?;
    text(&mut out, 1, "cbc:ID", &document.number, "number")?;
    text(&mut out, 1, "cbc:UUID", &document.uuid.to_string(), "uuid")?;
    text(
        &mut out,
        1,
        "cbc:IssueDate",
        &document.issued_at.format("%Y-%m-%d").to_string(),
        "issued_at",
    )?;
    text(
        &mut out,
        1,
        "cbc:IssueTime",
        &document.issued_at.format("%H:%M:%S").to_string(),
        "issued_at",
    )?;

    // The type code and, in its `name`, which of the two obligations this is.
    let _ = writeln!(
        out,
        "  <cbc:InvoiceTypeCode name=\"{}\">{}</cbc:InvoiceTypeCode>",
        document.kind.transaction_code(),
        document.type_code.code()
    );

    if !document.note.is_empty() {
        text(&mut out, 1, "cbc:Note", &document.note, "note")?;
    }

    let currency = document.currency.as_str();
    text(
        &mut out,
        1,
        "cbc:DocumentCurrencyCode",
        currency,
        "currency",
    )?;
    // The currency tax is *declared* in. The same one: a business invoicing in
    // another currency still declares in riyals, and this build does not yet
    // carry the exchange rate that would need.
    text(&mut out, 1, "cbc:TaxCurrencyCode", currency, "currency")?;

    if let Some(reference) = &document.reference {
        out.push_str("  <cac:BillingReference>\n");
        out.push_str("    <cac:InvoiceDocumentReference>\n");
        text(&mut out, 3, "cbc:ID", &reference.number, "reference")?;
        text(
            &mut out,
            3,
            "cbc:IssueDate",
            &reference.issued_at.format("%Y-%m-%d").to_string(),
            "reference",
        )?;
        out.push_str("    </cac:InvoiceDocumentReference>\n");
        out.push_str("  </cac:BillingReference>\n");
    }

    chain(&mut out, document)?;

    match enveloped {
        Some(enveloped) => stamp(&mut out, enveloped)?,
        // Two elements removed here, so two text nodes left behind.
        None => out.push_str("  \n  \n"),
    }

    supplier(&mut out, &document.seller)?;
    buyer(&mut out, document)?;

    // The supply date. `sales` records one date, so this is that date: an issue
    // date and a delivery date that differ is a fact somebody entered, and
    // nobody entered one.
    out.push_str("  <cac:Delivery>\n");
    text(
        &mut out,
        2,
        "cbc:ActualDeliveryDate",
        &document.issued_at.format("%Y-%m-%d").to_string(),
        "issued_at",
    )?;
    out.push_str("  </cac:Delivery>\n");

    if document.type_code != TypeCode::Invoice {
        why_the_note_was_issued(&mut out, document)?;
    }

    // **After `cac:PaymentMeans`, before `cac:TaxTotal`** — UBL's order, and a
    // document out of it is one ZATCA's schema check refuses.
    for allowance in &document.allowances {
        discount(&mut out, document, allowance)?;
    }

    tax_total(&mut out, document)?;
    monetary_total(&mut out, document);

    for (index, line) in document.lines.iter().enumerate() {
        invoice_line(&mut out, document, index, line)?;
    }

    let _ = write!(out, "</{element}>");
    Ok(out)
}

/// The same document with the declaration a file or an HTTP body wants.
///
/// Never hashed. C14N removes the declaration, so hashing this and hashing
/// [`render`]'s output give the same answer — but only one of them is what the
/// signature covers, and keeping them separate is what stops that being a
/// question.
#[must_use]
pub fn with_declaration(canonical: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{canonical}")
}

/// One discount, as `cac:AllowanceCharge`.
///
/// # Why the tax category is on it
///
/// Because a discount reduces the tax only if the thing discounted carried
/// any. UBL puts the treatment on the allowance itself, so a business can take
/// 10 off the standard-rated part of an invoice without touching the exempt
/// part — and so the taxable amount below adds up.
fn discount(
    out: &mut String,
    document: &Document,
    allowance: &super::Allowance,
) -> Result<(), NotRenderable> {
    let currency = document.currency.as_str();

    out.push_str("  <cac:AllowanceCharge>\n");
    // `false` is an allowance; `true` would be a charge. This build issues no
    // charges — a surcharge is a line, and one nobody has asked for.
    text(out, 2, "cbc:ChargeIndicator", "false", "allowance")?;
    text(
        out,
        2,
        "cbc:AllowanceChargeReason",
        &allowance.reason,
        "allowance_reason",
    )?;
    money(out, 2, "cbc:Amount", &amount(allowance.amount), currency);

    out.push_str("    <cac:TaxCategory>\n");
    let _ = writeln!(
        out,
        "      <cbc:ID schemeAgencyID=\"6\" schemeID=\"UN/ECE 5305\">{}</cbc:ID>",
        category_code(allowance.category)
    );
    let _ = writeln!(
        out,
        "      <cbc:Percent>{}</cbc:Percent>",
        percent(allowance.rate_bp)
    );
    out.push_str("      <cac:TaxScheme>\n");
    let _ = writeln!(
        out,
        "        <cbc:ID schemeAgencyID=\"6\" schemeID=\"UN/ECE 5153\">VAT</cbc:ID>"
    );
    out.push_str("      </cac:TaxScheme>\n");
    out.push_str("    </cac:TaxCategory>\n");
    out.push_str("  </cac:AllowanceCharge>\n");
    Ok(())
}

/// The counter and the document before this one. Two references, both
/// required, and the order is ZATCA's.
fn chain(out: &mut String, document: &Document) -> Result<(), NotRenderable> {
    out.push_str("  <cac:AdditionalDocumentReference>\n");
    text(out, 2, "cbc:ID", "ICV", "icv")?;
    text(out, 2, "cbc:UUID", &document.link.icv.to_string(), "icv")?;
    out.push_str("  </cac:AdditionalDocumentReference>\n");

    out.push_str("  <cac:AdditionalDocumentReference>\n");
    text(out, 2, "cbc:ID", "PIH", "pih")?;
    out.push_str("    <cac:Attachment>\n");
    let _ = write!(
        out,
        "      <cbc:EmbeddedDocumentBinaryObject mimeCode=\"text/plain\">"
    );
    escaped(out, &document.link.previous, "pih")?;
    out.push_str("</cbc:EmbeddedDocumentBinaryObject>\n");
    out.push_str("    </cac:Attachment>\n");
    out.push_str("  </cac:AdditionalDocumentReference>\n");
    Ok(())
}

/// **The reason a note was issued**, which ZATCA reads out of KSA-10 —
/// `cac:PaymentMeans/cbc:InstructionNote` — and not out of the general
/// `cbc:Note` where an earlier version of this put it. Without it, `BR-KSA-17`
/// refuses every credit and debit note.
///
/// UBL order: after `cac:Delivery`, before `cac:TaxTotal`. Only notes carry it:
/// an ordinary invoice is accepted without one, and adding it there would be
/// inventing a payment method nobody chose.
fn why_the_note_was_issued(out: &mut String, document: &Document) -> Result<(), NotRenderable> {
    out.push_str("  <cac:PaymentMeans>\n");
    // UN/ECE 4461. Not meaningful on a note, and required by UBL wherever
    // `cac:PaymentMeans` appears at all.
    text(out, 2, "cbc:PaymentMeansCode", "10", "payment_means")?;
    text(
        out,
        2,
        "cbc:InstructionNote",
        if document.note.is_empty() {
            "تصحيح"
        } else {
            &document.note
        },
        "note",
    )?;
    out.push_str("  </cac:PaymentMeans>\n");
    Ok(())
}

/// The QR and the `cac:Signature` that points at it.
///
/// UBL puts `cac:Signature` after the last `cac:AdditionalDocumentReference` and
/// before the parties, and a document out of that order is one ZATCA's schema
/// check refuses.
fn stamp(out: &mut String, enveloped: &Enveloped<'_>) -> Result<(), NotRenderable> {
    out.push_str("  <cac:AdditionalDocumentReference>\n");
    text(out, 2, "cbc:ID", "QR", "qr")?;
    out.push_str("    <cac:Attachment>\n");
    let _ = write!(
        out,
        "      <cbc:EmbeddedDocumentBinaryObject mimeCode=\"text/plain\">"
    );
    escaped(out, enveloped.qr, "qr")?;
    out.push_str("</cbc:EmbeddedDocumentBinaryObject>\n");
    out.push_str("    </cac:Attachment>\n");
    out.push_str("  </cac:AdditionalDocumentReference>\n");

    out.push_str("  <cac:Signature>\n");
    text(
        out,
        2,
        "cbc:ID",
        "urn:oasis:names:specification:ubl:signature:Invoice",
        "signature",
    )?;
    text(
        out,
        2,
        "cbc:SignatureMethod",
        "urn:oasis:names:specification:ubl:dsig:enveloped:xades",
        "signature",
    )?;
    out.push_str("  </cac:Signature>\n");
    Ok(())
}

// ---------------------------------------------------------------------------
// The parties
// ---------------------------------------------------------------------------

fn supplier(out: &mut String, seller: &Registration) -> Result<(), NotRenderable> {
    out.push_str("  <cac:AccountingSupplierParty>\n");
    out.push_str("    <cac:Party>\n");
    out.push_str("      <cac:PartyIdentification>\n");
    let _ = write!(
        out,
        "        <cbc:ID schemeID=\"{}\">",
        seller.scheme.as_str()
    );
    escaped(out, &seller.identifier, "identifier")?;
    out.push_str("</cbc:ID>\n");
    out.push_str("      </cac:PartyIdentification>\n");

    address(out, &seller.address, 3)?;

    out.push_str("      <cac:PartyTaxScheme>\n");
    text(out, 4, "cbc:CompanyID", &seller.vat_number, "vat_number")?;
    out.push_str("        <cac:TaxScheme>\n");
    text(out, 5, "cbc:ID", "VAT", "tax_scheme")?;
    out.push_str("        </cac:TaxScheme>\n");
    out.push_str("      </cac:PartyTaxScheme>\n");

    out.push_str("      <cac:PartyLegalEntity>\n");
    text(out, 4, "cbc:RegistrationName", &seller.name, "name")?;
    out.push_str("      </cac:PartyLegalEntity>\n");

    out.push_str("    </cac:Party>\n");
    out.push_str("  </cac:AccountingSupplierParty>\n");
    Ok(())
}

fn buyer(out: &mut String, document: &Document) -> Result<(), NotRenderable> {
    // A simplified invoice may have no buyer at all: nobody at a till gives a
    // name. The element is still required, so it carries what there is.
    out.push_str("  <cac:AccountingCustomerParty>\n");
    out.push_str("    <cac:Party>\n");

    if let Some(address) = document.buyer.as_ref().and_then(|b| b.address.as_ref()) {
        buyer_address(out, address)?;
    }

    if let Some(vat_number) = document.buyer.as_ref().and_then(|b| b.vat_number.as_ref()) {
        out.push_str("      <cac:PartyTaxScheme>\n");
        text(out, 4, "cbc:CompanyID", vat_number, "buyer_vat_number")?;
        out.push_str("        <cac:TaxScheme>\n");
        text(out, 5, "cbc:ID", "VAT", "tax_scheme")?;
        out.push_str("        </cac:TaxScheme>\n");
        out.push_str("      </cac:PartyTaxScheme>\n");
    }

    // UBL order: the address comes before the tax scheme and the legal entity,
    // so it is written into the buffer ahead of them.
    if let Some(name) = document.buyer.as_ref().map(|b| &b.name) {
        out.push_str("      <cac:PartyLegalEntity>\n");
        text(out, 4, "cbc:RegistrationName", name, "buyer_name")?;
        out.push_str("      </cac:PartyLegalEntity>\n");
    }

    out.push_str("    </cac:Party>\n");
    out.push_str("  </cac:AccountingCustomerParty>\n");
    Ok(())
}

/// The buyer's address, which is a looser shape than the seller's: a customer
/// abroad has no Saudi national address, and ZATCA asks only for street, city
/// and country.
fn buyer_address(out: &mut String, address: &sales::Address) -> Result<(), NotRenderable> {
    out.push_str("      <cac:PostalAddress>\n");
    text(out, 4, "cbc:StreetName", &address.street, "buyer_street")?;
    if let Some(building) = &address.building {
        text(out, 4, "cbc:BuildingNumber", building, "buyer_building")?;
    }
    if let Some(district) = &address.district {
        text(
            out,
            4,
            "cbc:CitySubdivisionName",
            district,
            "buyer_district",
        )?;
    }
    text(out, 4, "cbc:CityName", &address.city, "buyer_city")?;
    if let Some(postal_code) = &address.postal_code {
        text(out, 4, "cbc:PostalZone", postal_code, "buyer_postal_code")?;
    }
    out.push_str("        <cac:Country>\n");
    text(
        out,
        5,
        "cbc:IdentificationCode",
        &address.country,
        "buyer_country",
    )?;
    out.push_str("        </cac:Country>\n");
    out.push_str("      </cac:PostalAddress>\n");
    Ok(())
}

fn address(out: &mut String, address: &Address, depth: usize) -> Result<(), NotRenderable> {
    let pad = "  ".repeat(depth);
    let _ = writeln!(out, "{pad}<cac:PostalAddress>");
    text(out, depth + 1, "cbc:StreetName", &address.street, "street")?;
    text(
        out,
        depth + 1,
        "cbc:BuildingNumber",
        &address.building,
        "building",
    )?;
    if let Some(additional) = &address.additional {
        text(
            out,
            depth + 1,
            "cbc:PlotIdentification",
            additional,
            "additional",
        )?;
    }
    text(
        out,
        depth + 1,
        "cbc:CitySubdivisionName",
        &address.district,
        "district",
    )?;
    text(out, depth + 1, "cbc:CityName", &address.city, "city")?;
    text(
        out,
        depth + 1,
        "cbc:PostalZone",
        &address.postal_code,
        "postal_code",
    )?;
    let _ = writeln!(out, "{pad}  <cac:Country>");
    text(
        out,
        depth + 2,
        "cbc:IdentificationCode",
        &address.country,
        "country",
    )?;
    let _ = writeln!(out, "{pad}  </cac:Country>");
    let _ = writeln!(out, "{pad}</cac:PostalAddress>");
    Ok(())
}

// ---------------------------------------------------------------------------
// The money
// ---------------------------------------------------------------------------

fn tax_total(out: &mut String, document: &Document) -> Result<(), NotRenderable> {
    let currency = document.currency.as_str();

    out.push_str("  <cac:TaxTotal>\n");
    money(
        out,
        2,
        "cbc:TaxAmount",
        &amount(document.totals.tax),
        currency,
    );

    for band in &document.totals.bands {
        out.push_str("    <cac:TaxSubtotal>\n");
        money(out, 3, "cbc:TaxableAmount", &amount(band.net), currency);
        money(out, 3, "cbc:TaxAmount", &amount(band.tax), currency);
        out.push_str("      <cac:TaxCategory>\n");
        let _ = writeln!(
            out,
            "        <cbc:ID schemeAgencyID=\"6\" schemeID=\"UN/ECE 5305\">{}</cbc:ID>",
            category_code(band.category)
        );
        let _ = writeln!(
            out,
            "        <cbc:Percent>{}</cbc:Percent>",
            percent(band.rate_bp)
        );
        // Required on anything not standard-rated, and ZATCA's own code list.
        if let Some((code, reason)) = exemption_reason(band.category) {
            let _ = writeln!(
                out,
                "        <cbc:TaxExemptionReasonCode>{code}</cbc:TaxExemptionReasonCode>"
            );
            text(out, 4, "cbc:TaxExemptionReason", reason, "exemption")?;
        }
        out.push_str("        <cac:TaxScheme>\n");
        let _ = writeln!(
            out,
            "          <cbc:ID schemeAgencyID=\"6\" schemeID=\"UN/ECE 5153\">VAT</cbc:ID>"
        );
        out.push_str("        </cac:TaxScheme>\n");
        out.push_str("      </cac:TaxCategory>\n");
        out.push_str("    </cac:TaxSubtotal>\n");
    }
    out.push_str("  </cac:TaxTotal>\n");

    // **A second, bare total.** BR-KSA-EN16931-09: when `cbc:TaxCurrencyCode`
    // is present, exactly one `cac:TaxTotal` without subtotals must be too.
    // ZATCA warns about its absence rather than refusing, which is how the
    // first accepted document still had something wrong with it.
    out.push_str("  <cac:TaxTotal>\n");
    money(
        out,
        2,
        "cbc:TaxAmount",
        &amount(document.totals.tax),
        currency,
    );
    out.push_str("  </cac:TaxTotal>\n");
    Ok(())
}

fn monetary_total(out: &mut String, document: &Document) {
    let currency = document.currency.as_str();
    // **Three different numbers when there is a discount.** What the lines came
    // to, what is taxed, and what was taken off in between — and they have to
    // agree, because ZATCA checks that they do.
    let lines_came_to = amount(document.totals.lines_came_to());
    let taxable = amount(document.totals.net);
    let discounted = amount(document.totals.discount());
    let zero = amount(spa_types::Money::zero(document.currency));

    out.push_str("  <cac:LegalMonetaryTotal>\n");
    money(out, 2, "cbc:LineExtensionAmount", &lines_came_to, currency);
    money(out, 2, "cbc:TaxExclusiveAmount", &taxable, currency);
    money(
        out,
        2,
        "cbc:TaxInclusiveAmount",
        &amount(document.totals.gross),
        currency,
    );
    // The sum of the allowances above. Nothing prepaid: a payment against an
    // invoice is recorded separately and does not change what the invoice was
    // for.
    money(out, 2, "cbc:AllowanceTotalAmount", &discounted, currency);
    money(out, 2, "cbc:PrepaidAmount", &zero, currency);
    money(
        out,
        2,
        "cbc:PayableAmount",
        &amount(document.totals.gross),
        currency,
    );
    out.push_str("  </cac:LegalMonetaryTotal>\n");
}

fn invoice_line(
    out: &mut String,
    document: &Document,
    index: usize,
    line: &super::Line,
) -> Result<(), NotRenderable> {
    let currency = document.currency.as_str();
    // `cac:InvoiceLine` and `cbc:InvoicedQuantity` on all three, for the same
    // reason the root is always `<Invoice>`: ZATCA's schema is UBL's Invoice
    // schema, and a credit note is an invoice with a different type code.
    let element = "cac:InvoiceLine";
    let quantity = "cbc:InvoicedQuantity";

    let _ = writeln!(out, "  <{element}>");
    let _ = writeln!(out, "    <cbc:ID>{}</cbc:ID>", index + 1);
    // One, always. `sales` records what a line comes to, not the factors it was
    // computed from — see `sales::InvoiceLine`.
    let _ = writeln!(out, "    <{quantity} unitCode=\"PCE\">1</{quantity}>");
    money(
        out,
        2,
        "cbc:LineExtensionAmount",
        &amount(line.net),
        currency,
    );

    out.push_str("    <cac:TaxTotal>\n");
    money(out, 3, "cbc:TaxAmount", &amount(line.tax), currency);
    if let Some(gross) = line.gross() {
        money(out, 3, "cbc:RoundingAmount", &amount(gross), currency);
    }
    out.push_str("    </cac:TaxTotal>\n");

    out.push_str("    <cac:Item>\n");
    text(out, 3, "cbc:Name", &line.description, "description")?;
    out.push_str("      <cac:ClassifiedTaxCategory>\n");
    let _ = writeln!(
        out,
        "        <cbc:ID>{}</cbc:ID>",
        category_code(line.category)
    );
    let _ = writeln!(
        out,
        "        <cbc:Percent>{}</cbc:Percent>",
        percent(line.rate_bp)
    );
    out.push_str("        <cac:TaxScheme>\n");
    text(out, 5, "cbc:ID", "VAT", "tax_scheme")?;
    out.push_str("        </cac:TaxScheme>\n");
    out.push_str("      </cac:ClassifiedTaxCategory>\n");
    out.push_str("    </cac:Item>\n");

    out.push_str("    <cac:Price>\n");
    money(out, 3, "cbc:PriceAmount", &amount(line.net), currency);
    out.push_str("    </cac:Price>\n");

    let _ = writeln!(out, "  </{element}>");
    Ok(())
}

// ---------------------------------------------------------------------------
// The bytes
// ---------------------------------------------------------------------------

/// An element whose text is escaped and whose emptiness is still written out in
/// full — `<cbc:Note></cbc:Note>`, because C14N has no self-closing tag.
fn text(
    out: &mut String,
    depth: usize,
    element: &str,
    value: &str,
    field: &'static str,
) -> Result<(), NotRenderable> {
    let _ = write!(out, "{}<{element}>", "  ".repeat(depth));
    escaped(out, value, field)?;
    let _ = writeln!(out, "</{element}>");
    Ok(())
}

/// An amount, with the `currencyID` every UBL amount carries.
///
/// Takes the rendered string rather than `Money` so the caller is the one that
/// chose the format — there is exactly one formatter ([`super::amount`]) and
/// this keeps it from being bypassed by an accidental `Display`.
fn money(out: &mut String, depth: usize, element: &str, value: &str, currency: &str) {
    let _ = writeln!(
        out,
        "{}<{element} currencyID=\"{currency}\">{value}</{element}>",
        "  ".repeat(depth)
    );
}

/// Text content, escaped for C14N.
///
/// `&`, `<` and `>` in text; a control character is refused rather than dropped.
/// C14N escapes `>` as well as `<`, which a serialiser is free not to — and the
/// difference is a different hash.
fn escaped(out: &mut String, value: &str, field: &'static str) -> Result<(), NotRenderable> {
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // Tab, newline and carriage return are legal in XML text but arrive
            // in a customer's name only by accident, and C14N normalises them
            // differently in attributes than in text. Refused, so the hash never
            // depends on which one it was.
            c if c.is_control() => return Err(NotRenderable { field, found: c }),
            c => out.push(c),
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::zatca::{Band, Buyer, Kind, Line, Reference, Totals, document_uuid};
    use ledger::VatCategory;
    use spa_types::{CurrencyCode, Money, Timestamp};

    fn sar() -> CurrencyCode {
        CurrencyCode::new("SAR").expect("valid")
    }

    fn at(text: &str) -> Timestamp {
        text.parse().expect("a valid timestamp")
    }

    /// One invoice, 100.00 net at 15%.
    pub(crate) fn document() -> Document {
        let currency = sar();
        let net = Money::from_minor(10_000, currency);
        let tax = Money::from_minor(1_500, currency);
        Document {
            kind: Kind::Standard,
            type_code: TypeCode::Invoice,
            number: "INV-00001".to_owned(),
            uuid: document_uuid("310122393500003", "INV-00001"),
            issued_at: at("2026-03-01T10:00:00Z"),
            currency,
            seller: crate::taxpayer::tests::registration(),
            buyer: Some(Buyer {
                name: "شركة الأمل".to_owned(),
                vat_number: Some("300000000000003".to_owned()),
                address: Some(Box::new(sales::Address {
                    street: "طريق العروبة".to_owned(),
                    city: "الرياض".to_owned(),
                    country: "SA".to_owned(),
                    district: Some("الملز".to_owned()),
                    building: Some("4321".to_owned()),
                    postal_code: Some("12611".to_owned()),
                })),
            }),
            lines: vec![Line {
                description: "استشارات".to_owned(),
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
            link: super::super::Link::first(),
            reference: None,
            note: String::new(),
        }
    }

    /// The same invoice at a till: no buyer, and the other obligation.
    pub(crate) fn simplified() -> Document {
        Document {
            kind: crate::zatca::Kind::Simplified,
            buyer: None,
            ..document()
        }
    }

    /// The credit note that cancels it.
    pub(crate) fn credit_note() -> Document {
        Document {
            type_code: TypeCode::CreditNote,
            number: "CN-00001".to_owned(),
            note: "إرجاع & <البضاعة>".to_owned(),
            reference: Some(Reference {
                number: "INV-00001".to_owned(),
                issued_at: at("2026-03-01T10:00:00Z"),
            }),
            ..document()
        }
    }

    #[test]
    fn a_standard_invoice_carries_everything_zatca_reads() {
        let xml = render(&document()).expect("renders");

        assert!(xml.starts_with("<Invoice xmlns="), "{}", &xml[..60]);
        assert!(xml.contains("<cbc:ID>INV-00001</cbc:ID>"));
        assert!(xml.contains("<cbc:InvoiceTypeCode name=\"0100000\">388</cbc:InvoiceTypeCode>"));
        assert!(xml.contains("<cbc:IssueDate>2026-03-01</cbc:IssueDate>"));
        assert!(xml.contains("<cbc:IssueTime>10:00:00</cbc:IssueTime>"));
        // The seller, in Arabic.
        assert!(xml.contains("<cbc:RegistrationName>روابي للاستشارات</cbc:RegistrationName>"));
        assert!(xml.contains("<cbc:CompanyID>310122393500003</cbc:CompanyID>"));
        // The buyer's number is what made it standard, so it has to be in there.
        assert!(xml.contains("<cbc:CompanyID>300000000000003</cbc:CompanyID>"));
        // The chain.
        assert!(xml.contains("<cbc:ID>ICV</cbc:ID>\n    <cbc:UUID>1</cbc:UUID>"));
        assert!(xml.contains(&super::super::chain::genesis()));
        // The money.
        assert!(xml.contains("<cbc:TaxAmount currencyID=\"SAR\">15.00</cbc:TaxAmount>"));
        assert!(xml.contains("<cbc:PayableAmount currencyID=\"SAR\">115.00</cbc:PayableAmount>"));
        assert!(xml.contains("<cbc:Percent>15.00</cbc:Percent>"));
        assert!(xml.ends_with("</Invoice>"));
    }

    /// The thing that decides which ZATCA endpoint the document goes to has to
    /// be visible in the document itself.
    #[test]
    fn a_simplified_invoice_says_so_in_its_type_code() {
        let mut document = document();
        document.kind = Kind::Simplified;
        document.buyer = None;

        let xml = render(&document).expect("renders");
        assert!(xml.contains("<cbc:InvoiceTypeCode name=\"0200000\">388</cbc:InvoiceTypeCode>"));
        // No buyer at a till, and the element is still there.
        assert!(xml.contains("<cac:AccountingCustomerParty>"));
        assert!(!xml.contains("<cbc:CompanyID>300000000000003</cbc:CompanyID>"));
    }

    #[test]
    fn a_credit_note_is_a_credit_note_all_the_way_down() {
        let plain_invoice = render(&document()).expect("renders");
        let mut document = document();
        document.type_code = TypeCode::CreditNote;
        document.number = "CN-00001".to_owned();
        document.note = "إرجاع".to_owned();
        document.reference = Some(Reference {
            number: "INV-00001".to_owned(),
            issued_at: at("2026-03-01T10:00:00Z"),
        });

        let xml = render(&document).expect("renders");
        // **An `<Invoice>`, with 381 in the type code.** ZATCA's schema is UBL's
        // Invoice schema; the UBL `CreditNote` document type is rejected by the
        // gateway before validation begins.
        assert!(xml.starts_with("<Invoice xmlns="));
        assert!(xml.ends_with("</Invoice>"));
        assert!(xml.contains("CommonAggregateComponents"));
        assert!(xml.contains("<cbc:InvoiceTypeCode name=\"0100000\">381</cbc:InvoiceTypeCode>"));
        assert!(xml.contains("<cac:InvoiceLine>"));
        assert!(xml.contains("<cbc:InvoicedQuantity unitCode=\"PCE\">1</cbc:InvoicedQuantity>"));
        assert!(
            !xml.contains("CreditNoteLine"),
            "UBL's credit-note shape is not ZATCA's"
        );
        // The invoice it credits, which ZATCA requires on one.
        assert!(xml.contains("<cac:BillingReference>"));
        assert!(xml.contains("<cbc:ID>INV-00001</cbc:ID>"));
        // **And why**, in KSA-10 — `cac:PaymentMeans/cbc:InstructionNote`, not
        // the general note. `BR-KSA-17` refuses a note without it.
        assert!(xml.contains("<cbc:InstructionNote>إرجاع</cbc:InstructionNote>"));
        assert!(xml.contains("<cbc:PaymentMeansCode>10</cbc:PaymentMeansCode>"));
        assert!(xml.contains("<cbc:Note>إرجاع</cbc:Note>"));

        // An ordinary invoice carries neither, and ZATCA accepts it without.
        assert!(!plain_invoice.contains("<cac:PaymentMeans>"));
    }

    /// Zero-rated and exempt lines need a reason, and they need different ones.
    #[test]
    fn a_zero_rated_band_states_why_it_is_zero() {
        let mut document = document();
        let currency = sar();
        document.totals.bands = vec![
            Band {
                category: VatCategory::Zero,
                rate_bp: 0,
                net: Money::from_minor(5_000, currency),
                tax: Money::zero(currency),
            },
            Band {
                category: VatCategory::Exempt,
                rate_bp: 0,
                net: Money::from_minor(5_000, currency),
                tax: Money::zero(currency),
            },
        ];

        let xml = render(&document).expect("renders");
        assert!(xml.contains("<cbc:TaxExemptionReasonCode>VATEX-SA-32"));
        assert!(xml.contains("<cbc:TaxExemptionReasonCode>VATEX-SA-29"));
        assert!(xml.contains(">Z</cbc:ID>"));
        assert!(xml.contains(">E</cbc:ID>"));
    }

    /// Walks the rendered text and reports the first thing wrong with it.
    ///
    /// # Why a scanner and not a parser
    ///
    /// Because the claim being checked is about **bytes**, and a parser would
    /// throw those away — reading the document into a tree and asking whether it
    /// is canonical is asking the tree, which no longer knows. It also keeps a
    /// XML dependency out of a workspace that has no other use for one.
    ///
    /// It is a separate reading of the output from the one that produced it,
    /// which is the part that matters: it walks the string with a tag stack and
    /// knows nothing about how any of it was written.
    fn scan(xml: &str) -> Result<usize, String> {
        let root_prefixes: Vec<String> = xml
            .lines()
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|token| token.strip_prefix("xmlns:"))
            .filter_map(|token| token.split('=').next())
            .map(str::to_owned)
            .collect();

        let mut stack: Vec<String> = Vec::new();
        let mut elements = 0usize;
        let mut rest = xml;

        while let Some(open) = rest.find('<') {
            // Everything before the tag is text content.
            let (text, tail) = rest.split_at(open);
            if let Some(c) = text.chars().find(|c| matches!(c, '&' | '>')) {
                // `&` only ever as the start of an entity, `>` never raw.
                let entity = text.split('&').skip(1).all(|after| {
                    ["amp;", "lt;", "gt;", "quot;", "apos;"]
                        .iter()
                        .any(|e| after.starts_with(e))
                });
                if c == '>' || !entity {
                    return Err(format!("unescaped {c:?} in text: {text:?}"));
                }
            }

            let close = tail.find('>').ok_or("a `<` with no `>`")?;
            let tag = &tail[1..close];
            rest = &tail[close + 1..];

            if tag.ends_with('/') {
                return Err(format!("empty-element tag <{tag}>"));
            }
            if tag.starts_with('!') || tag.starts_with('?') {
                return Err(format!("a comment or declaration survived: <{tag}>"));
            }

            if let Some(name) = tag.strip_prefix('/') {
                match stack.pop() {
                    Some(open) if open == name => {}
                    Some(open) => return Err(format!("</{name}> closes <{open}>")),
                    None => return Err(format!("</{name}> closes nothing")),
                }
                continue;
            }

            // Split on whitespace *outside* quotes: `schemeID="UN/ECE 5305"` is
            // one attribute and the naive split makes it two, which is how the
            // first version of this scanner reported an error that was its own.
            let mut parts = Vec::new();
            let mut quoted = false;
            let mut token = String::new();
            for c in tag.chars() {
                match c {
                    '"' => {
                        quoted = !quoted;
                        token.push(c);
                    }
                    c if c.is_whitespace() && !quoted => {
                        if !token.is_empty() {
                            parts.push(std::mem::take(&mut token));
                        }
                    }
                    c => token.push(c),
                }
            }
            if !token.is_empty() {
                parts.push(token);
            }
            let mut parts = parts.into_iter();
            let name = parts.next().unwrap_or_default();
            elements += 1;

            // Every prefix used is one the root declared.
            if let Some((prefix, _)) = name.split_once(':')
                && !root_prefixes.iter().any(|p| p == prefix)
            {
                return Err(format!("<{name}> uses an undeclared prefix {prefix:?}"));
            }

            // Attributes in C14N order: `xmlns` declarations first, then the
            // rest, each group sorted. Namespace declarations only on the root.
            let attributes: Vec<String> = parts
                .filter_map(|a| a.split('=').next().map(str::to_owned))
                .collect();
            let (namespaces, plain): (Vec<&String>, Vec<&String>) = attributes
                .iter()
                .partition(|a| a.starts_with("xmlns:") || a.as_str() == "xmlns");
            if !namespaces.is_empty() && !stack.is_empty() {
                return Err(format!("<{name}> declares a namespace below the root"));
            }
            for group in [&namespaces, &plain] {
                let mut sorted = (*group).clone();
                sorted.sort_unstable();
                if group.as_slice() != sorted.as_slice() {
                    return Err(format!("<{name}> has attributes out of order: {group:?}"));
                }
            }

            stack.push(name);
        }

        if let Some(unclosed) = stack.first() {
            return Err(format!("<{unclosed}> never closed"));
        }
        Ok(elements)
    }

    /// **The property the hash depends on.** Everything the module docs promise,
    /// checked on real output rather than trusted.
    ///
    /// # What this does not check
    ///
    /// That the bytes are byte-identical to what a C14N 1.1 implementation would
    /// produce from them. There is no such implementation in this workspace, and
    /// the one that settles it is ZATCA's own SDK — which needs a certificate.
    /// What is checked here is every rule that produces the difference, so a
    /// change that breaks canonicality fails here rather than at a tax authority.
    #[test]
    fn the_output_is_already_canonical() {
        for document in [document(), simplified(), credit_note()] {
            let xml = render(&document).expect("renders");
            let what = format!("{:?}/{:?}", document.kind, document.type_code);

            assert!(
                !xml.contains("<?xml"),
                "{what}: C14N removes the declaration"
            );
            assert!(!xml.contains('\r'), "{what}: line endings are \\n");
            assert!(!xml.contains('\t'), "{what}: a tab would reach the hash");

            match scan(&xml) {
                Ok(elements) => assert!(elements > 40, "{what}: only {elements} elements"),
                Err(problem) => panic!("{what}: {problem}"),
            }

            // Namespace declarations on the root, in C14N's order: the default
            // one first — it has no local name, so it is lexicographically least
            // — and the prefixed ones after it, by prefix.
            let root = xml.lines().next().unwrap_or_default();
            let prefixes: Vec<&str> = root
                .split_whitespace()
                .filter(|token| token.starts_with("xmlns"))
                .filter_map(|token| token.split('=').next())
                .collect();
            let mut sorted = prefixes.clone();
            sorted.sort_unstable();
            assert_eq!(prefixes, sorted, "{what}: namespaces out of prefix order");
            assert_eq!(
                prefixes.first(),
                Some(&"xmlns"),
                "{what}: default not first"
            );
        }
    }

    /// The scanner is only worth having if it says no.
    #[test]
    fn the_scanner_refuses_what_it_claims_to() {
        let root = "<a xmlns:cbc=\"u\">";
        assert!(scan(&format!("{root}<cbc:b></cbc:b></a>")).is_ok());

        for (bad, why) in [
            (format!("{root}<cbc:b/></a>"), "empty-element tag"),
            (format!("{root}<cbc:b></cbc:c></a>"), "mismatched close"),
            (format!("{root}<cbc:b></a>"), "never closed"),
            (format!("{root}<zzz:b></zzz:b></a>"), "undeclared prefix"),
            (format!("{root}<cbc:b>a > b</cbc:b></a>"), "raw >"),
            (format!("{root}<cbc:b>a & b</cbc:b></a>"), "bare ampersand"),
            (
                format!("{root}<cbc:b schemeID=\"x\" mimeCode=\"y\"></cbc:b></a>"),
                "attributes out of order",
            ),
            (
                format!("{root}<cbc:b xmlns:x=\"u\"></cbc:b></a>"),
                "namespace below the root",
            ),
            (format!("{root}<!-- c --></a>"), "comment"),
        ] {
            assert!(scan(&bad).is_err(), "{why} was accepted: {bad}");
        }
    }

    /// **The hashed document and the submitted one are the same document.**
    ///
    /// The signature is over the first, and the second is what ZATCA receives
    /// and re-derives the first from — by removing exactly the three things
    /// [`signed`] added.
    #[test]
    fn the_submitted_document_is_the_hashed_one_plus_the_three_removed_things() {
        let document = document();
        let hashed = render(&document).expect("renders");
        let submitted = signed(
            &document,
            &Enveloped {
                extensions: "  <ext:UBLExtensions>the signature</ext:UBLExtensions>\n",
                qr: "AQID",
            },
        )
        .expect("renders");

        assert!(submitted.len() > hashed.len());
        assert!(submitted.contains("<ext:UBLExtensions>"));
        assert!(submitted.contains("<cbc:ID>QR</cbc:ID>"));
        assert!(submitted.contains(">AQID</cbc:EmbeddedDocumentBinaryObject>"));
        assert!(submitted.contains("<cac:Signature>"));
        assert!(submitted.contains("urn:oasis:names:specification:ubl:dsig:enveloped:xades"));

        // The extensions are the first child of the root, where ZATCA looks.
        let root_ends = submitted.find('\n').unwrap_or_default();
        assert!(
            submitted[root_ends..root_ends + 40].contains("<ext:UBLExtensions>"),
            "the extensions are not the first child"
        );

        // **The order UBL requires**: the QR reference is still an
        // AdditionalDocumentReference, and cac:Signature comes after the last of
        // them and before the parties.
        let qr = submitted
            .find("<cbc:ID>QR</cbc:ID>")
            .expect("the QR reference");
        let signature = submitted.find("<cac:Signature>").expect("the signature");
        let supplier = submitted
            .find("<cac:AccountingSupplierParty>")
            .expect("the supplier");
        let pih = submitted.find("<cbc:ID>PIH</cbc:ID>").expect("the chain");
        assert!(pih < qr, "the QR reference comes after the chain");
        assert!(qr < signature, "cac:Signature comes after the references");
        assert!(
            signature < supplier,
            "cac:Signature comes before the parties"
        );

        // And removing what was added gets the hashed document back — which is
        // what ZATCA's verifier does with the XPath transforms.
        assert!(hashed.contains("<cbc:ID>INV-00001</cbc:ID>"));
        assert!(!hashed.contains("<ext:UBLExtensions>"));
        assert!(!hashed.contains("<cac:Signature>"));
        assert!(!hashed.contains("<cbc:ID>QR</cbc:ID>"));
    }

    /// The signed document has to survive the same scanner as the unsigned one,
    /// because it is the one that is actually submitted.
    #[test]
    fn the_signed_document_is_still_well_formed() {
        let signed = signed(
            &document(),
            &Enveloped {
                extensions: "  <ext:UBLExtensions>\n    <ext:UBLExtension>x</ext:UBLExtension>\n  </ext:UBLExtensions>\n",
                qr: "AQID",
            },
        )
        .expect("renders");
        match scan(&signed) {
            Ok(elements) => assert!(elements > 45, "only {elements} elements"),
            Err(problem) => panic!("{problem}"),
        }
    }

    /// **A discount is its own figure on the document**, and the three monetary
    /// totals it produces have to agree.
    #[test]
    fn a_discount_is_an_allowance_charge_and_the_totals_add_up() {
        let currency = sar();
        let mut document = document();
        document.allowances = vec![crate::zatca::Allowance {
            reason: "خصم".to_owned(),
            amount: Money::from_minor(1_500, currency),
            category: VatCategory::Standard,
            rate_bp: 1_500,
        }];
        // 100.00 of lines, 15.00 off, so 85.00 taxed at 15% = 12.75.
        document.totals = Totals {
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

        let xml = render(&document).expect("renders");

        assert!(xml.contains("<cac:AllowanceCharge>"));
        assert!(xml.contains("<cbc:ChargeIndicator>false</cbc:ChargeIndicator>"));
        assert!(xml.contains("<cbc:AllowanceChargeReason>خصم</cbc:AllowanceChargeReason>"));
        assert!(xml.contains("<cbc:Amount currencyID=\"SAR\">15.00</cbc:Amount>"));

        // **The three numbers.** What the lines came to, what was taken off,
        // and what is taxed — ZATCA checks that they agree.
        assert!(xml.contains(
            "<cbc:LineExtensionAmount currencyID=\"SAR\">100.00</cbc:LineExtensionAmount>"
        ));
        assert!(xml.contains(
            "<cbc:AllowanceTotalAmount currencyID=\"SAR\">15.00</cbc:AllowanceTotalAmount>"
        ));
        assert!(
            xml.contains(
                "<cbc:TaxExclusiveAmount currencyID=\"SAR\">85.00</cbc:TaxExclusiveAmount>"
            )
        );
        assert!(
            xml.contains(
                "<cbc:TaxInclusiveAmount currencyID=\"SAR\">97.75</cbc:TaxInclusiveAmount>"
            )
        );
        assert!(xml.contains("<cbc:PayableAmount currencyID=\"SAR\">97.75</cbc:PayableAmount>"));
        // And the band reports the discounted amount, because that is what was
        // supplied.
        assert!(xml.contains("<cbc:TaxableAmount currencyID=\"SAR\">85.00</cbc:TaxableAmount>"));

        // UBL order: after `cac:Delivery`, before `cac:TaxTotal`.
        let delivery = xml.find("<cac:Delivery>").expect("delivery");
        let allowance = xml.find("<cac:AllowanceCharge>").expect("allowance");
        let tax = xml.find("<cac:TaxTotal>").expect("tax total");
        assert!(delivery < allowance && allowance < tax);

        match scan(&xml) {
            Ok(_) => {}
            Err(problem) => panic!("{problem}"),
        }
    }

    /// An invoice with no discount says so by saying nothing, and its totals
    /// are the ones every invoice had before discounts existed.
    #[test]
    fn without_a_discount_nothing_is_allowed_and_the_totals_are_unchanged() {
        let xml = render(&document()).expect("renders");
        assert!(!xml.contains("<cac:AllowanceCharge>"));
        assert!(xml.contains(
            "<cbc:AllowanceTotalAmount currencyID=\"SAR\">0.00</cbc:AllowanceTotalAmount>"
        ));
        assert!(xml.contains(
            "<cbc:LineExtensionAmount currencyID=\"SAR\">100.00</cbc:LineExtensionAmount>"
        ));
        assert!(xml.contains(
            "<cbc:TaxExclusiveAmount currencyID=\"SAR\">100.00</cbc:TaxExclusiveAmount>"
        ));
    }

    /// Two renders of the same document are the same bytes — or the chain breaks
    /// on the first rebuild.
    #[test]
    fn rendering_is_deterministic() {
        assert_eq!(render(&document()), render(&document()));
    }

    #[test]
    fn markup_in_a_customers_name_is_escaped_and_not_executed() {
        let mut document = document();
        document.buyer = Some(Buyer {
            name: "Ampersand & <Sons>".to_owned(),
            vat_number: Some("300000000000003".to_owned()),
            address: None,
        });

        let xml = render(&document).expect("renders");
        assert!(xml.contains("Ampersand &amp; &lt;Sons&gt;"));
        assert!(!xml.contains("<Sons>"));
    }

    /// Refused, not stripped: a name that silently lost a character is a hash
    /// nobody can reproduce from the source.
    #[test]
    fn a_control_character_is_refused_rather_than_dropped() {
        let mut document = document();
        document.lines[0].description = "before\u{0}after".to_owned();
        assert_eq!(
            render(&document),
            Err(NotRenderable {
                field: "description",
                found: '\u{0}'
            })
        );
    }

    #[test]
    fn the_declaration_is_added_for_storage_and_never_for_the_hash() {
        let canonical = render(&document()).expect("renders");
        let stored = with_declaration(&canonical);
        assert!(stored.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Invoice"));
        assert!(stored.ends_with(&canonical));
    }
}
