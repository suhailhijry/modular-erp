# erp-control

The control plane: identities, memberships, tenants, entitlements, clusters, the
fleet, and the `TenantDb` handle that is the only route to a tenant's data.

**Depends on:** `erp-tenant`, `erp-projection`, `erp-eventlog`.
**Used by:** `erp-web`, `erp-worker`, `erp-api`. **Never by a module.**

## What lives here and what does not

This plane answers three questions, all of them on the hot path of every request.

- *Who is this?* Identity.
- *May they enter this tenant?* Membership.
- *Which modules apply?* Entitlement.

It does not answer *what may they do here*. Fine-grained permission is
tenant-local and lives in the tenant's own database, next to the data it governs.
The split means no request ever joins across the two planes.

Persistence is normalized tables plus an append-only audit trail, not an event
stream (D2). These records are small, highly relational, read constantly, and
must support cross-tenant reporting, none of which an event log helps with.

## The files

| File | What is in it |
|---|---|
| [`lib.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/lib.rs) | `ControlPlane` and most of its methods, `AccessError` |
| [`model.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/model.rs) | `Tenant`, `Identity`, `Membership`, `Scope`, `Actor`, the status enums |
| [`auth.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/auth.rs) | Passwords, sessions, `SessionToken`, `InvitationToken` |
| [`members.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/members.rs) | Adding, removing and re-roling people |
| [`invitations.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/invitations.rs) | Invite links and acceptance |
| [`provision.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/provision.rs) | Provisioning, module install, refresh, demo reaping |
| [`signup.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/signup.rs) | The two halves of signing up, and the mailbox between them |
| [`pools.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/pools.rs) | `ClusterRegistry`, `PoolConfig`, `TenantPools` |
| [`placement.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/placement.rs) | Which cluster a new tenant lands on |
| [`fleet.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/fleet.rs) | Fleet migration, the migration floor, the deploy gates |
| [`leases.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/leases.rs) | Which worker visits which tenant, and when |
| [`cache.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/cache.rs) | The entry-path TTL cache |
| [`shared.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/shared.rs) | Redis: shared sessions and cross-node invalidation |
| [`mail.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/mail.rs) | The `Email` value and the `email.send` effect kind |

## ControlPlane

```rust
pub struct ControlPlane { … }

impl ControlPlane {
    pub fn new(pool: PgPool, tenants: TenantPools) -> Self;
    pub fn sharing(self, shared: Shared) -> Self;
    pub const fn pool(&self) -> &PgPool;
    pub fn tenants(&self) -> &TenantPools;
    pub fn pools(&self) -> &TenantPools;
    pub const fn shared(&self) -> Option<&Shared>;
    pub async fn migrate(&self) -> Result<(), MigrateError>;
    pub fn entry_cache_stats(&self) -> (u64, u64);
    pub fn clear_caches(&self);
    pub fn apply_invalidation(&self, what: &Invalidate);
}
```

`pool()` is the core database, control-plane queries only. It is not a route to
tenant data.

## Entering a tenant

Three doors, and which one you use is a statement about who is acting.

```rust
pub async fn enter(&self, identity_id: IdentityId, tenant_id: TenantId, lane: Lane)
    -> Result<TenantDb, AccessError>;

pub async fn enter_for_support(&self, staff_id: IdentityId, tenant_id: TenantId,
    reason: &str) -> Result<TenantDb, AccessError>;

pub async fn enter_for_maintenance(&self, tenant_id: TenantId)
    -> Result<TenantDb, AccessError>;
```

`enter` runs four checks, ordered so the cheapest refusals happen first and no
connection is spent on a request that will be denied:

1. The identity exists and is active.
2. The tenant exists and is enterable.
3. A live membership joins them.
4. The connection budget has room.

Only then is a `TenantDb` minted, and because that type has no other constructor,
every function taking one has been handed proof that all four passed.

Platform staff do not get in this way, even superadmins. **There is no
`is_system` bypass.** Support access is `enter_for_support`, which records who
and why, because an engineer reading a tenant's ledger must not be
indistinguishable from the tenant's owner doing it.

`enter_for_maintenance` takes **no identity**, and that is the whole safety
argument. A request handler always has one, so it has no way to reach this path
by accident and no way to use it to act as somebody. It is fixed to
`Lane::Background`, so however much work the fleet is doing it draws from its own
bulkhead. It is unaudited on purpose: a projection tick per tenant per interval
would bury the entries that mean something.

```rust
pub async fn access(&self, identity: IdentityId, tenant: TenantId)
    -> Result<Option<Access>, AccessError>;
```

The same answer `enter` decides on, without needing the tenant's database to
exist. Ask this when the question is about authorization and not about data.
`None` means no live membership.

### AccessError

```rust
pub enum AccessError {
    NoSuchIdentity,
    IdentitySuspended,
    NoSuchTenant,
    TenantNotActive { status: TenantStatus },
    NotAMember,
    Pool(PoolError),
    Database(sqlx::Error),
    Corrupt(String),
    TooOldToUpgrade { … },
}
```

