# erp-projection

The projection runtime. Three of the architecture's laws live here, and each is
enforced by a mechanism instead of by review.

| Law | Mechanism |
|---|---|
| **L2** projections are pure functions of the event stream | `ProjectionCtx` holds no clock, no RNG, no pool. There is nothing in scope to be impure with |
| **L3** groups are the unit of consistency and never read each other | Each group owns a Postgres schema, and the runner sets `search_path` to it, so reaching outside fails at runtime |
| **L4** checkpoints advance with their effects | Both happen in one transaction, whose row lock is also the lease against a second worker |

None of those catches everything, so `replay_shadow` rebuilds a group from the
log and diffs it against the live tables. That is what turns "replay is
reproducible" from a claim into something CI checks.

**Depends on:** `erp-eventlog`.
**Used by:** `erp-worker`, `erp-control`, and every module.

## The files

| File | What is in it |
|---|---|
| [`group.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-projection/src/group.rs) | `ProjectionGroup`, `Projection`, `ProjectionCtx` |
| [`runner.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-projection/src/runner.rs) | `run_once`, `run_to_head`, `checkpoint`, `ensure_group_schema` |
| [`shadow.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-projection/src/shadow.rs) | `replay_shadow`, `rebuild_swap`, `ShadowReport` |

## ProjectionGroup

```rust
pub trait ProjectionGroup: Send + Sync + 'static {
    const NAME:   &'static str;   // checkpoint key. Lowercase, underscores
    const SCHEMA: &'static str;   // the Postgres schema this group owns exclusively
}
```

A set of tables that must agree with one another, and the unit of both
consistency and replay. Tables in one group advance together in a single
transaction against a single checkpoint. Tables in different groups never read
each other.

**Group by what must agree, not by what is related.** A ledger's postings and its
trial balance must agree, so they are one group. An audit log that merely
mentions the same accounts does not, so it is another, and keeping it separate
means it can fall behind, be rebuilt, or fail without touching the numbers anyone
is looking at.

From [`modules/sales/src/projections.rs:24`](https://github.com/suhailhijry/modular-erp/blob/main/modules/sales/src/projections.rs):

```rust
pub struct Sales;

impl ProjectionGroup for Sales {
    const NAME: &'static str = "sales";
    const SCHEMA: &'static str = "proj_sales";
}
```

## Projection

```rust
pub trait Projection: Send + Sync {
    type Group: ProjectionGroup;

