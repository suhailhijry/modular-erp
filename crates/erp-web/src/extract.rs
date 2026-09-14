//! What a handler gets to assume, and who checked it.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use erp_control::{Lane, Session, TenantDb};
use erp_i18n::Locale;

use crate::error::ApiError;
use crate::problem::Problem;
use erp_types::{AggregateId, ModuleId, TenantId};

use crate::state::AppState;

/// The caller's language, from `Accept-Language`.
///
/// Infallible: an absent or unparseable header is English, not a 400. Extracted
/// on its own so an error response can be localized even when the *next*
/// extractor is what failed.
#[derive(Debug, Clone, Copy)]
pub struct Language(pub Locale);

impl<S: Send + Sync> FromRequestParts<S> for Language {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .headers
                .get(header::ACCEPT_LANGUAGE)
                .and_then(|v| v.to_str().ok())
                .map_or(Locale::DEFAULT, Locale::from_accept_language),
        ))
    }
}

/// The identity a write creates its record under, from `Idempotency-Key`.
///
/// # Why the client supplies this and does not supply an id
///
/// A create needs two things that look like one: a name for the record, and a
/// way to tell a retry from a new request. This system used to take a single
/// `id` in the body doing both jobs, and the job it did badly was the second —
/// because a human picking `INV-0001` on one till collides with a human picking
/// `INV-0001` on another, and the write that arrived second was silently
/// dropped as a "retry".
///
/// A UUID cannot collide by accident, so making the key a UUID and refusing
/// anything else removes the failure rather than detecting it. What the business
/// calls the record is a separate thing the *server* issues — an invoice number
/// from a gapless series — which is what it always should have been.
///
/// # Why a header and not a field
///
/// Because it is not part of what is being described. A body says what the
/// record is; this says which attempt at saying it. Keeping them apart is also
/// what lets one extractor cover every write instead of every module repeating
/// a field and the rule that goes with it.
///
/// # What it costs to store
///
/// Nothing. It **is** the aggregate id, so telling a retry from a repeat falls
/// out of the event log's own uniqueness constraint — there is no keys table, no
/// expiry, and idempotency is permanent rather than lasting a day. See
/// `erp_eventlog::try_create`, which is where the decision is actually made.
#[derive(Debug, Clone)]
pub struct IdempotencyKey(pub AggregateId);

impl IdempotencyKey {
    /// The header a client sends it in.
    pub const HEADER: &'static str = "idempotency-key";

    /// What the created record is stored under.
    #[must_use]
    pub const fn id(&self) -> &AggregateId {
        &self.0
    }

    /// What `try_create` compares to tell a retry from a collision.
    ///
    /// The key itself. Two requests carrying one key **are** the same request as
    /// far as this system is concerned, which is what the client promised by
    /// sending it, and the aggregate id already carries it.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        self.0.as_str()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for IdempotencyKey {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let locale = Language::from_request_parts(parts, state)
            .await
            .map_or(Locale::DEFAULT, |Language(locale)| locale);

        let sent = parts
            .headers
            .get(Self::HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();

        // A UUID and nothing else. Accepting a free-form string would put the
        // collision back: the whole point is that the caller cannot choose
        // something another caller would also choose.
        uuid::Uuid::parse_str(sent)
            .ok()
            .and_then(|uuid| AggregateId::new(uuid.to_string()).ok())
            .map(Self)
            .ok_or_else(|| {
                crate::wire::bad_request(
                    crate::messages::MISSING_IDEMPOTENCY_KEY,
                    "value",
                    sent,
                    locale,
                )
            })
    }
}

/// Proof that a live session presented a valid token.
///
/// Not cached, unlike every other entry-path lookup: a stale membership for five
/// seconds is survivable, a stale *logout* is not.
#[derive(Debug, Clone)]
pub struct Authenticated {
    /// Who this is.
    ///
    /// **For an API key this is derived and was never issued**: no row in
    /// `session`, no token that could be replayed. It carries the key's machine
    /// identity so everything downstream — membership, roles, the audit trail —
    /// works without learning a second shape.
    pub session: Session,
    pub token: String,
    /// Set when the caller presented an API key rather than a session.
    ///
    /// Carried through to [`Allowed`], which is where the scopes narrow what the
    /// role already allows. A handler never reads it: a key is not a different
    /// kind of caller to a handler, it is a caller with fewer permissions.
    pub key: Option<erp_control::KeyContext>,
}

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let Language(locale) = Language::from_request_parts(parts, state)
            .await
            .unwrap_or(Language(Locale::DEFAULT));

        // **Two surfaces, one session.** A browser holds the token in an
        // `HttpOnly` cookie and everything else sends it as a bearer; they name
        // the same row, so authorization has one answer rather than two.
        //
        // The bearer wins when both are present. A request that took the
        // trouble to send a header meant it, and a stale cookie left in a
        // browser should not quietly override it.
        let token = bearer(parts)
            .or_else(|| cookie(parts, SESSION_COOKIE))
            .ok_or_else(|| {
                ApiError::Auth(erp_control::AuthError::NoSession)
                    .into_problem(locale, &crate::CATALOG)
            })?;

        // **The prefix decides, and it is the key's own.** A session token is
        // hex and an API key says `sk_`, so there is no value that could be
        // tried as both — which is what stops a leaked key being replayed as a
        // session or the reverse.
        if token.starts_with(API_KEY_PREFIX) {
            let key = state
                .control
                .key(&token)
                .await
                .map_err(|e| ApiError::Auth(e).into_problem(locale, &crate::CATALOG))?;

            // **Bounded here, where a key is a name to attribute abuse to.**
            // The interactive surface has a session and a person behind it; an
            // integration has neither and is the thing most likely to loop.
            // This is the primitive Phase 3's signup note was waiting for.
            if let Err(seconds) = state
                .limiter
                .check(&key.tenant.to_string(), &key.public_key)
                .await
            {
                return Err(too_many_requests(seconds, locale));
            }

            return Ok(Self {
                session: Session {
                    identity: key.identity,
                    // Not a session and never stored — see the field's docs.
                    // The instant is the request's, so nothing downstream can
                    // mistake this for something with a life of its own.
                    expires_at: chrono::Utc::now(),
                },
                token,
                key: Some(key),
            });
        }

        let session = state
            .control
            .session(&token)
            .await
            .map_err(|e| ApiError::Auth(e).into_problem(locale, &crate::CATALOG))?;

        Ok(Self {
            session,
            token,
            key: None,
        })
    }
}

