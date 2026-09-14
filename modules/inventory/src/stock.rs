//! What is on hand at one place, in lots, and how it got there.
//!
//! # Open lots and a window, and nothing else
//!
//! A shelf is the lots that still have something on them. A lot that empties
//! **closes and leaves** — a café receiving beans every morning for three years
//! carries the four lots it can still pour from, not the eleven hundred it has
//! ever bought. The read model keeps the history; the aggregate keeps what a
//! command has to decide from.
//!
//! Beside them is a bounded window of the movement references already heard, so
//! a client retrying a day of requests writes nothing twice. The same idiom, and
//! the same reasoning, as `conversations::Thread`: a stream that runs for years
//! may not remember everything it has ever seen.
//!
//! **The window answers retries and nothing else.** A return has to put back
//! exactly what one sale took, however long ago, so it follows that sale
//! through the whole stream with [`Returning`] rather than asking a window that
//! forgets. A review found the window doing both jobs: an invoice for a busy
//! product could not be cancelled once its shelf had seen two hundred more
//! movements.
//!
//! # A shelf can owe
//!
//! A plain product's sale takes what the lots cannot cover as a shortfall (R1)
//! and the shelf records the debt. [`Stock::on_hand`] and [`Stock::value`] both
//! come down by it, because the books already have: the sale credited
//! `1300 Inventory` for the shortfall at the last unit cost it knew. A shelf
//! that owed without saying so would disagree with that account for ever.
//!
//! # Cost comes from the lot, not from an average
//!
//! Each lot carries what its own delivery cost, and a portion taken off it
//! costs `value × units / quantity` — `Money::apportioned`, exact at the whole
//! lot, so the last portion takes the remainder and the lot closes on exactly
//! zero rather than stranding a halala in `1300 Inventory` for ever. The rule
//! that says *which* lot is [`crate::picking::pick`], and it is the only way
//! units leave.
//!
//! **Reversed, this is wrong.** A weighted average across the shelf is two
//! integers instead of a list and is genuinely cheaper — and it cannot answer
//! "which delivery is this", which is the entire question expiry and recall
//! ask. A product that spoils cannot be costed by a method that has forgotten
//! which units are old.
//!
//! # Why quantities are integers
//!
//! `Money` is integer minor units carrying its own exponent, and
//! `float_arithmetic` is denied workspace-wide — the pricing engine already
//! paid for that lesson. A quantity is the same decision: an integer in the
//! product's own frozen unit. Declare coffee in grams and a drink consumes 18
//! of them.

use std::collections::VecDeque;

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{
    AggregateId, CurrencyCode, DomainName, EventName, Money, MoneyError, SchemaVersion, Timestamp,
};
use serde::{Deserialize, Serialize};

use crate::picking::{OpenLot, Portion, Shortfall};

/// How many movement references one shelf remembers.
///
/// Enough that a client retrying a day of requests writes nothing twice, and
/// bounded so a product moving for years does not carry every reference it ever
/// saw. The same window, and the same reasoning, as `conversations::Thread`.
pub const HEARD_WINDOW: usize = 200;

/// Why stock was written off.
///
/// Two, because two is what a person can pick from a list without thinking and
/// what a loss account can be argued from. **Not free text**: a reason nobody
/// can group by is a reason nobody reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Past its date. The lot's expiry is what made it so.
    Expired,
    /// Broken, spoiled, spilt, stolen.
    Damaged,
}

