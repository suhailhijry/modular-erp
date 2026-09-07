//! The crm module's HTTP surface.
//!
//! Translation only, like every module's. See [`ledger::http`] for why these
//! live in the module rather than in the composition root.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_eventlog::ExecuteError;
use erp_i18n::{Catalog as _, Locale, Localize};
use erp_tenant::CommandError;
use erp_types::Timestamp;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::AppState;
use erp_web::Problem;
use erp_web::csv::{Imported, Rejected};
use erp_web::{After, Allowed, IdempotencyKey, Language, ManageTenant, Paged, PostEntries, Read};
use erp_web::{Consistency, nudge};
use erp_web::{IfMatch, Versioned, config_problem};
use erp_web::{Json, Query, bad_request, creating, importing, metadata, parse_id, require_module};

use crate::{Address, Contact, CrmError, CustomerKind, Details, TaxRegistration};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_customers, register_customer))
        .routes(routes!(get_customer, amend_customer))
        .routes(routes!(archive_customer, restore_customer))
        .routes(routes!(import_customers))
        .routes(routes!(customer_fields, set_customer_fields))
        .routes(routes!(orphaned_fields))
        .routes(routes!(erase_field_values))
        .routes(routes!(held_fields, set_held_fields, erase_held_fields))
        .routes(routes!(clear_held_field))
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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

/// **Import customers from a spreadsheet.**
///
/// # Partial failure is the outcome, not an exception
///
/// A thousand-row file with three bad rows imports 997 and returns the three,
/// with the row number the person's editor is showing them and what was wrong.
/// The alternative — refuse the file — is what every import in this category
/// does, and it means somebody fixing a spreadsheet by bisection.
///
/// # Re-uploading a corrected file is safe
///
/// Each row is its own command under a key derived from the file's key **and**
/// the row's id, so the 997 that went in the first time are recognised as
/// retries rather than duplicated. See `erp_web::importing`.
///
/// # Columns
///
/// `id` and `name` are required; `kind` defaults to `person`. The rest —
/// `name_latin`, `phone`, `email`, `vat_number`, `vat_scheme`,
/// `vat_identifier`, `street`, `building`, `district`, `city`, `postal_code`,
/// `country` — are taken when present. A column this does not know is ignored,
/// because a spreadsheet exported from somewhere else always has three.
#[utoipa::path(
    post,
    path = "/v1/crm/customers/import",
    tag = "crm",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("Idempotency-Key" = String, Header, description = "The key for this file. Re-uploading a corrected version under the same key does not duplicate the rows that already went in."),
        ("Content-Type" = String, Header, description = "`text/csv`."),
    ),
    request_body(content = String, description = "The spreadsheet. A header row, then one customer per row.", content_type = "text/csv"),
    responses(
        (status = OK, description = "What went in and what did not. **A 200 with rejected rows is the normal outcome**, not an error.", body = Imported),
        (status = BAD_REQUEST, description = "Not a spreadsheet, no header row, or more rows than one upload takes", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn import_customers(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    body: axum::body::Bytes,
) -> Result<Json<Imported>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let rows = erp_web::csv::parse(&body).map_err(|e| {
        bad_request(
            crate::messages::UNREADABLE_FILE,
            "reason",
            &e.to_string(),
            locale,
        )
    })?;

    let mut imported = 0;
    let mut rejected = Vec::new();

    for (index, row) in rows.iter().enumerate() {
        // The header is row 1, so the first customer is row 2 — which is the
        // number the person's editor is showing them.
        let number = index + 2;
        let id = row.get("id").cloned().unwrap_or_default();

        match one(&tenant, &key, row, &id, locale).await {
            Ok(()) => imported += 1,
            Err(problem) => rejected.push(Rejected {
                row: number,
                code: problem.0,
                detail: problem.1,
            }),
        }
    }

    // Once, at the end. A nudge per row would ask for a visit a thousand times
    // for one file.
    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(Imported { imported, rejected }))
}

