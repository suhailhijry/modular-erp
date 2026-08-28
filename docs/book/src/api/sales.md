# sales

Invoicing with Saudi VAT, posting to the ledger. The second module, and the one
that answered a question the first could only assert: **how two modules meet.**

**Depends on:** `ledger`, plus the core.
**Depended on by:** `tax_sa`.

## The answer, and the one it replaced

The plan said cross-module integration by event: sales would emit an event, the
outbox would carry a promise, and a handler would post to the ledger a moment
later. Building it made the cost obvious.

The outbox is at-least-once delivery to something this process cannot roll back.
That is the right tool for an email or a call to ZATCA, and a strictly worse one
for two aggregates in the same database, where atomicity is *available*. Taking
the asynchronous route would have traded a guarantee for a dead-letter queue and
a sweeper.

So an invoice and its journal entry commit together, and `ledger::post_entry_in`
is the seam that makes it possible. The ledger owns what posting means; sales
owns when.

What sales does **not** get is a connection to the ledger's tables. `proj_sales`
and `proj_ledger` never read each other, which is L3. They share the event log
and nothing else.

## The files

| File | What is in it |
|---|---|
| [`invoice.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/sales/src/invoice.rs) | `Invoice`, `InvoiceEvent`, `Customer`, `Address`, lines, discounts |
| [`vat.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/sales/src/vat.rs) | `Vat`, `TaxBand`, `Totals`, `total` |
| [`posting.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/sales/src/posting.rs) | `PostingAccounts`, `entry_for_issue`, `entry_for_payment` |
| [`commands.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/sales/src/commands.rs) | `issue_invoice`, `record_payment`, `cancel_invoice` |
| [`projections.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/sales/src/projections.rs) | The `Sales` group, invoices, the VAT return, receivables |
| [`http.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/sales/src/http.rs) | The routes |

## The invoice

```rust
pub struct Customer { … }
impl Customer {
    pub fn new(name: impl Into<String>) -> Self;
    pub fn at(self, address: Address) -> Self;
    pub fn with_vat_number(self, number: impl Into<String>) -> Self;
}

pub struct Address { … }
```

**Who the invoice is addressed to, as it was at the time.** A snapshot, not a
reference. A tax invoice is a legal document: changing a customer's registered
name next year must not rewrite what was issued this year, and a foreign key
would do exactly that. This is L5 applied to the most visible place it matters.

There is no customer aggregate yet. When somebody wants a customer list or a
statement of account, that is what earns one, and it will still be copied onto
the invoice at issue for the reason above.

The address is on the invoice for the same reason, and it exists at all because
ZATCA wants a buyer address on a **standard** invoice: street, city and country
at minimum (BT-50, BT-52, BT-55). Without one it accepts the document and warns,
which is a warning that becomes a finding at an inspection. It is optional,
because a consumer at a till gives no address.

```rust
pub struct InvoiceLine { … }     // what is being charged for, and its treatment
pub struct DraftLine { … }       // as a client sends it
pub struct Discount { … }        // taken off the whole invoice
pub struct DraftDiscount { … }
```

A `DraftLine` carries the treatment and **not the rate**. The rate is the
tenant's configuration, resolved in the command's own transaction, because a rate
that changed between the request and the write would stamp an invoice with one
that was never current.

There is no quantity or unit price. A client that shows "3 × 250.00" already
computed the 750.00 it sends. Storing the factors matters when ZATCA's line-level
fields are implemented, and adding them then is an upcaster, which is the
mechanism this system already has and tests.

### Why a discount is not a negative line

A negative line is what this system had, and it is invisible on the document: the
invoice shows a smaller total and nothing says why.

ZATCA models a discount as `cac:AllowanceCharge`, an amount and a reason and the
tax treatment it comes off, and prints it as its own figure, so a customer sees
what they were charged and what they were let off.

**The tax treatment is part of it.** Discounting a standard-rated invoice reduces
the tax; discounting an exempt one does not, because there was none.

