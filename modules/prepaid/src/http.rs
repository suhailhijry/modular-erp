//! The prepaid module's HTTP surface.
//!
//! Translation only, like every module's.
//!
//! # Amounts on the wire are minor units and a currency
//!
//! `{"minor": 30000, "currency": "SAR"}` is three hundred riyals. The same
//! shape `sales` uses for a payment, and for the same reason: a decimal string
//! is a float somewhere in every client, and this module's whole job is a
//! number that has to be exactly right.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_eventlog::ExecuteError;
use erp_i18n::{Locale, Localize};
use erp_tenant::CommandError;
use erp_types::{Money, Timestamp};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::AppState;
use erp_web::Problem;
use erp_web::{
    After, Allowed, Amount, IdempotencyKey, Language, ManageAccounts, Paged, PostEntries, Read,
};
use erp_web::{Consistency, nudge};
use erp_web::{Json, Query, bad_request, creating, metadata, parse_id, require_module};

use crate::{
    Card, Earning, Grant, Mechanic, PointsRedemption, PrepaidError, Reason, Redemption, Term,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_entitlements, grant_entitlement))
        .routes(routes!(get_entitlement))
        .routes(routes!(redeem_entitlement))
        .routes(routes!(expire_entitlement))
        .routes(routes!(revoke_entitlement))
        .routes(routes!(list_subscriptions, start_subscription))
        .routes(routes!(get_subscription))
        .routes(routes!(recognise_subscription))
        .routes(routes!(freeze_subscription, resume_subscription))
        .routes(routes!(renew_subscription))
        .routes(routes!(cancel_subscription))
        .routes(routes!(list_cards, open_card))
        .routes(routes!(get_card))
        .routes(routes!(earn_on_card))
        .routes(routes!(redeem_card_points))
        .routes(routes!(expire_card_points))
        .routes(routes!(outstanding))
        .routes(routes!(deferral_accounts, set_deferral_accounts))
        .routes(routes!(loyalty_scheme, set_loyalty_scheme))
}

