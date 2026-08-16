//! The Saudi tax module's HTTP surface.
//!
//! Translation only, like every other module's. What is different is where the
//! *domain* went: the return used to be netted here, in the composition root,
//! and it is computed in `tax_sa::vat_return` now. This file asks which modules
//! the tenant has, calls the module, and renders.
//!
//! ponytail: still in `spa-api`, like `ledger_routes` and the rest. Under the
//! core/module split a module ships its own routes and core mounts them — which
//! is the restructure this file is waiting for rather than an argument against
//! it.

use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use spa_control::CommandError;
use spa_eventlog::ExecuteError;
use spa_i18n::{Locale, Localize};
use spa_types::{CurrencyCode, Timestamp};
use tax_sa::{Sides, TaxError};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::consistency::{Consistency, nudge};
use crate::error::ApiError;
use crate::extract::{Allowed, Language, ManageAccounts, Read};
use crate::problem::Problem;
use crate::state::AppState;
use crate::wire::{Json, Query, bad_request, metadata, require_module};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(vat_return))
        .routes(routes!(filed_returns, file_return))
}

/// How many filed returns a list gives back. A business files four a year.
const PAGE: i64 = 200;

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
struct BandView {
    vat: &'static str,
    vat_rate: i32,
    net: i64,
    tax: i64,
    /// Documents with a tax point in this period. Invoices and credit notes on
    /// the output side; bills on the input side.
    documents: i64,
}

#[derive(Debug, Serialize, ToSchema)]
struct SideView {
    bands: Vec<BandView>,
    net: i64,
    tax: i64,
}

#[derive(Debug, Serialize, ToSchema)]
struct ReturnView {
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    from: Timestamp,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
    currency: String,
    /// What was charged on sales, net of credit notes with a tax point in this
    /// period. Empty when the tenant has no sales module.
    output: SideView,
    /// What was paid on purchases, and the reclaimable part of it. Empty when
    /// the tenant has no purchases module.
    input: SideView,
    /// **The number that gets paid, or reclaimed.** Output tax less input tax;
    /// negative means ZATCA owes the business.
    payable: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
struct Period {
    /// Inclusive.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    from: Timestamp,
    /// **Exclusive**, so consecutive returns neither overlap nor leave a day out.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
    currency: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "from": "2026-01-01T00:00:00Z",
    "until": "2026-04-01T00:00:00Z",
    "currency": "SAR",
    "filed_on": "2026-04-28T00:00:00Z"
}))]
struct NewFiling {
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    from: Timestamp,
    /// **Exclusive.**
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
    currency: String,
    /// The date the business treats the filing as made. Not a clock reading, for
    /// the same reason a tax point is not one.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    filed_on: Timestamp,
}

