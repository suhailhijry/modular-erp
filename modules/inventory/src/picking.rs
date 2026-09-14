//! **Which lots a movement comes out of, and what each portion costs.**
//!
//! One pure function over a list of open lots. It is the heart of this module:
//! every quantity that ever leaves a shelf leaves through [`pick`], so the
//! order stock is consumed in is written down once rather than argued about at
//! each call site.
//!
//! # Earliest expiry first, and why that is also FIFO
//!
//! A dated lot sorts before an undated one, earlier date first; undated lots
//! sort among themselves oldest-received first. So a pharmacy's stock goes out
//! in the order it will spoil, and a hardware shop's — whose lots carry no
//! dates at all — goes out in the order it arrived, **down the same code
//! path**. There is no second rule for untracked products, because a second
//! rule is a second thing to get wrong.
//!
//! # Naming a lot overrides it
//!
//! A recall, a scanned batch, stock promised to a customer. Naming a lot is a
//! claim about *that* lot, so a lot that cannot cover what was asked for is
//! **refused** rather than topped up from the next one — the caller is looking
//! at the goods and is wrong about them, which is L6's case exactly.
//!
//! # What a portion costs
//!
//! `value × units / quantity` on the lot it came out of, which is
//! `Money::apportioned` and therefore exact at `n/n`: the movement that empties
//! a lot costs whatever is left on it, so nothing is stranded and the lot
//! closes on exactly zero. Cost comes from the lot and never from an average
//! across the shelf — that is the whole of the difference between this module
//! and the one an earlier draft of it was.
//!
//! # What the lots cannot cover
//!
//! Reported as a [`Shortfall`], costed at the shelf's last known unit cost and
//! never averaged into a lot that does not exist. **A plain product's sale
//! records one** (R1) and everything else refuses it: a person holding the
//! goods who asks to write off more than is there is wrong about the goods, and
//! a lot- or serial-tracked shelf is meant to be known exactly. See
//! `commands::taken`.
//!
//! **A later receipt does not pay the debt.** The shortfall was costed at a
//! guess — the last unit cost — and netting the next delivery against it would
//! have to put the difference between that guess and what the delivery actually
//! cost somewhere, which is a price-variance account nobody has opened. So the
//! debt stands until a count settles it, which is decision 16's own answer, and
//! the delivery lands as the lot it is.

use erp_types::{Money, MoneyError};

/// One lot with something still on it.
///
/// **Open lots only.** A lot that empties closes and leaves the aggregate, so a
/// café receiving daily for years carries what is on the shelf and not what it
/// has ever bought. The read model keeps the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenLot {
    /// Derived from the receipt that created it — never minted (L8).
    pub id: String,
    /// The tenant's own word for this batch. Data, and it may repeat: two
    /// deliveries of `B-2026-04` are two lots, and [`Self::id`] keeps them
    /// apart.
    pub code: Option<String>,
    /// `None` on a lot of something that does not spoil, which sorts after
    /// everything dated. See the module doc.
    pub expires_on: Option<chrono::NaiveDate>,
    /// **Always more than nothing.** A lot at zero is closed and is not here.
    pub quantity: i64,
    /// What those units are still carried at, all of them together.
    pub value: Money,
    /// The units still on this lot, by name, for a serial-tracked product.
    /// Empty for every other product, and `quantity` is the whole story.
    pub serials: Vec<String>,
}

impl OpenLot {
    /// What `units` of this lot are carried at.
    ///
    /// Exact at the whole lot, so the last portion takes the remainder and the
    /// lot closes on nothing.
    fn cost_of(&self, units: i64) -> Result<Money, MoneyError> {
        self.value.apportioned(units, self.quantity)
    }

    /// What this lot gives up, **carrying everything a restoration needs to put
    /// it back** — see [`Portion::code`]. One constructor, so the three ways of
    /// picking cannot freeze three different subsets of the lot.
    ///
    /// **And what a count's overage puts onto it**, which is why `units` may be
    /// more than the lot holds: the found units are carried at this lot's own
    /// unit cost, so joining it leaves the lot costing what it cost.
    pub(crate) fn gives(&self, units: i64, serials: Vec<String>) -> Result<Portion, MoneyError> {
        Ok(Portion {
            lot: self.id.clone(),
            quantity: units,
            cost: self.cost_of(units)?,
            serials,
            code: self.code.clone(),
            expires_on: self.expires_on,
        })
    }
}

