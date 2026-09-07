//! What was collected, as a table.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};
use sqlx::PgConnection;

use crate::card::CardEvent;
use crate::payment::PaymentEvent;
use crate::payout::PayoutEvent;

/// This module's projection group.
///
/// One group and one table: everything a person asks about a payment is about
/// **that payment**, so there is nothing here that would need a second group to
/// stay consistent with (L3).
#[derive(Debug)]
pub struct Payments;

impl ProjectionGroup for Payments {
    const NAME: &'static str = "payments";
    const SCHEMA: &'static str = "proj_payments";
}

/// Every projection this module runs.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Payments>>> {
    vec![std::sync::Arc::new(Collected), std::sync::Arc::new(Kept)]
}

/// Cards a customer left behind.
///
/// **No token column, and there must never be one** — the table says so at
/// length, and `card.rs` gives the argument. Every event this reads is display
/// only, so there is nothing here that could grow one by accident.
#[derive(Debug)]
pub struct Kept;

#[async_trait::async_trait]
impl Projection for Kept {
    type Group = Payments;

    fn name(&self) -> &'static str {
        "kept"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !CardEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str().to_owned();
        let position = envelope.position;
        let event: CardEvent = ctx
            .decode(envelope)
            .map_err(|source| ProjectionError::Decode {
                event_name: envelope.event_name.as_str().to_owned(),
                position: envelope.position,
                source,
            })?;