/// This module's own failures plus everything any route can produce.
///
/// `ledger`'s is in here because a posting this module makes can be refused by
/// a closed period or a missing account, and the ledger's message names the
/// account — which is what the person fixing it needs.
static CATALOG: erp_i18n::Composite = erp_i18n::Composite::new(&[
    &crate::CATALOG,
    &ledger::CATALOG,
    &crm::CATALOG,
    &erp_web::CATALOG,
]);

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "customer": "CUST-0001",
    "what": "قص",
    "uses": 10,
    "value": {"minor": 80_000, "currency": "SAR"},
    "reason": "bought"
}))]
struct NewEntitlement {
    /// The `crm` record that holds it.
    customer: String,
    /// What it is for, in your own words.
    what: String,
    /// Uses granted. Absent for a deposit, which is an amount and not a count.
    uses: Option<u32>,
    /// **What is deferred, excluding tax.** Zero when nobody paid.
    value: Amount,
    /// `bought`, `gifted_by_customer`, `granted_by_business` or
    /// `free_from_coupon`. The last two carry no value: nobody paid, so there
    /// is nothing to defer.
    reason: String,
    /// The thing it is held against — the booking a deposit secures. Opaque.
    against: Option<String>,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    expires_at: Option<Timestamp>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewRedemption {
    /// Your key for this redemption. Sending it twice is a no-op.
    reference: String,
    /// How many uses. Defaults to one, and is ignored on a deposit.
    #[serde(default = "one")]
    uses: u32,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

const fn one() -> u32 {
    1
}

#[derive(Debug, Deserialize, ToSchema)]
struct EndingIt {
    #[serde(default)]
    why: String,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct AtAMoment {
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

/// An amount on the way out. `erp_web::Amount` is the way in and is
/// deserialize-only, which is the right asymmetry: a client sends what it
/// believes and reads back what the books say.
#[derive(Debug, Serialize, ToSchema)]
struct Cash {
    /// The amount in the currency's smallest unit. `1050` is 10.50 SAR.
    minor: i64,
    /// ISO 4217, upper case.
    currency: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct EntitlementRecord {
    id: String,
    customer: String,
    what: String,
    uses_granted: Option<u32>,
    uses_left: Option<u32>,
    /// What was deferred when it was granted.
    deferred: Cash,
    /// **What is still owed to the customer.**
    outstanding: Cash,
    reason: String,
    against: Option<String>,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    expires_at: Option<Timestamp>,
    /// `spent`, `expired`, `revoked`, or absent while it is live.
    closed: Option<String>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    granted_on: Timestamp,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "customer": "CUST-0001",
    "plan": "سنوي",
    "price": {"minor": 120_000, "currency": "SAR"},
    "from": "2026-01-01T00:00:00Z",
    "until": "2027-01-01T00:00:00Z"
}))]
struct NewSubscription {
    customer: String,
    plan: String,
    /// **What is deferred, excluding tax**, for this term.
    price: Amount,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    from: Timestamp,
    /// Exclusive.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct Renewal {
    price: Amount,
    /// The end of the new term, exclusive.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct SubscriptionRecord {
    id: String,
    customer: String,
    plan: String,
    price: Cash,
    /// What has been earned of this term.
    recognised: Cash,
    /// **What is still owed**: the unearned part of the term.
    outstanding: Cash,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    starts_at: Timestamp,
    /// Exclusive, and it **moves**: resuming pushes it out by exactly the time
    /// the clock was stopped for.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    ends_at: Timestamp,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    frozen_since: Option<Timestamp>,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    cancelled_at: Option<Timestamp>,
    cancelled_why: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct PrepaidAccepted {
    id: String,
    /// The log position this landed at. Pass it to a read as
    /// `?consistent_after=`.
    position: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
struct Owed {
    /// One per currency anything is held in.
    outstanding: Vec<Cash>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct DeferralAccounts {
    /// Where what is owed to customers is held. `2400` in every shipped chart.
    deferred: String,
    /// Where it lands as it is earned. `4000`.
    revenue: String,
}

#[derive(Debug, Deserialize)]
struct HeldQuery {
    #[serde(flatten)]
    page: After,
    /// Only what this customer holds.
    customer: Option<String>,
    /// Include what has been spent, has lapsed, or was taken back.
    #[serde(default)]
    closed: bool,
}

// ---------------------------------------------------------------------------
// Entitlements
// ---------------------------------------------------------------------------

/// Packages, courses and deposits, newest first.
#[utoipa::path(
    get,
    path = "/v1/prepaid/entitlements",
    tag = "prepaid",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Rows per page. Clamped, never refused."),
        ("customer" = Option<String>, Query, description = "Only what this customer holds."),
        ("closed" = Option<bool>, Query, description = "Include what is finished."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Paged<EntitlementRecord>),
        (status = BAD_REQUEST, description = "An unreadable cursor", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable prepaid", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn list_entitlements(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<HeldQuery>,
) -> Result<Json<Paged<EntitlementRecord>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = query.page.cursor(locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let page = crate::entitlements(
        &mut conn,
        query.customer.as_deref(),
        query.closed,
        query.page.limit(50, 200),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;

    Ok(Json(Paged::of(page, held)))
}

/// Record something bought now and delivered later.
#[utoipa::path(
    post,
    path = "/v1/prepaid/entitlements",
    tag = "prepaid",
    request_body = NewEntitlement,
    responses(
        (status = CREATED, body = PrepaidAccepted),
        (status = BAD_REQUEST, description = "A value on a grant nobody paid for, or a reason that is not one", body = Problem),
        (status = CONFLICT, description = "That id has already been granted", body = Problem),
        (status = NOT_FOUND, description = "No such customer, or the tenant did not enable prepaid", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The ledger refused the posting", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn grant_entitlement(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Json(body): Json<NewEntitlement>,
) -> Result<(StatusCode, Json<PrepaidAccepted>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let id = key.id().clone();
    let reason: Reason = body.reason.parse().map_err(|e: crate::UnknownReason| {
        bad_request(crate::messages::UNKNOWN_REASON, "value", &e.0, locale)
    })?;

    let grant = Grant {
        customer: parse_id(&body.customer, locale)?,
        what: body.what,
        uses: body.uses,
        value: amount(&body.value, locale)?,
        reason,
        against: body
            .against
            .as_deref()
            .map(|a| parse_id(a, locale))
            .transpose()?,
        expires_at: body.expires_at,
        at: body.at.unwrap_or_else(chrono::Utc::now),
    };

    let committed = crate::grant(&tenant.db, &id, &grant, &creating(&tenant, &key))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(accepted(id.to_string(), &committed)),
    ))
}

/// One of them.
#[utoipa::path(
    get,
    path = "/v1/prepaid/entitlements/{entitlement}",
    tag = "prepaid",
    params(("entitlement" = String, Path, description = "The key it was granted under.")),
    responses(
        (status = OK, body = EntitlementRecord),
        (status = NOT_FOUND, description = "No such entitlement, or the projection has not caught up", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn get_entitlement(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<EntitlementRecord>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    crate::entitlement(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?
        .map(|found| Json(held(found)))
        .ok_or_else(|| missing(crate::messages::NO_SUCH_ENTITLEMENT, &id, locale))
}

/// Draw it down, and recognise what that delivered.
#[utoipa::path(
    post,
    path = "/v1/prepaid/entitlements/{entitlement}/redemptions",
    tag = "prepaid",
    params(("entitlement" = String, Path, description = "The key it was granted under.")),
    request_body = NewRedemption,
    responses(
        (status = OK, body = PrepaidAccepted),
        (status = NOT_FOUND, description = "No such entitlement", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "Finished, lapsed, or not enough left", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn redeem_entitlement(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<NewRedemption>,
) -> Result<Json<PrepaidAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let committed = crate::redeem(
        &tenant.db,
        &aggregate,
        &Redemption {
            reference: body.reference,
            uses: body.uses,
            at: body.at.unwrap_or_else(chrono::Utc::now),
        },
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(accepted(id, &committed)))
}

/// Write off what was never used. **Breakage is revenue.**
#[utoipa::path(
    post,
    path = "/v1/prepaid/entitlements/{entitlement}/expiry",
    tag = "prepaid",
    params(("entitlement" = String, Path, description = "The key it was granted under.")),
    request_body = AtAMoment,
    responses(
        (status = OK, description = "Nothing happens if it has not lapsed yet.", body = PrepaidAccepted),
        (status = NOT_FOUND, description = "No such entitlement", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The ledger refused the posting", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn expire_entitlement(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<AtAMoment>,
) -> Result<Json<PrepaidAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let committed = crate::expire(
        &tenant.db,
        &aggregate,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(accepted(id, &committed)))
}

/// Take it back. **Nothing is recognised**, because nothing was delivered.
#[utoipa::path(
    post,
    path = "/v1/prepaid/entitlements/{entitlement}/revocation",
    tag = "prepaid",
    params(("entitlement" = String, Path, description = "The key it was granted under.")),
    request_body = EndingIt,
    responses(
        (status = OK, body = PrepaidAccepted),
        (status = NOT_FOUND, description = "No such entitlement", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The ledger refused the posting", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn revoke_entitlement(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<EndingIt>,
) -> Result<Json<PrepaidAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let committed = crate::revoke(
        &tenant.db,
        &aggregate,
        &body.why,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(accepted(id, &committed)))
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

/// Terms paid for in advance, newest first.
#[utoipa::path(
    get,
    path = "/v1/prepaid/subscriptions",
    tag = "prepaid",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Rows per page. Clamped, never refused."),
        ("customer" = Option<String>, Query, description = "Only this customer's."),
        ("closed" = Option<bool>, Query, description = "Include cancelled ones."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Paged<SubscriptionRecord>),
        (status = BAD_REQUEST, description = "An unreadable cursor", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable prepaid", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn list_subscriptions(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<HeldQuery>,
) -> Result<Json<Paged<SubscriptionRecord>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = query.page.cursor(locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let page = crate::subscriptions(
        &mut conn,
        query.customer.as_deref(),
        query.closed,
        query.page.limit(50, 200),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;

    Ok(Json(Paged::of(page, term)))
}

/// Start a term and defer its price.
#[utoipa::path(
    post,
    path = "/v1/prepaid/subscriptions",
    tag = "prepaid",
    request_body = NewSubscription,
    responses(
        (status = CREATED, body = PrepaidAccepted),
        (status = BAD_REQUEST, description = "A term that ends before it starts, or a price of nothing", body = Problem),
        (status = CONFLICT, description = "That id has already started", body = Problem),
        (status = NOT_FOUND, description = "No such customer, or the tenant did not enable prepaid", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The ledger refused the posting", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn start_subscription(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Json(body): Json<NewSubscription>,
) -> Result<(StatusCode, Json<PrepaidAccepted>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let id = key.id().clone();
    let term = Term {
        customer: parse_id(&body.customer, locale)?,
        plan: body.plan,
        price: amount(&body.price, locale)?,
        from: body.from,
        until: body.until,
        at: body.at.unwrap_or_else(chrono::Utc::now),
    };

    let committed = crate::start_subscription(&tenant.db, &id, &term, &creating(&tenant, &key))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(accepted(id.to_string(), &committed)),
    ))
}

/// One of them.
#[utoipa::path(
    get,
    path = "/v1/prepaid/subscriptions/{subscription}",
    tag = "prepaid",
    params(("subscription" = String, Path, description = "The key it was started under.")),
    responses(
        (status = OK, body = SubscriptionRecord),
        (status = NOT_FOUND, description = "No such subscription, or the projection has not caught up", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn get_subscription(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<SubscriptionRecord>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    crate::subscription(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?
        .map(|found| Json(term(found)))
        .ok_or_else(|| missing(crate::messages::NO_SUCH_SUBSCRIPTION, &id, locale))
}

/// Earn whatever time has passed, up to a moment.
///
/// **Safe to run twice.** It computes what should have been earned in total and
/// posts the difference, so a month-end job that runs again posts nothing.
#[utoipa::path(
    post,
    path = "/v1/prepaid/subscriptions/{subscription}/recognition",
    tag = "prepaid",
    params(("subscription" = String, Path, description = "The key it was started under.")),
    request_body = AtAMoment,
    responses(
        (status = OK, body = PrepaidAccepted),
        (status = NOT_FOUND, description = "No such subscription", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "Cancelled, or the ledger refused the posting", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn recognise_subscription(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<AtAMoment>,
) -> Result<Json<PrepaidAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let committed = crate::recognise_through(
        &tenant.db,
        &aggregate,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(accepted(id, &committed)))
}

/// Stop the clock, after earning everything up to that moment.
#[utoipa::path(
    post,
    path = "/v1/prepaid/subscriptions/{subscription}/freeze",
    tag = "prepaid",
    params(("subscription" = String, Path, description = "The key it was started under.")),
    request_body = EndingIt,
    responses(
        (status = OK, body = PrepaidAccepted),
        (status = NOT_FOUND, description = "No such subscription", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "Already frozen, or cancelled", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn freeze_subscription(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<EndingIt>,
) -> Result<Json<PrepaidAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let committed = crate::freeze(
        &tenant.db,
        &aggregate,
        &body.why,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(accepted(id, &committed)))
}

/// Start the clock, and push the term out by exactly the time it was stopped.
#[utoipa::path(
    delete,
    path = "/v1/prepaid/subscriptions/{subscription}/freeze",
    tag = "prepaid",
    params(("subscription" = String, Path, description = "The key it was started under.")),
    responses(
        (status = OK, body = PrepaidAccepted),
        (status = NOT_FOUND, description = "No such subscription", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "It is not frozen", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn resume_subscription(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
) -> Result<Json<PrepaidAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let committed = crate::resume(
        &tenant.db,
        &aggregate,
        chrono::Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(accepted(id, &committed)))
}

/// End the current term, earning whatever is left of it, and start another.
#[utoipa::path(
    post,
    path = "/v1/prepaid/subscriptions/{subscription}/renewal",
    tag = "prepaid",
    params(("subscription" = String, Path, description = "The key it was started under.")),
    request_body = Renewal,
    responses(
        (status = OK, body = PrepaidAccepted),
        (status = BAD_REQUEST, description = "A price of nothing, or a term that ends before it starts", body = Problem),
        (status = NOT_FOUND, description = "No such subscription", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The current term is still running", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn renew_subscription(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<Renewal>,
) -> Result<Json<PrepaidAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let committed = crate::renew_subscription(
        &tenant.db,
        &aggregate,
        amount(&body.price, locale)?,
        body.until,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(accepted(id, &committed)))
}

/// End it, earning whatever time was served.
///
/// Whatever is left stays a liability, because the business still owes it. A
/// refund is a credit note in `sales`.
#[utoipa::path(
    post,
    path = "/v1/prepaid/subscriptions/{subscription}/cancellation",
    tag = "prepaid",
    params(("subscription" = String, Path, description = "The key it was started under.")),
    request_body = EndingIt,
    responses(
        (status = OK, body = PrepaidAccepted),
        (status = NOT_FOUND, description = "No such subscription", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The ledger refused the posting", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn cancel_subscription(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<EndingIt>,
) -> Result<Json<PrepaidAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let committed = crate::cancel_subscription(
        &tenant.db,
        &aggregate,
        &body.why,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(accepted(id, &committed)))
}

// ---------------------------------------------------------------------------
// The liability
// ---------------------------------------------------------------------------

/// **What customers are owed**, per currency.
///
/// The number the ledger's deferred revenue account has to agree with. This is
/// one half of that check: the other half is the account balance, which lives
/// in another projection group and which L3 forbids this module from reading.
#[utoipa::path(
    get,
    path = "/v1/prepaid/outstanding",
    tag = "prepaid",
    responses(
        (status = OK, body = Owed),
        (status = NOT_FOUND, description = "The tenant did not enable prepaid", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn outstanding(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
) -> Result<Json<Owed>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let held = crate::outstanding(&mut conn)
        .await
        .map_err(|e| database(&e, locale))?;

    Ok(Json(Owed {
        outstanding: held.into_iter().map(money).collect(),
    }))
}

/// Where this module holds what is owed, and where it lands as it is earned.
#[utoipa::path(
    get,
    path = "/v1/prepaid/posting-accounts",
    tag = "prepaid",
    responses(
        (status = OK, body = DeferralAccounts),
        (status = NOT_FOUND, description = "The tenant did not enable prepaid", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
    ),
)]
async fn deferral_accounts(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<DeferralAccounts>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let accounts = crate::PostingAccounts::resolve(&mut conn)
        .await
        .map_err(|e| config(&e, locale))?;

    Ok(Json(DeferralAccounts {
        deferred: accounts.deferred.to_string(),
        revenue: accounts.revenue.to_string(),
    }))
}

/// Choose them.
#[utoipa::path(
    put,
    path = "/v1/prepaid/posting-accounts",
    tag = "prepaid",
    request_body = DeferralAccounts,
    responses(
        (status = NO_CONTENT, description = "Set."),
        (status = BAD_REQUEST, description = "Not an account code", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable prepaid", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
    ),
)]
async fn set_deferral_accounts(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    Json(body): Json<DeferralAccounts>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let accounts = crate::PostingAccounts {
        deferred: parse_id(&body.deferred, locale)?,
        revenue: parse_id(&body.revenue, locale)?,
    };

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    erp_eventlog::configuration::set(
        &mut conn,
        crate::PostingAccounts::KEY,
        &accounts,
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| config(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Loyalty
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "customer": "CUST-0001",
    "mechanic": "points"
}))]
struct NewCard {
    /// The `crm` record that holds it.
    customer: String,
    /// `points`, `stamps` or `visits`. It decides what produces the count and
    /// nothing after that — the accounting is the same for all three.
    mechanic: String,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "reference": "INV-0001",
    "spend": {"minor": 10_000, "currency": "SAR"},
    "from": "INV-0001"
}))]
struct NewEarning {
    /// Your key for this movement. Sending it twice is a no-op.
    reference: String,
    /// **What was spent, excluding tax.** Part of it is deferred to the counts
    /// this awards, which is IFRS 15's separate performance obligation.
    spend: Amount,
    /// How many counts. Omit it for points, which are computed from the
    /// scheme's rate at this card's rank; send it for stamps and visits, which
    /// count their own.
    count: Option<u32>,
    /// The sale it came from. Opaque, and not checked.
    from: Option<String>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewPointsRedemption {
    /// Your key for this redemption. Sending it twice is a no-op.
    reference: String,
    /// How many counts to spend.
    #[serde(default = "one")]
    count: u32,
    /// What they were spent on. Opaque, and not checked.
    toward: Option<String>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct CardRecord {
    id: String,
    customer: String,
    /// `points`, `stamps` or `visits`.
    mechanic: String,
    /// Counts redeemable now.
    counts: u32,
    /// Every count ever earned. **Never decreases**, and it is what a rank is
    /// read from: spending points does not cost a rank.
    lifetime: u32,
    /// **What is still owed** against the counts. Absent on a card that has
    /// never earned, which has no currency to say it in.
    deferred: Option<Cash>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    opened_on: Timestamp,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct SchemeTier {
    /// What you call it. Never matched on.
    name: String,
    /// The lifetime count at which this rank begins.
    from: u32,
    /// Counts earned per major unit spent, in basis points. `15_000` is a point
    /// and a half per riyal.
    rate_bp: u32,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "worth": {"minor": 10, "currency": "SAR"},
    "rate_bp": 10_000,
    "tiers": [{"name": "ذهبي", "from": 2_000, "rate_bp": 15_000}]
}))]
struct NewLoyaltyScheme {
    /// **What one count is worth when it is redeemed**, and the currency every
    /// card under this scheme is held in. It is the standalone selling price
    /// the IFRS 15 allocation is computed from, so it decides how much of each
    /// sale is deferred.
    worth: Amount,
    /// The base earning rate, in counts per major unit spent, in basis points.
    /// `10_000` is one point per riyal. Ignored for stamps and visits.
    rate_bp: u32,
    /// Ranks, in any order. The highest `from` at or below a card's lifetime
    /// count wins; below all of them the base rate applies.
    #[serde(default)]
    tiers: Vec<SchemeTier>,
}

#[derive(Debug, Serialize, ToSchema)]
struct LoyaltySchemeRecord {
    worth: Cash,
    rate_bp: u32,
    tiers: Vec<SchemeTier>,
}

#[derive(Debug, Deserialize)]
struct CardQuery {
    #[serde(flatten)]
    page: After,
    /// Only the cards this customer holds.
    customer: Option<String>,
}

/// Loyalty cards, newest first.
#[utoipa::path(
    get,
    path = "/v1/prepaid/cards",
    tag = "prepaid",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Rows per page. Clamped, never refused."),
        ("customer" = Option<String>, Query, description = "Only this customer's cards."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Paged<CardRecord>),
        (status = BAD_REQUEST, description = "An unreadable cursor", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable prepaid", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn list_cards(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<CardQuery>,
) -> Result<Json<Paged<CardRecord>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = query.page.cursor(locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let page = crate::cards(
        &mut conn,
        query.customer.as_deref(),
        query.page.limit(50, 200),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;

    Ok(Json(Paged::of(page, card)))
}

/// Open a card. Nothing is deferred until something is earned on it.
#[utoipa::path(
    post,
    path = "/v1/prepaid/cards",
    tag = "prepaid",
    request_body = NewCard,
    responses(
        (status = CREATED, body = PrepaidAccepted),
        (status = BAD_REQUEST, description = "A mechanic that is not one", body = Problem),
        (status = CONFLICT, description = "That id is already open", body = Problem),
        (status = NOT_FOUND, description = "No such customer, or the tenant did not enable prepaid", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn open_card(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Json(body): Json<NewCard>,
) -> Result<(StatusCode, Json<PrepaidAccepted>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let id = key.id().clone();
    let mechanic: Mechanic = body.mechanic.parse().map_err(|e: crate::UnknownMechanic| {
        bad_request(crate::messages::UNKNOWN_MECHANIC, "mechanic", &e.0, locale)
    })?;

    let card = Card {
        customer: parse_id(&body.customer, locale)?,
        mechanic,
        at: body.at.unwrap_or_else(chrono::Utc::now),
    };

    let committed = crate::open_card(&tenant.db, &id, &card, &creating(&tenant, &key))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(accepted(id.to_string(), &committed)),
    ))
}

/// One of them.
#[utoipa::path(
    get,
    path = "/v1/prepaid/cards/{card}",
    tag = "prepaid",
    params(("card" = String, Path, description = "The key it was opened under.")),
    responses(
        (status = OK, body = CardRecord),
        (status = NOT_FOUND, description = "No such card, or the projection has not caught up", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn get_card(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<CardRecord>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    crate::card(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?
        .map(|found| Json(card(found)))
        .ok_or_else(|| missing(crate::messages::NO_SUCH_CARD, &id, locale))
}

/// Award counts, and defer the part of the sale that belongs to them.
///
/// **IFRS 15**: points are a separate performance obligation, so part of the
/// sale's price is allocated to them and held until they are honoured. There is
/// no setting that selects the other treatment.
#[utoipa::path(
    post,
    path = "/v1/prepaid/cards/{card}/earnings",
    tag = "prepaid",
    params(("card" = String, Path, description = "The key it was opened under.")),
    request_body = NewEarning,
    responses(
        (status = OK, body = PrepaidAccepted),
        (status = NOT_FOUND, description = "No such card", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No scheme is configured, or the ledger refused the posting", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn earn_on_card(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<NewEarning>,
) -> Result<Json<PrepaidAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;

    let earning = Earning {
        reference: body.reference,
        spend: amount(&body.spend, locale)?,
        count: body.count,
        from: body
            .from
            .as_deref()
            .map(|from| parse_id(from, locale))
            .transpose()?,
        at: body.at.unwrap_or_else(chrono::Utc::now),
    };

    let committed = crate::earn(&tenant.db, &aggregate, &earning, &metadata(&tenant))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(accepted(id, &committed)))
}

/// Spend counts, and recognise what honouring them delivered.
#[utoipa::path(
    post,
    path = "/v1/prepaid/cards/{card}/redemptions",
    tag = "prepaid",
    params(("card" = String, Path, description = "The key it was opened under.")),
    request_body = NewPointsRedemption,
    responses(
        (status = OK, body = PrepaidAccepted),
        (status = NOT_FOUND, description = "No such card", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "Not enough counts left", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn redeem_card_points(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<NewPointsRedemption>,
) -> Result<Json<PrepaidAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;

    let redemption = PointsRedemption {
        reference: body.reference,
        count: body.count,
        toward: body
            .toward
            .as_deref()
            .map(|toward| parse_id(toward, locale))
            .transpose()?,
        at: body.at.unwrap_or_else(chrono::Utc::now),
    };

    let committed = crate::redeem_points(&tenant.db, &aggregate, &redemption, &metadata(&tenant))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(accepted(id, &committed)))
}

/// Write off counts that timed out, and recognise them as breakage.
///
/// The card survives it: a balance running out is not the end of the card.
#[utoipa::path(
    post,
    path = "/v1/prepaid/cards/{card}/expiry",
    tag = "prepaid",
    params(("card" = String, Path, description = "The key it was opened under.")),
    request_body = AtAMoment,
    responses(
        (status = OK, body = PrepaidAccepted),
        (status = NOT_FOUND, description = "No such card", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The ledger refused the posting", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn expire_card_points(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<AtAMoment>,
) -> Result<Json<PrepaidAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let at = body.at.unwrap_or_else(chrono::Utc::now);

    let committed = crate::expire_points(&tenant.db, &aggregate, at, &metadata(&tenant))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(accepted(id, &committed)))
}

/// What a count is worth and what earns one.
#[utoipa::path(
    get,
    path = "/v1/prepaid/loyalty-scheme",
    tag = "prepaid",
    responses(
        (status = OK, body = LoyaltySchemeRecord),
        (status = NOT_FOUND, description = "No scheme is configured, or the tenant did not enable prepaid", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
    ),
)]
async fn loyalty_scheme(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<LoyaltySchemeRecord>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let scheme = crate::Scheme::resolve(&mut conn)
        .await
        .map_err(|e| config(&e, locale))?
        // **There is no default**, and saying so is more useful than inventing
        // one: what a point is worth is a business decision with no defensible
        // fallback. See `crate::loyalty::Scheme`.
        .ok_or_else(|| missing(crate::messages::NO_SCHEME, "", locale))?;

    Ok(Json(LoyaltySchemeRecord {
        worth: money(scheme.worth),
        rate_bp: scheme.rate_bp,
        tiers: scheme.tiers.into_iter().map(tier).collect(),
    }))
}

/// Choose them.
#[utoipa::path(
    put,
    path = "/v1/prepaid/loyalty-scheme",
    tag = "prepaid",
    request_body = NewLoyaltyScheme,
    responses(
        (status = NO_CONTENT, description = "Set."),
        (status = BAD_REQUEST, description = "Not a currency", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable prepaid", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
    ),
)]
async fn set_loyalty_scheme(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    Json(body): Json<NewLoyaltyScheme>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let scheme = crate::Scheme {
        worth: amount(&body.worth, locale)?,
        rate_bp: body.rate_bp,
        tiers: body
            .tiers
            .into_iter()
            .map(|t| crate::Tier {
                name: t.name,
                from: t.from,
                rate_bp: t.rate_bp,
            })
            .collect(),
    };

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    erp_eventlog::configuration::set(
        &mut conn,
        crate::Scheme::KEY,
        &scheme,
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| config(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

fn amount(sent: &Amount, locale: Locale) -> Result<Money, Problem> {
    let currency = erp_types::CurrencyCode::new(&sent.currency).map_err(|_| {
        bad_request(
            erp_web::messages::UNKNOWN_CURRENCY,
            "currency",
            &sent.currency,
            locale,
        )
    })?;
    Ok(Money::from_minor(sent.minor, currency))
}

fn money(value: Money) -> Cash {
    Cash {
        minor: value.minor(),
        currency: value.currency().as_str().to_owned(),
    }
}

fn accepted<E>(id: String, committed: &erp_eventlog::Committed<E>) -> PrepaidAccepted {
    PrepaidAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }
}

fn held(e: crate::EntitlementSummary) -> EntitlementRecord {
    EntitlementRecord {
        id: e.id,
        customer: e.customer,
        what: e.what,
        uses_granted: e.uses_granted,
        uses_left: e.uses_left,
        deferred: money(e.deferred),
        outstanding: money(e.outstanding),
        reason: e.reason,
        against: e.against,
        expires_at: e.expires_at,
        closed: e.closed,
        granted_on: e.granted_on,
    }
}

fn term(s: crate::SubscriptionSummary) -> SubscriptionRecord {
    SubscriptionRecord {
        id: s.id,
        customer: s.customer,
        plan: s.plan,
        price: money(s.price),
        recognised: money(s.recognised),
        outstanding: money(s.outstanding),
        starts_at: s.starts_at,
        ends_at: s.ends_at,
        frozen_since: s.frozen_since,
        cancelled_at: s.cancelled_at,
        cancelled_why: s.cancelled_why,
    }
}

fn card(c: crate::CardSummary) -> CardRecord {
    CardRecord {
        id: c.id,
        customer: c.customer,
        mechanic: c.mechanic,
        counts: c.counts,
        lifetime: c.lifetime,
        deferred: c.deferred.map(money),
        opened_on: c.opened_on,
    }
}

fn tier(t: crate::Tier) -> SchemeTier {
    SchemeTier {
        name: t.name,
        from: t.from,
        rate_bp: t.rate_bp,
    }
}

fn missing(code: erp_i18n::MessageCode, id: &str, locale: Locale) -> Problem {
    Problem::new(
        StatusCode::NOT_FOUND,
        &erp_i18n::Message::new(code).with("id", erp_i18n::MessageArg::text(id.to_owned())),
        locale,
        &CATALOG,
    )
}

fn problem_for(error: &CommandError<PrepaidError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                // The id is taken. A different thing was meant.
                // Well-formed, and about something that is not there.
                PrepaidError::NoSuchCustomer(_)
                | PrepaidError::NoSuchEntitlement(_)
                | PrepaidError::NoSuchSubscription(_)
                | PrepaidError::NoSuchCard(_) => StatusCode::NOT_FOUND,

                // Well-formed, and refused on the state of the world. This is
                // where a lapsed package and an exhausted balance land.
                PrepaidError::NotLive(_)
                | PrepaidError::Lapsed { .. }
                | PrepaidError::NothingLeft { .. }
                | PrepaidError::AlreadyFrozen(_)
                | PrepaidError::NotFrozen(_)
                | PrepaidError::Cancelled(_)
                | PrepaidError::TermNotOver { .. }
                | PrepaidError::Ledger(_)
                | PrepaidError::NoScheme
                | PrepaidError::WrongCurrency(_)
                | PrepaidError::Unbalanced(_) => StatusCode::UNPROCESSABLE_ENTITY,

                _ => StatusCode::BAD_REQUEST,
            },
            rejection.message(),
        ),

        CommandError::Pool(e @ erp_tenant::PoolError::Overloaded { .. }) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.message())
        }

        CommandError::Execute(ExecuteError::Contended { .. }) => (
            StatusCode::CONFLICT,
            erp_i18n::Message::new(erp_eventlog::messages::CONCURRENT_MODIFICATION),
        ),

        // **The one that must never be silent.** A different request reused an
        // identifier that is taken; a retry of the request that created it
        // never reaches here, because the kernel reports those as success.
        CommandError::Execute(ExecuteError::AlreadyExists { .. }) => (
            StatusCode::CONFLICT,
            erp_i18n::Message::new(erp_eventlog::messages::ALREADY_EXISTS),
        ),

        other => {
            tracing::error!(error = %other, "prepaid command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &CATALOG)
}

fn pool(error: &erp_tenant::PoolError, locale: Locale) -> Problem {
    let status = match error {
        erp_tenant::PoolError::Overloaded { .. } => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    Problem::new(status, &error.message(), locale, &CATALOG)
}

fn config(error: &erp_eventlog::ConfigError, locale: Locale) -> Problem {
    tracing::error!(error = %error, "prepaid configuration failed");
    Problem::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        &error.message(),
        locale,
        &CATALOG,
    )
}

fn database(error: &sqlx::Error, locale: Locale) -> Problem {
    tracing::error!(error = %error, "prepaid read failed");
    Problem::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        &erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
        locale,
        &CATALOG,
    )
}
