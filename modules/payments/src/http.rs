//! The payments module's HTTP surface.
//!
//! Translation only, like every module's. See [`ledger::http`] for why these
//! live in the module rather than in the composition root.
//!
//! # There is no route that settles a payment
//!
//! Deliberately. A settlement is decided by asking the gateway, not by anybody
//! telling this system what happened — see the module docs. What a person can
//! do here is start a collection, look at one, and give money back.

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
use erp_web::{Allowed, IdempotencyKey, Language, ManageTenant, PostEntries, Read};
use erp_web::{Consistency, nudge};
use erp_web::{Json, Query, bad_request, creating, parse_id, require_module};

use crate::PaymentsError;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_payments, start_payment))
        .routes(routes!(get_payment))
        .routes(routes!(refund_gateway_payment))
        .routes(routes!(set_gateway))
        .routes(routes!(list_settlement, record_payout))
}

/// This module's own failures plus everything any route can produce.
static CATALOG: erp_i18n::Composite = erp_i18n::Composite::new(&[
    &crate::CATALOG,
    &sales::CATALOG,
    &ledger::CATALOG,
    &erp_payments::CATALOG,
    &erp_web::CATALOG,
]);

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

/// **Not `sales`' `NewPayment`.** That one is a customer handing over money;
/// this is a charge created at a gateway, which may never become one. The
/// `OpenAPI` document keeps one schema per name, so the two have to be told
/// apart — see `no_two_modules_claim_the_same_schema_name`.
#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "provider": "moyasar",
    "gateway_id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
    "invoice": "INV-1",
    "amount": 10000,
    "currency": "SAR"
}))]
struct NewGatewayPayment {
    /// `moyasar`, `tabby` or `tamara`.
    provider: String,
    /// **The gateway's own id**, from the charge you created against it. Every
    /// callback names this and nothing else, so a payment recorded without one
    /// can never be settled.
    gateway_id: String,
    /// The invoice this is collecting against.
    invoice: String,
    /// Minor units — halalas for SAR. Never a decimal.
    amount: i64,
    /// ISO-4217, three letters.
    currency: String,
    /// When the charge was created. Defaults to now.
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    started_at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewGatewayRefund {
    /// Your own reference for this refund. Sending it again is a retry, not a
    /// second refund.
    reference: String,
    /// Minor units. Omit to give back everything that is left.
    #[serde(default)]
    amount: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
struct GatewayPaymentRecorded {
    id: String,
    position: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
struct GatewayPaymentView {
    id: String,
    provider: String,
    gateway_id: String,
    invoice: String,
    amount: i64,
    currency: String,
    /// `pending`, `settled`, `failed`, `refunded` or `voided`.
    stage: String,
    /// What the gateway kept. Absent until it says, which for most providers is
    /// not until the payout.
    fee: Option<i64>,
    refunded: i64,
    /// In the gateway's words, when it refused.
    failed_why: Option<String>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    started_at: Timestamp,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    settled_at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema, utoipa::IntoParams)]
struct Against {
    /// The invoice to list attempts against.
    invoice: String,
}

fn view(row: crate::PaymentRow) -> GatewayPaymentView {
    GatewayPaymentView {
        id: row.id,
        provider: row.provider,
        gateway_id: row.gateway_id,
        invoice: row.invoice,
        amount: row.amount.minor(),
        currency: row.amount.currency().to_string(),
        stage: row.stage,
        fee: row.fee.map(Money::minor),
        refunded: row.refunded.minor(),
        failed_why: row.failed_why,
        started_at: row.started_at,
        settled_at: row.settled_at,
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// What has been tried against an invoice.
#[utoipa::path(
    get,
    path = "/v1/payments",
    tag = "payments",
    params(Against),
    responses(
        (status = OK, body = Vec<GatewayPaymentView>),
        (status = BAD_REQUEST, body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure, or the projection did not catch up in time. Retryable.", body = Problem),
    ),
)]
async fn list_payments(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(against): Query<Against>,
) -> Result<Json<Vec<GatewayPaymentView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let rows = crate::against(&mut conn, &against.invoice, PAGE)
        .await
        .map_err(|e| database(&e, locale))?;

    Ok(Json(rows.into_iter().map(view).collect()))
}

/// How many attempts one listing gives back.
const PAGE: i64 = 100;

/// Record a charge created at a gateway.
///
/// **Call this before sending the customer anywhere.** An attempt this system
/// does not know about is one no callback can be matched to, and the customer
/// will still have been charged.
///
/// It records; it does not collect. What happens next comes from the gateway.
#[utoipa::path(
    post,
    path = "/v1/payments",
    tag = "payments",
    params(("Idempotency-Key" = String, Header, description = "Sending it again is a retry.")),
    request_body = NewGatewayPayment,
    responses(
        (status = CREATED, body = GatewayPaymentRecorded),
        (status = BAD_REQUEST, description = "Not an id, not a currency, or not a positive amount", body = Problem),
        (status = CONFLICT, description = "That id is taken by a different payment", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn start_payment(
    tenant: Allowed<PostEntries>,
    Language(locale): Language,
    State(state): State<AppState>,
    key: IdempotencyKey,
    Json(body): Json<NewGatewayPayment>,
) -> Result<(StatusCode, Json<GatewayPaymentRecorded>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let invoice = parse_id(&body.invoice, locale)?;
    let currency = erp_types::CurrencyCode::new(&body.currency).map_err(|e| {
        bad_request(
            erp_web::messages::MALFORMED_BODY,
            "reason",
            &e.to_string(),
            locale,
        )
    })?;
    if !body.amount.is_positive() {
        return Err(bad_request(
            erp_web::messages::MALFORMED_BODY,
            "reason",
            "a payment must be for a positive amount",
            locale,
        ));
    }

    // **The gateway's id is this system's id too.** One key, so a callback can
    // find the payment and a person reading two screens sees one number.
    let id = parse_id(&body.gateway_id, locale)?;

    let mut tx = tenant.db.begin().await.map_err(|e| pool(&e, locale))?;
    let committed = crate::start_in(
        &mut tx,
        &id,
        &crate::Attempt {
            provider: body.provider.clone(),
            gateway_id: body.gateway_id.clone(),
            invoice,
            amount: Money::from_minor(body.amount, currency),
        },
        body.started_at.unwrap_or_else(chrono::Utc::now),
        &creating(&tenant, &key),
    )
    .await
    .map_err(|e| problem_for(&CommandError::Execute(e), locale))?;
    tx.commit().await.map_err(|e| database(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(GatewayPaymentRecorded {
            id: id.to_string(),
            position: committed.at.map(erp_types::LogPosition::get),
        }),
    ))
}

/// One attempt.
#[utoipa::path(
    get,
    path = "/v1/payments/{payment}",
    tag = "payments",
    params(("payment" = String, Path, description = "The gateway's id for it.")),
    responses(
        (status = OK, body = GatewayPaymentView),
        (status = NOT_FOUND, description = "No such payment, or the projection has not caught up", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn get_payment(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<GatewayPaymentView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let found = crate::payment(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?;

    found.map(view).map(Json).ok_or_else(|| {
        Problem::new(
            StatusCode::NOT_FOUND,
            &erp_i18n::Message::new(crate::messages::NOT_STARTED)
                .with("id", erp_i18n::MessageArg::text(id.clone())),
            locale,
            &CATALOG,
        )
    })
}

/// Give money back.
///
/// **Records and posts; it does not ask the gateway.** Instruct the gateway
/// first and record what it confirmed — a refund posted here that the gateway
/// refused is a set of books saying money went back when it did not.
#[utoipa::path(
    post,
    path = "/v1/payments/{payment}/refunds",
    tag = "payments",
    params(
        ("payment" = String, Path, description = "The gateway's id for it."),
        ("Idempotency-Key" = String, Header, description = "Sending it again is a retry."),
    ),
    request_body = NewGatewayRefund,
    responses(
        (status = OK, body = GatewayPaymentRecorded),
        (status = BAD_REQUEST, description = "More than is left to refund, or a payment that never settled", body = Problem),
        (status = NOT_FOUND, description = "No such payment", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn refund_gateway_payment(
    tenant: Allowed<PostEntries>,
    Language(locale): Language,
    State(state): State<AppState>,
    key: IdempotencyKey,
    Path(id): Path<String>,
    Json(body): Json<NewGatewayRefund>,
) -> Result<Json<GatewayPaymentRecorded>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let id = parse_id(&id, locale)?;

    let mut tx = tenant.db.begin().await.map_err(|e| pool(&e, locale))?;
    let record = crate::payment(&mut tx, id.as_str())
        .await
        .map_err(|e| database(&e, locale))?
        .ok_or_else(|| {
            Problem::new(
                StatusCode::NOT_FOUND,
                &erp_i18n::Message::new(crate::messages::NOT_STARTED)
                    .with("id", erp_i18n::MessageArg::text(id.to_string())),
                locale,
                &CATALOG,
            )
        })?;

    // Everything left, when no amount is named. The refundable balance is the
    // aggregate's to decide; this is only the wire default.
    let amount = Money::from_minor(
        body.amount
            .unwrap_or(record.amount.minor() - record.refunded.minor()),
        record.amount.currency(),
    );

    let committed = crate::refund_in(
        &mut tx,
        &id,
        &body.reference,
        amount,
        chrono::Utc::now(),
        &creating(&tenant, &key),
    )
    .await
    .map_err(|e| problem_for(&CommandError::Execute(e), locale))?;

    tx.commit().await.map_err(|e| database(&e, locale))?;
    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(GatewayPaymentRecorded {
        id: id.to_string(),
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "provider": "moyasar",
    "reference": "po_2026_03_04",
    "amount": 1_118_400,
    "currency": "SAR",
    "into": "1010",
    "covers": ["pay_1", "pay_2"]
}))]
struct NewPayout {
    provider: String,
    /// The gateway's own id for the transfer.
    reference: String,
    /// **What arrived.** Minor units — the number on the bank statement.
    amount: i64,
    currency: String,
    /// The bank account it landed in.
    into: String,
    /// The gateway payment ids it covers.
    ///
    /// **Empty is allowed and reconciles nothing** — right for somebody typing
    /// from a bank statement, who has an amount and no transaction list. The
    /// answer says `covered: 0` rather than pretending it agreed.
    #[serde(default)]
    covers: Vec<String>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    received_on: Option<Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct PayoutRecorded {
    id: String,
    /// What arrived less what the covered payments said should have.
    /// **Negative is short.**
    difference: i64,
    covered: usize,
    position: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
struct SettlementView {
    /// Per provider: what the gateway has settled and not yet paid over.
    awaiting: Vec<AwaitingView>,
    /// The transfers recorded, newest first.
    payouts: Vec<PayoutSummary>,
}

#[derive(Debug, Serialize, ToSchema)]
struct AwaitingView {
    provider: String,
    /// Settled amounts less the fees already booked. **This is what the
    /// clearing account should say**, and a disagreement is worth chasing.
    held: i64,
    currency: String,
    payments: i64,
    /// The oldest one still waiting.
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    since: Option<Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct PayoutSummary {
    id: String,
    provider: String,
    reference: String,
    amount: i64,
    expected: i64,
    /// **Negative is short.**
    difference: i64,
    currency: String,
    /// Zero reconciles nothing, which is not the same as agreeing.
    covered: i32,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    received_on: Timestamp,
}

/// What a tenant hands over so this system can talk to their gateway.
///
/// Shaped per provider, because the three need different things: Moyasar a
/// secret key, Tabby a key **and** the merchant code its integration manager
/// issued, Tamara a token and which host it belongs to.
#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "provider": "moyasar", "secret": "sk_live_…" }))]
struct NewGateway {
    /// `moyasar`, `tabby` or `tamara`.
    provider: String,
    /// The secret key, or Tamara's API token. **Stored sealed and never
    /// readable again.**
    secret: String,
    /// Tabby only, and required there.
    #[serde(default)]
    merchant_code: Option<String>,
    /// Tamara only. Their sandbox is a different host rather than a different
    /// token.
    #[serde(default)]
    sandbox: bool,
}

/// Configure a payment provider.
///
/// The key is **sealed** and cannot be read back — send a new one to rotate.
/// A deployment with no sealing key refuses rather than storing it in the
/// clear, which is the same call every other secret makes.
///
/// The key is checked here, not on the first customer's card: a publishable key
/// in the secret slot can create a payment and cannot capture, refund or fetch
/// one, which would fail *after* the money moved.
#[utoipa::path(
    put,
    path = "/v1/payments/gateways/{provider}",
    tag = "payments",
    params(("provider" = String, Path, description = "`moyasar`, `tabby` or `tamara`.")),
    request_body = NewGateway,
    responses(
        (status = NO_CONTENT, description = "Stored, sealed"),
        (status = BAD_REQUEST, description = "Not a provider this system integrates, or a key it cannot use", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "No sealing key configured, so nothing is stored in the clear", body = Problem),
    ),
)]
async fn set_gateway(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(provider): Path<String>,
    Json(body): Json<NewGateway>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    // The path and the body must agree, so a copy-pasted body cannot quietly
    // overwrite a different provider's key.
    if body.provider != provider {
        return Err(bad_request(
            erp_web::messages::MALFORMED_BODY,
            "reason",
            "the provider in the path and in the body disagree",
            locale,
        ));
    }

    let credentials = match provider.as_str() {
        "moyasar" => crate::Credentials::Moyasar {
            secret: body.secret,
        },
        "tabby" => crate::Credentials::Tabby {
            secret: body.secret,
            merchant_code: body.merchant_code.unwrap_or_default(),
        },
        "tamara" => crate::Credentials::Tamara {
            token: body.secret,
            sandbox: body.sandbox,
        },
        other => {
            return Err(bad_request(
                erp_web::messages::MALFORMED_BODY,
                "reason",
                &format!("{other} is not a payment provider this system integrates"),
                locale,
            ));
        }
    };

    // **Refuses rather than storing it in the clear** (L6).
    let Some(sealing) = state.sealing.clone() else {
        return Err(Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            &erp_i18n::Message::new(erp_web::messages::NO_SEALING_KEY),
            locale,
            &CATALOG,
        ));
    };

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    crate::configure(&mut conn, &sealing, &credentials)
        .await
        .map_err(|e| match e {
            crate::GatewayConfigError::Unusable(why) => bad_request(
                erp_web::messages::MALFORMED_BODY,
                "reason",
                &why.to_string(),
                locale,
            ),
            other => {
                tracing::error!(error = %other, "a gateway credential could not be stored");
                Problem::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
                    locale,
                    &CATALOG,
                )
            }
        })?;

    Ok(StatusCode::NO_CONTENT)
}

/// **What the gateway still owes, and what it has sent.**
///
/// `awaiting` is the reconciliation surface: settled payments the gateway has
/// not paid over, which is what the clearing account should be holding. A
/// disagreement between the two means a payment posted and its payout did not,
/// or the other way round.
#[utoipa::path(
    get,
    path = "/v1/payments/settlement",
    tag = "payments",
    responses(
        (status = OK, body = SettlementView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_settlement(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
) -> Result<Json<SettlementView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let awaiting = crate::awaiting_payout(&mut conn)
        .await
        .map_err(|e| database(&e, locale))?;
    let recorded = crate::payouts(&mut conn, PAGE)
        .await
        .map_err(|e| database(&e, locale))?;

    Ok(Json(SettlementView {
        awaiting: awaiting
            .into_iter()
            .map(|a| AwaitingView {
                provider: a.provider,
                held: a.held.minor(),
                currency: a.held.currency().to_string(),
                payments: a.payments,
                since: a.since,
            })
            .collect(),
        payouts: recorded
            .into_iter()
            .map(|p| PayoutSummary {
                id: p.id,
                provider: p.provider,
                reference: p.reference,
                amount: p.amount.minor(),
                expected: p.expected.minor(),
                difference: p.difference.minor(),
                currency: p.amount.currency().to_string(),
                covered: p.covered,
                received_on: p.received_on,
            })
            .collect(),
    }))
}

/// Record a transfer from a gateway, and reconcile it.
///
/// The arithmetic is what arrived against what the covered payments say should
/// have. **A difference is booked, not refused** — a payout that cannot be
/// recorded leaves the books saying the gateway still holds money it has
/// already sent, and the next reconciliation inherits that.
///
/// Naming no payments records the transfer and reconciles nothing, which is
/// right for somebody working from a bank statement.
#[utoipa::path(
    post,
    path = "/v1/payments/payouts",
    tag = "payments",
    params(("Idempotency-Key" = String, Header, description = "Sending it again is a retry.")),
    request_body = NewPayout,
    responses(
        (status = CREATED, body = PayoutRecorded),
        (status = BAD_REQUEST, description = "Not a currency, or a payment that never settled", body = Problem),
        (status = CONFLICT, description = "That payout has already been recorded", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn record_payout(
    tenant: Allowed<PostEntries>,
    Language(locale): Language,
    State(state): State<AppState>,
    key: IdempotencyKey,
    Json(body): Json<NewPayout>,
) -> Result<(StatusCode, Json<PayoutRecorded>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let currency = erp_types::CurrencyCode::new(&body.currency).map_err(|e| {
        bad_request(
            erp_web::messages::MALFORMED_BODY,
            "reason",
            &e.to_string(),
            locale,
        )
    })?;
    let into = parse_id(&body.into, locale)?;
    // **The gateway's payout id is this system's id too**, so a settlement
    // report imported twice records one payout.
    let id = parse_id(&body.reference, locale)?;
    let covered = body.covers.len();

    let mut tx = tenant.db.begin().await.map_err(|e| pool(&e, locale))?;
    let committed = crate::record_payout_in(
        &mut tx,
        &id,
        &crate::Transfer {
            reference: body.reference.clone(),
            provider: body.provider,
            amount: Money::from_minor(body.amount, currency),
            into,
            covers: body.covers,
        },
        body.received_on.unwrap_or_else(chrono::Utc::now),
        &creating(&tenant, &key),
    )
    .await
    .map_err(|e| problem_for(&CommandError::Execute(e), locale))?;
    tx.commit().await.map_err(|e| database(&e, locale))?;

    let difference = committed.events.first().map_or(
        0,
        |crate::PayoutEvent::Received {
             amount, expected, ..
         }| amount.minor() - expected.minor(),
    );

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(PayoutRecorded {
            id: id.to_string(),
            difference,
            covered,
            position: committed.at.map(erp_types::LogPosition::get),
        }),
    ))
}

// ---------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------

impl Localize for PaymentsError {
    fn message(&self) -> erp_i18n::Message {
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NotStarted(id) => {
                Message::new(crate::messages::NOT_STARTED).with("id", MessageArg::text(id))
            }
            Self::AlreadyStarted(id) => {
                Message::new(crate::messages::ALREADY_STARTED).with("id", MessageArg::text(id))
            }
            Self::WrongAmount { expected, found } => Message::new(crate::messages::WRONG_AMOUNT)
                .with("expected", MessageArg::text(expected.to_string()))
                .with("found", MessageArg::text(found.to_string())),
            Self::NotCollectable { id, stage } => Message::new(crate::messages::NOT_COLLECTABLE)
                .with("id", MessageArg::text(id))
                .with("stage", MessageArg::text(*stage)),
            Self::RefundTooLarge(amount) => Message::new(crate::messages::REFUND_TOO_LARGE)
                .with("amount", MessageArg::text(amount.to_string())),
            Self::PayoutRecorded(id) => {
                Message::new(crate::messages::PAYOUT_RECORDED).with("id", MessageArg::text(id))
            }
            Self::NotSettled(payment) => Message::new(crate::messages::NOT_SETTLED)
                .with("payment", MessageArg::text(payment)),
            Self::PayoutCurrency { expected, found } => {
                Message::new(crate::messages::PAYOUT_CURRENCY)
                    .with("expected", MessageArg::text(expected.to_string()))
                    .with("found", MessageArg::text(found.to_string()))
            }
            Self::Unbalanced(e) => e.message(),
            Self::Config(e) => e.message(),
            // The composed error already reads as a sentence, and it is the
            // one `sales` or `ledger` wrote — better than anything this module
            // could say about somebody else's rule.
            Self::Sales(why) => Message::new(erp_web::messages::MALFORMED_BODY)
                .with("reason", MessageArg::text(why)),
        }
    }
}

fn problem_for(error: &CommandError<PaymentsError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                // The payment is not there to settle or refund.
                PaymentsError::NotStarted(_) => StatusCode::NOT_FOUND,
                PaymentsError::AlreadyStarted(_) | PaymentsError::PayoutRecorded(_) => {
                    StatusCode::CONFLICT
                }
                // **Well-formed, and refused on what the gateway said.** A 422
                // rather than a 400: nothing about the request was wrong.
                PaymentsError::WrongAmount { .. } | PaymentsError::NotCollectable { .. } => {
                    StatusCode::UNPROCESSABLE_ENTITY
                }
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

        other => {
            tracing::error!(error = %other, "a payments command failed");
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

fn database(error: &sqlx::Error, locale: Locale) -> Problem {
    tracing::error!(error = %error, "a payments read failed");
    Problem::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        &erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
        locale,
        &CATALOG,
    )
}
