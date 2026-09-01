//! What a caller can ask `prepaid` to do.
//!
//! # Every command writes an event and a journal entry, together
//!
//! The same reason `sales` does: a liability that exists in one place and not
//! the other is a state nobody could explain and nothing would clean up. So
//! none of these use `TenantDb::execute`, which runs exactly one aggregate.
//!
//! # The posting is derived from what was recorded, not from what was asked
//!
//! Each command reads `Committed::events` back and posts for what is actually
//! in them. A decision that recorded nothing posts nothing, so a retried
//! command is silent all the way down to the ledger — which is the property
//! that makes a month-end recognition job safe to run twice.

use erp_eventlog::{
    Aggregate, Committed, Decision, ExecuteError, Loaded, MAX_ATTEMPTS, Metadata, try_create,
    try_execute,
};
use erp_tenant::{CommandError, TenantDb};
use erp_types::{AggregateId, DomainName, Money, StreamId, Timestamp};

use crate::entitlement::{Balance, Entitlement, EntitlementEvent, Reason};
use crate::loyalty::{Loyalty, LoyaltyEvent, Mechanic, Scheme, allocate};
use crate::posting::{PostingAccounts, entry_for_deferral, entry_for_release};
use crate::subscription::{Subscription, SubscriptionEvent};

#[derive(Debug, thiserror::Error)]
pub enum PrepaidError {
    #[error("there is no customer {0}")]
    NoSuchCustomer(String),
    #[error("there is no package or deposit {0}")]
    NoSuchEntitlement(String),
    #[error("{0} is finished")]
    NotLive(String),
    #[error("{id} expired on {on}")]
    Lapsed { id: String, on: Timestamp },
    #[error("only {left} is left on {id}, and {wanted} was asked for")]
    NothingLeft {
        id: String,
        left: String,
        wanted: String,
    },
    #[error("an amount here must be more than nothing")]
    NotAValue,
    #[error("nobody paid for this, so it carries no value")]
    FreeGrantWithValue,
    #[error("an amount must either count uses or name what it is held against")]
    OpenValue,
    #[error("there is no card {0}")]
    NoSuchCard(String),
    #[error("no loyalty scheme has been configured")]
    NoScheme,
    #[error("card {0} holds a balance in another currency than the scheme")]
    WrongCurrency(String),
    #[error("there is no subscription {0}")]
    NoSuchSubscription(String),
    #[error("a term must end after it starts")]
    NotATerm,
    #[error("subscription {0} is already frozen")]
    AlreadyFrozen(String),
    #[error("subscription {0} is not frozen")]
    NotFrozen(String),
    #[error("subscription {0} has been cancelled")]
    Cancelled(String),
    #[error("the current term of {id} runs until {until}")]
    TermNotOver { id: String, until: Timestamp },
    #[error("{0} cannot be used as a reference")]
    InvalidReference(String),
    #[error(transparent)]
    Money(#[from] erp_types::MoneyError),
    #[error(transparent)]
    Unbalanced(#[from] ledger::Unbalanced),
    #[error(transparent)]
    Config(#[from] erp_eventlog::ConfigError),
    /// The ledger refused the posting — a missing or closed account, or a
    /// closed period. Passed through rather than reworded: its message names
    /// the account, and that is what the person fixing it needs.
    #[error(transparent)]
    Ledger(#[from] ledger::LedgerError),
}

impl erp_i18n::Localize for PrepaidError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NoSuchCustomer(id) => {
                Message::new(messages::NO_SUCH_CUSTOMER).with("customer", MessageArg::text(id))
            }
            Self::NoSuchEntitlement(id) => {
                Message::new(messages::NO_SUCH_ENTITLEMENT).with("id", MessageArg::text(id))
            }
            Self::NotLive(id) => Message::new(messages::NOT_LIVE).with("id", MessageArg::text(id)),
            Self::Lapsed { id, on } => Message::new(messages::LAPSED)
                .with("id", MessageArg::text(id))
                .with("on", MessageArg::text(on.to_rfc3339())),
            Self::NothingLeft { id, left, wanted } => Message::new(messages::NOTHING_LEFT)
                .with("id", MessageArg::text(id))
                .with("left", MessageArg::text(left))
                .with("wanted", MessageArg::text(wanted)),
            Self::NotAValue => Message::new(messages::NOT_A_VALUE),
            Self::FreeGrantWithValue => Message::new(messages::FREE_GRANT_WITH_VALUE),
            Self::OpenValue => Message::new(messages::OPEN_VALUE),
            Self::NoSuchCard(id) => {
                Message::new(messages::NO_SUCH_CARD).with("id", MessageArg::text(id))
            }
            Self::NoScheme => Message::new(messages::NO_SCHEME),
            Self::WrongCurrency(id) => {
                Message::new(messages::WRONG_CURRENCY).with("id", MessageArg::text(id))
            }
            Self::NoSuchSubscription(id) => {
                Message::new(messages::NO_SUCH_SUBSCRIPTION).with("id", MessageArg::text(id))
            }
            Self::NotATerm => Message::new(messages::NOT_A_TERM),
            Self::AlreadyFrozen(id) => {
                Message::new(messages::ALREADY_FROZEN).with("id", MessageArg::text(id))
            }
            Self::NotFrozen(id) => {
                Message::new(messages::NOT_FROZEN).with("id", MessageArg::text(id))
            }
            Self::Cancelled(id) => {
                Message::new(messages::CANCELLED).with("id", MessageArg::text(id))
            }
            Self::TermNotOver { id, until } => Message::new(messages::TERM_NOT_OVER)
                .with("id", MessageArg::text(id))
                .with("until", MessageArg::text(until.to_rfc3339())),
            Self::InvalidReference(r) => {
                Message::new(messages::NO_SUCH_ENTITLEMENT).with("id", MessageArg::text(r))
            }
            Self::Money(_) => Message::new(messages::AMOUNT_OUT_OF_RANGE),
            // Each already says the right thing in both languages.
            Self::Unbalanced(e) => e.message(),
            Self::Config(e) => e.message(),
            Self::Ledger(e) => e.message(),
        }
    }
}

type Refusal = CommandError<PrepaidError>;
type Outcome<E> = Result<Committed<E>, Refusal>;

/// Commits, rolls back and retries — the one place that decides which.
///
/// The loop is written out at each command for the reason `booking` writes it
/// out: a generic `AsyncFn` helper reads better and does not compile, because
/// axum needs a handler's future to be `Send` and there is no stable way to say
/// that about the future an async closure returns.
async fn settle<T>(
    tx: erp_tenant::Tx,
    outcome: Result<T, ExecuteError<PrepaidError>>,
) -> Result<Option<T>, Refusal> {
    match outcome {
        Ok(done) => {
            tx.commit().await.map_err(ExecuteError::from)?;
            Ok(Some(done))
        }
        Err(e) if e.is_conflict() => {
            tx.rollback().await.map_err(ExecuteError::from)?;
            Ok(None)
        }
        Err(e) => {
            tx.rollback().await.map_err(ExecuteError::from)?;
            Err(e.into())
        }
    }
}

fn contended<T>(stream: &AggregateId, domain: DomainName) -> Result<T, Refusal> {
    Err(CommandError::Execute(ExecuteError::Contended {
        stream: StreamId::new(domain, stream.clone()),
        attempts: MAX_ATTEMPTS,
    }))
}

fn rejected(error: PrepaidError) -> Refusal {
    CommandError::Execute(ExecuteError::Rejected(error))
}

fn derived(prefix: &str, parts: &[&str]) -> Result<AggregateId, PrepaidError> {
    let joined = format!("{prefix}.{}", parts.join("."));
    AggregateId::new(&joined).map_err(|_| PrepaidError::InvalidReference(parts.join(".")))
}

// ------------------------------------------------------------- entitlements

/// Everything a package, a course or a deposit needs.
#[derive(Debug, Clone)]
pub struct Grant {
    pub customer: AggregateId,
    /// What it is for, in the business's own words.
    pub what: String,
    /// Uses granted. `None` for a deposit, which is an amount and not a count.
    pub uses: Option<u32>,
    /// **What was deferred**, excluding tax, and zero when nobody paid.
    pub value: Money,
    pub reason: Reason,
    pub against: Option<AggregateId>,
    pub expires_at: Option<Timestamp>,
    pub at: Timestamp,
}

/// Records something bought now and delivered later, and defers its value.
///
/// A second, *different* grant under a taken id is refused rather than ignored —
/// an entitlement is a balance somebody holds, and quietly returning success
/// would lose whichever one arrived second. A retry of the same request is not:
/// `erp_eventlog::try_create` tells them apart, and this module does not.
pub async fn grant(
    db: &TenantDb,
    id: &AggregateId,
    grant: &Grant,
    metadata: &Metadata,
) -> Outcome<EntitlementEvent> {
    if grant.value.is_negative() {
        return Err(rejected(PrepaidError::NotAValue));
    }
    // **A coupon is not a liability.** No consideration was received, so there
    // is nothing to defer — and a caller who put a number on one has misread
    // what they are recording.
    if !grant.reason.was_paid_for() && !grant.value.is_zero() {
        return Err(rejected(PrepaidError::FreeGrantWithValue));
    }
    if grant.reason.was_paid_for() && !grant.value.is_positive() {
        return Err(rejected(PrepaidError::NotAValue));
    }
    // **A multi-purpose voucher is refused, and this is the guard.** An amount
    // with no uses and nothing to be held against is a card spendable on
    // anything: what it buys is not known when it is sold, so neither is the
    // rate it should have been taxed at. A package counts uses of a named
    // service and a deposit names the booking it secures; both settle that
    // question at the sale. Nothing else here does, so nothing else is allowed.
    if grant.uses.is_none() && grant.against.is_none() {
        return Err(rejected(PrepaidError::OpenValue));
    }
    let entry = derived("pdg", &[id.as_str()]).map_err(rejected)?;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            check_customer(&mut *conn, &grant.customer).await?;

            let committed = try_create::<Entitlement, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |_loaded: &Loaded<Entitlement>| {
                    Ok(Decision::one(EntitlementEvent::Granted {
                        customer: grant.customer.clone(),
                        what: grant.what.clone(),
                        uses: grant.uses,
                        value: grant.value,
                        reason: grant.reason,
                        against: grant.against.clone(),
                        expires_at: grant.expires_at,
                        at: grant.at,
                    }))
                },
            )
            .await?;

