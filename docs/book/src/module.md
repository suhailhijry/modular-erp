# Writing a module

A module owns a domain, its schema, its projections, its routes and its
messages. It depends on `erp-tenant` and `erp-web`, plus any module it's built
on top of.

## What it has to declare

```rust
pub fn setup() -> erp_tenant::ModuleSetup;   // schema, seed data, dependencies
pub fn module_id() -> erp_types::ModuleId;
pub fn upcasters() -> &'static erp_eventlog::Upcasters;
pub fn projections() -> Vec<Arc<dyn Projection<Group = MyGroup>>>;
pub mod http;                                 // routes() and the module's catalog
pub static CATALOG: erp_i18n::Composite;      // its messages plus its dependencies'
```

`ModuleSetup` carries the DDL that creates the module's tables, and separately
any data the module can't work without. Those are deliberately two fields. The
Saudi VAT rate used to ride along with `tax_sa`'s schema install because that was
the only hook available, which made a tenant's data and a tenant's tables look
like one thing, and `just prepare` installs the DDL into a throwaway type-check
database where a configuration row is noise at best.

## Registering it

One list in `erp-api/src/modules.rs` carries both views of a module, its setup
and its routes:

```rust
const REGISTERED: &[Registered] = &[ /* ledger, sales, purchases, tax_sa */ ];
```

Three tests guard it. Every module's routes live under its own name, no two
modules claim the same path, and a module that requires one of several
dependencies is satisfied by any one of them.

## Extending another module

By subscribing to its events. `tax_sa` learns that an invoice was issued by
reading the log, and `sales` has no idea Saudi Arabia exists.

This is the direction that keeps the dependency graph honest. If `sales` had to
call into `tax_sa`, then adding a second country would mean editing `sales`, and
a tenant who doesn't need tax would still compile it.

## What a module must not do

It can't depend on `erp-control`, which is checked by
`erp-tenant/tests/boundary.rs`. That crate exports the cluster registry, the
placement policy and the connection pools, and a binary shipped to a customer's
own cloud can't carry the map of every other tenant.

It can't read inside `apply`, which is checked by
`erp-projection/tests/purity.rs`.

It can't load an aggregate outside command handling, which is checked by
`erp-eventlog/tests/write_side.rs`.

It can't generate its own identities on a write path, which is checked by
`erp-api/tests/idempotence.rs`. An invoice carries the key the client sent, so a
retry carries the same one and the database refuses the duplicate.

## Errors are data

A module's failures are message codes carrying typed arguments, so the same error
renders in Arabic and in English from one definition. A module
renders its own failures through a composite of its catalog and `erp_web::CATALOG`,
because it can't name its sibling modules and has no reason to.

`erp_api::CATALOG` is the union of all of them and is what `docs/ERRORS.md` gets
generated from, so a code missing from any part fails the build. Otherwise it
would reach a user as a bare string.
