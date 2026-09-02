//! API keys, in pairs.
//!
//! # Two keys, because they answer two questions
//!
//! A **public key** identifies. It is safe in a mobile app, in a browser, in a
//! support ticket and in a log line — it says *which integration this is* and
//! proves nothing. A **private key** authenticates. It is shown once, stored
//! hashed, and if it leaks the answer is rotation.
//!
//! Systems that ship one key end up with it in both places, and then the thing
//! that identifies an integration in a log is the thing that can act as it.
//!
//! # A key acts as a machine identity
//!
//! Not as the person who made it. A key carrying its creator's identity dies
//! when they leave, and everything it does reads in the audit trail as theirs —
//! which is a lie somebody eventually relies on.
//!
//! So issuing one creates an identity with no password, joins it to the tenant
//! with a role, and records who asked. Everything downstream of authentication —
//! sessions, membership, the audit trail — then works without learning a second
//! shape, which is what `0004_authentication.sql` predicted when it said API
//! keys would be more rows rather than more tables.
//!
//! # Scopes narrow, they never widen
//!
//! A key can never do more than the role its identity holds. The scopes are a
//! second gate in front of that, so an integration that reads bookings cannot
//! post journal entries **even if** somebody gives its identity the owner's
//! role by mistake.
//!
//! # Why the secret is not Argon2
//!
//! Because there is nothing to brute-force. A password is short and chosen by a
//! person; a key is 256 bits from the OS, so the slow hash buys nothing and
//! costs ~50ms **on every request** an integration makes. This is the same
//! argument `session` already makes for its tokens, in the same schema, for the
//! same reason: what matters is that a stolen database dump cannot be replayed,
//! and a digest gives that.

use erp_types::{IdentityId, TenantId, Timestamp};
use sha2::Digest as _;

use crate::auth::AuthError;
use crate::model::{Actor, Scope};
use crate::{AccessError, ControlPlane};
use erp_tenant::Role;

/// What a key may do.
///
/// `module:capability`, or `*:capability` for every module. The wildcard is only
/// on the module: a key that may do *anything* is a key nobody has thought
/// about, and `*:*` is deliberately not a scope this type can hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyScope {
    /// `None` is every module.
    module: Option<String>,
    capability: crate::Capability,
}

impl KeyScope {
    /// From `booking:read`, `*:read`, `ledger:post_entries`.
    pub fn parse(raw: &str) -> Result<Self, BadScope> {
        let (module, capability) = raw
            .trim()
            .split_once(':')
            .ok_or_else(|| BadScope(raw.to_owned()))?;

        let capability = match capability {
            "read" => crate::Capability::Read,
            "post_entries" => crate::Capability::PostEntries,
            "manage_accounts" => crate::Capability::ManageAccounts,
            "manage_tenant" => crate::Capability::ManageTenant,
            _ => return Err(BadScope(raw.to_owned())),
        };

        let module = match module {
            "*" => None,
            name if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') =>
            {
                Some(name.to_owned())
            }
            _ => return Err(BadScope(raw.to_owned())),
        };

        Ok(Self { module, capability })
    }

    #[must_use]
    pub fn as_string(&self) -> String {
        format!(
            "{}:{}",
            self.module.as_deref().unwrap_or("*"),
            self.capability.as_str()
        )
    }

