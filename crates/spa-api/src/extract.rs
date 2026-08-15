//! What a handler gets to assume, and who checked it.

use std::collections::HashMap;

use axum::extract::{FromRequestParts, Path};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use spa_control::{Lane, Session, TenantDb};
use spa_i18n::Locale;

use crate::error::ApiError;
use crate::problem::Problem;
use spa_types::ModuleId;

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
            ApiError::Auth(spa_control::AuthError::NoSession).into_problem(locale)
        })?;

        let session = state
            .control
            .session(&token)
            .await
            .map_err(|e| ApiError::Auth(e).into_problem(locale))?;

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
}

impl FromRequestParts<AppState> for Tenant {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let Language(locale) = Language::from_request_parts(parts, state)
            .await
            .unwrap_or(Language(Locale::DEFAULT));
        let auth = Authenticated::from_request_parts(parts, state).await?;

        // By name, not by position. `Path<String>` only matches a route with
        // exactly one parameter, so it silently 404s on
        // `/tenants/{slug}/members/{identity}` — every nested route this API
        // will ever grow.
        let Path(params): Path<HashMap<String, String>> = Path::from_request_parts(parts, state)
            .await
            .map_err(|_| not_found(locale))?;
        let slug = params.get("slug").ok_or_else(|| not_found(locale))?;

        // Slug → id is one cached lookup, and the same 404 covers "no such
        // tenant" and "not yours".
        let tenant = state
            .control
            .tenant_by_slug(slug)
            .await
            .map_err(|e| ApiError::Access(e).into_problem(locale))?
            .ok_or_else(|| {
                ApiError::Access(spa_control::AccessError::NoSuchTenant).into_problem(locale)
            })?;

        let db = state
            .control
            .enter(auth.session.identity, tenant.id, Lane::Interactive)
            .await
            .map_err(|e| ApiError::Access(e).into_problem(locale))?;

        Ok(Self {
            db,
            session: auth.session,
        })
    }
}

/// The same 404 a genuinely missing tenant gets.
fn not_found(locale: Locale) -> Problem {
    Problem::new(
        StatusCode::NOT_FOUND,
        &spa_i18n::Message::new(spa_control::messages::ACCESS_DENIED),
        locale,
        &crate::catalog::CATALOG,
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
    const CAPABILITY: spa_control::Capability;
}

macro_rules! capability {
    ($(#[$doc:meta])* $name:ident => $variant:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub struct $name;

        impl Capability for $name {
            const CAPABILITY: spa_control::Capability = spa_control::Capability::$variant;
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
/// `/v1/tenants/{slug}/sales/invoices` is a sales request; `/v1/tenants/{slug}/members`
/// is not any module's business. The URL namespace *is* the module namespace,
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
fn module_of(path: &str) -> Option<ModuleId> {
    // /v1/tenants/{slug}/{module}/...
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    if segments.next()? != "v1" || segments.next()? != "tenants" {
        return None;
    }
    segments.next()?; // the slug
    let candidate = segments.next()?;

    // Only names this build actually offers. An unknown segment is a route that
    // does not exist, and treating it as a module would let a request opt out of
    // its tenant-wide role by inventing a path.
    crate::modules::available()
        .into_iter()
        .find(|(name, _)| *name == candidate)
        .map(|(_, setup)| setup.module)
}

impl<C: Capability> FromRequestParts<AppState> for Allowed<C> {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let Language(locale) = Language::from_request_parts(parts, state)
            .await
            .unwrap_or(Language(Locale::DEFAULT));
        let tenant = Tenant::from_request_parts(parts, state).await?;
        let module = module_of(parts.uri.path());

        if !tenant.db.allows_in(C::CAPABILITY, module.as_ref()) {
            // 403, not 404. The caller has already proved they are a member, so
            // hiding the tenant's existence buys nothing — and "you cannot do
            // this" is the answer they need in order to ask someone who can.
            return Err(Problem::new(
                StatusCode::FORBIDDEN,
                &spa_i18n::Message::new(spa_control::messages::NOT_PERMITTED).with(
                    "capability",
                    spa_i18n::MessageArg::text(C::CAPABILITY.as_str()),
                ),
                locale,
                &crate::catalog::CATALOG,
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
        let module = |path: &str| module_of(path).map(|m| m.as_str().to_owned());

        // Every module's routes, scoped to it.
        assert_eq!(
            module("/v1/tenants/acme/sales/invoices").as_deref(),
            Some("sales")
        );
        assert_eq!(
            module("/v1/tenants/acme/sales/invoices/INV-1/payments").as_deref(),
            Some("sales")
        );
        assert_eq!(
            module("/v1/tenants/acme/ledger/accounts").as_deref(),
            Some("ledger")
        );
        assert_eq!(
            module("/v1/tenants/acme/ledger/chart").as_deref(),
            Some("ledger")
        );

        // The tenant's own surface belongs to no module, so it is judged on the
        // tenant-wide role. This is what stops an accountant-for-sales from
        // deciding who else has access.
        for tenant_wide in [
            "/v1/tenants/acme",
            "/v1/tenants/acme/members",
            "/v1/tenants/acme/members/01a00000-0000-7000-8000-000000000000",
            "/v1/tenants/acme/modules",
            "/v1/tenants/acme/invitations",
        ] {
            assert_eq!(module(tenant_wide), None, "{tenant_wide}");
        }

        // Nothing outside a tenant is a module's business either.
        for outside in [
            "/v1/health",
            "/v1/sessions",
            "/v1/signups",
            "/v1/modules",
            "/",
        ] {
            assert_eq!(module(outside), None, "{outside}");
        }
    }

    /// A request cannot opt out of its tenant-wide role by inventing a segment.
    ///
    /// If an unknown segment counted as "some module", a caller held back in
    /// every module they have would find that `/v1/tenants/acme/anything/…`
    /// fell back to a role they were deliberately not given there.
    #[test]
    fn an_invented_module_segment_is_not_a_module() {
        assert_eq!(module_of("/v1/tenants/acme/nonsense/x"), None);
        assert_eq!(module_of("/v1/tenants/acme/Sales/invoices"), None);
        assert_eq!(module_of("/v1/tenants/acme/../sales/invoices"), None);
    }

    /// Every module this build offers is reachable as a path segment, so no
    /// module's routes are silently judged tenant-wide.
    #[test]
    fn every_module_is_addressable_by_its_own_name() {
        for (name, setup) in crate::modules::available() {
            let path = format!("/v1/tenants/acme/{name}/anything");
            assert_eq!(
                module_of(&path).as_ref(),
                Some(&setup.module),
                "{name} routes are not scoped to {name}"
            );
        }
    }
}