impl Reason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Damaged => "damaged",
        }
    }

    #[must_use]
    pub fn parse(literal: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == literal)
    }

    pub const ALL: [Self; 2] = [Self::Expired, Self::Damaged];
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StockEvent {
    /// It arrived, and it is a lot — whatever the product's tracking mode.
    ///
    /// **Posts `Dr` inventory, `Cr` goods received not invoiced** at what the
    /// delivery cost (R3): the goods are an asset the moment they land, and what
    /// they owe is not accounts payable until somebody bills it. The supplier's
    /// bill debits the holding account back on the line that names the product.
    Received {
        /// Derived from the receipt: the request's own key, never minted (L8).
        lot: String,
        /// The tenant's own batch code, on a lot-tracked product. Data, and it
        /// may repeat — `lot` is what keeps two deliveries of `B-2026-04`
        /// apart.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_on: Option<chrono::NaiveDate>,
        quantity: i64,
        /// What the whole `quantity` cost, not what one of them did. A unit
        /// price would have to be multiplied back out, and the rounding would
        /// not always land on what the supplier actually charged.
        value: Money,
        /// One per unit, on a serial-tracked product. **Given by the caller**:
        /// a serial is an identity and this module does not invent identities
        /// (L8).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        serials: Vec<String>,
        reference: String,
        at: Timestamp,
    },
    /// It went out on a document — a sale, a bill, anything that consumes.
    ///
    /// **Posts `Dr` cost of goods sold, `Cr` inventory**, at the sum of what the
    /// portions below were carried at on their own lots, plus the shortfall.
    /// `sales::issue_in` is the only thing that writes one.
    Consumed {
        /// Which lots gave it up and what each portion was carried at — frozen
        /// here, as on a write-off, because by the next delivery the lot may be
        /// closed and gone.
        ///
        /// **May be empty**, and only here: a plain product may be sold off a
        /// shelf with nothing on it, and then the whole of what left is the
        /// shortfall below. Every other movement out is refused for what the
        /// lots cannot cover.
        portions: Vec<Portion>,
        /// **What no lot could cover**, at the shelf's last known unit cost
        /// (decision 16, R1). Only a plain product can carry one; a lot- or
        /// serial-tracked sale is refused instead.
        ///
        /// `#[serde(default)]`, so every consumption written before this
        /// existed decodes as one with none — which is what it was.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shortfall: Option<Shortfall>,
        /// The document line this went out on. What makes consuming it twice
        /// nothing.
        reference: String,
        at: Timestamp,
    },
    /// A customer brought it back, and it is on the shelf again.
    ///
    /// **Posts `Dr` inventory, `Cr` cost of goods sold** — the consumption
    /// undone at what that consumption froze, never at today's cost and never
    /// at a proportion of what was credited (decision 12).
    ///
    /// The portions name the lots the goods left on and carry their own code
    /// and date, so a lot that closed in between **reopens as itself** rather
    /// than as an undated lot of unknown batch. See [`Portion::code`].
    Restored {
        /// **The consumption this undoes**, by its reference. Here and not only
        /// in the command, because the shelf has to take what came back off
        /// what that movement still has out — otherwise two credit notes
        /// against one sale each put the whole of it back.
        taken_on: String,
        /// Which lots take them back, and what each portion is put back at.
        /// **May be empty** when everything coming back is settling a debt.
        portions: Vec<Portion>,
        /// **What this return settles of what the shelf owed**, undone at the
        /// cost the shortfall was charged out at. Taken before the lots, so a
        /// returned unit pays a phantom off before it refills a real batch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        settles: Option<Shortfall>,
        /// The credit note line this came back on — the caller's own key, never
        /// the consumption's, or the shelf would hear a retry of the sale.
        reference: String,
        at: Timestamp,
    },
    /// Somebody threw it away, and said why.
    ///
    /// **Posts `Dr` waste, `Cr` inventory**, at what the portions were carried
    /// at. The reason stays on the movement rather than choosing an account:
    /// expired and damaged are the same expense and a different conversation.
    WrittenOff {
        reason: Reason,
        /// Which lots gave it up and what each portion was carried at — frozen
        /// here, because by the next delivery the lot may be closed and gone.
        ///
        /// **Never empty.** Everything that left is here: a movement the lots
        /// could not cover is refused, and there is nowhere else for a unit to
        /// have come from. See [`crate::write_off`].
        portions: Vec<Portion>,
        reference: String,
        at: Timestamp,
    },
    /// Somebody counted the shelf, or one lot on it.
    ///
    /// **The variance is frozen into the event, and so is where it landed**,
    /// exactly as `pos::ShiftEvent::Closed` freezes the drawer's — L5 in its
    /// concrete form. Recomputing either on read would answer from whatever the
    /// books say today, which is not what was found on the day.
    ///
    /// **`taken` has no default, on purpose.** A count written before a count
    /// could take a shelf set its lot to `declared` and carried no portions;
    /// decoded as one with none, it would replay as a count that moved nothing
    /// while the read model had moved the lot. No tenant has such an event —
    /// the module had not shipped — so it fails to decode instead (L6).
    Counted {
        /// **The lot that was counted**, on a count of one batch. `None` when
        /// the whole shelf was. See [`crate::count`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lot: Option<String>,
        /// What the books said: that lot's quantity, or the shelf's
        /// [`Stock::on_hand`] — the open lots less what the shelf owes, which
        /// is the number a screen showed the counter.
        expected: i64,
        /// What the person counted.
        declared: i64,
        /// `declared - expected`. Negative is short.
        variance: i64,
        /// What the variance is worth, signed the same way: what `joined` and
        /// `settles` put back, less what `taken` was carried at. `None` only on
        /// a shelf nothing was ever received onto, which has no currency to
        /// state a zero in.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Money>,
        /// **What a shortage came off**, in picking order and each portion at
        /// its own lot's cost — or, on a serial-tracked product, the named
        /// units that were on hand and not found.
        taken: Vec<Portion>,
        /// **Where an overage went: the lot that goes out last** — the other end
        /// of the picking order a shortage walks — at that lot's own unit cost,
        /// so joining it does not quietly turn the lot into an average of two
        /// prices.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        joined: Option<Portion>,
        /// **What the shelf owed, and the count cleared** — at what the sale
        /// charged it out at. Only on a count of the shelf: the debt is on no
        /// lot, so counting one cannot settle it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        settles: Option<Shortfall>,
        reference: String,
        at: Timestamp,
    },
}

