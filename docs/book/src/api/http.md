# The HTTP API

Eighty operations across fifty-seven paths. Everything below is generated from
the same router that serves the requests, so `docs/openapi.json` and this chapter
cannot describe a route the server does not have.

## Conventions

**Tenants are subdomains.** `bassat.erp.com` is one company and `najd.erp.com` is
another, which is why no path carries a tenant name. Locally that is
`acme.localhost`, which every browser and curl resolve to the loopback with no
DNS and no `/etc/hosts`.

```bash
curl -H 'Host: demo.localhost' http://localhost:8080/v1/tenant
```

**Authentication** is a bearer token from `POST /v1/sessions`. Sessions last 12
hours.

**Every create needs an `Idempotency-Key`,** holding a UUID you generate per
request. It is not a nicety: **it is the identity the record is stored under**,
which is why nothing here takes an `id` in the body.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: $(uuidgen)" \
  http://localhost:8080/v1/sales/invoices -d '{ … }'
```

Send the same key again with the same request and you get the same answer,
having written nothing — which is what a client whose request timed out needs.
Send it with a *different* request and you get **409**, because two different
documents were given one name and losing the second silently is the failure this
replaces.

The identity a human reads is separate and ours: an invoice's `number` comes
from a gapless statutory series. A key you chose by hand — `INV-0001` — would
collide with the one another till chose, which is why only a UUID is accepted.

Anything that is not a create keeps its own idempotency: a payment, a redemption
or an earning is keyed by the `reference` in its body.

**`X-Branch`** names the place a write happened, and it travels on the request
rather than in any body. One header, read once, folded into the event metadata
of everything the request writes — so an invoice, its journal entry, its payment
and the till event it came from all carry the same branch without a single
module holding a `branch` field.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: $(uuidgen)" -H 'X-Branch: BR-OLAYA' \
  http://localhost:8080/v1/sales/invoices -d '{ … }'
```

It is checked once, where every posting funnels through, and a closed branch
refuses new documents. On a read it is a **default and not a wall**: the rota
narrows to your branch, and `?branch=` sees another one.

**Every error is `application/problem+json`** with a stable `code`. Branch on the
code, never on `detail`.

```json
{
  "type":   "https://errors.example.com/ledger.does_not_balance",
  "title":  "Unprocessable Entity",
  "status": 422,
  "code":   "ledger.does_not_balance",
  "detail": "Debits and credits differ by 500.",
  "args":   { "difference": { "kind": "text", "value": "500" } }
}
```

`docs/ERRORS.md` lists every code the build can produce.

**`Accept-Language`** is honoured on every response, including failures. Quality
values work, and `ar-SA` resolves to Arabic.

**Money is minor units plus a currency.** Never a decimal string and never a
float.

```json
{ "minor": 11500, "currency": "SAR" }
```

**Dates are RFC 3339 in UTC.** The tax point, the accounting date and the date a
row was written are three different things and the API says which it wants.

**Lists are keyset-paged.**

```json
{ "items": [ … ], "next": "MjAyNi0wMy0wMVQxMDowMDowMFo=" }
```

`next` absent means the list ended. Pass it back as `?after=`. `?limit=` is
clamped to what the server will serve, not refused.

**Reading your own write.** Every write returns the log `position` it landed at.
Pass it to a read as `?consistent_after=<position>` and the read waits for the
projection to reach it.

```bash
POS=$(curl -s … -d @invoice.json | jq -r .position)
curl -s "…/v1/sales/invoices?consistent_after=$POS"
```

Without it a read taken immediately after a write can legitimately not see it.
In practice the wait is short, because every write nudges the worker.

## Getting in

### POST /v1/signups

Unauthenticated, and it **creates nothing**. It records the request and sends a
confirmation to the address; the account, the tenant and the database are what
the confirmation builds. An unauthenticated endpoint that built a database was
one HTTP request away from a disk.

```bash
curl -sX POST http://localhost:8080/v1/signups \
  -H 'Content-Type: application/json' \
  -d '{
    "email":    "owner@acme.test",
    "password": "a-long-passphrase",
    "slug":     "acme",
    "company":  "Acme Trading",
    "modules":  ["ledger", "sales", "purchases", "tax_sa"]
  }'
# 202 {"email":"owner@acme.test","slug":"acme","expires_at":"…"}
```

`modules` is optional and defaults to none. Dependencies are checked here:
`sales` without `ledger` is refused with `request.module_requires`, and `tax_sa`
without either of sales or purchases with `request.module_requires_one_of`. The
slug is checked here too, so a name that is gone is a 409 at the form.

An address that **already has an account** must give that account's password.
Without that, naming a stranger's address would be a way to post mail through us.

The response carries no token, deliberately: a caller that had one could confirm
its own signup, which is the whole thing this endpoint refuses to allow.

One address gets one confirmation a minute. A second request inside that window
is `429 signups.too_soon`, and the message says how long to wait.

### POST /v1/signups/{token}

The token comes from the link in the email. This is where everything is built,
and the response is a working bearer token: confirming logs you in.

```bash
curl -sX POST http://localhost:8080/v1/signups/$TOKEN
# 201 {"tenant":"…","slug":"acme","token":"…","expires_at":"…","modules":[…]}
```

The link works once. A second use is `404 signups.not_valid`, which is also the
answer for a token that never existed and for one that expired, since links last
a day.

If the name was taken while the link sat in a mailbox, this is `409
provisioning.slug_taken` and **the link still works**. Ask for another name.

There is deliberately no `GET` beside this. `/v1/join/{token}` has one because
whoever opens an invitation did not write it and has to be told what they are
joining; whoever opens this one filled the form in themselves.

### POST /v1/sessions

```bash
curl -sX POST http://localhost:8080/v1/sessions \
  -H 'Host: acme.localhost' -H 'Content-Type: application/json' \
  -d '{"handle":"owner@acme.test","password":"a-long-passphrase"}'
```

Every failure is `auth.invalid_credentials` and every failure costs the same
time.

