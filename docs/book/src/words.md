# Words this system uses

Each of these has exactly one meaning throughout the book, and it's the meaning
given here.

## Tenant

One company. A tenant has its own database, its own accounts and its own users,
and two tenants never share a table or a row.

## Control plane

The part that knows which tenants exist, who's allowed into them, and where
their databases live. It holds no accounting data itself.

## Tenant plane

A tenant's own database, holding the event log, the read models built from it,
and that tenant's configuration.

## Event

Something that happened, written once and never changed afterwards, like
`InvoiceIssued` or `PaymentReceived`. An event carries whatever was decided at
the time, holding the value itself, so an invoice from last year still shows the
tax rate that actually applied to it.

## Log

Every event belonging to one tenant, in the order they committed. Positions in
the log have no gaps.

## Projection

A table built by reading the log. The invoice list is a projection, and so is
the trial balance. Delete one and the system can rebuild it.

## Projection group

A set of tables that have to agree with each other. A group shares one position
marker and moves as a unit, and tables in different groups aren't allowed to
read each other.

## Module

A part of the product a tenant can switch on or leave off, like `ledger`,
`sales`, `purchases` or `tax_sa`. Every build contains all of them, and which
ones actually run is a per-tenant setting.

## Command

A request to change something. A command reads what it needs, decides, and
writes events, and it doesn't send email or call anyone while it's doing that.

## Effect

Work that a command wants done outside the database, like an email or a message.
The command writes the effect alongside its events in the same transaction, and
a worker carries it out afterwards, which is why a command that rolls back never
sends anything.

## Outbox

Where effects wait for the worker to pick them up.

## Blueprint

A named list of commands that sets a tenant up, whether that's a chart of
accounts, a demo dataset or a set of rules. Tenants install blueprints, so
nobody has to run a script by hand.