impl StockEvent {
    pub const NAMES: [&'static str; 5] = [
        "inventory.stock.received",
        "inventory.stock.consumed",
        "inventory.stock.written_off",
        "inventory.stock.counted",
        "inventory.stock.restored",
    ];
}

impl DomainEvent for StockEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Received { .. } => Self::NAMES[0],
            Self::Consumed { .. } => Self::NAMES[1],
            Self::WrittenOff { .. } => Self::NAMES[2],
            Self::Counted { .. } => Self::NAMES[3],
            Self::Restored { .. } => Self::NAMES[4],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// What one consumption **still has out**: the lots it took as it froze them,
/// less whatever has come back against it since.
#[derive(Debug, Default, Clone)]
pub struct WentOut {
    /// The lots it took, and what each portion was carried at.
    pub portions: Vec<Portion>,
    /// What no lot could cover and **the shelf still owes**, on the same terms.
    pub shortfall: Option<Shortfall>,
    /// **What no lot could cover and a count of the shelf has since cleared.**
    ///
    /// Not a debt any more: a count clears everything the shelf owes, so these
    /// units are ones the books now accept went out. A return puts them back
    /// as **stock**, on the lot [`Returning::lands_on`] names, at what the sale
    /// charged them out at. Settling them against the debt instead — which is
    /// what a return did before review — paid off nothing, and the units the
    /// customer brought back vanished from the shelf while the books and the
    /// read model counted them.
    pub counted: Option<Shortfall>,
}

impl WentOut {
    /// Takes a return off what is still out, so a second credit note against
    /// one sale can only put back what the first one left. A portion on
    /// `lands_on` is the part a count had cleared.
    fn give_back(&mut self, portions: &[Portion], settles: Option<Shortfall>, lands_on: &str) {
        if let (Some(owed), Some(paid)) = (self.shortfall.as_mut(), settles) {
            owed.quantity -= paid.quantity;
            if let (Some(cost), Some(off)) = (owed.cost, paid.cost) {
                owed.cost = cost.checked_sub(off).ok();
            }
        }
        if self.shortfall.is_some_and(|owed| owed.quantity <= 0) {
            self.shortfall = None;
        }
        for back in portions {
            if back.lot == lands_on {
                if let Some(cleared) = self.counted.as_mut() {
                    cleared.quantity -= back.quantity;
                    cleared.cost = cleared
                        .cost
                        .and_then(|cost| cost.checked_sub(back.cost).ok());
                }
                self.counted = self.counted.filter(|cleared| cleared.quantity > 0);
                continue;
            }
            if let Some(mine) = self.portions.iter_mut().find(|held| held.lot == back.lot) {
                mine.quantity -= back.quantity;
                mine.cost = mine.cost.checked_sub(back.cost).unwrap_or(mine.cost);
                mine.serials.retain(|held| !back.serials.contains(held));
            }
        }
        self.portions.retain(|held| held.quantity > 0);
    }
}

