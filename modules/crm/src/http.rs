//! The crm module's HTTP surface.
//!
//! Translation only, like every module's. See [`ledger::http`] for why these
//! live in the module rather than in the composition root.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_eventlog::ExecuteError;
use erp_i18n::{Locale, Localize};
use erp_tenant::CommandError;
use erp_types::Timestamp;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::AppState;
use erp_web::Problem;
use erp_web::{After, Allowed, IdempotencyKey, Language, ManageTenant, Paged, Read};
use erp_web::{Consistency, nudge};
use erp_web::{Json, Query, bad_request, creating, metadata, parse_id, require_module};

use crate::{Address, Contact, CrmError, CustomerKind, Details, TaxRegistration};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_customers, register_customer))
        .routes(routes!(get_customer, amend_customer))
        .routes(routes!(archive_customer, restore_customer))
}

/// This module's own failures plus everything any route can produce.
///
/// `crm` depends on no other module, so this composite is the smallest one in
/// the build: its own catalog and the request-level union.
static CATALOG: erp_i18n::Composite =
    erp_i18n::Composite::new(&[&crate::CATALOG, &erp_web::CATALOG]);

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
struct CustomerAddress {
    street: String,
    building: Option<String>,
    district: Option<String>,
    city: String,
    postal_code: Option<String>,
    /// ISO 3166-1 alpha-2.
    country: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct CustomerTaxRegistration {
    /// Fifteen digits, beginning and ending with 3.
    vat_number: String,
    /// ZATCA's `schemeID`, usually `CRN`.
    scheme: Option<String>,
    identifier: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "name": "نجد للاستشارات",
    "name_latin": "Najd Consulting",
    "kind": "company",
    "phone": "+966500000000",
    "vat_number": { "vat_number": "399999999900003", "scheme": "CRN", "identifier": "1010101010" }
}))]
struct NewCustomerRecord {
    name: String,
    name_latin: Option<String>,
    /// `person` or `company`. Only a company may carry a VAT number.
    kind: String,
    phone: Option<String>,
    email: Option<String>,
    address: Option<CustomerAddress>,
    #[serde(default, rename = "vat_number")]
    tax: Option<CustomerTaxRegistration>,
    /// When they became a customer. Defaults to now.
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    registered_on: Option<Timestamp>,
}

