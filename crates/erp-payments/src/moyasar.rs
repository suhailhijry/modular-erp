//! Cards through Moyasar.
//!
//! # The amount needs no conversion, and that is worth saying
//!
//! Moyasar's `amount` is *"a positive integer representing the payment amount
//! in the smallest currency unit"* — `1.00 SAR = 100`, `1.00 KWD = 1000`,
//! `1 JPY = 1`. That is exactly what [`Money`] stores, so the integer goes
//! straight onto the wire with no arithmetic at all.
//!
//! It is worth a paragraph because the other two gateways this system will talk
//! to are not like this: both take decimal *major* units, where getting the
//! exponent wrong charges a hundred times too much or too little. Here there is
//! nothing to get wrong, and a future reader comparing the three files should
//! know that is a fact about Moyasar rather than an omission here.
//!
//! # `given_id` is the idempotency key
//!
//! Moyasar has no `Idempotency-Key` header. What it has is a top-level
//! `given_id` — *"a UUID that you generate from your side … it is going to be
//! the ID of the created payment"* — so a retried charge lands on the same
//! payment instead of charging the customer twice.
//!
//! This client therefore requires the caller's reference to be a UUID, and
//! refuses rather than dropping it. Silently sending no `given_id` would turn
//! every network timeout into a possible double charge, which is the failure
//! this whole design exists to avoid (L8).
//!
//! # A webhook is not signed, so it is not believed
//!
//! Moyasar does not sign webhook bodies. There is no HMAC and no signing
//! header; what arrives is a `secret_token` field **inside the JSON**, holding
//! the shared secret configured when the webhook was registered.
//!
//! So [`Moyasar::callback`] compares that token in constant time and then
//! returns **only the payment id**. Everything the body says about money is
//! discarded, and [`Gateway::fetch`] is asked over an authenticated connection
//! instead. That is what Moyasar's own reference plugin does, and it is the
//! only design that survives somebody who learned the callback URL.
//!
//! The same goes for `callback_url`: the `id`, `status` and `message` query
//! parameters Moyasar appends are followed by the *customer's own browser* and
//! are therefore theirs to edit.
//!
//! # No card number ever reaches this process
//!
//! *"Sending cardholder data to the merchant backend is prohibited and will
//! result in canceling the agreement between Moyasar and the merchant"*. So
//! [`Source::Token`] is the only source this client can express, and the token
//! is minted in the browser against the publishable key.

use erp_types::Money;
use serde::Deserialize;

use crate::{CallbackError, Charge, Charged, Gateway, GatewayError, Source, Status, secrets_match};

/// Live and test are the same host; the key prefix decides which.
const LIVE: &str = "https://api.moyasar.com";

pub struct Moyasar {
    /// `sk_live_…` or `sk_test_…`. Every operation this client performs needs
    /// the secret key: `charge` could use the publishable one, but a server
    /// that already holds the secret gains nothing by holding both.
    secret: String,
    base: String,
    client: reqwest::Client,
}

impl std::fmt::Debug for Moyasar {
    /// The base URL and **whether this is test mode**, which is the one thing
    /// worth knowing from a log line — and never the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Moyasar")
            .field("base", &self.base)
            .field("test_mode", &self.secret.starts_with("sk_test_"))
            .finish_non_exhaustive()
    }
}

impl Moyasar {
    /// Longer than a message transport's, and deliberately.
    ///
    /// A card authorization goes to an issuing bank, and eight seconds is
    /// inside the range where a real one is still thinking. Still comfortably
    /// under the dispatcher's thirty-second lease.
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

    pub fn new(secret: &str) -> Result<Self, GatewayError> {
        let secret = secret.trim();
        if secret.is_empty() {
            return Err(GatewayError::Unauthenticated);
        }
        // Caught here rather than as a `401` on the first customer's card: a
        // publishable key in the secret slot can create a payment and cannot
        // capture, refund or fetch one, which would fail *after* the money
        // moved.
        if !secret.starts_with("sk_") {
            return Err(GatewayError::Refused(
                "a Moyasar secret key starts with sk_; a publishable key cannot \
                 fetch or refund a payment"
                    .to_owned(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Self::TIMEOUT)
            .build()
            .map_err(|e| GatewayError::Refused(format!("the client cannot be built: {e}")))?;

        Ok(Self {
            secret: secret.to_owned(),
            base: LIVE.to_owned(),
            client,
        })
    }

    /// Points this somewhere else. For tests.
    #[must_use]
    pub fn at(mut self, base: &str) -> Self {
        base.trim_end_matches('/').clone_into(&mut self.base);
        self
    }

    /// Every call is HTTP Basic with the key as the username and **an empty
    /// password**, which is what the documentation is emphatic about.
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.base))
            .basic_auth(&self.secret, Some(""))
    }

