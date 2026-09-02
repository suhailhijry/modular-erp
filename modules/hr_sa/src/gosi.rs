//! Social insurance contributions.
//!
//! # The rates are configuration, and the shipped ones are a starting point
//!
//! **This is the important sentence in this file.** GOSI's schedule is set by
//! the authority and has changed — most recently for people entering the system
//! after the 2024 pension reform, who are on a different and rising scale from
//! those already in it. A build that hard-coded a percentage would be quietly
//! wrong for some employees from the day it shipped and for everybody
//! eventually.
//!
//! So the rates are a tenant configuration value with defaults, exactly as
//! `tax_sa` treats the VAT rate as seeded data rather than a constant. **The
//! defaults here must be checked against the current GOSI schedule before a
//! tenant runs payroll against them**, and the API returns them so somebody
//! can see what they are rather than discovering them on a payslip.
//!
//! # The shape, which is stable even when the numbers are not
//!
//! Two sides — the employee's share, withheld from pay, and the employer's,
//! which is a cost the business bears on top. Both are a percentage of a
//! **contribution base** that is not the whole salary: it is basic plus
//! housing, and it is capped.
//!
//! Non-Saudi employees are on a different footing: no pension, and the employer
//! pays occupational hazards only. That distinction is a fact about the person,
//! so it is on the calculation's input rather than inferred.

use erp_types::{Money, MoneyError};
use serde::{Deserialize, Serialize};

/// Which schedule somebody is on.
///
/// **A fact about the person, stated rather than inferred.** Nothing here can
/// work it out from a name or an iqama number, and a module that tried would be
/// wrong about somebody on their first payslip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Footing {
    /// A Saudi national: pension, hazards and unemployment insurance.
    Saudi,
    /// A non-Saudi employee: **occupational hazards only**, and the employer
    /// pays all of it.
    NonSaudi,
}

/// What a tenant's GOSI schedule is.
///
/// # These defaults are a starting point and not an authority
///
/// They reflect the long-standing schedule — 9% pension each side, 2% hazards
/// from the employer, 0.75% unemployment insurance each side for Saudis, and
/// 2% hazards for non-Saudis — and **the authority's current schedule is what
/// governs**. A tenant running payroll must confirm them; see the module docs
/// for why they are configuration at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// Withheld from a Saudi employee's pay, in basis points.
    pub saudi_employee_bp: u32,
    /// Paid by the employer for a Saudi employee, in basis points.
    pub saudi_employer_bp: u32,
    /// Withheld from a non-Saudi employee's pay. **Zero**: hazards cover is the
    /// employer's alone.
    pub non_saudi_employee_bp: u32,
    /// Paid by the employer for a non-Saudi employee.
    pub non_saudi_employer_bp: u32,
    /// The most of one month's pay that contributions are computed on, in minor
    /// units. Above it, the base stops rising.
    ///
    /// `None` disables the cap, which is not a configuration anybody should
    /// want — it is here so a tenant whose schedule has no ceiling can say so
    /// rather than typing a number large enough to never bind.
    pub ceiling_minor: Option<i64>,
}

impl Default for Schedule {
    /// # These numbers were not verified against the authority when written
    ///
    /// They are the long-standing figures and they are here so the module is
    /// usable and its shape visible. **The 2024 pension reform put new entrants
    /// on a separate and rising scale, which this shape does not express at
    /// all** — it has one rate per footing, not one per cohort. If that
    /// distinction is real for a tenant, `Footing` needs a third case or a date
    /// and the change is to the shape rather than to the numbers.
    ///
    /// See `docs/IMPLEMENTATION.md`, "For review".
    fn default() -> Self {
        Self {
            // 9% pension + 0.75% unemployment insurance.
            saudi_employee_bp: 975,
            // 9% pension + 2% occupational hazards + 0.75% unemployment.
            saudi_employer_bp: 1_175,
            non_saudi_employee_bp: 0,
            // Occupational hazards.
            non_saudi_employer_bp: 200,
            // 45,000 riyals a month.
            ceiling_minor: Some(4_500_000),
        }
    }
}

impl Schedule {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "hr_sa.gosi";

    /// What this tenant has configured, or the shipped default.
    ///
    /// A tenant who *has* configured one and stored something unusable gets an
    /// error rather than silently falling back to the default — the same
    /// posture `pos` and `prepaid` take, and for a sharper reason: a payroll run
    /// on the wrong schedule under-withholds from every employee, and the
    /// business owes the difference.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or_else(Self::default, |configured| configured.value))
    }

    fn rates(self, footing: Footing) -> (u32, u32) {
        match footing {
            Footing::Saudi => (self.saudi_employee_bp, self.saudi_employer_bp),
            Footing::NonSaudi => (self.non_saudi_employee_bp, self.non_saudi_employer_bp),
        }
    }
}

