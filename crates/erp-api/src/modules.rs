//! Which modules exist, and turning them on and off.
//!
//! # Why this is a list and not a `Module` trait
//!
//! Because half of what a trait would carry cannot cross this boundary. A
//! module ships its **routes** — those are in the module now — but its worker
//! jobs are registered in `bin/worker.rs`, and a module cannot depend on
//! `erp-worker` any more than `erp-worker` can be made to know what a ZATCA
//! document is. A trait with two of its three methods implemented somewhere else
//! is a trait that describes nothing.
//!
//! So each composition root lists what it composes, and [`REGISTERED`] is this
//! one's list. It carries both views a caller needs — the [`ModuleSetup`] the
//! control plane installs from, and the router the server mounts — from **one**
//! entry per module. That is the property that matters: a module cannot be added
//! to the platform and have its routes forgotten, because there is nowhere to
//! add it that does not also mount them.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_control::{Actor, ModuleSetup};
use erp_i18n::{Locale, Message, MessageArg};
use erp_web::ApiError;
use erp_web::AppState;
use erp_web::Json;
use erp_web::Problem;
use erp_web::{Allowed, Anonymous, Language, ManageTenant, Read};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_modules, enable_module))
        .routes(routes!(disable_module))
        // Unauthenticated on purpose, and on the apex: a pricing page needs the
        // catalogue before anyone has an account.
        .routes(routes!(catalogue))
}

/// A module, as the platform holds it: what to install, and what to serve.
struct Registered {
    name: &'static str,
    setup: fn() -> ModuleSetup,
    /// The module's own router. A function pointer rather than a value because
    /// [`REGISTERED`] is a `const` and building a router allocates.
    http: fn() -> OpenApiRouter<AppState>,
    /// **What its failures say, in every language.**
    ///
    /// Here for the same reason `http` is: a module that reached the platform
    /// without its catalogue would render bare codes at a user, and
    /// `docs/ERRORS.md` would not know its refusals existed. Three modules
    /// arrived that way before this field did — `hr`, `payroll` and `hr_sa` —
    /// and nothing failed, because the completeness audit could only check
    /// codes it had already been given.
    ///
    /// Now there is nowhere to add a module that does not also say what it can
    /// say.
    catalog: &'static dyn erp_i18n::Catalog,
}

/// **The list.** Every module this build offers, in the order they are mounted.
///
/// Several things read it and they must not disagree: signup, this file, the
/// worker's job registry, the migrator's fleet check, and the demo tenant, which
/// enables *all* of it. "The demo has every module enabled" is a requirement
/// nothing could check while the set was a `match` arm.
const REGISTERED: &[Registered] = &[
    Registered {
        name: "booking",
        setup: booking::setup,
        http: booking::http::routes,
        catalog: &booking::CATALOG,
    },
    // First, because everything else may name one and it names nothing.
    Registered {
        name: "branches",
        setup: branches::setup,
        http: branches::http::routes,
        catalog: &branches::CATALOG,
    },
    Registered {
        name: "crm",
        setup: crm::setup,
        http: crm::http::routes,
        catalog: &crm::CATALOG,
    },
    Registered {
        name: "hr",
        setup: hr::setup,
        http: hr::http::routes,
        catalog: &hr::CATALOG,
    },
    Registered {
        name: "payments",
        setup: payments::setup,
        http: payments::http::routes,
        catalog: &payments::CATALOG,
    },
    Registered {
        name: "payroll",
        setup: payroll::setup,
        http: payroll::http::routes,
        catalog: &payroll::CATALOG,
    },
    Registered {
        name: "hr_sa",
        setup: hr_sa::setup,
        http: hr_sa::http::routes,
        catalog: &hr_sa::CATALOG,
    },
    Registered {
        name: "ledger",
        setup: ledger::setup,
        http: ledger::http::routes,
        catalog: &ledger::CATALOG,
    },
    Registered {
        name: "sales",
        setup: sales::setup,
        http: sales::http::routes,
        catalog: &sales::CATALOG,
    },
    Registered {
        name: "prepaid",
        setup: prepaid::setup,
        http: prepaid::http::routes,
        catalog: &prepaid::CATALOG,
    },
    // After `ledger`, which it is built on: where a count's discrepancy and a
    // sale's cost land is a decision about the chart of accounts, and the codes
    // a tenant chooses are checked against their own chart.
    Registered {
        name: "inventory",
        setup: inventory::setup,
        http: inventory::http::routes,
        catalog: &inventory::CATALOG,
    },
    Registered {
        name: "pos",
        setup: pos::setup,
        http: pos::http::routes,
        catalog: &pos::CATALOG,
    },
    Registered {
        name: "purchases",
        setup: purchases::setup,
        http: purchases::http::routes,
        catalog: &purchases::CATALOG,
    },
    Registered {
        name: "tax_sa",
        setup: tax_sa::setup,
        http: tax_sa::http::routes,
        catalog: &tax_sa::CATALOG,
    },
    Registered {
        name: "reports",
        setup: reports::setup,
        http: reports::http::routes,
        catalog: &reports::CATALOG,
    },
    Registered {
        name: "messaging",
        setup: messaging::setup,
        http: messaging::http::routes,
        catalog: &messaging::CATALOG,
    },
    // After `messaging`, which it is built on: an audience is resolved there
    // and a tenant's own wording for a kind is a template there.
    Registered {
        name: "notifications",
        setup: notifications::setup,
        http: notifications::http::routes,
        catalog: &notifications::CATALOG,
    },
    // After `messaging` and `crm`, which it is built on: a thread resolves the
    // customer of a subject through the first and matches an inbound number
    // through the second.
    Registered {
        name: "conversations",
        setup: conversations::setup,
        http: conversations::http::routes,
        catalog: &conversations::CATALOG,
    },
    Registered {
        name: "files",
        setup: files::setup,
        http: files::http::routes,
        catalog: &files::CATALOG,
    },
];