    async fn read(&self, response: reqwest::Response) -> Result<Charged, GatewayError> {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if status.is_success() {
            let payment: Payment = serde_json::from_str(&body)
                .map_err(|e| GatewayError::Unreadable(format!("{e}: {}", clipped(&body))))?;
            return payment.into_charged();
        }
        Err(refusal(status, &body))
    }

    /// An amount only when there is one — Moyasar reads a missing `amount` as
    /// "all of it", and sending `null` is not the same thing.
    async fn act(
        &self,
        id: &str,
        action: &str,
        amount: Option<Money>,
    ) -> Result<Charged, GatewayError> {
        let mut request = self.post(&format!("/v1/payments/{id}/{action}"));
        if let Some(amount) = amount {
            request = request.json(&serde_json::json!({ "amount": amount.minor() }));
        }
        let response = request
            .send()
            .await
            .map_err(|e| GatewayError::Unreachable(e.to_string()))?;
        self.read(response).await
    }
}

#[async_trait::async_trait]
impl Gateway for Moyasar {
    fn provider(&self) -> &'static str {
        "moyasar"
    }

    async fn charge(&self, charge: &Charge) -> Result<Charged, GatewayError> {
        // **The idempotency key, or nothing.** See the module docs: without a
        // `given_id` a retried timeout is a second charge.
        if !is_uuid(&charge.reference) {
            return Err(GatewayError::Refused(format!(
                "{} is not a UUID, and Moyasar's idempotency key must be one",
                charge.reference
            )));
        }
        if !charge.amount.minor().is_positive() {
            return Err(GatewayError::Refused(
                "a charge must be for a positive amount".to_owned(),
            ));
        }

        let Source::Token { token } = &charge.source;
        let response = self
            .post("/v1/payments")
            .json(&serde_json::json!({
                "given_id": charge.reference,
                // Straight through. See the module docs.
                "amount": charge.amount.minor(),
                "currency": charge.amount.currency().to_string(),
                "description": charge.description,
                "callback_url": charge.return_to,
                "source": { "type": "token", "token": token },
            }))
            .send()
            .await
            .map_err(|e| GatewayError::Unreachable(e.to_string()))?;

        self.read(response).await
    }

    async fn fetch(&self, id: &str) -> Result<Charged, GatewayError> {
        let response = self
            .client
            .get(format!("{}/v1/payments/{id}", self.base))
            .basic_auth(&self.secret, Some(""))
            .send()
            .await
            .map_err(|e| GatewayError::Unreachable(e.to_string()))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(GatewayError::NoSuchPayment(id.to_owned()));
        }
        self.read(response).await
    }

    async fn capture(&self, id: &str, amount: Option<Money>) -> Result<Charged, GatewayError> {
        self.act(id, "capture", amount).await
    }

    async fn refund(&self, id: &str, amount: Option<Money>) -> Result<Charged, GatewayError> {
        self.act(id, "refund", amount).await
    }

    async fn void(&self, id: &str) -> Result<Charged, GatewayError> {
        // Documented as taking no body at all.
        self.act(id, "void", None).await
    }
}

/// **Whether a callback really came from Moyasar.**
///
/// Moyasar does not sign webhook bodies: there is no HMAC and no signing
/// header. What arrives is a `secret_token` field *inside the JSON*, holding
/// the shared secret configured when the webhook was registered. So the token
/// is compared in constant time, and then **only the payment id** is returned.
pub(crate) fn authenticate(secret: &[u8], body: &[u8]) -> Result<String, CallbackError> {
    // **Parsed leniently, on purpose.** Anything that is not a JSON object
    // carrying the right token is `NotAuthentic` and not `Unreadable`: a caller
    // who can tell "your JSON is malformed" from "your secret is wrong" has an
    // oracle, and there is no legitimate sender who needs to know the
    // difference. `Unreadable` is reserved for a body that *did* authenticate
    // and still made no sense, where the answer helps whoever is on call.
    let event: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| CallbackError::NotAuthentic)?;

    let token = event
        .get("secret_token")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if !secrets_match(token.as_bytes(), secret) {
        return Err(CallbackError::NotAuthentic);
    }

    // Authenticated. **And now only the id** — everything this body says about
    // money is discarded, and `Gateway::fetch` is asked instead.
    event
        .get("data")
        .and_then(|data| data.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| CallbackError::Unreadable("no payment id in the event".to_owned()))
}

