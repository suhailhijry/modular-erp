//! Taking the deposit a public booking was asked for.
//!
//! # Why this route is here and not in a module
//!
//! Because it is the one place three modules have to meet, and none of them may
//! name the others. A deposit needs `booking` to say what was asked for,
//! `ledger` to say what rate it is taxed at, and `payments` to name a charge
//! the customer can pay — and `requires` is a hard AND, so a dependency in any
//! direction forces a diary on every shop that takes a card, or a gateway on
//! every salon that keeps one.
//!
//! Modules ship their own routes, and this is the exception the rule needed:
//! `erp-api` is where everything is already assembled, which makes it the only
//! honest home for a route that is about the seam rather than about a module.
//!
//! # What a caller cannot decide
//!
//! **The amount.** It is worked out from the reservation — the fraction the
//! business set, of what the booking was priced at, taxed at the rate the
//! tenant has configured — and the request carries no money at all. That is
//! what makes an unauthenticated route safe to have: the worst a stranger can
//! do is create a charge they would have to pay themselves.
//!
//! **Which payment.** The `Idempotency-Key` becomes the payment's id and is
//! passed to the gateway as its own, so the customer's browser pays *this*
//! charge rather than reporting back about one it chose. Without that, anybody
//! who learned a payment id could attach a stranger's money to their booking.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_i18n::Locale;
use erp_types::Timestamp;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::{AppState, IdempotencyKey, Json, Language, Problem, Public};
use erp_web::{parse_id, publicly, require_module};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(public_deposit))
}

static CATALOG: erp_i18n::Composite = erp_i18n::Composite::new(&[
    &booking::CATALOG,
    &payments::CATALOG,
    &ledger::CATALOG,
    &erp_web::CATALOG,
]);

/// What the customer has to pay, and the charge to pay it against.
#[derive(Debug, Serialize, ToSchema)]
struct DepositDue {
    /// **The charge to pay.** Give this to the gateway's form as its own id —
    /// Moyasar's `given_id` — so what the customer pays is this charge and not
    /// one the browser chose.
    payment: String,
    /// Minor units, tax included. Worked out here; the caller sends no amount.
    amount: i64,
    /// ISO-4217, three letters.
    currency: String,
    /// What it comes to before tax, and the tax on it, so a page can show the
    /// customer the breakdown it is about to charge them.
    net: i64,
    tax: i64,
    /// **When the slot stops being held.** Nothing is reserved for somebody who
    /// has not paid by then.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    due_by: Timestamp,
}

