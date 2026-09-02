//! This module's HTTP surface.
//!
//! Translation only, like every module's — see [`ledger::http`] for why these
//! live in the module rather than in the composition root.
//!
//! **Every route here is a GET**, because this module has no commands. It says
//! nothing about the world that the log did not already say; it only reads what
//! its own group has built from it.

use axum::http::StatusCode;
use erp_i18n::{Locale, Localize, Message, MessageArg};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::ApiError;
use erp_web::AppState;
use erp_web::Problem;
use erp_web::{Allowed, Consistency, Language, ManageAccounts, Read};
use erp_web::{Json, Query, require_module};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(revenue))
        .routes(routes!(utilisation))
        .routes(routes!(takings))
        .routes(routes!(people_cost))
        .routes(routes!(reconciliation))
}

/// **What this module's routes can answer with.**
///
/// Its own failures and everything any route can produce. It surfaces no other
/// module's messages, because it calls no other module's commands — the only
/// thing it does with `sales`, `booking`, `pos`, `payroll` and `ledger` is
/// decode their events.
static CATALOG: erp_i18n::Composite =
    erp_i18n::Composite::new(&[&crate::CATALOG, &erp_web::CATALOG]);

// ---------------------------------------------------------------------------
// The period range
// ---------------------------------------------------------------------------

/// Why a range is not one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReportError {
    #[error("{0} is not a month")]
    NotAPeriod(String),
    #[error("a report may cover at most {0} months")]
    TooLong(i64),
    #[error("{0} is later than {1}")]
    Backwards(String, String),
}

