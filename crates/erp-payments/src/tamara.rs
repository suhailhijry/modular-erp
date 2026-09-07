//! Buy now, pay later through Tamara.
//!
//! # The amount is an unquoted JSON number, and that is the hard part
//!
//! Tamara's money is `{"amount": 300.50, "currency": "SAR"}` — a JSON *number*
//! in major units, where Tabby wants a quoted string and Moyasar an integer of
//! minor units. All three, in one crate.
//!
//! An unquoted decimal cannot be produced by `serde_json` without going through
//! `f64`, and this workspace forbids floating-point arithmetic for exactly the
//! reason it matters here: `300.50` has no exact binary representation. So the
//! number is written by [`crate::decimal`] — integer division and remainder —
//! and spliced into the body as a **raw JSON token** rather than a value
//! `serde_json` computed.
//!
//! Reading is the same hazard the other way. A response saying `"amount": 300.5`
//! parsed into an `f64` and multiplied by a hundred is where a halala goes
//! missing, so responses are read as raw text and parsed as digits.
//!
//! # `approved` is the merchant's turn, and leaving it there loses the sale
//!
//! When the customer comes back, the order is `approved` — they have paid the
//! first instalment and **the merchant still has to act**. Tamara is blunt:
//! *"orders should NOT be left pending at `approved` status, as it would
//! usually indicate a technical/status sync issue and must be addressed
//! immediately"*, and an order not authorised within 72 hours expires.
//!
//! So `approved` is [`Status::Authorized`], the same as `authorised`: the
//! customer has committed and the money is the merchant's to take, which is
//! exactly what a sweep does with an authorised payment — it captures. The
//! first version read it as [`Status::Initiated`], "somebody still has to do
//! something", and that somebody was this system, waiting on the customer for
//! ever. [`Tamara::capture`] asks the order where it is and authorises first
//! when it has to.
//!
//! # Captured is settled; authorised is not
//!
//! *"❗️ Orders NOT captured are NOT settled to your account!"* Tamara's own
//! wording around `authorised` — "you can consider the order as paid" — is
//! about credit risk rather than cash, and this adapter does not repeat it:
//! `authorised` is [`Status::Authorized`], and only a capture is money.
//!
//! # The notification token proves a sender, not a payload
//!
//! Tamara sends a JWT, HS256, signed with a **Notification Token** that is a
//! different credential from the API token. Its claims are `iss`, `iat` and
//! `exp` and nothing else — no order id, no body hash.
//!
//! Tamara's documentation says this ensures the payload arrived "without any
//! modifications". **It does not.** The token commits to nothing but itself, so
//! anybody who captures one — and it is also sent in the query string, where it
//! lands in access logs — can replay it with a body of their choosing for the
//! rest of its fifteen-minute life.
//!
//! It is therefore treated as what it is: a short-lived bearer credential that
//! says Tamara sent *something*. The answer is an order id, and the truth comes
//! from asking Tamara.
//!
//! Two things their own SDK does not do and this does: the algorithm is
//! **pinned** to HS256 rather than read from the token, and `iss` is checked.
//!
//! # Two messages, one token
//!
//! Tamara talks back in two shapes, and both carry the same token. A **webhook**
//! — registered once for the account in the partner portal — is
//! `{order_id, order_reference_id, order_number, event_type, data}`, where
//! `event_type` is `order_approved`, `order_authorised`, `order_captured`,
//! `order_refunded`, `order_canceled`, `order_declined` or `order_expired`, and
//! `data` is whatever that event has to say (`capture_id`, `refund_id`,
//! `cancel_id`, a declined reason) or an empty list. A **notification** — sent to
//! the `merchant_url.notification` a checkout names, when it names one — is
//! `{order_id, order_reference_id, order_number, order_status}`: the order's
//! status, not an event. Their SDK reads them through two different services
//! (`processWebhook`, `processAuthoriseNotification`); this reads both through
//! one door, because the answer to either is the same — go and ask about
//! `order_id`.
//!
//! Neither carries a delivery id, so one is made: the order and what was said
//! about it, plus the capture, refund or cancellation id when there is one. A
//! resend is then a duplicate and a second capture is not.

use erp_types::{CurrencyCode, Money};
use serde::Deserialize;

use crate::decimal::{from_decimal, to_decimal};
use crate::{
    Basket, Callback, CallbackError, Charge, Charged, Gateway, GatewayError, Source, Status,
    clipped, header, secrets_match,
};

const LIVE: &str = "https://api.tamara.co";

/// The sandbox, which is a different host rather than a different key.
pub const SANDBOX: &str = "https://api-sandbox.tamara.co";

/// What the notification token's `iss` must say.
const ISSUER: &str = "Tamara";

pub struct Tamara {
    token: String,
    base: String,
    client: reqwest::Client,
}

impl std::fmt::Debug for Tamara {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tamara")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Tamara {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

    pub fn new(token: &str) -> Result<Self, GatewayError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(GatewayError::Unauthenticated);
        }
        let client = reqwest::Client::builder()
            .timeout(Self::TIMEOUT)
            .build()
            .map_err(|e| GatewayError::Refused(format!("the client cannot be built: {e}")))?;

        Ok(Self {
            token: token.to_owned(),
            base: LIVE.to_owned(),
            client,
        })
    }

    /// Points this at the sandbox, or at a test's server.
    #[must_use]
    pub fn at(mut self, base: &str) -> Self {
        base.trim_end_matches('/').clone_into(&mut self.base);
        self
    }

    /// Sends, and reads an order back. `about` is the order the request named,
    /// so a `404` can be the absence it is — see [`crate::refusal`].
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        about: Option<&str>,
    ) -> Result<Order, GatewayError> {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| GatewayError::Unreachable(e.to_string()))?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            return serde_json::from_str::<Order>(&body)
                .map_err(|e| GatewayError::Unreadable(format!("{e}: {}", clipped(&body))));
        }
        Err(refusal(status, &body, about))
    }
}