/// One row, as a code and a sentence when it does not go in.
async fn one(
    tenant: &Allowed<ManageTenant>,
    key: &IdempotencyKey,
    row: &erp_web::csv::Row,
    id: &str,
    locale: Locale,
) -> Result<(), (String, String)> {
    let say = |message: &erp_i18n::Message| {
        (
            message.code.as_str().to_owned(),
            CATALOG.render_or_code(locale, message),
        )
    };

    if id.is_empty() {
        return Err(say(&erp_i18n::Message::new(crate::messages::NO_ID_COLUMN)));
    }
    let id = erp_types::AggregateId::new(id).map_err(|_| {
        say(&erp_i18n::Message::new(erp_web::messages::INVALID_ID)
            .with("id", erp_i18n::MessageArg::text(id)))
    })?;

    let kind: CustomerKind = row
        .get("kind")
        .filter(|k| !k.is_empty())
        .map_or("person", String::as_str)
        .parse()
        .map_err(|_| {
            say(&erp_i18n::Message::new(crate::messages::UNKNOWN_KIND).with(
                "kind",
                erp_i18n::MessageArg::text(row.get("kind").map_or("", String::as_str)),
            ))
        })?;

    let details = Details {
        name: row.get("name").cloned().unwrap_or_default(),
        name_latin: taken(row, "name_latin"),
        kind,
        contact: Contact {
            phone: taken(row, "phone"),
            email: taken(row, "email"),
        },
        address: taken(row, "city").map(|city| Address {
            street: row.get("street").cloned().unwrap_or_default(),
            building: taken(row, "building"),
            district: taken(row, "district"),
            city,
            postal_code: taken(row, "postal_code"),
            country: row
                .get("country")
                .filter(|c| !c.is_empty())
                .cloned()
                .unwrap_or_else(|| "SA".to_owned()),
        }),
        tax: taken(row, "vat_number").map(|vat_number| TaxRegistration {
            vat_number,
            scheme: taken(row, "vat_scheme"),
            identifier: taken(row, "vat_identifier"),
        }),
    };

    // Checked here rather than left to the command, so the row's refusal is
    // this module's own message rather than a wrapped command error.
    details.check().map_err(|e| say(&e.message()))?;

    match crate::register_customer(
        &tenant.db,
        &id,
        &details,
        chrono::Utc::now(),
        &importing(tenant, key, id.as_str()),
    )
    .await
    {
        // **Already there counts as imported.** A re-upload of a corrected
        // file is meant to be safe, and a row that went in last time going in
        // again is the whole point — see `erp_web::importing`.
        Ok(_) | Err(CommandError::Execute(ExecuteError::AlreadyExists { .. })) => Ok(()),
        Err(CommandError::Execute(ExecuteError::Rejected(rejection))) => {
            Err(say(&rejection.message()))
        }
        Err(other) => {
            tracing::warn!(error = %other, row = %id, "a row of an import failed");
            Err(say(&erp_i18n::Message::new(erp_tenant::messages::INTERNAL)))
        }
    }
}

/// A column's value, when it has one.
fn taken(row: &erp_web::csv::Row, column: &str) -> Option<String> {
    row.get(column).filter(|v| !v.is_empty()).cloned()
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

// ---------------------------------------------------------------------------
// Fields a business adds to a customer
// ---------------------------------------------------------------------------

/// One field a business has added to its customers.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct FieldView {
    /// The stable machine name. Lowercase letters, digits and underscores.
    key: String,
    /// What a person reads.
    label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label_latin: Option<String>,
    /// `text`, `number`, `date`, `choice` or `flag`.
    kind: String,
    /// **Characters**, on a `text` field. Ignored on every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max: Option<u16>,
    /// What may be chosen, on a `choice` field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    options: Vec<String>,
    /// Whether a customer is expected to have one. **Reported, not enforced** —
    /// see `Fields::missing_from`.
    #[serde(default)]
    required: bool,
}

/// Every field a business has added.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({ "fields": [
    { "key": "blood_group", "label": "فصيلة الدم", "label_latin": "Blood group",
      "kind": "choice", "options": ["A+", "A-", "O+", "O-"], "required": false }
]}))]
struct FieldSet {
    /// **The order is yours**, and it is the order a form shows them in.
    fields: Vec<FieldView>,
}

