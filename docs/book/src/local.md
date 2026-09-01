# On your computer

You'll need Rust, Postgres 18, Docker and `just`.

## Setting up

Put your database settings in `.env`, which the test harness reads because cargo
doesn't.

Start Redis on port 6379:

```bash
just redis
```

The test suite needs it, and it refuses to run without it. Skipping the affected
tests would leave a suite that quietly covers less than it claims, which is worse
than one that stops and tells you. Watch the port here: the compose file
publishes Redis on 56379 so a full stack won't collide with a Redis you're
already running, which means starting it from compose leaves four tests failing.
`just redis` is the one that matches what the tests expect.

Then build the offline query data, and run this again any time you change a
migration:

```bash
just prepare
```

## Checking your work

```bash
just check
```

That runs the format check, then clippy, then the tests, in the order that fails
soonest. It's what CI runs.

The tests need [`cargo-nextest`](https://nexte.st):

```bash
cargo install cargo-nextest
```

## Seeing something real

```bash
docker compose up
```

```bash
just demo my-password
```

The second command creates a tenant with every module enabled and populates it
through the public API, so you can sign in and click around actual data.

## How queries get checked

Queries are verified against a real schema at compile time using data committed
to the repository, so a build needs no database at all. If you change a migration
and forget `just prepare`, the compiler tells you a query has no cached data, so
you find out at build time.