#[async_trait::async_trait]
impl Gateway for Tamara {
    fn provider(&self) -> &'static str {
        "tamara"
    }

    async fn charge(&self, charge: &Charge) -> Result<Charged, GatewayError> {
        if !matches!(charge.source, Source::Hosted) {
            return Err(GatewayError::Refused(
                "Tamara hosts its own checkout; there is no card token to send it".to_owned(),
            ));
        }
        let buyer = charge.buyer.as_ref().ok_or_else(|| {
            GatewayError::Refused(
                "Tamara scores the buyer before it will lend, so it needs their name, \
                 email and mobile number"
                    .to_owned(),
            )
        })?;
        let basket = charge.basket.as_ref().ok_or_else(|| {
            GatewayError::Refused(
                "Tamara needs the order and its lines, because it is buying the receivable"
                    .to_owned(),
            )
        })?;

        let (first, last) = split_name(&buyer.name);
        // **The notification URL is where Tamara talks to this system**, and
        // it is sent only when the caller named one. It used to be the
        // customer's success page, which is where every callback then went.
        let notification = charge
            .returns
            .notification
            .as_ref()
            .map_or_else(String::new, |url| {
                format!(r#","notification":{}"#, quoted(url))
            });
        let body = format!(
            r#"{{"order_reference_id":{reference},"total_amount":{total},
                 "description":{description},"country_code":{country},
                 "payment_type":"PAY_BY_INSTALMENTS","locale":"ar_SA",
                 "items":[{items}],
                 "consumer":{{"first_name":{first},"last_name":{last},
                              "phone_number":{phone},"email":{email}}},
                 "shipping_address":{{"first_name":{first},"last_name":{last},
                                      "line1":{line},"city":{city},
                                      "country_code":{country}}},
                 "tax_amount":{tax},"shipping_amount":{zero},
                 "merchant_url":{{"success":{success},"failure":{failure},
                                  "cancel":{cancel}{notification}}}}}"#,
            reference = quoted(&basket.reference),
            total = amount_json(charge.amount),
            description = quoted(&charge.description),
            country = quoted(&basket.deliver_to.country),
            tax = amount_json(basket.tax),
            items = items(basket)?,
            first = quoted(first),
            last = quoted(last),
            phone = quoted(&buyer.phone),
            email = quoted(&buyer.email),
            line = quoted(&basket.deliver_to.line),
            city = quoted(&basket.deliver_to.city),
            zero = amount_json(Money::from_minor(0, charge.amount.currency())),
            success = quoted(&charge.returns.success),
            failure = quoted(&charge.returns.failure),
            cancel = quoted(&charge.returns.cancel),
        );

        let response = self
            .client
            .post(format!("{}/checkout", self.base))
            // **Built as text, not by `serde_json`.** An unquoted decimal
            // cannot be produced without an `f64`. See the module docs.
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| GatewayError::Unreachable(e.to_string()))?;

        let status = response.status();
        let said = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(refusal(status, &said, None));
        }

        let created: Created = serde_json::from_str(&said)
            .map_err(|e| GatewayError::Unreadable(format!("{e}: {}", clipped(&said))))?;

        Ok(Charged {
            id: created.order_id,
            status: Status::Initiated,
            amount: charge.amount,
            refunded: Money::from_minor(0, charge.amount.currency()),
            fee: None,
            challenge: created.checkout_url,
            message: None,
        })
    }

    async fn fetch(&self, id: &str) -> Result<Charged, GatewayError> {
        // **Only a `404` is an absence.** This used to read every refusal as
        // "no such order", which is the answer a sweep charges again on.
        self.send(
            self.client.get(format!("{}/orders/{id}", self.base)),
            Some(id),
        )
        .await?
        .into_charged()
    }

    /// Authorise when it has to, **and then** capture, because Tamara needs
    /// both and skipping the first loses the order.
    ///
    /// An order the customer has finished paying the first instalment on is
    /// `approved`, not `authorised`, and only the merchant can move it. Capture
    /// is what settles; authorise is what makes capture possible — and asking
    /// Tamara to authorise an order that already is answers `409`, so the order
    /// is asked where it stands first. One more round trip, on a call made
    /// once per payment.
    ///
    /// **No idempotency key.** Tamara's capture takes an order id, an amount and
    /// shipping details and nothing of the merchant's own; `_reference` has
    /// nowhere to go. What stands in for it is the `auto_captured` check and the
    /// fetch-before-capture the caller does.
    async fn capture(
        &self,
        id: &str,
        _reference: &str,
        amount: Option<Money>,
    ) -> Result<Charged, GatewayError> {
        let order = self
            .send(
                self.client.get(format!("{}/orders/{id}", self.base)),
                Some(id),
            )
            .await?;
        if order.status == "approved" {
            let authorised = self
                .send(
                    self.client
                        .post(format!("{}/orders/{id}/authorise", self.base)),
                    Some(id),
                )
                .await?;
            // Some accounts capture on authorise. Asking again would be a
            // second capture, which is the one mistake worth a round trip to
            // avoid.
            if authorised.auto_captured.unwrap_or(false) {
                return authorised.into_charged();
            }
        }

        let total = amount.ok_or_else(|| {
            GatewayError::Refused(
                "Tamara requires the amount on a capture; there is no 'all of it' form".to_owned(),
            )
        })?;
        let body = format!(
            r#"{{"order_id":{id},"total_amount":{total},
                 "shipping_info":{{"shipped_at":{now},"shipping_company":"-"}}}}"#,
            id = quoted(id),
            total = amount_json(total),
            now = quoted(&chrono::Utc::now().to_rfc3339()),
        );

        let captured = self
            .client
            .post(format!("{}/payments/capture", self.base))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| GatewayError::Unreachable(e.to_string()))?;

        let status = captured.status();
        let said = captured.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(refusal(status, &said, Some(id)));
        }
        serde_json::from_str::<Order>(&said)
            .map_err(|e| GatewayError::Unreadable(format!("{e}: {}", clipped(&said))))?
            .into_charged()
    }

    /// **The reference travels as the refund's `comment`**, which is the only
    /// field of the merchant's own Tamara keeps on a refund. Tamara documents
    /// no idempotency key, so this does not make a retry safe — it makes two
    /// refunds of the same amount tell apart on their statement, and a support
    /// conversation possible.
    async fn refund(
        &self,
        id: &str,
        reference: &str,
        amount: Option<Money>,
    ) -> Result<Charged, GatewayError> {
        let total = amount.ok_or_else(|| {
            GatewayError::Refused(
                "Tamara requires the amount on a refund; there is no 'all of it' form".to_owned(),
            )
        })?;
        // The simplified endpoint. The older one wants the refund broken down
        // per capture and is marked deprecated.
        let body = format!(
            r#"{{"total_amount":{total},"comment":{comment}}}"#,
            total = amount_json(total),
            comment = quoted(reference),
        );
        let response = self
            .client
            .post(format!("{}/payments/simplified-refund/{id}", self.base))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| GatewayError::Unreachable(e.to_string()))?;

        let status = response.status();
        let said = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(refusal(status, &said, Some(id)));
        }
        serde_json::from_str::<Order>(&said)
            .map_err(|e| GatewayError::Unreadable(format!("{e}: {}", clipped(&said))))?
            .into_charged()
    }

    /// Cancel, which Tamara allows **only from `authorised`**.
    ///
    /// An `approved` order has to be authorised first; cancelling it directly
    /// is a `409` naming the transition, which is a better error than anything
    /// this client could invent.
    async fn void(&self, id: &str) -> Result<Charged, GatewayError> {
        let order = self
            .send(
                self.client
                    .post(format!("{}/orders/{id}/cancel", self.base)),
                Some(id),
            )
            .await?;
        order.into_charged()
    }
}

