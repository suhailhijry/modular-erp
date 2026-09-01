//! What a customer holds, and what the business still owes them.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{CurrencyCode, Cursor, Money, Page, Timestamp};
use sqlx::PgConnection;

use crate::entitlement::EntitlementEvent;
use crate::loyalty::LoyaltyEvent;
use crate::subscription::SubscriptionEvent;

/// Two tables, one group.
///
/// One and not two, because the screen this module exists for shows both at
/// once — *what does this customer have with us* is a package, a course and a
/// gym year on one page — and a group is the unit of consistency (L3).
#[derive(Debug)]
pub struct Prepaid;

impl ProjectionGroup for Prepaid {
    const NAME: &'static str = "prepaid";
    const SCHEMA: &'static str = "proj_prepaid";
}

fn decode<E: serde::de::DeserializeOwned>(
    ctx: &ProjectionCtx<'_>,
    envelope: &Envelope,
) -> Result<E, ProjectionError> {
    ctx.decode(envelope)
        .map_err(|source| ProjectionError::Decode {
            event_name: envelope.event_name.as_str().to_owned(),
            position: envelope.position,
            source,
        })
}

/// Packages, courses and deposits.
#[derive(Debug)]
pub struct Entitlements;

#[async_trait::async_trait]
impl Projection for Entitlements {
    type Group = Prepaid;

    fn name(&self) -> &'static str {
        "entitlements"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !EntitlementEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<EntitlementEvent>(ctx, envelope)? {
            EntitlementEvent::Granted {
                customer,
                what,
                uses,
                value,
                reason,
                against,
                expires_at,
                at,
            } => {
                sqlx::query(
                    "INSERT INTO entitlement
                         (id, customer, what, uses_granted, uses_left,
                          deferred, outstanding, currency, reason, against,
                          expires_at, granted_on, recorded_at, position)
                     VALUES ($1,$2,$3,$4,$4,$5,$5,$6,$7,$8,$9,$10,$11,$12)",
                )
                .bind(id)
                .bind(customer.as_str())
                .bind(&what)
                .bind(uses.map(|n| i32::try_from(n).unwrap_or(i32::MAX)))
                .bind(value.minor())
                .bind(value.currency().as_str())
                .bind(reason.as_str())
                .bind(against.as_ref().map(erp_types::AggregateId::as_str))
                .bind(expires_at)
                .bind(at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            EntitlementEvent::Redeemed { uses, value, .. } => {
                // **Closed the moment nothing is left**, computed here rather
                // than read from a later event: a package's last session is the
                // end of it, and there is no separate fact saying so.
                sqlx::query(
                    "UPDATE entitlement
                        SET uses_left   = GREATEST(COALESCE(uses_left, 0) - $2, 0),
                            outstanding = GREATEST(outstanding - $3, 0),
                            closed = CASE
                                WHEN GREATEST(outstanding - $3, 0) = 0
                                  OR COALESCE(uses_left, 1) - $2 <= 0
                                THEN 'spent' ELSE closed END,
                            recorded_at = $4, position = $5
                      WHERE id = $1",
                )
                .bind(id)
                .bind(i32::try_from(uses).unwrap_or(i32::MAX))
                .bind(value.minor())
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            EntitlementEvent::Expired { .. } => {
                close_entitlement(ctx, conn, id, "expired").await?;
            }
            EntitlementEvent::Revoked { .. } => {
                close_entitlement(ctx, conn, id, "revoked").await?;
            }
        }
        Ok(())
    }
}

async fn close_entitlement(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    how: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE entitlement
            SET outstanding = 0, uses_left = CASE WHEN uses_left IS NULL THEN NULL ELSE 0 END,
                closed = $2, recorded_at = $3, position = $4
          WHERE id = $1",
    )
    .bind(id)
    .bind(how)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await
    .map(|_| ())
}

/// Terms paid for in advance.
#[derive(Debug)]
pub struct Subscriptions;

#[async_trait::async_trait]
impl Projection for Subscriptions {
    type Group = Prepaid;

