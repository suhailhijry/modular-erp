//! One month's pay, from the moment it is drafted to the moment it posts.
//!
//! # Why a run is an aggregate and not a report
//!
//! Because it is a decision, not a derivation. What somebody is paid this month
//! depends on their salary *as it was when the run was made*, and a rise
//! recorded next week must not restate last month's payslip. So the amounts are
//! frozen onto the run's events (L5) — the same argument an invoice makes about
//! the buyer's name, and for higher stakes: a payslip is a document a person
//! files with their bank.
//!
//! # Two steps, and why they are not one
//!
//! **Drafting** computes what everybody would be paid and posts nothing.
//! **Approving** posts the journal entry. A business reads the draft, finds the
//! two people whose overtime is wrong, fixes them and runs it again — and a
//! single-step run would have posted the first attempt to the ledger before
//! anybody looked at it.
//!
//! That is also what makes the run idempotent in the way that matters: a draft
//! can be recomputed as often as anybody likes, and the posting happens once.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, Money, MoneyError, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

/// What one person is paid in one run.
///
/// **Frozen at drafting**, including their name: a payslip says who it was for,
/// and somebody who marries next month does not get a new copy of last month's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payslip {
    pub employee: AggregateId,
    /// As it was when the run was made.
    pub name: String,
    pub basic: Money,
    /// Basic plus allowances, which is what statutory contributions are
    /// computed from.
    pub gross: Money,
    /// What they earned on the work they performed, at the rate on their
    /// salary. Zero for everybody a business pays no commission to, which is
    /// most people.
    ///
    /// **Included in `gross`**, because it is pay: statutory contributions and
    /// end-of-service are computed from what somebody earned, not from the part
    /// of it that was predictable.
    ///
    /// Always present, even at zero. `Money` has no `Default` — a zero needs a
    /// currency — so there is no `#[serde(default)]` to fall back on, and a
    /// payslip that omitted the field would be one a reader had to guess at.
    pub commission: Money,
    /// What it was earned on. Recorded so the payslip can justify the number —
    /// "five per cent of 24,000" is a figure somebody will query.
    pub performed: Money,
    /// What is taken off. **Not tax and not GOSI**: those are the country
    /// module's, and `hr` refuses to let a business type them in.
    pub deductions: Money,
    /// What actually gets paid.
    pub net: Money,
}

/// The month a run is for.
///
/// A year and a month, not a date range: payroll is monthly everywhere this
/// system will run, and a range would invite two runs that overlap by a day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Period {
    pub year: i32,
    /// 1–12.
    pub month: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a month")]
pub struct NotAPeriod(pub String);

impl Period {
    /// From `2026-05`.
    pub fn parse(raw: &str) -> Result<Self, NotAPeriod> {
        let (year, month) = raw
            .split_once('-')
            .ok_or_else(|| NotAPeriod(raw.to_owned()))?;
        let year: i32 = year.parse().map_err(|_| NotAPeriod(raw.to_owned()))?;
        let month: u32 = month.parse().map_err(|_| NotAPeriod(raw.to_owned()))?;
        if !(1..=12).contains(&month) {
            return Err(NotAPeriod(raw.to_owned()));
        }
        Ok(Self { year, month })
    }

    /// The first day of the month.
    #[must_use]
    pub fn starts_on(self) -> Option<chrono::NaiveDate> {
        chrono::NaiveDate::from_ymd_opt(self.year, self.month, 1)
    }

    /// **The last day of the month**, which is when pay is treated as having
    /// been earned and what the journal entry is dated to.
    ///
    /// Not the day the run happened: a February run posted on the 3rd of March
    /// would land in the wrong period, and the whole point of a period is that
    /// it does not.
    #[must_use]
    pub fn ends_on(self) -> Option<chrono::NaiveDate> {
        let (year, month) = if self.month == 12 {
            (self.year + 1, 1)
        } else {
            (self.year, self.month + 1)
        };
        chrono::NaiveDate::from_ymd_opt(year, month, 1)?.pred_opt()
    }
}