            if committed.at.is_some() && grant.value.is_positive() {
                let accounts = accounts(&mut *conn).await?;
                let lines = entry_for_deferral(grant.value, &accounts)
                    .map_err(|e| ExecuteError::Rejected(PrepaidError::Unbalanced(e)))?;
                post(
                    &mut *conn,
                    &entry,
                    grant.at,
                    &format!("Deferred · {id} · {}", grant.what),
                    &lines,
                    metadata,
                )
                .await?;
            }
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Entitlement::domain())
}

/// Money or uses coming off an entitlement.
#[derive(Debug, Clone)]
pub struct Redemption {
    /// The caller's key. Redeeming the same one twice is a no-op, which is what
    /// makes a retried handler harmless (L8).
    pub reference: String,
    /// How many uses. One for a session; ignored on an entitlement that is only
    /// an amount, which is drawn once for whatever it covers.
    pub uses: u32,
    pub at: Timestamp,
}

/// Draws an entitlement down and recognises what that delivered.
pub async fn redeem(
    db: &TenantDb,
    id: &AggregateId,
    redemption: &Redemption,
    metadata: &Metadata,
) -> Outcome<EntitlementEvent> {
    let entry = derived("pdr", &[id.as_str(), &redemption.reference]).map_err(rejected)?;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let committed = try_execute::<Entitlement, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Entitlement>| {
                    let held = &loaded.aggregate;
                    if !held.exists() {
                        return Err(PrepaidError::NoSuchEntitlement(id.to_string()));
                    }
                    if held.has_redemption(&redemption.reference) {
                        return Ok(Decision::nothing());
                    }
                    if !held.is_live() {
                        return Err(PrepaidError::NotLive(id.to_string()));
                    }
                    // Checked against the moment of redemption, not a clock, so
                    // a replay reproduces what was decided.
                    if held.has_lapsed(redemption.at) {
                        return Err(PrepaidError::Lapsed {
                            id: id.to_string(),
                            on: held.expires_at.unwrap_or(redemption.at),
                        });
                    }
                    let balance = held
                        .balance
                        .ok_or_else(|| PrepaidError::NoSuchEntitlement(id.to_string()))?;
                    let (uses, value) = draw(balance, redemption.uses, id)?;
                    Ok(Decision::one(EntitlementEvent::Redeemed {
                        reference: redemption.reference.clone(),
                        uses,
                        value,
                        at: redemption.at,
                    }))
                },
            )
            .await?;

            release_for(
                &mut *conn,
                &entry,
                redemption.at,
                &format!("Delivered · {id}"),
                released(&committed.events),
                metadata,
            )
            .await?;
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Entitlement::domain())
}