/// **A shelf, and one consumption on it followed through the whole stream** —
/// what a return decides from.
///
/// Seeded with the reference it follows and folded with
/// `erp_eventlog::try_execute_from`, in the transaction that appends the
/// return, so what the decision saw is what the append is checked against.
///
/// # Why not the window
///
/// Because the window forgets, and a sale does not stop being returnable when
/// its shelf has moved on. Before this, a shelf kept each consumption's portions
/// in [`Stock::heard`] and a return read them from there, so an invoice whose
/// product had seen [`HEARD_WINDOW`] more movements could never be cancelled.
/// Keeping every consumption in [`Stock`] instead would grow a café's shelf for
/// ever; following one costs nothing a load was not already paying, because a
/// load reads the whole stream anyway.
///
/// Read from the shelf's own stream, never from `proj_inventory` (L3), and at
/// what the consumption froze, never a guess (L6).
///
/// # A count is part of what it follows
///
/// A count of the shelf clears **every** debt on it, and so the part of this
/// consumption the shelf still owed ([`WentOut::shortfall`]) stops being owed
/// the moment one is folded ([`WentOut::counted`]). The shelf's debt and each
/// consumption's outstanding shortfall are one number read two ways; before
/// review only a return moved both and a count moved one.
#[derive(Debug, Default, Clone)]
pub struct Returning {
    pub stock: Stock,
    /// The consumption being followed, by its reference.
    pub taken_on: String,
    /// **The lot what a count cleared comes back on** — derived from the shelf
    /// and the consumption by `crate::returned_lot_of`, never minted (L8).
    pub lands_on: String,
    /// What it still has out. `None` until the fold meets it — and so `None`
    /// for a reference this shelf never recorded going out.
    pub went_out: Option<WentOut>,
}

impl Returning {
    #[must_use]
    pub fn of(taken_on: &str, lands_on: String) -> Self {
        Self {
            taken_on: taken_on.to_owned(),
            lands_on,
            ..Self::default()
        }
    }
}

impl Aggregate for Returning {
    type Event = StockEvent;

    fn domain() -> DomainName {
        Stock::domain()
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            StockEvent::Consumed {
                portions,
                shortfall,
                reference,
                ..
            } if *reference == self.taken_on => {
                self.went_out = Some(WentOut {
                    portions: portions.clone(),
                    shortfall: *shortfall,
                    counted: None,
                });
            }
            StockEvent::Restored {
                taken_on,
                portions,
                settles,
                ..
            } if *taken_on == self.taken_on => {
                if let Some(went_out) = self.went_out.as_mut() {
                    went_out.give_back(portions, *settles, &self.lands_on);
                }
            }
            // Only a count of the shelf settles, and it settles all of it. A
            // shortfall is only ever added by the consumption itself, so this
            // consumption cannot owe again once a count has cleared it.
            StockEvent::Counted {
                settles: Some(_), ..
            } => {
                if let Some(went_out) = self.went_out.as_mut()
                    && let Some(owed) = went_out.shortfall.take()
                {
                    went_out.counted = Some(owed);
                }
            }
            _ => {}
        }
        self.stock.apply(event);
    }
}

/// What a command needs to know about one shelf before deciding.
#[derive(Debug, Default, Clone)]
pub struct Stock {
    /// **Open lots only**, in the order they were received. See the module doc.
    pub lots: Vec<OpenLot>,
    /// What one unit of the last delivery cost. What a shortfall — units the
    /// lots cannot cover — is charged at, and `None` until something has been
    /// received.
    pub last_unit_cost: Option<Money>,
    /// **Units sold that no lot could cover, and what they were charged out
    /// at** — the shelf's debt (decision 16, R1).
    ///
    /// Kept because [`Self::on_hand`] and [`Self::value`] would otherwise lie:
    /// the books have already been relieved of the cost, so a shelf whose lots
    /// say ten while it owes three is worth seven units and not ten, and the
    /// invariant that compares stock against `1300 Inventory` would report the
    /// difference for ever.
    ///
    /// **A receipt does not pay it off.** See `crate::picking`: netting a
    /// delivery against a debt costed at a guess needs a price-variance account
    /// nobody has opened. A count settles it.
    pub owed: Option<crate::picking::Shortfall>,
    /// Movement references already recorded here, oldest first. Bounded; see
    /// [`HEARD_WINDOW`]. **For retries only** — see [`Returning`].
    pub heard: VecDeque<String>,
}

impl Aggregate for Stock {
    type Event = StockEvent;

