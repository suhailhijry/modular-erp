//! Stock, over HTTP.
//!
//! Translation only. Every refusal here is a refusal `commands` already made,
//! and the one decision this file takes is which status carries it.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_eventlog::ExecuteError;
use erp_i18n::{Locale, Localize, Message, MessageArg};
use erp_tenant::CommandError;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::{
    After, Allowed, Amount, AppState, Consistency, IdempotencyKey, IfMatch, Json, Language,
    ManageAccounts, ManageTenant, Paged, PostEntries, Problem, Query, Read, Versioned,
    config_problem, creating, metadata, nudge, parse_id, require_module,
};

use crate::InventoryError;
use crate::expiry::ExpiryWindow;
use crate::product::Tracking;
use crate::stock::Reason;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_products, declare_product))
        .routes(routes!(list_stock))
        .routes(routes!(list_lots))
        .routes(routes!(stock_summary))
        .routes(routes!(receive_stock))
        .routes(routes!(count_stock))
        .routes(routes!(write_off_stock))
        .routes(routes!(list_movements))
        .routes(routes!(stock_accounts, set_stock_accounts))
        .routes(routes!(expiry_window, set_expiry_window))
}

static CATALOG: erp_i18n::Composite =
    erp_i18n::Composite::new(&[&crate::CATALOG, &ledger::CATALOG, &erp_web::CATALOG]);

const PAGE: i64 = 50;
const MAX_PAGE: i64 = 200;

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "name": "حليب طازج", "unit": "bottle", "tracking": "lot" }))]
struct NewProduct {
    name: String,
    /// **Frozen.** What one of these is — `gram`, `piece`, `bottle`. Every
    /// quantity ever recorded against this product is a number in it, and there
    /// is no route that changes it.
    unit: String,
    /// **Frozen.** `none`, `lot` or `serial`; `none` when absent. A lot-tracked
    /// delivery names its batch and may name an expiry; a serial-tracked one
    /// names every unit.
    #[serde(default)]
    tracking: Option<String>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<erp_types::Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct ProductView {
    id: String,
    name: String,
    unit: String,
    /// `none`, `lot` or `serial`.
    tracking: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    declared_at: erp_types::Timestamp,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "quantity": 24,
    "value": { "minor": 12_000, "currency": "SAR" },
    "code": "B-2026-04",
    "expires_on": "2026-04-21"
}))]
struct NewReceipt {
    /// A whole number of the product's own unit.
    quantity: i64,
    /// What the **whole** delivery cost, not what one unit did.
    value: Amount,
    /// The batch this came in, in the tenant's own words. Required on a
    /// lot-tracked product and refused on any other. It may repeat: the lot's
    /// own id keeps two deliveries of one code apart.
    #[serde(default)]
    code: Option<String>,
    /// When this batch goes off. Only on a lot-tracked product, and only when
    /// the batch has a shelf life — an undated lot goes out after every dated
    /// one, oldest first.
    #[serde(default)]
    #[schema(value_type = Option<String>, example = "2026-04-21")]
    expires_on: Option<chrono::NaiveDate>,
    /// One per unit, on a serial-tracked product. **Yours, not ours**: a serial
    /// is an identity and this system does not invent identities.
    #[serde(default)]
    serials: Vec<String>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<erp_types::Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "declared": 21 }))]
struct NewStockCount {
    /// **Which lot was counted**, for someone counting batches. Leave it out to
    /// count the shelf — see the `count` operation's description.
    #[serde(default)]
    lot: Option<String>,
    /// What was actually there. On a serial-tracked product, how many units
    /// `serials` names.
    declared: i64,
    /// The units found, by name, on a serial-tracked product. What was on hand
    /// and is not named here leaves as missing.
    #[serde(default)]
    serials: Vec<String>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<erp_types::Timestamp>,
}

