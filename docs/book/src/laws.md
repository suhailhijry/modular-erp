# The eight laws

These aren't guidelines. Each one has a test behind it, and a change that breaks
a law fails the build. When a law and the code disagree, one of them is a defect.

## L1 The log has no gaps and follows commit order

Positions are consecutive, and the order of positions matches the order of
commits.

A counter row hands out each position inside the writing transaction, and the
row lock means a second writer can't take a position until the first has
committed. The counter is ordinary transactional data, so a transaction that rolls back
gives its number back. A sequence would have burned it.

## L2 A read model is a function of the log and nothing else

A projection can look at the event, its position, the event time and its own
group's tables. It has no clock, no random numbers and no network access.

This gets checked by experiment, because a `Utc::now()` buried twelve calls deep
looks exactly like a timestamp that came from the event. The system replays the whole log into empty tables and diffs the
result against the live ones. Two projections in the test suite are
deliberately wrong, one reading a clock and one generating random keys, so that
an empty diff can't quietly mean the differ itself is broken.

## L3 A projection group is the unit of agreement

Tables that must agree belong to the same group, and across groups there are no
reads at all.

The projection transaction restricts itself to one schema, so a query naming
another group's table fails the very first time it runs.

## L4 A position marker moves with the work it records

Applying a batch and recording how far you got happen in one transaction. After
a crash the marker names exactly the events whose work was lost, so recovery
replays those and nothing else.

## L5 Events carry outcomes

Covered in [Why it is built this way](./decisions.md).

## L6 Failures stop

Nothing swallows an error, and nothing carries on with a feature quietly
disabled. In a system of record a loud failure costs you an incident and a quiet
one costs you an audit.

## L7 Aggregates load only while handling a command

Reads are served by read models. Loading an object's entire history to answer a
question makes the cost of that answer grow every time the object changes, which
is exactly what a read model exists to prevent.

## L8 Every change is safe to retry

The identity of a change comes from the caller. An invoice carries the key the
client sent and a payment carries the bank's reference, so a retry arrives with
the same identity as the attempt it repeats and the database refuses the second
write.

No request handler invents an identity of its own. One that did would make its
own retries indistinguishable from new requests, and for a payment that means
taking the money twice.