The variants are distinct because the caller must be able to tell them apart.
"Still provisioning" is a retry and "not a member" is not.

What reaches an API client is deliberately coarser. `NoSuchTenant` and
`NotAMember` render identically, because distinguishing them tells an attacker
which tenant slugs exist. The distinction survives in logs and in `Display`.

## The entry cache

`enter` asks four questions. Uncached, that is four queries per request against a
single control database:

| Requests per second | Control-plane queries per second |
|---|---|
| 2,000 | 8,000 |
| 10,000 | 40,000 |
| 25,000 | 100,000 |

No amount of connection tuning survives the second row. The control plane is one
database and cannot be sharded the way tenant data is, so the entry path has to
be nearly free.

What is traded away is freshness, bounded by the TTL. A suspension or a revoked
membership takes effect within the TTL on nodes that did not perform it, and
immediately on the node that did.

**That window is a deliberate, documented security property.** If a shorter bound
is ever required, the answer is out-of-band invalidation and not a smaller TTL,
and `shared.rs` is that answer.

Nothing security-critical is cached longer than the TTL, and nothing is cached
negatively at all: a failed lookup is not stored, so granting access takes effect
at once. Sessions are deliberately **not** cached in process, because a stale
membership for five seconds is survivable and a stale logout is not.

Measured: a cold `enter` costs exactly 4 lookups and 200 warm ones cost 0.

## Authentication

```rust
pub const SESSION_LIFETIME: Duration = Duration::from_hours(12);

pub struct SessionToken(String);       // Debug redacted
impl SessionToken { pub fn expose(&self) -> &str; }

pub struct Session { … }               // Serializable; carries no token

// One-time links, from the `link_token!` macro. Each a distinct type.
pub struct InvitationToken(String);    // Debug redacted
pub struct SignupToken(String);        // Debug redacted

pub fn hash_password(password: &str) -> Result<String, AuthError>;

impl ControlPlane {
    pub async fn register_login(&self, identity: IdentityId, handle: String,
                                password: String) -> Result<(), AuthError>;
    pub async fn log_in(&self, handle: &str, password: &str)
        -> Result<(SessionToken, Session), AuthError>;
    pub async fn authenticate(&self, handle: &str, password: &str)
        -> Result<IdentityId, AuthError>;
    pub async fn start_session(&self, identity: IdentityId)
        -> Result<(SessionToken, Session), AuthError>;
    pub async fn session(&self, token: &str) -> Result<Session, AuthError>;
    pub async fn log_out(&self, token: &str) -> Result<(), AuthError>;
    pub async fn log_out_everywhere(&self, identity: IdentityId) -> Result<u64, AuthError>;
    pub async fn sweep_sessions(&self) -> Result<u64, AuthError>;
}
```

`register_hashed_login` is the same insert with the hashing already done. It is
`pub(crate)` and has exactly one caller: `confirm_signup`, moving the hash out of
`pending_signup`. A caller that passed a plaintext would store a password that
verifies against nothing, and the account would be unopenable instead of open.

Both token types redact their `Debug`. A token in a log line is a working
credential, and log lines outlive the sessions they mention. They are two types and not one. Both are opaque strings, both
are credentials, and neither is interchangeable with the other, so the compiler
is the cheapest place to find a mix-up.

Every login failure is `InvalidCredentials` and every failure costs the same
time. Wrong handle, wrong password, unknown handle and suspended identity are one
message for the same reason `NoSuchTenant` and `NotAMember` are.

`authenticate` is the credential half of `log_in`, separated because two flows
need to know who this is without starting a session: signing up when the address
already has an account, and accepting an invitation to one.

### register_login has no ON CONFLICT, deliberately

It used to. `ON CONFLICT (kind, handle) DO UPDATE SET secret` is the right shape
for *changing your own password* and a full account takeover for *registering a
new one*, and this function had both callers. Signing up with somebody else's
address overwrote their password, left the row pointing at their identity, and
let the attacker log in as them, from an unauthenticated endpoint.

A taken handle is now an error, and every caller decides what that means.

## Model types

```rust
pub enum Scope { Platform, Tenant(TenantId) }
impl Scope { pub const fn tenant(self) -> Option<TenantId>; }

pub enum IdentityStatus { Active, Suspended }
pub struct Identity { pub id: IdentityId, pub status: IdentityStatus,
                      pub created_at: Timestamp }
impl Identity { pub const fn is_active(&self) -> bool; }

pub enum TenantStatus { Provisioning, Active, Suspended, Deleted }
pub struct Tenant {
    pub id: TenantId,
    pub slug: String,
    pub display_name: String,
    pub status: TenantStatus,
    pub cluster: String,
    pub database_name: String,
    pub demo_expires_at: Option<Timestamp>,
    pub created_at: Timestamp,
}
impl Tenant {
    pub const fn is_enterable(&self) -> bool;   // Active only
    pub const fn is_demo(&self) -> bool;
}

pub struct Actor { pub identity: Option<IdentityId>,
                   pub on_behalf_of: Option<IdentityId> }
impl Actor {
    pub const fn system() -> Self;
    pub const fn identity(id: IdentityId) -> Self;
    pub const fn impersonating(staff: IdentityId, subject: IdentityId) -> Self;
}
```