impl NewStockCount {
    /// The command this body asks for, under the request's own key.
    fn into_count(self, reference: String) -> crate::Count {
        crate::Count {
            lot: self.lot,
            declared: self.declared,
            serials: self.serials,
            reference,
            at: self.at.unwrap_or_else(chrono::Utc::now),
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "reason": "expired", "quantity": 6 }))]
struct NewWriteOff {
    /// `expired` or `damaged`.
    reason: String,
    /// How many. On a product tracked by quantity, and refused on a
    /// serial-tracked one.
    #[serde(default)]
    quantity: Option<i64>,
    /// Which lot to take it from, overriding earliest-expiry-first. A lot that
    /// cannot cover it is refused rather than topped up from the next.
    #[serde(default)]
    lot: Option<String>,
    /// Which units. On a serial-tracked product, and refused on any other.
    #[serde(default)]
    serials: Vec<String>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<erp_types::Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct StockView {
    product: String,
    /// The product's name, as declared. `null` only when the read model holds
    /// no declaration for it: the row is still listed.
    name: Option<String>,
    /// Absent on a business that sends no `X-Branch`.
    branch: Option<String>,
    /// The open lots added up, **less what the shelf owes**. Negative when a
    /// plain product has been sold off an empty shelf — a till does not stop
    /// for a bad count (R1), and the negative number is the report a count
    /// corrects.
    on_hand: i64,
    /// What that is carried at: the sum of the lots' own values, less what any
    /// units sold short were charged out at, and never an average across them.
    /// Absent until something has been received here.
    value: Option<Amount>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    last_at: erp_types::Timestamp,
}

#[derive(Debug, Serialize, ToSchema)]
struct LotView {
    id: String,
    product: String,
    /// The product's name, as declared. `null` only when the read model holds
    /// no declaration for it: the lot is still listed.
    name: Option<String>,
    branch: Option<String>,
    /// The tenant's own batch code, on a lot-tracked product.
    code: Option<String>,
    /// Absent on a batch with no shelf life. Those go out after every dated
    /// lot, oldest first.
    #[schema(value_type = Option<String>, example = "2026-04-21")]
    expires_on: Option<chrono::NaiveDate>,
    /// What arrived.
    quantity: i64,
    /// What is still here.
    remaining: i64,
    /// What the remainder is carried at.
    value: Amount,
    /// The units still on this lot, on a serial-tracked product.
    serials: Vec<String>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    received_at: erp_types::Timestamp,
}

/// **The shelves at a glance**, a branch at a time.
#[derive(Debug, Serialize, ToSchema)]
struct StockSummary {
    /// The tenant's day the dates below were read against, on its own
    /// calendar.
    #[schema(value_type = String, example = "2026-09-14")]
    today: chrono::NaiveDate,
    /// The last day `expiring` reaches: `today` plus the tenant's expiry
    /// window.
    #[schema(value_type = Option<String>, example = "2026-10-14")]
    expiring_through: Option<chrono::NaiveDate>,
    /// One per branch with a shelf, the shelves of no branch first. Empty when
    /// nothing has been received or sold.
    branches: Vec<BranchStock>,
}

#[derive(Debug, Serialize, ToSchema)]
struct BranchStock {
    /// Absent on the shelves of a business that sends no `X-Branch`.
    branch: Option<String>,
    /// What the shelves here are carried at, one amount per currency: the
    /// `value`s `GET /v1/inventory/stock?branch=` lists, added up — the debt
    /// of a shelf below zero already off it.
    value: Vec<Amount>,
    /// How many products have a shelf here, whatever is on it: the rows
    /// `GET /v1/inventory/stock?branch=` lists.
    products: i64,
    /// Open lots dated from `today` to `expiring_through`. A lot dated today is
    /// still good today, so it is here and not in `expired`.
    expiring: i64,
    /// Open lots dated before `today`, still on the shelf.
    expired: i64,
    /// The shelves here below zero, by product.
    below_zero: Vec<StockBelowZero>,
}

#[derive(Debug, Serialize, ToSchema)]
struct StockBelowZero {
    product: String,
    name: Option<String>,
    /// Negative.
    on_hand: i64,
    /// **What the shelf owes**: what the units no lot could cover were charged
    /// out at, less what returns and counts have settled. A delivery since does
    /// not pay it; a count does. Absent while nothing has ever been received
    /// onto the shelf.
    owes: Option<Amount>,
}

#[derive(Debug, Serialize, ToSchema)]
struct MovementView {
    product: String,
    branch: Option<String>,
    /// Which lot this came off, or went back onto. **Absent only where no lot
    /// was involved** — units a plain product's sale took that no lot could
    /// cover (R1), the part of a return or a count of the shelf that settles
    /// that debt, and the one row a count of the shelf leaves when it moved
    /// nothing.
    lot: Option<String>,
    /// `received` (bought in, or put back by a credit note), `consumed` (sold
    /// on a document), `written_off` or `counted`.
    kind: String,
    /// `expired` or `damaged`, on a write-off.
    reason: Option<String>,
    /// Signed: what this movement did to what is on hand. The movements sum to
    /// the quantity above, lot by lot.
    quantity: i64,
    value: Option<Amount>,
    /// What the books said was there — on the lot, or on the shelf for a count
    /// of the shelf. Set on a count, and frozen as it was found.
    expected: Option<i64>,
    /// What was counted.
    declared: Option<i64>,
    /// The request or document this movement was for.
    reference: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    moved_at: erp_types::Timestamp,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "inventory": "1300", "goods_received": "2010", "cogs": "5010",
    "variance": "5900", "waste": "5900"
}))]
struct StockAccounts {
    /// What is on the shelf, as an asset. Defaults to 1300.
    inventory: String,
    /// What a delivery owes before anybody has billed for it — a liability.
    /// Defaults to 2010. A receipt credits it; the supplier's bill line that
    /// names the product debits it back.
    goods_received: String,
    /// What went out the door cost. Defaults to 5010.
    cogs: String,
    /// What a count could not find, or found more of. Defaults to 5900.
    variance: String,
    /// What a write-off threw away. Defaults to 5900 as well — they are two
    /// fields so a business that wants spoilage apart from shrinkage can have
    /// it without a code change, not because the shipped charts separate them.
    waste: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({ "days": 30 }))]
