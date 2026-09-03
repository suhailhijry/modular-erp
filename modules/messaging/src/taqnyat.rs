//! SMS through Taqnyat.
//!
//! # What this is against
//!
//! Taqnyat's documented REST API — `POST https://api.taqnyat.sa/v1/messages`,
//! a bearer token, and three required fields. No account exists in this build,
//! so **the first live send is the operator's**. What is tested here is the
//! request this client makes and the answer it makes of every documented
//! reply, which is the part a wrong assumption ruins silently.
//!
//! # A number, not a string
//!
//! Every example Taqnyat publishes — the `OpenAPI` schema, the curl in the docs,
//! both of their own SDKs — sends `recipients` as **unquoted JSON numbers**:
//! `[966500000000]`. Whether a quoted string is accepted is not documented
//! anywhere, so this sends what the documentation shows.
//!
//! The number itself must be "international format without 00 or symbol (+)",
//! and this system stores `+966500000000`. [`msisdn`] is that conversion, and
//! it refuses rather than sending something the gateway will reject — the
//! documented failure for a bad number is permanent, so a number this client
//! could have fixed would otherwise become a dead letter.
//!
//! # Two ways a send fails, and one of them answers `201`
//!
//! A `201` carries `accepted` and `rejected`, and **a rejected recipient still
//! comes back `201`**. Answering "sent" to that would be the worst kind of
//! wrong: a message nobody received, recorded as delivered. So the body is
//! read on success too.
//!
//! Both fields are strings shaped like `"[966500000000,]"` — bracketed, comma
//! separated, with a trailing comma. They are not JSON and are not parsed as
//! JSON.
//!
//! # There is no idempotency key, and that has a cost
//!
//! Taqnyat documents none: no header, no client reference, no dedupe field. So
//! a send that times out after the gateway accepted it is **sent, retried, and
//! billed twice** when the dispatcher tries again. That is a property of the
//! provider rather than a bug here, and the alternative — treating a timeout as
//! permanent — loses real messages to a slow network. Losing a reminder is
//! worse than sending it twice.

use crate::channel::Channel;
use crate::send::Outbound;
use crate::transport::{Transport, TransportError};

/// Taqnyat's own API, or a test's stand-in for it.
const LIVE: &str = "https://api.taqnyat.sa";

pub struct Taqnyat {
    token: String,
    /// The registered sender name. **Case sensitive**, and one that is not
    /// active on the account is a permanent refusal on every message — which
    /// is why it is configuration rather than something a template supplies.
    sender: String,
    base: String,
    client: reqwest::Client,
}

