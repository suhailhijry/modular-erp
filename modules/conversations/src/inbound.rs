//! Landing what a customer said.
//!
//! # The contract, and why it is a contract rather than an adapter
//!
//! An operator registers their gateway relay's secret under the provider name
//! [`PROVIDER`] and posts to `POST /v1/hooks/messages`, which Phase 12b already
//! verifies, deduplicates and records:
//!
//! ```json
//! { "id": "<the gateway's own message id>", "from": "+9665…", "body": "…", "sent_at": "…" }
//! ```
//!
//! Not a vendor client: §26's argument stands that one written from
//! documentation against an account nobody has is a file which passes its tests
//! and fails every real message. This is the same call `messaging`'s transports
//! made.
//!
//! # Why a job and not a handler
//!
//! An effect handler is given no database connection — deliberately, so a slow
//! gateway cannot exhaust a tenant's pool — and landing a reply is three read
//! models deep: what was last said to that number, who that number is, and the
//! thread either of those names.
//!
//! # Why the sweep needs no cursor
//!
//! Correlation is against the **reply's own instant**, so the same webhook
//! lands on the same thread however often the sweep runs; and hearing it twice
//! writes nothing, because the thread remembers the gateway's id. A window that
//! overlaps the last one therefore costs nothing, which is what lets this be a
//! scan rather than a queue.

use erp_eventlog::Metadata;
use erp_tenant::TenantDb;
use erp_types::{AggregateId, Timestamp};
use messaging::{Subject, Topic};
use serde::{Deserialize, Serialize};

/// The provider name a tenant registers their inbound relay's secret under.
///
/// Fixed rather than configurable: a setting naming it would be one more thing
/// to get wrong in two places, and the operator is already choosing this string
/// when they store the secret.
pub const PROVIDER: &str = "messages";

/// What a gateway relay sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inbound {
    /// **The gateway's own id**, which is what makes hearing it twice nothing.
    pub id: String,
    /// The number or address it came from.
    pub from: String,
    pub body: String,
    /// When they sent it. **What correlation is measured against** — see the
    /// module docs.
    pub sent_at: Timestamp,
}

/// Where one message went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    pub thread: AggregateId,
    /// What it turned out to be about, or `None` for the tray.
    pub about: Option<Subject>,
    /// Whether this call wrote it. `false` means it had already been heard.
    pub fresh: bool,
}

/// What one sweep did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Landing {
    /// Messages written into a thread.
    pub landed: usize,
    /// Already heard, so nothing was written.
    pub known: usize,
    /// Left in the tray, because nothing and nobody matched the number.
    pub unmatched: usize,
    /// Payloads that were not an inbound message at all.
    pub unreadable: usize,
}

/// Why a sweep stopped.
///
/// **Not why one message was not landed** — those are counted in [`Landing`]
/// and logged, because a window is a batch of unrelated messages and one that
/// cannot be read is no reason to drop the rest.
#[derive(Debug, thiserror::Error)]
pub enum InboundError {
    #[error(transparent)]
    Pool(#[from] erp_tenant::PoolError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Command(#[from] erp_tenant::CommandError<crate::ConversationError>),
}

/// Lands one message in the thread it belongs to.
///
/// Three questions in order, and the first that answers wins:
///
/// 1. what was last said to this number, as of when they replied;
/// 2. whose number it is;
/// 3. nobody's — the tray.
pub async fn hear_from(
    db: &TenantDb,
    inbound: &Inbound,
    within: chrono::TimeDelta,
    metadata: &Metadata,
) -> Result<Landed, InboundError> {
    let about = {
        let mut conn = db.read().await?;

        // **As of the reply's own instant, never the clock.** Against `now`
        // this would give a different answer every time the sweep ran, and the
        // same reply would land on a different thread each time.
        let answered =
            messaging::last_sent_to(&mut conn, &inbound.from, inbound.sent_at, within).await?;
        match answered {
            Some(subject) => Some(subject),
            None => crm::customer_by_phone(&mut conn, &inbound.from)
                .await?
                .and_then(|customer| {
                    AggregateId::new(customer.id)
                        .ok()
                        .map(|id| Subject::new(Topic::Customer, id))
                }),
        }
    };

    let thread = about
        .as_ref()
        .map_or_else(|| crate::tray_id(&inbound.from), crate::thread_id);

    let committed = crate::hear(
        db,
        &thread,
        &inbound.from,
        &inbound.body,
        &inbound.id,
        inbound.sent_at,
        metadata,
    )
    .await?;

    Ok(Landed {
        thread,
        about,
        fresh: !committed.events.is_empty(),
    })
}

/// **The sweep.** Reads what has arrived and lands all of it.
///
/// Nothing here is fatal to the batch: a payload that is not an inbound message
/// is counted and logged, and the rest of the window is still landed.
pub async fn land(
    db: &TenantDb,
    since: Timestamp,
    within: chrono::TimeDelta,
    limit: i64,
) -> Result<Landing, InboundError> {
    let arrived: Vec<serde_json::Value> = {
        let mut conn = db.read().await?;
        sqlx::query_scalar!(
            r#"SELECT payload as "payload!"
                 FROM webhook_event
                WHERE provider = $1 AND received_at > $2
                ORDER BY received_at
                LIMIT $3"#,
            PROVIDER,
            since,
            limit,
        )
        .fetch_all(&mut *conn)
        .await?
    };

    let mut landing = Landing::default();
    for payload in arrived {
        let Ok(inbound) = serde_json::from_value::<Inbound>(payload) else {
            // A relay sending something else is a misconfiguration somebody has
            // to fix, and not a reason to stop landing everybody else's
            // replies.
            landing.unreadable += 1;
            continue;
        };

        match hear_from(db, &inbound, within, &Metadata::default()).await {
            Ok(landed) => {
                if !landed.fresh {
                    landing.known += 1;
                } else if landed.about.is_some() {
                    landing.landed += 1;
                } else {
                    landing.landed += 1;
                    landing.unmatched += 1;
                }
            }
            Err(error) => {
                tracing::warn!(message_id = inbound.id, %error, "a reply could not be landed");
            }
        }
    }

    Ok(landing)
}
