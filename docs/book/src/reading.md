# The read path

A read request resolves the tenant, waits if it was asked to, then queries a
read model. It never loads an aggregate. Law L7 keeps the log as a write model and a rebuild
mechanism, and reads come from read models.

## Getting to a tenant's data

`TenantDb` is the only handle that reaches tenant data, and it has no public
constructor. The only way to obtain one is `ControlPlane::enter`, which checks
that the identity is active, the tenant is enterable, and a live membership joins
them.

A function taking `&TenantDb` has therefore been handed proof that those checks
passed. There's no ambient pool anywhere, so a query against the wrong tenant
isn't prevented by a `WHERE` clause, it can't be written at all.

## Budget is spent per query, not per request

Holding a `TenantDb` is free. Budget gets spent by `acquire`, `begin` and `read`,
and only for as long as the returned guard lives. A handle kept across business
logic, an HTTP call or response serialisation costs nothing.

That's what keeps connection demand proportional to concurrent queries rather
than concurrent requests. At ten thousand requests a second it's the difference
between roughly 120 connections and roughly 400.

The corollary is that you must not hold a `Conn` or `Tx` across an await that
isn't database work, since that reintroduces exactly the problem.

## Where the connection comes from

`TenantDb` holds an `Arc<dyn Budget>`, whose entire surface is one method:

```rust
pub trait Budget: Debug + Send + Sync {
    fn permit(&self, lane: Lane) -> Result<OwnedSemaphorePermit, PoolError>;
}
```

In the shared fleet that's `TenantPools`, drawing from a budget shared across
every tenant in the process. A single-tenant deployment supplies its own and
never links the fleet at all. This one trait is what makes the deployment tiers
possible.

Exhaustion returns `PoolError::Overloaded`, which becomes a 503. It fails fast. A
caller queued against an exhausted budget is a request holding resources it
won't get, and that's how a slow database becomes an outage.

## Lanes

Permits come from one of three lanes, so a flood of one kind of traffic can't
starve another. Interactive traffic is a person waiting, client traffic is a
customer-facing API, and background traffic yields to both because nobody is
watching it.

## Reading your own write

Read models are driven by a worker, so a read taken immediately after a write can
legitimately miss it. Every write returns the log position it landed at, and a
client that cares passes it back as `?consistent_after=<position>`. The read then
waits for the projection to reach that position.

Waiting beats the alternatives. Reading the log directly would mean every read
model has a second implementation that has to agree with the first, and writing
synchronously to the projection is exactly what the checkpoint design exists to
avoid.