#[derive(Debug, Serialize, ToSchema)]
struct FiledView {
    /// The period, as it identifies itself: `SAR.2026-01-01.2026-04-01`.
    period: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    from: Timestamp,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
    output_tax: i64,
    input_tax: i64,
    payable: i64,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    filed_on: Timestamp,
    /// ZATCA's acknowledgement, once clearance exists to produce one.
    reference: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// The VAT return for a period: what was charged, what was paid, the difference.
///
/// Each document is reported in the period of **its own tax point** — an invoice
/// on its issue date, a credit note on its credit date, a bill on the date the
/// supplier stated. Re-running a filed period gives the number that was filed.
///
/// A tenant with only one of sales and purchases gets zeroes for the other side
/// rather than a 404: a business that has not enabled purchases genuinely
/// reclaimed nothing, and that is a return they can file.
#[utoipa::path(
    get,
    path = "/v1/tenants/{slug}/tax_sa/vat-return",
    tag = "tax_sa",
    params(
        ("slug" = String, Path, description = "The tenant's name in URLs."),
        ("from" = String, Query, description = "Start of the period, inclusive. RFC 3339."),
        ("until" = String, Query, description = "End of the period, **exclusive**. RFC 3339."),
        ("currency" = String, Query, description = "ISO 4217."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = ReturnView),
        (status = BAD_REQUEST, description = "An unknown currency, or a period that ends before it starts", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, not yours, or the tax_sa module is not enabled here", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn vat_return(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(period): Query<Period>,
) -> Result<Json<ReturnView>, Problem> {
    require_module(&tenant, &tax_sa::module_id(), locale)?;
    let (currency, from, until) = period_of(&period.currency, period.from, period.until, locale)?;

    let sides = sides_of(&tenant);
    // Waited on per side, so a tenant with one module is not made to wait for a
    // projection that will never run.
    if sides.sells {
        consistency
            .wait_for(&tenant.db, sales::GROUP_NAME, locale)
            .await?;
    }
    if sides.buys {
        consistency
            .wait_for(&tenant.db, purchases::GROUP_NAME, locale)
            .await?;
    }

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    let declared = tax_sa::vat_return(&mut conn, sides, currency, from, until)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    drop(conn);

    Ok(Json(view(&declared)))
}

/// Record that a period was filed, with the numbers that went.
///
/// Filing the same period twice is a **conflict**, not a no-op: a second filing
/// is an amendment, which is a different document with its own rules.
#[utoipa::path(
    post,
    path = "/v1/tenants/{slug}/tax_sa/returns",
    tag = "tax_sa",
    params(("slug" = String, Path, description = "The tenant's name in URLs.")),
    request_body = NewFiling,
    responses(
        (status = CREATED, description = "Filed. The numbers are recorded as they stood.", body = FiledView),
        (status = BAD_REQUEST, description = "An unknown currency, or a period that ends before it starts", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "The period was already filed — correcting it is an amendment", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn file_return(
    tenant: Allowed<ManageAccounts>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewFiling>,
) -> Result<(StatusCode, Json<FiledView>), Problem> {
    require_module(&tenant, &tax_sa::module_id(), locale)?;
    let (currency, from, until) = period_of(&body.currency, body.from, body.until, locale)?;

    let filed = tax_sa::file_return(
        &tenant.db,
        sides_of(&tenant),
        currency,
        from,
        until,
        body.filed_on,
        &metadata(&tenant),
    )
    .await
    .map_err(|e| tax_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    let period = tax_sa::period_id(currency, from, until)
        .map(|id| id.as_str().to_owned())
        .unwrap_or_default();

    Ok((
        StatusCode::CREATED,
        Json(FiledView {
            period,
            from,
            until,
            output_tax: 0,
            input_tax: 0,
            payable: filed.payable.minor(),
            filed_on: filed.filed_on,
            reference: None,
        }),
    ))
}

/// Every return this business has filed.
#[utoipa::path(
    get,
    path = "/v1/tenants/{slug}/tax_sa/returns",
    tag = "tax_sa",
    params(
        ("slug" = String, Path, description = "The tenant's name in URLs."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<FiledView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn filed_returns(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
) -> Result<Json<Vec<FiledView>>, Problem> {
    require_module(&tenant, &tax_sa::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, tax_sa::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    let returns = tax_sa::filed(&mut conn, PAGE)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    drop(conn);

    Ok(Json(
        returns
            .into_iter()
            .map(|r| FiledView {
                period: r.period,
                from: r.from,
                until: r.until,
                output_tax: r.output_tax.minor(),
                input_tax: r.input_tax.minor(),
                payable: r.payable.minor(),
                filed_on: r.filed_on,
                reference: r.reference,
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------

/// Which sides of the return this tenant contributes to.
fn sides_of<C: crate::extract::Capability>(tenant: &Allowed<C>) -> Sides {
    Sides {
        sells: tenant.db.has_module(&sales::module_id()),
        buys: tenant.db.has_module(&purchases::module_id()),
    }
}

fn period_of(
    currency: &str,
    from: Timestamp,
    until: Timestamp,
    locale: Locale,
) -> Result<(CurrencyCode, Timestamp, Timestamp), Problem> {
    let parsed = CurrencyCode::new(currency).map_err(|_| {
        bad_request(
            crate::messages::UNKNOWN_CURRENCY,
            "currency",
            currency,
            locale,
        )
    })?;
    if until <= from {
        return Err(bad_request(
            crate::messages::EMPTY_PERIOD,
            "period",
            &from.to_rfc3339(),
            locale,
        ));
    }
    Ok((parsed, from, until))
}

fn view(declared: &tax_sa::Return) -> ReturnView {
    let side = |s: &tax_sa::Side| SideView {
        bands: s
            .bands
            .iter()
            .map(|b| BandView {
                vat: b.category.as_str(),
                vat_rate: b.basis_points,
                net: b.net.minor(),
                tax: b.tax.minor(),
                documents: b.documents,
            })
            .collect(),
        net: s.net.minor(),
        tax: s.tax.minor(),
    };

    ReturnView {
        from: declared.from,
        until: declared.until,
        currency: declared.currency.to_string(),
        output: side(&declared.output),
        input: side(&declared.input),
        payable: declared.payable.minor(),
    }
}

/// Maps a command failure onto a status.
fn tax_problem(error: &CommandError<TaxError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                // Already filed. Look at what is there and decide whether an
                // amendment is what you meant.
                TaxError::AlreadyFiled { .. } => StatusCode::CONFLICT,
                TaxError::Read(_) => StatusCode::INTERNAL_SERVER_ERROR,
                _ => StatusCode::BAD_REQUEST,
            },
            rejection.message(),
        ),

        CommandError::Pool(e @ spa_control::PoolError::Overloaded { .. }) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.message())
        }

        CommandError::Execute(ExecuteError::Contended { .. }) => (
            StatusCode::CONFLICT,
            spa_i18n::Message::new(spa_eventlog::messages::CONCURRENT_MODIFICATION),
        ),

        other => {
            tracing::error!(error = %other, "tax command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                spa_i18n::Message::new(spa_control::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &crate::catalog::CATALOG)
}
