//! The fiscal calendar: when a business's periods begin and end.
//!
//! # Why the tenant sets it
//!
//! A statement is a question about a period, and businesses do not agree on
//! what a period is. Most close monthly on calendar months; a retailer may run
//! a 4-4-5 calendar so every period has the same number of trading weeks and
//! quarters compare like for like; a year need not start in January. Decided
//! by the product owner on 2026-09-14: the calendar is the tenant's, chosen
//! from a **pattern** and a **start date**, and every period is generated from
//! those two rather than typed in one by one.
//!
//! # What a period is
//!
//! A half-open range of days, `from` inclusive and `until` exclusive — the
//! convention the VAT return and `Books::closed_before` already use, so
//! "closed through 31 January" is never a comparison somebody gets wrong by a
//! day. A fiscal year is named by the calendar year it **starts** in, and its
//! periods are `2026-P01` … `2026-P12` (or `-P04`, or `-P01` for a yearly
//! calendar).
//!
//! # Month patterns and week patterns
//!
//! Monthly, quarterly and yearly periods start on the start date's day of the
//! month, clamped where a month is short: a calendar starting on the 31st has
//! periods starting on 28 or 29 February. The 4-4-5 family counts weeks
//! instead: each quarter is three periods of four, four and five weeks (or
//! 4-5-4, or 5-4-4), so a year is 52 weeks — and every few years 53, because
//! 52 weeks is a day short of a year. Each fiscal year starts on the start
//! date's **weekday nearest its anniversary**, which is the rule that keeps a
//! week calendar from drifting, and the 53rd week, when it comes, goes into
//! the last period of the year.
//!
//! # What is not here yet
//!
//! Periods are computed, not stored, so nothing here says which are closed:
//! that is the books' watermark (`period.rs`), which the formal period close
//! will move period by period. Until periods are rows, a calendar change is
//! refused while any period is closed at all — stricter than the decided rule
//! (from the next open year), and the interim is deliberate.

use chrono::{Datelike, Months, NaiveDate, Weekday};
use erp_eventlog::ConfigError;
use serde::{Deserialize, Serialize};

/// How a year divides into periods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pattern {
    /// Twelve periods on the start date's day of the month.
    #[serde(rename = "monthly")]
    Monthly,
    /// Four periods of three months.
    #[serde(rename = "quarterly")]
    Quarterly,
    /// Four quarters of four, four and five weeks.
    #[serde(rename = "4-4-5")]
    FourFourFive,
    /// Four quarters of four, five and four weeks.
    #[serde(rename = "4-5-4")]
    FourFiveFour,
    /// Four quarters of five, four and four weeks.
    #[serde(rename = "5-4-4")]
    FiveFourFour,
    /// One period: the year.
    #[serde(rename = "yearly")]
    Yearly,
}

impl Pattern {
    /// Every pattern, for the document and for tests.
    pub const ALL: [Self; 6] = [
        Self::Monthly,
        Self::Quarterly,
        Self::FourFourFive,
        Self::FourFiveFour,
        Self::FiveFourFour,
        Self::Yearly,
    ];

    /// The stored form, for messages and the document.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Monthly => "monthly",
            Self::Quarterly => "quarterly",
            Self::FourFourFive => "4-4-5",
            Self::FourFiveFour => "4-5-4",
            Self::FiveFourFour => "5-4-4",
            Self::Yearly => "yearly",
        }
    }

    /// Months per period, for the month patterns.
    const fn months(self) -> Option<u32> {
        match self {
            Self::Monthly => Some(1),
            Self::Quarterly => Some(3),
            Self::Yearly => Some(12),
            Self::FourFourFive | Self::FourFiveFour | Self::FiveFourFour => None,
        }
    }

    /// Weeks per period across one quarter, for the week patterns.
    const fn quarter(self) -> Option<[i64; 3]> {
        match self {
            Self::FourFourFive => Some([4, 4, 5]),
            Self::FourFiveFour => Some([4, 5, 4]),
            Self::FiveFourFour => Some([5, 4, 4]),
            Self::Monthly | Self::Quarterly | Self::Yearly => None,
        }
    }
}

impl std::str::FromStr for Pattern {
    type Err = NotAPattern;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|pattern| pattern.as_str() == s)
            .ok_or_else(|| NotAPattern(s.to_owned()))
    }
}

