# purchases

Supplier bills, and the input-tax side of a Saudi VAT return.

**Depends on:** `ledger`, plus the core.
**Depended on by:** `tax_sa`.

## What this module was for

The third module, which is a different job from the second. `sales` answered "how
do two modules meet". This one answered "was that answer a general one, or did it
just happen to fit sales".

**All of the mechanism generalised.** An aggregate, events at version 1, a
projection group nobody else reads, `ModuleSetup` describing what to install, a
`requires` on the ledger, a rejection-to-status mapping, and a command that
writes its document and its journal entry in one transaction through
`ledger::post_entry_in`. None of it needed changing, and the closed-period check
arrived for free: this module never mentions a fiscal period and cannot post into
one, because every posting goes through the same seam.

Exactly one thing moved: `VatCategory`, from `sales` to `ledger`. Two sibling
modules must not depend on each other, so what they share has to live in the one
they both stand on. That is a two-module rule only a third module could have
tested.

## What is genuinely different

**Sales computes tax; purchases records it.**

Input VAT is reclaimed against the supplier's tax invoice, so the figure in the
books has to be the figure on the document you hold. A recomputation landing a
halala away produces a reclaim that does not match its own evidence, and the
evidence is what an inspector asks to see.

So there is no `vat::total` here and no rounding. The module validates that the
stated tax is *possible* and then stores what it was told.

Two smaller consequences fall out of the same fact. There is **no gapless
numbering**, because we did not issue this document: the supplier's own number is
recorded, and a duplicate of it against the same supplier is refused by a unique
index, since recording one bill twice is a duplicate reclaim. And **exempt input
tax never reaches `1200 Input VAT`**, because it is irrecoverable, so it is a cost
of the purchase and rides on the line's own account.

## The files

