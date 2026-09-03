//! Files in an S3-compatible bucket.
//!
//! # Compatible, not Amazon
//!
//! Nothing here is specific to AWS. The endpoint is configuration, so the same
//! engine talks to Hetzner Object Storage, Contabo, `MinIO` on a developer's
//! machine, or S3 itself. That is deliberate: the first market is Saudi Arabia
//! and a tenant who wants their documents in Frankfurt rather than in an AWS
//! region is not a special case (D15).
//!
//! Two providers this was written against, and what each needs:
//!
//! | | `S3_REGION` | `S3_ENDPOINT` | addressing |
//! |---|---|---|---|
//! | Hetzner | `fsn1`, `nbg1` or `hel1` | `https://<region>.your-objectstorage.com` | either |
//! | Contabo | `default` | `https://<region>.contabostorage.com` | path only |
//!
//! Path-style addressing is the default here, because it is the one both
//! accept. `S3_VIRTUAL_HOSTED_STYLE=true` switches to `bucket.host`, which real
//! AWS prefers and Contabo cannot do — it has no wildcard DNS and its
//! certificate covers only `*.contabostorage.com`.
//!
//! # No checksum mode to configure
//!
//! `object_store` sends a checksum when it is asked to and not otherwise, so
//! the trap that broke `aws-sdk-s3` against every non-AWS endpoint from 1.69.0
//! — CRC32 and `aws-chunked` turned on by default — is not reachable from here.
//! Integrity is not left to the provider either way: [`crate::fetch`] verifies
//! a SHA-256 recorded at write time, against every engine.
//!
//! # What is not here
//!
//! **Presigned URLs.** Every byte goes through the API process, which is the
//! shape the rest of this system already has — the route that serves a file is
//! the route that knows who is asking. Handing a browser a signed URL is a
//! different design with a different authorization story, and it is not this
//! one yet.

use object_store::ObjectStoreExt;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path;

use crate::{Storage, StorageError, check_key};

/// Everything needed to reach a bucket.
///
/// Credentials are in here as plain `String`s, which is what they are: this
/// process holds them for its whole life to sign every request.
#[derive(Clone)]
pub struct S3Config {
    pub bucket: String,
    /// Required, even where it is decorative. Contabo's is the literal
    /// `default` and Hetzner's is the location code; `SigV4` signs it either way,
    /// so a wrong one is a `403` rather than a redirect.
    pub region: String,
    /// `None` for Amazon itself. Anything else needs it.
    pub endpoint: Option<String>,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// `bucket.host` rather than `host/bucket`. Off by default — see the module
    /// docs.
    pub virtual_hosted_style: bool,
    /// Whether a `http://` endpoint is acceptable.
    ///
    /// **Development only.** It exists because `MinIO` on a laptop has no
    /// certificate, and it is off by default because a bucket reached over
    /// cleartext hands every document, and the signature over it, to anything
    /// on the path. `object_store` refuses cleartext without it, which is the
    /// same call `messaging::Relay` makes.
    pub allow_http: bool,
}

/// **Hand-written so the secret cannot be printed.** A derived `Debug` puts the
/// secret access key in the first `tracing` line, panic message or test failure
/// that formats a config — which is how a credential ends up in a log
/// aggregator that a different set of people can read.
impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("virtual_hosted_style", &self.virtual_hosted_style)
            .field("allow_http", &self.allow_http)
            .finish()
    }
}

/// Where files live, when they do not live on this machine's disk.
#[derive(Debug, Clone)]
pub struct S3 {
    store: AmazonS3,
}

impl S3 {
    /// Builds the client. **No request is made**, so this succeeding says the
    /// configuration parses and nothing about whether the bucket exists.
    pub fn new(config: &S3Config) -> Result<Self, String> {
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&config.bucket)
            .with_region(&config.region)
            .with_access_key_id(&config.access_key_id)
            .with_secret_access_key(&config.secret_access_key)
            .with_virtual_hosted_style_request(config.virtual_hosted_style)
            .with_allow_http(config.allow_http)
            // The OpenSSL this build already links, instead of the `aws-lc-rs`
            // that `object_store`'s own `aws` feature would pull in. See
            // `crate::crypto`.
            .with_crypto_provider(std::sync::Arc::new(crate::crypto::Openssl));

        if let Some(endpoint) = &config.endpoint {
            builder = builder.with_endpoint(endpoint);
        }

