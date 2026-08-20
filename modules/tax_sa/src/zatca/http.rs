//! The client that actually reaches ZATCA.
//!
//! # Why the module holds it, and why that is still not domain code doing I/O
//!
//! Six endpoints on one host, and the host, the headers and the shapes are all
//! Saudi facts — they belong beside the rest of them. What keeps D9 true is
//! *when* this runs: never inside a command, never inside a projection, only
//! from a sweep, which is the boundary this system already has for talking to
//! the outside world.
//!
//! # What every call has in common
//!
//! ```text
//!   accept-version: V2            ZATCA's API version, not ours
//!   Accept-Language: en           the language of the validation messages
//!   Content-Type: application/json
//!   Accept: application/json
//! ```
//!
//! and one of:
//!
//! ```text
//!   OTP: 123456                   onboarding, from the taxpayer's portal
//!   Authorization: Basic …        everything after it, from a CSID
//!   Clearance-Status: 0 | 1       which of the two obligations this is
//! ```
//!
//! # Failing to ask is not a verdict
//!
//! Every error out of here is [`Unanswered`], which callers treat as "nothing
//! was decided": the document stays pending, the onboarding is not half done,
//! and the next sweep tries again. A refusal — an answer that says no — comes
//! back as `Ok`, because it is one.

use std::time::Duration;

use super::csr::Environment;
use super::onboarding::{ComplianceRequest, Csid, CsidResponse, Otp, ProductionRequest, Registrar};
use super::wire::{Endpoint, Submission, Submitter, Unanswered, Verdict};

/// How long one call may take.
///
/// Clearance is **synchronous and in front of a person**: a standard invoice
/// cannot be given to the buyer until it comes back, so a sweep that blocks for
/// a minute on one document is a sale that stalls. Thirty seconds is ZATCA's own
/// stated ceiling for a clearance response.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A client for one ZATCA environment.
#[derive(Debug, Clone)]
pub struct Fatoora {
    client: reqwest::Client,
    environment: Environment,
    /// The production CSID, for [`Submitter`]. Onboarding does not need it —
    /// that is what the OTP and the compliance certificate are for — so a
    /// client can exist before there is one.
    credentials: Option<Csid>,
    /// Where the calls go, when it is not where [`Environment`] says.
    ///
    /// For tests, which point it at a local socket, and for a deployment that
    /// egresses through a proxy. `None` is the normal case and means ZATCA's
    /// own address — a client that had to be told its authority's hostname
    /// would be one a typo could redirect.
    base_url: Option<String>,
}

impl Fatoora {
    /// A client with this build's timeouts.
    ///
    /// Cheap to clone and meant to be — the connection pool is inside, so one
    /// per environment per process rather than one per call.
    pub fn new(environment: Environment) -> Result<Self, Unanswered> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("erp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Unanswered::Unavailable(e.to_string()))?;

        Ok(Self {
            client,
            environment,
            credentials: None,
            base_url: None,
        })
    }

    /// The same client, able to submit invoices.
    #[must_use]
    pub fn with_credentials(mut self, credentials: Csid) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// The same client, pointed somewhere else. See [`Self::base_url`].
    #[must_use]
    pub fn at(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    fn url(&self, path: &str) -> String {
        let base = self
            .base_url
            .as_deref()
            .unwrap_or_else(|| self.environment.base_url());
        format!("{base}{path}")
    }

    /// The headers every call carries.
    fn request(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .post(self.url(path))
            .header("accept-version", "V2")
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::ACCEPT_LANGUAGE, "en")
    }

    /// Sends it, and reads the status and the body — **without deciding what
    /// they mean**. That is [`Verdict::of`]'s job for invoices and
    /// [`CsidResponse`]'s for certificates, and both are tested against recorded
    /// answers rather than against a live service.
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<(u16, String), Unanswered> {
        let response = request.send().await.map_err(|e| {
            if e.is_timeout() {
                Unanswered::Unavailable(format!("timed out after {}s", TIMEOUT.as_secs()))
            } else if e.is_connect() {
                Unanswered::Unavailable(format!("could not connect: {e}"))
            } else {
                Unanswered::Unavailable(e.to_string())
            }
        })?;

        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| Unanswered::Unreadable(format!("the body could not be read: {e}")))?;