        match event {
            CardEvent::Saved {
                customer,
                provider,
                brand,
                last4,
                expiry_month,
                expiry_year,
                saved_at,
            } => {
                sqlx::query(
                    "INSERT INTO card
                        (id, customer, provider, brand, last4,
                         expiry_month, expiry_year, saved_at, position)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                     ON CONFLICT (id) DO NOTHING",
                )
                .bind(&id)
                .bind(customer.as_str())
                .bind(&provider)
                .bind(&brand)
                .bind(&last4)
                .bind(expiry_month)
                .bind(expiry_year)
                .bind(saved_at)
                .bind(position)
                .execute(&mut *conn)
                .await?;
            }
            // **The row stays.** That a customer had a card and asked for it to
            // go is history; what goes is the sealed token, which is not here.
            CardEvent::Forgotten { forgotten_at } => {
                sqlx::query(
                    "UPDATE card
                        SET forgotten = TRUE, forgotten_at = $2, position = $3
                      WHERE id = $1",
                )
                .bind(&id)
                .bind(forgotten_at)
                .bind(position)
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct Collected;

#[async_trait::async_trait]
impl Projection for Collected {
    type Group = Payments;

    fn name(&self) -> &'static str {
        "collected"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if PayoutEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return self.paid_out(ctx, envelope, conn).await;
        }
        if envelope.event_name.as_str() == PaymentEvent::NAMES[0] {
            return self.requested(ctx, envelope, conn).await;
        }
        if envelope.event_name.as_str() == PaymentEvent::NAMES[1] {
            return self.started(ctx, envelope, conn).await;
        }
        if !PaymentEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str().to_owned();
        let position = envelope.position;
        let event: PaymentEvent =
            ctx.decode(envelope)
                .map_err(|source| ProjectionError::Decode {
                    event_name: envelope.event_name.as_str().to_owned(),
                    position: envelope.position,
                    source,
                })?;

        // The refund requests first, because `Refunded` is both a change to
        // the payment and an answer to a request.
        refund_request(&mut *conn, &id, position, &event).await?;
        stage(&mut *conn, &id, position, event).await
    }
}

/// **The payment's stage follows the event**, in SQL where there is arithmetic,
/// so a replay reaches the same answer as the aggregate did.
async fn stage(
    conn: &mut PgConnection,
    id: &str,
    position: erp_types::LogPosition,
    event: PaymentEvent,
) -> Result<(), ProjectionError> {
    match event {
        // Answered by their own methods, or by `refund_request`.
        PaymentEvent::Requested { .. }
        | PaymentEvent::Started { .. }
        | PaymentEvent::RefundRequested { .. }
        | PaymentEvent::RefundRefused { .. } => {}
        PaymentEvent::Settled {
            amount,
            fee,
            invoice,
            settled_at,
            ..
        } => {
            // **The invoice lands here for a deposit**, because a deposit does
            // not have one until it settles: the prepayment invoice is raised
            // in the same transaction, and this is where the row learns which
            // document its money cleared.
            sqlx::query(
                "UPDATE payment
                    SET stage = 'settled', amount_minor = $2, fee_minor = $3,
                        invoice = $4, settled_at = $5, position = $6
                  WHERE id = $1",
            )
            .bind(id)
            .bind(amount.minor())
            .bind(fee.map(Money::minor))
            .bind(invoice.as_str())
            .bind(settled_at)
            .bind(position)
            .execute(&mut *conn)
            .await?;
        }
        PaymentEvent::Failed { why, .. } => {
            sqlx::query(
                "UPDATE payment SET stage = 'failed', failed_why = $2, position = $3
                  WHERE id = $1",
            )
            .bind(id)
            .bind(&why)
            .bind(position)
            .execute(&mut *conn)
            .await?;
        }
        PaymentEvent::Refunded { amount, .. } => {
            sqlx::query(
                "UPDATE payment
                    SET refunded_minor = refunded_minor + $2,
                        stage = CASE
                            WHEN refunded_minor + $2 >= amount_minor THEN 'refunded'
                            ELSE stage
                        END,
                        position = $3
                  WHERE id = $1",
            )
            .bind(id)
            .bind(amount.minor())
            .bind(position)
            .execute(&mut *conn)
            .await?;
        }
        // **Kept, and there is nothing left to give back.** The amount is
        // recorded as retained because that is what the column means — how
        // much of this payment is no longer available — and the stage is what
        // says where it went.
        PaymentEvent::Retained { amount, .. } => {
            sqlx::query(
                "UPDATE payment
                    SET stage = 'retained', retained_minor = $2, position = $3
                  WHERE id = $1",
            )
            .bind(id)
            .bind(amount.minor())
            .bind(position)
            .execute(&mut *conn)
            .await?;
        }
        PaymentEvent::Voided { .. } => {
            sqlx::query("UPDATE payment SET stage = 'voided', position = $2 WHERE id = $1")
                .bind(id)
                .bind(position)
                .execute(&mut *conn)
                .await?;
        }
    }
    Ok(())
}

/// The `refund_request` row: opened by a request, closed by the gateway's
/// answer. A refund recorded straight from the gateway (a legacy row, or a
/// test) has no request and updates nothing.
async fn refund_request(
    conn: &mut PgConnection,
    id: &str,
    position: erp_types::LogPosition,
    event: &PaymentEvent,
) -> Result<(), ProjectionError> {
    match event {
        PaymentEvent::RefundRequested {
            reference,
            amount,
            reason,
            requested_at,
        } => {
            sqlx::query(
                "INSERT INTO refund_request
                    (payment_id, reference, amount_minor, currency, reason,
                     requested_at, position)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (payment_id, reference) DO NOTHING",
            )
            .bind(id)
            .bind(reference)
            .bind(amount.minor())
            .bind(amount.currency().to_string())
            .bind(reason)
            .bind(*requested_at)
            .bind(position)
            .execute(&mut *conn)
            .await?;
        }
        PaymentEvent::RefundRefused {
            reference,
            why,
            refused_at,
        } => {
            sqlx::query(
                "UPDATE refund_request
                    SET outcome = 'refused', outcome_why = $3, outcome_at = $4, position = $5
                  WHERE payment_id = $1 AND reference = $2",
            )
            .bind(id)
            .bind(reference)
            .bind(why)
            .bind(*refused_at)
            .bind(position)
            .execute(&mut *conn)
            .await?;
        }
        PaymentEvent::Refunded {
            reference,
            refunded_at,
            ..
        } => {
            sqlx::query(
                "UPDATE refund_request
                    SET outcome = 'refunded', outcome_at = $3, position = $4
                  WHERE payment_id = $1 AND reference = $2",
            )
            .bind(id)
            .bind(reference)
            .bind(*refunded_at)
            .bind(position)
            .execute(&mut *conn)
            .await?;
        }
        _ => {}
    }
    Ok(())
}

impl Collected {
    /// A payout: the transfer itself, and the payments it accounts for.
    /// A saved-card charge somebody asked for and the worker has not sent.
    ///
    /// **`gateway_id` is the payment's own id**, before the gateway has been
    /// told anything. That is not a placeholder: it is passed as Moyasar's
    /// `given_id` and *becomes* the gateway's id, so a charge that succeeded
    /// and whose `Started` was never recorded can still be found at the
    /// provider by the id on this row. `Started` overwrites it with whatever
    /// actually came back.
    async fn requested(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        let PaymentEvent::Requested {
            card,
            provider,
            invoice,
            advance,
            amount,
            callback_url,
            checkout,
            requested_at,
        } = ctx
            .decode(envelope)
            .map_err(|source| ProjectionError::Decode {
                event_name: envelope.event_name.as_str().to_owned(),
                position: envelope.position,
                source,
            })?
        else {
            return Ok(());
        };

        // **The checkout travels as JSON**, whole: the worker reads it back
        // to build the charge, and a column per field would be a second
        // schema for a shape the event already owns.
        let checkout = checkout
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| {
                ProjectionError::Rejected(format!("a checkout that will not serialize: {e}"))
            })?;
        sqlx::query(
            "INSERT INTO payment
                (id, provider, gateway_id, invoice, advance_for,
                 advance_net_minor, advance_buyer, advance_buyer_vat,
                 amount_minor, currency, stage, card, callback_url,
                 started_at, position, checkout)
             VALUES ($1, $2, $1, $3, $4, $5, $6, $7, $8, $9, 'requested',
                     $10, $11, $12, $13, $14)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(envelope.stream.id.as_str())
        .bind(&provider)
        .bind(invoice.as_ref().map(AggregateId::as_str))
        .bind(advance.as_ref().map(|a| a.against.as_str()))
        .bind(advance.as_ref().map(|a| a.net.minor()))
        .bind(advance.as_ref().map(|a| a.buyer.name.clone()))
        .bind(advance.as_ref().and_then(|a| a.buyer.vat_number.clone()))
        .bind(amount.minor())
        .bind(amount.currency().to_string())
        .bind(card.as_ref().map(AggregateId::as_str))
        .bind(&callback_url)
        .bind(requested_at)
        .bind(envelope.position)
        .bind(checkout)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// A charge that now exists at the gateway.
    ///
    /// **Upserts**, because this is either the first this system has heard of
    /// the payment — a client that created the charge itself — or the moment a
    /// saved-card charge the worker just sent stops being merely requested.
    async fn started(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        let PaymentEvent::Started {
            provider,
            gateway_id,
            invoice,
            advance,
            amount,
            pay_at,
            started_at,
        } = ctx
            .decode(envelope)
            .map_err(|source| ProjectionError::Decode {
                event_name: envelope.event_name.as_str().to_owned(),
                position: envelope.position,
                source,
            })?
        else {
            return Ok(());
        };

        sqlx::query(
            "INSERT INTO payment
                (id, provider, gateway_id, invoice, advance_for,
                 advance_net_minor, advance_buyer, advance_buyer_vat,
                 amount_minor, currency, stage, started_at, position, pay_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'pending', $11, $12, $13)
             ON CONFLICT (id) DO UPDATE
                SET stage = 'pending',
                    gateway_id = EXCLUDED.gateway_id,
                    pay_at = EXCLUDED.pay_at,
                    started_at = EXCLUDED.started_at,
                    position = EXCLUDED.position
              WHERE payment.stage = 'requested'",
        )
        .bind(envelope.stream.id.as_str())
        .bind(&provider)
        .bind(&gateway_id)
        .bind(invoice.as_ref().map(AggregateId::as_str))
        .bind(advance.as_ref().map(|a| a.against.as_str()))
        .bind(advance.as_ref().map(|a| a.net.minor()))
        .bind(advance.as_ref().map(|a| a.buyer.name.clone()))
        .bind(advance.as_ref().and_then(|a| a.buyer.vat_number.clone()))
        .bind(amount.minor())
        .bind(amount.currency().to_string())
        .bind(started_at)
        .bind(envelope.position)
        .bind(&pay_at)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn paid_out(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        let PayoutEvent::Received {
            provider,
            reference,
            amount,
            expected,
            covers,
            into,
            received_on,
            ..
        } = ctx
            .decode::<PayoutEvent>(envelope)
            .map_err(|source| ProjectionError::Decode {
                event_name: envelope.event_name.as_str().to_owned(),
                position: envelope.position,
                source,
            })?;
        let id = envelope.stream.id.as_str().to_owned();

        sqlx::query(
            "INSERT INTO payout
                (id, provider, reference, amount_minor, expected_minor, currency,
                 covered, into_account, received_on, position)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&id)
        .bind(&provider)
        .bind(&reference)
        .bind(amount.minor())
        .bind(expected.minor())
        .bind(amount.currency().to_string())
        .bind(i32::try_from(covers.len()).unwrap_or(i32::MAX))
        .bind(into.as_str())
        .bind(received_on)
        .bind(envelope.position)
        .execute(&mut *conn)
        .await?;

        // **What the clearing account is no longer holding.** A payment with a
        // payout against it drops out of `payment_awaiting_payout`, which is
        // the list the next reconciliation works from.
        sqlx::query(
            "UPDATE payment SET paid_out_in = $1, position = $2
              WHERE gateway_id = ANY($3) AND provider = $4",
        )
        .bind(&id)
        .bind(envelope.position)
        .bind(&covers)
        .bind(&provider)
        .execute(&mut *conn)
        .await?;

        Ok(())
    }
}

/// One attempt, as somebody reading a screen sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentRow {
    pub id: String,
    pub provider: String,
    pub gateway_id: String,
    /// The invoice, when it collects against one.
    pub invoice: Option<String>,
    /// What a deposit was taken for, when it does not.
    pub advance_for: Option<String>,
    pub amount: Money,
    pub stage: String,
    pub fee: Option<Money>,
    pub refunded: Money,
    pub failed_why: Option<String>,
    /// Where the customer goes to pay, while there is somewhere: the checkout
    /// the worker opened, or a card's 3-D Secure page. `None` until the
    /// gateway has said, and for a charge that needs nobody.
    pub pay_at: Option<String>,
    pub started_at: Timestamp,
    pub settled_at: Option<Timestamp>,
}

/// **The lookup a callback makes.** A gateway names its own id and nothing
/// else, so this is what turns one into a payment this system knows about.
pub async fn by_gateway_id(
    conn: &mut sqlx::PgConnection,
    provider: &str,
    gateway_id: &str,
) -> Result<Option<AggregateId>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!" FROM proj_payments.payment
            WHERE provider = $1 AND gateway_id = $2"#,
        provider,
        gateway_id,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.and_then(|r| AggregateId::new(&r.id).ok()))
}

