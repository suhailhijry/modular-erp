//! How long before a lot expires the business wants to hear about it.
//!
//! # Why this is a tenant setting and the costing method is not
//!
//! Because the right answer differs by trade and nothing in this module depends
//! on which one is chosen. A pharmacy orders six months out and a café would
//! throw away a week of milk warnings before it read one; neither answer is
//! more correct and neither changes an arithmetic. A costing method, by
//! contrast, decides what every movement ever recorded was worth, which is why
//! that one is frozen in the code (see `crate::stock`).
//!
//! # What reads it
//!
//! The worker, in `erp-worker`'s composition root: on every visit it tells
//! whoever may write stock off about each open lot that reaches its date within
//! this many days of the tenant's today, and about each one already past its
//! date — once apiece. It warns and does nothing else — writing stock off is a
//! person's decision (decision 9). A window of none still tells what has gone,
//! and what goes today. **Changing the window tells nobody twice**: what is said
//! once is keyed on the lot and whether it has gone, never on the window.
//! `GET /v1/inventory/summary` counts, per branch, the open lots going off
//! within it and the ones already past their date, from the tenant's today on
//! its calendar and to the last day [`ExpiryWindow::warns_until`] gives, so what
//! the bell tells and what the summary counts are one rule.
//! `GET /v1/inventory/lots?expiring_before=` takes a day per request and does
//! not fall back to this one, deliberately: a listing that hid lots by default
//! would be a listing nobody could trust.

use erp_eventlog::ConfigError;

/// The longest window this build will store.
///
/// Ten years. Not a policy — a bound, so a fat finger on a form cannot store a
/// number that makes every lot in the tenant permanently "expiring".
pub const MAX_DAYS: i32 = 3_650;

/// How many days ahead of an expiry the business wants warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExpiryWindow {
    pub days: i32,
}

/// A window that is not a number of days anybody could act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("an expiry warning window is a whole number of days, from none to ten years")]
pub struct NotAWindow;

impl erp_i18n::Localize for NotAWindow {
    fn message(&self) -> erp_i18n::Message {
        erp_i18n::Message::new(crate::messages::NOT_A_WINDOW)
    }
}

impl ExpiryWindow {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "inventory.expiry_window";

    /// **Thirty days**, which is the shortest window that catches a monthly
    /// order cycle. A business that reorders weekly will want less and one that
    /// imports will want more, and both can say so.
    pub const DEFAULT: Self = Self { days: 30 };

    /// # Errors
    /// [`NotAWindow`] for a negative window or one beyond [`MAX_DAYS`].
    pub const fn new(days: i32) -> Result<Self, NotAWindow> {
        if days < 0 || days > MAX_DAYS {
            return Err(NotAWindow);
        }
        Ok(Self { days })
    }

    /// **The last day a lot's date falls inside this window**: `today` plus
    /// the window, so thirty days on the 10th of April reaches everything dated
    /// up to the 10th of May. `today` is the tenant's day, through its calendar.
    /// `None` only past the end of the calendar.
    ///
    /// The one place the window's far end is worked out, so the worker telling
    /// somebody a lot is going off and the summary counting it cannot disagree
    /// about which lots that is.
    #[must_use]
    pub fn warns_until(self, today: chrono::NaiveDate) -> Option<chrono::NaiveDate> {
        today.checked_add_days(chrono::Days::new(
            u64::try_from(self.days).unwrap_or_default(),
        ))
    }

    /// What this tenant has set, or [`Self::DEFAULT`].
    ///
    /// # Errors
    /// A stored value this build cannot read, or the database.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or(Self::DEFAULT, |configured| configured.value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window is bounded at both ends, and the bound is what stops a form
    /// storing a number that warns about everything for ever.
    #[test]
    fn a_window_is_a_number_of_days_within_reason() {
        assert_eq!(ExpiryWindow::new(0), Ok(ExpiryWindow { days: 0 }));
        assert_eq!(
            ExpiryWindow::new(MAX_DAYS),
            Ok(ExpiryWindow { days: MAX_DAYS })
        );
        assert_eq!(ExpiryWindow::new(-1), Err(NotAWindow));
        assert_eq!(ExpiryWindow::new(MAX_DAYS + 1), Err(NotAWindow));
    }
}
