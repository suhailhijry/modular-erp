//! Something bought now and delivered later, drawn down as it is used.
//!
//! # Why a package, a course and a deposit are one aggregate
//!
//! They differ in what the balance is counted in and in what draws it down,
//! and in nothing else. A ten-session package is ten uses of a named service;
//! a deposit is an amount held against a named booking. Both are *money
//! received for value not yet delivered*, both release that value on delivery,
//! and both leave breakage behind if they expire unused.
//!
//! That system models each separately and pays for it: its `ServicePackage`
//! and its deposits share an expiry rule, a grant reason and a redemption
//! record, written twice. The lesson is the one 7b and 8a already taught —
//! a seat, a shower and a service are one concept, and so are these.
//!
//! # What is refused, and why
//!
//! **An open-value gift card.** A card spendable on anything is a
//! *multi-purpose voucher*: what it buys is not known when it is sold, so
//! neither is the rate it should have been taxed at, and this module settles no
//! tax of its own. Every shape below is single-purpose — a package counts uses
//! of a named service, a deposit names the booking it secures — and
//! [`crate::grant`] refuses an amount that is neither. That refusal is what
//! lets this module stay out of tax entirely rather than merely hoping callers
//! do. See the crate docs.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, Money, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

/// How an entitlement came to exist.
///
/// **It decides the accounting, not the wording.** Two of these were paid for
/// and two were not, and that is the whole of what this enum is for: a grant
/// nobody paid for creates no liability, so it posts nothing and has nothing to
/// release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The customer paid for it.
    Bought,
    /// Another customer paid for it and gave it away. Somebody paid, so the
    /// liability is real; who is holding it is a different question from who
    /// funded it, and this is what keeps that answerable a year later.
    GiftedByCustomer,
    /// The business gave it. A goodwill session after a complaint.
    GrantedByBusiness,
    /// It came with a coupon.
    ///
    /// **A coupon is not a liability.** No consideration was received, so there
    /// is nothing to defer and nothing to recognise. That system has a full
    /// coupon model and no coupon liability account, which is correct and worth
    /// not undoing.
    FreeFromCoupon,
}

impl Reason {
    /// Whether money changed hands, and therefore whether there is a liability.
    #[must_use]
    pub const fn was_paid_for(self) -> bool {
        matches!(self, Self::Bought | Self::GiftedByCustomer)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bought => "bought",
            Self::GiftedByCustomer => "gifted_by_customer",
            Self::GrantedByBusiness => "granted_by_business",
            Self::FreeFromCoupon => "free_from_coupon",
        }
    }

    pub const ALL: [Self; 4] = [
        Self::Bought,
        Self::GiftedByCustomer,
        Self::GrantedByBusiness,
        Self::FreeFromCoupon,
    ];
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a way an entitlement is granted")]
pub struct UnknownReason(pub String);

impl std::str::FromStr for Reason {
    type Err = UnknownReason;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == s)
            .ok_or_else(|| UnknownReason(s.to_owned()))
    }
}

/// What is left, and what one use of it is worth.
///
/// Two numbers rather than one because a package is counted and a deposit is
/// not, and because the value has to be tracked in money regardless — it is the
/// liability, and the liability is what has to come out right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    /// Uses remaining. `None` on an entitlement that is only an amount — a
    /// deposit against a booking, which is drawn once for whatever it covers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uses: Option<u32>,
    /// **The liability.** What is still owed as value, in the currency it was
    /// sold in.
    pub value: Money,
}

impl Balance {
    /// What one more use is worth, and therefore what redeeming it recognises.
    ///
    /// # Why the last use takes the remainder
    ///
    /// Ten sessions of a 100 riyal package is 10 riyals each and divides
    /// exactly. Three sessions of 100 is 33.33, and three of those is 99.99 —
    /// a halala stranded in a liability account for ever, on a package the
    /// customer has finished.
    ///
    /// So a use is worth `remaining value / remaining uses`, recomputed each
    /// time. When one use is left that is the whole remaining value, which is
    /// the definition rather than a special case. `Money::apportioned` is exact
    /// at `n/n` for the same reason.
    pub fn worth_of_one_use(self) -> Result<Money, erp_types::MoneyError> {
        match self.uses {
            Some(uses) if uses > 1 => self.value.apportioned(1, i64::from(uses)),
            // The last use, or an entitlement that is only an amount.
            _ => Ok(self.value),
        }
    }

