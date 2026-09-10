//! What a booking costs.
//!
//! # One function, and it touches nothing
//!
//! [`price`] takes a charge and returns what it comes to. **No database, no
//! configuration, no clock.** Everything that varies — the rate, which band
//! applied, what was taken off — is an argument, so the arithmetic can be
//! tested without a tenant and cannot drift when somebody changes a setting.
//!
//! The impure half is [`Tariff::band_for`], which reads a span and says which
//! window it falls in. That is resolved inside the booking's own transaction
//! and **frozen onto the line** (L5), so a tenant who moves their peak hours
//! next month does not restate what was booked this month.
//!
//! # Where this diverges from the system it was measured against
//!
//! Its pricing engine takes floating-point amounts, and its own docblock
//! records three implementations that disagreed — every fixed discount
//! differing by exactly the tax on it. Everything here is [`Money`], which is
//! integer minor units, and the one place a rate is applied is
//! [`Money::scaled_by`], which has one rounding rule and says what it is.
//!
//! # Tax-exclusive, and why the tax is not here
//!
//! An allowance comes off the **net**, and tax is charged on what remains. That
//! is how ZATCA models a `cac:AllowanceCharge` and it is the difference between
//! a discount and a credit note: a discounted booking was never for the larger
//! amount, so the smaller one is what is taxed and what is declared.
//!
//! No tax is computed here, and that is the point. A reservation is not a tax
//! document. The allowances travel with the line to `sales` when it is
//! invoiced, where they become allowances on the invoice and reduce the band
//! they come off — so the tax-exclusive property is what falls out, rather than
//! something two modules each have to remember.

use erp_types::{CurrencyCode, Money, MoneyError};
use serde::{Deserialize, Serialize};

use erp_occupancy::Span;
use erp_recurrence::Availability;
use erp_recurrence::Calendar;
use erp_rules::{Authored, DynCondition, Facts, Rule, Rules};

/// Something taken off a line, and why.
///
/// # Why this is not just a smaller rate
///
/// The same reason `sales::Discount` is not a negative line: a reduced rate is
/// invisible on the document. A customer sees a smaller number and nothing says
/// why, and the business cannot answer "how much did we give away in loyalty
/// discounts this month" without guessing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allowance {
    /// Why. A customer reads it, so it is text and not a code.
    pub reason: String,
    /// What comes off, **positive**. A negative allowance is a surcharge, which
    /// is a different element and a different conversation.
    pub amount: Money,
}

/// One window of a price list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Band {
    /// What the business calls it. Printed beside the price, so "Peak" and
    /// "Ramadan evenings" rather than an index.
    pub name: String,
    /// When it applies.
    ///
    /// **The same recurrence that says when a resource is offered.** "Open
    /// Thursday evening" and "dearer Thursday evening" are one shape, and
    /// having one type for both means a tenant learns the calendar rules once.
    pub when: Availability,
    /// What it does to the rate, in basis points. `2500` is a quarter more;
    /// `-1000` is a tenth off, which is what an off-peak band is.
    ///
    /// A rate rather than an amount because a salon's peak supplement is a
    /// percentage of whatever the service costs, and an absolute one would have
    /// to be restated every time a price changed.
    pub uplift: i32,
}

