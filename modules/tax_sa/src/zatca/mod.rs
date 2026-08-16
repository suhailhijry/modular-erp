//! ZATCA: the document a Saudi invoice has to become.
//!
//! # Two documents, two obligations
//!
//! ZATCA splits every invoice by who the buyer is, and the split decides *when*
//! the authority has to see it:
//!
//! | | buyer | before or after | call |
//! |---|---|---|---|
//! | **Standard** | a VAT-registered business | **cleared before it is given to the buyer** | `/invoices/clearance/single` |
//! | **Simplified** | a consumer | **reported within 24 hours** | `/invoices/reporting/single` |
//!
//! One field decides it: whether the buyer gave a VAT number. That is
//! [`Kind::of`], and it is the only place the decision is made.
//!
//! The difference is not paperwork. A standard invoice is not a valid invoice
//! until ZATCA has stamped it, so the seller cannot hand it over yet; a
//! simplified one is handed over at the till and the clock starts.
//!
//! # What this module builds, and what it cannot
//!
//! Built here, and testable to the byte:
//!
//! - the UBL 2.1 XML, rendered already canonical ([`ubl`]),
//! - the invoice hash and the chain that links each document to the last
//!   ([`chain`]),
//! - the QR block a phone reads ([`qr`]),
//! - the standard/simplified decision and everything that follows from it,
//! - the request and response bodies ZATCA's API speaks ([`wire`]).
//!
//! Not built here, because it needs a certificate this project does not have:
//! the `XAdES` signature over the document, and the HTTPS call that carries it.
//! Those are one implementation of [`wire::Submitter`], and the shape of the
//! thing it submits is settled without them.
//!
//! # Why the document is a projection and not a command
//!
//! Because nothing in the issuing transaction can build it. `sales` issues the
//! invoice and must not know that Saudi Arabia exists — the dependency runs
//! `tax_sa → sales`, and inverting it would put ZATCA in every tenant's sales
//! module including the ones in other countries.
//!
//! So the document is derived from the log, in this module's own projection
//! group, from `sales.invoice.issued` and `sales.invoice.cancelled`. That is the
//! shape of every extension module: **the module being extended does not know**,
//! and the extending module subscribes.

pub mod chain;
pub mod qr;
pub mod ubl;
pub mod wire;

use ledger::VatCategory;
use serde::{Deserialize, Serialize};
use spa_types::{CurrencyCode, Money, Timestamp};
use uuid::Uuid;

pub use chain::Link;

use crate::taxpayer::Registration;

/// Which of the two obligations this document falls under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// B2B and B2G. **Cleared before the buyer gets it.**
    Standard,
    /// B2C. Handed over at the till and **reported within 24 hours**.
    Simplified,
}

impl Kind {
    pub const ALL: [Self; 2] = [Self::Standard, Self::Simplified];

    /// **The decision**, and the only place it is taken.
    ///
    /// A buyer who gave a VAT registration number is a business, and a business
    /// needs a standard invoice to reclaim the tax on it. Everyone else gets a
    /// simplified one.
    #[must_use]
    pub const fn of(buyer_vat_number: Option<&String>) -> Self {
        match buyer_vat_number {
            Some(_) => Self::Standard,
            None => Self::Simplified,
        }
    }

    /// The seven-digit subtype ZATCA puts in `InvoiceTypeCode/@name`.
    ///
    /// Position 1 is standard, position 2 simplified, and the remaining five are
    /// third-party, nominal, export, summary and self-billed — none of which
    /// this build issues yet, so they are zero.
    #[must_use]
    pub const fn transaction_code(self) -> &'static str {
        match self {
            Self::Standard => "0100000",
            Self::Simplified => "0200000",
        }
    }

    /// How long after issue the authority has to have seen it.
    ///
    /// `None` for a standard invoice, and that is not "no deadline": it has to
    /// be cleared *before* issue, so there is no window to be late in.
    #[must_use]
    pub const fn reporting_window(self) -> Option<chrono::TimeDelta> {
        match self {
            Self::Standard => None,
            Self::Simplified => Some(chrono::TimeDelta::hours(24)),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Simplified => "simplified",
        }
    }
}

impl std::str::FromStr for Kind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| format!("unknown ZATCA document kind {s:?}"))
    }
}

/// What kind of document this is, in UN/EDIFACT 1001 codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeCode {
    Invoice,
    /// Cancels or reduces an invoice. What a credit note is.
    CreditNote,
    /// Increases one. Not issued here yet — `sales` has no such command.
    DebitNote,
}

