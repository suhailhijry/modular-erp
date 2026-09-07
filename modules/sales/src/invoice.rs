//! The invoice, as an aggregate.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{
    AggregateId, CurrencyCode, DomainName, EventName, Money, SchemaVersion, Timestamp,
};
use serde::{Deserialize, Serialize};

use crate::vat::{TaxBand, Totals, Vat};

/// Who the invoice is addressed to, **as it was at the time**.
///
/// A snapshot, not a reference. A tax invoice is a legal document: changing a
/// customer's registered name next year must not rewrite what was issued this
/// year, and a foreign key would do exactly that. This is architecture L5
/// applied to the most visible place it matters.
///
/// # The reference and the copy, both
///
/// [`Customer::id`] points at a `crm` record and the rest of this struct is
/// what was printed. Both, never either. The reference is what makes "every
/// invoice for this customer" answerable when they are spelled two ways; the
/// copy is what the law requires the document to say, and it does not move when
/// the record does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Customer {
    /// The `crm` record this was issued to, when there is one.
    ///
    /// **Optional, and it stays optional.** Every invoice issued before this
    /// field existed has none, a walk-in at a till has none, and making it
    /// required would mean a backfill that has to invent a customer for every
    /// historic document. It is a reconciliation surface and not a foreign key.
    ///
    /// `#[serde(default)]`, so those older events still decode, which is why
    /// this needs no upcaster — the same argument as `address` below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<erp_types::AggregateId>,
    pub name: String,
    /// The buyer's VAT registration number. Required by ZATCA on a B2B invoice
    /// and absent on a simplified one, which is why it is optional here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vat_number: Option<String>,
    /// Where they are, as they were at the time.
    ///
    /// `#[serde(default)]`, so every invoice issued before this field existed
    /// still decodes — an absent address is exactly what those invoices had,
    /// which is why this needs no upcaster.
    ///
    /// Boxed because `InvoiceEvent::Issued` is the largest variant in the enum
    /// and an address is six strings; a `Box` is one pointer and serialises
    /// identically, so nothing on the wire or in the log changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Box<Address>>,
}

/// Where a customer is.
///
/// # Why this is on the invoice and not on a customer record
///
/// The same reason the name is: a tax invoice is a legal document, and the
/// address on last year's copy must not change because the customer moved.
///
/// # Why it exists at all
///
/// ZATCA wants a buyer address on a **standard** invoice — street, city and
/// country at minimum (BT-50, BT-52, BT-55). Without one it accepts the
/// document and warns, which is a warning that becomes a finding at an
/// inspection. Confirmed against ZATCA; see `modules/tax_sa/tests/sandbox.rs`.
///
/// Optional, because a consumer at a till gives no address and a simplified
/// invoice needs none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub street: String,
    pub city: String,
    /// ISO 3166-1 alpha-2. `SA` for a Saudi buyer.
    pub country: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub district: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub building: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postal_code: Option<String>,
}

impl Customer {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: None,
            name: name.into(),
            vat_number: None,
            address: None,
        }
    }

    /// Points this at a `crm` record.
    ///
    /// Does not fill anything in from it. What the document says is the
    /// caller's, because it is what will be printed and cleared.
    #[must_use]
    pub fn of(mut self, id: erp_types::AggregateId) -> Self {
        self.id = Some(id);
        self
    }

    #[must_use]
    pub fn at(mut self, address: Address) -> Self {
        self.address = Some(Box::new(address));
        self
    }

    #[must_use]
    pub fn with_vat_number(mut self, number: impl Into<String>) -> Self {
        let number = number.into();
        self.vat_number = (!number.trim().is_empty()).then_some(number);
        self
    }
}

