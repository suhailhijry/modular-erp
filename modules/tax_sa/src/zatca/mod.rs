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
pub mod csr;
pub mod finish;
pub mod http;
pub mod onboarding;
pub mod qr;
pub mod samples;
pub mod signing;
pub mod ubl;
pub mod wire;

use erp_types::{CurrencyCode, Money, Timestamp};
use ledger::VatCategory;
use serde::{Deserialize, Serialize};
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
    /// **An invoice for money taken before the supply.** A deposit.
    ///
    /// Its own code because receiving consideration is itself a tax point: the
    /// VAT is due when the money arrives, not when the service happens, and the
    /// authority wants the document within fifteen days of that month's end.
    /// The final invoice that follows carries a deduction line pointing back at
    /// this one, so the same money is not taxed twice.
    Prepayment,
}

impl TypeCode {
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Invoice => 388,
            Self::CreditNote => 381,
            Self::DebitNote => 383,
            Self::Prepayment => 386,
        }
    }

    /// The UBL element name, which is **`Invoice` for all three**.
    ///
    /// Generic UBL has a separate `CreditNote` document type, and an earlier
    /// version of this used it. ZATCA does not: its schema is the UBL *Invoice*
    /// schema, and a credit note is an `<Invoice>` whose `cbc:InvoiceTypeCode`
    /// says 381. Sending a `<CreditNote>` root is rejected before validation
    /// even begins — `HTTP 400 Invalid Request`, plain text, from the gateway
    /// rather than the validator, with nothing to say what was wrong.
    ///
    /// Confirmed against ZATCA; see `modules/tax_sa/tests/sandbox.rs`.
    #[must_use]
    pub const fn element(self) -> &'static str {
        "Invoice"
    }
}

/// The buyer, as they were on the document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Buyer {
    pub name: String,
    /// Present exactly when this is a standard invoice — it is what makes it one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vat_number: Option<String>,
    /// Where they are. ZATCA wants street, city and country on a standard
    /// invoice (BT-50, BT-52, BT-55) and accepts one without them **with a
    /// warning** — which is a warning that becomes a finding at an inspection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Box<sales::Address>>,
}

/// One charged thing, with the tax that was charged on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub description: String,
    /// **BT-131**, the line net amount: after this line's own allowances, and
    /// what the tax is worked out on.
    pub net: Money,
    pub category: VatCategory,
    /// Basis points, as stamped on the invoice. Never today's rate.
    pub rate_bp: i32,
    pub tax: Money,
    /// **BT-129**, the invoiced quantity — how many units this line charges
    /// for.
    ///
    /// `None` on a line given as a single amount, which renders as one: that is
    /// what every document issued before `sales` stored the factors says, and
    /// it is a true statement about a line with no quantity on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantity: Option<i64>,
    /// **BT-146**, the item net price — what one unit was charged at, before
    /// this line's own allowances.
    ///
    /// Present exactly when [`Self::quantity`] is, and taken from `sales`
    /// rather than divided out of the net: BT-131 is `quantity × BT-146` less
    /// the allowances, and a price worked back out of a total does not always
    /// land on a whole halala, so the rule would fail on the rounding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_price: Option<Money>,
    /// Why this line carries no tax, as stamped at issue time. See
    /// [`Band::exemption_reason`] — the band's is taken from its lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exemption_reason: Option<String>,
    /// **What came off this line** — UBL's `cac:AllowanceCharge` inside
    /// `cac:InvoiceLine`.
    ///
    /// No tax category on these, unlike [`Allowance`]: the line already carries
    /// one, and the standard's line-level allowance has no place to put a
    /// second. See `crate::zatca::ubl`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowances: Vec<LineAllowance>,
}

impl Line {
    /// Net plus tax — what UBL calls the line's rounding amount.
    pub fn gross(&self) -> Option<Money> {
        self.net.checked_add(self.tax).ok()
    }

    /// **What this line charges for**, as `cbc:InvoicedQuantity`. One on a line
    /// given as a single amount, which is what a line with no factors on it is.
    pub fn units(&self) -> i64 {
        self.quantity.unwrap_or(1)
    }

