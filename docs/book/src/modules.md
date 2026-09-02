# What the modules do

Tenants switch these on individually, and each one depends only on the modules
above it.

## branches

Places the business trades from, and the dimension every document is reported by.

It depends on nothing, which is what lets everything else name a place without
depending on each other. Opening, moving, renaming and closing are all events, so
a trial balance for one branch run in March still says in June what it said then
— a dimension edited in place rewrites its own history.

The branch travels on the request as a header, folded into event metadata by one
extractor and checked once where every posting funnels through. **No module holds
a `branch` field.**

## crm

Customers as records: who a document was for.

An invoice freezes the buyer's name, so two spellings were two rows and nothing
could answer *everything for this customer*. The fix is the reference and the
frozen copy, both — never either.

Archiving is not deletion. The customer is on documents that have been issued,
filed against and cleared with ZATCA.

## hr

The org chart, and the claims that travel up it.

**A manager automatically holds everything their reports hold**, so nobody has to
remember that a new permission for a junior also needs giving to whoever covers
for them. That one rule makes the org chart the authorization model — which is
why granting reports back *everyone* who gained it, and why a claim can be marked
non-propagating so segregation of duties survives.

These claims never leave the tenant. The platform still answers "may you reach
this endpoint"; `hr` answers "may you approve this particular thing", checked
inside the command that decides.

## ledger

Double-entry accounting: accounts, journal entries, fiscal periods and VAT rates.

A journal entry has to sum to zero, and the type carrying its lines revalidates
when it's read back, so a bad migration surfaces as a decode error instead of a
trial balance that quietly doesn't balance.

It ships with ready-made charts of accounts in Arabic and English. Installing a
chart in English and asking a Saudi accountant to rename every account would be
technically correct and completely useless.

## sales

Invoices, credit notes and the money customers pay.

An invoice is addressed by a key the client chooses, so sending the same one
twice does nothing twice. It freezes the buyer's name, tax number and address as
they were on the day it was issued, because a tax invoice is a legal document
and last year's copy mustn't change when somebody edits a customer record.

It also answers who owes money, how much, and how overdue they are, bucketed by
age and grouped by customer and currency.

## purchases

Bills, the money you pay suppliers, and input tax. The same supplier invoice
can't be recorded twice.

## tax_sa

Saudi VAT and ZATCA electronic invoicing.

It builds the VAT return by netting output tax against input tax, which is why
it sits above both `sales` and `purchases`. Each invoice gets signed, given a
sequential counter and the hash of the invoice before it, then submitted to
ZATCA.

The Saudi rate arrives as seeded data when a tenant enables the module, kept
separate from the schema because a tenant's data and a tenant's tables are
different things.

## booking

Reservations, rotas, availability and pricing.

Whether one more fits is answered by `erp-occupancy`, which is a crate and not a
module for a reason: a read model can be rebuilt and an accepted booking cannot
be un-accepted. A resource belongs to a branch and is set once — a chair that
physically moves is a new resource, because changing it would retroactively
re-attribute every booking it ever held to a place it was not at.

## prepaid

Packages, deposits, subscriptions and loyalty — everything a customer has paid
for and not yet had.

The money is a liability until it is honoured, so all of it is deferred revenue
until it is not, and `GET /v1/prepaid/outstanding` must equal the deferred
revenue account's balance. Loyalty points are allocated by IFRS 15 relative
standalone selling price with no shortcut, which is why 100 riyals earning 100
points worth 0.10 each defers 9.09 and not 10.00.

## pos

The counter: a shift, a till sale, and the variance a manager reads.

**It writes no document of its own.** A till transaction *is* a ZATCA simplified
invoice, so `pos` composes `sales` — the invoice, its payment and the drawer land
in one transaction. A second document model would give revenue two sources of
truth, and the VAT return and the till report could disagree with nobody able to
say which was right.

What is left is the drawer: the float, takings by tender, refunds and pay-outs,
the count somebody took, and the variance. Only cash is in the box, and the
variance posts — a shortage that is recorded but not booked leaves the ledger
saying the drawer holds what it does not.

## payroll

What a business pays its people, and the entry it makes.

**Drafting posts nothing and approving posts.** A business reads the draft, fixes
the two people whose overtime is wrong, and runs it over — a single-step run
would have posted the first attempt before anybody looked.

A hundred employees is a hundred payslips and one journal entry. Gross is the
expense and net is what is owed; what is withheld is a liability, because it is
somebody else's money. GOSI and the WPS file are Saudi statute and belong in a
country module, for the reason VAT lives in `tax_sa`.

## hr_sa

What the Kingdom requires of an employer: GOSI contributions, and the
end-of-service benefit.

A country module, mirroring `tax_sa`, for the same reason VAT is not in `sales`.
It holds no state at all: every function is arithmetic over what it is given,
and the one thing it stores — the GOSI schedule — is a configuration value.

**The rates are configuration and the shipped ones are a starting point.** The
authority sets the schedule and has changed it; a build that hard-coded a
percentage would be quietly wrong for somebody from the day it shipped. The API
says whether anybody has confirmed them.

## reports

Figures that agree with the books: what was sold, how the diary went, what the
tills took, and what people cost.

**The one module that consumes other modules' events rather than serving its
own.** A dashboard mixing four subjects looks like it must read four projection
groups, and L3 forbids that: four checkpoints can sit at four positions, so a
total across them is a number that was never true. This subscribes to the log
instead, and keeps one checkpoint.

The price is that it remembers things — what each invoice came to, what each
booking holds, who has which till open — because the events it reads carry what
changed and not what did not. That is a working table, and it is cheaper than a
report that is occasionally wrong in a way nobody can reproduce.

**A discrepancy is a failure.** Every figure reconciles against the ledger, from
this module's own copy of it at its own checkpoint, and a difference makes the
tenant unhealthy rather than a cell amber.

## messaging

Reaching somebody: SMS, email, push and WhatsApp, from templates that ask the
read model for what they say.

**A template names an audience, not an address** — the client, the person doing
the work, whoever runs that branch — and the address is a query run minutes
before the message goes. Somebody who changes their number this morning gets
this afternoon's reminder.

**Bindings are declared**, so a template that says something it cannot know is
refused when it is saved rather than sent with a gap in it. Both languages are
the template, per D12.

**Segments are counted and a budget refuses.** SMS is billed per 160 characters,
or per 70 in Arabic, which here means every message — so the count is part of
sending and a tenant finds out when they write the template rather than when the
invoice arrives.

## files

Documents attached to things: an invoice, a bill, a booking, a customer, an
employee record, a journal entry, or the business itself.

**An event stores a key, never a URL.** A URL is where a file is today; a key is
what it is, and a tenant who moves from disk to object storage has not changed
any of their documents. Where the bytes live is the tenant's choice (D15), which
for a business that keeps its own records is the reason they can buy this at all.

**The checksum is verified on read.** A document that comes back different from
what was stored is a failure, not a warning.

**Taking one off does not erase it.** A document that was on an invoice is part
of what happened.

## How modules learn about each other

By subscribing to the log. `tax_sa` finds out an invoice was issued by reading
events, and `sales` has no idea Saudi Arabia exists. Adding a country module
changes nothing in the modules it depends on.
