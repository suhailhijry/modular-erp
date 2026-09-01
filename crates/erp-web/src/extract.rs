//! What a handler gets to assume, and who checked it.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use erp_control::{Lane, Session, TenantDb};
use erp_i18n::Locale;

use crate::error::ApiError;
use crate::problem::Problem;
use erp_types::{AggregateId, ModuleId};

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
    pub session: Session,
    pub token: String,
}

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let Language(locale) = Language::from_request_parts(parts, state)
            .await
            .unwrap_or(Language(Locale::DEFAULT));

        let token = bearer(parts).ok_or_else(|| {
            ApiError::Auth(erp_control::AuthError::NoSession).into_problem(locale, &crate::CATALOG)
        })?;

        let session = state
            .control
            .session(&token)
            .await
            .map_err(|e| ApiError::Auth(e).into_problem(locale, &crate::CATALOG))?;

        Ok(Self { session, token })
    }
}

/// A route into one tenant, with every access check already passed.
///
/// The extractor *is* the authorization: `ControlPlane::enter` refuses unless
/// the identity is active, the tenant is enterable, and a live membership joins
/// them. A handler taking this has been handed proof of all three, and cannot
/// obtain a `TenantDb` any other way.
#[derive(Debug)]
pub struct Tenant {
    pub db: TenantDb,
    pub session: Session,
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

        // **The tenant is the subdomain.** `bassat.erp.com` is Bassat Media
        // Productions, and every path below it is about them — which is why no
        // route carries a `{slug}` any more.
        let slug = subdomain(parts, &state.domain).ok_or_else(|| not_found(locale))?;

        // Slug → id is one cached lookup, and the same 404 covers "no such
        // tenant" and "not yours".
        let tenant = state
            .control
            .tenant_by_slug(&slug)
            .await
            .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?
            .ok_or_else(|| {
                ApiError::Access(erp_control::AccessError::NoSuchTenant)
                    .into_problem(locale, &crate::CATALOG)
            })?;

        let db = state
            .control
            .enter(auth.session.identity, tenant.id, Lane::Interactive)
            .await
            .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;

        Ok(Self {
            db,
            session: auth.session,
            slug: tenant.slug,
        })
    }
}

/// The tenant's name, from the host a request arrived on.
///
/// # Why the `Host` header is safe to trust with this
///
/// It is not trusted with anything. A forged host reaches a tenant the caller is
/// **already a member of**, or it reaches nothing: `ControlPlane::enter` is what
/// decides, and it is the same check a forged `{slug}` used to run into. What a
/// host does is *name* a tenant, and the name has never been the secret.
///
/// # Where it comes from
///
/// `Host` on HTTP/1.1, and the URI's authority on HTTP/2, where `Host` is often
/// absent because `:authority` replaced it. A reverse proxy in front of this has
/// to pass one of them through unchanged — if it rewrites the host to its own,
/// every tenant-scoped request becomes a 404, which is at least loud.
fn subdomain(parts: &Parts, domain: &str) -> Option<String> {
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| parts.uri.host().map(str::to_owned))?;

    // A port is not part of the name: `acme.localhost:8080` in development is
    // the same tenant as `acme.localhost`. Nor is a trailing dot.
    let host = host
        .split(':')
        .next()
        .unwrap_or(&host)
        .trim()
        .to_lowercase();
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
    capability: std::marker::PhantomData<C>,
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

        if !tenant.db.allows_in(C::CAPABILITY, module.as_ref()) {
            // 403, not 404. The caller has already proved they are a member, so
            // hiding the tenant's existence buys nothing — and "you cannot do
            // this" is the answer they need in order to ask someone who can.
            return Err(Problem::new(
                StatusCode::FORBIDDEN,
                &erp_i18n::Message::new(erp_control::messages::NOT_PERMITTED).with(
                    "capability",
                    erp_i18n::MessageArg::text(C::CAPABILITY.as_str()),
                ),
                locale,
                &crate::CATALOG,
            ));
        }

        Ok(Self {
            tenant,
            capability: std::marker::PhantomData,
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