    /// Whether this scope permits `capability` on `module`.
    ///
    /// A request outside any module — `/v1/members`, `/v1/health` — is `None`,
    /// and only a wildcard scope covers it. That is the strict reading and the
    /// right one: a key scoped to `booking:read` has no business listing a
    /// tenant's members.
    #[must_use]
    pub fn permits(&self, capability: crate::Capability, module: Option<&str>) -> bool {
        if self.capability != capability {
            return false;
        }
        match (&self.module, module) {
            (None, _) => true,
            (Some(mine), Some(theirs)) => mine == theirs,
            (Some(_), None) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a scope")]
pub struct BadScope(pub String);

impl erp_i18n::Localize for BadScope {
    fn message(&self) -> erp_i18n::Message {
        erp_i18n::Message::new(crate::messages::NOT_A_SCOPE)
            .with("scope", erp_i18n::MessageArg::text(&self.0))
    }
}

/// A key as anybody but its holder sees it. **No secret.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKey {
    pub id: uuid::Uuid,
    pub tenant: TenantId,
    /// The machine identity it acts as.
    pub identity: IdentityId,
    /// Safe to show anywhere.
    pub public_key: String,
    pub name: String,
    pub scopes: Vec<KeyScope>,
    /// The role its machine identity holds in the tenant.
    ///
    /// **The ceiling the scopes narrow.** Read here rather than stored twice:
    /// membership is where a role lives, and a second copy on this row is a
    /// second thing that can be wrong.
    pub role: Role,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
    /// When it stops working. Set by a rotation on the key being replaced.
    pub expires_at: Option<Timestamp>,
    pub revoked_at: Option<Timestamp>,
    pub revoked_why: Option<String>,
    pub rotated_from: Option<uuid::Uuid>,
}

impl ApiKey {
    /// Whether this key would be accepted right now.
    #[must_use]
    pub fn usable(&self, at: Timestamp) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|until| until > at)
    }
}

/// The private half, which is returned **once**.
///
/// No `Debug` that shows it and no `Serialize`: the only way it reaches a
/// client is a handler that puts [`Secret::expose`] into a response body on
/// purpose.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// What a presented key proved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyContext {
    pub key: uuid::Uuid,
    pub public_key: String,
    pub identity: IdentityId,
    pub tenant: TenantId,
    pub scopes: std::sync::Arc<[KeyScope]>,
}

impl KeyContext {
    /// Whether the scopes permit `capability` on `module`.
    #[must_use]
    pub fn permits(&self, capability: crate::Capability, module: Option<&str>) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope.permits(capability, module))
    }
}

/// The prefix a public key carries, so a value found in a log is recognisable
/// on sight.
const PUBLIC_PREFIX: &str = "pk_";
/// …and a private one, which is what secret scanners look for.
const PRIVATE_PREFIX: &str = "sk_";

/// How long a rotated key keeps working by default.
///
/// Seven days: long enough for a deploy that has to go through somebody else's
/// change process, short enough that a rotation is finished within a sprint. A
/// key that cannot be rotated without downtime is a key nobody rotates.
// `from_days` is not const on stable, and this is a const. Seconds, spelt out.
#[expect(
    clippy::duration_suboptimal_units,
    reason = "`Duration::from_days` is not const on stable, and this is a const"
)]
pub const ROTATION_OVERLAP: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

impl ControlPlane {
    /// Issues a key, and returns the private half **once**.
    pub async fn issue_key(
        &self,
        tenant: TenantId,
        name: &str,
        scopes: &[KeyScope],
        role: Role,
        actor: Actor,
    ) -> Result<(ApiKey, Secret), AccessError> {
        self.issue_key_replacing(tenant, name, scopes, role, None, actor)
            .await
    }

