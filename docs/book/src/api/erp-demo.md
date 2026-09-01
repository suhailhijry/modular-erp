# erp-demo

A tenant with every module enabled, filled with plausible data.

**Depends on:** `erp-api`.
**Used by:** `bin/demo`, and `tests/demo.rs`, which is the widest integration
test the system has.

## Why this is a client, not a script

Every step goes through the public HTTP API. Sign up, install a chart, issue an
invoice, take a payment. A demo built out of internal calls can be perfect while
the API a customer would use is broken.

**One exception, and it is a mailbox.** Signing up takes two calls with a
confirmation email in between, and the token is only ever in that email. An API
that handed it back would let anybody confirm their own signup, which is exactly
what `POST /v1/signups` refuses. So `confirmation_link` runs one `SELECT` against
the control plane's outbox and reads the message a person would have read. Both
HTTP calls are still made, in order, the way a customer makes them. It lives here and not as a back door in the product, because a back door built
for a seeder is a back door.

So it doubles as an integration test. `tests/demo.rs` runs it and then asserts
the three things that are hard to check any other way:

- every module is enabled and answering,
- every projection group rebuilds from the log to exactly what is live,
- every invariant is clean.

## Why it is deterministic

Fixed dates, fixed amounts, fixed identifiers. A demo whose numbers change
between runs cannot be screenshotted, cannot be talked through twice, and turns a
CI failure into "did the data change or did the code?".

## The surface

```rust
pub struct Seeded { … }        // what the demo produced, and how to get into it
pub enum DemoError { … }

pub fn modules() -> Vec<&'static str>;

pub async fn bootstrap(control: &ControlPlane, cluster: &str, url_variable: &str)
    -> Result<(), DemoError>;

pub async fn seed(state: &AppState, slug: &str, password: &str,
                  ttl: Option<Duration>) -> Result<Seeded, DemoError>;

pub async fn project(control: &Arc<ControlPlane>, tenant: TenantId)
    -> Result<(), DemoError>;

pub async fn get(app: &axum::Router, slug: &str, path: &str, token: &str)
    -> Result<serde_json::Value, DemoError>;
```

`seed` also holds the private `confirmation_link`, which is the mailbox above.
It fails loudly when there is no message or no link in it: a seeder that shrugged
would carry on and fail somewhere unrelated, which is the failure mode `get`
exists to refuse.

`modules()` reads from `erp_api::modules` and lists nothing of its own, so "the
demo has every module enabled" is true because it cannot be false. A module added
to the API is a module this demo signs up for on the next run.

What that does not buy: a new module still needs teaching to the seeder, or the
demo enables it and shows nothing. `tests/demo.rs` asks each module for something
only it can answer, which is where that shows up.

`bootstrap` prepares a database that has never run anything: it migrates the
control schema and registers the cluster tenants are placed on. Both are
idempotent, so running it against a live deployment is a no-op.

It lives here and not in `bin/api` because migrating on start is a deployment
decision, and several API instances racing to do it is a bad one. A one-shot
seeder is the exception: `just demo` is usually the first thing pointed at a
fresh database, and a demo that fails with `relation "cluster" does not exist` is
a demo nobody sees.

`seed` is idempotent only in the sense that every write it makes is. Re-running
it against an existing slug fails at signup, which is the honest outcome. Drop
the tenant and run it again.

`ttl` is how long the tenant lives before the reaper destroys it. `None` makes it
an ordinary tenant that nothing will ever clean up, which is right for a test
that drops its own database and wrong for anything reachable from outside.

`get` insists on the status the demo expected, and anything else stops the demo.
A seeder that logs a failure and carries on produces a half-built demo that looks
like a product bug. That is L6.

## Running it

```bash
just demo my-password
```

Or directly:

```bash
CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… DEMO_PASSWORD=… cargo run --bin demo
```

It prints the credentials it created. **Nothing here reads a default password.** A
demo is usually the most reachable thing a deployment exposes, and a credential
baked into a binary is one that is the same everywhere it runs.

In containers:

```bash
docker compose run --rm demo
curl -H 'Host: demo.localhost' http://localhost:8080/v1/tenant
```
