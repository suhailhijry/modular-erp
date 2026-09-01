//! Points, stamps and visits — a balance earned by activity, redeemed later.
//!
//! # Why the three mechanics are one aggregate
//!
//! Points are earned at a rate on spend, stamps are earned one per named item,
//! visits are earned one per attendance. That is the whole of the difference,
//! and it is a difference in **what produces the count**, not in what the count
//! then does. All three accumulate, all three are redeemed for something, and
//! all three carry the same obligation to honour what was earned.
//!
//! So [`Mechanic`] is fixed when a card is opened and read by the business, and
//! the caller supplies the count for stamps and visits while [`Scheme`]
//! computes it for points. Nothing below branches on it. Rekaz models the three
//! separately and pays for it in three earning paths and three balances; the
//! lesson is the one [`crate::Entitlement`] already learned from packages and
//! deposits.
//!
//! # IFRS 15, and no shortcut
//!
//! Points are a **separate performance obligation**. A sale that awards them
//! has not delivered everything it was paid for, so part of its price belongs
//! to the points and is deferred until they are redeemed or expire:
//!
//! ```text
//! allocated = spend × (count × worth) / (spend + count × worth)
//! ```
//!
//! That is IFRS 15's relative standalone selling price, with the sale's own
//! price standing as the goods' standalone price and [`Scheme::worth`] as the
//! points'. The common SMB shortcut instead accrues a liability at redemption
//! value when the points are earned, which charges the whole reward to expense
//! and leaves revenue overstated. Saudi requires IFRS, so this module does not
//! implement the shortcut and takes no setting that would select it: what an
//! accountant may not choose, a tenant may not either.
//!
//! **The allocation is frozen into the event** (L5). A scheme that changes
//! `worth` next year does not restate what was deferred last year, because the
//! amount is in the log and not recomputed from configuration.
//!
//! # What this module needs from the sale, and what it does not
//!
//! It needs the sale's **price**, because the allocation is a fraction of it.
//! It does not need the sale: [`crate::Earning::from`] is an opaque id, the
//! same reconciliation surface a deposit uses for the booking it secures, and
//! for the same reason — `sales` and `prepaid` are siblings and neither may
//! depend on the other.
//!
//! That the two postings are separate transactions is deliberate and is the
//! module's existing bargain: `sales` books the invoice, `prepaid` books the
//! deferral, and the canary in `a_liability_agrees_with_the_ledger` is what
//! catches a pair that came apart.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, Money, MoneyError, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

use crate::entitlement::Balance;

/// What a card counts.
///
/// **It decides what produces the count and nothing else.** The accounting is
/// identical across all three — see the module docs — so this is a label the
/// business reads and a rule nobody here applies. Recorded because a stamp card
/// and a points balance are different things to the person holding one, and a
/// screen that could not tell them apart would be useless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mechanic {
    /// A balance earned at a rate on what is spent.
    Points,
    /// N of a named thing buys one free. The coffee-shop punch card.
    Stamps,
    /// Attendances, counted whatever they cost.
    Visits,
}

impl Mechanic {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Points => "points",
            Self::Stamps => "stamps",
            Self::Visits => "visits",
        }
    }

    pub const ALL: [Self; 3] = [Self::Points, Self::Stamps, Self::Visits];
}

impl std::fmt::Display for Mechanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a way a card counts")]
pub struct UnknownMechanic(pub String);

impl std::str::FromStr for Mechanic {
    type Err = UnknownMechanic;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|mechanic| mechanic.as_str() == s)
            .ok_or_else(|| UnknownMechanic(s.to_owned()))
    }
}

/// A rank, reached by lifetime count and never lost by spending.
///
/// Rekaz calls this `Membership`, which is easy to misread as a gym membership;
/// it is a rank. What it changes is the earning rate, which is why it is here
/// and not in a projection: it is an input to a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier {
    /// What the business calls it. Never matched on.
    pub name: String,
    /// The lifetime count at which this rank begins.
    pub from: u32,
    /// Counts earned per major unit spent, in basis points. `15_000` is a point
    /// and a half per riyal.
    pub rate_bp: u32,
}

/// How a tenant's loyalty works.
///
/// **There is no default.** Account codes have a conventional value that every
/// chart ships, and what a point is worth does not: it is a business decision
/// with no defensible fallback, and guessing one would put a number nobody
/// chose into the allocation that decides deferred revenue. A tenant who has
/// not configured a scheme cannot earn (L6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scheme {
    /// What one count is worth when it is redeemed.
    ///
    /// **This is the standalone selling price** the IFRS 15 allocation is
    /// computed from, and the currency every card under this scheme is held in.
    pub worth: Money,
    /// The base earning rate, in counts per major unit spent, in basis points.
    /// `10_000` is one point per riyal. Ignored for stamps and visits, whose
    /// count the caller supplies.
    pub rate_bp: u32,
    /// Ranks, in any order. The highest `from` at or below a card's lifetime
    /// count wins; below all of them the rate is [`Self::rate_bp`].
    #[serde(default)]
    pub tiers: Vec<Tier>,
}

