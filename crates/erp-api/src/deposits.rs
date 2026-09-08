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
//!
//! # Two ways to pay, one route
//!
//! A **card** is paid in the customer's own browser: the gateway's form creates
//! the charge against the publishable key and this system's id, and the worker
//! asks whether they have. A **lender** — Tabby, Tamara — hosts its own page and
//! has to be told about the order first: who is buying, where the service is
//! delivered, where to send them afterwards. That is an outbound call, so it is
//! the worker's (`payments::open_checkouts`), and the page it answers with is
//! what the `GET` beside this route hands the waiting customer. Everything the
//! lender is told is frozen on the request — see `payments::Checkout` — so what
//! this system recorded telling it is what it said.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use erp_i18n::Locale;
use erp_types::Timestamp;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::{AppState, IdempotencyKey, Json, Language, Problem, Public};
use erp_web::{parse_id, publicly, require_module};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(public_deposit, public_deposit_status))
        .routes(routes!(public_verification))
}

static CATALOG: erp_i18n::Composite = erp_i18n::Composite::new(&[
    &booking::CATALOG,
    &messaging::CATALOG,
    &payments::CATALOG,
    &ledger::CATALOG,
    &erp_web::CATALOG,
]);

/// The provider a deposit is paid through when the request names none.
const CARD: &str = "moyasar";