/// The same fields without the id, which cannot change.
#[derive(Debug, Deserialize, ToSchema)]
struct AmendCustomerRecord {
    name: String,
    name_latin: Option<String>,
    kind: String,
    phone: Option<String>,
    email: Option<String>,
    address: Option<CustomerAddress>,
    #[serde(default, rename = "vat_number")]
    tax: Option<CustomerTaxRegistration>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct ArchiveCustomerRecord {
    reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct CustomerRecord {
    id: String,
    name: String,
    name_latin: Option<String>,
    kind: String,
    phone: Option<String>,
    email: Option<String>,
    vat_number: Option<String>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    registered_on: Timestamp,
    archived: bool,
}

#[derive(Debug, Serialize, ToSchema)]
struct CustomerRecordDetail {
    #[serde(flatten)]
    customer: CustomerRecord,
    street: Option<String>,
    building: Option<String>,
    district: Option<String>,
    city: Option<String>,
    postal_code: Option<String>,
    country: Option<String>,
    id_scheme: Option<String>,
    identifier: Option<String>,
    archived_why: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct CustomerRegistered {
    id: String,
    /// The log position this landed at. Pass it to a read as
    /// `?consistent_after=` to see it in a list.
    position: Option<i64>,
}

/// Paging, plus the one flag this list needs.
///
/// Deliberately not `ToSchema`: query parameters are declared by hand on each
/// route, the way every other paged route in this build declares them, because
/// `After` is shared and deriving a schema for it here would put a different
/// name on the same three parameters in every module.
#[derive(Debug, Deserialize)]
struct ListQuery {
    #[serde(flatten)]
    page: After,
    /// Include archived customers. Off by default, because a list is what a
    /// clerk works from.
    #[serde(default)]
    archived: bool,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Customers, most recently registered first.
#[utoipa::path(
    get,
    path = "/v1/crm/customers",
    tag = "crm",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Rows per page. Clamped, never refused."),
        ("archived" = Option<bool>, Query, description = "Include archived customers."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, description = "One page. `next` is absent when the list ended.", body = Paged<CustomerRecord>),
        (status = BAD_REQUEST, description = "An unreadable cursor", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable crm", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure, or the projection did not catch up in time. Retryable.", body = Problem),
    ),
)]
async fn list_customers(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<ListQuery>,
) -> Result<Json<Paged<CustomerRecord>>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = query.page.cursor(locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let page = crate::customers(
        &mut conn,
        query.archived,
        query.page.limit(50, 200),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;

    Ok(Json(Paged::of(page, view)))
}

/// Record a customer.
#[utoipa::path(
    post,
    path = "/v1/crm/customers",
    tag = "crm",
    request_body = NewCustomerRecord,
    responses(
        (status = CREATED, body = CustomerRegistered),
        (status = BAD_REQUEST, description = "A missing name, no way to contact them, or a VAT number that is not one", body = Problem),
        (status = CONFLICT, description = "That id is already a customer", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable crm", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure, or the projection did not catch up in time. Retryable.", body = Problem),
    ),
)]
async fn register_customer(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Json(body): Json<NewCustomerRecord>,
) -> Result<(StatusCode, Json<CustomerRegistered>), Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let id = key.id().clone();
    let details = details(
        body.name,
        body.name_latin,
        &body.kind,
        body.phone,
        body.email,
        body.address,
        body.tax,
        locale,
    )?;

    let committed = crate::register_customer(
        &tenant.db,
        &id,
        &details,
        body.registered_on.unwrap_or_else(chrono::Utc::now),
        &creating(&tenant, &key),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(CustomerRegistered {
            id: id.to_string(),
            position: committed.at.map(erp_types::LogPosition::get),
        }),
    ))
}