/// One payment.
pub async fn payment(
    conn: &mut sqlx::PgConnection,
    id: &str,
) -> Result<Option<PaymentRow>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", provider as "provider!", gateway_id as "gateway_id!",
                  invoice, advance_for, amount_minor as "amount_minor!",
                  currency as "currency!", stage as "stage!", fee_minor,
                  refunded_minor as "refunded_minor!", failed_why, pay_at,
                  started_at as "started_at!", settled_at
             FROM proj_payments.payment WHERE id = $1"#,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.and_then(|r| {
        let currency = CurrencyCode::new(&r.currency).ok()?;
        Some(PaymentRow {
            id: r.id,
            provider: r.provider,
            gateway_id: r.gateway_id,
            invoice: r.invoice,
            advance_for: r.advance_for,
            amount: Money::from_minor(r.amount_minor, currency),
            stage: r.stage,
            fee: r.fee_minor.map(|m| Money::from_minor(m, currency)),
            refunded: Money::from_minor(r.refunded_minor, currency),
            failed_why: r.failed_why,
            pay_at: r.pay_at,
            started_at: r.started_at,
            settled_at: r.settled_at,
        })
    }))
}

/// **The deposit held against something**, for the public read beside the
/// deposit route: the one payment a booking is waiting to be paid, and where
/// the customer goes to pay it.
///
/// The newest, because a deposit that failed is released and another may be
/// asked for; `Awaiting` in the log is what stops two being live at once.
pub async fn awaited_for(
    conn: &mut sqlx::PgConnection,
    against: &str,
) -> Result<Option<PaymentRow>, sqlx::Error> {
    let id = sqlx::query_scalar!(
        r#"SELECT id as "id!" FROM proj_payments.payment
            WHERE advance_for = $1
            ORDER BY started_at DESC, position DESC LIMIT 1"#,
        against,
    )
    .fetch_optional(&mut *conn)
    .await?;
    match id {
        Some(id) => payment(conn, &id).await,
        None => Ok(None),
    }
}