/// How the customer wants to pay. **Optional, and empty is a card.**
#[derive(Debug, Default, Deserialize, ToSchema)]
#[schema(example = json!({
    "provider": "tabby",
    "email": "sara@example.com",
    "return_to": {
        "success": "https://salon.example/booked",
        "cancel": "https://salon.example/cancelled",
        "failure": "https://salon.example/declined"
    }
}))]
struct DepositRequest {
    /// `moyasar` (the default), `tabby` or `tamara`. Only one this business has
    /// configured; anything else is refused by name.
    #[serde(default)]
    provider: Option<String>,
    /// **Required by a lender**, which scores the buyer before it will lend.
    /// Never shown to staff, and never used to match a customer record.
    #[serde(default)]
    email: Option<String>,
    /// E.164, or something a person would write that reads as one. Falls back
    /// to the number the booking was made with. Required by a lender, which
    /// sends its one-time code there.
    #[serde(default)]
    phone: Option<String>,
    /// **Where the customer lands when the lender is done with them.** Required
    /// by a lender; each must be on an origin this business has allowed.
    #[serde(default)]
    return_to: Option<ReturnTo>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
struct ReturnTo {
    success: String,
    /// The customer backed out.
    cancel: String,
    /// The lender said no — a normal outcome, and the page should offer a card.
    failure: String,
}

/// What the customer has to pay, and the charge to pay it against.
#[derive(Debug, Serialize, ToSchema)]
struct DepositDue {
    /// **The charge to pay.** For a card, give this to the gateway's form as its
    /// own id — Moyasar's `given_id` — so what the customer pays is this charge
    /// and not one the browser chose. For a lender, poll `GET` on this same
    /// path until `pay_at` names the page to send them to.
    payment: String,
    /// Which provider it is paid through.
    provider: String,
    /// **Where the customer goes to pay**, when a lender hosts the page. `null`
    /// until the worker has opened the checkout — a tick, not a request — and
    /// always `null` for a card, which is paid in the browser.
    pay_at: Option<String>,
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

/// Where a booking's deposit has got to.
#[derive(Debug, Serialize, ToSchema)]
struct DepositStatus {
    payment: String,
    provider: String,
    /// `requested` (asked for, nothing at the gateway yet), `pending` (at the
    /// gateway, waiting on the customer or a capture), `settled`, `failed`.
    stage: String,
    /// Where to send the customer, while there is somewhere. See
    /// [`DepositDue::pay_at`].
    pay_at: Option<String>,
    amount: i64,
    currency: String,
    /// Whether the money is real and the slot is theirs.
    paid: bool,
    /// When the hold lapses, while it is still a hold.
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    due_by: Option<Timestamp>,
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
/// With no body, or `provider: "moyasar"`, the browser then pays that charge
/// against the gateway's publishable key, giving it this `payment` as its own
/// id. This system finds out by asking the gateway, not by being told: a
/// callback is a doorbell, and what a browser reports is not evidence.
///
/// With a lender (`tabby`, `tamara`) the request has to carry what the lender
/// asks — an email, a phone, and where to send the customer afterwards — and
/// the page to send them to arrives a moment later on `GET`, because opening a
/// checkout is a call to a third party and this system makes those from the
/// worker.
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
    request_body(content = DepositRequest, description = "Optional. Empty is a card paid in the browser; a lender needs the rest.", content_type = "application/json"),
    security(),
    responses(
        (status = CREATED, description = "The charge to pay. When this booking already has a deposit in flight, the answer names *that* charge rather than making a second one.", body = DepositDue),
        (status = BAD_REQUEST, description = "Not an id, a key that is not a UUID, a provider this business does not take, something the lender needs missing, or a return address on a site the business has not allowed", body = Problem),
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
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<DepositDue>), Problem> {
    // **Both modules, and the same 404 for either missing.** `require_module`
    // names the module it did not find, and on this surface that would tell a
    // stranger which modules a business pays for. Every refusal here is the
    // one answer, `nothing_here`.
    if !caller.db.has_module(&booking::module_id()) || !caller.db.has_module(&payments::module_id())
    {
        return Err(nothing_here(locale));
    }
    let reservation = parse_id(&reservation, locale)?;

    // **Off unless the business turned it on**, and a 404 rather than a 403 for
    // the reason the reservation route gives: "forbidden" would confirm the
    // route works for somebody else, which is neither true nor their business.
    if !public_settings(&caller, locale).await?.open {
        return Err(nothing_here(locale));
    }

    // **Moyasar's rule, said here rather than discovered in the worker.** The
    // key becomes `given_id`, which must be a UUID — and a lender is given it
    // as the order reference, where the same shape does no harm.
    if !is_uuid(key.id().as_str()) {
        return Err(erp_web::bad_request(
            erp_web::messages::MALFORMED_BODY,
            "reason",
            "the Idempotency-Key must be a UUID: it becomes the gateway's own id for the charge",
            locale,
        ));
    }

    // **An empty body is a card.** Read by hand rather than through the JSON
    // extractor so a site that never sends one keeps working.
    let request: DepositRequest = if body.is_empty() {
        DepositRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            erp_web::bad_request(
                erp_web::messages::MALFORMED_BODY,
                "reason",
                &e.to_string(),
                locale,
            )
        })?
    };
    let provider = request.provider.clone().unwrap_or_else(|| CARD.to_owned());
    offered(&caller, &state, &provider, locale).await?;

    let Owed {
        hold: owed,
        amount,
        tax,
        detail,
    } = owed_for(&caller.db, &reservation, locale).await?;

    // **A lender is told everything up front**, frozen on the request. A card
    // is told nothing: the browser creates that charge itself.
    let checkout = if provider == CARD {
        None
    } else {
        Some(
            checkout_for(
                &caller,
                &state,
                &headers,
                &provider,
                &request,
                &detail,
                (amount, tax),
                locale,
            )
            .await?,
        )
    };

    let (payment, chosen, pay_at) = record(
        &caller,
        &key,
        Asked {
            provider,
            reservation,
            net: owed.deposit,
            buyer: detail.summary.customer_name.clone(),
            amount,
            checkout,
        },
        locale,
    )
    .await?;

    erp_web::nudge(&state, caller.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(DepositDue {
            payment,
            provider: chosen,
            pay_at,
            amount: amount.minor(),
            currency: amount.currency().to_string(),
            net: owed.deposit.minor(),
            tax: tax.minor(),
            due_by: owed.due_by,
        }),
    ))
}