/// A route into one tenant, with every access check already passed.
///
/// The extractor *is* the authorization: `ControlPlane::enter` refuses unless
/// the identity is active, the tenant is enterable, and a live membership joins
/// them. A handler taking this has been handed proof of all three, and cannot
/// obtain a `TenantDb` any other way.
///
/// On a module's route it also refuses, `503 request.read_model_rebuilding`,
/// while a read model that route is served from is older than this build's —
/// see [`read_models_current`]. [`Allowed`] comes through here, and
/// [`Public`] asks the same question.
#[derive(Debug)]
pub struct Tenant {
    pub db: TenantDb,
    pub session: Session,
    /// Set when an API key got in. See [`Authenticated::key`].
    pub key: Option<erp_control::KeyContext>,
    /// The subdomain this request arrived on, which is the tenant's name.
    ///
    /// Carried because a handler that has to build a link back into this tenant
    /// — an invitation email is the first — would otherwise have to re-derive it
    /// from the `Host` header it no longer has.
    pub slug: String,
}

impl FromRequestParts<AppState> for Tenant {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let Language(locale) = Language::from_request_parts(parts, state)
            .await
            .unwrap_or(Language(Locale::DEFAULT));
        let auth = Authenticated::from_request_parts(parts, state).await?;

        // **The tenant is the host.** `bassat.erp.com` is Bassat Media
        // Productions, and so is `api.bassat.sa` once they have proved
        // `bassat.sa` — which is why no route carries a `{slug}` any more, and
        // why the same 404 covers "no such tenant" and "not yours".
        let tenant = tenant_of(parts, state, locale).await?;

        let db = state
            .control
            .enter(auth.session.identity, tenant.id, Lane::Interactive)
            .await
            .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
        read_models_current(parts, state, &db, locale).await?;

        Ok(Self {
            db,
            session: auth.session,
            key: auth.key,
            slug: tenant.slug,
        })
    }
}

/// A tenant, reached by one of **its customers**, who has no account here.
///
/// The booking site, the order form — anything a shop's own customers touch.
/// Phase 17's foundation, and the first thing in this build to open a tenant
/// without a person behind it.
///
/// # What makes this safe, and why it is a separate type
///
/// **There is no access on the handle.** `TenantDb::role()` is `None`, so
/// `allows()` refuses every capability. A public handler therefore cannot reach
/// a guarded command by forgetting something: it has to call a module function
/// directly, which is a visible line of code rather than a missing one.
///
/// It is a separate extractor from [`Tenant`] rather than a flag on it, because
/// the two answer different questions and a boolean would let a handler written
/// for one silently accept the other. A handler that takes this **is** the
/// declaration that it is public, and
/// `only_the_deliberately_public_routes_are_public` is the test that keeps the
/// list of them honest.
///
/// # It still has to be a real tenant, and a live one
///
/// The subdomain names it exactly as it does everywhere else — see
/// [`subdomain`] for why that header is safe to trust with a *name*. A
/// suspended tenant's public page goes dark, which is the difference between
/// this and maintenance access: suspension stops people using the system, and
/// a booking form is people using the system.
///
/// # It is bounded, per caller and per business
///
/// There is no session to attribute abuse to, so the bound is keyed on the
/// caller's **address** — see [`caller_address`] for why that is the only header
/// a caller cannot write — and on the tenant being reached. Both are charged
/// here, in the extractor, so a public route added tomorrow is bounded without
/// anybody remembering to bound it; `every_public_route_is_rate_limited` is the
/// test that would notice if one were not.
#[derive(Debug)]
pub struct Public {
    pub db: TenantDb,
    /// The subdomain this arrived on, which is the tenant's name.
    pub slug: String,
    /// Where the request came from, as [`caller_address`] answers it.
    pub address: String,
    locale: Locale,
}

impl Public {
    /// **One more text is about to be sent because of this caller.**
    ///
    /// A code is money — the business's on this surface — and a per-number
    /// cooldown bounds only how often *one* number is texted. This bounds how
    /// many numbers one address may cause texts to, and how many the platform
    /// sends in an hour at all: the second is the circuit breaker for a caller
    /// whose own premium numbers are the ones receiving the codes.
    pub async fn charge_for_a_code(&self, state: &AppState) -> Result<(), Problem> {
        charge_for_a_code(state, &self.address, self.locale).await
    }
}

impl FromRequestParts<AppState> for Public {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let Language(locale) = Language::from_request_parts(parts, state)
            .await
            .unwrap_or(Language(Locale::DEFAULT));

        let tenant = tenant_of(parts, state, locale).await?;

        // **Charged here and not in each handler**, for the same reason the
        // branch is read here: a public route added tomorrow is bounded without
        // anybody remembering to bound it. Charged *after* the tenant resolves,
        // so a flood aimed at names that do not exist cannot consume a real
        // tenant's budget — and before the database is opened, so a refused
        // request costs no connection.
        let address = caller_address(parts, state);
        if let Err(seconds) = state.limiter.check(tenant.slug.as_str(), &address).await {
            return Err(too_many_requests(seconds, locale));
        }

        let db = state
            .control
            .enter_for_the_public(tenant.id)
            .await
            .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
        read_models_current(parts, state, &db, locale).await?;

        Ok(Self {
            db,
            slug: tenant.slug,
            address,
            locale,
        })
    }
}