/// One thing being charged for.
///
/// ponytail: no quantity or unit price. A client that shows "3 × 250.00" already
/// computed the 750.00 it sends; storing the factors matters when ZATCA's
/// line-level fields are implemented, and adding them then is an upcaster — the
/// mechanism this system already has and tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvoiceLine {
    pub description: String,
    /// **What this line is charged, excluding tax and after its own
    /// allowances.** UBL's `LineExtensionAmount`, BT-131 — which the standard
    /// defines as the price less the line's allowances, and which is what the
    /// tax is worked out on.
    ///
    /// Negative is allowed: a discount is a line.
    pub net: Money,
    /// The treatment **and the rate that applied when it was issued**. Written
    /// once and never recomputed, so a rate change cannot restate a filed
    /// return (architecture L5).
    pub vat: Vat,
    /// **What was taken off this line**, each printed as its own figure.
    ///
    /// A line's allowance carries no tax treatment of its own: it reduces the
    /// line, and the line already says how it is taxed. That is the difference
    /// from [`Discount`], which is taken off the *document* and therefore has
    /// to name which treatment it comes off — and it is why UBL puts a
    /// `cac:TaxCategory` on one and not the other.
    ///
    /// `#[serde(default)]`, so every line written before this existed decodes
    /// as one with none, which is what it was.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowances: Vec<Allowance>,
}

impl InvoiceLine {
    /// What the line came to **before** its own allowances — UBL's item net
    /// price, BT-146, and the base the allowance is taken from.
    ///
    /// Derived rather than stored, so the two cannot disagree.
    pub fn before_allowances(&self) -> Result<Money, crate::vat::TaxError> {
        self.allowances
            .iter()
            .try_fold(self.net, |running, a| running.checked_add(a.amount))
            .map_err(Into::into)
    }
}

/// One line of a credit note, and the invoice line it comes off.
///
/// **The reference is the point.** A credit note that only said "500 of
/// standard-rated" described nothing: the description was retyped by the caller
/// and could say anything, and nothing tied the document to what was actually
/// returned. Naming the line takes the wording and the treatment from the
/// invoice, so a credit note can only ever describe something the invoice
/// charged for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditedLine {
    /// Which line of the invoice, by position.
    pub against: u16,
    /// The line as this credit note states it — the invoice line's own
    /// description and treatment, at the amount being credited.
    #[serde(flatten)]
    pub line: InvoiceLine,
}

/// Something taken off **one line**.
///
/// # Why this has no tax treatment and [`Discount`] does
///
/// Because the line already has one. An allowance on a line reduces that line,
/// so what it comes off at is settled; a discount on the whole document is not
/// attached to anything, so it has to say which treatment it reduces or the
/// taxable amounts do not add up.
///
/// UBL models the difference the same way: `cac:AllowanceCharge` inside
/// `cac:InvoiceLine` takes an amount and a reason and **no `cac:TaxCategory`**,
/// while the document-level one requires the category and the rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allowance {
    /// Why. A customer reads it, so it is text rather than a code.
    pub reason: String,
    /// What comes off, **positive**. A negative allowance is a surcharge, which
    /// is a different element and a different conversation.
    pub amount: Money,
}

/// A line as a client sends it: what is being charged for, and how it is
/// treated. **Not what it is taxed at** — that is the tenant's configured rate,
/// resolved in the command's own transaction, because a rate that changed
/// between the request and the write would stamp an invoice with one that was
/// never current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftLine {
    pub description: String,
    /// **Before this line's own allowances.** The list amount; what is charged
    /// is this less [`Self::allowances`], and that is what gets taxed.
    pub net: Money,
    pub category: ledger::VatCategory,
    /// What comes off this line, each with its own reason.
    #[allow(clippy::struct_field_names, reason = "it is what it is called")]
    pub allowances: Vec<Allowance>,
}