/// What has been tried against one thing, newest first.
///
/// **An invoice or a booking.** One column each and one query: a caller asking
/// "what has been collected against this" does not want to know which of the
/// two shapes the answer happens to be, and two reads would make them care.
pub async fn against(
    conn: &mut sqlx::PgConnection,
    invoice: &str,
    limit: i64,
) -> Result<Vec<PaymentRow>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", provider as "provider!", gateway_id as "gateway_id!",
                  invoice, advance_for, amount_minor as "amount_minor!",
                  currency as "currency!", stage as "stage!", fee_minor,
                  refunded_minor as "refunded_minor!", failed_why, pay_at,
                  started_at as "started_at!", settled_at
             FROM proj_payments.payment
            WHERE invoice = $1 OR advance_for = $1
            ORDER BY started_at DESC LIMIT $2"#,
        invoice,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let currency = CurrencyCode::new(&r.currency).ok()?;
            Some(PaymentRow {
                id: r.id,
                provider: r.provider,
                gateway_id: r.gateway_id,
                invoice: r.invoice,
                advance_for: r.advance_for,
                amount: Money::from_minor(r.amount_minor, currency),
                stage: r.stage,
                fee: r.fee_minor.map(|m| Money::from_minor(m, currency)),
                refunded: Money::from_minor(r.refunded_minor, currency),
                failed_why: r.failed_why,
                pay_at: r.pay_at,
                started_at: r.started_at,
                settled_at: r.settled_at,
            })
        })
        .collect())
}