/// **Whether a callback really came from Tamara.**
///
/// The notification token is a JWT, HS256, signed with the Notification Token —
/// a different credential from the API token. See the module docs for why it
/// authenticates a sender and not a payload.
pub(crate) fn authenticate(
    secret: &[u8],
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<Callback, CallbackError> {
    // The header rather than the query parameter: the query string is where a
    // credential ends up in an access log.
    let token = header(headers, "authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| header(headers, "tamaratoken"))
        .ok_or(CallbackError::NotAuthentic)?;

    verify_jwt(token, secret, chrono::Utc::now().timestamp())?;

    callback(body)
}

/// Reads either of the two shapes Tamara sends. See the module docs.
fn callback(body: &[u8]) -> Result<Callback, CallbackError> {
    let text = |value: Option<&serde_json::Value>| {
        value
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let body: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| CallbackError::Unreadable(format!("not JSON: {e}")))?;

    let order = text(body.get("order_id"))
        .ok_or_else(|| CallbackError::Unreadable("no order id in the callback".to_owned()))?;

    // A webhook says what happened; a notification says where the order is.
    let said = text(body.get("event_type"))
        .or_else(|| text(body.get("order_status")).map(|status| format!("status.{status}")))
        .ok_or_else(|| {
            CallbackError::Unreadable(
                "neither an event_type (a webhook) nor an order_status (a notification)".to_owned(),
            )
        })?;

    // A capture, a refund or a cancellation has an id of its own, and two
    // captures on one order are two events.
    let movement = body.get("data").and_then(|data| {
        ["capture_id", "refund_id", "cancel_id"]
            .into_iter()
            .find_map(|field| text(data.get(field)))
    });

    let event = match movement {
        Some(id) => format!("{order}.{said}.{id}"),
        None => format!("{order}.{said}"),
    };
    Ok(Callback {
        payment: order,
        event,
        kind: Some(said),
    })
}

/// HS256, and only HS256.
///
/// **The algorithm is pinned rather than read from the token.** A verifier that
/// trusts the header's `alg` accepts `none`, and accepts an RS256 token whose
/// "signature" was made with the public key as an HMAC secret. Tamara's own SDK
/// reads the algorithm from the token; this does not.
fn verify_jwt(token: &str, secret: &[u8], now: i64) -> Result<(), CallbackError> {
    use base64::Engine as _;
    const B64: base64::engine::general_purpose::GeneralPurpose =
        base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let mut parts = token.split('.');
    let (Some(header), Some(claims), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(CallbackError::NotAuthentic);
    };

    let decode = |part: &str| B64.decode(part).map_err(|_| CallbackError::NotAuthentic);
    let json = |bytes: &[u8]| {
        serde_json::from_slice::<serde_json::Value>(bytes).map_err(|_| CallbackError::NotAuthentic)
    };

    if json(&decode(header)?)?
        .get("alg")
        .and_then(serde_json::Value::as_str)
        != Some("HS256")
    {
        return Err(CallbackError::NotAuthentic);
    }

    let expected = hmac_sha256(secret, format!("{header}.{claims}").as_bytes())
        .map_err(|_| CallbackError::NotAuthentic)?;
    if !secrets_match(&decode(signature)?, &expected) {
        return Err(CallbackError::NotAuthentic);
    }

    // Signature good. Now the claims — `exp` because a fifteen-minute token
    // replayed a day later is not a live one, and `iss` because their own SDK
    // does not check it and a token from somewhere else should not pass.
    let claims = json(&decode(claims)?)?;
    if claims.get("iss").and_then(serde_json::Value::as_str) != Some(ISSUER) {
        return Err(CallbackError::NotAuthentic);
    }
    match claims.get("exp").and_then(serde_json::Value::as_i64) {
        Some(expiry) if expiry > now => Ok(()),
        _ => Err(CallbackError::NotAuthentic),
    }
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Result<Vec<u8>, openssl::error::ErrorStack> {
    let key = openssl::pkey::PKey::hmac(key)?;
    let mut signer = openssl::sign::Signer::new(openssl::hash::MessageDigest::sha256(), &key)?;
    signer.update(message)?;
    signer.sign_to_vec()
}

/// What `POST /checkout` answers with.
#[derive(Deserialize)]
struct Created {
    order_id: String,
    #[serde(default)]
    checkout_url: Option<String>,
}

/// Tamara's order, as much of it as this system reads.
#[derive(Debug, Deserialize)]
struct Order {
    #[serde(alias = "order_id")]
    id: String,
    status: String,
    #[serde(default)]
    auto_captured: Option<bool>,
    #[serde(default)]
    total_amount: Option<RawMoney>,
    #[serde(default)]
    captured_amount: Option<RawMoney>,
    #[serde(default)]
    refunded_amount: Option<RawMoney>,
}

/// An amount, kept as the **text** Tamara sent.
///
/// `serde_json::Number` would turn `300.50` into an `f64`, and multiplying that
/// by a hundred is where a halala goes missing. `to_string` on the raw value is
/// the digits as they arrived.
#[derive(Debug, Deserialize)]
struct RawMoney {
    amount: serde_json::Value,
    currency: String,
}

impl RawMoney {
    fn read(&self) -> Result<Money, GatewayError> {
        let currency: CurrencyCode = self.currency.parse().map_err(|_| {
            GatewayError::Unreadable(format!("{} is not a currency code", self.currency))
        })?;
        // A number arrives as a number and a string as a string; Tamara's own
        // spec disagrees with itself about which, and both are read the same
        // way — as digits.
        let text = match &self.amount {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        from_decimal(&text, currency).map_err(|e| GatewayError::Unreadable(e.to_string()))
    }
}

impl Order {
    fn into_charged(self) -> Result<Charged, GatewayError> {
        let total = self
            .total_amount
            .as_ref()
            .or(self.captured_amount.as_ref())
            .ok_or_else(|| GatewayError::Unreadable("the order carries no amount".to_owned()))?
            .read()?;
        let zero = Money::from_minor(0, total.currency());
        let refunded = match &self.refunded_amount {
            Some(amount) => amount.read()?,
            None => zero,
        };
        // **Paid means the captured sum, not the order.** A capture of 60 on
        // an order of 100 is `partially_captured`, and reporting 100 posts a
        // receipt for money that never moved. Before anything is captured the
        // order's own amount is the honest figure.
        let captured = match &self.captured_amount {
            Some(amount) => amount.read()?,
            None => zero,
        };

        let status = match self.status.as_str() {
            // The customer has not finished yet.
            "new" => Status::Initiated,
            // **`approved` is the merchant's turn.** The customer has paid
            // their first instalment; authorising and capturing are the
            // merchant's to do, or the order expires. See the module docs.
            // `updated` is a *partial* cancellation, which leaves the rest of
            // the order live and still to be captured — so it is the same
            // state as an authorization, not an ending.
            "approved" | "authorised" | "authorized" | "updated" => Status::Authorized,
            // Only a capture is settled money.
            "partially_captured" | "fully_captured" => Status::Paid,
            "partially_refunded" | "fully_refunded" => Status::Refunded,
            "declined" => Status::Failed,
            // `updated` is a partial cancellation, which leaves the rest live.
            "canceled" | "cancelled" | "expired" => Status::Voided,
            other => {
                return Err(GatewayError::Unreadable(format!(
                    "{other} is not a Tamara order status this system knows"
                )));
            }
        };

        let amount = if matches!(status, Status::Paid | Status::Refunded) && !captured.is_zero() {
            captured
        } else {
            total
        };

        Ok(Charged {
            id: self.id,
            status,
            amount,
            refunded,
            // Tamara reports its cut on the settlement, not on the order.
            fee: None,
            challenge: None,
            message: None,
        })
    }
}

/// A JSON string literal, escaped.
fn quoted(value: &str) -> String {
    serde_json::Value::String(value.to_owned()).to_string()
}

/// A Tamara money object, with the amount as a **raw decimal token**.
fn amount_json(money: Money) -> String {
    format!(
        r#"{{"amount":{},"currency":{}}}"#,
        to_decimal(money),
        quoted(&money.currency().to_string())
    )
}

/// The basket's lines, each with its own id and a total that is the unit price
/// times the quantity — Tamara checks that the lines add up to the order and
/// refuses a basket where they do not.
fn items(basket: &Basket) -> Result<String, GatewayError> {
    basket
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let total = item
                .total()
                .map_err(|e| GatewayError::Refused(format!("line {}: {e}", index + 1)))?;
            Ok(format!(
                r#"{{"name":{name},"type":"Physical","reference_id":{reference},
                     "sku":{reference},"quantity":{quantity},"unit_price":{price},
                     "total_amount":{total}}}"#,
                name = quoted(&item.title),
                // **Each line its own id.** Tamara refunds and cancels by
                // line, and lines that all carry the order's reference are
                // one line to it.
                reference = quoted(&format!("{}-{}", basket.reference, index + 1)),
                quantity = item.quantity,
                price = amount_json(item.unit_price),
                total = amount_json(total),
            ))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|lines| lines.join(","))
}

/// Tamara wants two names and this system holds one.
///
/// Splitting on the last space is what every integration does; a single-word
/// name gets a placeholder rather than an empty field the API refuses.
fn split_name(name: &str) -> (&str, &str) {
    match name.trim().rsplit_once(' ') {
        Some((first, last)) => (first, last),
        None if name.trim().is_empty() => ("-", "-"),
        None => (name.trim(), "-"),
    }
}

/// Tamara's refusal, in its own words where it gave any. What the status
/// means is decided once for every provider — see [`crate::refusal`].
fn refusal(status: reqwest::StatusCode, body: &str, about: Option<&str>) -> GatewayError {
    let said = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| clipped(body));
    crate::refusal(status, said, about)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::OneRequest;
    use crate::{Address, Buyer, Item, Returns};
    use base64::Engine as _;

    const B64: base64::engine::general_purpose::GeneralPurpose =
        base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn sar(minor: i64) -> Money {
        Money::from_minor(minor, "SAR".parse().expect("a currency"))
    }

    fn charge() -> Charge {
        Charge {
            reference: "INV-1".to_owned(),
            amount: sar(30_050),
            returns: Returns {
                success: "https://bassat.erp.com/paid".to_owned(),
                cancel: "https://bassat.erp.com/cancelled".to_owned(),
                failure: "https://bassat.erp.com/declined".to_owned(),
                notification: Some("https://bassat.erp.com/v1/hooks/tamara".to_owned()),
            },
            source: Source::Hosted,
            description: "Invoice INV-1".to_owned(),
            buyer: Some(Buyer {
                name: "Sara Al-Otaibi".to_owned(),
                email: "sara@example.com".to_owned(),
                phone: "+966500000001".to_owned(),
                registered_since: "2024-01-01T00:00:00Z".parse().expect("an instant"),
                purchases: 3,
            }),
            basket: Some(Basket {
                reference: "INV-1".to_owned(),
                deliver_to: Address {
                    line: "King Fahd Road 12".to_owned(),
                    city: "Riyadh".to_owned(),
                    postcode: "12211".to_owned(),
                    country: "SA".to_owned(),
                },
                tax: sar(3_920),
                items: vec![Item {
                    title: "Deep tissue massage".to_owned(),
                    category: "Services".to_owned(),
                    quantity: 1,
                    unit_price: sar(30_050),
                }],
            }),
        }
    }

    /// A token the way Tamara makes them.
    fn token(secret: &[u8], claims: &str) -> String {
        let header = B64.encode(br#"{"typ":"JWT","alg":"HS256"}"#);
        let claims = B64.encode(claims.as_bytes());
        let signature =
            hmac_sha256(secret, format!("{header}.{claims}").as_bytes()).expect("signs");
        format!("{header}.{claims}.{}", B64.encode(signature))
    }

    fn live_claims() -> String {
        format!(
            r#"{{"iss":"Tamara","iat":1700000000,"exp":{}}}"#,
            chrono::Utc::now().timestamp() + 600
        )
    }

    /// **The amount goes out unquoted**, which is the whole reason this client
    /// builds its body as text.
    #[tokio::test]
    async fn the_amount_goes_out_as_an_unquoted_decimal_in_major_units() {
        let server = OneRequest::answering(
            200,
            r#"{"order_id":"ord_1","checkout_id":"c1","status":"new",
                "checkout_url":"https://checkout.tamara.co/ord_1"}"#,
        )
        .await;

        let charged = Tamara::new("api-token")
            .expect("built")
            .at(&server.url())
            .charge(&charge())
            .await
            .expect("creates a checkout");
        assert_eq!(charged.id, "ord_1");
        assert_eq!(charged.status, Status::Initiated);
        assert_eq!(
            charged.challenge.as_deref(),
            Some("https://checkout.tamara.co/ord_1")
        );

        let sent = server.seen().await;
        assert!(sent.starts_with("POST /checkout "), "{sent}");
        assert!(sent.contains("authorization: Bearer api-token"), "{sent}");
        // A number, not a string, and in riyals rather than halalas.
        assert!(sent.contains(r#""amount":300.50"#), "{sent}");
        assert!(!sent.contains(r#""amount":"300.50""#), "{sent}");
        assert!(!sent.contains(r#""amount":30050"#), "{sent}");
        assert!(sent.contains(r#""currency":"SAR""#), "{sent}");
        // **Tamara reports to the hook, not to the customer's thank-you
        // page.** The notification URL is the one the caller named.
        assert!(
            sent.contains(r#""notification":"https://bassat.erp.com/v1/hooks/tamara""#),
            "{sent}"
        );
        assert!(
            !sent.contains(r#""notification":"https://bassat.erp.com/paid""#),
            "{sent}"
        );
        // And the address is the caller's, not a placeholder; the tax is the
        // caller's figure, not zero.
        assert!(sent.contains(r#""line1":"King Fahd Road 12""#), "{sent}");
        assert!(
            sent.contains(r#""tax_amount":{"amount":39.20,"currency":"SAR"}"#),
            "{sent}"
        );
        assert!(sent.contains(r#""city":"Riyadh""#), "{sent}");
        assert!(!sent.contains(r#""line1":"-""#), "{sent}");
        // **And it is still valid JSON, built by hand or not.** That is the
        // risk this file takes on by writing its own body, so it is asserted
        // rather than assumed.
        let body = sent
            .split("\r\n\r\n")
            .nth(1)
            .expect("a body")
            .trim_end_matches("\n===\n");
        serde_json::from_str::<serde_json::Value>(body).expect("valid JSON");
    }

    /// A checkout with no notification URL sends none, rather than a page.
    #[tokio::test]
    async fn no_notification_url_means_none_is_sent() {
        let server = OneRequest::answering(
            200,
            r#"{"order_id":"ord_1","checkout_id":"c1","status":"new"}"#,
        )
        .await;
        let mut charge = charge();
        charge.returns.notification = None;
        Tamara::new("api-token")
            .expect("built")
            .at(&server.url())
            .charge(&charge)
            .await
            .expect("creates a checkout");
        let sent = server.seen().await;
        assert!(!sent.contains(r#""notification""#), "{sent}");
        let body = sent
            .split("\r\n\r\n")
            .nth(1)
            .expect("a body")
            .trim_end_matches("\n===\n");
        serde_json::from_str::<serde_json::Value>(body).expect("valid JSON");
    }

    /// **The lines add up.** A line of three at 50 is sent as 150, and each
    /// line carries its own id — Tamara validates the sum and refunds by line.
    #[test]
    fn a_basket_line_totals_its_quantity_and_has_its_own_id() {
        let basket = Basket {
            reference: "INV-7".to_owned(),
            deliver_to: Address {
                line: "-".to_owned(),
                city: "Riyadh".to_owned(),
                postcode: "12211".to_owned(),
                country: "SA".to_owned(),
            },
            tax: sar(4_565),
            items: vec![
                Item {
                    title: "Wax strips".to_owned(),
                    category: "Beauty".to_owned(),
                    quantity: 3,
                    unit_price: sar(5_000),
                },
                Item {
                    title: "Massage".to_owned(),
                    category: "Services".to_owned(),
                    quantity: 1,
                    unit_price: sar(20_000),
                },
            ],
        };
        let lines = items(&basket).expect("lines");
        assert!(lines.contains(r#""quantity":3"#), "{lines}");
        assert!(
            lines.contains(r#""unit_price":{"amount":50.00,"currency":"SAR"}"#),
            "{lines}"
        );
        assert!(
            lines.contains(r#""total_amount":{"amount":150.00,"currency":"SAR"}"#),
            "{lines}"
        );
        assert!(lines.contains(r#""reference_id":"INV-7-1""#), "{lines}");
        assert!(lines.contains(r#""reference_id":"INV-7-2""#), "{lines}");
        assert!(lines.contains(r#""sku":"INV-7-2""#), "{lines}");
        assert!(!lines.contains(r#""reference_id":"INV-7""#), "{lines}");
    }

    /// **Capture asks the order where it stands.** An `approved` order is
    /// authorised and then captured; an `authorised` one is captured straight
    /// away, because authorising it again is a `409` Tamara answers with.
    #[tokio::test]
    async fn a_capture_authorises_first_only_when_the_order_is_approved() {
        let order = |status: &str| -> &'static str {
            match status {
                "approved" => {
                    r#"{"order_id":"ord_1","status":"approved",
                    "total_amount":{"amount":100.00,"currency":"SAR"}}"#
                }
                "authorised" => {
                    r#"{"order_id":"ord_1","status":"authorised",
                    "total_amount":{"amount":100.00,"currency":"SAR"}}"#
                }
                _ => {
                    r#"{"order_id":"ord_1","status":"fully_captured",
                    "total_amount":{"amount":100.00,"currency":"SAR"},
                    "captured_amount":{"amount":100.00,"currency":"SAR"}}"#
                }
            }
        };

        let server = OneRequest::sequence(vec![
            (200, order("approved")),
            (200, order("authorised")),
            (200, order("captured")),
        ])
        .await;
        let captured = Tamara::new("api-token")
            .expect("built")
            .at(&server.url())
            .capture("ord_1", "pay_1.capture", Some(sar(10_000)))
            .await
            .expect("captures");
        assert_eq!(captured.status, Status::Paid);
        assert_eq!(captured.amount, sar(10_000));
        let sent = server.seen().await;
        let requests: Vec<&str> = sent.split("\n===\n").collect();
        assert!(
            requests[0].starts_with("GET /orders/ord_1 "),
            "{}",
            requests[0]
        );
        assert!(
            requests[1].starts_with("POST /orders/ord_1/authorise "),
            "{}",
            requests[1]
        );
        assert!(
            requests[2].starts_with("POST /payments/capture "),
            "{}",
            requests[2]
        );
        assert!(
            requests[2].contains(r#""amount":100.00"#),
            "{}",
            requests[2]
        );

        // Already authorised: no second authorise.
        let server =
            OneRequest::sequence(vec![(200, order("authorised")), (200, order("captured"))]).await;
        Tamara::new("api-token")
            .expect("built")
            .at(&server.url())
            .capture("ord_1", "pay_1.capture", Some(sar(10_000)))
            .await
            .expect("captures");
        let sent = server.seen().await;
        let requests: Vec<&str> = sent.split("\n===\n").collect();
        assert_eq!(requests.len(), 3, "two requests and a trailing separator");
        assert!(
            requests[0].starts_with("GET /orders/ord_1 "),
            "{}",
            requests[0]
        );
        assert!(
            requests[1].starts_with("POST /payments/capture "),
            "{}",
            requests[1]
        );
    }

    /// A name with a quote in it must not break out of the string it is in.
    #[test]
    fn a_hand_built_body_still_escapes_what_goes_into_it() {
        assert_eq!(quoted(r#"O"Brien"#), r#""O\"Brien""#);
        assert_eq!(quoted("سارة"), "\"سارة\"");
        assert_eq!(
            amount_json(sar(30_050)),
            r#"{"amount":300.50,"currency":"SAR"}"#
        );
        assert_eq!(amount_json(sar(0)), r#"{"amount":0.00,"currency":"SAR"}"#);
    }

    #[test]
    fn a_name_is_split_into_the_two_tamara_asks_for() {
        assert_eq!(split_name("Sara Al-Otaibi"), ("Sara", "Al-Otaibi"));
        assert_eq!(
            split_name("Sara bint Ahmed Al-Otaibi"),
            ("Sara bint Ahmed", "Al-Otaibi")
        );
        assert_eq!(split_name("Sara"), ("Sara", "-"));
        assert_eq!(split_name("   "), ("-", "-"));
    }

    /// **`approved` is not an ending**, and only a capture is money.
    #[test]
    fn every_status_tamara_documents_is_read_or_refused() {
        let read = |status: &str| {
            serde_json::from_str::<Order>(&format!(
                r#"{{"order_id":"ord_1","status":"{status}",
                     "total_amount":{{"amount":300.50,"currency":"SAR"}}}}"#
            ))
            .expect("parses")
            .into_charged()
        };

        assert_eq!(read("new").expect("read").status, Status::Initiated);
        // **The one that loses a sale if it is read as waiting on the
        // customer.** They have paid; the merchant has to capture.
        assert_eq!(read("approved").expect("read").status, Status::Authorized);
        assert_eq!(read("authorised").expect("read").status, Status::Authorized);
        assert_eq!(read("fully_captured").expect("read").status, Status::Paid);
        assert_eq!(
            read("partially_captured").expect("read").status,
            Status::Paid
        );
        assert_eq!(
            read("fully_refunded").expect("read").status,
            Status::Refunded
        );
        assert_eq!(read("declined").expect("read").status, Status::Failed);
        assert_eq!(read("canceled").expect("read").status, Status::Voided);
        assert_eq!(read("expired").expect("read").status, Status::Voided);
        // A partial cancellation leaves the rest live.
        assert_eq!(read("updated").expect("read").status, Status::Authorized);

        assert!(matches!(
            read("something_new"),
            Err(GatewayError::Unreadable(_))
        ));
    }

    /// **Paid is the captured sum.** A capture of 60 on an order of 100 is a
    /// receipt for 60, and the order's total is what is reported only while
    /// nothing has been captured.
    #[test]
    fn a_partial_capture_reports_what_was_captured_not_the_order() {
        let read = |status: &str, captured: &str| {
            serde_json::from_str::<Order>(&format!(
                r#"{{"order_id":"ord_1","status":"{status}",
                     "total_amount":{{"amount":100.00,"currency":"SAR"}},
                     "captured_amount":{{"amount":{captured},"currency":"SAR"}}}}"#
            ))
            .expect("parses")
            .into_charged()
            .expect("read")
        };
        let partial = read("partially_captured", "60.00");
        assert_eq!(partial.status, Status::Paid);
        assert_eq!(partial.amount, sar(6_000));

        let full = read("fully_captured", "100.00");
        assert_eq!(full.amount, sar(10_000));

        // Authorised and nothing captured: the order is the figure.
        let held = read("authorised", "0.00");
        assert_eq!(held.status, Status::Authorized);
        assert_eq!(held.amount, sar(10_000));
    }

    /// **Only a `404` is an absence.** Tamara's `409` for a transition it will
    /// not make, or a `400` for an id it cannot parse, is a refusal of this
    /// request — reading it as "no such order" is what a sweep charges again on.
    #[tokio::test]
    async fn a_fetch_is_no_such_payment_only_on_a_404() {
        let missing = OneRequest::answering(404, r#"{"message":"Order not found"}"#).await;
        let tamara = Tamara::new("api-token").expect("built");
        assert!(matches!(
            tamara.at(&missing.url()).fetch("ord_x").await,
            Err(GatewayError::NoSuchPayment(id)) if id == "ord_x"
        ));

        let refused = OneRequest::answering(409, r#"{"message":"Invalid transition"}"#).await;
        let tamara = Tamara::new("api-token").expect("built");
        assert!(matches!(
            tamara.at(&refused.url()).fetch("ord_x").await,
            Err(GatewayError::Refused(why)) if why == "Invalid transition"
        ));
    }

    /// The reference rides as the refund's comment, so two equal refunds are
    /// two lines on Tamara's statement rather than one.
    #[tokio::test]
    async fn a_refund_carries_the_reference_as_its_comment() {
        let server = OneRequest::answering(
            200,
            r#"{"order_id":"ord_1","status":"partially_refunded",
                "total_amount":{"amount":300.50,"currency":"SAR"},
                "captured_amount":{"amount":300.50,"currency":"SAR"},
                "refunded_amount":{"amount":40.00,"currency":"SAR"}}"#,
        )
        .await;
        Tamara::new("api-token")
            .expect("built")
            .at(&server.url())
            .refund("ord_1", "pay_1.refund-2", Some(sar(4_000)))
            .await
            .expect("refunds");
        let sent = server.seen().await;
        assert!(
            sent.starts_with("POST /payments/simplified-refund/ord_1 "),
            "{sent}"
        );
        assert!(sent.contains(r#""comment":"pay_1.refund-2""#), "{sent}");
        assert!(!sent.contains(r#""comment":"refund""#), "{sent}");
    }

    /// **The read that a float would get wrong.** Tamara's own spec disagrees
    /// with itself about whether an amount is a number or a string.
    #[test]
    fn an_amount_is_read_as_digits_whichever_way_tamara_wrote_it() {
        let read = |amount: &str| {
            serde_json::from_str::<Order>(&format!(
                r#"{{"order_id":"o","status":"fully_captured",
                     "total_amount":{{"amount":{amount},"currency":"SAR"}}}}"#
            ))
            .expect("parses")
            .into_charged()
            .expect("read")
            .amount
        };

        assert_eq!(read("300.50"), sar(30_050));
        assert_eq!(read(r#""300.50""#), sar(30_050));
        assert_eq!(read("300"), sar(30_000));
        assert_eq!(read("300.5"), sar(30_050));
        assert_eq!(read("0.01"), sar(1));
    }

    /// A token this system minted itself, verified the way Tamara's would be.
    #[test]
    fn a_notification_token_is_verified_against_the_notification_secret() {
        let good = token(b"notify-secret", &live_claims());
        assert_eq!(
            authenticate(
                b"notify-secret",
                &[("authorization", &format!("Bearer {good}"))],
                br#"{"order_id":"ord_1","event_type":"order_approved","data":[]}"#,
            )
            .expect("authentic")
            .payment,
            "ord_1"
        );

        // The query-parameter form Tamara also sends, for a caller that has it.
        assert_eq!(
            authenticate(
                b"notify-secret",
                &[("tamaratoken", &good)],
                br#"{"order_id":"ord_1","order_status":"approved"}"#,
            )
            .expect("authentic")
            .payment,
            "ord_1"
        );
    }

    /// **Both shapes Tamara sends are read**, and each delivery gets an id of
    /// its own: a resend is a duplicate, a second capture is not, and a
    /// notification saying `approved` is not the webhook saying the same.
    #[test]
    fn a_webhook_and_a_notification_are_both_read_and_told_apart() {
        let webhook = callback(
            br#"{"order_id":"ord_1","order_reference_id":"INV-1","order_number":"90001860",
                 "event_type":"order_approved","data":[]}"#,
        )
        .expect("a webhook");
        assert_eq!(webhook.payment, "ord_1");
        assert_eq!(webhook.event, "ord_1.order_approved");
        assert_eq!(webhook.kind.as_deref(), Some("order_approved"));

        let notification = callback(
            br#"{"order_id":"ord_1","order_reference_id":"INV-1","order_number":"90001860",
                 "order_status":"approved"}"#,
        )
        .expect("a notification");
        assert_eq!(notification.payment, "ord_1");
        assert_eq!(notification.event, "ord_1.status.approved");
        assert_ne!(notification.event, webhook.event);

        // Two captures on one order are two deliveries.
        let capture = |id: &str| {
            callback(
                format!(
                    r#"{{"order_id":"ord_1","event_type":"order_captured",
                         "data":{{"capture_id":"{id}","captured_amount":{{"amount":60.00,"currency":"SAR"}}}}}}"#
                )
                .as_bytes(),
            )
            .expect("a capture")
            .event
        };
        assert_eq!(capture("cap_1"), "ord_1.order_captured.cap_1");
        assert_ne!(capture("cap_1"), capture("cap_2"));

        // A body that is neither is not guessed at.
        assert!(matches!(
            callback(br#"{"order_id":"ord_1"}"#),
            Err(CallbackError::Unreadable(_))
        ));
        assert!(matches!(
            callback(br#"{"event_type":"order_approved"}"#),
            Err(CallbackError::Unreadable(_))
        ));
    }

    /// **The algorithm is pinned.** A verifier that trusts the token's own
    /// `alg` accepts `none`, which is a forged token that verifies.
    #[test]
    fn a_token_that_names_its_own_algorithm_is_not_believed() {
        let now = chrono::Utc::now().timestamp();
        let claims = B64.encode(live_claims().as_bytes());

        let unsigned = format!("{}.{claims}.", B64.encode(br#"{"alg":"none"}"#));
        assert_eq!(
            verify_jwt(&unsigned, b"notify-secret", now),
            Err(CallbackError::NotAuthentic)
        );

        // And one signed with the right key but claiming another algorithm.
        let header = B64.encode(br#"{"typ":"JWT","alg":"HS512"}"#);
        let signature =
            hmac_sha256(b"notify-secret", format!("{header}.{claims}").as_bytes()).expect("signs");
        assert_eq!(
            verify_jwt(
                &format!("{header}.{claims}.{}", B64.encode(signature)),
                b"notify-secret",
                now
            ),
            Err(CallbackError::NotAuthentic)
        );
    }

    /// The three ways a real-looking token is still not one.
    #[test]
    fn a_token_that_is_wrong_expired_or_from_elsewhere_is_refused() {
        let now = chrono::Utc::now().timestamp();

        // Signed with somebody else's secret.
        assert_eq!(
            verify_jwt(&token(b"other", &live_claims()), b"notify-secret", now),
            Err(CallbackError::NotAuthentic)
        );

        // **Expired.** The token lives fifteen minutes and is also sent in a
        // query string, so a replay an hour later is the case to refuse.
        let stale = format!(r#"{{"iss":"Tamara","iat":1,"exp":{}}}"#, now - 1);
        assert_eq!(
            verify_jwt(&token(b"notify-secret", &stale), b"notify-secret", now),
            Err(CallbackError::NotAuthentic)
        );

        // Signed correctly, issued by somebody else. Their own SDK does not
        // check this.
        let foreign = format!(r#"{{"iss":"Someone","iat":1,"exp":{}}}"#, now + 600);
        assert_eq!(
            verify_jwt(&token(b"notify-secret", &foreign), b"notify-secret", now),
            Err(CallbackError::NotAuthentic)
        );

        // No claims at all.
        assert_eq!(
            verify_jwt(&token(b"notify-secret", "{}"), b"notify-secret", now),
            Err(CallbackError::NotAuthentic)
        );

        // Not a token.
        assert_eq!(
            verify_jwt("nonsense", b"notify-secret", now),
            Err(CallbackError::NotAuthentic)
        );
    }

    #[test]
    fn a_callback_with_no_token_at_all_is_not_believed() {
        assert_eq!(
            authenticate(b"notify-secret", &[], br#"{"order_id":"ord_1"}"#),
            Err(CallbackError::NotAuthentic)
        );
    }
}
