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
//! # The same job also sends what has only been asked for
//!
//! A saved-card charge is an outbound call to a third party, so it is here for
//! the first reason above and not because of the second: a request handler
//! must not hold a database connection while somebody else's server thinks
//! about it. [`charge_requested`] is that pass, and it runs before
//! [`settle_pending`] on each tick so a card charged this minute is settled
//! this minute rather than next.
//!
//! **It asks before it charges.** Every requested payment is `fetch`ed first,
//! and only a gateway that has never heard of it is sent a charge. That costs
//! one extra call per saved-card payment, once, and it buys the one guarantee
//! worth paying for: a pass that died between charging and recording does not
//! charge the customer again. Moyasar's `given_id` is supposed to make the
//! retry safe on its own, and it may well; what a duplicate `given_id` actually
//! answers is not something this build has verified, and a double charge is not
//! the place to find out.
//!
//! # It stops rather than degrading
//!
//! A gateway that is unreachable stops the sweep for that tenant and says so
//! (L6). The payments stay pending and the next tick tries again; marking them
//! anything else would be inventing a fact about somebody's money.

use erp_eventlog::Metadata;
use erp_payments::{Charge, Gateway, GatewayError, Returns, Source};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};

/// What one pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Swept {
    /// Payments that reached an ending — settled, failed or voided.
    pub resolved: usize,
    /// **Deposits that settled on this pass, and what they were held against.**
    ///
    /// Reported rather than acted on, because acting on it means telling
    /// `booking` its slot is paid for and this module may not name `booking` —
    /// `requires` is a hard AND, and depending on it would force a diary on
    /// every shop that takes a card. The worker composes the two; see
    /// `SettleGatewayPayments`.
    pub secured: Vec<(AggregateId, AggregateId)>,
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
                    // A deposit, and whatever it was held against wants telling.
                    if let Some(crate::PaymentEvent::Settled {
                        advance: Some(advance),
                        ..
                    }) = committed.events.first()
                    {
                        swept.secured.push((advance.against.clone(), id.clone()));
                    }
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

/// A saved-card charge that has been asked for and not yet sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    /// This system's id for the payment, which is also what the gateway will
    /// use — it is passed as Moyasar's `given_id`.
    pub id: AggregateId,
    /// The saved card to charge, when there is one.
    ///
    /// **`None` is a deposit waiting for the customer**, created in their own
    /// browser against the publishable key using the id this system already
    /// chose. Nobody here charges it; the only question is whether they have.
    pub card: Option<AggregateId>,
    /// An invoice, or the booking a deposit was taken for.
    pub collects: crate::Collects,
    pub amount: Money,
    pub callback_url: String,
}

/// What one charging pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Attempted {
    /// Sent to the gateway, or found already there. Now `pending`, and the
    /// settle pass is what decides whether anybody actually paid.
    pub started: usize,
    /// The gateway would not take it, or the card had been forgotten. Recorded
    /// as failed with the reason, because a charge nobody can collect must not
    /// sit in a queue looking like work.
    pub refused: usize,
    /// Why it stopped early, when it did.
    pub stopped: Option<String>,
}

/// Everything asked for against one provider and not yet sent, oldest first.
pub async fn requested(
    conn: &mut sqlx::PgConnection,
    provider: &str,
    limit: i64,
) -> Result<Vec<Waiting>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", card, invoice, advance_for,
                  advance_net_minor, advance_buyer, advance_buyer_vat,
                  amount_minor as "amount_minor!", currency as "currency!",
                  callback_url as "callback_url!"
             FROM proj_payments.payment
            WHERE stage = 'requested' AND provider = $1
            ORDER BY started_at ASC LIMIT $2"#,
        provider,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let currency = CurrencyCode::new(&row.currency).ok()?;
            let invoice = row
                .invoice
                .as_deref()
                .and_then(|v| AggregateId::new(v).ok());
            // **Rebuilt, not looked up.** Everything a deposit needs in order
            // to be billed travels with it, so the pass that charges it can
            // hand the whole thing on without loading an aggregate (L7).
            let advance = row.advance_for.as_deref().and_then(|against| {
                Some(crate::Advance {
                    against: AggregateId::new(against).ok()?,
                    net: Money::from_minor(row.advance_net_minor?, currency),
                    buyer: crate::Buyer {
                        name: row.advance_buyer.clone()?,
                        vat_number: row.advance_buyer_vat.clone(),
                    },
                })
            });
            Some(Waiting {
                id: AggregateId::new(&row.id).ok()?,
                card: row.card.as_deref().and_then(|c| AggregateId::new(c).ok()),
                collects: crate::Collects::of(invoice.as_ref(), advance.as_ref())?,
                amount: Money::from_minor(row.amount_minor, currency),
                callback_url: row.callback_url,
            })
        })
        .collect())
}