`Scope` is one enum. A nullable tenant field would make "platform membership with
a tenant id" a state somebody has to check for.

`Provisioning` is a real state and not a transient one. Signup returns
immediately and the provisioner works in the background, so a tenant is
visible-but-not-yet-enterable for a few seconds.

`Actor::system()` is explicit. A bare `None` would let an unattributed audit row
happen by omission; this way it is a choice somebody made.

## Tenants, memberships, entitlements

```rust
pub async fn register_tenant(&self, slug: &str, display_name: &str,
    policy: PlacementPolicy, actor: Actor) -> Result<Tenant, AccessError>;
pub async fn register_tenant_on(&self, slug: &str, display_name: &str,
    cluster: &str, actor: Actor) -> Result<Tenant, AccessError>;
pub async fn tenant(&self, id: TenantId) -> Result<Option<Tenant>, AccessError>;
pub async fn tenant_by_slug(&self, slug: &str) -> Result<Option<Tenant>, AccessError>;
pub async fn activate_tenant(&self, id: TenantId, actor: Actor) -> Result<(), AccessError>;

pub async fn create_identity(&self, actor: Actor) -> Result<Identity, AccessError>;
pub async fn identity(&self, id: IdentityId) -> Result<Option<Identity>, AccessError>;
pub async fn suspend_identity(&self, id: IdentityId, reason: &str, actor: Actor)
    -> Result<(), AccessError>;
pub async fn erase_identity(&self, id: IdentityId, actor: Actor) -> Result<(), AccessError>;

pub async fn grant_membership(&self, identity_id: IdentityId, scope: Scope,
    role: &str, actor: Actor) -> Result<MembershipId, AccessError>;
pub async fn revoke_membership(&self, identity_id: IdentityId, scope: Scope,
    actor: Actor) -> Result<bool, AccessError>;
pub async fn tenants_for_identity(&self, identity_id: IdentityId)
    -> Result<Vec<Tenant>, AccessError>;

pub async fn enable_module(&self, tenant_id: TenantId, module: &ModuleId, actor: Actor)
    -> Result<(), AccessError>;
pub async fn disable_module(&self, tenant_id: TenantId, module: &ModuleId, actor: Actor)
    -> Result<(), AccessError>;
pub async fn enabled_modules(&self, tenant_id: TenantId)
    -> Result<EnabledModules, AccessError>;

pub async fn record(&self, actor: Actor, action: &str, subject_type: &str,
    subject_id: &str, detail: serde_json::Value) -> Result<(), AccessError>;
```

`register_tenant` is the normal path. Signup does not know or care which machine
it lands on. `register_tenant_on` pins one, for migrations and for an enterprise
tenant with dedicated hardware.

`activate_tenant` is called by the provisioning workflow once the database
exists, is migrated and is seeded, never before, or entry would succeed against a
database with no schema.

`enable_module` is idempotent, because the caller is usually a workflow that may
be retried. `disable_module` **never drops the module's tables**. A tenant who
downgrades and returns expects their data, and storage is reclaimed only on
explicit deletion after an export.

`record` is the only way the audit trail changes. The table refuses `UPDATE` and
`DELETE` at the database level.

### erase_identity

The identity row goes, and with it every authenticator, session and membership,
which cascade. What stays is the audit trail with this person's name removed from
it: the entries they produced remain, saying what was done and when, attributed
to nobody. That is the same shape an entry has always had for a system action.

Business records are untouched, deliberately. An invoice naming a customer is a
legal document a tax authority requires to be kept for six years, and erasing a
*user account* is a different request from erasing a *commercial record*.

## Members and invitations

```rust
pub struct Member { … }

pub async fn members(&self, tenant_id: TenantId) -> Result<Vec<Member>, AccessError>;
pub async fn add_member(&self, tenant_id: TenantId, handle: String, password: String,
    role: Role, actor: Actor) -> Result<IdentityId, MemberError>;
pub async fn change_role(&self, tenant_id: TenantId, identity: IdentityId,
    role: Role, actor: Actor) -> Result<(), MemberError>;
pub async fn remove_member(&self, tenant_id: TenantId, identity: IdentityId,
    actor: Actor) -> Result<(), MemberError>;
pub async fn set_module_role(&self, tenant_id: TenantId, identity: IdentityId,
    module: &ModuleId, role: Option<Role>, actor: Actor) -> Result<(), MemberError>;
```

`add_member` is idempotent in the direction that matters. An existing identity is
reused, so adding `owner@acme.test` to a second tenant does not make a second
account for them. Adding them to a tenant they are already in is refused, because
the caller almost certainly meant `change_role`.

`change_role` and `remove_member` both refuse to touch the last owner.

`set_module_role(…, None, …)` removes the override and puts somebody back on
their tenant-wide role, which is a different thing from setting them to `Viewer`
there. The difference matters when the tenant-wide role later changes.

