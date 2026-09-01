//! When a resource is offered.
//!
//! Opening hours, a stylist's shifts, a room's out-of-service week and a
//! studio's Ramadan timetable are one shape: **which days, and between which
//! two times on those days.**
//!
//! # Where this diverges from the plan, and why
//!
//! `docs/IMPLEMENTATION.md` specified the recurrence as *"months, weekdays,
//! days, hours, minutes as bit fields"*, taken from the system this phase was
//! read against. That is cron, and cron cannot say "half past nine".
//!
//! Cron matches an instant when every one of its fields matches, so the hours
//! and minutes are independent sets. "Open 09:30 to 17:00" would need hours
//! `{9..16}` and minutes `{30..59} ∪ {0..29}` — which is every minute, and
//! therefore also matches 09:05. There is no assignment of those two bit fields
//! that means what a salon means. The fields are not wrong for *days*; they are
//! wrong for *times*, because a time window is an interval and cron has no
//! intervals.
//!
//! So the calendar half stays as bit fields, which is what made theirs compact
//! and indexable, and the clock half becomes what it actually is: two
//! minutes-past-midnight bounds, half-open like everything else here.
//!
//! # One window per rule, and midnight is a boundary
//!
//! A rule that runs past midnight — a bar open 22:00 to 02:00 — is refused, and
//! is written as the two rules it is: 22:00 to 24:00, and 00:00 to 02:00 on the
//! following weekday. Allowing the wrap would make every day test in this file
//! ask "or is it yesterday's rule still running", which is a question that
//! would then have to be right in six places.
//!
//! # Local time, and the ceiling on that
//!
//! A [`Span`] is UTC because instants are. Opening hours are local, so a rule
//! is evaluated at the tenant's offset — see [`crate::Calendar`].
//!
//! ponytail: a fixed offset, not a timezone database. Saudi Arabia is `+03:00`
//! and has no daylight saving, so a fixed offset is exact for the first market
//! and for the Gulf. A market with daylight saving needs `chrono-tz` and a
//! named zone, and the day that arrives this becomes `Tz` and nothing else
//! about the shape changes.

use chrono::{Datelike, FixedOffset, NaiveDate, Timelike};
use erp_occupancy::Span;
use serde::{Deserialize, Serialize};

/// Minutes in a day. `closes_at` may equal it, meaning midnight.
const DAY: u16 = 24 * 60;

/// Why a rule is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BadRule {
    #[error("a window must close after it opens, and may not run past midnight")]
    NotAWindow,
    #[error("a time of day is minutes past midnight, from 0 to {DAY}")]
    NotATimeOfDay,
    #[error("a rule that has an end must not end before it starts")]
    BackwardsDates,
    #[error("day {0} is not a day of any month")]
    NotADayOfTheMonth(u32),
    #[error("month {0} is not a month")]
    NotAMonth(u32),
    #[error("weekday {0} is not a weekday; Monday is 1 and Sunday is 7")]
    NotAWeekday(u32),
}

impl erp_i18n::Localize for BadRule {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NotAWindow => Message::new(messages::NOT_A_WINDOW),
            Self::NotATimeOfDay => Message::new(messages::NOT_A_TIME_OF_DAY)
                .with("most", MessageArg::Int(i64::from(DAY))),
            Self::BackwardsDates => Message::new(messages::BACKWARDS_DATES),
            Self::NotADayOfTheMonth(n) => Message::new(messages::NOT_A_DAY_OF_THE_MONTH)
                .with("value", MessageArg::Int(i64::from(*n))),
            Self::NotAMonth(n) => {
                Message::new(messages::NOT_A_MONTH).with("value", MessageArg::Int(i64::from(*n)))
            }
            Self::NotAWeekday(n) => {
                Message::new(messages::NOT_A_WEEKDAY).with("value", MessageArg::Int(i64::from(*n)))
            }
        }
    }
}

/// One window in a repeating calendar.
///
/// Built by [`Availability::new`], which is the only thing that can produce
/// one. `#[serde(try_from)]` so a rule replayed out of the log is checked
/// again, per architecture §4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Fields", into = "Fields")]
pub struct Availability {
    /// Bit 0 is January, bit 11 is December. **Zero means every month**, which
    /// is what makes the common rule short.
    months: u16,
    /// Bit 0 is Monday, bit 6 is Sunday. Zero means every weekday.
    weekdays: u8,
    /// Bit 0 is the 1st, bit 30 is the 31st. Zero means every day.
    ///
    /// Independent of `weekdays` and both must match, so "the first Monday of
    /// the month" is `days = 1..=7` and `weekdays = Monday` — which is one of
    /// the few things cron's shape gets exactly right.
    days: u32,
    /// Minutes past midnight, local. Inclusive.
    opens_at: u16,
    /// Minutes past midnight, local. **Exclusive**, so back-to-back rules meet
    /// without a gap, the same as every interval in this system.
    closes_at: u16,
    /// The first day the rule applies, local. `None` is "always has".
    from: Option<NaiveDate>,
    /// The last day it applies, local, **inclusive** — because a person writing
    /// "closed until the 5th" means the 5th, and an exclusive end here would be
    /// the one place in this codebase where a date meant the day before.
    until: Option<NaiveDate>,
}