/// A value on a customer.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct ValueView {
    key: String,
    /// Exactly one of the four below, matching what the field declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    number: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    date: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    flag: Option<bool>,
    /// When it was last set, and by whom. Read only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    set_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    set_by: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct CustomerFields {
    values: Vec<ValueView>,
}

fn field_view(field: &crate::fields::FieldDef) -> FieldView {
    use crate::fields::FieldKind;
    FieldView {
        key: field.key.clone(),
        label: field.label.clone(),
        label_latin: field.label_latin.clone(),
        kind: field.kind.as_str().to_owned(),
        max: match &field.kind {
            FieldKind::Text { max } => Some(*max),
            _ => None,
        },
        options: match &field.kind {
            FieldKind::Choice { options } => options.clone(),
            _ => Vec::new(),
        },
        required: field.required,
    }
}

fn field_def(view: FieldView, locale: Locale) -> Result<crate::fields::FieldDef, Problem> {
    use crate::fields::FieldKind;
    let kind = match view.kind.as_str() {
        "text" => FieldKind::Text {
            // A text field with no stated length gets a sensible one rather
            // than a refusal: "add a note field" is the commonest thing anybody
            // does here, and asking them for a number first is friction.
            max: view.max.unwrap_or(500),
        },
        "number" => FieldKind::Number,
        "date" => FieldKind::Date,
        "choice" => FieldKind::Choice {
            options: view.options,
        },
        "flag" => FieldKind::Flag,
        other => {
            return Err(bad_request(
                erp_web::messages::MALFORMED_BODY,
                "reason",
                &format!("{other} is not a kind of field"),
                locale,
            ));
        }
    };
    Ok(crate::fields::FieldDef {
        key: view.key,
        label: view.label,
        label_latin: view.label_latin,
        kind,
        required: view.required,
    })
}

fn value_of(view: &ValueView, locale: Locale) -> Result<crate::fields::Value, Problem> {
    use crate::fields::Held;
    let held = match (view.text.as_ref(), view.number, view.date, view.flag) {
        // **Text and choice are the same column and different fields**, so which
        // one this is comes from the declaration rather than from the request —
        // `crm::fields::check` refuses the pair that do not match.
        (Some(text), None, None, None) => Held::Text(text.clone()),
        (None, Some(number), None, None) => Held::Number(number),
        (None, None, Some(date), None) => Held::Date(date),
        (None, None, None, Some(flag)) => Held::Flag(flag),
        _ => {
            return Err(bad_request(
                erp_web::messages::MALFORMED_BODY,
                "reason",
                "a value carries exactly one of text, number, date and flag",
                locale,
            ));
        }
    };
    Ok(crate::fields::Value {
        key: view.key.clone(),
        held,
    })
}

fn held_view(holding: &crate::fields::Holding) -> ValueView {
    use crate::fields::Held;
    let mut view = ValueView {
        key: holding.key.clone(),
        text: None,
        number: None,
        date: None,
        flag: None,
        set_at: Some(holding.set_at),
        set_by: holding.set_by.clone(),
    };
    match &holding.held {
        Held::Text(text) | Held::Choice(text) => view.text = Some(text.clone()),
        Held::Number(n) => view.number = Some(*n),
        Held::Date(d) => view.date = Some(*d),
        Held::Flag(f) => view.flag = Some(*f),
    }
    view
}

/// The fields this business has added to its customers.
#[utoipa::path(
    get,
    path = "/v1/crm/fields",
    tag = "crm",
    responses(
        (status = OK, body = FieldSet, headers(("ETag" = String, description = "The version of this setting. Send it back as `If-Match` to write only if nobody else has since."))),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn customer_fields(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Versioned<FieldSet>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let version = erp_eventlog::configuration::version_of(&mut conn, crate::fields::Fields::KEY)
        .await
        .map_err(|e| config_problem(&e, locale, &CATALOG))?;
    let fields = crate::fields::Fields::resolve(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale, &CATALOG))?;

    Ok(Versioned(
        version,
        FieldSet {
            fields: fields.fields.iter().map(field_view).collect(),
        },
    ))
}

