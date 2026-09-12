//! Billing a booking: the final invoice, with the deposit deducted.
//!
//! # Why this is here and not in a module
//!
//! The same reason the deposit route is: it is where three modules meet and
//! none of them may name the others. `booking` knows what was done and for
//! whom; `payments` knows which prepayment invoice the deposit raised;
//! `sales` raises the document. `requires` is a hard AND, so a dependency in
//! any direction would force a diary on every shop that invoices, or an
//! invoice book on every salon. `erp-api` is where everything is already
//! assembled, and the worker depends on it, so both the desk's route and the
//! worker's pass run one function.
//!
//! # What the final invoice is
//!
//! A booking with a deposit has already been billed once: settling the deposit
//! raised a **prepayment invoice** (ZATCA 386) for the deposit, because
//! receiving consideration is its own tax point. When the service is delivered
//! the customer owes the rest, and ZATCA's shape for that is a final invoice
//! (388) that shows the whole supply, names the prepayment invoice, and
//! deducts what it declared — band by band — so the deposit's tax is not
//! declared twice. `sales::Draft::prepaid` is that deduction, and `sales`
//! charges and posts only the remainder.
//!
//! # Once, however it is asked
//!
//! The invoice's id is derived from the booking (`bk-<reservation>`), so the
//! desk asking twice, or the desk and the worker asking at once, raise one
//! document: `sales` answers the existing invoice on a retry, and `booking`
//! records the billing once. Both happen in one transaction — an invoice
//! raised and a booking that does not know it is what a worker would bill
//! again.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_control::TenantDb;
use erp_eventlog::Metadata;
use erp_i18n::Locale;
use erp_types::{AggregateId, Timestamp};
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::{Allowed, AppState, Json, Language, PostEntries, Problem};
use erp_web::{parse_id, require_module};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(bill_reservation_route))
}

static CATALOG: erp_i18n::Composite =
    erp_i18n::Composite::new(&[&booking::CATALOG, &sales::CATALOG, &erp_web::CATALOG]);

/// How many completed bookings one worker pass bills.
pub const BATCH: i64 = 50;

/// The invoice a booking was billed on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Billed {
    /// The invoice's id — `bk-<reservation>`, so asking again finds it.
    pub invoice: String,
    /// Its statutory number.
    pub number: String,
    /// The prepayment invoice it deducted, when a deposit had been paid.
    pub deducted: Option<String>,
    /// **Whether this call raised it**, or found it already raised. A retry,
    /// or the worker reading a read model that had not yet caught up, answers
    /// the same invoice and `false`.
    pub raised: bool,
}

