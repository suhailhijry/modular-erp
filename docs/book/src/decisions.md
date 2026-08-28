# Why it is built this way

Every decision here is written down because reversing it later costs far more
than making it did. The full reasoning lives in `docs/ARCHITECTURE.md`, which is
the document you change before you change the code.

## One database for each tenant

Two tenants share no tables at all. A query that reads the wrong tenant isn't
prevented by a `WHERE` clause, it's prevented because there's no connection in
scope that could run it.

The cost is that thousands of databases need managing, so the system treats a
database cluster as a row in a table. An operator can bring capacity online
without a deploy.

## The log is the truth

Commands write events, and read models get built from those events. That buys a
company a record nothing can alter, a traceable reason behind every figure, and
the ability to answer questions nobody thought to ask when the data was
originally written.

For developers it buys something more immediate: a read model can be wrong and
then fixed. If a total is computed incorrectly, you correct the code and rebuild
the table, because the events were never wrong in the first place.

## Postgres carries both the log and the messages

There's no message broker anywhere. Read models are built by reading the log in
order, and adding a broker would create a second copy of the truth that somebody
then has to keep reconciled with the first.

## Events carry outcomes, not references

When a command resolves a tax rate it writes the number straight into the event.
The rate table can change tomorrow and today's invoice still says what it
charged.

This is what lets the system be configurable and reproducible at the same time.
Without it you get to pick one.

## Every build contains every module

Tenants switch modules on and off, and nobody deploys a different binary for a
different customer. One build serves everyone, so a fix reaches all of them at
once.

## Effects are values, not calls

A command that wants to send an email writes that intention beside its events in
a single transaction, and a worker sends it later. So a command that rolls back
emails nobody, a command that crashed after committing still owes the email, and
rebuilding a read model sends nothing at all.

## Money carries its currency

An amount is an integer count of the smallest unit and it knows which currency
it's in. There's no `+` operator, so adding two amounts means calling a function
that returns a result, and the compiler makes you handle the case where the
currencies don't match. Floating point arithmetic is denied across the entire
codebase.

## The core holds no business rules

Accounting is a module, and so is selling, buying and Saudi tax. What stays in
the core is identity, tenancy, the log and the machinery that reads it, which
means a company that doesn't do accounting can still run this system.

## Upgrades move one step at a time

We support the current major version and the one before it. Coming from further
back means installing the version in between first, because a single step is the
only upgrade path that can be tested exhaustively. The system refuses anything
larger.
