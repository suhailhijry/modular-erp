# erp-worker

Everything the system does that no request asked for: advancing projections,
delivering what the outbox owes, checking a tenant's invariants, and the three
binaries that run outside the API.

**Depends on:** `erp-control`, `erp-projection`.
**Used by:** nothing. It is a leaf, and `bin/worker.rs` is a composition root.

## Three problems, three mechanisms

| Problem | Mechanism |
|---|---|
| Which worker looks at which tenant | A per-visit lease claimed with `FOR UPDATE SKIP LOCKED` |
| Not burning connections on idle tenants | `next_visit_at`, pushed out by a visit that found nothing |
| Stopping without losing work | A `CancellationToken` checked between ticks, then a `TaskTracker` drain |

## The files

| File | What is in it |
|---|---|
| [`worker.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-worker/src/worker.rs) | `Worker`, `WorkerConfig`, `Shutdown`, the visit loop |
| [`job.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-worker/src/job.rs) | `Job`, `PlatformJob`, `Activity` |
| [`jobs.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-worker/src/jobs.rs) | `ProjectionJob`, `OutboxJob`, `PlatformOutboxJob` |
| [`health.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-worker/src/health.rs) | `HealthJob`, `Invariant`, `Finding` |
| [`mail.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-worker/src/mail.rs) | `EmailHandler`, `Mailer`, `Smtp` |
| [`bin/worker.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-worker/src/bin/worker.rs) | The composition root: every job, every handler, every invariant |
| [`bin/migrator.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-worker/src/bin/migrator.rs) | Fleet migration and the two pre-deploy gates |
| [`bin/reaper.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-worker/src/bin/reaper.rs) | Destroys expired demo tenants, abandons stuck signups, sweeps stale links, delivered control-plane mail and empty unclaimed databases |
| [`bin/operator.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-worker/src/bin/operator.rs) | Grants the first superadmin, and revokes the last |

## Job

```rust
pub enum Activity { Idle, Worked }
impl Activity { pub const fn worked(&self) -> bool; }

pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

pub trait Job: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn module(&self) -> Option<ModuleId> { None }
    async fn tick(&self, db: &TenantDb) -> Result<Activity, BoxError>;
}
```

One kind of background work, for one tenant. Three obligations:

**Bounded.** A tick does *some* work and returns. It does not loop until
finished. The worker decides how many ticks a tenant gets before yielding its
slot, and that is what stops one busy tenant starving the rest.

**Abandonable.** The worker may stop calling `tick` at any point between calls: a
deploy, a crash, a lost lease. Anything a tick leaves behind must be safe to find
later, which for everything here means each tick is its own transaction.

**Honest about `Activity`.** It drives the visit schedule. A tenant that worked is
looked at again immediately; one that did not is pushed out by the idle interval.
Getting it wrong in the `Worked` direction burns connections on an idle tenant.
Getting it wrong in the `Idle` direction leaves work sitting.

`module()` is what makes "modular" mean something. The worker skips a job for
tenants that have not enabled its module, so a tenant declining accounting pays
nothing for its projections. `None` is a kernel job that every tenant gets.

## PlatformJob

```rust
pub trait PlatformJob: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    async fn tick(&self, control: &ControlPlane) -> Result<Activity, BoxError>;
}
```

Background work for the platform, not for any one tenant.

A `Job` is handed a `TenantDb`, and the things that need this have no tenant. The
control plane's outbox is the first: an invitation is a control-plane row, so the
promise to email it is a control-plane row, and there is no tenant database it
could sensibly live in.

Running it as a `Job` would mean doing control-plane work once per tenant, under
a tenant's lease, N times a cycle. `SKIP LOCKED` would make that *safe* and it
would still be wrong, because the amount of work would scale with the number of
tenants instead of with the amount of work.

## The kernel's jobs

```rust
pub struct ProjectionJob<G: ProjectionGroup> { … }
impl<G: ProjectionGroup> ProjectionJob<G> {
    pub fn new(projections: Vec<Arc<dyn Projection<Group = G>>>,
               upcasters: Arc<Upcasters>, batch_size: i64) -> Self;
    pub fn for_module(self, module: ModuleId) -> Self;
}

pub struct OutboxJob { … }
impl OutboxJob { pub const fn new(dispatcher: Arc<Dispatcher>, batch_size: i64) -> Self; }