        let store = builder
            .build()
            .map_err(|e| format!("the S3 configuration is not usable: {e}"))?;
        Ok(Self { store })
    }

    /// The engine this deployment is configured for, or `None` for none.
    ///
    /// `S3_BUCKET` is the switch: without it this returns `Ok(None)` and the
    /// caller falls back to local disk. With it, **anything else missing is an
    /// error rather than a default** (law L6) — a deployment that meant to use
    /// a bucket and quietly wrote to a container's filesystem instead loses
    /// every file it was given the moment that container is replaced.
    pub fn from_env() -> Result<Option<Self>, String> {
        // Empty is unset. `compose.yaml` and every deployment tool in this world
        // spell "not configured" as a blank string at least as often as they
        // spell it as an absent variable — the same call `PRIMARY_REPLICA_URL`
        // makes.
        let Some(bucket) = std::env::var("S3_BUCKET")
            .ok()
            .map(|b| b.trim().to_owned())
            .filter(|b| !b.is_empty())
        else {
            return Ok(None);
        };
        // Blank counts as missing here too, and refusing is the point: an empty
        // region signs a request nothing will accept, and the `403` that comes
        // back names neither the variable nor the reason.
        let required = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("S3_BUCKET is set, so {name} must be too"))
        };

        Self::new(&S3Config {
            bucket,
            region: required("S3_REGION")?,
            endpoint: std::env::var("S3_ENDPOINT")
                .ok()
                .map(|e| e.trim().to_owned())
                .filter(|e| !e.is_empty()),
            access_key_id: required("S3_ACCESS_KEY_ID")?,
            secret_access_key: required("S3_SECRET_ACCESS_KEY")?,
            virtual_hosted_style: flag("S3_VIRTUAL_HOSTED_STYLE")?,
            allow_http: flag("S3_ALLOW_HTTP")?,
        })
        .map(Some)
    }

    /// Where a key lands, once it has been proved to be one.
    ///
    /// **`check_key` first, always** — the same order [`crate::Local`] uses, and
    /// for the same reason: a key is generated by this system, and a key that
    /// is not is refused before it reaches anything that would act on it.
    fn path(key: &str) -> Result<Path, StorageError> {
        check_key(key)?;
        Path::parse(key).map_err(|_| StorageError::NotAKey(key.to_owned()))
    }
}

#[async_trait::async_trait]
impl Storage for S3 {
    fn engine(&self) -> &'static str {
        "s3"
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), StorageError> {
        // A PUT of a whole object is atomic at the bucket: a reader sees the
        // old object or the new one, never half of either. That is what
        // `Local` needs its write-beside-and-rename for, and what this does not.
        self.store
            .put(&Self::path(key)?, bytes.to_vec().into())
            .await
            .map(|_| ())
            .map_err(unavailable)
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        let path = Self::path(key)?;
        let response = self.store.get(&path).await.map_err(unavailable)?;
        Ok(response.bytes().await.map_err(unavailable)?.to_vec())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        match self.store.delete(&Self::path(key)?).await {
            // Already gone is success, per the trait: deleting twice is the
            // same world either way, and a caller cleaning up after a failed
            // upload should not have to care which half succeeded.
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(unavailable(e)),
        }
    }
}

/// Whether an environment variable says yes.
///
/// Deliberately strict. `S3_VIRTUAL_HOSTED_STYLE=yes` silently meaning `false`
/// is a deployment that addresses its bucket the wrong way and gets a `404` for
/// every file it has.
fn flag(name: &str) -> Result<bool, String> {
    // Split from the reading so it can be tested: `unsafe_code` is forbidden in
    // this workspace, and `std::env::set_var` is unsafe as of the 2024 edition.
    parse_flag(name, std::env::var(name).ok().as_deref())
}

fn parse_flag(name: &str, value: Option<&str>) -> Result<bool, String> {
    match value.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" | "" => Ok(false),
        other => Err(format!("{name} must be true or false, not {other:?}")),
    }
}

