# Notifications inside the system — design

**Phase 13c.** A notification is a durable record first and a live signal
second. One that only existed on a socket did not happen for whoever was at
lunch.

## The problem this is the answer to

The system reaches people well and tells them nothing. `messaging` (Phase 11)
resolves an audience against the read models and promises an effect — an email,
an SMS, a push — and that is the whole outbound surface. Three things follow
from there being no record:

- **A message that was sent is not a thing anybody can look at.** A reminder
  that went out at 09:00 exists in an outbox row and a gateway's logs. Nobody
  at the counter can see it.
- **Anything the system notices and nobody is waiting for is lost.** An iqama
  expiring in three weeks is logged as a *health finding* — at `error`, into an
  operator's log, alongside "the event log is not contiguous" — because there
  was nowhere else to put it. A ZATCA refusal sits in a read model until
  somebody opens the right screen.
- **Read state has nowhere to live.** "Sara has seen this, Ahmed has not" is
  per person, and it must survive a rebuild, so it cannot be a column somebody
  updates.

Phase 13a and 13b built the live half: a projection group advances, the worker
publishes `Advanced`, and every open stream re-fetches. What is missing is the
thing to re-fetch.

## What is built

Two new things and one rule.

### 1 · An employee may name the login that is that person

`hr` gains an optional `identity` on the employee record —
`hr.employee.login_linked { identity }` and `hr.employee.login_unlinked` — set
by an owner from `GET /v1/members`.

**This does not widen anything.** 9c decided that a tenant's org chart must not
feed the platform's authorization, and that stands: a reader still needs the
module role to see any notification, and no capability check consults this
field. It answers one question only — *which bell rings* — and it is the
question an inbox cannot avoid, because the person an audience resolves to is a
tenant-plane **employee** and the person who logs in is a control-plane
**identity**.

Identity ids are already in the tenant log: `Metadata.actor` has carried one on
every event a person caused since the log existed. This adds no new crossing.

**Uniqueness is checked, not enforced.** Two employees holding one login would
mean one person with two inboxes. The handler refuses a login another employee
already holds by reading the read model, and a simultaneous pair of links can
still slip through — the cost is a duplicate notification, not a wrong one. A
unique index is not available: a projection that can fail to apply is a
projection that stops.

### 2 · A `notifications` module

Requires `messaging`. Two aggregates, one projection group, five routes.

| | |
|---|---|
| Module id | `notifications` |
| Schema | `proj_notifications` |
| Group | `notifications` |
| Aggregates | `Notification` (`notifications_notification`), `Preferences` (`notifications_preferences`) |
| Events | `notifications.notification.announced`, `notifications.notification.read`, `notifications.preferences.set` |

### 3 · The rule: only what sits above the modules may announce

The dependency arrow decides this and there is no way around it:

```
notifications  →  messaging  →  booking, crm, hr, sales
        ↑
erp-api, bin/worker.rs        (the composition roots)
```

`notifications` announces by resolving an audience, which is `messaging`'s job,
which reads the domain modules' read models. A domain module calling
`notifications::announce` would close that loop and cargo would refuse to build
it.

So **announcements are raised from worker jobs and API handlers**. This is not
a workaround: three of the four producers below already live there, and the
fourth is a scan of the diary that belongs in the same place as
`BookingReminders`.

## Announcing

`notifications::announce(&mut tx, &Announcing { kind, subject, at })`, in the
caller's transaction, four steps that mirror `messaging::send`:

**1 · Resolve.** `Channel::InSystem` joins `messaging::Channel`, and
`audience::Person` gains an `identity` read from `hr`. So
`reachable_on(InSystem)` returns a login exactly where `reachable_on(Sms)`
returns a phone number — the same query that finds the branch manager finds
their inbox, from the same org-chart walk, with no second answer to maintain.

A customer has no login, so `Audience::Client` resolves to nobody in-system.
That is a fact, not a failure: reaching customers stays `messaging::send`'s job,
and a kind may not be addressed to a client (`Kind::audiences` never returns
`Client`, and a test says so).

**2 · Word it.** Both languages, rendered at announce time against
`messaging::bindings::of(subject)` — so "Reservation for Sara at 10:00" is true
as of the moment it is announced, the same guarantee a reminder has.