/// Whether strangers may write into this business's diary, and on what terms.
///
/// # Why this is off until somebody turns it on
///
/// The two public *reads* are safe by their nature: a shop's own front page is
/// what they are. A public **write** claims a real slot in a real diary, and an
/// unauthenticated one can be made by anybody — so a salon that has never asked
/// for online booking must not find their week full of appointments nobody
/// intends to keep.
///
/// The rate limiter bounds how *fast* that can happen; it does not make it
/// something a business did not agree to. So this is a tenant's own decision,
/// stored where their other decisions are, and its default is no.
///
/// # What a deposit does now
///
/// [`Self::deposit_bp`] is a fraction of what the booking was priced at, and it
/// is **taken before the slot is held**: the reservation records what was asked
/// for and by when, and the booking is not secured until something says the
/// money arrived. An unpaid hold lapses at [`Self::hold_minutes`].
///
/// The fraction applies to the booking's **net** — what it comes to before tax
/// — because receiving the money is itself a tax point and whoever raises the
/// document for it works the tax forward from a net. See
/// `crate::Deposit`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicBooking {
    /// Off by default. **The absence of a setting is a no**, not a yes.
    pub open: bool,
    /// What fraction of the booking a deposit would be, in basis points.
    ///
    /// Recorded, not charged — see above. Zero means none.
    #[serde(default)]
    pub deposit_bp: u32,
    /// **How long a slot is held for somebody who has not paid.**
    ///
    /// Zero, the default, means indefinitely — which is right for a business
    /// that asks for no deposit, and wrong for one that does: a slot held for
    /// somebody who never pays is a slot nobody else could take.
    ///
    /// Stamped onto the booking when it is made, so changing this does not move
    /// a deadline somebody was already given (L5).
    #[serde(default)]
    pub hold_minutes: u32,
    /// **Whether a public booker has to prove their phone number first.**
    ///
    /// Off by default, and that is a judgement rather than a shrug: what stops
    /// a booking form being spammed is the **deposit**, not a verified number —
    /// a slot that cannot be held without paying for it cannot be spammed by
    /// anybody. What verifying buys is being able to *reach* somebody: to send
    /// the reminder, to ring when the stylist is ill, to tell a real customer
    /// from a typo.
    ///
    /// So it is the business's call. A clinic that must reach patients turns it
    /// on; a salon taking a deposit on every booking has what it needs already,
    /// and a second step before a stranger can book costs them bookings.
    ///
    /// See `crate::verification` for what a code is and is not.
    #[serde(default)]
    pub verify_phone: bool,
}

impl PublicBooking {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "booking.public";

    /// What this tenant has configured, or the closed default.
    ///
    /// A tenant who has stored something unusable gets an error rather than the
    /// default, the same way `Tariff` does — but note the asymmetry that makes
    /// this safe either way: the default here is *closed*, so the failure mode
    /// of an unreadable setting is a booking page that stops working, never one
    /// that opens up.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or_else(Self::default, |configured| configured.value))
    }
}

/// A tenant's price bands. Configuration, resolved at the moment of booking.
///
/// **Bands, not prices.** What a service costs is the caller's to send; when it
/// costs more is the tenant's to configure, and it is the half that must not be
/// something a client can decide for itself.
/// **When a booking is billed.**
///
/// The desk can always raise the invoice for a booking on demand. This says
/// whether the worker does it too, the moment a booking is completed — right
/// for a salon that never adds anything at the till, and wrong for one that
/// does, which is why it is off until the business turns it on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Billing {
    /// Raise the invoice when a booking is moved to completed.
    #[serde(default)]
    pub on_completion: bool,
}

impl Billing {
    pub const KEY: &'static str = "booking.billing";

    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or_else(Self::default, |configured| configured.value))
    }
}

/// **Why a booking was priced the way it was.**
///
/// `erp_rules::Explained` borrows from the rule set it explained; a tariff
/// builds its rules for the call, so this is the owned answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceExplained {
    /// The band that applied, and its uplift in basis points.
    pub matched: Option<(String, i32)>,
    /// Every band tried, in order, and whether it matched. At most one did, and
    /// it is the last — the evaluator stops there.
    pub considered: Vec<(String, bool)>,
}

/// **The tariff as the tenant wrote it**, which is what is stored.
///
/// A band written from a template holds its *answers*, and the band is rebuilt
/// from them by [`Self::resolve`] on every read. Nothing here holds both, so a
/// screen showing somebody their form and the engine pricing their booking are
/// reading the same thing. See `crate::templates`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TariffAsWritten {
    /// **First match wins**, so the order is the tenant's priority. A specific
    /// window — a public holiday — goes above a general one.
    pub bands: Vec<Authored<Band>>,
}

impl TariffAsWritten {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "booking.tariff";

