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
| [`commands.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/sales/src/commands.rs) | `issue_invoice`, `record_payment`, `refund_invoice`, `cancel_invoice`, `attach_customer` |
| [`limit.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/sales/src/limit.rs) | `Authority`, `DocumentLimit`, `Basis` — how large a document a member may issue |
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

A line may be given as a **price and a quantity**, and then the line comes to
their product — computed here and never divided back out of a total, because
going backwards from a total is a division that does not always land on a whole
halala and `cbc:PriceAmount` has to be exact. A line given as a single amount is
quantity one, which is what every invoice issued before the factors existed is,
and both fields are optional on the wire.

A line may also name a **product**, and then issuing the invoice takes that many
units off that product's shelf at the request's branch, in the invoice's own
transaction, and books what they cost. A serial-tracked line names its units,
and any line may name the **lot** to take them from instead of the one that
expires first. A credit line says how many units came back and, for
serial-tracked stock, which. See `inventory`.

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
    metadata: &Metadata, authority: Authority) -> Result<Numbered, CommandError<SalesError>>;

pub async fn record_payment(db: &TenantDb, invoice: &AggregateId,
    receipt: &Receipt, metadata: &Metadata) -> Result<Committed<InvoiceEvent>, …>;

pub async fn refund_invoice(db: &TenantDb, invoice: &AggregateId,
    receipt: &Receipt, reason: &str, metadata: &Metadata, authority: Authority)
    -> Result<Committed<InvoiceEvent>, …>;

pub async fn cancel_invoice(db: &TenantDb, invoice: &AggregateId,
    credit_note: &str, reason: &str, on: Timestamp, metadata: &Metadata,
    authority: Authority) -> Result<Numbered, CommandError<SalesError>>;
```

Everything that issues a document takes an [`Authority`](#the-document-limit):
these, `credit_invoice_part`, and the `_in` forms other modules compose
(`issue_in`, `credit_in`, `credit_part_in`, `refund_in`, `credit_what_is_clear`).
Recording a payment issues nothing and takes none.

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

**An invoice the business is still holding money against is refused** — not one
that was ever paid, one where `held()` (paid less refunded) is not zero.
Cancelling without addressing that leaves a payment against a document that no
longer says anything is owed.

### refund_invoice

The mirror of a payment, and the thing this module had no concept of until a
till needed it.

The rule above used to be `payments.is_empty()` — *ever paid*, not *still
holding* — and **every till sale is paid the instant it happens**, so no till
sale could be credited through any route at all. The rule was not wrong so much
as too blunt: what a credit note may not do is undo a supply while the business
keeps the cash.

So the money goes back first and the credit note follows, and `pos::take_back`
does both in one transaction — which is also the only order in which the books
are never briefly wrong.

Refunding more than is held is refused for the reason overpaying is: a business
handing back money it never took has made a decision somebody needs to see, and
a negative balance is how that decision never gets made.

`InvoiceEvent::Refunded` carries the amount and the account it went out of, so
the entry is `Dr` the customer, `Cr` wherever the money left — the exact reverse
of the payment, and the reason `invoice_payment` accepts a negative `amount`
with the sign carrying the meaning.

### Crediting is a claim

Once a tenant has granted **any** claim, issuing a credit note — cancelling an
invoice, crediting part of one, a refund that clears one, or asking a gateway
for a refund that will leave one owing — needs `sales:approve_credit_note` on
the org chart. The refusal is `SalesError::NotApproved`,
`403 sales.not_approved`.

It is asked for **in the roots**, `cancel_in` and `credit_part_in`, which every
credit note in this system goes through, and in the same branch the document
limit is judged in: `Authority::Member` who is not the owner. So `pos::take_back`
is asked exactly what `/v1/sales` is — a deliberate change at the till, decided
by the product owner (§70), because until then the check sat in the two wrappers
the sales routes call and the counter went round it. `Authority::System` is not
claim-judged, the same way it is not limited: the credit note a gateway's
confirmed refund implies is a consequence of money that has already moved.

**A member who asks a gateway for a refund is asked when they ask**, in
`may_refund`, beside the document limit that already judges them there — because
the credit note the gateway's answer leaves owing is written with
`Authority::System`, when there is nobody left to ask. Whole or part: both are
credit notes. A refund that leaves no credit note owing is not asked for one.

The refusal is applied **inside** the decision, after the retry check, which is
where §68 put the limit's comparison. So resending a return's `reference` still
answers with the credit note it issued, even once the claim has been revoked: a
retry is not a second document, and answering `403` to one would only invite a
second credit note under a new reference.

The claim is one of `hr::SEGREGATED`, so unlike `sales:exceed_document_limit` it
**does not travel up the chart**: a manager does not hold what their report was
granted. Raising a document and cancelling it must not land in one pair of
hands.

### The document limit

```rust
pub enum Authority { Member { owner: bool }, System }
impl Authority { pub fn of(db: &TenantDb) -> Self; }     // never System