    /// **BT-146**, the item net price: what *one unit* was charged at, before
    /// this line's own allowances.
    ///
    /// The standard defines BT-131 as `quantity × BT-146` less the line's
    /// allowances, so a document that printed the line total here would fail
    /// the rule the moment a line carried an allowance or more than one unit.
    /// A line with no factors is one unit, and then the two coincide — which is
    /// what every document issued before `sales` stored them printed.
    pub fn price(&self) -> Option<Money> {
        self.unit_price.or_else(|| self.before_allowances())
    }

    /// **The line's amount before its own allowances**: BT-131 with the
    /// allowances added back, and the base they were taken from.
    ///
    /// The standard defines BT-131 as this less the line's allowances, so a
    /// document that printed the same figure for both would fail the rule the
    /// moment a line carried one.
    pub fn before_allowances(&self) -> Option<Money> {
        self.allowances
            .iter()
            .try_fold(self.net, |running, a| running.checked_add(a.amount))
            .ok()
    }
}

/// Something taken off one line — UBL's `cac:AllowanceCharge` within
/// `cac:InvoiceLine`.
///
/// **Deliberately smaller than [`Allowance`].** A line-level allowance takes an
/// indicator, an amount and a reason, and has **no `cac:TaxCategory`** — the
/// line it sits in already says how it is taxed. The document-level one has no
/// line to inherit from, so it must name the category and the rate or the
/// taxable amounts do not add up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineAllowance {
    pub reason: String,
    /// Positive: what comes off.
    pub amount: Money,
}

/// A band of the document's tax total: everything at one treatment and rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Band {
    pub category: VatCategory,
    pub rate_bp: i32,
    pub net: Money,
    pub tax: Money,
    /// **Why this band carries no tax**, as ZATCA's own `VATEX-SA-*` code,
    /// taken from the lines in it — which took it from the tenant's configured
    /// [`ledger::Rates`] at issue time (L5).
    ///
    /// `None` on a standard-rated band, which has nothing to explain, and on
    /// documents issued before the code was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exemption_reason: Option<String>,
}

/// Something taken off the whole document — UBL's `cac:AllowanceCharge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allowance {
    pub reason: String,
    /// Positive: what comes off.
    pub amount: Money,
    pub category: VatCategory,
    pub rate_bp: i32,
}

/// What the document comes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    /// **After the discounts**, and therefore what is taxed —
    /// `TaxExclusiveAmount`.
    pub net: Money,
    pub tax: Money,
    pub gross: Money,
    /// What the lines came to before any discount — `LineExtensionAmount`.
    ///
    /// Stored rather than derived because a document is rendered from what was
    /// recorded, and `net + sum(allowances)` would be a second computation that
    /// has to agree with the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_discount: Option<Money>,
    pub bands: Vec<Band>,
}

impl Totals {
    /// `LineExtensionAmount`: what the lines came to. The same as `net` when
    /// nothing was discounted.
    #[must_use]
    pub fn lines_came_to(&self) -> Money {
        self.before_discount.unwrap_or(self.net)
    }

    /// `AllowanceTotalAmount`.
    #[must_use]
    pub fn discount(&self) -> Money {
        self.lines_came_to()
            .checked_sub(self.net)
            .unwrap_or_else(|_| Money::zero(self.net.currency()))
    }
}

/// The invoice a credit note is against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub number: String,
    pub issued_at: Timestamp,
}

/// **The prepayment invoice a final invoice deducts**, and what it declared.
///
/// ZATCA's final invoice after a deposit shows the whole supply on its lines,
/// a prepayment line naming the earlier document with what it declared per
/// band, `PrepaidAmount` for what was paid up front, and tax totals for what
/// is left to declare. This carries the earlier document's half of that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepaidRef {
    pub number: String,
    pub issued_at: Timestamp,
    /// What the prepayment invoice declared, per band.
    pub bands: Vec<Band>,
}

impl PrepaidRef {
    #[must_use]
    pub fn net(&self, currency: CurrencyCode) -> Money {
        self.bands
            .iter()
            .try_fold(Money::zero(currency), |sum, b| sum.checked_add(b.net))
            .unwrap_or_else(|_| Money::zero(currency))
    }

