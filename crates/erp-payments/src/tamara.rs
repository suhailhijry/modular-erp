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
//! # `approved` is not `authorised`, and leaving it there loses the sale
//!
//! When the customer comes back, the order is `approved` — they have paid the
//! first instalment and **the merchant still has to act**. Tamara is blunt:
//! *"orders should NOT be left pending at `approved` status, as it would
//! usually indicate a technical/status sync issue and must be addressed
//! immediately"*, and an order not authorised within 72 hours expires.
//!
//! So [`Status::Initiated`] covers `approved` as well as `new`: both mean
//! somebody still has to do something, and calling `approved` an ending is how
//! an order sits until it expires.
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

use erp_types::{CurrencyCode, Money};
use serde::Deserialize;

use crate::decimal::{from_decimal, to_decimal};
use crate::{
    Basket, CallbackError, Charge, Charged, Gateway, GatewayError, Source, Status, header,
    secrets_match,
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

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Order, GatewayError> {
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
        Err(refusal(status, &body))
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
        let body = format!(
            r#"{{"order_reference_id":{reference},"total_amount":{total},
                 "description":{description},"country_code":"SA",
                 "payment_type":"PAY_BY_INSTALMENTS","locale":"ar_SA",
                 "items":[{items}],
                 "consumer":{{"first_name":{first},"last_name":{last},
                              "phone_number":{phone},"email":{email}}},
                 "shipping_address":{{"first_name":{first},"last_name":{last},
                                      "line1":"-","city":"-","country_code":"SA"}},
                 "tax_amount":{zero},"shipping_amount":{zero},
                 "merchant_url":{{"success":{success},"failure":{failure},
                                  "cancel":{cancel},"notification":{success}}}}}"#,
            reference = quoted(&basket.reference),
            total = amount_json(charge.amount),
            description = quoted(&charge.description),
            items = items(basket),
            first = quoted(first),
            last = quoted(last),
            phone = quoted(&buyer.phone),
            email = quoted(&buyer.email),
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
            return Err(refusal(status, &said));
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
        match self
            .send(self.client.get(format!("{}/orders/{id}", self.base)))
            .await
        {
            Ok(order) => order.into_charged(),
            Err(GatewayError::Refused(_)) => Err(GatewayError::NoSuchPayment(id.to_owned())),
            Err(e) => Err(e),
        }
    }

    /// Authorise **and then** capture, because Tamara needs both and skipping
    /// the first loses the order.
    ///
    /// An order the customer has finished paying the first instalment on is
    /// `approved`, not `authorised`, and only the merchant can move it. Capture
    /// is what settles; authorise is what makes capture possible.
    async fn capture(&self, id: &str, amount: Option<Money>) -> Result<Charged, GatewayError> {
        let order = self
            .send(
                self.client
                    .post(format!("{}/orders/{id}/authorise", self.base)),
            )
            .await?;

        // Some accounts capture on authorise. Asking again would be a second
        // capture, which is the one mistake worth a round trip to avoid.
        if order.auto_captured.unwrap_or(false) {
            return order.into_charged();
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
            return Err(refusal(status, &said));
        }
        serde_json::from_str::<Order>(&said)
            .map_err(|e| GatewayError::Unreadable(format!("{e}: {}", clipped(&said))))?
            .into_charged()
    }

    async fn refund(&self, id: &str, amount: Option<Money>) -> Result<Charged, GatewayError> {
        let total = amount.ok_or_else(|| {
            GatewayError::Refused(
                "Tamara requires the amount on a refund; there is no 'all of it' form".to_owned(),
            )
        })?;
        // The simplified endpoint. The older one wants the refund broken down
        // per capture and is marked deprecated.
        let body = format!(
            r#"{{"total_amount":{total},"comment":"refund"}}"#,
            total = amount_json(total)
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
            return Err(refusal(status, &said));
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
) -> Result<String, CallbackError> {
    // The header rather than the query parameter: the query string is where a
    // credential ends up in an access log.
    let token = header(headers, "authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| header(headers, "tamaratoken"))
        .ok_or(CallbackError::NotAuthentic)?;

    verify_jwt(token, secret, chrono::Utc::now().timestamp())?;

    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .as_ref()
        .and_then(|v| v.get("order_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| CallbackError::Unreadable("no order id in the callback".to_owned()))
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

        let status = match self.status.as_str() {
            // **`approved` is not an ending.** The customer has paid their
            // first instalment and the merchant still has to authorise, or the
            // order expires. See the module docs.
            "new" | "approved" => Status::Initiated,
            // `updated` is a *partial* cancellation, which leaves the rest of
            // the order live and still to be captured — so it is the same
            // state as an authorization, not an ending.
            "authorised" | "authorized" | "updated" => Status::Authorized,
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

        Ok(Charged {
            id: self.id,
            status,
            amount: total,
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

fn items(basket: &Basket) -> String {
    basket
        .items
        .iter()
        .map(|item| {
            format!(
                r#"{{"name":{name},"type":"Physical","reference_id":{reference},
                     "sku":{reference},"quantity":{quantity},"unit_price":{price},
                     "total_amount":{price}}}"#,
                name = quoted(&item.title),
                reference = quoted(&basket.reference),
                quantity = item.quantity,
                price = amount_json(item.unit_price),
            )
        })
        .collect::<Vec<_>>()
        .join(",")
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

fn refusal(status: reqwest::StatusCode, body: &str) -> GatewayError {
    let said = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| clipped(body));

    match status {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            GatewayError::Unauthenticated
        }
        reqwest::StatusCode::TOO_MANY_REQUESTS => GatewayError::Unreachable(said),
        _ if status.is_server_error() => GatewayError::Unreachable(format!("{status}: {said}")),
        _ => GatewayError::Refused(said),
    }
}

fn clipped(body: &str) -> String {
    body.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::OneRequest;
    use crate::{Buyer, Item, Returns};
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
            },
            source: Source::Hosted,
            description: "Invoice INV-1".to_owned(),
            buyer: Some(Buyer {
                name: "Sara Al-Otaibi".to_owned(),
                email: "sara@example.com".to_owned(),
                phone: "+966500000001".to_owned(),
            }),
            basket: Some(Basket {
                reference: "INV-1".to_owned(),
                items: vec![Item {
                    title: "Deep tissue massage".to_owned(),
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
        // The one that loses a sale if it is called an ending.
        assert_eq!(read("approved").expect("read").status, Status::Initiated);
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
            .expect("authentic"),
            "ord_1"
        );

        // The query-parameter form Tamara also sends, for a caller that has it.
        assert_eq!(
            authenticate(
                b"notify-secret",
                &[("tamaratoken", &good)],
                br#"{"order_id":"ord_1"}"#,
            )
            .expect("authentic"),
            "ord_1"
        );
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