        Ok((status, body))
    }

    /// A certificate response, whatever the HTTP status was.
    ///
    /// ZATCA answers `400` with a body that explains why, and that body is the
    /// useful part — so it is parsed at every status except the ones that say
    /// the credentials themselves are wrong.
    fn certificate_answer(status: u16, body: &str) -> Result<CsidResponse, Unanswered> {
        match status {
            401 | 403 => return Err(Unanswered::NotOnboarded { status }),
            500..=599 => return Err(Unanswered::Unavailable(format!("HTTP {status}: {body}"))),
            _ => {}
        }
        serde_json::from_str(body).map_err(|e| Unanswered::Unreadable(format!("{e}: {body}")))
    }
}

#[async_trait::async_trait]
impl Registrar for Fatoora {
    async fn compliance_csid(
        &self,
        environment: Environment,
        otp: &Otp,
        request: &ComplianceRequest,
    ) -> Result<CsidResponse, Unanswered> {
        let client = self.for_environment(environment)?;
        let (status, body) = client
            .send(
                client
                    .request("/compliance")
                    // The only place this value is ever put on a wire.
                    .header("OTP", otp.header())
                    .json(request),
            )
            .await?;
        Self::certificate_answer(status, &body)
    }

    async fn check_compliance(
        &self,
        environment: Environment,
        compliance: &Csid,
        submission: &Submission,
    ) -> Result<Verdict, Unanswered> {
        let client = self.for_environment(environment)?;
        let (status, body) = client
            .send(
                client
                    .request("/compliance/invoices")
                    .header(reqwest::header::AUTHORIZATION, compliance.authorization())
                    .json(submission),
            )
            .await?;
        Verdict::of(status, &body)
    }

    async fn production_csid(
        &self,
        environment: Environment,
        compliance: &Csid,
        request: &ProductionRequest,
    ) -> Result<CsidResponse, Unanswered> {
        let client = self.for_environment(environment)?;
        let (status, body) = client
            .send(
                client
                    .request("/production/csids")
                    .header(reqwest::header::AUTHORIZATION, compliance.authorization())
                    .json(request),
            )
            .await?;
        Self::certificate_answer(status, &body)
    }

    async fn renew_csid(
        &self,
        environment: Environment,
        production: &Csid,
        otp: &Otp,
        request: &ComplianceRequest,
    ) -> Result<CsidResponse, Unanswered> {
        let client = self.for_environment(environment)?;
        // **PATCH**, not POST: a renewal replaces a certificate rather than
        // asking for another one.
        let (status, body) = client
            .send(
                client
                    .client
                    .patch(client.url("/production/csids"))
                    .header("accept-version", "V2")
                    .header(reqwest::header::ACCEPT, "application/json")
                    .header(reqwest::header::ACCEPT_LANGUAGE, "en")
                    .header("OTP", otp.header())
                    .header(reqwest::header::AUTHORIZATION, production.authorization())
                    .json(request),
            )
            .await?;
        Self::certificate_answer(status, &body)
    }
}

#[async_trait::async_trait]
impl Submitter for Fatoora {
    async fn submit(
        &self,
        endpoint: Endpoint,
        submission: &Submission,
    ) -> Result<Verdict, Unanswered> {
        let credentials = self
            .credentials
            .as_ref()
            .ok_or(Unanswered::NotOnboarded { status: 401 })?;

        let (status, body) = self
            .send(
                self.request(endpoint.path())
                    .header(reqwest::header::AUTHORIZATION, credentials.authorization())
                    // Which of the two obligations, stated twice — once in the
                    // path and once here, because ZATCA reads both.
                    .header("Clearance-Status", endpoint.clearance_status())
                    .json(submission),
            )
            .await?;

        Verdict::of(status, &body)
    }
}

