//! **The tenant's clock** — the one place an instant becomes a day.
//!
//! # Why this is a kernel type
//!
//! Opening hours are local and instants are not, so something has to say what
//! "nine in the morning" means here; and a quarter, a month, a payroll period
//! and an invoice date are all *days*, which an instant only becomes once you
//! know whose clock you are reading it by. Riyadh is `+03:00`, so a quarter
//! that starts at local midnight starts at `21:00Z` the evening before — and
//! every module that wrote `at.date_naive()` was quietly filing the last three
//! hours of March into April.
//!
//! So every conversion goes through this type, and a workspace test refuses the
//! alternatives (`erp-eventlog/tests/write_side.rs`). The kernel stamps the
//! calendar onto every event's metadata at append time, so a projection reads
//! the clock the event was written under rather than today's setting, and a
//! rebuild reproduces exactly what was live (L2) even after the setting changes.
//!
//! # A zone, not an offset
//!
//! A tenant names an IANA zone — `Asia/Riyadh`, `Europe/Berlin` — and the
//! zone's own rules say what the offset is on any given instant, daylight
//! saving included. A fixed offset would be exact for the Gulf and wrong twice
//! a year everywhere north of it; a business in Berlin whose 09:00 opening
//! moved to 08:00 every March is not a business this should sell to. The first
//! version stored minutes; that shape is still read (see `Deserialize`), so
//! events written under it keep the clock they were written under.

use std::str::FromStr;

use chrono::{DateTime, NaiveDate, TimeZone};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Timestamp;

/// Not a name in the IANA zone database.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a timezone; use a name from the IANA database, such as Asia/Riyadh")]
pub struct NotAZone(pub String);

/// An IANA timezone. Stored and sent as its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Calendar {
    zone: Tz,
}

impl Calendar {
    /// Where a tenant's choice is stored. `tenant.`, not any one module's: the
    /// diary, the rota, the tax return and the payroll all read the same clock.
    pub const KEY: &'static str = "tenant.calendar";

    /// The default, and the first market.
    pub const RIYADH: Self = Self {
        zone: Tz::Asia__Riyadh,
    };

    /// No offset, ever. What a test that wants instants to be days reads by.
    pub const UTC: Self = Self { zone: Tz::UTC };

    /// A zone by its IANA name.
    pub fn named(name: &str) -> Result<Self, NotAZone> {
        Tz::from_str(name.trim())
            .map(|zone| Self { zone })
            .map_err(|_| NotAZone(name.trim().to_owned()))
    }

    /// The IANA name.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.zone.name()
    }

    /// The instant, on this clock.
    #[must_use]
    pub fn local(self, at: Timestamp) -> DateTime<Tz> {
        at.with_timezone(&self.zone)
    }

    /// The day an instant falls on, here.
    #[must_use]
    pub fn day(self, at: Timestamp) -> NaiveDate {
        self.local(at).date_naive()
    }

    /// `YYYY-MM`, the month an instant falls in, here. A string because it is a
    /// report's key and sorts correctly as text.
    #[must_use]
    pub fn month(self, at: Timestamp) -> String {
        self.local(at).format("%Y-%m").to_string()
    }

    /// The first instant of `day`, here. What a period boundary given as a date
    /// becomes before it is compared with anything.
    ///
    /// **Local midnight, or the first instant after it that exists.** A zone
    /// that springs forward *at* midnight (Chile, among others) has no 00:00 on
    /// that day; the day then starts at 01:00, which is what the clocks in the
    /// shop say too. A midnight that exists twice takes the earlier one.
    #[must_use]
    pub fn start_of(self, day: NaiveDate) -> Timestamp {
        let midnight = day.and_hms_opt(0, 0, 0).unwrap_or_default();
        let local = self
            .zone
            .from_local_datetime(&midnight)
            .earliest()
            .or_else(|| {
                // Into the gap: the first minute that maps to a real instant.
                (1..=180).find_map(|minute| {
                    self.zone
                        .from_local_datetime(&(midnight + chrono::Duration::minutes(minute)))
                        .earliest()
                })
            });
        local.map_or_else(
            || midnight.and_utc(),
            |local| local.with_timezone(&chrono::Utc),
        )
    }

    /// `YYYY-MM-DD HH:MM` on this clock — how an instant reads in a message to
    /// a person. No seconds and no zone: the reader knows where they are.
    #[must_use]
    pub fn clock(self, at: Timestamp) -> String {
        self.local(at).format("%Y-%m-%d %H:%M").to_string()
    }
}

impl Default for Calendar {
    fn default() -> Self {
        Self::RIYADH
    }
}

impl Serialize for Calendar {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.zone.name())
    }
}

/// What has ever been stored under `tenant.calendar` or on an event.
#[derive(Deserialize)]
#[serde(untagged)]
enum Stored {
    /// An IANA name.
    Zone(String),
    /// **The first format**: minutes east of UTC. Read as the fixed-offset zone
    /// it named, so an event stamped `180` is still on `+03:00` for ever,
    /// whatever the tenant sets later.
    Minutes(i32),
}

impl<'de> Deserialize<'de> for Calendar {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Stored::deserialize(deserializer)? {
            Stored::Zone(name) => Self::named(&name).map_err(serde::de::Error::custom),
            Stored::Minutes(minutes) => fixed(minutes).map_err(serde::de::Error::custom),
        }
    }
}

