# Signing in

Three ways in, one session.

| | |
|---|---|
| `POST /v1/sessions` | An email address and a password |
| `POST /v1/codes` → `POST /v1/sessions/code` | A phone number and a one-time code |
| `Authorization: Bearer sk_…` | An API key, for an integration |

The first two produce a **session**, and it is one row whichever way it was
made. The third is not a session at all — see [API keys](./keys.md).

## Signing in with a phone number

A phone number is the identity in this market and an email address often is not.
A login that insists on one excludes people who have a phone, a bank account and
a business, and no inbox they read.

```
POST /v1/codes            { "phone": "+966500000000" }   → 202
POST /v1/sessions/code    { "phone": "+966500000000", "code": "123456" } → 201
```

**Nothing is created by asking.** No identity, no account — a request is
somebody typing a number, and creating an account on the strength of that would
let anybody fill the table by typing numbers. The identity is made when a code is
*verified*, on a code they proved they received.

Requesting answers the same way whether or not the number is known. Saying "no
account" would make this a way to find out whether somebody has one.

## Two limiters, because they fail differently

**Requesting** is bounded by a cooldown per number — sixty seconds. The failure
is somebody using this system to send texts, which costs money and annoys
whoever owns the number.

**Verifying** is bounded by attempts on the code itself — five. The failure is
guessing, and a million guesses against six digits is minutes.

One limiter would have to be the stricter of the two everywhere, which makes the
ordinary case worse in order to defend against the rarer one. Both are in the
database, so they hold across pods rather than per process.

## What the code is worth, honestly

Six digits, five minutes, single use, five attempts. The stored SHA-256 stops a
casual `SELECT *` from being a login and nothing more — twenty bits is
reversible by anybody with a laptop. `0013_one_time_codes.sql` says so rather
than implying otherwise with an expensive hash, which would cost every
verification and buy the same nothing.

Claiming a code is **one statement**: marking it used and deciding it was valid
happen together, so two requests racing with the same code resolve to one.

One answer for every way it fails — wrong, expired, spent, out of attempts,
never issued. Distinguishing them would say whether the number is known and
whether a code is outstanding.

## Two surfaces, one session

A browser gets a cookie; everything else gets a bearer token. **They are the same
session row**, so authorization has one answer rather than two parallel notions
of who somebody is.

```
Set-Cookie: erp_session=…; HttpOnly; SameSite=Strict; Secure; Path=/
```

`HttpOnly`, so a script cannot read it — the whole point of a cookie over
`localStorage`. `SameSite=Strict` is the CSRF defence: a cross-site request does
not carry the cookie at all, so there is nothing for a forged form to ride on and
no token to check on every write. It costs one thing — a link from an email into
the app arrives signed out — and the fix for that is `Lax` on a day somebody
minds.

The bearer wins when both are sent. A request that took the trouble to send a
header meant it, and a stale cookie should not quietly override it.

Logging out clears the cookie as well as the row.

## The text itself

Promised in the same transaction as the code (D9), under `sms.send` — **the same
effect kind `messaging` uses**. A sign-in code is control-plane, because
identities are, and a booking reminder is a tenant's, but they are the same act:
one handler answers for both rather than two that could drift.

The message is deliberately short. An SMS is billed per 160 characters, or per 70
in Arabic, and a code text that runs to two segments costs twice for the length
of a sentence nobody reads.
