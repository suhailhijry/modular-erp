# The party model — design

**Phase 20a.** The first box of Phase 20, and the one that gates every other box
in it. §49 of `docs/IMPLEMENTATION.md` is the assessment this follows from and is
not repeated here.

## The problem this is the answer to

Phase 20a was written as *"roles and relationships between parties, so one person
is the **owner** of unit A, the **tenant** of unit B and the **guarantor** on unit
C at once."* That phrasing implies a relationship aggregate. Three questions
established that it should not be built.

**Every one of those three roles is already carried by a document.** A lease names
its tenant and its guarantor. An ownership record names its owner. Storing the
role separately would be a second source of truth for something the lease already
states, and the two would eventually disagree.

What *is* missing is smaller and harder to see. `crm::Customer` carries
`TaxRegistration { vat_number: String, scheme, identifier }`
(`modules/crm/src/customer.rs:118`) and nothing else that identifies a person.
`vat_number` is required, not optional. `tax_sa::IdScheme`
(`modules/tax_sa/src/taxpayer.rs:83`) is `Crn | Mom | Mls | Sag | Number700 |
Other` — every variant a **business** register, because ZATCA's `schemeID` list
was written for the seller side.

And the refusal is harder than it first looks. `Details::check`
(`modules/crm/src/commands.rs:117`) rejects `CrmError::PersonWithVatNumber` — a
`CustomerKind::Person` may not hold a `TaxRegistration` **at all**, because one
requires a VAT number and a natural person does not have a VAT registration. The
same rule is enforced on replay by the `customer_person_has_no_vat_number`
constraint in `modules/crm/schema/install.sql`. Since `id_scheme` and
`identifier` are only ever populated from `tax`, they are **unreachable for a
person**.

So **a residential tenant — a natural person with an iqama and no VAT number —
cannot be identified on a customer record at all**, and there is no workaround:
inventing a VAT number is refused outright, and the only way through today is to
misrepresent the person as a company. Ejar, the mandatory rental-contract
registration, compels that identifier. That is the gate on the vertical.

Note that both rules are *correct* and stay: a person genuinely has no VAT
registration, and the standard-versus-simplified decision depends on it. The gap
is that identity was only ever modelled as a property of a tax registration.

## 1 · The rule: relationships are document-carried

A role is never stored. "Owner of unit A, tenant of unit B, guarantor on unit C"
is answered by asking **which documents name this party**.

The consequence is worth writing down before it surprises somebody: a view of
everything about one party spans leases, invoices, payments and conversations —
four projection groups — and **L3 forbids reading across them**. That view is a
log-subscribing module maintaining its own group on one checkpoint, which is
exactly the argument `modules/reports/src/lib.rs` already makes, including its
honest cost: *"It keeps its own copies of what it needs."*

**This part is prose, not a test.** There is no guard that meaningfully bites on
"nobody stored a role", and inventing one that passes vacuously would be worse
than the sentence.

## 2 · A `crm::Customer` is a party, not a buyer

No code change — a decision recorded, and the naming stays as it is.

An owner, a tenant and a guarantor are all `crm::Customer`.
`crm::accepts_documents` keeps precisely its current meaning — *may we raise a
document against this party* — and `sales` keeps calling it unchanged
(`modules/sales/src/commands.rs:302`). An owner simply never has a sales invoice.

What that decision buys with no work, and what a separate `property::Owner` would
have had to rebuild:

| Reused | Where |
|---|---|
| Contact details, archive and restore | `CustomerEvent::{Registered, Amended, Archived, Restored}` |
| Lookup by phone | `crm::customer_by_phone` — so an owner's SMS lands on their own thread |
| Custom fields, typed and erasable | `modules/crm/src/fields.rs` |
| Messaging audiences | `messaging::Topic::Customer`, which already exists |
| Conversation threads | `modules/conversations`, on that same topic |

Three of those five are the closed enums Phase 20b already has to open. Building
a second party type would mean opening them twice.

**The cost, stated plainly:** the word *customer* now covers somebody the business
pays. That is a naming compromise, taken deliberately over a rename of `Customer`
to `Party` across `sales`, `booking`, `messaging`, `conversations`, `pos` and
every route and wire type.

## 3 · `Identification`, separate from `TaxRegistration`

A new optional field on the `Customer` aggregate:

```rust
/// Which document proves who this party is.
///
/// **Not a tax registration.** A residential tenant has an iqama and no VAT
/// number, and `TaxRegistration` cannot express that because `vat_number` is
/// required — correctly, since a tax registration without one is not one.
pub struct Identification {
    pub scheme: IdScheme,
    pub number: String,
}

pub enum IdScheme {
    /// A Saudi national.
    NationalId,
    /// A resident.
    Iqama,
    Passport,
    GccId,
    /// Where the party is the company itself.
    Crn,
}
```

Carried on `Registered` and `Amended` — one event for the whole record, following
the reasoning already written at `modules/crm/src/customer.rs:150`: *"One event
and not six, because a form saves once and a customer record has no field whose
change means something on its own."*