/// What a movement is asking for.
#[derive(Debug, Clone, Copy)]
pub enum Wanted<'a> {
    /// So many units, earliest-expiring first.
    Quantity(i64),
    /// So many units, all of them from this lot.
    From { lot: &'a str, quantity: i64 },
    /// **These** units. A serial is an identity, not a quantity.
    Serials(&'a [String]),
}

/// What one lot gave up.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Portion {
    pub lot: String,
    pub quantity: i64,
    /// What those units were carried at **on that lot** — frozen here, because
    /// by the next delivery the lot may be gone.
    pub cost: Money,
    /// Which units, when they have names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub serials: Vec<String>,
    /// **The lot's own batch code and date, frozen with the cost and for the
    /// same reason** — a lot this movement empties closes and leaves the
    /// aggregate, so by the time a customer brings the goods back there is
    /// nothing left to ask. `crate::restore_in` reopens the lot from here, with
    /// the date it will still spoil on; without them a returned carton of milk
    /// would come back undated and go out last.
    ///
    /// `#[serde(default)]` on both, so every portion written before they
    /// existed decodes as one with neither — which is what an untracked
    /// product's portion has anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_on: Option<chrono::NaiveDate>,
}

/// What the lots could not cover.
///
/// **A plain product's sale records one; every other movement refuses it.**
/// Revision R1: what is on the shelf of a lot- or serial-tracked product is
/// meant to be known exactly, and a phantom unit has no batch and no expiry —
/// so those refuse, and only an untracked product goes below zero, costed at
/// the last unit cost the shelf saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Shortfall {
    pub quantity: i64,
    /// At the shelf's last known unit cost. `None` when nothing has ever been
    /// received onto it — there is no purchase behind these units and no
    /// currency to state a zero in, so nothing is relieved from the asset
    /// either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Money>,
}

impl Shortfall {
    /// Two shortfalls as one — what a shelf owes after a second short sale.
    ///
    /// A cost is only present when a cost was known, so `None` and `Some` add
    /// to the `Some`: the units nothing was ever paid for contribute nothing,
    /// which is exactly what was booked for them.
    ///
    /// # Errors
    /// Arithmetic that will not fit, or two currencies on one shelf — which
    /// `receive` refuses before a lot can land.
    pub fn plus(self, other: Self) -> Result<Self, MoneyError> {
        Ok(Self {
            quantity: self.quantity.saturating_add(other.quantity),
            cost: match (self.cost, other.cost) {
                (Some(a), Some(b)) => Some(a.checked_add(b)?),
                (some, None) | (None, some) => some,
            },
        })
    }
}

/// Where a movement's units came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    /// **May be empty**, on a plain product whose shelf had nothing left: then
    /// the whole of it is the shortfall below.
    pub portions: Vec<Portion>,
    /// `None` when the lots covered the whole of it. A write-off, a named lot
    /// and every tracked product refuse anything else; a plain product's sale
    /// records it (R1).
    pub shortfall: Option<Shortfall>,
}

impl Picked {
    /// Everything taken, lots and shortfall together.
    #[must_use]
    pub fn quantity(&self) -> i64 {
        self.portions.iter().map(|p| p.quantity).sum::<i64>()
            + self.shortfall.map_or(0, |short| short.quantity)
    }
}

/// A movement that cannot be taken as asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PickError {
    #[error("a quantity is a whole number of the product's own unit, and more than nothing")]
    NotAQuantity,
    #[error("there is no open lot {0}")]
    NoSuchLot(String),
    #[error("lot {lot} holds {held} and {wanted} were asked for")]
    LotIsShort { lot: String, held: i64, wanted: i64 },
    #[error("{0} is not on the shelf")]
    NotOnHand(String),
    #[error(transparent)]
    Money(#[from] MoneyError),
}

