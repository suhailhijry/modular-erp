# branches

Places the business trades from, and the dimension every document is reported
by. A leaf, like `crm`, and for the same reason.

**Depends on:** the core, and nothing else.
**Depended on by:** `ledger` (which checks it once for all four posting
modules), and `booking`.

## Why it depends on nothing

Because everything else depends on *it*. `sales`, `pos` and `booking` all name a
place on a document, and `ledger` carries it through to the posting so a trial
balance can be read per branch. Put the same thing inside any one of them and
the other three would have to depend on that one.

It is also why `setup()` has **no `requires`**. A branch is a place, and a place
needs no ledger, no customers and no calendar to exist.

## Why it is an aggregate and not a settings row

A dimension that can be edited in place rewrites history. A trial balance for
Olaya run in March and again in June would differ, with nothing in the system
able to say why — and nothing anybody could point at to explain the difference
to an auditor.

So renaming a branch is an event, moving it is an event, and closing it is an
event that keeps everything the branch ever issued. It is the same argument
`crm` makes about a customer, one layer along, and the same one `sales` makes
about the address frozen onto a tax invoice.

## The files

| File | What is in it |
|---|---|
| [`branch.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/branches/src/branch.rs) | `Branch`, `BranchEvent`, `Details`, `Address`, `BadBranch` |
| [`commands.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/branches/src/commands.rs) | The four commands and `accepts_documents` |
| [`projections.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/branches/src/projections.rs) | The `Branches` group and the one table |
| [`http.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/branches/src/http.rs) | Six routes |

## The record

```rust
pub struct Details {
    pub name:       String,
    pub name_latin: Option<String>,   // for a document that prints in English
    pub address:    Address,
}

impl Details {
    pub fn check(&self) -> Result<(), BadBranch>;
}
```

`check` enforces a name, a street and a city, and **a two-letter country code**.
The country is checked here rather than at clearance for the reason
`crm` checks a VAT number here rather than there: ZATCA prints it on every
document this branch issues, and by the time ZATCA says no the document exists.

`Address` is the **fourth copy** of a struct `crm`, `sales` and `tax_sa` each
also define. That is real duplication and it is recorded rather than hidden: the
three are *event schemas*, frozen by L5 at different moments and for different
reasons — `sales` freezes the buyer's, `crm` holds the current one, `tax_sa`
holds the taxpayer's registered one. They are equal today by coincidence, and
ZATCA adding a field to one is what would separate them again.

## Commands

```rust
pub async fn open_branch(db, id, details, opened_on, metadata)  -> Outcome;
pub async fn amend_branch(db, id, details, metadata)            -> Outcome;
pub async fn close_branch(db, id, why, at, metadata)            -> Outcome;
pub async fn reopen_branch(db, id, at, metadata)                -> Outcome;
```

Four commands, one aggregate, no posting. `TenantDb::execute` is enough here for
the reason it is enough in `crm`: a branch touches one aggregate and nothing
else, so there is no second write to hold a transaction open for.

**There is no delete.** A branch that vanished would take the meaning of its own
history with it — a year of documents pointing at an id nothing answers for.

## The seam every posting module uses

```rust
pub async fn accepts_documents(
    conn: &mut sqlx::PgConnection,
    id:   &AggregateId,
) -> Result<bool, erp_eventlog::LoadError>;
```

True when the branch exists and has not been closed.

It reads **the log, not `proj_branches`** — the same shape as
`crm::accepts_documents`, and for the same reason: this module is its own
projection group on its own checkpoint, so a branch opened a moment ago is not
in the table yet, and validating against the table would refuse a document to a
place the caller has just created.

### It is called in exactly one place

```rust
// modules/ledger/src/commands.rs, inside post_entry_in
if let Some(branch) = metadata.branch() {
    let id = AggregateId::new(branch).map_err(…)?;
    if !branches::accepts_documents(&mut *conn, &id).await.map_err(ExecuteError::Load)? {
        return Err(ExecuteError::Rejected(LedgerError::NoSuchBranch(branch.to_owned())));
    }
}
```

Every posting in the system funnels through `post_entry_in` — `sales`,
`purchases`, `pos` and `prepaid` all reach the ledger through it. So one check
in one function gives all four the rule, and **not one of those modules holds a
`branch` field**: the dimension travels in `Metadata`, put there by one
extractor reading one header.

The one exception is `booking`, and it is worth knowing why: declaring a
resource posts nothing, so there is no journal entry to carry the check. It
validates against this seam itself, at declare time.

## How a branch reaches an event

`X-Branch` on the request → `Allowed<C>.branch` → `metadata()` folds it in →
every event that request writes carries it. A module that never mentions
branches still gets the dimension on everything it wrote, which is the whole
argument for putting it in the metadata rather than in each event's payload.

## Read models

One table, one group, small on purpose — the thing everything else points at
should never be waiting on a projection that has nothing to do with it.

```rust
pub async fn branches(conn, include_closed: bool, limit: i64, after: Option<&Cursor>)
    -> Result<Page<BranchSummary>, sqlx::Error>;

pub async fn branch(conn, id: &str) -> Result<Option<BranchSummary>, sqlx::Error>;
```

Keyset on `(name, id)` **ascending** — the one list in this codebase that is not
newest-first, because a settings screen and a picker on a document both read
alphabetically.

## What a per-branch report can and cannot claim

**A per-branch trial balance does not have to balance, and this module does not
pretend otherwise.** Debits and credits balance per *currency*, which is what
`ledger` asserts. Move cash from one branch to another and you debit one and
credit the other: each side is out by the transfer until inter-branch clearing
accounts exist.

What Phase 16 delivers is that every branch can be *reported* separately and
that the branches sum to the whole. Not that each is a balanced set of books.

## What is deliberately not here

**Opening hours.** The plan asked for them and nothing would read them.
`booking` already keeps availability per resource, which is finer than a branch
and is what a diary actually needs; branch hours are something the booking site
would *display*, and that site is a separate React project reading this API.
A rule nobody applies is a rule that is wrong by the time somebody does.

**A branch on a person.** Employees are scoped per branch and that needs `hr`,
which is Phase 9. Recorded as blocked rather than half-built.

**Per-branch ZATCA EGS units.** A group with several branches may register each
as its own unit with its own certificate and its own counter. `tax_sa` holds one
today. Real, known, and not this phase.

## Routes

See [The HTTP API](./http.md#branches).