/// **A module's route is not served from a read model this build no longer
/// projects.** Decision 7 of 2026-09-11: while the tenant's tables for the
/// module — or for any module whose code its routes run: `ModuleSetup::reads`,
/// and what `erp-api` composes under its path — were built for an older
/// read-model version than [`AppState::read_models`] says, the answer is
/// `503 request.read_model_rebuilding`, never numbers worked out by rules this
/// build has replaced. Other modules' routes are untouched.
///
/// Asked after entry, so only a caller who may be here learns the module is
/// being rebuilt, and on the module [`module_of`] finds — the same answer the
/// capability check uses. A path that is no module the tenant has is not
/// checked: its handler answers 404.
///
/// Per request, one cache read per group, and a query only for a group not
/// known current — see `ControlPlane::read_model_behind` for why a stale
/// answer is never cached and a finished rebuild is served at once.
async fn read_models_current(
    parts: &Parts,
    state: &AppState,
    db: &TenantDb,
    locale: Locale,
) -> Result<(), Problem> {
    let Some(module) = module_of(parts.uri.path(), db.modules()) else {
        return Ok(());
    };
    let Some(wanted) = state.read_models.get(&module) else {
        return Ok(());
    };
    let behind = state
        .control
        .read_model_behind(db, wanted)
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
    let Some((group, installed)) = behind else {
        return Ok(());
    };

    // A warning per request, not an error: the alarm is the worker's stalled
    // job for the same group, once a visit, and this would repeat it for
    // every caller.
    tracing::warn!(
        tenant = %db.tenant(),
        module = module.as_str(),
        group,
        installed,
        "refusing a module's route: a read model it serves from is older than this build's; \
         `just migrate-fleet` rebuilds it"
    );
    Err(Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        &erp_i18n::Message::new(crate::messages::READ_MODEL_REBUILDING).with(
            "module",
            erp_i18n::MessageArg::text(module.as_str().to_owned()),
        ),
        locale,
        &crate::CATALOG,
    ))
}

/// **Where a request came from, as something the caller did not write.**
///
/// # Why this is the key and `Origin` was not
///
/// The first limiter keyed on `Origin`, which is whatever the client sends: a
/// flood rotated it and had a fresh budget per request, and a caller that sent
/// none shared one bucket with every legitimate non-browser client. An address
/// is the one thing about a request the caller cannot choose.
///
/// # Which address
///
/// Behind a proxy this deployment runs, the client is the **last** entry of
/// `X-Forwarded-For` — the one that proxy appended. Earlier entries are
/// whatever the client sent and are ignored. Whether there is such a proxy is
/// [`AppState::trust_forwarded`], set from the environment, because a header
/// trusted with no proxy in front is a header the caller writes.
///
/// Without one, the socket's peer address, which axum supplies when the server
/// is started with `into_make_service_with_connect_info`. A build that started
/// it any other way, or a test driving the router directly, has neither and
/// gets `"unknown"` — one shared bucket, which is the per-node limit at its
/// weakest and still a limit.
pub(crate) fn caller_address(parts: &Parts, state: &AppState) -> String {
    if state.trust_forwarded
        && let Some(forwarded) = parts
            .headers
            .get(FORWARDED_FOR)
            .and_then(|v| v.to_str().ok())
            .and_then(|chain| chain.rsplit(',').next())
            .map(str::trim)
            .filter(|last| !last.is_empty())
    {
        return forwarded.to_owned();
    }

    parts
        .extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map_or_else(|| "unknown".to_owned(), |info| info.0.ip().to_string())
}

/// The header a trusted proxy appends the client's address to.
pub const FORWARDED_FOR: &str = "x-forwarded-for";

/// **A caller with no session and no business**, on the authentication surface.
///
/// # What this is for
///
/// Logging in, signing up, accepting an invitation, asking for a code and
/// verifying one all happen before there is anybody to blame, and every one of
/// them either hashes a password or sends a text. The first version of this API
/// let all of them run unbounded, which made each a password oracle with
/// Argon2 attached and the OTP route a phone bill somebody else pays.
///
/// So this is the extractor every one of them takes, and taking it **is** the
/// bound: the per-address budget ([`crate::rate::AUTH_PER_CALLER`]) is charged
/// before the handler runs, and a handler that names an account or a number
/// charges that too through [`Self::charge_for_handle`] — because the attack on
/// one account comes from many addresses and the bound has to follow the
/// account.
///
/// # Why it is a type
///
/// The same reason [`Allowed`] is. A handler that takes this cannot forget to
/// bound itself, and `every_public_route_is_rate_limited` refuses a build with
/// a public route that takes neither this nor [`Public`].
#[derive(Debug)]
pub struct Anonymous {
    /// Where the request came from, as [`caller_address`] answers it.
    pub address: String,
    locale: Locale,
}

impl Anonymous {
    /// **One attempt against this account or number**, whoever is making it.
    ///
    /// Call it before hashing the password or looking the number up, so a
    /// refused attempt costs nothing but the lookup in the limiter. The handle is
    /// lowercased and trimmed, the way the authenticators store it, so
    /// `Ali@Example.com` and `ali@example.com` are one budget.
    pub async fn charge_for_handle(&self, state: &AppState, handle: &str) -> Result<(), Problem> {
        let handle = handle.trim().to_lowercase();
        if let Err(seconds) = state
            .limiter
            .charge(&format!("handle:{handle}"), crate::rate::AUTH_PER_HANDLE)
            .await
        {
            return Err(too_many_requests(seconds, self.locale));
        }
        Ok(())
    }

    /// **One more text is about to be sent because of this caller.** See
    /// [`Public::charge_for_a_code`].
    pub async fn charge_for_a_code(&self, state: &AppState) -> Result<(), Problem> {
        charge_for_a_code(state, &self.address, self.locale).await
    }
}

impl FromRequestParts<AppState> for Anonymous {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let Language(locale) = Language::from_request_parts(parts, state)
            .await
            .unwrap_or(Language(Locale::DEFAULT));
        let address = caller_address(parts, state);
        if let Err(seconds) = state
            .limiter
            .charge(&format!("auth:{address}"), crate::rate::AUTH_PER_CALLER)
            .await
        {
            return Err(too_many_requests(seconds, locale));
        }
        Ok(Self { address, locale })
    }
}