/// **The picking rule.** See the module doc.
///
/// `last_unit_cost` is what a unit the lots cannot cover is charged at; it has
/// no bearing on anything the lots *do* cover.
///
/// # Errors
/// A lot that was named and is not open, a lot named and asked for more than it
/// holds, a serial that is not on the shelf, or arithmetic that will not fit.
pub fn pick(
    lots: &[OpenLot],
    wanted: Wanted<'_>,
    last_unit_cost: Option<Money>,
) -> Result<Picked, PickError> {
    match wanted {
        Wanted::Quantity(quantity) => by_quantity(lots, quantity, last_unit_cost),
        Wanted::From { lot, quantity } => from_one(lots, lot, quantity),
        Wanted::Serials(serials) => by_serial(lots, serials),
    }
}

/// The lots a take walks, in the order it walks them.
///
/// `false` sorts before `true`, so everything dated comes before everything
/// undated; among dated lots the earlier date wins. The sort is **stable** and
/// `lots` is in received order, so undated lots — and lots sharing a date —
/// come out oldest first without a second key.
///
/// **A count's overage joins the other end of it** (`commands::counted`): one
/// order for stock leaving and for stock found, so the lot a shortage comes off
/// last is the lot an overage lands on. Walking `lots` from its own end instead
/// was a second order, and a return reopening an old batch at the end of the
/// list made it disagree with this one.
pub(crate) fn earliest_first(lots: &[OpenLot]) -> Vec<&OpenLot> {
    let mut order: Vec<&OpenLot> = lots.iter().collect();
    order.sort_by_key(|lot| (lot.expires_on.is_none(), lot.expires_on));
    order
}

fn by_quantity(
    lots: &[OpenLot],
    quantity: i64,
    last_unit_cost: Option<Money>,
) -> Result<Picked, PickError> {
    if quantity <= 0 {
        return Err(PickError::NotAQuantity);
    }

    let mut left = quantity;
    let mut portions = Vec::new();
    for lot in earliest_first(lots) {
        if left == 0 {
            break;
        }
        let take = left.min(lot.quantity);
        portions.push(lot.gives(take, Vec::new())?);
        left -= take;
    }

    Ok(Picked {
        portions,
        shortfall: (left > 0)
            .then(|| {
                Ok::<_, MoneyError>(Shortfall {
                    quantity: left,
                    cost: last_unit_cost
                        .map(|unit| unit.checked_mul_int(left))
                        .transpose()?,
                })
            })
            .transpose()?,
    })
}

fn from_one(lots: &[OpenLot], named: &str, quantity: i64) -> Result<Picked, PickError> {
    if quantity <= 0 {
        return Err(PickError::NotAQuantity);
    }
    let lot = lots
        .iter()
        .find(|lot| lot.id == named)
        .ok_or_else(|| PickError::NoSuchLot(named.to_owned()))?;
    if quantity > lot.quantity {
        return Err(PickError::LotIsShort {
            lot: named.to_owned(),
            held: lot.quantity,
            wanted: quantity,
        });
    }

    Ok(Picked {
        portions: vec![lot.gives(quantity, Vec::new())?],
        shortfall: None,
    })
}