impl Localize for ReportError {
    fn message(&self) -> Message {
        match self {
            Self::NotAPeriod(raw) => {
                Message::new(crate::messages::NOT_A_PERIOD).with("period", MessageArg::text(raw))
            }
            Self::TooLong(months) => {
                Message::new(crate::messages::RANGE_TOO_LONG).with("n", MessageArg::Count(*months))
            }
            Self::Backwards(from, until) => Message::new(crate::messages::BACKWARDS)
                .with("from", MessageArg::text(from))
                .with("until", MessageArg::text(until)),
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
struct Range {
    /// The first month, inclusive. `2026-01`.
    from: String,
    /// The last month, **inclusive**. A report is read by month, and an
    /// exclusive end would make "January to December" end in the next year —
    /// which is how a chart comes to be missing December.
    until: String,
}

/// A month as an ordinal, for the length check. `2026-05` is `24317`.
///
/// Integer arithmetic, like everything in this workspace that computes with
/// numbers a person will reconcile.
fn ordinal(raw: &str) -> Result<i64, ReportError> {
    let bad = || ReportError::NotAPeriod(raw.to_owned());
    let (year, month) = raw.split_once('-').ok_or_else(bad)?;
    if year.len() != 4 || month.len() != 2 {
        return Err(bad());
    }
    let year: i64 = year.parse().map_err(|_| bad())?;
    let month: i64 = month.parse().map_err(|_| bad())?;
    if !(1..=12).contains(&month) {
        return Err(bad());
    }
    Ok(year * 12 + month)
}

/// Checks a range and hands back its two ends.
fn range_of(range: &Range) -> Result<(&str, &str), ReportError> {
    let from = ordinal(&range.from)?;
    let until = ordinal(&range.until)?;

    if until < from {
        return Err(ReportError::Backwards(
            range.from.clone(),
            range.until.clone(),
        ));
    }
    if until - from >= crate::MONTHS {
        return Err(ReportError::TooLong(crate::MONTHS));
    }
    Ok((&range.from, &range.until))
}

fn refused(error: &ReportError, locale: Locale) -> Problem {
    Problem::new(StatusCode::BAD_REQUEST, &error.message(), locale, &CATALOG)
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
struct RevenueView {
    /// `2026-05`.
    period: String,
    /// Empty on a business with no branches, which is honest about there being
    /// none rather than inventing a name for it.
    branch: String,
    /// **Net of credit notes**, in minor units. What the business kept.
    net: i64,
    tax: i64,
    /// Documents issued in this period.
    documents: i32,
    /// Documents credited in this period. A period can show both, and the pair
    /// is a different fact from a single net figure.
    credited: i32,
}

#[derive(Debug, Serialize, ToSchema)]
struct UtilisationView {
    period: String,
    resource: String,
    booked: i32,
    completed: i32,
    /// **Counted, not inferred**: a no-show is a stage somebody moved a booking
    /// to. Deriving it from "booked and not completed" would count everything
    /// still in the diary the moment a month ended.
    no_shows: i32,
    cancelled: i32,
    /// Diary minutes the completed work took — the denominator for revenue per
    /// resource-hour.
    minutes: i64,
    /// No-shows as a share of what was booked, in **basis points**. Integer, so
    /// it is the same number on every client.
    no_show_rate_bp: i32,
    /// Average notice a booking gave, in minutes.
    average_lead_minutes: i64,
}

#[derive(Debug, Serialize, ToSchema)]
struct TakingsView {
    period: String,
    /// Whoever had the till open. An identity, not a `crm` record.
    operator: String,
    /// `cash`, `card` or `transfer`.
    method: String,
    taken: i64,
    refunded: i64,
    /// **What the drawer disagreed by**, summed over the shifts this operator
    /// closed. Negative is short.
    variance: i64,
    /// Cash out that was not a refund — a banking run, a float moved, a
    /// supplier paid in notes. The closest the log comes to "what was banked",
    /// and named for what it is: nothing here has seen a bank statement.
    paid_out: i64,
    /// Shifts this operator **closed**. A till still taking money has no
    /// variance yet.
    shifts: i32,
}

#[derive(Debug, Serialize, ToSchema)]
struct PeopleCostView {
    period: String,
    gross: i64,
    /// Part of `gross`, not on top of it.
    commission: i64,
    deductions: i64,
    net: i64,
    people: i32,
}

#[derive(Debug, Serialize, ToSchema)]
struct ReconciliationView {
    /// **True is the only acceptable answer.** See below.
    reconciles: bool,
    /// One line each, empty when it reconciles.
    discrepancies: Vec<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// What was sold, by month and branch.
///
/// **Net of credit notes**: a cancelled invoice takes its own numbers back out,
/// in the month the credit was dated to. A December invoice credited in January
/// is December revenue that January took back — moving it would restate a month
/// somebody has already filed a return against.
#[utoipa::path(
    get,
    path = "/v1/reports/revenue",
    tag = "reports",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("from" = String, Query, description = "First month, inclusive. `2026-01`."),
        ("until" = String, Query, description = "Last month, inclusive. `2026-12`."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<RevenueView>),
        (status = BAD_REQUEST, description = "Not a month, backwards, or longer than ten years", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, not yours, or the reports module is not enabled here", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn revenue(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(range): Query<Range>,
) -> Result<Json<Vec<RevenueView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let (from, until) = range_of(&range).map_err(|e| refused(&e, locale))?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let rows = crate::revenue(&mut conn, from, until)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    Ok(Json(
        rows.into_iter()
            .map(|row| RevenueView {
                period: row.period,
                branch: row.branch,
                net: row.net.minor(),
                tax: row.tax.minor(),
                documents: row.documents,
                credited: row.credited,
            })
            .collect(),
    ))
}

/// How well the diary was used, by month and resource.
#[utoipa::path(
    get,
    path = "/v1/reports/utilisation",
    tag = "reports",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("from" = String, Query, description = "First month, inclusive. `2026-01`."),
        ("until" = String, Query, description = "Last month, inclusive. `2026-12`."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<UtilisationView>),
        (status = BAD_REQUEST, description = "Not a month, backwards, or longer than ten years", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn utilisation(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(range): Query<Range>,
) -> Result<Json<Vec<UtilisationView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let (from, until) = range_of(&range).map_err(|e| refused(&e, locale))?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let rows = crate::utilisation(&mut conn, from, until)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    Ok(Json(
        rows.into_iter()
            .map(|row| UtilisationView {
                no_show_rate_bp: row.no_show_rate_bp(),
                average_lead_minutes: row.average_lead_minutes(),
                period: row.period,
                resource: row.resource,
                booked: row.booked,
                completed: row.completed,
                no_shows: row.no_shows,
                cancelled: row.cancelled,
                minutes: row.minutes,
            })
            .collect(),
    ))
}

/// What the tills took, by month, person and method.
#[utoipa::path(
    get,
    path = "/v1/reports/takings",
    tag = "reports",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("from" = String, Query, description = "First month, inclusive. `2026-01`."),
        ("until" = String, Query, description = "Last month, inclusive. `2026-12`."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<TakingsView>),
        (status = BAD_REQUEST, description = "Not a month, backwards, or longer than ten years", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn takings(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(range): Query<Range>,
) -> Result<Json<Vec<TakingsView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let (from, until) = range_of(&range).map_err(|e| refused(&e, locale))?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let rows = crate::takings(&mut conn, from, until)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    Ok(Json(
        rows.into_iter()
            .map(|row| TakingsView {
                period: row.period,
                operator: row.operator,
                method: row.method,
                taken: row.taken.minor(),
                refunded: row.refunded.minor(),
                variance: row.variance.minor(),
                paid_out: row.paid_out.minor(),
                shifts: row.shifts,
            })
            .collect(),
    ))
}

/// What people cost, by month. **Approved payroll runs only** — a draft is not
/// a cost, and counting one would make this move when somebody opened a screen.
#[utoipa::path(
    get,
    path = "/v1/reports/people-cost",
    tag = "reports",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("from" = String, Query, description = "First month, inclusive. `2026-01`."),
        ("until" = String, Query, description = "Last month, inclusive. `2026-12`."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<PeopleCostView>),
        (status = BAD_REQUEST, description = "Not a month, backwards, or longer than ten years", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "What people are paid is not a figure every viewer may read", body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn people_cost(
    // **Not `Read`.** Every other report on this router is a business figure a
    // viewer may see; the wage bill is what the people in the room are paid,
    // and a receptionist with a dashboard should not be able to total it.
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    consistency: Consistency,
    Query(range): Query<Range>,
) -> Result<Json<Vec<PeopleCostView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let (from, until) = range_of(&range).map_err(|e| refused(&e, locale))?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let rows = crate::people_cost(&mut conn, from, until)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    Ok(Json(
        rows.into_iter()
            .map(|row| PeopleCostView {
                period: row.period,
                gross: row.gross.minor(),
                commission: row.commission.minor(),
                deductions: row.deductions.minor(),
                net: row.net.minor(),
                people: row.people,
            })
            .collect(),
    ))
}

/// **Whether these figures agree with the books.**
///
/// The invariant behind every other route here. It compares what this module
/// says was invoiced against the journal entries those documents posted, and
/// checks that every currency's postings still sum to zero — both against this
/// group's **own** copy of the ledger, at its own checkpoint, so a
/// disagreement is a disagreement and never one group being behind another.
///
/// A discrepancy is a failure, not a coloured cell (L6). This route reports
/// them; the worker's health check refuses a tenant that has any.
#[utoipa::path(
    get,
    path = "/v1/reports/reconciliation",
    tag = "reports",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, description = "`reconciles` is true and the list is empty, or something disagrees", body = ReconciliationView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn reconciliation(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    consistency: Consistency,
) -> Result<Json<ReconciliationView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let found = crate::reconciles(&mut conn)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    Ok(Json(ReconciliationView {
        reconciles: found.is_empty(),
        discrepancies: found.iter().map(crate::Discrepancy::describe).collect(),
    }))
}