```rust
pub const INVITATION_LIFETIME: Duration = Duration::from_hours(24 * 14);

pub struct Invitation { … }          // as its tenant sees it. Never carries the token
pub struct PendingInvitation { … }   // what the link-holder is shown before accepting
pub struct Accepted { … }

pub async fn invite(&self, tenant_id: TenantId, handle: String, role: Role,
    invited_by: IdentityId, accept_base: &str, locale: Locale)
    -> Result<(Invitation, InvitationToken), InvitationError>;
pub async fn invitations(&self, tenant_id: TenantId) -> Result<Vec<Invitation>, AccessError>;
pub async fn revoke_invitation(&self, tenant_id: TenantId, invitation: Uuid, actor: Actor)
    -> Result<(), AccessError>;
pub async fn pending_invitation(&self, token: &str)
    -> Result<PendingInvitation, InvitationError>;
pub async fn accept_invitation(&self, token: &str, password: String)
    -> Result<Accepted, InvitationError>;
```

The link is returned to whoever created it, once, so they can pass it on however
they already talk to that person. For a small business in this market that is
frequently better than mail. An email is promised **in the same transaction**, so
it also arrives without anybody copying anything. That email is an outbox effect,
which is what makes the second route safe: sending inline would either mail
somebody about an invitation that rolled back, or lose the send to a crash with
nothing recording it was owed.

The link is worth everything the invited address is worth, so it is treated as a
credential: 256 bits of entropy, only the SHA-256 stored, one live link per
address per tenant, and an expiry. Re-inviting an address revokes the previous
invitation. Two live links would mean that revoking one still leaves a way in,
which is not revoking.

What the link cannot do is become somebody else's account. Acceptance always
binds to the invited handle: an existing account for that address must prove
itself with its password, and a new one is created under that address and no
other.

`accept_base` is where the invitee accepts, **without** the token, which `invite`
appends. That keeps this the only place the token exists in the clear. `locale`
is the language the inviter was working in. Both are the caller's because neither
is something this crate can know: the public domain is a deployment fact.

`PendingInvitation` deliberately includes the tenant's name and the role.
Accepting is granting somebody access to something, and a link that does not say
what is a link people click without reading.

## Signing up

Two calls with a mailbox in between. An unauthenticated endpoint that built a
database was one HTTP request away from a disk, and one that wrote an
authenticator was one request away from taking somebody's address for good.

```text
  POST /v1/signups            ──►  pending_signup + an email          202
                                         │
                                   the mailbox
                                         │
  POST /v1/signups/{token}    ──►  identity, tenant, database, session 201
```

```rust
pub const SIGNUP_LIFETIME: Duration = Duration::from_hours(24);
pub const REQUEST_INTERVAL: Duration = Duration::from_mins(1);

pub struct SignupRequest {
    pub email: String,
    pub password: String,
    pub slug: String,
    pub company: String,
    pub modules: Vec<ModuleSetup>,
}

pub struct PendingSignup { … }    // never carries the token
pub struct Confirmed { … }        // tenant, identity, session
pub enum SignupError { NotValid, TooSoon { .. }, Access(..), Auth(..) }

pub async fn request_signup(&self, request: SignupRequest, confirm_base: &str,
    locale: Locale) -> Result<(PendingSignup, SignupToken), SignupError>;

pub async fn pending_signup_modules(&self, token: &str)
    -> Result<Vec<String>, SignupError>;

pub async fn confirm_signup(&self, token: &str, modules: Vec<ModuleSetup>)
    -> Result<Confirmed, SignupError>;

pub async fn sweep_signups(&self) -> Result<u64, AccessError>;
```

`request_signup` builds **nothing**. One row, one outbox effect, and a token the
caller should put in a link and forget. It checks two things first: that the slug
is free, so a name that is gone is refused at the form and not after a round trip
through a mailbox; and that an address which already has an account can prove it,
because otherwise naming a stranger's address would be a way to post mail through
us.

The password hash waits in `pending_signup` until the address answers. Writing it
to `authenticator` any sooner *is* the handle claim: signing up as
`ceo@bigcorp.example` used to lock the real owner out of ever signing up, because
they would have to prove a password they never set.

`confirm_signup` claims the row in one `UPDATE … WHERE confirmed_at IS NULL`
before anything is built, so two clicks cannot both provision. A failure
**unclaims** it, because provisioning can fail on a slug somebody took meanwhile
and burning the link over that turns a recoverable error into a support ticket.

`pending_signup_modules` exists because the stored names have to become
`ModuleSetup`s and only the composition root knows how. That resolution is not a
formality: a module withdrawn between the request and the click is refused rather
than installed from a description nothing offers any more.

### What this deliberately does not do

**Rate limit.** `REQUEST_INTERVAL` caps mail per *address*, which is what stops
the new flow being a way to fill one mailbox. Limiting per *caller* needs a
notion of caller that does not exist yet, and Phase 12c builds it for API keys.

**Reserve the slug.** A unique index on a pending slug would make squatting free,
one throwaway address per name. So first to *confirm* wins, and confirming costs
a mailbox.