| File | What is in it |
|---|---|
| [`bill.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/purchases/src/bill.rs) | `Bill`, `BillEvent`, `BillLine`, `Supplier` |
| [`commands.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/purchases/src/commands.rs) | `record_bill`, `pay_bill` |
| [`posting.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/purchases/src/posting.rs) | `PostingAccounts`, `entry_for_bill`, `entry_for_payment` |
| [`projections.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/purchases/src/projections.rs) | The `Purchases` group, bills, `input_tax` |
| [`http.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/purchases/src/http.rs) | The routes |

## The bill

```rust
pub struct Supplier { … }
impl Supplier {
    pub fn new(name: impl Into<String>) -> Self;
    pub fn with_vat_number(self, number: impl Into<String>) -> Self;
}

pub struct BillLine {
    pub description: String,
    pub account: AggregateId,      // per line: one bill covers rent and stationery
    pub net: Money,                // excluding tax
    pub category: VatCategory,
    pub rate_bp: i32,              // the rate the supplier charged
    pub tax: Money,                // the tax the supplier charged
}

pub enum BillEvent { Received { … }, Paid { … } }
impl BillEvent {
    pub const NAMES: [&'static str; 2] = ["purchases.bill.received", "purchases.bill.paid"];
}

pub struct Bill { … }
impl Bill {
    pub fn outstanding(&self) -> Option<Money>;
    pub fn has_payment(&self, reference: &str) -> bool;
}
```

`Supplier` is a snapshot for the same reason `sales::Customer` is one: a tax
invoice is a legal document, and a supplier changing their registered name next
year must not rewrite the copy in the filing cabinet.

`rate_bp` is recorded, never resolved. It is on their document, and if it
disagrees with today's statutory rate that is a thing worth being able to see.

**Why `tax` is a field and not a calculation** is the whole difference between a
bill and an invoice, and it is a domain rule, not a shortcut. `record_bill`
checks the stated tax is *plausible*, and never that it is exact:

- never negative,
- zero on anything not standard-rated,
- never claimed without a supplier VAT number to evidence it.

## Commands

```rust
pub struct Draft {
    pub supplier: Supplier,
    pub supplier_reference: String,   // their invoice number. What a reclaim is evidenced by
    pub billed_on: Timestamp,         // the tax point, from their document
    pub due_on: Option<Timestamp>,
    pub currency: CurrencyCode,
    pub lines: Vec<BillLine>,
    pub note: String,
}

pub struct Payment {
    pub reference: String,            // our own
    pub amount: Money,
    pub paid_on: Timestamp,
    pub from: AggregateId,            // the cash or bank account it left
}

pub async fn record_bill(db: &TenantDb, id: &AggregateId, draft: &Draft,
    metadata: &Metadata) -> Result<Committed<BillEvent>, CommandError<PurchaseError>>;

pub async fn pay_bill(db: &TenantDb, bill: &AggregateId, payment: &Payment,
    metadata: &Metadata) -> Result<Committed<BillEvent>, CommandError<PurchaseError>>;
```

`id` is **our** key for the bill. The supplier's own number is
`supplier_reference` and it goes on the document. It cannot be the identity,
because two suppliers can both call something `INV-001`.

Recording the same `id` twice is a no-op, and the stored bill wins. Recording the
same payment `reference` twice is a no-op.

The internal `Totals` does summation only. Nothing here rounds, because nothing
here decides an amount, which is the whole difference between this and
`sales::vat::total`.

## Posting

```rust
pub struct PostingAccounts { … }
impl PostingAccounts {
    pub const KEY: &'static str = "purchases.posting_accounts";
    pub fn conventional() -> Self;
    pub async fn resolve(conn: &mut PgConnection) -> Result<Self, ConfigError>;
}

pub fn entry_for_bill(lines: &[BillLine], gross: Money, accounts: &PostingAccounts)
    -> Result<BalancedLines, Unbalanced>;
pub fn entry_for_payment(amount: Money, from: &AggregateId, accounts: &PostingAccounts)
    -> Result<BalancedLines, Unbalanced>;
```

Mirror image of a sale. Where an invoice debits what a customer owes and credits
revenue, a bill debits what was bought and credits what is owed:

```text
Dr  each line's expense or asset account   net
Dr  input VAT                              tax          (reclaimable only)
    Cr  accounts payable                        gross
```

The line's own expense account is on the line, because one bill routinely covers
rent and stationery. The two on `PostingAccounts` are the ones every bill
touches.

`entry_for_bill` takes the lines as the supplier stated them, and the arithmetic
is only summation, so it cannot disagree with the document.

### Why exempt tax does not go to input VAT

Input tax on an exempt supply is **not reclaimable**. It is a cost of the
purchase, not a debt ZATCA owes back. Putting it in `1200 Input VAT` would claim
it, and the reclaim would be disallowed. It goes to the line's own account
instead, which is where an irrecoverable cost belongs.

In practice a supplier charges no tax on an exempt supply, so this arm is rarely
reached. "Rarely" is not "never", and a rule that only holds for the common case
is the one that produces an unexplainable balance.

## Read models

```rust
pub struct Purchases;
impl ProjectionGroup for Purchases {
    const NAME: &'static str = "purchases";
    const SCHEMA: &'static str = "proj_purchases";
}

pub struct Bills;
pub fn projections() -> Vec<Arc<dyn Projection<Group = Purchases>>>;

pub struct BillSummary { … }
pub struct BillLineRow { … }
pub struct PaymentRow { … }
pub struct BillDetail { … }

pub async fn bills(conn: &mut PgConnection, limit: i64, after: Option<&Cursor>)
    -> Result<Page<BillSummary>, sqlx::Error>;
pub async fn bill(conn: &mut PgConnection, id: &str)
    -> Result<Option<BillDetail>, sqlx::Error>;
```

A group of its own, never reading `proj_sales` or `proj_ledger`. The combined VAT
return is composed in the API from each module's own reads, which is where
cross-module composition belongs.

`bills` is keyset on `(billed_on, id)`, the same shape as `sales::invoices` and
for the same reasons.

### Input tax

```rust
pub struct InputBand { … }
pub struct InputTax { … }

pub async fn input_tax(conn: &mut PgConnection, currency: CurrencyCode,
    from: Timestamp, until: Timestamp) -> Result<InputTax, sqlx::Error>;
```

What a business can reclaim for a period. Each bill is reported on **its own tax
point**, the date the supplier stated and not the date it was typed in. The same
rule the output side follows, and for the same reason: a period that has been
declared must not change.

### The health check

```rust
pub struct Overpaid { … }
pub async fn overpaid(conn: &mut PgConnection) -> Result<Vec<Overpaid>, sqlx::Error>;
```

A bill paid more than it was for. Impossible through `pay_bill`, so a row here
means the pipeline is broken. Registered in `bin/worker.rs` as `NoOverpaidBill`.

## Routes

| Method | Path | Capability |
|---|---|---|
| `GET` `POST` | `/v1/purchases/bills` | Read / PostEntries |
| `GET` | `/v1/purchases/bills/{bill}` | Read |
| `POST` | `/v1/purchases/bills/{bill}/payments` | PostEntries |

The three modules' `http.rs` files have a set of wire shapes, a module gate and a
rejection-to-status mapping in common. The only part that resists being shared is
the mapping, because which rejection is a 409 and which is a 422 is exactly the
part a shared helper could not decide.