/// Something taken off the whole invoice, rather than off one line.
///
/// # Why this is not just a negative line
///
/// A negative line is what this system had, and it is invisible on the
/// document: the invoice shows a smaller total and nothing says why. ZATCA
/// models a discount as `cac:AllowanceCharge` — an amount, a reason, and the
/// tax treatment it comes off — and prints it as its own figure, so a customer
/// sees what they were charged and what they were let off.
///
/// **The tax treatment is part of it.** Discounting a standard-rated invoice
/// reduces the tax; discounting an exempt one does not, because there was none.
/// Taking a discount off the wrong band would reclaim tax that was never
/// charged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discount {
    /// Why. ZATCA prints it, and "discount" is a reason a customer can read.
    pub reason: String,
    /// What comes off, **positive**. A negative discount is a charge, which is
    /// a different element and a different conversation.
    pub amount: Money,
    /// The treatment **and the rate that applied when it was issued**, for the
    /// same reason a line carries one.
    pub vat: Vat,
}

/// A discount as a client sends it: what and why, but not at what rate — that
/// is the tenant's configuration, resolved in the command's own transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftDiscount {
    pub reason: String,
    pub amount: Money,
    pub category: ledger::VatCategory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InvoiceEvent {
    Issued {
        /// The invoice number, from the tenant's gapless series.
        ///
        /// In the event rather than derived on read, because that is the whole
        /// point: a replay must reproduce the number the document was issued
        /// under, not the one today's counter would give (architecture L5).
        ///
        /// `None` on invoices issued before this system numbered them, whose
        /// number *was* their client-chosen id. Not an upcaster: an upcaster
        /// sees the payload and not the stream it came from, so there is
        /// nowhere for the old number to come from — and `None` is the honest
        /// statement that nothing allocated one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        number: Option<String>,
        /// Boxed for the reason [`Customer::address`] is, and it is now the
        /// customer that carries the weight: a reference, a name, a VAT number
        /// and an address. `Box<T>` serialises as `T`, so nothing on the wire
        /// or in the log changes and no upcaster is needed.
        customer: Box<Customer>,
        /// The tax point — the date the supply is treated as made. Not when the
        /// row was written.
        issued_on: Timestamp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        due_on: Option<Timestamp>,
        currency: CurrencyCode,
        lines: Vec<InvoiceLine>,
        /// What was taken off the whole invoice.
        ///
        /// `#[serde(default)]`, so every invoice issued before discounts
        /// existed decodes as one with none — which is what it was, and why
        /// this needs no upcaster.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        discounts: Vec<Discount>,
        /// Computed once, at issue, and stored. Recomputing on read would let a
        /// rate change or a rounding fix silently restate a document somebody
        /// has already filed a return against.
        totals: Totals,
        /// **Whether this bills for money taken before the supply.** A deposit.
        ///
        /// It changes what the document *is* to the authority — a prepayment
        /// invoice rather than an ordinary one — because receiving
        /// consideration is itself a tax point and the two are reported
        /// differently. Nothing else about it differs: same series, same
        /// clearance, same bands.
        ///
        /// `#[serde(default)]`, so every invoice issued before deposits existed
        /// decodes as the ordinary one it was, and no upcaster is needed.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        prepayment: bool,
        /// **What a prepayment invoice already billed for this supply**, and
        /// what `totals` therefore leaves out. The lines are the whole supply;
        /// the totals are what this document charges and declares — the rest.
        /// `None` on every invoice that is not the final one after a deposit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prepaid: Option<crate::vat::Prepaid>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
    },
    /// Cancelled by a credit note, which reversed its journal entry.
    ///
    /// Not a deletion: the invoice was issued, somebody may hold a copy, and
    /// the books show both it and the credit. What changes is that nothing is
    /// owed on it.
    Cancelled {
        /// The credit note's number, from the tenant's gapless series — and on
        /// events written before that existed, the client's own identifier,
        /// which is what it meant then.
        credit_note: String,
        /// The client's key for this cancellation, which is what makes a
        /// retried request a no-op and a second, different one a conflict.
        ///
        /// Separate from `credit_note` since the number stopped being the
        /// client's to choose. `None` on older events, where they were the same
        /// thing — see [`Invoice::cancelled_by`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
        reason: String,
        on: Timestamp,
    },
    /// **Part of the invoice credited, as a document with lines of its own.**
    ///
    /// The difference from [`Self::Cancelled`] is not the size. A cancellation
    /// says the supply is undone and reverses the journal entry the invoice
    /// posted; this says *some* of it is undone and posts its own entry for
    /// what it takes back. So it carries lines, allowances and totals the way
    /// [`Self::Issued`] does — because a credit note is a document in its own
    /// right, with its own number, its own tax point and its own place in the
    /// ZATCA chain, and the authority computes its bands from its own lines.
    ///
    /// **The rates are the invoice's, never today's.** A 2019 invoice is
    /// credited at 5% for ever; resolving the current rate here would restate a
    /// return somebody filed six years ago (L5). The command reads them off the
    /// invoice's own bands, which is also what makes crediting a category the
    /// invoice never had impossible rather than merely discouraged.
    ///
    /// An invoice may have several of these. It may not have one *and* a
    /// [`Self::Cancelled`] — see `Invoice::credited`.
    Credited {
        /// The credit note's number, from the tenant's gapless series. The same
        /// series [`Self::Cancelled`] draws on: both are credit notes, and the
        /// authority does not care which shape produced one.
        credit_note: String,
        /// The client's key for this credit, which is what makes a retried
        /// request a no-op.
        reference: String,
        /// Each line, and **which line of the invoice it credits**.
        lines: Vec<CreditedLine>,
        /// Computed once, here, and stored — the same argument
        /// [`Self::Issued`] makes about its own.
        totals: Totals,
        reason: String,
        on: Timestamp,
    },
    PaymentRecorded {
        /// The payer's or the client's own reference. Recording it twice is a
        /// no-op, which is what makes a retried request safe.
        payment: String,
        amount: Money,
        received_on: Timestamp,
        /// Which cash or bank account took it. Chosen per payment, because a
        /// business with two banks needs to say which one.
        account: AggregateId,
    },
    /// **Money handed back.** The mirror of a payment, and the thing this module
    /// had no concept of until a till needed to take a return.
    ///
    /// Separate from `Cancelled`, because they are separate facts: a credit note
    /// says the supply is undone, and this says the cash left. A shop can do the
    /// first without the second — crediting an unpaid invoice — and must do both
    /// for a paid one, or it is holding money it no longer has a sale for.
    Refunded {
        /// The caller's own reference. Refunding it twice is a no-op.
        refund: String,
        amount: Money,
        refunded_on: Timestamp,
        /// Which cash or bank account it came out of.
        account: AggregateId,
    },
    /// **A `crm` record was matched to this invoice after the fact.**
    ///
    /// The reconciliation surface Phase 7a asked for. Invoices issued before
    /// `crm` existed name a buyer that no record matches, and a foreign key
    /// would have refused every one of them; this attaches the reference
    /// afterwards, one invoice at a time, as somebody works through the list.
    ///
    /// **It does not touch what the document printed.** The frozen `customer`
    /// on `Issued` is what the law requires the invoice to say and it never
    /// moves (L5). This is the *reference* — the thing that makes "everything
    /// for this customer" answerable — and the two were always meant to be
    /// separate, which is the whole argument in the `crm` chapter.
    ///
    /// Re-attaching to a different record is allowed and is itself an event: a
    /// match made to the wrong Ahmed has to be correctable, and the log keeps
    /// both so the correction is visible rather than silent.
    CustomerAttached {
        customer: AggregateId,
        at: Timestamp,
    },
}

