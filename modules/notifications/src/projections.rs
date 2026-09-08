//! The bell, derived from the log.
//!
//! **Read state included.** `read_at` is set by applying
//! `notifications.notification.read`, never by a handler updating a row, which
//! is what makes a rebuild reproduce who had seen what.

use std::collections::{BTreeMap, HashMap, HashSet};

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{Cursor, Page, Timestamp};
use messaging::Channel;
use sqlx::PgConnection;

use crate::notification::{NotificationEvent, Wording};
use crate::person::PersonEvent;

#[derive(Debug)]
pub struct Notifications;

impl ProjectionGroup for Notifications {
    const NAME: &'static str = "notifications";
    const SCHEMA: &'static str = "proj_notifications";
}

#[derive(Debug)]
pub struct Inbox;

#[async_trait::async_trait]
impl Projection for Inbox {
    type Group = Notifications;

    fn name(&self) -> &'static str {
        "inbox"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        let name = envelope.event_name.as_str();
        if NotificationEvent::NAMES.contains(&name) {
            return self.notification(ctx, envelope, conn).await;
        }
        if PersonEvent::NAMES.contains(&name) {
            return self.person(ctx, envelope, conn).await;
        }
        Ok(())
    }
}

impl Inbox {
    async fn notification(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        let id = envelope.stream.id.as_str();
        match decode::<NotificationEvent>(ctx, envelope)? {
            NotificationEvent::Announced {
                kind,
                topic,
                subject,
                recipients,
                wording,
                at,
            } => {
                let words = serde_json::to_value(&wording).unwrap_or(serde_json::Value::Null);
                for recipient in &recipients {
                    sqlx::query(
                        "INSERT INTO inbox
                             (notification, recipient, kind, topic, subject_id, wording,
                              announced_at, recorded_at, position)
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
                         ON CONFLICT (notification, recipient) DO NOTHING",
                    )
                    .bind(id)
                    .bind(recipient)
                    .bind(&kind)
                    .bind(&topic)
                    .bind(subject.as_str())
                    .bind(&words)
                    .bind(at)
                    .bind(ctx.event_time())
                    .bind(ctx.position().get())
                    .execute(&mut *conn)
                    .await?;
                }
            }
            NotificationEvent::Read { by, at } => {
                // **Only if it is still unread.** Marking read twice must not
                // move the instant it was first seen.
                sqlx::query(
                    "UPDATE inbox SET read_at = $3, position = $4
                      WHERE notification = $1 AND recipient = $2 AND read_at IS NULL",
                )
                .bind(id)
                .bind(&by)
                .bind(at)
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }

    async fn person(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        match decode::<PersonEvent>(ctx, envelope)? {
            PersonEvent::PreferencesSet {
                identity, entries, ..
            } => {
                // **The whole grid, replacing.** The event is a statement of
                // what this person wants, so applying it leaves exactly what it
                // says and nothing from before it.
                sqlx::query("DELETE FROM preference WHERE identity = $1")
                    .bind(&identity)
                    .execute(&mut *conn)
                    .await?;
                for (kind, channels) in &entries {
                    let names: Vec<String> = channels
                        .iter()
                        .map(|c| c.as_str().to_owned())
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    sqlx::query(
                        "INSERT INTO preference (identity, kind, channels, recorded_at, position)
                         VALUES ($1,$2,$3,$4,$5)
                         ON CONFLICT (identity, kind) DO UPDATE
                             SET channels = EXCLUDED.channels,
                                 recorded_at = EXCLUDED.recorded_at,
                                 position = EXCLUDED.position",
                    )
                    .bind(&identity)
                    .bind(kind)
                    .bind(&names)
                    .bind(ctx.event_time())
                    .bind(ctx.position().get())
                    .execute(&mut *conn)
                    .await?;
                }
            }
            PersonEvent::ReadAll { identity, at } => {
                // **Everything of theirs that exists at this point in the log.**
                // Events apply in position order, so that set is exactly what
                // had been announced when they cleared it — during a rebuild as
                // much as live. See `crate::person` for why there is no
                // watermark.
                sqlx::query(
                    "UPDATE inbox SET read_at = $2, position = $3
                      WHERE recipient = $1 AND read_at IS NULL",
                )
                .bind(&identity)
                .bind(at)
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}

fn decode<E: serde::de::DeserializeOwned + 'static>(
    ctx: &ProjectionCtx<'_>,
    envelope: &Envelope,
) -> Result<E, ProjectionError> {
    ctx.decode::<E>(envelope)
        .map_err(|source| ProjectionError::Decode {
            event_name: envelope.event_name.as_str().to_owned(),
            position: envelope.position,
            source,
        })
}

#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Notifications>>> {
    vec![std::sync::Arc::new(Inbox)]
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// One notification, as the person it was addressed to reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxRow {
    pub id: String,
    pub kind: String,
    pub topic: String,
    pub subject_id: String,
    /// Locale code → what it says.
    pub wording: BTreeMap<String, Wording>,
    pub announced_at: Timestamp,
    pub read_at: Option<Timestamp>,
}

/// **One person's bell.**
///
/// The filter is a `WHERE`, not a decision the handler makes afterwards:
/// somebody else's notification is never selected, so no future refactor of the
/// handler can leak one.
pub async fn inbox(
    conn: &mut PgConnection,
    recipient: &str,
    unread_only: bool,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<InboxRow>, sqlx::Error> {
    let (announced_at, id) = match after.map(Cursor::parts) {
        Some(parts) if parts.len() == 2 => (parts[0].parse::<Timestamp>().ok(), parts[1].clone()),
        _ => (None, String::new()),
    };

    let rows = sqlx::query!(
        r#"SELECT notification as "id!", kind as "kind!", topic as "topic!",
                  subject_id as "subject_id!", wording as "wording!",
                  announced_at as "announced_at!", read_at
             FROM proj_notifications.inbox
            WHERE recipient = $1
              AND (NOT $2 OR read_at IS NULL)
              AND ($4::timestamptz IS NULL OR (announced_at, notification) < ($4, $5))
            ORDER BY announced_at DESC, notification DESC
            LIMIT $3"#,
        recipient,
        unread_only,
        limit,
        announced_at,
        id,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter()
            .map(|r| InboxRow {
                id: r.id,
                kind: r.kind,
                topic: r.topic,
                subject_id: r.subject_id,
                wording: serde_json::from_value(r.wording).unwrap_or_default(),
                announced_at: r.announced_at,
                read_at: r.read_at,
            })
            .collect(),
        limit,
        |n: &InboxRow| Cursor::over(&[&n.announced_at.to_rfc3339(), &n.id]),
    ))
}

/// The number on the badge.
pub async fn unread(conn: &mut PgConnection, recipient: &str) -> Result<i64, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT count(*) as "count!"
             FROM proj_notifications.inbox
            WHERE recipient = $1 AND read_at IS NULL"#,
        recipient,
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(row.count)
}

/// Everything of this person's that is still unread, newest first.
///
/// What *mark everything read* reads before it writes. Bounded, because a
/// person with ten thousand unread notifications is a person nobody should
/// clear in one transaction.
pub async fn unread_ids(
    conn: &mut PgConnection,
    recipient: &str,
    limit: i64,
) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT notification as "id!"
             FROM proj_notifications.inbox
            WHERE recipient = $1 AND read_at IS NULL
            ORDER BY announced_at DESC
            LIMIT $2"#,
        recipient,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|r| r.id).collect())
}

