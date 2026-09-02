# API versioning

Phase 12d. One layer in `erp_api::router`, and a header.

## What it is not

**Not app version gating.** Nothing here cares which build of a phone app is
calling; it cares which *contract* the caller was compiled against, which is a
different fact with a different answer. A mobile app three releases behind is
fine if the contract it uses is still served.

## The shape

```
x-api-version: 1        # in, optional
x-api-current: 1        # out, on every response
x-api-minimum: 1        # out, on every response
x-api-deprecated: true  # out, when the version asked for is behind
```

The same shape as `MIGRATION_FLOOR` refusing a tenant that is too far behind, and
the same reasoning as D17's two majors: a server that will serve *anything* is a
server whose old paths were never tested.

## The refusal names what to build against

```json
{ "code": "request.api_version_too_old",
  "detail": "This API no longer serves version 1. Build against version 3; the oldest still served is 2.",
  "args": { "declared": 1, "minimum": 2, "current": 3 } }
```

A typed error a client can branch on, with the numbers in `args` — because
"unsupported" with no number is a support ticket. `400`, never a `500`.

The version headers are on the refusal too: a client that has just been told its
version is wrong is exactly the one that needs to know which is right.

## An absent header is current

Deliberately, and it is the one permissive choice here. `curl`, a browser and
this crate's own tests send nothing, and refusing them would mean the API could
not be tried without reading the documentation first.

A client that *declares* a version gets the contract it asked for; one that does
not is asking for whatever is current, which is what it gets.

Something that is not a number — `v2`, `2.1` — is a refusal rather than "assume
current". A client sending it believes it is asking for something, and serving
it whatever we have is how it finds out at the worst moment.

## There has only ever been one contract

`FLOOR` and `CURRENT` are both `1`, so every request this build sees is current.
That makes the interesting cases — behind, too old, too new — untestable through
the API.

So `decide(requested, floor, current)` is a pure function and the tests exercise
it at versions this build does not have: a hypothetical server serving `2..=4`,
with clients at 1, 2, 3, 4 and 5. **A range nobody has exercised is a range that
does not work**, and the first time it matters is the deploy that moves the
floor.