/// What one person's contributions come to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contribution {
    /// What contributions were computed on: the base, after the ceiling.
    pub base: Money,
    /// Withheld from their pay.
    pub employee: Money,
    /// Paid by the business on top of their pay.
    pub employer: Money,
}

impl Contribution {
    /// Both sides together, which is what reaches the authority.
    pub fn total(&self) -> Result<Money, MoneyError> {
        self.employee.checked_add(self.employer)
    }
}

/// Computes one person's contributions.
///
/// `base` is basic plus housing — **not the whole salary**, and not net. Which
/// allowances count is a question about the contract and the authority's
/// definition, so the caller passes the figure rather than this guessing from a
/// `Salary`.
///
/// The ceiling is applied to the base before either rate, which is the order
/// that matters: capping the *contribution* instead would give the employee and
/// the employer different effective bases.
pub fn contribution(
    base: Money,
    footing: Footing,
    schedule: Schedule,
) -> Result<Contribution, MoneyError> {
    let base = match schedule.ceiling_minor {
        Some(ceiling) if base.minor() > ceiling => Money::from_minor(ceiling, base.currency()),
        _ => base,
    };
    if !base.is_positive() {
        let zero = Money::zero(base.currency());
        return Ok(Contribution {
            base: zero,
            employee: zero,
            employer: zero,
        });
    }

    let (withheld, borne) = schedule.rates(footing);
    Ok(Contribution {
        base,
        employee: base.apportioned(i64::from(withheld), 10_000)?,
        employer: base.apportioned(i64::from(borne), 10_000)?,
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

    /// A Saudi employee on 10,000: 9.75% withheld, 11.75% from the employer.
    #[test]
    fn a_saudi_employee_has_both_sides_computed_on_the_base() {
        let c =
            contribution(riyals(10_000), Footing::Saudi, Schedule::default()).expect("computes");
        assert_eq!(c.base, riyals(10_000));
        assert_eq!(c.employee, riyals(975));
        assert_eq!(c.employer, riyals(1_175));
        assert_eq!(c.total().expect("sums"), riyals(2_150));
    }

    /// **A non-Saudi employee has nothing withheld**, and the employer pays
    /// hazards cover alone. Getting this backwards takes money from somebody
    /// who does not owe it.
    #[test]
    fn a_non_saudi_employee_has_nothing_withheld() {
        let c =
            contribution(riyals(10_000), Footing::NonSaudi, Schedule::default()).expect("computes");
        assert_eq!(c.employee, riyals(0));
        assert_eq!(c.employer, riyals(200));
    }

    /// **The ceiling caps the base, not the contribution.**
    ///
    /// Capping the contribution instead would give the employee and the
    /// employer different effective bases, which is the subtle version of this
    /// bug and the one a payslip does not show.
    #[test]
    fn the_ceiling_applies_to_the_base_and_both_sides_see_it() {
        let schedule = Schedule::default();
        let c = contribution(riyals(60_000), Footing::Saudi, schedule).expect("computes");

        assert_eq!(c.base, riyals(45_000), "the base was not capped");
        assert_eq!(
            c.employee,
            riyals(4_387).checked_add(sar(50)).expect("sums")
        );
        assert_eq!(
            c.employer,
            riyals(5_287).checked_add(sar(50)).expect("sums")
        );

        // Exactly at the ceiling is not above it.
        let at = contribution(riyals(45_000), Footing::Saudi, schedule).expect("computes");
        assert_eq!(at.base, riyals(45_000));
        assert_eq!(at.employee, c.employee);
    }

    /// A tenant on a different schedule gets their own numbers, which is the
    /// whole reason this is configuration.
    #[test]
    fn a_configured_schedule_is_what_is_used() {
        let schedule = Schedule {
            saudi_employee_bp: 1_100,
            saudi_employer_bp: 1_300,
            ceiling_minor: None,
            ..Schedule::default()
        };
        let c = contribution(riyals(60_000), Footing::Saudi, schedule).expect("computes");
        assert_eq!(c.base, riyals(60_000), "a schedule with no ceiling capped");
        assert_eq!(c.employee, riyals(6_600));
        assert_eq!(c.employer, riyals(7_800));
    }
}