```bash
TOKEN=…
AUTH=(-H "Authorization: Bearer $TOKEN" -H 'Host: acme.localhost')
```

### DELETE /v1/sessions/current

Ends the session this request authenticated with.

### GET /v1/tenant

What this tenant is, and what you may do in it.

```bash
curl -s "${AUTH[@]}" http://localhost:8080/v1/tenant
```

## Service

| | |
|---|---|
| `GET /v1/health` | Liveness. Unauthenticated |
| `GET /v1/openapi.json` | This API, as OpenAPI 3.1. Unauthenticated |
| `GET /v1/catalogue` | Every module this build offers. Unauthenticated, because a pricing page needs it before anyone has an account |

## Modules

| | | Capability |
|---|---|---|
| `GET /v1/modules` | What this tenant has, and what else it could have | Read |
| `POST /v1/modules` | Turn one on | ManageTenant |
| `DELETE /v1/modules/{module}` | Turn one off | ManageTenant |

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/modules -d '{"module":"tax_sa"}'
```

Enabling is idempotent. Disabling never drops the module's tables, and is refused
with `request.module_in_use` while another module stands on it.

## People

| | | Capability |
|---|---|---|
| `GET /v1/members` | Everyone with access | **Read**, not ManageTenant |
| `POST /v1/members` | Add somebody, choosing their password | ManageTenant |
| `PATCH /v1/members/{identity}` | Change their tenant-wide role | ManageTenant |
| `DELETE /v1/members/{identity}` | Take access away | ManageTenant |
| `PUT /v1/members/{identity}/modules/{module}` | A different role in one module | ManageTenant |
| `DELETE /v1/members/{identity}/modules/{module}` | Back to their tenant-wide role | ManageTenant |

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/members \
  -d '{"email":"sara@acme.test","password":"another-passphrase","role":"accountant"}'
```

Roles are `owner`, `accountant`, `clerk`, `viewer`. The last owner cannot be
demoted or removed.

`DELETE …/modules/{module}` removes the override, which is different from setting
them to `viewer` there.

## Invitations

| | | |
|---|---|---|
| `GET /v1/invitations` | Outstanding ones. Never carries a token | ManageTenant |
| `POST /v1/invitations` | Invite somebody, leaving the password to them | ManageTenant |
| `DELETE /v1/invitations/{invitation}` | Withdraw one | ManageTenant |
| `GET /v1/join/{token}` | What the link is for, before accepting | Unauthenticated |
| `POST /v1/join/{token}` | Take it up | Unauthenticated |

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/invitations \
  -d '{"handle":"noura@acme.test","role":"clerk"}'
```

The link comes back **once**, in the response, and an email is promised in the
same transaction. Re-inviting an address revokes the previous link. They live 14
days.

`/v1/join` is on the apex and not under a tenant, because the person accepting
does not know which tenant they are joining until they look.

Accepting binds to the invited handle. If that address already has an account the
password must match; if not, the password becomes that account's.

## Customers

| | | Capability |
|---|---|---|
| `GET /v1/crm/customers` | Most recently registered first | Read |
| `POST /v1/crm/customers` | Record one | ManageTenant |
| `GET /v1/crm/customers/{customer}` | One, with everything on the record | Read |
| `PATCH /v1/crm/customers/{customer}` | Change what is known | ManageTenant |
| `POST /v1/crm/customers/{customer}/archive` | Out of the lists, documents intact | ManageTenant |
| `DELETE /v1/crm/customers/{customer}/archive` | Back | ManageTenant |

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/crm/customers -d '{
    "name":       "نجد للاستشارات",
    "name_latin": "Najd Consulting",
    "kind":       "company",
    "phone":      "+966500000000",
    "vat_number": {"vat_number":"399999999900003","scheme":"CRN","identifier":"1010101010"}
  }'
```

`kind` is `person` or `company`, and **only a company may carry a VAT number** —
a person does not hold a registration, and allowing both would make the
standard-against-simplified decision ambiguous at the moment it is taken.

A customer needs a phone number or an email address. Refused with
`crm.no_contact` otherwise, because a customer nobody can reach is a row rather
than a customer.

Archiving is not deletion:

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/crm/customers/CUST-0001/archive \
  -d '{"reason":"Closed the account"}'
```

They come out of the lists a clerk works from. Every invoice they are on stays
exactly as it was.

Passing `?archived=true` to the list includes them, which is what a search box
wants and a working list does not.

## Branches

| | | Capability |
|---|---|---|
| `GET /v1/branches` | Alphabetically, which is the one list in this API that is | Read |
| `POST /v1/branches` | Open a place to trade from | ManageTenant |
| `GET /v1/branches/{branch}` | One of them | Read |
| `PUT /v1/branches/{branch}` | Move it, or rename it | ManageTenant |
| `POST /v1/branches/{branch}/closure` | Stop it trading. It keeps everything it issued | ManageTenant |
| `DELETE /v1/branches/{branch}/closure` | Trade again | ManageTenant |

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: $(uuidgen)" \
  http://localhost:8080/v1/branches -d '{
    "name":       "فرع العليا",
    "name_latin": "Olaya Branch",
    "address": {
      "street":  "طريق الملك فهد",
      "city":    "الرياض",
      "country": "SA"
    }
  }'
```

The country is two letters and it is checked, because ZATCA prints it on every
document this branch issues and a two-letter field is the one a caller gets
wrong.

**Amending is a `PUT` and it is still an event.** A branch that moved is not a
row to overwrite: a trial balance for Olaya run in March and again in June would
differ with nothing able to say why. The old details stay in the log, so the
March report can still say what it said.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/branches/BR-OLAYA/closure \
  -d '{"why":"انتقل الفرع"}'