/// Every module this build offers, as `(name, setup)`.
///
/// What the control plane, the worker and the migrator all read. The router half
/// is [`mounted`], from the same entries.
#[must_use]
pub fn available() -> Vec<(&'static str, ModuleSetup)> {
    REGISTERED
        .iter()
        .map(|module| (module.name, (module.setup)()))
        .collect()
}

/// **What this build projects, by module**: every `(group, version)` a
/// module's routes are served from — the groups of every module in its
/// [`closure`]. A module with none is absent.
///
/// What `erp_web::AppState::read_models` holds, and [`crate::router`] is what
/// puts it there.
#[must_use]
pub(crate) fn read_models()
-> std::collections::HashMap<erp_types::ModuleId, Vec<(&'static str, i16)>> {
    let modules = available();
    modules
        .iter()
        .filter_map(|(name, setup)| {
            let mut groups: Vec<(&'static str, i16)> = closure(&modules, name)
                .into_iter()
                // A name no module has cannot be here:
                // `a_modules_reads_are_its_crate_dependencies` holds `reads`
                // to registered modules, and `COMPOSED` is checked the same.
                .filter_map(|module| modules.iter().find(|(known, _)| *known == module))
                .flat_map(|(_, read)| {
                    read.groups
                        .iter()
                        .map(|(group, _, version)| (*group, *version))
                })
                .collect();
            groups.sort_unstable();
            (!groups.is_empty()).then(|| (setup.module.clone(), groups))
        })
        .collect()
}

/// **The modules whose code a module's routes run**: the module, what this
/// crate's own routes under its path run ([`COMPOSED`]), and everything those
/// read over [`ModuleSetup::reads`], transitively.
///
/// Transitively because a read goes through code: `notifications` runs
/// `messaging`, which reads `booking`'s tables to fill a template, and
/// `messaging` has no tables of its own. `COMPOSED` only at the root: another
/// module reading `booking` runs `booking`'s crate, not these routes.
fn closure(
    modules: &[(&'static str, ModuleSetup)],
    root: &'static str,
) -> std::collections::BTreeSet<&'static str> {
    let mut seen = std::collections::BTreeSet::new();
    let mut next: Vec<&'static str> = COMPOSED
        .iter()
        .filter(|(module, _)| *module == root)
        .flat_map(|(_, runs)| runs.iter().copied())
        .chain([root])
        .collect();
    while let Some(name) = next.pop() {
        if seen.insert(name)
            && let Some((_, read)) = modules.iter().find(|(known, _)| *known == name)
        {
            next.extend(read.reads.iter().copied());
        }
    }
    seen
}

/// **Routes this crate serves under a module's path, and the modules they run
/// beyond that module's own `reads`.**
///
/// A route is refused by the module its path names, and a module crate's
/// `reads` is its `Cargo.toml` — which cannot see what this crate composes
/// under `/v1/booking`: a reservation's final invoice (`billing.rs`) is issued
/// through `sales` against `payments`' deposits, and a public deposit
/// (`deposits.rs`) is taken through `payments`, posted through `ledger`,
/// announced through `messaging` and taxed by asking `tax_sa`. Without these,
/// a stale `sales` would 503 `/v1/sales/*` and still let the invoice route
/// issue a tax invoice from it.
///
/// `composed_routes_run_only_what_their_module_is_refused_on` scans this
/// crate's files and fails on a module one of them runs that is missing here.
const COMPOSED: &[(&str, &[&str])] = &[(
    "booking",
    &["ledger", "messaging", "payments", "sales", "tax_sa"],
)];

/// Every module's catalogue, for the completeness audit.
///
/// Exists so `every_module_reaches_the_reference` can ask the registry rather
/// than a list somebody maintains beside it — which is what let `hr`, `payroll`
/// and `hr_sa` reach the platform with their refusals absent from
/// `docs/ERRORS.md`.
#[must_use]
pub fn catalogues() -> Vec<(&'static str, &'static dyn erp_i18n::Catalog)> {
    REGISTERED
        .iter()
        .map(|module| (module.name, module.catalog))
        .collect()
}

/// Every module's routes, merged.
///
/// Each module owns its HTTP surface — `sales::http` sits next to the aggregates
/// and read models it serves — and this is the only thing that mounts them.
pub(crate) fn mounted() -> OpenApiRouter<AppState> {
    REGISTERED
        .iter()
        .fold(OpenApiRouter::new(), |router, module| {
            router.merge((module.http)())
        })
}

/// Looks a module up by the name a client sent.
pub(crate) fn find(name: &str, locale: Locale) -> Result<ModuleSetup, Problem> {
    available()
        .into_iter()
        .find(|(known, _)| *known == name)
        .map(|(_, setup)| setup)
        .ok_or_else(|| {
            ApiError::BadRequest(
                Message::new(erp_web::messages::UNKNOWN_MODULE)
                    .with("module", MessageArg::text(name.to_owned())),
            )
            .into_problem(locale, &crate::CATALOG)
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
        Message::new(erp_web::messages::MODULE_DEPRECATED)
            .with("module", MessageArg::text(setup.module.as_str().to_owned()))
            .with("why", MessageArg::text(why.to_owned())),
    )
    .into_problem(locale, &crate::CATALOG))
}

/// Refuses a module whose dependencies are not in `present`.
///
/// Shared by signup (where `present` is what was asked for) and enabling (where
/// it is what the tenant already has), because "sales needs the ledger" must not
/// be true in one and forgotten in the other.
///
/// Both kinds of dependency: everything in `requires`, and **at least one** of
/// `requires_any`. See [`ModuleSetup::requires_any`].
pub(crate) fn check_requirements(
    setup: &ModuleSetup,
    present: &[String],
    locale: Locale,
) -> Result<(), Problem> {
    let has = |name: &str| present.iter().any(|p| p == name);

    for required in setup.requires {
        if has(required) {
            continue;
        }
        return Err(ApiError::BadRequest(
            Message::new(erp_web::messages::MODULE_REQUIRES)
                .with("module", MessageArg::text(setup.module.as_str().to_owned()))
                .with("required", MessageArg::text((*required).to_owned())),
        )
        .into_problem(locale, &crate::CATALOG));
    }

    if !setup.requires_any.is_empty() && !setup.requires_any.iter().copied().any(has) {
        return Err(ApiError::BadRequest(
            Message::new(erp_web::messages::MODULE_REQUIRES_ONE_OF)
                .with("module", MessageArg::text(setup.module.as_str().to_owned()))
                .with("required", MessageArg::text(setup.requires_any.join(", "))),
        )
        .into_problem(locale, &crate::CATALOG));
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
    /// What this module needs underneath it, all of it. A client building a
    /// picker needs this to grey out the impossible combinations rather than
    /// let someone discover them.
    requires: &'static [&'static str],
    /// What this module needs **at least one** of. Empty for most.
    requires_any: &'static [&'static str],
    enabled: bool,
}

#[derive(Debug, Serialize, ToSchema)]
struct CatalogueView {
    name: &'static str,
    /// Set when the module is no longer offered. A picker should hide it.
    deprecated: Option<&'static str>,
    requires: &'static [&'static str],
    /// What this module needs **at least one** of. Empty for most.
    requires_any: &'static [&'static str],
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
/// Unauthenticated, and on the **apex** — a signup form and a pricing page both
/// need this before anyone has an account, and before there is a subdomain to
/// ask from. It is product information, not data.
///
/// `/v1/catalogue` rather than `/v1/modules` because that is now what a tenant's
/// own list is called. The two collided the moment the tenant moved to the
/// subdomain, and the router refused to start — correctly: a path has to name
/// one thing, because nothing routes on the host.
#[utoipa::path(
    get,
    path = "/v1/catalogue",
    tag = "modules",
    security(),
    responses(
        (status = OK, body = Vec<CatalogueView>),
        (status = TOO_MANY_REQUESTS, description = "Too many attempts from this address, or against this account. `args.seconds` says how long to wait.", body = Problem),
    ),
)]
async fn catalogue(_anonymous: Anonymous) -> Json<Vec<CatalogueView>> {
    Json(
        available()
            .into_iter()
            .map(|(name, setup)| CatalogueView {
                name,
                deprecated: setup.deprecated,
                requires: setup.requires,
                requires_any: setup.requires_any,
            })
            .collect(),
    )
}