/// What a deposit request has settled on, for [`record`].
struct Asked {
    provider: String,
    reservation: erp_types::AggregateId,
    /// Before tax: what the prepayment invoice is raised for.
    net: erp_types::Money,
    buyer: String,
    amount: erp_types::Money,
    checkout: Option<payments::Checkout>,
}

/// Records the charge, and answers which one the customer pays.
///
/// **One charge per booking, whatever key the browser minted.** A second tab
/// does not get a second charge: `payments` refuses it against the log and
/// names the one that exists, and that is the one this answers with — same
/// amounts, because they come from the same booking, and with wherever it has
/// already got to. Everything else that can go wrong is either the booking not
/// being there to pay for (a 404, like every other refusal on this surface) or
/// the database being unwell (a 503, and a retry), and the two must not be
/// confused: the first version answered 404 to both, and a pool hiccup told
/// customers their booking was gone.
///
/// Answers `(payment, provider, pay_at)`.
async fn record(
    caller: &Public,
    key: &IdempotencyKey,
    asked: Asked,
    locale: Locale,
) -> Result<(String, String, Option<String>), Problem> {
    let mut tx = caller
        .db
        .begin()
        .await
        .map_err(|e| unavailable(&e, locale))?;
    let requested = payments::request_in(
        &mut tx,
        key.id(),
        &payments::Collection {
            // **No card.** Nobody here charges this; the customer's browser
            // creates it against the publishable key, or the worker opens the
            // lender's checkout, and the worker asks the gateway whether they
            // have paid.
            card: None,
            provider: asked.provider.clone(),
            collects: payments::Collects::Advance(payments::Advance {
                against: asked.reservation,
                net: asked.net,
                buyer: payments::Buyer {
                    name: asked.buyer,
                    vat_number: None,
                },
            }),
            amount: asked.amount,
            // **Empty, and it is not needed here.** A callback URL is where
            // the *worker* sends a customer when it creates a card charge for
            // them; a browser-created charge lands wherever the site says, and
            // a lender's landing pages travel on the checkout.
            callback_url: String::new(),
            checkout: asked.checkout,
        },
        chrono::Utc::now(),
        &publicly(key),
    )
    .await;

    let existing = match requested {
        Ok(_) => {
            tx.commit().await.map_err(|e| unavailable(&e, locale))?;
            return Ok((key.id().to_string(), asked.provider, None));
        }
        Err(erp_eventlog::ExecuteError::Rejected(payments::PaymentsError::AlreadyAwaited {
            payment,
            ..
        })) => {
            tx.rollback().await.map_err(|e| unavailable(&e, locale))?;
            payment
        }
        Err(erp_eventlog::ExecuteError::Rejected(_)) => {
            tx.rollback().await.map_err(|e| unavailable(&e, locale))?;
            return Err(nothing_here(locale));
        }
        Err(e) => {
            tx.rollback().await.map_err(|e| unavailable(&e, locale))?;
            return Err(unavailable(&e, locale));
        }
    };

    // The charge that already existed may already have somewhere to pay, and
    // was made through whichever provider the first tab chose.
    let mut conn = caller
        .db
        .read()
        .await
        .map_err(|e| unavailable(&e, locale))?;
    let row = payments::payment(&mut conn, &existing)
        .await
        .map_err(|e| unavailable(&e, locale))?;
    Ok(match row {
        Some(row) => (existing, row.provider, row.pay_at),
        None => (existing, asked.provider, None),
    })
}