/// Writes off what was never used, and recognises it.
///
/// **Breakage is revenue.** The obligation to deliver has gone, so what was
/// held against it is earned. IFRS 15 allows recognising it in proportion to
/// redemption; this recognises it at expiry, which is the simpler treatment and
/// the one a business that has just watched a package lapse expects.
///
/// `at` is when it lapsed, not when somebody noticed.
pub async fn expire(
    db: &TenantDb,
    id: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<EntitlementEvent> {
    close(db, id, at, metadata, "Breakage", |held| {
        if held.has_lapsed(at) {
            held.outstanding()
                .map(|value| EntitlementEvent::Expired { value, at })
        } else {
            None
        }
    })
    .await
}

/// Takes an entitlement back, and reverses the deferral.
///
/// **Nothing is recognised**, because nothing was delivered. The refund itself
/// is a credit note in `sales`, which takes the revenue back down; this entry
/// only clears the liability so the two do not both sit on the books.
pub async fn revoke(
    db: &TenantDb,
    id: &AggregateId,
    why: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<EntitlementEvent> {
    let why = why.to_owned();
    close(db, id, at, metadata, "Revoked", move |held| {
        held.outstanding().map(|value| EntitlementEvent::Revoked {
            why: why.clone(),
            value,
            at,
        })
    })
    .await
}

/// The shared half of `expire` and `revoke`: end it, and release what is left.
async fn close<F>(
    db: &TenantDb,
    id: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
    memo: &str,
    ending: F,
) -> Outcome<EntitlementEvent>
where
    F: Fn(&Entitlement) -> Option<EntitlementEvent> + Send + Sync,
{
    let entry = derived("pdc", &[id.as_str(), &at.timestamp().to_string()]).map_err(rejected)?;
    let ending = &ending;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let committed = try_execute::<Entitlement, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Entitlement>| {
                    let held = &loaded.aggregate;
                    if !held.exists() {
                        return Err(PrepaidError::NoSuchEntitlement(id.to_string()));
                    }
                    if !held.is_live() {
                        return Ok(Decision::nothing());
                    }
                    Ok(ending(held).map_or_else(Decision::nothing, Decision::one))
                },
            )
            .await?;

            release_for(
                &mut *conn,
                &entry,
                at,
                &format!("{memo} · {id}"),
                released(&committed.events),
                metadata,
            )
            .await?;
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Entitlement::domain())
}