```

A closed branch **refuses new documents and keeps its old ones** — the same
distinction an archived customer draws, and for the same reason. It is checked
where every posting funnels through, so every module gets the rule without
holding it.

Closing is not deletion, and there is no route that deletes: a dimension that
vanished would take the meaning of its own history with it.

## Booking

### The rota

| | | Capability |
|---|---|---|
| `GET /v1/booking/trades` | Ready-made rotas. **Unauthenticated** | — |
| `POST /v1/booking/fit-out` | Declare a whole trade's rota at once | ManageTenant |
| `GET /v1/booking/resources` | Everything bookable, people first | Read |
| `POST /v1/booking/resources` | Record one | ManageTenant |
| `GET /v1/booking/resources/{resource}` | One, with its timetable | Read |
| `PATCH /v1/booking/resources/{resource}` | Rename, or change how much of it there is | ManageTenant |
| `PUT /v1/booking/resources/{resource}/availability` | Set the whole timetable | ManageTenant |
| `POST /v1/booking/resources/{resource}/withdrawal` | Out of service | ManageTenant |
| `DELETE /v1/booking/resources/{resource}/withdrawal` | Back, at the capacity it had | ManageTenant |

`/v1/booking/trades` needs no session, for the same reason `/v1/ledger/charts`
does not: a signup form has to show a salon what a salon gets before anybody has
an account.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/booking/fit-out -d '{"trade":"salon"}'
# {"declared":5,"skipped":0,"scheduled":5}
```

Six trades ship: `salon`, `restaurant`, `hotel`, `studio`, `gym`, `museum`.
Running it twice is harmless — anything already there is counted as `skipped`
and keeps whatever it has been renamed to.

Or declare one by hand:

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/booking/resources -d '{
    "id":         "stylist-noura",
    "name":       "نورة",
    "name_latin": "Noura",
    "kind":       "person",
    "capacity":   1
  }'
```

`kind` is `person`, `place` or `thing`, and it is **display only** — no rule in
the module branches on it, which is what keeps a stylist and a hotel room the
same code.

`capacity` is where most of the difference between trades lives: one stylist,
six covers at a table, three rooms of a type, twelve places in a class, five
hundred tickets in a slot. Names beginning `customer.` are refused; that prefix
is kept for customers' own diaries.

Opening hours:

```bash
curl -sX PUT "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/booking/resources/stylist-noura/availability -d '{
    "hours": [
      {"weekdays":[7,1,2,3,4], "opens_at":540, "closes_at":1260}
    ]
  }'
```

Times are **minutes past local midnight** — `540` is 09:00, `1260` is 21:00 —
and the local part is the tenant's `booking.calendar` offset, `+03:00` by
default. Weekdays are ISO, Monday as 1, so Sunday to Thursday is `[7,1,2,3,4]`.

`months`, `weekdays` and `days` are all optional and **an empty list means
every**. An empty `hours` means always open, which is what a hotel room wants.

A window may not run past midnight: 22:00 to 02:00 is two windows.

Withdrawing keeps the bookings already against it — a chair that broke on
Tuesday was still booked on Monday:

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/booking/resources/chair-1/withdrawal -d '{"why":"انكسر"}'
```

### The tariff

| | | Capability |
|---|---|---|
| `GET /v1/booking/tariff` | Which hours cost more, and by how much | Read |
| `PUT /v1/booking/tariff` | Set the whole thing | ManageTenant |

```bash
curl -sX PUT "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/booking/tariff -d '{
    "bands": [
      {"name":"ذروة المساء", "uplift":2500,
       "hours":{"weekdays":[3,4],"opens_at":1020,"closes_at":1260}}
    ]
  }'
```

`uplift` is basis points: `2500` is a quarter more, `-1000` is a tenth off,
which is what an off-peak band is. **First match wins**, so the order is your
priority — a public holiday goes above a general evening band.

**Bands, not prices.** What a service costs is yours to send on the line; when
it costs more is the tenant's to configure, and that is the half a client must
not be able to decide for itself.

Bookings already taken keep the price they were given. The band is frozen onto
the line when the booking is written, so moving your peak hours changes what the
next booking costs and nothing that was already agreed.

### The diary

| | | Capability |
|---|---|---|
| `GET /v1/booking/reservations` | Bookings overlapping a window, earliest first | Read |
| `POST /v1/booking/reservations` | Take one | PostEntries |
| `GET /v1/booking/reservations/{reservation}` | One, with its lines | Read |
| `POST /v1/booking/reservations/{reservation}/stage` | Move it along | PostEntries |
| `PUT /v1/booking/reservations/{reservation}/lines` | Move it in time, or onto other resources | PostEntries |
| `PUT /v1/booking/reservations/{reservation}/lines/{line}/unit` | Pick a unit out of a pool | PostEntries |

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/booking/reservations -d '{
    "customer":      "CUST-0001",
    "customer_name": "سارة",
    "lines": [{
      "what":  "قص وتصفيف",
      "from":  "2026-09-02T07:00:00Z",
      "until": "2026-09-02T08:00:00Z",
      "takes": [{"resource":"stylist-noura"}, {"resource":"chair-1"}],
      "charge": {"rate": 8000, "currency": "SAR", "quantity": 1}
    }]
  }'
```

`id` is yours, and **taking the same one twice is a no-op that takes no capacity
the second time**. That gate matters more here than anywhere else in this API: a
client whose request timed out and retried would otherwise book the chair twice.

`takes` is everything the line needs at once — the stylist *and* the chair. Each
entry defaults to `quantity: 1`; a party of four at a table is
`{"resource":"table-6","quantity":4}`.

`until` is exclusive, so a line ending at 11:00 and one starting at 11:00 are
back to back and do not clash.

`customer` is a `crm` id and is optional — a walk-in has none. When it is there,
**the customer is held as a resource at capacity one**, so booking the same
person into two chairs at the same hour is refused by the same machinery that
refuses two people in one chair. `customer_name` is what the diary prints and is
frozen, so somebody changing their name next year does not rewrite last year's
calendar.

`charge` is optional. The `rate` is yours; the band is the tenant's, resolved
when the booking is written. What comes back on a read is the whole working:

```json
{"rate":8000,"currency":"SAR","quantity":1,"band":"ذروة المساء","uplift":2500,
 "allowances":[],"gross":10000,"net":10000}