/// Decide what a business records about its customers.
///
/// # It replaces the whole set
///
/// Send every field you want, in the order you want them shown. A field that is
/// not in the list is removed — which is refused while anybody still holds a
/// value for it, because a field that vanished with its data still in the table
/// is exactly how a business comes to hold health details it has forgotten
/// about. Erase them first: `DELETE /v1/crm/fields/{field}/values`.
///
/// **Redefining a field under its own values is refused for the same reason.**
/// Turning a text field into a date leaves every stored value unreadable, and a
/// settings screen that allowed it would be quietly discarding what somebody
/// typed.
///
/// # Whose decision this is
///
/// The owner's. What a business records about a person — and especially that it
/// records health details at all — is not a preference, and the fields decide
/// what every customer page asks for from then on.
#[utoipa::path(
    put,
    path = "/v1/crm/fields",
    tag = "crm",
    params(("If-Match" = Option<String>, Header, description = "The `ETag` a GET answered with. With it, the write happens only if the setting is still at that version; without it, unconditionally.")),
    request_body = FieldSet,
    responses(
        (status = NO_CONTENT, description = "Recorded."),
        (status = PRECONDITION_FAILED, description = "`If-Match` named a version that is no longer current; reload and try again", body = Problem),
        (status = BAD_REQUEST, description = "Not a key, not a kind, a repeat, or a choice with nothing to choose from", body = Problem),
        (status = CONFLICT, description = "A field was removed or redefined while customers still hold values for it", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn set_customer_fields(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
    IfMatch(expected): IfMatch,
    Json(body): Json<FieldSet>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let mut wanted = crate::fields::Fields {
        fields: Vec::with_capacity(body.fields.len()),
    };
    for view in body.fields {
        wanted.fields.push(field_def(view, locale)?);
    }
    wanted
        .check()
        .map_err(|e| field_problem(&e, StatusCode::BAD_REQUEST, locale))?;

    let mut tx = tenant.db.begin().await.map_err(|e| pool(&e, locale))?;

    // **What is being taken away, and what is being changed underneath.** Both
    // checked against what is actually stored rather than against what the
    // previous setting said, because the values are what would be lost.
    let before = crate::fields::Fields::resolve(&mut tx)
        .await
        .map_err(|e| config_problem(&e, locale, &CATALOG))?;
    for was in &before.fields {
        let still = wanted.get(&was.key);
        let changed = still.is_none_or(|now| now.kind.as_str() != was.kind.as_str());
        if !changed {
            continue;
        }
        let held = crate::fields::anyone_holds(&mut tx, &was.key)
            .await
            .map_err(|e| store_problem(&e, locale))?;
        if held {
            return Err(field_problem(
                &crate::fields::FieldError::Required(was.key.clone()),
                StatusCode::CONFLICT,
                locale,
            ));
        }
    }

    erp_eventlog::configuration::set(
        &mut tx,
        crate::fields::Fields::KEY,
        &wanted,
        Some(&tenant.session.identity.to_string()),
        expected,
    )
    .await
    .map_err(|e| config_problem(&e, locale, &CATALOG))?;
    tx.commit().await.map_err(|e| database(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// What this customer holds.
///
/// In the order the fields were declared, which is the order a form shows them.
/// A value under a field the business has since removed is **not** here — see
/// `GET /v1/crm/fields/orphaned`, which is how one is found and erased.
#[utoipa::path(
    get,
    path = "/v1/crm/customers/{customer}/fields",
    tag = "crm",
    params(("customer" = String, Path, description = "From `GET /v1/crm/customers`.")),
    responses(
        (status = OK, body = CustomerFields),
        (status = BAD_REQUEST, body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn held_fields(
    tenant: Allowed<Read>,
    Language(locale): Language,
    Path(customer): Path<String>,
) -> Result<Json<CustomerFields>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let customer = parse_id(&customer, locale)?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let holdings = crate::fields::held(&mut conn, customer.as_str())
        .await
        .map_err(|e| store_problem(&e, locale))?;

    Ok(Json(CustomerFields {
        values: holdings.iter().map(held_view).collect(),
    }))
}

/// Set what this customer holds.
///
/// # All of them or none
///
/// Every value is checked against the field set before any is written, so a
/// form with one bad date stores nothing rather than half of itself.
///
/// # It sets what you send and leaves the rest
///
/// A field you do not mention keeps what it had. Emptying one is
/// `DELETE /v1/crm/customers/{customer}/fields/{field}`, which is a different act
/// and refused on a required field.
///
/// **The old value is kept**, marked with the moment it stopped being true, so
/// "who changed this and when" is answerable. Erasing takes that history with
/// it.
#[utoipa::path(
    put,
    path = "/v1/crm/customers/{customer}/fields",
    tag = "crm",
    params(("customer" = String, Path, description = "From `GET /v1/crm/customers`.")),
    request_body = CustomerFields,
    responses(
        (status = NO_CONTENT, description = "Recorded."),
        (status = BAD_REQUEST, description = "A value that is not the kind its field declared, too long, or not one of the options", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such field", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn set_held_fields(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(customer): Path<String>,
    Json(body): Json<CustomerFields>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let customer = parse_id(&customer, locale)?;

    let mut values = Vec::with_capacity(body.values.len());
    for view in &body.values {
        values.push(value_of(view, locale)?);
    }

    let mut tx = tenant.db.begin().await.map_err(|e| pool(&e, locale))?;
    // **A value needs a customer to be about.** Written under an id that
    // parses but names nobody, these would be shown on no page, found by no
    // erasure request, and reported by nothing — health data the business does
    // not know it holds. Checked against the log, in this transaction, the way
    // every other command that names a customer checks.
    if !crate::accepts_documents(&mut tx, &customer)
        .await
        .map_err(|e| problem_for(&CommandError::Execute(ExecuteError::Load(e)), locale))?
    {
        return Err(problem_for(
            &CommandError::Execute(ExecuteError::Rejected(CrmError::NoSuchCustomer(
                customer.to_string(),
            ))),
            locale,
        ));
    }
    crate::fields::set(
        &mut tx,
        customer.as_str(),
        &values,
        chrono::Utc::now(),
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| store_problem(&e, locale))?;
    tx.commit().await.map_err(|e| database(&e, locale))?;

    let _ = &state;
    Ok(StatusCode::NO_CONTENT)
}

/// Empty one field on one customer.
///
/// Refused on a required field: adding one does not refuse the customers who
/// already lack it, but deliberately taking one away is a different act.
///
/// **This is not erasure.** The old value is kept as history, the way every
/// change is. To remove it altogether, see
/// `DELETE /v1/crm/customers/{customer}/fields`.
#[utoipa::path(
    delete,
    path = "/v1/crm/customers/{customer}/fields/{field}",
    tag = "crm",
    params(
        ("customer" = String, Path, description = "From `GET /v1/crm/customers`."),
        ("field" = String, Path, description = "From `GET /v1/crm/fields`."),
    ),
    responses(
        (status = NO_CONTENT, description = "Emptied, or already was."),
        (status = BAD_REQUEST, body = Problem),
        (status = CONFLICT, description = "The field is required", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such field", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn clear_held_field(
    tenant: Allowed<PostEntries>,
    Language(locale): Language,
    Path(params): Path<std::collections::HashMap<String, String>>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let customer = parse_id(params.get("customer").map_or("", String::as_str), locale)?;
    let field = params.get("field").map_or("", String::as_str);

    let mut tx = tenant.db.begin().await.map_err(|e| pool(&e, locale))?;
    crate::fields::clear(&mut tx, customer.as_str(), field, chrono::Utc::now())
        .await
        .map_err(|e| store_problem(&e, locale))?;
    tx.commit().await.map_err(|e| database(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// **Erase everything this business added about this person.**
///
/// This is the answer to somebody asking for their data to be deleted. It takes
/// the history with it, because a deletion that left the old value behind would
/// not be one.
///
/// # What it does not do
///
/// It does not erase the customer. A customer record is what documents point
/// at, and a tax invoice does not stop having been issued — see the `crm` module
/// docs on the copy a document freezes. What this removes is the fields a
/// business chose to keep on top of that, which is where health details and
/// private notes live and where a right to erasure actually bites.
///
/// # Whose decision this is
///
/// The owner's, deliberately. Erasing somebody's record is not an ordinary
/// clerical act, and this system has already declined once to answer "who may
/// erase whom" in passing.
#[utoipa::path(
    delete,
    path = "/v1/crm/customers/{customer}/fields",
    tag = "crm",
    params(("customer" = String, Path, description = "From `GET /v1/crm/customers`.")),
    responses(
        (status = OK, description = "How many rows went, history included.", body = Erased),
        (status = BAD_REQUEST, body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn erase_held_fields(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
    Path(customer): Path<String>,
) -> Result<Json<Erased>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let customer = parse_id(&customer, locale)?;

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    let erased = crate::fields::forget(&mut conn, customer.as_str())
        .await
        .map_err(|e| store_problem(&e, locale))?;

    Ok(Json(Erased { erased }))
}

#[derive(Debug, Serialize, ToSchema)]
struct Erased {
    /// Rows removed, current and superseded together.
    erased: u64,
}

/// **Values nothing declares any more.**
///
/// A field removed from the set leaves its values in the table, shown to
/// nobody — which is exactly the state that becomes "we still hold health data
/// we forgot about". This is how they are found; erasing them is
/// `DELETE /v1/crm/fields/{field}/values`.
#[utoipa::path(
    get,
    path = "/v1/crm/fields/orphaned",
    tag = "crm",
    responses(
        (status = OK, body = Vec<String>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn orphaned_fields(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
) -> Result<Json<Vec<String>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;

    Ok(Json(
        crate::fields::orphaned(&mut conn)
            .await
            .map_err(|e| store_problem(&e, locale))?,
    ))
}

/// Erase one field's values across every customer, history included.
///
/// What a business runs before removing a field they should never have
/// collected, and what makes removing one from the set possible at all.
#[utoipa::path(
    delete,
    path = "/v1/crm/fields/{field}/values",
    tag = "crm",
    params(("field" = String, Path, description = "The field's key.")),
    responses(
        (status = OK, description = "How many rows went.", body = Erased),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn erase_field_values(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
    Path(field): Path<String>,
) -> Result<Json<Erased>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    let erased = crate::fields::forget_field(&mut conn, &field)
        .await
        .map_err(|e| store_problem(&e, locale))?;

    Ok(Json(Erased { erased }))
}

fn field_problem(error: &crate::fields::FieldError, status: StatusCode, locale: Locale) -> Problem {
    Problem::new(
        status,
        &erp_i18n::Message::new(crate::messages::field_code(error))
            .with("field", erp_i18n::MessageArg::text(field_named(error))),
        locale,
        &CATALOG,
    )
}

/// Which field a refusal is about, for the sentence.
fn field_named(error: &crate::fields::FieldError) -> String {
    use crate::fields::FieldError;
    match error {
        FieldError::NotAKey(key)
        | FieldError::DuplicateKey(key)
        | FieldError::NoLabel(key)
        | FieldError::NotALength(key)
        | FieldError::NoOptions(key)
        | FieldError::NotAnOption(key)
        | FieldError::NoSuchField(key)
        | FieldError::TooLong(key)
        | FieldError::Required(key) => key.clone(),
        FieldError::WrongKind { field, .. } | FieldError::NotOneOfTheOptions { field, .. } => {
            field.clone()
        }
        FieldError::TooManyFields => String::new(),
    }
}

fn store_problem(error: &crate::fields::StoreError, locale: Locale) -> Problem {
    use crate::fields::{FieldError, StoreError};
    match error {
        StoreError::Field(field) => {
            let status = match field {
                // Well-formed, and refused on what the field declared.
                FieldError::NoSuchField(_) => StatusCode::NOT_FOUND,
                FieldError::Required(_) => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            field_problem(field, status, locale)
        }
        StoreError::Config(e) => {
            Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, e, locale, &CATALOG)
        }
        StoreError::Database(e) => {
            tracing::error!(error = %e, "a customer field could not be read or written");
            Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                &erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
                locale,
                &CATALOG,
            )
        }
    }
}