    /// The bands these come to, rebuilding every templated one.
    ///
    /// # Errors
    /// [`ConfigError::Invalid`](erp_eventlog::ConfigError::Invalid) if a band
    /// names a template this build no longer ships, or its answers no longer
    /// fill one. **Refuses rather than dropping the band** (L6): a tariff
    /// silently missing its peak rate is a month of underbilling nobody
    /// notices.
    pub fn resolve(&self) -> Result<Tariff, erp_eventlog::ConfigError> {
        self.bands
            .iter()
            .map(|band| band.rule(crate::templates::TARIFF_TEMPLATES))
            .collect::<Result<Vec<_>, _>>()
            .map(|bands| Tariff { bands })
            .map_err(|why| erp_eventlog::ConfigError::Invalid {
                key: Self::KEY.to_owned(),
                reason: why.to_string(),
            })
    }
}

/// **The tariff as it applies**, which is what prices a booking.
///
/// Not stored and not on the wire: it is what [`TariffAsWritten::resolve`]
/// produces. Everything downstream — [`Self::band_for`], [`price`] — works on
/// this, so how a band was authored is a question only the settings screen
/// ever asks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tariff {
    /// First match wins, in the order the tenant wrote them.
    pub bands: Vec<Band>,
}

impl Tariff {
    /// What this tenant has configured, or nothing.
    ///
    /// An empty tariff is the shipped default and it means every hour is the
    /// same price, which is what most businesses want and all of them start
    /// with. A tenant who *has* configured one and stored something unusable
    /// gets an error rather than silently losing their peak rates.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        erp_eventlog::configuration::get::<TariffAsWritten>(conn, TariffAsWritten::KEY)
            .await?
            .map_or_else(TariffAsWritten::default, |configured| configured.value)
            .resolve()
    }

    /// The band a span falls in, if any.
    ///
    /// **The whole span, not the start.** A treatment that begins before peak
    /// and runs into it is charged at the base rate, because the alternative —
    /// charging peak for an appointment that mostly was not — is the answer a
    /// customer argues with. A business that wants the other rule splits the
    /// booking, which is what they would do at the till anyway.
    #[must_use]
    pub fn band_for(&self, span: Span, calendar: Calendar) -> Option<&Band> {
        let at = self.explain(span, calendar);
        at.matched.as_ref()?;
        // `explain` decides; this returns the band it decided on. Two
        // implementations of "which band wins" would disagree eventually, and
        // the disagreement would be a price nobody could account for.
        //
        // **By position, not by name.** `considered` is every band tried in
        // order up to and including the winner, so the winner is its last
        // entry — and two bands a tenant happened to give the same name stay
        // two bands. Looking the name up would have quietly priced the second
        // at the first one's rate.
        self.bands.get(at.considered.len() - 1)
    }

    /// **Which band applies, and every one considered getting there.**
    ///
    /// The question a tenant asks when a price surprises them, answered by the
    /// evaluator that decided it rather than by a second reading of the rules.
    ///
    /// Owned rather than borrowed: the rule set is built for the call, so
    /// there is nothing for a borrow to point at afterwards. The names are the
    /// tenant's own and are short.
    ///
    /// ponytail: builds the rule set per call — a handful of small clones
    /// against a booking that has already made several database round trips.
    /// Hold it on `Tariff` if a profile ever says otherwise.
    #[must_use]
    pub fn explain(&self, span: Span, calendar: Calendar) -> PriceExplained {
        let rules = self.rules();
        let decided = rules.explain(&Facts::new().over(span, calendar));
        PriceExplained {
            matched: decided.matched.map(|rule| (rule.name.clone(), rule.then)),
            considered: decided
                .considered
                .iter()
                .map(|c| (c.name.to_owned(), c.matched))
                .collect(),
        }
    }

    /// This tariff as rules the engine evaluates.
    ///
    /// **A `Band` is still `{ name, when, uplift }`.** What a tenant writes did
    /// not change when the engine took over evaluating it, and did not change
    /// again when templates arrived — a templated band produces exactly this,
    /// which is what "all producing the same artifact" means.
    #[must_use]
    pub fn rules(&self) -> Rules<i32> {
        Rules::new(
            self.bands
                .iter()
                .map(|band| Rule {
                    name: band.name.clone(),
                    when: DynCondition::Covers { window: band.when },
                    then: band.uplift,
                })
                .collect(),
        )
    }
}