```

Refusals worth knowing:

| Code | Status | What happened |
|---|---|---|
| `occupancy.overbooked` | 422 | Nothing free then. Carries how much is held, of what capacity |
| `booking.not_offered` | 422 | The resource is not open at that hour |
| `booking.withdrawn` | 422 | It is out of service |
| `booking.no_such_customer` | 404 | No `crm` record answers to that id |
| `booking.allowance_too_large` | 400 | A discount bigger than the line it comes off |

Move it along the lifecycle:

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/booking/reservations/BK-0001/stage \
  -d '{"stage":"arrived"}'
```

`reserved → confirmed → arrived → in_service → completed`, with `cancelled` and
`no_show` as ends. **Skipping forwards is allowed** — a walk-in arrives without
ever being confirmed. Backwards is not, and neither is `no_show` after they have
arrived. Moving to the stage it is already in is a no-op, so a retried *mark them
arrived* is harmless.

Cancelling and no-show give the capacity back. **Completing does not** — a
finished appointment held that chair, and freeing it would make the past look
free.

Reschedule, which moves it in time and onto different resources at once:

```bash
curl -sX PUT "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/booking/reservations/BK-0001/lines -d '{
    "lines": [{
      "what":  "قص وتصفيف",
      "from":  "2026-09-02T07:30:00Z",
      "until": "2026-09-02T08:30:00Z",
      "takes": [{"resource":"stylist-noura"}, {"resource":"chair-1"}]
    }]
  }'
```

The whole set is replaced. A booking never conflicts with where it already was,
so nudging it half an hour later works. **Assignments are dropped**: a line that
moved has to be given a unit again, because the unit that was free at the old
hour is a different question from the new one.

Book the type, give out the room later:

```bash
curl -sX PUT "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/booking/reservations/BK-0006/lines/0/unit \
  -d '{"unit":"room-201"}'
```

The pool holds the count and the unit holds the identity, so this takes a second
claim on a different resource and nothing is counted twice. Assigning a
different unit replaces the first and gives it back.

Read the day:

```bash
curl -s "${AUTH[@]}" 'http://localhost:8080/v1/booking/reservations\
?from=2026-09-02T00:00:00Z&until=2026-09-03T00:00:00Z&stage=reserved&limit=50'
```

The window is half-open and matches the way a claim overlaps, so a booking that
straddles midnight appears on both days rather than on whichever one it happens
to start in. Both ends are optional, so the same read serves *everything from
now on* and *this week*.

## Ledger

| | | Capability |
|---|---|---|
| `GET /v1/ledger/accounts` | Every account and what it holds | Read |
| `POST /v1/ledger/accounts` | Open one | ManageAccounts |
| `GET /v1/ledger/charts` | Ready-made charts, in the caller's language | Unauthenticated |
| `POST /v1/ledger/chart` | Open every account in one | ManageAccounts |
| `POST /v1/ledger/entries` | Post a journal entry | PostEntries |
| `POST /v1/ledger/entries/{entry}/reversal` | Post its opposite | PostEntries |
| `GET /v1/ledger/trial-balance` | Debits and credits per currency | Read |
| `GET /v1/ledger/books` | How far the books are closed | Read |
| `PUT /v1/ledger/books` | Close them, or reopen them | ManageAccounts |
| `GET /v1/ledger/vat-rates` | What this business charges | Read |
| `PUT /v1/ledger/vat-rates` | Set it | ManageAccounts |

Install a chart first. `services` and `retail` both ship VAT and Zakat accounts.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/ledger/chart \
  -d '{"template":"services","currency":"SAR"}'
```

Open one by hand:

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/ledger/accounts \
  -d '{"code":"1010","name":"Cash at bank","kind":"asset","currency":"SAR"}'
```

`kind` is `asset`, `liability`, `equity`, `revenue` or `expense`.

Post an entry. Amounts are signed: positive debits, negative credits, and they
must sum to zero.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/ledger/entries -d '{
    "occurred_on": "2026-03-01T00:00:00Z",
    "memo":        "Opening balance",
    "lines": [
      {"account":"1010","amount":{"minor": 100000,"currency":"SAR"}},
      {"account":"3000","amount":{"minor":-100000,"currency":"SAR"}}
    ]
  }'
```

`id` is yours and posting it twice is a no-op, which is what makes a retry safe.
Fewer than two lines, mixed currencies, a zero line, or lines that do not sum to
zero are all refused with a `ledger.*` code.

Reverse one:

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/ledger/entries/JE-2026-0001/reversal \
  -d '{"occurred_on":"2026-04-01T00:00:00Z","memo":"Correction"}'
```

Close the books. `closed_before` is the first instant that is **still open**, so
closing January is `2026-02-01T00:00:00Z`. `null` reopens.

```bash
curl -sX PUT "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/ledger/books -d '{"closed_before":"2026-02-01T00:00:00Z"}'
```

Any entry dated before that is refused with `ledger.period_closed`, including
invoices, because an invoice and its journal entry commit together.

## Sales