/// The IANA zone for a whole-hour fixed offset: `Etc/GMT-3` is `+03:00` — the
/// sign is inverted, by a convention older than this codebase. Anything not on
/// the hour has no such zone and is refused rather than rounded.
fn fixed(minutes: i32) -> Result<Calendar, NotAZone> {
    if minutes == 0 {
        return Ok(Calendar::UTC);
    }
    if minutes % 60 != 0 {
        return Err(NotAZone(format!("{minutes} minutes")));
    }
    let hours = minutes / 60;
    let name = if hours > 0 {
        format!("Etc/GMT-{hours}")
    } else {
        format!("Etc/GMT+{}", -hours)
    };
    Calendar::named(&name).map_err(|_| NotAZone(format!("{minutes} minutes")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> Timestamp {
        rfc3339.parse().expect("a valid instant")
    }

    fn date(ymd: &str) -> NaiveDate {
        ymd.parse().expect("a valid date")
    }

    /// **The three hours the review found.** 21:00Z on the last day of March is
    /// already April in Riyadh, and a return that filed it under March was
    /// wrong.
    #[test]
    fn the_day_is_the_tenants_day_not_utcs() {
        let riyadh = Calendar::RIYADH;
        assert_eq!(riyadh.day(at("2026-03-31T21:00:00Z")), date("2026-04-01"));
        assert_eq!(riyadh.month(at("2026-03-31T21:00:00Z")), "2026-04");
        assert_eq!(riyadh.month(at("2026-03-31T20:59:59Z")), "2026-03");
        assert_eq!(riyadh.clock(at("2026-03-31T21:00:00Z")), "2026-04-01 00:00");
    }

    /// A date is a period boundary only once it is an instant, and the instant
    /// is local midnight — so a round trip through the same clock is identity.
    #[test]
    fn a_day_starts_at_local_midnight() {
        let riyadh = Calendar::RIYADH;
        let april = date("2026-04-01");
        assert_eq!(riyadh.start_of(april), at("2026-03-31T21:00:00Z"));
        assert_eq!(riyadh.day(riyadh.start_of(april)), april);
        assert_eq!(Calendar::UTC.start_of(april), at("2026-04-01T00:00:00Z"));
    }

    /// **Daylight saving is the zone's business, not a setting.** Berlin is
    /// `+01:00` in March and `+02:00` after the last Sunday of it; the same
    /// local midnight is a different instant on either side of the change, and
    /// 22:30Z is tomorrow there in July but today in January.
    #[test]
    fn a_zone_with_daylight_saving_moves_with_it() {
        let berlin = Calendar::named("Europe/Berlin").expect("a zone");
        assert_eq!(berlin.name(), "Europe/Berlin");
        // Sunday 29 March 2026, 02:00 CET becomes 03:00 CEST.
        assert_eq!(
            berlin.start_of(date("2026-03-29")),
            at("2026-03-28T23:00:00Z")
        );
        assert_eq!(
            berlin.start_of(date("2026-03-30")),
            at("2026-03-29T22:00:00Z")
        );
        assert_eq!(berlin.day(at("2026-07-01T22:30:00Z")), date("2026-07-02"));
        assert_eq!(berlin.day(at("2026-01-01T22:30:00Z")), date("2026-01-01"));
        assert_eq!(berlin.clock(at("2026-07-01T22:30:00Z")), "2026-07-02 00:30");
        // Sunday 25 October 2026, 03:00 CEST becomes 02:00 CET: the day is 25
        // hours long and still starts once.
        let long_day = date("2026-10-25");
        assert_eq!(berlin.start_of(long_day), at("2026-10-24T22:00:00Z"));
        assert_eq!(
            berlin.start_of(date("2026-10-26")) - berlin.start_of(long_day),
            chrono::Duration::hours(25)
        );
    }

    /// **A day whose midnight does not exist still starts.** Chile springs
    /// forward at midnight, so the first Sunday of September has no 00:00; the
    /// day begins at 01:00, which is what the shop's clock says, and the
    /// instant before it belongs to Saturday.
    #[test]
    fn a_day_with_no_midnight_starts_at_the_first_instant_it_has() {
        let santiago = Calendar::named("America/Santiago").expect("a zone");
        let sunday = date("2026-09-06");
        let starts = santiago.start_of(sunday);
        assert_eq!(santiago.day(starts), sunday);
        assert_eq!(
            santiago.day(starts - chrono::Duration::seconds(1)),
            date("2026-09-05")
        );
        assert_eq!(santiago.local(starts).format("%H:%M").to_string(), "01:00");
    }

    /// The name is what is stored and sent; the first format's minutes are
    /// still read, as the fixed-offset zone they named.
    #[test]
    fn a_zone_round_trips_by_name_and_the_old_minutes_are_still_read() {
        assert_eq!(
            serde_json::to_string(&Calendar::RIYADH).expect("serializes"),
            "\"Asia/Riyadh\""
        );
        assert_eq!(
            serde_json::from_str::<Calendar>("\"Europe/Berlin\"").expect("parses"),
            Calendar::named("Europe/Berlin").expect("a zone")
        );
        let legacy = serde_json::from_str::<Calendar>("180").expect("the first format");
        assert_eq!(legacy.name(), "Etc/GMT-3");
        assert_eq!(
            legacy.start_of(date("2026-04-01")),
            Calendar::RIYADH.start_of(date("2026-04-01")),
            "+03:00 either way"
        );
        assert_eq!(
            serde_json::from_str::<Calendar>("0").expect("utc"),
            Calendar::UTC
        );
        assert_eq!(
            serde_json::from_str::<Calendar>("-300")
                .expect("west")
                .name(),
            "Etc/GMT+5"
        );
        assert!(
            serde_json::from_str::<Calendar>("90").is_err(),
            "not on the hour"
        );
        assert!(serde_json::from_str::<Calendar>("\"Mars/Olympus\"").is_err());
        assert_eq!(
            Calendar::named("Mars/Olympus"),
            Err(NotAZone("Mars/Olympus".to_owned()))
        );
    }
}