/// The wire shape. Public field names, private type: the only way in is
/// [`Availability::new`] or a deserialize that calls it.
#[derive(Serialize, Deserialize)]
struct Fields {
    months: u16,
    weekdays: u8,
    days: u32,
    opens_at: u16,
    closes_at: u16,
    from: Option<NaiveDate>,
    until: Option<NaiveDate>,
}

impl TryFrom<Fields> for Availability {
    type Error = BadRule;

    fn try_from(f: Fields) -> Result<Self, Self::Error> {
        Self::new(
            f.months,
            f.weekdays,
            f.days,
            f.opens_at,
            f.closes_at,
            f.from,
            f.until,
        )
    }
}

impl From<Availability> for Fields {
    fn from(a: Availability) -> Self {
        Self {
            months: a.months,
            weekdays: a.weekdays,
            days: a.days,
            opens_at: a.opens_at,
            closes_at: a.closes_at,
            from: a.from,
            until: a.until,
        }
    }
}

impl Availability {
    /// A rule, checked.
    ///
    /// The three calendar fields are bit sets where zero means "every", so the
    /// most common rule — open every day between two times — names two numbers
    /// and leaves the rest at nothing.
    pub fn new(
        months: u16,
        weekdays: u8,
        days: u32,
        opens_at: u16,
        closes_at: u16,
        from: Option<NaiveDate>,
        until: Option<NaiveDate>,
    ) -> Result<Self, BadRule> {
        if opens_at > DAY || closes_at > DAY {
            return Err(BadRule::NotATimeOfDay);
        }
        if closes_at <= opens_at {
            return Err(BadRule::NotAWindow);
        }
        // Bit 12 and up is a thirteenth month, bit 7 an eighth weekday, bit 31
        // a thirty-second day. Each is a caller who shifted by the wrong base,
        // and each would silently narrow the rule to nothing.
        if months >> 12 != 0 {
            return Err(BadRule::NotAMonth(u32::from(months >> 12)));
        }
        if weekdays >> 7 != 0 {
            return Err(BadRule::NotAWeekday(u32::from(weekdays >> 7)));
        }
        if days >> 31 != 0 {
            return Err(BadRule::NotADayOfTheMonth(days >> 31));
        }
        if let (Some(from), Some(until)) = (from, until)
            && until < from
        {
            return Err(BadRule::BackwardsDates);
        }
        Ok(Self {
            months,
            weekdays,
            days,
            opens_at,
            closes_at,
            from,
            until,
        })
    }

    /// A rule from the lists a person actually writes.
    ///
    /// Months are 1 to 12, weekdays 1 to 7 with Monday as 1 — ISO 8601, which
    /// is what everything else that talks about weekdays here uses — and days
    /// of the month 1 to 31. **An empty list means every one of them**, so the
    /// ordinary rule names no lists at all.
    ///
    /// The bit fields are what get stored, because they are compact and they
    /// index; the lists are what a client sends, because nobody should be
    /// shifting by twelve to say "December".
    pub fn from_parts(
        months: &[u8],
        weekdays: &[u8],
        days: &[u8],
        opens_at: u16,
        closes_at: u16,
        from: Option<NaiveDate>,
        until: Option<NaiveDate>,
    ) -> Result<Self, BadRule> {
        let mut month_bits = 0_u16;
        for month in months {
            if !(1..=12).contains(month) {
                return Err(BadRule::NotAMonth(u32::from(*month)));
            }
            month_bits |= 1 << (month - 1);
        }
        let mut weekday_bits = 0_u8;
        for weekday in weekdays {
            if !(1..=7).contains(weekday) {
                return Err(BadRule::NotAWeekday(u32::from(*weekday)));
            }
            weekday_bits |= 1 << (weekday - 1);
        }
        let mut day_bits = 0_u32;
        for day in days {
            if !(1..=31).contains(day) {
                return Err(BadRule::NotADayOfTheMonth(u32::from(*day)));
            }
            day_bits |= 1 << (day - 1);
        }
        Self::new(
            month_bits,
            weekday_bits,
            day_bits,
            opens_at,
            closes_at,
            from,
            until,
        )
    }

    /// The months this applies in, or empty for every month.
    #[must_use]
    pub fn months(&self) -> Vec<u8> {
        (1..=12_u8)
            .filter(|m| self.months & (1 << (m - 1)) != 0)
            .collect()
    }

    /// The weekdays, Monday as 1, or empty for every one.
    #[must_use]
    pub fn weekdays(&self) -> Vec<u8> {
        (1..=7_u8)
            .filter(|d| self.weekdays & (1 << (d - 1)) != 0)
            .collect()
    }

    /// The days of the month, or empty for every one.
    #[must_use]
    pub fn days(&self) -> Vec<u8> {
        (1..=31_u8)
            .filter(|d| self.days & (1 << (d - 1)) != 0)
            .collect()
    }

