//! Which modules exist, and turning them on and off.
//!
//! # Why this is not a `Module` trait
//!
//! A trait would also have to carry the routes and the worker's jobs, and
//! neither can cross this boundary — a module must not depend on `spa-api` or
//! `spa-worker`. So each composition root still lists what it composes, and only
//! the *set* is shared. [`available`] is that set.
//!
//! What a module does describe for itself is [`ModuleSetup`]: its install SQL,
//! its projection groups, and what it needs underneath it. The three places that
//! ask about dependencies — signing up, enabling later, and refusing to disable
//! — all read the same field, so they cannot drift.

use crate::wire::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use spa_control::{Actor, ModuleSetup};
use spa_i18n::{Locale, Message, MessageArg};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::error::ApiError;
use crate::extract::{Allowed, Language, ManageTenant, Read};
use crate::problem::Problem;
use crate::state::AppState;

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_modules, enable_module))
        .routes(routes!(disable_module))
        // Unauthenticated on purpose: a pricing page needs the catalogue before
        // anyone has an account. It is product information, not data.
        .routes(routes!(catalogue))
}

/// Every module this build offers, as `(name, setup)`.
///
/// The list, in one place — because several things need it and they must not
/// disagree: signup, this file, and the demo tenant, which enables *all* of it.
/// "The demo has every module enabled" is a requirement nothing could check
/// while the set was a `match` arm.
#[must_use]
pub fn available() -> Vec<(&'static str, ModuleSetup)> {
    vec![
        ("ledger", ledger::setup()),
        ("sales", sales::setup()),
        ("purchases", purchases::setup()),
        ("tax_sa", tax_sa::setup()),
    ]
}

/// Looks a module up by the name a client sent.
pub(crate) fn find(name: &str, locale: Locale) -> Result<ModuleSetup, Problem> {
    available()
        .into_iter()
        .find(|(known, _)| *known == name)
        .map(|(_, setup)| setup)
        .ok_or_else(|| {
            ApiError::BadRequest(
                Message::new(crate::messages::UNKNOWN_MODULE)
                    .with("module", MessageArg::text(name.to_owned())),
            )
            .into_problem(locale)
        })
}

/// Refuses a module that is no longer offered.
///
/// Called where a module is **added** — signing up and enabling — and
/// deliberately not in [`find`], because `find` also serves disabling and module
/// roles. A tenant on a deprecated module has to be able to turn it off, and to
/// keep managing who uses it in the meantime; refusing there would trap them.
pub(crate) fn check_offered(setup: &ModuleSetup, locale: Locale) -> Result<(), Problem> {
    let Some(why) = setup.deprecated else {
        return Ok(());
    };
    Err(ApiError::BadRequest(
        Message::new(crate::messages::MODULE_DEPRECATED)
            .with("module", MessageArg::text(setup.module.as_str().to_owned()))
            .with("why", MessageArg::text(why.to_owned())),
    )
    .into_problem(locale))
}

/// Refuses a module whose dependencies are not in `present`.
///
/// Shared by signup (where `present` is what was asked for) and enabling (where
/// it is what the tenant already has), because "sales needs the ledger" must not
/// be true in one and forgotten in the other.
pub(crate) fn check_requirements(
    setup: &ModuleSetup,
    present: &[String],
    locale: Locale,
) -> Result<(), Problem> {
    for required in setup.requires {
        if present.iter().any(|p| p == required) {
            continue;
        }
        return Err(ApiError::BadRequest(
            Message::new(crate::messages::MODULE_REQUIRES)
                .with("module", MessageArg::text(setup.module.as_str().to_owned()))
                .with("required", MessageArg::text((*required).to_owned())),
        )
        .into_problem(locale));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
struct ModuleView {
    name: &'static str,
    /// Set when the module is no longer offered, and says why. A tenant that has
    /// it keeps it; nobody new can turn it on.
    deprecated: Option<&'static str>,
    /// What this module needs underneath it. A client building a picker needs
    /// this to grey out the impossible combinations rather than discover them.
    requires: &'static [&'static str],
    enabled: bool,
}

#[derive(Debug, Serialize, ToSchema)]
struct CatalogueView {
    name: &'static str,
    /// Set when the module is no longer offered. A picker should hide it.
    deprecated: Option<&'static str>,
    requires: &'static [&'static str],
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "module": "sales" }))]
struct Enable {
    /// A name from `GET /v1/modules`.
    module: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Every module this build offers.
///
/// Unauthenticated: a signup form and a pricing page both need this before
/// anyone has an account. It is product information, not data.
#[utoipa::path(
    get,
    path = "/v1/modules",
    tag = "modules",
    security(),
    responses((status = OK, body = Vec<CatalogueView>)),
)]
async fn catalogue() -> Json<Vec<CatalogueView>> {
    Json(
        available()
            .into_iter()
            .map(|(name, setup)| CatalogueView {
                name,
                deprecated: setup.deprecated,
                requires: setup.requires,
            })
            .collect(),
    )
}

