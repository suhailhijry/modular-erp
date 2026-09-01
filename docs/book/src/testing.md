# How it is tested

Around 717 tests run on every build, against a real Postgres. There are no
database mocks, because a mock of Postgres would be a model of what we believe
Postgres does.

## A database per test

`erp-testkit` builds a template database once per schema, then clones it for each
test with `CREATE DATABASE ... TEMPLATE`. That's about 280ms to acquire and 140ms
to drop, which is cheap enough that tests run in parallel and stay genuinely
isolated. No test cleans up after another one.

## Proving a test isn't vacuous

Writing a test that passes is easy. The discipline here is to break the code
afterwards and confirm the test fails, because a test that passes for the wrong
reason is worse than no test at all.

Several tests carry the results of that exercise in their comments. The shadow
replay differ has two deliberately broken projections in its own suite, one
reading a clock and one making random ids, so an empty diff can't quietly mean
the differ is broken.

Twice during development a test appeared to pass after a change that should have
broken it, and both times the edit hadn't actually applied. `cargo fmt` had split
the target line, so a pattern-based edit matched nothing. The tell was
`git checkout` reporting "Updated 0 paths" straight after an edit that claimed to
succeed. Those two facts are contradictory and the second one is right.

## Rules enforced by scanning the source

Some invariants can't be expressed in the type system, so a test reads the source
and fails on a violation.

`tests/pooler.rs` fails on a session-scoped `SET`, a session advisory lock, or a
`LISTEN` outside the files where DDL legitimately needs one. Those are the things
a transaction pooler won't carry between statements.

`tests/purity.rs` fails on a database read inside a projection's `apply`.

`tests/write_side.rs` fails on an aggregate load outside command handling.

`tests/idempotence.rs` fails on a write path that generates its own identity.

`tests/boundary.rs` fails on a module that depends on the control plane.

Each of these carries a second assertion that the scan actually found something.
Without it the test goes quietly green the day somebody renames a method or
reformats a closing brace, which is how a source-scanning test rots into
decoration.

## What runs where

```bash
just check
```

Format check, clippy with warnings denied, then the tests.

Tests needing the network or real credentials are marked ignored, which covers
the soak test, the rebuild benchmark, and the ZATCA sandbox tests that need a
real taxpayer certificate. CI runs the same command against Postgres 18 and
Redis as service containers, plus a second job that regenerates the offline query
data and fails if it moved.

Redis is a required service. The tests that need it refuse to run without it, so
a runner without Redis fails four tests instead of quietly covering less than the
badge claims.
