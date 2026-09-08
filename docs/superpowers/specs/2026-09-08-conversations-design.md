# Conversations — design

**Phase 13d.** A thread against a subject: a booking, an invoice, a customer.
Not a chat room, which nobody can find afterwards.

## The problem this is the answer to

**A customer who replies has replied to nobody.** `messaging` reaches people
well — a reminder goes out, resolved and metered, minutes before it is needed —
and there the story ends. An SMS reply arrives at a gateway and stops. The
business finds out when the customer does not turn up.

**And what somebody wrote down is scattered.** A note today is a `note: String`
on a booking and a custom field on a customer. Neither is a conversation, and
neither can hold the sentence that matters most — *"she called, moving to
Thursday"* — against the thing it is about.

Three things follow, and they are 13d's three bullets: a thread has to be
**against a subject** so it can be found afterwards; an inbound message has to
**land in it**; and an internal note and a customer-visible message have to be
**in the same thread and plainly different**.

## 1 · Messaging starts remembering what it sent

Correlating a reply to what it answers needs a record that does not exist:
`send` resolves, renders, meters, promises an effect, and keeps nothing.

So `messaging` gains **`message_sent`** in the tenant migration chain:

```sql
CREATE TABLE message_sent (
    key           TEXT PRIMARY KEY,   -- the outbox key, so a repeat records once
    channel       TEXT NOT NULL,
    addressed_to  TEXT NOT NULL,      -- the number or address it went to
    topic         TEXT,               -- what it was about, when the sender knew
    subject_id    TEXT,
    sent_at       TIMESTAMPTZ NOT NULL
);
CREATE INDEX message_sent_by_address_idx ON message_sent (addressed_to, sent_at DESC);
```

**Write-side state, not a projection**, and that is the same argument the meter
and the device tokens already make in that module: a send is an **effect
promise, not an event**, so it is not derivable from the log — and a rebuild
must not destroy it. `messaging` keeps its documented "no projections, no
schema" property intact.

`messaging::deliver` gains the subject as a parameter and writes this row in the
same transaction as the promise, so nothing is recorded that was not also
promised. One query comes out of it:

```rust
messaging::last_sent_to(conn, address, before: Timestamp, within: Duration)
    -> Option<Subject>
```

**`before`, not "now"** — see §2.

## 2 · Inbound

**The signed contract, documented.** No vendor adapter: §26's argument stands
that a client written from documentation against an account nobody has is a file
which passes its tests and fails every real message. An operator registers their
gateway relay's secret under the provider name **`messages`** and posts to the
route Phase 12b already built:

```json
{ "id": "<the gateway's own message id>", "from": "+9665…", "body": "…", "sent_at": "…" }
```

`POST /v1/hooks/messages` verifies the HMAC over `<timestamp>.<body>`,
deduplicates on `id`, records a `webhook_event` row and answers `202`. Nothing
about that route changes.

**A worker job lands it.** A webhook handler is handed no database connection,
and correlation is three read models deep. `LandInboundMessages` scans recent
`webhook_event` rows for that provider and, for each:

1. `last_sent_to(from, before: sent_at, within: 7 days)` → the subject that
   message was about → its thread;
2. otherwise `crm::customer_by_phone(from)` → that customer's own thread;
3. otherwise the **unmatched tray** (§3), which is a thread of its own keyed by
   the number.

**Correlation is against the reply's own timestamp, never the clock.** That is
what makes the answer stable: the same webhook lands on the same thread however
often the job runs, which is what lets the job have no cursor and re-scan a
window every tick — the same trick 13c uses with derived ids.

**The phone number is matched exactly.** No normalisation, no last-nine-digits
heuristic: a heuristic that matches the wrong customer puts one person's reply
in another person's conversation, which is worse than not matching. The
unmatched tray is what catches the rest, and putting the number on the customer
record is the fix that stops it recurring.

## 3 · The thread

A `conversations` module, requiring `messaging` and `crm`.

One aggregate, `Thread`, whose id is **derived from what it is about** —
`Uuid::new_v5("{topic}:{id}")`, or `"unmatched:{address}"` for the tray — so the
thread for a booking always has the same id, nothing has to be created first,
and two people opening it at once are opening one thread.

| Event | |
|---|---|
| `conversations.thread.noted` | Internal. **Never leaves.** |
| `conversations.thread.said` | Outward. Promised through `messaging::deliver` in the same transaction, charged to the meter, refused when the budget is spent (L6). |
| `conversations.thread.heard` | Inbound, carrying the gateway's message id. |
| `conversations.thread.assigned` | The tray only: these messages were about *that* after all. |

The aggregate keeps a **bounded window of message ids it has heard** — the same
idiom `hr::Employee` uses to tell a retry from a correction — so landing the
same reply twice writes nothing.

**The sender picks the channel**, and two of the four are refused:

- **SMS and email** are what a person may choose. Either is refused when that
  customer has no address for it, naming the channel.
- **WhatsApp is refused** with a message that says why: outside a 24-hour
  customer-service window Meta accepts only pre-approved templates, so free text
  typed into a thread would be accepted here and rejected there (§26). Sending
  it would be this system pretending.
- **Push is refused**: it addresses devices, not people, and a reply to a
  customer's phone app is a different act from answering their message.

**A subject with no client is notes-only.** A thread about an employee has
nobody to send to, and `said` on one is refused rather than resolved to nobody.

## 4 · The unmatched tray

An inbound message from a number matching nothing gets a thread keyed by the
number itself: `GET /v1/conversations/unmatched` lists them, newest first, and

```
POST /v1/conversations/unmatched/{address}/assign  { "topic": "reservation", "id": "BK-1" }
```

appends `assigned` to it. The projection **moves** those message rows onto the
named thread — rebuild-safe, because the assignment applies after the messages
it moves, in position order, live and during a replay alike.

**It moves what has arrived, and does not bind the number.** The next message
from it lands in the tray again, because "this number is that customer" is a
fact about the customer record and `crm` is where it belongs. The refusal to
guess that here is the same one §2 makes about matching.

## 5 · Reading it

| Route | Capability | |
|---|---|---|
| `GET /v1/conversations/{topic}/{id}` | `PostEntries` | The thread, oldest first, notes and messages together with what each one is |
| `POST /v1/conversations/{topic}/{id}/notes` | `PostEntries` | An internal note |
| `POST /v1/conversations/{topic}/{id}/messages` | `PostEntries` | Outward, on a named channel |
| `GET /v1/conversations/unmatched` | `PostEntries` | The tray |
| `POST /v1/conversations/unmatched/{address}/assign` | `PostEntries` | Move a tray thread onto a subject |

**`PostEntries` to read, not `Read`.** A thread holds staff's private notes
about a customer, and `Read` is the role for an external accountant at year end
— somebody with every reason to see the books and none to see what the front
desk wrote about a client. This is the one place in the API where reading is not
the most permissive capability, and it is deliberate.

The group advances, the 13a signal names it, an open thread re-fetches. Nothing
new is needed for the live half.

## Refusals

| Code | When |
|---|---|
| `conversations.nothing_to_say` | An empty note or message |
| `conversations.no_client` | `said` on a subject with nobody to send to — an employee thread |
| `conversations.not_reachable_there` | The chosen channel, for that customer, has no address |
| `conversations.not_a_channel_for_this` | WhatsApp or push from a thread, with the reason |
| `conversations.unknown_subject` | A topic that is not one, or an id that is not one |
| `conversations.already_assigned` | Assigning a tray thread that has been assigned |

## Testing

Every guard is falsified: revert the fix, watch the test fail, restore it.

1. **A reply lands on what it answers** — send a reminder about a booking, hear
   a reply from that number, and it is in the booking's thread.
2. **…and on the customer when nothing was sent** — no recent outbound, a known
   number, their own thread.
3. **…and in the tray when nobody is known** — an unknown number, and assigning
   it moves the messages onto a real thread.
4. **The same reply twice is one message**, however often the job runs.
5. **Correlation does not drift** — a reply is landed on the subject that was
   current *when it was sent*, not when the job ran: a later reminder about a
   different booking must not move it.
6. **A note never leaves** — posting one promises no effect and charges nothing.
7. **A message does leave** — posting one promises exactly one effect on the
   chosen channel, charges the meter, and records what was sent.
8. **A spent budget refuses the message and records nothing** — no `said` event,
   no row, no promise.
9. **A channel that customer has no address for is refused**, naming it; and
   WhatsApp is refused with its own reason.
10. **An employee thread is notes-only.**
11. **A thread survives a rebuild**, message order and kinds included.
12. **A reply reaches an open screen** — landing one advances the group, and the
    13a stream carries `advanced{group:"conversations"}` without anybody
    polling.

Plus the three source-scan meta-tests, the role matrix (five endpoints), the
OpenAPI document, and `just prepare`.

## What is deliberately not built

- **A customer-facing view of a thread.** The public booking page showing a
  conversation is a public read surface with its own authorization story; the
  deposit link's shape would be the model, and nobody has asked.
- **Attachments.** `files` attaches documents to a reservation already, and a
  thread that also holds files is two answers to "where is that photo".
- **Assignment, unread state, or who is handling a thread.** This is not a
  helpdesk. When a business asks for a queue, that is a queue.
- **Phone-number normalisation**, for the reason §2 gives.
- **Binding a number to a customer from the tray.** A `crm` field edit does it,
  and doing it from here would be a second way to change a customer record.

## Deploy note

One tenant migration (`message_sent`). No projection changes shape, so no
`migrate-fleet refresh` anywhere.