**Offer a `GET` beside the confirmation.** `/v1/join/{token}` has one because
whoever opens an invitation did not write it. Whoever opens this one filled the
form in themselves, and confirming does the thing they asked for.

## Provisioning

```rust
pub struct SignedUp { … }

pub async fn sign_up(&self, email: String, password: String, slug: String,
    company: String, modules: Vec<ModuleSetup>) -> Result<SignedUp, AccessError>;

pub fn provision(&self, slug: String, company: String, owner: IdentityId,
    modules: Vec<ModuleSetup>) -> Pin<Box<dyn Future<Output = Result<Tenant, AccessError>> + Send + '_>>;

pub async fn install_module(&self, tenant_id: TenantId, setup: ModuleSetup, actor: Actor)
    -> Result<(), AccessError>;
pub async fn refresh_module(&self, tenant_id: TenantId, setup: ModuleSetup)
    -> Result<(), AccessError>;
pub async fn maintenance_pool(&self, tenant_id: TenantId) -> Result<PgPool, AccessError>;
pub async fn tenants_with_module(&self, module: &ModuleId) -> Result<Vec<Tenant>, AccessError>;
```

### Why this is not a transaction

`CREATE DATABASE` cannot run inside one, and the work spans two databases anyway.
So partial failure is real: a tenant row can exist with no database behind it, or
a database with no schema in it.

Two things make that survivable, and neither is a workflow engine. **Every step
is idempotent**, so recover and retry are the same operation. And **a failure
compensates**: `provision` drops the database and the row on its way out, which
frees the name, and the person who just failed to sign up is exactly the person
about to try that name again.

`sign_up` is what `confirm_signup` calls once the address is proved. It is no
longer reachable from the API on its own.

### Why sign_up is one method

An account, a tenant, its database, its modules, and a session to start using it
with. One business operation, because a half-done signup is not a state anyone
wants to name, and because it keeps the `async fn` chain short enough to stay
provably `Send`.

That last point is not stylistic. An axum handler's future must be `Send`, and
rustc cannot prove it for a chain of `async fn`s carrying elided lifetimes. It
reports the failure at the *route table*, naming borrows from files that look
unrelated, and `#[axum::debug_handler]` finds nothing. `provision` returns a
boxed `Pin<Box<dyn Future … + Send>>` for the same reason, which is why its
signature looks the way it does.

### install_module: schema first, entitlement second

The opposite order from `provision`, and on purpose. During provisioning the
tenant is invisible, so entitling early is free and buys retry visibility. Here
the tenant is live, and entitling before the tables exist opens a window in which
the module's routes are found and every one of them fails on a missing relation.

### refresh_module

`install.sql` is `CREATE TABLE IF NOT EXISTS` throughout, so re-running it will
not add a column to a table that already exists. That is deliberate: everything a
module projects is derived, so a changed read model is answered by a rebuild.
There is nothing to migrate. Drop the schema, install it again, replay the log into
it.

It takes `SELECT … FOR UPDATE` on the checkpoint row first, which is the same
lock a projection run takes, so it waits for a run in flight instead of racing it.

The version that does this **without an outage** is
`erp_projection::rebuild_swap`. `refresh_module` leaves the tenant reading empty
tables until the worker catches up.

### maintenance_pool

A pool straight at one tenant's database, for a deploy step. `TenantDb` is the
request path: it carries lanes, per-operation permits, and proof that somebody
was allowed in. None of that applies to a rebuild, which has no member behind it
and wants a pool because it runs several transactions.

## Demo tenants

```rust
pub async fn set_demo_expiry(&self, tenant_id: TenantId, ttl: Duration, actor: Actor)
    -> Result<(), AccessError>;
pub async fn expired_demos(&self, limit: i64) -> Result<Vec<Tenant>, AccessError>;
pub async fn reap_demo(&self, tenant: &Tenant) -> Result<bool, AccessError>;
pub async fn reap_expired_demos(&self, limit: i64) -> Result<usize, AccessError>;
```

The expiry instant is computed by Postgres and not by this process, for the same
reason event times are: two machines' clocks disagree, and the one that decides
when a database is destroyed should be the one everybody already agrees with.

`reap_demo` is the only code in the system that deletes a live tenant, so "which
tenant" is checked three times. The argument must carry an expiry, the row is
re-read under the same condition before anything is dropped, and the final
`DELETE` repeats it. A tenant converted to a real one between the sweep and this
call is skipped, never destroyed.

`reap_expired_demos` does not stop on a failure. A cluster that is unreachable
should not keep every other expired demo alive.

## Clusters and placement

```rust
pub async fn register_cluster(&self, name: &str, dsn_env: &str,
    replica_dsn_env: Option<&str>, max_active_tenants: i32, max_databases: i32,
    actor: Actor) -> Result<(), AccessError>;
pub async fn set_cluster_status(&self, name: &str, status: ClusterStatus, actor: Actor)
    -> Result<(), AccessError>;
pub async fn cluster_load(&self) -> Result<Vec<ClusterLoad>, AccessError>;
pub async fn choose_cluster(&self, policy: PlacementPolicy) -> Result<String, AccessError>;

pub enum ClusterStatus { … }
impl ClusterStatus { pub const fn accepts_placements(self) -> bool; }

pub struct ClusterLoad { … }
impl ClusterLoad {
    pub fn utilization_bp(&self) -> i64;   // basis points; 10,000 is 100%
    pub fn has_room(&self) -> bool;
}

pub enum PlacementPolicy { … }
impl PlacementPolicy { pub fn choose(self, clusters: &[ClusterLoad]) -> Option<&ClusterLoad>; }
```