/// **Named units, and every one of them has to be there.**
///
/// A serial that is unknown, already gone, or named twice in one movement is
/// refused (L6). Decision 7's *"never refuse for stock"* governs quantities: a
/// count corrects a quantity, and nothing corrects a unit that was never on the
/// shelf.
///
/// Portions come out in lot order rather than in the order the serials were
/// given, so one lot named twice is one portion and the costing divides once.
fn by_serial(lots: &[OpenLot], serials: &[String]) -> Result<Picked, PickError> {
    if serials.is_empty() {
        return Err(PickError::NotAQuantity);
    }

    let mut taken: Vec<(usize, Vec<String>)> = Vec::new();
    for serial in serials {
        let at = lots
            .iter()
            .position(|lot| lot.serials.iter().any(|held| held == serial))
            .ok_or_else(|| PickError::NotOnHand(serial.clone()))?;
        match taken.iter_mut().find(|(lot, _)| *lot == at) {
            Some((_, named)) if named.iter().any(|seen| seen == serial) => {
                return Err(PickError::NotOnHand(serial.clone()));
            }
            Some((_, named)) => named.push(serial.clone()),
            None => taken.push((at, vec![serial.clone()])),
        }
    }

    // **Lot order, not the order the serials were typed in**, so the same two
    // units always produce the same portions in the same `seq` — which is what
    // makes a movement's rows stable across a rebuild.
    taken.sort_by_key(|(at, _)| *at);

    taken
        .into_iter()
        .map(|(at, named)| {
            let units = i64::try_from(named.len()).unwrap_or(i64::MAX);
            Ok(lots[at].gives(units, named)?)
        })
        .collect::<Result<Vec<_>, PickError>>()
        .map(|portions| Picked {
            portions,
            shortfall: None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_types::CurrencyCode;

    fn sar(minor: i64) -> Money {
        Money::from_minor(
            minor,
            CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!("SAR is a real code")),
        )
    }

    fn day(literal: &str) -> chrono::NaiveDate {
        literal.parse().unwrap_or_else(|_| unreachable!("a date"))
    }

    fn lot(id: &str, expires_on: Option<&str>, quantity: i64, value: i64) -> OpenLot {
        OpenLot {
            id: id.to_owned(),
            code: None,
            expires_on: expires_on.map(day),
            quantity,
            value: sar(value),
            serials: Vec::new(),
        }
    }

    fn from(picked: &Picked) -> Vec<(&str, i64, i64)> {
        picked
            .portions
            .iter()
            .map(|p| (p.lot.as_str(), p.quantity, p.cost.minor()))
            .collect()
    }

    /// **The rule.** Received first is not taken first — expiring first is.
    #[test]
    fn the_earliest_expiry_goes_out_first() {
        let lots = [
            lot("l1", Some("2026-06-01"), 10, 1_000),
            lot("l2", Some("2026-04-01"), 10, 2_000),
            lot("l3", Some("2026-05-01"), 10, 3_000),
        ];

        let picked = pick(&lots, Wanted::Quantity(25), None).expect("picks");
        assert_eq!(
            from(&picked),
            vec![("l2", 10, 2_000), ("l3", 10, 3_000), ("l1", 5, 500)],
            "April, then May, then half of June"
        );
        assert_eq!(picked.shortfall, None);
    }

    /// **An undated lot sorts after every dated one, and undated lots go out
    /// oldest first** — which is plain FIFO, down this same function, for a
    /// product nobody tracks batches of.
    #[test]
    fn an_undated_lot_waits_and_then_goes_oldest_first() {
        let lots = [
            lot("old", None, 5, 500),
            lot("new", None, 5, 900),
            lot("dated", Some("2027-01-01"), 5, 100),
        ];

        let picked = pick(&lots, Wanted::Quantity(15), None).expect("picks");
        assert_eq!(
            from(&picked),
            vec![("dated", 5, 100), ("old", 5, 500), ("new", 5, 900)],
            "the dated lot first however late it expires, then received order"
        );
    }

    /// A lot may be named — a recall, a scanned batch — and then the rule does
    /// not apply.
    #[test]
    fn naming_a_lot_overrides_the_order() {
        let lots = [
            lot("l1", Some("2026-04-01"), 10, 1_000),
            lot("l2", Some("2026-09-01"), 10, 4_000),
        ];

        let picked = pick(
            &lots,
            Wanted::From {
                lot: "l2",
                quantity: 4,
            },
            None,
        )
        .expect("picks");
        assert_eq!(from(&picked), vec![("l2", 4, 1_600)]);

        assert_eq!(
            pick(
                &lots,
                Wanted::From {
                    lot: "l2",
                    quantity: 11
                },
                None
            ),
            Err(PickError::LotIsShort {
                lot: "l2".to_owned(),
                held: 10,
                wanted: 11,
            }),
            "a named lot that cannot cover it is refused, not topped up"
        );
        assert_eq!(
            pick(
                &lots,
                Wanted::From {
                    lot: "l9",
                    quantity: 1
                },
                None
            ),
            Err(PickError::NoSuchLot("l9".to_owned())),
        );
    }

    /// **A part of a lot costs its share, and the rest of it costs the rest** —
    /// the last portion takes the remainder, so the lot closes on nothing.
    #[test]
    fn the_last_portion_of_a_lot_takes_the_remainder() {
        let three = [lot("l1", None, 3, 10_000)];

        // One at a time, the way three sessions of a hundred-riyal package go.
        let mut left = three[0].clone();
        let mut paid = 0;
        for expected in [3_333, 3_334, 3_333] {
            let picked =
                pick(std::slice::from_ref(&left), Wanted::Quantity(1), None).expect("picks");
            let portion = &picked.portions[0];
            assert_eq!(portion.cost.minor(), expected);
            paid += portion.cost.minor();
            left = OpenLot {
                quantity: left.quantity - 1,
                value: left.value.checked_sub(portion.cost).expect("same currency"),
                ..left
            };
        }
        assert_eq!(paid, 10_000, "a halala was stranded on an empty lot");
        assert_eq!(left.quantity, 0);
        assert_eq!(left.value, sar(0));
    }

    /// **What the lots cannot cover comes back as a shortfall**, costed at the
    /// last unit cost and never averaged into a lot that does not exist.
    #[test]
    fn what_the_lots_cannot_cover_is_a_shortfall() {
        let lots = [lot("l1", None, 2, 5_000)];

        let picked = pick(&lots, Wanted::Quantity(5), Some(sar(2_500))).expect("picks");
        assert_eq!(from(&picked), vec![("l1", 2, 5_000)]);
        assert_eq!(
            picked.shortfall,
            Some(Shortfall {
                quantity: 3,
                cost: Some(sar(7_500)),
            }),
        );
        assert_eq!(picked.quantity(), 5);

        // Nothing was ever received, so there is no cost to attribute and no
        // currency to state a zero in.
        let picked = pick(&[], Wanted::Quantity(4), None).expect("picks");
        assert_eq!(
            picked.shortfall,
            Some(Shortfall {
                quantity: 4,
                cost: None
            }),
        );
    }

    /// A serial names the unit it takes, and an unknown one is refused rather
    /// than counted.
    #[test]
    fn a_serial_is_found_or_refused() {
        let lots = [
            OpenLot {
                serials: vec!["SN-1".to_owned(), "SN-2".to_owned()],
                ..lot("l1", None, 2, 20_000)
            },
            OpenLot {
                serials: vec!["SN-3".to_owned()],
                ..lot("l2", None, 1, 30_000)
            },
        ];

        let picked = pick(
            &lots,
            Wanted::Serials(&["SN-3".to_owned(), "SN-1".to_owned()]),
            None,
        )
        .expect("picks");
        assert_eq!(
            from(&picked),
            vec![("l1", 1, 10_000), ("l2", 1, 30_000)],
            "each unit costs what its own lot carries it at"
        );

        for named in [
            vec!["SN-9".to_owned()],
            vec!["SN-1".to_owned(), "SN-1".to_owned()],
        ] {
            assert_eq!(
                pick(&lots, Wanted::Serials(&named), None),
                Err(PickError::NotOnHand(named[0].clone())),
                "a serial that is not on the shelf, or named twice, is refused"
            );
        }
    }

    /// Nothing is not a quantity, in either shape.
    #[test]
    fn nothing_is_not_a_movement() {
        let lots = [lot("l1", None, 5, 500)];
        assert_eq!(
            pick(&lots, Wanted::Quantity(0), None),
            Err(PickError::NotAQuantity)
        );
        assert_eq!(
            pick(
                &lots,
                Wanted::From {
                    lot: "l1",
                    quantity: -1
                },
                None
            ),
            Err(PickError::NotAQuantity)
        );
        assert_eq!(
            pick(&lots, Wanted::Serials(&[]), None),
            Err(PickError::NotAQuantity)
        );
    }
}
