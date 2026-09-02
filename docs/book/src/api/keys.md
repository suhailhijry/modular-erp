# API keys

Phase 12c. Control-plane, like members and modules: a key is a way into a
tenant, and what a tenant *is* lives in the control plane.

## Two keys, because they answer two questions

A **public key** (`pk_…`) identifies. It is safe in a mobile app, in a browser,
in a support ticket and in a log line — it says which integration this is and
proves nothing.

A **private key** (`sk_…`) authenticates. It is in exactly one response, stored
hashed, and if it leaks the answer is rotation.

Systems that ship one key end up with it in both places, and then the thing that
identifies an integration in a log is the thing that can act as it.

The private key carries its public half — `sk_<token>.<secret>` — so a presented
key is looked up by one indexed row rather than by trying every key a tenant has.

## A key acts as a machine identity

Issuing one creates an identity with no password, joins it to the tenant with a
role, and records who asked for it.

Not the creator's identity: a key that carried it would die when they left, and
everything it did would read in the audit trail as theirs.

The consequence is that nothing downstream had to learn a second shape.
Membership, roles, `enter`, the audit trail and every module's
`metadata(&tenant)` work unchanged — and **a key for one tenant is nothing on
another's subdomain**, because its identity is a member of exactly one. That is a
check nobody had to remember to write.

`0004_authentication.sql` predicted this when it said API keys would be more rows
rather than more tables.

## Scopes narrow, they never widen

```json
{ "name": "Booking widget", "scopes": ["booking:read", "crm:read"], "role": "viewer" }
```

A scope is `module:capability`, or `*:capability` for every module. The wildcard
is only on the module — `*:*` is not a scope this system will parse, because a
key that may do anything is a key nobody has thought about.

Both gates have to pass: the role its identity holds **and** a scope. A key given
the owner's role and `booking:read` still cannot post a journal entry, which is
the property `an_api_key_is_narrowed_by_its_scopes_and_survives_a_rotation`
asserts directly.

A route outside any module — `/v1/members`, `/v1/keys` — needs a wildcard scope.
That is the strict reading and the right one: a key scoped to `booking:read` has
no business listing a tenant's members.

## Rotation has an overlap, because otherwise nobody rotates

```
POST /v1/keys/{key}/rotation   { "overlap_seconds": 604800 }
```

The replacement is issued immediately and the old key is given an `expires_at`.
Both work until then, so the integration holding it is redeployed on its own
schedule. Seven days is the default; **zero is legitimate** and means stop now,
for a key known to have leaked.

The replacement carries the same scopes and role. Changing what a key may do is
a different act, and folding it into a rotation is how an integration comes back
with permissions nobody chose.

## Rate limited per key

The same limiter the public surface uses, keyed by the public key. An
integration has no session and no person behind it and is the thing most likely
to loop — this is the primitive the signup note in the plan's review section has
been waiting for.

## Why the secret is not Argon2

There is nothing to brute-force. A password is short and chosen by a person; a
key is 256 bits from the OS, so the slow hash buys nothing and costs ~50ms **on
every request**. This is the same argument `session` makes for its own tokens, in
the same schema, for the same reason: what matters is that a stolen database dump
cannot be replayed.

The comparison is constant-time regardless.

## Revoking

`DELETE /v1/keys/{key}` with a reason, which is kept — "which of these did we
revoke after the incident, and why" is the question afterwards. Revoking a
revoked key is `204` and keeps the first reason, because the first reason is the
true one.

Revoked keys stay in the listing, for the same reason.
