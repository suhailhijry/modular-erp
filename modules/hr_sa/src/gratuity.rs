//! End-of-service benefit, by the Saudi Labour Law formula.
//!
//! # Why this is a pure function and nothing else
//!
//! Because it is arithmetic over a wage and a length of service, and every part
//! of it is a number somebody will check against a calculator. Anything that
//! reads a database, resolves configuration or takes a clock would make it
//! harder to test and no more correct.
//!
//! # The formula
//!
//! Articles 84 and 85 of the Labour Law. Two rules stacked:
//!
//! **The entitlement**, on the wage at the end of service:
//!
//! | service | award |
//! |---|---|
//! | each of the first five years | half a month |
//! | each year after that | one month |
//!
//! Part years are pro-rated, which is why this works in days rather than whole
//! years.
//!
//! **The reduction, when the employee resigns.** A dismissal pays the full
//! entitlement; a resignation is scaled by how long they stayed:
//!
//! | service on resigning | paid |
//! |---|---|
//! | under 2 years | nothing |
//! | 2 to under 5 years | one third |
//! | 5 to under 10 years | two thirds |
//! | 10 years or more | in full |
//!
//! # What this does not decide
//!
//! **Which wage.** The award is on "the last wage", and what counts as wage —
//! basic alone, or basic plus which allowances — is a question about the
//! contract, not about the arithmetic. The caller passes the figure, and
//! `payroll::Salary::gross` is the usual answer.
//!
//! **The exceptions.** Article 87 pays a woman leaving within six months of
//! marriage or three months of childbirth in full, and Article 80 dismissals for
//! cause pay nothing. Both are facts about *why* somebody left, which this
//! module is not told and should not guess: the caller states the reason, and
//! the two exceptional cases are [`Leaving::InFull`] and [`Leaving::ForCause`].

use erp_types::{Money, MoneyError};

/// Days in a year, for pro-rating part years.
///
/// **365 and not 360.** Some Gulf practice uses a 360-day year for convenience;
/// the Labour Law speaks in years and months, and a calendar year is what a
/// court would read. A tenant who needs the other convention has an argument to
/// make, and it should be an argument rather than a constant nobody noticed.
const DAYS_IN_YEAR: i64 = 365;

/// Where the award rate changes: after five years, a full month a year instead
/// of half.
const STEP_DAYS: i64 = 5 * DAYS_IN_YEAR;

/// Basis points of a month earned per day, before and after the step.
///
/// Half a month per year is `5_000 / 365` of a month per day; a full month is
/// `10_000 / 365`. Kept as the numerators so the whole calculation is one
/// integer fraction — see [`end_of_service`].
const HALF_MONTH_BP: i64 = 5_000;
const FULL_MONTH_BP: i64 = 10_000;

/// Why somebody left, which is what decides whether the award is reduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Leaving {
    /// The employer ended it. **The full entitlement**, whatever the length of
    /// service.
    Dismissed,
    /// The employee ended it. Scaled by length of service — see the module
    /// docs.
    Resigned,
    /// Paid in full despite being the employee's choice: Article 87's marriage
    /// and childbirth cases, and a fixed-term contract simply running out.
    ///
    /// A variant rather than a flag on `Resigned`, so a caller has to say which
    /// they mean rather than pass a boolean somebody will read backwards.
    InFull,
    /// Article 80: dismissal for cause. **Nothing is owed**, and it is here
    /// rather than left to the caller returning zero, because it is a decision
    /// the record should carry.
    ForCause,
}

/// What somebody is owed when they leave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Award {
    /// Before the resignation reduction.
    pub entitlement: Money,
    /// What is actually paid.
    pub payable: Money,
    /// How long they served, in days.
    pub days: i64,
}