    #[must_use]
    pub fn is_spent(&self) -> bool {
        self.uses.is_some_and(|uses| uses == 0) || self.value.is_zero()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntitlementEvent {
    Granted {
        /// The `crm` record it belongs to. Not optional here and optional on an
        /// invoice, because an entitlement is a balance somebody holds — a
        /// walk-in with no record has nowhere to hold one.
        customer: AggregateId,
        /// What it is for, in the business's own words. Never looked at by any
        /// rule here; it is what a redemption is checked against by whoever
        /// redeems, and what a customer reads on a statement.
        what: String,
        /// Uses granted. Absent on an entitlement that is only an amount.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uses: Option<u32>,
        /// **What was deferred**, excluding tax. Zero when nobody paid.
        value: Money,
        reason: Reason,
        /// The thing this is held against, when it is held against one — the
        /// booking a deposit secures. An opaque id: this module does not know
        /// what a booking is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        against: Option<AggregateId>,
        /// When it stops being redeemable. `None` never expires, which is what
        /// a deposit against a dated booking wants.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at: Option<Timestamp>,
        at: Timestamp,
    },
    Redeemed {
        /// The caller's key for this redemption. Redeeming the same one twice
        /// is a no-op, which is what makes a retried handler harmless (L8).
        reference: String,
        uses: u32,
        /// What was released to revenue.
        value: Money,
        at: Timestamp,
    },
    /// Time ran out with value unredeemed. **Breakage**, and it is revenue:
    /// the obligation to deliver has gone, so what was held against it is
    /// earned.
    Expired { value: Money, at: Timestamp },
    /// Taken back — a refund, a chargeback, a goodwill grant withdrawn. The
    /// liability goes and **nothing is recognised**, because nothing was
    /// delivered.
    Revoked {
        why: String,
        value: Money,
        at: Timestamp,
    },
}

impl DomainEvent for EntitlementEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Granted { .. } => Self::NAMES[0],
            Self::Redeemed { .. } => Self::NAMES[1],
            Self::Expired { .. } => Self::NAMES[2],
            Self::Revoked { .. } => Self::NAMES[3],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

impl EntitlementEvent {
    pub const NAMES: [&'static str; 4] = [
        "prepaid.entitlement.granted",
        "prepaid.entitlement.redeemed",
        "prepaid.entitlement.expired",
        "prepaid.entitlement.revoked",
    ];
}

#[derive(Debug, Default, Clone)]
pub struct Entitlement {
    pub granted: bool,
    pub customer: Option<AggregateId>,
    pub what: String,
    pub reason: Option<Reason>,
    pub balance: Option<Balance>,
    pub expires_at: Option<Timestamp>,
    pub against: Option<AggregateId>,
    /// Ended, and how. `None` while it is still live.
    pub closed: Option<Closed>,
    /// Redemption keys already seen. Small — an entitlement is drawn down a
    /// handful of times — and the only way to make redeeming idempotent
    /// without a second table.
    pub redemptions: Vec<String>,
}

/// How an entitlement stopped being redeemable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closed {
    /// Every use taken, or every riyal drawn.
    Spent,
    Expired,
    Revoked,
}

impl Aggregate for Entitlement {
    type Event = EntitlementEvent;

    fn domain() -> DomainName {
        crate::domain("prepaid_entitlement")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            EntitlementEvent::Granted {
                customer,
                what,
                uses,
                value,
                reason,
                against,
                expires_at,
                ..
            } => {
                self.granted = true;
                self.customer = Some(customer.clone());
                self.what.clone_from(what);
                self.reason = Some(*reason);
                self.balance = Some(Balance {
                    uses: *uses,
                    value: *value,
                });
                self.against.clone_from(against);
                self.expires_at = *expires_at;
            }
            EntitlementEvent::Redeemed {
                reference,
                uses,
                value,
                ..
            } => {
                self.redemptions.push(reference.clone());
                if let Some(balance) = &mut self.balance {
                    balance.uses = balance.uses.map(|left| left.saturating_sub(*uses));
                    // Saturating rather than checked: `apply` cannot fail, and
                    // the command already refused anything that would not fit.
                    // A balance that went negative here would mean the log is
                    // corrupt, which the deferred-revenue canary then catches.
                    balance.value = balance.value.checked_sub(*value).unwrap_or(balance.value);
                }
                if self.balance.is_some_and(|b| b.is_spent()) {
                    self.closed = Some(Closed::Spent);
                }
            }
            EntitlementEvent::Expired { .. } => {
                self.closed = Some(Closed::Expired);
                self.balance = self.balance.map(|balance| Balance {
                    uses: balance.uses.map(|_| 0),
                    value: Money::zero(balance.value.currency()),
                });
            }
            EntitlementEvent::Revoked { .. } => {
                self.closed = Some(Closed::Revoked);
                self.balance = self.balance.map(|balance| Balance {
                    uses: balance.uses.map(|_| 0),
                    value: Money::zero(balance.value.currency()),
                });
            }
        }
    }
}

impl Entitlement {
    #[must_use]
    pub const fn exists(&self) -> bool {
        self.granted
    }

    /// Whether it can still be drawn down.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.granted && self.closed.is_none()
    }

    /// Whether it has run out of time by a given moment.
    ///
    /// Takes the moment rather than reading a clock, so a replay reproduces
    /// what was decided rather than what today would decide.
    #[must_use]
    pub fn has_lapsed(&self, at: Timestamp) -> bool {
        self.expires_at.is_some_and(|expiry| at >= expiry)
    }

    #[must_use]
    pub fn has_redemption(&self, reference: &str) -> bool {
        self.redemptions.iter().any(|seen| seen == reference)
    }

    /// What is still owed to the customer.
    #[must_use]
    pub fn outstanding(&self) -> Option<Money> {
        self.balance.map(|balance| balance.value)
    }
}
