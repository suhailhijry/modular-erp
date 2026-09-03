//! Push through Firebase Cloud Messaging.
//!
//! # The legacy API is gone, which is why this file is long
//!
//! `POST fcm.googleapis.com/fcm/send` with `Authorization: key=<server key>`
//! was deprecated in June 2023 and shut down from July 2024. Every tutorial
//! that shows it is wrong now. HTTP v1 authenticates with a **short-lived
//! OAuth 2.0 access token**, minted by signing a JWT with a service account's
//! private key and exchanging it at Google's token endpoint.
//!
//! That exchange is what [`Fcm::access_token`] does, and it is the reason this
//! adapter is not thirty lines. Google's own documentation says to use their
//! client library instead. Their library is not available to this build without
//! a large dependency tree and a second TLS stack, and the flow is one signed
//! assertion over a POST — so it is written here, and
//! [`the_assertion_is_a_jwt_google_would_accept`] verifies the signature it
//! produces against the public half of the key that signed it.
//!
//! [`the_assertion_is_a_jwt_google_would_accept`]: tests::the_assertion_is_a_jwt_google_would_accept
//!
//! # A dead token is the one failure that gets written down
//!
//! `UNREGISTERED` means the app was uninstalled, reinstalled, or the platform
//! rotated the token: it will never work again and the row must stop being
//! used. That is [`TransportError::AddressRetired`], which the sweep acts on.
//!
//! **`SENDER_ID_MISMATCH` is deliberately not that.** It means the credentials
//! this process is using belong to a different Firebase project than the token
//! was issued for — which is one wrong environment variable, not a dead device.
//! Retiring on it would erase every push token a tenant has, unrecoverably,
//! because of a deployment mistake. Google's own token-management guidance
//! names only `UNREGISTERED` and `INVALID_ARGUMENT` as invalid-token signals,
//! and `INVALID_ARGUMENT` also covers "your payload is broken" — which is a bug
//! here, not a dead device. So only `UNREGISTERED` retires anything.
//!
//! # No idempotency key
//!
//! FCM has none — not a header, not a field. `collapse_key` is not one: it
//! collapses *undelivered* messages on the device, and says nothing about two
//! sends of the same message. A retried delivery can arrive twice.

use std::sync::Mutex;

use base64::Engine as _;
use serde::Deserialize;

use crate::channel::Channel;
use crate::push::Platform;
use crate::send::Outbound;
use crate::transport::{Transport, TransportError};

const LIVE: &str = "https://fcm.googleapis.com";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// The one scope FCM needs. The reference also accepts `cloud-platform`, which
/// is every Google API at once — this is the narrow one.
const SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";

/// Percent-encoded, exactly as Google's own documented request writes it.
const JWT_BEARER: &str = "urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer";

/// An access token lives an hour. Minting a new one at fifty-five minutes means
/// a token is never used inside its last five, which is the window a clock
/// skewed against Google's would otherwise fail in.
const REFRESH_MARGIN: std::time::Duration = std::time::Duration::from_mins(5);

/// The parts of a Firebase service account key this needs.
///
/// Deserialized from the JSON file the Firebase console hands out. Everything
/// else in that file is ignored.
#[derive(Clone, Deserialize)]
pub struct ServiceAccount {
    pub project_id: String,
    pub client_email: String,
    /// PKCS#8 PEM. Carried as the text it is; never printed.
    pub private_key: String,
    /// Optional. Google tries every key on the account when it is absent.
    #[serde(default)]
    pub private_key_id: Option<String>,
}