    fn name(&self) -> &'static str {
        "subscriptions"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !SubscriptionEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<SubscriptionEvent>(ctx, envelope)? {
            SubscriptionEvent::Started {
                customer,
                plan,
                price,
                from,
                until,
                at,
            } => {
                sqlx::query(
                    "INSERT INTO subscription
                         (id, customer, plan, price, recognised, outstanding, currency,
                          starts_at, ends_at, started_on, recorded_at, position)
                     VALUES ($1,$2,$3,$4,0,$4,$5,$6,$7,$8,$9,$10)",
                )
                .bind(id)
                .bind(customer.as_str())
                .bind(&plan)
                .bind(price.minor())
                .bind(price.currency().as_str())
                .bind(from)
                .bind(until)
                .bind(at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            SubscriptionEvent::Recognised { value, .. } => {
                sqlx::query(
                    "UPDATE subscription
                        SET recognised = recognised + $2,
                            outstanding = GREATEST(outstanding - $2, 0),
                            recorded_at = $3, position = $4
                      WHERE id = $1",
                )
                .bind(id)
                .bind(value.minor())
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            SubscriptionEvent::Frozen { at, .. } => {
                sqlx::query(
                    "UPDATE subscription
                        SET frozen_since = $2, recorded_at = $3, position = $4
                      WHERE id = $1",
                )
                .bind(id)
                .bind(at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            SubscriptionEvent::Resumed { until, .. } => {
                // The term moves out by exactly the time the clock was stopped
                // for, which the aggregate has already worked out.
                sqlx::query(
                    "UPDATE subscription
                        SET frozen_since = NULL, ends_at = $2,
                            recorded_at = $3, position = $4
                      WHERE id = $1",
                )
                .bind(id)
                .bind(until)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            SubscriptionEvent::Renewed {
                price, from, until, ..
            } => {
                renewed(ctx, conn, id, price, from, until).await?;
            }
            SubscriptionEvent::Cancelled { why, at } => {
                cancelled(ctx, conn, id, &why, at).await?;
            }
        }
        Ok(())
    }
}

/// A new term resets what has been earned. The old one was recognised in full
/// before this event was written — see `renew_subscription` — so nothing is
/// lost by the reset.
async fn renewed(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    price: Money,
    from: Timestamp,
    until: Timestamp,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE subscription
            SET price = $2, recognised = 0, outstanding = $2,
                starts_at = $3, ends_at = $4, frozen_since = NULL,
                recorded_at = $5, position = $6
          WHERE id = $1",
    )
    .bind(id)
    .bind(price.minor())
    .bind(from)
    .bind(until)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await
    .map(|_| ())
}

async fn cancelled(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    why: &str,
    at: Timestamp,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE subscription
            SET cancelled_at = $2, cancelled_why = $3,
                recorded_at = $4, position = $5
          WHERE id = $1",
    )
    .bind(id)
    .bind(at)
    .bind(none_if_blank(why))
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await
    .map(|_| ())
}

/// Loyalty cards: points, stamps and visits.
#[derive(Debug)]
pub struct Cards;

#[async_trait::async_trait]
impl Projection for Cards {
    type Group = Prepaid;

    fn name(&self) -> &'static str {
        "cards"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !LoyaltyEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<LoyaltyEvent>(ctx, envelope)? {
            LoyaltyEvent::Opened {
                customer,
                mechanic,
                at,
            } => {
                sqlx::query(
                    "INSERT INTO card
                         (id, customer, mechanic, opened_on, recorded_at, position)
                     VALUES ($1,$2,$3,$4,$5,$6)",
                )
                .bind(id)
                .bind(customer.as_str())
                .bind(mechanic.as_str())
                .bind(at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            LoyaltyEvent::Earned {
                count, allocated, ..
            } => {
                // The currency is set by the first earning and never changes;
                // `earn` refuses a scheme that would change it.
                sqlx::query(
                    "UPDATE card
                        SET counts   = counts + $2,
                            lifetime = lifetime + $2,
                            deferred = deferred + $3,
                            currency = COALESCE(currency, $4),
                            recorded_at = $5, position = $6
                      WHERE id = $1",
                )
                .bind(id)
                .bind(i32::try_from(count).unwrap_or(i32::MAX))
                .bind(allocated.minor())
                .bind(allocated.currency().as_str())
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            LoyaltyEvent::Redeemed { count, value, .. } => {
                sqlx::query(
                    "UPDATE card
                        SET counts   = GREATEST(counts - $2, 0),
                            deferred = GREATEST(deferred - $3, 0),
                            recorded_at = $4, position = $5
                      WHERE id = $1",
                )
                .bind(id)
                .bind(i32::try_from(count).unwrap_or(i32::MAX))
                .bind(value.minor())
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            LoyaltyEvent::Expired { .. } => {
                // **The card survives it.** Only the balance goes; `lifetime`
                // is untouched, so a rank is not lost to breakage.
                sqlx::query(
                    "UPDATE card
                        SET counts = 0, deferred = 0, recorded_at = $2, position = $3
                      WHERE id = $1",
                )
                .bind(id)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}

fn none_if_blank(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Every projection this module contributes.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Prepaid>>> {
    vec![
        std::sync::Arc::new(Entitlements),
        std::sync::Arc::new(Subscriptions),
        std::sync::Arc::new(Cards),
    ]
}

// -------------------------------------------------------------------- reads

/// A package, a course or a deposit, as a customer's page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntitlementSummary {
    pub id: String,
    pub customer: String,
    pub what: String,
    pub uses_granted: Option<u32>,
    pub uses_left: Option<u32>,
    pub deferred: Money,
    /// What is still owed to the customer.
    pub outstanding: Money,
    pub reason: String,
    pub against: Option<String>,
    pub expires_at: Option<Timestamp>,
    /// `spent`, `expired`, `revoked`, or absent while it is live.
    pub closed: Option<String>,
    pub granted_on: Timestamp,
}

/// A term, as a customer's page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionSummary {
    pub id: String,
    pub customer: String,
    pub plan: String,
    pub price: Money,
    pub recognised: Money,
    pub outstanding: Money,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
    pub frozen_since: Option<Timestamp>,
    pub cancelled_at: Option<Timestamp>,
    pub cancelled_why: Option<String>,
}

/// What a customer holds, newest first.
///
/// `customer` is optional so the same read serves one person's page and the
/// whole list, which are otherwise identical queries.
pub async fn entitlements(
    conn: &mut PgConnection,
    customer: Option<&str>,
    include_closed: bool,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<EntitlementSummary>, sqlx::Error> {
    let (granted_on, id) = resume(after);
    let rows = sqlx::query!(
        r#"SELECT id as "id!", customer as "customer!", what as "what!",
                  uses_granted, uses_left,
                  deferred as "deferred!", outstanding as "outstanding!",
                  currency as "currency!", reason as "reason!", against,
                  expires_at, closed, granted_on as "granted_on!"
             FROM proj_prepaid.entitlement
            WHERE ($4::text IS NULL OR customer = $4)
              AND ($5 OR closed IS NULL)
              AND ($2::timestamptz IS NULL OR (granted_on, id) < ($2, $3))
            ORDER BY granted_on DESC, id DESC
            LIMIT $1"#,
        limit,
        granted_on,
        id,
        customer,
        include_closed,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter()
            .map(|r| {
                let currency = currency_of(&r.currency);
                EntitlementSummary {
                    id: r.id,
                    customer: r.customer,
                    what: r.what,
                    uses_granted: r.uses_granted.and_then(|n| u32::try_from(n).ok()),
                    uses_left: r.uses_left.and_then(|n| u32::try_from(n).ok()),
                    deferred: Money::from_minor(r.deferred, currency),
                    outstanding: Money::from_minor(r.outstanding, currency),
                    reason: r.reason,
                    against: r.against,
                    expires_at: r.expires_at,
                    closed: r.closed,
                    granted_on: r.granted_on,
                }
            })
            .collect(),
        limit,
        |e| Cursor::over(&[&e.granted_on.to_rfc3339(), &e.id]),
    ))
}

/// One of them.
pub async fn entitlement(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<EntitlementSummary>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", customer as "customer!", what as "what!",
                  uses_granted, uses_left,
                  deferred as "deferred!", outstanding as "outstanding!",
                  currency as "currency!", reason as "reason!", against,
                  expires_at, closed, granted_on as "granted_on!"
             FROM proj_prepaid.entitlement WHERE id = $1"#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|r| {
        let currency = currency_of(&r.currency);
        EntitlementSummary {
            id: r.id,
            customer: r.customer,
            what: r.what,
            uses_granted: r.uses_granted.and_then(|n| u32::try_from(n).ok()),
            uses_left: r.uses_left.and_then(|n| u32::try_from(n).ok()),
            deferred: Money::from_minor(r.deferred, currency),
            outstanding: Money::from_minor(r.outstanding, currency),
            reason: r.reason,
            against: r.against,
            expires_at: r.expires_at,
            closed: r.closed,
            granted_on: r.granted_on,
        }
    }))
}

/// Terms, newest first.
pub async fn subscriptions(
    conn: &mut PgConnection,
    customer: Option<&str>,
    include_ended: bool,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<SubscriptionSummary>, sqlx::Error> {
    let (starts_at, id) = resume(after);
    let rows = sqlx::query!(
        r#"SELECT id as "id!", customer as "customer!", plan as "plan!",
                  price as "price!", recognised as "recognised!",
                  outstanding as "outstanding!", currency as "currency!",
                  starts_at as "starts_at!", ends_at as "ends_at!",
                  frozen_since, cancelled_at, cancelled_why
             FROM proj_prepaid.subscription
            WHERE ($4::text IS NULL OR customer = $4)
              AND ($5 OR cancelled_at IS NULL)
              AND ($2::timestamptz IS NULL OR (starts_at, id) < ($2, $3))
            ORDER BY starts_at DESC, id DESC
            LIMIT $1"#,
        limit,
        starts_at,
        id,
        customer,
        include_ended,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter()
            .map(|r| {
                let currency = currency_of(&r.currency);
                SubscriptionSummary {
                    id: r.id,
                    customer: r.customer,
                    plan: r.plan,
                    price: Money::from_minor(r.price, currency),
                    recognised: Money::from_minor(r.recognised, currency),
                    outstanding: Money::from_minor(r.outstanding, currency),
                    starts_at: r.starts_at,
                    ends_at: r.ends_at,
                    frozen_since: r.frozen_since,
                    cancelled_at: r.cancelled_at,
                    cancelled_why: r.cancelled_why,
                }
            })
            .collect(),
        limit,
        |s| Cursor::over(&[&s.starts_at.to_rfc3339(), &s.id]),
    ))
}

/// One of them.
pub async fn subscription(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<SubscriptionSummary>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", customer as "customer!", plan as "plan!",
                  price as "price!", recognised as "recognised!",
                  outstanding as "outstanding!", currency as "currency!",
                  starts_at as "starts_at!", ends_at as "ends_at!",
                  frozen_since, cancelled_at, cancelled_why
             FROM proj_prepaid.subscription WHERE id = $1"#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(rows.map(|r| {
        let currency = currency_of(&r.currency);
        SubscriptionSummary {
            id: r.id,
            customer: r.customer,
            plan: r.plan,
            price: Money::from_minor(r.price, currency),
            recognised: Money::from_minor(r.recognised, currency),
            outstanding: Money::from_minor(r.outstanding, currency),
            starts_at: r.starts_at,
            ends_at: r.ends_at,
            frozen_since: r.frozen_since,
            cancelled_at: r.cancelled_at,
            cancelled_why: r.cancelled_why,
        }
    }))
}

/// A loyalty card, as a customer's page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSummary {
    pub id: String,
    pub customer: String,
    /// `points`, `stamps` or `visits`.
    pub mechanic: String,
    /// Counts redeemable now.
    pub counts: u32,
    /// Every count ever earned. **Never decreases** — it is what a rank is read
    /// from, and spending points does not cost a rank.
    pub lifetime: u32,
    /// What is still owed against the counts. Absent on a card that has never
    /// earned, which has no currency to say it in.
    pub deferred: Option<Money>,
    pub opened_on: Timestamp,
}

/// The cards a customer holds, newest first.
///
/// `customer` is optional so the same read serves one person's page and the
/// whole list.
pub async fn cards(
    conn: &mut PgConnection,
    customer: Option<&str>,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<CardSummary>, sqlx::Error> {
    let (opened_on, id) = resume(after);
    let rows = sqlx::query!(
        r#"SELECT id as "id!", customer as "customer!", mechanic as "mechanic!",
                  counts as "counts!", lifetime as "lifetime!",
                  deferred as "deferred!", currency, opened_on as "opened_on!"
             FROM proj_prepaid.card
            WHERE ($4::text IS NULL OR customer = $4)
              AND ($2::timestamptz IS NULL OR (opened_on, id) < ($2, $3))
            ORDER BY opened_on DESC, id DESC
            LIMIT $1"#,
        limit,
        opened_on,
        id,
        customer,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter()
            .map(|r| CardSummary {
                id: r.id,
                customer: r.customer,
                mechanic: r.mechanic,
                counts: u32::try_from(r.counts).unwrap_or(0),
                lifetime: u32::try_from(r.lifetime).unwrap_or(0),
                deferred: r
                    .currency
                    .map(|code: String| Money::from_minor(r.deferred, currency_of(&code))),
                opened_on: r.opened_on,
            })
            .collect(),
        limit,
        |c| Cursor::over(&[&c.opened_on.to_rfc3339(), &c.id]),
    ))
}

/// One of them.
pub async fn card(conn: &mut PgConnection, id: &str) -> Result<Option<CardSummary>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", customer as "customer!", mechanic as "mechanic!",
                  counts as "counts!", lifetime as "lifetime!",
                  deferred as "deferred!", currency, opened_on as "opened_on!"
             FROM proj_prepaid.card WHERE id = $1"#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|r| CardSummary {
        id: r.id,
        customer: r.customer,
        mechanic: r.mechanic,
        counts: u32::try_from(r.counts).unwrap_or(0),
        lifetime: u32::try_from(r.lifetime).unwrap_or(0),
        deferred: r
            .currency
            .map(|code: String| Money::from_minor(r.deferred, currency_of(&code))),
        opened_on: r.opened_on,
    }))
}

