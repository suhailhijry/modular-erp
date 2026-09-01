//! A period paid for in advance, earned as the time passes.
//!
//! # Why this is not an entitlement with a different unit
//!
//! Because the recognition model is different, and that is the one thing in
//! this module that is an accounting error if it is got wrong.
//!
//! A ten-session package recognises **per session delivered**. A gym year
//! recognises **ratably over the period, attended or not** — the member who
//! never comes has still had a year of the right to come, and the business has
//! earned it. Treating them alike misstates revenue every month in one
//! direction or the other.
//!
//! Rekaz splits its own product along the same line, which is evidence the
//! distinction is real rather than theoretical.
//!
//! # Recognition is a cumulative total, never a sum of instalments
//!
//! Every recognition computes what *should* have been recognised by a date and
//! posts the difference. Two consequences, both of which are why it is written
//! this way:
//!
//! - **It is idempotent.** Recognising through the same date twice posts
//!   nothing the second time, so a month-end job that runs twice is harmless.
//! - **It cannot drift.** `Money::apportioned` is exact at `n/n`, so the last
//!   day of the term brings the cumulative to exactly the price and the
//!   liability closes at zero. Summing instalments would leave a halala behind
//!   on almost every term, in an account that is supposed to be a canary.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, Money, MoneyError, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubscriptionEvent {
    Started {
        customer: AggregateId,
        /// What they are on, in the business's own words.
        plan: String,
        /// **What was deferred**, excluding tax, for this term.
        price: Money,
        from: Timestamp,
        /// Exclusive, like every other interval in this codebase.
        until: Timestamp,
        at: Timestamp,
    },
    /// Time earned, up to a moment. See the module docs: this carries the
    /// *difference*, and `through` is what the cumulative was computed at.
    Recognised {
        through: Timestamp,
        value: Money,
        at: Timestamp,
    },
    /// The clock stops. Travel, injury, a closed branch.
    Frozen {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        why: String,
        at: Timestamp,
    },
    /// The clock starts, and the term is pushed out by exactly the time it was
    /// stopped for.
    Resumed {
        /// The new end of the term.
        until: Timestamp,
        at: Timestamp,
    },
    /// The term ended and another began. The old one is recognised in full
    /// first — see `crate::renew_subscription`.
    Renewed {
        price: Money,
        from: Timestamp,
        until: Timestamp,
        at: Timestamp,
    },
    Cancelled {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        why: String,
        at: Timestamp,
    },
}

impl DomainEvent for SubscriptionEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Started { .. } => Self::NAMES[0],
            Self::Recognised { .. } => Self::NAMES[1],
            Self::Frozen { .. } => Self::NAMES[2],
            Self::Resumed { .. } => Self::NAMES[3],
            Self::Renewed { .. } => Self::NAMES[4],
            Self::Cancelled { .. } => Self::NAMES[5],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

impl SubscriptionEvent {
    pub const NAMES: [&'static str; 6] = [
        "prepaid.subscription.started",
        "prepaid.subscription.recognised",
        "prepaid.subscription.frozen",
        "prepaid.subscription.resumed",
        "prepaid.subscription.renewed",
        "prepaid.subscription.cancelled",
    ];
}

#[derive(Debug, Default, Clone)]
pub struct Subscription {
    pub started: bool,
    pub customer: Option<AggregateId>,
    pub plan: String,
    pub price: Option<Money>,
    pub from: Option<Timestamp>,
    pub until: Option<Timestamp>,
    /// When the clock stopped, while it is stopped.
    pub frozen_since: Option<Timestamp>,
    /// How long it has been stopped for in total, across every freeze.
    pub frozen_seconds: i64,
    /// Cumulative, never a sum of instalments. See the module docs.
    pub recognised: Option<Money>,
    pub cancelled: bool,
}

impl Aggregate for Subscription {
    type Event = SubscriptionEvent;

    fn domain() -> DomainName {
        crate::domain("prepaid_subscription")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            SubscriptionEvent::Started {
                customer,
                plan,
                price,
                from,
                until,
                ..
            } => {
                self.started = true;
                self.customer = Some(customer.clone());
                self.plan.clone_from(plan);
                self.price = Some(*price);
                self.from = Some(*from);
                self.until = Some(*until);
                self.recognised = Some(Money::zero(price.currency()));
            }
            SubscriptionEvent::Recognised { value, .. } => {
                // Saturating for the reason `Entitlement::apply` is: `apply`
                // cannot fail, and the command already refused what would not
                // fit.
                self.recognised = self
                    .recognised
                    .map(|total| total.checked_add(*value).unwrap_or(total));
            }
            SubscriptionEvent::Frozen { at, .. } => self.frozen_since = Some(*at),
            SubscriptionEvent::Resumed { until, at } => {
                if let Some(since) = self.frozen_since {
                    self.frozen_seconds = self
                        .frozen_seconds
                        .saturating_add((*at - since).num_seconds().max(0));
                }
                self.frozen_since = None;
                self.until = Some(*until);
            }
            SubscriptionEvent::Renewed {
                price, from, until, ..
            } => {
                self.price = Some(*price);
                self.from = Some(*from);
                self.until = Some(*until);
                self.recognised = Some(Money::zero(price.currency()));
                self.frozen_seconds = 0;
                self.frozen_since = None;
            }
            SubscriptionEvent::Cancelled { .. } => self.cancelled = true,
        }
    }
}

impl Subscription {
    #[must_use]
    pub const fn exists(&self) -> bool {
        self.started
    }

    #[must_use]
    pub const fn is_frozen(&self) -> bool {
        self.frozen_since.is_some()
    }

    /// Whether it is live at a moment: started, not cancelled, inside its term,
    /// and not frozen.
    ///
    /// **This is what a gym door asks.** Rekaz sells biometric readers against
    /// exactly this question, and it has to be answerable from state rather
    /// than by a query across a projection that may be a second behind.
    #[must_use]
    pub fn admits(&self, at: Timestamp) -> bool {
        self.started
            && !self.cancelled
            && !self.is_frozen()
            && self.from.is_some_and(|from| at >= from)
            && self.until.is_some_and(|until| at < until)
    }

    /// What is still owed as unearned time.
    #[must_use]
    pub fn outstanding(&self) -> Option<Money> {
        let price = self.price?;
        let recognised = self.recognised?;
        price.checked_sub(recognised).ok()
    }

    /// How much of the term has been served by a moment, and how long the term
    /// is, both in seconds and both excluding frozen time.
    ///
    /// `None` before it starts. The frozen case caps at the moment the clock
    /// stopped, because time since then is not served and is not yet in
    /// `frozen_seconds` — it is still accruing.
    #[must_use]
    pub fn served(&self, through: Timestamp) -> Option<(i64, i64)> {
        let from = self.from?;
        let until = self.until?;

        let cap = self.frozen_since.map_or(until, |since| since.min(until));
        let effective = through.min(cap);
        let served = (effective - from).num_seconds() - self.frozen_seconds;
        let term = (until - from).num_seconds() - self.frozen_seconds;
        Some((served.clamp(0, term.max(0)), term.max(0)))
    }

    /// What should have been recognised by a moment, in total.
    ///
    /// A term of no length recognises everything: a subscription that starts
    /// and ends at the same instant has been fully delivered, which is odd but
    /// is what the arithmetic has to say rather than dividing by zero.
    pub fn earned_by(&self, through: Timestamp) -> Option<Result<Money, MoneyError>> {
        let price = self.price?;
        let (served, term) = self.served(through)?;
        Some(if term == 0 {
            Ok(price)
        } else {
            price.apportioned(served, term)
        })
    }
}
