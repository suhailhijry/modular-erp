//! A till, from the moment it is opened to the moment the drawer is counted.
//!
//! # What this aggregate is for, and what it is not
//!
//! It is **not** a second invoice. A till transaction is a ZATCA simplified
//! invoice and `sales` already builds one; see the crate docs for why writing a
//! second document model here would be the expensive mistake.
//!
//! What a till has that an invoice does not is a **drawer**. Money physically
//! arrives, in a mix of tenders, into a box that somebody counts at the end of
//! the day — and the number that box is short by is the only number in this
//! module a manager actually reads. That is what this aggregate holds.
//!
//! # Why the sales are on the shift and not somewhere else
//!
//! Because the expected drawer is a running total, and a running total computed
//! from a projection is a number that can be a second behind while somebody is
//! counting against it. `Shift::expected` is answered from aggregate state for
//! the reason `Subscription::admits` is: it is an input to a decision taken at
//! the counter.
//!
//! A shift is bounded by a working day, so the stream is bounded too — forty
//! coffees is forty events, which is a cheap load. A till that never closes is a
//! till nobody is reconciling, and the fix for that is a manager, not a snapshot.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{
    AggregateId, CurrencyCode, DomainName, EventName, Money, MoneyError, SchemaVersion, Timestamp,
};
use serde::{Deserialize, Serialize};

/// How money arrived.
///
/// **Only cash is in the drawer**, and that is the whole reason this enum
/// exists: the expected count at close is the float plus cash taken less cash
/// refunded, and a card sale must not move it. Everything else is recorded so
/// the takings add up to the day, and settles elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// Notes and coins. The only one that changes what the drawer should hold.
    Cash,
    /// Mada, or a card scheme. Settles to the bank, not to the box.
    Card,
    /// A bank transfer taken at the counter.
    Transfer,
}

impl Method {
    /// Whether taking this moves the drawer.
    #[must_use]
    pub const fn is_in_the_drawer(self) -> bool {
        matches!(self, Self::Cash)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cash => "cash",
            Self::Card => "card",
            Self::Transfer => "transfer",
        }
    }

    pub const ALL: [Self; 3] = [Self::Cash, Self::Card, Self::Transfer];
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a way money arrives")]
pub struct UnknownMethod(pub String);

impl std::str::FromStr for Method {
    type Err = UnknownMethod;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|method| method.as_str() == s)
            .ok_or_else(|| UnknownMethod(s.to_owned()))
    }
}

/// One way one sale was paid for. A split payment is several of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tender {
    pub method: Method,
    pub amount: Money,
}

impl Tender {
    #[must_use]
    pub const fn new(method: Method, amount: Money) -> Self {
        Self { method, amount }
    }

    /// What this tender puts in the drawer, which is nothing unless it is cash.
    #[must_use]
    pub fn in_the_drawer(&self) -> Option<Money> {
        self.method.is_in_the_drawer().then_some(self.amount)
    }
}

/// What was taken, by tender, over a shift.
///
/// A total per method rather than a list, because a shift's takings are read as
/// "how much cash, how much card" and never as a sequence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Takings {
    entries: Vec<Tender>,
}

impl Takings {
    /// Adds a tender, netting it against what that method already holds.
    ///
    /// Saturating on a currency mismatch: `apply` cannot fail, and the command
    /// refused a mismatched tender before it got here.
    fn add(&mut self, tender: Tender) {
        if let Some(held) = self
            .entries
            .iter_mut()
            .find(|held| held.method == tender.method)
        {
            held.amount = held
                .amount
                .checked_add(tender.amount)
                .unwrap_or(held.amount);
        } else {
            self.entries.push(tender);
        }
    }

    /// What one method took, if it took anything.
    #[must_use]
    pub fn by(&self, method: Method) -> Option<Money> {
        self.entries
            .iter()
            .find(|tender| tender.method == method)
            .map(|tender| tender.amount)
    }

    /// Every method that took something, in the order it first did.
    #[must_use]
    pub fn each(&self) -> &[Tender] {
        &self.entries
    }