/// What a gateway has settled and not yet paid over.
///
/// **The balance the clearing account should agree with.** A number here that
/// the ledger disagrees with means a payment posted and its payout did not, or
/// the other way round — which is the whole reason both are recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitingPayout {
    pub provider: String,
    /// What the gateway still owes: settled amounts less the fees already
    /// booked against them.
    pub held: Money,
    pub payments: i64,
    /// The oldest one still waiting. What somebody chasing a late payout looks
    /// at first.
    pub since: Option<Timestamp>,
}

/// One transfer, as somebody reconciling sees it.
/// A refund the gateway has been asked for and has not answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitedRefund {
    pub payment: AggregateId,
    pub provider: String,
    /// The gateway's own id for the payment — what the refund is made against.
    pub gateway_id: String,
    pub reference: String,
    pub amount: Money,
    /// Why it was asked for, as the request said — carried onto the refund
    /// the worker records, so the aggregate is not loaded to fetch it (L7).
    pub reason: String,
    /// What this system has recorded as refunded so far, so the pass can tell a
    /// refund that already happened at the gateway from one it has to make.
    pub already_refunded: Money,
}

/// Everything the gateway has been asked to give back and has not yet
/// answered, for one provider, oldest first.
pub async fn awaiting_refunds(
    conn: &mut sqlx::PgConnection,
    provider: &str,
    limit: i64,
) -> Result<Vec<AwaitedRefund>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT r.payment_id as "payment!", p.provider as "provider!",
                  p.gateway_id as "gateway_id!", r.reference as "reference!",
                  r.amount_minor as "amount_minor!", r.currency as "currency!",
                  r.reason as "reason!", p.refunded_minor as "refunded_minor!"
             FROM proj_payments.refund_request r
             JOIN proj_payments.payment p ON p.id = r.payment_id
            WHERE r.outcome IS NULL AND p.provider = $1
            ORDER BY r.requested_at ASC LIMIT $2"#,
        provider,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let currency = CurrencyCode::new(&r.currency).ok()?;
            Some(AwaitedRefund {
                payment: AggregateId::new(&r.payment).ok()?,
                provider: r.provider,
                gateway_id: r.gateway_id,
                reference: r.reference,
                amount: Money::from_minor(r.amount_minor, currency),
                reason: r.reason,
                already_refunded: Money::from_minor(r.refunded_minor, currency),
            })
        })
        .collect())
}