/// What one redemption takes, and what it is worth.
fn draw(balance: Balance, wanted: u32, id: &AggregateId) -> Result<(u32, Money), PrepaidError> {
    match balance.uses {
        Some(left) => {
            let wanted = wanted.max(1);
            if left < wanted {
                return Err(PrepaidError::NothingLeft {
                    id: id.to_string(),
                    left: left.to_string(),
                    wanted: wanted.to_string(),
                });
            }
            // Each use is worth what is left divided by what is left to use, so
            // the last one takes the remainder and nothing is stranded.
            let mut value = Money::zero(balance.value.currency());
            let mut running = balance;
            for _ in 0..wanted {
                let one = running.worth_of_one_use()?;
                value = value.checked_add(one)?;
                running = Balance {
                    uses: running.uses.map(|u| u.saturating_sub(1)),
                    value: running.value.checked_sub(one)?,
                };
            }
            Ok((wanted, value))
        }
        // An amount, drawn once for whatever it covers.
        None => Ok((0, balance.value)),
    }
}

/// What a set of entitlement events released to revenue.
fn released(events: &[EntitlementEvent]) -> Option<Money> {
    events.iter().fold(None, |total, event| {
        let value = match event {
            EntitlementEvent::Redeemed { value, .. }
            | EntitlementEvent::Expired { value, .. }
            | EntitlementEvent::Revoked { value, .. } => *value,
            EntitlementEvent::Granted { .. } => return total,
        };
        Some(total.map_or(value, |running: Money| {
            running.checked_add(value).unwrap_or(running)
        }))
    })
}

// ------------------------------------------------------------ subscriptions

/// Everything a term needs.
#[derive(Debug, Clone)]
pub struct Term {
    pub customer: AggregateId,
    pub plan: String,
    /// **What was deferred**, excluding tax, for this term.
    pub price: Money,
    pub from: Timestamp,
    /// Exclusive.
    pub until: Timestamp,
    pub at: Timestamp,
}

/// Starts a subscription and defers its price.
pub async fn start_subscription(
    db: &TenantDb,
    id: &AggregateId,
    term: &Term,
    metadata: &Metadata,
) -> Outcome<SubscriptionEvent> {
    if term.until <= term.from {
        return Err(rejected(PrepaidError::NotATerm));
    }
    if !term.price.is_positive() {
        return Err(rejected(PrepaidError::NotAValue));
    }
    let entry = derived("pds", &[id.as_str()]).map_err(rejected)?;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            check_customer(&mut *conn, &term.customer).await?;

            let committed = try_create::<Subscription, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |_loaded: &Loaded<Subscription>| {
                    Ok(Decision::one(SubscriptionEvent::Started {
                        customer: term.customer.clone(),
                        plan: term.plan.clone(),
                        price: term.price,
                        from: term.from,
                        until: term.until,
                        at: term.at,
                    }))
                },
            )
            .await?;

            if committed.at.is_some() {
                let accounts = accounts(&mut *conn).await?;
                let lines = entry_for_deferral(term.price, &accounts)
                    .map_err(|e| ExecuteError::Rejected(PrepaidError::Unbalanced(e)))?;
                post(
                    &mut *conn,
                    &entry,
                    term.at,
                    &format!("Deferred · {id} · {}", term.plan),
                    &lines,
                    metadata,
                )
                .await?;
            }
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Subscription::domain())
}

