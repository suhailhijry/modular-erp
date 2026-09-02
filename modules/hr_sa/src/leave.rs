//! What the Labour Law entitles somebody to be away for.
//!
//! # Why the entitlement is here and what has been taken is not
//!
//! `hr` records leave: which days, which kind, how many. That is the same in
//! every country. **How much somebody is owed** is Article 109 and Article 117,
//! and putting it in `hr` would make that module learn one country's statute —
//! the same argument that keeps VAT out of `sales`.
//!
//! # Annual leave
//!
//! Article 109. Twenty-one days a year, rising to **thirty after five years'
//! service** with the same employer. Pro-rated for a part year, because
//! somebody who joined in July is owed half a year's worth and the law says so
//! rather than leaving it to a business's generosity.
//!
//! The step is at five years *of service*, not at the fifth anniversary of the
//! leave year — so it can land mid-year, and the entitlement for that year is
//! the two rates over the days each applied to. Rounding a statutory
//! entitlement downward is the version that ends up in front of a labour
//! office, so it rounds up.
//!
//! # Sick leave
//!
//! Article 117, and it is a **pay** scale rather than a day count: the first
//! thirty days are at full pay, the next sixty at three quarters, and the
//! thirty after that unpaid — one hundred and twenty days in a year, after
//! which there is no further entitlement.
//!
//! So this answers what a period of sickness is *worth*, which is what payroll
//! needs, rather than how many days are left.

use erp_types::{Money, MoneyError};

/// Days in a year, matching [`crate::gratuity`] and for the same reason.
const DAYS_IN_YEAR: i64 = 365;

/// Service at which annual leave steps up, in days.
const STEP_DAYS: i64 = 5 * DAYS_IN_YEAR;

/// Article 109's two rates.
const BASE_DAYS: i64 = 21;
const LONG_SERVICE_DAYS: i64 = 30;

/// How much annual leave a year of service earns.
///
/// `served_at_start` is how long they had been employed on the first day of the
/// leave year, and `days_in_year` is how much of that year they were employed
/// for — 365 for a full one, fewer for a joiner or a leaver.
///
/// **The step can land mid-year**, so this is the two rates over the days each
/// applied to rather than one rate chosen by a comparison. A business whose
/// employee passes five years in July owes them the higher rate from July, and
/// an implementation that picked a single rate would be wrong by up to nine
/// days in exactly the year somebody notices.
#[must_use]
pub fn annual_entitlement(served_at_start: i64, days_in_year: i64) -> i64 {
    if days_in_year <= 0 {
        return 0;
    }
    let served_at_start = served_at_start.max(0);
    let served_at_end = served_at_start + days_in_year;

    // Days of this year spent below the five-year mark, and above it.
    let below = (STEP_DAYS - served_at_start).clamp(0, days_in_year);
    let above = (served_at_end - STEP_DAYS.max(served_at_start)).clamp(0, days_in_year);

    // Rounded **up**: rounding a statutory entitlement down is the version that
    // ends up in front of a labour office. Integer arithmetic throughout, which
    // this workspace requires and which is right for a number somebody counts.
    let earned = below * BASE_DAYS + above * LONG_SERVICE_DAYS;
    (earned + DAYS_IN_YEAR - 1) / DAYS_IN_YEAR
}

/// What a stretch of sick leave is worth.
///
/// Article 117's scale, over the **leave year**: the first thirty days at full
/// pay, the next sixty at three quarters, the thirty after that unpaid.
///
/// `already_taken` is how many sick days have been used in the year before this
/// stretch, which is what makes the bands land in the right place — a second
/// illness in the same year does not start again at full pay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SickPay {
    /// Days paid in full.
    pub full: i64,
    /// Days at three quarters.
    pub partial: i64,
    /// Days with no pay, including anything past the hundred and twenty.
    pub unpaid: i64,
}

/// The band boundaries, in days taken within the year.
const FULL_UNTIL: i64 = 30;
const PARTIAL_UNTIL: i64 = 90;
const ENTITLEMENT_ENDS: i64 = 120;

/// Splits a stretch of sick leave across Article 117's bands.
#[must_use]
pub fn sick_days(already_taken: i64, days: i64) -> SickPay {
    if days <= 0 {
        return SickPay {
            full: 0,
            partial: 0,
            unpaid: 0,
        };
    }
    let from = already_taken.max(0);
    let until = from + days;

    let overlap = |start: i64, end: i64| (until.min(end) - from.max(start)).max(0);

    SickPay {
        full: overlap(0, FULL_UNTIL),
        partial: overlap(FULL_UNTIL, PARTIAL_UNTIL),
        // Everything past ninety days, including past the hundred and twenty
        // where the entitlement itself ends — both are unpaid, and a caller
        // asking "what is this worth" gets the same answer.
        unpaid: overlap(PARTIAL_UNTIL, i64::MAX),
    }
}

