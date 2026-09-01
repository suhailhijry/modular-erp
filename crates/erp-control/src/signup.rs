//! Signing up, in two halves with a mailbox in between.
//!
//! # What this replaces
//!
//! One call that did everything: an identity, an authenticator, a tenant, a
//! database, a migration chain, and a session. From an unauthenticated
//! endpoint, with nothing between the request and the `CREATE DATABASE` but a
//! password-length check.
//!
//! That gave an attacker two things for one HTTP request. A **database**, which
//! at this fleet's size is a disk rather than an inconvenience. And the
//! **login handle**, because signing up as somebody else's address wrote an
//! authenticator under it with a password of the attacker's choosing, after
//! which the real owner could never sign up: they would have to prove a
//! password they never set.
//!
//! So a request now writes one row and one outbox effect and stops there.
//! Nothing is built until the address answers.
//!
//! ```text
//!   POST /v1/signups            ──►  pending_signup + an email          202
//!                                          │
//!                                    the mailbox
//!                                          │
//!   POST /v1/signups/{token}    ──►  identity, tenant, database, session 201
//! ```
//!
//! # Why there is no "look at this link first" call
//!
//! `/v1/join/{token}` has one, because the person opening an invitation did not
//! write it and needs to be told what they are joining. The person opening this
//! one filled the form in themselves five minutes ago, and confirming does the
//! thing they asked for. A page that asked them to confirm their own request
//! would be a click with nothing behind it.
//!
//! # What this does not do
//!
//! Rate limit. [`REQUEST_INTERVAL`] caps mail **per address**, which is what
//! stops this being a way to post a thousand messages into one mailbox, and it
//! is all this module can do on its own. Limiting per *caller* needs a notion
//! of caller that does not exist yet; it arrives with API keys in Phase 12c,
//! and this is its second user.
//!
//! What is fixed here is the part that was never about rate limiting: one
//! request no longer costs a database, and no longer takes an address.

use erp_types::{IdentityId, Timestamp};
use uuid::Uuid;

use crate::auth::{SignupToken, hash_password};
use crate::model::Actor;
use crate::{AccessError, AuthError, ControlPlane, ModuleSetup, Session, SessionToken};

/// How long a confirmation link works for.
///
/// A day, against an invitation's fortnight. An invitation waits on somebody
/// else's attention and has to survive a holiday; this one waits on the person
/// who was filling in a form a moment ago, so a long window is exposure without
/// a use for it.
pub const SIGNUP_LIFETIME: std::time::Duration = std::time::Duration::from_hours(24);

/// The least time between two confirmation emails to one address.
///
/// **Not a rate limit** — see the module docs. It is the smallest thing that
/// stops `POST /v1/signups` being a way to send somebody unlimited mail, which
/// is a vector this flow would otherwise have introduced while closing a bigger
/// one.
///
/// A minute is long enough to make the volume useless and short enough that
/// somebody who genuinely lost the first message is not stuck. The refusal says
/// when to try again, because "too soon" with no number is a page people
/// reload.
pub const REQUEST_INTERVAL: std::time::Duration = std::time::Duration::from_mins(1);

/// Everything a signup asks for.
///
/// A struct rather than five parameters, for the reason `sales::Draft` is one:
/// four of them are strings, and transposing two strings is a bug no type can
/// catch. Slug and company are the pair that would go unnoticed longest.
#[derive(Debug)]
pub struct SignupRequest {
    /// The first user's login, and where the confirmation goes.
    pub email: String,
    pub password: String,
    pub slug: String,
    pub company: String,
    /// What to install once the address answers. Resolved by the caller,
    /// because the control plane holds no domain and cannot name a module.
    pub modules: Vec<ModuleSetup>,
}

/// A signup waiting on its address.
///
/// Never carries the token. The only place that exists in the clear is the
/// return of [`ControlPlane::request_signup`], which puts it straight into an
/// email and drops it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSignup {
    pub id: Uuid,
    pub handle: String,
    pub slug: String,
    pub company: String,
    pub modules: Vec<String>,
    pub expires_at: Timestamp,
    pub created_at: Timestamp,
}

/// What confirming produced. The same thing the old one-shot signup returned.
#[derive(Debug)]
pub struct Confirmed {
    pub tenant: crate::Tenant,
    pub identity: IdentityId,
    pub token: SessionToken,
    pub session: Session,
}