impl Scheme {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "prepaid.loyalty_scheme";

    /// What this tenant has configured, or nothing.
    pub async fn resolve(
        conn: &mut sqlx::PgConnection,
    ) -> Result<Option<Self>, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map(|configured| configured.value))
    }

    /// The rank a lifetime count has reached, if it has reached one.
    #[must_use]
    pub fn rank_at(&self, lifetime: u32) -> Option<&Tier> {
        self.tiers
            .iter()
            .filter(|tier| tier.from <= lifetime)
            .max_by_key(|tier| tier.from)
    }

    /// The rate that applies at a lifetime count.
    #[must_use]
    pub fn rate_at(&self, lifetime: u32) -> u32 {
        self.rank_at(lifetime)
            .map_or(self.rate_bp, |tier| tier.rate_bp)
    }

    /// How many counts a spend earns at a given rank.
    ///
    /// Rounded **down**: a business that promised a point per riyal has not
    /// promised one for eighty halalas, and rounding up would award a point
    /// nobody paid for and defer revenue against it.
    #[must_use]
    pub fn counts_for(&self, spend: Money, lifetime: u32) -> u32 {
        if !spend.is_positive() {
            return 0;
        }
        let per_major = i128::from(spend.currency().minor_per_major());
        let earned =
            i128::from(spend.minor()) * i128::from(self.rate_at(lifetime)) / (per_major * 10_000);
        u32::try_from(earned).unwrap_or(u32::MAX)
    }
}

/// What a sale gives up to the points it awards.
///
/// IFRS 15's relative standalone selling price: the sale's own price stands as
/// the goods' standalone price, `count × worth` as the points', and the
/// transaction price is split between them in that ratio.
///
/// Zero when nothing was spent or nothing was earned — a bonus point awarded
/// for nothing is not a contract with a customer and defers nothing, which is
/// the same conclusion [`crate::Reason::was_paid_for`] reaches about a coupon.
pub fn allocate(spend: Money, count: u32, worth: Money) -> Result<Money, MoneyError> {
    let standalone = worth.checked_mul_int(i64::from(count))?;
    if !spend.is_positive() || !standalone.is_positive() {
        return Ok(Money::zero(spend.currency()));
    }
    let total = spend.checked_add(standalone)?;
    spend.apportioned(standalone.minor(), total.minor())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoyaltyEvent {
    Opened {
        /// The `crm` record the card belongs to.
        customer: AggregateId,
        mechanic: Mechanic,
        at: Timestamp,
    },
    Earned {
        /// The caller's key for this movement. Earning against the same one
        /// twice is a no-op, which is what makes a retried till harmless (L8).
        reference: String,
        count: u32,
        /// **What the sale gave up to the points**, frozen at the moment it was
        /// decided rather than recomputed from a scheme that may have changed.
        allocated: Money,
        /// The sale it came from. An opaque id: this module does not know what
        /// an invoice is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<AggregateId>,
        at: Timestamp,
    },
    Redeemed {
        reference: String,
        count: u32,
        /// What was released to revenue: the obligation, delivered.
        value: Money,
        /// What they were spent on. Opaque, for the same reason `from` is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        toward: Option<AggregateId>,
        at: Timestamp,
    },
    /// Counts timed out unredeemed. **Breakage**, and it is revenue: the
    /// obligation has gone, so what was held against it is earned.
    ///
    /// The card survives it. A points balance running out is not the end of the
    /// card, which is the difference between this and an entitlement expiring.
    Expired {
        count: u32,
        value: Money,
        at: Timestamp,
    },
}

impl DomainEvent for LoyaltyEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Opened { .. } => Self::NAMES[0],
            Self::Earned { .. } => Self::NAMES[1],
            Self::Redeemed { .. } => Self::NAMES[2],
            Self::Expired { .. } => Self::NAMES[3],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

impl LoyaltyEvent {
    pub const NAMES: [&'static str; 4] = [
        "prepaid.loyalty.opened",
        "prepaid.loyalty.earned",
        "prepaid.loyalty.redeemed",
        "prepaid.loyalty.expired",
    ];
}

/// How many movement keys a card remembers.
///
/// A card is earned on at every visit for years, so remembering every key it
/// ever saw would grow without bound. A retry arrives within seconds; anything
/// arriving sixty-four movements later is a new movement and not a retry.
const MOVEMENT_WINDOW: usize = 64;

#[derive(Debug, Default, Clone)]
pub struct Loyalty {
    pub opened: bool,
    pub customer: Option<AggregateId>,
    pub mechanic: Option<Mechanic>,
    /// Counts redeemable now, and the value deferred against them.
    pub balance: Option<Balance>,
    /// Every count ever earned. **Never decreases** — spending points does not
    /// cost a rank — and it is what [`Scheme::rank_at`] reads.
    pub lifetime: u32,
    /// Movement keys already seen, oldest first. Bounded; see
    /// [`MOVEMENT_WINDOW`].
    pub movements: std::collections::VecDeque<String>,
}

impl Aggregate for Loyalty {
    type Event = LoyaltyEvent;

    fn domain() -> DomainName {
        crate::domain("prepaid_loyalty")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            LoyaltyEvent::Opened {
                customer, mechanic, ..
            } => {
                self.opened = true;
                self.customer = Some(customer.clone());
                self.mechanic = Some(*mechanic);
            }
            LoyaltyEvent::Earned {
                reference,
                count,
                allocated,
                ..
            } => {
                self.remember(reference);
                self.lifetime = self.lifetime.saturating_add(*count);
                self.balance = Some(match self.balance {
                    Some(held) => Balance {
                        uses: Some(held.uses.unwrap_or(0).saturating_add(*count)),
                        // Saturating on the currency: `apply` cannot fail, and
                        // the command refused a mismatch before it got here.
                        value: held.value.checked_add(*allocated).unwrap_or(held.value),
                    },
                    None => Balance {
                        uses: Some(*count),
                        value: *allocated,
                    },
                });
            }
            LoyaltyEvent::Redeemed {
                reference,
                count,
                value,
                ..
            } => {
                self.remember(reference);
                if let Some(balance) = &mut self.balance {
                    balance.uses = balance.uses.map(|left| left.saturating_sub(*count));
                    balance.value = balance.value.checked_sub(*value).unwrap_or(balance.value);
                }
            }
            LoyaltyEvent::Expired { .. } => {
                // The balance goes and the card stays. A points balance running
                // out is not the end of the card.
                self.balance = self.balance.map(|balance| Balance {
                    uses: Some(0),
                    value: Money::zero(balance.value.currency()),
                });
            }
        }
    }
}