/// A pattern this build does not know.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a period pattern; one of monthly, quarterly, 4-4-5, 4-5-4, 5-4-4, yearly")]
pub struct NotAPattern(pub String);

/// The calendar a business keeps its periods by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiscalCalendar {
    /// The first day of a fiscal year, once. Every other year starts on its
    /// anniversary — or, for a week pattern, on that weekday nearest to it.
    pub starts_on: NaiveDate,
    pub pattern: Pattern,
}

impl Default for FiscalCalendar {
    /// Calendar months from 1 January — what a business that never chose gets,
    /// and what most Saudi businesses would choose.
    fn default() -> Self {
        Self {
            starts_on: NaiveDate::from_ymd_opt(2000, 1, 1).unwrap_or_default(),
            pattern: Pattern::Monthly,
        }
    }
}

/// One period of one fiscal year: `[from, until)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Period {
    /// `2026-P03`: the fiscal year it belongs to, and its number in it.
    pub id: String,
    /// The calendar year the fiscal year starts in.
    pub year: i32,
    /// One-based.
    pub index: u32,
    pub from: NaiveDate,
    /// Exclusive: the first day of the next period.
    pub until: NaiveDate,
}

impl FiscalCalendar {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "ledger.fiscal_calendar";

    /// The first day of fiscal year `year`.
    #[must_use]
    pub fn year_start(&self, year: i32) -> NaiveDate {
        let anniversary = anniversary(self.starts_on, year);
        if self.pattern.quarter().is_some() {
            nearest(anniversary, self.starts_on.weekday())
        } else {
            anniversary
        }
    }

    /// The fiscal year `day` falls in.
    #[must_use]
    pub fn fiscal_year_of(&self, day: NaiveDate) -> i32 {
        let mut year = day.year();
        if self.year_start(year) > day {
            year -= 1;
        } else if self.year_start(year + 1) <= day {
            year += 1;
        }
        year
    }

    /// Every period of fiscal year `year`, in order.
    #[must_use]
    pub fn periods(&self, year: i32) -> Vec<Period> {
        let start = self.year_start(year);
        let next = self.year_start(year + 1);
        let bounds: Vec<NaiveDate> = if let Some(months) = self.pattern.months() {
            (0..12 / months)
                .map(|k| add_months(start, k * months))
                .chain(std::iter::once(next))
                .collect()
        } else {
            let quarter = self.pattern.quarter().unwrap_or([4, 4, 5]);
            let mut weeks: Vec<i64> = quarter.iter().copied().cycle().take(12).collect();
            // A 53-week year keeps its extra week at the end, where a fiscal
            // year absorbs it in practice.
            let total = (next - start).num_days() / 7;
            if let Some(last) = weeks.last_mut() {
                *last += total - 52;
            }
            let mut at = start;
            let mut bounds = vec![start];
            for length in weeks {
                at += chrono::Duration::weeks(length);
                bounds.push(at);
            }
            // The last bound is `next` by construction; say so rather than
            // trust the arithmetic.
            if let Some(last) = bounds.last_mut() {
                *last = next;
            }
            bounds
        };
        bounds
            .windows(2)
            .enumerate()
            .map(|(i, pair)| {
                let index = u32::try_from(i + 1).unwrap_or(u32::MAX);
                Period {
                    id: format!("{year}-P{index:02}"),
                    year,
                    index,
                    from: pair[0],
                    until: pair[1],
                }
            })
            .collect()
    }

    /// The period `day` falls in.
    #[must_use]
    pub fn period_containing(&self, day: NaiveDate) -> Period {
        let year = self.fiscal_year_of(day);
        self.periods(year)
            .into_iter()
            .find(|p| p.from <= day && day < p.until)
            // Every day of a fiscal year is in exactly one of its periods; the
            // last period runs to the next year's start.
            .unwrap_or_else(|| Period {
                id: format!("{year}-P01"),
                year,
                index: 1,
                from: self.year_start(year),
                until: self.year_start(year + 1),
            })
    }