/// The two bounds on sending a code: this address's, and the platform's.
async fn charge_for_a_code(state: &AppState, address: &str, locale: Locale) -> Result<(), Problem> {
    if let Err(seconds) = state
        .limiter
        .charge(&format!("codes:{address}"), crate::rate::CODES_PER_CALLER)
        .await
    {
        return Err(too_many_requests(seconds, locale));
    }
    if let Err(seconds) = state
        .limiter
        .charge("codes", crate::rate::CODES_PER_PLATFORM)
        .await
    {
        tracing::error!(
            address,
            "the platform-wide one-time-code breaker tripped; somebody is pumping texts"
        );
        return Err(too_many_requests(seconds, locale));
    }
    Ok(())
}

/// The tenant's name, from the host a request arrived on.
///
/// # Why the `Host` header is safe to trust with this
///
/// It is not trusted with anything. A forged host reaches a tenant the caller is
/// **already a member of**, or it reaches nothing: `ControlPlane::enter` is what
/// decides (`admit`, for [`ManagesTenant`]), and it is the same check a forged
/// `{slug}` used to run into. What a host does is *name* a tenant, and the name
/// has never been the secret.
///
/// # Where it comes from
///
/// `Host` on HTTP/1.1, and the URI's authority on HTTP/2, where `Host` is often
/// absent because `:authority` replaced it. A reverse proxy in front of this has
/// to pass one of them through unchanged — if it rewrites the host to its own,
/// every tenant-scoped request becomes a 404, which is at least loud.
/// The slug, when the host is a label under the platform domain.
#[cfg(test)]
fn subdomain(parts: &Parts, domain: &str) -> Option<String> {
    tenant_label(&host_of(parts)?, domain)
}

/// The host a request was addressed to, from `Host` or the URI.
fn host_of(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| parts.uri.host().map(str::to_owned))
}

/// **The tenant a host names.** A label under the platform domain
/// (`bassat.erp.com`) is a slug; anything else is a custom host, which reaches
/// a tenant only if it is under a domain that tenant has **proved** — see
/// `erp_control::domains`. Unproved, unclaimed and lookalike hosts are all the
/// same `None`.
pub async fn tenant_of_host(
    state: &AppState,
    host: &str,
) -> Result<Option<erp_control::Tenant>, erp_control::AccessError> {
    match tenant_label(host, &state.domain) {
        Some(slug) => state.control.tenant_by_slug(&slug).await,
        None => state.control.tenant_by_host(host).await,
    }
}

async fn tenant_of(
    parts: &Parts,
    state: &AppState,
    locale: Locale,
) -> Result<erp_control::Tenant, Problem> {
    let host = host_of(parts).ok_or_else(|| not_found(locale))?;
    tenant_of_host(state, &host)
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?
        .ok_or_else(|| {
            ApiError::Access(erp_control::AccessError::NoSuchTenant)
                .into_problem(locale, &crate::CATALOG)
        })
}

/// The tenant label in a host, or nothing.
///
/// Split out from [`subdomain`] so the CORS middleware — which has a `HeaderMap`
/// and not a `Parts` — asks the same question through the same code. Two
/// implementations of "which tenant is this host" is how one of them comes to
/// admit `a.b.acme.erp.com`.
pub(crate) fn tenant_label(host: &str, domain: &str) -> Option<String> {
    // A port is not part of the name: `acme.localhost:8080` in development is
    // the same tenant as `acme.localhost`. Nor is a trailing dot.
    let host = host.split(':').next().unwrap_or(host).trim().to_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);

    // The apex is not a tenant. It is where signing up and logging in happen.
    let label = host.strip_suffix(domain)?.strip_suffix('.')?;

    // Exactly one label. `a.b.acme.erp.com` is not a tenant, and treating it as
    // one would let arbitrary nesting under a wildcard certificate name things.
    (!label.is_empty() && !label.contains('.')).then(|| label.to_owned())
}

/// The same 404 a genuinely missing tenant gets.
fn not_found(locale: Locale) -> Problem {
    Problem::new(
        StatusCode::NOT_FOUND,
        &erp_i18n::Message::new(erp_control::messages::ACCESS_DENIED),
        locale,
        &crate::CATALOG,
    )
}

/// The bearer token, if the header is well formed.
/// What an API key's private half starts with.
///
/// The one place this crate knows the shape, and it is a prefix rather than a
/// length or a charset: it has to be something a secret scanner can grep for in
/// a repository, which is the whole reason keys have prefixes at all.
const API_KEY_PREFIX: &str = "sk_";

/// The cookie a browser session lives in.
///
/// Named here as well as where it is set, because this is the only place that
/// *reads* it and a constant defined in the writer would make `erp-web` depend
/// on `erp-api`.
pub(crate) const SESSION_COOKIE: &str = "erp_session";

/// One cookie out of a `Cookie` header.
///
/// Written rather than pulled in: the header is `a=1; b=2`, the name is a
/// literal, and the parser is four lines. **No percent-decoding**: a session
/// token is hex, so anything that needed decoding is not one.
fn cookie(parts: &Parts, name: &str) -> Option<String> {
    parts
        .headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The 429, in one place.
///
/// The wait is in the message's `args`, not a `Retry-After` header — which is
/// what signup's 429 already does, and one shape for one answer beats a second
/// mechanism for the same fact.
fn too_many_requests(seconds: u64, locale: Locale) -> Problem {
    Problem::new(
        StatusCode::TOO_MANY_REQUESTS,
        &erp_i18n::Message::new(crate::messages::TOO_MANY_REQUESTS).with(
            "seconds",
            erp_i18n::MessageArg::Count(i64::try_from(seconds).unwrap_or(i64::MAX)),
        ),
        locale,
        &crate::CATALOG,
    )
}

fn bearer(parts: &Parts) -> Option<String> {
    let value = parts.headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_owned())
        .filter(|t| !t.is_empty())
}

// ---------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------