/// Where a booking's deposit has got to, and where to pay it.
///
/// **The read beside the write.** A customer sent to a lender waits here for
/// the page to go to, and a site that lost the answer to `POST` — a reload, a
/// second tab — finds it again. Public, keyed on the booking, and it says
/// nothing a stranger could not learn by asking to pay: the amount, and where.
#[utoipa::path(
    get,
    path = "/v1/booking/public/reservations/{reservation}/deposit",
    tag = "booking",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — which is how a public request names the business."),
        ("reservation" = String, Path, description = "From `POST /v1/booking/public/reservations`."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the booking read model to reach this log position — the one a stream's `advanced` named."),
    ),
    security(),
    responses(
        (status = OK, body = DepositStatus),
        (status = BAD_REQUEST, description = "Not an id", body = Problem),
        (status = NOT_FOUND, description = "No such business, it does not take bookings online, or no deposit has been asked for against that booking", body = Problem),
        (status = TOO_MANY_REQUESTS, description = "This surface is bounded per origin and per business.", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn public_deposit_status(
    caller: Public,
    Language(locale): Language,
    Path(reservation): Path<String>,
    consistency: erp_web::Consistency,
) -> Result<Json<DepositStatus>, Problem> {
    if !caller.db.has_module(&booking::module_id()) || !caller.db.has_module(&payments::module_id())
    {
        return Err(nothing_here(locale));
    }
    let reservation = parse_id(&reservation, locale)?;
    if !public_settings(&caller, locale).await?.open {
        return Err(nothing_here(locale));
    }
    // The phone re-fetches at the position a stream named; it must not read a
    // row the worker has not written yet.
    consistency
        .wait_for(
            &caller.db,
            <booking::Booking as erp_projection::ProjectionGroup>::NAME,
            locale,
        )
        .await?;

    let mut conn = caller
        .db
        .read()
        .await
        .map_err(|e| unavailable(&e, locale))?;
    let row = payments::awaited_for(&mut conn, reservation.as_str())
        .await
        .map_err(|e| unavailable(&e, locale))?
        .ok_or_else(|| nothing_here(locale))?;
    let hold = booking::awaiting_deposit(&mut conn, reservation.as_str())
        .await
        .map_err(|e| unavailable(&e, locale))?;

    Ok(Json(DepositStatus {
        payment: row.id,
        provider: row.provider,
        paid: row.stage == "settled",
        stage: row.stage,
        pay_at: row.pay_at,
        amount: row.amount.minor(),
        currency: row.amount.currency().to_string(),
        due_by: hold.map(|h| h.due_by),
    }))
}

/// Whether this business takes payment through `provider`.
///
/// **Configured, not merely known.** A charge asked for at a provider whose
/// credentials the worker does not hold is a charge nobody will ever confirm —
/// a customer who paid, waiting on a slot that lapses. Refused here, by name:
/// which providers a business takes is what its payment form shows, not a
/// secret.
async fn offered(
    caller: &Public,
    state: &AppState,
    provider: &str,
    locale: Locale,
) -> Result<(), Problem> {
    let not_offered = || {
        Problem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(payments::messages::PROVIDER_NOT_OFFERED)
                .with("provider", erp_i18n::MessageArg::text(provider)),
            locale,
            &CATALOG,
        )
    };
    if !payments::PROVIDERS.contains(&provider) {
        return Err(not_offered());
    }
    let Some(sealing) = state.sealing.as_ref() else {
        return Err(not_offered());
    };
    let mut conn = caller
        .db
        .acquire()
        .await
        .map_err(|e| unavailable(&e, locale))?;
    match payments::credentials(&mut conn, sealing, provider).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(not_offered()),
        Err(e) => Err(unavailable(&e, locale)),
    }
}