/// Everything the bucket said, in this crate's terms.
///
/// Only `NotFound` is distinguished. A `403` from a wrong signature and a
/// timeout from a dead network are both "storage could not be reached" to a
/// person looking at a screen, and the difference between them is in the log
/// line — which carries the provider's own message.
fn unavailable(error: object_store::Error) -> StorageError {
    match error {
        object_store::Error::NotFound { .. } => StorageError::NoSuchFile,
        other => StorageError::Unavailable(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> S3Config {
        S3Config {
            bucket: "documents".to_owned(),
            region: "fsn1".to_owned(),
            endpoint: Some("https://fsn1.your-objectstorage.com".to_owned()),
            access_key_id: "AKIAEXAMPLE".to_owned(),
            secret_access_key: "wJalrXUtnFEMIK7MDENGbPxRfiCY".to_owned(),
            virtual_hosted_style: false,
            allow_http: false,
        }
    }

    #[test]
    fn a_configuration_builds_a_client_without_touching_the_network() {
        let s3 = S3::new(&config()).expect("builds");
        assert_eq!(s3.engine(), "s3");
    }

    /// Contabo's shape, which is the one with no endpoint region in it.
    #[test]
    fn contabos_literal_region_is_a_region_like_any_other() {
        S3::new(&S3Config {
            region: "default".to_owned(),
            endpoint: Some("https://eu2.contabostorage.com".to_owned()),
            ..config()
        })
        .expect("builds");
    }

    /// Amazon itself, where the endpoint is derived rather than given.
    #[test]
    fn no_endpoint_means_amazon() {
        S3::new(&S3Config {
            region: "eu-central-1".to_owned(),
            endpoint: None,
            virtual_hosted_style: true,
            ..config()
        })
        .expect("builds");
    }

    /// **The traversal cases, again.** `check_key` runs before anything else
    /// here exactly as it does in `Local`, so a key that climbs is refused
    /// rather than becoming an object name with `..` in it — which some
    /// gateways normalise and some do not.
    #[tokio::test]
    async fn a_key_that_climbs_out_is_refused_before_a_request_is_made() {
        let s3 = S3::new(&config()).expect("builds");
        assert!(matches!(
            s3.put("../escaped.txt", b"x").await,
            Err(StorageError::NotAKey(_))
        ));
        assert!(matches!(
            s3.get("../../etc/passwd").await,
            Err(StorageError::NotAKey(_))
        ));
        assert!(matches!(
            s3.delete("a//b").await,
            Err(StorageError::NotAKey(_))
        ));
    }

    #[test]
    fn a_generated_key_survives_the_trip_through_a_path() {
        let path = S3::path("invoice/INV-1/a1b2c3.pdf").expect("a usable key");
        assert_eq!(path.as_ref(), "invoice/INV-1/a1b2c3.pdf");
    }

    /// A missing object is `NoSuchFile` and not a generic failure, because the
    /// route above it answers `404` for one and `503` for the other.
    #[test]
    fn a_missing_object_is_distinguished_from_a_bucket_that_is_not_answering() {
        assert_eq!(
            unavailable(object_store::Error::NotFound {
                path: "invoice/INV-1/a.pdf".to_owned(),
                source: "no such key".into(),
            }),
            StorageError::NoSuchFile
        );
        assert!(matches!(
            unavailable(object_store::Error::Generic {
                store: "S3",
                source: "403 Forbidden".into(),
            }),
            StorageError::Unavailable(_)
        ));
    }

    /// `S3_VIRTUAL_HOSTED_STYLE=yes` silently meaning `false` is a deployment
    /// that addresses its bucket the wrong way and gets a `404` for every file
    /// it holds.
    #[test]
    fn a_flag_that_is_neither_true_nor_false_is_refused() {
        assert_eq!(parse_flag("F", None), Ok(false));
        assert_eq!(parse_flag("F", Some("")), Ok(false));
        assert_eq!(parse_flag("F", Some(" TRUE ")), Ok(true));
        assert_eq!(parse_flag("F", Some("1")), Ok(true));
        assert_eq!(parse_flag("F", Some("0")), Ok(false));
        assert!(parse_flag("F", Some("yes")).is_err());
        assert!(parse_flag("F", Some("on")).is_err());
    }

    /// The secret is the one field that must never reach a log line.
    #[test]
    fn the_secret_is_not_printable() {
        let printed = format!("{:?}", config());
        assert!(
            !printed.contains("wJalrXUtnFEMIK7MDENGbPxRfiCY"),
            "{printed}"
        );
        // The key *id* is not a secret, and a log line that cannot say which
        // credential a deployment is using is a log line nobody can act on.
        assert!(printed.contains("AKIAEXAMPLE"), "{printed}");
    }
}