    /// The period an id names — `2026-P03` — or `None` for one this calendar
    /// does not have.
    #[must_use]
    pub fn period(&self, id: &str) -> Option<Period> {
        let (year, index) = id.split_once("-P")?;
        let year: i32 = year.parse().ok()?;
        let index: u32 = index.parse().ok()?;
        self.periods(year).into_iter().find(|p| p.index == index)
    }
}

/// `starts_on`'s month and day in `year`, clamped to the month's length —
/// a calendar anchored on 29 February starts on the 28th in other years.
fn anniversary(starts_on: NaiveDate, year: i32) -> NaiveDate {
    let day = starts_on.day();
    (0..=3)
        .find_map(|back| NaiveDate::from_ymd_opt(year, starts_on.month(), day - back))
        .unwrap_or(starts_on)
}

/// `date` plus `n` months, the day clamped where the month is short.
fn add_months(date: NaiveDate, n: u32) -> NaiveDate {
    date.checked_add_months(Months::new(n)).unwrap_or(date)
}

/// The date with `weekday` nearest to `date`: within three days either side,
/// so there is exactly one.
fn nearest(date: NaiveDate, weekday: Weekday) -> NaiveDate {
    (-3..=3)
        .map(|offset| date + chrono::Duration::days(offset))
        .find(|candidate| candidate.weekday() == weekday)
        .unwrap_or(date)
}