impl TypeCode {
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Invoice => 388,
            Self::CreditNote => 381,
            Self::DebitNote => 383,
        }
    }

    /// The UBL element name. A credit note is a different *document type* in
    /// UBL, not an invoice with a flag — and ZATCA reads it as one.
    #[must_use]
    pub const fn element(self) -> &'static str {
        match self {
            Self::Invoice => "Invoice",
            Self::CreditNote | Self::DebitNote => "CreditNote",
        }
    }
}

/// The buyer, as they were on the document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Buyer {
    pub name: String,
    /// Present exactly when this is a standard invoice — it is what makes it one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vat_number: Option<String>,
}

/// One charged thing, with the tax that was charged on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub description: String,
    pub net: Money,
    pub category: VatCategory,
    /// Basis points, as stamped on the invoice. Never today's rate.
    pub rate_bp: i32,
    pub tax: Money,
}

impl Line {
    /// Net plus tax — what UBL calls the line's rounding amount.
    pub fn gross(&self) -> Option<Money> {
        self.net.checked_add(self.tax).ok()
    }
}

/// A band of the document's tax total: everything at one treatment and rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Band {
    pub category: VatCategory,
    pub rate_bp: i32,
    pub net: Money,
    pub tax: Money,
}

/// What the document comes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub net: Money,
    pub tax: Money,
    pub gross: Money,
    pub bands: Vec<Band>,
}

/// The invoice a credit note is against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub number: String,
    pub issued_at: Timestamp,
}

/// A ZATCA document, ready to render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub kind: Kind,
    pub type_code: TypeCode,
    /// The statutory number, from the tenant's gapless series.
    pub number: String,
    /// ZATCA's own identifier for the document, distinct from the number.
    /// Derived, never random — see [`document_uuid`].
    pub uuid: Uuid,
    /// The tax point. Both the issue date and the supply date come from it,
    /// because `sales` records one date and inventing a second would be a
    /// difference nobody entered.
    pub issued_at: Timestamp,
    pub currency: CurrencyCode,
    pub seller: Registration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buyer: Option<Buyer>,
    pub lines: Vec<Line>,
    pub totals: Totals,
    pub link: Link,
    /// The invoice this credits, on a credit note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<Reference>,
    /// Why, on a credit note. ZATCA requires a reason on one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// ZATCA's per-document UUID, derived so a replay reproduces it.
///
/// A v5 UUID over the document's number: same number, same UUID, forever,
/// without storing one. A random v4 would be regenerated by every rebuild, and
/// the UUID is submitted alongside the hash — so a rebuild would disagree with
/// what ZATCA holds, on every document, with nothing to compare against.
///
/// The namespace is this system's, so two tenants numbering from `INV-00001`
/// do not collide: it is derived from the tenant's own VAT registration number,
/// which is unique by construction and is on the document anyway.
#[must_use]
pub fn document_uuid(vat_number: &str, number: &str) -> Uuid {
    let namespace = Uuid::new_v5(&Uuid::NAMESPACE_URL, b"https://zatca.gov.sa/einvoicing");
    Uuid::new_v5(&namespace, format!("{vat_number}:{number}").as_bytes())
}

/// An amount as ZATCA prints it: the bare number, at the currency's exponent.
///
/// `Money`'s own `Display` carries the currency (`115.00 SAR`), which is right
/// for a message to a person and wrong for an XML element whose `currencyID`
/// attribute already says it.
#[must_use]
pub fn amount(money: Money) -> String {
    let exponent = u32::from(money.currency().exponent());
    let sign = if money.minor() < 0 { "-" } else { "" };
    let magnitude = money.minor().unsigned_abs();
    if exponent == 0 {
        return format!("{sign}{magnitude}");
    }
    let divisor = 10u64.pow(exponent);
    let width = exponent as usize;
    format!(
        "{sign}{whole}.{fraction:0width$}",
        whole = magnitude / divisor,
        fraction = magnitude % divisor
    )
}

/// A rate as a percentage: 1500 basis points is `15.00`.
#[must_use]
pub fn percent(basis_points: i32) -> String {
    format!("{}.{:02}", basis_points / 100, (basis_points % 100).abs())
}

/// The UN/ECE 5305 code for a treatment.
///
/// `S`, `Z` and `E` — and the difference between the last two is money, which is
/// why `ledger::VatCategory` keeps them apart in the first place.
#[must_use]
pub const fn category_code(category: VatCategory) -> &'static str {
    match category {
        VatCategory::Standard => "S",
        VatCategory::Zero => "Z",
        VatCategory::Exempt => "E",
    }
}