impl std::fmt::Debug for ServiceAccount {
    /// **Never the key.** A derived `Debug` puts a private key in the first log
    /// line that formats a transport.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceAccount")
            .field("project_id", &self.project_id)
            .field("client_email", &self.client_email)
            .field("private_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl ServiceAccount {
    /// Reads one out of the JSON Firebase hands out.
    pub fn parse(json: &str) -> Result<Self, TransportError> {
        let account: Self = serde_json::from_str(json).map_err(|e| {
            TransportError::Refused(format!("that is not a Firebase service account key: {e}"))
        })?;
        if account.project_id.is_empty() || account.client_email.is_empty() {
            return Err(TransportError::Refused(
                "a service account key needs project_id and client_email".to_owned(),
            ));
        }
        Ok(account)
    }
}

/// What Google's token endpoint answers with.
#[derive(Deserialize)]
struct Granted {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// An access token, and when it stops being one.
#[derive(Debug, Clone)]
struct Minted {
    token: String,
    expires: std::time::Instant,
}

pub struct Fcm {
    account: ServiceAccount,
    base: String,
    token_endpoint: String,
    client: reqwest::Client,
    /// **Not held across an await.** Two concurrent sends that both find it
    /// stale will both mint a token, and both tokens are valid — an extra
    /// round trip once an hour, against a lock that cannot deadlock a
    /// dispatcher.
    minted: Mutex<Option<Minted>>,
}

impl std::fmt::Debug for Fcm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fcm")
            .field("project", &self.account.project_id)
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Fcm {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

    pub fn new(account: ServiceAccount) -> Result<Self, TransportError> {
        // Parsed once, at start-up, rather than on the first push at 3am: a key
        // this process cannot read is a configuration failure and should read
        // like one.
        private_key(&account)?;

        let client = reqwest::Client::builder()
            .timeout(Self::TIMEOUT)
            .build()
            .map_err(|e| TransportError::Refused(format!("the client cannot be built: {e}")))?;

        Ok(Self {
            account,
            base: LIVE.to_owned(),
            token_endpoint: TOKEN_ENDPOINT.to_owned(),
            client,
            minted: Mutex::new(None),
        })
    }

    /// Points both endpoints somewhere else. For tests.
    #[must_use]
    pub fn at(mut self, base: &str) -> Self {
        let base = base.trim_end_matches('/');
        self.token_endpoint = format!("{base}/token");
        base.clone_into(&mut self.base);
        self
    }

    /// A bearer token for FCM, minted if the cached one is spent.
    async fn access_token(&self) -> Result<String, TransportError> {
        if let Some(live) = self.cached() {
            return Ok(live);
        }

        let assertion = self.assertion(std::time::SystemTime::now())?;
        let response = self
            .client
            .post(&self.token_endpoint)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            // The assertion is base64url, whose alphabet needs no escaping.
            .body(format!("grant_type={JWT_BEARER}&assertion={assertion}"))
            .send()
            .await
            .map_err(|e| TransportError::Unreachable(format!("Google's token endpoint: {e}")))?;

        let status = response.status();
        let said = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // A refused assertion is a bad key or a disabled service account —
            // permanent — but a 5xx at Google is worth another go.
            return Err(if status.is_server_error() {
                TransportError::Unreachable(format!("Google's token endpoint: {status}"))
            } else {
                TransportError::Refused(format!(
                    "Google refused this service account: {status}: {}",
                    clipped(&said)
                ))
            });
        }

        let granted: Granted = serde_json::from_str(&said).map_err(|e| {
            TransportError::Unreachable(format!("Google's token endpoint answered oddly: {e}"))
        })?;

        // An hour is what Google documents; trusting `expires_in` means a
        // shorter one is honoured without a code change.
        let lifetime = std::time::Duration::from_secs(granted.expires_in.unwrap_or(3600));
        let expires = std::time::Instant::now() + lifetime.saturating_sub(REFRESH_MARGIN);

        if let Ok(mut held) = self.minted.lock() {
            *held = Some(Minted {
                token: granted.access_token.clone(),
                expires,
            });
        }
        Ok(granted.access_token)
    }

    /// The cached token, if it is still one.
    fn cached(&self) -> Option<String> {
        let held = self.minted.lock().ok()?;
        let minted = held.as_ref()?;
        (minted.expires > std::time::Instant::now()).then(|| minted.token.clone())
    }

    /// The signed JWT Google exchanges for an access token.
    ///
    /// `now` is a parameter so a test can pin it and read the claims back.
    fn assertion(&self, now: std::time::SystemTime) -> Result<String, TransportError> {
        let issued = now
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| TransportError::Refused("this machine's clock is before 1970".to_owned()))?
            .as_secs();

        let mut header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
        if let (Some(object), Some(kid)) = (header.as_object_mut(), &self.account.private_key_id) {
            object.insert("kid".to_owned(), serde_json::Value::String(kid.clone()));
        }
        let claims = serde_json::json!({
            "iss": self.account.client_email,
            "scope": SCOPE,
            "aud": self.token_endpoint,
            // The documented maximum, and the lifetime of what comes back.
            "exp": issued + 3600,
            "iat": issued,
        });