`dsn_env` names the environment variable holding the connection string. **The DSN
itself is never stored**, so a control-plane backup carries no credentials.

`Draining` is the status to reach for when retiring hardware. It keeps serving
existing tenants while taking no new ones.

### What capacity means here

The soak test settled this, and it is not what one would guess. Open connections
are bounded by `concurrently_active_tenants × max_connections_per_tenant`, not by
the lane budget and not by request rate. So "can this cluster take another
tenant" is really "how many of its tenants are busy at once", and a cluster
holding ten thousand dormant tenants may have more room than one holding two
hundred busy ones.

| Limit | Bounds | Binds when |
|---|---|---|
| `max_active_tenants` | Connections | Tenants are busy |
| `max_databases` | Storage, migration time, catalog size | Tenants are numerous |

Placement respects both. `utilization_bp` returns the **maximum** of the two
ratios and not an average: a cluster 20% full on storage and 99% full on active
tenants is 99% full, and averaging would place a tenant onto it. Basis points and
not a float, for the same reason money is integers.

## Pools

```rust
pub struct PoolConfig { … }
impl PoolConfig {
    pub fn from_env() -> Self;
    pub fn demand(&self, active_tenants: usize) -> usize;
}

pub struct ClusterRegistry { … }
impl ClusterRegistry {
    pub fn new() -> Self;
    pub fn with_url(self, name: impl Into<String>, url: &str) -> Result<Self, sqlx::Error>;
    pub fn with_replica(self, name: &str, url: &str) -> Result<Self, sqlx::Error>;
    pub fn with_direct(self, name: &str, url: &str) -> Result<Self, sqlx::Error>;
    pub fn direct_options(&self, cluster: &str) -> Result<PgConnectOptions, PoolError>;
    pub fn from_env() -> Result<Self, sqlx::Error>;
}

pub struct TenantPools { … }
impl TenantPools {
    pub fn new(clusters: ClusterRegistry, config: PoolConfig) -> Self;
    pub fn available(&self, lane: Lane) -> usize;
    pub async fn cached_pool_count(&self) -> usize;
    pub async fn report_budget(&self, cluster: &str);
}
impl erp_tenant::Budget for TenantPools { … }
```

### The problem this solves

Postgres spends a process per connection. Five thousand tenants with a pool of
four each would want twenty thousand connections, which no cluster will give you.
Database-per-tenant fails here or it fails nowhere.

By Little's law, connection demand is arrival rate times how long a connection is
held:

| 10,000 req/s, permit held for… | Connections needed |
|---|---|
| The whole request (~40 ms) | ~400 |
| One query (~8 ms, 1.5 per request) | ~120 |

So **a permit is taken per database operation, not per request.** A handle held
while business logic runs, an HTTP call is made, or a response is serialized
costs nothing. This is the difference between the design scaling and not, and it
is why `TenantPools::handles` is deliberately free.

### The bound that actually sizes a cluster

The lane budget does **not** bound open connections, and assuming it does is the
mistake this paragraph exists to prevent. A connection returned to a tenant's
pool stays open until the idle timeout, so connections accumulate across every
tenant touched in that window no matter how small the budget is.

| Quantity | Bounded by |
|---|---|
| Connections *executing* | The `Lane` budget |
| Connections *open* | Active tenants × `max_connections_per_tenant` |

Measured in `tests/soak.rs` with 256 workers across 40 tenants: 95 open
connections at 40 active tenants and 4 per tenant. And 300 concurrent requests
against **one** tenant held 8 connections across two API replicas, because the
per-tenant bound was 4 per process and the lane budget never came near.

`PoolConfig` values are **per process.** Four API nodes with
`client_operations: 240` allow 960 concurrent client operations in total, so the
sum across nodes must fit the cluster's `max_connections` with headroom.

They are configuration and not constants because they used to be compiled in, and
the sum of them is a claim about a database this crate has never seen. Four
processes each holding a private 400-permit budget against a 200-connection
server is not a bug anybody wrote.

`report_budget` states this process's share and the server's limit next to each
other at start-up. It is a log line and not a refusal, because one process cannot
know how many siblings a deployment runs.

### with_replica errors on an unknown name

It used to be a no-op, which meant `with_replica("primry", …)` returned `Ok` with
the replica silently dropped. Every read would go to the primary, the deploy
would look correct, and the only symptom would be a primary carrying twice the
load it was sized for.

Relatedly, `from_env` exists because five binaries each registered the primary
and **not one of them ever called `with_replica`**, so the replica routing in
`TenantDb::read` was reachable only from a unit test.