/// Computes the end-of-service benefit.
///
/// `wage` is the monthly figure the award is calculated on — see the module
/// docs on which one. `days` is the length of service.
///
/// A service length of zero or less pays nothing rather than failing: somebody
/// who left the day they joined is a real record, and an error would make the
/// caller handle a case that has an obvious answer.
pub fn end_of_service(wage: Money, days: i64, leaving: Leaving) -> Result<Award, MoneyError> {
    let zero = Money::zero(wage.currency());
    if days <= 0 || !wage.is_positive() || leaving == Leaving::ForCause {
        return Ok(Award {
            entitlement: zero,
            payable: zero,
            days: days.max(0),
        });
    }

    // **Integers throughout, and one rounding at the end.**
    //
    // The award is a fraction of a month's wage:
    //
    //     (days in the first five years × 5,000
    //      + days after that            × 10,000)
    //     ────────────────────────────────────────  of one month
    //                 365 × 10,000
    //
    // Written as a single numerator over a single denominator so `apportioned`
    // rounds once. Computing months as a decimal and rounding along the way is
    // how a gratuity comes out a halala short of what the employee's own
    // calculator says — and this workspace forbids floating-point arithmetic
    // for exactly that reason.
    let first_five = days.min(STEP_DAYS);
    let beyond = (days - STEP_DAYS).max(0);
    let numerator = first_five * HALF_MONTH_BP + beyond * FULL_MONTH_BP;
    let denominator = DAYS_IN_YEAR * FULL_MONTH_BP;

    // Half a halala goes to the employee: `apportioned` rounds away from zero.
    // Rounding a statutory entitlement downward is the version that ends up in
    // front of a labour office.
    let entitlement = wage.apportioned(numerator, denominator)?;

    let payable = match leaving {
        Leaving::Dismissed | Leaving::InFull => entitlement,
        Leaving::ForCause => zero,
        // The ladder, in days rather than years, so the boundary is the same
        // arithmetic as the award above rather than a second conversion that
        // could disagree with it at the edge.
        Leaving::Resigned => match days {
            d if d < 2 * DAYS_IN_YEAR => zero,
            d if d < 5 * DAYS_IN_YEAR => entitlement.apportioned(1, 3)?,
            d if d < 10 * DAYS_IN_YEAR => entitlement.apportioned(2, 3)?,
            _ => entitlement,
        },
    };

    Ok(Award {
        entitlement,
        payable,
        days,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_types::CurrencyCode;

    fn sar(minor: i64) -> Money {
        Money::from_minor(
            minor,
            CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!("a real code")),
        )
    }

    fn riyals(major: i64) -> Money {
        sar(major * 100)
    }

    /// Whole years, in days. Part years are stated in days directly, so no
    /// test's expected figure comes from the same rounding as the code.
    fn years(n: i64) -> i64 {
        n * DAYS_IN_YEAR
    }

    /// **Half a month for each of the first five years, a month for each after.**
    ///
    /// The worked example everybody checks: 10,000 a month, ten years served,
    /// dismissed. Five years at half a month is 2.5 months; five more at a full
    /// month is 5. Seven and a half months, so 75,000.
    #[test]
    fn ten_years_at_ten_thousand_is_seven_and_a_half_months() {
        let award =
            end_of_service(riyals(10_000), years(10), Leaving::Dismissed).expect("computes");
        assert_eq!(award.entitlement, riyals(75_000));
        assert_eq!(award.payable, riyals(75_000), "a dismissal pays in full");
    }

    /// Under five years is half a month a year and nothing else.
    #[test]
    fn three_years_is_a_month_and_a_half() {
        let award = end_of_service(riyals(10_000), years(3), Leaving::Dismissed).expect("computes");
        assert_eq!(award.entitlement, riyals(15_000));
    }

    /// **Part years are pro-rated**, which is why the arithmetic is in days.
    ///
    /// Stated in days rather than through the `years` helper, because the
    /// helper truncates — 2.5 years is 912.5 days and it gives 912 — and a test
    /// whose expected figure comes from the same rounding as the code under it
    /// is a test that agrees with itself.
    ///
    /// 730 days is exactly two years: 1 month of wage, so 10,000.
    /// 1,095 days is exactly three: 1.5 months, so 15,000.
    /// Halfway between, 912 days, is `912 × 5,000 / 3,650,000` of a month —
    /// 1.24931506… — **not** 1.25, and the difference is the point. On 10,000 a
    /// month that is 12,493.15, and the fifteen halalas are there because the
    /// arithmetic rounds once at the end rather than rounding the months first.
    #[test]
    fn a_part_year_is_pro_rated_rather_than_rounded_away() {
        let wage = riyals(10_000);
        assert_eq!(
            end_of_service(wage, 730, Leaving::Dismissed)
                .expect("computes")
                .entitlement,
            riyals(10_000)
        );
        assert_eq!(
            end_of_service(wage, 1_095, Leaving::Dismissed)
                .expect("computes")
                .entitlement,
            riyals(15_000)
        );
        assert_eq!(
            end_of_service(wage, 912, Leaving::Dismissed)
                .expect("computes")
                .entitlement,
            sar(1_249_315),
            "a part year was rounded to a whole one"
        );
    }

    /// **The resignation ladder**, which is the half most calculators get wrong.
    #[test]
    fn resigning_is_scaled_by_how_long_they_stayed() {
        let wage = riyals(10_000);

        // Under two years: nothing.
        // A day short of two years, stated in days: the boundary is what is
        // being tested, so it must not come from a helper that rounds.
        let award = end_of_service(wage, 2 * 365 - 1, Leaving::Resigned).expect("computes");
        assert!(award.entitlement.is_positive(), "they earned something");
        assert_eq!(
            award.payable,
            riyals(0),
            "resigning under two years pays nothing"
        );

        // Two to five: a third. Three years earns 1.5 months = 15,000; a third
        // is 5,000.
        let award = end_of_service(wage, years(3), Leaving::Resigned).expect("computes");
        assert_eq!(award.payable, riyals(5_000));

        // Five to ten: two thirds. Seven years earns 2.5 + 2 = 4.5 months =
        // 45,000; two thirds is 30,000.
        let award = end_of_service(wage, years(7), Leaving::Resigned).expect("computes");
        assert_eq!(award.entitlement, riyals(45_000));
        assert_eq!(award.payable, riyals(30_000));

        // Ten and over: in full.
        let award = end_of_service(wage, years(10), Leaving::Resigned).expect("computes");
        assert_eq!(award.payable, riyals(75_000));
    }

    /// The two exceptional reasons, which are facts about *why* somebody left
    /// and cannot be inferred from the numbers.
    #[test]
    fn the_exceptional_reasons_are_stated_and_not_inferred() {
        let wage = riyals(10_000);

        // Article 87: paid in full even though she chose to leave, and even
        // under two years — where a resignation would pay nothing.
        let award = end_of_service(wage, years(1), Leaving::InFull).expect("computes");
        assert_eq!(award.payable, riyals(5_000));

        // Article 80: nothing, however long they served.
        let award = end_of_service(wage, years(20), Leaving::ForCause).expect("computes");
        assert_eq!(award.entitlement, riyals(0));
        assert_eq!(award.payable, riyals(0));
    }

    /// Somebody who left the day they joined is a real record with an obvious
    /// answer, not an error the caller has to handle.
    #[test]
    fn no_service_pays_nothing_rather_than_failing() {
        let award = end_of_service(riyals(10_000), 0, Leaving::Dismissed).expect("computes");
        assert_eq!(award.payable, riyals(0));
        assert_eq!(award.days, 0);

        let award = end_of_service(riyals(10_000), -5, Leaving::Dismissed).expect("computes");
        assert_eq!(award.days, 0);
    }

    /// **The rounding goes to the employee.** Rounding a statutory entitlement
    /// down is the version that ends up in front of a labour office.
    #[test]
    fn an_awkward_wage_rounds_towards_the_employee() {
        // 3,333.33 a month, one year: half a month is 1,666.665 → 1,666.67.
        let award = end_of_service(sar(333_333), years(1), Leaving::Dismissed).expect("computes");
        assert_eq!(award.entitlement, sar(166_667));
    }
}
