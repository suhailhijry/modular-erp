# pos

The counter: a shift, a till sale, and the variance a manager reads.

**Depends on:** `sales`, `ledger`, `crm`, and the core.
**Depended on by:** nothing.

## This module does not invoice, and that is its whole design

A till transaction **is** a ZATCA simplified invoice. `sales` already builds
one, numbers it from a gapless statutory series, hashes it, chains it, signs it
and reports it within the day; `tax_sa` already decides whether a buyer's VAT
number makes it standard, and therefore cleared before the customer can be
handed it.

A second document model here would duplicate VAT, discounts, numbering, the
ZATCA chain and credit notes — and, worse, give revenue **two sources of
truth**, so the VAT return and the till report could disagree with nobody able
to say which was right.

So `pos` composes. `sell` writes the shift's event, the invoice and its payment
in **one transaction**, through `sales::issue_in` and `sales::pay_in` — the same
seam `sales` itself uses on `ledger`. A sale that exists as an invoice but not
as takings, or as takings but not as an invoice, is a state nobody could explain
and nothing would clean up.

The absence shows in the schema: **there is no `sale` table in `proj_pos`**, and
no `GET /v1/pos/sales`.

## What is left, once the document is somebody else's

The **drawer**. Money physically arrives, in a mix of tenders, into a box
somebody counts at the end of the day, and the number that box is short by is
the only number in this module a manager actually reads.

| The shift holds | Because |
|---|---|
| the opening float | the drawer did not start empty |
| takings by tender | only cash is in the box; a card settles to a bank |
| refunds and pay-outs | cash leaves the box for reasons that are not sales |
| the declared count | what a person actually counted |
| **the variance** | the difference, which is the number that gets read |

## The files