/// Why a zero-rated or exempt line carries no tax.
///
/// ZATCA requires a reason on anything not at the standard rate, from its own
/// `VATEX-SA-*` list. This build states the general article for each, which is
/// the honest default when the reason is not something the line records.
///
/// ponytail: per-line exemption reasons want a field on the invoice line, which
/// is a `sales` change for a tenant who has asked for one. Until then a business
/// exporting and a business renting residential property both get the article
/// that covers them, and neither gets a reason that is wrong.
#[must_use]
pub const fn exemption_reason(category: VatCategory) -> Option<(&'static str, &'static str)> {
    match category {
        VatCategory::Standard => None,
        VatCategory::Zero => Some(("VATEX-SA-32", "Export of goods")),
        VatCategory::Exempt => Some(("VATEX-SA-29", "Financial services mentioned in Article 29")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buyers_vat_number_is_what_makes_an_invoice_standard() {
        let registered = "310122393500003".to_owned();
        assert_eq!(Kind::of(Some(&registered)), Kind::Standard);
        assert_eq!(Kind::of(None), Kind::Simplified);
    }

    /// The two obligations differ in *when*, and this is where that is written.
    #[test]
    fn only_a_simplified_invoice_has_a_window_to_be_late_in() {
        assert_eq!(
            Kind::Simplified.reporting_window(),
            Some(chrono::TimeDelta::hours(24))
        );
        assert_eq!(
            Kind::Standard.reporting_window(),
            None,
            "a standard invoice is cleared before issue, so there is no window"
        );
    }

    #[test]
    fn the_transaction_code_marks_the_right_position() {
        assert_eq!(Kind::Standard.transaction_code(), "0100000");
        assert_eq!(Kind::Simplified.transaction_code(), "0200000");
        for kind in Kind::ALL {
            assert_eq!(kind.transaction_code().len(), 7);
            assert_eq!(kind.as_str().parse::<Kind>(), Ok(kind));
        }
    }

    #[test]
    fn a_credit_note_is_a_different_document_and_not_a_flag() {
        assert_eq!(TypeCode::Invoice.code(), 388);
        assert_eq!(TypeCode::CreditNote.code(), 381);
        assert_eq!(TypeCode::Invoice.element(), "Invoice");
        assert_eq!(TypeCode::CreditNote.element(), "CreditNote");
    }

    #[test]
    fn the_uuid_is_the_same_every_time_it_is_derived() {
        let once = document_uuid("310122393500003", "INV-00001");
        assert_eq!(once, document_uuid("310122393500003", "INV-00001"));
        assert_ne!(once, document_uuid("310122393500003", "INV-00002"));
        assert_ne!(
            once,
            document_uuid("300000000000003", "INV-00001"),
            "two tenants both numbering from INV-00001 must not collide"
        );
    }

    #[test]
    fn amounts_render_at_the_currencys_exponent() {
        let sar = CurrencyCode::new("SAR").expect("valid");
        assert_eq!(amount(Money::from_minor(11_500, sar)), "115.00");
        assert_eq!(amount(Money::from_minor(5, sar)), "0.05");
        assert_eq!(amount(Money::from_minor(-11_500, sar)), "-115.00");
        assert_eq!(amount(Money::zero(sar)), "0.00");

        // No currency code in it — the element's attribute says that.
        assert!(!amount(Money::from_minor(11_500, sar)).contains("SAR"));

        let jpy = CurrencyCode::new("JPY").expect("valid");
        assert_eq!(amount(Money::from_minor(1_050, jpy)), "1050");
    }

    #[test]
    fn rates_render_as_percentages() {
        assert_eq!(percent(1_500), "15.00");
        assert_eq!(percent(0), "0.00");
        assert_eq!(percent(500), "5.00");
        assert_eq!(percent(1_505), "15.05");
    }

    #[test]
    fn every_category_has_a_code_and_only_the_zero_ones_need_a_reason() {
        assert_eq!(category_code(VatCategory::Standard), "S");
        assert_eq!(category_code(VatCategory::Zero), "Z");
        assert_eq!(category_code(VatCategory::Exempt), "E");

        assert!(exemption_reason(VatCategory::Standard).is_none());
        for category in [VatCategory::Zero, VatCategory::Exempt] {
            let (code, _) = exemption_reason(category).expect("a reason is required");
            assert!(code.starts_with("VATEX-SA-"), "{code} is not a ZATCA code");
        }
    }
}