pub struct DocumentLimit { … }                           // KEY = "sales.document_limit"
impl DocumentLimit {
    pub fn new(limit: Money, basis: Basis) -> Result<Self, NotALimit>;  // more than nothing
    pub async fn resolve(conn) -> Result<Option<Self>, ConfigError>;  // None: no limit
}
pub enum Basis { BeforeVat, AfterVat }
pub const EXCEED_DOCUMENT_LIMIT: &str = "sales:exceed_document_limit";
pub async fn may_refund(conn, invoice, refunded: Money, authority, metadata) -> Result<(), …>;
pub async fn may_issue(conn, net: Money, gross: Money, authority, metadata) -> Result<(), …>;
```

**How large a document a member may issue**, set by the owner at
`PUT /v1/sales/document-limit`. Every invoice, every credit note (whole or part)
and every refund issued by a `Member` who is not the owner is refused with
`SalesError::OverDocumentLimit` — `403 sales.over_document_limit`, naming the
limit and the amount — when its total on the chosen basis is more than the
limit. Equal is within it. A document in another currency than the limit's
cannot be compared, and is refused (`sales.document_limit_currency`).

It is judged **inside the command**, after the totals exist and in the same
transaction: in `issue_in` on what the invoice charges (after its discounts and
any deposit deducted), in the two credit-note roots on what the note credits, and
in `refund_in` on what goes back. A refund's before-VAT figure is its share of
the invoice's net, `refunded × net ÷ gross`. A credit note a refund issues is
judged too, so a refund that leaves an invoice holding nothing is refused when
the whole-invoice credit note would be over the limit — `credit_what_is_clear`
issues that note by the same `owed` decision `may_refund` judges by, so the
two answers cannot differ. The comparison sits after the retry check, so a retry
of a document issued before the limit was lowered answers with it.

**Who is not limited.** The owner. Anybody the org chart gives
`sales:exceed_document_limit` in the branch the request names — the claim
travels up like any claim that is not segregated, so a manager holds it when
somebody beneath them does. A member with no employee record can hold no claim,
and is limited. And `Authority::System`, which the paths nobody performs pass in
so many words: a customer's own deposit, the prepayment invoice raised when the
gateway settles one, the credit note and refund the gateway already made
(`payments::refund_in`), and the worker billing completed bookings.

**What a member starts and a gateway finishes is judged when they start it**,
because by the time the gateway answers the money has moved and refusing to
record it would only make the books wrong. A gateway refund: `may_refund` in
`payments::request_refund_in`, on the money and on the credit note it will
imply. A deposit a member charges: `may_issue` in `payments::request_in` and
`start_in`, on the totals its prepayment invoice will have — the deposit's net
and what the customer is charged, which settling refuses to bill unless the
invoice comes to exactly that.

**Not a permission limit.** `erp_tenant::Limits` judges a capability at the
edge, where an invoice's total does not exist yet. Use a permission limit for
what a role may do (`post_entries` over an amount on the ledger's own routes, by
branch, by role); use this for how large one sales document may be.

### attach_customer

```rust
pub async fn attach_customer(db: &TenantDb, invoice: &AggregateId,
    customer: &AggregateId, at: Timestamp, metadata: &Metadata)
    -> Result<Committed<InvoiceEvent>, …>;
```

**The reconciliation surface Phase 7a asked for, and the reason that phase says
*surface* and not *foreign key*.** Invoices issued before `crm` existed name a
buyer that no record matches. A constraint would have refused every one of them
at once; this lets somebody work through the backlog an invoice at a time.

It writes the **reference** and never the printed name. What the document says
about its buyer was frozen at issue and stays frozen (L5) — a reconciliation
does not get to restate a document somebody has already filed a return against.
That separation is the whole argument in the [`crm`](./crm.md) chapter, and this
is the command that depends on it being true.

The customer is validated against `crm`'s **log**, so a record created a moment
ago can be matched immediately rather than being refused for lagging behind its
own projection.

Matching the same record twice writes nothing. Matching a *different* one is a
correction and does write: a match made to the wrong Ahmed has to be fixable,
and the log keeps both so the correction is visible rather than silent.

The worklist it works from:

```rust
pub async fn unmatched_customers(conn, limit: i64)
    -> Result<Vec<UnmatchedCustomer>, sqlx::Error>;
```

Grouped by the frozen name, largest backlog first, because the job is matching
*people* and forty invoices for one spelling is one decision. A name that
appears with two different VAT numbers comes back as two rows — they are two
buyers who share a spelling, and merging them would hide the exact case the
person is looking for.

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

## Two views over the same numbers

`invoice_status` groups; `invoice_row` correlates. They answer identically and a
test asserts it, because two shapes of one rule is exactly how a rule comes to
be written twice and drift.

They exist separately because `GROUP BY i.id` cannot preserve `issued_on DESC`
order, so a paged read through the grouped view has to aggregate **every**
invoice in the tenant, sort the lot, and throw away all but twenty rows.
Measured on 200,000 invoices and 400,000 payments:

| | grouped | correlated |
|---|---|---|
| one page of invoices | 410 ms, 443k buffers | **0.3 ms, 118 buffers** |
| the receivables report | **292 ms** | 760 ms |
| the overpayment check | **273 ms** | 945 ms |

Neither is better in general, which is why there are two and not a replacement.
One view per access pattern, each fast at the thing it is for.

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
| `POST` | `/v1/sales/invoices/{invoice}/refunds` | PostEntries |
| `POST` | `/v1/sales/invoices/{invoice}/customer` | ManageTenant |
| `GET` | `/v1/sales/unmatched-customers` | Read |
| `POST` | `/v1/sales/invoices/{invoice}/credit-note` | PostEntries |
| `GET` | `/v1/sales/receivables` | Read |
| `GET` `PUT` | `/v1/sales/posting-accounts` | Read / ManageAccounts |
| `GET` `PUT` | `/v1/sales/document-limit` | ManageTenant |

## What is deliberately absent

Customers as records, quantities and unit prices, and partial credit notes. Every
one of them is additive.