    #[must_use]
    pub fn tax(&self, currency: CurrencyCode) -> Money {
        self.bands
            .iter()
            .try_fold(Money::zero(currency), |sum, b| sum.checked_add(b.tax))
            .unwrap_or_else(|_| Money::zero(currency))
    }

    /// What was paid up front, tax included — `cbc:PrepaidAmount`.
    #[must_use]
    pub fn gross(&self, currency: CurrencyCode) -> Money {
        self.net(currency)
            .checked_add(self.tax(currency))
            .unwrap_or_else(|_| Money::zero(currency))
    }
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
    /// **The clock `IssueDate` and `IssueTime` are read by.** An invoice
    /// issued at 23:30 in Riyadh on the last day of the quarter is dated that
    /// day, not the UTC day after; ZATCA sees the seller's calendar. From the
    /// event that produced the document, so a rebuild renders the same XML.
    #[serde(default)]
    pub calendar: erp_types::Calendar,
    pub currency: CurrencyCode,
    pub seller: Registration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buyer: Option<Buyer>,
    pub lines: Vec<Line>,
    /// What was taken off the whole document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowances: Vec<Allowance>,
    /// **What this document charges and declares.** For a final invoice after
    /// a deposit that is the supply less the prepayment; the lines still show
    /// the whole supply, and [`Self::prepaid`] says what came off.
    pub totals: Totals,
    pub link: Link,
    /// The invoice this credits, on a credit note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<Reference>,
    /// The prepayment invoice deducted from this one, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepaid: Option<PrepaidRef>,
    /// Why, on a credit note. ZATCA requires a reason on one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl Document {
    /// **What the whole supply came to before tax**, prepayment included:
    /// `cbc:TaxExclusiveAmount`.
    #[must_use]
    pub fn supply_net(&self) -> Money {
        match &self.prepaid {
            Some(prepaid) => self
                .totals
                .net
                .checked_add(prepaid.net(self.currency))
                .unwrap_or(self.totals.net),
            None => self.totals.net,
        }
    }

    /// The whole supply, tax included: `cbc:TaxInclusiveAmount`, and what the
    /// QR code calls the invoice total.
    #[must_use]
    pub fn supply_gross(&self) -> Money {
        match &self.prepaid {
            Some(prepaid) => self
                .totals
                .gross
                .checked_add(prepaid.gross(self.currency))
                .unwrap_or(self.totals.gross),
            None => self.totals.gross,
        }
    }

    /// What the lines came to, before document discounts and prepayment:
    /// `cbc:LineExtensionAmount`, which ZATCA checks against the lines.
    #[must_use]
    pub fn lines_came_to(&self) -> Money {
        match &self.prepaid {
            Some(prepaid) => self
                .totals
                .lines_came_to()
                .checked_add(prepaid.net(self.currency))
                .unwrap_or_else(|_| self.totals.lines_came_to()),
            None => self.totals.lines_came_to(),
        }
    }
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

/// How the QR's timestamp is written.
///
/// **No `Z`**, though ZATCA's own QR specification shows one. Its validator
/// compares the value against `cbc:IssueDate` + `T` + `cbc:IssueTime`, which
/// carries no zone — so a `Z` produces
/// `invoiceTimeStamp_QRCODE_INVALID: Time on QR Code does not match with
/// Invoice Issue Time`. Confirmed against ZATCA; see
/// `modules/tax_sa/tests/sandbox.rs`.
pub const QR_TIME: &str = "%Y-%m-%dT%H:%M:%S";

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

/// **ZATCA's `VATEX-SA-*` exemption reason codes**, and the category each
/// belongs to.
///
/// # Why this is an enum here and a string everywhere else
///
/// The list is the authority's. `ledger` stores the tenant's chosen code and
/// `sales` stamps it on the line, and neither knows what any of them mean —
/// exactly as `crm::TaxRegistration.scheme` carries ZATCA's `schemeID` without
/// `crm` owning it. This is the country module, so this is where the list
/// lives and where an unknown code is refused.
///
/// # What this replaced, and why it was a defect and not a gap
///
/// Until 2026-09-09 the reason was derived from the *category alone*: every
/// exempt line in the system was declared to ZATCA as `VATEX-SA-29`,
/// **financial services**. For a financial services business that was right.
/// For a landlord letting residential property — `VATEX-SA-30`, real estate
/// transactions — it was a false statement to a tax authority on every
/// invoice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExemptionReason {
    /// Financial services, VAT Regulations Article 29.
    Sa29,
    /// Life insurance services, Article 29.
    Sa29_7,
    /// **Real estate transactions, Article 30.** Residential rent.
    Sa30,
    Sa32,
    Sa33,
    Sa34_1,
    Sa34_2,
    Sa34_3,
    Sa34_4,
    Sa34_5,
    Sa35,
    Sa36,
    /// Private education supplied to a citizen.
    SaEdu,
    /// Private healthcare supplied to a citizen.
    SaHea,
    SaMltry,
    SaDiplomat,
}