```rust
pub enum InvoiceEvent {
    Issued { number: Option<String>, customer: Customer, issued_on: Timestamp,
             due_on: Option<Timestamp>, currency: CurrencyCode,
             lines: …, discounts: …, totals: …, note: … },
    PaymentRecorded { … },
    Cancelled { … },
}
impl InvoiceEvent { pub const NAMES: [&'static str; 3]; }

pub struct Invoice { … }
impl Invoice {
    pub fn outstanding(&self) -> Option<Money>;
    pub const fn is_cancelled(&self) -> bool;
    pub fn has_payment(&self, reference: &str) -> bool;
}
```

`number` is in the event and never derived on read, because that is the whole
point: a replay must reproduce the number the document was issued under, not the
one today's counter would give. It is `None` on invoices issued before this
system numbered them, whose number *was* their client-chosen id. That is not an
upcaster's job, because an upcaster sees the payload and not the stream it came
from, so there is nowhere for the old number to come from, and `None` is the
honest statement that nothing allocated one.

`issued_on` is the tax point, the date the supply is treated as made. Not when
the row was written.

## VAT

```rust
pub use ledger::VatCategory;

pub struct Vat { … }
impl Vat {
    pub const fn at(rates: ledger::Rates, category: VatCategory) -> Self;
    pub const fn shipped(category: VatCategory) -> Self;
    pub fn on(self, net: Money) -> Result<Money, TaxError>;
}

pub struct TaxBand { … }
pub struct Totals { … }
impl Totals {
    pub fn discount(&self) -> Money;
    pub fn before_discount(&self) -> Result<Money, TaxError>;
}

pub fn total(amounts: impl IntoIterator<Item = (Vat, Money)>,
             discounts: impl IntoIterator<Item = (Vat, Money)>,
             currency: CurrencyCode) -> Result<Totals, TaxError>;
```

**The rate is stored, not looked up.** Saudi VAT was 5% until July 2020 and has
been 15% since. An invoice issued in 2019 is still 5%, and it must still print as
5% in 2031, so the rate is resolved when the invoice is issued and written into
the event.

`Vat::at` is the only constructor an issuing command should use. `Vat::shipped`
is for tests and for anything that has no tenant to ask, and **never on a write
path**.

**Rounding is half away from zero**, which is what ZATCA's invoicing rules specify
and what every till in the country does. `15.005` becomes `15.01` and `-15.005`
becomes `-15.01`, symmetric, so crediting an invoice line reverses it exactly
instead of leaving a halala behind.

**Saudi invoices report per rate, not per line**, which is also the only way the
arithmetic can be checked: rounding each line and summing gives a different
answer from rounding the subtotal, and the subtotal is the one the authority
computes. That is what `TaxBand` is.

`total` sums nets by band and taxes each band once. **Ordering of the result does
not depend on the order of the input**, so two clients sending the same lines in
different orders get byte-identical events.

## Posting

```rust
pub struct PostingAccounts { … }
impl PostingAccounts {
    pub const KEY: &'static str = "sales.posting_accounts";
    pub fn conventional() -> Self;
    pub async fn resolve(conn: &mut PgConnection) -> Result<Self, ConfigError>;
}

pub fn entry_for_issue(totals: &Totals, accounts: &PostingAccounts)
    -> Result<BalancedLines, Unbalanced>;
pub fn entry_for_payment(amount: Money, into: &AggregateId, accounts: &PostingAccounts)
    -> Result<BalancedLines, Unbalanced>;
```

This is the whole of the cross-module integration, and it is deliberately a
**pure function**. An invoice and a set of account codes go in, `BalancedLines`
comes out. Nothing here touches a database, so what the ledger will be told is
decided, and testable, before any transaction is open.

`entry_for_issue`: debit the customer for the whole bill, credit revenue for the
part that is income and VAT payable for the part that belongs to the authority.
**The tax line is omitted when there is no tax**, never posted as zero. A zero
line is not a posting, and the ledger refuses one.

`entry_for_payment`: debit whatever took the money, credit the customer's
balance. **Nothing here touches revenue**, which was recognised when the invoice
was issued. Recognising it again on payment is the classic way to double-count a
year.