/// Earns whatever time has passed, up to a moment.
///
/// **Idempotent by construction.** It computes what should have been recognised
/// in total and posts the difference, so running a month-end job twice posts
/// nothing the second time. See `crate::subscription`.
pub async fn recognise_through(
    db: &TenantDb,
    id: &AggregateId,
    through: Timestamp,
    metadata: &Metadata,
) -> Outcome<SubscriptionEvent> {
    let entry =
        derived("pdt", &[id.as_str(), &through.timestamp().to_string()]).map_err(rejected)?;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let committed = try_execute::<Subscription, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Subscription>| {
                    catch_up(&loaded.aggregate, id, through)
                        .map(|event| event.map_or_else(Decision::nothing, Decision::one))
                },
            )
            .await?;

            release_for(
                &mut *conn,
                &entry,
                through,
                &format!("Earned · {id}"),
                earned(&committed.events),
                metadata,
            )
            .await?;
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Subscription::domain())
}

/// Stops the clock, after earning everything up to that moment.
///
/// **A freeze is not a pause on the calendar, it is a pause on the earning.**
/// A member who freezes in June has not had June, so June is not revenue, and
/// the term moves out by however long they were away. Rekaz's own copy concedes
/// that freeze rules are policy-dependent, which is why how *long* a freeze may
/// run is not decided here.
pub async fn freeze(
    db: &TenantDb,
    id: &AggregateId,
    why: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<SubscriptionEvent> {
    let why = why.to_owned();
    step(db, id, at, metadata, "Frozen", move |held| {
        if held.is_frozen() {
            return Err(PrepaidError::AlreadyFrozen(id.to_string()));
        }
        let mut events = Vec::new();
        if let Some(caught) = catch_up(held, id, at)? {
            events.push(caught);
        }
        events.push(SubscriptionEvent::Frozen {
            why: why.clone(),
            at,
        });
        Ok(events)
    })
    .await
}

/// Starts the clock, and pushes the term out by exactly the time it was stopped.
pub async fn resume(
    db: &TenantDb,
    id: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<SubscriptionEvent> {
    step(db, id, at, metadata, "Resumed", move |held| {
        let since = held
            .frozen_since
            .ok_or_else(|| PrepaidError::NotFrozen(id.to_string()))?;
        let until = held
            .until
            .ok_or_else(|| PrepaidError::NoSuchSubscription(id.to_string()))?;
        let stopped = (at - since).max(chrono::TimeDelta::zero());
        Ok(vec![SubscriptionEvent::Resumed {
            until: until + stopped,
            at,
        }])
    })
    .await
}

/// Ends the current term, earning whatever is left of it, and starts another.
///
/// **The old term is recognised in full first.** A term that ended is a term
/// that was delivered, however few times the member turned up, so leaving a
/// remainder in the liability would understate revenue for ever.
pub async fn renew_subscription(
    db: &TenantDb,
    id: &AggregateId,
    price: Money,
    until: Timestamp,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<SubscriptionEvent> {
    if !price.is_positive() {
        return Err(rejected(PrepaidError::NotAValue));
    }
    step(db, id, at, metadata, "Renewed", move |held| {
        let from = held
            .until
            .ok_or_else(|| PrepaidError::NoSuchSubscription(id.to_string()))?;
        if at < from {
            return Err(PrepaidError::TermNotOver {
                id: id.to_string(),
                until: from,
            });
        }
        if until <= from {
            return Err(PrepaidError::NotATerm);
        }
        let mut events = Vec::new();
        if let Some(caught) = catch_up(held, id, from)? {
            events.push(caught);
        }
        events.push(SubscriptionEvent::Renewed {
            price,
            from,
            until,
            at,
        });
        Ok(events)
    })
    .await
}

/// Ends it, earning whatever time was served.
///
/// **Whatever is left stays a liability**, because the business still owes it.
/// What happens to it is a decision made elsewhere: a refund is a credit note
/// in `sales`, and forfeiting it is a policy this module does not hold an
/// opinion on. The number is right and visible either way.
pub async fn cancel_subscription(
    db: &TenantDb,
    id: &AggregateId,
    why: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<SubscriptionEvent> {
    let why = why.to_owned();
    step(db, id, at, metadata, "Cancelled", move |held| {
        if held.cancelled {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        if let Some(caught) = catch_up(held, id, at)? {
            events.push(caught);
        }
        events.push(SubscriptionEvent::Cancelled {
            why: why.clone(),
            at,
        });
        Ok(events)
    })
    .await
}

/// One decision on an existing subscription, with whatever it earned posted.
async fn step<F>(
    db: &TenantDb,
    id: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
    memo: &str,
    decide: F,
) -> Outcome<SubscriptionEvent>
where
    F: Fn(&Subscription) -> Result<Vec<SubscriptionEvent>, PrepaidError> + Send + Sync,
{
    let entry = derived("pdt", &[id.as_str(), &at.timestamp().to_string()]).map_err(rejected)?;
    let decide = &decide;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let committed = try_execute::<Subscription, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Subscription>| {
                    let held = &loaded.aggregate;
                    if !held.exists() {
                        return Err(PrepaidError::NoSuchSubscription(id.to_string()));
                    }
                    if held.cancelled && !memo.starts_with("Cancel") {
                        return Err(PrepaidError::Cancelled(id.to_string()));
                    }
                    Ok(Decision::record(decide(held)?))
                },
            )
            .await?;

            release_for(
                &mut *conn,
                &entry,
                at,
                &format!("{memo} · {id}"),
                earned(&committed.events),
                metadata,
            )
            .await?;

            // **A renewal is both directions in one step**: the term that ended
            // is released, and the term that started is deferred. Only the
            // first was posted here until `a_renewal_earns_the_whole_of_the_
            // term_that_ended` found the liability for the new term missing
            // from the books while the read model carried it.
            if let Some(value) = deferring(&committed.events) {
                let started = derived("pdd", &[id.as_str(), &at.timestamp().to_string()])
                    .map_err(ExecuteError::Rejected)?;
                let accounts = accounts(&mut *conn).await?;
                let lines = entry_for_deferral(value, &accounts)
                    .map_err(|e| ExecuteError::Rejected(PrepaidError::Unbalanced(e)))?;
                post(
                    &mut *conn,
                    &started,
                    at,
                    &format!("Deferred · {id}"),
                    &lines,
                    metadata,
                )
                .await?;
            }
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Subscription::domain())
}

/// The recognition a subscription owes by a moment, if any.
fn catch_up(
    held: &Subscription,
    id: &AggregateId,
    through: Timestamp,
) -> Result<Option<SubscriptionEvent>, PrepaidError> {
    let earned = held
        .earned_by(through)
        .ok_or_else(|| PrepaidError::NoSuchSubscription(id.to_string()))??;
    let already = held
        .recognised
        .ok_or_else(|| PrepaidError::NoSuchSubscription(id.to_string()))?;
    let value = earned.checked_sub(already)?;
    // Never negative: time does not un-pass, and a rounding step that produced
    // one would be un-recognising revenue somebody has already reported.
    Ok(value
        .is_positive()
        .then_some(SubscriptionEvent::Recognised {
            through,
            value,
            at: through,
        }))
}

/// What a set of subscription events newly deferred. A renewal is the only one
/// that does: everything else here releases or does nothing.
fn deferring(events: &[SubscriptionEvent]) -> Option<Money> {
    events.iter().find_map(|event| match event {
        SubscriptionEvent::Renewed { price, .. } => Some(*price),
        _ => None,
    })
}

/// What a set of subscription events earned.
fn earned(events: &[SubscriptionEvent]) -> Option<Money> {
    events.iter().fold(None, |total, event| {
        let SubscriptionEvent::Recognised { value, .. } = event else {
            return total;
        };
        Some(total.map_or(*value, |running: Money| {
            running.checked_add(*value).unwrap_or(running)
        }))
    })
}

// ------------------------------------------------------------------ loyalty

/// A card a customer holds.
#[derive(Debug, Clone)]
pub struct Card {
    pub customer: AggregateId,
    pub mechanic: Mechanic,
    pub at: Timestamp,
}

/// Opens a card. Nothing is deferred until something is earned on it.
pub async fn open_card(
    db: &TenantDb,
    id: &AggregateId,
    card: &Card,
    metadata: &Metadata,
) -> Outcome<LoyaltyEvent> {
    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            check_customer(&mut *conn, &card.customer).await?;

            let committed = try_create::<Loyalty, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |_loaded: &Loaded<Loyalty>| {
                    Ok(Decision::one(LoyaltyEvent::Opened {
                        customer: card.customer.clone(),
                        mechanic: card.mechanic,
                        at: card.at,
                    }))
                },
            )
            .await?;
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Loyalty::domain())
}

/// What a movement earned, and what was spent to earn it.
#[derive(Debug, Clone)]
pub struct Earning {
    /// The caller's key. Earning against the same one twice is a no-op (L8).
    pub reference: String,
    /// **What was spent, excluding tax.** The allocation is a fraction of it,
    /// so a caller that passes zero earns counts that defer nothing.
    pub spend: Money,
    /// How many counts. `None` computes them from the scheme's rate at this
    /// card's rank, which is what points do; stamps and visits count their own.
    pub count: Option<u32>,
    /// The sale it came from. Opaque — this module does not know what an
    /// invoice is.
    pub from: Option<AggregateId>,
    pub at: Timestamp,
}

/// Awards counts, and defers the part of the sale that belongs to them.
///
/// **IFRS 15, with no shortcut available.** See [`crate::loyalty`] for the
/// allocation and for why there is no setting that would select the other
/// treatment.
pub async fn earn(
    db: &TenantDb,
    id: &AggregateId,
    earning: &Earning,
    metadata: &Metadata,
) -> Outcome<LoyaltyEvent> {
    if earning.spend.is_negative() {
        return Err(rejected(PrepaidError::NotAValue));
    }
    let entry = derived("pdle", &[id.as_str(), &earning.reference]).map_err(rejected)?;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let scheme = Scheme::resolve(&mut *conn)
                .await
                .map_err(|e| ExecuteError::Rejected(PrepaidError::Config(e)))?
                .ok_or_else(|| ExecuteError::Rejected(PrepaidError::NoScheme))?;

            let committed = try_execute::<Loyalty, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Loyalty>| {
                    let held = &loaded.aggregate;
                    if !held.exists() {
                        return Err(PrepaidError::NoSuchCard(id.to_string()));
                    }
                    if held.has_movement(&earning.reference) {
                        return Ok(Decision::nothing());
                    }
                    // The rate is read at the rank the card has **already**
                    // reached, so the movement that crosses a threshold earns at
                    // the old rate and the next one at the new. Any other
                    // reading makes the award depend on itself.
                    let count = earning
                        .count
                        .unwrap_or_else(|| scheme.counts_for(earning.spend, held.lifetime));
                    if count == 0 {
                        return Ok(Decision::nothing());
                    }
                    let allocated = allocate(earning.spend, count, scheme.worth)?;
                    // `apply` cannot fail, so a scheme whose currency changed
                    // under a card holding a balance has to be stopped here or
                    // the addition would be silently dropped (L6).
                    if held
                        .outstanding()
                        .is_some_and(|held| held.currency() != allocated.currency())
                    {
                        return Err(PrepaidError::WrongCurrency(id.to_string()));
                    }
                    Ok(Decision::one(LoyaltyEvent::Earned {
                        reference: earning.reference.clone(),
                        count,
                        allocated,
                        from: earning.from.clone(),
                        at: earning.at,
                    }))
                },
            )
            .await?;

            if let Some(value) = awarded(&committed.events).filter(|value| value.is_positive()) {
                let accounts = accounts(&mut *conn).await?;
                let lines = entry_for_deferral(value, &accounts)
                    .map_err(|e| ExecuteError::Rejected(PrepaidError::Unbalanced(e)))?;
                post(
                    &mut *conn,
                    &entry,
                    earning.at,
                    &format!("Allocated to points · {id}"),
                    &lines,
                    metadata,
                )
                .await?;
            }
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Loyalty::domain())
}

