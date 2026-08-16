//! Value-added tax, as Saudi Arabia charges it.
//!
//! # Why the rate is stored, not looked up
//!
//! Saudi VAT was 5% until July 2020 and has been 15% since. An invoice issued in
//! 2019 is still 5%, and it must still print as 5% in 2031 — so the rate is
//! resolved when the invoice is issued and written into the event (architecture
//! L5). Nothing here reads [`VatCategory::rate_now`] except the code that issues
//! a new invoice.
//!
//! # Why zero-rated and exempt are different
//!
//! Both charge nothing, so they look interchangeable until the VAT return: an
//! exempt supply also blocks the input tax attached to it, a zero-rated one does
//! not. Collapsing them into "0%" is a decision that cannot be undone later
//! without asking a bookkeeper to reclassify every historic line.

use serde::{Deserialize, Serialize};
use spa_types::{CurrencyCode, Money};

// Defined in `ledger` because `purchases` classifies by the same three
// categories, and two sibling modules must not depend on each other.
pub use ledger::VatCategory;

/// One ten-thousandth. Rates are basis points because 15% is exact there and
/// `0.15` is not — and `float_arithmetic` is denied workspace-wide anyway.
const BASIS: i128 = 10_000;

/// A tax treatment together with the rate that applied when it was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vat {
    pub category: VatCategory,
    /// Basis points — 1500 is 15%. Written at issue time and never recomputed.
    pub basis_points: i32,
}

impl Vat {
    /// The treatment at today's statutory rate. The only constructor an issuing
    /// command should use.
    #[must_use]
    pub const fn current(category: VatCategory) -> Self {
        Self {
            category,
            basis_points: category.rate_now(),
        }
    }