/// A capability, as a type.
///
/// One marker per thing a caller might be allowed to do. The point of the type
/// is [`Allowed`]: `Allowed<PostEntries>` in a handler's signature *is* the
/// check, so the failure mode is a compile error rather than a forgotten line.
pub trait Capability {
    const CAPABILITY: erp_control::Capability;
}

macro_rules! capability {
    ($(#[$doc:meta])* $name:ident => $variant:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub struct $name;

        impl Capability for $name {
            const CAPABILITY: erp_control::Capability = erp_control::Capability::$variant;
        }
    };
}

capability! {
    /// See the tenant and everything in it.
    Read => Read
}
capability! {
    /// Record what happened — journal entries, and later documents.
    PostEntries => PostEntries
}
capability! {
    /// Change the shape of the books: open, rename and close accounts, install
    /// a chart.
    ManageAccounts => ManageAccounts
}
capability! {
    /// Change the tenant: who has access, which modules, what it pays for.
    ManageTenant => ManageTenant
}

/// A tenant handle the caller is allowed to use for `C`.
///
/// # Why this is a type and not a call
///
/// `Tenant` proves *membership*. This proves membership **and** that the role
/// on it permits `C`. A handler taking `Allowed<PostEntries>` cannot be reached
/// by a viewer, and cannot be written to skip the check, because there is no
/// other way to get one.
///
/// The alternative — `tenant.require(Capability::PostEntries)?` on the first
/// line — fails by omission: silent, security-relevant, and invisible in review.
/// Same argument as `TenantDb` having no public constructor.
///
/// Derefs to [`Tenant`], so a handler still reaches `.db` and `.session`.
#[derive(Debug)]
pub struct Allowed<C: Capability> {
    tenant: Tenant,
    /// Which branch this request is for, from `X-Branch`.
    ///
    /// **On the authorization extractor and not on each handler**, so every
    /// write in the system carries it without forty handlers remembering to.
    /// It is not validated here — `erp-web` is core and knows nothing of
    /// modules — but `ledger::post_entry_in` refuses one that names no open
    /// branch, and every posting in the system arrives there.
    ///
    /// It is also where a person scoped to one branch would be refused another,
    /// which is why it sits beside the capability check rather than beyond it.
    pub branch: Option<AggregateId>,
    capability: std::marker::PhantomData<C>,
}

impl<C: Capability> Allowed<C> {
    /// **Narrows again, with a fact the edge could not know.**
    ///
    /// An amount is in the request body — or, for a reversal, in the entry it
    /// undoes — which the extractor has not read when it decides. So a limit like *"a bookkeeper may post entries under ten
    /// thousand riyals"* is checked here, by the handler that has parsed one.
    ///
    /// The branch and the capability are supplied again, and `TenantDb::permits`
    /// adds the role, so a rule naming any combination of the four sees all of
    /// them.
    ///
    /// # Errors
    /// `403` naming the capability when a limit refuses, or `503` when the
    /// tenant's limits cannot be read — refused rather than ignored, for the
    /// reason `TenantDb::permits` gives.
    pub async fn still_permits(
        &self,
        module: Option<&erp_types::ModuleId>,
        extra: impl IntoIterator<Item = (&'static str, erp_rules::Value)>,
        locale: Locale,
    ) -> Result<(), Problem> {
        let mut facts = erp_tenant::limits::facts_at(
            C::CAPABILITY,
            self.branch.as_ref().map(AggregateId::as_str),
        );
        for (name, value) in extra {
            facts = facts.with(name, value);
        }

        let permitted = self
            .tenant
            .db
            .permits(C::CAPABILITY, module, &facts)
            .await
            .map_err(|e| {
                Problem::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &erp_i18n::Message::new(erp_control::messages::INTERNAL)
                        .with("detail", erp_i18n::MessageArg::text(e.to_string())),
                    locale,
                    &crate::CATALOG,
                )
            })?;

        if permitted {
            Ok(())
        } else {
            Err(not_permitted(C::CAPABILITY, locale))
        }
    }
}

impl<C: Capability> std::ops::Deref for Allowed<C> {
    type Target = Tenant;
    fn deref(&self) -> &Self::Target {
        &self.tenant
    }
}

/// Which module a request is about, from its path.
///
/// # Why the path decides
///
/// `/v1/sales/invoices` is a sales request; `/v1/members` is not any
/// module's business. The URL namespace *is* the module namespace,
/// by construction — every module mounts under its own name — so reading it
/// here means a module route added tomorrow is scoped without anybody
/// remembering to scope it.
///
/// The alternative, an explicit marker on each handler, fails the other way: a
/// handler that forgets it silently gets the *tenant-wide* role, which is the
/// more permissive answer. Forgetting must never be the permissive option.
///
/// `module_paths_are_what_they_look_like` pins the mapping, so a route that
/// moves changes a test rather than changing permissions quietly.
///
/// # Why the tenant's own modules are the list
///
/// It used to be the *build's* list, read from `erp_api::modules()` — which is
/// above this crate now that a module ships its own routes, and cannot be
/// reached from here without closing a dependency cycle.
///
/// The tenant's list is the better answer anyway, and gives the same one where
/// it matters: a segment that is not a module the tenant has is judged on the
/// **tenant-wide** role, exactly as `/v1/members` is, and then the handler's own
/// `require_module` answers 404. So a request for a module the tenant does not
/// have cannot reach data by any route, and the reply says the honest thing —
/// that route does not exist here — rather than "forbidden", which would confirm
/// what they are not paying for.
fn module_of(path: &str, enabled: &erp_control::EnabledModules) -> Option<ModuleId> {
    // /v1/{module}/...
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    if segments.next()? != "v1" {
        return None;
    }
    let candidate = ModuleId::new(segments.next()?).ok()?;

    // An unknown segment is a route that does not exist, and treating it as a
    // module would let a request opt out of its tenant-wide role by inventing a
    // path.
    enabled.contains(&candidate).then_some(candidate)
}

impl<C: Capability> FromRequestParts<AppState> for Allowed<C> {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let Language(locale) = Language::from_request_parts(parts, state)
            .await
            .unwrap_or(Language(Locale::DEFAULT));
        let tenant = Tenant::from_request_parts(parts, state).await?;
        let module = module_of(parts.uri.path(), tenant.db.modules());

        // **Scopes narrow, they never widen.** The role check below still has
        // to pass; this is a second gate in front of it, so an integration
        // scoped to `booking:read` cannot post journal entries even if somebody
        // gives its identity the owner's role by mistake.
        if let Some(key) = &tenant.key
            && !key.permits(
                C::CAPABILITY,
                module.as_ref().map(erp_types::ModuleId::as_str),
            )
        {
            return Err(out_of_scope(module.as_ref(), C::CAPABILITY, locale));
        }

        // **Parsed before the check, because it is one of the facts.** A limit
        // like "only their own branch" cannot be evaluated by a check that has
        // not yet read `X-Branch`.
        let branch = parts
            .headers
            .get(BRANCH_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .map(|raw| {
                AggregateId::new(raw).map_err(|_| {
                    crate::wire::bad_request(crate::messages::INVALID_ID, "branch", raw, locale)
                })
            })
            .transpose()?;

        // **What the edge knows.** An amount is in a body this extractor has
        // not read, so a limit about one is narrowed later by the handler that
        // learns it — see `Allowed::still_permits`.
        let facts =
            erp_tenant::limits::facts_at(C::CAPABILITY, branch.as_ref().map(AggregateId::as_str));

        let permitted = tenant
            .db
            .permits(C::CAPABILITY, module.as_ref(), &facts)
            .await
            .map_err(|e| {
                // **Refused, not ignored.** A tenant who configured limits and
                // stored something unusable must not get the unlimited answer.
                Problem::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &erp_i18n::Message::new(erp_control::messages::INTERNAL)
                        .with("detail", erp_i18n::MessageArg::text(e.to_string())),
                    locale,
                    &crate::CATALOG,
                )
            })?;

        if !permitted {
            // 403, not 404. The caller has already proved they are a member, so
            // hiding the tenant's existence buys nothing — and "you cannot do
            // this" is the answer they need in order to ask someone who can.
            return Err(not_permitted(C::CAPABILITY, locale));
        }

        Ok(Self {
            tenant,
            branch,
            capability: std::marker::PhantomData,
        })
    }
}

