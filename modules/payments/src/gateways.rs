//! A tenant's own gateway credentials, and the clients they build.
//!
//! # Sealed, because they are money
//!
//! A Moyasar secret key charges cards. A Tabby key captures and refunds. These
//! are stored the way a ZATCA signing key is — sealed under the deployment's
//! `SEALING_KEY`, in `module_secret` — and a deployment with no sealing key
//! **refuses to store one** rather than keeping it in the clear (L6).
//!
//! # One secret per provider
//!
//! `payments.moyasar`, `payments.tabby`, `payments.tamara`, mirroring the
//! `webhooks.{provider}` key the callback secret already uses. Per provider
//! rather than one blob, so rotating one key does not rewrite the others and so
//! a sweep unseals only what it is about to use — which is usually one.

use erp_payments::{Gateway, Moyasar, Tabby, Tamara};
use serde::{Deserialize, Serialize};

/// What a tenant has to give this system before it can talk to a gateway.
///
/// **Never `Debug`-derived**: every variant holds something that moves money.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum Credentials {
    Moyasar {
        /// `sk_live_…` or `sk_test_…`.
        secret: String,
    },
    Tabby {
        secret: String,
        /// Issued by Tabby's integration manager, and required on every
        /// checkout.
        merchant_code: String,
    },
    Tamara {
        token: String,
        /// Tamara's sandbox is a different host rather than a different key,
        /// so which one is part of the configuration.
        #[serde(default)]
        sandbox: bool,
    },
}

impl std::fmt::Debug for Credentials {
    /// The provider, and nothing else. A derived `Debug` puts a key that can
    /// charge a card into the first log line that formats one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("provider", &self.provider())
            .finish_non_exhaustive()
    }
}

impl Credentials {
    #[must_use]
    pub const fn provider(&self) -> &'static str {
        match self {
            Self::Moyasar { .. } => "moyasar",
            Self::Tabby { .. } => "tabby",
            Self::Tamara { .. } => "tamara",
        }
    }

    /// Where this provider's credentials are sealed.
    #[must_use]
    pub fn key(provider: &str) -> String {
        format!("payments.{provider}")
    }

    /// A client, or the reason there cannot be one.
    pub fn client(&self) -> Result<Box<dyn Gateway>, erp_payments::GatewayError> {
        Ok(match self {
            Self::Moyasar { secret } => Box::new(Moyasar::new(secret)?),
            Self::Tabby {
                secret,
                merchant_code,
            } => Box::new(Tabby::new(secret, merchant_code)?),
            Self::Tamara { token, sandbox } => {
                let tamara = Tamara::new(token)?;
                Box::new(if *sandbox {
                    tamara.at(erp_payments::SANDBOX)
                } else {
                    tamara
                })
            }
        })
    }
}

/// Every provider this system can be configured for.
///
/// The list the settings route validates against and the sweep iterates, in one
/// place — so a provider that can be configured and never swept, or swept and
/// never configured, is not expressible.
pub const PROVIDERS: &[&str] = &["moyasar", "tabby", "tamara"];