impl Loyalty {
    fn remember(&mut self, reference: &str) {
        if self.movements.len() >= MOVEMENT_WINDOW {
            self.movements.pop_front();
        }
        self.movements.push_back(reference.to_owned());
    }

    #[must_use]
    pub const fn exists(&self) -> bool {
        self.opened
    }

    #[must_use]
    pub fn has_movement(&self, reference: &str) -> bool {
        self.movements.iter().any(|seen| seen == reference)
    }

    /// Counts redeemable now.
    #[must_use]
    pub fn counts(&self) -> u32 {
        self.balance.and_then(|balance| balance.uses).unwrap_or(0)
    }

    /// What is still owed against the counts.
    #[must_use]
    pub fn outstanding(&self) -> Option<Money> {
        self.balance.map(|balance| balance.value)
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

    fn scheme(rate_bp: u32, tiers: Vec<Tier>) -> Scheme {
        Scheme {
            worth: sar(10),
            rate_bp,
            tiers,
        }
    }

    /// **The allocation is a fraction of the sale, not the reward's value.**
    ///
    /// A hundred riyals awarding a hundred points worth ten halalas each: the
    /// points' standalone price is ten riyals against the goods' hundred, so
    /// they take ten elevenths of a riyal short of ten — 9.09 — and not ten.
    /// The shortcut would defer the whole ten and overstate the liability.
    #[test]
    fn the_allocation_splits_the_sale_and_does_not_add_to_it() {
        assert_eq!(allocate(sar(10_000), 100, sar(10)), Ok(sar(909)));
    }

    /// Nothing spent, or nothing earned, allocates nothing.
    #[test]
    fn a_reward_nobody_paid_for_defers_nothing() {
        assert_eq!(allocate(sar(0), 100, sar(10)), Ok(sar(0)));
        assert_eq!(allocate(sar(10_000), 0, sar(10)), Ok(sar(0)));
    }

    /// **The rate is per major unit and rounds down.** A point per riyal on
    /// eighty halalas is no points: a business that promised one per riyal did
    /// not promise one for eighty halalas.
    #[test]
    fn counts_round_down_to_what_was_actually_promised() {
        let scheme = scheme(10_000, vec![]);
        assert_eq!(scheme.counts_for(sar(10_000), 0), 100);
        assert_eq!(scheme.counts_for(sar(80), 0), 0);
        assert_eq!(scheme.counts_for(sar(199), 0), 1);
    }

    /// **A rank is reached by lifetime count and changes the rate.**
    #[test]
    fn the_highest_rank_reached_sets_the_rate() {
        let scheme = scheme(
            10_000,
            vec![
                Tier {
                    name: "فضي".to_owned(),
                    from: 500,
                    rate_bp: 15_000,
                },
                Tier {
                    name: "ذهبي".to_owned(),
                    from: 2_000,
                    rate_bp: 20_000,
                },
            ],
        );
        assert_eq!(scheme.rate_at(0), 10_000);
        assert_eq!(scheme.rate_at(499), 10_000);
        assert_eq!(scheme.rate_at(500), 15_000);
        assert_eq!(scheme.rate_at(5_000), 20_000);
        assert_eq!(scheme.counts_for(sar(10_000), 2_000), 200);
        assert_eq!(
            scheme.rank_at(600).map(|tier| tier.name.as_str()),
            Some("فضي")
        );
        assert_eq!(scheme.rank_at(1), None);
    }
}
