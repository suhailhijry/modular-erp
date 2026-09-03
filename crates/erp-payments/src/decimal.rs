//! Money on the wire, when the wire wants a decimal.
//!
//! # Why this exists at all
//!
//! [`Money`] is an integer count of minor units, and one of the three gateways
//! this crate talks to takes exactly that. The other two do not:
//!
//! | | `amount` |
//! |---|---|
//! | Moyasar | JSON **integer**, minor units — `1.00 SAR` is `100` |
//! | Tabby | JSON **string**, major units — `"100.00"` |
//! | Tamara | JSON **number**, major units — `100.50` |
//!
//! So two of them need a conversion, and **the conversion may not use floating
//! point**. The workspace forbids it, and the reason is not stylistic: `100.50`
//! has no exact binary representation, so a round trip through `f64` is a
//! rounding step nobody asked for in the middle of an amount somebody is going
//! to be charged.
//!
//! Everything here is integer division and remainder.
//!
//! # The exponent is the currency's
//!
//! Two decimal places is right for SAR and AED and wrong for KWD, BHD and OMR,
//! which are thousandths. [`CurrencyCode::exponent`] already knows, so nothing
//! here hard-codes a two.

use erp_types::{CurrencyCode, Money};

/// Why a decimal string was not an amount.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecimalError {
    #[error("{0} is not a decimal number")]
    NotANumber(String),
    /// **More places than the currency has.** `100.005 SAR` is not a smaller
    /// amount than a halala, it is an amount this system cannot hold — and
    /// rounding somebody's refund silently is not a decision to make here.
    #[error("{value} has more decimal places than {currency} has")]
    TooPrecise {
        value: String,
        currency: CurrencyCode,
    },
    #[error("{0} does not fit")]
    TooLarge(String),
}

/// `Money` as a decimal string in major units: `10050` SAR becomes `100.50`.
///
/// Always the currency's full number of places, which is the canonical form
/// every provider documents and the one that makes two logs comparable.
#[must_use]
pub fn to_decimal(amount: Money) -> String {
    let places = u32::from(amount.currency().exponent());
    let sign = if amount.minor() < 0 { "-" } else { "" };
    // `unsigned_abs` rather than `abs`: `i64::MIN.abs()` panics.
    let magnitude = amount.minor().unsigned_abs();

    if places == 0 {
        return format!("{sign}{magnitude}");
    }
    let scale = 10u64.pow(places);
    let whole = magnitude / scale;
    let fraction = magnitude % scale;
    let width = places as usize;
    format!("{sign}{whole}.{fraction:0width$}")
}