pub struct PlatformOutboxJob { … }
impl PlatformOutboxJob { pub const fn new(dispatcher: Arc<Dispatcher>, batch_size: i64) -> Self; }
```

Both are deliberately thin. Their whole reason to exist is that they take their
connections from `TenantDb`, so background work is metered by the same lane
budget as everything else and no pool escapes the boundary that makes
cross-tenant access a type error.

`ProjectionJob` gets its transaction from `TenantDb::begin`, so the connection is
counted against the background lane for exactly as long as the batch takes.
`run_once_in` does the lease, the batch and the checkpoint inside it, which is
L4, and committing here is what makes it hold.

`OutboxJob` takes **three connections, not one**. Claim, deliver, settle, and the
delivery in the middle holds no connection because it is network I/O with a
timeout measured in seconds. The two database moments take a permit each and give
it straight back.

## Health checks

```rust
pub struct Finding { … }
impl Finding { pub fn new(check: &'static str, detail: impl Into<String>) -> Self; }

pub trait Invariant: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn module(&self) -> Option<ModuleId> { None }
    async fn check(&self, db: &TenantDb) -> Result<Vec<Finding>, BoxError>;
}

pub struct HealthJob { … }
impl HealthJob {
    pub fn every(interval: Duration) -> Self;
    pub fn with(self, invariant: Arc<dyn Invariant>) -> Self;
    pub async fn control_findings(control: &ControlPlane) -> Result<Vec<Finding>, BoxError>;
}
impl Job for HealthJob { … }            // "health", per tenant
impl PlatformJob for HealthJob { … }    // "control.health"
```

The architecture lists what must hold per tenant. Listing them is not the same as
checking them, and a system of record that only finds out at an audit has the
worst possible discovery latency. So they run here, on the same visit loop as
everything else.

**A finding is not a user error.** Every check is of a property that cannot be
false if the code is right: log positions are contiguous by construction, effects
are delivered or dead-lettered, debits equal credits because the type says so. A
finding means the pipeline is broken, which is why they log at `error` and why
the count is worth alerting on directly.

The kernel's own invariants are checked directly by `HealthJob`, because
they apply to every tenant and there is nothing to register:

- Schema version equals target, per module
- Projection lag below threshold, per group
- Event positions contiguous (L1)
- Unresolved dead letters at zero
- Outbox backlog age below threshold

`Invariant` is how a **module** adds one, which is what makes the trial balance
the ledger's property and not the platform's. From
[`bin/worker.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-worker/src/bin/worker.rs) the registered
ones are `TrialBalance`, `CertificateExpiry`, `NoOverpaidBill` and
`NoOverpaidInvoice`.

It runs on an interval and not on every visit. A busy tenant is visited
continuously and `integrity()` counts every event, so checking a property that
changes only when something is badly wrong, several times a second, is the kind
of load nobody notices until it is the top query in the slow log. The interval
lives in memory. Keeping it in the tenant's database would cost a table, and
losing it on a deploy costs one extra check per tenant.

**The control plane is checked too**, by the same job registered as a platform
job: its outbox's two invariants, dead letters and backlog age, because that
outbox carries every signup, invitation, reset and sign-in code, and the plane
has no event log or read models. It has its own turn of the interval, so a
tenant being checked never uses it up, and its findings log the same
`invariant violated` line with `plane = "control"`. Every worker process runs
it, so N workers log a finding N times per interval.

## The Worker

```rust
pub struct WorkerConfig {
    pub name: String,                  // identifies this worker in the lease column
    pub schedule: WorkSchedule,
    pub tenants_per_claim: i64,
    pub concurrency: usize,            // tenants worked at once
    pub max_ticks_per_visit: usize,
    pub empty_claim_pause: Duration,
    pub drain_timeout: Duration,
}

pub struct Shutdown {
    pub visits: usize,
    pub failed_visits: usize,
    pub drained: bool,
    pub leases_released: u64,
}

pub struct Worker { … }
impl Worker {
    pub fn new(control: Arc<ControlPlane>, config: WorkerConfig) -> Self;
    pub fn with_job(self, job: Arc<dyn Job>) -> Self;
    pub fn with_platform_job(self, job: Arc<dyn PlatformJob>) -> Self;
    pub async fn run(&self, cancel: CancellationToken) -> Shutdown;
}

pub fn shutdown_signal() -> CancellationToken;
```

`concurrency` is the number that sets this worker's share of the connection
budget. Measured, open connections track `active_tenants × per_tenant_pool`, and
this *is* `active_tenants` for this process.

`max_ticks_per_visit` bounds how long a tenant with a large backlog can hold a
slot. It does not bound how much work gets done: the tenant is immediately due
again, so it goes to the back of the queue and not to the back of the day.

### The shutdown property, stated precisely