Copy comes from `notifications::copy`, a compiled table of one English and one
Arabic wording per kind, using the same `{{ binding }}` vocabulary a template
uses. **A tenant may override it**: an active `messaging` template named after
the kind, on the `in_system` channel, wins over the compiled copy. Its
bindings, its audience and its two languages are validated when it is saved,
exactly as every other template is.

Compiled copy is deliberately **not** in the i18n catalog: that catalogue is
audited into `docs/ERRORS.md`, and a notification is not an error.

**3 · Read each recipient's preferences.** Per identity, per kind, per channel.
A person with no preferences set gets the default: **in-system on, every paid
channel off.** A default that spends money is a default nobody chose.

**4 · Write one event, then fan out.**
`notifications.notification.announced { kind, subject, recipients, wording }`
is the durable record, and it is written before anything leaves the building.
For each recipient's other preferred channels, `messaging::deliver` charges the
meter and promises the effect — the tail half of `send`, split out so an
already-resolved person can be reached without inventing a template addressed
to them.

**An audience that resolves to nobody is a refusal**, not a silent success —
`SendError::Unreachable`'s argument applies unchanged: an announcement that
reached nobody is something only the caller can do anything about. Every
producer below logs it and carries on to the next row, exactly as
`BookingReminders` does with a customer who has no mobile number.

**Announcing twice writes nothing.** The aggregate id is
`Uuid::new_v5(NAMESPACE_OID, "<kind>:<topic>:<subject id>")`, so the second
attempt loads a notification that already exists and returns `announced: 0` —
the same shape `send` uses to report a repeat. This is what removes the queue:
every producer below is a scan that may run twice, and "have I said this
already?" is answered by trying to say it.

## The inbox

| Route | Capability | What |
|---|---|---|
| `GET /v1/notifications` | `Read` | The caller's own, newest first, `?unread=true`, paged, with the unread count |
| `POST /v1/notifications/{id}/read` | `Read` | Marks one read. Idempotent |
| `POST /v1/notifications/read` | `Read` | Marks every unread one read |
| `GET /v1/notifications/preferences` | `Read` | The effective grid, defaults filled in |
| `PUT /v1/notifications/preferences` | `Read` | Replaces the caller's own grid |

`PUT` is a **whole-grid replacement**, not a patch: the body carries every kind
the caller wants changed from the default, and anything absent goes back to the
default. A partial update of a preference grid is how two tabs open at once
produce a setting nobody chose.

**`Read` on the four that write**, which is deliberate. A viewer must be able
to clear their own bell and say they do not want SMS; neither is an
administrative act on the tenant, and requiring `PostEntries` would make the
most junior person's inbox unusable.

**The list is filtered to the caller's identity in SQL**, not in the handler's
logic — one `WHERE recipient = $1` that a reviewer can see. Somebody else's
notification is not merely hidden, it is not selected.

**Read state is an event.** `POST .../read` appends
`notifications.notification.read { by, at }`; the projection sets `read_at` on
that person's row. A rebuild reproduces exactly who had read what, which is the
13c requirement that rules out a flag on a row. Marking read twice is a no-op
the aggregate decides, not an error.

**The live half is already built.** The group advances, the worker publishes
`Advanced { group: "notifications" }`, every open staff stream re-fetches its
own inbox. The signal carries a position and never the data, so a notification
for the manager does not appear on the clerk's screen — the clerk re-fetches
and their unread count is unchanged.

## What announces, and when

Five kinds, four producers, all in `bin/worker.rs`, all the same shape: **scan
a read model for something worth telling somebody, announce what has not been
announced.** No cursors and no checkpoints — the derived id is the dedup key,
and a cheap `notifications::announced_subjects(kind, &ids)` read keeps a tick
from re-attempting what the read model already shows as announced.

| Kind | Topic | Audience | Producer |
|---|---|---|---|
| `booking.reserved` | Reservation | the assigned worker, else the branch manager | new `AnnounceNewBookings` job |
| `payments.settled` | Reservation or Invoice | branch manager | `SettleGatewayPayments` |
| `payments.failed` | Reservation or Invoice | branch manager | `SettleGatewayPayments` |
| `tax_sa.refused` | Invoice | branch manager | the ZATCA submission sweep |
| `hr.document_expiring` | Employee | branch manager | new `AnnounceExpiringDocuments` job |

