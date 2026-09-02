# Reading this reference

This half of the book documents the code itself: every crate, what each public
type and function is for, and how to call it.

## Two kinds of documentation, and which one you want

`cargo doc` generates the exhaustive version straight from the source. Every
item, every signature, every trait implementation, cross-linked and searchable,
and it cannot fall out of date because it is the source:

```bash
just docs
```

That opens `target/doc/erp_api/index.html`. Use it when you know the name of the
thing you are looking for.

These chapters are the other kind. They cover the same items with the parts
rustdoc cannot show you: which crate to reach for, what a normal call looks like
end to end, which errors you actually have to handle, and what will happen if
you use something the way it was not meant to be used. Use them when you know
what you want to do and not yet which type does it.

## How a chapter is laid out

Every crate chapter has the same four parts.

**What it is for** in a paragraph, and what depends on it. Dependencies point one
way in this workspace, so the position of a crate in the list tells you what it
is allowed to know about.

**The files**, with one line each. This is the map. A crate of nine files is
nine ideas, and the table says which file holds which.

**The public surface**, grouped by what you would be trying to do. Each entry
carries the signature, what it does, and the reason it is shaped that way when
the shape is surprising. Signatures are copied from the source and are current as
of the build that produced this book.

**Worked examples**, taken from real call sites in the codebase where one exists.
An example that nothing in the repository actually does is a guess, and this book
tries not to guess.

## The order

Bottom to top, the way the dependencies point. `erp-types` knows nothing about
anything; `erp-api` knows about everything. If you read straight through, no
chapter uses a type you have not already met.

| | Crate | One line |
|---|---|---|
| 1 | [`erp-types`](./erp-types.md) | Newtypes, `Money`, `NonEmpty`, paging. No I/O |
| 2 | [`erp-i18n`](./erp-i18n.md) | Message codes, locales, the `Localize` trait |
| 3 | [`erp-eventlog`](./erp-eventlog.md) | The tenant log: append, load, upcast, number, enqueue |
| 4 | [`erp-occupancy`](./erp-occupancy.md) | Capacity over time: does one more fit |
| 5 | [`erp-projection`](./erp-projection.md) | Groups, the runner, shadow replay |
| 6 | [`erp-tenant`](./erp-tenant.md) | `TenantDb`, the connection budget, roles, module setup |
| 7 | [`erp-control`](./erp-control.md) | Identities, tenants, entitlements, clusters, the fleet |
| 8 | [`erp-web`](./erp-web.md) | Extractors, problem+json, paging, consistency |
| 9 | [`erp-worker`](./erp-worker.md) | The `Job` trait, the visit loop, the three binaries |
| 10 | [`erp-api`](./erp-api.md) | The core's routes, the module list, the composition root |
| 11 | [`erp-demo`](./erp-demo.md) | The seeded tenant |
| 12 | [`erp-testkit`](./erp-testkit.md) | Template databases, fault injection, the differ |
| 13 | [`branches`](./branches.md) | Places to trade from; the dimension on every document |
| 14 | [`crm`](./crm.md) | Customers as records |
| 15 | [`ledger`](./ledger.md) | Accounts, journal entries, periods, VAT, charts |
| 16 | [`sales`](./sales.md) | Invoices, credit notes, payments in, refunds, receivables |
| 17 | [`purchases`](./purchases.md) | Bills, payments out, input tax |
| 18 | [`tax_sa`](./tax_sa.md) | The Saudi rate, the VAT return, ZATCA |
| 19 | [`booking`](./booking.md) | Reservations, rotas, availability, pricing |
| 20 | [`prepaid`](./prepaid.md) | Packages, deposits, subscriptions, loyalty, deferred revenue |
| 21 | [`pos`](./pos.md) | Shifts, till sales, the drawer and its variance |
| | [The HTTP API](./http.md) | All 124 operations, with curl for each |

`erp-occupancy` sits at 4 rather than beside `booking` because that is where its
dependencies put it: it knows about `erp-types` and nothing else, and `booking`
is the only thing that knows about it. It is a crate and not a module for a
reason worth reading before the booking chapter — a read model can be rebuilt,
and an accepted booking cannot be un-accepted.

## A note on the signatures

Where a signature is long it is wrapped, and where a generic bound adds nothing
to the explanation it is elided with `…`. The authoritative version is always
`cargo doc` or the file itself, and every entry names its file so you can go
there in one jump.
