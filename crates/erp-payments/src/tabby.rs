//! Buy now, pay later through Tabby.
//!
//! # Not a card gateway wearing different branding
//!
//! Tabby pays the merchant and collects from the buyer, so the customer is
//! **scored before they are lent to** and can be declined — a normal outcome,
//! not an error. That is why [`Charge`] carries a [`crate::Buyer`] and a
//! [`crate::Basket`] and why this adapter refuses without them: the provider is
//! buying a receivable and wants to know who from and against what.
//!
//! # The amount is a string, and only here
//!
//! *"Amounts are decimal strings in major currency units — e.g. one hundred
//! dirhams is `"100.00"`, not `10000`."* Every money field is that type, not
//! just the total. See [`crate::decimal`] for the conversion, which is integer
//! arithmetic because the alternative is a rounding step in the middle of
//! somebody's bill.
//!
//! # Capture is mandatory, and forgetting it is not free
//!
//! *"Only captured payments are settled to you."* An authorized payment is a
//! reservation; the money arrives when it is captured. And a partial capture
//! **leaves the payment `AUTHORIZED` for ever** until it is closed — the
//! leftover is not released on its own.
//!
//! Tabby's own backstop is not a plan: *"After 21 days, Tabby may capture the
//! remaining amount in full on its side."*
//!
//! # `CLOSED` is not "paid"
//!
//! It is the terminal state for three different endings: captured in full,
//! cancelled without capture, and partially captured then closed. So
//! [`Status::Paid`] is reported only when something was actually captured, and
//! a payment closed with nothing captured is [`Status::Voided`] — which is what
//! it is.
//!
//! # The callback is a doorbell
//!
//! Tabby signs nothing. There is no HMAC, no timestamp, no signing scheme — the
//! most it offers is a **static header of the merchant's own choosing**, echoed
//! back verbatim. So the header is checked, and then the payment is re-fetched;
//! Tabby's own integration checklist says to do exactly that.

use erp_types::{CurrencyCode, Money};
use serde::Deserialize;

use crate::decimal::{from_decimal, to_decimal};
use crate::{
    Basket, CallbackError, Charge, Charged, Gateway, GatewayError, SECRET_HEADER, Source, Status,
    header, secrets_match,
};

/// Saudi Arabia. Tabby pins the host to the region rather than the key, and
/// this system's first market is here.
const LIVE: &str = "https://api.tabby.sa";

/// The other one, for a deployment operating in the Emirates.
pub const UAE: &str = "https://api.tabby.ai";

pub struct Tabby {
    secret: String,
    /// *"Please contact your integration manager to get the merchant code."*
    /// Required on every checkout, and it identifies the shop rather than the
    /// account.
    merchant_code: String,
    base: String,
    client: reqwest::Client,
}

impl std::fmt::Debug for Tabby {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tabby")
            .field("base", &self.base)
            .field("merchant_code", &self.merchant_code)
            .field("test_mode", &self.secret.starts_with("sk_test_"))
            .finish_non_exhaustive()
    }
}

impl Tabby {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

