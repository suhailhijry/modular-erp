//! Where a push notification goes.
//!
//! # Why a device is not a person
//!
//! Every other channel resolves to something a person owns — an address, a
//! number — and one of each. Push resolves to **devices**, and one person has a
//! phone and a tablet and last year's phone. A message goes to all of them that
//! still work.
//!
//! # Tokens expire, and the platform is the only thing that knows
//!
//! A device token stops working when the app is reinstalled, the device is
//! wiped, or the platform decides to rotate it. Nothing here can tell — the
//! only signal is the platform rejecting a send, which is what
//! [`retire`] records.
//!
//! So cleaning them up is **scheduled work over a column**, not a guess about
//! age. A token nobody has sent to in six months may be perfectly good; one the
//! platform rejected this morning is not.

use erp_types::Timestamp;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

/// What kind of device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Apns,
    Fcm,
    Web,
}

impl Platform {
    pub const ALL: [Self; 3] = [Self::Apns, Self::Fcm, Self::Web];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Apns => "apns",
            Self::Fcm => "fcm",
            Self::Web => "web",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a platform")]
pub struct UnknownPlatform(pub String);

impl std::str::FromStr for Platform {
    type Err = UnknownPlatform;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|p| p.as_str() == s)
            .ok_or_else(|| UnknownPlatform(s.to_owned()))
    }
}

/// A registered device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub token: String,
    pub recipient: String,
    pub platform: Platform,
    pub registered_at: Timestamp,
    pub retired_at: Option<Timestamp>,
    pub retired_why: Option<String>,
}

/// Records a device, or brings one back that was retired.
///
/// **Idempotent on the token** (L8), which is what an app calling this on every
/// launch needs. Re-registering also clears a retirement: a token the platform
/// rejected and the device has since offered again is working, and refusing to
/// believe the device about its own token would leave somebody permanently
/// unreachable.
pub async fn register(
    conn: &mut PgConnection,
    token: &str,
    recipient: &str,
    platform: Platform,
    at: Timestamp,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO push_token (token, recipient, platform, registered_at)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (token) DO UPDATE
             SET recipient = EXCLUDED.recipient,
                 platform = EXCLUDED.platform,
                 registered_at = EXCLUDED.registered_at,
                 retired_at = NULL,
                 retired_why = NULL",
        token,
        recipient,
        platform.as_str(),
        at,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Every working token for somebody. Empty is ordinary — most people have no
/// app installed.
pub async fn tokens(
    conn: &mut PgConnection,
    recipient: &str,
) -> Result<Vec<Registered>, sqlx::Error> {
    // **The platform travels with the token.** Two device tokens are both
    // opaque strings, and a transport handed one it cannot deliver to has no
    // way to tell by looking — see `crate::send::Outbound::platform`.
    let rows = sqlx::query!(
        r#"SELECT token as "token!", platform as "platform!" FROM push_token
            WHERE recipient = $1 AND retired_at IS NULL
            ORDER BY registered_at DESC"#,
        recipient,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Registered {
                token: row.token,
                // A platform this build does not know is a row a future version
                // wrote. Skipping it is better than sending to a transport that
                // cannot read it.
                platform: row.platform.parse().ok()?,
            })
        })
        .collect())
}

/// A token, and what kind of token it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub token: String,
    pub platform: Platform,
}

/// Records that the platform will not accept this token any more.
///
/// Called by the handler when a send is refused permanently. Not a delete: the
/// row is what stops the next message being addressed to it, and what a sweep
/// later removes.
pub async fn retire(
    conn: &mut PgConnection,
    token: &str,
    why: &str,
    at: Timestamp,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE push_token SET retired_at = $2, retired_why = $3
          WHERE token = $1 AND retired_at IS NULL",
        token,
        at,
        why,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Removes tokens retired before an instant.
///
/// **Scheduled work, not an afterthought.** A retired row costs an index entry
/// and nothing else, so the sweep is unhurried — the reason it exists at all is
/// that a table nobody ever deletes from grows for the life of the tenant.
///
/// Returns how many went.
pub async fn sweep(conn: &mut PgConnection, before: Timestamp) -> Result<u64, sqlx::Error> {
    let done = sqlx::query!(
        "DELETE FROM push_token WHERE retired_at IS NOT NULL AND retired_at < $1",
        before,
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// Every device on record for somebody, retired ones included.
pub async fn devices(conn: &mut PgConnection, recipient: &str) -> Result<Vec<Device>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT token as "token!", recipient as "recipient!", platform as "platform!",
                  registered_at as "registered_at!", retired_at, retired_why
             FROM push_token WHERE recipient = $1
            ORDER BY registered_at DESC"#,
        recipient,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let platform = row
                .platform
                .parse()
                .map_err(|e: UnknownPlatform| sqlx::Error::Decode(Box::new(e)))?;
            Ok(Device {
                token: row.token,
                recipient: row.recipient,
                platform,
                registered_at: row.registered_at,
                retired_at: row.retired_at,
                retired_why: row.retired_why,
            })
        })
        .collect()
}
