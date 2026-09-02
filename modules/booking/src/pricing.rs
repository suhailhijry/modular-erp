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

use crate::availability::Availability;
use crate::calendar::Calendar;
use erp_occupancy::Span;

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
/// # Why a deposit is not enforceable here yet
///
/// A deposit is the honest answer to no-shows and `prepaid` already models one
/// — an entitlement with no uses, held against the booking it secures. What is
/// missing is the half that takes the money: card payments are Phase 12a, and
/// there is no gateway.
///
/// So [`Self::deposit_bp`] is recorded and **not charged**. A tenant who sets it
/// is describing what they will ask for; nothing in this build collects it, and
/// pretending otherwise would be a public booking that claims to be secured and
/// is not. It is here rather than added later because the shape is known and a
/// setting that arrives with the gateway is a setting nobody had configured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicBooking {
    /// Off by default. **The absence of a setting is a no**, not a yes.
    pub open: bool,
    /// What fraction of the booking a deposit would be, in basis points.
    ///
    /// Recorded, not charged — see above. Zero means none.
    #[serde(default)]
    pub deposit_bp: u32,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tariff {
    /// **First match wins**, so the order is the tenant's priority. A specific
    /// window — a public holiday — goes above a general one.
    pub bands: Vec<Band>,
}

impl Tariff {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "booking.tariff";

    /// What this tenant has configured, or nothing.
    ///
    /// An empty tariff is the shipped default and it means every hour is the
    /// same price, which is what most businesses want and all of them start
    /// with. A tenant who *has* configured one and stored something unusable
    /// gets an error rather than silently losing their peak rates.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or_else(Self::default, |configured| configured.value))
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
        let at = calendar.offset();
        self.bands.iter().find(|band| band.when.covers(span, at))
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