    async fn issue_key_replacing(
        &self,
        tenant: TenantId,
        name: &str,
        scopes: &[KeyScope],
        role: Role,
        replacing: Option<uuid::Uuid>,
        actor: Actor,
    ) -> Result<(ApiKey, Secret), AccessError> {
        let identity = self.create_identity(actor).await?;
        self.grant_membership(identity.id, Scope::Tenant(tenant), role.as_str(), actor)
            .await?;

        let token = crate::auth::key_token()?;
        let public_key = format!("{PUBLIC_PREFIX}{token}");
        let secret_half = crate::auth::key_token()?;
        let secret = Secret(format!("{PRIVATE_PREFIX}{token}.{secret_half}"));

        // Time-ordered, like every other id this system mints: a v4 in a
        // primary key scatters writes across the index.
        let authenticator = uuid::Uuid::now_v7();
        sqlx::query!(
            "INSERT INTO authenticator (id, identity_id, kind, handle, secret)
             VALUES ($1, $2, 'api_key', $3, $4)",
            authenticator,
            identity.id.as_uuid(),
            public_key,
            hex::encode(sha2::Sha256::digest(secret_half.as_bytes())),
        )
        .execute(&self.pool)
        .await?;

        let id = uuid::Uuid::now_v7();
        let stored: Vec<String> = scopes.iter().map(KeyScope::as_string).collect();
        let row = sqlx::query!(
            r#"INSERT INTO api_key
                   (id, authenticator_id, tenant_id, identity_id, name, scopes,
                    created_by, rotated_from)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
               RETURNING created_at"#,
            id,
            authenticator,
            tenant.as_uuid(),
            identity.id.as_uuid(),
            name.trim(),
            &stored,
            actor.identity.as_ref().map(IdentityId::as_uuid),
            replacing,
        )
        .fetch_one(&self.pool)
        .await?;

        self.record(
            actor,
            "api_key.issued",
            "api_key",
            &id.to_string(),
            serde_json::json!({
                "tenant": tenant.to_string(),
                "public_key": public_key,
                "scopes": stored,
                "role": role.as_str(),
            }),
        )
        .await?;

        Ok((
            ApiKey {
                id,
                tenant,
                identity: identity.id,
                public_key,
                name: name.trim().to_owned(),
                scopes: scopes.to_vec(),
                role,
                created_at: row.created_at,
                last_used_at: None,
                expires_at: None,
                revoked_at: None,
                revoked_why: None,
                rotated_from: replacing,
            },
            secret,
        ))
    }

    /// Checks a presented private key.
    ///
    /// # Why this is not cached
    ///
    /// Because revoking a key has to take effect **now**. A five-second TTL on
    /// a credential is five seconds of a leaked key still working, which is the
    /// one place in this system where a stale answer is the wrong trade — and
    /// the lookup is one indexed row.
    pub async fn key(&self, presented: &str) -> Result<KeyContext, AuthError> {
        let Some(rest) = presented.strip_prefix(PRIVATE_PREFIX) else {
            return Err(AuthError::InvalidCredentials);
        };
        let Some((token, secret)) = rest.split_once('.') else {
            return Err(AuthError::InvalidCredentials);
        };
        let public_key = format!("{PUBLIC_PREFIX}{token}");

        let row = sqlx::query!(
            r#"SELECT k.id, k.tenant_id as "tenant: TenantId",
                      k.identity_id as "identity: IdentityId",
                      k.scopes, k.revoked_at, k.expires_at,
                      a.secret, i.status
                 FROM api_key k
                 JOIN authenticator a ON a.id = k.authenticator_id
                 JOIN identity i ON i.id = k.identity_id
                WHERE a.kind = 'api_key' AND a.handle = $1"#,
            public_key,
        )
        .fetch_optional(&self.pool)
        .await?;

        // **Constant time against the stored digest**, and computed whether or
        // not the row exists, so a wrong public key and a wrong secret take the
        // same time — the same discipline `authenticate` uses.
        let offered = hex::encode(sha2::Sha256::digest(secret.as_bytes()));
        let stored = row.as_ref().map_or_else(String::new, |r| r.secret.clone());
        let matches = constant_time_eq(offered.as_bytes(), stored.as_bytes());

        let Some(row) = row else {
            return Err(AuthError::InvalidCredentials);
        };
        if !matches || row.status != "active" {
            return Err(AuthError::InvalidCredentials);
        }
        let now = chrono::Utc::now();
        if row.revoked_at.is_some() || row.expires_at.is_some_and(|until| until <= now) {
            return Err(AuthError::InvalidCredentials);
        }

        let scopes: Vec<KeyScope> = row
            .scopes
            .iter()
            .filter_map(|raw| KeyScope::parse(raw).ok())
            .collect();
        if scopes.len() != row.scopes.len() {
            // A stored scope this build cannot read. Refusing is the only safe
            // answer: carrying on with the ones that parsed would silently
            // widen or narrow a key depending on which failed (L6).
            return Err(AuthError::InvalidCredentials);
        }

        // Best effort, and deliberately not awaited into the critical path's
        // correctness: a lost update here costs a slightly stale timestamp.
        let _ = sqlx::query!(
            "UPDATE api_key SET last_used_at = now() WHERE id = $1",
            row.id
        )
        .execute(&self.pool)
        .await;

        Ok(KeyContext {
            key: row.id,
            public_key,
            identity: row.identity,
            tenant: row.tenant,
            scopes: scopes.into(),
        })
    }