/// Counts coming off a card.
#[derive(Debug, Clone)]
pub struct PointsRedemption {
    /// The caller's key. Redeeming the same one twice is a no-op (L8).
    pub reference: String,
    pub count: u32,
    /// What they were spent on. Opaque.
    pub toward: Option<AggregateId>,
    pub at: Timestamp,
}

/// Spends counts, and recognises what honouring them delivered.
pub async fn redeem_points(
    db: &TenantDb,
    id: &AggregateId,
    redemption: &PointsRedemption,
    metadata: &Metadata,
) -> Outcome<LoyaltyEvent> {
    let entry = derived("pdlr", &[id.as_str(), &redemption.reference]).map_err(rejected)?;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let committed = try_execute::<Loyalty, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Loyalty>| {
                    let held = &loaded.aggregate;
                    if !held.exists() {
                        return Err(PrepaidError::NoSuchCard(id.to_string()));
                    }
                    if held.has_movement(&redemption.reference) || redemption.count == 0 {
                        return Ok(Decision::nothing());
                    }
                    let balance = held.balance.ok_or_else(|| PrepaidError::NothingLeft {
                        id: id.to_string(),
                        left: "0".to_owned(),
                        wanted: redemption.count.to_string(),
                    })?;
                    // The same drawdown a package uses: each count is worth
                    // what is left divided by what is left to spend, so the last
                    // one takes the remainder and nothing is stranded.
                    let (count, value) = draw(balance, redemption.count, id)?;
                    Ok(Decision::one(LoyaltyEvent::Redeemed {
                        reference: redemption.reference.clone(),
                        count,
                        value,
                        toward: redemption.toward.clone(),
                        at: redemption.at,
                    }))
                },
            )
            .await?;

            release_for(
                &mut *conn,
                &entry,
                redemption.at,
                &format!("Points honoured · {id}"),
                honoured(&committed.events),
                metadata,
            )
            .await?;
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Loyalty::domain())
}

