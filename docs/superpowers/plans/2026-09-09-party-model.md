# Party model Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `crm::Customer` an `Identification` — a national ID, iqama, passport, GCC id or CRN — so that a natural person can be identified without a tax registration, which is the gate on Phase 20's property vertical.

**Architecture:** A new optional `Identification { scheme, number }` on the `Customer` aggregate, carried on `Registered` and `Amended` alongside the existing `tax` field, projected into two new nullable columns, and exposed on the HTTP surface. Deliberately **separate** from `TaxRegistration`, whose `vat_number` is required and whose presence is refused outright for a `CustomerKind::Person`. No relationship model, no role table, no rename — see the spec for why each was ruled out.

**Tech Stack:** Rust, axum, sqlx (offline mode), Postgres 18, `erp-eventlog` (event sourcing), `erp-projection`, `erp-i18n` (Arabic + English), `cargo-nextest`, `utoipa` (OpenAPI).

**Spec:** `docs/superpowers/specs/2026-09-09-party-model-design.md`

## Global Constraints

- **Do not commit.** This project's convention is to leave all work in the working tree for review. Every task below ends with a verification step, not a commit.
- **Every guard test must be falsified**: revert the fix, watch the test fail, restore it, watch it pass. A test that has never been seen to fail is not a guard.
- **`clippy -- -D warnings`** is enforced. Watch for `too_many_lines` (limit 100 — use `#[expect(clippy::too_many_lines, reason = "…")]`), `doc_markdown` (backtick proper nouns), and no `expect_used`/`unwrap_used` outside tests.
- **Every user-facing message needs Arabic and English.** Neither is a translation of a compiled string (D12). A missing translation fails the build.
- **Backward compatibility is mandatory**: every `Registered` and `Amended` event already in every tenant's log has no `identification` key. The field must be `#[serde(default, skip_serializing_if = "Option::is_none")]` or the module cannot load a single existing customer.
- **New projection columns are `identification_scheme` and `identification_number`.** `id_scheme` and `identifier` are already taken on the `customer` table by `TaxRegistration`'s ZATCA `schemeID` pair.
- **A read model is never migrated.** `modules/crm/schema/install.sql` uses `CREATE TABLE IF NOT EXISTS` and a changed read model is answered by dropping the schema and replaying. Do **not** write an `ALTER TABLE` migration for the new columns.
- Iteration environment:
  ```
  export SQLX_OFFLINE=false
  export DATABASE_URL="postgres://postgres:postgres@localhost:55432/erp_typecheck"
  export REDIS_URL=redis://127.0.0.1:56379/
  docker compose -p erp up -d --wait pg-primary redis
  ```
  Finish with `just prepare` (regenerates `.sqlx/`) and `just openapi` (regenerates `docs/openapi.json`).

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `modules/crm/src/customer.rs` | The `Customer` aggregate and its events | Add `Identification`, `IdScheme`; add the field to `Registered` and `Amended` and their `apply` arms |
| `modules/crm/src/commands.rs` | Command input and validation | Add `identification` to `Details`; validate the number in `Details::check`; pass it into both events |
| `modules/crm/src/messages.rs` | Message codes, Arabic and English | One new code for the one new refusal |
| `modules/crm/schema/install.sql` | The `customer` read model | Two nullable columns and a lookup index |
| `modules/crm/src/projections.rs` | Applying events to the read model | Bind the two columns in the `Registered` and `Amended` arms; add to `CustomerSummary` and its `SELECT`s |
| `modules/crm/src/http.rs` | The HTTP surface | Wire type, request and response fields, OpenAPI examples |
| `modules/crm/tests/crm.rs` | Integration tests against a real tenant | The rebuild and persistence tests |

There is **no new file**. The change is small and every piece belongs beside code that already does the same job for `tax`.

---

### Task 1: `Identification` and `IdScheme` on the aggregate

**Files:**
- Modify: `modules/crm/src/customer.rs` (the types, the two event variants, the two `apply` arms)
- Test: `modules/crm/src/customer.rs` (the `#[cfg(test)]` module at the bottom)