/// One refund of a payment, as a screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundRow {
    pub reference: String,
    pub amount: Money,
    pub reason: String,
    pub requested_at: Timestamp,
    /// `None` while the gateway has not answered; `refunded` or `refused`.
    pub outcome: Option<String>,
    pub outcome_why: Option<String>,
    pub outcome_at: Option<Timestamp>,
}

/// Every refund asked of one payment, newest first.
pub async fn refunds_of(
    conn: &mut sqlx::PgConnection,
    payment: &str,
) -> Result<Vec<RefundRow>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT reference as "reference!", amount_minor as "amount_minor!",
                  currency as "currency!", reason as "reason!",
                  requested_at as "requested_at!", outcome, outcome_why, outcome_at
             FROM proj_payments.refund_request
            WHERE payment_id = $1
            ORDER BY requested_at DESC"#,
        payment,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let currency = CurrencyCode::new(&r.currency).ok()?;
            Some(RefundRow {
                reference: r.reference,
                amount: Money::from_minor(r.amount_minor, currency),
                reason: r.reason,
                requested_at: r.requested_at,
                outcome: r.outcome,
                outcome_why: r.outcome_why,
                outcome_at: r.outcome_at,
            })
        })
        .collect())
}

/// **Every deposit that arrived, and what it was held against.**
///
/// The durable half of telling `booking` its slot is paid for. The settle pass
/// reports what it settled *this* pass, and the worker joins that to
/// `booking::secure_in` at once; but a join that failed once was, in the first
/// version, never retried — the payment was no longer `pending`, so no later
/// pass saw it, and the booking expired as unpaid with the money taken. This
/// is the query the worker repairs from instead: everything ever settled
/// against something, oldest first, for `booking` to say which of them it has
/// not heard about.
///
/// `settled` and `retained`, not `refunded`: a deposit given back in full is not
/// one a booking should be told arrived.
pub async fn settled_advances(
    conn: &mut sqlx::PgConnection,
    limit: i64,
) -> Result<Vec<(AggregateId, AggregateId)>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", advance_for as "against!"
             FROM proj_payments.payment
            WHERE advance_for IS NOT NULL AND stage IN ('settled', 'retained')
            ORDER BY settled_at ASC NULLS LAST, id
            LIMIT $1"#,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            Some((
                AggregateId::new(&r.against).ok()?,
                AggregateId::new(&r.id).ok()?,
            ))
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayoutRow {
    pub id: String,
    pub provider: String,
    pub reference: String,
    pub amount: Money,
    pub expected: Money,
    /// What arrived less what was owed. **Negative is short.**
    pub difference: Money,
    /// How many payments it reconciles against. **Zero reconciles nothing.**
    pub covered: i32,
    pub received_on: Timestamp,
}

/// Per provider: what the gateway still owes.
pub async fn awaiting_payout(
    conn: &mut sqlx::PgConnection,
) -> Result<Vec<AwaitingPayout>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT provider as "provider!", currency as "currency!",
                  -- Cast, because `SUM` of a bigint is NUMERIC in Postgres and
                  -- a money total that needs a decimal library is a money total
                  -- this build will not have.
                  SUM(amount_minor - COALESCE(fee_minor, 0))::BIGINT as "held!",
                  COUNT(*) as "payments!",
                  MIN(settled_at) as "since"
             FROM proj_payments.payment
            WHERE stage = 'settled' AND paid_out_in IS NULL
            GROUP BY provider, currency
            ORDER BY provider"#
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let currency = CurrencyCode::new(&row.currency).ok()?;
            Some(AwaitingPayout {
                provider: row.provider,
                held: Money::from_minor(row.held, currency),
                payments: row.payments,
                since: row.since,
            })
        })
        .collect())
}