impl InvoiceEvent {
    pub const NAMES: [&'static str; 6] = [
        "sales.invoice.issued",
        "sales.invoice.payment_recorded",
        "sales.invoice.cancelled",
        "sales.invoice.credited",
        "sales.invoice.refunded",
        "sales.invoice.customer_attached",
    ];
}

impl DomainEvent for InvoiceEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Issued { .. } => Self::NAMES[0],
            Self::PaymentRecorded { .. } => Self::NAMES[1],
            Self::Cancelled { .. } => Self::NAMES[2],
            Self::Credited { .. } => Self::NAMES[3],
            Self::Refunded { .. } => Self::NAMES[4],
            Self::CustomerAttached { .. } => Self::NAMES[5],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// What a command needs to know about an invoice before deciding.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Invoice {
    pub issued: bool,
    /// The statutory number this invoice was issued under. `None` before it is
    /// issued, and on invoices from before the system numbered them — where the
    /// aggregate id was the number.
    pub number: Option<String>,
    pub currency: Option<CurrencyCode>,
    pub gross: Option<Money>,
    /// Total received so far. `None` until the invoice is issued.
    pub paid: Option<Money>,
    /// Cancelled, and under which client key — the client's `reference`, or on
    /// an older event the credit note's identifier, which was the same thing.
    /// Compared against on a retry.
    /// Whether it billed for money taken before the supply.
    pub prepayment: bool,
    pub cancelled_by: Option<String>,
    /// The credit note's number, for reporting it back to a caller who asked to
    /// cancel an invoice that was already cancelled.
    pub credit_note: Option<String>,
    /// **The lines this invoice was issued with**, which is what a credit note
    /// names. Kept on the aggregate rather than looked up, because what a line
    /// said and what it was charged at are facts about *this* invoice and there
    /// is nowhere else that still knows them.
    pub lines: Vec<InvoiceLine>,
    /// What has been credited against each line so far, cumulatively, indexed
    /// alongside [`Self::lines`].
    pub credited_lines: Vec<Money>,
    /// **The bands this invoice was issued under.**
    ///
    /// Kept as well as the lines, and not derivable from them: a document
    /// discount comes off the *band*, so the lines sum to more than the bands
    /// on any invoice that carried one. Both caps are needed and they catch
    /// different things — see `crate::commands::credit_part_in`.
    pub bands: Vec<TaxBand>,
    /// What has been credited so far, per band, cumulatively.
    ///
    /// **Per band and not one total**, because that is the check that protects
    /// the tax: crediting 100 of standard-rated against an invoice of 50
    /// standard and 50 zero-rated reclaims VAT that was never charged, and a
    /// gross total of 100 against 100 would not notice.
    pub credited: Vec<TaxBand>,
    /// Credit notes already issued against part of this invoice, as
    /// `(the caller's reference, the number it got)`.
    ///
    /// **Both halves, because a retry needs the number it was given the first
    /// time.** An invoice may have several, so "the credit note" is not a
    /// question with one answer here — which is also why `credit_note` above
    /// stays the *cancellation's* and is not touched by these.
    pub credits: Vec<(String, String)>,
    /// Payment references already recorded. Small — an invoice is settled in a
    /// handful of instalments at most — and the only way to make recording a
    /// payment idempotent without a separate table.
    pub payments: Vec<String>,
    /// Total handed back. `None` until the invoice is issued.
    pub refunded: Option<Money>,
    /// Refund references already recorded, for the reason `payments` is a list.
    pub refunds: Vec<String>,
    /// The `crm` record this invoice points at, if one has been matched to it.
    ///
    /// Set at issue when the caller knew it, or afterwards by
    /// [`InvoiceEvent::CustomerAttached`]. Never the frozen name — that is on
    /// the `Issued` event and does not move.
    pub customer: Option<AggregateId>,
}