impl std::fmt::Display for Period {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}", self.year, self.month)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    /// Computed, and posting nothing.
    ///
    /// Drafting again **replaces**: a business fixes two payslips and runs it
    /// over, and a run that accumulated drafts would pay somebody twice.
    Drafted {
        period: Period,
        payslips: Vec<Payslip>,
        /// What the whole run comes to. Stored rather than derived so a replay
        /// reproduces what was decided rather than what today's arithmetic
        /// would decide (L5).
        gross: Money,
        deductions: Money,
        net: Money,
        at: Timestamp,
    },
    /// Approved and posted. **The run is now a fact about the books.**
    Approved {
        /// The journal entry it made.
        entry: AggregateId,
        at: Timestamp,
    },
}

impl DomainEvent for RunEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Drafted { .. } => Self::NAMES[0],
            Self::Approved { .. } => Self::NAMES[1],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

impl RunEvent {
    pub const NAMES: [&'static str; 2] = ["payroll.run.drafted", "payroll.run.approved"];
}

#[derive(Debug, Default, Clone)]
pub struct Run {
    pub drafted: bool,
    pub period: Option<Period>,
    pub payslips: Vec<Payslip>,
    pub gross: Option<Money>,
    pub deductions: Option<Money>,
    pub net: Option<Money>,
    /// Set when it posted. A run is approved once.
    pub entry: Option<AggregateId>,
}

impl Aggregate for Run {
    type Event = RunEvent;

    fn domain() -> DomainName {
        crate::domain("payroll_run")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            RunEvent::Drafted {
                period,
                payslips,
                gross,
                deductions,
                net,
                ..
            } => {
                self.drafted = true;
                self.period = Some(*period);
                self.payslips.clone_from(payslips);
                self.gross = Some(*gross);
                self.deductions = Some(*deductions);
                self.net = Some(*net);
            }
            RunEvent::Approved { entry, .. } => self.entry = Some(entry.clone()),
        }
    }
}

impl Run {
    #[must_use]
    pub const fn exists(&self) -> bool {
        self.drafted
    }

    /// Whether it has posted. **An approved run cannot be redrafted**: the
    /// entry is in the books and the payslips are what people were told.
    #[must_use]
    pub const fn is_approved(&self) -> bool {
        self.entry.is_some()
    }
}

/// Adds a payslip's totals to a run's.
///
/// Its own function because the three totals must be summed the same way, and
/// three separate folds is three places to get one of them wrong.
pub(crate) fn total(
    payslips: &[Payslip],
    currency: erp_types::CurrencyCode,
) -> Result<(Money, Money, Money), MoneyError> {
    let gross = Money::checked_sum(payslips.iter().map(|p| p.gross), currency)?;
    let deductions = Money::checked_sum(payslips.iter().map(|p| p.deductions), currency)?;
    let net = Money::checked_sum(payslips.iter().map(|p| p.net), currency)?;
    Ok((gross, deductions, net))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_period_reads_as_a_month_and_ends_on_its_last_day() {
        let may = Period::parse("2026-05").expect("a month");
        assert_eq!(may.to_string(), "2026-05");
        assert_eq!(
            may.ends_on(),
            chrono::NaiveDate::from_ymd_opt(2026, 5, 31),
            "pay is earned by the end of the month it is for"
        );

        // December rolls the year, which is the arithmetic that is wrong in
        // every hand-written version of this.
        let december = Period::parse("2026-12").expect("a month");
        assert_eq!(
            december.ends_on(),
            chrono::NaiveDate::from_ymd_opt(2026, 12, 31)
        );

        // February in a leap year, which is the other one.
        let february = Period::parse("2028-02").expect("a month");
        assert_eq!(
            february.ends_on(),
            chrono::NaiveDate::from_ymd_opt(2028, 2, 29)
        );

        assert!(Period::parse("2026-13").is_err());
        assert!(Period::parse("2026").is_err());
        assert!(Period::parse("May").is_err());
    }
}