    /// Every key a tenant has, newest first. Revoked ones included, because
    /// "what did we revoke and when" is the question after an incident.
    pub async fn keys(&self, tenant: TenantId) -> Result<Vec<ApiKey>, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT k.id, k.tenant_id as "tenant: TenantId",
                      k.identity_id as "identity: IdentityId",
                      a.handle as "public_key!", k.name, k.scopes,
                      m.role as "role?",
                      k.created_at, k.last_used_at, k.expires_at,
                      k.revoked_at, k.revoked_why, k.rotated_from
                 FROM api_key k
                 JOIN authenticator a ON a.id = k.authenticator_id
                 LEFT JOIN membership m
                        ON m.identity_id = k.identity_id
                       AND m.tenant_id = k.tenant_id
                       AND m.revoked_at IS NULL
                WHERE k.tenant_id = $1
                ORDER BY k.created_at DESC"#,
            tenant.as_uuid(),
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| ApiKey {
                id: row.id,
                tenant: row.tenant,
                identity: row.identity,
                public_key: row.public_key,
                name: row.name,
                scopes: row
                    .scopes
                    .iter()
                    .filter_map(|raw| KeyScope::parse(raw).ok())
                    .collect(),
                // A key whose membership was revoked has no role, and `Viewer`
                // is the honest floor: it can do nothing, because `enter`
                // refuses it before any capability is consulted.
                role: row
                    .role
                    .as_deref()
                    .and_then(|r| r.parse().ok())
                    .unwrap_or(Role::Viewer),
                created_at: row.created_at,
                last_used_at: row.last_used_at,
                expires_at: row.expires_at,
                revoked_at: row.revoked_at,
                revoked_why: row.revoked_why,
                rotated_from: row.rotated_from,
            })
            .collect())
    }

    /// Stops a key working, now.
    ///
    /// Idempotent: revoking a revoked key keeps the first reason, because the
    /// first reason is the true one and a retry must not overwrite it.
    pub async fn revoke_key(
        &self,
        tenant: TenantId,
        key: uuid::Uuid,
        why: &str,
        actor: Actor,
    ) -> Result<bool, AccessError> {
        let changed = sqlx::query!(
            "UPDATE api_key SET revoked_at = now(), revoked_why = $3
              WHERE id = $1 AND tenant_id = $2 AND revoked_at IS NULL",
            key,
            tenant.as_uuid(),
            why.trim(),
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        if changed > 0 {
            self.record(
                actor,
                "api_key.revoked",
                "api_key",
                &key.to_string(),
                serde_json::json!({ "why": why.trim() }),
            )
            .await?;
        }
        Ok(changed > 0)
    }

    /// Issues a replacement and gives the old key an expiry.
    ///
    /// **The overlap is the point.** The old key keeps working for `overlap`,
    /// so the integration holding it can be redeployed on its own schedule —
    /// which is what makes rotation something a business will actually do.
    ///
    /// The new key has the same scopes and role as the old one. Changing what a
    /// key may do is a different act, and folding it into a rotation is how an
    /// integration comes back with permissions nobody chose.
    pub async fn rotate_key(
        &self,
        tenant: TenantId,
        key: uuid::Uuid,
        overlap: std::time::Duration,
        actor: Actor,
    ) -> Result<Option<(ApiKey, Secret)>, AccessError> {
        let Some(old) = self
            .keys(tenant)
            .await?
            .into_iter()
            .find(|k| k.id == key && k.revoked_at.is_none())
        else {
            return Ok(None);
        };

        let issued = self
            .issue_key_replacing(
                tenant,
                &old.name,
                &old.scopes,
                old.role,
                Some(old.id),
                actor,
            )
            .await?;

        let seconds = i64::try_from(overlap.as_secs()).unwrap_or(i64::MAX);
        sqlx::query!(
            "UPDATE api_key
                SET expires_at = now() + ($2::BIGINT * INTERVAL '1 second')
              WHERE id = $1",
            key,
            seconds,
        )
        .execute(&self.pool)
        .await?;

        self.record(
            actor,
            "api_key.rotated",
            "api_key",
            &key.to_string(),
            serde_json::json!({ "into": issued.0.id.to_string(), "overlap_seconds": seconds }),
        )
        .await?;

        Ok(Some(issued))
    }
}