/// Everything a lender has to be told, from the request and what the modules
/// around this route know. Each thing missing is refused by name.
#[expect(
    clippy::too_many_arguments,
    reason = "every one is a value already in hand; a struct would only rename them"
)]
async fn checkout_for(
    caller: &Public,
    state: &AppState,
    headers: &HeaderMap,
    provider: &str,
    request: &DepositRequest,
    detail: &booking::ReservationDetail,
    money: (erp_types::Money, erp_types::Money),
    locale: Locale,
) -> Result<payments::Checkout, Problem> {
    let (amount, tax) = money;
    let needs = |what: &str| {
        Problem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(payments::messages::LENDER_NEEDS)
                .with("provider", erp_i18n::MessageArg::text(provider))
                .with("what", erp_i18n::MessageArg::text(what)),
            locale,
            &CATALOG,
        )
    };

    let email = request
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| e.contains('@'))
        .ok_or_else(|| needs("the customer's email address"))?;
    let phone = request
        .phone
        .as_deref()
        .or(detail.summary.customer_phone.as_deref())
        .and_then(erp_types::phone::normalise)
        .ok_or_else(|| needs("the customer's mobile number"))?;
    let return_to = request
        .return_to
        .clone()
        .ok_or_else(|| needs("where to send the customer afterwards"))?;
    for url in [&return_to.success, &return_to.cancel, &return_to.failure] {
        landing_allowed(caller, state, url, locale).await?;
    }
    let deliver_to = place_for(&caller.db, detail)
        .await
        .map_err(|e| unavailable(&e, locale))?
        .ok_or_else(|| {
            needs("an address for where the service is delivered: a branch, or the business's ZATCA registration")
        })?;

    // **Where the lender reports, server to server**: this system's hook on
    // the host this request came in on, which is the tenant's own.
    let notification = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|host| format!("https://{host}/v1/hooks/{provider}"));

    let what: Vec<&str> = detail.lines.iter().map(|l| l.what.as_str()).collect();
    let title = format!("{}: {}", deposit_word(locale), what.join("، "));

    Ok(payments::Checkout {
        shopper: payments::Shopper {
            name: detail.summary.customer_name.clone(),
            email: email.to_owned(),
            phone,
            // A stranger became a customer when they booked.
            since: chrono::Utc::now(),
            purchases: 0,
        },
        deliver_to,
        landing: payments::Landing {
            success: return_to.success,
            cancel: return_to.cancel,
            failure: return_to.failure,
            notification,
        },
        description: title.clone(),
        items: vec![payments::Line {
            title,
            category: "Services".to_owned(),
            quantity: 1,
            unit_price: amount,
        }],
        tax,
    })
}

/// **A landing page has to be on a site the business has allowed.** The same
/// list CORS answers from, so a lender sends the customer back to the
/// business's own site and never to a page somebody else named.
async fn landing_allowed(
    caller: &Public,
    state: &AppState,
    url: &str,
    locale: Locale,
) -> Result<(), Problem> {
    let refused = || {
        Problem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(payments::messages::LANDING_NOT_ALLOWED)
                .with("url", erp_i18n::MessageArg::text(url)),
            locale,
            &CATALOG,
        )
    };
    let origin = url
        .strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .filter(|host| !host.is_empty())
        .map(|host| format!("https://{}", host.to_lowercase()))
        .ok_or_else(refused)?;
    let allowed = state
        .control
        .origins(caller.db.tenant())
        .await
        .map_err(|e| unavailable(&e, locale))?;
    if allowed.contains(&origin) {
        Ok(())
    } else {
        Err(refused())
    }
}