| File | What is in it |
|---|---|
| [`shift.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/pos/src/shift.rs) | `Shift`, `ShiftEvent`, `Method`, `Tender`, `Takings` |
| [`posting.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/pos/src/posting.rs) | `PostingAccounts`, and the only two entries this module makes |
| [`commands.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/pos/src/commands.rs) | `open_shift`, `sell`, `take_back`, `pay_out`, `close_shift` |
| [`projections.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/pos/src/projections.rs) | The `Pos` group: shifts and takings |
| [`http.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/pos/src/http.rs) | Ten routes |

## Only cash is in the drawer

```rust
pub enum Method { Cash, Card, Transfer }

impl Method {
    pub const fn is_in_the_drawer(self) -> bool { matches!(self, Self::Cash) }
}
```

This is the module's single most important rule, and the failure it prevents is
the one that makes the whole feature useless: counting card takings into the
expected drawer makes **every honest till look short by exactly the day's card
sales**, and a manager who sees that twice stops reading the number.

The rule lives in exactly one place, and that is not decoration. It was written
twice — `Shift::expected` naming cash directly while the projection asked
`is_in_the_drawer()` — and mutating `is_in_the_drawer` to return `true` left the
entire test suite green, because nothing ever asked the aggregate and the
projection the same question. `Takings::in_the_drawer(currency)` is the fix: one
answer, asked by both.

```rust
pub fn in_the_drawer(&self, currency: CurrencyCode) -> Result<Money, MoneyError> {
    Money::checked_sum(self.entries.iter().filter_map(Tender::in_the_drawer), currency)
}
```

## The two entries the drawer makes for itself

A till sale posts `Dr receivable, Cr revenue, Cr VAT` and its payment posts
`Dr cash, Cr receivable` — and **`sales` writes both**. What is left is money
that moves for reasons a sale cannot explain:

| When | Entry |
|---|---|
| Paid out — a banking run, a supplier in cash | `Dr` the account named, `Cr` cash |
| Closed short — the count is under the books | `Dr` cash over and short, `Cr` cash |
| Closed over — the count is above the books | `Dr` cash, `Cr` cash over and short |

**The variance has to post, and that is the accounting reason it exists.** A
till that records a shortage and does not book it leaves the ledger saying the
drawer holds what it does not, for ever, and the next reconciliation inherits
the lie.

```rust
pub struct PostingAccounts {
    pub cash:       AggregateId,  // 1000 Cash on hand — not 1010, which is the bank
    pub bank:       AggregateId,  // 1010 Bank: a card settles here, not into the box
    pub over_short: AggregateId,  // 5910
}
```

Configured under `pos.posting_accounts`. A tenant who never opens the settings
gets the conventional codes; one who *has* configured it and stored something
unusable gets an error rather than a silent default, for the reason `prepaid`
does the same — a year of shortages posted to the wrong account is found at an
audit.

## Commands

```rust
pub async fn open_shift(db, id, opening: &Opening, metadata)                 -> Outcome;
pub async fn sell(db, shift, sale, basket: &Basket, metadata)                -> Result<Rung, _>;
pub async fn take_back(db, shift, sale, returning: &Return, metadata)        -> Outcome;
pub async fn pay_out(db, shift, payment: &PayOut, metadata)                  -> Outcome;
pub async fn close_shift(db, shift, declared: Money, at, metadata)           -> Outcome;
```

**Opening posts nothing.** A float is cash moved from a safe to a drawer and
both are `1000 Cash on hand`, so the business is no richer for having moved it.
It follows that what the drawer should physically hold is a larger number than
what the shift added to the ledger, and that the two answer different questions.
The variance is what reconciles them, and it is the only one of the three that
posts.

**The tenders must come to exactly the sale.** Not less: a till sale that leaves
a balance is an invoice on credit. Not more: `sales` refuses an overpayment and
is right to — change handed back is a counter concern, not a record. A customer
who hands over fifty riyals for a forty-three riyal basket is recorded as
forty-three, which is what the drawer actually gains.

`Rung` answers with the **statutory number and the total**, because that is what
a receipt prints. The document is read at `GET /v1/sales/invoices/{sale}`.

## Returns, and the change to `sales` they needed first

`cancel_invoice` refused any invoice that had ever been paid — and **every till
sale is paid the instant it happens**, so no till sale could be credited through
any route.

The rule was not wrong so much as too blunt. What a credit note may not do is
undo a supply *while the business keeps the cash*. So `sales` gained a refund,
the rule became `held() == 0` — paid less refunded — and `take_back` hands the
money back and credits the document in the same transaction, which is also the
only order in which the books are never briefly wrong.

A return credits the **whole** sale: a partial credit note is not something
`sales` can write.

### A footgun this uncovered

`sales::issue_in` used to take the journal entry's id as an argument, and
`cancel_in` reverses that entry by rebuilding the same name. `pos` passed its
own prefix — reasonably — so every pos invoice was uncreditable, failing as
`NoSuchEntry("si.SALE-1")`. The name is now derived inside `sales` and is no
longer a parameter anybody can get wrong. It was invisible until a second caller
existed, which is the argument for having one.

## Idempotency, and a bug in it worth recording

Every command here is a no-op on a retry, and each keys that differently:

| Command | Keyed by |
|---|---|
| `open_shift`, `sell` | the `Idempotency-Key`, which **is** the stream id |
| `take_back` | `Return::reference`, seen-listed on the shift |
| `pay_out` | `PayOut::reference`, seen-listed on the shift |
| `close_shift` | the state: closing a shut till is a no-op, not an error |

`take_back` originally checked `has_pay_out` — the *pay-out* list — and
`ShiftEvent::Refunded` recorded no key at all. So a retried return deduplicated
perfectly in `sales` (the credit note and the money are keyed by reference
there) while the shift appended a second `Refunded` every time, taking the
drawer down once per retry.

The test passed, because it asserted the ledger balances and the ledger was the
half somebody else was already protecting. Three retries of a 17.25 return left
a drawer that should have held nothing holding **−34.50**. `Refunded` now
carries its own `reference` and `Shift::has_return` answers for it, and the test
asserts the drawer as well as the books.

Two seen-lists rather than one shared list, because a banking run and a return
are different caller namespaces, and sharing one would let a reference collision
silence the wrong thing.

## Read models

```rust
pub async fn shifts(conn, till: Option<&str>, open_only: bool, limit, after)
    -> Result<Page<ShiftSummary>, sqlx::Error>;

pub async fn shift(conn, id: &str)     -> Result<Option<ShiftSummary>, sqlx::Error>;
pub async fn takings(conn, shift: &str) -> Result<Vec<TakingRow>, sqlx::Error>;
```

`till` is optional so one read serves a counter's day and the whole shop's.
`expected` is maintained as sales land, so it can be read mid-shift.

**`expected` is answered from aggregate state at the counter, not from this
table.** A running total computed from a projection is a number that can be a
second behind while somebody is counting against it — the same reason
`Subscription::admits` is answered from the aggregate.

## What this module deliberately does not do

**Offline.** A till that queues sales locally and reconciles later is a second
write path with its own ordering problem, and L1 — gapless, commit-ordered
positions — is not negotiable. It is cheap to demonstrate and expensive to be
correct about. Revisit when a customer loses money to its absence.

**Snapshots.** A shift is bounded by a working day, so its stream is bounded
too: forty coffees is forty events, which is a cheap load. A till that never
closes is a till nobody is reconciling, and the fix for that is a manager.

## Routes

See [The HTTP API](./http.md#the-counter).