    fn domain() -> DomainName {
        crate::domain("inventory_stock")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            StockEvent::Received {
                lot,
                code,
                expires_on,
                quantity,
                value,
                serials,
                reference,
                ..
            } => {
                self.lots.push(OpenLot {
                    id: lot.clone(),
                    code: code.clone(),
                    expires_on: *expires_on,
                    quantity: *quantity,
                    value: *value,
                    serials: serials.clone(),
                });
                // `ok()` rather than a refusal for the reason `apply` cannot
                // fail: `receive` has already refused a quantity of nothing,
                // which is the only input that makes this division fail.
                self.last_unit_cost = value.apportioned(1, *quantity).ok();
                self.remember(reference);
            }
            StockEvent::Consumed {
                portions,
                shortfall,
                reference,
                ..
            } => {
                for portion in portions {
                    self.draw_down(portion);
                }
                // `ok()` for the reason `apply` cannot fail: two shortfalls in
                // two currencies would mean a shelf `receive` already refused.
                if let Some(short) = shortfall {
                    self.owed = Some(self.owed.unwrap_or_default().plus(*short).unwrap_or(*short));
                }
                self.remember(reference);
            }
            StockEvent::WrittenOff {
                portions,
                reference,
                ..
            } => {
                for portion in portions {
                    self.draw_down(portion);
                }
                self.remember(reference);
            }
            StockEvent::Restored {
                portions,
                settles,
                reference,
                ..
            } => {
                if let Some(settled) = settles {
                    self.settle(*settled);
                }
                for portion in portions {
                    self.put_back(portion);
                }
                self.remember(reference);
            }
            StockEvent::Counted {
                taken,
                joined,
                settles,
                reference,
                ..
            } => {
                for portion in taken {
                    self.draw_down(portion);
                }
                if let Some(portion) = joined {
                    self.put_back(portion);
                }
                if let Some(settled) = settles {
                    self.settle(*settled);
                }
                self.remember(reference);
            }
        }
    }
}

impl Stock {
    /// What is on the shelf: the open lots, **less what it owes**.
    ///
    /// **Negative on a plain product that has been sold short** (R1): a till
    /// does not stop for a bad count, and the negative number is the report a
    /// count corrects. A lot- or serial-tracked shelf cannot get here — a sale
    /// it cannot cover is refused.
    #[must_use]
    pub fn on_hand(&self) -> i64 {
        let on_lots: i64 = self.lots.iter().map(|lot| lot.quantity).sum();
        on_lots - self.owed.map_or(0, |owed| owed.quantity)
    }

    /// What [`Self::on_hand`] is carried at: the open lots, less what the units
    /// it owes were charged out at. `None` until something has been received,
    /// and on a shelf whose every lot has closed and which owes nothing.
    ///
    /// **The debt comes off here too**, because the books have already been
    /// relieved of it: `consume_in` credited `1300 Inventory` for the shortfall
    /// when it sold it. A value that ignored the debt would disagree with the
    /// account by exactly that, for ever.
    ///
    /// # Errors
    /// Arithmetic that will not fit, which would mean a log this module did not
    /// write.
    pub fn value(&self) -> Result<Option<Money>, MoneyError> {
        let Some(currency) = self.currency() else {
            return Ok(None);
        };
        let held = Money::checked_sum(self.lots.iter().map(|lot| lot.value), currency)?;
        match self.owed.and_then(|owed| owed.cost) {
            Some(owed) => held.checked_sub(owed).map(Some),
            None => Ok(Some(held)),
        }
    }

    /// The currency this shelf is carried in, from the lots on it.
    #[must_use]
    pub fn currency(&self) -> Option<CurrencyCode> {
        self.lots
            .first()
            .map(|lot| lot.value.currency())
            .or_else(|| self.last_unit_cost.map(Money::currency))
    }

    /// One open lot by name.
    #[must_use]
    pub fn lot(&self, id: &str) -> Option<&OpenLot> {
        self.lots.iter().find(|lot| lot.id == id)
    }

    /// The open lot one named unit is on, if it is on the shelf at all.
    #[must_use]
    pub fn lot_holding(&self, serial: &str) -> Option<&OpenLot> {
        self.lots
            .iter()
            .find(|lot| lot.serials.iter().any(|held| held == serial))
    }

    /// Whether this movement has already been recorded here.
    #[must_use]
    pub fn has_heard(&self, reference: &str) -> bool {
        self.heard.iter().any(|seen| seen == reference)
    }