impl Aggregate for Invoice {
    type Event = InvoiceEvent;

    fn domain() -> DomainName {
        crate::domain("sales_invoice")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            InvoiceEvent::Issued {
                number,
                customer,
                currency,
                totals,
                lines,
                prepayment,
                ..
            } => {
                self.issued = true;
                self.number.clone_from(number);
                self.customer.clone_from(&customer.id);
                self.currency = Some(*currency);
                self.gross = Some(totals.gross);
                self.paid = Some(Money::zero(*currency));
                self.refunded = Some(Money::zero(*currency));
                self.bands.clone_from(&totals.bands);
                self.lines.clone_from(lines);
                self.credited_lines = lines
                    .iter()
                    .map(|line| Money::zero(line.net.currency()))
                    .collect();
                self.prepayment = *prepayment;
            }
            InvoiceEvent::Credited {
                credit_note,
                reference,
                lines,
                totals,
                ..
            } => {
                self.credits.push((reference.clone(), credit_note.clone()));
                for line in lines {
                    if let Some(seen) = self.credited_lines.get_mut(line.against as usize) {
                        // Saturating for the reason `paid` is: `apply` cannot
                        // fail, and the command refused anything that would not
                        // fit before this ran.
                        *seen = seen.checked_add(line.line.net).unwrap_or(*seen);
                    }
                }
                for band in &totals.bands {
                    match self.credited.iter_mut().find(|b| {
                        b.category == band.category && b.basis_points == band.basis_points
                    }) {
                        // Saturating for the reason `paid` is: `apply` cannot
                        // fail, and the command refused anything that would not
                        // fit before this ran.
                        Some(seen) => {
                            seen.net = seen.net.checked_add(band.net).unwrap_or(seen.net);
                            seen.tax = seen.tax.checked_add(band.tax).unwrap_or(seen.tax);
                        }
                        None => self.credited.push(*band),
                    }
                }
            }
            InvoiceEvent::Cancelled {
                credit_note,
                reference,
                ..
            } => {
                self.cancelled_by = Some(reference.clone().unwrap_or_else(|| credit_note.clone()));
                self.credit_note = Some(credit_note.clone());
            }
            InvoiceEvent::Refunded { refund, amount, .. } => {
                self.refunds.push(refund.clone());
                self.refunded = match self.refunded {
                    Some(refunded) => refunded.checked_add(*amount).ok(),
                    None => None,
                };
            }
            InvoiceEvent::CustomerAttached { customer, .. } => {
                self.customer = Some(customer.clone());
            }
            InvoiceEvent::PaymentRecorded {
                payment, amount, ..
            } => {
                self.payments.push(payment.clone());
                // Saturating rather than checked: `apply` cannot fail, and the
                // command already refused anything that would not fit. A total
                // that overflows here would mean the log itself is corrupt,
                // which the outstanding-amount check then catches.
                self.paid = match self.paid {
                    Some(paid) => paid.checked_add(*amount).ok(),
                    None => None,
                };
            }
        }
    }
}

