//! Writing in a thread: a note, something said, something heard.

use erp_eventlog::{Decision, Metadata};
use erp_i18n::{Localize, Message, MessageArg};
use erp_tenant::TenantDb;
use erp_types::{AggregateId, Timestamp};
use messaging::{Channel, Subject};

use crate::thread::{Thread, ThreadEvent};

type Wrote =
    Result<erp_eventlog::Committed<ThreadEvent>, erp_tenant::CommandError<ConversationError>>;

/// The longest a single line may be.
///
/// Generous for a note and far past what any gateway will carry in one message
/// — the segment meter is what makes a long text expensive, and that is
/// `messaging`'s to say, not this module's.
pub const MAX_BODY: usize = 4_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversationError {
    #[error("a message needs something in it")]
    NothingToSay,
    /// No customer to send to — a thread about an employee, or about a record
    /// with nobody attached.
    #[error("there is nobody to send this to")]
    NoClient,
    #[error("that customer has no {0} address")]
    NotReachableThere(String),
    /// This month's budget for that channel is spent.
    ///
    /// **Carried through as itself**, so the route can answer `402` — a client
    /// that cannot tell "out of money" from "the database is unwell" retries
    /// the one thing retrying will never fix.
    #[error("{channel} has spent its budget of {limit} this month")]
    OverBudget { channel: String, limit: i32 },
    /// `WhatsApp` and push, and each for its own reason. See [`say`].
    #[error("{0} is not a channel a person types into")]
    NotAChannelForThis(String),
    #[error("that conversation has already been assigned")]
    AlreadyAssigned,
}

impl Localize for ConversationError {
    fn message(&self) -> Message {
        use crate::messages as m;
        match self {
            Self::NothingToSay => Message::new(m::NOTHING_TO_SAY),
            Self::NoClient => Message::new(m::NO_CLIENT),
            Self::NotReachableThere(channel) => {
                Message::new(m::NOT_REACHABLE_THERE).with("channel", MessageArg::text(channel))
            }
            Self::OverBudget { channel, limit } => Message::new(messaging::messages::OVER_BUDGET)
                .with("channel", MessageArg::text(channel))
                .with("limit", MessageArg::Int(i64::from(*limit))),
            Self::NotAChannelForThis(channel) => {
                Message::new(m::NOT_A_CHANNEL_FOR_THIS).with("channel", MessageArg::text(channel))
            }
            Self::AlreadyAssigned => Message::new(m::ALREADY_ASSIGNED),
        }
    }
}