/// What this tenant has, and what else it could have.
#[utoipa::path(
    get,
    path = "/v1/tenants/{slug}/modules",
    tag = "modules",
    params(("slug" = String, Path, description = "The tenant's name in URLs.")),
    responses(
        (status = OK, body = Vec<ModuleView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn list_modules(tenant: Allowed<Read>) -> Json<Vec<ModuleView>> {
    Json(
        available()
            .into_iter()
            .map(|(name, setup)| ModuleView {
                name,
                deprecated: setup.deprecated,
                requires: setup.requires,
                enabled: tenant.db.has_module(&setup.module),
            })
            .collect(),
    )
}

/// Turns a module on for a running tenant.
///
/// Installs its read models *and* records the entitlement — both, because
/// either alone is a tenant that 500s: entitled with no tables, or tables
/// nothing can reach.
///
/// Enabling something already on is a no-op, not a conflict.
#[utoipa::path(
    post,
    path = "/v1/tenants/{slug}/modules",
    tag = "modules",
    params(("slug" = String, Path, description = "The tenant's name in URLs.")),
    request_body = Enable,
    responses(
        (status = NO_CONTENT, description = "On. Already-on is the same answer."),
        (status = BAD_REQUEST, description = "No such module, or one whose dependencies are not enabled", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn enable_module(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<Enable>,
) -> Result<StatusCode, Problem> {
    let setup = find(&body.module, locale)?;
    check_offered(&setup, locale)?;

    let enabled: Vec<String> = tenant
        .db
        .modules()
        .iter()
        .map(|m| m.as_str().to_owned())
        .collect();
    check_requirements(&setup, &enabled, locale)?;

    if tenant.db.has_module(&setup.module) {
        // Already on. A no-op rather than a conflict: the caller wanted it
        // enabled and it is.
        return Ok(StatusCode::NO_CONTENT);
    }

    state
        .control
        .install_module(
            tenant.db.tenant(),
            setup,
            Actor::identity(tenant.session.identity),
        )
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Turns a module off.
///
/// **Nothing is deleted.** The entitlement is marked disabled, the routes stop
/// answering and the worker stops visiting on its behalf — but the events and
/// the read models stay exactly where they are, so a tenant who downgrades and
/// comes back finds their data. That is the "updates never break old data"
/// requirement applied to the one operation most likely to violate it.
#[utoipa::path(
    delete,
    path = "/v1/tenants/{slug}/modules/{module}",
    tag = "modules",
    params(
        ("slug" = String, Path, description = "The tenant's name in URLs."),
        ("module" = String, Path, description = "A name from `GET /v1/modules`."),
    ),
    responses(
        (status = NO_CONTENT, description = "Off. Nothing was deleted; already-off is the same answer."),
        (status = BAD_REQUEST, description = "No such module, or one another enabled module is built on", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn disable_module(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(params): Path<std::collections::HashMap<String, String>>,
) -> Result<StatusCode, Problem> {
    let name = params.get("module").map_or("", String::as_str);
    let setup = find(name, locale)?;

    if !tenant.db.has_module(&setup.module) {
        return Ok(StatusCode::NO_CONTENT);
    }

    // Refuse to pull the rug from under something still running on it. The
    // alternative — disabling anyway — leaves sales issuing invoices that
    // cannot post, which is a worse outcome than an error.
    if let Some(dependent) = dependent_on(name, tenant.db.modules()) {
        return Err(ApiError::BadRequest(
            Message::new(crate::messages::MODULE_IN_USE)
                .with("module", MessageArg::text(name.to_owned()))
                .with("dependent", MessageArg::text(dependent)),
        )
        .into_problem(locale));
    }

    state
        .control
        .disable_module(
            tenant.db.tenant(),
            &setup.module,
            Actor::identity(tenant.session.identity),
        )
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// An enabled module that needs `name`, if there is one.
fn dependent_on(name: &str, enabled: &spa_control::EnabledModules) -> Option<String> {
    available()
        .into_iter()
        .filter(|(_, setup)| enabled.contains(&setup.module))
        .find(|(_, setup)| setup.requires.contains(&name))
        .map(|(dependent, _)| dependent.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_requirement_names_a_module_that_exists() {
        // A typo in `requiring(&["ledgre"])` would make that module permanently
        // un-enableable, and nothing else would notice until someone tried.
        let names: Vec<&str> = available().into_iter().map(|(name, _)| name).collect();
        for (name, setup) in available() {
            for required in setup.requires {
                assert!(
                    names.contains(required),
                    "{name} requires {required:?}, which is not a module"
                );
            }
            assert_ne!(
                setup.module.as_str(),
                *setup.requires.first().unwrap_or(&""),
                "{name} requires itself"
            );
        }
    }

    /// **The database is stricter than `ModuleId`, and it wins.**
    ///
    /// `ModuleId` accepts `.` and `-`; `entitlement.module_id` is
    /// `^[a-z][a-z0-9_]{0,47}$`. A module named `tax-sa` therefore constructs
    /// fine, passes every test that does not touch the control plane, and fails
    /// at the moment a tenant enables it — which is what happened, and is a
    /// terrible place to find out.
    ///
    /// ponytail: the honest fix is for `ModuleId` to carry the narrower rule, so
    /// the type refuses what the schema will. That is a change in `spa-types`
    /// with no consumer asking for it yet; this catches the same mistake at
    /// build time in the meantime.
    #[test]
    fn every_module_id_satisfies_the_entitlement_constraint() {
        for (name, setup) in available() {
            let id = setup.module.as_str();
            let mut chars = id.chars();
            let first = chars.next().unwrap_or(' ');
            assert!(
                first.is_ascii_lowercase()
                    && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                    && (1..=48).contains(&id.len()),
                "{name} installs under `{id}`, which `entitlement.module_id` refuses: \
                 it wants `^[a-z][a-z0-9_]{{0,47}}$`"
            );
        }
    }

    /// **`just prepare` guesses this, so it has to be true.**
    ///
    /// The type-check database installs each module's schema-relative SQL with
    /// `search_path` pointed at `proj_<crate directory, hyphens to underscores>`.
    /// A module that named its schema anything else would have its tables land
    /// somewhere the qualified read queries do not look, and the failure would
    /// be a type-check error in a crate nobody touched.
    #[test]
    fn a_modules_schema_is_named_after_its_crate() {
        for (name, setup) in available() {
            let expected = format!("proj_{}", name.replace('-', "_"));
            for (group, schema) in setup.groups {
                assert_eq!(
                    *schema, expected,
                    "{name}'s group `{group}` is in `{schema}`, and `just prepare` \
                     will install it into `{expected}`"
                );
            }
        }
    }

    #[test]
    fn a_modules_name_matches_the_id_it_installs_under() {
        // The catalogue key and the entitlement row have to agree, or enabling
        // "sales" would entitle something else and every check would disagree
        // with every other.
        for (name, setup) in available() {
            assert_eq!(name, setup.module.as_str());
        }
    }
}
