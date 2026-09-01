//! Money as integer minor units plus a runtime currency.
//!
//! # Why there is no `Add`
//!
//! An earlier design sketched `Money<C: Currency>`, with the currency as a type
//! parameter so that adding SAR to USD would not compile. That is wrong for this
//! system: currencies are **tenant configuration**, chosen at runtime, so they
//! cannot be type parameters.
//!
//! The guarantee is preserved a different way — by omitting the operators.
//! `Money` implements no `Add`, `Sub`, `Neg`, `Sum`, or `Ord`. Every operation
//! that could fail returns `Result`, so a currency mismatch is something the
//! caller must handle rather than something an operator quietly performs.
//!
//! `PartialOrd` is absent for the same reason `Add` is: ordering two amounts in
//! different currencies is meaningless, and a silent `false` from a comparison
//! is worse than a `Result` the caller has to read. Use [`Money::checked_cmp`].

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{InvalidCurrency, MoneyError};

/// An ISO-4217 alphabetic currency code.
///
/// Stored as three ASCII uppercase bytes rather than a `String`: it is `Copy`,
/// allocation-free, and cannot hold a value that isn't a well-formed code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CurrencyCode([u8; 3]);

impl CurrencyCode {
    /// Currencies whose minor unit is not the usual two decimal places.
    ///
    /// ISO-4217 exponents. Anything unlisted is 2, which is correct for the
    /// large majority including SAR, USD, EUR and GBP.
    const ZERO_EXPONENT: [&'static str; 17] = [
        "BIF", "CLP", "DJF", "GNF", "ISK", "JPY", "KMF", "KRW", "PYG", "RWF", "UGX", "UYI", "VND",
        "VUV", "XAF", "XOF", "XPF",
    ];
    const THREE_EXPONENT: [&'static str; 7] = ["BHD", "IQD", "JOD", "KWD", "LYD", "OMR", "TND"];
    const FOUR_EXPONENT: [&'static str; 2] = ["CLF", "UYW"];

    pub fn new(code: &str) -> Result<Self, InvalidCurrency> {
        if code.len() != 3 {
            return Err(InvalidCurrency::WrongLength(code.len()));
        }
        if !code.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err(InvalidCurrency::NotAlphabetic(code.to_owned()));
        }
        let mut bytes = [0u8; 3];
        for (slot, byte) in bytes.iter_mut().zip(code.bytes()) {
            *slot = byte.to_ascii_uppercase();
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        // Safe by construction: `new` is the only constructor and admits only
        // ASCII alphabetic bytes.
        core::str::from_utf8(&self.0).unwrap_or("???")
    }

    /// Number of decimal digits in this currency's minor unit.
    ///
    /// SAR/USD → 2, JPY → 0, KWD → 3. Callers must not assume 2; formatting and
    /// rounding both depend on getting this right.
    #[must_use]
    pub fn exponent(&self) -> u8 {
        let code = self.as_str();
        if Self::ZERO_EXPONENT.contains(&code) {
            0
        } else if Self::THREE_EXPONENT.contains(&code) {
            3
        } else if Self::FOUR_EXPONENT.contains(&code) {
            4
        } else {
            2
        }
    }

    /// Minor units per major unit — 100 for SAR, 1 for JPY, 1000 for KWD.
    #[must_use]
    pub fn minor_per_major(&self) -> i64 {
        10i64.pow(u32::from(self.exponent()))
    }
}

impl fmt::Display for CurrencyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CurrencyCode {
    type Err = InvalidCurrency;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl Serialize for CurrencyCode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CurrencyCode {
    /// `Cow`, not `&str`.
    ///
    /// A borrowed `&str` only works when the deserializer can point into its
    /// input — true for `serde_json::from_str`, false for `from_value`, which is
    /// how every event payload is decoded. This failed on the first real event
    /// that carried a `Money`.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = std::borrow::Cow::<'de, str>::deserialize(d)?;
        Self::new(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "sqlx")]
impl sqlx::Type<sqlx::Postgres> for CurrencyCode {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::Type<sqlx::Postgres>>::type_info()
    }
    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        <String as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}

#[cfg(feature = "sqlx")]
impl sqlx::Encode<'_, sqlx::Postgres> for CurrencyCode {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        <&str as sqlx::Encode<sqlx::Postgres>>::encode(self.as_str(), buf)
    }
}

