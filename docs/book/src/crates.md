# How the crates fit together

The workspace splits into a core that a tenant can't switch off and modules that
they choose. Dependencies only ever point one way: modules depend on the core,
and on modules below them.

```
erp-types       newtypes, Money, NonEmpty. No I/O, so a frontend could share it
erp-i18n        message codes, locales, the Localize trait
erp-eventlog    the tenant log: append, load, upcasters, numbering, outbox
erp-occupancy   capacity over time: does one more fit
erp-recurrence  when something repeats: which days, between which two times
erp-links       short links: a token, where it points, who followed it
erp-storage     where a file lives: an engine, a key, a checksum verified on read
erp-projection  groups, ProjectionCtx, the runner, shadow replay and the differ
erp-tenant      TenantDb, the Budget trait, roles, EnabledModules, ModuleSetup
erp-control     identities, tenants, entitlements, clusters, placement, the fleet
erp-worker      the Job trait, the tenant visit loop, bin/worker and bin/migrator
erp-web         extractors, problem+json, paging, request-level messages
erp-api         the core's own routes, the module list, the composition root
erp-demo        the seeded tenant, and bin/demo
erp-testkit     template databases, fault injection, the differ

modules/branches    places to trade from; the dimension on every document
modules/crm         customers as records
modules/hr          the org chart, and the claims that travel up it
modules/ledger      accounts, journal entries, periods, VAT, charts
modules/sales       invoices, credit notes, payments, refunds, receivables
modules/purchases   bills, payments out, input tax
modules/tax_sa      the Saudi rate, the VAT return, ZATCA
modules/booking     reservations, rotas, availability, pricing
modules/prepaid     packages, deposits, subscriptions, loyalty
modules/pos         shifts, till sales, the drawer and its variance
modules/payroll     what a business pays its people, and the entry it makes
modules/hr_sa       GOSI, and end of service
modules/reports     figures that agree with the books, built from the log
modules/messaging   channels, templates that fetch their own data, and what they cost
modules/files       documents attached to things, and proof they came back unchanged
```

`branches` and `crm` are at the top of that list because they depend on nothing
and everything else names them. `pos` is at the bottom because it depends on
three of the others and writes no document of its own.

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