impl Invoice {
    /// The number a credit note under this reference was issued as, if one was.
    ///
    /// **What a retry is answered with.** Reporting the most recent credit
    /// note would be wrong the moment an invoice has two, which is the whole
    /// point of partial ones.
    #[must_use]
    pub fn credit_note_for(&self, reference: &str) -> Option<&str> {
        self.credits
            .iter()
            .find(|(seen, _)| seen == reference)
            .map(|(_, number)| number.as_str())
    }

    /// Whether this credit note has already been issued.
    #[must_use]
    pub fn has_credit(&self, reference: &str) -> bool {
        self.credit_note_for(reference).is_some()
    }

    /// Whether any part of this invoice has been credited.
    #[must_use]
    pub fn is_partly_credited(&self) -> bool {
        !self.credits.is_empty()
    }

    /// The rate this invoice charged a category at, if it charged one at all.
    ///
    /// **The band check, as a lookup.** A category with no band was never on
    /// this invoice, so there is no rate to credit it at and no tax to reclaim
    /// — which is why this returning `None` is a refusal and not a default.
    #[must_use]
    pub fn rate_for(&self, category: ledger::VatCategory) -> Option<i32> {
        self.bands
            .iter()
            .find(|b| b.category == category)
            .map(|b| b.basis_points)
    }