/// What a caller asks for: a rate, how many, and what comes off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Charge {
    /// The list rate for one of whatever this line is.
    pub rate: Money,
    /// How many. Four covers at a per-cover rate, twelve places in a class,
    /// three nights at a nightly one.
    pub quantity: u16,
    pub allowances: Vec<Allowance>,
}

/// What a line came to, frozen onto it.
///
/// Every input is here beside the answer, because "why is this 92 riyals" is a
/// question a receptionist is asked at the counter and the log is the only
/// place that can answer it a year later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Charged {
    /// The list rate that applied, before the band.
    pub rate: Money,
    pub quantity: u16,
    /// The band that applied and what it did, or absent for the base rate.
    /// Frozen: a tenant moving their peak hours does not restate this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub band: Option<Applied>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowances: Vec<Allowance>,
    /// What the line comes to before any allowance — rate, banded, times
    /// quantity.
    pub gross: Money,
    /// **What is charged, before tax.** `gross` less every allowance.
    pub net: Money,
}

/// A band, as it was when it applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    pub name: String,
    pub uplift: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PriceError {
    #[error("a rate cannot be negative")]
    NotARate,
    #[error("a line must be for at least one")]
    NothingCharged,
    /// Stated as the amount taken off, so it is positive. A negative one is a
    /// surcharge, which is a different element.
    #[error("an allowance must be a positive amount")]
    NotAnAllowance,
    #[error("an allowance cannot be larger than what it is taken off")]
    AllowanceTooLarge,
    #[error("every amount on a line must be in the same currency")]
    MixedCurrencies,
    #[error("that amount is too large to record")]
    OutOfRange,
}

impl erp_i18n::Localize for PriceError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::Message;
        match self {
            Self::NotARate => Message::new(messages::NOT_A_RATE),
            Self::NothingCharged => Message::new(messages::NOTHING_CHARGED),
            Self::NotAnAllowance => Message::new(messages::NOT_AN_ALLOWANCE),
            Self::AllowanceTooLarge => Message::new(messages::ALLOWANCE_TOO_LARGE),
            Self::MixedCurrencies => Message::new(messages::MIXED_CURRENCIES),
            Self::OutOfRange => Message::new(messages::AMOUNT_OUT_OF_RANGE),
        }
    }
}

impl From<MoneyError> for PriceError {
    fn from(error: MoneyError) -> Self {
        match error {
            MoneyError::CurrencyMismatch { .. } => Self::MixedCurrencies,
            MoneyError::Overflow { .. } | MoneyError::DivideByZero => Self::OutOfRange,
        }
    }
}

/// **The one pricing function.**
///
/// `band` is what the tariff resolved for this line's hour, already looked up.
/// Passing it in rather than looking it up is what keeps this pure: the same
/// arguments give the same answer for ever, which is what makes a replay
/// reproduce a booking's price rather than today's.
///
/// # The order of operations, and why it is this one
///
/// The band moves the **rate**, then quantity multiplies, then allowances come
/// off the total. Applying the band after the multiplication gives the same
/// answer only when the rounding does not bite, and it bites at exactly the
/// prices businesses use: a 33.33 service at a 25% peak is 41.66 each, so four
/// of them are 166.64 — banding the total instead gives 166.65 and a customer
/// who checks the arithmetic finds a halala nobody can explain.
pub fn price(charge: &Charge, band: Option<&Band>) -> Result<Charged, PriceError> {
    if charge.rate.is_negative() {
        return Err(PriceError::NotARate);
    }
    if charge.quantity == 0 {
        return Err(PriceError::NothingCharged);
    }

    let currency = charge.rate.currency();
    let banded = match band {
        Some(band) => apply(charge.rate, band.uplift)?,
        None => charge.rate,
    };
    let gross = banded.checked_mul_int(i64::from(charge.quantity))?;

    let mut net = gross;
    for allowance in &charge.allowances {
        if allowance.amount.currency() != currency {
            return Err(PriceError::MixedCurrencies);
        }
        if !allowance.amount.is_positive() {
            return Err(PriceError::NotAnAllowance);
        }
        net = net.checked_sub(allowance.amount)?;
    }
    if net.is_negative() {
        return Err(PriceError::AllowanceTooLarge);
    }

    Ok(Charged {
        rate: charge.rate,
        quantity: charge.quantity,
        band: band.map(|b| Applied {
            name: b.name.clone(),
            uplift: b.uplift,
        }),
        allowances: charge.allowances.clone(),
        gross,
        net,
    })
}