/// Which of these subjects this kind has already been announced about.
///
/// **What keeps a scan cheap, not what makes it correct.** The aggregate
/// refuses a repeat outright — see `crate::announce` — and this is how a
/// producer avoids opening a transaction per row to be told so. It reads a
/// projection, so it can lag; the answer being stale costs one wasted attempt.
pub async fn announced_subjects(
    conn: &mut PgConnection,
    kind: crate::Kind,
    subjects: &[String],
) -> Result<HashSet<String>, sqlx::Error> {
    if subjects.is_empty() {
        return Ok(HashSet::new());
    }
    let rows = sqlx::query!(
        r#"SELECT DISTINCT subject_id as "subject_id!"
             FROM proj_notifications.inbox
            WHERE kind = $1 AND subject_id = ANY($2)"#,
        kind.as_str(),
        subjects,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|r| r.subject_id).collect())
}

/// What these people want told to them, for one kind.
///
/// Only those who have said something appear. Everybody else takes
/// [`crate::DEFAULT_CHANNELS`], which the caller applies — a missing row and a
/// row saying "nothing" are different answers and this keeps them different.
pub async fn preferences_for(
    conn: &mut PgConnection,
    identities: &[String],
    kind: crate::Kind,
) -> Result<HashMap<String, Vec<Channel>>, sqlx::Error> {
    if identities.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        r#"SELECT identity as "identity!", channels as "channels!"
             FROM proj_notifications.preference
            WHERE identity = ANY($1) AND kind = $2"#,
        identities,
        kind.as_str(),
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let channels = r
                .channels
                .iter()
                .filter_map(|c| c.parse::<Channel>().ok())
                .collect();
            (r.identity, channels)
        })
        .collect())
}

/// One person's whole grid, as they set it.
///
/// Kinds they have never spoken about are absent; the caller fills them in with
/// [`crate::DEFAULT_CHANNELS`].
pub async fn preferences(
    conn: &mut PgConnection,
    identity: &str,
) -> Result<BTreeMap<String, Vec<Channel>>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT kind as "kind!", channels as "channels!"
             FROM proj_notifications.preference
            WHERE identity = $1"#,
        identity,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let channels = r
                .channels
                .iter()
                .filter_map(|c| c.parse::<Channel>().ok())
                .collect();
            (r.kind, channels)
        })
        .collect())
}
