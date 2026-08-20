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
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl erp_i18n::Localize for ConfigError {
    fn message(&self) -> erp_i18n::Message {
        // Both are ours: a corrupt row, or a database that is unwell. Neither
        // is something a user did.
        erp_i18n::Message::new(crate::messages::INTERNAL)
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
pub async fn set<T: Serialize>(
    conn: &mut PgConnection,
    key: &str,
    value: &T,
    set_by: Option<&str>,
) -> Result<i64, ConfigError> {
    let encoded = serde_json::to_value(value).map_err(|e| ConfigError::Invalid {
        key: key.to_owned(),
        reason: e.to_string(),
    })?;

    let version = sqlx::query_scalar!(
        r#"INSERT INTO configuration (key, value, version, set_by)
           VALUES ($1, $2, nextval('configuration_version'), $3)
           ON CONFLICT (key) DO UPDATE
              SET value   = EXCLUDED.value,
                  version = nextval('configuration_version'),
                  set_at  = now(),
                  set_by  = EXCLUDED.set_by
         RETURNING version"#,
        key,
        encoded,
        set_by,
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(version)
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