    /// Minutes past local midnight, inclusive.
    #[must_use]
    pub const fn opens_at(&self) -> u16 {
        self.opens_at
    }

    /// Minutes past local midnight, exclusive.
    #[must_use]
    pub const fn closes_at(&self) -> u16 {
        self.closes_at
    }

    /// The first day this applies, if it has one.
    #[must_use]
    pub const fn starting(&self) -> Option<NaiveDate> {
        self.from
    }

    /// The last day it applies, inclusive, if it has one.
    #[must_use]
    pub const fn ending(&self) -> Option<NaiveDate> {
        self.until
    }

    /// Open every day, all day, for ever.
    ///
    /// What a hotel room and a museum have, and what a resource with no rules
    /// at all is treated as. Not the `Default`: a rule this permissive should
    /// be asked for by name.
    pub fn always() -> Result<Self, BadRule> {
        Self::new(0, 0, 0, 0, DAY, None, None)
    }

    /// A window between two clock times, every day.
    ///
    /// The rule almost every business actually has.
    pub fn daily(opens_at: u16, closes_at: u16) -> Result<Self, BadRule> {
        Self::new(0, 0, 0, opens_at, closes_at, None, None)
    }

    /// Whether the whole of a span falls inside this rule.
    ///
    /// **The whole of it.** A ninety-minute treatment starting half an hour
    /// before closing is not half-available, and answering "yes" for the part
    /// that fits is how a business ends up with a customer in the chair after
    /// the doors are locked.
    ///
    /// ponytail: walks the local days the span touches, at most a few and
    /// bounded by `Span`'s own year-long ceiling. The set-based version — turn
    /// the bit fields into a date series in SQL — is what to write when this
    /// has to answer "and when is it free?" for a month of calendar at once,
    /// which is a different question than this one.
    #[must_use]
    pub fn covers(&self, span: Span, at: FixedOffset) -> bool {
        let from = span.from().with_timezone(&at);
        let until = span.until().with_timezone(&at);

        // Minutes past local midnight, which is what the window is in. The
        // second is dropped deliberately: a rule is written to the minute, and
        // a span is normalised to the second, so 16:59:30 is inside a window
        // closing at 17:00 and would be outside one closing at 16:59.
        let mut day = from.date_naive();
        let last = until.date_naive();
        loop {
            // The part of this local day the span occupies, in minutes.
            let starts = if day == from.date_naive() {
                minute_of_day(from.hour(), from.minute())
            } else {
                0
            };
            // A span ending exactly at local midnight does not occupy the day
            // it ends on, which is why that day is skipped rather than tested
            // against a window it could never satisfy.
            let ends = if day == last {
                let end = minute_of_day(until.hour(), until.minute());
                if end == 0 {
                    break;
                }
                // Round up: a span ending at 17:00:30 needs the window open at
                // 17:00, so it needs a window that closes at 17:01 or later.
                if until.second() > 0 { end + 1 } else { end }
            } else {
                DAY
            };

            if !self.open_on(day) || starts < self.opens_at || ends > self.closes_at {
                return false;
            }
            if day == last {
                break;
            }
            day = match day.succ_opt() {
                Some(next) => next,
                // The end of the representable calendar. Nothing is available
                // there, which is a better answer than wrapping.
                None => return false,
            };
        }
        true
    }

    /// Whether the calendar half of the rule matches a local date.
    fn open_on(&self, day: NaiveDate) -> bool {
        if let Some(from) = self.from
            && day < from
        {
            return false;
        }
        if let Some(until) = self.until
            && day > until
        {
            return false;
        }
        // Zero is "every" in all three, which is what keeps the ordinary rule
        // to two numbers. The shift is typed by the literal and the count is
        // chrono's `u32`, so nothing here is cast and nothing can truncate.
        let month = self.months == 0 || self.months & (1_u16 << day.month0()) != 0;
        let weekday = self.weekdays == 0
            || self.weekdays & (1_u8 << day.weekday().num_days_from_monday()) != 0;
        let dom = self.days == 0 || self.days & (1_u32 << day.day0()) != 0;
        month && weekday && dom
    }
}

/// Whether any rule in a set covers the span.
///
/// **An empty set is open.** A resource nobody has given a timetable to is one
/// that takes bookings whenever, which is what a hotel room and a museum slot
/// are, and it means declaring a resource is one step rather than two.
#[must_use]
pub fn any_covers(rules: &[Availability], span: Span, at: FixedOffset) -> bool {
    rules.is_empty() || rules.iter().any(|rule| rule.covers(span, at))
}

const fn minute_of_day(hour: u32, minute: u32) -> u16 {
    // Both come from chrono and are in range; the arithmetic cannot overflow a
    // u16 because 23 * 60 + 59 is 1439.
    #[expect(clippy::cast_possible_truncation, reason = "hour < 24 and minute < 60")]
    {
        (hour * 60 + minute) as u16
    }
}