Projected as two nullable columns **named `identification_scheme` and
`identification_number`**, and returned on the customer read models.

**Not `id_scheme`/`identifier`**: those column names are already taken on the
`customer` table by `TaxRegistration`'s ZATCA `schemeID` pair
(`modules/crm/schema/install.sql`), which is a different thing that happens to
sound identical.

A company party may carry `Crn` in **both** `Identification` and
`TaxRegistration.scheme`, and the two are **not cross-validated**. They are
answering different questions of the same party and are allowed to differ — a
company identified by its commercial registration may be taxed under a group
registration held by its parent. Nothing reads one to check the other.

### Why this never has to agree with ZATCA's list

Checked rather than assumed. `modules/tax_sa/src/zatca/ubl.rs:420` builds
`cac:AccountingCustomerParty` from an address, `cbc:CompanyID` (the VAT number)
and `cbc:RegistrationName`. **There is no `cac:PartyIdentification` on the buyer
at all** — the one in that file, at line 392, is the seller's.

Residential rent to a natural person is a **simplified** (B2C) invoice, which
requires no buyer identification. The iqama is required by **Ejar, on the
contract**, which is a different document under different rules.

So `crm::IdScheme` and `tax_sa::IdScheme` answer different questions and are
allowed to differ. The warning on `TaxRegistration.scheme` — *"the list is the
authority's and `tax_sa` owns the enum; duplicating it would give two places to
disagree about what is valid"* — is about ZATCA's list specifically, and this is
not that list. `crm` gains no dependency on `tax_sa`, which would be the wrong
direction anyway: recording a passport should not require a Saudi tax module.

### Why it lives in the event log

Decided against the §41 treatment (shape replayed, values in a table where a
delete is a delete). A lease is a legal contract subject to a retention
obligation, and Ejar compels the identifier — a retention obligation is a lawful
basis that survives an erasure request under the PDPL. It is also read at
decision time by the command in §4, and a command reading a table that is not
derived from the log is what L7 exists to keep rare.

The asymmetry with custom fields is deliberate and is the point: a wellness
centre's blood-type field is erasable; the identity on a signed tenancy is not.

## 4 · Where an identity is required — not in `crm`

`crm` accepts a customer with or without `Identification`. A walk-in booking
client has none (`booking::Customer.id` is `Option<AggregateId>` for exactly this
reason), and refusing in `crm` would break booking.

**`property`'s `sign_lease` refuses a party with no `Identification`**, naming the
party in the refusal. L6 — refuse, do not degrade — at the place that knows the
requirement. This is the same argument `ledger::post_entry_in` makes for branch
validation: every path arrives there, so the check is written once rather than in
each caller.

That refusal lands with Phase 20e, not with this spec. What 20a owes it is the
field to check.

## 5 · What this deliberately does not do

- **No relationship aggregate, no role table, no household or group.** §1.
- **No rename** of `Customer` to `Party`. §2.
- **No erasure path for `Identification`.** It is retained under the lease's
  retention obligation, and a prospect who never signed a lease was never required
  to give one — their record is archived by the existing path and their custom
  field values erased by the existing `fields::forget`.
- **One identification, not many.** A person may hold both a national ID and a
  passport; Ejar needs one. A second is a list when somebody has a reason for it.

## 6 · Testing, and how each guard is falsified

Every guard below must be proved by reverting the fix, watching the test fail,
restoring, and watching it pass.

| Test | Falsified by |
|---|---|
| A customer registers with an `Iqama` and **no VAT number**, and reads back with both | Moving `Identification` inside `TaxRegistration` — the case becomes unrepresentable |
| `Identification` round-trips through `Registered` and `Amended`, and an amend that omits it clears it | Dropping the field from the `Amended` arm — the old value survives an edit that removed it |
| `Identification` survives a projection rebuild | Storing it outside the log — a shadow replay then produces an empty column |
| One customer is the party on two documents in different roles, and neither blocks the other | — the rule in §1, made concrete rather than asserted |
| **An existing `Registered` with no `identification` key still decodes** — every customer already in every tenant's log | Removing `#[serde(default)]` from the field; every historical event stops decoding and the module cannot load a single existing customer |
| A `Registered` carrying an identification serialises and decodes back unchanged | Adding the field without `skip_serializing_if`, which writes `"identification": null` into every event for every customer that has none |

The `sign_lease` refusal is tested in 20e, where the command exists.

## Size

One struct, one enum, two event arms, two projection columns, a route field and a
golden file. Small — the design work was establishing that nothing else was
needed.

## What lands where

- **20a (this spec):** `Identification` on `crm::Customer`, the rule in §1
  recorded in `docs/ARCHITECTURE.md`, the decision in §2 recorded in
  `docs/IMPLEMENTATION.md` §49.
- **20e:** `sign_lease` refusing an unidentified party; the Ejar submission that
  consumes the identifier.