/// Why a tenant's gateway configuration could not be used.
#[derive(Debug, thiserror::Error)]
pub enum GatewayConfigError {
    #[error(transparent)]
    Secret(#[from] erp_eventlog::SecretError),
    /// **A stored value that will not parse is a failure, not an absence**
    /// (L6). Treating it as "not configured" would silently stop collecting
    /// money for a tenant who thinks they are.
    #[error("the stored credentials for {provider} cannot be read")]
    Unreadable { provider: String },
    #[error("{0}")]
    Unusable(#[from] erp_payments::GatewayError),
}

/// Reads one provider's credentials out of the vault.
///
/// `None` when this tenant has not configured that provider, which is the
/// ordinary case for most tenants and most providers.
pub async fn credentials(
    conn: &mut sqlx::PgConnection,
    sealing: &erp_eventlog::SealingKey,
    provider: &str,
) -> Result<Option<Credentials>, GatewayConfigError> {
    let Some(sealed) =
        erp_eventlog::secrets::get(conn, sealing, &Credentials::key(provider)).await?
    else {
        return Ok(None);
    };
    serde_json::from_slice(&sealed)
        .map(Some)
        .map_err(|_| GatewayConfigError::Unreadable {
            provider: provider.to_owned(),
        })
}

/// Writes them, sealed.
pub async fn configure(
    conn: &mut sqlx::PgConnection,
    sealing: &erp_eventlog::SealingKey,
    credentials: &Credentials,
) -> Result<(), GatewayConfigError> {
    // Refused here rather than on the first customer's card: a key this system
    // cannot build a client from is a configuration mistake, and it should read
    // like one while somebody is looking at a settings screen.
    credentials.client()?;

    let json = serde_json::to_vec(credentials).map_err(|_| GatewayConfigError::Unreadable {
        provider: credentials.provider().to_owned(),
    })?;
    Ok(erp_eventlog::secrets::put(
        conn,
        sealing,
        &Credentials::key(credentials.provider()),
        &json,
    )
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_never_prints_what_it_holds() {
        let printed = format!(
            "{:?}",
            Credentials::Moyasar {
                secret: "sk_live_verysecret".to_owned(),
            }
        );
        assert!(printed.contains("moyasar"), "{printed}");
        assert!(!printed.contains("sk_live_verysecret"), "{printed}");

        let printed = format!(
            "{:?}",
            Credentials::Tabby {
                secret: "sk_live_other".to_owned(),
                merchant_code: "bassat".to_owned(),
            }
        );
        assert!(!printed.contains("sk_live_other"), "{printed}");
    }

    #[test]
    fn each_provider_builds_its_own_client() {
        assert_eq!(
            Credentials::Moyasar {
                secret: "sk_test_x".to_owned()
            }
            .client()
            .expect("builds")
            .provider(),
            "moyasar"
        );
        assert_eq!(
            Credentials::Tabby {
                secret: "sk_test_x".to_owned(),
                merchant_code: "bassat".to_owned(),
            }
            .client()
            .expect("builds")
            .provider(),
            "tabby"
        );
        assert_eq!(
            Credentials::Tamara {
                token: "api-token".to_owned(),
                sandbox: true,
            }
            .client()
            .expect("builds")
            .provider(),
            "tamara"
        );
    }

    /// A key that cannot build a client is refused where somebody is looking at
    /// a settings screen, not on the first customer's card.
    #[test]
    fn a_publishable_key_is_refused_at_configuration_time() {
        assert!(
            Credentials::Moyasar {
                secret: "pk_test_x".to_owned()
            }
            .client()
            .is_err()
        );
        assert!(
            Credentials::Tabby {
                secret: "sk_test_x".to_owned(),
                merchant_code: String::new(),
            }
            .client()
            .is_err()
        );
    }

    #[test]
    fn a_credential_is_sealed_under_its_own_providers_key() {
        assert_eq!(Credentials::key("moyasar"), "payments.moyasar");
        for provider in PROVIDERS {
            assert!(erp_payments::PROVIDERS.contains(provider), "{provider}");
        }
    }

    /// The stored shape is what this build writes and reads back.
    #[test]
    fn credentials_survive_the_vault() {
        for credentials in [
            Credentials::Moyasar {
                secret: "sk_test_x".to_owned(),
            },
            Credentials::Tabby {
                secret: "sk_test_x".to_owned(),
                merchant_code: "bassat".to_owned(),
            },
            Credentials::Tamara {
                token: "t".to_owned(),
                sandbox: true,
            },
        ] {
            let json = serde_json::to_vec(&credentials).expect("serializes");
            let back: Credentials = serde_json::from_slice(&json).expect("parses");
            assert_eq!(back.provider(), credentials.provider());
        }
    }
}