`PostingAccounts` is a struct and not four constants because it is the seam.
Account determination is configuration in every real ERP, by customer group, by
item, by branch, and when that arrives the only thing that changes is where this
value comes from.

A tenant who never opens the settings gets `conventional()`, the codes every
shipped chart uses. A tenant who *has* configured it and stored something
unusable gets an error, never the default. Silently falling back would hide a
misconfiguration until a month-end reconciliation found it.

## Commands

```rust
pub struct Numbered { pub committed: Committed<InvoiceEvent>, pub number: String }

pub struct Draft {
    pub customer: Customer,
    pub issued_on: Timestamp,          // the tax point
    pub due_on: Option<Timestamp>,
    pub currency: CurrencyCode,
    pub lines: Vec<DraftLine>,
    pub discounts: Vec<DraftDiscount>,
    pub note: String,
}

pub struct Receipt {
    pub reference: String,             // the client's or the bank's own
    pub amount: Money,
    pub received_on: Timestamp,
    pub into: AggregateId,             // the cash or bank account that took it
}

pub async fn issue_invoice(db: &TenantDb, id: &AggregateId, draft: &Draft,
    metadata: &Metadata) -> Result<Numbered, CommandError<SalesError>>;

pub async fn record_payment(db: &TenantDb, invoice: &AggregateId,
    receipt: &Receipt, metadata: &Metadata) -> Result<Committed<InvoiceEvent>, …>;

pub async fn cancel_invoice(db: &TenantDb, invoice: &AggregateId,
    credit_note: &str, reason: &str, on: Timestamp, metadata: &Metadata)
    -> Result<Numbered, CommandError<SalesError>>;
```

`Draft` is a struct and not eight parameters. Half of them are strings, and
transposing two strings is a bug no type can catch.

**The number comes back even when the command did nothing.** A client whose
request timed out and retried has to be told the number the invoice already
carries. Telling it "done" and nothing else would leave it to guess, and the
guess would be a number that does not exist.

Re-issuing the same `id` is a no-op: the stored invoice wins and the second
caller's lines are ignored, never applied. A client that meant a different
invoice should send a different id. Recording the same `reference` twice is a
no-op for the same reason.

### cancel_invoice

Credits the invoice: the journal entry it made is reversed, and the invoice
records which credit note did it.

**Not a deletion.** The invoice was issued, the customer may hold a copy, and the
books end up showing both it and the credit. Same reason the ledger reverses.

**Not a partial credit either.** Crediting some lines and not others is a document
with lines of its own, and nobody has asked for one. When they do, it is a second
command and this one stays as the whole-invoice case.

An invoice with payments is refused. Money came in against it, and cancelling
without addressing that leaves a payment against a document that no longer says
anything is owed.

## Numbering

```rust
pub const INVOICE_SERIES: &str = "sales.invoice";
pub const CREDIT_NOTE_SERIES: &str = "sales.credit_note";
pub fn format_number(prefix: &str, value: i64) -> String;
```

Two series, because a credit note is a statutory document too and ZATCA numbers
it separately from the invoices it credits.

The prefix and the five-digit width are fixed. They become a `sales.numbering`
configuration the first time a tenant asks, and the only new thing would be the
route, because the store and the typed surface both already exist. Worth knowing
the shape when that happens: a tenant must choose **before** their first invoice,
because a number that has been on a document cannot be restated. A year-reset
series is the other common shape and is a bigger change than a format string.

The mechanism is `erp_eventlog::numbering`, and the `reserve` / `consume` pairing
is why re-issuing does not move the series.

## Read models

```rust
pub struct Sales;
impl ProjectionGroup for Sales {
    const NAME: &'static str = "sales";
    const SCHEMA: &'static str = "proj_sales";
}

pub struct Invoices;
pub fn projections() -> Vec<Arc<dyn Projection<Group = Sales>>>;

pub struct InvoiceSummary { … }
pub struct InvoiceLineRow { … }
pub struct TaxRow { … }
pub struct PaymentRow { … }
pub struct InvoiceDetail { … }

pub async fn invoices(conn: &mut PgConnection, limit: i64, after: Option<&Cursor>)
    -> Result<Page<InvoiceSummary>, sqlx::Error>;
pub async fn invoice(conn: &mut PgConnection, id: &str)
    -> Result<Option<InvoiceDetail>, sqlx::Error>;
```

