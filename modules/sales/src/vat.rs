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
use erp_types::{CurrencyCode, Money};

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
    pub const fn at(rates: ledger::Rates, category: VatCategory) -> Self {
        Self {
            category,
            basis_points: rates.of(category),
        }
    }

    /// The rate this build ships, for tests and for anything that has no tenant
    /// to ask. **Never on a write path** — an invoice is stamped with the rate
    /// its tenant had configured, resolved in the command's own transaction.
    #[must_use]
    pub const fn shipped(category: VatCategory) -> Self {
        Self::at(ledger::Rates::saudi_arabia(), category)
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
    /// A discount is stated as the amount taken off, so it is positive. A
    /// negative one is a charge, which is a different UBL element and a
    /// different conversation with a customer.
    #[error("a discount must be a positive amount")]
    NotADiscount,
    /// Nothing on the invoice is treated the way this discount says it is.
    /// Discounting a standard-rated invoice at the zero rate would reclaim tax
    /// that was never charged.
    #[error("the invoice has no line treated the way this discount is")]
    DiscountWithoutABand,
    #[error("a discount cannot be larger than what it is taken off")]
    DiscountTooLarge,
}

impl From<erp_types::MoneyError> for TaxError {
    fn from(error: erp_types::MoneyError) -> Self {
        match error {
            erp_types::MoneyError::CurrencyMismatch { .. } => Self::MixedCurrencies,
            erp_types::MoneyError::Overflow { .. } | erp_types::MoneyError::DivideByZero => {
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
    /// **After any discount**, and therefore what is taxed.
    pub net: Money,
    pub tax: Money,
    pub gross: Money,
    /// What was taken off the whole document, as a positive amount.
    ///
    /// Derived from the discounts, like `net` and `tax` are derived from the
    /// bands — it is here because the monetary totals a tax invoice must print
    /// cannot be computed without it: what the lines came to is `net +
    /// discount`, and an invoice has to show both numbers.
    ///
    /// **`None` rather than zero**, because a `serde` default cannot see the
    /// rest of the struct and there is no zero without a currency. Absent is
    /// what every invoice issued before discounts existed carries, and it means
    /// exactly what it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discount: Option<Money>,
    /// One band per distinct (category, rate) present, in a stable order.
    pub bands: Vec<TaxBand>,
}

impl Totals {
    /// What was discounted, in this invoice's currency. Zero when nothing was.
    #[must_use]
    pub fn discount(&self) -> Money {
        self.discount
            .unwrap_or_else(|| Money::zero(self.net.currency()))
    }

    /// What the lines came to before any discount — UBL's
    /// `LineExtensionAmount`.
    pub fn before_discount(&self) -> Result<Money, TaxError> {
        Ok(self.net.checked_add(self.discount())?)
    }
}

/// Sums nets by band and taxes each band once.
///
/// The input is `(vat, net)` pairs — one per invoice line. Ordering of the
/// result does not depend on the order of the input, so two clients sending the
/// same lines in different orders get byte-identical events.
pub fn total(
    amounts: impl IntoIterator<Item = (Vat, Money)>,
    discounts: impl IntoIterator<Item = (Vat, Money)>,
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

    // **The discount comes off before the tax is worked out**, which is the
    // whole difference between a discount and a credit note: a discounted
    // invoice was never for the larger amount, so the smaller one is what is
    // taxed and what is declared.
    let mut discounted = Money::zero(currency);
    for (vat, amount) in discounts {
        if amount.currency() != currency {
            return Err(TaxError::MixedCurrencies);
        }
        if amount.minor() <= 0 {
            return Err(TaxError::NotADiscount);
        }

        let band = bands
            .iter_mut()
            .find(|b| b.category == vat.category && b.basis_points == vat.basis_points)
            .ok_or(TaxError::DiscountWithoutABand)?;

        band.net = band.net.checked_sub(amount)?;
        if band.net.minor() < 0 {
            return Err(TaxError::DiscountTooLarge);
        }
        discounted = discounted.checked_add(amount)?;
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
    let discount = (!discounted.is_zero()).then_some(discounted);

    Ok(Totals {
        net,
        tax,
        gross,
        discount,
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

    /// **The number the whole feature exists for.** 100.00 discounted by 15.00
    /// is taxed on 85.00, so the tax is 12.75 and not 15.00.
    #[test]
    fn a_discount_comes_off_before_the_tax_is_worked_out() {
        let standard = Vat::shipped(VatCategory::Standard);
        let totals = total(
            [(standard, money(10_000))],
            [(standard, money(1_500))],
            sar(),
        )
        .unwrap();

        assert_eq!(totals.net, money(8_500), "the taxable amount");
        assert_eq!(totals.tax, money(1_275), "15% of 85.00, not of 100.00");
        assert_eq!(totals.gross, money(9_775));
        assert_eq!(totals.discount(), money(1_500));
        assert_eq!(
            totals.before_discount().unwrap(),
            money(10_000),
            "what the lines came to"
        );
        // The band the invoice reports is the discounted one, because that is
        // what was supplied and what is declared.
        assert_eq!(totals.bands[0].net, money(8_500));
        assert_eq!(totals.bands[0].tax, money(1_275));
    }

    /// A discount comes off the band it names, and **only** that one.
    ///
    /// Discounting the exempt part of a mixed invoice must not reduce the tax
    /// on the standard-rated part, because that tax was charged.
    #[test]
    fn a_discount_comes_off_the_band_it_names() {
        let standard = Vat::shipped(VatCategory::Standard);
        let exempt = Vat::shipped(VatCategory::Exempt);

        let totals = total(
            [(standard, money(10_000)), (exempt, money(10_000))],
            [(exempt, money(5_000))],
            sar(),
        )
        .unwrap();

        let taxed = totals
            .bands
            .iter()
            .find(|b| b.category == VatCategory::Standard)
            .unwrap();
        assert_eq!(taxed.net, money(10_000), "the standard band is untouched");
        assert_eq!(taxed.tax, money(1_500));

        let untaxed = totals
            .bands
            .iter()
            .find(|b| b.category == VatCategory::Exempt)
            .unwrap();
        assert_eq!(untaxed.net, money(5_000));
        assert_eq!(untaxed.tax, money(0));

        assert_eq!(
            totals.tax,
            money(1_500),
            "discounting an exempt supply frees no tax"
        );
    }

    /// Nothing on the invoice is treated the way the discount says: taking it
    /// anyway would reclaim tax that was never charged.
    #[test]
    fn a_discount_needs_a_band_to_come_off() {
        let standard = Vat::shipped(VatCategory::Standard);
        let zero = Vat::shipped(VatCategory::Zero);
        assert_eq!(
            total([(standard, money(10_000))], [(zero, money(100))], sar()),
            Err(TaxError::DiscountWithoutABand)
        );
    }

    #[test]
    fn a_discount_is_positive_and_no_larger_than_what_it_comes_off() {
        let standard = Vat::shipped(VatCategory::Standard);
        assert_eq!(
            total(
                [(standard, money(10_000))],
                [(standard, money(-100))],
                sar()
            ),
            Err(TaxError::NotADiscount),
            "a negative discount is a charge"
        );
        assert_eq!(
            total([(standard, money(10_000))], [(standard, money(0))], sar()),
            Err(TaxError::NotADiscount)
        );
        assert_eq!(
            total(
                [(standard, money(10_000))],
                [(standard, money(10_001))],
                sar()
            ),
            Err(TaxError::DiscountTooLarge)
        );
        // Exactly all of it is allowed: a fully discounted line comes to zero.
        let nothing = total(
            [(standard, money(10_000))],
            [(standard, money(10_000))],
            sar(),
        )
        .unwrap();
        assert_eq!(nothing.net, money(0));
        assert_eq!(nothing.tax, money(0));
    }

    /// An invoice with no discount reports none — which is what every invoice
    /// issued before this existed decodes as.
    #[test]
    fn no_discount_is_absent_rather_than_zero() {
        let standard = Vat::shipped(VatCategory::Standard);
        let totals = total([(standard, money(10_000))], [], sar()).unwrap();
        assert_eq!(totals.discount, None);
        assert_eq!(totals.discount(), money(0));
        assert_eq!(totals.before_discount().unwrap(), totals.net);

        // And it does not appear on the wire at all, so an event written today
        // is byte-identical to one written before discounts existed.
        let json = serde_json::to_string(&totals).unwrap();
        assert!(!json.contains("discount"), "{json}");

        // Which is what makes this decode without an upcaster: the payload of
        // an invoice issued last year, verbatim.
        let older = r#"{"net":{"minor":10000,"currency":"SAR"},
                        "tax":{"minor":1500,"currency":"SAR"},
                        "gross":{"minor":11500,"currency":"SAR"},
                        "bands":[]}"#;
        let back: Totals = serde_json::from_str(older).unwrap();
        assert_eq!(back.discount, None);
        assert_eq!(back.discount(), money(0));
    }

    #[test]
    fn fifteen_percent_of_a_round_amount_is_exact() {
        let vat = Vat::shipped(VatCategory::Standard);
        assert_eq!(vat.basis_points, 1_500);
        assert_eq!(vat.on(money(10_000)).unwrap(), money(1_500));
    }

    #[test]
    fn halves_round_away_from_zero_in_both_directions() {
        let vat = Vat::shipped(VatCategory::Standard);
        // 0.10 × 15% = 0.015 — exactly half a halala.
        assert_eq!(vat.on(money(10)).unwrap(), money(2));
        assert_eq!(vat.on(money(-10)).unwrap(), money(-2));
    }

    #[test]
    fn a_credit_line_reverses_its_own_tax_exactly() {
        // The reason rounding is symmetric. With half-up, +0.015 rounds to 2 and
        // -0.015 rounds to -1, and an invoice plus its exact credit note leaves a
        // stray halala in VAT payable forever.
        let vat = Vat::shipped(VatCategory::Standard);
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
            Vat::shipped(VatCategory::Zero)
                .on(money(9_999))
                .unwrap()
                .minor(),
            0
        );
        assert_eq!(
            Vat::shipped(VatCategory::Exempt)
                .on(money(9_999))
                .unwrap()
                .minor(),
            0
        );

        let totals = total(
            [
                (Vat::shipped(VatCategory::Zero), money(1_000)),
                (Vat::shipped(VatCategory::Exempt), money(2_000)),
            ],
            [],
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
        let standard = Vat::shipped(VatCategory::Standard);
        let per_line: i64 = (0..3)
            .map(|_| standard.on(money(3_333)).unwrap().minor())
            .sum();
        let per_band = total((0..3).map(|_| (standard, money(3_333))), [], sar()).unwrap();
        assert_eq!(per_line, 1_500);
        assert_eq!(per_band.tax, money(1_500));

        // 0.10 three times: per line 0.02 × 3 = 0.06; per band 0.30 → 0.045 →
        // 0.05. The authority computes the second one.
        let per_line: i64 = (0..3)
            .map(|_| standard.on(money(10)).unwrap().minor())
            .sum();
        let per_band = total((0..3).map(|_| (standard, money(10))), [], sar()).unwrap();
        assert_eq!(per_line, 6);
        assert_eq!(per_band.tax, money(5), "banding is not the same as summing");
    }

    #[test]
    fn line_order_does_not_change_the_event() {
        let a = Vat::shipped(VatCategory::Standard);
        let z = Vat::shipped(VatCategory::Zero);

        let one = total(
            [(a, money(100)), (z, money(200)), (a, money(300))],
            [],
            sar(),
        )
        .unwrap();
        let two = total(
            [(z, money(200)), (a, money(300)), (a, money(100))],
            [],
            sar(),
        )
        .unwrap();

        assert_eq!(one, two, "a reordered request must produce the same totals");
    }

    #[test]
    fn an_empty_invoice_totals_to_nothing_rather_than_failing() {
        // Refusing an empty invoice is the command's job, not the arithmetic's.
        let totals = total([], [], sar()).unwrap();
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
            Vat::shipped(VatCategory::Standard)
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