impl ExemptionReason {
    pub const ALL: [Self; 16] = [
        Self::Sa29,
        Self::Sa29_7,
        Self::Sa30,
        Self::Sa32,
        Self::Sa33,
        Self::Sa34_1,
        Self::Sa34_2,
        Self::Sa34_3,
        Self::Sa34_4,
        Self::Sa34_5,
        Self::Sa35,
        Self::Sa36,
        Self::SaEdu,
        Self::SaHea,
        Self::SaMltry,
        Self::SaDiplomat,
    ];

    /// The code as it goes into `cbc:TaxExemptionReasonCode`.
    ///
    /// **No vendor prefix.** Some tooling documents these as
    /// `VRBL:SA:VATEX-SA-30`; that namespace is the tool's and is not in the
    /// XML ZATCA receives.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sa29 => "VATEX-SA-29",
            Self::Sa29_7 => "VATEX-SA-29-7",
            Self::Sa30 => "VATEX-SA-30",
            Self::Sa32 => "VATEX-SA-32",
            Self::Sa33 => "VATEX-SA-33",
            Self::Sa34_1 => "VATEX-SA-34-1",
            Self::Sa34_2 => "VATEX-SA-34-2",
            Self::Sa34_3 => "VATEX-SA-34-3",
            Self::Sa34_4 => "VATEX-SA-34-4",
            Self::Sa34_5 => "VATEX-SA-34-5",
            Self::Sa35 => "VATEX-SA-35",
            Self::Sa36 => "VATEX-SA-36",
            Self::SaEdu => "VATEX-SA-EDU",
            Self::SaHea => "VATEX-SA-HEA",
            Self::SaMltry => "VATEX-SA-MLTRY",
            Self::SaDiplomat => "VATEX-SA-DIPLOMAT",
        }
    }

    /// What goes in `cbc:TaxExemptionReason` beside the code — the article, in
    /// the authority's own words.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Sa29 => "Financial services mentioned in Article 29 of the VAT Regulations",
            Self::Sa29_7 => {
                "Life insurance services mentioned in Article 29 of the VAT Regulations"
            }
            Self::Sa30 => "Real estate transactions mentioned in Article 30 of the VAT Regulations",
            Self::Sa32 => "Export of goods",
            Self::Sa33 => "Export of services",
            Self::Sa34_1 => "International transport of goods",
            Self::Sa34_2 => "International transport of passengers",
            Self::Sa34_3 => "Services connected to international transport of passengers",
            Self::Sa34_4 => "Supply of a qualifying means of transport",
            Self::Sa34_5 => "Services relating to the transport of goods or passengers",
            Self::Sa35 => "Medicines and medical equipment",
            Self::Sa36 => "Qualifying metals",
            Self::SaEdu => "Private education to a citizen",
            Self::SaHea => "Private healthcare to a citizen",
            Self::SaMltry => "Supply of qualified military goods",
            Self::SaDiplomat => "Diplomatic",
        }
    }

    /// **Which tax category this code may be used with.**
    ///
    /// A code on the wrong category is refused rather than corrected: an
    /// exporter who set the exempt reason instead of the zero-rated one has a
    /// misconfiguration, and silently moving their supply between categories
    /// would change what they owe.
    #[must_use]
    pub const fn category(self) -> VatCategory {
        match self {
            Self::Sa29 | Self::Sa29_7 | Self::Sa30 => VatCategory::Exempt,
            _ => VatCategory::Zero,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a ZATCA exemption reason code")]
pub struct UnknownExemptionReason(pub String);

impl std::str::FromStr for ExemptionReason {
    type Err = UnknownExemptionReason;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| UnknownExemptionReason(s.to_owned()))
    }
}

