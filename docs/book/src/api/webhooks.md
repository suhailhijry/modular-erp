# Inbound webhooks

Phase 12b. The first surface where somebody else's system talks to this one.

## Verification is not a slow path, it is the only path

**A callback that is trusted without being verified is somebody else's command
executed under your authority** — anybody who can reach the URL can say a
payment succeeded.

So the signature is checked before the body is treated as meaning anything: it is
not parsed, not stored, not looked at.

```
POST https://bassat.erp.com/v1/hooks/gateway
x-webhook-timestamp: 1800000000
x-webhook-signature: <HMAC-SHA256 of "1800000000.<body>", hex>

{"id":"evt_1","type":"payment.succeeded"}
```

The secret is the tenant's, sealed in `module_secret` under `webhooks.<provider>`
and never readable again. A deployment with no sealing key **refuses** rather
than accepting unverified callbacks.

## The timestamp is inside the signature

That is what makes the five-minute window a replay window rather than a
suggestion. A signature is valid for ever; a copy somebody kept is only useful if
they can also make it look recent, and they cannot — changing the timestamp
changes what was signed.

Five minutes each way, because a provider's clock is not ours and one running a
minute ahead is a provider whose every callback would otherwise be refused.

## One answer to everything a stranger can provoke

Unsigned, wrong signature, unreadable timestamp and expired all render as
`webhooks.not_verified` with a `401`. They are the same answer to somebody who
should not be here, and telling them which they got is an oracle for guessing the
rest.

The one that is *ours* — no secret configured — is a separate code and a `503`,
because it is not their mistake.

## A webhook is a command with the provider's id as its key

It will be delivered more than once, out of order, and replayed by anybody who
kept a copy. All three are answered by two decisions: verify the signature, and
record the id.

`webhook_event` has `(provider, event_id)` as its primary key, in the tenant
migration chain rather than a projection — it is the dedupe record, consulted
inside the transaction that accepts the callback, and a check against a number a
second out of date is not a check.

A repeat is still `202` with `duplicate: true`. The provider did nothing wrong,
and an error would make them retry something that is already done. `deliveries`
counts how many times they have sent it, because nine deliveries is a provider
whose retries are not being acknowledged.

## Accepted fast, processed as an effect

A provider that times out retries, and a retry storm is self-inflicted. The route
verifies, writes the row and promises `webhook.<provider>` — in **one
transaction**, because a row whose effect was not promised is a callback nothing
will ever process and the provider will not send again — then answers.

## What is not built

**A handler.** Nothing registers for `webhook.<provider>` yet: payments are 12a
and a delivery receipt is `messaging`'s to claim. Until one does, a verified
callback is recorded and its effect waits, which is what the dispatcher does with
any kind nobody handles. `GET /v1/hooks/{provider}/events` is what makes it
visible.

**The polling reconciliation** — "a payment confirmed by a webhook nobody
received is money the tenant cannot see" — needs a provider to poll, which is the
same decision.

## Why HMAC is written here

Fifteen lines of RFC 2104, verified against RFC 4231's published vectors —
including case 6, a key longer than the block size, which is the line every
hand-written HMAC gets wrong. A dependency for this would be a dependency whose
correctness is checked exactly as much.
