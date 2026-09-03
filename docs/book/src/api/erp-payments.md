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

**No gateway here signs its webhook bodies.** Moyasar puts a shared secret
*inside the JSON*; Tabby offers at most a static header of the merchant's own
choosing; Tamara sends a JWT that authenticates the sender and not the payload.
None of it proves the amount.

Tamara's deserves naming, because their documentation is wrong about it. The
token is HS256 over its own header and claims — `iss`, `iat`, `exp`, nothing
else. The docs say it ensures the payload arrived "without any modifications";
it does not, because it commits to no part of the body, and it is also sent in
the query string where it lands in access logs. Two things this crate does that
Tamara's own SDK does not: the algorithm is **pinned** to HS256 rather than read
from the token — a verifier that trusts `alg` accepts `none` — and `iss` is
checked.

Tabby's header name is the merchant's to choose and Tabby fixes none, so
`SECRET_HEADER` fixes it here: one constant, so the value a tenant registers and
the value this code looks for cannot drift apart.

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

The three gateways disagree, and the disagreement is expensive:

| | `amount` |
|---|---|
| Moyasar | JSON **integer**, smallest currency unit — `1.00 SAR = 100`, `1.00 KWD = 1000` |
| Tabby | JSON **string**, major units — `"100.00"` |
| Tamara | JSON **number**, major units — `100.50` |

Moyasar's is exactly what `Money` stores, so the integer goes straight onto the
wire with no arithmetic at all. The other two go through `decimal`, which is
integer division and remainder: floating point is forbidden in this workspace,
and `100.50` has no exact binary representation anyway, so a round trip through
`f64` would be a rounding step in the middle of somebody's bill.

**Tamara forces its client to build its own body.** An unquoted decimal cannot
be produced by `serde_json` without going through an `f64`, so that one adapter
writes JSON as text — and a test asserts the result still parses, because that
is the risk it takes on.

Reading is the same hazard backwards, and it is where a halala actually goes
missing: `"amount": 300.50` parsed into a float and multiplied by a hundred.
Response amounts are read as raw text and parsed as digits. `decimal` refuses a
value with more places than the currency has rather than rounding it — quietly
rounding somebody's refund is not a decision a parser gets to make.

## Three lifecycles, and two of them mislead

| | "the customer still has to act" | "authorized, not captured" | "the money moved" |
|---|---|---|---|
| Moyasar | `initiated` | `authorized` | `paid` / `captured` |
| Tabby | `CREATED` | `AUTHORIZED` | `CLOSED` **with a capture** |
| Tamara | `new`, **`approved`** | `authorised`, `updated` | `partially_captured`, `fully_captured` |

**Tabby's `CLOSED` is not "paid".** It is the terminal state for captured in
full, cancelled without capture, *and* partially captured then closed — so the
adapter reports `Paid` only when something was actually captured, and a payment
closed with nothing captured is `Voided`. A partial capture also leaves the
payment `AUTHORIZED` for ever; the leftover is not released on its own.

**Tamara's `approved` is not `authorised`.** The customer has paid the first
instalment and the *merchant* still has to act, or the order expires after 72
hours. So `approved` reads as `Initiated`, and `capture` authorises first —
reading the `auto_captured` flag on the way, so an account configured to capture
on authorise is not captured twice.

For both, only a capture is settled money. Tamara's own phrase about
`authorised` — "you can consider the order as paid" — is about credit risk
rather than cash, and this crate does not repeat it.

## A lender wants to know who is borrowing

`Charge` carries an optional `Buyer` and `Basket`. Cards ignore both — a card is
its own credit decision, made by somebody else. Both BNPL providers require
them, and the adapters refuse without them naming the missing field: a shop
assistant can act on "we need their mobile number", and cannot act on a Tabby
validation error.

A decline is a **normal outcome** there, not a failure — which is why `Returns`
carries three URLs rather than one. Collapsing them would lose the difference
between "they changed their mind" and "they were refused credit and should be
offered a card".

## Idempotency: one out of three

Moyasar has no `Idempotency-Key` header but it has `given_id` — *"a UUID that
you generate from your side … it is going to be the ID of the created payment"*
— which is a real one. This client therefore **requires** the caller's reference
to be a UUID and refuses otherwise, because silently omitting it turns every
network timeout into a possible double charge.

Tabby has `reference_id` on capture and refund, and nothing on checkout.
**Tamara has none at all.** Both facts were searched for specifically. They are
a property of the world rather than a gap to find later, and the domain half has
to be built knowing it.

## What is honestly not here

**A live account.** Nothing in this crate has been called against a real
merchant. What is tested is the bytes it puts on the wire and the answer it
makes of every documented reply, against a server that shows those bytes.

**Everything a payment *means*.** No aggregate, no ledger posting, no refund as
a credit note, no settlement. That is `modules/payments`.
