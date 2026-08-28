# How the crates fit together

The workspace splits into a core that a tenant can't switch off and modules that
they choose. Dependencies only ever point one way: modules depend on the core,
and on modules below them.

```
erp-types       newtypes, Money, NonEmpty. No I/O, so a frontend could share it
erp-i18n        message codes, locales, the Localize trait
erp-eventlog    the tenant log: append, load, upcasters, numbering, outbox
erp-projection  groups, ProjectionCtx, the runner, shadow replay and the differ
erp-tenant      TenantDb, the Budget trait, roles, EnabledModules, ModuleSetup
erp-control     identities, tenants, entitlements, clusters, placement, the fleet
erp-worker      the Job trait, the tenant visit loop, bin/worker and bin/migrator
erp-web         extractors, problem+json, paging, request-level messages
erp-api         the core's own routes, the module list, the composition root
erp-demo        the seeded tenant, and bin/demo
erp-testkit     template databases, fault injection, the differ

modules/ledger      accounts, journal entries, periods, VAT, charts
modules/sales       invoices, credit notes, payments, receivables
modules/purchases   bills, payments out, input tax
modules/tax_sa      the Saudi rate, the VAT return, ZATCA
```

## Why erp-tenant exists

This is the seam that makes the deployment tiers possible. It holds the six
things a module actually uses, `TenantDb` and `CommandError` and `PoolError` and
`ModuleSetup` and `EnabledModules` and the message codes those render as, and
nothing else.

Before it existed, every module depended on `erp-control`, which exports the
cluster registry, the fleet plan, the placement policy and the connection pools.
A tenant binary shipped to a customer's own cloud would have carried the map of
every other tenant in it. Modules were only using six symbols and getting the
rest for free.

`erp-tenant/tests/boundary.rs` fails the build if a module names `erp-control`
in its dependencies.

## Why erp-web sits below the modules

A module ships its own routes, so `sales::http::routes()` is a router the sales
crate owns and `erp-api` mounts. That only works if a module can name the
extractors it builds those routes from, which means the extractors have to live
below it. If a module reached up into `erp-api` for one, it would close a cycle,
because `erp-api` names every module.

The composition root decides what gets mounted. The modules write the routes
themselves.

## The one dependency that still points the wrong way

`erp-web` depends on `erp-control`, because `AppState` holds an
`Arc<ControlPlane>` and its extractors check a session against the control
database on every request. Every module depends on `erp-web`, so the transitive
path from a module to the fleet still exists.

Closing it needs a tenant that verifies a signed token locally instead of asking
the control plane. That work hasn't been done, and this page says so.