/// The payouts recorded, newest first.
pub async fn payouts(
    conn: &mut sqlx::PgConnection,
    limit: i64,
) -> Result<Vec<PayoutRow>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", provider as "provider!", reference as "reference!",
                  amount_minor as "amount_minor!", expected_minor as "expected_minor!",
                  currency as "currency!", covered as "covered!",
                  received_on as "received_on!"
             FROM proj_payments.payout
            ORDER BY received_on DESC LIMIT $1"#,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let currency = CurrencyCode::new(&row.currency).ok()?;
            Some(PayoutRow {
                id: row.id,
                provider: row.provider,
                reference: row.reference,
                amount: Money::from_minor(row.amount_minor, currency),
                expected: Money::from_minor(row.expected_minor, currency),
                difference: Money::from_minor(row.amount_minor - row.expected_minor, currency),
                covered: row.covered,
                received_on: row.received_on,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    /// Every stage the aggregate can reach is one the table's constraint
    /// allows. A stage the projection writes and the schema refuses is a
    /// rebuild that fails halfway.
    #[test]
    fn every_stage_the_aggregate_reaches_is_one_the_table_takes() {
        use crate::payment::Stage;
        let allowed = include_str!("../schema/install.sql");
        for stage in [
            Stage::Pending,
            Stage::Settled,
            Stage::Failed,
            Stage::Refunded,
            Stage::Voided,
        ] {
            assert!(
                allowed.contains(&format!("'{}'", stage.as_str())),
                "the schema does not allow {}",
                stage.as_str()
            );
        }
    }
}

/// One saved card, as somebody picking from a list sees it.
///
/// **There is no token field**, here or anywhere a route can reach. What
/// charges the card is sealed in `module_secret` and is read by exactly one
/// caller, in the worker, at the moment it charges — see `crate::card`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardRow {
    pub id: String,
    pub customer: String,
    pub provider: String,
    pub brand: String,
    pub last4: String,
    pub expiry_month: i16,
    pub expiry_year: i16,
    pub forgotten: bool,
    pub saved_at: Timestamp,
}

/// One card, forgotten or not.
///
/// Includes forgotten ones, because a route asking about a card by id is
/// usually asking *why* it will not work, and "there is no such card" is a
/// worse answer than "that one was removed".
pub async fn card(conn: &mut sqlx::PgConnection, id: &str) -> Result<Option<CardRow>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", customer as "customer!", provider as "provider!",
                  brand as "brand!", last4 as "last4!",
                  expiry_month as "expiry_month!", expiry_year as "expiry_year!",
                  forgotten as "forgotten!", saved_at as "saved_at!"
             FROM proj_payments.card WHERE id = $1"#,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|r| CardRow {
        id: r.id,
        customer: r.customer,
        provider: r.provider,
        brand: r.brand,
        last4: r.last4,
        expiry_month: r.expiry_month,
        expiry_year: r.expiry_year,
        forgotten: r.forgotten,
        saved_at: r.saved_at,
    }))
}

/// What this customer can be offered, newest first.
///
/// **Forgotten cards are not in it.** The list exists to be picked from, and a
/// card that cannot be charged has no business being offered.
pub async fn cards(
    conn: &mut sqlx::PgConnection,
    customer: &str,
    limit: i64,
) -> Result<Vec<CardRow>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", customer as "customer!", provider as "provider!",
                  brand as "brand!", last4 as "last4!",
                  expiry_month as "expiry_month!", expiry_year as "expiry_year!",
                  forgotten as "forgotten!", saved_at as "saved_at!"
             FROM proj_payments.card
            WHERE customer = $1 AND NOT forgotten
            ORDER BY saved_at DESC LIMIT $2"#,
        customer,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| CardRow {
            id: r.id,
            customer: r.customer,
            provider: r.provider,
            brand: r.brand,
            last4: r.last4,
            expiry_month: r.expiry_month,
            expiry_year: r.expiry_year,
            forgotten: r.forgotten,
            saved_at: r.saved_at,
        })
        .collect())
}