impl SickPay {
    /// What the stretch comes to, on a daily rate.
    ///
    /// Three quarters is `apportioned(3, 4)`, so the rounding is the one
    /// policy this codebase uses everywhere — half away from zero, which here
    /// means toward the employee.
    pub fn value(&self, daily: Money) -> Result<Money, MoneyError> {
        let full = daily.checked_mul_int(self.full)?;
        let partial = daily.checked_mul_int(self.partial)?.apportioned(3, 4)?;
        full.checked_add(partial)
    }

    /// Whether this stretch runs past the hundred and twenty days the law
    /// entitles somebody to in a year.
    ///
    /// Not a refusal — being ill longer than the entitlement is a real thing
    /// that happens, and what follows is a conversation rather than an error.
    #[must_use]
    pub const fn exhausts_entitlement(&self, already_taken: i64, days: i64) -> bool {
        already_taken + days > ENTITLEMENT_ENDS
    }
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

    /// **Twenty-one days, and thirty after five years.**
    #[test]
    fn annual_leave_steps_up_after_five_years() {
        // A first full year.
        assert_eq!(annual_entitlement(0, 365), 21);
        // A fourth full year, still below the step.
        assert_eq!(annual_entitlement(3 * 365, 365), 21);
        // A sixth, entirely above it.
        assert_eq!(annual_entitlement(6 * 365, 365), 30);
    }

    /// **The step can land mid-year**, and an implementation that picked one
    /// rate would be wrong by up to nine days in exactly the year somebody
    /// notices.
    #[test]
    fn a_year_that_crosses_five_years_is_paid_at_both_rates() {
        // Four and a half years in at the start: half the year below the step,
        // half above. 182 × 21 + 183 × 30 = 3,822 + 5,490 = 9,312, over 365 is
        // 25.5 — rounded up, 26.
        let entitlement = annual_entitlement(4 * 365 + 182, 365);
        assert_eq!(entitlement, 26);
        assert!(
            entitlement > 21 && entitlement < 30,
            "a mid-year step was resolved to one rate or the other"
        );
    }

    /// A joiner is owed the part of the year they were here for, and the law
    /// says so rather than leaving it to a business's generosity.
    #[test]
    fn a_part_year_is_pro_rated() {
        // Half a year, first year of service: half of 21, rounded up.
        assert_eq!(annual_entitlement(0, 182), 11);
        assert_eq!(annual_entitlement(0, 0), 0);
        assert_eq!(annual_entitlement(0, -5), 0);
    }

    /// **Rounding goes to the employee**, for the reason the gratuity's does.
    #[test]
    fn an_awkward_part_year_rounds_up() {
        // 100 days of a first year: 100 × 21 / 365 = 5.75.
        assert_eq!(annual_entitlement(0, 100), 6);
    }

    /// **Article 117's scale**, and a second illness does not start again at
    /// full pay.
    #[test]
    fn sick_leave_falls_into_the_bands_it_reaches() {
        // A fortnight, nothing taken before: all at full pay.
        let pay = sick_days(0, 14);
        assert_eq!(pay.full, 14);
        assert_eq!(pay.partial, 0);

        // Forty days from cold: thirty full, ten at three quarters.
        let pay = sick_days(0, 40);
        assert_eq!(pay.full, 30);
        assert_eq!(pay.partial, 10);
        assert_eq!(pay.unpaid, 0);

        // **A second illness in the same year.** Twenty-five days already
        // taken, then ten more: five at full pay and five at three quarters,
        // not ten at full.
        let pay = sick_days(25, 10);
        assert_eq!(pay.full, 5, "a second illness restarted the full-pay band");
        assert_eq!(pay.partial, 5);

        // Past ninety days is unpaid, and past a hundred and twenty the
        // entitlement is gone — both are worth nothing, and the caller is told
        // which by `exhausts_entitlement`.
        let pay = sick_days(85, 40);
        assert_eq!(pay.partial, 5);
        assert_eq!(pay.unpaid, 35);
        assert!(pay.exhausts_entitlement(85, 40));
        assert!(!sick_days(0, 30).exhausts_entitlement(0, 30));
    }

    /// What a stretch is worth, with the codebase's one rounding policy.
    #[test]
    fn sick_pay_values_the_bands_it_split() {
        // Ten full days and ten at three quarters, on 100 a day.
        let pay = sick_days(20, 20);
        assert_eq!(pay.full, 10);
        assert_eq!(pay.partial, 10);
        assert_eq!(
            pay.value(riyals(100)).expect("computes"),
            riyals(1_750),
            "1,000 at full plus 750 at three quarters"
        );

        // Nothing at all is nothing, not an error.
        let none = sick_days(0, 0);
        assert_eq!(none.value(riyals(100)).expect("computes"), riyals(0));
    }
}