| | | Capability |
|---|---|---|
| `GET /v1/sales/invoices` | Most recently issued first | Read |
| `POST /v1/sales/invoices` | Issue one, and post it | PostEntries |
| `GET /v1/sales/invoices/{invoice}` | One, with lines, tax bands and payments | Read |
| `POST /v1/sales/invoices/{invoice}/payments` | Record money received | PostEntries |
| `POST /v1/sales/invoices/{invoice}/refunds` | Hand money back | PostEntries |
| `POST /v1/sales/invoices/{invoice}/customer` | Match a record to an old invoice | ManageTenant |
| `GET /v1/sales/unmatched-customers` | Buyers with no record behind them | Read |
| `POST /v1/sales/invoices/{invoice}/credit-note` | Cancel by crediting | PostEntries |
| `GET /v1/sales/receivables` | Who owes what, and for how long | Read |
| `GET /v1/sales/posting-accounts` | What sales posts to | Read |
| `PUT /v1/sales/posting-accounts` | Choose | ManageAccounts |

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/sales/invoices -d '{
    "issued_on":  "2026-03-01T10:00:00Z",
    "due_on":     "2026-03-31T00:00:00Z",
    "currency":   "SAR",
    "customer": {
      "name":       "Najd Consulting",
      "vat_number": "310122393500003",
      "address": {
        "street":  "King Fahd Road",
        "city":    "Riyadh",
        "country": "SA"
      }
    },
    "lines": [
      {"description":"Advisory, March","net":100000,"vat":"standard","vat_rate":1500}
    ],
    "discounts": [
      {"amount":5000,"reason":"Introductory","vat":"standard"}
    ],
    "note": "Thank you"
  }'
```

`id` is yours. Re-issuing the same one is a no-op and **the number comes back
either way**, which is what a client whose request timed out needs.

`vat` is `standard`, `zero` or `exempt`. The rate on the wire is what the client
believes; the rate that is stamped on the invoice is the tenant's configured one,
resolved inside the command's transaction.

A `vat_number` on the customer makes it a **standard** invoice, which ZATCA
clears before it may be given to the buyer. No VAT number makes it
**simplified**, reported within 24 hours.

A discount is `cac:AllowanceCharge` on the document, not a negative line, so the
customer sees what they were let off. Its `vat` decides whether it reduces the
tax.

Record a payment:

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/sales/invoices/INV-0001/payments -d '{
    "reference":   "BANK-8817",
    "amount":      {"minor": 60000, "currency": "SAR"},
    "received_on": "2026-03-15T00:00:00Z",
    "account":     "1010"
  }'
```

The same `reference` twice is a no-op. Overpaying is refused with
`sales.overpayment`, which carries what is outstanding.

Hand it back:

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/sales/invoices/INV-0001/refunds -d '{
    "reference":   "BANK-8842",
    "amount":      {"minor": 60000, "currency": "SAR"},
    "refunded_on": "2026-03-18T00:00:00Z",
    "account":     "1010"
  }'
```

The mirror of a payment, and **what makes a paid invoice creditable**: the money
goes back first and the credit note follows. Refunding more than is held is
refused with `sales.overrefund`, for the same reason overpaying is.

Credit it:

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/sales/invoices/INV-0001/credit-note \
  -d '{"id":"CN-0001","on":"2026-03-20T00:00:00Z","reason":"Cancelled order"}'
```

Whole-invoice only, and refused with `sales.has_payments` while the business is
**still holding** the customer's money — paid less refunded, not *ever paid*.
What a credit note may not do is undo a supply while the cash stays in the till.

### Matching old invoices to customer records

Invoices issued before anybody kept a customer list name a buyer that no record
matches. Rather than a foreign key that would refuse all of them at once, there
is a worklist and a way to work through it.

```bash
curl -s "${AUTH[@]}" http://localhost:8080/v1/sales/unmatched-customers
# [{"name":"نجد للاستشارات","vat_number":null,"invoices":2,
#   "gross":150000,"currency":"SAR","last_issued":"2026-03-01T00:00:00Z"}]
```

One row per spelling, biggest backlog first — the job is matching people, and
forty invoices for one name is one decision. A name that appears with two
different VAT numbers is two rows, because it is two buyers.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/sales/invoices/INV-0001/customer \
  -d '{"customer":"CUST-0001"}'
```

**This sets the reference and never the printed name.** What the invoice says
about its buyer was frozen when it was issued — that is what the law requires
the document to say, and a reconciliation does not get to restate a document
somebody has already filed a return against.

Sending the same record twice is a no-op. Sending a different one is a
correction and is recorded as one, because a match made to the wrong customer
has to be fixable.

Receivables:

```bash
curl -s "${AUTH[@]}" \
  'http://localhost:8080/v1/sales/receivables?as_of=2026-03-31T00:00:00Z&limit=50'
```

Grouped by customer **and currency**, aged from the due date and falling back to
the issue date. `as_of` is a parameter, so an accountant closing March gets the
ageing as it stood on 31 March.

## The counter

| | | Capability |
|---|---|---|
| `GET /v1/pos/shifts` | Today's tills, newest first | Read |
| `POST /v1/pos/shifts` | Open one, with what is in the drawer | PostEntries |
| `GET /v1/pos/shifts/{shift}` | One, with what the drawer should hold | Read |
| `GET /v1/pos/shifts/{shift}/takings` | Split by how the money arrived | Read |
| `POST /v1/pos/shifts/{shift}/sales` | Ring a sale | PostEntries |
| `POST /v1/pos/shifts/{shift}/sales/{sale}/returns` | Hand one back | PostEntries |
| `POST /v1/pos/shifts/{shift}/pay-outs` | Cash out that is not a refund | PostEntries |
| `POST /v1/pos/shifts/{shift}/count` | Count the drawer and shut the till | PostEntries |
| `GET /v1/pos/till-accounts` | Where the drawer posts | Read |
| `PUT /v1/pos/till-accounts` | Choose them | ManageAccounts |

**There is no `GET /v1/pos/sales`,** and the absence is the module's design. A
till sale *is* a ZATCA simplified invoice, so it is in `GET /v1/sales/invoices`
with every other one. A second list would be a second answer to "what did we
sell", and the VAT return would have to pick one.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: $(uuidgen)" -H 'X-Branch: BR-OLAYA' \
  http://localhost:8080/v1/pos/shifts -d '{
    "till": "١", "operator": "staff-1", "float": {"minor": 50000, "currency": "SAR"}
  }'
```

Opening posts nothing. A float is cash moved from a safe to a drawer and both
are `1000 Cash on hand`, so the business is no richer for having moved it.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: $(uuidgen)" \
  http://localhost:8080/v1/pos/shifts/SHIFT-1/sales -d '{
    "customer": {"name": "زبون"},
    "lines":    [{"description": "قهوة", "net": 1500, "vat": "standard"}],
    "currency": "SAR",
    "tenders":  [{"method": "cash", "amount": {"minor": 1725, "currency": "SAR"}}]
  }'