    /// **How much of this is physically in the box.**
    ///
    /// Asks [`Method::is_in_the_drawer`] rather than naming cash, so the rule
    /// lives in one place. Naming cash here is how the aggregate and the
    /// projection would come to disagree about what a drawer holds.
    pub fn in_the_drawer(&self, currency: CurrencyCode) -> Result<Money, MoneyError> {
        Money::checked_sum(
            self.entries.iter().filter_map(Tender::in_the_drawer),
            currency,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShiftEvent {
    Opened {
        /// Which counter this is. The business's own name for it.
        till: String,
        /// Who opened it. A `crm` record is the wrong shape — this is staff —
        /// so it is the identity from the session, recorded as an opaque id.
        operator: String,
        /// What was in the drawer before anything was sold.
        float: Money,
        at: Timestamp,
    },
    /// A sale rang up. **The document is `sales`'**: this records that the
    /// money arrived at this till, and what it arrived as.
    Sold {
        /// The invoice `sales` issued. Opaque here.
        sale: AggregateId,
        /// What the customer paid, which is the invoice's gross.
        total: Money,
        tenders: Vec<Tender>,
        at: Timestamp,
    },
    /// Money handed back. The credit note is `sales`'; this is the drawer half.
    Refunded {
        sale: AggregateId,
        total: Money,
        tenders: Vec<Tender>,
        why: String,
        at: Timestamp,
    },
    /// Cash out of the drawer that is not a refund — a float to another till, a
    /// supplier paid in cash, a banking run.
    PaidOut {
        /// The caller's key. Paying the same one out twice is a no-op, which is
        /// what makes a retried banking run harmless (L8).
        reference: String,
        why: String,
        amount: Money,
        at: Timestamp,
    },
    /// The drawer counted and the till shut. **`variance` is the number that
    /// gets read**, and it is stored rather than derived so a replay reproduces
    /// what was decided rather than what today's arithmetic would decide.
    Closed {
        /// What the drawer should have held.
        expected: Money,
        /// What was actually counted.
        declared: Money,
        /// `declared - expected`. Negative is short.
        variance: Money,
        at: Timestamp,
    },
}

impl DomainEvent for ShiftEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Opened { .. } => Self::NAMES[0],
            Self::Sold { .. } => Self::NAMES[1],
            Self::Refunded { .. } => Self::NAMES[2],
            Self::PaidOut { .. } => Self::NAMES[3],
            Self::Closed { .. } => Self::NAMES[4],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

impl ShiftEvent {
    pub const NAMES: [&'static str; 5] = [
        "pos.shift.opened",
        "pos.shift.sold",
        "pos.shift.refunded",
        "pos.shift.paid_out",
        "pos.shift.closed",
    ];
}

#[derive(Debug, Default, Clone)]
pub struct Shift {
    pub opened: bool,
    pub till: String,
    pub operator: String,
    pub float: Option<Money>,
    pub takings: Takings,
    pub refunds: Takings,
    /// Cash out that is not a refund.
    pub paid_out: Option<Money>,
    /// Set when it is shut. `None` while it is still taking money.
    pub closed_at: Option<Timestamp>,
    pub variance: Option<Money>,
    /// Sales rung up on this shift, in order. Small — a shift is a working day
    /// — and it is what makes ringing the same sale twice a no-op (L8).
    pub sales: Vec<AggregateId>,
    /// Pay-out keys already seen, for the same reason.
    pub pay_outs: Vec<String>,
}

impl Aggregate for Shift {
    type Event = ShiftEvent;

    fn domain() -> DomainName {
        crate::domain("pos_shift")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            ShiftEvent::Opened {
                till,
                operator,
                float,
                ..
            } => {
                self.opened = true;
                self.till.clone_from(till);
                self.operator.clone_from(operator);
                self.float = Some(*float);
            }
            ShiftEvent::Sold { sale, tenders, .. } => {
                self.sales.push(sale.clone());
                for tender in tenders {
                    self.takings.add(*tender);
                }
            }
            ShiftEvent::Refunded { tenders, .. } => {
                for tender in tenders {
                    self.refunds.add(*tender);
                }
            }
            ShiftEvent::PaidOut {
                reference, amount, ..
            } => {
                self.pay_outs.push(reference.clone());
                self.paid_out = Some(
                    self.paid_out
                        .map_or(*amount, |held| held.checked_add(*amount).unwrap_or(held)),
                );
            }
            ShiftEvent::Closed { at, variance, .. } => {
                self.closed_at = Some(*at);
                self.variance = Some(*variance);
            }
        }
    }
}

impl Shift {
    #[must_use]
    pub const fn exists(&self) -> bool {
        self.opened
    }

    /// Whether it can still take money.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.opened && self.closed_at.is_none()
    }

    #[must_use]
    pub fn has_sale(&self, sale: &AggregateId) -> bool {
        self.sales.iter().any(|seen| seen == sale)
    }

    #[must_use]
    pub fn has_pay_out(&self, reference: &str) -> bool {
        self.pay_outs.iter().any(|seen| seen == reference)
    }

    /// **What the drawer should hold**: the float, plus cash taken, less cash
    /// refunded, less cash paid out.
    ///
    /// Only cash. A card sale settles to the bank and moving the expected count
    /// by it would make every honest drawer look short by the day's card
    /// takings.
    pub fn expected(&self) -> Option<Result<Money, MoneyError>> {
        let float = self.float?;
        Some(self.expected_from(float))
    }

    fn expected_from(&self, float: Money) -> Result<Money, MoneyError> {
        let currency = float.currency();
        let mut running = float.checked_add(self.takings.in_the_drawer(currency)?)?;
        running = running.checked_sub(self.refunds.in_the_drawer(currency)?)?;
        if let Some(out) = self.paid_out {
            running = running.checked_sub(out)?;
        }
        Ok(running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sar(minor: i64) -> Money {
        Money::from_minor(
            minor,
            erp_types::CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!("a real code")),
        )
    }

    fn opened(float: i64) -> Shift {
        let mut shift = Shift::default();
        shift.apply(&ShiftEvent::Opened {
            till: "١".to_owned(),
            operator: "staff-1".to_owned(),
            float: sar(float),
            at: chrono::Utc::now(),
        });
        shift
    }

    fn sold(shift: &mut Shift, n: u32, tenders: Vec<Tender>) {
        shift.apply(&ShiftEvent::Sold {
            sale: AggregateId::new(format!("S-{n}")).unwrap_or_else(|_| unreachable!("a literal")),
            total: tenders
                .iter()
                .fold(sar(0), |t, x| t.checked_add(x.amount).unwrap_or(t)),
            tenders,
            at: chrono::Utc::now(),
        });
    }

    /// **A card sale does not move the drawer.**
    ///
    /// The failure this refuses is the one that makes the whole feature
    /// useless: counting card takings into the expected drawer makes every
    /// honest till look short by exactly the day's card sales, and a manager
    /// who sees that twice stops reading the number.
    #[test]
    fn only_cash_changes_what_the_drawer_should_hold() {
        let mut shift = opened(50_000);
        sold(&mut shift, 1, vec![Tender::new(Method::Cash, sar(1_500))]);
        sold(&mut shift, 2, vec![Tender::new(Method::Card, sar(9_000))]);
        sold(
            &mut shift,
            3,
            vec![Tender::new(Method::Transfer, sar(2_000))],
        );

        assert_eq!(shift.expected(), Some(Ok(sar(51_500))));
        assert_eq!(shift.takings.by(Method::Card), Some(sar(9_000)));
        assert_eq!(shift.takings.by(Method::Transfer), Some(sar(2_000)));
    }

    /// A split payment is two tenders on one sale, and only its cash half
    /// reaches the drawer.
    #[test]
    fn a_split_payment_puts_only_its_cash_half_in_the_drawer() {
        let mut shift = opened(0);
        sold(
            &mut shift,
            1,
            vec![
                Tender::new(Method::Cash, sar(2_000)),
                Tender::new(Method::Card, sar(3_000)),
            ],
        );

        assert_eq!(shift.expected(), Some(Ok(sar(2_000))));
        assert_eq!(shift.takings.by(Method::Cash), Some(sar(2_000)));
        assert_eq!(shift.takings.each().len(), 2);
    }

    /// Cash refunded and cash paid out both come back off.
    #[test]
    fn cash_out_of_the_drawer_comes_off_what_it_should_hold() {
        let mut shift = opened(20_000);
        sold(&mut shift, 1, vec![Tender::new(Method::Cash, sar(10_000))]);
        shift.apply(&ShiftEvent::Refunded {
            sale: AggregateId::new("S-1").unwrap_or_else(|_| unreachable!("a literal")),
            total: sar(2_500),
            tenders: vec![Tender::new(Method::Cash, sar(2_500))],
            why: "أعادت المنتج".to_owned(),
            at: chrono::Utc::now(),
        });
        shift.apply(&ShiftEvent::PaidOut {
            reference: "BANK-1".to_owned(),
            why: "إيداع بنكي".to_owned(),
            amount: sar(15_000),
            at: chrono::Utc::now(),
        });

        assert_eq!(shift.expected(), Some(Ok(sar(12_500))));
    }

    /// Ringing the same sale twice is caught by the shift, not by the drawer
    /// arithmetic — which is what makes a retried till harmless (L8).
    #[test]
    fn a_shift_remembers_which_sales_it_rang() {
        let mut shift = opened(0);
        sold(&mut shift, 1, vec![Tender::new(Method::Cash, sar(1_000))]);

        let rung = AggregateId::new("S-1").unwrap_or_else(|_| unreachable!("a literal"));
        assert!(shift.has_sale(&rung));
        assert!(
            !shift.has_sale(&AggregateId::new("S-2").unwrap_or_else(|_| unreachable!("a literal")))
        );
    }
}