/// A rate with an uplift on it.
///
/// `10_000` basis points is the rate itself, so the uplift is added to par
/// rather than being the multiplier — `2500` means a quarter more, which is
/// what a person writing it means.
fn apply(rate: Money, uplift: i32) -> Result<Money, PriceError> {
    let multiplier = 10_000_i32
        .checked_add(uplift)
        .ok_or(PriceError::OutOfRange)?;
    if multiplier < 0 {
        // An uplift below -100% would make a service cost the business money.
        return Err(PriceError::NotARate);
    }
    Ok(rate.scaled_by(multiplier)?)
}

/// What a whole reservation comes to, before tax.
///
/// Summed over the lines that have a price. A reservation where nothing is
/// priced comes to zero in the currency asked for, which is different from
/// having no answer.
pub fn total(
    charged: impl IntoIterator<Item = Money>,
    currency: CurrencyCode,
) -> Result<Money, PriceError> {
    Ok(Money::checked_sum(charged, currency)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sar() -> CurrencyCode {
        CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!("a real code"))
    }
    fn m(minor: i64) -> Money {
        Money::from_minor(minor, sar())
    }
    fn charge(rate: i64, quantity: u16, allowances: &[(&str, i64)]) -> Charge {
        Charge {
            rate: m(rate),
            quantity,
            allowances: allowances
                .iter()
                .map(|(reason, amount)| Allowance {
                    reason: (*reason).to_owned(),
                    amount: m(*amount),
                })
                .collect(),
        }
    }
    fn band(uplift: i32) -> Band {
        Band {
            name: "Peak".to_owned(),
            when: Availability::always().unwrap_or_else(|_| unreachable!("always is a rule")),
            uplift,
        }
    }

    #[test]
    fn a_plain_line_is_the_rate_times_the_quantity() {
        let priced = price(&charge(8_000, 3, &[]), None).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(priced.gross, m(24_000));
        assert_eq!(priced.net, m(24_000));
        assert!(priced.band.is_none());
    }

    /// **The band moves the rate, and the rate is what quantity multiplies.**
    ///
    /// 33.33 at a quarter more is 41.66 each, so four are 166.64. Banding the
    /// total instead gives 166.65, and that halala is the whole reason the
    /// order of operations is written down.
    #[test]
    fn a_band_is_applied_to_the_rate_and_not_to_the_total() {
        let priced =
            price(&charge(3_333, 4, &[]), Some(&band(2_500))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(priced.gross, m(16_664));

        let banded_total = m(3_333)
            .checked_mul_int(4)
            .and_then(|total| total.scaled_by(12_500))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(banded_total, m(16_665), "the wrong order, for the record");
    }

    #[test]
    fn an_off_peak_band_takes_the_rate_down() {
        let priced =
            price(&charge(8_000, 1, &[]), Some(&band(-1_000))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(priced.net, m(7_200));
        assert_eq!(
            priced.band.as_ref().map(|b| b.uplift),
            Some(-1_000),
            "the band was not frozen onto the line"
        );
    }

    /// **An allowance comes off the net, and nothing here computes tax.**
    ///
    /// The tax-exclusive property is what falls out when the allowance travels
    /// to `sales` and reduces the band it comes off, rather than something two
    /// modules each have to remember.
    #[test]
    fn an_allowance_comes_off_what_is_charged() {
        let priced = price(&charge(10_000, 2, &[("عرض الافتتاح", 2_500)]), None)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(priced.gross, m(20_000));
        assert_eq!(priced.net, m(17_500));
        assert_eq!(priced.allowances.len(), 1);
    }

    #[test]
    fn a_line_refuses_what_is_not_a_price() {
        assert_eq!(price(&charge(-1, 1, &[]), None), Err(PriceError::NotARate));
        assert_eq!(
            price(&charge(100, 0, &[]), None),
            Err(PriceError::NothingCharged)
        );
        assert_eq!(
            price(&charge(100, 1, &[("no", 0)]), None),
            Err(PriceError::NotAnAllowance)
        );
        assert_eq!(
            price(&charge(100, 1, &[("no", -5)]), None),
            Err(PriceError::NotAnAllowance)
        );
        assert_eq!(
            price(&charge(100, 1, &[("too much", 101)]), None),
            Err(PriceError::AllowanceTooLarge)
        );
        // A whole discount is legal: a comped appointment is still a booking.
        assert_eq!(
            price(&charge(100, 1, &[("مجاملة", 100)]), None)
                .unwrap_or_else(|e| panic!("{e}"))
                .net,
            m(0)
        );
        // And an uplift that would make the business pay is not.
        assert_eq!(
            price(&charge(100, 1, &[]), Some(&band(-20_000))),
            Err(PriceError::NotARate)
        );
    }

    #[test]
    fn a_price_in_another_currency_is_refused_rather_than_added() {
        let usd = CurrencyCode::new("USD").unwrap_or_else(|_| unreachable!("a real code"));
        let mixed = Charge {
            rate: m(10_000),
            quantity: 1,
            allowances: vec![Allowance {
                reason: "x".to_owned(),
                amount: Money::from_minor(100, usd),
            }],
        };
        assert_eq!(price(&mixed, None), Err(PriceError::MixedCurrencies));
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;

    fn peak() -> Band {
        Band {
            name: "Thursday peak".to_owned(),
            when: Availability::from_parts(&[], &[4], &[], 17 * 60, 21 * 60, None, None)
                .expect("a window"),
            uplift: 2_500,
        }
    }

    fn base() -> Band {
        Band {
            name: "Base".to_owned(),
            when: Availability::from_parts(&[], &[], &[], 0, 24 * 60, None, None)
                .expect("a window"),
            uplift: 0,
        }
    }

    /// One hour starting at `hour` **on the tenant's clock**, which is Riyadh
    /// (`+03:00`) by default. A window is written in local time, so a span
    /// built in UTC would test a different hour than the one it names — the
    /// mistake this helper exists to stop making twice.
    fn at(day: &str, hour: u32) -> Span {
        let from: erp_types::Timestamp = format!("{day}T{hour:02}:00:00+03:00")
            .parse()
            .expect("an instant");
        Span::new(from, from + chrono::Duration::hours(1)).expect("a span")
    }

    /// **The tariff a tenant already stored must price the same.**
    ///
    /// This refactor moved who evaluates a band, not what a band means. If this
    /// fails, somebody's salon was silently repriced.
    #[test]
    fn the_engine_picks_the_band_the_old_matcher_would_have() {
        let tariff = Tariff {
            bands: vec![peak(), base()],
        };
        let calendar = Calendar::default();

        for (span, expected) in [
            // 2026-05-07 is a Thursday.
            // Thursday, inside 17:00–21:00 local.
            (at("2026-05-07", 18), Some("Thursday peak")),
            (at("2026-05-07", 10), Some("Base")),
            // Wednesday at the same hour: the peak band is Thursday only.
            (at("2026-05-06", 18), Some("Base")),
        ] {
            let by_engine = tariff.band_for(span, calendar).map(|b| b.name.as_str());
            // What `find(|band| band.when.covers(..))` answered before.
            let by_hand = tariff
                .bands
                .iter()
                .find(|band| band.when.covers(span, calendar))
                .map(|b| b.name.as_str());
            assert_eq!(by_engine, expected);
            assert_eq!(by_engine, by_hand, "the engine and the old matcher agree");
        }
    }

    #[test]
    fn explain_names_the_bands_tried_and_stops_at_the_winner() {
        let tariff = Tariff {
            bands: vec![peak(), base()],
        };
        let why = tariff.explain(at("2026-05-07", 10), Calendar::default());

        assert_eq!(
            why.matched.as_ref().map(|(n, u)| (n.as_str(), *u)),
            Some(("Base", 0))
        );
        assert_eq!(
            why.considered,
            vec![
                ("Thursday peak".to_owned(), false),
                ("Base".to_owned(), true)
            ],
            "a tenant asking why can see the peak band was tried and missed"
        );
    }

    #[test]
    fn band_for_and_explain_never_disagree() {
        let tariff = Tariff {
            bands: vec![peak(), base()],
        };
        let calendar = Calendar::default();
        for hour in 0..24 {
            let span = at("2026-05-07", hour);
            assert_eq!(
                tariff.band_for(span, calendar).map(|b| b.name.clone()),
                tariff.explain(span, calendar).matched.map(|(n, _)| n),
                "at {hour}:00"
            );
        }
    }

    /// **Two bands a tenant gave the same name stay two bands.**
    ///
    /// A form makes this easy to do by accident — two evenings, both called
    /// "Peak" — and looking the winner up by name would have quietly priced
    /// the second at the first one's rate.
    #[test]
    fn two_bands_with_the_same_name_stay_two_bands() {
        let tariff = Tariff {
            bands: vec![
                Band {
                    name: "Peak".to_owned(),
                    when: Availability::from_parts(&[], &[4], &[], 17 * 60, 21 * 60, None, None)
                        .expect("a window"),
                    uplift: 2_500,
                },
                Band {
                    name: "Peak".to_owned(),
                    when: Availability::from_parts(&[], &[], &[], 0, 24 * 60, None, None)
                        .expect("a window"),
                    uplift: 500,
                },
            ],
        };

        // 10:00 Thursday: outside the first band, inside the second.
        assert_eq!(
            tariff.band_for(at("2026-05-07", 10), Calendar::default()),
            Some(&tariff.bands[1]),
            "the second band applied, so its own uplift is what is charged"
        );
    }

    /// A band written from a template comes back as the band its answers
    /// describe, and prices exactly as a hand-written one would.
    #[test]
    fn a_templated_band_resolves_to_what_its_answers_describe() {
        let answers = [
            ("name", erp_rules::Value::Text("ذروة الخميس".to_owned())),
            ("weekday", erp_rules::Value::Int(4)),
            ("from_hour", erp_rules::Value::Int(17)),
            ("percent", erp_rules::Value::Int(25)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let written = TariffAsWritten {
            bands: vec![
                Authored::written(
                    crate::templates::TARIFF_TEMPLATES,
                    "weekday_evening",
                    answers,
                )
                .expect("filled in"),
            ],
        };

        let tariff = written.resolve().expect("it builds");

        assert_eq!(
            tariff.band_for(at("2026-05-07", 18), Calendar::default()),
            Some(&tariff.bands[0])
        );
        assert_eq!(tariff.bands[0].uplift, 2_500);
    }

    /// **A withdrawn template does not quietly drop the band.**
    ///
    /// A tariff silently missing its peak rate is a month of underbilling
    /// nobody notices, which is what L6 refuses on behalf of.
    #[test]
    fn a_band_naming_a_template_this_build_does_not_ship_refuses_the_whole_tariff() {
        let written = TariffAsWritten {
            bands: vec![Authored::Preset {
                template: "seasonal".to_owned(),
            }],
        };

        let why = written.resolve().expect_err("there is no such template");

        assert!(
            matches!(why, erp_eventlog::ConfigError::Invalid { ref key, .. }
                if key == TariffAsWritten::KEY),
            "{why:?}"
        );
    }

    /// A tariff nobody has written is empty rather than an error, which is what
    /// every tenant starts with.
    #[test]
    fn an_unwritten_tariff_resolves_to_no_bands() {
        assert_eq!(
            TariffAsWritten::default().resolve().expect("no bands"),
            Tariff::default()
        );
    }

    /// An empty tariff is the shipped default: every hour the same price.
    #[test]
    fn an_unconfigured_tariff_matches_nothing_and_explains_that() {
        let tariff = Tariff::default();
        let why = tariff.explain(at("2026-05-07", 18), Calendar::default());
        assert!(why.matched.is_none());
        assert!(why.considered.is_empty());
    }
}