/// Moyasar's payment object, as much of it as this system reads.
#[derive(Debug, Deserialize)]
struct Payment {
    id: String,
    status: String,
    amount: i64,
    currency: String,
    #[serde(default)]
    fee: Option<i64>,
    #[serde(default)]
    refunded: i64,
    #[serde(default)]
    source: Option<PaymentSource>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PaymentSource {
    /// Where the customer goes for the 3-D Secure challenge. Present only while
    /// the payment is `initiated`.
    #[serde(default)]
    transaction_url: Option<String>,
}

impl Payment {
    fn into_charged(self) -> Result<Charged, GatewayError> {
        let currency = self.currency.parse().map_err(|_| {
            GatewayError::Unreadable(format!("{} is not a currency code", self.currency))
        })?;
        let money = |minor| Money::from_minor(minor, currency);

        let status = match self.status.as_str() {
            "initiated" => Status::Initiated,
            "authorized" => Status::Authorized,
            // **Both are money moved**, one in a step and one in two.
            "paid" | "captured" => Status::Paid,
            "failed" => Status::Failed,
            "refunded" => Status::Refunded,
            "voided" => Status::Voided,
            // `verified` is the one-riyal check a card tokenization makes. It
            // is not a purchase, and calling it one would record a sale that
            // did not happen.
            other => {
                return Err(GatewayError::Unreadable(format!(
                    "{other} is not a payment status this system knows"
                )));
            }
        };

        Ok(Charged {
            id: self.id,
            status,
            amount: money(self.amount),
            refunded: money(self.refunded),
            fee: self.fee.map(money),
            challenge: self.source.and_then(|s| s.transaction_url),
            message: self.message,
        })
    }
}

/// What a non-2xx means for this payment.
fn refusal(status: reqwest::StatusCode, body: &str) -> GatewayError {
    #[derive(Deserialize)]
    struct Failure {
        #[serde(default)]
        message: Option<String>,
    }
    let said = serde_json::from_str::<Failure>(body)
        .ok()
        .and_then(|f| f.message)
        .unwrap_or_else(|| clipped(body));

    match status {
        // The account, not the card. The one that should page somebody.
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            GatewayError::Unauthenticated
        }
        // Moyasar's own documented rate limit answer, and a real outage. Both
        // are worth another go.
        reqwest::StatusCode::TOO_MANY_REQUESTS => GatewayError::Unreachable(said),
        _ if status.is_server_error() => GatewayError::Unreachable(format!("{status}: {said}")),
        _ => GatewayError::Refused(said),
    }
}

