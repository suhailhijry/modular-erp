//! **Proving a tenant owns a domain**, and what may stand on one.
//!
//! # The proof
//!
//! A tenant that wants their own domain on this API — for the browsers that may
//! call it (CORS) and, once proved, as the host the API answers on — publishes
//! one DNS record:
//!
//! ```text
//!   _erp-challenge.<domain>   TXT   "erp-verification=<token>"
//! ```
//!
//! The token was minted when the domain was claimed and is shown once more on
//! request. A TXT record at the apex proves control of the **zone**, which is
//! why one proof licenses `www.`, `api.` and everything else under the domain;
//! an HTTP well-known file would prove one host and justify nothing beside it.
//!
//! # Why a trait
//!
//! Resolution is a network call to somebody else's servers. Behind a trait, the
//! control plane's tests hand it a resolver that answers what the test says,
//! and the real one talks to the system's DNS.

use std::future::Future;
use std::pin::Pin;

use hickory_resolver::TokioResolver;

/// The label the record is published under.
pub const CHALLENGE_LABEL: &str = "_erp-challenge";
/// What the record's text begins with.
pub const RECORD_PREFIX: &str = "erp-verification=";

/// Where a tenant publishes the proof for `domain`.
#[must_use]
pub fn record_name(domain: &str) -> String {
    format!(
        "{CHALLENGE_LABEL}.{}",
        domain.trim().trim_end_matches('.').to_lowercase()
    )
}

/// What the record has to say for `token`.
#[must_use]
pub fn record_value(token: &str) -> String {
    format!("{RECORD_PREFIX}{token}")
}

/// Whether a published record proves `token`. The bare token is accepted too,
/// because somebody will paste it without the prefix and be right about what
/// they meant.
#[must_use]
pub fn proves(records: &[String], token: &str) -> bool {
    let expected = record_value(token);
    records
        .iter()
        .map(|r| r.trim().trim_matches('"'))
        .any(|r| r == expected || r == token)
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ProofError {
    /// The resolver could not answer. Not "the record is absent": that is an
    /// empty answer, and the difference is a 503 against a 409.
    #[error("the domain could not be looked up: {0}")]
    Dns(String),
}

/// Something that reads TXT records.
pub trait DomainProver: Send + Sync + std::fmt::Debug {
    /// Every TXT string published at `name`. Empty when there are none.
    fn txt_records<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, ProofError>> + Send + 'a>>;
}

/// The system's DNS.
pub struct DnsProver {
    resolver: TokioResolver,
}

impl std::fmt::Debug for DnsProver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DnsProver")
    }
}

impl DnsProver {
    /// Configured from `/etc/resolv.conf` (or the platform's equivalent).
    pub fn from_system() -> Result<Self, ProofError> {
        let resolver = TokioResolver::builder_tokio()
            .map_err(|e| ProofError::Dns(e.to_string()))?
            .build()
            .map_err(|e| ProofError::Dns(e.to_string()))?;
        Ok(Self { resolver })
    }
}

impl DomainProver for DnsProver {
    fn txt_records<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, ProofError>> + Send + 'a>> {
        Box::pin(async move {
            use hickory_resolver::net::{DnsError, NetError};
            use hickory_resolver::proto::rr::{RData, RecordType};
            match self.resolver.lookup(name, RecordType::TXT).await {
                Ok(lookup) => Ok(lookup
                    .answers()
                    .iter()
                    .filter_map(|record| match &record.data {
                        RData::TXT(txt) => Some(
                            txt.txt_data
                                .iter()
                                .map(|segment| String::from_utf8_lossy(segment).into_owned())
                                .collect::<String>(),
                        ),
                        _ => None,
                    })
                    .collect()),
                // No such name, or a name with no TXT: the record is not there,
                // which is an answer rather than a failure.
                Err(NetError::Dns(DnsError::NoRecordsFound(_))) => Ok(Vec::new()),
                Err(e) => Err(ProofError::Dns(e.to_string())),
            }
        })
    }
}

/// A deployment with no usable resolver. Every proof fails loudly as a 503
/// rather than quietly as "not published", so the operator hears about it.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoResolver;

impl DomainProver for NoResolver {
    fn txt_records<'a>(
        &'a self,
        _name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, ProofError>> + Send + 'a>> {
        Box::pin(async {
            Err(ProofError::Dns(
                "this deployment has no DNS resolver configured".to_owned(),
            ))
        })
    }
}

/// **What an origin may be**: `https://<host>[:port]`, nothing else, with the
/// host equal to or under a domain the tenant has proved.
///
/// Returns the host. `http://` is refused because a page served over it can be
/// rewritten by anybody on the path, and an allow-list entry is, once CORS
/// serves authenticated routes, the whole tenant.
pub fn origin_host(origin: &str) -> Option<String> {
    let rest = origin.strip_prefix("https://")?;
    if rest.is_empty()
        || rest
            .chars()
            .any(|c| matches!(c, '/' | '?' | '#' | '@' | '\\') || c.is_whitespace())
    {
        return None;
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (rest, None),
    };
    if let Some(port) = port
        && (port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) || port.len() > 5)
    {
        return None;
    }
    let host = host.to_lowercase();
    let usable = !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('.')
        && !host.ends_with('.')
        && !host.contains("..")
        && host
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-'));
    usable.then_some(host)
}

/// Whether `host` is `domain` or lies under it.
#[must_use]
pub fn is_under(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|rest| rest.ends_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_is_named_and_spelled_predictably() {
        assert_eq!(
            record_name("Salon.Example."),
            "_erp-challenge.salon.example"
        );
        assert_eq!(
            record_value("erp-verify-abc"),
            "erp-verification=erp-verify-abc"
        );
        assert!(proves(
            &["\"erp-verification=erp-verify-abc\"".to_owned()],
            "erp-verify-abc"
        ));
        assert!(proves(&["erp-verify-abc".to_owned()], "erp-verify-abc"));
        assert!(!proves(
            &["erp-verification=other".to_owned()],
            "erp-verify-abc"
        ));
        assert!(!proves(&[], "erp-verify-abc"));
    }

    /// **`https://host[:port]` and nothing else.** A path, a query, a userinfo
    /// or a plain `http://` is not an origin this API will hand a tenant to.
    #[test]
    fn an_origin_is_https_and_a_host_only() {
        assert_eq!(
            origin_host("https://salon.example").as_deref(),
            Some("salon.example")
        );
        assert_eq!(
            origin_host("https://App.Salon.Example:8443").as_deref(),
            Some("app.salon.example")
        );
        for bad in [
            "http://salon.example",
            "https://salon.example/",
            "https://salon.example/booking",
            "https://salon.example?x=1",
            "https://user@salon.example",
            "https://salon.example:",
            "https://salon.example:abc",
            "https://",
            "salon.example",
            "https://.salon.example",
            "https://salon..example",
        ] {
            assert_eq!(origin_host(bad), None, "{bad} was accepted");
        }
    }

    #[test]
    fn under_means_equal_or_a_subdomain_and_never_a_lookalike() {
        assert!(is_under("salon.example", "salon.example"));
        assert!(is_under("api.salon.example", "salon.example"));
        assert!(!is_under("salon.example.attacker.test", "salon.example"));
        assert!(!is_under("notsalon.example", "salon.example"));
    }
}