    pub fn new(secret: &str, merchant_code: &str) -> Result<Self, GatewayError> {
        let secret = secret.trim();
        if secret.is_empty() {
            return Err(GatewayError::Unauthenticated);
        }
        if !secret.starts_with("sk_") {
            return Err(GatewayError::Refused(
                "a Tabby secret key starts with sk_; the pk_ key is for the browser".to_owned(),
            ));
        }
        if merchant_code.trim().is_empty() {
            return Err(GatewayError::Refused(
                "Tabby needs the merchant code its integration manager issued".to_owned(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Self::TIMEOUT)
            .build()
            .map_err(|e| GatewayError::Refused(format!("the client cannot be built: {e}")))?;

        Ok(Self {
            secret: secret.to_owned(),
            merchant_code: merchant_code.trim().to_owned(),
            base: LIVE.to_owned(),
            client,
        })
    }

    /// Points this at the Emirates host, a staging one, or a test's.
    #[must_use]
    pub fn at(mut self, base: &str) -> Self {
        base.trim_end_matches('/').clone_into(&mut self.base);
        self
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Charged, GatewayError> {
        let response = request
            .bearer_auth(&self.secret)
            .send()
            .await
            .map_err(|e| GatewayError::Unreachable(e.to_string()))?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            return serde_json::from_str::<Payment>(&body)
                .map_err(|e| GatewayError::Unreadable(format!("{e}: {}", clipped(&body))))?
                .into_charged();
        }
        Err(refusal(status, &body))
    }
}

#[async_trait::async_trait]
impl Gateway for Tabby {
    fn provider(&self) -> &'static str {
        "tabby"
    }

    async fn charge(&self, charge: &Charge) -> Result<Charged, GatewayError> {
        if !matches!(charge.source, Source::Hosted) {
            return Err(GatewayError::Refused(
                "Tabby hosts its own checkout; there is no card token to send it".to_owned(),
            ));
        }
        // Refused here, naming the field, rather than as a Tabby validation
        // error a shop assistant cannot act on.
        let buyer = charge.buyer.as_ref().ok_or_else(|| {
            GatewayError::Refused(
                "Tabby scores the buyer before it will lend, so it needs their name, \
                 email and mobile number"
                    .to_owned(),
            )
        })?;
        let basket = charge.basket.as_ref().ok_or_else(|| {
            GatewayError::Refused(
                "Tabby needs the order and its lines, because it is buying the receivable"
                    .to_owned(),
            )
        })?;

        let session = self
            .client
            .post(format!("{}/api/v2/checkout", self.base))
            .json(&serde_json::json!({
                "lang": "ar",
                "merchant_code": self.merchant_code,
                "merchant_urls": {
                    "success": charge.returns.success,
                    "cancel": charge.returns.cancel,
                    // **A decline is not a failure of this system.** The
                    // customer was scored and refused credit, and the shop
                    // should offer them a card.
                    "failure": charge.returns.failure,
                },
                "payment": {
                    "amount": to_decimal(charge.amount),
                    "currency": charge.amount.currency().to_string(),
                    "description": charge.description,
                    "buyer": {
                        "name": buyer.name,
                        "email": buyer.email,
                        "phone": buyer.phone,
                    },
                    "order": {
                        "reference_id": basket.reference,
                        "items": items(basket),
                    },
                    // Required by the schema and legitimately empty for a shop
                    // that has not sold to this person before. Tabby's own
                    // quick-start sends exactly this.
                    "buyer_history": {},
                    "order_history": [],
                    "shipping_address": {},
                },
            }))
            .bearer_auth(&self.secret)
            .send()
            .await
            .map_err(|e| GatewayError::Unreachable(e.to_string()))?;

        let status = session.status();
        let body = session.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(refusal(status, &body));
        }

        let session: Session = serde_json::from_str(&body)
            .map_err(|e| GatewayError::Unreadable(format!("{e}: {}", clipped(&body))))?;

        // **`payment.id`, not the session id.** They are different UUIDs and
        // every later call names the payment: *"Save `payment.id` from the
        // response — it will be used to verify, capture and refund."*
        let mut charged = session.payment.into_charged()?;
        charged.challenge = session
            .configuration
            .and_then(|c| c.available_products)
            .and_then(|p| p.installments.into_iter().next())
            .and_then(|i| i.web_url);
        Ok(charged)
    }

    async fn fetch(&self, id: &str) -> Result<Charged, GatewayError> {
        let request = self
            .client
            .get(format!("{}/api/v2/payments/{id}", self.base));
        match self.send(request).await {
            Err(GatewayError::Refused(_)) => Err(GatewayError::NoSuchPayment(id.to_owned())),
            other => other,
        }
    }

    async fn capture(&self, id: &str, amount: Option<Money>) -> Result<Charged, GatewayError> {
        // **Both fields are required**, and `reference_id` is documented as the
        // idempotency key — the only one Tabby has, and it exists on capture
        // and refund and nowhere else.
        let amount = amount.ok_or_else(|| {
            GatewayError::Refused(
                "Tabby requires the amount on a capture; there is no 'all of it' form".to_owned(),
            )
        })?;
        self.send(
            self.client
                .post(format!("{}/api/v2/payments/{id}/captures", self.base))
                .json(&serde_json::json!({
                    "amount": to_decimal(amount),
                    "reference_id": format!("cap-{id}-{}", amount.minor()),
                })),
        )
        .await
    }

    async fn refund(&self, id: &str, amount: Option<Money>) -> Result<Charged, GatewayError> {
        let amount = amount.ok_or_else(|| {
            GatewayError::Refused(
                "Tabby requires the amount on a refund; there is no 'all of it' form".to_owned(),
            )
        })?;
        self.send(
            self.client
                .post(format!("{}/api/v2/payments/{id}/refunds", self.base))
                .json(&serde_json::json!({
                    "amount": to_decimal(amount),
                    "reference_id": format!("ref-{id}-{}", amount.minor()),
                })),
        )
        .await
    }

    /// Close, which is what Tabby calls cancelling.
    ///
    /// *"If an order is fully cancelled, please close the payment without
    /// capturing it — the customer will be refunded for all paid amount."* It
    /// is also how the leftover of a partial capture is released, which does
    /// not happen on its own.
    async fn void(&self, id: &str) -> Result<Charged, GatewayError> {
        self.send(
            self.client
                .post(format!("{}/api/v2/payments/{id}/close", self.base)),
        )
        .await
    }
}

/// **Whether a callback really came from Tabby.**
///
/// Tabby signs nothing — no HMAC, no timestamp, no signing scheme. The most it
/// offers is a static header the merchant names at registration, echoed back
/// verbatim. Since the name is ours to choose, this system fixes it as
/// [`SECRET_HEADER`] so the value a tenant configures and the value this code
/// looks for cannot drift apart.
///
/// A leaked header value replays perfectly. That is why the answer is an id and
/// the truth comes from `fetch`.
pub(crate) fn authenticate(
    secret: &[u8],
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<String, CallbackError> {
    let offered = header(headers, SECRET_HEADER).unwrap_or_default();
    if !secrets_match(offered.as_bytes(), secret) {
        return Err(CallbackError::NotAuthentic);
    }

    // Authenticated, and now only the id. The body's own `amount` and `status`
    // are read by nothing.
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| CallbackError::Unreadable("no payment id in the callback".to_owned()))
}

fn items(basket: &Basket) -> Vec<serde_json::Value> {
    basket
        .items
        .iter()
        .map(|item| {
            serde_json::json!({
                "title": item.title,
                "quantity": item.quantity,
                "unit_price": to_decimal(item.unit_price),
            })
        })
        .collect()
}

/// Tabby's payment object, as much of it as this system reads.
#[derive(Debug, Deserialize)]
struct Payment {
    id: String,
    status: String,
    amount: String,
    currency: String,
    #[serde(default)]
    captures: Vec<Movement>,
    #[serde(default)]
    refunds: Vec<Movement>,
}

#[derive(Debug, Deserialize)]
struct Movement {
    amount: String,
}

#[derive(Debug, Deserialize)]
struct Session {
    payment: Payment,
    #[serde(default)]
    configuration: Option<Configuration>,
}

#[derive(Debug, Deserialize)]
struct Configuration {
    #[serde(default)]
    available_products: Option<Products>,
}

#[derive(Debug, Deserialize)]
struct Products {
    #[serde(default)]
    installments: Vec<Product>,
}

#[derive(Debug, Deserialize)]
struct Product {
    #[serde(default)]
    web_url: Option<String>,
}

impl Payment {
    fn into_charged(self) -> Result<Charged, GatewayError> {
        let currency: CurrencyCode = self.currency.parse().map_err(|_| {
            GatewayError::Unreadable(format!("{} is not a currency code", self.currency))
        })?;
        let amount = read(&self.amount, currency)?;
        let sum = |movements: &[Movement]| -> Result<Money, GatewayError> {
            let mut total = Money::from_minor(0, currency);
            for movement in movements {
                total = total
                    .checked_add(read(&movement.amount, currency)?)
                    .map_err(|e| GatewayError::Unreadable(e.to_string()))?;
            }
            Ok(total)
        };
        let captured = sum(&self.captures)?;
        let refunded = sum(&self.refunds)?;

        // **The status is uppercase from the API and lowercase in a webhook**,
        // which is Tabby's inconsistency rather than a distinction.
        let status = match self.status.to_ascii_uppercase().as_str() {
            "CREATED" => Status::Initiated,
            "AUTHORIZED" => Status::Authorized,
            // **`CLOSED` is three endings.** Captured in full, cancelled
            // without capture, or partially captured and then closed — so what
            // happened is decided by whether anything was actually captured,
            // not by the word.
            "CLOSED" if captured.minor() > 0 => {
                if refunded.minor() >= captured.minor() {
                    Status::Refunded
                } else {
                    Status::Paid
                }
            }
            // Closed with nothing captured is a cancellation. `EXPIRED` is the
            // checkout being abandoned or timing out — Tabby is explicit that
            // it never means an authorization expired. Both end with no money.
            "CLOSED" | "EXPIRED" => Status::Voided,
            "REJECTED" => Status::Failed,
            other => {
                return Err(GatewayError::Unreadable(format!(
                    "{other} is not a Tabby payment status this system knows"
                )));
            }
        };

        Ok(Charged {
            id: self.id,
            status,
            amount,
            refunded,
            // Tabby reports its cut on the settlement report rather than on the
            // payment. Claiming a zero here would post a fee that is not one.
            fee: None,
            challenge: None,
            message: None,
        })
    }
}

fn read(value: &str, currency: CurrencyCode) -> Result<Money, GatewayError> {
    from_decimal(value, currency).map_err(|e| GatewayError::Unreadable(e.to_string()))
}

fn refusal(status: reqwest::StatusCode, body: &str) -> GatewayError {
    let said = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .or_else(|| v.get("errorType"))
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

    fn sar(minor: i64) -> Money {
        Money::from_minor(minor, "SAR".parse().expect("a currency"))
    }

    fn charge() -> Charge {
        Charge {
            reference: "INV-1".to_owned(),
            amount: sar(34_000),
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
                    unit_price: sar(34_000),
                }],
            }),
        }
    }

    fn built() -> Tabby {
        Tabby::new("sk_test_secret", "bassat").expect("built")
    }

    #[test]
    fn a_browser_key_is_refused_and_a_merchant_code_is_required() {
        assert!(matches!(
            Tabby::new("pk_test_x", "bassat"),
            Err(GatewayError::Refused(_))
        ));
        assert!(matches!(
            Tabby::new("sk_test_x", "  "),
            Err(GatewayError::Refused(_))
        ));
        assert!(matches!(
            Tabby::new("", "bassat"),
            Err(GatewayError::Unauthenticated)
        ));
    }

    #[test]
    fn a_tabby_client_does_not_print_its_key() {
        let printed = format!("{:?}", built());
        assert!(printed.contains("bassat"), "{printed}");
        assert!(!printed.contains("sk_test_secret"), "{printed}");
    }

    /// **Refused here, naming the field.** A shop assistant can act on "we need
    /// their mobile number"; they cannot act on a Tabby validation error.
    #[tokio::test]
    async fn a_buyer_and_a_basket_are_not_optional_for_a_lender() {
        // Nothing listens here; reaching the network would be the failure.
        let tabby = built().at("http://127.0.0.1:1");

        let mut anonymous = charge();
        anonymous.buyer = None;
        assert!(matches!(
            tabby.charge(&anonymous).await,
            Err(GatewayError::Refused(_))
        ));

        let mut basketless = charge();
        basketless.basket = None;
        assert!(matches!(
            tabby.charge(&basketless).await,
            Err(GatewayError::Refused(_))
        ));

        // And a card token, which is a different product than the customer chose.
        let mut carded = charge();
        carded.source = Source::Token {
            token: "tok_1".to_owned(),
        };
        assert!(matches!(
            tabby.charge(&carded).await,
            Err(GatewayError::Refused(_))
        ));
    }

    /// **The bytes on the wire.** Every money field is a quoted decimal string
    /// in major units, which is the thing that costs a hundred times too much
    /// if it is wrong.
    #[tokio::test]
    async fn the_amount_goes_out_as_a_quoted_decimal_in_major_units() {
        let server = OneRequest::answering(
            200,
            r#"{"id":"sess_1","status":"created",
                "payment":{"id":"pay_1","status":"CREATED","amount":"340.00","currency":"SAR"},
                "configuration":{"available_products":{"installments":[
                    {"web_url":"https://checkout.tabby.ai/?sessionId=1"}]}}}"#,
        )
        .await;

        let charged = built()
            .at(&server.url())
            .charge(&charge())
            .await
            .expect("creates a session");

        // The payment id, never the session id.
        assert_eq!(charged.id, "pay_1");
        assert_eq!(charged.status, Status::Initiated);
        assert_eq!(
            charged.challenge.as_deref(),
            Some("https://checkout.tabby.ai/?sessionId=1")
        );
        assert_eq!(charged.amount, sar(34_000));

        let sent = server.seen().await;
        assert!(sent.starts_with("POST /api/v2/checkout "), "{sent}");
        assert!(
            sent.contains("authorization: Bearer sk_test_secret"),
            "{sent}"
        );
        // Quoted, in riyals, two places.
        assert!(sent.contains(r#""amount":"340.00""#), "{sent}");
        assert!(!sent.contains(r#""amount":34000"#), "{sent}");
        assert!(sent.contains(r#""unit_price":"340.00""#), "{sent}");
        assert!(sent.contains(r#""merchant_code":"bassat""#), "{sent}");
        assert!(sent.contains(r#""phone":"+966500000001""#), "{sent}");
    }

    /// **`CLOSED` is three endings**, and only one of them is money.
    #[test]
    fn closed_means_paid_only_when_something_was_captured() {
        let read = |status: &str, captures: &str, refunds: &str| {
            serde_json::from_str::<Payment>(&format!(
                r#"{{"id":"pay_1","status":"{status}","amount":"340.00","currency":"SAR",
                     "captures":{captures},"refunds":{refunds}}}"#
            ))
            .expect("parses")
            .into_charged()
            .expect("read")
        };

        let captured = read("CLOSED", r#"[{"amount":"340.00"}]"#, "[]");
        assert_eq!(captured.status, Status::Paid);
        assert_eq!(captured.amount, sar(34_000));

        // Closed with nothing captured is a cancellation, not a sale.
        assert_eq!(read("CLOSED", "[]", "[]").status, Status::Voided);

        // Captured and then given all of it back.
        let refunded = read(
            "CLOSED",
            r#"[{"amount":"340.00"}]"#,
            r#"[{"amount":"340.00"}]"#,
        );
        assert_eq!(refunded.status, Status::Refunded);
        assert_eq!(refunded.refunded, sar(34_000));

        // Partly given back is still a sale that happened.
        let partly = read(
            "CLOSED",
            r#"[{"amount":"340.00"}]"#,
            r#"[{"amount":"40.00"}]"#,
        );
        assert_eq!(partly.status, Status::Paid);
        assert_eq!(partly.refunded, sar(4_000));

        // A partial capture leaves it authorized, which is Tabby's own trap.
        assert_eq!(
            read("AUTHORIZED", r#"[{"amount":"40.00"}]"#, "[]").status,
            Status::Authorized
        );
    }

    /// A webhook says `closed`; the API says `CLOSED`. Tabby's inconsistency,
    /// not a distinction.
    #[test]
    fn the_status_is_read_in_either_case() {
        let lower = serde_json::from_str::<Payment>(
            r#"{"id":"p","status":"authorized","amount":"1.00","currency":"SAR"}"#,
        )
        .expect("parses")
        .into_charged()
        .expect("read");
        assert_eq!(lower.status, Status::Authorized);
    }

    #[test]
    fn a_status_this_system_does_not_know_is_not_guessed_at() {
        assert!(matches!(
            serde_json::from_str::<Payment>(
                r#"{"id":"p","status":"SOMETHING_NEW","amount":"1.00","currency":"SAR"}"#
            )
            .expect("parses")
            .into_charged(),
            Err(GatewayError::Unreadable(_))
        ));
    }

    /// The header is the whole credential, and a leaked one replays.
    #[test]
    fn a_callback_is_believed_only_with_the_header_this_system_registered() {
        let body = br#"{"id":"pay_1","status":"closed","amount":"340.00"}"#;

        assert_eq!(
            authenticate(b"shhh", &[(SECRET_HEADER, "shhh")], body).expect("authentic"),
            "pay_1"
        );
        // Capitalised differently by whatever proxy it came through.
        assert_eq!(
            authenticate(b"shhh", &[("X-Erp-Webhook-Secret", "shhh")], body).expect("authentic"),
            "pay_1"
        );

        assert_eq!(
            authenticate(b"shhh", &[(SECRET_HEADER, "wrong")], body),
            Err(CallbackError::NotAuthentic)
        );
        assert_eq!(
            authenticate(b"shhh", &[], body),
            Err(CallbackError::NotAuthentic)
        );
    }

    /// Capture and refund both require an amount, because Tabby has no "all of
    /// it" form and guessing one would capture the wrong sum.
    #[tokio::test]
    async fn a_capture_without_an_amount_is_refused_rather_than_guessed() {
        let tabby = built().at("http://127.0.0.1:1");
        assert!(matches!(
            tabby.capture("pay_1", None).await,
            Err(GatewayError::Refused(_))
        ));
        assert!(matches!(
            tabby.refund("pay_1", None).await,
            Err(GatewayError::Refused(_))
        ));
    }

    #[tokio::test]
    async fn a_capture_carries_the_amount_and_the_only_idempotency_key_tabby_has() {
        let server = OneRequest::answering(
            200,
            r#"{"id":"pay_1","status":"CLOSED","amount":"340.00","currency":"SAR",
                "captures":[{"amount":"340.00"}]}"#,
        )
        .await;

        let charged = built()
            .at(&server.url())
            .capture("pay_1", Some(sar(34_000)))
            .await
            .expect("captures");
        assert_eq!(charged.status, Status::Paid);

        let sent = server.seen().await;
        assert!(
            sent.starts_with("POST /api/v2/payments/pay_1/captures "),
            "{sent}"
        );
        assert!(sent.contains(r#""amount":"340.00""#), "{sent}");
        assert!(sent.contains(r#""reference_id""#), "{sent}");
    }
}