On SIGTERM, a batch that has started **commits**, and no batch starts after.
Then the worker waits for in-flight visits and releases its leases.

1. The claim loop stops asking for tenants.
2. Visits already in flight keep going and check the token between job ticks.
3. `TaskTracker::wait` blocks until they are done, or the drain timeout passes.
4. Leases are released, so a replacement picks the tenants up in milliseconds.

Step 2 is the part worth being precise about. Aborting mid-batch would also be
*safe*, because an unfinished transaction rolls back and the checkpoint stays
exactly where it was. The reason not to is that abandoning throws away work that
was about to commit, on every deploy, for every tenant, forever.

`Shutdown::drained == false` is **not data loss**. It is a signal that the drain
timeout is too short for the batch size.

`tests/shutdown.rs` proves the result is indistinguishable from never having been
interrupted, by rebuilding the projection from the log and diffing it.

`shutdown_signal` treats SIGTERM and SIGINT the same. SIGTERM is what an
orchestrator sends and SIGINT is Ctrl-C, both mean the same thing here, and
treating them differently is how a local run behaves unlike production. A
**second** signal aborts immediately: an operator pressing Ctrl-C twice means it,
and a drain that will not finish must not be the only way out.

## Email

```rust
pub trait Mailer: Send + Sync {
    async fn send(&self, email: &Email, key: &str) -> Result<(), MailError>;
}

pub struct EmailHandler { … }
impl EmailHandler { pub fn new(mailer: Arc<dyn Mailer>) -> Self; }
impl EffectHandler for EmailHandler { … }   // kind() == "email.send"

pub struct Smtp { … }
impl Smtp { pub fn new(url: &str, from: &str) -> Result<Self, MailError>; }
impl Mailer for Smtp { … }
```

This is the outbox's first handler, and deliberately the smallest thing that can
be one. Before it, the outbox was finished, tested and reaching nothing, and an
invitation was a link somebody copied out of an API response by hand.

**Why a `Mailer` trait and not lettre directly.** The interesting part of this
file is not SMTP. It is what a failure means: which errors are worth retrying and
which will never work, and what the handler does with an address the relay
rejects. A test of that has to be able to make the transport fail on demand, and
it must never send real mail by accident. `tax_sa`'s `Registrar` has the same
shape for the same reason.

`key` is the effect's idempotency key. SMTP has no idempotency parameter, so it
goes out as a `Message-ID`-adjacent header, which is what lets a relay, or a
person reading a mailbox, tell a duplicate delivery from two genuine invitations
to the same address.

`Smtp::new` takes lettre's URL form: `smtps://user:pass@relay:465` for implicit
TLS, `smtp://user:pass@relay:587?tls=required` for STARTTLS. **`tls=required`
matters.** lettre's `smtp://` without it will happily continue in the clear if
the relay does not offer STARTTLS, and mail carrying an invitation link is mail
carrying a credential.

## The binaries

### worker

The composition root. The only file that knows both the kernel and the modules:
`erp-worker` depends on no module and `modules/ledger` depends on no worker, and
they meet here. That is what keeps the dependency arrow pointing one way and lets
a module be dropped from a build by deleting three lines.

It registers every projection group, both outbox dispatchers, the email handler,
the four module invariants, and the two `tax_sa` jobs (`SignZatcaDocuments`,
`SubmitToZatca`).

### migrator

```bash
cargo run --bin migrator                 # apply, and rebuild read models an older build made
cargo run --bin migrator -- check        # look only: are the schema and read models where this build expects?
cargo run --bin migrator -- versions     # look only: can this build read the logs?
cargo run --bin migrator -- refresh sales
SEALING_KEY=new:…,old:… cargo run --bin migrator -- reseal         # every sealed value onto `new`
SEALING_KEY=new:…,old:… cargo run --bin migrator -- reseal check   # look only: may `old` go?
```

Or through `just migrate-fleet <mode> <module>`. **Anything else is refused**
with the usage and exit 2. An unknown word used to run as the bare, applying
command, so `reseal check` on an image from before `reseal` would have migrated
the fleet.

`reseal` calls `ControlPlane::reseal_fleet` and exits non-zero unless the result
is settled: every sealed value under the first key in `SEALING_KEY`, nothing
unopenable, every tenant reached. `reseal check` is that same answer without
writing, and it is the gate a retired key leaves the list behind. It needs
`SEALING_KEY` and neither migrates the control plane nor registers a cluster.

`check` and `versions` are the two pre-deploy gates, and they answer different
questions. `check` is about the *schema*. `versions` is about the two-deploy
rule: `erp_eventlog::upcast` refuses an event written by a newer build rather
than guessing at it, which means a build deployed out of order does not fail at
deploy time. It fails later, when a projection reaches the first event it cannot
read and stops.