# 201 {"sale":"…","number":"INV-00001","total":{"minor":1725,"currency":"SAR"},"position":…}
```

It answers with the **statutory number and the total**, because that is what a
receipt prints. The invoice itself is `sales`' and is read at
`GET /v1/sales/invoices/{sale}`.

The number comes from **the same gapless series as every other invoice**, which
is the point: one series means one sequence to prove to ZATCA, and a till that
numbered itself would be a second one nobody had to keep gapless.

The tenders must come to **exactly** the sale. Less is an invoice on credit and
not a till sale; more is an overpayment, which `sales` refuses and is right to —
change handed back is a counter concern and not a record.

`method` is `cash`, `card` or `transfer`, and **only `cash` moves the drawer**.
Counting card takings into the expected count makes every honest till look short
by exactly the day's card sales, and a manager who sees that twice stops reading
the number.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/pos/shifts/SHIFT-1/sales/SALE-1/returns -d '{
    "reference": "RET-1",
    "tenders":   [{"method": "cash", "amount": {"minor": 1725, "currency": "SAR"}}],
    "why":       "أعاد المنتج"
  }'
```

The money, the credit note and the drawer in one write. `reference` is your key
for *this* return, so sending it again is a no-op; the sale in the path is the
document being credited, which is why they are two fields and not one.

A return credits the **whole** sale: a partial credit note is not something
`sales` can write.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/pos/shifts/SHIFT-1/count \
  -d '{"declared": {"minor": 61500, "currency": "SAR"}}'
```

`variance` is `declared - expected`, and **negative is short**. It posts: a till
that records a shortage without booking it leaves the ledger saying the drawer
holds what it does not, for ever, and the next reconciliation inherits the lie.

## Purchases

| | | Capability |
|---|---|---|
| `GET /v1/purchases/bills` | Most recently billed first | Read |
| `POST /v1/purchases/bills` | Record one, and post it | PostEntries |
| `GET /v1/purchases/bills/{bill}` | One, with lines and payments | Read |
| `POST /v1/purchases/bills/{bill}/payments` | Pay a supplier | PostEntries |

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/purchases/bills -d '{
    "reference": "SUP-INV-4471",
    "billed_on": "2026-03-05T00:00:00Z",
    "currency":  "SAR",
    "supplier":  {"name":"Riyadh Stationery","vat_number":"310987654300003"},
    "lines": [
      {"description":"Paper","account":"6100","net":20000,"tax":3000,
       "vat":"standard","vat_rate":1500}
    ],
    "note": ""
  }'
```

**`tax` is what the supplier charged**, not something the server computes. Input
tax is reclaimed against their document, so the books have to hold the figure on
it. What is checked is that the figure is possible: never negative, zero on
anything not standard-rated, and never claimed without the supplier's VAT number.

`id` is ours; `reference` is theirs, and a duplicate of it against the same
supplier is refused, because recording a bill twice is a duplicate reclaim.

`account` is per line, because one bill covers rent and stationery.

## Saudi tax

| | | Capability |
|---|---|---|
| `GET /v1/tax_sa/vat-return` | Charged, paid, and the difference | Read |
| `GET /v1/tax_sa/returns` | Everything filed | Read |
| `POST /v1/tax_sa/returns` | Record that a period was filed | PostEntries |
| `GET /v1/tax_sa/registration` | What is registered with ZATCA | Read |
| `PUT /v1/tax_sa/registration` | Register, or correct it | ManageTenant |
| `GET /v1/tax_sa/zatca` | Where the business stands, in one answer | Read |
| `GET /v1/tax_sa/zatca/documents` | Every document, most recent first | Read |
| `GET /v1/tax_sa/zatca/documents/{number}` | One, with its UBL and its stamp | Read |
| `GET /v1/tax_sa/zatca/onboarding` | How far onboarding has got | Read |
| `POST /v1/tax_sa/zatca/onboarding` | Generate the key pair and the CSR | ManageTenant |
| `PUT /v1/tax_sa/zatca/onboarding/certificate` | Record a certificate ZATCA issued | ManageTenant |
| `POST /v1/tax_sa/zatca/onboarding/activate` | All the way live, from an OTP | ManageTenant |

Register the business first. Every document rendered after this carries it, and
nothing already rendered changes.

```bash
curl -sX PUT "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/tax_sa/registration -d '{
    "name":       "شركة أكمي للتجارة",
    "name_latin": "Acme Trading",
    "vat_number": "310122393500003",
    "scheme":     "CRN",
    "identifier": "1010101010",
    "address": {
      "street":      "طريق الملك فهد",
      "building":    "1234",
      "district":    "العليا",
      "city":        "الرياض",
      "postal_code": "12211",
      "country":     "SA"
    }
  }'
```

`name` must be Arabic, because that is the name that goes on the invoice. The
Latin one is for our own screens and never reaches ZATCA. `scheme` is ZATCA's
`schemeID`, usually `CRN` for the commercial registration.

Everything ZATCA validates is checked here, because by the time ZATCA says no the
invoice exists.

The VAT return for a period, half-open `[from, until)`:

```bash
curl -s "${AUTH[@]}" \
  'http://localhost:8080/v1/tax_sa/vat-return?currency=SAR&from=2026-01-01T00:00:00Z&until=2026-04-01T00:00:00Z'
```

Record that you filed it:

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/tax_sa/returns -d '{
    "currency": "SAR",
    "from":     "2026-01-01T00:00:00Z",
    "until":    "2026-04-01T00:00:00Z",
    "filed_on": "2026-04-20T00:00:00Z"
  }'