/// Writes off counts that timed out, and recognises them.
///
/// **Breakage is revenue**, and the card survives it: a points balance running
/// out is not the end of the card, which is the difference between this and an
/// entitlement expiring.
///
/// `at` is when they lapsed, not when somebody noticed.
pub async fn expire_points(
    db: &TenantDb,
    id: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<LoyaltyEvent> {
    let entry = derived("pdlx", &[id.as_str(), &at.timestamp().to_string()]).map_err(rejected)?;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let committed = try_execute::<Loyalty, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Loyalty>| {
                    let held = &loaded.aggregate;
                    if !held.exists() {
                        return Err(PrepaidError::NoSuchCard(id.to_string()));
                    }
                    let Some(value) = held.outstanding().filter(|value| !value.is_zero()) else {
                        return Ok(Decision::nothing());
                    };
                    Ok(Decision::one(LoyaltyEvent::Expired {
                        count: held.counts(),
                        value,
                        at,
                    }))
                },
            )
            .await?;

            release_for(
                &mut *conn,
                &entry,
                at,
                &format!("Points breakage · {id}"),
                honoured(&committed.events),
                metadata,
            )
            .await?;
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Loyalty::domain())
}

/// What a set of loyalty events newly deferred.
fn awarded(events: &[LoyaltyEvent]) -> Option<Money> {
    events.iter().find_map(|event| match event {
        LoyaltyEvent::Earned { allocated, .. } => Some(*allocated),
        _ => None,
    })
}