impl Fatoora {
    /// The same client, refusing an environment it was not built for.
    ///
    /// A client built for the sandbox being handed a production certificate is
    /// a mistake that otherwise succeeds against the wrong authority — the same
    /// hazard `Environment` exists to make visible.
    fn for_environment(&self, environment: Environment) -> Result<&Self, Unanswered> {
        if environment == self.environment {
            return Ok(self);
        }
        Err(Unanswered::Unavailable(format!(
            "this client is for {} and the call is for {}",
            self.environment.as_str(),
            environment.as_str()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-request HTTP server, so the client can be tested without ZATCA.
    ///
    /// Hand-written rather than a framework: the point is to see the **exact
    /// bytes** the client sends, which a framework would parse away.
    struct OneRequest {
        address: std::net::SocketAddr,
        handle: tokio::task::JoinHandle<String>,
    }

    impl OneRequest {
        /// Answers the first request with this status and body, and returns
        /// what it was sent.
        async fn answering(status: u16, body: &'static str) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("binds");
            let address = listener.local_addr().expect("an address");

            let handle = tokio::spawn(async move {
                use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                let (mut socket, _) = listener.accept().await.expect("accepts");

                let mut seen = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = socket.read(&mut buffer).await.expect("reads");
                    seen.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&seen);
                    // Headers, then a body as long as `Content-Length` says.
                    if let Some(head) = text.find("\r\n\r\n") {
                        let length: usize = text
                            .to_lowercase()
                            .split("content-length:")
                            .nth(1)
                            .and_then(|rest| rest.split("\r\n").next())
                            .and_then(|value| value.trim().parse().ok())
                            .unwrap_or(0);
                        if seen.len() >= head + 4 + length || read == 0 {
                            break;
                        }
                    }
                    if read == 0 {
                        break;
                    }
                }

                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.expect("writes");
                socket.flush().await.expect("flushes");
                String::from_utf8_lossy(&seen).into_owned()
            });

            Self { address, handle }
        }

        fn url(&self) -> String {
            format!("http://{}", self.address)
        }

        async fn seen(self) -> String {
            self.handle.await.expect("the server finished")
        }
    }

    /// A client pointed at a local address rather than at ZATCA.
    fn pointed_at(url: &str) -> Fatoora {
        Fatoora::new(Environment::Sandbox)
            .expect("a client")
            .at(url)
    }

    fn otp() -> Otp {
        "123456".parse().expect("six digits")
    }

    /// **The OTP reaches ZATCA in a header, and the CSR in the body.**
    #[tokio::test]
    async fn a_compliance_request_carries_the_otp_and_the_csr() {
        let server = OneRequest::answering(
            200,
            r#"{"requestID":123,"dispositionMessage":"ISSUED",
                "binarySecurityToken":"dG9rZW4=","secret":"c2VjcmV0"}"#,
        )
        .await;
        let client = pointed_at(&server.url());

        let answer = client
            .compliance_csid(
                Environment::Sandbox,
                &otp(),
                &ComplianceRequest {
                    csr: "Q1NS".to_owned(),
                },
            )
            .await
            .expect("answers");
        let csid = answer.issued().expect("issued");
        assert_eq!(csid.token, "dG9rZW4=");
        assert_eq!(csid.request_id, "123");

        let sent = server.seen().await;
        assert!(sent.starts_with("POST /compliance "), "{sent}");
        assert!(
            sent.contains("otp: 123456") || sent.contains("OTP: 123456"),
            "{sent}"
        );
        assert!(sent.contains("accept-version: V2"), "{sent}");
        assert!(sent.ends_with(r#"{"csr":"Q1NS"}"#), "{sent}");
    }

    /// The production call authenticates as the compliance certificate.
    #[tokio::test]
    async fn a_production_request_authenticates_with_the_compliance_csid() {
        let server = OneRequest::answering(
            200,
            r#"{"requestID":"456","dispositionMessage":"ISSUED",
                "binarySecurityToken":"cHJvZA==","secret":"cw=="}"#,
        )
        .await;
        let client = pointed_at(&server.url());

        let compliance = Csid {
            token: "dG9rZW4=".to_owned(),
            secret: "c2VjcmV0".to_owned(),
            request_id: "123".to_owned(),
        };
        client
            .production_csid(
                Environment::Sandbox,
                &compliance,
                &ProductionRequest {
                    compliance_request_id: "123".to_owned(),
                },
            )
            .await
            .expect("answers")
            .issued()
            .expect("issued");

        let sent = server.seen().await;
        assert!(sent.starts_with("POST /production/csids "), "{sent}");
        assert!(
            sent.to_lowercase()
                .contains(&compliance.authorization().to_lowercase()),
            "{sent}"
        );
        assert!(
            sent.contains(r#"{"compliance_request_id":"123"}"#),
            "{sent}"
        );
        // The OTP belongs to onboarding, not to this call.
        assert!(!sent.to_lowercase().contains("otp:"), "{sent}");
    }

    /// **The header that says which obligation this is.**
    #[tokio::test]
    async fn a_submission_says_whether_it_is_clearance_or_reporting() {
        for (endpoint, expected, path) in [
            (Endpoint::Clearance, "1", "POST /invoices/clearance/single "),
            (Endpoint::Reporting, "0", "POST /invoices/reporting/single "),
        ] {
            let server = OneRequest::answering(
                200,
                r#"{"clearanceStatus":"CLEARED","validationResults":{}}"#,
            )
            .await;
            let client = pointed_at(&server.url()).with_credentials(Csid {
                token: "t".to_owned(),
                secret: "s".to_owned(),
                request_id: "1".to_owned(),
            });

            client
                .submit(
                    endpoint,
                    &Submission {
                        invoice_hash: "aGFzaA==".to_owned(),
                        uuid: "an-id".to_owned(),
                        invoice: "PGludm9pY2Uv==".to_owned(),
                    },
                )
                .await
                .expect("answers");

            let sent = server.seen().await;
            assert!(sent.starts_with(path), "{sent}");
            assert!(
                sent.to_lowercase()
                    .contains(&format!("clearance-status: {expected}")),
                "{sent}"
            );
            assert!(sent.contains(r#""invoiceHash":"aGFzaA==""#), "{sent}");
        }
    }

    /// Submitting with no credentials is refused before a connection is opened.
    #[tokio::test]
    async fn submitting_without_credentials_never_reaches_the_network() {
        let client = pointed_at("http://127.0.0.1:1");
        let refused = client
            .submit(
                Endpoint::Reporting,
                &Submission {
                    invoice_hash: "h".to_owned(),
                    uuid: "u".to_owned(),
                    invoice: "i".to_owned(),
                },
            )
            .await
            .expect_err("there is nothing to authenticate with");
        assert!(matches!(refused, Unanswered::NotOnboarded { status: 401 }));
    }

    /// A refusal is an answer; a connection that fails is not.
    #[tokio::test]
    async fn a_connection_that_fails_is_not_a_verdict() {
        // Port 1 on loopback: nothing listens, and the connection is refused
        // rather than hanging.
        let client = pointed_at("http://127.0.0.1:1").with_credentials(Csid {
            token: "t".to_owned(),
            secret: "s".to_owned(),
            request_id: "1".to_owned(),
        });

        let failed = client
            .submit(
                Endpoint::Reporting,
                &Submission {
                    invoice_hash: "h".to_owned(),
                    uuid: "u".to_owned(),
                    invoice: "i".to_owned(),
                },
            )
            .await
            .expect_err("nothing answered");
        assert!(
            matches!(failed, Unanswered::Unavailable(_)),
            "a failure to connect became {failed:?}"
        );
    }

    /// A 401 is about the credentials, and every document would fail the same
    /// way — so it is not a verdict on any of them.
    #[tokio::test]
    async fn an_unauthorised_certificate_request_is_not_a_refusal() {
        let server = OneRequest::answering(401, "unauthorized").await;
        let client = pointed_at(&server.url());

        let failed = client
            .compliance_csid(
                Environment::Sandbox,
                &otp(),
                &ComplianceRequest {
                    csr: "Q1NS".to_owned(),
                },
            )
            .await
            .expect_err("401 is not an answer about the request");
        assert!(matches!(failed, Unanswered::NotOnboarded { status: 401 }));
        let _ = server.seen().await;
    }

    /// ZATCA explains a rejection in the body of a 400, and that body is the
    /// useful part.
    #[tokio::test]
    async fn a_rejected_certificate_request_is_read_rather_than_discarded() {
        let server = OneRequest::answering(
            400,
            r#"{"dispositionMessage":"REJECTED","errors":["the OTP has expired"]}"#,
        )
        .await;
        let client = pointed_at(&server.url());

        let answer = client
            .compliance_csid(
                Environment::Sandbox,
                &otp(),
                &ComplianceRequest {
                    csr: "Q1NS".to_owned(),
                },
            )
            .await
            .expect("a 400 with a body is an answer");
        let refused = answer.issued().expect_err("not a certificate");
        assert!(refused.to_string().contains("expired"), "{refused}");
        let _ = server.seen().await;
    }

    /// A client for one environment refuses a call for another, because the
    /// mistake otherwise succeeds against the wrong authority.
    #[tokio::test]
    async fn a_client_refuses_a_call_for_another_environment() {
        let client = pointed_at("http://127.0.0.1:1");
        let refused = client
            .compliance_csid(
                Environment::Production,
                &otp(),
                &ComplianceRequest {
                    csr: "Q1NS".to_owned(),
                },
            )
            .await
            .expect_err("a sandbox client cannot onboard into production");
        assert!(matches!(refused, Unanswered::Unavailable(_)), "{refused:?}");
    }

    #[test]
    fn the_urls_are_the_ones_zatca_publishes() {
        let production = Fatoora::new(Environment::Production).expect("a client");
        assert_eq!(
            production.url("/compliance"),
            "https://gw-fatoora.zatca.gov.sa/e-invoicing/core/compliance"
        );
        let simulation = Fatoora::new(Environment::Simulation).expect("a client");
        assert!(
            simulation
                .url("/invoices/clearance/single")
                .contains("/simulation/")
        );
    }
}