`with_direct` and `direct_options` are for the one thing a transaction pooler
cannot serve: `CREATE DATABASE` cannot run inside a transaction, and installing a
module's schema is a sequence whose steps share a `search_path`.

## Leases and the visit schedule

```rust
pub struct Claimed { … }
pub struct WorkSchedule { … }
impl WorkSchedule {
    pub fn next_idle_delay(&self, tenant: TenantId, idle_visits: i32) -> Duration;
}

pub async fn claim_tenants(&self, owner: &str, limit: i64, schedule: WorkSchedule)
    -> Result<Vec<Claimed>, AccessError>;
pub async fn schedule_next_visit(&self, tenant_id: TenantId, after: Duration, worked: bool)
    -> Result<(), AccessError>;
pub async fn request_visit(&self, tenant_id: TenantId) -> Result<(), AccessError>;
pub async fn release_leases(&self, owner: &str) -> Result<u64, AccessError>;
```

### The lease is per visit, not per tenant

The obvious model, a worker owning a shard of tenants and renewing forever, is
more machinery than the problem needs. Two workers processing the same projection
group is already refused by the checkpoint lock (L4), so a lease is not what
makes concurrency safe. What it is for is stopping two workers from opening
connections to the same tenant at the same moment to discover there is nothing to
do.

`claim_tenants` does the claiming and the scheduling in one statement. `SKIP
LOCKED` means two workers claiming at the same instant get disjoint sets. A
worker that dies mid-visit is recovered from by the lease expiring: there is
nothing to detect and nothing to rebalance. A tenant this worker already holds is
re-claimable, so renewing and claiming are the same call.

`release_leases` on the way out is not needed for correctness. Releasing them
means a rolling deploy hands work over in milliseconds instead of one lease
interval.

### Why idle tenants are cheap

Most tenants are idle most of the time, and the sizing rule is
`connections ≈ active_tenants × per_tenant_pool`, so a design that visits every
tenant constantly makes every tenant active and blows the budget.

`next_visit_at` is the throttle. A visit that finds nothing pushes it out, and
per-tenant pools hold no connection in between. At five thousand tenants and a
thirty-second interval that is under two hundred short-lived queries a second
across the whole fleet.

The jitter is not decoration. Without it, a batch claimed together becomes due
together forever, a thundering herd re-forming itself once per interval. It comes
from the tenant's own id and not from a random source, so the spread is stable
per tenant, a worker restart does not reshuffle everything, and the function
stays pure.

`request_visit` is **the seam the push path attaches to.** Today the worker polls
on an interval; when the API can tell a worker directly that a tenant just wrote
something, it calls this. Polling becomes the floor instead of the mechanism, and
nothing downstream changes.

## The fleet

```rust
pub struct TenantSchema { … }
impl TenantSchema { pub fn is_current(&self, latest: i64) -> bool; }

pub struct FleetPlan { … }
impl FleetPlan {
    pub fn total(&self) -> usize;
    pub fn is_uniform(&self) -> bool;
}

pub const MIGRATION_FLOOR: i64 = 0;
pub const UPGRADE_FROM_RELEASE: &str = "the previous major release";

pub struct EventVersions { … }

impl ControlPlane {
    pub fn latest_tenant_migration() -> i64;
    pub async fn survey_fleet(&self) -> Result<FleetPlan, AccessError>;
    pub async fn migrate_fleet(&self) -> Result<FleetPlan, AccessError>;
    pub async fn survey_event_versions(&self)
        -> Result<(Vec<EventVersions>, Vec<(TenantId, String)>), AccessError>;
}
```

### Why this has to exist before the next migration does

`provision` runs the tenant migrations when it builds a database, and nothing has
ever run them again. So the day `migrations/tenant/0004_*.sql` ships, new tenants
get it and every existing tenant does not, while the code that needs it is
deployed to all of them. Queries compile, because they are checked against a
database that has the migration, and fail at runtime, per tenant, on the live
fleet.

At two tenants that is a manual `psql`. At two to five thousand it is an outage.

It is safe to run because sqlx records what it has applied in each tenant
database, so a tenant already current is a no-op and a re-run is the resume. It
does not stop on a failure: one unreachable cluster must not leave the rest of
the fleet un-migrated. And it can look without touching, which is what you run
before deploying and not after.

`is_uniform` is the question a deploy gate asks, and a failure counts as no. An
unreachable tenant is not a migrated one.

### The two pre-deploy gates

```bash
just migrate-fleet check      # is the fleet's schema where this build expects?
just migrate-fleet versions   # can this build read what is already in the logs?
```

The second one exists because `erp_eventlog::upcast` refuses an event written by
a newer build instead of guessing at it, which is right, and which means a build
deployed out of order does not fail at deploy time. It fails later, when a
projection reaches the first event it cannot read and stops, by which time the
pods are up and the read models are falling behind.

So the fleet is asked first: what is in the logs, and can this build read all of
it? A build that cannot is one somebody is deploying backwards, and the answer is
to roll forward.

`EventVersions` is raw counts with no opinion about what they mean. The
comparison needs the modules' upcasters and the control plane holds no domain, so
the migrator binary has both and does the judging.

