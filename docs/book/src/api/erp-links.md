# erp-links

Short links: a token, where it points, and who followed it.

## Why a crate and not a module

It holds no business meaning (D11). This crate does not know whether the thing
on the other end is a booking, an invoice, an export somebody asked for or a
page on a supplier's website — it knows a string. Every module may make a link
in a line, nobody enables `links`, and the day this crate learns what a
reservation is, is the day it stops being usable by the next module.

It is also not a projection, for the reason `erp-occupancy` is not: **a read
model can be rebuilt; a link somebody has already been sent cannot be
un-issued.** A token in a text message on a customer's phone is a fact about the
world, so these rows live in the tenant migration chain
(`migrations/tenant/0009_short_links.sql`) where `rebuild_swap` cannot reach
them.

## The practical reason it exists

SMS is billed by length, and a segment boundary at 160 characters is a real cost
per message per customer. A booking link is otherwise a tenant subdomain, a
versioned path and an aggregate id — most of a segment before the message has
said anything.

## A token and a key are different things

```rust
let token = links::shorten(&mut tx, &links::New {
    key: format!("booking.reminder.{booking}"),
    target: format!("/v1/booking/public/reservations/{booking}"),
    external: false,
    expires_at: Some(starts_at),
    single_use: false,
    at: now,
}).await?;
```

The **key** is the caller's, and it is what makes shortening the same thing
twice return the same link rather than two (L8) — a reminder that is retried
must not send a customer a second URL. The **token** is what goes in the
message, and the database generates it at random.

Deriving the token *from* the key would make it guessable: a key reads
`booking.reminder.BK-1041`, and anybody who could guess a booking id could walk
the whole diary.

**The first target wins.** Re-shortening a key with a different target does not
move the link, because a token already on somebody's phone has to keep meaning
what it meant. Repointing is a new link, deliberately.

## Following is one statement

```sql
UPDATE short_link
   SET visits = visits + 1, ...
 WHERE token = $1
   AND (expires_at IS NULL OR expires_at > $2)
   AND (NOT single_use OR visits = 0)
```

Expiry, single use and the visit count are decided and written together, so two
people tapping a single-use link at the same instant cannot both be told yes.
Checking first and updating second is the shape of that bug, and
`only_one_of_two_people_racing_for_a_single_use_link_gets_it` is what holds it
down.

## What a target may be

An internal target is a rooted path; an external one must be `https://`.

This is an **open redirect** otherwise, and an open redirect on a tenant's own
domain is a phishing primitive: a link reading `bassat.erp.com/l/a1b2` that
lands on somebody else's login page borrows the tenant's credibility to do it.
Restricting the scheme does not stop that on its own — nothing can, once a
tenant may name any host — but `javascript:` and `data:` targets are the ones
that turn a redirect into script execution in the tenant's own origin, and those
are refusable. `//evil.test` is refused as an internal target for the same
reason: it is a path to a reader and another host to a browser.

## The route

`GET /l/{token}`, public, and the shortest path in the API.

The person tapping it has never signed in and never will, so it goes through
`erp_web::Public` — the same tenant resolution, rate limiting and
capability-free entry the public booking routes use. The token is the whole of
the authorization.

Three answers, because they are three different instructions to whoever is
holding the phone:

| | |
|---|---|
| `302` | Here you are. `Location`, and `Cache-Control: no-store` so a proxy cannot serve a single-use link twice |
| `404` | `links.no_such_link` — check it was copied whole |
| `410` | `links.expired` or `links.already_used` — ask for a new one |

## What is deliberately absent

**A row per visit.** The visit record is a count and the two ends. A link in a
marketing message can be followed thousands of times, and an unbounded table
nobody reads is a cost with no reader. Per-visit rows, with a user agent and a
referrer, are the upgrade when somebody wants attribution.

**An endpoint to create one.** Links are made by the code that sends the
message, in the same transaction. A person making one by hand is a different
feature and nobody has asked for it.