/// The way back, exactly.
///
/// **What a response body has to go through.** A gateway that answers
/// `"amount": 100.50` and a client that parses it into a float and multiplies
/// by a hundred is where a halala goes missing; this parses the digits.
pub fn from_decimal(value: &str, currency: CurrencyCode) -> Result<Money, DecimalError> {
    let trimmed = value.trim();
    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };

    let malformed = || DecimalError::NotANumber(value.to_owned());
    let mut parts = digits.split('.');
    let whole = parts.next().ok_or_else(malformed)?;
    let fraction = parts.next().unwrap_or("");
    if parts.next().is_some() || whole.is_empty() && fraction.is_empty() {
        return Err(malformed());
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !fraction.chars().all(|c| c.is_ascii_digit()) {
        return Err(malformed());
    }

    let places = usize::from(currency.exponent());
    // **Trailing zeros are free.** `100.500` in a two-place currency is
    // `100.50` written oddly, and refusing it would refuse a body some gateway
    // is entitled to send. Anything else past the exponent is a real digit
    // being thrown away, and that is a refusal.
    let significant = fraction.trim_end_matches('0');
    if significant.len() > places {
        return Err(DecimalError::TooPrecise {
            value: value.to_owned(),
            currency,
        });
    }

    let scale = 10u64.pow(u32::from(currency.exponent()));
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole
            .parse()
            .map_err(|_| DecimalError::TooLarge(value.to_owned()))?
    };
    // Padded to the currency's width, so `100.5` and `100.50` are one number.
    let mut padded = fraction.to_owned();
    padded.truncate(places);
    while padded.len() < places {
        padded.push('0');
    }
    let fraction: u64 = if padded.is_empty() {
        0
    } else {
        padded.parse().map_err(|_| malformed())?
    };

    let minor = whole
        .checked_mul(scale)
        .and_then(|w| w.checked_add(fraction))
        .and_then(|m| i64::try_from(m).ok())
        .ok_or_else(|| DecimalError::TooLarge(value.to_owned()))?;

    Ok(Money::from_minor(
        if negative { -minor } else { minor },
        currency,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn currency(code: &str) -> CurrencyCode {
        code.parse().expect("a currency")
    }

    fn money(minor: i64, code: &str) -> Money {
        Money::from_minor(minor, currency(code))
    }

    #[test]
    fn an_amount_is_written_with_the_currencys_own_places() {
        assert_eq!(to_decimal(money(10_050, "SAR")), "100.50");
        assert_eq!(to_decimal(money(10_000, "SAR")), "100.00");
        assert_eq!(to_decimal(money(1, "SAR")), "0.01");
        assert_eq!(to_decimal(money(0, "SAR")), "0.00");
        assert_eq!(to_decimal(money(-2_550, "SAR")), "-25.50");

        // Thousandths, which is why nothing here hard-codes a two.
        assert_eq!(to_decimal(money(100_500, "KWD")), "100.500");
        assert_eq!(to_decimal(money(1, "KWD")), "0.001");

        // And a currency with no minor unit at all.
        assert_eq!(to_decimal(money(1_050, "JPY")), "1050");
    }

    /// **The round trip is the whole point.** Every amount that goes out has to
    /// come back as itself, or a payment is recorded for the wrong sum.
    #[test]
    fn every_amount_survives_the_trip_out_and_back() {
        for code in ["SAR", "AED", "KWD", "BHD", "JPY"] {
            for minor in [0, 1, 7, 99, 100, 999, 1_000, 10_050, 123_456_789] {
                let amount = money(minor, code);
                let written = to_decimal(amount);
                assert_eq!(
                    from_decimal(&written, currency(code)),
                    Ok(amount),
                    "{code} {minor} wrote as {written}"
                );
            }
        }
    }

    /// The forms a gateway is entitled to send, which are not always the form
    /// this system writes.
    #[test]
    fn a_reply_is_read_however_the_gateway_chose_to_write_it() {
        let sar = currency("SAR");
        assert_eq!(from_decimal("100.50", sar), Ok(money(10_050, "SAR")));
        assert_eq!(from_decimal("100.5", sar), Ok(money(10_050, "SAR")));
        assert_eq!(from_decimal("100", sar), Ok(money(10_000, "SAR")));
        assert_eq!(from_decimal("100.", sar), Ok(money(10_000, "SAR")));
        assert_eq!(from_decimal(" 100.50 ", sar), Ok(money(10_050, "SAR")));
        assert_eq!(from_decimal("+100.50", sar), Ok(money(10_050, "SAR")));
        assert_eq!(from_decimal("-100.50", sar), Ok(money(-10_050, "SAR")));
        assert_eq!(from_decimal("0.01", sar), Ok(money(1, "SAR")));
        assert_eq!(from_decimal(".5", sar), Ok(money(50, "SAR")));
        // Trailing zeros past the exponent are the same number written oddly.
        assert_eq!(from_decimal("100.5000", sar), Ok(money(10_050, "SAR")));
    }

    /// **A digit that would be thrown away is a refusal.** Rounding somebody's
    /// refund quietly is not a decision this function gets to make.
    #[test]
    fn more_precision_than_the_currency_has_is_refused() {
        let sar = currency("SAR");
        assert!(matches!(
            from_decimal("100.005", sar),
            Err(DecimalError::TooPrecise { .. })
        ));
        assert!(matches!(
            from_decimal("0.001", sar),
            Err(DecimalError::TooPrecise { .. })
        ));
        // …and the same string is fine where the currency has the places.
        assert_eq!(from_decimal("0.001", currency("KWD")), Ok(money(1, "KWD")));
    }

    #[test]
    fn something_that_is_not_a_number_is_not_an_amount() {
        let sar = currency("SAR");
        for nonsense in ["", "abc", "1.2.3", "1,50", "1 000", "١٠٠", "1e5", "--1"] {
            assert!(
                matches!(
                    from_decimal(nonsense, sar),
                    Err(DecimalError::NotANumber(_))
                ),
                "{nonsense} should not parse"
            );
        }
        assert!(matches!(
            from_decimal("999999999999999999999", sar),
            Err(DecimalError::TooLarge(_))
        ));
    }
}
