//! What an operation is allowed to spend, and the guards that hold it.
//!
//! # Why this is a trait and not `TenantPools`
//!
//! In the shared fleet one process serves many tenants, so a permit is drawn
//! from a budget shared across all of them — that is [`erp_control::TenantPools`].
//! A tenant running as its own deployment (D15) has no fleet to share with, and
//! must not link one: a binary that ships to a customer's own cloud cannot carry
//! the map of everybody else's.
//!
//! Both answer the same question — *may this operation run now?* — so the answer
//! is a trait, and the two deployments supply different implementations.

use std::ops::{Deref, DerefMut};

use sqlx::{PgConnection, Postgres};
use tokio::sync::OwnedSemaphorePermit;

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("no cluster named {0:?} is configured")]
    UnknownCluster(String),
    #[error("the {lane} connection budget is exhausted; retry shortly")]
    Overloaded { lane: Lane },
    #[error(transparent)]
    Connect(#[from] sqlx::Error),
}

/// Which bulkhead an operation draws its budget from.
///
/// Sized separately so one class of traffic cannot exhaust another. The API
/// layer picks the lane from the authenticated audience and the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// An employee waiting on a screen. Smallest allowance, most protected —
    /// a counter that stops working is worse than a slow consumer app.
    Interactive,
    /// A tenant's customers, through their app or website. The flood.
    Client,
    /// Projections, outbox delivery, migrations, reapers. Yields to both of the
    /// above: nobody is watching.
    Background,
}

impl std::fmt::Display for Lane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Interactive => "interactive",
            Self::Client => "client",
            Self::Background => "background",
        })
    }
}

/// Whatever decides if an operation may run now.
///
/// The only thing [`TenantDb`](crate::TenantDb) needs from the machinery that
/// owns connections, which is what keeps a module from linking the fleet.
pub trait Budget: std::fmt::Debug + Send + Sync {
    /// A permit for one operation in this lane, or [`PoolError::Overloaded`].
    ///
    /// **Fails fast rather than queueing.** A caller waiting on an exhausted
    /// budget is a request holding resources it will not get, which is how a
    /// slow database becomes an outage.
    fn permit(&self, lane: Lane) -> Result<OwnedSemaphorePermit, PoolError>;
}

// ---------------------------------------------------------------------------
// Permit-carrying connection handles
// ---------------------------------------------------------------------------

/// A pooled connection that holds its budget permit for exactly as long as it
/// lives. Dropping it returns both.
#[derive(Debug)]
pub struct Conn {
    inner: sqlx::pool::PoolConnection<Postgres>,
    _permit: OwnedSemaphorePermit,
}

impl Conn {
    pub(crate) const fn new(
        inner: sqlx::pool::PoolConnection<Postgres>,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            inner,
            _permit: permit,
        }
    }
}

impl Deref for Conn {
    type Target = PgConnection;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Conn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// A transaction that holds its budget permit until it commits or rolls back.
///
/// Not `Drop`-committing: an unfinished transaction rolls back, which is the
/// safe default and matches sqlx.
#[derive(Debug)]
pub struct Tx {
    inner: sqlx::Transaction<'static, Postgres>,
    _permit: OwnedSemaphorePermit,
}

impl Tx {
    pub(crate) const fn new(
        inner: sqlx::Transaction<'static, Postgres>,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            inner,
            _permit: permit,
        }
    }

    pub async fn commit(self) -> Result<(), sqlx::Error> {
        self.inner.commit().await
    }

    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        self.inner.rollback().await
    }
}

impl Deref for Tx {
    type Target = PgConnection;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Tx {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl erp_i18n::Localize for PoolError {
    fn message(&self) -> erp_i18n::Message {
        match self {
            Self::Overloaded { .. } => erp_i18n::Message::new(crate::messages::OVERLOADED),
            // An unconfigured cluster is our misconfiguration, not the user's
            // problem to understand.
            Self::UnknownCluster(_) | Self::Connect(_) => {
                erp_i18n::Message::new(crate::messages::INTERNAL)
            }
        }
    }
}