/// **The number the ledger's deferred revenue account has to agree with.**
///
/// Every unredeemed entitlement, every unearned subscription month and every
/// unhonoured loyalty count, per currency, in a stable order.
///
/// # Why this returns a number instead of checking it
///
/// The comparison needs the ledger's account balance, and that lives in
/// `proj_ledger` — a different projection group, which L3 forbids this module
/// from reading. It is the same reason `crm` cannot show a customer's invoices.
///
/// So this is one half of a canary and not the whole one. The half that
/// compares belongs to something that declares both groups: a report module, or
/// a test. `a_liability_agrees_with_the_ledger` in `tests/prepaid.rs` is that
/// test today, and it is the same class of check as `ledger::imbalances` —
/// if the two disagree the pipeline is broken, not the arithmetic.
pub async fn outstanding(conn: &mut PgConnection) -> Result<Vec<Money>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT currency as "currency!", SUM(owed)::BIGINT as "owed!"
             FROM (
                 SELECT currency, outstanding AS owed
                   FROM proj_prepaid.entitlement WHERE closed IS NULL
                 UNION ALL
                 SELECT currency, outstanding
                   FROM proj_prepaid.subscription
                 UNION ALL
                 -- Cards that have never earned have no currency and no
                 -- liability; every other row is one of both.
                 SELECT currency, deferred
                   FROM proj_prepaid.card WHERE currency IS NOT NULL
             ) AS held
            GROUP BY currency
            ORDER BY currency"#
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| Money::from_minor(r.owed, currency_of(&r.currency)))
        .collect())
}

fn resume(after: Option<&Cursor>) -> (Option<Timestamp>, String) {
    match after.map(Cursor::parts) {
        Some(parts) if parts.len() == 2 => (parts[0].parse().ok(), parts[1].clone()),
        _ => (None, String::new()),
    }
}

/// A currency code that was written by this module and is therefore valid.
///
/// Falls back to the tenant's own rather than failing the read: a row with an
/// unreadable currency is a corrupt row, and the number beside it is still
/// worth showing while somebody works out how it got there.
fn currency_of(code: &str) -> CurrencyCode {
    CurrencyCode::new(code).unwrap_or_else(|_| {
        CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!("SAR is a real code"))
    })
}