### MIGRATION_FLOOR

The oldest tenant-plane migration this build will upgrade from. **Bump it to the
previous major's final migration at each major release.** Between majors it does
not move.

D17 supports the current major and the one before it, and upgrades are
sequential: N-2 reaches N by passing through N-1. That is not tidiness. A single
hop is the only upgrade path that can be exhaustively tested, and skip-version
support multiplies the matrix by the length of the support window to save a
customer one afternoon.

It is zero while the product is pre-1.0, so the predicate that uses it is tested
against a chosen floor. Testing it against the constant would prove nothing while
that constant is zero.

A tenant below the floor is refused with `AccessError::TooOldToUpgrade`, whose
`Display` names `UPGRADE_FROM_RELEASE`. "Too old" with no next step makes an
operator guess, which is the failure being prevented. A tenant that has never
been migrated is still allowed, because that is fresh provisioning and not a
skip.

## Shared state (Redis)

```rust
pub const SESSION_TTL: Duration = Duration::from_mins(1);

pub enum Invalidate { … }        // adjacently tagged
pub struct Shared { … }

impl Shared {
    pub async fn connect(url: &str) -> Result<Self, RedisError>;
    pub async fn from_env() -> Result<Option<Self>, RedisError>;
    pub async fn session(&self, digest: &[u8]) -> Option<Session>;
    pub async fn remember_session(&self, digest: &[u8], session: &Session);
    pub async fn forget_session(&self, digest: &[u8]);
    pub async fn forget_sessions_of(&self, identity: IdentityId);
    pub async fn publish(&self, what: &Invalidate);
    pub async fn subscribe(&self) -> Result<redis::aio::PubSub, RedisError>;
}

pub fn apply_invalidations_in_background(control: &Arc<ControlPlane>)
    -> Option<JoinHandle<()>>;
```

**This is not a second copy of the entry cache.** That cache answers the entry
path from process memory in nanoseconds at a 99.9% hit rate, and putting those
lookups in Redis would replace a memory read with a network round trip. What
Redis adds is the thing a per-process cache structurally cannot do: agreement
between nodes.

**Sessions.** `ControlPlane::session` runs on every authenticated request and was
deliberately uncached, because a stale logout is not survivable. So the busiest
lookup in the system was the one query that always went to the control database.
A *shared* cache resolves that, because a logout deletes the entry for every node
at once.

**Invalidation.** The entry caches invalidate locally on write. With one API
process that is complete; with three it means a role change reaches the other two
only when their TTL lapses. A write publishes what it changed, and every node
drops that key.

`SESSION_TTL` is **the blast radius of a failed logout**, not a performance knob.
Sixty seconds, matching the order of the entry cache's own TTL, because having
one number to reason about is worth more than shaving a few queries.

`Invalidate` is serialized and not sent as a bare string, so a new variant fails
to deserialize on old nodes. A bare string would be silently ignored there.
During a rolling deploy both versions are live, and "the old pods quietly stopped
invalidating" is precisely the bug that would never be found.

### When Redis is not there

`None` is a supported deployment and not a broken one, because a single API
process has nobody to agree with. Every path degrades to exactly the behaviour of
the build before this module existed, and says so in the log: a session read
falls through to Postgres, a session write is skipped, and an invalidation that
cannot be published still happened locally.

A URL that is set and unusable is an error, never a shrug. An operator who
configured Redis and typed it wrong wants to find out at start-up.

`forget_session` is the one place where a Redis outage widens a security window.
Everywhere else it costs a query, which is why this one is logged at `error`.

`apply_invalidations_in_background` takes a `Weak`, because a strong
`Arc<ControlPlane>` would keep the pools open through shutdown and the drain
would never finish.

## Mail

```rust
pub fn email_kind() -> EffectKind;    // "email.send"

pub struct Email { … }
impl Email {
    pub fn rendered(catalog: &dyn Catalog, locale: Locale, to: String,
                    subject: &Message, body: &Message) -> Self;
    pub fn promised(&self, key: String) -> erp_eventlog::Effect;
    pub fn from_payload(payload: &serde_json::Value) -> Result<Self, serde_json::Error>;
}

pub fn invitation_messages(company: &str, link: &str) -> (Message, Message);
```

Here: the shape of an email and the effect kind that carries it. Sending is an
`erp-worker` concern, and this crate has no SMTP, no relay address and no
credentials.

**The text is rendered here and not at delivery** because the effect must record
a resolved decision (L5). The recipient of an invitation has no account, so no
stored language preference exists to render from later. What does exist, at the
moment of inviting, is the language the inviter was working in, and that is gone
by the time a worker picks the row up. It also means a catalog edit does not
silently change what an already-issued invitation says, which is the same reason
an invoice stores its VAT rate.

`subject` and `body` are two codes. A subject line and a body wrap differently in
Arabic, and pretending they are one string produces a subject with a paragraph in
it.

The key on `promised` is **pinned by the caller and never derived**. The control
plane has no event log, so there is no position to derive one from, and pinning
is what makes re-inviting the same address enqueue one email and not two.
