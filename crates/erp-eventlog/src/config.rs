//! What a tenant has chosen.
//!
//! # Why this is here and not in a `erp-config` crate
//!
//! The table is part of the tenant-plane schema, and this crate is what embeds
//! those migrations. A separate crate could not own its own migration without a
//! second migrator against the same database, which is a problem this project
//! has already had once (see `modules/ledger/schema/install.sql`).
//!
//! It moves out the day configuration grows layers, declarations and resolution
//! rules — the system architecture §6 describes. This is the store underneath
//! that, and deliberately only the store.
//!
//! # What it is not
//!
//! Not a settings bag anything may write to. The *mechanism* is key-value; the
//! *surface* is typed, one endpoint per thing a tenant can configure, so a
//! value that reaches this table has already been through the type that gives
//! it meaning. A generic "set any key to any JSON" endpoint would make every
//! reader's validation the only thing standing between a typo and a broken
//! module.

use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::PgConnection;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A stored value does not fit the type that gives it meaning.
    ///
    /// Stops rather than falling back to a default (L6): a tenant who
    /// configured something and is silently getting the shipped value instead
    /// has a problem nobody will notice until the month end.
    #[error("configuration {key} is not usable: {reason}")]
    Invalid { key: String, reason: String },
    /// **Somebody else wrote this key since the caller read it.** The caller
    /// said "only if it is still version `expected`", and it is not.
    ///
    /// The first version had no way to say that: every settings screen was
    /// last-write-wins when two people edited at once, and the loser was never
    /// told.
    #[error("configuration {key} is at version {found}, not {expected}")]
    Conflict {
        key: String,
        expected: i64,
        found: i64,
    },
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl erp_i18n::Localize for ConfigError {
    fn message(&self) -> erp_i18n::Message {
        match self {
            Self::Conflict { .. } => {
                erp_i18n::Message::new(crate::messages::CONFIGURATION_CONFLICT)
            }
            // Both are ours: a corrupt row, or a database that is unwell.
            // Neither is something a user did.
            Self::Invalid { .. } | Self::Database(_) => {
                erp_i18n::Message::new(crate::messages::INTERNAL)
            }
        }
    }
}

/// A configured value and the generation it was written in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configured<T> {
    pub value: T,
    pub version: i64,
}

/// Reads a configured value, decoded into the type that gives it meaning.
///
/// `None` when the tenant has never set it — which is the normal case, and why
/// every caller pairs this with a shipped default rather than an error. "Most
/// tenants never open the settings" is the requirement, not a shortcut.
pub async fn get<T: DeserializeOwned>(
    conn: &mut PgConnection,
    key: &str,
) -> Result<Option<Configured<T>>, ConfigError> {
    let row = sqlx::query!(
        "SELECT value, version FROM configuration WHERE key = $1",
        key,
    )
    .fetch_optional(&mut *conn)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let value = serde_json::from_value(row.value).map_err(|e| ConfigError::Invalid {
        key: key.to_owned(),
        reason: e.to_string(),
    })?;

    Ok(Some(Configured {
        value,
        version: row.version,
    }))
}

/// Writes a configured value, returning the generation it landed in.
///
/// Takes `&T` rather than raw JSON: the only way into this table is through the
/// type that gives the value meaning, so a reader's decode cannot be the first
/// thing to notice a mistake.
///
/// **`expected` is the version the caller read**, or `None` to write whatever
/// is there. With `Some(n)`, the write happens only if the key is still at
/// version `n` — `0` meaning "nothing was set" — and is otherwise refused with
/// [`ConfigError::Conflict`], which is how the second of two people editing the
/// same screen finds out they were second. The check and the write are one
/// statement, so there is no window between them.
pub async fn set<T: Serialize>(
    conn: &mut PgConnection,
    key: &str,
    value: &T,
    set_by: Option<&str>,
    expected: Option<i64>,
) -> Result<i64, ConfigError> {
    let encoded = serde_json::to_value(value).map_err(|e| ConfigError::Invalid {
        key: key.to_owned(),
        reason: e.to_string(),
    })?;

    // A fresh key is at version zero, so `expected = Some(0)` lets the insert
    // through and refuses the update; `expected = Some(n)` refuses the update
    // unless the row is at `n`. `NULL` is the unconditional write.
    let version = sqlx::query_scalar!(
        r#"INSERT INTO configuration (key, value, version, set_by)
           VALUES ($1, $2, nextval('configuration_version'), $3)
           ON CONFLICT (key) DO UPDATE
              SET value   = EXCLUDED.value,
                  version = nextval('configuration_version'),
                  set_at  = now(),
                  set_by  = EXCLUDED.set_by
            WHERE $4::bigint IS NULL OR configuration.version = $4
         RETURNING version"#,
        key,
        encoded,
        set_by,
        expected,
    )
    .fetch_optional(&mut *conn)
    .await?;

    match (version, expected) {
        (Some(version), _) => Ok(version),
        (None, Some(expected)) => Err(ConfigError::Conflict {
            key: key.to_owned(),
            expected,
            found: version_of(&mut *conn, key).await?,
        }),
        // Cannot happen: an unconditional upsert always returns a row.
        (None, None) => Err(ConfigError::Invalid {
            key: key.to_owned(),
            reason: "the write returned nothing".to_owned(),
        }),
    }
}

/// The generation one key is at, or zero when it has never been set.
///
/// What a settings screen hands back as its `ETag`, and what it sends back as
/// `If-Match` — see [`set`]. Zero is a real answer: "nothing is set, and I know
/// it" is a state somebody can be second to change.
pub async fn version_of(conn: &mut PgConnection, key: &str) -> Result<i64, ConfigError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT COALESCE(max(version), 0) as "version!" FROM configuration WHERE key = $1"#,
        key,
    )
    .fetch_one(&mut *conn)
    .await?)
}

/// **The tenant's clock**, or Riyadh when nobody has set one.
///
/// Read by every command that turns an instant into a day, and by `append`,
/// which stamps it onto each event's metadata so a projection reads the clock
/// the event was written under — see `erp_types::Calendar`.
pub async fn calendar(conn: &mut PgConnection) -> Result<erp_types::Calendar, ConfigError> {
    Ok(get::<erp_types::Calendar>(conn, erp_types::Calendar::KEY)
        .await?
        .map_or_else(erp_types::Calendar::default, |configured| configured.value))
}

/// The generation of a tenant's configuration as a whole.
///
/// What goes into [`Metadata::config_version`](crate::Metadata) — the answer to
/// "what was configured when this command decided?", recorded so it can be
/// asked later without ever being *read* later. Zero when nothing is
/// configured, which is a real answer rather than a missing one.
pub async fn version(conn: &mut PgConnection) -> Result<i64, ConfigError> {
    Ok(
        sqlx::query_scalar!(r#"SELECT COALESCE(max(version), 0) as "version!" FROM configuration"#)
            .fetch_one(&mut *conn)
            .await?,
    )
}