#[cfg(feature = "sqlx")]
impl sqlx::Decode<'_, sqlx::Postgres> for CurrencyCode {
    fn decode(value: sqlx::postgres::PgValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        let raw = <&str as sqlx::Decode<sqlx::Postgres>>::decode(value)?;
        Ok(Self::new(raw)?)
    }
}

/// An amount in a specific currency, held in integer minor units.
///
/// Never floating point: `float_arithmetic` is denied workspace-wide, and a
/// ledger that rounds differently on two machines is not a ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Money {
    minor: i64,
    currency: CurrencyCode,
}

impl Money {
    #[must_use]
    pub const fn from_minor(minor: i64, currency: CurrencyCode) -> Self {
        Self { minor, currency }
    }

    #[must_use]
    pub const fn zero(currency: CurrencyCode) -> Self {
        Self { minor: 0, currency }
    }

    #[must_use]
    pub const fn minor(self) -> i64 {
        self.minor
    }

    #[must_use]
    pub const fn currency(self) -> CurrencyCode {
        self.currency
    }

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.minor == 0
    }

    #[must_use]
    pub const fn is_positive(self) -> bool {
        self.minor > 0
    }

    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.minor < 0
    }

    fn require_same(self, other: Self) -> Result<(), MoneyError> {
        if self.currency == other.currency {
            Ok(())
        } else {
            Err(MoneyError::CurrencyMismatch {
                left: self.currency,
                right: other.currency,
            })
        }
    }

    pub fn checked_add(self, rhs: Self) -> Result<Self, MoneyError> {
        self.require_same(rhs)?;
        self.minor
            .checked_add(rhs.minor)
            .map(|minor| Self::from_minor(minor, self.currency))
            .ok_or(MoneyError::Overflow {
                currency: self.currency,
            })
    }

    pub fn checked_sub(self, rhs: Self) -> Result<Self, MoneyError> {
        self.require_same(rhs)?;
        self.minor
            .checked_sub(rhs.minor)
            .map(|minor| Self::from_minor(minor, self.currency))
            .ok_or(MoneyError::Overflow {
                currency: self.currency,
            })
    }

    pub fn checked_neg(self) -> Result<Self, MoneyError> {
        self.minor
            .checked_neg()
            .map(|minor| Self::from_minor(minor, self.currency))
            .ok_or(MoneyError::Overflow {
                currency: self.currency,
            })
    }

    pub fn checked_abs(self) -> Result<Self, MoneyError> {
        self.minor
            .checked_abs()
            .map(|minor| Self::from_minor(minor, self.currency))
            .ok_or(MoneyError::Overflow {
                currency: self.currency,
            })
    }

    /// Multiply by a whole number — quantity times unit price.
    ///
    /// There is deliberately no multiplication by a fraction or a percentage:
    /// those need an explicit rounding policy, and burying one in an operator is
    /// how ledgers drift by a halala per line. Where a rate is genuinely needed,
    /// [`Self::scaled_by`] names its policy in the doc rather than hiding it in
    /// a `*`.
    pub fn checked_mul_int(self, factor: i64) -> Result<Self, MoneyError> {
        self.minor
            .checked_mul(factor)
            .map(|minor| Self::from_minor(minor, self.currency))
            .ok_or(MoneyError::Overflow {
                currency: self.currency,
            })
    }

    /// Scales by a rate in basis points, **rounding halves away from zero**.
    ///
    /// One ten-thousandth, because 15% is exact in basis points and `0.15` is
    /// not — and `float_arithmetic` is denied workspace-wide anyway.
    ///
    /// # Why the rounding is this and not something else
    ///
    /// Half away from zero is what ZATCA's invoicing rules specify and what
    /// every till in the country does. `15.005` becomes `15.01`, and `-15.005`
    /// becomes `-15.01` — symmetric, so crediting a line reverses it exactly
    /// instead of leaving a halala behind.
    ///
    /// # Why this is on `Money` and not in whichever module needed it first
    ///
    /// It was in `sales::vat`, privately, and `booking` needed the same rule to
    /// put a peak-hour uplift on a price. Two implementations of one rounding
    /// rule is the defect the booking system this was measured against records
    /// in its own source: three of them, disagreeing, and every fixed discount
    /// differing by exactly the tax on it. One rule, one place, and the tax
    /// tests in `sales` are what keep it honest.
    pub fn scaled_by(self, basis_points: i32) -> Result<Self, MoneyError> {
        self.apportioned(i64::from(basis_points), 10_000)
    }

    /// The share of this amount that `numerator / denominator` represents,
    /// **rounding halves away from zero**. The same policy as
    /// [`Self::scaled_by`], which is written in terms of this.
    ///
    /// # What it is for, and the property that matters
    ///
    /// Recognising revenue over a period: four months into a year of a 1,200
    /// riyal membership is `apportioned(122, 365)`. The reason it takes the
    /// fraction rather than a rate in basis points is that a rate would round
    /// the *fraction* first — 122/365 is 33.4247%, which is 3342 basis points
    /// and 0.0047% of a year thrown away every time it is asked.
    ///
    /// **Apportioning by the whole gives back the whole.** `apportioned(n, n)`
    /// is exactly `self`, so a caller who recognises cumulative amounts and
    /// posts the difference cannot strand a halala in a liability account at
    /// the end of a period. That is the property the deferred-revenue canary
    /// rests on, and it is why recognition is written as a cumulative total
    /// rather than as a sum of instalments.
    pub fn apportioned(self, numerator: i64, denominator: i64) -> Result<Self, MoneyError> {
        if denominator == 0 {
            return Err(MoneyError::DivideByZero);
        }
        let product = i128::from(self.minor) * i128::from(numerator);
        let rounded = div_round_half_away(product, i128::from(denominator));
        i64::try_from(rounded)
            .map(|minor| Self::from_minor(minor, self.currency))
            .map_err(|_| MoneyError::Overflow {
                currency: self.currency,
            })
    }

    /// Ordering within a currency. Returns an error across currencies rather
    /// than a meaningless answer — which is why `Money` has no `PartialOrd`.
    pub fn checked_cmp(self, other: Self) -> Result<core::cmp::Ordering, MoneyError> {
        self.require_same(other)?;
        Ok(self.minor.cmp(&other.minor))
    }

    /// Sum a sequence, in a currency the caller states.
    ///
    /// The currency is a parameter rather than inferred from the first element
    /// so that an empty sequence yields `zero(currency)` instead of failing —
    /// summing no journal lines is legitimately zero, not an error.
    pub fn checked_sum<I>(items: I, currency: CurrencyCode) -> Result<Self, MoneyError>
    where
        I: IntoIterator<Item = Self>,
    {
        items
            .into_iter()
            .try_fold(Self::zero(currency), Money::checked_add)
    }
}

