//! The tenant's clock.
//!
//! Opening hours are local and instants are not, so something has to say what
//! "nine in the morning" means here. This is that, and it is deliberately the
//! only thing in the module that knows.
//!
//! ponytail: a fixed offset, not a named zone. Saudi Arabia is `+03:00` all
//! year, and so is every Gulf market next to it, so a fixed offset is exact for
//! where this ships and needs no timezone database in the binary. A market with
//! daylight saving needs `chrono-tz` and a zone name; that is a change to this
//! file and to nothing else, which is why the offset is resolved here rather
//! than passed around.

use chrono::{FixedOffset, Offset};
use serde::{Deserialize, Serialize};

/// The furthest a real timezone is from UTC, in minutes. `+14:00` is Kiritimati
/// and `-12:00` is Baker Island; nothing is outside that.
const LIMIT: i32 = 14 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a timezone offset is minutes from UTC, between -{LIMIT} and {LIMIT}")]
pub struct NotAnOffset;

impl erp_i18n::Localize for NotAnOffset {
    fn message(&self) -> erp_i18n::Message {
        erp_i18n::Message::new(crate::messages::NOT_AN_OFFSET)
            .with("limit", erp_i18n::MessageArg::Int(i64::from(LIMIT)))
    }
}

/// Where the tenant is, as minutes east of UTC.
///
/// `#[serde(try_from)]`, so a stored offset is checked on the way back in and
/// [`Self::offset`] cannot fail — architecture §4's proof-carrying constructor,
/// applied to the one number that would otherwise need a fallible conversion at
/// every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "i32", into = "i32")]
pub struct Calendar {
    minutes: i32,
}

impl TryFrom<i32> for Calendar {
    type Error = NotAnOffset;

    fn try_from(minutes: i32) -> Result<Self, Self::Error> {
        if minutes.abs() > LIMIT {
            return Err(NotAnOffset);
        }
        Ok(Self { minutes })
    }
}

impl From<Calendar> for i32 {
    fn from(calendar: Calendar) -> Self {
        calendar.minutes
    }
}

impl Calendar {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "booking.calendar";

    /// `+03:00`. Saudi Arabia, and every market next to it.
    pub const RIYADH: Self = Self { minutes: 3 * 60 };

    /// What this tenant has configured, or what ships.
    ///
    /// A tenant who never opens the settings gets Riyadh, which is the whole of
    /// "simplify for the people who do not want the dynamism". One who *has*
    /// configured it and stored something unusable gets an error rather than a
    /// silent fallback, because a salon whose opening hours are three hours out
    /// would find out from a customer.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or(Self::RIYADH, |configured| configured.value))
    }

    #[must_use]
    pub const fn minutes(self) -> i32 {
        self.minutes
    }

    /// The offset, as chrono wants it.
    #[must_use]
    pub fn offset(self) -> FixedOffset {
        // Checked in `TryFrom`, so the `None` arm is unreachable: 14 hours is
        // well inside what `east_opt` accepts. UTC rather than a panic, because
        // an unreachable branch in a query path should not be the one thing
        // that can bring a request down.
        FixedOffset::east_opt(self.minutes * 60).unwrap_or_else(|| chrono::Utc.fix())
    }
}

impl Default for Calendar {
    fn default() -> Self {
        Self::RIYADH
    }
}