**The bare form rebuilds read models too.** Each projection group's checkpoint
records the read-model version that built it (`ProjectionGroup::VERSION`). After
the migrations, `ControlPlane::survey_read_models` reads every tenant's, and every
group whose version is not this build's — behind, at 0 from before versions were
recorded, or ahead after a rollback — is rebuilt; `check` lists them instead.
A group no module declares is reported, as a module dropped rather than
deprecated, and an unreachable tenant is not a current one. The judge is the
pure `stale`, tested without a fleet. Run the bare form again after a rollout
that changed a read model: a tenant signed up on a draining pod was built with
the old one.

`a_read_model_change_bumps_its_version` pins `(group, version, sha256)` for every
module's `install.sql`, comments and whitespace squeezed out. Change the script
without moving `VERSION` and it fails with the pin to write; move one without the
other and it says which. Re-pinning the hash alone passes, which is a deliberate
act a reviewer sees.

A rebuild does **not** drop anything first. `rebuild_swap` builds the new tables
in a staging schema, catches them up under the checkpoint lock, and exchanges the
two in one transaction, stamping the version with them. A tenant reads the old
shape, then the new one, and never an empty one. `refresh <module>` is the same
rebuild for one module, on every tenant that has it enabled, whatever its
version.

**Why the API and the worker do not do this themselves.** Migrating on start is a
deployment decision, and several instances racing to do it is a bad one. It is
also the wrong shape: a process that refuses to start until every tenant is
reachable turns one unreachable cluster into a total outage. This reports; the
pipeline decides.

It exits non-zero when the fleet is not uniform afterwards — behind on a
migration or on a read model — so `check` is usable as a gate.

### reaper

```bash
cargo run --bin reaper       # or: just reap
```

Destroys demo tenants whose time is up, abandons tenants stuck in
`provisioning` for longer than `PROVISIONING_GRACE_SECONDS`, and exits. It also
sweeps unanswered signups, expired password-reset and enrolment links and the
control plane's delivered
mail and texts older than `DELIVERED_EFFECTS` (`Retention::sweep_control`), and
drops unclaimed tenant databases that hold nothing. The receipts are swept here
and not by the worker because platform jobs run every claim cycle and `outbox`
has no index on `delivered_at`.

A stuck tenant is one whose build died with its process, a crash or a deploy
kill, before it could compensate. A request that was only cut off does not
leave one, because a confirmation builds on a task the request just waits for.
The sweep runs the compensation the build never got to, so the name is free
again after the grace plus this binary's schedule.

**Why a one-shot and not a job in the worker.** The worker's unit of work is a
tenant it holds a lease on, and this deletes the tenant, including the database
the lease lives beside. It is fleet-level work with a different shape, and giving
the worker a second shape to support one caller would be inventing structure.

It still has to be scheduled, at least hourly, whether or not the deployment
has demos: a signup whose build died holds its name until this runs, and stale
links sit until it does. One-shot means a person who wants to look before it
deletes can also run it by hand.

It exits non-zero if the sweep itself failed. An individual tenant that could not
be destroyed is logged and retried on the next run, because one unreachable
cluster must not keep every other expired demo alive.

### operator

```bash
cargo run --bin operator -- grant-staff noura@erp.example superadmin
cargo run --bin operator -- revoke-staff noura@erp.example
```

Or `just operator grant-staff …`. Needs `CONTROL_DATABASE_URL`: staff are
control-plane rows and nothing here opens a tenant. **Give it `REDIS_URL` too
wherever the API has one** — it broadcasts its cache invalidations there, as the
API does, so a revocation reaches every API node at once. Without it they keep
the old role for up to the five-second cache.

**Why a CLI when `/v1/platform/staff` exists.** Every staff route needs a
superadmin, so HTTP cannot make the first one; and the route refuses to remove
the last one, so it cannot help when that account is the problem. This is both.
Holding the control database's credentials is the authority, and both commands
are recorded in the audit trail as the system.

`grant-staff` goes through `ControlPlane::grant_staff`, the same checks the route
makes: the account must already exist, have a second factor, and not already be
staff. To change a role here, revoke and grant again. `revoke-staff` is the one
thing the route cannot do — it has no last-superadmin guard — and it also ends
every session the account holds, which the route does not: it is for the
account that is the problem.

Anything other than exactly those two shapes prints the usage and exits 2 before
connecting to anything. A tool whose unrecognised arguments fall through to a
default action is a tool where a typo on an old image does something.