/// Compares two digests without leaking where they differ.
///
/// Both are hex of a SHA-256, so they are the same length in every real case —
/// the length check is for the one where the stored value is empty because the
/// key does not exist, and it comes after the loop for the same reason the loop
/// exists.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut difference = u8::from(a.len() != b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        difference |= x ^ y;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Capability;

    #[test]
    fn a_scope_is_a_module_and_a_capability() {
        let read = KeyScope::parse("booking:read").expect("a scope");
        assert!(read.permits(Capability::Read, Some("booking")));
        assert!(!read.permits(Capability::Read, Some("ledger")));
        assert!(!read.permits(Capability::PostEntries, Some("booking")));
        assert_eq!(read.as_string(), "booking:read");
    }

    /// **The scope in the plan's own words**: an integration that reads
    /// bookings cannot post journal entries.
    #[test]
    fn a_key_that_reads_bookings_cannot_post_journal_entries() {
        let scopes = [KeyScope::parse("booking:read").expect("a scope")];
        let permits = |capability, module| {
            scopes
                .iter()
                .any(|s: &KeyScope| s.permits(capability, module))
        };

        assert!(permits(Capability::Read, Some("booking")));
        assert!(!permits(Capability::PostEntries, Some("ledger")));
        assert!(!permits(Capability::ManageTenant, Some("booking")));
    }

    /// A wildcard is on the module and never on the capability.
    #[test]
    fn a_wildcard_covers_every_module_and_no_extra_capability() {
        let all = KeyScope::parse("*:read").expect("a scope");
        assert!(all.permits(Capability::Read, Some("booking")));
        assert!(all.permits(Capability::Read, Some("ledger")));
        assert!(
            all.permits(Capability::Read, None),
            "a route outside any module"
        );
        assert!(!all.permits(Capability::ManageTenant, Some("booking")));

        assert_eq!(KeyScope::parse("*:*"), Err(BadScope("*:*".to_owned())));
    }

    /// **A route outside any module needs a wildcard.**
    ///
    /// A key scoped to `booking:read` has no business listing a tenant's
    /// members, and `/v1/members` is not any module's path.
    #[test]
    fn a_module_scope_does_not_reach_outside_its_module() {
        let booking = KeyScope::parse("booking:read").expect("a scope");
        assert!(!booking.permits(Capability::Read, None));
    }

    #[test]
    fn nonsense_is_not_a_scope() {
        for raw in [
            "",
            "read",
            "booking:",
            ":read",
            "booking:fly",
            "BOOKING:read",
        ] {
            assert!(KeyScope::parse(raw).is_err(), "{raw} parsed");
        }
    }

    #[test]
    fn a_secret_does_not_print_itself() {
        let secret = Secret("sk_abc.def".to_owned());
        assert_eq!(format!("{secret:?}"), "Secret(***)");
        assert_eq!(secret.expose(), "sk_abc.def");
    }

    #[test]
    fn comparing_digests_is_length_safe() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b""));
        assert!(!constant_time_eq(b"", b"abc"));
    }
}
