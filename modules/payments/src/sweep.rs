//! Asking the gateway what happened, for everything still waiting.
//!
//! # Why a sweep and not a webhook handler
//!
//! Two reasons, and the second is the real one.
//!
//! **The dispatcher holds no connection.** `EffectHandler::deliver` is handed
//! an effect and nothing else — a documented property, and the reason a slow
//! provider cannot exhaust a tenant's pool. Settling writes to the database, so
//! it cannot happen there. `messaging`'s retired push tokens are in the same
//! position for the same reason.
//!
//! **And a callback is not a reliable trigger.** Moyasar retries six times over
//! about four hours and then *drops the message*. Tamara documents no retry
//! policy at all. A system that only settles when a callback arrives loses the
//! payments whose callbacks did not — quietly, and in the direction of a
//! customer who was charged and an invoice that says they were not.
//!
//! So the callback is a doorbell: it is authenticated, recorded and
//! acknowledged, and then this sweep answers the door. It works whether or not
//! the doorbell rang.
//!
//! # Every answer comes from `fetch`
//!
//! Nothing here reads a callback body. The gateway is asked over an
//! authenticated connection, and [`crate::settle_in`] checks the amount against
//! what was started before it posts anything.
//!
//! # It stops rather than degrading
//!
//! A gateway that is unreachable stops the sweep for that tenant and says so
//! (L6). The payments stay pending and the next tick tries again; marking them
//! anything else would be inventing a fact about somebody's money.

use erp_eventlog::Metadata;
use erp_payments::Gateway;
use erp_types::{AggregateId, Timestamp};

/// What one pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Swept {
    /// Payments that reached an ending — settled, failed or voided.
    pub resolved: usize,
    /// Asked about, and still waiting on the customer or a capture.
    pub still_pending: usize,
    /// Why it stopped early, when it did.
    pub stopped: Option<String>,
}

/// Everything still waiting on one provider, oldest first.
///
/// Oldest first because a payment that has been pending longest is the one
/// somebody is chasing, and because it makes the batch a queue rather than a
/// lottery.
pub async fn pending(
    conn: &mut sqlx::PgConnection,
    provider: &str,
    limit: i64,
) -> Result<Vec<(AggregateId, String)>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", gateway_id as "gateway_id!"
             FROM proj_payments.payment
            WHERE stage = 'pending' AND provider = $1
            ORDER BY started_at ASC LIMIT $2"#,
        provider,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| Some((AggregateId::new(&row.id).ok()?, row.gateway_id)))
        .collect())
}

/// Asks the gateway about everything still waiting, and records what it says.
///
/// Each payment settles in its own transaction: one gateway answering oddly
/// must not roll back the ten before it that were fine.
pub async fn settle_pending(
    db: &erp_tenant::TenantDb,
    gateway: &dyn Gateway,
    now: Timestamp,
    limit: i64,
    metadata: &Metadata,
) -> Result<Swept, Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = db.read().await?;
    let waiting = pending(&mut conn, gateway.provider(), limit).await?;
    drop(conn);

    let mut swept = Swept::default();
    for (id, gateway_id) in waiting {
        let charged = match gateway.fetch(&gateway_id).await {
            Ok(charged) => charged,
            // **The gateway has no record of it.** Permanent, and not a reason
            // to stop: the rest of the batch is unaffected. Left pending and
            // reported, because a payment this system started and the gateway
            // has never heard of is a bug worth somebody seeing.
            Err(erp_payments::GatewayError::NoSuchPayment(_)) => {
                tracing::warn!(
                    tenant = %db.tenant(),
                    provider = gateway.provider(),
                    %gateway_id,
                    "the gateway has no record of a payment this system started"
                );
                swept.still_pending += 1;
                continue;
            }
            // Everything else stops the sweep. An unreachable gateway is not a
            // fact about any of these payments (L6).
            Err(e) => {
                swept.stopped = Some(e.to_string());
                break;
            }
        };

        let mut tx = db.begin().await?;
        match crate::settle_in(&mut tx, &id, &charged, now, metadata).await {
            Ok(committed) => {
                tx.commit().await?;
                if committed.events.is_empty() {
                    swept.still_pending += 1;
                } else {
                    swept.resolved += 1;
                }
            }
            Err(e) => {
                tx.rollback().await?;
                // **Loudly, and then on to the next.** The most likely cause is
                // the amount check refusing — which means the gateway is
                // reporting something other than what was started, and that is
                // exactly the thing somebody has to look at.
                tracing::error!(
                    tenant = %db.tenant(),
                    provider = gateway.provider(),
                    payment = %id,
                    error = %e,
                    "a gateway payment could not be settled"
                );
                swept.still_pending += 1;
            }
        }
    }

    Ok(swept)
}