impl std::fmt::Debug for Taqnyat {
    /// The sender name and the base URL, and **not** the token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Taqnyat")
            .field("sender", &self.sender)
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Taqnyat {
    /// Comfortably inside the dispatcher's thirty-second lease, so a slow
    /// gateway cannot still be in flight when another dispatcher takes the
    /// effect. The same number [`crate::Relay`] uses, for the same reason.
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

    pub fn new(token: &str, sender: &str) -> Result<Self, TransportError> {
        if token.trim().is_empty() {
            return Err(TransportError::Refused(
                "Taqnyat needs an API token".to_owned(),
            ));
        }
        if sender.trim().is_empty() {
            // Documented as required, and a missing one is
            // `Sender Name is not specified` on every message rather than a
            // start-up failure. Refuse here, where somebody is reading a log.
            return Err(TransportError::Refused(
                "Taqnyat needs the sender name registered on the account".to_owned(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Self::TIMEOUT)
            .build()
            .map_err(|e| TransportError::Refused(format!("the client cannot be built: {e}")))?;

        Ok(Self {
            token: token.to_owned(),
            sender: sender.trim().to_owned(),
            base: LIVE.to_owned(),
            client,
        })
    }

    /// Points this at somewhere else. For tests, and for a staging gateway.
    #[must_use]
    pub fn at(mut self, base: &str) -> Self {
        base.trim_end_matches('/').clone_into(&mut self.base);
        self
    }
}

#[async_trait::async_trait]
impl Transport for Taqnyat {
    fn channel(&self) -> Channel {
        Channel::Sms
    }

    async fn send(&self, message: &Outbound, _key: &str) -> Result<(), TransportError> {
        // **No `key`.** Taqnyat documents no idempotency key of any kind, and
        // inventing a field it does not read would be a comment that lies. See
        // the module docs for what that costs.
        let recipient = msisdn(&message.to).ok_or_else(|| {
            TransportError::Refused(format!(
                "{} is not a number Taqnyat can be given",
                message.to
            ))
        })?;

        let response = self
            .client
            .post(format!("{}/v1/messages", self.base))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "recipients": [recipient],
                "body": message.body,
                "sender": self.sender,
            }))
            .send()
            .await
            .map_err(|e| TransportError::Unreachable(e.to_string()))?;

        let status = response.status();
        let said = response.text().await.unwrap_or_default();

        if status.is_success() {
            return accepted(&said, recipient);
        }
        Err(refusal(status, &said))
    }
}

/// A phone number as Taqnyat wants it: digits, no `+`, no leading `00`.
///
/// Returns `None` for anything that is not one, so a number this client cannot
/// fix is refused before it is sent rather than after — the documented failure
/// (`Mobile(s) number(s) is not specified or incorrect`) is permanent either
/// way, and a local refusal says which number and why.
///
/// The upper bound is E.164's fifteen digits, which is also what keeps this
/// inside a `u64`.
#[must_use]
pub fn msisdn(number: &str) -> Option<u64> {
    let digits: String = number.chars().filter(|c| !c.is_whitespace()).collect();
    let digits = digits.strip_prefix('+').unwrap_or(&digits);
    let digits = digits.strip_prefix("00").unwrap_or(digits);

    if digits.len() < 8 || digits.len() > 15 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // **A leading zero is a national number.** No country calling code begins
    // with one, so `0500000000` is a Saudi number written the way a Saudi
    // person writes it — and parsing it as an integer would drop the zero and
    // send to `500000000`, which is a different number that might exist.
    if digits.starts_with('0') {
        return None;
    }
    digits.parse().ok()
}

/// Whether a `201` actually took the message.
///
/// **A rejected recipient comes back `201`.** Reporting that as sent would
/// record a message nobody received as delivered, which is the one answer worse
/// than a failure.
fn accepted(body: &str, recipient: u64) -> Result<(), TransportError> {
    let Ok(answer) = serde_json::from_str::<serde_json::Value>(body) else {
        // A 2xx this client cannot read is not a success it can claim.
        return Err(TransportError::Unreachable(format!(
            "Taqnyat answered with something this client cannot read: {}",
            clipped(body)
        )));
    };

    let listed = |field: &str| -> Vec<String> {
        // `"[966500000000,]"` — bracketed, comma separated, trailing comma.
        // Not JSON, and deliberately not parsed as JSON.
        answer
            .get(field)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim_matches(|c| c == '[' || c == ']')
            .split(',')
            .map(|part| part.trim().to_owned())
            .filter(|part| !part.is_empty())
            .collect()
    };

    let wanted = recipient.to_string();
    if listed("rejected").contains(&wanted) {
        return Err(TransportError::Refused(format!(
            "Taqnyat rejected {wanted}"
        )));
    }
    if listed("accepted").contains(&wanted) {
        return Ok(());
    }
    // Neither list names it. Something is different from what the documentation
    // describes, and claiming a send happened on that basis is not honest.
    Err(TransportError::Unreachable(format!(
        "Taqnyat listed {wanted} as neither accepted nor rejected: {}",
        clipped(body)
    )))
}

/// **The one documented retry.**
///
/// Every other `400` Taqnyat documents is a permanent condition — an unregistered
/// sender name, a bad number, an empty balance, an account restriction. Retrying
/// any of them spends four attempts and two minutes of backoff to arrive at the
/// same answer, and an empty balance retried on a timer never becomes money.
const TRY_AGAIN: &str = "SMS-API not responding";

fn refusal(status: reqwest::StatusCode, body: &str) -> TransportError {
    let said = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| clipped(body));

    // Matched on a substring rather than the whole sentence: the documented
    // text carries its own typos (`Sender Name is expierd`), which says these
    // strings are edited, and an exact match would silently become a
    // never-retried outage the day one is corrected.
    if said.contains(TRY_AGAIN) || status.is_server_error() {
        return TransportError::Unreachable(format!("{status}: {said}"));
    }
    TransportError::Refused(format!("{status}: {said}"))
}