/// The 403 a role that does not allow `capability` gets, naming it.
///
/// Public because one route is not decided by its extractor alone:
/// `reset_member_second_factor` is the owner's **or** a claim-holder's, and a
/// claim lives in the tenant's own database, which this crate cannot reach. It
/// answers with this rather than a second shape, so every "you may not" in the
/// API is one sentence with one argument in it.
pub fn not_permitted(capability: erp_control::Capability, locale: Locale) -> Problem {
    Problem::new(
        StatusCode::FORBIDDEN,
        &erp_i18n::Message::new(erp_control::messages::NOT_PERMITTED).with(
            "capability",
            erp_i18n::MessageArg::text(capability.as_str()),
        ),
        locale,
        &crate::CATALOG,
    )
}

/// The 403 an API key gets from a route that is **a person's act**, whatever
/// its scopes.
///
/// Not the same refusal as [`out_of_scope`], and deliberately not a wider scope
/// away: a key's identity is a machine, so it holds no employee record, no
/// claim, and nobody's trust. Two routes answer with it — the personal audit
/// trail and `reset_member_second_factor` — and both are routes where the
/// caller's *role* is not the question being asked.
pub fn not_a_person(locale: Locale) -> Problem {
    Problem::new(
        StatusCode::FORBIDDEN,
        &erp_i18n::Message::new(erp_control::messages::NOT_A_PERSON),
        locale,
        &crate::CATALOG,
    )
}

/// The 403 an API key whose scopes do not cover this gets, naming the scope
/// it would need.
fn out_of_scope(
    module: Option<&ModuleId>,
    capability: erp_control::Capability,
    locale: Locale,
) -> Problem {
    Problem::new(
        StatusCode::FORBIDDEN,
        &erp_i18n::Message::new(erp_control::messages::OUT_OF_SCOPE).with(
            "scope",
            erp_i18n::MessageArg::text(format!(
                "{}:{}",
                module.map_or("*", ModuleId::as_str),
                capability.as_str()
            )),
        ),
        locale,
        &crate::CATALOG,
    )
}

/// **Somebody who may manage this tenant, whatever state it is in** — for
/// what the control plane keeps *about* a tenant, which never needed its
/// database. Its audit trail is the one route.
///
/// [`Allowed<ManageTenant>`] goes through [`Tenant`] and `ControlPlane::enter`,
/// which answers a tenant that is not active 503. That is right for the
/// tenant's data and wrong for its trail: a suspended tenant's owner reads why
/// there (decision 12 of 2026-09-11), and through `enter` never could. So this
/// asks `ControlPlane::admit` — `enter`'s checks bar the status, and the same
/// second-factor rule — and then `Allowed`'s two gates in `Allowed`'s order: a
/// key's scopes, then the role. It hands out no `TenantDb`, so nothing *in*
/// the tenant is reachable through it.
///
/// **Limits are not consulted**, and would change nothing if they were:
/// `ManageTenant` is the one capability a permission limit never narrows (see
/// `TenantDb::permits`), so the role alone decides here, as it does for
/// `Allowed<ManageTenant>`. The same 404 covers "no such tenant" and "not
/// yours".
#[derive(Debug)]
pub struct ManagesTenant {
    pub session: Session,
    pub tenant: TenantId,
}

impl FromRequestParts<AppState> for ManagesTenant {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        const CAPABILITY: erp_control::Capability = erp_control::Capability::ManageTenant;

        let Language(locale) = Language::from_request_parts(parts, state)
            .await
            .unwrap_or(Language(Locale::DEFAULT));
        let auth = Authenticated::from_request_parts(parts, state).await?;
        let tenant = tenant_of(parts, state, locale).await?;

