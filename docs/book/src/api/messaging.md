# messaging

Reaching somebody: channels, templates that fetch their own data, and what it
costs.

**Depends on:** `crm`, and it reads `hr`, `branches`, `booking` and `sales` to
resolve an audience and render a template.
**Depended on by:** nothing.

## What Phases 7–10 assumed and did not build

They describe a domain and no way to reach anybody in it. The system had exactly
one effect kind — `email.send`, enqueued by the control plane for invitations —
which was the entire outbound surface. The tenant dispatcher had **no handlers
at all**, which is why `hr`'s expiring-document reminder had to be a health
finding: an effect enqueued from a module would have sat in the outbox for ever.

For a product sold in this market the channel is not plumbing. A reminder that
does not arrive is a chair that stays empty.

## The module in one call

```rust
messaging::send(&mut tx, &Sending {
    template: "booking.reminder".to_owned(),
    subject: Subject::new(Topic::Reservation, booking),
    key: format!("booking.reminder.{booking}"),
    extra: BTreeMap::from([("link".to_owned(), url)]),
    ..
}).await?;
```

The caller supplies a subject and a key. It does not know the customer's name,
their number, what language they read, or what the message says.

## The four corrections it makes

**A template names an audience, not an address.** The system this was read
against freezes a phone number into the thing that will be sent, so a customer
who changes their number keeps getting messages at the old one. Here the address
is a query — `client`, `worker`, `branch_manager`, `operator` — run minutes
before the send.

**A template fetches its own data.** That system has two template mechanisms
that do not meet: a database aggregate whose parameters the caller fills in by
hand, and hardcoded classes with the copy and the business name compiled in,
where changing a reminder's wording is a deploy. Both have one cause — a
template cannot ask for anything, so somebody must hand it everything.

**Bindings are declared, so they fail when the template is saved.**
`{{ reservation.starts_at }}` is in the vocabulary for a template about a
reservation and `{{ invoice.total }}` is not, and saving one that says the wrong
thing is a `400` on the author's screen. `GET /v1/messaging/vocabulary` is what
an editor shows.

**Segments are counted, and a budget refuses (L6).** SMS is billed per segment:
160 characters of the GSM alphabet, or **70 of anything else**, which in this
market means every message. A 200-character Arabic reminder is three segments
and costs three times what its author expected.

## Where each thing lives, and why none of it is a projection

| | |
|---|---|
| Templates, settings, budget | `configuration` — typed per key, versioned, named in every event's `config_version` |
| The meter | `message_meter`, in the tenant migration chain |
| Push tokens | `push_token`, in the same place |

The meter is consulted inside the transaction that adds to it, which is what
makes two sends racing for the last segment resolve to one — a budget enforced
against a projection is enforced against a number a second out of date, and for
a per-segment bill that is money. A device token is worse: it arrives from a
device, is in nobody's event log, and a rebuild would destroy it.

So the module has **no projection group and no schema**, like `hr_sa`.

## Promise, then charge

The outbox deduplicates on the key, so promising the same message twice writes
one row and says so. The charge is made **only for what was actually written**.

That is not a detail. A reminder job runs every few minutes and calls `send` for
the same booking over and over; charging each time would spend a month's budget
on one reminder. `sending_the_same_key_again_promises_nothing_and_charges_nothing`
is what holds it down.

A refusal — no template, nobody reachable, the month spent — rolls back the
caller's transaction, meter included. The charge is written before the limit is
checked because that write is the lock, so a refusal that committed would have
spent budget on a message nobody got. Same contract as `erp_occupancy::take`.

## Rendering happens in the worker, minutes before the send

The dispatcher holds **no connection** while it delivers — a documented property,
and the reason a slow relay cannot exhaust a tenant's pool — so a handler can
read nothing at all. "At send time" therefore means *as late as possible while a
connection is legitimately held*: the reminder job runs shortly before the
message goes, resolves the audience and renders the template, and hands the
dispatcher bytes.

A booking somebody moved this morning is described as it stands this morning,
which is the property that matters.

## What is honestly not here

**A provider adapter.** Twilio, Unifonic, FCM, APNs and the WhatsApp Business
API are five APIs with five sets of credentials, and this build has an account
with none of them. A client that has never made a successful call is a file that
looks finished and is not — the same judgement the WPS file got in Phase 9.

What ships is `Relay`: an outbound contract this system defines, which an
operator points at their own small service.

```json
POST https://relay.example/send
Authorization: Bearer …

{ "channel": "sms", "to": "+9665…", "subject": "", "body": "…",
  "locale": "arabic", "key": "booking.reminder.BK-1.0" }
```

`key` is the idempotency key and the relay must treat two posts with the same
one as the same message. `410 Gone` means the address is dead — a retired push
token — and is never retried; any other `4xx` is dead-lettered; `5xx`, a timeout
or a refused connection is worth another go.

That is the same choice the email handler makes in preferring SMTP to one
vendor's JSON, and it means the adapter for whichever provider a tenant uses is
a forty-line service outside this repository rather than a fork of it. A
provider adapter inside the crate is one `impl Transport` when somebody has an
account to verify it against.

**Delivery receipts.** "Sent" and "delivered" are different words and should
stay that way, but a receipt arrives as an *inbound* callback and this system
has no verified inbound surface yet. That is Phase 12; doing it first would mean
accepting somebody else's word about what happened to a message.

**A per-customer language.** The tenant's own language is one setting; a
preference per customer is a `crm` field nobody has asked for. A Saudi salon
writes Arabic to everybody.

## The defect this module found

`crm::amend_customer` decided *nothing moved* by comparing the name and the VAT
number — the only two fields the aggregate kept — so an amendment that changed
the **phone number**, the email, the address or the Latin spelling wrote no
event and did nothing at all. The caller got `Ok`.

It was found on the first day something depended on a customer's number being
current. The aggregate holds every field the event carries now, because an
aggregate cannot answer "did anything move" about a field it does not have.