```

Filing the same period twice is a **conflict**, not a no-op, because a second
filing is an amendment.

### Onboarding with ZATCA

The one-call path, when the taxpayer has an OTP from the Fatoora portal in front
of them. It runs all four steps.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/tax_sa/zatca/onboarding/activate -d '{
    "environment":       "sandbox",
    "otp":               "123456",
    "common_name":       "Acme Trading EGS",
    "serial":            "1-ERP|2-1.0|3-0001",
    "branch":            "Riyadh",
    "industry":          "Consulting",
    "issues_standard":   true,
    "issues_simplified": true
  }'
```

`environment` is `sandbox`, `simulation` or `production` and is required
everywhere it appears, because the only visible difference is one certificate
template string and a base URL, and getting it wrong does not fail. It succeeds
against the wrong authority.

The manual path, for when the automated one breaks. `POST` returns the CSR;
submit it yourself and `PUT` what comes back.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/tax_sa/zatca/onboarding -d '{
    "environment": "sandbox", "common_name": "Acme Trading EGS",
    "serial": "1-ERP|2-1.0|3-0001", "branch": "Riyadh", "industry": "Consulting",
    "issues_standard": true, "issues_simplified": true
  }'

curl -sX PUT "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/tax_sa/zatca/onboarding/certificate -d '{
    "environment": "sandbox", "stage": "compliance",
    "token": "<binarySecurityToken>", "secret": "<secret>", "request_id": "<id>"
  }'
```

The certificate is checked against the key this tenant holds before anything is
stored. A certificate over somebody else's key fails at clearance, on an invoice
a customer is waiting for, with an error that says nothing about why.

Where things stand:

```bash
curl -s "${AUTH[@]}" http://localhost:8080/v1/tax_sa/zatca
# {"registered":true,"unsigned":0,"overdue":0,"awaiting_clearance":2,"chain_length":7,…}
```

A document's status is `unregistered`, `pending`, `cleared`, `reported` or
`refused`.

## Prepaid

What the customer has already paid for. Three shapes, one liability account, and
**no tax anywhere** — the sale was invoiced by `sales`, and this records that the
revenue is not earned yet.

| | | Capability |
|---|---|---|
| `GET /v1/prepaid/entitlements` | Packages, courses and deposits | Read |
| `POST /v1/prepaid/entitlements` | Grant one, and defer its value | PostEntries |
| `GET /v1/prepaid/entitlements/{entitlement}` | One, with what is left | Read |
| `POST /v1/prepaid/entitlements/{entitlement}/redemptions` | Draw it down | PostEntries |
| `POST /v1/prepaid/entitlements/{entitlement}/expiry` | Write off what lapsed | PostEntries |
| `POST /v1/prepaid/entitlements/{entitlement}/revocation` | Take it back | PostEntries |
| `GET /v1/prepaid/subscriptions` | Terms paid in advance | Read |
| `POST /v1/prepaid/subscriptions` | Start one | PostEntries |
| `GET /v1/prepaid/subscriptions/{subscription}` | One, with what is earned | Read |
| `POST /v1/prepaid/subscriptions/{subscription}/recognition` | Earn time up to a moment | PostEntries |
| `POST /v1/prepaid/subscriptions/{subscription}/freeze` | Stop the clock | PostEntries |
| `DELETE /v1/prepaid/subscriptions/{subscription}/freeze` | Start it again | PostEntries |
| `POST /v1/prepaid/subscriptions/{subscription}/renewal` | End the term and begin another | PostEntries |
| `POST /v1/prepaid/subscriptions/{subscription}/cancellation` | End it | PostEntries |
| `GET /v1/prepaid/cards` | Loyalty cards | Read |
| `POST /v1/prepaid/cards` | Open one | PostEntries |
| `GET /v1/prepaid/cards/{card}` | One, with counts and rank | Read |
| `POST /v1/prepaid/cards/{card}/earnings` | Award counts against a sale | PostEntries |
| `POST /v1/prepaid/cards/{card}/redemptions` | Spend them | PostEntries |
| `POST /v1/prepaid/cards/{card}/expiry` | Lapse them as breakage | PostEntries |
| `GET /v1/prepaid/outstanding` | What every customer is owed, per currency | Read |
| `GET /v1/prepaid/posting-accounts` | What this posts to | Read |
| `PUT /v1/prepaid/posting-accounts` | Choose | ManageAccounts |
| `GET /v1/prepaid/loyalty-scheme` | What a count is worth, and what earns one | Read |
| `PUT /v1/prepaid/loyalty-scheme` | Choose | ManageAccounts |

Amounts are minor units and a currency: `{"minor": 30000, "currency": "SAR"}` is
three hundred riyals. A decimal string is a float somewhere in every client, and
this module's whole job is a number that has to be exactly right.

### A package

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/prepaid/entitlements -d '{
    "customer": "CUST-0001",
    "what":     "استشارة",
    "uses":     10,
    "value":    {"minor": 500000, "currency": "SAR"},
    "reason":   "bought",
    "at":       "2026-01-20T00:00:00Z"
  }'
```

`value` is **what is deferred, excluding tax**. The tax was settled on the
invoice `sales` issued; sending a gross figure here overstates the liability by
the VAT.

`reason` is `bought`, `gifted_by_customer`, `granted_by_business` or
`free_from_coupon`. The last two carry no value and are refused with
`prepaid.free_grant_with_value` if you send one — nobody paid, so there is
nothing to defer and nothing to recognise later.

A deposit is the same endpoint with an amount instead of a count, naming what it
is held against:

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/prepaid/entitlements -d '{
    "customer": "CUST-0001", "what": "عربون",
    "value": {"minor": 15000, "currency": "SAR"},
    "reason": "bought", "against": "BK-0001"
  }'
```

**An amount with neither `uses` nor `against` is refused** with
`prepaid.open_value`. That shape is a card spendable on anything — a
multi-purpose voucher — and what it buys is not known when it is sold, so neither
is the rate it should have been taxed at.

Draw it down:

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/prepaid/entitlements/PKG-0001/redemptions \
  -d '{"reference":"VISIT-0001","uses":1,"at":"2026-02-10T00:00:00Z"}'
```