        let access = state
            .control
            .admit(auth.session.identity, tenant.id)
            .await
            .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
        if let Some(key) = &auth.key
            && !key.permits(CAPABILITY, None)
        {
            return Err(out_of_scope(None, CAPABILITY, locale));
        }
        if !access.allows(CAPABILITY, None) {
            return Err(not_permitted(CAPABILITY, locale));
        }

        Ok(Self {
            session: auth.session,
            tenant: tenant.id,
        })
    }
}

/// The header a request names its branch in.
pub const BRANCH_HEADER: &str = "x-branch";

/// A platform power, as a type — what [`Capability`] is to [`Allowed`], this is
/// to [`Staff`].
pub trait Power {
    const POWER: erp_control::PlatformPower;
}

/// Grant, change and revoke platform staff.
#[derive(Debug, Clone, Copy)]
pub struct ManageStaff;

impl Power for ManageStaff {
    const POWER: erp_control::PlatformPower = erp_control::PlatformPower::ManageStaff;
}

/// Suspend and reinstate tenants.
#[derive(Debug, Clone, Copy)]
pub struct SuspendTenants;

impl Power for SuspendTenants {
    const POWER: erp_control::PlatformPower = erp_control::PlatformPower::SuspendTenants;
}

/// List, requeue and dismiss the control plane's dead letters.
#[derive(Debug, Clone, Copy)]
pub struct HandleDeadLetters;

impl Power for HandleDeadLetters {
    const POWER: erp_control::PlatformPower = erp_control::PlatformPower::HandleDeadLetters;
}

/// Read the whole audit trail, every tenant's and the platform's own.
#[derive(Debug, Clone, Copy)]
pub struct ReadAuditTrail;

impl Power for ReadAuditTrail {
    const POWER: erp_control::PlatformPower = erp_control::PlatformPower::ReadAuditTrail;
}

/// Reset anybody's second factor, with a reason. Resetting platform staff's
/// needs [`ManageStaff`] on top, which `reset_any_second_factor` asks for.
#[derive(Debug, Clone, Copy)]
pub struct ResetSecondFactors;

impl Power for ResetSecondFactors {
    const POWER: erp_control::PlatformPower = erp_control::PlatformPower::ResetSecondFactors;
}

/// **Platform staff permitted `P`**, on a route that is about no tenant.
///
/// The platform's [`Allowed`]: taking one is the check, for the same reason.
/// It asks `ControlPlane::staff_may`, which is also what support access asks,
/// so there is one answer to "may this person do this to the platform" and the
/// HTTP surface cannot drift from it. That refuses unless the identity is
/// active, holds a platform role that may `P`, **and has a second factor** —
/// see `staff_may` for why enrolled is enough.
///
/// **An API key never gets in.** A key is one tenant's integration and acts as
/// a machine identity inside that tenant; nothing about it is staff, and no
/// grant can make it so (staff are granted by login handle, which a key has
/// none of). Refused here anyway, so that stays true without depending on it.
///
/// Like the tenant's own member routes, a session gets no rate limit here and
/// there is no `Idempotency-Key`: none of these writes can happen twice — a
/// repeated grant is a 409 and a repeated revocation a 404.
#[derive(Debug)]
pub struct Staff<P: Power> {
    pub session: Session,
    pub role: erp_control::PlatformRole,
    power: std::marker::PhantomData<P>,
}

impl<P: Power> FromRequestParts<AppState> for Staff<P> {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let Language(locale) = Language::from_request_parts(parts, state)
            .await
            .unwrap_or(Language(Locale::DEFAULT));
        let auth = Authenticated::from_request_parts(parts, state).await?;
        let refused = |e| ApiError::Access(e).into_problem(locale, &crate::CATALOG);

        if auth.key.is_some() {
            return Err(refused(erp_control::AccessError::StaffOnly(P::POWER)));
        }
        let role = state
            .control
            .staff_may(auth.session.identity, P::POWER)
            .await
            .map_err(refused)?;

