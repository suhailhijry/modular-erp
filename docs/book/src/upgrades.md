# Upgrades

We support two major versions, the current one and the one before it, and an
upgrade moves a single step. Coming from further back means installing the
version in between first.

A single step is the only upgrade path that can be tested exhaustively.
Supporting jumps multiplies the test matrix by the length of the support window
and saves a customer one afternoon.

The system refuses a tenant that's further behind than it will move, and the
refusal names the version to install first, because an operator who's told only
that something is "too old" will guess.

## Before you deploy

```bash
just migrate-fleet check
```

```bash
just migrate-fleet versions
```

The first asks whether every tenant's schema is where this build expects it. The
second asks whether this build can read what's already sitting in the logs. Both
look without touching anything and exit non-zero when the answer is no, so run
them before the new processes come up.

## Applying

```bash
just migrate-fleet
```

```bash
just migrate-fleet refresh sales
```

The second rebuilds one module's read models by building the new tables beside
the live ones, catching them up, and swapping at the end. The old tables keep
answering questions until the new ones are complete.

## Why this is cheap here

An upgrade can only touch three things.

The event log can't be migrated at all, since the database refuses to change it.
What moves forward is how the bytes are interpreted, not the bytes.

Read models can be thrown away and rebuilt, and that's where a conventional
system does most of its damage during an upgrade.

The control database takes an ordinary schema change with ordinary risk.

So the one part that can't be replaced is also the one part that can't be
altered.

## Reading older events

Every event records the version of its shape that it was written under, and a
build expecting a newer shape passes the old one through small transformations
until it matches. Adding a fourth version means writing one
transformation, and the older paths keep working because they compose through
it.

An event newer than the build understands gets refused. The build will not guess
at a shape it doesn't know.
That situation comes up during a rolling deploy, which is why the order is
fixed: deploy the build that can read the new shape first, then deploy the build
that writes it.