/// Asks the gateway whether the customer has paid yet.
///
/// # Why this is a poll and not a callback
///
/// The callback is a doorbell — it is authenticated, recorded and acknowledged,
/// and the handler is handed no database connection, so it cannot record
/// anything. And Moyasar drops a webhook after six attempts. A deposit that
/// settled and was never recorded is a slot released out from under somebody
/// who paid for it, so the answer has to come from asking.
///
/// # The gateway not knowing it is the ordinary case
///
/// This system names the payment before the customer pays it — the id is passed
/// as `given_id`, which is what stops anybody attaching a stranger's payment to
/// their own booking. Until the widget creates it, `fetch` says there is no such
/// payment, and that is a customer who has not got round to it rather than
/// anything to warn about. The hold expiring is what eventually answers for
/// them.
pub async fn collect_awaited(
    db: &erp_tenant::TenantDb,
    gateway: &dyn Gateway,
    now: Timestamp,
    limit: i64,
    metadata: &Metadata,
) -> Result<Attempted, Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = db.read().await?;
    let waiting = requested(&mut conn, gateway.provider(), limit).await?;
    drop(conn);

    let mut attempted = Attempted::default();
    for want in waiting {
        // A saved card. `charge_requested` sends those.
        if want.card.is_some() {
            continue;
        }
        let id = want.id;
        let charged = match gateway.fetch(id.as_str()).await {
            Ok(charged) => charged,
            // Not paid yet. Ordinary, and silent — see above.
            Err(GatewayError::NoSuchPayment(_)) => continue,
            Err(e) => {
                attempted.stopped = Some(e.to_string());
                break;
            }
        };

        // **Only that it exists.** Whether anybody paid is `settle_in`'s to
        // decide, on the same tick, where the amount and the currency are
        // checked against what was asked for.
        let mut tx = db.begin().await?;
        crate::start_in(
            &mut tx,
            &id,
            &crate::Attempt {
                provider: gateway.provider().to_owned(),
                gateway_id: charged.id.clone(),
                // **Ignored, and it has to be.** What this collects was settled
                // when the deposit was asked for, and `start_in` keeps the
                // payment's own target over anything a later pass hands it.
                collects: want.collects,
                amount: charged.amount,
            },
            now,
            metadata,
        )
        .await?;
        tx.commit().await?;
        attempted.started += 1;
    }

    Ok(attempted)
}

/// Sends every saved-card charge that has been asked for.
///
/// # It asks before it charges
///
/// See the module docs. Every payment here is `fetch`ed first and charged only
/// when the gateway has never heard of it, so a pass that died between charging
/// and recording does not charge the customer twice.
///
/// # It records only that a charge exists
///
/// Never that it was paid — even though the answer in hand says so. Money is
/// recorded in exactly one place, [`crate::settle_in`], where the amount and
/// the currency are checked against what was asked for; a second path that
/// posts from a `charge` response would be a second place for that check to be
/// got wrong. The settle pass runs immediately after this one and picks it up.
pub async fn charge_requested(
    db: &erp_tenant::TenantDb,
    gateway: &dyn Gateway,
    sealing: &erp_eventlog::SealingKey,
    now: Timestamp,
    limit: i64,
    metadata: &Metadata,
) -> Result<Attempted, Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = db.read().await?;
    let waiting = requested(&mut conn, gateway.provider(), limit).await?;
    drop(conn);

    let mut attempted = Attempted::default();
    for want in waiting {
        // A deposit the customer pays themselves. `collect_awaited` asks about
        // those; there is nothing to charge here.
        let Some(card) = want.card.clone() else {
            continue;
        };
        // **The token is the authority on whether this card can be charged.**
        // It is also the only thing a `forget` actually deletes, so its absence
        // is the answer — and it is checked here rather than at the request,
        // because a card can be forgotten in between whatever was checked then.
        let mut conn = db.acquire().await?;
        let token =
            erp_eventlog::secrets::get(&mut conn, sealing, &crate::card::token_key(&card)).await?;
        drop(conn);

        let Some(token) = token else {
            attempted.refused += 1;
            fail(
                db,
                &want.id,
                "the card was removed before this could be charged",
                now,
                metadata,
            )
            .await?;
            continue;
        };
        let token = String::from_utf8(token).unwrap_or_default();

        let charged = match gateway.fetch(want.id.as_str()).await {
            // Already created on an earlier pass that did not get to record it.
            Ok(charged) => charged,
            Err(GatewayError::NoSuchPayment(_)) => {
                let charge = Charge {
                    reference: want.id.as_str().to_owned(),
                    amount: want.amount,
                    // **Moyasar takes one URL**, and the other two are here
                    // because buy-now-pay-later providers distinguish three
                    // endings. Neither of those can hold a saved card, so
                    // there is nothing to distinguish.
                    returns: Returns {
                        success: want.callback_url.clone(),
                        cancel: want.callback_url.clone(),
                        failure: want.callback_url.clone(),
                    },
                    source: Source::Token { token },
                    description: format!("Saved card · {}", want.collects.invoice(&want.id)),
                    buyer: None,
                    basket: None,
                };
                match gateway.charge(&charge).await {
                    Ok(charged) => charged,
                    // **The gateway refused, and will refuse again.** A dead
                    // token, a declined card, an amount below the floor. Not a
                    // reason to stop the pass, and not a reason to try forever.
                    Err(GatewayError::Refused(why)) => {
                        attempted.refused += 1;
                        fail(db, &want.id, &why, now, metadata).await?;
                        continue;
                    }
                    Err(e) => {
                        attempted.stopped = Some(e.to_string());
                        break;
                    }
                }
            }
            Err(e) => {
                attempted.stopped = Some(e.to_string());
                break;
            }
        };

        let mut tx = db.begin().await?;
        crate::start_in(
            &mut tx,
            &want.id,
            &crate::Attempt {
                provider: gateway.provider().to_owned(),
                gateway_id: charged.id.clone(),
                collects: want.collects.clone(),
                amount: want.amount,
            },
            now,
            metadata,
        )
        .await?;
        tx.commit().await?;
        attempted.started += 1;
    }

    Ok(attempted)
}

/// Records that a requested charge will never happen, and why.
async fn fail(
    db: &erp_tenant::TenantDb,
    id: &AggregateId,
    why: &str,
    now: Timestamp,
    metadata: &Metadata,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing::warn!(tenant = %db.tenant(), payment = %id, why, "a saved-card charge was refused");
    let mut tx = db.begin().await?;
    crate::fail_in(&mut tx, id, why, now, metadata).await?;
    tx.commit().await?;
    Ok(())
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