`Kind::audiences` returns an **ordered** list and the first that resolves to
anybody wins. That is how `booking.reserved` reaches the stylist it was booked
with and the manager only when nobody was assigned — one field, no fallback
logic at four call sites.

Two supporting reads are added, both ordinary:

- **`booking::reserved_since(since, limit)`**, over the `reserved_on` column
  `proj_booking.reservation` already carries. No schema change and no fleet
  refresh: the instant a booking was made is already recorded, it simply had no
  reader. The window keeps the first tick after enabling from announcing every
  future booking at once.
- **`payments::finished_since(since, limit)`**, rows that settled or failed in
  a window. `settled_at` exists; a failure is windowed on `started_at`, which
  is looser and harmless because the derived id makes a repeat free.

**The health findings stay.** `work_document_expiring` and
`work_document_lapsed` keep logging for the operator, because a tenant without
the `notifications` module, or without a linked login, would otherwise be told
by nobody. Two audiences, two mechanisms — the finding is the operator's
channel and the bell is the tenant's.

## Refusals

Every one localized, English and Arabic, in the module catalogue:

| Code | When |
|---|---|
| `notifications.unknown_kind` | A preference names a kind that does not exist |
| `notifications.unknown_channel` | …or a channel that does not exist |
| `notifications.not_yours` | Marking read a notification addressed to somebody else — **404, not 403**, because a 403 confirms it exists |
| `hr.login_taken` | Linking a login another employee already holds |
| `hr.not_a_member` | Linking a login that is not a member of this tenant |
| `messaging.not_sendable` | `send` called with an `in_system` template — announcements are announced, not sent |

## Testing

Every guard below is falsified: the fix is reverted, the test is watched to
fail, and the fix is restored.

1. **A notification is a record before it is a signal** — announce, kill the
   stream, and the row is still there to be listed.
2. **Somebody else's notification is not mine** — two identities, one
   announcement, the other gets an empty list and a 404 on `read`.
3. **Read state survives a rebuild** — mark read, rebuild the group, still
   read.
4. **The audience decides the inbox** — a booking with a stylist assigned
   reaches the stylist; the same booking with nobody assigned reaches the
   manager.
5. **An unlinked employee has no inbox** — the manager with no login gets no
   notification and the announcement refuses rather than silently reaching
   nobody.
6. **A default does not spend money** — announcing with no preferences set
   writes the bell and promises no effect; turning SMS on for that kind
   promises one and charges the meter.
7. **A tenant's template beats the compiled copy**, and an inactive one does
   not.
8. **Announcing twice writes one notification** — and the second call reports
   it.
9. **The bell rings without polling** — the 13a stream carries
   `advanced{group:"notifications"}` within a second of an announcement, and
   the recipient's re-fetch shows one more unread while a second identity's
   shows none.
10. **Every kind is addressable** — a source-level test that each kind's
    audiences are legal for its topic, that its copy exists in both languages,
    and that every binding its copy names is in that topic's vocabulary.
11. **A linked login is one person's** — linking a login another employee holds
    is refused.

Plus the three source-scan meta-tests, the role matrix (which gains five
endpoints), the OpenAPI document, and `just prepare`.

## What is deliberately not built

- **A customer inbox.** `Audience::Client` resolves to nobody in-system,
  because a customer has no login. A customer portal is a product decision, not
  a notification mechanism.
- **Digests and quiet hours.** "Don't tell me between 22:00 and 07:00" is a
  real request and a scheduling problem; the preference grid is the place it
  will attach when somebody asks.
- **Per-branch scoping of the bell.** A person sees what is addressed to them,
  which the org chart already scopes. A manager of two branches gets both, and
  that is correct.
- **Announcements from inside a module.** Blocked by the dependency arrow, and
  the shape that would unblock it — raise now, address later — buys a delivery
  queue with a lag hazard that none of these five kinds needs.
- **Retiring the health findings.** See above; two audiences.

## Deploy note

**None.** Every schema change is inside the new module, which provisioning
installs when a tenant enables it. No existing projection changes shape, so no
`migrate-fleet refresh` is needed anywhere.