#[derive(Debug, thiserror::Error)]
pub enum SignupError {
    /// No live request for that token. Covers wrong, expired, cancelled and
    /// already-confirmed as one answer, for the same reason
    /// [`InvitationError::NotValid`](crate::InvitationError::NotValid) does:
    /// telling a link-holder *which* is telling them something they have not
    /// proved they should know.
    #[error("that confirmation link is not valid")]
    NotValid,
    /// A confirmation is already on its way to this address, and it was sent
    /// less than [`REQUEST_INTERVAL`] ago.
    #[error("a confirmation was sent to this address {sent} seconds ago; retry in {retry_in}")]
    TooSoon { sent: i64, retry_in: i64 },
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error(transparent)]
    Auth(#[from] AuthError),
}

impl erp_i18n::Localize for SignupError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NotValid => Message::new(messages::SIGNUP_NOT_VALID),
            Self::TooSoon { retry_in, .. } => Message::new(messages::SIGNUP_TOO_SOON)
                .with("seconds", MessageArg::Count(*retry_in)),
            Self::Access(e) => e.message(),
            Self::Auth(e) => e.message(),
        }
    }
}

impl ControlPlane {
    /// Records a signup and promises the email that proves the address.
    ///
    /// Builds **nothing**. On the way out there is one `pending_signup` row and
    /// one outbox effect, and the caller has a token it should put in a link
    /// and forget.
    ///
    /// `confirm_base` is where the confirmation lands, **without** the token:
    /// this function appends it, which keeps it the only place the token exists
    /// in the clear. `locale` is the language the form was in, and it is the
    /// caller's because it is the only signal there will ever be — the person
    /// has no account, so there is no stored preference to render from when a
    /// worker picks the row up an instant later.
    ///
    /// # What is checked before anything is written
    ///
    /// The slug is free, so a name that is already gone is refused at the form
    /// rather than after a round trip through a mailbox. And if the address
    /// **already has an account**, its password has to match: without that,
    /// anybody could send a confirmation to any address by naming it, and a
    /// confirmation is a link that creates a company.
    ///
    /// That the address has an account is therefore something a caller can
    /// learn from the error. It always was — the old signup answered the same
    /// way — and the alternative is letting a stranger post mail through us.
    pub async fn request_signup(
        &self,
        request: SignupRequest,
        confirm_base: &str,
        locale: erp_i18n::Locale,
    ) -> Result<(PendingSignup, SignupToken), SignupError> {
        let SignupRequest {
            email,
            password,
            slug,
            company,
            modules,
        } = request;
        let handle = email.trim().to_lowercase();

        // Before the slow parts, and before anything is written: a name that is
        // already a tenant is not going to become one.
        if self.tenant_by_slug(&slug).await?.is_some() {
            return Err(AccessError::SlugTaken(slug).into());
        }

        // An address with an account has to prove it, here rather than at
        // confirmation. Two reasons and both matter: it stops this being a way
        // to mail strangers, and by the time the link is opened there is no
        // password left to check it against.
        let identity = match self.identity_for_handle(&handle).await? {
            Some(existing) => {
                self.authenticate(&handle, &password).await?;
                Some(existing)
            }
            None => None,
        };

        // The hash only when there is no account. Storing one beside an
        // identity would be a second password for the same login, and the
        // constraint refuses it.
        let secret = match identity {
            Some(_) => None,
            None => Some(hash_password(&password)?),
        };

        let (token, digest) = SignupToken::mint()?;
        let names: Vec<String> = modules.iter().map(|m| m.module.to_string()).collect();
        let seconds = i64::try_from(SIGNUP_LIFETIME.as_secs()).unwrap_or(i64::MAX);
        let interval = i64::try_from(REQUEST_INTERVAL.as_secs()).unwrap_or(i64::MAX);
        let id = Uuid::now_v7();

        let mut tx = self.pool.begin().await.map_err(AccessError::Database)?;
        Self::supersede_live_request(&mut tx, &handle, interval).await?;

        let row = sqlx::query!(
            "INSERT INTO pending_signup
                 (id, token_hash, handle, slug, company, modules,
                  identity_id, password_hash, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
                     now() + ($9::BIGINT * INTERVAL '1 second'))
             RETURNING expires_at, created_at",
            id,
            digest,
            handle,
            slug,
            company,
            &names,
            identity.map(|i| *i.as_uuid()),
            secret,
            seconds,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(AccessError::Database)?;

        // The email, in the same transaction as the row it is about. D9, and
        // the reason the control plane has an outbox at all: sending inline
        // would either mail somebody about a signup that rolled back, or lose
        // the send to a crash with nothing recording it was owed.
        //
        // The key is this request's own id, so the row and the promise are
        // one-to-one. Re-requesting writes a new id and therefore a new email,
        // which is right — the previous link was just cancelled.
        let (subject, body) =
            crate::mail::signup_messages(&company, &format!("{confirm_base}{}", token.expose()));
        let email =
            crate::mail::Email::rendered(&crate::CATALOG, locale, handle.clone(), &subject, &body);
        erp_eventlog::enqueue(&mut tx, None, &[email.promised(format!("signup:{id}"))])
            .await
            .map_err(|e| AccessError::Corrupt(e.to_string()))?;

        tx.commit().await.map_err(AccessError::Database)?;

        // Attributed to the system, because there is no identity yet in the
        // case this exists for. The handle is the subject, so an operator
        // looking at a burst of these can see what was being aimed at.
        self.record(
            Actor::system(),
            "signup.requested",
            "handle",
            &handle,
            serde_json::json!({ "slug": slug, "modules": names }),
        )
        .await?;

        Ok((
            PendingSignup {
                id,
                handle,
                slug,
                company,
                modules: names,
                expires_at: row.expires_at,
                created_at: row.created_at,
            },
            token,
        ))
    }

    /// Makes room for a new request, or refuses because one is too recent.
    ///
    /// # Why this is inside the caller's transaction
    ///
    /// **The interval is read where the row would be replaced.** Reading it on
    /// its own connection would leave a window in which two requests both see
    /// nothing recent and both send, which is the one thing the interval
    /// exists to prevent.
    ///
    /// `FOR UPDATE` makes the row itself the lock — the partial unique index
    /// guarantees there is at most one — so whichever transaction takes it
    /// decides, and the other waits and then sees what it did.
    ///
    /// The cancel is here too, and in the same transaction as the insert that
    /// follows it, for the reason `invite` does the same one table over: the
    /// index allows one live row per address, so two commits would leave a
    /// window where the insert fails and the previous request is already gone.
    async fn supersede_live_request(
        tx: &mut sqlx::PgConnection,
        handle: &str,
        interval: i64,
    ) -> Result<(), SignupError> {
        let live = sqlx::query!(
            "SELECT created_at,
                    EXTRACT(EPOCH FROM now() - created_at)::BIGINT AS \"age!\"
               FROM pending_signup
              WHERE handle = $1 AND confirmed_at IS NULL AND cancelled_at IS NULL
                FOR UPDATE",
            handle,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(AccessError::Database)?;

        if let Some(row) = live
            && row.age < interval
        {
            return Err(SignupError::TooSoon {
                sent: row.age,
                retry_in: interval - row.age,
            });
        }

        sqlx::query!(
            "UPDATE pending_signup SET cancelled_at = now()
              WHERE handle = $1 AND confirmed_at IS NULL AND cancelled_at IS NULL",
            handle,
        )
        .execute(&mut *tx)
        .await
        .map_err(AccessError::Database)?;

        Ok(())
    }

    /// The modules a live request asked for, without claiming it.
    ///
    /// # Why this is separate from confirming
    ///
    /// Because the stored names have to become [`ModuleSetup`]s before anything
    /// can be installed, and only the composition root knows how — it is the
    /// one crate that names every module, and the control plane holds no domain
    /// (D11). So the caller reads the names, resolves them through the same
    /// lookup signup used, and hands the setups back.
    ///
    /// That resolution is not a formality. A module withdrawn between the
    /// request and the click is refused here, rather than installed from a
    /// description nothing offers any more.
    ///
    /// A token spent between this call and [`Self::confirm_signup`] is answered
    /// by the claim in that function, which is the authoritative one. This can
    /// only be optimistic, and nothing is written on the strength of it.
    pub async fn pending_signup_modules(&self, token: &str) -> Result<Vec<String>, SignupError> {
        sqlx::query_scalar!(
            "SELECT modules FROM pending_signup
              WHERE token_hash = $1
                AND confirmed_at IS NULL AND cancelled_at IS NULL
                AND expires_at > now()",
            SignupToken::digest_of(token),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AccessError::Database)?
        .ok_or(SignupError::NotValid)
    }

    /// Proves the address, and builds everything the request asked for.
    ///
    /// The account, the tenant, its database, its modules and a session, in the
    /// operation that compensates if any part of it fails — which is what
    /// [`Self::sign_up`] has always been. This is the only caller that reaches
    /// it from outside.
    ///
    /// # The claim comes first
    ///
    /// One `UPDATE … WHERE confirmed_at IS NULL` decides the winner before any
    /// building starts, so two clicks on the same link cannot both provision.
    /// Losing the race and losing the link are the same answer, [`NotValid`],
    /// because they are the same thing from where the caller stands.
    ///
    /// A failure **unclaims** it. Provisioning can fail on a slug somebody took
    /// in the meantime, and burning the link over that would turn a recoverable
    /// error into a support ticket.
    ///
    /// [`NotValid`]: SignupError::NotValid
    pub async fn confirm_signup(
        &self,
        token: &str,
        modules: Vec<ModuleSetup>,
    ) -> Result<Confirmed, SignupError> {
        let claimed = sqlx::query!(
            r#"UPDATE pending_signup SET confirmed_at = now()
                WHERE token_hash = $1
                  AND confirmed_at IS NULL AND cancelled_at IS NULL
                  AND expires_at > now()
            RETURNING id, handle, slug, company,
                      identity_id as "identity_id: IdentityId",
                      password_hash"#,
            SignupToken::digest_of(token),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AccessError::Database)?
        .ok_or(SignupError::NotValid)?;

        let built = self
            .build_signup(
                claimed.identity_id,
                claimed.password_hash,
                claimed.handle.clone(),
                claimed.slug.clone(),
                claimed.company,
                modules,
            )
            .await;

        match built {
            Ok(confirmed) => {
                self.record(
                    Actor::identity(confirmed.identity),
                    "signup.confirmed",
                    "tenant",
                    &confirmed.tenant.id.to_string(),
                    serde_json::json!({ "handle": claimed.handle, "slug": claimed.slug }),
                )
                .await?;
                Ok(confirmed)
            }
            Err(e) => {
                sqlx::query!(
                    "UPDATE pending_signup SET confirmed_at = NULL WHERE id = $1",
                    claimed.id,
                )
                .execute(&self.pool)
                .await
                .map_err(AccessError::Database)?;
                Err(e)
            }
        }
    }

    /// The account and everything under it, once the address is proved.
    ///
    /// Split out so [`Self::confirm_signup`] can put the claim on one side of
    /// it and the unclaim on the other, and so the two ways an owner is named —
    /// an account that already existed, or a hash waiting to become one — are
    /// resolved in one place.
    async fn build_signup(
        &self,
        existing: Option<IdentityId>,
        secret: Option<String>,
        handle: String,
        slug: String,
        company: String,
        modules: Vec<ModuleSetup>,
    ) -> Result<Confirmed, SignupError> {
        let identity = match (existing, secret) {
            // An account that was already there and proved its password when
            // the request was made. Nothing to create.
            (Some(id), _) => id,
            (None, Some(secret)) => {
                let created = self.create_identity(Actor::system()).await?;
                self.register_hashed_login(created.id, handle, secret)
                    .await?;
                created.id
            }
            // The constraint refuses both-null and both-set, so this is a row
            // that cannot exist. Corrupt rather than unreachable: it is stored
            // data, and stored data is exactly where "it was valid when we
            // wrote it" stops being a guarantee.
            (None, None) => {
                return Err(AccessError::Corrupt(
                    "pending_signup names neither an identity nor a password".to_owned(),
                )
                .into());
            }
        };

        let tenant = self.provision(slug, company, identity, modules).await?;

        let (token, session) = self
            .start_session(identity)
            .await
            .map_err(|e| AccessError::Corrupt(e.to_string()))?;

        Ok(Confirmed {
            tenant,
            identity,
            token,
            session,
        })
    }

    /// Forgets requests nobody answered. For the reaper.
    ///
    /// Deleted rather than marked: an unanswered signup holds a password hash
    /// and an address somebody typed, and there is nothing to learn from
    /// keeping either. The audit entry from `signup.requested` stays, which is
    /// the part an operator watching a burst of these actually reads.
    ///
    /// Returns how many went.
    pub async fn sweep_signups(&self) -> Result<u64, AccessError> {
        Ok(sqlx::query!(
            "DELETE FROM pending_signup
              WHERE confirmed_at IS NULL AND expires_at < now()",
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}