/// Why a band carries no tax, as the code and the article ZATCA prints.
///
/// **Taken from what the document recorded**, never from the category: the
/// category says *that* there is no tax and the code says *why*, and only the
/// tenant knows which article covers their business.
#[must_use]
pub fn exemption_reason(stamped: Option<&str>) -> Option<(&'static str, &'static str)> {
    let reason: ExemptionReason = stamped?.parse().ok()?;
    Some((reason.as_str(), reason.describe()))
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

    /// **In generic UBL a credit note is a different document type; to ZATCA it
    /// is an `<Invoice>` with a different code.** Sending the UBL `CreditNote`
    /// root is rejected by the gateway before validation, with nothing said
    /// about why — so this is pinned rather than left to read naturally.
    #[test]
    fn every_document_type_is_an_invoice_element_whatever_its_code_says() {
        assert_eq!(TypeCode::Invoice.code(), 388);
        assert_eq!(TypeCode::CreditNote.code(), 381);
        assert_eq!(TypeCode::DebitNote.code(), 383);
        for type_code in [TypeCode::Invoice, TypeCode::CreditNote, TypeCode::DebitNote] {
            assert_eq!(type_code.element(), "Invoice", "{type_code:?}");
        }
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
    fn every_category_has_a_code() {
        assert_eq!(category_code(VatCategory::Standard), "S");
        assert_eq!(category_code(VatCategory::Zero), "Z");
        assert_eq!(category_code(VatCategory::Exempt), "E");
    }

    /// **The defect this replaced.** The reason used to be derived from the
    /// category, so every exempt line in the system said `VATEX-SA-29` —
    /// financial services — including a landlord's rent.
    #[test]
    fn a_reason_comes_from_what_was_stamped_and_never_from_the_category() {
        assert_eq!(
            exemption_reason(Some("VATEX-SA-30")),
            Some((
                "VATEX-SA-30",
                "Real estate transactions mentioned in Article 30 of the VAT Regulations"
            )),
            "residential rent is article 30, not article 29"
        );
        assert_eq!(
            exemption_reason(Some("VATEX-SA-32")),
            Some(("VATEX-SA-32", "Export of goods"))
        );
    }

    #[test]
    fn a_document_that_stamped_no_reason_states_none() {
        // Better than a wrong one: omitting is silence, and `VATEX-SA-29` on a
        // landlord's invoice is a false statement to a tax authority.
        assert!(exemption_reason(None).is_none());
    }

    #[test]
    fn an_unknown_code_is_not_passed_through_to_the_authority() {
        assert!(exemption_reason(Some("VATEX-SA-999")).is_none());
        assert!(exemption_reason(Some("")).is_none());
    }

    #[test]
    fn every_code_round_trips_and_names_its_category() {
        for reason in ExemptionReason::ALL {
            assert_eq!(
                reason.as_str().parse::<ExemptionReason>().ok(),
                Some(reason)
            );
            assert!(
                reason.as_str().starts_with("VATEX-SA-"),
                "{} is not a ZATCA code",
                reason.as_str()
            );
            assert!(!reason.describe().is_empty());
            assert_ne!(
                reason.category(),
                VatCategory::Standard,
                "a standard-rated line has nothing to explain"
            );
        }
    }

    /// The three articles that make a supply exempt rather than zero-rated.
    #[test]
    fn only_the_article_29_and_30_codes_are_exempt() {
        use ExemptionReason as R;
        for reason in R::ALL {
            let expected = matches!(reason, R::Sa29 | R::Sa29_7 | R::Sa30);
            assert_eq!(
                reason.category() == VatCategory::Exempt,
                expected,
                "{} is on the wrong category",
                reason.as_str()
            );
        }
    }
}