/// What a set of loyalty events released to revenue — spent, or timed out.
fn honoured(events: &[LoyaltyEvent]) -> Option<Money> {
    events.iter().find_map(|event| match event {
        LoyaltyEvent::Redeemed { value, .. } | LoyaltyEvent::Expired { value, .. } => Some(*value),
        _ => None,
    })
}

// ------------------------------------------------------------------ helpers

/// Posts a release, when there is one to post.
async fn release_for(
    conn: &mut sqlx::PgConnection,
    entry: &AggregateId,
    on: Timestamp,
    memo: &str,
    value: Option<Money>,
    metadata: &Metadata,
) -> Result<(), ExecuteError<PrepaidError>> {
    // A grant nobody paid for releases nothing, and so does a decision that
    // recorded nothing. Both reach here as zero, and neither should put an
    // empty journal entry in the books.
    let Some(value) = value.filter(|value| value.is_positive()) else {
        return Ok(());
    };
    let accounts = accounts(&mut *conn).await?;
    let lines = entry_for_release(value, &accounts)
        .map_err(|e| ExecuteError::Rejected(PrepaidError::Unbalanced(e)))?;
    post(conn, entry, on, memo, &lines, metadata).await
}

async fn post(
    conn: &mut sqlx::PgConnection,
    entry: &AggregateId,
    on: Timestamp,
    memo: &str,
    lines: &ledger::BalancedLines,
    metadata: &Metadata,
) -> Result<(), ExecuteError<PrepaidError>> {
    ledger::post_entry_in(conn, entry, on, memo, lines, metadata)
        .await
        .map(|_| ())
        .map_err(|e| match e {
            ExecuteError::Rejected(refusal) => {
                ExecuteError::Rejected(PrepaidError::Ledger(refusal))
            }
            ExecuteError::Load(e) => ExecuteError::Load(e),
            ExecuteError::Append(e) => ExecuteError::Append(e),
            ExecuteError::Enqueue(e) => ExecuteError::Enqueue(e),
            ExecuteError::Database(e) => ExecuteError::Database(e),
            ExecuteError::Contended { stream, attempts } => {
                ExecuteError::Contended { stream, attempts }
            }
            ExecuteError::AlreadyExists { stream } => ExecuteError::AlreadyExists { stream },
        })
}

async fn accounts(
    conn: &mut sqlx::PgConnection,
) -> Result<PostingAccounts, ExecuteError<PrepaidError>> {
    PostingAccounts::resolve(conn)
        .await
        .map_err(|e| ExecuteError::Rejected(PrepaidError::Config(e)))
}

/// Refuses a `crm` reference nothing answers to.
///
/// Against the **log** and not `proj_crm`, for the reason `sales` and `booking`
/// do the same: `crm` is another projection group on another checkpoint, and a
/// customer created a moment ago is not in that table yet.
async fn check_customer(
    conn: &mut sqlx::PgConnection,
    customer: &AggregateId,
) -> Result<(), ExecuteError<PrepaidError>> {
    if crm::accepts_documents(&mut *conn, customer)
        .await
        .map_err(ExecuteError::Load)?
    {
        Ok(())
    } else {
        Err(ExecuteError::Rejected(PrepaidError::NoSuchCustomer(
            customer.to_string(),
        )))
    }
}