    /// What is left to credit in one band. `None` when the invoice had no such
    /// band at that rate.
    #[must_use]
    pub fn creditable_in(&self, category: ledger::VatCategory, basis_points: i32) -> Option<Money> {
        let issued = self
            .bands
            .iter()
            .find(|b| b.category == category && b.basis_points == basis_points)?;
        let credited = self
            .credited
            .iter()
            .find(|b| b.category == category && b.basis_points == basis_points)
            .map_or_else(|| Money::zero(issued.net.currency()), |b| b.net);
        issued.net.checked_sub(credited).ok()
    }

    /// What is still owed. `None` before the invoice exists.
    #[must_use]
    pub fn outstanding(&self) -> Option<Money> {
        self.gross?.checked_sub(self.paid?).ok()
    }

    /// Whether a credit note has cancelled this invoice.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        self.cancelled_by.is_some()
    }

    /// Whether this payment reference has already been recorded.
    #[must_use]
    pub fn has_payment(&self, reference: &str) -> bool {
        self.payments.iter().any(|p| p == reference)
    }

    #[must_use]
    pub fn has_refund(&self, reference: &str) -> bool {
        self.refunds.iter().any(|r| r == reference)
    }

    /// Whether this invoice already points at this exact record.
    ///
    /// The retry check for attaching: the same match twice writes nothing, and
    /// a *different* one is a correction that does write.
    #[must_use]
    pub fn points_at(&self, customer: &AggregateId) -> bool {
        self.customer.as_ref() == Some(customer)
    }

    /// **What the business is still holding of the customer's money.**
    ///
    /// Paid less refunded. This is what decides whether an invoice may be
    /// credited: a credit note says the supply is undone, and undoing a supply
    /// while keeping the cash is not a credit note, it is a debt.
    #[must_use]
    pub fn held(&self) -> Option<Money> {
        self.paid?.checked_sub(self.refunded?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vat::{VatCategory, total};

    fn sar() -> CurrencyCode {
        CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!())
    }

    fn issued(gross_net: i64) -> InvoiceEvent {
        let currency = sar();
        let vat = Vat::shipped(VatCategory::Standard);
        let net = Money::from_minor(gross_net, currency);
        InvoiceEvent::Issued {
            prepayment: false,
            prepaid: None,
            number: Some("INV-00001".to_owned()),
            customer: Box::new(Customer::new("Acme")),
            issued_on: Timestamp::UNIX_EPOCH,
            due_on: None,
            currency,
            lines: vec![InvoiceLine {
                allowances: Vec::new(),
                description: "Consulting".to_owned(),
                net,
                vat,
            }],
            discounts: Vec::new(),
            totals: total([(vat, net)], [], currency).unwrap_or_else(|_| unreachable!()),
            note: String::new(),
        }
    }

    #[test]
    fn a_payment_reduces_what_is_outstanding() {
        let mut invoice = Invoice::default();
        // 100.00 net, 15.00 tax, 115.00 gross.
        invoice.apply(&issued(10_000));
        assert_eq!(
            invoice.outstanding(),
            Some(Money::from_minor(11_500, sar()))
        );

        invoice.apply(&InvoiceEvent::PaymentRecorded {
            payment: "wire-1".to_owned(),
            amount: Money::from_minor(5_000, sar()),
            received_on: Timestamp::UNIX_EPOCH,
            account: AggregateId::new("1010").unwrap_or_else(|_| unreachable!()),
        });

        assert_eq!(invoice.outstanding(), Some(Money::from_minor(6_500, sar())));
        assert!(invoice.has_payment("wire-1"));
        assert!(!invoice.has_payment("wire-2"));
    }

    #[test]
    fn an_unissued_invoice_owes_nothing_rather_than_zero() {
        // `None`, not `Some(0)` — the difference between "not there" and
        // "settled", which is what stops a payment landing on a blank id.
        let invoice = Invoice::default();
        assert_eq!(invoice.outstanding(), None);
        assert!(!invoice.issued);
    }
}