struct StockExpiryWindow {
    /// How many days ahead of an expiry the business wants warning. Nothing to
    /// ten years.
    days: i32,
}

#[derive(Debug, Serialize, ToSchema)]
struct InventoryAccepted {
    id: String,
    position: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ByProduct {
    product: Option<String>,
    #[serde(flatten)]
    page: After,
}

#[derive(Debug, Deserialize)]
struct StockQuery {
    product: Option<String>,
    branch: Option<String>,
    #[serde(flatten)]
    page: After,
}

#[derive(Debug, Deserialize)]
struct ByBranch {
    branch: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LotQuery {
    product: Option<String>,
    branch: Option<String>,
    expiring_before: Option<chrono::NaiveDate>,
    #[serde(flatten)]
    page: After,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Declare something the business keeps: its unit, and how closely it is
/// watched.
///
/// Both are **frozen**. Every quantity ever recorded against this product is a
/// number in its unit, and every rule about batches and serials is a rule about
/// its tracking mode — changing either would restate history. A product in a
/// new unit, or tracked a new way, is a new product.
#[utoipa::path(
    post,
    path = "/v1/inventory/products",
    tag = "inventory",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("Idempotency-Key" = String, Header, description = "A UUID. **It is the product's id.**"),
    ),
    request_body = NewProduct,
    responses(
        (status = CREATED, body = InventoryAccepted),
        (status = BAD_REQUEST, description = "No name, no unit, or a tracking mode that is not one of the three", body = Problem),
        (status = CONFLICT, description = "That id is already a different product", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable inventory", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn declare_product(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Json(body): Json<NewProduct>,
) -> Result<(StatusCode, Json<InventoryAccepted>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let id = key.id().clone();
    let tracking = match body.tracking.as_deref() {
        None => Tracking::None,
        Some(literal) => Tracking::parse(literal).ok_or_else(|| {
            malformed(
                &Message::new(crate::messages::NOT_A_TRACKING_MODE)
                    .with("tracking", MessageArg::text(literal)),
                locale,
            )
        })?,
    };

    let committed = crate::declare(
        &tenant.db,
        &id,
        &body.name,
        &body.unit,
        tracking,
        body.at.unwrap_or_else(chrono::Utc::now),
        &creating(&tenant, &key),
    )
    .await
    .map_err(|e| refused(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(accepted(id.to_string(), committed.at)),
    ))
}

/// What the business keeps.
#[utoipa::path(
    get,
    path = "/v1/inventory/products",
    tag = "inventory",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("limit" = Option<i64>, Query, description = "Up to 200. Defaults to 50."),
        ("after" = Option<String>, Query, description = "The `next` from the previous page."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = Paged<ProductView>),
        (status = BAD_REQUEST, description = "Not a cursor", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_products(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(page): Query<After>,
) -> Result<Json<Paged<ProductView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = page.cursor(locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let found = crate::products(&mut conn, page.limit(PAGE, MAX_PAGE), after.as_ref())
        .await
        .map_err(|e| database(&e, locale))?;
    drop(conn);

    Ok(Json(Paged::of(found, |row: crate::ProductRow| {
        ProductView {
            id: row.id,
            name: row.name,
            unit: row.unit,
            tracking: row.tracking,
            declared_at: row.declared_at,
        }
    })))
}

/// Record a delivery onto the shelf this request is at. **It becomes a lot.**
///
/// Every receipt makes one, whatever the tracking mode, because cost is carried
/// per lot: what a movement costs is what *its own* delivery cost, and a
/// product with no lots would need a second costing method.
///
/// **It posts**, in this request's own transaction: `Dr` inventory, `Cr` goods
/// received not invoiced, at what the delivery cost. Stock is in the books the
/// moment it lands. The supplier's bill debits the holding account back on the
/// line that names this product, so between the two that account is what has
/// arrived and not been invoiced — and after both it is zero.
#[utoipa::path(
    post,
    path = "/v1/inventory/stock/{product}/receipts",
    tag = "inventory",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("X-Branch" = Option<String>, Header, description = "Where this arrived. Stock is held per branch."),
        ("Idempotency-Key" = String, Header, description = "A UUID. **The lot is named after it**, so receiving twice under one key records one delivery."),
        ("product" = String, Path, description = "The key the product was declared under."),
    ),
    request_body = NewReceipt,
    responses(
        (status = CREATED, body = InventoryAccepted),
        (status = BAD_REQUEST, description = "Not a quantity, not an amount, a lot-tracked delivery with no code, or serials that do not match the units", body = Problem),
        (status = CONFLICT, description = "The shelf is under sustained contention", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable inventory", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such product, a batch code on a product that is not lot-tracked, a serial already on the shelf, another currency, or a ledger that refuses the entry — a closed account, a closed period, or a branch nobody opened", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn receive_stock(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Path(product): Path<String>,
    Json(body): Json<NewReceipt>,
) -> Result<(StatusCode, Json<InventoryAccepted>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let product = parse_id(&product, locale)?;
    // The shelf this lands on, because the lot is named after it as well as
    // after the key — see `crate::lot_of`. The same refusal `commands` makes
    // for the same input, which is the only kind this file translates.
    let metadata = metadata(&tenant);
    let shelf = crate::stock_id(&product, metadata.branch()).map_err(|_| {
        refused(
            &CommandError::Execute(ExecuteError::Rejected(InventoryError::NotAProductId(
                product.to_string(),
            ))),
            locale,
        )
    })?;

    let receipt = crate::Receipt {
        quantity: body.quantity,
        value: body.value.parse(locale)?,
        code: body.code,
        expires_on: body.expires_on,
        serials: body.serials,
        reference: reference(&key),
        at: body.at.unwrap_or_else(chrono::Utc::now),
    };

    let committed = crate::receive(&tenant.db, &product, &receipt, &metadata)
        .await
        .map_err(|e| refused(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(accepted(
            crate::lot_of(&shelf, &reference(&key)),
            committed.at,
        )),
    ))
}

/// Count **the shelf** or one lot on it, and record what the books disagreed by.
///
/// # The shelf, and where the difference lands
///
/// Send what is there. A shortage comes off the lots in the order stock goes
/// out — earliest expiry first, undated oldest-first, the order
/// `GET /v1/inventory/lots` lists them in — each at its own lot's cost. An
/// overage joins the lot at the far end of that order, at that lot's cost; more than the lots hold
/// with no lot open to join is refused, and is a delivery instead. A count of
/// the shelf also clears what a plain product sold below zero owes.
///
/// **Name a `lot` to count one batch**, and the difference lands on it. What the
/// shelf owes is on no lot, so a count of one leaves it alone.
///
/// # A serial-tracked product names what it found
///
/// `serials` is the units found and `declared` how many they are. What was on
/// hand and is not named leaves at its own lot's cost; a name that is not on
/// hand is refused, because a count corrects a quantity and not an identity.
///
/// The variance and where it landed are frozen as they were found: by the next
/// delivery a lot may be closed, and a stocktake that answers a different number
/// tomorrow is evidence of nothing.
///
/// **The discrepancy is booked**, in this request's own transaction: short is a
/// loss against inventory, over is the same entry the other way round, and a
/// count that moved no value posts nothing at all.
#[utoipa::path(
    post,
    path = "/v1/inventory/stock/{product}/counts",
    tag = "inventory",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("X-Branch" = Option<String>, Header, description = "Which shelf was counted."),
        ("Idempotency-Key" = String, Header, description = "A UUID. Counting twice under one key records one count."),
        ("product" = String, Path, description = "The key the product was declared under."),
    ),
    request_body = NewStockCount,
    responses(
        (status = CREATED, body = InventoryAccepted),
        (status = BAD_REQUEST, description = "Not a quantity, serials that do not agree with `declared`, or serials on a product that is not serial-tracked", body = Problem),
        (status = CONFLICT, description = "The shelf is under sustained contention", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable inventory", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such product, no such open lot, a serial that is not on the shelf, more found than the lots hold with no lot open to join, or a ledger that refuses the entry", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn count_stock(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Path(product): Path<String>,
    Json(body): Json<NewStockCount>,
) -> Result<(StatusCode, Json<InventoryAccepted>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let product = parse_id(&product, locale)?;

    let counted = body.into_count(reference(&key));

    let committed = crate::count(&tenant.db, &product, &counted, &metadata(&tenant))
        .await
        .map_err(|e| refused(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(accepted(key.id().to_string(), committed.at)),
    ))
}

/// Throw stock away, and say why.
///
/// **Earliest expiry first**, unless a lot is named — a recall, a scanned batch.
/// Each portion is costed on the lot it came off. A serial-tracked product names
/// the units that are going, and one that is not on the shelf is refused rather
/// than invented.
///
/// **More than is there is refused.** This is somebody holding the goods, not a
/// till: a till may not stop for a bad count, and a person looking at a shelf
/// may be told the shelf says otherwise.
///
/// **The loss is booked**, in this request's own transaction: `Dr` waste, `Cr`
/// inventory at what the lots it took were carried at. A closed account or a
/// closed period therefore refuses the write-off itself.
#[utoipa::path(
    post,
    path = "/v1/inventory/stock/{product}/write-offs",
    tag = "inventory",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("X-Branch" = Option<String>, Header, description = "Which shelf it went off."),
        ("Idempotency-Key" = String, Header, description = "A UUID. Writing off twice under one key throws away one lot's worth."),
        ("product" = String, Path, description = "The key the product was declared under."),
    ),
    request_body = NewWriteOff,
    responses(
        (status = CREATED, body = InventoryAccepted),
        (status = BAD_REQUEST, description = "Not a reason, not a quantity, or serials that do not match how the product is tracked", body = Problem),
        (status = CONFLICT, description = "The shelf is under sustained contention", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable inventory", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such product, no such lot or serial, more than the shelf holds, or a ledger that refuses the entry", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn write_off_stock(
    tenant: Allowed<WritesOff>,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Path(product): Path<String>,
    Json(body): Json<NewWriteOff>,
) -> Result<(StatusCode, Json<InventoryAccepted>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let product = parse_id(&product, locale)?;
    let reason = Reason::parse(&body.reason).ok_or_else(|| {
        malformed(
            &Message::new(crate::messages::NOT_A_REASON)
                .with("reason", MessageArg::text(&body.reason)),
            locale,
        )
    })?;

    let taken = crate::WriteOff {
        reason,
        quantity: body.quantity,
        lot: body.lot,
        serials: body.serials,
        reference: reference(&key),
        at: body.at.unwrap_or_else(chrono::Utc::now),
    };

    let committed = crate::write_off(&tenant.db, &product, &taken, &metadata(&tenant))
        .await
        .map_err(|e| refused(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(accepted(key.id().to_string(), committed.at)),
    ))
}

/// **Who may throw stock away**: the capability the write-off route above is
/// judged on, under `/v1/inventory`.
///
/// Named, so that the worker telling somebody a lot is going off tells exactly
/// the people this route would let write it off ([`WRITE_OFF`]). Change the
/// route's capability here and whoever is told changes with it.
pub type WritesOff = PostEntries;

/// [`WritesOff`], as the value a permission check takes.
pub const WRITE_OFF: erp_tenant::Capability = <WritesOff as erp_web::Capability>::CAPABILITY;

/// What is on hand, per product and branch.
#[utoipa::path(
    get,
    path = "/v1/inventory/stock",
    tag = "inventory",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("product" = Option<String>, Query, description = "Only this product."),
        ("branch" = Option<String>, Query, description = "Only this branch. Absent means every branch; not the request's `X-Branch`."),
        ("limit" = Option<i64>, Query, description = "Up to 200. Defaults to 50."),
        ("after" = Option<String>, Query, description = "The `next` from the previous page."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = Paged<StockView>),
        (status = BAD_REQUEST, description = "Not a cursor", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_stock(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<StockQuery>,
) -> Result<Json<Paged<StockView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = query.page.cursor(locale)?;
    // A confined member reads their own shelves and no other's.
    let branch = tenant.branch_scope(query.branch.as_deref(), locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let found = crate::stock(
        &mut conn,
        query.product.as_deref(),
        branch.as_deref(),
        query.page.limit(PAGE, MAX_PAGE),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;
    drop(conn);

    Ok(Json(Paged::of(found, |row: crate::StockRow| StockView {
        product: row.product,
        name: row.name,
        branch: row.branch,
        on_hand: row.on_hand,
        value: amount(row.value, row.currency),
        last_at: row.last_at,
    })))
}

/// **What is on the shelf by batch, and what expires when.**
///
/// Open lots only, earliest expiry first, undated last, oldest received first
/// among equals — which is the order stock will actually go out in, because it
/// is the same sort the picking rule follows. The list somebody reads before
/// they count, and before they decide what to write off.
#[utoipa::path(
    get,
    path = "/v1/inventory/lots",
    tag = "inventory",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("product" = Option<String>, Query, description = "Only this product."),
        ("branch" = Option<String>, Query, description = "Only this branch. Absent means every branch; not the request's `X-Branch`."),
        ("expiring_before" = Option<String>, Query, description = "Only lots that go off before this day. Undated lots are never in the answer."),
        ("limit" = Option<i64>, Query, description = "Up to 200. Defaults to 50."),
        ("after" = Option<String>, Query, description = "The `next` from the previous page."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = Paged<LotView>),
        (status = BAD_REQUEST, description = "Not a cursor, or not a date", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_lots(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<LotQuery>,
) -> Result<Json<Paged<LotView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = query.page.cursor(locale)?;
    let branch = tenant.branch_scope(query.branch.as_deref(), locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let found = crate::lots(
        &mut conn,
        query.product.as_deref(),
        branch.as_deref(),
        query.expiring_before,
        query.page.limit(PAGE, MAX_PAGE),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;
    drop(conn);

    Ok(Json(Paged::of(found, |row: crate::LotRow| LotView {
        id: row.id,
        product: row.product,
        name: row.name,
        branch: row.branch,
        code: row.code,
        expires_on: row.expires_on,
        quantity: row.quantity,
        remaining: row.remaining,
        value: Amount {
            minor: row.value,
            currency: row.currency,
        },
        serials: row.serials,
        received_at: row.received_at,
    })))
}

/// **The shelves at a glance**: per branch, what they are worth, how many
/// products are on them, what is going off and what has gone, and which shelves
/// are below zero.
///
/// One row per branch with a shelf, or only `branch` when it is asked for.
/// **Worth is per currency**: a tenant whose inventory account changed currency
/// holds stock in both, and adding them would be a number nobody could use.
///
/// **Going off is the tenant's own window, from the tenant's own today.** A lot
/// counts as `expiring` when its date is from today to `expiring_through`, and
/// as `expired` before today while anything is on it; an undated lot is
/// neither. That is the rule the notification bell is rung by, and `today` is
/// the day on the tenant's calendar, not UTC's.
///
/// **What it adds up is what the lists show.** `value` and `products` are
/// `GET /v1/inventory/stock?branch=` added up, and the lot counts are the open
/// lots `GET /v1/inventory/lots?branch=` lists, before and through those days.
/// A shelf below zero comes with what it owes — what its units no lot covered
/// were charged out at — which a count settles and a delivery does not.
#[utoipa::path(
    get,
    path = "/v1/inventory/summary",
    tag = "inventory",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("branch" = Option<String>, Query, description = "Only this branch. Absent means every branch; not the request's `X-Branch`."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = StockSummary),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn stock_summary(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<ByBranch>,
) -> Result<Json<StockSummary>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let window = ExpiryWindow::resolve(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale, &CATALOG))?;
    // The tenant's day: at one in the morning on the 12th in Riyadh a batch
    // dated the 11th has gone, though it is still the 11th in UTC.
    let today = erp_eventlog::configuration::calendar(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale, &CATALOG))?
        .day(chrono::Utc::now());
    let expiring_through = window.warns_until(today);
    let branch = tenant.branch_scope(query.branch.as_deref(), locale)?;
    let found = crate::summary(&mut conn, branch.as_deref(), today, expiring_through)
        .await
        .map_err(|e| database(&e, locale))?;
    drop(conn);

    Ok(Json(StockSummary {
        today,
        expiring_through,
        branches: found
            .into_iter()
            .map(|row| BranchStock {
                branch: row.branch,
                value: row
                    .value
                    .into_iter()
                    .map(|(currency, minor)| Amount { minor, currency })
                    .collect(),
                products: row.products,
                expiring: row.expiring,
                expired: row.expired,
                below_zero: row
                    .below_zero
                    .into_iter()
                    .map(|short| StockBelowZero {
                        product: short.product,
                        name: short.name,
                        on_hand: short.on_hand,
                        owes: amount(short.owes, short.currency),
                    })
                    .collect(),
            })
            .collect(),
    }))
}

/// Why a quantity is what it is: every movement, newest first.
///
/// One row per lot a movement touched, so a write-off that came off two batches
/// is two rows. What a lot holds is the sum of the rows against it.
#[utoipa::path(
    get,
    path = "/v1/inventory/movements",
    tag = "inventory",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("product" = Option<String>, Query, description = "Only this product."),
        ("limit" = Option<i64>, Query, description = "Up to 200. Defaults to 50."),
        ("after" = Option<String>, Query, description = "The `next` from the previous page."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = Paged<MovementView>),
        (status = BAD_REQUEST, description = "Not a cursor", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_movements(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<ByProduct>,
) -> Result<Json<Paged<MovementView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = query.page.cursor(locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let found = crate::movements(
        &mut conn,
        query.product.as_deref(),
        query.page.limit(PAGE, MAX_PAGE),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;
    drop(conn);

    Ok(Json(Paged::of(found, |row: crate::MovementRow| {
        MovementView {
            product: row.product,
            branch: row.branch,
            lot: row.lot,
            kind: row.kind,
            reason: row.reason,
            quantity: row.quantity,
            value: amount(row.value, row.currency),
            expected: row.expected,
            declared: row.declared,
            reference: row.reference,
            moved_at: row.moved_at,
        }
    })))
}

/// Where stock posts.
///
/// A receipt debits inventory and credits goods received not invoiced, a
/// write-off's loss goes to the waste account against inventory, a count's
/// discrepancy to the variance account against the same one, and what a
/// document consumes to cost of goods sold. The holding account is the one the
/// supplier's bill relieves, on a line that names a stocked product.
///
/// **The two loss accounts default to the same code.** They are separate fields
/// so a business can put spoilage somewhere other than shrinkage in one request,
/// not because the shipped charts separate them.
#[utoipa::path(
    get,
    path = "/v1/inventory/posting-accounts",
    tag = "inventory",
    params(("Host" = String, Header, description = "The tenant's subdomain.")),
    responses(
        (status = OK, body = StockAccounts, headers(("ETag" = String, description = "The version of this setting. Send it back as `If-Match` to write only if nobody else has since."))),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable inventory", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn stock_accounts(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Versioned<StockAccounts>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let version = erp_eventlog::configuration::version_of(&mut conn, crate::PostingAccounts::KEY)
        .await
        .map_err(|e| config_problem(&e, locale, &CATALOG))?;
    let accounts = crate::PostingAccounts::resolve(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale, &CATALOG))?;
    drop(conn);

    Ok(Versioned(
        version,
        StockAccounts {
            inventory: accounts.inventory.to_string(),
            goods_received: accounts.goods_received.to_string(),
            cogs: accounts.cogs.to_string(),
            variance: accounts.variance.to_string(),
            waste: accounts.waste.to_string(),
        },
    ))
}

/// Choose them.
///
/// `ManageAccounts`, not `ManageTenant`, for the reason `sales` gives: this is a
/// decision about the chart of accounts, and the person who maintains the chart
/// is the person who should make it.
///
/// **Not retrospective.** Entries already posted keep the accounts they were
/// posted to, because those went into the journal entry as values (L5).
///
/// Each code is checked against the tenant's own chart before it is stored —
/// asked of the **log**, not of `proj_ledger.account`, because the read model
/// lags and would refuse a chart installed a second ago.
#[utoipa::path(
    put,
    path = "/v1/inventory/posting-accounts",
    tag = "inventory",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("If-Match" = Option<String>, Header, description = "The `ETag` a GET answered with. With it, the write happens only if the setting is still at that version; without it, unconditionally."),
    ),
    request_body = StockAccounts,
    responses(
        (status = NO_CONTENT, description = "Stored. Applies to the next entry this module posts; entries already posted keep the accounts they were posted to."),
        (status = PRECONDITION_FAILED, description = "`If-Match` named a version that is no longer current; reload and try again", body = Problem),
        (status = BAD_REQUEST, description = "An unusable code, or one that is not an open account here", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable inventory", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn set_stock_accounts(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    IfMatch(expected): IfMatch,
    Json(body): Json<StockAccounts>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let accounts = crate::PostingAccounts {
        inventory: parse_id(&body.inventory, locale)?,
        goods_received: parse_id(&body.goods_received, locale)?,
        cogs: parse_id(&body.cogs, locale)?,
        variance: parse_id(&body.variance, locale)?,
        waste: parse_id(&body.waste, locale)?,
    };

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    for code in accounts.all() {
        let usable = ledger::accepts_postings(&mut conn, code)
            .await
            .map_err(|e| {
                tracing::error!(%e, "an account could not be checked");
                Problem::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &Message::new(crate::messages::DATABASE),
                    locale,
                    &CATALOG,
                )
            })?;
        if !usable {
            return Err(Problem::new(
                StatusCode::BAD_REQUEST,
                &Message::new(ledger::messages::NO_SUCH_ACCOUNT)
                    .with("code", MessageArg::text(code.as_str().to_owned())),
                locale,
                &CATALOG,
            ));
        }
    }

    erp_eventlog::configuration::set(
        &mut conn,
        crate::PostingAccounts::KEY,
        &accounts,
        Some(&tenant.session.identity.to_string()),
        expected,
    )
    .await
    .map_err(|e| config_problem(&e, locale, &CATALOG))?;

    Ok(StatusCode::NO_CONTENT)
}

/// How long before an expiry the business wants warning.
///
/// The worker reads it: each open lot that reaches its date within this many
/// days, and each one already past it, is announced once to whoever may write
/// stock off, on their notification bell. It warns and nothing more — stock
/// leaves through a write-off. `GET /v1/inventory/summary` counts the same lots
/// against it.
/// The warning needs the `notifications` module: without it nothing is
/// announced, and the window only drives the summary's counts.
/// `GET /v1/inventory/lots?expiring_before=` takes a day per request and does
/// not fall back to it.
#[utoipa::path(
    get,
    path = "/v1/inventory/expiry-window",
    tag = "inventory",
    params(("Host" = String, Header, description = "The tenant's subdomain.")),
    responses(
        (status = OK, body = StockExpiryWindow, headers(("ETag" = String, description = "The version of this setting. Send it back as `If-Match` to write only if nobody else has since."))),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable inventory", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn expiry_window(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Versioned<StockExpiryWindow>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let version = erp_eventlog::configuration::version_of(&mut conn, ExpiryWindow::KEY)
        .await
        .map_err(|e| config_problem(&e, locale, &CATALOG))?;
    let window = ExpiryWindow::resolve(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale, &CATALOG))?;
    drop(conn);

    Ok(Versioned(version, StockExpiryWindow { days: window.days }))
}

/// Choose it.
#[utoipa::path(
    put,
    path = "/v1/inventory/expiry-window",
    tag = "inventory",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("If-Match" = Option<String>, Header, description = "The `ETag` a GET answered with."),
    ),
    request_body = StockExpiryWindow,
    responses(
        (status = NO_CONTENT, description = "Stored."),
        (status = PRECONDITION_FAILED, description = "`If-Match` named a version that is no longer current", body = Problem),
        (status = BAD_REQUEST, description = "Not a number of days between none and ten years", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable inventory", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn set_expiry_window(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
    IfMatch(expected): IfMatch,
    Json(body): Json<StockExpiryWindow>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let window = ExpiryWindow::new(body.days).map_err(|e| malformed(&e.message(), locale))?;

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    erp_eventlog::configuration::set(
        &mut conn,
        ExpiryWindow::KEY,
        &window,
        Some(&tenant.session.identity.to_string()),
        expected,
    )
    .await
    .map_err(|e| config_problem(&e, locale, &CATALOG))?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Views and refusals
// ---------------------------------------------------------------------------

/// **The reference a movement is recorded under: the request's own key**, so
/// nothing is minted here (L8) and a retry records one movement rather than
/// two. A receipt's lot is named after it too.
///
/// Not `creating(&tenant, &key)`, which is the shape a *create* takes: a
/// movement lands on a stream that already exists and often has years of
/// history, so `try_create`'s fingerprint would never be read. The shelf's
/// bounded window of references already heard is what makes the retry nothing.
fn reference(key: &IdempotencyKey) -> String {
    key.id().to_string()
}

fn accepted(id: String, at: Option<erp_types::LogPosition>) -> InventoryAccepted {
    InventoryAccepted {
        id,
        position: at.map(erp_types::LogPosition::get),
    }
}

/// `None` when nothing has ever been received onto a shelf: there is no
/// currency to state a zero in, and a zero in a currency nobody chose would be
/// a number this system made up.
fn amount(minor: i64, currency: Option<String>) -> Option<Amount> {
    currency.map(|currency| Amount { minor, currency })
}

/// A request this file could not turn into a command — a word that is not one
/// of a fixed set, a window out of range. Always the shape of the request and
/// never the state of the world, so always a 400.
fn malformed(message: &Message, locale: Locale) -> Problem {
    Problem::new(StatusCode::BAD_REQUEST, message, locale, &CATALOG)
}

fn pool(error: &erp_tenant::PoolError, locale: Locale) -> Problem {
    Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, error, locale, &CATALOG)
}

fn database(error: &sqlx::Error, locale: Locale) -> Problem {
    tracing::error!(%error, "stock could not be read");
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        &Message::new(crate::messages::DATABASE),
        locale,
        &CATALOG,
    )
}

fn refused(error: &CommandError<InventoryError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            if rejection.is_malformed() {
                StatusCode::BAD_REQUEST
            } else {
                // Well-formed, refused on the state of the world.
                StatusCode::UNPROCESSABLE_ENTITY
            },
            rejection.message(),
        ),
        CommandError::Pool(e @ erp_tenant::PoolError::Overloaded { .. }) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.message())
        }
        CommandError::Execute(
            ExecuteError::Contended { .. } | ExecuteError::AlreadyExists { .. },
        ) => (
            StatusCode::CONFLICT,
            Message::new(erp_eventlog::messages::CONCURRENT_MODIFICATION),
        ),
        other => {
            tracing::error!(error = %other, "an inventory command failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Message::new(crate::messages::DATABASE),
            )
        }
    };
    Problem::new(status, &message, locale, &CATALOG)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A count carries its lot and its serials into the command**, and one
    /// that names neither is a count of the shelf. A field dropped in this
    /// translation is a serial count refused for naming nothing, or a count of
    /// one batch that quietly counts the whole shelf.
    #[test]
    fn a_count_carries_its_lot_and_its_serials_to_the_command() {
        let sent: NewStockCount = serde_json::from_value(serde_json::json!({
            "lot": "lot.x",
            "declared": 2,
            "serials": ["A-1", "A-2"]
        }))
        .expect("a count of one lot");
        let count = sent.into_count("cnt-1".to_owned());
        assert_eq!(count.lot.as_deref(), Some("lot.x"));
        assert_eq!(count.declared, 2);
        assert_eq!(count.serials, ["A-1", "A-2"]);
        assert_eq!(count.reference, "cnt-1");

        let shelf: NewStockCount = serde_json::from_value(serde_json::json!({ "declared": 21 }))
            .expect("a count of the shelf");
        let count = shelf.into_count("cnt-2".to_owned());
        assert_eq!(count.lot, None, "a count that names no lot is of the shelf");
        assert_eq!(count.declared, 21);
        assert!(count.serials.is_empty());
    }
}
