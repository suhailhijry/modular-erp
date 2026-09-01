# crm

Customers as records: who a document was for. The smallest module in the build,
and the one everything above it points at.

**Depends on:** the core, and nothing else.
**Depended on by:** `sales`, `booking`.

## Why it depends on nothing

A customer is not an accounting document, so there is no `ledger` dependency and
nothing to post. That is deliberate and load-bearing: `sales` and `booking` can
both name a customer without either depending on the other, because the thing
they share sits underneath both.

It is also why `setup()` has **no `requires`**. A customer list is useful on its
own — a business can keep one before it issues a single invoice, and a tenant
who only ever wants contacts should not be made to enable accounting for them.

## The gap it was built to close

Receivables exposed it. An invoice freezes the buyer's name (L5), so two
spellings were two rows and nothing could answer *everything for this customer*.

The fix is **the reference and the frozen copy, both** — never either. The
reference is what makes the question answerable; the copy is what the law
requires the document to say, and it does not move when the record does.

## The files

| File | What is in it |
|---|---|
| [`customer.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/crm/src/customer.rs) | `Customer`, `CustomerEvent`, `CustomerKind`, `Contact`, `Address`, `TaxRegistration` |
| [`commands.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/crm/src/commands.rs) | `Details`, the four commands, `accepts_documents` |
| [`projections.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/crm/src/projections.rs) | The `Crm` group and the customer list |
| [`http.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/crm/src/http.rs) | Six routes |

## The record

```rust
pub struct Details {
    pub name:       String,
    pub name_latin: Option<String>,
    pub kind:       CustomerKind,   // Person | Company
    pub contact:    Contact,        // phone and/or email
    pub address:    Option<Address>,
    pub tax:        Option<TaxRegistration>,
}

impl Details {
    pub fn check(&self) -> Result<(), CrmError>;
}
```

`check` is everything checkable without the stored state, and it is separate
from the commands so both apply exactly the same rules — and so a caller can
validate a form before opening a transaction.

Three rules it enforces:

- **A name**, at most 200 characters, which is what ZATCA accepts in the buyer
  name field. A name that can be stored is a name that can be invoiced.
- **A way to reach them** — phone or email. Enforced here *and* by a `CHECK`
  constraint, which is not belt and braces: the table is rebuilt from the log,
  and a rule that lives only in the command is one an older event can walk
  straight past.
- **A person does not hold a VAT registration.** Allowing both would make the
  standard-against-simplified decision ambiguous at the moment it is taken.

A VAT number is fifteen digits beginning and ending with `3`, checked here
rather than at clearance for the reason `tax_sa::Registration` checks the
seller's: by the time ZATCA says no, the invoice exists and cannot be given to
the buyer.

## Commands

```rust
pub async fn register_customer(db, id, details, registered_on, metadata) -> Outcome;
pub async fn amend_customer(db, id, details, metadata)                    -> Outcome;
pub async fn archive_customer(db, id, reason, metadata)                   -> Outcome;
pub async fn restore_customer(db, id, metadata)                           -> Outcome;
```

These use `TenantDb::execute` and `sales` does not, and the reason is worth
knowing: a customer touches one aggregate and nothing else. `sales` has to open
its own transaction because an invoice and its journal entry commit together;
there is no second thing here.

**Archiving is not deletion.** The customer is on documents that have been
issued, filed against and possibly cleared with ZATCA. Archiving takes them out
of the lists a clerk works from and leaves every one of those documents intact.

## The seam other modules use

```rust
pub async fn accepts_documents(
    conn: &mut sqlx::PgConnection,
    id:   &AggregateId,
) -> Result<bool, erp_eventlog::LoadError>;
```

True when the customer exists and is not archived.

**It reads the log, not `proj_crm`.** That is the whole point of it. `crm` is a
different projection group on its own checkpoint, so a customer created a moment
ago is not in the table yet, and validating against the table would refuse an
invoice to somebody the caller has just created.

Reading crm's *write* side is not the cross-group read L3 forbids. That law is
about projection groups, and this touches none: it is the event log, which every
module shares by design.

Both callers do the same thing with it:

```rust
// modules/sales/src/commands.rs, inside the issuing transaction
if let Some(customer) = &draft.customer.id
    && !crm::accepts_documents(&mut *conn, customer).await.map_err(ExecuteError::Load)?
{
    return Err(ExecuteError::Rejected(SalesError::NoSuchCustomer(customer.to_string())));
}
```

## Read models

One table, and it is the whole group. Small on purpose: a customer is referenced
by `sales` and `booking`, and a group is the unit of consistency (L3), so
keeping this one narrow means the thing everything else points at is never
waiting on a projection that has nothing to do with it.

```rust
pub async fn customers(conn, include_archived: bool, limit: i64, after: Option<&Cursor>)
    -> Result<Page<CustomerSummary>, sqlx::Error>;

pub async fn customer(conn, id: &str) -> Result<Option<CustomerDetail>, sqlx::Error>;
```

Keyset on `(registered_on, id)` descending, the same shape as `sales::invoices`.

`include_archived` is a parameter and not two functions, because the caller that
wants both is a search box and the caller that wants one is a list, and they are
otherwise identical.

**The VAT number index is not unique.** Two customers can share one: a group
with several trading entities, or the same company entered twice by two
branches. That second case is a reconciliation the tenant has to make, and
refusing the insert would stop them recording a real invoice until they had made
it.

## What this module deliberately does not do

**A customer's whole picture** — their invoices, bookings and packages — spans
several projection groups, and L3 forbids reading across them. That composition
belongs in a module that declares all of them, the way `tax_sa` nets sales
against purchases. Doing it here would be the exact cross-group read the law
exists to prevent.

## Routes

See [The HTTP API](./http.md#customers).