        let signing_input = format!("{}.{}", encode(&header)?, encode(&claims)?);
        let key = private_key(&self.account)?;
        let mut signer = openssl::sign::Signer::new(openssl::hash::MessageDigest::sha256(), &key)
            .map_err(|e| TransportError::Refused(format!("cannot sign: {e}")))?;
        signer
            .update(signing_input.as_bytes())
            .map_err(|e| TransportError::Refused(format!("cannot sign: {e}")))?;
        let signature = signer
            .sign_to_vec()
            .map_err(|e| TransportError::Refused(format!("cannot sign: {e}")))?;

        Ok(format!("{signing_input}.{}", B64.encode(signature)))
    }
}

#[async_trait::async_trait]
impl Transport for Fcm {
    fn channel(&self) -> Channel {
        Channel::Push
    }

    async fn send(&self, message: &Outbound, _key: &str) -> Result<(), TransportError> {
        // **No `key`.** FCM documents no idempotency key. See the module docs.
        if let Some(platform) = message.platform {
            // A raw APNs token is not an FCM registration token, and sending it
            // would be an `INVALID_ARGUMENT` nobody could read. A deployment
            // with only this transport should not have `apns` devices at all —
            // if it does, saying so is more use than a Google error code.
            if platform == Platform::Apns {
                return Err(TransportError::Refused(
                    "this is an APNs device token and FCM cannot deliver to one; \
                     register the device through the Firebase SDK instead"
                        .to_owned(),
                ));
            }
        }

        let mut notification = serde_json::Map::new();
        notification.insert(
            "body".to_owned(),
            serde_json::Value::String(message.body.clone()),
        );
        // Only email has a subject in this system, so a push arrives with a
        // body and the app's own name above it. A title needs a template that
        // carries one; it is not something to invent here.
        if !message.subject.is_empty() {
            notification.insert(
                "title".to_owned(),
                serde_json::Value::String(message.subject.clone()),
            );
        }

        let response = self
            .client
            .post(format!(
                "{}/v1/projects/{}/messages:send",
                self.base, self.account.project_id
            ))
            .bearer_auth(self.access_token().await?)
            .json(&serde_json::json!({
                "message": {
                    "token": message.to,
                    "notification": notification,
                }
            }))
            .send()
            .await
            .map_err(|e| TransportError::Unreachable(e.to_string()))?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let said = response.text().await.unwrap_or_default();
        Err(verdict(status, &said))
    }
}

/// The private key, as OpenSSL sees it.
fn private_key(
    account: &ServiceAccount,
) -> Result<openssl::pkey::PKey<openssl::pkey::Private>, TransportError> {
    openssl::pkey::PKey::private_key_from_pem(account.private_key.as_bytes()).map_err(|e| {
        // The error, and never the key: an OpenSSL error stack is safe to
        // print, the PEM it was given is not.
        TransportError::Refused(format!(
            "the service account's private key cannot be read: {e}"
        ))
    })
}

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

fn encode(value: &serde_json::Value) -> Result<String, TransportError> {
    let json = serde_json::to_vec(value)
        .map_err(|e| TransportError::Refused(format!("cannot build the assertion: {e}")))?;
    Ok(B64.encode(json))
}

/// What FCM's answer means for this message and this token.
///
/// Keyed off the documented `error.status` enum rather than the HTTP status,
/// because that is what Google documents as stable. The HTTP status is the
/// fallback for anything unrecognised — including a `401` that is a spent
/// access token rather than a bad APNs key, which is worth another go with a
/// fresh one.
fn verdict(status: reqwest::StatusCode, body: &str) -> TransportError {
    let named = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.get("status")?.as_str().map(str::to_owned));
    let said = format!("{status}: {}", clipped(body));

    match named.as_deref() {
        // The app was uninstalled, reinstalled, or the platform rotated it.
        // The only answer that writes something down.
        Some("UNREGISTERED") => TransportError::AddressRetired(said),

        // `SENDER_ID_MISMATCH`: the credentials belong to another project — a
        // deployment mistake, and **not** a reason to erase a tenant's devices.
        // `INVALID_ARGUMENT`: either the payload is wrong, which is a bug here,
        // or the token string is malformed; Google's only discriminator is the
        // prose in the message, which is not something to retire a device on.
        // `THIRD_PARTY_AUTH_ERROR`: the APNs or web push key is wrong.
        // Configuration, not the device. See the module docs.
        Some(
            "SENDER_ID_MISMATCH"
            | "INVALID_ARGUMENT"
            | "THIRD_PARTY_AUTH_ERROR"
            | "UNSPECIFIED_ERROR",
        ) => TransportError::Refused(said),

        Some("QUOTA_EXCEEDED" | "UNAVAILABLE" | "INTERNAL") => TransportError::Unreachable(said),

        _ if status.is_server_error()
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::UNAUTHORIZED =>
        {
            TransportError::Unreachable(said)
        }
        _ => TransportError::Refused(said),
    }
}