/// Where the service is delivered, for a lender that requires an address.
///
/// The branch the first resource belongs to, when the business keeps branches;
/// otherwise the address it registered with ZATCA. `None` when it has neither,
/// which is refused by name rather than filled with dashes — a placeholder is
/// one more thing the lender scores.
async fn place_for(
    db: &erp_control::TenantDb,
    detail: &booking::ReservationDetail,
) -> Result<Option<payments::Place>, sqlx::Error> {
    let mut conn = db
        .read()
        .await
        .map_err(|e| sqlx::Error::Io(std::io::Error::other(e.to_string())))?;

    if db.has_module(&branches::module_id()) {
        for held in detail.lines.iter().flat_map(|line| line.takes.iter()) {
            let Some(resource) = booking::resource(&mut conn, held.resource.as_str()).await? else {
                continue;
            };
            let Some(branch) = resource.summary.branch else {
                continue;
            };
            if let Some(branch) = branches::branch(&mut conn, &branch).await? {
                let address = branch.address;
                let line = match address.building {
                    Some(building) => format!("{} {building}", address.street),
                    None => address.street,
                };
                return Ok(Some(payments::Place {
                    line,
                    city: address.city,
                    postcode: address.postal_code.unwrap_or_default(),
                    country: address.country,
                }));
            }
        }
    }

    if db.has_module(&tax_sa::module_id())
        && let Some(registration) = tax_sa::registered(&mut conn).await?
    {
        let address = registration.address;
        return Ok(Some(payments::Place {
            line: format!("{} {}", address.street, address.building),
            city: address.city,
            postcode: address.postal_code,
            country: address.country,
        }));
    }

    Ok(None)
}

/// What the lender's line is called, in the customer's language.
const fn deposit_word(locale: Locale) -> &'static str {
    match locale {
        Locale::Arabic => "عربون حجز",
        Locale::English => "Booking deposit",
    }
}

pub(crate) async fn public_settings(
    caller: &Public,
    locale: Locale,
) -> Result<booking::PublicBooking, Problem> {
    let mut conn = caller
        .db
        .read()
        .await
        .map_err(|e| unavailable(&e, locale))?;
    booking::PublicBooking::resolve(&mut conn)
        .await
        .map_err(|e| Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &CATALOG))
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

/// What a booking is owed as a deposit, and by whom: what `booking` recorded,
/// the same with tax at the tenant's rate, and the booking itself for the
/// prepayment invoice and for what a lender is told.
struct Owed {
    /// The hold as `booking` recorded it: the net asked for, and by when.
    hold: booking::Lapsed,
    amount: erp_types::Money,
    tax: erp_types::Money,
    detail: booking::ReservationDetail,
}

/// **Every way this cannot be answered is the same 404.** A booking that does
/// not exist, is not held, or has already been paid for is, to a stranger, a
/// booking that is not waiting to be paid.
async fn owed_for(
    db: &erp_control::TenantDb,
    reservation: &erp_types::AggregateId,
    locale: Locale,
) -> Result<Owed, Problem> {
    let mut conn = db.read().await.map_err(|e| unavailable(&e, locale))?;
    let owed = booking::awaiting_deposit(&mut conn, reservation.as_str())
        .await
        .map_err(|e| unavailable(&e, locale))?
        .ok_or_else(|| nothing_here(locale))?;
    let detail = booking::reservation(&mut conn, reservation.as_str())
        .await
        .map_err(|e| unavailable(&e, locale))?
        .ok_or_else(|| nothing_here(locale))?;
    let (amount, tax) = with_tax(&mut conn, owed.deposit, locale).await?;
    Ok(Owed {
        hold: owed,
        amount,
        tax,
        detail,
    })
}