    /// Takes a portion off the lot it came from, closing the lot when it
    /// empties.
    ///
    /// A portion naming a lot that is not open is skipped rather than
    /// panicking: `apply` cannot fail, and a log that says otherwise is corrupt
    /// rather than a case to handle.
    fn draw_down(&mut self, portion: &Portion) {
        let Some(at) = self.lots.iter().position(|lot| lot.id == portion.lot) else {
            return;
        };
        let lot = &mut self.lots[at];
        lot.quantity -= portion.quantity;
        lot.value = lot.value.checked_sub(portion.cost).unwrap_or(lot.value);
        lot.serials.retain(|held| !portion.serials.contains(held));
        if lot.quantity <= 0 {
            self.lots.remove(at);
        }
    }

    /// Takes what a return or a count cleared off what the shelf owes.
    fn settle(&mut self, settled: Shortfall) {
        let owed = self.owed.unwrap_or_default();
        let left = Shortfall {
            quantity: owed.quantity - settled.quantity,
            cost: match (owed.cost, settled.cost) {
                (Some(a), Some(b)) => a.checked_sub(b).ok(),
                (some, None) => some,
                (None, _) => None,
            },
        };
        self.owed = (left.quantity > 0).then_some(left);
    }

    /// Puts a portion back on the lot it came off, **reopening that lot** when
    /// it had closed.
    ///
    /// The portion carries the lot's own code and date, so the reopened lot is
    /// the batch that left rather than an anonymous one — see [`Portion::code`].
    /// A reopened lot is pushed on the end, which changes nothing for a dated
    /// one — the picking rule sorts by date, and a count's overage joins the
    /// far end of that same order — and makes an undated one the newest of its
    /// shelf: it is stock that came back today, and the alternative is
    /// remembering a received order for lots that are gone. So is a lot a
    /// return opens for units a count had cleared ([`WentOut::counted`]).
    fn put_back(&mut self, portion: &Portion) {
        let Some(at) = self.lots.iter().position(|lot| lot.id == portion.lot) else {
            self.lots.push(OpenLot {
                id: portion.lot.clone(),
                code: portion.code.clone(),
                expires_on: portion.expires_on,
                quantity: portion.quantity,
                value: portion.cost,
                serials: portion.serials.clone(),
            });
            return;
        };
        let lot = &mut self.lots[at];
        lot.quantity += portion.quantity;
        lot.value = lot.value.checked_add(portion.cost).unwrap_or(lot.value);
        lot.serials.extend(portion.serials.iter().cloned());
    }

    fn remember(&mut self, reference: &str) {
        self.heard.push_back(reference.to_owned());
        while self.heard.len() > HEARD_WINDOW {
            self.heard.pop_front();
        }
    }
}

/// **One shelf**: this product, at this branch.
///
/// Derived and never minted (L8). Stock is per branch because a record's branch
/// is not the request's branch only for records that *have* one — and stock on
/// hand has nothing else it could mean: what is at Olaya is at Olaya. A
/// business that sends no `X-Branch` lands on one key per product, which is
/// what a single-branch business wants and what most of them are.
///
/// The two halves come back out with [`parts`], so nothing has to be written
/// down twice. A product id is the `Idempotency-Key` it was declared under and
/// therefore a UUID, which is why the first `.` is an unambiguous seam — and
/// why a product id carrying one is refused here rather than in each caller: no
/// such product can be declared through the API, and this is the invariant
/// [`parts`] rests on.
///
/// # Errors
/// [`InvalidKey`] for a product id this decomposition could not survive, or a
/// branch that will not fit beside it.
pub fn stock_id(product: &AggregateId, branch: Option<&str>) -> Result<AggregateId, InvalidKey> {
    if product.as_str().contains('.') {
        return Err(InvalidKey);
    }
    let joined = branch.map_or_else(
        || product.as_str().to_owned(),
        |branch| format!("{}.{branch}", product.as_str()),
    );
    AggregateId::new(joined).map_err(|_| InvalidKey)
}

/// The product and branch a stock stream is named after — [`stock_id`] undone.
#[must_use]
pub fn parts(stock: &str) -> (&str, Option<&str>) {
    stock
        .split_once('.')
        .map_or((stock, None), |(product, branch)| (product, Some(branch)))
}