        Ok(Self {
            session: auth.session,
            role,
            power: std::marker::PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Authorization now depends on URL shape, so the shape is pinned here.**
    ///
    /// A route that moves changes this test rather than changing permissions
    /// quietly, which is the whole price of deriving the module from the path.
    #[test]
    fn module_paths_are_what_they_look_like() {
        let enabled = erp_control::EnabledModules::new(
            ["ledger", "sales", "purchases", "tax_sa"]
                .into_iter()
                .map(|name| ModuleId::new(name).expect("a module id"))
                .collect(),
        );
        let module = |path: &str| module_of(path, &enabled).map(|m| m.as_str().to_owned());

        // Every module's routes, scoped to it.
        assert_eq!(module("/v1/sales/invoices").as_deref(), Some("sales"));
        assert_eq!(
            module("/v1/sales/invoices/INV-1/payments").as_deref(),
            Some("sales")
        );
        assert_eq!(module("/v1/ledger/accounts").as_deref(), Some("ledger"));
        assert_eq!(module("/v1/ledger/chart").as_deref(), Some("ledger"));
        assert_eq!(module("/v1/purchases/bills").as_deref(), Some("purchases"));
        assert_eq!(module("/v1/tax_sa/vat-return").as_deref(), Some("tax_sa"));

        // The tenant's own surface belongs to no module, so it is judged on the
        // tenant-wide role. This is what stops an accountant-for-sales from
        // deciding who else has access.
        for tenant_wide in [
            "/v1/tenant",
            "/v1/members",
            "/v1/members/01a00000-0000-7000-8000-000000000000",
            "/v1/modules",
            "/v1/invitations",
        ] {
            assert_eq!(module(tenant_wide), None, "{tenant_wide}");
        }

        // Nothing outside a module is a module's business either.
        for outside in ["/v1/health", "/v1/sessions", "/v1/signups", "/"] {
            assert_eq!(module(outside), None, "{outside}");
        }

        // **A module the tenant does not have is not a module here.** The
        // request is judged on the tenant-wide role and the handler answers 404,
        // which is the reply that does not confirm what they are not paying for.
        let without =
            erp_control::EnabledModules::new(vec![ModuleId::new("ledger").expect("a module id")]);
        assert_eq!(module_of("/v1/sales/invoices", &without), None);
        assert_eq!(
            module_of("/v1/ledger/accounts", &without).map(|m| m.as_str().to_owned()),
            Some("ledger".to_owned())
        );
    }

    fn host(value: &str) -> Parts {
        let mut request = axum::http::Request::builder();
        if !value.is_empty() {
            request = request.header(header::HOST, value);
        }
        request
            .uri("/v1/tenant")
            .body(())
            .unwrap_or_else(|_| unreachable!("a valid request"))
            .into_parts()
            .0
    }

    /// **Which host names which tenant.**
    ///
    /// The tenant used to be a path segment somebody could mistype; it is a
    /// subdomain now, and the parsing is the one place that decides. Off by one
    /// label here is a request served against the wrong company.
    #[test]
    fn a_tenant_is_exactly_one_label_under_the_domain() {
        let of = |h: &str| subdomain(&host(h), "erp.com");

        assert_eq!(of("bassat.erp.com").as_deref(), Some("bassat"));
        assert_eq!(
            of("BASSAT.ERP.COM").as_deref(),
            Some("bassat"),
            "hosts are case-insensitive and tenants are lower case"
        );
        assert_eq!(
            of("bassat.erp.com:8080").as_deref(),
            Some("bassat"),
            "a port is not part of the name"
        );
        assert_eq!(
            of("bassat.erp.com.").as_deref(),
            Some("bassat"),
            "a fully-qualified name ends in a dot and means the same thing"
        );

        // The apex is where signing up and logging in happen. It is not a
        // tenant, and reading it as one would make `www` a company.
        assert_eq!(of("erp.com"), None);
        assert_eq!(of(""), None, "no host at all");

        // Exactly one label. Nesting under a wildcard certificate must not name
        // anything, or `evil.bassat.erp.com` starts looking addressable.
        assert_eq!(of("a.bassat.erp.com"), None);
        assert_eq!(of(".erp.com"), None);

        // A different domain is not this deployment.
        assert_eq!(of("bassat.example.com"), None);
        assert_eq!(
            of("noterp.com"),
            None,
            "a suffix match is not a subdomain match"
        );
    }

    /// Development runs under `.localhost`, which resolves without touching
    /// `/etc/hosts` in every browser and in curl.
    #[test]
    fn localhost_works_the_same_way() {
        assert_eq!(
            subdomain(&host("acme.localhost"), "localhost").as_deref(),
            Some("acme")
        );
        assert_eq!(subdomain(&host("localhost"), "localhost"), None);
    }

    /// A request cannot opt out of its tenant-wide role by inventing a segment.
    ///
    /// If an unknown segment counted as "some module", a caller held back in
    /// every module they have would find that `/v1/tenants/acme/anything/…`
    /// fell back to a role they were deliberately not given there.
    #[test]
    fn an_invented_module_segment_is_not_a_module() {
        let enabled =
            erp_control::EnabledModules::new(vec![ModuleId::new("sales").expect("a module id")]);
        assert_eq!(module_of("/v1/nonsense/x", &enabled), None);
        assert_eq!(module_of("/v1/Sales/invoices", &enabled), None);
        assert_eq!(module_of("/v1/../sales/invoices", &enabled), None);
    }
}

/// **The version a settings write is conditional on**, from `If-Match`.
///
/// A settings `GET` answers with an `ETag` carrying the setting's version; a
/// client that sends it back as `If-Match` on the `PUT` writes only if nobody
/// else has written since, and is told with `412` if somebody has. Without the
/// header the write is unconditional, which is what a script that owns the
/// setting wants and what a screen two people can have open does not.
///
/// `"12"`, `12` and `W/"12"` all name version twelve; `*` is "whatever is
/// there", the same as no header. Anything else is a `400`, because a client
/// that meant to be conditional and was not is the bug this exists to catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IfMatch(pub Option<i64>);

impl IfMatch {
    pub const HEADER: &'static str = "if-match";

    /// What a header value means: `Ok(None)` is "any version", `Ok(Some(n))`
    /// is version `n`, and `Err(())` is not a version at all.
    fn parse(raw: &str) -> Result<Option<i64>, ()> {
        let raw = raw.trim();
        if raw == "*" {
            return Ok(None);
        }
        let raw = raw.strip_prefix("W/").unwrap_or(raw);
        let raw = raw
            .strip_prefix('"')
            .and_then(|r| r.strip_suffix('"'))
            .unwrap_or(raw);
        raw.parse::<i64>()
            .ok()
            .filter(|v| *v >= 0)
            .map(Some)
            .ok_or(())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for IfMatch {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let sent = parts
            .headers
            .get(Self::HEADER)
            .map(|value| value.to_str().unwrap_or_default().to_owned());
        let Some(raw) = sent else {
            return Ok(Self(None));
        };
        let locale = Language::from_request_parts(parts, state)
            .await
            .map_or(Locale::DEFAULT, |Language(locale)| locale);
        Self::parse(&raw).map(Self).map_err(|()| {
            crate::wire::bad_request(crate::messages::NOT_A_VERSION, "if_match", &raw, locale)
        })
    }
}

#[cfg(test)]
mod if_match_tests {
    use super::IfMatch;

    #[test]
    fn a_version_is_read_the_ways_a_client_writes_one() {
        assert_eq!(IfMatch::parse("\"12\""), Ok(Some(12)));
        assert_eq!(IfMatch::parse("12"), Ok(Some(12)));
        assert_eq!(IfMatch::parse("W/\"12\""), Ok(Some(12)));
        assert_eq!(IfMatch::parse(" \"0\" "), Ok(Some(0)));
        assert_eq!(IfMatch::parse("*"), Ok(None), "any version is no condition");
    }

    #[test]
    fn what_is_not_a_version_is_refused_rather_than_ignored() {
        for bad in ["", "abc", "\"-1\"", "\"1", "1\"", "\"1\", \"2\""] {
            assert_eq!(
                IfMatch::parse(bad),
                Err(()),
                "{bad:?} was read as a version"
            );
        }
    }
}