pub(crate) fn nothing_here(locale: Locale) -> Problem {
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

// ---------------------------------------------------------------------------
// Proving a phone number
// ---------------------------------------------------------------------------

/// A number to send a code to.
#[derive(Debug, serde::Deserialize, ToSchema)]
#[schema(example = json!({ "phone": "+966500000000" }))]
struct NewVerification {
    /// E.164, or something a person would write that reads as one — spaces,
    /// dashes and a leading `00` are all fine. A national number is refused
    /// rather than repaired: `0500000000` is a Saudi number to a Saudi reader
    /// and nothing at all to a message gateway.
    phone: String,
}

/// Send a code to a phone number, so a booking can prove it.
///
/// # Only when the business asks for one
///
/// Off unless they turned it on, and a `404` when they have not — for the same
/// reason the reservation route gives about "forbidden". What stops a booking
/// form being spammed is the **deposit**, not a verified number; what verifying
/// buys is being able to *reach* whoever booked.
///
/// # It says nothing about the number
///
/// The answer is the same whether the number is one this business has seen
/// before or one nobody has ever used, because anything else would turn a public
/// form into a way to ask who a business's customers are.
///
/// # The text is promised in the same transaction as the code
///
/// So a code stored and never sent, or sent and never stored, is not a state
/// this can reach (D9). It goes out on the same effect kind everything else uses
/// — one handler answers for a booking reminder and a verification alike.
#[utoipa::path(
    post,
    path = "/v1/booking/public/verifications",
    tag = "booking",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — which is how a public request names the business."),
        ("Idempotency-Key" = String, Header, description = "Sending it again is a retry, not a second text."),
    ),
    request_body = NewVerification,
    security(),
    responses(
        (status = ACCEPTED, description = "A code is on its way, if that number can receive one."),
        (status = BAD_REQUEST, description = "Not a phone number this can send to", body = Problem),
        (status = NOT_FOUND, description = "No such business, or it does not ask for a verified number", body = Problem),
        (status = TOO_MANY_REQUESTS, description = "A code was sent a moment ago, or this surface is bounded per origin and per business.", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn public_verification(
    caller: Public,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Json(body): Json<NewVerification>,
) -> Result<StatusCode, Problem> {
    require_module(&caller.db, &booking::module_id(), locale)?;

    let settings = public_settings(&caller, locale).await?;
    if !settings.open || !settings.verify_phone {
        return Err(nothing_here(locale));
    }
    // A text is about to cost the business money. Per address and per platform,
    // on top of the per-number cooldown `issue` keeps.
    caller.charge_for_a_code(&state).await?;

    let mut tx = caller
        .db
        .begin()
        .await
        .map_err(|e| unavailable(&e, locale))?;
    let issued = booking::verification::issue(&mut tx, &body.phone, chrono::Utc::now())
        .await
        .map_err(|e| verification_problem(&e, locale))?;

    // **In the same transaction as the code.** A row written whose text was
    // never promised is a customer waiting for a message nobody will send.
    let text = messaging::Outbound {
        channel: messaging::Channel::Sms,
        to: issued.handle.clone(),
        subject: String::new(),
        body: format!("{}: {}", code_word(locale), issued.code),
        locale,
        platform: None,
    };
    let promise = text
        .promised(key.id().to_string())
        // SMS leaves this system, so it has an effect kind. Only the in-system
        // channel does not, and this one is a literal.
        .unwrap_or_else(|| unreachable!("a text message is promised as an effect"));
    erp_eventlog::enqueue(&mut tx, None, &[promise])
        .await
        .map_err(|e| unavailable(&e, locale))?;
    tx.commit().await.map_err(|e| unavailable(&e, locale))?;

    erp_web::nudge(&state, caller.db.tenant()).await;
    Ok(StatusCode::ACCEPTED)
}

/// What the text calls the code.
///
/// **Not a template.** A tenant's own templates are `messaging`'s and a business
/// may write whatever it likes in them; this one message has to go out before
/// anybody has configured anything, or the first customer to try to book cannot.
const fn code_word(locale: Locale) -> &'static str {
    match locale {
        Locale::Arabic => "رمز الحجز",
        Locale::English => "Your booking code",
    }
}

/// **One answer for every way a code can fail**, except the two a caller can act
/// on: a number this cannot read, and a resend asked for too soon.
fn verification_problem(
    error: &booking::verification::VerificationError,
    locale: Locale,
) -> Problem {
    use booking::verification::VerificationError;
    let status = match error {
        VerificationError::NotANumber(_) => StatusCode::BAD_REQUEST,
        VerificationError::TooSoon => StatusCode::TOO_MANY_REQUESTS,
        VerificationError::NotValid => StatusCode::UNPROCESSABLE_ENTITY,
        VerificationError::Database(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    Problem::new(
        status,
        &erp_i18n::Message::new(booking::messages::code_for(error)),
        locale,
        &CATALOG,
    )
}