/// A product or branch id that will not fit in one aggregate id together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("that product and branch do not make a usable stock key")]
pub struct InvalidKey;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picking::{Wanted, pick};

    fn sar(minor: i64) -> Money {
        Money::from_minor(
            minor,
            CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!("SAR is a real code")),
        )
    }

    fn id(literal: &str) -> AggregateId {
        AggregateId::new(literal).unwrap_or_else(|_| unreachable!("a literal id"))
    }

    fn at() -> Timestamp {
        "2026-04-01T08:00:00Z"
            .parse()
            .unwrap_or_else(|_| unreachable!("an instant"))
    }

    fn received(lot: &str, quantity: i64, value: i64, expires_on: Option<&str>) -> StockEvent {
        StockEvent::Received {
            lot: lot.to_owned(),
            code: None,
            expires_on: expires_on.map(|d| d.parse().unwrap_or_else(|_| unreachable!("a date"))),
            quantity,
            value: sar(value),
            serials: Vec::new(),
            reference: format!("r-{lot}"),
            at: at(),
        }
    }

    fn write_off(stock: &Stock, quantity: i64, reference: &str) -> StockEvent {
        let picked = pick(
            &stock.lots,
            Wanted::Quantity(quantity),
            stock.last_unit_cost,
        )
        .expect("picks");
        StockEvent::WrittenOff {
            reason: Reason::Damaged,
            portions: picked.portions,
            reference: reference.to_owned(),
            at: at(),
        }
    }

    /// **A lot that empties leaves the shelf**, which is what keeps a café's
    /// three-year-old espresso stream a small aggregate.
    #[test]
    fn a_depleted_lot_closes_and_the_rest_stays() {
        let mut stock = Stock::default();
        stock.apply(&received("l1", 10, 1_000, Some("2026-05-01")));
        stock.apply(&received("l2", 10, 3_000, Some("2026-06-01")));
        assert_eq!(stock.on_hand(), 20);
        assert_eq!(stock.value().expect("sums"), Some(sar(4_000)));

        let taken = write_off(&stock, 12, "w1");
        stock.apply(&taken);

        assert_eq!(stock.lots.len(), 1, "the May lot emptied and is gone");
        assert_eq!(stock.lots[0].id, "l2");
        assert_eq!(stock.on_hand(), 8);
        // A thousand off the first lot, six hundred off the second.
        assert_eq!(stock.value().expect("sums"), Some(sar(2_400)));
    }

    /// **Cost comes from the lot a portion was taken from**, not from an
    /// average across the shelf — and the lot that empties leaves nothing
    /// behind.
    #[test]
    fn each_portion_costs_what_its_own_lot_cost() {
        let mut stock = Stock::default();
        // Cheap beans expiring first, dear beans after.
        stock.apply(&received("cheap", 5, 1_000, Some("2026-05-01")));
        stock.apply(&received("dear", 5, 9_000, Some("2026-09-01")));

        let taken = write_off(&stock, 6, "w1");
        let StockEvent::WrittenOff { ref portions, .. } = taken else {
            unreachable!("a write-off")
        };
        assert_eq!(
            portions
                .iter()
                .map(|p| (p.lot.as_str(), p.quantity, p.cost.minor()))
                .collect::<Vec<_>>(),
            vec![("cheap", 5, 1_000), ("dear", 1, 1_800)],
            "an average would have charged 1,666 for every unit"
        );

        stock.apply(&taken);
        assert_eq!(stock.on_hand(), 4);
        assert_eq!(stock.value().expect("sums"), Some(sar(7_200)));
    }

    /// A count of one lot, as the portion it took off that lot.
    fn counted_off(stock: &Stock, lot: &str, declared: i64) -> StockEvent {
        let expected = stock.lot(lot).expect("open").quantity;
        let taken = pick(
            &stock.lots,
            Wanted::From {
                lot,
                quantity: expected - declared,
            },
            None,
        )
        .expect("picks")
        .portions;
        StockEvent::Counted {
            lot: Some(lot.to_owned()),
            expected,
            declared,
            variance: declared - expected,
            value: Some(sar(-taken.iter().map(|p| p.cost.minor()).sum::<i64>())),
            taken,
            joined: None,
            settles: None,
            reference: "c1".to_owned(),
            at: at(),
        }
    }

    /// A count moves the lot it took the shortage off.
    #[test]
    fn a_count_moves_the_lot_it_counted() {
        let mut stock = Stock::default();
        stock.apply(&received("l1", 10, 1_000, None));
        stock.apply(&received("l2", 10, 5_000, None));

        let count = counted_off(&stock, "l2", 8);
        stock.apply(&count);

        assert_eq!(stock.lot("l1").expect("still open").quantity, 10);
        let counted = stock.lot("l2").expect("still open");
        assert_eq!(counted.quantity, 8);
        assert_eq!(counted.value, sar(4_000));
        assert_eq!(stock.on_hand(), 18);
    }

    /// A lot counted down to nothing closes, like one that was taken down to
    /// nothing.
    #[test]
    fn a_lot_counted_to_nothing_closes() {
        let mut stock = Stock::default();
        stock.apply(&received("l1", 4, 400, None));
        let count = counted_off(&stock, "l1", 0);
        stock.apply(&count);

        assert!(stock.lots.is_empty());
        assert_eq!(stock.on_hand(), 0);
        assert_eq!(
            stock.value().expect("sums"),
            Some(sar(0)),
            "the shelf still knows its currency from the last delivery"
        );
    }

    /// Serials leave with the units they name, and the rest of the lot stays.
    #[test]
    fn a_written_off_serial_leaves_its_lot() {
        let mut stock = Stock::default();
        stock.apply(&StockEvent::Received {
            lot: "l1".to_owned(),
            code: None,
            expires_on: None,
            quantity: 3,
            value: sar(30_000),
            serials: vec!["SN-1".to_owned(), "SN-2".to_owned(), "SN-3".to_owned()],
            reference: "r1".to_owned(),
            at: at(),
        });

        let picked = pick(&stock.lots, Wanted::Serials(&["SN-2".to_owned()]), None).expect("picks");
        stock.apply(&StockEvent::WrittenOff {
            reason: Reason::Damaged,
            portions: picked.portions,
            reference: "w1".to_owned(),
            at: at(),
        });

        let lot = stock.lot("l1").expect("still open");
        assert_eq!(lot.quantity, 2);
        assert_eq!(lot.serials, vec!["SN-1".to_owned(), "SN-3".to_owned()]);
        assert_eq!(lot.value, sar(20_000));
    }

    /// The window forgets the oldest and keeps the newest, so a client
    /// replaying its recent requests records nothing twice.
    #[test]
    fn a_shelf_remembers_what_it_has_already_recorded() {
        let mut stock = Stock::default();
        for n in 0..=HEARD_WINDOW {
            stock.apply(&received(&format!("l{n}"), 1, 100, None));
        }
        assert!(stock.has_heard(&format!("r-l{HEARD_WINDOW}")));
        assert!(stock.has_heard("r-l1"));
        assert!(
            !stock.has_heard("r-l0"),
            "the window is unbounded, so a product moving for years carries every \
             reference it ever saw"
        );
    }

    /// One shelf per product per branch, and the two halves come back out.
    #[test]
    fn a_shelf_is_named_by_its_product_and_its_place() {
        let product = id("f81d4fae-7dec-11d0-a765-00a0c91e6bf6");
        let olaya = stock_id(&product, Some("BRANCH-OLAYA")).expect("a key");
        let malaz = stock_id(&product, Some("BRANCH-MALAZ")).expect("a key");
        let anywhere = stock_id(&product, None).expect("a key");

        assert_ne!(olaya, malaz, "two branches share one shelf");
        assert_ne!(olaya, anywhere);
        assert_eq!(
            parts(olaya.as_str()),
            (product.as_str(), Some("BRANCH-OLAYA"))
        );
        assert_eq!(parts(anywhere.as_str()), (product.as_str(), None));
        // A branch id carrying a dot of its own still comes back whole: the
        // seam is the *first* dot, and a product id never has one.
        let dotted = stock_id(&product, Some("b.1")).expect("a key");
        assert_eq!(parts(dotted.as_str()), (product.as_str(), Some("b.1")));

        // **A product id with a dot in it is refused rather than guessed at**,
        // because it would make the seam ambiguous and `parts` would hand the
        // projection a product nobody declared. No such product can reach here
        // — an id is an `Idempotency-Key`, which is a UUID — and this is the
        // invariant that stays true if that ever changes.
        assert_eq!(stock_id(&id("a.b"), Some("BRANCH-OLAYA")), Err(InvalidKey));
    }

    /// A reason is one of two, and anything else is refused rather than stored
    /// as a word nobody can group by.
    #[test]
    fn a_write_off_has_one_of_two_reasons() {
        for reason in Reason::ALL {
            assert_eq!(Reason::parse(reason.as_str()), Some(reason));
        }
        assert_eq!(Reason::parse("shrinkage"), None);
    }
}
