//! What was said, so a reply can be answered.
//!
//! # Why this module keeps a record at all
//!
//! Everything else here is fire-and-forget by design: resolve late, render
//! late, promise an effect, hold nothing. That is right for sending and useless
//! for receiving — an SMS reply is a number and a body, and *what it answers*
//! exists nowhere unless what was sent to that number was written down.
//!
//! # Why it is not a projection
//!
//! A send is an **effect promise, not an event**. Nothing in the log says a
//! message went out, so nothing here could be rebuilt from it — and a rebuild
//! that emptied this table would silently stop every reply correlating. Same
//! argument, same place, as the meter and the device tokens.

use erp_types::Timestamp;
use sqlx::PgConnection;

use crate::audience::{Subject, Topic};
use crate::send::Outbound;

/// Records one promised message.
///
/// Keyed on the outbox key, so a promise the outbox deduplicated is recorded
/// once here too — and called from inside [`crate::deliver`], in the same
/// transaction as the promise, so nothing is recorded that was not also sent.
pub async fn record(
    conn: &mut PgConnection,
    key: &str,
    message: &Outbound,
    about: Option<&Subject>,
    at: Timestamp,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO message_sent (key, channel, addressed_to, topic, subject_id, sent_at)
         VALUES ($1,$2,$3,$4,$5,$6)
         ON CONFLICT (key) DO NOTHING",
        key,
        message.channel.as_str(),
        message.to,
        about.map(|subject| subject.topic.as_str()),
        about.map(|subject| subject.id.as_str()),
        at,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// **What was last said to this address, as of an instant.**
///
/// # Why `before` and never the clock
///
/// This answers "what does this reply answer", and a reply has its own moment.
/// Correlating against *now* would give a different answer every time the
/// question was asked — the same reply landing on one booking this minute and
/// another the next, as later messages went out. Against the reply's own
/// instant the answer never moves, which is what lets the sweep that uses it
/// re-run over an overlapping window without a cursor.
///
/// Messages sent about nothing — a one-time code, a signup email — are not
/// candidates: there is no subject to correlate to.
pub async fn last_sent_to(
    conn: &mut PgConnection,
    address: &str,
    before: Timestamp,
    within: chrono::TimeDelta,
) -> Result<Option<Subject>, sqlx::Error> {
    let since = before - within;
    let row = sqlx::query!(
        r#"SELECT topic as "topic!", subject_id as "subject_id!"
             FROM message_sent
            WHERE addressed_to = $1
              AND sent_at <= $2
              AND sent_at > $3
              AND topic IS NOT NULL
              AND subject_id IS NOT NULL
            ORDER BY sent_at DESC
            LIMIT 1"#,
        address,
        before,
        since,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.and_then(|row| {
        let topic: Topic = row.topic.parse().ok()?;
        let id = erp_types::AggregateId::new(row.subject_id).ok()?;
        Some(Subject::new(topic, id))
    }))
}