/// One customer, with everything on the record.
#[utoipa::path(
    get,
    path = "/v1/crm/customers/{customer}",
    tag = "crm",
    params(("customer" = String, Path, description = "The id you registered them under.")),
    responses(
        (status = OK, body = CustomerRecordDetail),
        (status = NOT_FOUND, description = "No such customer, or the projection has not caught up", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure, or the projection did not catch up in time. Retryable.", body = Problem),
    ),
)]
async fn get_customer(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<CustomerRecordDetail>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let found = crate::customer(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?;

    let detail = found.ok_or_else(|| {
        Problem::new(
            StatusCode::NOT_FOUND,
            &erp_i18n::Message::new(crate::messages::NO_SUCH_CUSTOMER)
                .with("customer", erp_i18n::MessageArg::text(id.clone())),
            locale,
            &CATALOG,
        )
    })?;

    Ok(Json(CustomerRecordDetail {
        customer: view(detail.summary),
        street: detail.street,
        building: detail.building,
        district: detail.district,
        city: detail.city,
        postal_code: detail.postal_code,
        country: detail.country,
        id_scheme: detail.id_scheme,
        identifier: detail.identifier,
        archived_why: detail.archived_why,
    }))
}

/// Change what is known about a customer.
#[utoipa::path(
    patch,
    path = "/v1/crm/customers/{customer}",
    tag = "crm",
    params(("customer" = String, Path, description = "The id you registered them under.")),
    request_body = AmendCustomerRecord,
    responses(
        (status = OK, body = CustomerRegistered),
        (status = BAD_REQUEST, description = "A missing name, no way to contact them, or a VAT number that is not one", body = Problem),
        (status = NOT_FOUND, description = "No such customer", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "They are archived", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure, or the projection did not catch up in time. Retryable.", body = Problem),
    ),
)]
async fn amend_customer(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<AmendCustomerRecord>,
) -> Result<Json<CustomerRegistered>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let key = parse_id(&id, locale)?;
    let details = details(
        body.name,
        body.name_latin,
        &body.kind,
        body.phone,
        body.email,
        body.address,
        body.tax,
        locale,
    )?;

    let committed = crate::amend_customer(&tenant.db, &key, &details, &metadata(&tenant))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(CustomerRegistered {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Take a customer out of the lists, keeping every document they are on.
#[utoipa::path(
    post,
    path = "/v1/crm/customers/{customer}/archive",
    tag = "crm",
    params(("customer" = String, Path, description = "The id you registered them under.")),
    request_body = ArchiveCustomerRecord,
    responses(
        (status = OK, body = CustomerRegistered),
        (status = NOT_FOUND, description = "No such customer", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure, or the projection did not catch up in time. Retryable.", body = Problem),
    ),
)]
async fn archive_customer(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<ArchiveCustomerRecord>,
) -> Result<Json<CustomerRegistered>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let key = parse_id(&id, locale)?;
    let committed = crate::archive_customer(&tenant.db, &key, body.reason, &metadata(&tenant))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(CustomerRegistered {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Put them back.
#[utoipa::path(
    delete,
    path = "/v1/crm/customers/{customer}/archive",
    tag = "crm",
    params(("customer" = String, Path, description = "The id you registered them under.")),
    responses(
        (status = OK, body = CustomerRegistered),
        (status = NOT_FOUND, description = "No such customer", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure, or the projection did not catch up in time. Retryable.", body = Problem),
    ),
)]
async fn restore_customer(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
) -> Result<Json<CustomerRegistered>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let key = parse_id(&id, locale)?;
    let committed = crate::restore_customer(&tenant.db, &key, &metadata(&tenant))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(CustomerRegistered {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

fn view(c: crate::CustomerSummary) -> CustomerRecord {
    CustomerRecord {
        id: c.id,
        name: c.name,
        name_latin: c.name_latin,
        kind: c.kind,
        phone: c.phone,
        email: c.email,
        vat_number: c.vat_number,
        registered_on: c.registered_on,
        archived: c.archived,
    }
}

#[expect(clippy::too_many_arguments, reason = "one wire shape, taken apart")]
fn details(
    name: String,
    name_latin: Option<String>,
    kind: &str,
    phone: Option<String>,
    email: Option<String>,
    address: Option<CustomerAddress>,
    tax: Option<CustomerTaxRegistration>,
    locale: Locale,
) -> Result<Details, Problem> {
    let kind: CustomerKind = kind
        .parse()
        .map_err(|_| bad_request(crate::messages::UNKNOWN_KIND, "kind", kind, locale))?;

    Ok(Details {
        name,
        name_latin,
        kind,
        contact: Contact { phone, email },
        address: address.map(|a| Address {
            street: a.street,
            building: a.building,
            district: a.district,
            city: a.city,
            postal_code: a.postal_code,
            country: a.country,
        }),
        tax: tax.map(|t| TaxRegistration {
            vat_number: t.vat_number,
            scheme: t.scheme,
            identifier: t.identifier,
        }),
    })
}

/// Which failure is which, over HTTP.
///
/// The mapping is per module and not shared, because *which rejection is a 409
/// and which is a 422* is exactly the part a shared helper could not decide.
fn problem_for(error: &CommandError<CrmError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                // The id is taken. A different customer was meant.
                // Well-formed, and about somebody who is not there.
                CrmError::NoSuchCustomer(_) => StatusCode::NOT_FOUND,
                // Well-formed, and refused on the state of the record.
                CrmError::Archived(_) => StatusCode::UNPROCESSABLE_ENTITY,
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
            tracing::error!(error = %other, "crm command failed");
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
    tracing::error!(error = %error, "crm read failed");
    Problem::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        &erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
        locale,
        &CATALOG,
    )
}
