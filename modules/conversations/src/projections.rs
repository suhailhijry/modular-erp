//! What a thread reads as.
//!
//! **Nothing here says what a thread is about**: its id is derived from its
//! subject, so the route looking at booking `BK-1` computes the same id the
//! writer did. See `schema/install.sql`.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{Cursor, Page, Timestamp};
use sqlx::PgConnection;

use crate::thread::ThreadEvent;

#[derive(Debug)]
pub struct Conversations;

impl ProjectionGroup for Conversations {
    const NAME: &'static str = "conversations";
    const SCHEMA: &'static str = "proj_conversations";
}

#[derive(Debug)]
pub struct Messages;

#[async_trait::async_trait]
impl Projection for Messages {
    type Group = Conversations;

    fn name(&self) -> &'static str {
        "messages"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !ThreadEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let thread = envelope.stream.id.as_str();
        let who = envelope.metadata.actor.clone();

        match ctx
            .decode::<ThreadEvent>(envelope)
            .map_err(|source| ProjectionError::Decode {
                event_name: envelope.event_name.as_str().to_owned(),
                position: envelope.position,
                source,
            })? {
            ThreadEvent::Noted { text, at } => {
                line(ctx, conn, thread, "note", &text, None, None, who, at).await?;
            }
            ThreadEvent::Said {
                text,
                channel,
                to,
                at,
            } => {
                line(
                    ctx,
                    conn,
                    thread,
                    "said",
                    &text,
                    Some(channel.as_str()),
                    Some(&to),
                    who,
                    at,
                )
                .await?;
            }
            ThreadEvent::Heard {
                from,
                text,
                message_id: _,
                at,
            } => {
                // **No `who`.** It came from outside, and who sent it is the
                // address rather than one of this tenant's people.
                line(
                    ctx,
                    conn,
                    thread,
                    "heard",
                    &text,
                    None,
                    Some(&from),
                    None,
                    at,
                )
                .await?;

                // **Only the tray.** A thread is the tray for a number when
                // its id is what `tray_id` makes of that number — which the
                // projection can check, because the id is derived rather than
                // minted. Marking every thread that ever heard something would
                // put answered bookings in the list of replies nobody could
                // place.
                if crate::tray_id(&from).as_str() == thread {
                    sqlx::query(
                        "UPDATE conversation_thread SET address = COALESCE(address, $2)
                          WHERE thread = $1",
                    )
                    .bind(thread)
                    .bind(&from)
                    .execute(&mut *conn)
                    .await?;
                }
            }
            ThreadEvent::Assigned { topic, subject, at } => {
                let Ok(topic) = topic.parse::<messaging::Topic>() else {
                    // **Already history.** An event naming a topic this build
                    // does not know cannot be refused here — refusing would
                    // stop the group for every tenant. It is left where it is.
                    tracing::warn!(%thread, "a thread was assigned to a topic this build does not know");
                    return Ok(());
                };
                let onto = crate::thread_id(&messaging::Subject::new(topic, subject.clone()));

                // **The messages move.** Rebuild-safe because the assignment
                // applies after the lines it moves, in position order — live
                // and during a replay alike, since the log's order is the only
                // order either follows.
                sqlx::query("UPDATE conversation_message SET thread = $2 WHERE thread = $1")
                    .bind(thread)
                    .bind(onto.as_str())
                    .execute(&mut *conn)
                    .await?;

                // The tray row stays, saying where they went.
                sqlx::query(
                    "UPDATE conversation_thread
                        SET assigned_to = $2, messages = 0, recorded_at = $3, position = $4
                      WHERE thread = $1",
                )
                .bind(thread)
                .bind(onto.as_str())
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;

                summarise(ctx, conn, onto.as_str(), at).await?;
            }
        }
        Ok(())
    }
}

/// One line, and the thread's summary with it.
#[expect(
    clippy::too_many_arguments,
    reason = "one row's worth of columns; a struct here would be the same list \
              with a name in front of it"
)]
async fn line(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    thread: &str,
    kind: &str,
    body: &str,
    channel: Option<&str>,
    address: Option<&str>,
    who: Option<String>,
    at: Timestamp,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "INSERT INTO conversation_message
             (thread, position, kind, body, channel, address, who, said_at, recorded_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
         ON CONFLICT (thread, position) DO NOTHING",
    )
    .bind(thread)
    .bind(ctx.position().get())
    .bind(kind)
    .bind(body)
    .bind(channel)
    .bind(address)
    .bind(who)
    .bind(at)
    .bind(ctx.event_time())
    .execute(&mut *conn)
    .await?;

    summarise(ctx, conn, thread, at).await
}

/// The thread's own row: how many lines, and when the last one was.
async fn summarise(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    thread: &str,
    at: Timestamp,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "INSERT INTO conversation_thread
             (thread, messages, last_at, recorded_at, position)
         VALUES ($1,1,$2,$3,$4)
         ON CONFLICT (thread) DO UPDATE
             SET messages = conversation_thread.messages + 1,
                 last_at = GREATEST(conversation_thread.last_at, EXCLUDED.last_at),
                 recorded_at = EXCLUDED.recorded_at,
                 position = EXCLUDED.position",
    )
    .bind(thread)
    .bind(at)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Conversations>>> {
    vec![std::sync::Arc::new(Messages)]
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// One line of a conversation, as somebody reading it sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// `note`, `said` or `heard`.
    pub kind: String,
    pub body: String,
    pub channel: Option<String>,
    pub address: Option<String>,
    /// The identity that wrote it. `None` for anything heard.
    pub who: Option<String>,
    pub said_at: Timestamp,
    pub position: i64,
}

/// A conversation, **oldest first** — which is how a conversation reads.
pub async fn messages(
    conn: &mut PgConnection,
    thread: &str,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<Line>, sqlx::Error> {
    let since = match after.map(Cursor::parts) {
        Some(parts) if parts.len() == 1 => parts[0].parse::<i64>().unwrap_or(0),
        _ => 0,
    };

    let rows = sqlx::query!(
        r#"SELECT kind as "kind!", body as "body!", channel, address, who,
                  said_at as "said_at!", position as "position!"
             FROM proj_conversations.conversation_message
            WHERE thread = $1 AND position > $2
            ORDER BY position
            LIMIT $3"#,
        thread,
        since,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter()
            .map(|r| Line {
                kind: r.kind,
                body: r.body,
                channel: r.channel,
                address: r.address,
                who: r.who,
                said_at: r.said_at,
                position: r.position,
            })
            .collect(),
        limit,
        |line: &Line| Cursor::over(&[&line.position.to_string()]),
    ))
}

/// A thread nobody could be matched to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmatched {
    pub thread: String,
    /// The number it is with.
    pub address: String,
    pub messages: i32,
    pub last_at: Timestamp,
}

/// **The tray**: threads with a number and nobody attached, newest first.
pub async fn unmatched(conn: &mut PgConnection, limit: i64) -> Result<Vec<Unmatched>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT thread as "thread!", address as "address!",
                  messages as "messages!", last_at as "last_at!"
             FROM proj_conversations.conversation_thread
            WHERE address IS NOT NULL AND assigned_to IS NULL
            ORDER BY last_at DESC
            LIMIT $1"#,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| Unmatched {
            thread: r.thread,
            address: r.address,
            messages: r.messages,
            last_at: r.last_at,
        })
        .collect())
}