/// Whether a string is a UUID, in the only sense Moyasar cares about: the
/// 8-4-4-4-12 shape.
fn is_uuid(value: &str) -> bool {
    let groups = [8, 4, 4, 4, 12];
    let mut parts = value.split('-');
    for width in groups {
        let Some(part) = parts.next() else {
            return false;
        };
        if part.len() != width || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}

fn clipped(body: &str) -> String {
    body.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::OneRequest;

    fn sar(minor: i64) -> Money {
        Money::from_minor(minor, "SAR".parse().expect("a currency"))
    }

    const REFERENCE: &str = "3fa85f64-5717-4562-b3fc-2c963f66afa6";

    fn charge() -> Charge {
        Charge {
            reference: REFERENCE.to_owned(),
            amount: sar(10_000),
            return_to: "https://bassat.erp.com/paid".to_owned(),
            source: Source::Token {
                token: "token_qbmmXzo97AESrZLS6KpWvof6uK2hAKcQGfEcKg".to_owned(),
            },
            description: "Invoice INV-1".to_owned(),
        }
    }

    fn paid(status: &str) -> String {
        format!(
            r#"{{"id":"pay_1","status":"{status}","amount":10000,"currency":"SAR",
                 "fee":275,"refunded":0,"source":{{"type":"token"}}}}"#
        )
    }

    #[test]
    fn a_publishable_key_is_refused_before_it_can_fail_after_the_money_moved() {
        assert!(Moyasar::new("sk_test_abc").is_ok());
        assert!(matches!(
            Moyasar::new("pk_test_abc"),
            Err(GatewayError::Refused(_))
        ));
        assert!(matches!(
            Moyasar::new("  "),
            Err(GatewayError::Unauthenticated)
        ));
    }

    #[test]
    fn a_moyasar_client_does_not_print_its_key() {
        let printed = format!("{:?}", Moyasar::new("sk_test_secret").expect("built"));
        assert!(printed.contains("test_mode: true"), "{printed}");
        assert!(!printed.contains("sk_test_secret"), "{printed}");
    }

    /// **The bytes on the wire**, against a server that shows them.
    #[tokio::test]
    async fn the_request_is_the_one_moyasar_documents() {
        let server =
            OneRequest::answering(201, Box::leak(paid("initiated").into_boxed_str())).await;

        let charged = Moyasar::new("sk_test_secret")
            .expect("built")
            .at(&server.url())
            .charge(&charge())
            .await
            .expect("charges");
        assert_eq!(charged.status, Status::Initiated);

        let sent = server.seen().await;
        assert!(sent.starts_with("POST /v1/payments "), "{sent}");
        // Basic, key as the username, **empty password** — `sk_test_secret:`.
        assert!(
            sent.contains("authorization: Basic c2tfdGVzdF9zZWNyZXQ6"),
            "{sent}"
        );
        // The integer, straight through, unquoted.
        assert!(sent.contains(r#""amount":10000"#), "{sent}");
        assert!(sent.contains(r#""currency":"SAR""#), "{sent}");
        assert!(
            sent.contains(&format!(r#""given_id":"{REFERENCE}""#)),
            "{sent}"
        );
        assert!(sent.contains(r#""type":"token""#), "{sent}");
        // A card number cannot appear, because there is nowhere to put one.
        assert!(!sent.contains("number"), "{sent}");
    }

    /// Without `given_id` a retried timeout is a second charge, so a reference
    /// Moyasar will not take as one never becomes a request.
    #[tokio::test]
    async fn a_reference_that_is_not_a_uuid_never_leaves_this_process() {
        let moyasar = Moyasar::new("sk_test_x")
            .expect("built")
            // Nothing listens here; reaching the network would be the failure.
            .at("http://127.0.0.1:1");

        let mut named = charge();
        named.reference = "INV-1".to_owned();
        assert!(matches!(
            moyasar.charge(&named).await,
            Err(GatewayError::Refused(_))
        ));

        let mut free = charge();
        free.amount = sar(0);
        assert!(matches!(
            moyasar.charge(&free).await,
            Err(GatewayError::Refused(_))
        ));
    }

    #[test]
    fn a_uuid_is_the_shape_moyasar_asks_for() {
        assert!(is_uuid(REFERENCE));
        assert!(is_uuid("00000000-0000-0000-0000-000000000000"));
        assert!(!is_uuid("3fa85f64-5717-4562-b3fc-2c963f66afa"), "too short");
        assert!(!is_uuid("3fa85f645717456 2b3fc2c963f66afa6"), "no hyphens");
        assert!(
            !is_uuid("3fa85f64-5717-4562-b3fc-2c963f66afa6-x"),
            "a group too many"
        );
        assert!(!is_uuid("zfa85f64-5717-4562-b3fc-2c963f66afa6"), "not hex");
        assert!(!is_uuid(""));
    }

    /// `captured` and `paid` are both money moved; `verified` is a card check
    /// and recording it as a sale would invent revenue.
    #[test]
    fn every_status_moyasar_documents_is_read_or_refused() {
        let read = |status: &str| {
            serde_json::from_str::<Payment>(&paid(status))
                .expect("parses")
                .into_charged()
        };

        assert_eq!(read("initiated").expect("read").status, Status::Initiated);
        assert_eq!(read("authorized").expect("read").status, Status::Authorized);
        assert_eq!(read("paid").expect("read").status, Status::Paid);
        assert_eq!(read("captured").expect("read").status, Status::Paid);
        assert_eq!(read("failed").expect("read").status, Status::Failed);
        assert_eq!(read("refunded").expect("read").status, Status::Refunded);
        assert_eq!(read("voided").expect("read").status, Status::Voided);

        assert!(matches!(read("verified"), Err(GatewayError::Unreadable(_))));
        assert!(matches!(
            read("something_new"),
            Err(GatewayError::Unreadable(_))
        ));
    }

    /// The fee is read where it is given, and it is an amount rather than a
    /// number — a fee in the wrong currency is a fee posted wrong.
    #[test]
    fn the_fee_comes_back_as_money_in_the_payments_currency() {
        let charged = serde_json::from_str::<Payment>(&paid("paid"))
            .expect("parses")
            .into_charged()
            .expect("read");
        assert_eq!(charged.fee, Some(sar(275)));
        assert_eq!(charged.amount, sar(10_000));
        assert!(charged.matches(sar(10_000)));
    }

    /// A wrong key pages somebody; a declined card does not.
    #[test]
    fn the_accounts_problem_and_the_cards_problem_are_different_answers() {
        assert_eq!(
            refusal(reqwest::StatusCode::UNAUTHORIZED, "{}"),
            GatewayError::Unauthenticated
        );
        assert!(matches!(
            refusal(
                reqwest::StatusCode::BAD_REQUEST,
                r#"{"message":"Card declined"}"#
            ),
            GatewayError::Refused(_)
        ));
        assert!(matches!(
            refusal(reqwest::StatusCode::TOO_MANY_REQUESTS, "slow down"),
            GatewayError::Unreachable(_)
        ));
        assert!(matches!(
            refusal(reqwest::StatusCode::BAD_GATEWAY, "oops"),
            GatewayError::Unreachable(_)
        ));
    }

    /// **The callback tells you what to look at, and nothing else.**
    #[test]
    fn a_callback_yields_an_id_and_never_an_amount() {
        let body = format!(
            r#"{{"id":"evt_1","type":"payment_paid","secret_token":"shhh",
                 "live":true,"data":{}}}"#,
            paid("paid")
        );
        assert_eq!(
            authenticate(b"shhh", body.as_bytes()).expect("authentic"),
            "pay_1"
        );
    }

    /// Anybody can reach the URL. The secret is the whole credential.
    #[test]
    fn a_callback_with_the_wrong_secret_is_not_believed() {
        let body = format!(
            r#"{{"id":"evt_1","secret_token":"shhh","data":{}}}"#,
            paid("paid")
        );
        assert_eq!(
            authenticate(b"not-it", body.as_bytes()),
            Err(CallbackError::NotAuthentic)
        );
        assert_eq!(
            authenticate(b"", body.as_bytes()),
            Err(CallbackError::NotAuthentic)
        );

        // A body with no token at all, which is what somebody who has only ever
        // read the documentation would send.
        assert_eq!(
            authenticate(
                b"shhh",
                br#"{"id":"evt_1","type":"payment_paid","data":{"id":"pay_1"}}"#
            ),
            Err(CallbackError::NotAuthentic)
        );

        // **And a malformed body answers the same way.** Anything else is an
        // oracle: a caller who can tell "your JSON is wrong" from "your secret
        // is wrong" learns something about the secret.
        assert_eq!(
            authenticate(b"shhh", b"{\"secret_token\": 12}"),
            Err(CallbackError::NotAuthentic)
        );
    }

    /// A capture for part of the hold sends the amount; a capture for all of it
    /// sends no body, because a missing amount is how Moyasar spells "all".
    #[tokio::test]
    async fn a_partial_capture_names_its_amount_and_a_full_one_does_not() {
        let server = OneRequest::sequence(vec![
            (
                200,
                r#"{"id":"pay_1","status":"paid","amount":10000,"currency":"SAR","refunded":0}"#,
            ),
            (
                200,
                r#"{"id":"pay_1","status":"paid","amount":10000,"currency":"SAR","refunded":0}"#,
            ),
        ])
        .await;
        let moyasar = Moyasar::new("sk_test_x").expect("built").at(&server.url());

        moyasar
            .capture("pay_1", Some(sar(3_000)))
            .await
            .expect("captures");
        moyasar.capture("pay_1", None).await.expect("captures");

        let sent = server.seen().await;
        let (partial, full) = sent.split_once("\n===\n").expect("two requests");
        assert!(
            partial.starts_with("POST /v1/payments/pay_1/capture "),
            "{partial}"
        );
        assert!(partial.contains(r#"{"amount":3000}"#), "{partial}");
        assert!(!full.contains("amount"), "{full}");
    }
}