/// Writes something down, for whoever opens this next.
///
/// **Internal, and it never leaves.** No effect is promised and no meter is
/// charged: a note is a thing the business says to itself.
pub async fn note(
    db: &TenantDb,
    subject: &Subject,
    text: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Wrote {
    let text = trimmed(text)?;
    db.execute::<Thread, _, ConversationError>(
        &crate::thread_id(subject),
        crate::upcasters(),
        metadata,
        move |_| {
            Ok(Decision::one(ThreadEvent::Noted {
                text: text.clone(),
                at,
            }))
        },
    )
    .await
}

/// Says something to the customer this thread is about.
///
/// # The record and the promise are one transaction
///
/// The event and the effect that carries it are written together, so there is
/// no state in which this system believes it said something it did not — and a
/// spent budget refuses both. `messaging::deliver` charges the meter inside
/// that same transaction, which is why a refusal must roll the whole thing
/// back rather than be swallowed.
pub async fn say(
    db: &TenantDb,
    subject: &Subject,
    text: &str,
    channel: Channel,
    at: Timestamp,
    metadata: &Metadata,
) -> Wrote {
    let text = trimmed(text)?;

    // **SMS and email, and nothing else.**
    //
    // WhatsApp takes pre-approved templates outside a 24-hour service window
    // and refuses free text with error 131047 (§26) — every message typed here
    // is outside one, so promising it would be this system pretending. Push
    // addresses a device rather than a person, and a reply to somebody's phone
    // app is a different act from answering their message.
    if !matches!(channel, Channel::Sms | Channel::Email) {
        return Err(rejected(ConversationError::NotAChannelForThis(
            channel.as_str().to_owned(),
        )));
    }

    let id = crate::thread_id(subject);
    let mut tx = db.begin().await?;
    let outcome = async {
        let conn = &mut *tx;

        // Who this is, right now — the same resolution a reminder makes, and
        // for the same reason: a customer who changed their number this
        // morning is answered at the new one.
        let client =
            messaging::audience::people(&mut *conn, messaging::Audience::Client, subject, None)
                .await
                .map_err(erp_eventlog::ExecuteError::Database)?;
        let Some(address) = client
            .iter()
            .find_map(|person| person.reachable_on(channel))
        else {
            return Err(erp_eventlog::ExecuteError::Rejected(if client.is_empty() {
                ConversationError::NoClient
            } else {
                ConversationError::NotReachableThere(channel.as_str().to_owned())
            }));
        };

        let said = text.clone();
        let to = address.value.clone();
        let committed = erp_eventlog::try_execute::<Thread, _, ConversationError>(
            &mut *conn,
            &id,
            crate::upcasters(),
            metadata,
            move |_| {
                Ok(Decision::one(ThreadEvent::Said {
                    text: said.clone(),
                    channel,
                    to: to.clone(),
                    at,
                }))
            },
        )
        .await?;

        let message = messaging::Outbound {
            channel,
            to: address.value,
            subject: if channel.has_a_subject() {
                // An email needs a subject line and a person typing into a
                // thread is not writing one. What it is about is what the
                // thread is about.
                crate::subject_line(subject)
            } else {
                String::new()
            },
            body: text.clone(),
            locale: erp_i18n::Locale::DEFAULT,
            platform: None,
        };
        // Keyed on the line's own position, so a retry of this request promises
        // one message rather than two (L8).
        let key = format!(
            "conversations.{}.{}",
            id.as_str(),
            committed.at.map_or(0, erp_types::LogPosition::get)
        );
        messaging::deliver(&mut *conn, &message, key, Some(subject), at)
            .await
            .map_err(|e| match e {
                // **A spent budget is the caller's news**, not a fault: it is
                // the one refusal here that a person can do something about,
                // and it has to survive as itself to reach them as one.
                messaging::SendError::Spend(messaging::SpendError::Refused(over)) => {
                    erp_eventlog::ExecuteError::Rejected(ConversationError::OverBudget {
                        channel: over.channel,
                        limit: over.limit,
                    })
                }
                messaging::SendError::Database(e) => erp_eventlog::ExecuteError::Database(e),
                other => {
                    erp_eventlog::ExecuteError::Database(sqlx::Error::Protocol(other.to_string()))
                }
            })?;

        Ok(committed)
    }
    .await;

    match outcome {
        Ok(committed) => {
            tx.commit()
                .await
                .map_err(erp_eventlog::ExecuteError::from)?;
            Ok(committed)
        }
        Err(e) => {
            // **Roll back, including the meter.** It is written before the
            // budget is checked — that write is the lock that makes two
            // concurrent sends resolve to one — so a caller that swallowed this
            // would have spent budget on a message it did not send.
            tx.rollback()
                .await
                .map_err(erp_eventlog::ExecuteError::from)?;
            Err(e.into())
        }
    }
}

/// Records something a customer said, in the thread it belongs to.
///
/// Hearing the same message twice writes nothing: the gateway's own id is what
/// the thread remembers, and theirs is what is stable across their retries.
pub async fn hear(
    db: &TenantDb,
    thread: &AggregateId,
    from: &str,
    text: &str,
    message_id: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Wrote {
    let text = text.trim().to_owned();
    let from = from.to_owned();
    let message_id = message_id.to_owned();

    db.execute::<Thread, _, ConversationError>(
        thread,
        crate::upcasters(),
        metadata,
        move |loaded| {
            if loaded.aggregate.has_heard(&message_id) {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(ThreadEvent::Heard {
                from: from.clone(),
                text: text.clone(),
                message_id: message_id.clone(),
                at,
            }))
        },
    )
    .await
}

/// Says what a tray conversation was about after all.
///
/// **Moves what has arrived, and binds nothing.** The next message from that
/// number lands in the tray again, because "this number is that customer" is a
/// fact about the customer record — `crm` is where it belongs, and a second way
/// to change one from here would be a second answer.
pub async fn assign(
    db: &TenantDb,
    address: &str,
    subject: &Subject,
    at: Timestamp,
    metadata: &Metadata,
) -> Wrote {
    let topic = subject.topic.as_str().to_owned();
    let onto = subject.id.clone();

    db.execute::<Thread, _, ConversationError>(
        &crate::tray_id(address),
        crate::upcasters(),
        metadata,
        move |loaded| {
            if loaded.aggregate.assigned_to.is_some() {
                return Err(ConversationError::AlreadyAssigned);
            }
            Ok(Decision::one(ThreadEvent::Assigned {
                topic: topic.clone(),
                subject: onto.clone(),
                at,
            }))
        },
    )
    .await
}

fn trimmed(text: &str) -> Result<String, erp_tenant::CommandError<ConversationError>> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > MAX_BODY {
        return Err(rejected(ConversationError::NothingToSay));
    }
    Ok(text.to_owned())
}

fn rejected(error: ConversationError) -> erp_tenant::CommandError<ConversationError> {
    erp_tenant::CommandError::Execute(erp_eventlog::ExecuteError::Rejected(error))
}