fn clipped(body: &str) -> String {
    body.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_i18n::Locale;

    /// A real RSA key, generated here. Signing against a fixture key would
    /// prove nothing that verifying against the matching public half does not.
    fn account() -> (ServiceAccount, openssl::pkey::PKey<openssl::pkey::Public>) {
        let rsa = openssl::rsa::Rsa::generate(2048).expect("a key");
        let private = String::from_utf8(
            openssl::pkey::PKey::from_rsa(rsa.clone())
                .expect("a key")
                .private_key_to_pem_pkcs8()
                .expect("pem"),
        )
        .expect("utf-8");
        let public =
            openssl::pkey::PKey::public_key_from_pem(&rsa.public_key_to_pem().expect("pem"))
                .expect("a public key");

        (
            ServiceAccount {
                project_id: "bassat-erp".to_owned(),
                client_email: "push@bassat-erp.iam.gserviceaccount.com".to_owned(),
                private_key: private,
                private_key_id: Some("abc123".to_owned()),
            },
            public,
        )
    }

    fn message(to: &str) -> Outbound {
        Outbound {
            channel: Channel::Push,
            to: to.to_owned(),
            subject: String::new(),
            body: "موعدك غدًا".to_owned(),
            locale: Locale::Arabic,
            platform: Some(Platform::Fcm),
        }
    }

    fn decode(part: &str) -> serde_json::Value {
        serde_json::from_slice(&B64.decode(part).expect("base64url")).expect("json")
    }

    /// **The whole reason this file is long.** Three base64url parts, the
    /// claims Google documents, and a signature the matching public key
    /// verifies — which is what Google's server does with it.
    #[test]
    fn the_assertion_is_a_jwt_google_would_accept() {
        let (account, public) = account();
        let fcm = Fcm::new(account).expect("built");

        let at = std::time::UNIX_EPOCH + std::time::Duration::from_hours(500_000);
        let jwt = fcm.assertion(at).expect("signs");

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "{jwt}");
        assert!(!jwt.contains('='), "base64url is unpadded: {jwt}");

        let header = decode(parts[0]);
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");
        assert_eq!(header["kid"], "abc123");

        let claims = decode(parts[1]);
        assert_eq!(claims["iss"], "push@bassat-erp.iam.gserviceaccount.com");
        assert_eq!(claims["scope"], SCOPE);
        assert_eq!(claims["aud"], TOKEN_ENDPOINT);
        assert_eq!(claims["iat"], 1_800_000_000_u64);
        // The documented maximum, and not a second more.
        assert_eq!(claims["exp"], 1_800_003_600_u64);

        let mut verifier =
            openssl::sign::Verifier::new(openssl::hash::MessageDigest::sha256(), &public)
                .expect("a verifier");
        verifier
            .update(format!("{}.{}", parts[0], parts[1]).as_bytes())
            .expect("updates");
        assert!(
            verifier
                .verify(&B64.decode(parts[2]).expect("base64url"))
                .expect("verifies"),
            "the signature does not match the key that made it"
        );
    }

    /// A key this process cannot read is a start-up failure, not a 3am one.
    #[test]
    fn an_unreadable_key_is_refused_when_the_transport_is_built() {
        let (mut account, _) = account();
        account.private_key = "-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----".into();
        assert!(matches!(Fcm::new(account), Err(TransportError::Refused(_))));
    }

    #[test]
    fn a_service_account_never_prints_its_key() {
        let (account, _) = account();
        let printed = format!("{account:?}");
        assert!(printed.contains("bassat-erp"), "{printed}");
        assert!(!printed.contains("PRIVATE KEY"), "{printed}");

        let printed = format!("{:?}", Fcm::new(account).expect("built"));
        assert!(!printed.contains("PRIVATE KEY"), "{printed}");
    }

    #[test]
    fn a_service_account_key_is_read_out_of_the_json_firebase_hands_out() {
        let json = r#"{"type":"service_account","project_id":"p","private_key_id":"k",
                       "private_key":"-----BEGIN PRIVATE KEY-----\nx\n-----END PRIVATE KEY-----\n",
                       "client_email":"a@b.iam.gserviceaccount.com","client_id":"1"}"#;
        let account = ServiceAccount::parse(json).expect("parses");
        assert_eq!(account.project_id, "p");
        assert_eq!(account.private_key_id.as_deref(), Some("k"));
        // The `\n` escapes are real newlines by the time OpenSSL sees them.
        assert!(account.private_key.contains('\n'));

        assert!(ServiceAccount::parse("{}").is_err());
        assert!(ServiceAccount::parse("not json").is_err());
    }

    /// **The one that erases data if it is wrong.** Only `UNREGISTERED`
    /// retires a device.
    #[test]
    fn only_an_unregistered_token_is_retired() {
        let fcm_said = |status: u16, code: &str| {
            verdict(
                reqwest::StatusCode::from_u16(status).expect("a status"),
                &format!(r#"{{"error":{{"code":{status},"status":"{code}"}}}}"#),
            )
        };

        assert!(matches!(
            fcm_said(404, "UNREGISTERED"),
            TransportError::AddressRetired(_)
        ));

        // A wrong service account in an environment variable must not erase
        // every push token a tenant has.
        assert!(matches!(
            fcm_said(403, "SENDER_ID_MISMATCH"),
            TransportError::Refused(_)
        ));
        assert!(matches!(
            fcm_said(400, "INVALID_ARGUMENT"),
            TransportError::Refused(_)
        ));
        assert!(matches!(
            fcm_said(401, "THIRD_PARTY_AUTH_ERROR"),
            TransportError::Refused(_)
        ));

        for transient in [
            (429, "QUOTA_EXCEEDED"),
            (503, "UNAVAILABLE"),
            (500, "INTERNAL"),
        ] {
            assert!(
                matches!(
                    fcm_said(transient.0, transient.1),
                    TransportError::Unreachable(_)
                ),
                "{transient:?} is worth another go"
            );
        }
    }

    /// A `401` that names nothing is a spent access token, and the next attempt
    /// mints a fresh one.
    #[test]
    fn an_unexplained_401_is_retried_rather_than_dead_lettered() {
        assert!(matches!(
            verdict(reqwest::StatusCode::UNAUTHORIZED, "<html>"),
            TransportError::Unreachable(_)
        ));
        assert!(matches!(
            verdict(reqwest::StatusCode::BAD_REQUEST, "<html>"),
            TransportError::Refused(_)
        ));
    }

    /// An APNs token is refused with a sentence, rather than sent and returned
    /// as an unreadable Google error.
    #[tokio::test]
    async fn an_apns_token_is_not_offered_to_fcm() {
        let (account, _) = account();
        let fcm = Fcm::new(account).expect("built").at("http://127.0.0.1:1");

        let mut apns = message("device-1");
        apns.platform = Some(Platform::Apns);
        assert!(matches!(
            fcm.send(&apns, "k").await,
            Err(TransportError::Refused(_))
        ));
    }

    /// **Both requests, in order**, against a server that shows their bytes.
    #[tokio::test]
    async fn the_token_is_exchanged_and_then_the_message_is_sent() {
        let server = crate::fake::OneRequest::sequence(vec![
            (
                200,
                r#"{"access_token":"ya29.granted","expires_in":3600,"token_type":"Bearer"}"#,
            ),
            (
                200,
                r#"{"name":"projects/bassat-erp/messages/0:1500415314455276"}"#,
            ),
        ])
        .await;

        let (account, _) = account();
        let fcm = Fcm::new(account).expect("built").at(&server.url());
        fcm.send(&message("device-token-1"), "booking.reminder.BK-1.0")
            .await
            .expect("sends");

        let sent = server.seen().await;
        let (exchange, message) = sent.split_once("\n===\n").expect("two requests");

        assert!(exchange.starts_with("POST /token "), "{exchange}");
        assert!(
            exchange.contains("content-type: application/x-www-form-urlencoded"),
            "{exchange}"
        );
        assert!(
            exchange.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"),
            "{exchange}"
        );
        assert!(exchange.contains("&assertion=eyJ"), "{exchange}");

        assert!(
            message.starts_with("POST /v1/projects/bassat-erp/messages:send "),
            "{message}"
        );
        assert!(
            message.contains("authorization: Bearer ya29.granted"),
            "{message}"
        );
        assert!(message.contains(r#""token":"device-token-1""#), "{message}");
        assert!(message.contains(r#""notification":{"body":"#), "{message}");
        // Only email has a subject, so a push carries no title.
        assert!(!message.contains(r#""title""#), "{message}");
    }
}