    fn name(&self) -> &'static str;

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError>;
}
```

One read model, fed by the log. Three obligations:

**Pure.** Same events in, same tables out, every time. `ProjectionCtx` removes
most ways to break this and the shadow differ catches the rest.

**Confined.** Writes only to its group's schema. `search_path` is already set
when `apply` is called, so unqualified names resolve there and nowhere else.

**Ordered.** Events arrive in position order and are applied one at a time within
the group's transaction. Idempotency is not required, because L4 commits effects
and checkpoint together, so an event is never applied twice.

`conn` is inside the group's transaction. Do not commit it, do not roll it back,
and do not open another transaction on it. The runner owns the boundary, and that
is what makes L4 hold.

A projection is also forbidden from reading the database. That is L2 and it is
enforced: `crates/erp-projection/tests/purity.rs` scans every `async fn apply`
body for `fetch_one`, `fetch_optional`, `fetch_all` and `.fetch(`, and fails on a
hit. It also fails if it cannot find the five files it expects, with the message
"the scan is broken, not the code", so it cannot pass vacuously.

The shape every projection has:

```rust
impl Projection for Invoices {
    type Group = Sales;

    fn name(&self) -> &'static str { "invoices" }

    async fn apply(&self, ctx: &ProjectionCtx<'_>, envelope: &Envelope,
                   conn: &mut PgConnection) -> Result<(), ProjectionError> {
        // Filter on the name first. An event this projection does not care
        // about is skipped here, never by a failed decode.
        if !InvoiceEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<InvoiceEvent>(ctx, envelope)? {
            InvoiceEvent::Issued { number, customer, totals, .. } => {
                // unqualified table names resolve inside proj_sales
                sqlx::query!("INSERT INTO invoice (…) VALUES (…)", …)
                    .execute(&mut *conn).await?;
            }
            // …
        }
        Ok(())
    }
}
```

## ProjectionCtx

```rust
pub struct ProjectionCtx<'a> { … }

impl<'a> ProjectionCtx<'a> {
    pub const fn position(&self) -> LogPosition;
    pub const fn event_time(&self) -> Timestamp;
    pub fn derive_id(&self, salt: &str) -> Uuid;
    pub fn decode<E: DeserializeOwned>(&self, envelope: &Envelope) -> Result<E, UpcastError>;
}
```

Everything a projection is permitted to know, and it is the compile-time half of
L2. It holds no pool, no clock, no random source and no HTTP client, so a
projector cannot call `Utc::now()` because it has no reason to import chrono, and
cannot generate a random id because it has no RNG.

`event_time` is the only clock a projection may read. `Utc::now()` inside a
projector makes a replay produce different rows than the live run, and the
difference is a timestamp column nobody thinks to check.

`derive_id` gives a stable surrogate key from the event's position. `Uuid::new_v4()`
would produce a different key on every replay, so every row would differ and the
differ would report the whole table as changed, which is at least loud. Worse is
a projection that *joins* on such a key: replay silently reassociates rows.
`salt` distinguishes several ids derived from one event, a line index for
example. The namespace is a fixed constant, because changing it would change
every derived id already in a tenant's tables.

`decode` runs the payload through the upcaster chain. Filter on `event_name`
first; an event a projection does not care about should never reach a decode.

## Running a group

```rust
pub enum Progress {
    UpToDate { at: LogPosition },
    Advanced { from: LogPosition, to: LogPosition, events: usize },
    Busy,                                  // another worker holds the lease
}
impl Progress { pub const fn may_have_more(&self) -> bool; }

pub async fn run_once_in<G: ProjectionGroup>(
    conn: &mut PgConnection, projections: &[&dyn Projection<Group = G>],
    upcasters: &Upcasters, batch_size: i64,
) -> Result<Progress, RunError>;

pub async fn run_once<G: ProjectionGroup>(
    pool: &PgPool, projections: &[&dyn Projection<Group = G>],
    upcasters: &Upcasters, batch_size: i64,
) -> Result<Progress, RunError>;

pub async fn run_to_head<G: ProjectionGroup>(
    pool: &PgPool, projections: &[&dyn Projection<Group = G>],
    upcasters: &Upcasters, batch_size: i64,
) -> Result<LogPosition, RunError>;
```

`Busy` is not an error. The group is being processed, just not by this caller.

### What makes run_once_in correct

Everything below happens in one transaction:

1. `SELECT … FOR UPDATE NOWAIT` on the checkpoint row. This is the lease, so a
   second worker gets `Busy` instead of applying the same events twice.
2. `SET LOCAL search_path` to the group's schema. This is L3, so a projection
   reaching into another group's tables fails here.
3. Apply each event, in position order, through every projection.
4. Update the checkpoint.

Because the checkpoint moves with the effects, a crash anywhere in the middle
rolls back both. There is no window in which rows were written and the checkpoint
did not know.

`run_once` wraps that in a transaction of its own and rolls back when nothing was
applied, so a `Busy` or up-to-date pass leaves no trace and holds no locks past
its return.

## Checkpoints and schemas

```rust
pub async fn ensure_group_schema<G: ProjectionGroup>(conn: &mut PgConnection)
    -> Result<(), sqlx::Error>;
pub async fn ensure_group(conn: &mut PgConnection, name: &str, schema: &str)
    -> Result<(), sqlx::Error>;

pub async fn checkpoint<G: ProjectionGroup>(conn: &mut PgConnection)
    -> Result<LogPosition, sqlx::Error>;
pub async fn checkpoint_of(conn: &mut PgConnection, group: &str)
    -> Result<LogPosition, sqlx::Error>;
```

`ensure_group_schema` is called when a module is enabled for a tenant, separate
from the migrations because which groups exist depends on which modules that
tenant has.

Both have an untyped twin, and the reason is worth knowing if you ever wonder why
there are two. Provisioning installs whichever modules a tenant chose, which is a
runtime list, and a generic function awaited through that list produces a future
rustc cannot prove `Send`, reported at the HTTP route instead of at the call.
`checkpoint_of` exists for the API's `?consistent_after=`, which knows a group by
name because the group belongs to whichever module served the route.

## Shadow replay

```rust
pub struct TableDiff { … }
impl TableDiff { pub const fn is_identical(&self) -> bool; }

pub struct ShadowReport { … }
impl ShadowReport {
    pub fn is_reproducible(&self) -> bool;
    pub fn differences(&self) -> Vec<&TableDiff>;
}

pub async fn replay_shadow<G: ProjectionGroup>(
    pool: &PgPool, projections: &[&dyn Projection<Group = G>],
    upcasters: &Upcasters, batch_size: i64,
) -> Result<ShadowReport, RunError>;
```

L2 and L5 are claims about code nobody can check by reading it. A `Utc::now()`
twelve calls deep in a helper looks exactly like a timestamp that came from the
event. So it is checked by experiment: replay the whole log into an empty copy of
the group's tables, then diff. Identical means reproducible, and any difference
names the table, and usually the column, where determinism was lost.

Two details make the comparison fair. The rebuild stops at exactly the live
checkpoint, because comparing a rebuild that ran further would report differences
that are only "the log moved on", which is noise that trains people to ignore the
check. And the shadow tables are created with `LIKE … INCLUDING ALL`, so the
structure comes from the live tables. A second declaration could drift from
them.

**Why the result is trustworthy.** `tests/shadow.rs` includes projections that
are deliberately non-deterministic, one reading the wall clock and one generating
a random id. The differ has to catch both. Without them, an empty diff would be
ambiguous between "reproducible" and "the differ does not work".

This runs in CI against the demo tenant on every commit, over all four groups,
and it is available as an operator command per tenant. It replays the entire log,
so it is cheap for a demo tenant and not something to run against a large one
during business hours.

## Rebuilding without an outage

```rust
pub async fn rebuild_swap<G: ProjectionGroup>(
    pool: &PgPool, install_sql: &str,
    projections: &[&dyn Projection<Group = G>],
    upcasters: &Upcasters, batch_size: i64,
) -> Result<LogPosition, RunError>;
```

`ControlPlane::refresh_module` drops the schema, installs it again, and rewinds
the checkpoint. That is correct, and it leaves the tenant reading empty tables
until the worker catches up. Seconds on a small tenant, minutes on a large one,
and every screen in the product wrong for the whole of it. That is an outage with
a nicer name.

`rebuild_swap` builds the new tables in a staging schema from position zero while
the live ones keep serving, and exchanges the two at the end. Readers see the old
shape, then the new one.

This is also why a module's `install.sql` is schema-relative and says `invoice`
and not `proj_sales.invoice`. The same SQL has to be aimable at a staging
schema.