**Interfaces:**
- Produces: `crm::Identification { scheme: IdScheme, number: String }`, `crm::IdScheme::{NationalId, Iqama, Passport, GccId, Crn}` with `as_str(&self) -> &'static str`, `ALL: [Self; 5]`, `FromStr`. `CustomerEvent::Registered` and `CustomerEvent::Amended` each gain `identification: Option<Identification>`. `Customer` gains `pub identification: Option<Identification>`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` module at the bottom of `modules/crm/src/customer.rs`:

```rust
#[test]
fn an_event_written_before_identification_existed_still_decodes() {
    // Every `Registered` already in every tenant's log looks like this. If this
    // fails, the module cannot load a single existing customer.
    let stored = serde_json::json!({
        "name": "Fahd",
        "kind": "person",
        "contact": { "phone": "+966500000001" },
        "registered_on": "2026-01-01T00:00:00Z"
    });
    let decoded: CustomerEvent = serde_json::from_value(stored).expect("decodes");
    match decoded {
        CustomerEvent::Registered { identification, .. } => assert!(identification.is_none()),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn a_customer_with_no_identification_writes_no_identification_key() {
    let event = CustomerEvent::Registered {
        name: "Fahd".to_owned(),
        name_latin: None,
        kind: CustomerKind::Person,
        contact: Contact { phone: Some("+966500000001".to_owned()), email: None },
        address: None,
        tax: None,
        identification: None,
        registered_on: "2026-01-01T00:00:00Z".parse().expect("a timestamp"),
    };
    let written = serde_json::to_value(&event).expect("serializes");
    assert!(
        written.get("identification").is_none(),
        "an absent identification must not be written as null: {written}"
    );
}

#[test]
fn an_identification_round_trips() {
    let event = CustomerEvent::Registered {
        name: "Fahd".to_owned(),
        name_latin: None,
        kind: CustomerKind::Person,
        contact: Contact { phone: Some("+966500000001".to_owned()), email: None },
        address: None,
        tax: None,
        identification: Some(Identification {
            scheme: IdScheme::Iqama,
            number: "2312345678".to_owned(),
        }),
        registered_on: "2026-01-01T00:00:00Z".parse().expect("a timestamp"),
    };
    let written = serde_json::to_value(&event).expect("serializes");
    assert_eq!(written["identification"]["scheme"], "iqama");
    let back: CustomerEvent = serde_json::from_value(written).expect("decodes");
    assert_eq!(back, event);
}

#[test]
fn applying_registered_records_the_identification() {
    let mut customer = Customer::default();
    customer.apply(&CustomerEvent::Registered {
        name: "Fahd".to_owned(),
        name_latin: None,
        kind: CustomerKind::Person,
        contact: Contact { phone: Some("+966500000001".to_owned()), email: None },
        address: None,
        tax: None,
        identification: Some(Identification {
            scheme: IdScheme::NationalId,
            number: "1010101010".to_owned(),
        }),
        registered_on: "2026-01-01T00:00:00Z".parse().expect("a timestamp"),
    });
    assert_eq!(
        customer.identification.as_ref().map(|i| i.scheme),
        Some(IdScheme::NationalId)
    );
}

#[test]
fn amending_without_an_identification_clears_it() {
    // An amend carries the whole record, so omitting the field means removing
    // it — the same rule `tax` and `address` already follow.
    let mut customer = Customer::default();
    customer.apply(&CustomerEvent::Registered {
        name: "Fahd".to_owned(),
        name_latin: None,
        kind: CustomerKind::Person,
        contact: Contact { phone: Some("+966500000001".to_owned()), email: None },
        address: None,
        tax: None,
        identification: Some(Identification {
            scheme: IdScheme::Iqama,
            number: "2312345678".to_owned(),
        }),
        registered_on: "2026-01-01T00:00:00Z".parse().expect("a timestamp"),
    });
    customer.apply(&CustomerEvent::Amended {
        name: "Fahd".to_owned(),
        name_latin: None,
        kind: CustomerKind::Person,
        contact: Contact { phone: Some("+966500000001".to_owned()), email: None },
        address: None,
        tax: None,
        identification: None,
    });
    assert!(customer.identification.is_none(), "an omitted field is a removal");
}

#[test]
fn every_scheme_parses_back_from_its_string() {
    for scheme in IdScheme::ALL {
        assert_eq!(scheme.as_str().parse::<IdScheme>().ok(), Some(scheme));
    }
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

```bash
cargo nextest run -p crm --lib
```

Expected: FAIL to **compile** — `cannot find type Identification in this scope`, and `struct CustomerEvent::Registered has no field named identification`. A compile failure is the correct first failure here; there is nothing to name yet.

- [ ] **Step 3: Add the types**

In `modules/crm/src/customer.rs`, beside `TaxRegistration` (around line 118):

```rust
/// Which document proves who this party is.
///
/// **Not a tax registration.** `TaxRegistration` requires a VAT number, and
/// `Details::check` refuses one outright for a `CustomerKind::Person` — so
/// before this existed a natural person had no identifier field at all. A
/// residential tenant has an iqama and no VAT number, and Ejar compels that
/// identifier on the contract.
///
/// A company may carry `Crn` here *and* a `CRN` scheme on its
/// [`TaxRegistration`]. The two are **not cross-validated**: a company
/// identified by its own commercial registration may be taxed under a group
/// registration held by its parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identification {
    pub scheme: IdScheme,
    /// As it appears on the document. Not normalised: a leading zero is part of
    /// the number on some registers, and stripping it would make two different
    /// people the same one.
    pub number: String,
}

/// Which register a party is identified in.
///
/// **Deliberately not `tax_sa::IdScheme`**, which is ZATCA's `schemeID` list and
/// carries only business registers. This list identifies a *person* as well, is
/// consumed by Ejar rather than by ZATCA, and `crm` gaining a dependency on a
/// country module to record a passport would be the wrong direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdScheme {
    /// A Saudi national.
    NationalId,
    /// A resident.
    Iqama,
    Passport,
    /// Identified in another Gulf state.
    GccId,
    /// Where the party is the company itself.
    Crn,
}

impl IdScheme {
    pub const ALL: [Self; 5] = [
        Self::NationalId,
        Self::Iqama,
        Self::Passport,
        Self::GccId,
        Self::Crn,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NationalId => "national_id",
            Self::Iqama => "iqama",
            Self::Passport => "passport",
            Self::GccId => "gcc_id",
            Self::Crn => "crn",
        }
    }
}

impl std::fmt::Display for IdScheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not an identification scheme")]
pub struct UnknownIdScheme(pub String);

impl std::str::FromStr for IdScheme {
    type Err = UnknownIdScheme;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|scheme| scheme.as_str() == s)
            .ok_or_else(|| UnknownIdScheme(s.to_owned()))
    }
}
```

- [ ] **Step 4: Add the field to both event variants**

In `CustomerEvent::Registered`, immediately after the `tax` field:

```rust
        /// **Absent on every event written before this field existed**, which
        /// is why it defaults. Omitting it on an `Amended` removes it, the same
        /// rule `tax` and `address` follow.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identification: Option<Identification>,
```

Add the identical field to `CustomerEvent::Amended`, after its `tax` field.

- [ ] **Step 5: Add the field to the aggregate and both `apply` arms**

Add to `pub struct Customer`:

```rust
    pub identification: Option<Identification>,
```

In `Customer::apply`, set `self.identification = identification.clone();` in both the `Registered` and the `Amended` arms, beside where each already sets `self.tax`.

- [ ] **Step 6: Run the tests and confirm they pass**

```bash
cargo nextest run -p crm --lib
```

Expected: PASS, six new tests.

- [ ] **Step 7: Falsify the backward-compatibility guard**

Remove `default` from the `#[serde(...)]` attribute on `Registered.identification`, leaving `#[serde(skip_serializing_if = "Option::is_none")]`. Run:

```bash
cargo nextest run -p crm --lib an_event_written_before_identification_existed_still_decodes
```

Expected: **FAIL** — `missing field \`identification\``. Restore `default`, re-run, expect PASS. If it passes while broken, the test is not a guard and must be rewritten before continuing.

- [ ] **Step 8: Falsify the null-writing guard**

Remove `skip_serializing_if = "Option::is_none"` from the same attribute. Run:

```bash
cargo nextest run -p crm --lib a_customer_with_no_identification_writes_no_identification_key
```

Expected: **FAIL** — the assertion reports `"identification": null` in the written event. Restore it, re-run, expect PASS.

---

### Task 2: Accepting an identification through the command

**Files:**
- Modify: `modules/crm/src/commands.rs` (`Details`, `Details::check`, `register_customer`, `amend_customer`, `CrmError`)
- Modify: `modules/crm/src/messages.rs` (one new code, Arabic and English)
- Test: `modules/crm/src/commands.rs` (the `#[cfg(test)]` module) and `modules/crm/tests/crm.rs`

**Interfaces:**
- Consumes: `Identification`, `IdScheme` from Task 1.
- Produces: `Details.identification: Option<Identification>`; `CrmError::NotAnIdentificationNumber(String)`; both commands write the field into their events.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` module in `modules/crm/src/commands.rs`:

```rust
#[test]
fn a_person_may_be_identified_without_a_vat_number() {
    // The case that was impossible: `TaxRegistration` requires a VAT number and
    // `PersonWithVatNumber` refuses one for a person, so a natural person had
    // no identifier field at all.
    let details = Details {
        name: "Fahd".to_owned(),
        name_latin: None,
        kind: CustomerKind::Person,
        contact: Contact { phone: Some("+966500000001".to_owned()), email: None },
        address: None,
        tax: None,
        identification: Some(Identification {
            scheme: IdScheme::Iqama,
            number: "2312345678".to_owned(),
        }),
    };
    assert!(details.check().is_ok());
}

#[test]
fn a_company_may_hold_both_a_vat_number_and_a_commercial_registration() {
    let details = Details {
        name: "Najd Supplies".to_owned(),
        name_latin: None,
        kind: CustomerKind::Company,
        contact: Contact { phone: Some("+966500000002".to_owned()), email: None },
        address: None,
        tax: Some(TaxRegistration {
            vat_number: "310000000000003".to_owned(),
            scheme: None,
            identifier: None,
        }),
        identification: Some(Identification {
            scheme: IdScheme::Crn,
            number: "1010101010".to_owned(),
        }),
    };
    assert!(details.check().is_ok(), "the two are not cross-validated");
}

#[test]
fn an_identification_number_may_not_be_blank() {
    let details = Details {
        name: "Fahd".to_owned(),
        name_latin: None,
        kind: CustomerKind::Person,
        contact: Contact { phone: Some("+966500000001".to_owned()), email: None },
        address: None,
        tax: None,
        identification: Some(Identification {
            scheme: IdScheme::Iqama,
            number: "   ".to_owned(),
        }),
    };
    assert!(matches!(
        details.check(),
        Err(CrmError::NotAnIdentificationNumber(_))
    ));
}

#[test]
fn an_identification_number_may_not_be_longer_than_the_column() {
    let details = Details {
        name: "Fahd".to_owned(),
        name_latin: None,
        kind: CustomerKind::Person,
        contact: Contact { phone: Some("+966500000001".to_owned()), email: None },
        address: None,
        tax: None,
        identification: Some(Identification {
            scheme: IdScheme::Passport,
            number: "X".repeat(MAX_ID_NUMBER + 1),
        }),
    };
    assert!(matches!(
        details.check(),
        Err(CrmError::NotAnIdentificationNumber(_))
    ));
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

```bash
cargo nextest run -p crm --lib
```

Expected: FAIL to compile — `struct Details has no field named identification`, `no variant NotAnIdentificationNumber`, `cannot find value MAX_ID_NUMBER`.

- [ ] **Step 3: Add the error and its messages**

In `modules/crm/src/commands.rs`, add to `CrmError` after `PersonWithVatNumber`:

```rust
    #[error("{0} is not an identification number")]
    NotAnIdentificationNumber(String),
```

Add its `Localize` arm beside the others:

```rust
            Self::NotAnIdentificationNumber(number) => {
                Message::new(messages::NOT_AN_IDENTIFICATION_NUMBER)
                    .with("number", MessageArg::text(number.clone()))
            }
```

In `modules/crm/src/messages.rs`, add the code beside `PERSON_WITH_VAT_NUMBER`:

```rust
pub const NOT_AN_IDENTIFICATION_NUMBER: MessageCode =
    MessageCode::new("crm.not_an_identification_number");
```

Then add both translations to the catalogue in the same file, following the exact shape of the `PERSON_WITH_VAT_NUMBER` entry already there:

- English: `"{number} is not an identification number"`
- Arabic: `"{number} ليس رقم هوية"`

- [ ] **Step 4: Add the field and its validation**

Add to `pub struct Details`, after `tax`:

```rust
    pub identification: Option<Identification>,
```

Add the bound near `MAX_NAME` at the top of the file:

```rust
/// Matches the `identification_number` column. A passport number is at most
/// nine characters and a CRN ten; sixty is room for a register nobody has
/// named yet without being room for a paragraph.
pub const MAX_ID_NUMBER: usize = 60;
```

Add to `Details::check`, after the existing `tax` block:

```rust
        if let Some(identification) = &self.identification {
            let number = identification.number.trim();
            if number.is_empty() || number.chars().count() > MAX_ID_NUMBER {
                return Err(CrmError::NotAnIdentificationNumber(
                    identification.number.clone(),
                ));
            }
        }
```

**No check that the scheme suits the kind.** A company can be identified by the CRN of its owner-manager, and a sole trader is both. Refusing combinations here would reject real customers to enforce a rule nobody stated.

- [ ] **Step 5: Pass it into both events**

In `register_customer`, add to the `CustomerEvent::Registered { … }` construction, beside `tax`:

```rust
            identification: details.identification.clone(),
```

Do the same in `amend_customer` for `CustomerEvent::Amended`.

- [ ] **Step 6: Run the tests and confirm they pass**

```bash
cargo nextest run -p crm --lib
```

Expected: PASS. The four new tests plus everything from Task 1.

- [ ] **Step 7: Fix the call sites the new field broke**

`Details` is constructed in several test files across the workspace. Add `identification: None` to each:

```bash
cargo build --workspace --tests 2>&1 | grep -E "^error|missing field" | head -30
```

Expected call sites, from a workspace grep for `crm::Details` and `Details {`:
`modules/crm/tests/crm.rs`, `modules/sales/tests/sales.rs`, `modules/messaging/tests/messaging.rs`, `modules/conversations/tests/conversations.rs`, and `crates/erp-demo`. Add the field to each; do not change what any of those tests assert.

- [ ] **Step 8: Falsify the validation guard**

Change the check to `if number.is_empty() && false {` so it never fires. Run:

```bash
cargo nextest run -p crm --lib an_identification_number_may_not_be_blank
```

Expected: **FAIL** — a blank number is accepted. Restore, re-run, expect PASS.

---

### Task 3: The read model

**Files:**
- Modify: `modules/crm/schema/install.sql` (two columns, one index)
- Modify: `modules/crm/src/projections.rs` (both `apply` arms, `CustomerSummary`, every `SELECT`)
- Test: `modules/crm/tests/crm.rs`

**Interfaces:**
- Consumes: `CustomerEvent::{Registered, Amended}` carrying `identification` from Task 1.
- Produces: columns `identification_scheme` and `identification_number` on `customer`; `CustomerSummary.identification: Option<Identification>`; index `customer_by_identification_idx`.

- [ ] **Step 1: Write the failing tests**

Add to `modules/crm/tests/crm.rs`:

```rust
#[tokio::test]
async fn an_identification_survives_a_rebuild() {
    // The test that carries this change. The identification is in the log, so a
    // replay must reproduce it — unlike a custom field value, which lives
    // outside the log precisely so a delete is a delete.
    let fixture = fixture().await;

    register_customer(
        &fixture.db,
        &code("CUST-1"),
        &Details {
            name: "Fahd".to_owned(),
            name_latin: None,
            kind: CustomerKind::Person,
            contact: Contact { phone: Some("+966500000001".to_owned()), email: None },
            address: None,
            tax: None,
            identification: Some(Identification {
                scheme: IdScheme::Iqama,
                number: "2312345678".to_owned(),
            }),
        },
        "2026-01-01T00:00:00Z".parse().expect("a timestamp"),
        &Metadata::default(),
    )
    .await
    .expect("registers");

    run_to_head::<Crm>(&fixture.db).await.expect("projects");

    let mut conn = fixture.db.read().await.expect("a connection");
    let found = crm::customers(&mut conn, None, 10).await.expect("lists");
    let one = found.first().expect("one customer");
    assert_eq!(
        one.identification.as_ref().map(|i| i.scheme),
        Some(IdScheme::Iqama)
    );
    assert_eq!(
        one.identification.as_ref().map(|i| i.number.as_str()),
        Some("2312345678")
    );
    drop(conn);

    // And it is still there after the whole group is replayed from zero.
    replay_shadow::<Crm>(&fixture.db).await.expect("replays identically");

    fixture.cleanup().await;
}

#[tokio::test]
async fn amending_a_customer_can_remove_their_identification() {
    let fixture = fixture().await;

    register_customer(
        &fixture.db,
        &code("CUST-1"),
        &Details {
            name: "Fahd".to_owned(),
            name_latin: None,
            kind: CustomerKind::Person,
            contact: Contact { phone: Some("+966500000001".to_owned()), email: None },
            address: None,
            tax: None,
            identification: Some(Identification {
                scheme: IdScheme::Iqama,
                number: "2312345678".to_owned(),
            }),
        },
        "2026-01-01T00:00:00Z".parse().expect("a timestamp"),
        &Metadata::default(),
    )
    .await
    .expect("registers");

    amend_customer(
        &fixture.db,
        &code("CUST-1"),
        &Details {
            name: "Fahd".to_owned(),
            name_latin: None,
            kind: CustomerKind::Person,
            contact: Contact { phone: Some("+966500000001".to_owned()), email: None },
            address: None,
            tax: None,
            identification: None,
        },
        &Metadata::default(),
    )
    .await
    .expect("amends");

    run_to_head::<Crm>(&fixture.db).await.expect("projects");

    let mut conn = fixture.db.read().await.expect("a connection");
    let found = crm::customers(&mut conn, None, 10).await.expect("lists");
    assert!(
        found.first().expect("one customer").identification.is_none(),
        "an omitted field is a removal, in the read model as well as the aggregate"
    );
    drop(conn);

    fixture.cleanup().await;
}
```

**Note on `drop(conn)` before `fixture.cleanup()`:** holding a connection across cleanup makes the test hang for the full pool timeout, because cleanup closes the pool and waits for every connection to return. This has already cost a debugging session on this codebase.

- [ ] **Step 2: Run the tests and confirm they fail**

```bash
cargo nextest run -p crm --test crm an_identification_survives_a_rebuild
```

Expected: FAIL to compile — `struct CustomerSummary has no field identification`.

- [ ] **Step 3: Add the columns**

In `modules/crm/schema/install.sql`, inside `CREATE TABLE IF NOT EXISTS customer`, after the `identifier` column and its constraint:

```sql
    -- Which document proves who this party is. **Not the `id_scheme` and
    -- `identifier` above**, which are ZATCA's `schemeID` pair hanging off the
    -- VAT registration and are unreachable for a person, who may not hold one.
    -- This pair is what Ejar needs on a tenancy contract.
    identification_scheme TEXT CHECK (identification_scheme IN
        ('national_id', 'iqama', 'passport', 'gcc_id', 'crn')),
    identification_number TEXT CHECK (length(identification_number) BETWEEN 1 AND 60),

    -- Both or neither. A scheme naming no number identifies nobody, and a
    -- number with no scheme cannot be checked against any register.
    CONSTRAINT customer_identification_is_whole CHECK (
        (identification_scheme IS NULL) = (identification_number IS NULL)
    ),
```

And after the existing indexes:

```sql
-- Finding the party a contract names. **Not unique**: a person may be
-- registered twice by two branches before anybody notices, and refusing the
-- second write would stop a replay rather than surface the duplicate.
CREATE INDEX IF NOT EXISTS customer_by_identification_idx
    ON customer (identification_scheme, identification_number)
    WHERE identification_number IS NOT NULL;
```

- [ ] **Step 4: Bind the columns in both projection arms**

In `modules/crm/src/projections.rs`, in the `Registered` arm: add the two column
names to the `INSERT` column list immediately after `identifier`, append `,$19,$20`
to the end of the `VALUES` list, and insert the two binds immediately after the
`identifier` bind.

**Why that is correct even though it looks wrong:** the `VALUES` list is purely
positional (`$1…$20` in order), so inserting a column mid-list and its bind at the
matching mid-position keeps the two aligned, and only the *count* at the end
grows. Do not renumber the placeholders. Verify by counting: the column list and
the bind chain must both have exactly 20 entries.

```rust
                .bind(identification.as_ref().map(|i| i.scheme.as_str()))
                .bind(identification.as_ref().map(|i| i.number.as_str()))
```

Destructure `identification` in the arm's pattern alongside `tax`. Do the same for the `Amended` arm, following whatever `UPDATE … SET` shape it already uses for `vat_number`.

- [ ] **Step 5: Add it to the read model and every SELECT**

Add to `pub struct CustomerSummary`:

```rust
    pub identification: Option<Identification>,
```

Every `SELECT` that builds a `CustomerSummary` must select the two new columns and assemble the `Option<Identification>`. Find them all:

```bash
grep -n "CustomerSummary" modules/crm/src/projections.rs
```

Assemble with a helper next to the query functions, so the three call sites cannot drift:

```rust
/// Both columns or neither — the table's `customer_identification_is_whole`
/// constraint guarantees it, so a row with one and not the other is a bug
/// somewhere else and is read as absent rather than panicked on.
fn identification(scheme: Option<String>, number: Option<String>) -> Option<Identification> {
    let scheme = scheme?.parse().ok()?;
    Some(Identification { scheme, number: number? })
}
```

- [ ] **Step 6: Run the tests and confirm they pass**

```bash
cargo nextest run -p crm
```

Expected: PASS, including the existing `a_rebuild_reproduces_the_list`.

- [ ] **Step 7: Falsify the rebuild guard**

In the `Registered` arm, change the scheme bind to `.bind(None::<&str>)`. Run:

```bash
cargo nextest run -p crm --test crm an_identification_survives_a_rebuild
```

Expected: **FAIL** — the assertion reports `None` where `Some(Iqama)` was expected. Restore, re-run, expect PASS.

- [ ] **Step 8: Falsify the removal guard**

In the `Amended` arm, drop the two new columns from the `UPDATE … SET` list. Run:

```bash
cargo nextest run -p crm --test crm amending_a_customer_can_remove_their_identification
```

Expected: **FAIL** — the identification survives an amend that removed it. Restore, re-run, expect PASS.

---

### Task 4: The HTTP surface

**Files:**
- Modify: `modules/crm/src/http.rs` (wire type, request body, response body, OpenAPI examples)
- Test: `modules/crm/tests/crm.rs`

**Interfaces:**
- Consumes: `Details.identification` from Task 2, `CustomerSummary.identification` from Task 3.
- Produces: a JSON object `{"scheme": "iqama", "number": "2312345678"}` under the key `identification` on the customer request and response bodies.

- [ ] **Step 1: Write the failing test**

Add to `modules/crm/tests/crm.rs`:

```rust
#[tokio::test]
async fn a_customer_registered_through_the_api_keeps_their_identification() {
    let fixture = fixture().await;

    let body = serde_json::json!({
        "name": "Fahd",
        "kind": "person",
        "contact": { "phone": "+966500000001" },
        "identification": { "scheme": "iqama", "number": "2312345678" }
    });
    let details: crm::http::NewCustomer =
        serde_json::from_value(body).expect("the wire shape decodes");
    let parsed: Details = details.try_into().expect("converts to Details");

    assert_eq!(
        parsed.identification.as_ref().map(|i| i.scheme),
        Some(IdScheme::Iqama)
    );

    fixture.cleanup().await;
}
```

Adjust the type name and conversion to match whatever `http.rs` actually calls its request body — read it first with `grep -n "struct New\|struct .*Customer" modules/crm/src/http.rs`.

- [ ] **Step 2: Run the test and confirm it fails**

```bash
cargo nextest run -p crm --test crm a_customer_registered_through_the_api_keeps_their_identification
```

Expected: FAIL — the `identification` key is ignored, and the assertion reports `None`.

- [ ] **Step 3: Add the wire type**

In `modules/crm/src/http.rs`, beside the existing tax wire type:

```rust
/// Which document proves who this party is.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({"scheme": "iqama", "number": "2312345678"}))]
pub struct IdentificationBody {
    /// `national_id`, `iqama`, `passport`, `gcc_id` or `crn`.
    ///
    /// **Not the tax `scheme` above**, which is ZATCA's `schemeID` and is only
    /// meaningful for a company holding a VAT registration.
    scheme: String,
    number: String,
}
```

**`scheme` is a `String` on the wire, not `crm::IdScheme`.** That is this
module's established pattern, not a shortcut: `CustomerKind` does not derive
`ToSchema` either, and `http.rs` carries `kind: String` and parses it
(`modules/crm/src/http.rs:464`). Keeping domain enums out of the wire layer is
what lets an unrecognised value become a localised 400 naming the field, rather
than a serde error naming a Rust type.

So parse it in the conversion, mapping `UnknownIdScheme` to the same problem
response shape `UnknownKind` already produces:

```rust
let identification = body
    .identification
    .map(|i| {
        let scheme: crm::IdScheme = i.scheme.parse()?;
        Ok::<_, crm::UnknownIdScheme>(crm::Identification { scheme, number: i.number })
    })
    .transpose()?;
```

Add `identification: Option<IdentificationBody>` to the request body and the
response body, and map it in whichever direction each conversion goes.

- [ ] **Step 4: Update the OpenAPI examples**

Find the `#[schema(example = json!({…}))]` on the customer request body and add the key, so the generated document shows it:

```json
"identification": { "scheme": "iqama", "number": "2312345678" }
```

- [ ] **Step 5: Run the test and confirm it passes**

```bash
cargo nextest run -p crm
```

Expected: PASS.

- [ ] **Step 6: Regenerate the committed artefacts**

```bash
just prepare
just openapi
```

`just prepare` regenerates `.sqlx/` for the changed queries; `just openapi` regenerates `docs/openapi.json`. Both files are committed and both have a test that fails on drift, so skipping either fails CI.

- [ ] **Step 7: Confirm the whole module and its dependents are green**

```bash
cargo nextest run -p crm -p sales -p messaging -p conversations
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: PASS and clean. `sales`, `messaging` and `conversations` all construct `Details` and all call into `crm`.

---

### Task 5: The documentation the change owes

**Files:**
- Modify: `docs/IMPLEMENTATION.md` (tick 20a's boxes; record the decision in §49)
- Modify: `docs/ARCHITECTURE.md` (the rule from §1 of the spec)
- Modify: `docs/RUNNING.md` (how to register an identified customer)

**Interfaces:**
- Consumes: everything above. Produces nothing code depends on.

- [ ] **Step 1: Record the rule that has no test**

In `docs/ARCHITECTURE.md`, in the section describing modules and their boundaries, add:

> **A party's role is carried by the document that names it.** A lease names its
> tenant and its guarantor; an ownership record names its owner. There is no role
> table and no relationship aggregate, because storing the role separately would
> be a second source of truth for something the document already states. A view of
> everything about one party therefore spans four projection groups, which L3
> forbids reading across — so it is a log-subscribing module with its own
> checkpoint, exactly as `modules/reports` argues.

- [ ] **Step 2: Tick 20a and record what it turned out to be**

In `docs/IMPLEMENTATION.md`, tick the three boxes under `### 20a · The party model` and add one paragraph beneath them:

> **Built 2026-09-09, and it was not a party model.** The three roles in the
> requirement — owner, tenant, guarantor — are each carried by a document, so no
> relationship aggregate was needed. The single real gap was that identity was
> only ever modelled as a property of a tax registration: `TaxRegistration`
> requires a VAT number and `Details::check` refuses one for a
> `CustomerKind::Person`, so a natural person had no identifier field at all.
> `Identification` is that field. See the spec at
> `docs/superpowers/specs/2026-09-09-party-model-design.md`.

- [ ] **Step 3: Document the route change**

In `docs/RUNNING.md`, in the section that shows registering a customer, add an example carrying an identification:

```bash
curl -X POST "$API/v1/crm/customers/CUST-1" -H "$AUTH" -H 'Content-Type: application/json' -d '{
  "name": "فهد",
  "kind": "person",
  "contact": { "phone": "+966500000001" },
  "identification": { "scheme": "iqama", "number": "2312345678" }
}'
```

- [ ] **Step 4: Run the documentation gates**

```bash
cargo nextest run -p erp-api --test openapi
```

Expected: PASS. `every_code_the_document_cites_exists` scans the generated OpenAPI document for message codes written in backticks; the new `crm.not_an_identification_number` must exist in the catalogue, which Task 2 added.

- [ ] **Step 5: Run the four source-scan meta-tests**

```bash
cargo nextest run -p erp-api --test idempotence
cargo nextest run -p erp-projection --test purity
cargo nextest run -p erp-eventlog --test write_side
cargo nextest run -p erp-control --test pooler
```

Expected: all PASS. These need no database and finish in under a second. `erp-control` is the one most easily forgotten, because this change does not touch that crate — and it has been missed twice on this codebase for exactly that reason.

- [ ] **Step 6: Hand back the full check**

Do **not** commit. Report what changed and hand the user:

```bash
just check
```

---

## What this plan deliberately does not do

- **No `ALTER TABLE` migration.** `modules/crm/schema/install.sql` is a read model rebuilt from the log; a changed read model is answered by dropping the schema and replaying, which `just migrate-fleet refresh crm` already does.
- **No relationship aggregate, role table, or household grouping.** Spec §1 and §5.
- **No rename of `Customer` to `Party`.** Spec §2.
- **No `sign_lease` refusal.** That command does not exist yet; it lands with Phase 20e, and what 20a owes it is the field to check. Spec §4.
- **No erasure path for `Identification`.** It is in the log under the lease's retention obligation. Spec §3.
- **One test from the spec is deferred, deliberately.** Spec §6 lists *"one customer is the party on two documents in different roles, and neither blocks the other"*. It cannot be written here: the only document type that names a `crm::Customer` today is a sales invoice, so "two roles" has nothing to contrast with. It lands with Phase 20e, where a lease exists and the customer on it is also the customer on an invoice. Noted rather than dropped, because a spec requirement with no task is how coverage quietly rots.