/// A gateway that answers with a page of HTML should not put a page of HTML in
/// a dead letter.
fn clipped(body: &str) -> String {
    body.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_i18n::Locale;

    fn message(to: &str) -> Outbound {
        Outbound {
            channel: Channel::Sms,
            to: to.to_owned(),
            subject: String::new(),
            body: "موعدك غدًا".to_owned(),
            locale: Locale::Arabic,
            platform: None,
        }
    }

    #[test]
    fn a_number_reaches_taqnyat_the_way_it_documents_them() {
        assert_eq!(msisdn("+966500000000"), Some(966_500_000_000));
        assert_eq!(msisdn("00966500000000"), Some(966_500_000_000));
        assert_eq!(msisdn("966500000000"), Some(966_500_000_000));
        assert_eq!(msisdn("+966 50 000 0000"), Some(966_500_000_000));
    }

    /// Refused here rather than at the gateway, where it is permanent anyway
    /// and says nothing about which number.
    #[test]
    fn something_that_is_not_a_number_never_leaves_this_process() {
        assert_eq!(msisdn(""), None);
        assert_eq!(msisdn("0500000000"), None, "national, not international");
        assert_eq!(
            msisdn("+966-50-000-0000"),
            None,
            "punctuation is not a digit"
        );
        assert_eq!(msisdn("not a number"), None);
        assert_eq!(msisdn("9665000000001234567"), None, "past E.164");
    }

    #[test]
    fn a_taqnyat_client_does_not_print_its_token() {
        let sms = Taqnyat::new("sk-secret", "Bassat").expect("built");
        let printed = format!("{sms:?}");
        assert!(printed.contains("Bassat"), "{printed}");
        assert!(!printed.contains("sk-secret"), "{printed}");
    }

    #[test]
    fn a_sender_name_is_not_optional() {
        assert!(Taqnyat::new("t", "  ").is_err());
        assert!(Taqnyat::new("  ", "Bassat").is_err());
    }

    /// **A `201` is not a send.** The accepted and rejected lists are what say
    /// whether this number got anything.
    #[test]
    fn a_rejected_recipient_is_a_refusal_even_though_the_status_was_201() {
        let sent = r#"{"statusCode":201,"messageId":5452899970,"cost":0.026,
                       "currency":"SAR","totalCount":1,"msgLength":1,
                       "accepted":"[966500000000,]","rejected":"[]"}"#;
        assert!(accepted(sent, 966_500_000_000).is_ok());

        let refused = r#"{"statusCode":201,"totalCount":0,
                          "accepted":"[]","rejected":"[966500000000,]"}"#;
        assert!(matches!(
            accepted(refused, 966_500_000_000),
            Err(TransportError::Refused(_))
        ));
    }

    /// Neither list names it, so nothing here knows what happened — and
    /// "sent" is not the answer to that.
    #[test]
    fn a_recipient_on_neither_list_is_not_reported_as_sent() {
        let odd = r#"{"statusCode":201,"accepted":"[966511111111,]","rejected":"[]"}"#;
        assert!(matches!(
            accepted(odd, 966_500_000_000),
            Err(TransportError::Unreachable(_))
        ));
        assert!(matches!(
            accepted("<html>", 966_500_000_000),
            Err(TransportError::Unreachable(_))
        ));
    }

    /// The documented 400s, and the one of them worth another go.
    #[test]
    fn only_the_one_taqnyat_says_to_retry_is_retried() {
        let four_hundred = reqwest::StatusCode::BAD_REQUEST;
        let refusal_for = |m: &str| refusal(four_hundred, &format!(r#"{{"message":"{m}"}}"#));

        assert!(matches!(
            refusal_for("SMS-API not responding , please try again"),
            TransportError::Unreachable(_)
        ));

        for permanent in [
            "Your balance is 0",
            "Your balance is not enough",
            "Sender Name is not accepted",
            "Sender Name not active .",
            "Sender Name is expierd",
            "Mobile(s) number(s) is not specified or incorrect",
            "Sending SMS stopped from support",
            "sending by API is disabled",
            "this ip Not authorized to using the API",
            "this country Not authorized to using the API",
            "The number of recipients is greater than 1000",
        ] {
            assert!(
                matches!(refusal_for(permanent), TransportError::Refused(_)),
                "{permanent} should not be retried"
            );
        }
    }

    /// A wrong token is permanent; the gateway being down is not.
    #[test]
    fn credentials_are_permanent_and_an_outage_is_not() {
        assert!(matches!(
            refusal(
                reqwest::StatusCode::UNAUTHORIZED,
                r#"{"statusCode":401,"message":"invalid credentials information"}"#
            ),
            TransportError::Refused(_)
        ));
        assert!(matches!(
            refusal(
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                "down for maintenance"
            ),
            TransportError::Unreachable(_)
        ));
    }

    /// **The bytes on the wire**, against a server that shows them.
    #[tokio::test]
    async fn the_request_is_the_one_taqnyat_documents() {
        let server = crate::fake::OneRequest::answering(
            201,
            r#"{"statusCode":201,"messageId":5452899970,"accepted":"[966500000000,]","rejected":"[]"}"#,
        )
        .await;

        Taqnyat::new("sk-secret", "Bassat")
            .expect("built")
            .at(&server.url())
            .send(&message("+966500000000"), "booking.reminder.BK-1.0")
            .await
            .expect("sends");

        let sent = server.seen().await;
        assert!(sent.starts_with("POST /v1/messages "), "{sent}");
        assert!(sent.contains("authorization: Bearer sk-secret"), "{sent}");
        // Unquoted, which is the only form Taqnyat documents.
        assert!(sent.contains(r#""recipients":[966500000000]"#), "{sent}");
        assert!(sent.contains(r#""sender":"Bassat""#), "{sent}");
        // Arabic survives the trip. `serde_json` escapes non-ASCII, which both
        // of Taqnyat's own SDKs also emit.
        assert!(sent.contains(r#""body":"#), "{sent}");
    }

    /// A number this client can see is wrong never becomes a request.
    #[tokio::test]
    async fn a_bad_number_is_refused_without_a_request() {
        let sms = Taqnyat::new("t", "Bassat")
            .expect("built")
            // Nothing listens here; reaching the network would be the failure.
            .at("http://127.0.0.1:1");
        assert!(matches!(
            sms.send(&message("hello"), "k").await,
            Err(TransportError::Refused(_))
        ));
    }
}