    /// Tax on an amount, rounded to the currency's minor unit.
    ///
    /// # Rounding
    ///
    /// Half away from zero, which is what ZATCA's invoicing rules specify and
    /// what every till in the country does. `15.005` becomes `15.01`, and
    /// `-15.005` becomes `-15.01` — symmetric, so crediting an invoice line
    /// reverses it exactly instead of leaving a halala behind.
    pub fn on(self, net: Money) -> Result<Money, TaxError> {
        let product = i128::from(net.minor()) * i128::from(self.basis_points);
        let rounded = div_round_half_away(product, BASIS);
        let minor = i64::try_from(rounded).map_err(|_| TaxError::OutOfRange)?;
        Ok(Money::from_minor(minor, net.currency()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TaxError {
    #[error("the tax on that amount is too large to record")]
    OutOfRange,
    #[error("every line of an invoice must be in the same currency")]
    MixedCurrencies,
}

impl From<spa_types::MoneyError> for TaxError {
    fn from(error: spa_types::MoneyError) -> Self {
        match error {
            spa_types::MoneyError::CurrencyMismatch { .. } => Self::MixedCurrencies,
            spa_types::MoneyError::Overflow { .. } | spa_types::MoneyError::DivideByZero => {
                Self::OutOfRange
            }
        }
    }
}

/// `numerator / denominator`, rounding halves away from zero.
///
/// `denominator` is always [`BASIS`]; it is a parameter so the property test can
/// vary it.
fn div_round_half_away(numerator: i128, denominator: i128) -> i128 {
    let sign = if numerator < 0 { -1 } else { 1 };
    let magnitude = numerator.unsigned_abs();
    let denominator = denominator.unsigned_abs();
    // `(2n + d) / 2d` is `n/d` rounded half up, without a division producing a
    // remainder anyone has to interpret.
    let rounded = (magnitude * 2 + denominator) / (denominator * 2);
    sign * i128::try_from(rounded).unwrap_or(i128::MAX)
}

/// Net and tax for one rate, as a tax invoice must show them.
///
/// Saudi invoices report per rate rather than per line, which is also the only
/// way the arithmetic can be checked: rounding each line and summing gives a
/// different answer from rounding the subtotal, and the subtotal is the one the
/// authority computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxBand {
    pub category: VatCategory,
    pub basis_points: i32,
    pub net: Money,
    pub tax: Money,
}

/// What an invoice comes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub net: Money,
    pub tax: Money,
    pub gross: Money,
    /// One band per distinct (category, rate) present, in a stable order.
    pub bands: Vec<TaxBand>,
}

/// Sums nets by band and taxes each band once.
///
/// The input is `(vat, net)` pairs — one per invoice line. Ordering of the
/// result does not depend on the order of the input, so two clients sending the
/// same lines in different orders get byte-identical events.
pub fn total(
    amounts: impl IntoIterator<Item = (Vat, Money)>,
    currency: CurrencyCode,
) -> Result<Totals, TaxError> {
    // Small and fixed: three categories, and one rate each within an invoice.
    // A map would be more code and no faster.
    let mut bands: Vec<TaxBand> = Vec::new();

    for (vat, net) in amounts {
        if net.currency() != currency {
            return Err(TaxError::MixedCurrencies);
        }
        match bands
            .iter_mut()
            .find(|b| b.category == vat.category && b.basis_points == vat.basis_points)
        {
            Some(band) => {
                band.net = band.net.checked_add(net)?;
            }
            None => bands.push(TaxBand {
                category: vat.category,
                basis_points: vat.basis_points,
                net,
                tax: Money::zero(currency),
            }),
        }
    }

    // Stable regardless of line order, so a replay and a re-send agree.
    bands.sort_unstable_by_key(|b| (b.category, b.basis_points));

    for band in &mut bands {
        band.tax = Vat {
            category: band.category,
            basis_points: band.basis_points,
        }
        .on(band.net)?;
    }

    let net = Money::checked_sum(bands.iter().map(|b| b.net), currency)?;
    let tax = Money::checked_sum(bands.iter().map(|b| b.tax), currency)?;
    let gross = net.checked_add(tax)?;

    Ok(Totals {
        net,
        tax,
        gross,
        bands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sar() -> CurrencyCode {
        CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!())
    }

    fn money(minor: i64) -> Money {
        Money::from_minor(minor, sar())
    }

    #[test]
    fn fifteen_percent_of_a_round_amount_is_exact() {
        let vat = Vat::current(VatCategory::Standard);
        assert_eq!(vat.basis_points, 1_500);
        assert_eq!(vat.on(money(10_000)).unwrap(), money(1_500));
    }

    #[test]
    fn halves_round_away_from_zero_in_both_directions() {
        let vat = Vat::current(VatCategory::Standard);
        // 0.10 × 15% = 0.015 — exactly half a halala.
        assert_eq!(vat.on(money(10)).unwrap(), money(2));
        assert_eq!(vat.on(money(-10)).unwrap(), money(-2));
    }

    #[test]
    fn a_credit_line_reverses_its_own_tax_exactly() {
        // The reason rounding is symmetric. With half-up, +0.015 rounds to 2 and
        // -0.015 rounds to -1, and an invoice plus its exact credit note leaves a
        // stray halala in VAT payable forever.
        let vat = Vat::current(VatCategory::Standard);
        for minor in [1, 7, 10, 33, 3_333, 999_999] {
            let up = vat.on(money(minor)).unwrap();
            let down = vat.on(money(-minor)).unwrap();
            assert_eq!(
                up.minor(),
                -down.minor(),
                "{minor} taxes asymmetrically: {up} vs {down}"
            );
        }
    }

    #[test]
    fn exempt_and_zero_rated_are_both_untaxed_and_still_distinguishable() {
        assert_eq!(
            Vat::current(VatCategory::Zero)
                .on(money(9_999))
                .unwrap()
                .minor(),
            0
        );
        assert_eq!(
            Vat::current(VatCategory::Exempt)
                .on(money(9_999))
                .unwrap()
                .minor(),
            0
        );

        let totals = total(
            [
                (Vat::current(VatCategory::Zero), money(1_000)),
                (Vat::current(VatCategory::Exempt), money(2_000)),
            ],
            sar(),
        )
        .unwrap();

        // Two bands, not one — which is the whole reason they are separate
        // variants. A VAT return needs them apart.
        assert_eq!(totals.bands.len(), 2);
        assert_eq!(totals.tax, money(0));
        assert_eq!(totals.gross, money(3_000));
    }

    #[test]
    fn tax_is_computed_on_the_band_not_the_line() {
        // Three lines of 33.33 at 15%. Per line: 4.9995 → 5.00 each → 15.00.
        // Per band: 99.99 → 14.9985 → 15.00. They agree here, and the point of
        // the test is the third case below, where they do not.
        let standard = Vat::current(VatCategory::Standard);
        let per_line: i64 = (0..3)
            .map(|_| standard.on(money(3_333)).unwrap().minor())
            .sum();
        let per_band = total((0..3).map(|_| (standard, money(3_333))), sar()).unwrap();
        assert_eq!(per_line, 1_500);
        assert_eq!(per_band.tax, money(1_500));

        // 0.10 three times: per line 0.02 × 3 = 0.06; per band 0.30 → 0.045 →
        // 0.05. The authority computes the second one.
        let per_line: i64 = (0..3)
            .map(|_| standard.on(money(10)).unwrap().minor())
            .sum();
        let per_band = total((0..3).map(|_| (standard, money(10))), sar()).unwrap();
        assert_eq!(per_line, 6);
        assert_eq!(per_band.tax, money(5), "banding is not the same as summing");
    }

    #[test]
    fn line_order_does_not_change_the_event() {
        let a = Vat::current(VatCategory::Standard);
        let z = Vat::current(VatCategory::Zero);

        let one = total([(a, money(100)), (z, money(200)), (a, money(300))], sar()).unwrap();
        let two = total([(z, money(200)), (a, money(300)), (a, money(100))], sar()).unwrap();

        assert_eq!(one, two, "a reordered request must produce the same totals");
    }

    #[test]
    fn an_empty_invoice_totals_to_nothing_rather_than_failing() {
        // Refusing an empty invoice is the command's job, not the arithmetic's.
        let totals = total([], sar()).unwrap();
        assert!(totals.bands.is_empty());
        assert_eq!(totals.gross, money(0));
    }

    #[test]
    fn a_stored_rate_that_would_overflow_is_refused_rather_than_wrapped() {
        // 15% of the largest representable amount still fits, which is why this
        // needs an absurd rate to provoke. `Vat::current` cannot produce one —
        // decoding a stored event can, and that is exactly where "it was valid
        // when we wrote it" stops being a guarantee.
        assert!(
            Vat::current(VatCategory::Standard)
                .on(money(i64::MAX))
                .is_ok()
        );

        let absurd = Vat {
            category: VatCategory::Standard,
            basis_points: i32::MAX,
        };
        assert_eq!(absurd.on(money(i64::MAX)), Err(TaxError::OutOfRange));
    }

    #[test]
    fn rounding_matches_a_reference_implementation() {
        // The reference is the definition — `round(n/d)` with halves away from
        // zero — spelled out the slow, obvious way.
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
}