/// Ask for the charge that holds a public booking.
///
/// # It takes no money and no amount
///
/// It creates a charge and answers with its id. What the customer pays is
/// worked out from the booking — the fraction the business asked for, of what
/// the booking was priced at, with tax — so nothing in the request decides what
/// anybody is charged.
///
/// The browser then pays that charge against the gateway's publishable key,
/// giving it this `payment` as its own id. This system finds out by asking the
/// gateway, not by being told: a callback is a doorbell, and what a browser
/// reports is not evidence.
///
/// # It answers the same thing twice
///
/// Sending the same `Idempotency-Key` again returns the same charge rather than
/// making a second one, which is what a customer who reloaded the page needs.
#[utoipa::path(
    post,
    path = "/v1/booking/public/reservations/{reservation}/deposit",
    tag = "booking",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — which is how a public request names the business."),
        ("reservation" = String, Path, description = "From `POST /v1/booking/public/reservations`."),
        ("Idempotency-Key" = String, Header, description = "A UUID. Becomes the charge's id and the gateway's, so a reload pays the same charge."),
    ),
    security(),
    responses(
        (status = CREATED, body = DepositDue),
        (status = BAD_REQUEST, description = "Not an id, or a key that is not a UUID", body = Problem),
        (status = NOT_FOUND, description = "No such business, it does not take bookings online, or that booking is not waiting to be paid", body = Problem),
        (status = TOO_MANY_REQUESTS, description = "This surface is bounded per origin and per business.", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn public_deposit(
    caller: Public,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Path(reservation): Path<String>,
) -> Result<(StatusCode, Json<DepositDue>), Problem> {
    require_module(&caller.db, &booking::module_id(), locale)?;
    require_module(&caller.db, &payments::module_id(), locale)?;
    let reservation = parse_id(&reservation, locale)?;

    // **Off unless the business turned it on**, and a 404 rather than a 403 for
    // the reason the reservation route gives: "forbidden" would confirm the
    // route works for somebody else, which is neither true nor their business.
    let settings = {
        let mut conn = caller
            .db
            .read()
            .await
            .map_err(|e| unavailable(&e, locale))?;
        booking::PublicBooking::resolve(&mut conn)
            .await
            .map_err(|e| {
                Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &CATALOG)
            })?
    };
    if !settings.open {
        return Err(nothing_here(locale));
    }

    // **Moyasar's rule, said here rather than discovered in the worker.** The
    // key becomes `given_id`, which must be a UUID.
    if !is_uuid(key.id().as_str()) {
        return Err(erp_web::bad_request(
            erp_web::messages::MALFORMED_BODY,
            "reason",
            "the Idempotency-Key must be a UUID: it becomes the gateway's own id for the charge",
            locale,
        ));
    }

    let (owed, buyer) = {
        let mut conn = caller
            .db
            .read()
            .await
            .map_err(|e| unavailable(&e, locale))?;
        // `None` when the booking does not exist, is not held, or has already
        // been paid for. All three are the same answer to a stranger.
        let owed = booking::awaiting_deposit(&mut conn, reservation.as_str())
            .await
            .map_err(|e| unavailable(&e, locale))?
            .ok_or_else(|| nothing_here(locale))?;
        let detail = booking::reservation(&mut conn, reservation.as_str())
            .await
            .map_err(|e| unavailable(&e, locale))?
            .ok_or_else(|| nothing_here(locale))?;
        (owed, detail.summary.customer_name)
    };

    let (amount, tax) = {
        let mut conn = caller
            .db
            .read()
            .await
            .map_err(|e| unavailable(&e, locale))?;
        with_tax(&mut conn, owed.deposit, locale).await?
    };

    let mut tx = caller
        .db
        .begin()
        .await
        .map_err(|e| unavailable(&e, locale))?;
    payments::request_in(
        &mut tx,
        key.id(),
        &payments::Collection {
            // **No card.** Nobody here charges this; the customer's browser
            // creates it against the publishable key, and the worker asks the
            // gateway whether they have.
            card: None,
            provider: "moyasar".to_owned(),
            collects: payments::Collects::Advance(payments::Advance {
                against: reservation,
                net: owed.deposit,
                buyer: payments::Buyer {
                    name: buyer,
                    vat_number: None,
                },
            }),
            amount,
            // **Empty, and it is not needed here.** A callback URL is where
            // the *worker* sends a customer when it creates a charge for them;
            // this charge is created in the customer's own browser, so where
            // they land afterwards is the site's business and never reaches
            // this system.
            callback_url: String::new(),
        },
        chrono::Utc::now(),
        &publicly(&key),
    )
    .await
    .map_err(|_| nothing_here(locale))?;
    tx.commit().await.map_err(|e| unavailable(&e, locale))?;

    erp_web::nudge(&state, caller.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(DepositDue {
            payment: key.id().to_string(),
            amount: amount.minor(),
            currency: amount.currency().to_string(),
            net: owed.deposit.minor(),
            tax: tax.minor(),
            due_by: owed.due_by,
        }),
    ))
}

/// What a deposit comes to with tax, at the rate the tenant has configured.
///
/// **Worked out the way the invoice will be.** The prepayment invoice this
/// becomes is raised from the net, and settling refuses if what the customer was
/// charged is not what that invoice comes to — so the two have to agree, and
/// the way to make them agree is to run the tax forwards from the same net
/// rather than backwards out of a total.
async fn with_tax(
    conn: &mut sqlx::PgConnection,
    net: erp_types::Money,
    locale: Locale,
) -> Result<(erp_types::Money, erp_types::Money), Problem> {
    let rates = ledger::Rates::resolve(&mut *conn)
        .await
        .map_err(|e| Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &CATALOG))?;
    let tax = net
        .scaled_by(rates.of(ledger::VatCategory::Standard))
        .map_err(|_| nothing_here(locale))?;
    let amount = net.checked_add(tax).map_err(|_| nothing_here(locale))?;
    Ok((amount, tax))
}

/// **A 404 for every way this can fail to apply.** No such booking, one already
/// paid for, one whose hold lapsed — a stranger learns the same thing from all
/// of them, which is nothing.
fn nothing_here(locale: Locale) -> Problem {
    Problem::new(
        StatusCode::NOT_FOUND,
        &erp_i18n::Message::new(erp_web::messages::MODULE_NOT_ENABLED)
            .with("module", erp_i18n::MessageArg::text("booking")),
        locale,
        &CATALOG,
    )
}

fn unavailable(error: &impl std::fmt::Display, locale: Locale) -> Problem {
    tracing::warn!(error = %error, "a public deposit could not be prepared");
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        &erp_i18n::Message::new(erp_web::messages::MALFORMED_BODY)
            .with("reason", erp_i18n::MessageArg::text("try again")),
        locale,
        &CATALOG,
    )
}

/// The shape `erp_payments::moyasar` refuses on, checked at the edge.
fn is_uuid(value: &str) -> bool {
    let mut parts = value.split('-');
    for width in [8, 4, 4, 4, 12] {
        let Some(part) = parts.next() else {
            return false;
        };
        if part.len() != width || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}
