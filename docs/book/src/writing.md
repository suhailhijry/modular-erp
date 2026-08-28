# The write path

Everything that changes data goes through the same shape. A handler resolves the
tenant, loads an aggregate, calls a pure function that decides, and commits the
events and effects together.

## The decision is a pure function

```rust
pub async fn try_execute<A, F, E>(
    conn: &mut PgConnection,
    id: &AggregateId,
    upcasters: &Upcasters,
    metadata: &Metadata,
    decide: F,
) -> Result<Committed<A::Event>, ExecuteError<E>>
where
    A: Aggregate,
    F: Fn(&Loaded<A>) -> Result<Decision<A::Event>, E>,
```

`decide` receives the loaded aggregate and returns either a `Decision` or the
module's own error type. It has no connection, so it can't read anything else
and can't call anyone. Everything it needs has to be loaded before it runs or
passed in, which is what makes a command testable without a database.

A `Decision` carries both halves of what happens next:

```rust
pub struct Decision<E> {
    pub events: Vec<E>,
    pub effects: Vec<Effect>,
}
```

Events describe what happened. Effects describe work to be done outside the
database, and `decision.with_effect(effect)` attaches one.

## An aggregate is a fold

```rust
pub trait Aggregate: Default + Send + Sync + 'static {
    type Event: DomainEvent;
    fn domain() -> DomainName;
    fn apply(&mut self, event: &Self::Event);
}
```

Loading one means reading its events in order and applying each to a default
value. There's no snapshot yet, and adding one is an optimisation that will
happen when a stream gets long enough to justify it.

`apply` never fails. Validation belongs in `decide`, before an event exists,
because an event that was written is a thing that happened and refusing to
replay it later would mean the history can't be read.

## Events know their own shape

```rust
pub trait DomainEvent: Serialize + DeserializeOwned + Clone + Send + Sync {
    fn event_name(&self) -> EventName;
    fn schema_version(&self) -> SchemaVersion;
}
```

The version is what lets a build read events written by older builds. See
[Upgrades](./upgrades.md) for how the upcaster chain uses it.

## What the append actually does

Appending takes a position from a single counter row using `UPDATE ...
RETURNING`, inside the caller's transaction. The row lock is what makes position
order equal commit order. The counter is ordinary transactional data, so a
rollback returns its number. A sequence would have burned it.

The practical consequence is that appends within one tenant serialise, and the
lock is held from the append until the caller's transaction ends. So **append
last**. Do the reads, do the thinking, then write.

Optimistic concurrency comes from a unique constraint on
`(stream_domain, stream_id, sequence)`. Two writers who both loaded an aggregate
at version N will both try to write N+1, and the database refuses the second.
That's also what makes retries safe, since a retry carries the same sequence.

## Proof carrying types

Some payloads validate in their constructor and keep the proof across the log
boundary:

```rust
#[derive(Serialize, Deserialize)]
#[serde(try_from = "Vec<JournalLine>")]
pub struct BalancedLines(NonEmpty<JournalLine>);
```

The `try_from` means an event replayed from storage revalidates on the way in.
A bad migration surfaces as a decode error. Without it you'd get a trial balance
that quietly doesn't balance.