/// `numerator / denominator`, rounding halves away from zero.
///
/// The one rounding rule in the workspace: [`Money::scaled_by`] and
/// [`Money::apportioned`] are both written in terms of it.
fn div_round_half_away(numerator: i128, denominator: i128) -> i128 {
    // The sign of the quotient, which is the sign of both operands together.
    // `scaled_by` only ever passes a positive denominator; `apportioned` takes
    // one from a caller, and a negative one has to come out the right way round
    // rather than flipping the answer's sign silently.
    let sign = if (numerator < 0) == (denominator < 0) {
        1
    } else {
        -1
    };
    let magnitude = numerator.unsigned_abs();
    let denominator = denominator.unsigned_abs();
    // `(2n + d) / 2d` is `n/d` rounded half up, without a division producing a
    // remainder anyone has to interpret.
    let rounded = (magnitude * 2 + denominator) / (denominator * 2);
    sign * i128::try_from(rounded).unwrap_or(i128::MAX)
}

impl fmt::Display for Money {
    /// `1050 SAR` (exponent 2) renders as `10.50 SAR`; `1050 JPY` as `1050 JPY`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let exponent = u32::from(self.currency.exponent());
        let sign = if self.minor < 0 { "-" } else { "" };
        // `unsigned_abs` rather than `abs`: `i64::MIN.abs()` panics.
        let magnitude = self.minor.unsigned_abs();