/// Every provider this tenant has configured, as clients.
///
/// Skips the ones they have not: most tenants have one, and unsealing three
/// secrets to use one would be three reads a tick for nothing.
pub async fn configured(
    db: &erp_tenant::TenantDb,
    sealing: &erp_eventlog::SealingKey,
) -> Result<Vec<Box<dyn Gateway>>, Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = db.acquire().await?;
    let mut clients = Vec::new();
    for provider in crate::PROVIDERS {
        if let Some(credentials) = crate::credentials(&mut conn, sealing, provider).await? {
            clients.push(credentials.client()?);
        }
    }
    Ok(clients)
}

/// Acknowledges a provider's callback, having recorded it.
///
/// # Why this does nothing, and why that is honest
///
/// The API route authenticates the callback, records it in `webhook_event` and
/// promises `webhook.{provider}`. That promise is what makes the recording and
/// the acknowledgement one transaction — a row written whose effect was never
/// promised is a callback nothing will look at again.
///
/// The **work** is [`settle_pending`]'s, because settling writes to the
/// database and a handler is handed no connection. So what is left for this to
/// do is exactly what it does: say the effect was performed, because it was —
/// the callback was received and recorded, which is what it promised.
///
/// Without one, every callback a payment provider ever sends waits in the
/// outbox for a handler that does not exist. That is the dispatcher's
/// documented behaviour for an unclaimed kind, and it is right for a channel
/// that might get a handler later; it is wrong here, where the work is already
/// being done somewhere else.
///
/// **What this does not hide:** a payment stays `pending` until the sweep
/// resolves it, and `payment_pending` is the index somebody chases. A broken
/// sweep shows up there and in the worker's log, not in a silent outbox.
#[derive(Debug)]
pub struct Doorbell {
    kind: erp_types::EffectKind,
    provider: &'static str,
}

#[async_trait::async_trait]
impl erp_eventlog::EffectHandler for Doorbell {
    fn kind(&self) -> erp_types::EffectKind {
        self.kind.clone()
    }

    async fn deliver(
        &self,
        _effect: &erp_eventlog::PendingEffect,
    ) -> Result<(), erp_eventlog::DeliveryError> {
        tracing::debug!(
            provider = self.provider,
            "a payment callback was recorded; the sweep will ask the gateway"
        );
        Ok(())
    }
}

/// One per provider this module knows.
///
/// Registered on the tenant dispatcher by the worker. A provider whose
/// callbacks nothing acknowledges is a provider whose effects accumulate.
#[must_use]
pub fn doorbells() -> Vec<std::sync::Arc<dyn erp_eventlog::EffectHandler>> {
    crate::PROVIDERS
        .iter()
        .filter_map(|provider| {
            Some(std::sync::Arc::new(Doorbell {
                kind: erp_types::EffectKind::new(format!("webhook.{provider}")).ok()?,
                provider,
            })
                as std::sync::Arc<dyn erp_eventlog::EffectHandler>)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::gateways::Credentials;

    /// **Every provider gets a doorbell.** One without is one whose callbacks
    /// pile up in the outbox for a handler that will never exist.
    #[test]
    fn every_provider_has_something_that_acknowledges_its_callbacks() {
        let kinds: Vec<String> = super::doorbells()
            .iter()
            .map(|d| d.kind().to_string())
            .collect();
        assert_eq!(kinds.len(), crate::PROVIDERS.len());
        for provider in crate::PROVIDERS {
            assert!(
                kinds.contains(&format!("webhook.{provider}")),
                "{provider} has no doorbell"
            );
        }
    }

    /// A `Credentials` for every provider the sweep will iterate, so a provider
    /// that can be configured and never swept is not expressible.
    #[test]
    fn every_provider_the_sweep_visits_can_be_configured() {
        for provider in crate::PROVIDERS {
            let credentials = match *provider {
                "moyasar" => Credentials::Moyasar {
                    secret: "sk_test_x".to_owned(),
                },
                "tabby" => Credentials::Tabby {
                    secret: "sk_test_x".to_owned(),
                    merchant_code: "m".to_owned(),
                },
                "tamara" => Credentials::Tamara {
                    token: "t".to_owned(),
                    sandbox: true,
                },
                other => panic!("{other} has no credentials shape"),
            };
            assert_eq!(
                credentials.client().expect("builds").provider(),
                *provider,
                "a client that says it is a different provider would sweep the \
                 wrong payments"
            );
        }
    }
}
