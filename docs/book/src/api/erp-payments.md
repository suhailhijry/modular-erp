# erp-payments

Asking a gateway for money, and reading the answer.

## What it knows, and what it must never learn

An amount, a reference, a card token, and the shape of one provider's HTTP API.
It does not know which invoice a payment clears, which ledger accounts it posts
to, or whether a refund is a credit note — those are `modules/payments`', and
the day this crate learns any of them is the day it stops being swappable.

The same split `erp-storage` makes, for the same reason: a tenant's gateway is a
deployment fact, and a business that already has a Moyasar account should not
need a different build.

## A card number never reaches this process

Moyasar's terms are explicit — *"Sending cardholder data to the merchant backend
is prohibited and will result in canceling the agreement between Moyasar and the
merchant"* — and every other gateway says something similar. So `Source` has one
variant, `Token`, minted in the customer's browser against the publishable key.

There is no variant holding a PAN. That is not a convenience: a struct with a
`number: String` on it is a struct somebody eventually fills in.

## `201` is not paid

Creating a payment succeeds long before anybody has been charged. A card needing
3-D Secure comes back `Initiated` with somewhere for the customer to go, and
what happens next arrives as a callback — or never arrives, because the customer
closed the tab. So `Charged` carries a `Status`, and `Status::is_settled` and
`Status::took_the_money` are different questions with different answers.

## A callback is not evidence

**No gateway researched for this build signs its webhook bodies.** Moyasar puts
a shared secret *inside the JSON*; Tabby has no signature at all; Tamara sends a
JWT that authenticates the sender and not the payload. None of it proves the
amount.

So `authenticate` returns the **payment id and nothing else**. Its Moyasar
implementation deserializes only `{secret_token, data: {id}}` — there is
literally nowhere for the body to put an amount — and `Gateway::fetch` is asked
over an authenticated connection instead. `Charged::matches` then compares the
amount *and the currency* against what was expected before anything is recorded.

That is what Moyasar's own reference plugin does, and it is the only design that
survives somebody who guessed the callback URL.

Two ordering rules fall out of the same argument, both tested:

- **The secret is checked before the body means anything.** A malformed body
  answers "not authentic", not "unreadable" — a caller who can tell those apart
  has an oracle. `Unreadable` is reserved for a body that *did* authenticate and
  still made no sense, where the answer helps whoever is on call.
- **`callback_url` is not a channel.** The `id`, `status` and `message` query
  parameters Moyasar appends are followed by the customer's own browser and are
  therefore theirs to edit. It is where somebody lands, not how this system
  learns anything.

## Money on the wire

The three chosen gateways disagree, and the disagreement is expensive:

| | `amount` |
|---|---|
| Moyasar | JSON **integer**, smallest currency unit — `1.00 SAR = 100`, `1.00 KWD = 1000` |
| Tabby | JSON **string**, major units, 2 decimal places — `"100.00"` |
| Tamara | JSON **number**, major units — `100.50` |

Moyasar's is exactly what `Money` stores, so the integer goes straight onto the
wire with no arithmetic at all. The other two need a conversion that cannot use
floating point — the workspace forbids it, and `100.50` is not representable in
binary anyway.

## `given_id` is the idempotency key

Moyasar has no `Idempotency-Key` header. It has a top-level `given_id` — *"a
UUID that you generate from your side … it is going to be the ID of the created
payment"* — so a retried charge lands on the same payment rather than charging
twice. This client therefore **requires** the caller's reference to be a UUID
and refuses otherwise, because silently omitting it turns every network timeout
into a possible double charge.

Neither Tabby nor Tamara has any equivalent, which is written down in the plan
rather than discovered later.

## What is honestly not here

**A live account.** Nothing in this crate has been called against a real
merchant. What is tested is the bytes it puts on the wire and the answer it
makes of every documented reply, against a server that shows those bytes.

**Tabby and Tamara.** Their lifecycle is genuinely different — the provider pays
the merchant and collects from the buyer — and the plan says why that is not a
card gateway wearing different branding.

**Everything a payment *means*.** No aggregate, no ledger posting, no refund as
a credit note, no settlement. That is `modules/payments`.