/// What this tenant has, and what else it could have.
#[utoipa::path(
    get,
    path = "/v1/modules",
    tag = "modules",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
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
                requires_any: setup.requires_any,
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
    path = "/v1/modules",
    tag = "modules",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
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
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;

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
    path = "/v1/modules/{module}",
    tag = "modules",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
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
            Message::new(erp_web::messages::MODULE_IN_USE)
                .with("module", MessageArg::text(name.to_owned()))
                .with("dependent", MessageArg::text(dependent)),
        )
        .into_problem(locale, &crate::CATALOG));
    }

    state
        .control
        .disable_module(
            tenant.db.tenant(),
            &setup.module,
            Actor::identity(tenant.session.identity),
        )
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;

    Ok(StatusCode::NO_CONTENT)
}

/// An enabled module that would be left without something it needs, if `name`
/// were turned off.
///
/// Two ways to be left without:
///
/// - `name` is in its `requires`, which is an AND list — turning it off breaks
///   the dependent outright.
/// - `name` is in its `requires_any` and is the **last one enabled**. A tenant
///   with sales, purchases and `tax_sa` may turn either side off; the second one
///   is refused, because a VAT return with nothing on either side is not a
///   downgrade, it is a module that cannot answer.
fn dependent_on(name: &str, enabled: &erp_control::EnabledModules) -> Option<String> {
    let still_on = |module: &str| {
        module != name && erp_types::ModuleId::new(module).is_ok_and(|id| enabled.contains(&id))
    };

    available()
        .into_iter()
        .filter(|(_, setup)| enabled.contains(&setup.module))
        .find(|(_, setup)| {
            setup.requires.contains(&name)
                || (setup.requires_any.contains(&name)
                    && !setup.requires_any.iter().copied().any(still_on))
        })
        .map(|(dependent, _)| dependent.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The URL namespace is the module namespace, and authorization rests on
    /// it.**
    ///
    /// `erp_web`'s `Allowed<C>` extractor reads the module out of the path — so
    /// a module that mounted a route outside its own name would have that route
    /// judged on the *tenant-wide* role instead of the module-scoped one, which
    /// is the more permissive answer, arrived at silently.
    ///
    /// Now that a module writes its own `#[utoipa::path]` attributes, nothing in
    /// the composition root sees them go by. This is what does.
    #[test]
    fn every_modules_routes_live_under_its_own_name() {
        for module in REGISTERED {
            let paths = (module.http)().to_openapi().paths.paths;
            assert!(!paths.is_empty(), "{} mounted nothing", module.name);
            for path in paths.keys() {
                // The module's own root counts. `branches` is the first module
                // whose resource *is* its name — `/v1/branches` rather than
                // `/v1/branches/somethings` — and `module_of` reads that path
                // as `branches` correctly. Requiring a trailing segment would
                // reject a scoped route for having nothing after the scope.
                let root = format!("/v1/{}", module.name);
                assert!(
                    path == &root || path.starts_with(&format!("{root}/")),
                    "{} serves `{path}`, which `Allowed<C>` will judge on the \
                     tenant-wide role rather than on {}'s",
                    module.name,
                    module.name
                );
            }
        }
    }

    /// No two modules claim the same path.
    ///
    /// `merge` panics on a collision at startup, which is the right failure —
    /// but at startup, in production, on the pod that just rolled out. Here it
    /// is a build failure with both names in it.
    #[test]
    fn no_two_modules_claim_the_same_path() {
        let mut seen: std::collections::BTreeMap<String, &str> = std::collections::BTreeMap::new();
        for module in REGISTERED {
            for path in (module.http)().to_openapi().paths.paths.keys() {
                if let Some(other) = seen.insert(path.clone(), module.name) {
                    panic!("{} and {} both serve `{path}`", other, module.name);
                }
            }
        }
    }

    #[test]
    fn every_requirement_names_a_module_that_exists() {
        // A typo in `requiring(&["ledgre"])` would make that module permanently
        // un-enableable, and nothing else would notice until someone tried.
        let names: Vec<&str> = available().into_iter().map(|(name, _)| name).collect();
        for (name, setup) in available() {
            for required in setup.requires.iter().chain(setup.requires_any) {
                assert!(
                    names.contains(required),
                    "{name} requires {required:?}, which is not a module"
                );
                assert_ne!(setup.module.as_str(), *required, "{name} requires itself");
            }
            // A single alternative is an AND requirement written the confusing
            // way, and it takes the wrong error message with it.
            assert_ne!(
                setup.requires_any.len(),
                1,
                "{name} needs `at least one of` exactly one thing; that is `requiring`"
            );
        }
    }

    /// **"At least one of", in the two places it decides something.**
    ///
    /// `tax_sa` nets output tax against input tax and needs a source for one
    /// side or the other. Declaring both in `requires` would force a shop with
    /// no supplier bills to enable `purchases`; declaring neither — which is
    /// what it did — let a tenant turn on a VAT return with nothing on either
    /// side, and disable the last module feeding it without a word.
    #[test]
    fn one_of_several_is_enough_and_none_of_them_is_not() {
        let tax_sa = find("tax_sa", Locale::DEFAULT).expect("a module");
        let present =
            |names: &[&str]| -> Vec<String> { names.iter().map(|n| (*n).to_owned()).collect() };

        // Enabling: either side satisfies it, neither does not.
        for enough in [
            vec!["ledger", "sales"],
            vec!["ledger", "purchases"],
            vec!["ledger", "sales", "purchases"],
        ] {
            assert!(
                check_requirements(&tax_sa, &present(&enough), Locale::DEFAULT).is_ok(),
                "tax_sa refused with {enough:?}"
            );
        }
        let refused = check_requirements(&tax_sa, &present(&["ledger"]), Locale::DEFAULT)
            .expect_err("a VAT return with neither side is not a return");
        assert_eq!(refused.code, "request.module_requires_one_of");
        assert_eq!(
            refused.args["required"],
            MessageArg::text("sales, purchases")
        );

        // And the AND half still bites, with its own message.
        let refused = check_requirements(&tax_sa, &present(&["sales"]), Locale::DEFAULT)
            .expect_err("ledger is not optional");
        assert_eq!(refused.code, "request.module_requires");

        // Disabling: one of two may go, the last may not.
        let enabled = |names: &[&str]| {
            erp_control::EnabledModules::new(
                names
                    .iter()
                    .map(|n| erp_types::ModuleId::new(*n).expect("a module id"))
                    .collect(),
            )
        };
        let both = enabled(&["ledger", "sales", "purchases", "tax_sa"]);
        assert_eq!(
            dependent_on("sales", &both),
            None,
            "purchases still feeds the return, so sales may go"
        );
        assert_eq!(dependent_on("purchases", &both), None);

        let one = enabled(&["ledger", "sales", "tax_sa"]);
        assert_eq!(
            dependent_on("sales", &one).as_deref(),
            Some("tax_sa"),
            "turning off the only side leaves a return that cannot answer"
        );

        // A module that needs none of this is unaffected either way.
        let without = enabled(&["ledger", "sales", "purchases"]);
        assert_eq!(dependent_on("sales", &without), None);
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
    /// the type refuses what the schema will. That is a change in `erp-types`
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
            for (group, schema, _) in setup.groups {
                assert_eq!(
                    *schema, expected,
                    "{name}'s group `{group}` is in `{schema}`, and `just prepare` \
                     will install it into `{expected}`"
                );
            }
        }
    }

    /// **One install script, so one group.** `install_schema` aims the script
    /// at the first group's schema, and the read-model pin in `bin/migrator`
    /// hashes one script per group; a second group needs both to change first.
    #[test]
    fn a_module_has_at_most_one_projection_group() {
        for (name, setup) in available() {
            assert!(
                setup.groups.len() <= 1,
                "{name} declares {} projection groups, and its install script can only \
                 be aimed at one",
                setup.groups.len()
            );
        }
    }

    /// **`reads` is the module's `Cargo.toml`, not a list somebody keeps.**
    ///
    /// A route is refused while a read model it serves from is older than the
    /// build, and every read across groups goes through the other module's
    /// crate (L3 leaves no other way). So the modules a crate depends on are
    /// the read models its routes may be served from; a dependency missing
    /// from `reads` is a route that would serve from a stale shape without a
    /// word.
    #[test]
    fn a_modules_reads_are_its_crate_dependencies() {
        let registered: Vec<&str> = available().iter().map(|(name, _)| *name).collect();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules");

        for (name, setup) in available() {
            let manifest = root.join(name).join("Cargo.toml");
            let text = std::fs::read_to_string(&manifest)
                .unwrap_or_else(|e| panic!("{} is not readable: {e}", manifest.display()));

            // `[dependencies]` only: a dev-dependency is a test's, not a route's.
            let mut depends: Vec<&str> = text
                .split("\n[")
                .find(|section| section.starts_with("dependencies]"))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| line.split('=').next().map(str::trim))
                .filter(|crate_name| registered.contains(crate_name))
                .collect();
            depends.sort_unstable();

            let mut reads = setup.reads.to_vec();
            reads.sort_unstable();
            assert_eq!(
                reads, depends,
                "{name}'s `reads` must be the modules its crate depends on: \
                 `.reading(&{depends:?})` in its `setup()`"
            );
        }
    }

    /// The closure is what the request path refuses on, so it is worth one
    /// look: `notifications` has no tables and runs `messaging`, which has none
    /// either and reads `booking`'s.
    #[test]
    fn a_modules_read_models_are_its_closure_over_reads() {
        let models = super::read_models();
        let groups = |module: &str| -> Vec<&str> {
            models
                .get(&erp_types::ModuleId::new(module).expect("a module id"))
                .map(|groups| groups.iter().map(|(group, _)| *group).collect())
                .unwrap_or_default()
        };

        assert_eq!(groups("files"), ["files"]);
        assert!(groups("notifications").contains(&"booking"));
        assert!(groups("notifications").contains(&"notifications"));
        assert!(!groups("ledger").contains(&"files"));
        // Through `COMPOSED`: the reservation invoice is issued from `sales`.
        assert!(groups("booking").contains(&"sales"));
        // And only at the root: `notifications` reads `booking`'s crate, not
        // the routes this crate serves under `/v1/booking`.
        assert!(!groups("notifications").contains(&"tax_sa"));
    }

    /// **A route this crate serves under a module's path is refused on
    /// everything it runs.** `a_modules_reads_are_its_crate_dependencies`
    /// cannot see this crate's files, and `/v1/booking/reservations/{r}/invoice`
    /// issued tax invoices from `sales` while `sales` was stale because of it.
    ///
    /// Per file: every registered module named as `x::` in a file (comments
    /// aside) that declares a `path = "/v1/{m}/…"` must be in `m`'s
    /// [`super::closure`]. A helper in another file of this crate is not
    /// followed; the three files that serve module paths call none that reads
    /// a module outside `booking`'s own.
    #[test]
    fn composed_routes_run_only_what_their_module_is_refused_on() {
        let modules = available();
        let names: Vec<&'static str> = modules.iter().map(|(name, _)| *name).collect();
        for (module, runs) in super::COMPOSED {
            for name in std::iter::once(module).chain(runs.iter()) {
                assert!(names.contains(name), "COMPOSED names {name}, not a module");
            }
        }

        // `x::` not preceded by an identifier character: `hr::`, not `erp_hr::`.
        let names_module = |code: &str, module: &str| {
            code.match_indices(&format!("{module}::")).any(|(at, _)| {
                !code[..at].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
            })
        };

        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut served = 0;
        for entry in std::fs::read_dir(&src).expect("src is readable") {
            let path = entry.expect("an entry").path();
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("readable");
            let code: String = text
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");

            for module in names
                .iter()
                .filter(|m| text.contains(&format!("path = \"/v1/{m}/")))
            {
                served += 1;
                let refused_on = super::closure(&modules, module);
                let missing: Vec<&str> = names
                    .iter()
                    .copied()
                    .filter(|x| names_module(&code, x) && !refused_on.contains(x))
                    .collect();
                assert!(
                    missing.is_empty(),
                    "{} serves routes under /v1/{module}/ that run {missing:?}, and a \
                     stale read model of theirs would not refuse them: add them to \
                     `COMPOSED` for {module}",
                    path.display()
                );
            }
        }
        // Non-vacuity: billing.rs, deposits.rs and realtime.rs serve `booking`.
        assert!(
            served >= 3,
            "expected this crate to serve routes under a module's path in at least three \
             files, found {served}"
        );
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

#[cfg(test)]
mod schema_names {
    use super::REGISTERED;
    use std::collections::BTreeMap;

    /// **No two modules may name a wire type the same thing.**
    ///
    /// `utoipa` collects every schema into one `components/schemas` map keyed by
    /// the bare Rust type name, and a second module using a name **silently
    /// replaces the first**. There is no warning and no error: the document
    /// simply describes one of them for both.
    ///
    /// That is exactly how `crm`'s `AddressBody` overwrote `tax_sa`'s, which was
    /// found by a *contract* test noticing that a ZATCA registration was sending
    /// an `additional` field the document no longer mentioned. Finding a name
    /// collision through an unrelated semantic mismatch is luck, and this is the
    /// test that stops relying on it.
    #[test]
    fn no_two_modules_claim_the_same_schema_name() {
        // Keyed by name, holding every *distinct definition* seen under it.
        // A type shared from `erp-web` or `erp-i18n` appears in several modules
        // with one definition, which is correct and must pass. Two different
        // shapes under one name is the bug.
        let mut owners: BTreeMap<String, BTreeMap<String, Vec<&str>>> = BTreeMap::new();
        for module in REGISTERED {
            let (_, document) = (module.http)().split_for_parts();
            let Some(components) = document.components else {
                continue;
            };
            for (name, schema) in &components.schemas {
                let shape = serde_json::to_string(schema).unwrap_or_default();
                owners
                    .entry(name.clone())
                    .or_default()
                    .entry(shape)
                    .or_default()
                    .push(module.name);
            }
        }

        let clashes: Vec<_> = owners
            .iter()
            .filter(|(_, shapes)| shapes.len() > 1)
            .map(|(name, shapes)| (name, shapes.values().collect::<Vec<_>>()))
            .collect();
        assert!(
            clashes.is_empty(),
            "these names describe different shapes in different modules, and the \
             document keeps only one of each: {clashes:?}"
        );

        // Non-vacuity: a run that found no schemas at all would pass this and
        // mean nothing.
        assert!(
            owners.len() > 20,
            "expected the modules to declare a real set of schemas, found {}",
            owners.len()
        );
    }
}
