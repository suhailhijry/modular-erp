# Effects and the worker

A command never sends an email or calls a server. It writes the intention beside
its events in the same transaction, and a worker carries it out afterwards.

That single decision buys three properties. A command that rolls back sends
nothing, a command that crashed after committing still owes what it promised,
and rebuilding a read model sends nothing at all.

## Declaring an effect

```rust
let decision = Decision::default()
    .with_events(events)
    .with_effect(Effect::new(EffectKind::new("email.send"), payload)?);
```

The kind is what routes it to a handler at delivery time. Today there's exactly
one, `email.send`. Adding SMS or push notifications means writing another
handler, and how effects get stored and retried stays exactly as it is.

## Delivering one

```rust
pub trait EffectHandler: Send + Sync {
    fn kind(&self) -> EffectKind;
    async fn deliver(&self, effect: &PendingEffect) -> Result<(), DeliveryError>;
}
```

`deliver` may be called more than once for the same effect, so a handler has to
be safe to retry. That isn't a caveat, it's the contract: the alternative is a
distributed transaction across your database and somebody else's mail server.

A `DeliveryError` says whether the failure is worth retrying. A refused address
isn't, an unreachable server is, and the dispatcher applies backoff to the second
kind and dead-letters the first.

## Three connections, not one

The dispatcher claims, delivers, then settles. The delivery in the middle holds
no database connection at all, because it's network I/O with a timeout measured
in seconds. The two database moments take a permit each and give it straight
back.

Claiming uses `FOR UPDATE SKIP LOCKED`, so two dispatchers running at once take
disjoint work and never block on each other.

## The worker visits tenants

```rust
pub trait Job: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn module(&self) -> Option<ModuleId> { None }
    async fn tick(&self, db: &TenantDb) -> Result<Activity, BoxError>;
}
```

A job returns whether it did anything. That answer drives the schedule.

## How the schedule controls connection cost

The obvious design has every worker service every tenant. It's safe, since two
workers on one projection group is already refused by the checkpoint lock, but
each worker opens a connection to each tenant just to discover there's nothing to
do. Connections then scale as workers times tenants, and by the sizing rule in
`pools.rs` that makes every tenant permanently active.

So a worker claims tenants that are **due**, holds them for one visit, and lets
the claim lapse. Nothing renews and nothing rebalances, and a worker that dies is
recovered from by doing nothing at all.

The throttle is a separate column and it's the one that controls cost. A visit
that finds nothing pushes `next_visit_at` out, doubling up to a six hour cap, so
five thousand mostly-idle tenants cost about three and a half visits a second
instead of a hundred and sixty seven. Jitter is derived from the tenant's own id
and scales with the interval, since a batch claimed together would otherwise
become due together forever.

`request_visit` pulls a tenant forward when the API knows something just
happened, which is where a push path attaches later. Polling drops to a fallback, the push
carries the load, and nothing downstream changes.