The same `reference` twice is a no-op. Each use is worth what is left divided by
what is left to use, so the last session takes the remainder and the liability
closes at exactly zero — three sessions of 100 riyals is 33.33, 33.34, 33.33 and
not a stranded halala.

Expiry and revocation both clear the liability, and they are not the same event.
Expiry **recognises** it, because the obligation to deliver has gone and what was
held against it is earned. Revocation does not: nothing was delivered, and the
refund itself is a credit note in `sales`.

### A subscription

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/prepaid/subscriptions -d '{
    "customer": "CUST-0002",
    "plan":     "اشتراك سنوي",
    "price":    {"minor": 1200000, "currency": "SAR"},
    "from":     "2026-01-01T00:00:00Z",
    "until":    "2027-01-01T00:00:00Z"
  }'
```

`until` is exclusive, like every interval in this system.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/prepaid/subscriptions/SUB-0001/recognition \
  -d '{"at":"2026-06-30T00:00:00Z"}'
```

**Recognition is a cumulative total, not an instalment.** It earns everything up
to `at` and posts the difference from what was already earned, so running a
month-end job twice recognises nothing the second time, and the last day of the
term lands on exactly the price with no rounding drift.

Freeze and resume:

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/prepaid/subscriptions/SUB-0001/freeze \
  -d '{"why":"سفر","at":"2026-03-01T00:00:00Z"}'

curl -sX DELETE "${AUTH[@]}" \
  http://localhost:8080/v1/prepaid/subscriptions/SUB-0001/freeze
```

Freezing earns everything up to that moment and stops the clock; resuming pushes
`ends_at` out by exactly the time it was stopped for. Removing the freeze *is*
the operation, which is why it is a `DELETE`.

A renewal ends the current term and begins another. The old term is recognised
**in full** first — a term that ended was delivered, however few times the member
turned up — and the new term's price is deferred in the same transaction.

### A loyalty card

There is no default scheme and there could not be one: what a point is worth is a
business decision. Configure it before anything can be earned, or every earning
is refused with `prepaid.no_scheme`.

```bash
curl -sX PUT "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/prepaid/loyalty-scheme -d '{
    "worth":   {"minor": 10, "currency": "SAR"},
    "rate_bp": 10000,
    "tiers":   [{"name": "ذهبي", "from": 2000, "rate_bp": 15000}]
  }'
```

`worth` is what one count is worth when it is redeemed, and it is the standalone
selling price the allocation below is computed from. `rate_bp` is counts per
major unit spent in basis points: `10000` is one point per riyal, `15000` is one
and a half.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/prepaid/cards \
  -d '{"customer":"CUST-0001","mechanic":"points"}'

curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/prepaid/cards/CARD-0001/earnings -d '{
    "reference": "INV-0001",
    "spend":     {"minor": 10000, "currency": "SAR"},
    "from":      "INV-0001"
  }'
```

`mechanic` is `points`, `stamps` or `visits`. It decides what produces the count
and nothing after that — the accounting is identical for all three. Omit `count`
for points and it is computed from the scheme's rate at the card's rank; send it
for stamps and visits, which count their own.

`spend` is what was paid, excluding tax, and it is required because of what this
endpoint actually does. Under IFRS 15 the points are a **separate performance
obligation**, so part of that sale is allocated to them and deferred:

```text
allocated = spend × (count × worth) / (spend + count × worth)
```

A hundred riyals awarding a hundred points worth ten halalas each defers
**9.09, not 10.00**. The common shortcut accrues the reward's face value instead;
Saudi requires IFRS, so there is no setting here that selects it.

`from` is opaque and unchecked — a reconciliation surface, not a foreign key.

```bash
curl -sX POST "${AUTH[@]}" -H 'Content-Type: application/json' \
  http://localhost:8080/v1/prepaid/cards/CARD-0001/redemptions \
  -d '{"reference":"RWD-0001","count":40,"toward":"INV-0002"}'

curl -s "${AUTH[@]}" http://localhost:8080/v1/prepaid/cards/CARD-0001
# {"counts":60,"lifetime":100,"deferred":{"minor":549,"currency":"SAR"},…}
```

`counts` is what can be spent; `lifetime` is everything ever earned and **never
decreases**, because it is what a rank is read from and spending points does not
cost a rank. Neither does breakage — and a card survives its own expiry, unlike
an entitlement: the balance goes and the card can earn again tomorrow.

### The number that has to reconcile

```bash
curl -s "${AUTH[@]}" http://localhost:8080/v1/prepaid/outstanding
# {"outstanding":[{"minor":549,"currency":"SAR"}]}
```

Every unredeemed entitlement, every unearned subscription month and every
unhonoured count, per currency. **It must equal the deferred revenue account's
balance in the ledger.** If it does not, the pipeline is broken rather than the
arithmetic.

## Status codes

| Code | When |
|---|---|
| 400 | The body is not JSON, or a value did not parse |
| 401 | No token, or an expired one |
| 403 | A live membership without the capability. Names the capability |
| 404 | No such record, or the tenant did not enable this module |
| 409 | Lost an optimistic-concurrency race, or a state that moved. Retryable |
| 415 | A body without `Content-Type: application/json` |
| 422 | Valid JSON, refused on the state of things |
| 500 | A bug. The detail goes to the log and never to the caller |
| 429 | A confirmation went to this address moments ago. Retryable; the message says when |
| 503 | The connection budget is exhausted, or a read is not caught up. Retryable |
| 504 | The request took longer than 30 seconds |

A 409, a 429 and a 503 are worth retrying. A 422 is not: re-asking gets the same
answer.

## What is not here yet

`ETag`/`If-Match`. It needs a conflict real enough to shape it.

There is also **no per-caller rate limit** on any endpoint. Signup caps
confirmations per address, which is what stops it filling one mailbox, and that
is as far as an endpoint with no notion of caller can go. Phase 12c builds the
real limiter with API keys.
