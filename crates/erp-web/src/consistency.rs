//! Reading your own write.
//!
//! # The problem this solves
//!
//! Projections are driven by a worker, so a read taken immediately after a write
//! can legitimately not see it. That is the design — L4 buys exactly-once by
//! decoupling them — but it makes the most common client pattern (submit a form,
//! refresh the list) look like the write failed.
//!
//! Every write returns the log `position` it landed at. A client that cares
//! passes it back as `?consistent_after=<position>`, and the read waits for the
//! projection to reach it.
//!
//! # Why waiting is better than the alternatives
//!
//! *Reading the log directly* would mean every read model has a second
//! implementation that must agree with the first. *Writing synchronously to the
//! projection* is what L4 exists to avoid. *Serving stale and saying nothing* is
//! the current behaviour and the reason this file exists.
//!
//! # Why it does not usually wait
//!
//! A write also asks the control plane to visit the tenant
//! ([`ControlPlane::request_visit`](erp_control::ControlPlane::request_visit)),
//! so the worker picks it up within its claim interval rather than waiting out
//! the idle backoff. Without that, the first write after a quiet period would
//! wait up to thirty seconds — which is the difference between this being a
//! feature and a timeout.

use std::time::Duration;

use axum::extract::{FromRequestParts, Query};
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::Deserialize;
use erp_control::TenantDb;
use erp_i18n::Locale;
use erp_types::LogPosition;

use crate::problem::Problem;
use crate::state::AppState;

/// How long a read will wait for the projection to catch up.
///
/// Long enough to cover a worker claim cycle plus a batch; short enough that a
/// stalled projection surfaces as an error rather than as a hung request. A
/// client that wants to wait longer retries.
const MAX_WAIT: Duration = Duration::from_secs(2);

/// How often the checkpoint is re-read while waiting.
///
/// Polling, not notification. `LISTEN/NOTIFY` would be lower latency and is
/// deliberately not used (it is the legacy path this design replaced); at this
/// interval the cost is a handful of indexed lookups on a single row.
const POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Deserialize)]
struct ConsistentAfter {
    consistent_after: Option<i64>,
}

/// The position a read must see, if the caller named one.
#[derive(Debug, Clone, Copy, Default)]
pub struct Consistency(pub Option<LogPosition>);

impl<S: Send + Sync> FromRequestParts<S> for Consistency {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        // A malformed or negative value is ignored rather than refused: it can
        // only make the read *less* consistent, never wrong, and a 400 on a
        // hint is a worse trade than serving what the client would have got
        // without it.
        let query = Query::<ConsistentAfter>::from_request_parts(parts, state).await;
        Ok(Self(
            query
                .ok()
                .and_then(|Query(q)| q.consistent_after)
                .and_then(|n| LogPosition::new(n).ok()),
        ))
    }
}

impl Consistency {
    /// Waits for a projection group to reach the requested position.
    ///
    /// Returns immediately when no position was asked for, which is the common
    /// case — a client reading a list it did not just write to has nothing to
    /// wait for.
    pub async fn wait_for(self, db: &TenantDb, group: &str, locale: Locale) -> Result<(), Problem> {
        let Some(wanted) = self.0 else {
            return Ok(());
        };

        let deadline = tokio::time::Instant::now() + MAX_WAIT;
        loop {
            let reached = {
                let mut conn = db.read().await.map_err(|e| {
                    crate::error::ApiError::Access(e.into()).into_problem(locale, &crate::CATALOG)
                })?;
                erp_projection::checkpoint_of(&mut conn, group)
                    .await
                    .map_err(|e| {
                        crate::error::ApiError::Access(e.into())
                            .into_problem(locale, &crate::CATALOG)
                    })?
            };

            if reached >= wanted {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                // Not stale data with a shrug: the caller asked for a guarantee
                // this response cannot make. 503 with a retry hint, because the
                // projection is behind rather than broken.
                return Err(Problem::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &erp_i18n::Message::new(crate::messages::NOT_CAUGHT_UP).with(
                        "behind",
                        erp_i18n::MessageArg::Int(wanted.get() - reached.get()),
                    ),
                    locale,
                    &crate::CATALOG,
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

/// Asks the worker to visit this tenant now.
///
/// Called after a write. Without it, the first write after a quiet period waits
/// out the tenant's idle backoff — up to thirty seconds — before anything
/// projects it.
///
/// The update is a no-op for a tenant that is already due, so a busy tenant pays
/// one round trip that matches no rows.
///
/// ponytail: one control-plane round trip per write. If write rate ever makes
/// that hot, batch the ids in this process and flush on a timer — the call site
/// does not change.
pub async fn nudge(state: &AppState, tenant: erp_types::TenantId) {
    if let Err(e) = state.control.request_visit(tenant).await {
        // Not fatal: the write is committed, and the tenant is visited on its
        // normal schedule regardless. Only the latency is lost.
        tracing::warn!(%tenant, error = %e, "could not ask for a visit");
    }
}