/// Why a calendar was not stored.
#[derive(Debug, thiserror::Error)]
pub enum CalendarError {
    /// The books are closed somewhere, and periods already declared final must
    /// not move under the declaration. Interim rule; see the module docs.
    #[error("the books are closed, so the calendar cannot change")]
    Locked,
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// The calendar a tenant keeps, or the default for one that never chose.
///
/// # Errors
/// If the stored value cannot be read.
pub async fn fiscal_calendar(conn: &mut sqlx::PgConnection) -> Result<FiscalCalendar, ConfigError> {
    Ok(
        erp_eventlog::configuration::get::<FiscalCalendar>(conn, FiscalCalendar::KEY)
            .await?
            .map(|configured| configured.value)
            .unwrap_or_default(),
    )
}

/// Stores the calendar, or refuses because a period is already closed.
///
/// # Errors
/// [`CalendarError::Locked`] while the books' watermark is set, or the store.
pub async fn set_fiscal_calendar(
    conn: &mut sqlx::PgConnection,
    calendar: FiscalCalendar,
    by: Option<&str>,
) -> Result<FiscalCalendar, CalendarError> {
    if crate::period::books(&mut *conn)
        .await?
        .closed_before
        .is_some()
    {
        return Err(CalendarError::Locked);
    }
    erp_eventlog::configuration::set(conn, FiscalCalendar::KEY, &calendar, by, None).await?;
    Ok(calendar)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(text: &str) -> NaiveDate {
        text.parse()
            .unwrap_or_else(|_| unreachable!("a date literal"))
    }

    fn fiscal(starts_on: &str, pattern: Pattern) -> FiscalCalendar {
        FiscalCalendar {
            starts_on: day(starts_on),
            pattern,
        }
    }

    #[test]
    fn calendar_months_from_january() {
        let periods = fiscal("2000-01-01", Pattern::Monthly).periods(2026);
        assert_eq!(periods.len(), 12);
        assert_eq!(periods[0].id, "2026-P01");
        assert_eq!(
            (periods[0].from, periods[0].until),
            (day("2026-01-01"), day("2026-02-01"))
        );
        assert_eq!(
            (periods[11].from, periods[11].until),
            (day("2026-12-01"), day("2027-01-01"))
        );
    }

    /// A calendar anchored on the 31st starts February on its last day and
    /// March on the 31st again — clamped per period, never chained.
    #[test]
    fn a_month_calendar_clamps_short_months_without_drifting() {
        let periods = fiscal("2025-01-31", Pattern::Monthly).periods(2026);
        assert_eq!(periods[0].from, day("2026-01-31"));
        assert_eq!(periods[1].from, day("2026-02-28"));
        assert_eq!(periods[2].from, day("2026-03-31"));
        assert_eq!(periods[3].from, day("2026-04-30"));
        assert_eq!(periods[11].until, day("2027-01-31"));
    }

    #[test]
    fn quarters_and_years() {
        let quarters = fiscal("2000-04-01", Pattern::Quarterly).periods(2026);
        assert_eq!(quarters.len(), 4);
        assert_eq!(
            (quarters[0].from, quarters[0].until),
            (day("2026-04-01"), day("2026-07-01"))
        );
        assert_eq!(quarters[3].until, day("2027-04-01"));

        let year = fiscal("2000-04-01", Pattern::Yearly).periods(2026);
        assert_eq!(year.len(), 1);
        assert_eq!(
            (year[0].from, year[0].until),
            (day("2026-04-01"), day("2027-04-01"))
        );
        assert_eq!(year[0].id, "2026-P01");
    }

    /// **A 4-4-5 year is 52 weeks, and every few years 53.** Anchored on a
    /// Sunday, each year starts on the Sunday nearest 4 January; 2028 starts
    /// on the 2nd and 2029 on the 7th, so 2028 has 371 days and its last
    /// period six weeks.
    #[test]
    fn a_four_four_five_year_is_weeks_and_absorbs_the_fifty_third_at_the_end() {
        let calendar = fiscal("2026-01-04", Pattern::FourFourFive);
        assert_eq!(day("2026-01-04").weekday(), Weekday::Sun);
        assert_eq!(calendar.year_start(2026), day("2026-01-04"));
        assert_eq!(calendar.year_start(2027), day("2027-01-03"));
        assert_eq!(calendar.year_start(2028), day("2028-01-02"));
        assert_eq!(calendar.year_start(2029), day("2029-01-07"));

        let ordinary = calendar.periods(2026);
        assert_eq!(ordinary.len(), 12);
        let weeks: Vec<i64> = ordinary
            .iter()
            .map(|p| (p.until - p.from).num_days() / 7)
            .collect();
        assert_eq!(weeks, [4, 4, 5, 4, 4, 5, 4, 4, 5, 4, 4, 5]);
        assert_eq!(ordinary[11].until, day("2027-01-03"));

        let long = calendar.periods(2028);
        let weeks: Vec<i64> = long
            .iter()
            .map(|p| (p.until - p.from).num_days() / 7)
            .collect();
        assert_eq!(weeks, [4, 4, 5, 4, 4, 5, 4, 4, 5, 4, 4, 6]);
        assert_eq!(long[11].until, day("2029-01-07"));
        assert_eq!(
            long.iter()
                .map(|p| (p.until - p.from).num_days())
                .sum::<i64>(),
            371
        );

        let other = fiscal("2026-01-04", Pattern::FiveFourFour).periods(2026);
        let weeks: Vec<i64> = other
            .iter()
            .map(|p| (p.until - p.from).num_days() / 7)
            .collect();
        assert_eq!(weeks, [5, 4, 4, 5, 4, 4, 5, 4, 4, 5, 4, 4]);
    }

    #[test]
    fn a_day_is_in_exactly_one_period_and_the_id_finds_it_again() {
        let calendar = fiscal("2000-04-01", Pattern::Monthly);
        assert_eq!(calendar.fiscal_year_of(day("2026-03-31")), 2025);
        assert_eq!(calendar.fiscal_year_of(day("2026-04-01")), 2026);
        let march = calendar.period_containing(day("2026-03-31"));
        assert_eq!(march.id, "2025-P12");
        assert_eq!(
            (march.from, march.until),
            (day("2026-03-01"), day("2026-04-01"))
        );
        assert_eq!(calendar.period("2025-P12"), Some(march));
        assert_eq!(calendar.period("2025-P13"), None);
        assert_eq!(calendar.period("nonsense"), None);
    }

    #[test]
    fn a_leap_day_anchor_clamps_in_other_years() {
        let calendar = fiscal("2024-02-29", Pattern::Yearly);
        assert_eq!(calendar.year_start(2025), day("2025-02-28"));
        assert_eq!(calendar.year_start(2028), day("2028-02-29"));
    }

    #[test]
    fn patterns_round_trip_through_their_stored_form() {
        for pattern in Pattern::ALL {
            assert_eq!(pattern.as_str().parse::<Pattern>(), Ok(pattern));
            let json = serde_json::to_string(&pattern).unwrap_or_default();
            assert_eq!(json, format!("\"{}\"", pattern.as_str()));
        }
        assert!("weekly".parse::<Pattern>().is_err());
    }
}
