# Building read models

A projection turns events into tables. The runner reads a batch from the log,
hands each event to every projection in a group, and advances that group's
position marker, all inside one transaction.

## The two traits

```rust
pub trait ProjectionGroup: Send + Sync + 'static {
    const NAME: &'static str;
    const SCHEMA: &'static str;
}

pub trait Projection {
    type Group: ProjectionGroup;
    fn name(&self) -> &'static str;
    async fn apply(&self, ctx: &ProjectionCtx<'_>, envelope: &Envelope,
                   conn: &mut PgConnection) -> Result<(), ProjectionError>;
}
```

A group owns a Postgres schema, and the projection transaction sets `search_path`
to that schema alone. A query naming another group's table fails with "relation
does not exist" the first time it runs, which is how law L3 gets enforced rather
than reviewed.

## What a projection is allowed to see

```rust
impl ProjectionCtx<'_> {
    pub const fn position(&self) -> LogPosition;
    pub const fn event_time(&self) -> Timestamp;
    pub fn derive_id(&self, salt: &str) -> Uuid;
    pub fn decode<E: DeserializeOwned>(&self, envelope: &Envelope) -> Result<E, UpcastError>;
}
```

That's the whole surface. There's no pool, no clock, no random number generator
and no HTTP client, so there's nothing in scope to be impure with.

`derive_id` exists because projections often need a surrogate key. Deriving it
from the position means a rebuild produces the same key, whereas `Uuid::new_v4()`
would make every replayed row different from the one it replaced.

## A projection writes and never reads

Nothing stops you writing a `SELECT` inside `apply` against your own group's
schema, and `erp-projection/tests/purity.rs` fails the build if you do.

The reason is cost. A read inside `apply` is an N+1 paid on every event and paid
again on every rebuild, and it makes the projection depend on rows that might not
exist yet halfway through a replay, so the rebuild produces different output than
the live run did.

Everything a projection needs has to arrive in the event, which is law L5. If it
wants a name, a rate or a branch, the command that emitted the event resolves it
and writes it into the payload.

## Checkpoints and leases

The position marker advances in the same transaction as the work it records. A
crash therefore leaves a marker naming exactly the events whose effects were
lost, so recovery replays those and nothing else. There's no dedup table and no
second position space to reconcile.

`SELECT ... FOR UPDATE` on the checkpoint row doubles as the lease that stops two
workers processing the same group. It costs nothing extra, since the row has to
be read anyway, and it means the mutual exclusion can't drift out of step with
the checkpoint the way a separate lock table could.

## Proving replay is reproducible

Laws L2 and L5 are claims about code that nobody can verify by reading it, since
a `Utc::now()` twelve calls deep inside a helper looks exactly like a timestamp
that came from the event.

So it gets checked by experiment. `replay_shadow` rebuilds a group from scratch
into empty tables and diffs the result against the live ones, naming the table
and usually the column where determinism was lost. It runs in CI against the
demo tenant on every commit.

The test suite for the differ includes two projections that are deliberately
non-deterministic, one reading the wall clock and one generating random ids.
Without them, an empty diff would be ambiguous between "reproducible" and "the
differ doesn't work".

## Rebuilding without an outage

`rebuild_swap` builds the new tables beside the live ones, catches them up under
the checkpoint lock, and exchanges them at the end. Dropping the schema and
rewinding the marker would also work and would leave every screen in the product
empty for as long as the replay took.

A rebuild runs at roughly four thousand events a second, measured with
`tests/rebuild_throughput.rs` against the real runner.
