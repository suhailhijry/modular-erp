//! What has been sent, and what a tenant is willing to spend.
//!
//! # Why a budget at all
//!
//! Because SMS is billed per segment by somebody else, and this system is what
//! decides how many go out. A loop that sends a reminder per booking per day,
//! or a template somebody made 200 characters long in Arabic, is a bill that
//! arrives a month later with nothing in between to have stopped it.
//!
//! **It refuses rather than overspending (L6).** A tenant who has run out is
//! told so, loudly, at the moment a message would have gone — not by an invoice.
//!
//! # Why the meter is a table and not a projection
//!
//! It is consulted inside the transaction that adds to it, which is what makes
//! two sends racing for the last segment resolve to one. A projection is a
//! second behind by design, and a budget enforced against a number that is a
//! second out of date is not a budget. Same category as an occupancy claim, and
//! it lives in the migration chain for the same reason.

use erp_eventlog::{ConfigError, configuration as config};
use erp_i18n::{Localize, Message, MessageArg};
use erp_types::Timestamp;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

use crate::channel::Channel;

/// Where a tenant's spending limits live.
pub const KEY: &str = "messaging.budget";

/// What may be sent in a month, per channel.
///
/// # The shipped defaults, and why they are not "unlimited"
///
/// SMS and `WhatsApp` cost money per message and are the two that can run away.
/// Shipping them uncapped would mean the first bad loop is discovered by
/// finance; shipping a cap means it is discovered by whoever wrote the loop, on
/// the day, with a message naming the cap.
///
/// Email and push are effectively free per message and are capped by nothing,
/// because a cap there would refuse an invitation to save nobody anything.
///
/// **`configured` says whether anybody chose these**, exactly as
/// `hr_sa::Schedule` does. A business that means to send twenty thousand
/// messages a month should be able to see that nobody has said so yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Segments, not messages — see [`Channel::units`].
    pub sms: Option<i32>,
    pub whatsapp: Option<i32>,
    pub email: Option<i32>,
    pub push: Option<i32>,
    /// Whether a person set these, or they are what shipped.
    #[serde(default)]
    pub configured: bool,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            sms: Some(2_000),
            whatsapp: Some(2_000),
            email: None,
            push: None,
            configured: false,
        }
    }
}

impl Budget {
    /// The limit for one channel, if there is one.
    #[must_use]
    pub const fn limit(&self, channel: Channel) -> Option<i32> {
        match channel {
            Channel::Sms => self.sms,
            Channel::WhatsApp => self.whatsapp,
            Channel::Email => self.email,
            Channel::Push => self.push,
        }
    }
}

/// What one channel has sent in one month.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spent {
    pub period: String,
    pub channel: Channel,
    pub messages: i32,
    /// **What is billed.** Equal to `messages` on every channel but SMS.
    pub segments: i32,
    /// The cap in force, if there is one.
    pub limit: Option<i32>,
}

impl Spent {
    /// What is left, when there is a limit. Never negative.
    #[must_use]
    pub fn remaining(&self) -> Option<i32> {
        self.limit.map(|limit| (limit - self.segments).max(0))
    }
}

/// Why a message was not sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{channel} has spent {spent} of {limit} this month")]
pub struct OverBudget {
    pub channel: String,
    pub spent: i32,
    pub limit: i32,
}

impl Localize for OverBudget {
    fn message(&self) -> Message {
        Message::new(crate::messages::OVER_BUDGET)
            .with("channel", MessageArg::text(&self.channel))
            .with("limit", MessageArg::Int(i64::from(self.limit)))
    }
}

/// `YYYY-MM`, from the caller's instant.
///
/// The caller's, never a clock reading: a reminder enqueued on the 31st for a
/// booking on the 1st is metered in the month it was sent, and a replayed or
/// backdated send lands where it belongs.
#[must_use]
pub fn period(at: Timestamp) -> String {
    at.format("%Y-%m").to_string()
}

/// Adds to the meter and refuses if that takes the month over its budget.
///
/// # Roll back on a refusal
///
/// The meter is written **before** the limit is checked, because the write is
/// what takes the row lock that makes two concurrent sends resolve to one. A
/// caller that ignores the refusal and commits has spent budget on a message it
/// did not send. This crate never opens a transaction behind your back —
/// exactly as `erp_occupancy::take` does not, and for the same reason.
pub async fn charge(
    conn: &mut PgConnection,
    channel: Channel,
    units: i32,
    at: Timestamp,
) -> Result<Spent, SpendError> {
    let period = period(at);

    let row = sqlx::query!(
        r#"INSERT INTO message_meter (period, channel, messages, segments, first_at, last_at)
           VALUES ($1, $2, 1, $3, $4, $4)
           ON CONFLICT (period, channel) DO UPDATE
               SET messages = message_meter.messages + 1,
                   segments = message_meter.segments + EXCLUDED.segments,
                   last_at = EXCLUDED.last_at
           RETURNING messages as "messages!", segments as "segments!""#,
        period,
        channel.as_str(),
        units,
        at,
    )
    .fetch_one(&mut *conn)
    .await?;

    let budget = config::get::<Budget>(&mut *conn, KEY)
        .await?
        .map_or_else(Budget::default, |c| c.value);
    let limit = budget.limit(channel);

    if let Some(limit) = limit
        && row.segments > limit
    {
        return Err(SpendError::Refused(OverBudget {
            channel: channel.as_str().to_owned(),
            spent: row.segments,
            limit,
        }));
    }

    Ok(Spent {
        period,
        channel,
        messages: row.messages,
        segments: row.segments,
        limit,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum SpendError {
    #[error(transparent)]
    Refused(#[from] OverBudget),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// What every channel has spent in one month.
///
/// Every channel appears, including ones that have sent nothing — "we sent no
/// SMS this month" is an answer, and a missing row is not.
pub async fn spent(conn: &mut PgConnection, period: &str) -> Result<Vec<Spent>, SpendError> {
    let rows = sqlx::query!(
        r#"SELECT channel as "channel!", messages as "messages!", segments as "segments!"
             FROM message_meter WHERE period = $1"#,
        period,
    )
    .fetch_all(&mut *conn)
    .await?;

    let budget = config::get::<Budget>(&mut *conn, KEY)
        .await?
        .map_or_else(Budget::default, |c| c.value);

    Ok(Channel::ALL
        .into_iter()
        .map(|channel| {
            let found = rows.iter().find(|r| r.channel == channel.as_str());
            Spent {
                period: period.to_owned(),
                channel,
                messages: found.map_or(0, |r| r.messages),
                segments: found.map_or(0, |r| r.segments),
                limit: budget.limit(channel),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two channels that cost money ship with a cap; the two that do not,
    /// do not. See the type's docs for why that asymmetry is deliberate.
    #[test]
    fn the_channels_that_cost_money_ship_capped() {
        let shipped = Budget::default();
        assert!(shipped.limit(Channel::Sms).is_some());
        assert!(shipped.limit(Channel::WhatsApp).is_some());
        assert!(shipped.limit(Channel::Email).is_none());
        assert!(shipped.limit(Channel::Push).is_none());
        assert!(
            !shipped.configured,
            "nobody has chosen these, and the API has to be able to say so"
        );
    }

    #[test]
    fn what_is_left_never_goes_negative() {
        let over = Spent {
            period: "2026-05".to_owned(),
            channel: Channel::Sms,
            messages: 10,
            segments: 30,
            limit: Some(20),
        };
        assert_eq!(over.remaining(), Some(0));

        let uncapped = Spent {
            limit: None,
            ..over
        };
        assert_eq!(uncapped.remaining(), None);
    }
}