/// Why a booking could not be billed.
#[derive(Debug, thiserror::Error)]
pub enum BillingError {
    #[error("there is no booking {0}")]
    NoSuchReservation(String),
    /// The deposit's prepayment invoice is not in the read model yet — a
    /// deposit settled a moment ago. Retryable.
    #[error("the prepayment invoice for booking {0} has not been projected yet")]
    PrepaymentNotYetVisible(String),
    #[error(transparent)]
    Booking(#[from] erp_eventlog::ExecuteError<booking::BookingError>),
    #[error(transparent)]
    Sales(#[from] erp_eventlog::ExecuteError<sales::SalesError>),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Pool(#[from] erp_control::PoolError),
}

/// The id the final invoice for a booking is raised under.
///
/// Fails only for a reservation id so long that the prefix takes it past what
/// an id may be, which a route refuses before it gets here.
pub fn invoice_for(reservation: &AggregateId) -> Result<AggregateId, erp_types::InvalidString> {
    AggregateId::new(format!("bk-{}", reservation.as_str()))
}

/// **Raises the final invoice for a booking**, deducting its deposit.
///
/// Reads what `booking` and `sales` say in their read models — a route or a
/// worker may load no aggregate (L7) — and then, in one transaction, issues
/// the invoice and tells the diary. Idempotent: a booking already billed
/// answers the invoice it has.
///
/// `authority` is the desk's member, or the worker's pass: a member is held to
/// the tenant's document limit, and the worker billing what the business asked
/// to have billed on completion is not.
pub async fn bill_reservation(
    db: &TenantDb,
    reservation: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
    authority: sales::Authority,
) -> Result<Billed, BillingError> {
    let mut conn = db.read().await?;
    let detail = booking::reservation(&mut conn, reservation.as_str())
        .await?
        .ok_or_else(|| BillingError::NoSuchReservation(reservation.to_string()))?;

    // **What the deposit already billed.** The prepayment invoice's id is
    // derived from the payment that secured the slot, the way `payments`
    // derives it; what it declared is read from `sales`' own read model.
    let prepaid = match detail.summary.secured_by.as_deref() {
        Some(payment) => {
            let payment = AggregateId::new(payment)
                .map_err(|_| BillingError::NoSuchReservation(reservation.to_string()))?;
            let id = payments::deposit_invoice(&payment);
            let deposit = sales::invoice(&mut conn, id.as_str())
                .await?
                .ok_or_else(|| BillingError::PrepaymentNotYetVisible(reservation.to_string()))?;
            let bands = sales::bands_of(&mut conn, id.as_str()).await?;
            Some(sales::Prepaid {
                invoice: id,
                number: deposit.summary.number,
                issued_on: deposit.summary.issued_on,
                bands,
            })
        }
        None => None,
    };
    drop(conn);

    let lines: Vec<sales::DraftLine> = detail
        .lines
        .iter()
        .filter_map(|line| {
            let charge = line.charge.as_ref()?;
            Some(sales::DraftLine {
                allowances: Vec::new(),
                description: line.what.clone(),
                net: charge.net,
                category: sales::VatCategory::Standard,
            })
        })
        .collect();
    let currency = lines
        .first()
        .map(|line| line.net.currency())
        .ok_or_else(|| {
            BillingError::Booking(erp_eventlog::ExecuteError::Rejected(
                booking::BookingError::NothingToBill(reservation.to_string()),
            ))
        })?;

    let mut customer = sales::Customer::new(&detail.summary.customer_name);
    customer.id = detail
        .summary
        .customer_id
        .as_deref()
        .and_then(|id| AggregateId::new(id).ok());
    let invoice = invoice_for(reservation)
        .map_err(|_| BillingError::NoSuchReservation(reservation.to_string()))?;
    let draft = sales::Draft {
        customer,
        issued_on: at,
        due_on: None,
        currency,
        lines,
        discounts: Vec::new(),
        prepayment: false,
        prepaid: prepaid.clone(),
        note: format!("Booking {reservation}"),
    };

    let mut tx = db.begin().await?;
    let numbered = sales::issue_in(
        &mut tx,
        &invoice,
        &draft,
        &format!("Booking {reservation} · {}", detail.summary.customer_name),
        metadata,
        authority,
    )
    .await?;
    let raised = numbered.committed.at.is_some();
    booking::bill_in(&mut tx, reservation, &invoice, at, metadata).await?;
    tx.commit().await?;

    Ok(Billed {
        invoice: invoice.to_string(),
        number: numbered.number,
        deducted: prepaid.map(|p| p.number),
        raised,
    })
}

/// **Bills every completed booking nobody has billed**, when the business asks
/// for that. The worker's pass. Answers how many it billed; a booking that
/// cannot be billed is logged and left for a person, and does not stop the
/// rest.
pub async fn bill_completions(
    db: &TenantDb,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<usize, BillingError> {
    let mut conn = db.read().await?;
    let settings = booking::Billing::resolve(&mut conn)
        .await
        .map_err(|e| sqlx::Error::Configuration(e.to_string().into()))?;
    if !settings.on_completion {
        return Ok(0);
    }
    let waiting = booking::unbilled_completions(&mut conn, BATCH).await?;
    drop(conn);

    let mut billed = 0;
    for reservation in waiting {
        // **Nobody is at the desk.** The owner turned billing on completion
        // on, and this pass is that setting doing what it says.
        match bill_reservation(db, &reservation, at, metadata, sales::Authority::System).await {
            // Counted only when something was raised: a read model that has
            // not caught up with the last pass lists the same booking again,
            // and answering it is not work.
            Ok(done) if done.raised => billed += 1,
            // Already billed, or a deposit settled a moment ago whose invoice
            // is not visible yet: nothing now, and the latter next tick.
            Ok(_) | Err(BillingError::PrepaymentNotYetVisible(_)) => {}
            Err(e) => tracing::error!(
                tenant = %db.tenant(),
                %reservation,
                error = %e,
                "a completed booking could not be billed; left for a person"
            ),
        }
    }
    Ok(billed)
}

/// Raise the final invoice for a booking.
///
/// The whole supply, less the deposit: the prepayment invoice the deposit
/// raised is named on the document and what it declared comes off, band by
/// band, so the customer is charged and the authority told about only the
/// rest. Asking again answers the same invoice. A business that wants this
/// done the moment a booking is completed turns it on at
/// `PUT /v1/booking/billing`.
#[utoipa::path(
    post,
    path = "/v1/booking/reservations/{reservation}/invoice",
    tag = "booking",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("reservation" = String, Path, description = "The booking to bill."),
    ),
    responses(
        (status = CREATED, description = "Raised, or already raised — the same invoice either way.", body = Billed),
        (status = BAD_REQUEST, description = "Not an id", body = Problem),
        (status = NOT_FOUND, description = "No such booking, or a module it needs is not enabled", body = Problem),
        (status = CONFLICT, description = "Nothing to bill: no priced line, or the booking was cancelled or a no-show", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not a role that may, or the invoice is over the tenant's document limit (`sales.over_document_limit`)", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "The deposit's invoice is not visible yet, or the database is unwell. Retryable.", body = Problem),
    ),
)]
async fn bill_reservation_route(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(reservation): Path<String>,
) -> Result<(StatusCode, Json<Billed>), Problem> {
    require_module(&tenant.db, &booking::module_id(), locale)?;
    require_module(&tenant.db, &sales::module_id(), locale)?;
    let reservation = parse_id(&reservation, locale)?;

    let billed = bill_reservation(
        &tenant.db,
        &reservation,
        chrono::Utc::now(),
        &erp_web::metadata(&tenant),
        sales::Authority::of(&tenant.db),
    )
    .await
    .map_err(|e| problem(&e, locale))?;

    erp_web::nudge(&state, tenant.db.tenant()).await;
    Ok((StatusCode::CREATED, Json(billed)))
}

fn problem(error: &BillingError, locale: Locale) -> Problem {
    match error {
        BillingError::NoSuchReservation(id) => Problem::new(
            StatusCode::NOT_FOUND,
            &erp_i18n::Message::new(booking::messages::NO_SUCH_RESERVATION)
                .with("reservation", erp_i18n::MessageArg::text(id)),
            locale,
            &CATALOG,
        ),
        BillingError::Booking(erp_eventlog::ExecuteError::Rejected(refused)) => {
            Problem::from_error(StatusCode::CONFLICT, refused, locale, &CATALOG)
        }
        BillingError::Sales(erp_eventlog::ExecuteError::Rejected(refused)) => Problem::from_error(
            if refused.refuses_the_caller() {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::CONFLICT
            },
            refused,
            locale,
            &CATALOG,
        ),
        BillingError::Database(error) => {
            tracing::warn!(%error, "a booking could not be billed");
            erp_web::ApiError::Access(erp_control::AccessError::Database(sqlx::Error::Protocol(
                error.to_string(),
            )))
            .into_problem(locale, &CATALOG)
        }
        // A deposit settled a moment ago, a pool under pressure, or a
        // refusal this route did not expect: retryable, and said so.
        other => {
            tracing::warn!(error = %other, "a booking could not be billed");
            Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                &erp_i18n::Message::new(erp_web::messages::MALFORMED_BODY)
                    .with("reason", erp_i18n::MessageArg::text("try again")),
                locale,
                &CATALOG,
            )
        }
    }
}
