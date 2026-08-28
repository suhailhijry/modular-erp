# What comes next

The full plan lives in `docs/IMPLEMENTATION.md`, phase by phase with the
reasoning for the order. This is the shape of it.

## Booking

The booking engine is designed to be generic, so it can handle any kind of
bookable resource, whether that's a person, a room, a table or a piece of
equipment. The plan is to keep it as general as possible while it stays durable
and fast.

Every resource carries a capacity, and a booking claims some of that capacity
for an interval. A salon chair has a capacity of one, a class might have twenty,
and a hotel might hold twelve rooms of the same type where the guest books the
type and only gets a room number at check-in.

We'll prove it's general by configuring four different trades entirely from
data. If any of them needs a code change, that's where the design is still too
narrow, and we fix it before moving on.

## People

The HR module covers employees, their positions and contracts, their attendance
and their leave, plus the documents that go with all of it.

Expiry is the part that needs real care. An expired residence permit stops
somebody working, so the module warns well before the date and refuses to roster
anyone whose document has already lapsed.

Payroll posts its results straight to the ledger, and a Saudi country module
handles GOSI, the monthly WPS salary file, and the end-of-service payment that
the law defines.

## Reporting

A dashboard looks like it needs to read several read models at once, which the
third law forbids. Instead the reporting module subscribes to the log directly
and keeps its own tables, so it has one position marker and one set of numbers
that agree with each other.

Every figure has to reconcile against the trial balance, and a difference fails
the build. A coloured cell on a dashboard is something nobody investigates.

## Reaching people

Right now the system can only send email. SMS, push notifications and messaging
apps each become another handler alongside it, since the machinery that retries,
leases and eventually gives up is already built and tested.

Templates will fetch their own data instead of waiting for a caller to fill them
in. A template declares the audience it's for and the values it needs, so you
hand it a booking id and it finds the customer's name, the time and the branch
by itself. If a value can't be resolved, the template fails when you save it,
while you're still looking at the screen.

## Taking money

The system records payments today but it has never actually taken one, and those
are very different problems. Recording is just an assertion somebody makes.
Taking money means talking to a gateway that can time out halfway through, and
when that happens you ask the provider what actually occurred. You never charge
the card a second time.

Settlement is the other half. Money arrives days later in batches, net of fees,
and each batch has to reconcile against what was actually sold.

## Real time

A customer books from their phone and every screen showing that day should
update by itself.

The way this works is that the server sends a small signal saying how far the
read model has advanced, and the client then fetches through the normal API. The
timing matters: the signal goes out once the read model is ready, not when the
event was written. Send it too early and the screen looks, finds nothing new,
and stops asking.