        if exponent == 0 {
            return write!(f, "{sign}{magnitude} {}", self.currency);
        }

        let divisor = 10u64.pow(exponent);
        let whole = magnitude / divisor;
        let fraction = magnitude % divisor;
        let width = exponent as usize;
        write!(
            f,
            "{sign}{whole}.{fraction:0width$} {currency}",
            currency = self.currency
        )
    }
}

#[cfg(test)]
mod tests {
    /// **The rounding rule, against its own definition.**
    ///
    /// Lived in `sales::vat` until `booking` needed the same rule for a
    /// peak-hour uplift. The reference is `round(n/d)` with halves away from
    /// zero, spelled out the slow, obvious way.
    #[test]
    fn rounding_matches_a_reference_implementation() {
        for numerator in -50_i128..=50 {
            for denominator in 1_i128..=7 {
                let exact_twice = numerator * 2;
                let mut expected = numerator / denominator;
                let remainder_twice = exact_twice - expected * denominator * 2;
                if remainder_twice.abs() >= denominator {
                    expected += if numerator < 0 { -1 } else { 1 };
                }
                assert_eq!(
                    div_round_half_away(numerator, denominator),
                    expected,
                    "{numerator}/{denominator}"
                );
            }
        }
    }

    /// **Apportioning by the whole gives back the whole**, which is what stops
    /// a halala being stranded in a liability at the end of a period.
    #[test]
    fn apportioning_by_the_whole_is_exact() {
        let sar = CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!("a real code"));
        for minor in [1_i64, 7, 99, 100_000, 120_000, 999_983] {
            let whole = Money::from_minor(minor, sar);
            for denominator in [3_i64, 7, 30, 365, 366] {
                assert_eq!(
                    whole.apportioned(denominator, denominator),
                    Ok(whole),
                    "{minor} apportioned {denominator}/{denominator}"
                );
            }
        }
    }

    /// A year of a 1,200 riyal membership, recognised as cumulative totals.
    /// The last day has to bring it to exactly the price.
    #[test]
    fn recognising_cumulatively_over_a_period_ends_on_the_price() {
        let sar = CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!("a real code"));
        let price = Money::from_minor(120_000, sar);
        let mut previous = Money::zero(sar);
        let mut posted = Money::zero(sar);
        for day in 1..=365_i64 {
            let cumulative = price
                .apportioned(day, 365)
                .unwrap_or_else(|e| unreachable!("{e}"));
            let step = cumulative
                .checked_sub(previous)
                .unwrap_or_else(|e| unreachable!("{e}"));
            posted = posted
                .checked_add(step)
                .unwrap_or_else(|e| unreachable!("{e}"));
            previous = cumulative;
        }
        assert_eq!(
            posted, price,
            "a year of recognition did not come to the price"
        );
    }

    #[test]
    fn apportioning_by_nothing_is_refused_rather_than_guessed() {
        let sar = CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!("a real code"));
        assert_eq!(
            Money::from_minor(100, sar).apportioned(1, 0),
            Err(MoneyError::DivideByZero)
        );
    }

    /// The two callers that matter: 15% tax, and a 25% peak uplift.
    #[test]
    fn scaling_by_a_rate_rounds_halves_away_from_zero() {
        let sar = CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!("a real code"));
        let m = |minor| Money::from_minor(minor, sar);

        assert_eq!(m(10_000).scaled_by(1_500), Ok(m(1_500)));
        assert_eq!(m(10_000).scaled_by(2_500), Ok(m(2_500)));
        // 33.33 at 15% is 4.9995, which goes up rather than to the even neighbour.
        assert_eq!(m(3_333).scaled_by(1_500), Ok(m(500)));
        // And symmetrically downwards, so a credit reverses a charge exactly.
        assert_eq!(m(-3_333).scaled_by(1_500), Ok(m(-500)));
        assert_eq!(m(1).scaled_by(0), Ok(m(0)));
        assert!(m(i64::MAX).scaled_by(i32::MAX).is_err());
    }

    use super::*;

    fn sar() -> CurrencyCode {
        CurrencyCode::new("SAR").expect("SAR is valid")
    }
    fn usd() -> CurrencyCode {
        CurrencyCode::new("USD").expect("USD is valid")
    }

    #[test]
    fn currency_normalizes_case() {
        assert_eq!(CurrencyCode::new("sar").unwrap(), sar());
        assert_eq!(CurrencyCode::new("sAr").unwrap().as_str(), "SAR");
    }

    #[test]
    fn currency_rejects_malformed() {
        assert!(matches!(
            CurrencyCode::new("SA"),
            Err(InvalidCurrency::WrongLength(2))
        ));
        assert!(matches!(
            CurrencyCode::new("S4R"),
            Err(InvalidCurrency::NotAlphabetic(_))
        ));
        // Four characters, not three bytes of a multibyte char.
        assert!(CurrencyCode::new("SARR").is_err());
    }

    #[test]
    fn exponents_follow_iso_4217() {
        assert_eq!(sar().exponent(), 2);
        assert_eq!(usd().exponent(), 2);
        assert_eq!(CurrencyCode::new("JPY").unwrap().exponent(), 0);
        assert_eq!(CurrencyCode::new("KWD").unwrap().exponent(), 3);
        assert_eq!(CurrencyCode::new("CLF").unwrap().exponent(), 4);
        // Unknown-but-well-formed codes default to 2 rather than failing:
        // tenants may use codes we don't enumerate.
        assert_eq!(CurrencyCode::new("ZZZ").unwrap().exponent(), 2);
    }

    #[test]
    fn cross_currency_arithmetic_is_an_error_not_a_silent_result() {
        let a = Money::from_minor(1000, sar());
        let b = Money::from_minor(1000, usd());
        assert!(matches!(
            a.checked_add(b),
            Err(MoneyError::CurrencyMismatch { .. })
        ));
        assert!(matches!(
            a.checked_sub(b),
            Err(MoneyError::CurrencyMismatch { .. })
        ));
        assert!(matches!(
            a.checked_cmp(b),
            Err(MoneyError::CurrencyMismatch { .. })
        ));
    }

    #[test]
    fn overflow_is_reported_not_wrapped() {
        let big = Money::from_minor(i64::MAX, sar());
        let one = Money::from_minor(1, sar());
        assert!(matches!(
            big.checked_add(one),
            Err(MoneyError::Overflow { .. })
        ));

        let small = Money::from_minor(i64::MIN, sar());
        assert!(matches!(
            small.checked_sub(one),
            Err(MoneyError::Overflow { .. })
        ));
        assert!(matches!(
            small.checked_neg(),
            Err(MoneyError::Overflow { .. })
        ));
        assert!(matches!(
            small.checked_abs(),
            Err(MoneyError::Overflow { .. })
        ));
        assert!(matches!(
            big.checked_mul_int(2),
            Err(MoneyError::Overflow { .. })
        ));
    }

    #[test]
    fn sum_of_nothing_is_zero_in_the_stated_currency() {
        let total = Money::checked_sum(std::iter::empty(), sar()).unwrap();
        assert_eq!(total, Money::zero(sar()));
    }

    #[test]
    fn sum_rejects_a_foreign_amount_midway() {
        let items = vec![Money::from_minor(100, sar()), Money::from_minor(100, usd())];
        assert!(Money::checked_sum(items, sar()).is_err());
    }

    #[test]
    fn display_places_the_decimal_by_currency_exponent() {
        assert_eq!(Money::from_minor(1050, sar()).to_string(), "10.50 SAR");
        assert_eq!(Money::from_minor(5, sar()).to_string(), "0.05 SAR");
        assert_eq!(Money::from_minor(-50, sar()).to_string(), "-0.50 SAR");
        assert_eq!(
            Money::from_minor(1050, CurrencyCode::new("JPY").unwrap()).to_string(),
            "1050 JPY"
        );
        assert_eq!(
            Money::from_minor(1050, CurrencyCode::new("KWD").unwrap()).to_string(),
            "1.050 KWD"
        );
    }

    #[test]
    fn display_handles_the_extreme_negative_without_panicking() {
        // `i64::MIN.abs()` panics; the implementation uses `unsigned_abs`.
        let rendered = Money::from_minor(i64::MIN, sar()).to_string();
        assert!(rendered.starts_with("-92233720368547758.08"));
    }

    #[test]
    fn wire_format_is_explicit_about_both_fields() {
        let money = Money::from_minor(1050, sar());
        let json = serde_json::to_string(&money).unwrap();
        assert_eq!(json, r#"{"minor":1050,"currency":"SAR"}"#);
        assert_eq!(serde_json::from_str::<Money>(&json).unwrap(), money);
    }

    #[test]
    fn deserializing_a_bad_currency_fails_rather_than_defaulting() {
        assert!(serde_json::from_str::<Money>(r#"{"minor":1,"currency":"XX"}"#).is_err());
    }

    /// **The path every event payload takes.**
    ///
    /// Events are stored as `jsonb` and decoded with `from_value`, which owns
    /// its strings — so a `Deserialize` that insists on borrowing works in every
    /// unit test and fails on the first stored event.
    #[test]
    fn money_decodes_from_an_owned_value_not_just_a_borrowed_str() {
        let money = Money::from_minor(1050, sar());
        let value = serde_json::to_value(money).unwrap();
        assert_eq!(serde_json::from_value::<Money>(value).unwrap(), money);
        assert!(
            serde_json::from_value::<Money>(serde_json::json!({
                "minor": 1, "currency": "XX"
            }))
            .is_err(),
            "and validation still applies on that path"
        );
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn addition_is_commutative_within_a_currency(a: i32, b: i32) {
                let x = Money::from_minor(i64::from(a), sar());
                let y = Money::from_minor(i64::from(b), sar());
                prop_assert_eq!(
                    x.checked_add(y).unwrap(),
                    y.checked_add(x).unwrap()
                );
            }

            #[test]
            fn add_then_subtract_is_identity(a: i32, b: i32) {
                let x = Money::from_minor(i64::from(a), sar());
                let y = Money::from_minor(i64::from(b), sar());
                prop_assert_eq!(x.checked_add(y).unwrap().checked_sub(y).unwrap(), x);
            }

            /// The property the ledger depends on: a set of amounts summing to
            /// zero still sums to zero regardless of the order they arrive in.
            #[test]
            fn summation_is_order_independent(amounts: Vec<i32>) {
                let forward: Vec<_> = amounts
                    .iter()
                    .map(|a| Money::from_minor(i64::from(*a), sar()))
                    .collect();
                let mut backward = forward.clone();
                backward.reverse();
                prop_assert_eq!(
                    Money::checked_sum(forward, sar()).unwrap(),
                    Money::checked_sum(backward, sar()).unwrap()
                );
            }

            #[test]
            fn display_round_trips_through_serde(minor: i64) {
                let money = Money::from_minor(minor, sar());
                let json = serde_json::to_string(&money).unwrap();
                prop_assert_eq!(serde_json::from_str::<Money>(&json).unwrap(), money);
            }
        }
    }
}