Invoices, their lines, their tax bands and their payments are **one group**,
because a payment against an invoice that has not appeared yet is a state nobody
should be able to query. The foreign keys in `install.sql` turn that from a
convention into a constraint.

It is a *different* group from the ledger's, which is the point.

`invoices` is keyset on `(issued_on, id)` descending, and the cursor is the last
row's pair. It reads one index range whatever page it is on, and an invoice
issued while somebody pages cannot displace a row they have not seen yet.

`invoice` returns `None` if there is no such invoice, **or** if the projection
has not caught up with it yet, which is what `?consistent_after=` is for.

### The VAT return

```rust
pub struct VatBand { … }
pub struct VatReturn { … }

pub async fn vat_return(conn: &mut PgConnection, currency: CurrencyCode,
    from: Timestamp, until: Timestamp) -> Result<VatReturn, sqlx::Error>;
```

The **output-tax** side: what a business charged. A full return also nets off
input tax on purchases, which is `tax_sa`'s job.

**The period is half-open**, `[from, until)`. A period ending "31 March inclusive"
is a timestamp comparison somebody gets wrong once a quarter, and two consecutive
returns built that way either double-count the boundary or drop it.

### Receivables

```rust
pub struct AgedCustomer { … }

pub async fn receivables(conn: &mut PgConnection, as_of: Timestamp,
    limit: i64, after: Option<&Cursor>) -> Result<Page<AgedCustomer>, sqlx::Error>;
```

Who owes what, and for how long. The one question an accounts-receivable clerk
asks every morning, and until this existed the system could not answer it:
invoices could be listed and paid, but not summed by the person who owed them.

**Keyed by customer and currency.** `Money` has no `Add`, and arithmetic is
`checked_add`, which refuses a currency mismatch. That is not a limitation to
work around here, it is the answer: a customer invoiced in SAR and in USD owes
two amounts, and one number that added them would be a lie in whichever currency
it claimed to be. So a customer trading in two appears twice.

**Aged from the due date, falling back to the issue date.** An invoice with no
`due_on` carries no terms, which means it was due when it was issued. Treating
those as not-yet-due for ever is how a ledger fills up with debts nobody chases.

**`as_of` is a parameter, not the clock.** An accountant closing March needs the
ageing as it stood on 31 March, not as it stands today, and a function that read
the clock could not give them that. Being testable is the second reason.

Until customers are records, the grouping is by the frozen name on the invoice,
so two spellings are two rows. That is what Phase 7a fixes.

### The health check

```rust
pub struct Overpaid { … }
pub async fn overpaid(conn: &mut PgConnection) -> Result<Vec<Overpaid>, sqlx::Error>;
```

An invoice whose payments exceed it. **Impossible through `record_payment`**,
which refuses an overpayment against the aggregate's own state. A row here means
the pipeline is broken: a payment projected twice, or a rebuild that diverged. It
is the same kind of canary as the trial balance, and is registered in
`bin/worker.rs` as `NoOverpaidInvoice`.

## Routes

| Method | Path | Capability |
|---|---|---|
| `GET` `POST` | `/v1/sales/invoices` | Read / PostEntries |
| `GET` | `/v1/sales/invoices/{invoice}` | Read |
| `POST` | `/v1/sales/invoices/{invoice}/payments` | PostEntries |
| `POST` | `/v1/sales/invoices/{invoice}/credit-note` | PostEntries |
| `GET` | `/v1/sales/receivables` | Read |
| `GET` `PUT` | `/v1/sales/posting-accounts` | Read / ManageAccounts |

## What is deliberately absent

Customers as records, quantities and unit prices, and partial credit notes. Every
one of them is additive.
