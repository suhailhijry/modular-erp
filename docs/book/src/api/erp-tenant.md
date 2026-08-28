# erp-tenant

Everything a module needs, and nothing a module must not have.

**Depends on:** `erp-eventlog`, `erp-projection`.
**Used by:** `erp-control` (which re-exports it), `erp-web`, and every module.

## Why this crate exists

D15 forecloses it plainly: the tenant runtime may not contain fleet management,
cluster placement, or any other tenant's credentials. A binary that ships to a
customer's own cloud cannot carry the map of everybody else's.

Before this crate that was false. Every module depended on `erp-control`, which
exports `ClusterRegistry`, `FleetPlan`, `PlacementPolicy`, `TenantPools` and
`WorkSchedule`, so a tenant binary linked the whole vocabulary of the fleet. It
was false for a small reason: modules used six symbols from that crate and got
the rest for free.

Those six live here. `erp-control` depends on this and re-exports them, so the
control plane is unchanged, and modules depend on this alone.
`crates/erp-tenant/tests/boundary.rs` fails the build if a module names
`erp-control` in its dependencies, and refuses to pass unless it found at least
four module manifests, so it cannot go green by finding nothing.

## The files

| File | What is in it |
|---|---|
| [`db.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-tenant/src/db.rs) | `TenantDb`, `CommandError` |
| [`budget.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-tenant/src/budget.rs) | `Budget`, `Lane`, `PoolError`, `Conn`, `Tx` |
| [`roles.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-tenant/src/roles.rs) | `Role`, `Capability`, `Access` |
| [`modules.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-tenant/src/modules.rs) | `EnabledModules`, `ModuleSetup` |
| [`messages.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-tenant/src/messages.rs) | `system.overloaded` and `system.internal_error` |

## TenantDb

A route to one tenant's database, plus what is known about that tenant.

```rust
pub struct TenantDb { … }

impl TenantDb {
    pub fn new(tenant: TenantId, write: PgPool, read: Option<PgPool>,
               modules: EnabledModules, budget: Arc<dyn Budget>, lane: Lane) -> Self;

    // who the caller is
    pub fn set_access(&mut self, access: Option<Access>);
    pub fn role(&self) -> Option<Role>;
    pub fn role_in(&self, module: Option<&ModuleId>) -> Option<Role>;
    pub const fn access(&self) -> Option<&Access>;
    pub fn allows(&self, capability: Capability) -> bool;
    pub fn allows_in(&self, capability: Capability, module: Option<&ModuleId>) -> bool;

    // what this tenant is
    pub const fn tenant(&self) -> TenantId;
    pub const fn lane(&self) -> Lane;
    pub const fn modules(&self) -> &EnabledModules;
    pub fn has_module(&self, module: &ModuleId) -> bool;
    pub const fn has_replica(&self) -> bool;

    // getting at the data
    pub async fn acquire(&self) -> Result<Conn, PoolError>;
    pub async fn begin(&self) -> Result<Tx, PoolError>;
    pub async fn read(&self) -> Result<Conn, PoolError>;
    pub async fn execute<A, F, E>(&self, id: &AggregateId, upcasters: &Upcasters,
        metadata: &Metadata, decide: F) -> Result<Committed<A::Event>, CommandError<E>>;
}
```

### Why this type is the security boundary

`new` is public but nothing outside the control plane can usefully call it,
because it needs pools nothing else holds. The only way an application obtains a
`TenantDb` is `ControlPlane::enter`, which checks that the identity is active,
the tenant is enterable, and a live membership joins them. A function taking
`&TenantDb` has been handed proof that those checks passed.

The consequence worth being explicit about: **there is no ambient pool.** No
`AppState.pool`, no global. A query against the wrong tenant is not prevented by
a `WHERE` clause or a row-level policy. It is prevented because there is no
connection to the wrong tenant to be had.

### Choosing a connection method

| Method | Use it for |
|---|---|
| `execute` | A command against one aggregate. The normal write path |
| `begin` | A command that writes two aggregates, or a document plus its number |
| `acquire` | A read that must see a write it just made |
| `read` | A list, a report, anything that tolerates replication lag |

`read` routes to a replica when the cluster has one and to the primary when it
does not, so it is always safe to call and adding replicas later is a
configuration change. **Read-your-writes does not hold there.** The API contract
exposes the difference to clients as `?consistent_after=<position>`.

`execute` holds the retry loop, and it lives here and not in `erp-eventlog`
because each attempt needs a transaction and a transaction needs a permit from
this tenant's lane. A version taking a bare `PgPool` would either hand out an
unmetered connection or hold one permit across every attempt.

```rust
let committed = db.execute::<Account, _, LedgerError>(
    &id, ledger::upcasters(), &metadata,
    |loaded| {
        if loaded.aggregate.is_open() { return Ok(Decision::nothing()); }
        Ok(Decision::one(AccountEvent::Opened { code, name, kind }))
    },
).await?;
```

### CommandError

```rust
pub enum CommandError<E> {
    Pool(PoolError),
    Execute(ExecuteError<E>),
}
```

Two layers, kept apart because they mean different things to a caller.
`PoolError::Overloaded` is "come back in a moment" and deserves a 503, while
everything inside `ExecuteError` is about the command itself. Flattening them
would turn backpressure into a 500.

## The connection budget

```rust
pub enum Lane { Interactive, Client, Background }

pub trait Budget: Debug + Send + Sync {
    fn permit(&self, lane: Lane) -> Result<OwnedSemaphorePermit, PoolError>;
}

pub enum PoolError {
    UnknownCluster(String),
    Overloaded { lane: Lane },
    Connect(sqlx::Error),
}
```

Three bulkheads, sized separately so one class of traffic cannot exhaust another.
The API layer picks the lane from the authenticated audience and the route.

**Interactive** is an employee waiting on a screen. Smallest allowance and most
protected, because a counter that stops working is worse than a slow consumer
app. **Client** is a tenant's customers through their app or website, which is
the flood. **Background** is projections, outbox delivery, migrations and
reapers, and it yields to both, because nobody is watching.

`permit` **fails fast**. A caller queued against an exhausted budget is a request
holding resources it will not get, and that is how a slow database becomes an
outage.

### Why Budget is a trait

In the shared fleet one process serves many tenants, so a permit is drawn from a
budget shared across all of them, which is `erp_control::TenantPools`. A tenant
running as its own deployment has no fleet to share with and must not link one.
Both answer the same question, may this operation run now, so the answer is a
trait and the two deployments supply different implementations.

### Conn and Tx

```rust
pub struct Conn { … }   // Deref/DerefMut to PgConnection
pub struct Tx   { … }   // Deref/DerefMut to Transaction

impl Tx {
    pub async fn commit(self) -> Result<(), sqlx::Error>;
    pub async fn rollback(self) -> Result<(), sqlx::Error>;
}
```

Each holds its budget permit for exactly as long as it lives. Dropping a `Conn`
returns both the connection and the permit.

`Tx` does not commit on drop. An unfinished transaction rolls back, which is the
safe default and matches sqlx.

## Roles and capabilities

```rust
pub enum Role { Owner, Accountant, Clerk, Viewer }

pub enum Capability { Read, PostEntries, ManageAccounts, ManageTenant }

impl Role {
    pub const fn allows(self, capability: Capability) -> bool;
    pub const fn as_str(self) -> &'static str;
    pub const ALL: [Self; 4];
}
```

The matrix, which is the whole of it:

| | Read | PostEntries | ManageAccounts | ManageTenant |
|---|---|---|---|---|
| `Owner` | ✓ | ✓ | ✓ | ✓ |
| `Accountant` | ✓ | ✓ | ✓ | |
| `Clerk` | ✓ | ✓ | | |
| `Viewer` | ✓ | | | |

`Role::allows` is **the one place authorization is decided**, and every check in
the system reaches it. That is what makes a fact-based refinement later one change
here instead of an audit of every handler.

### Why roles and not a permission matrix

A small business has an owner, a bookkeeper, and some staff. Handing that owner a
grid of forty checkboxes on their first day is how a product gets configured
wrong and then blamed. So the surface is four roles, and the grid is what they
graduate to. The rule engine is the architecture's answer for "graduate to",
permissions derived from facts, so a bookkeeper can be allowed to post entries
under ten thousand riyals. That refines `Role::allows` and does not replace it.

There is deliberately no separate `admin`. With the capabilities that exist it
would permit exactly what `Accountant` does, and a role that is a synonym for
another is a support question with no answer.

Capabilities are deliberately coarse. These are the distinctions the current
endpoints actually make, and a capability nobody checks is a capability nobody
has thought about.

### Access

```rust
pub struct Access { … }

impl Access {
    pub const fn new(role: Role) -> Self;
    pub fn role_in(&self, module: Option<&ModuleId>) -> Role;
    pub fn allows(&self, capability: Capability, module: Option<&ModuleId>) -> bool;
}
```

What somebody may do in a tenant, module by module: a default, and a handful of
exceptions.

Most people have one job. A structure that made every module's role explicit
would turn "give Sara access" into a form with a row per module, most of them
saying the same thing, and it would silently give a *new* module no role rather
than the obvious one. So `Access::role` is what this person is here, and
`in_module` overrides it where the tenant said something different. A module
nobody has spoken about falls back, which is what makes adding a module to the
product not a permissions migration for every existing member.

`module: None` is not "no module" in the sense of no permission. It is the
tenant's own surface, members and invitations and entitlements, which are not any
module's business and use the tenant-wide role. That is what stops an
accountant-for-sales from managing who else has access.

```rust
pub struct UnknownRole(pub String);
```

A stored role this build does not recognise is refused and not defaulted.
Defaulting down to `Viewer` would silently lock somebody out, and defaulting up
would silently let them in. Both are worse than an error naming the row.

### Where the check actually goes

Not here. `Allowed<C>` in `erp-web` is the extractor form, and
`Allowed<PostEntries>` in a handler's signature *is* the check. The failure mode
of `tenant.require(Capability::Post)?` is forgetting to write it, which is
silent, security-relevant, and invisible in review. That is the same argument
that gave `TenantDb` no public constructor.

## Modules

```rust
pub struct EnabledModules(Vec<ModuleId>);

impl EnabledModules {
    pub fn new(modules: Vec<ModuleId>) -> Self;   // sorts
    pub fn contains(&self, module: &ModuleId) -> bool;
    pub fn iter(&self) -> impl Iterator<Item = &ModuleId>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

Resolved once and carried on the `TenantDb` handle. Sorted, so equality and
logging are stable.

```rust
pub struct ModuleSetup {
    pub module: ModuleId,
    pub install_sql: &'static str,
    // groups, upcasters, seed_sql, deprecated, requires, requires_any
}

impl ModuleSetup {
    pub const fn new(module: ModuleId, install_sql: &'static str,
        groups: &'static [(&'static str, &'static str)],
        upcasters: fn() -> &'static Upcasters) -> Self;

    pub const fn seeding(self, sql: &'static str) -> Self;
    pub const fn deprecated(self, why: &'static str) -> Self;
    pub const fn requiring(self, modules: &'static [&'static str]) -> Self;
    pub const fn requiring_any(self, modules: &'static [&'static str]) -> Self;
}
```

What a module needs installed in a tenant that enables it. Data and not a trait,
because there is exactly one thing to do with it and a trait would be an
interface with one method and one implementation per module.

The simplest one, from
[`modules/ledger/src/lib.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/ledger/src/lib.rs):

```rust
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
}
```

And the one that uses everything, from
[`modules/tax_sa/src/lib.rs:143`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/lib.rs):

```rust
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(module_id(), include_str!("../schema/install.sql"),
                                 GROUPS, upcasters)
        .seeding(include_str!("../schema/seed.sql"))
        .requiring(&["ledger"])
        .requiring_any(&["sales", "purchases"])
}
```

### install_sql is structure only

Idempotent DDL, run with `raw_sql` so it may hold several statements. Data a
module needs in order to work goes in `seeding`, which runs after it under the
same `search_path`.

The split has a reason. The Saudi rate used to ride on `tax_sa`'s schema install,
because that was the only hook a module had. It worked, since the insert is
idempotent, and it made two different things look like one: a tenant's *data* was
being written by something named "install schema". That is fine until somebody
makes the reasonable-looking change of running the DDL somewhere the data must
not go, and `just prepare` is already that somewhere. It installs every module's
DDL into a throwaway type-check database, where a `configuration` row is noise at
best.

Both are idempotent, because a rebuild runs both again.

### requiring against requiring_any

The crate dependency and the entitlement dependency are not the same thing.
`tax_sa` links against both sales and purchases, and a tenant needs at least one:
a business that only sells still files a return, and demanding both would force a
shop with no supplier bills to enable a module they do not use.

`requiring` is an AND list. `requiring_any` is a group satisfied by any member.
Signup, enabling, and refusing to disable all read the same declaration.
