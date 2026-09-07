# ZATCA Onboarding That Finishes Itself — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The taxpayer types the Fatoora OTP and nothing else; the route buys the compliance certificate, the worker runs the six samples and obtains the production certificate, and the status endpoint says where it stands.

**Architecture:** `tax_sa` is one module, so the whole feature lives in it plus one worker job. The route keeps `Onboarder::onboard` (key, CSR, OTP → compliance certificate) and stops there. A new `zatca/finish.rs` is the worker's composition: it reads the onboarding read model (never the aggregate, law L7), submits the samples, records `ChecksPassed` or `Refused` on the onboarding aggregate, and asks for the production certificate. The read model gains the columns that let the worker decide what is due and the status endpoint report it.

**Tech Stack:** Rust 2024 (1.97), axum + utoipa, sqlx (`query!` needs the typecheck database to have new columns), event-sourced aggregates via `erp_eventlog`, `erp_projection` read models rebuilt from the log, `openssl` for certificates, `uuid` (workspace features `v5`, `v7` — **no v4**).

## Global Constraints

- **Do not commit.** Leave every change in the working tree. The user commits.
- **Every guard is falsified**: revert the fix, run the test, watch it fail, restore, watch it pass. Record which line was reverted.
- Do not run the full suite. Run the crate or test named in each step. Hand the user `just check` at the end.
- Iteration environment for every `cargo` command:
  `export SQLX_OFFLINE=false DATABASE_URL="postgres://postgres:postgres@localhost:55432/erp_typecheck" REDIS_URL=redis://127.0.0.1:56379/`
- Clippy is `-D warnings` with `too_many_lines` at 100 (tests included; long tests take `#[expect(clippy::too_many_lines, reason = "…")]`), `expect_used` denied in `erp-api` non-test code, `match_same_arms`, `single_match_else`.
- Every HTTP handler needs a doc comment (`every_operation_declares_its_answers`). The role matrix `every_role_against_every_endpoint` asserts 216 role-scoped operations and must not change: no operation is added or removed.
- Two compatibility breaks are accepted by the user: `activate`'s response shape and the required `industry` on the registration body. Take them with `just baseline` in Task 8, not before.
- Messages: every new code has an English and an Arabic rendering in `modules/tax_sa/src/messages.rs`.
- Anchors for scripted edits must match raw text; verify with `grep -n` before patching.
- The spec is `docs/superpowers/specs/2026-09-07-zatca-automatic-onboarding-design.md`.

---

## File map

| file | responsibility after this plan |
|---|---|
| `modules/tax_sa/src/taxpayer.rs` | `Registration.industry` and its check |
| `modules/tax_sa/src/zatca/samples.rs` | sample documents from `Issues`, not a `Unit` |
| `modules/tax_sa/src/zatca/onboarding.rs` | route's half (`onboard`), `pass_compliance_checks(Issues)`, environment change forgets production credentials |
| `modules/tax_sa/src/zatca/finish.rs` (new) | worker's half: `due`, `finish`, `Finished` |
| `modules/tax_sa/src/onboarded.rs` | events `ChecksPassed`, `Refused`, `Step`, `Refusal`; aggregate resets on environment change |
| `modules/tax_sa/src/commands.rs` | `record_checks_passed`, `record_refusal` |
| `modules/tax_sa/src/projections.rs` + `schema/install.sql` | `onboarding` row gains checks/refusal columns; stage resets on environment change |
| `modules/tax_sa/src/http.rs` | slim bodies, derived unit, 409 guard, 202, status fields |
| `modules/tax_sa/src/messages.rs` | `tax_sa.no_industry`, `tax_sa.already_live` |
| `crates/erp-worker/src/bin/worker.rs` | `FinishOnboarding` job `tax_sa.onboard` |
| tests: `modules/tax_sa/tests/tax_sa.rs`, `crates/erp-api/tests/http.rs`, unit tests in the files above | guards |
| docs: `docs/IMPLEMENTATION.md`, `docs/RUNNING.md` | §45 and the operator's flow |

---

### Task 1: The registration carries the industry

**Files:**
- Modify: `modules/tax_sa/src/taxpayer.rs` (struct `Registration` ~line 54, `check()` ~line 176, tests `registration()` ~line 304)
- Modify: `modules/tax_sa/src/http.rs` (`RegistrationBody` ~line 477 and its `#[schema(example)]`, `register` ~line 675, `registration` GET ~line 760)
- Modify: `modules/tax_sa/tests/tax_sa.rs:555`, `modules/tax_sa/tests/high_volume.rs:74`, `modules/tax_sa/tests/sandbox.rs:96`, `crates/erp-api/tests/http.rs:7398` (struct literals), `crates/erp-api/tests/http.rs:416` (`register_with_zatca` JSON)

**Interfaces:**
- Produces: `Registration.industry: Option<String>`; `InvalidRegistration::Missing { field: "industry" }` for an empty one; `RegistrationBody.industry: String` (required on PUT).

- [x] **Step 1: Write the failing unit test** in `modules/tax_sa/src/taxpayer.rs`, inside `mod tests`, after the existing `registration()` helper:

```rust
    /// **An industry, once given, cannot be blank.** It goes in the certificate
    /// request as the business category, and ZATCA refuses an empty one after
    /// the OTP has been spent. A registration recorded before the field existed
    /// has none, and still checks.
    #[test]
    fn an_industry_given_empty_is_refused() {
        let mut registration = registration();
        registration.industry = Some("  ".to_owned());
        assert_eq!(
            registration.check(),
            Err(InvalidRegistration::Missing { field: "industry" })
        );

        registration.industry = None;
        assert_eq!(registration.check(), Ok(()));
    }
```

- [x] **Step 2: Run it, expect a compile error** (`no field industry`):

```bash
cargo nextest run -p tax_sa --lib an_industry_given_empty_is_refused
```

- [x] **Step 3: Add the field and the check.** In `Registration` after `pub address: Address,`:

```rust
    /// The business's industry, as ZATCA's certificate request names it in
    /// `businessCategory` — `Consulting`, `Retail`. Optional in the type
    /// because registrations recorded before it existed have none; required
    /// by the registration route, because no certificate can be asked for
    /// without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub industry: Option<String>,
```

In `check()`, before `self.address.check()`:

```rust
        if self
            .industry
            .as_deref()
            .is_some_and(|industry| industry.trim().is_empty())
        {
            return Err(InvalidRegistration::Missing { field: "industry" });
        }
```

In the tests' `registration()` helper (~line 304) add `industry: Some("Consulting".to_owned()),` after `address`.

- [x] **Step 4: Add the field to every other `Registration` literal** (each fails to compile otherwise): `modules/tax_sa/tests/tax_sa.rs:555`, `modules/tax_sa/tests/high_volume.rs:74`, `modules/tax_sa/tests/sandbox.rs:96`, `crates/erp-api/tests/http.rs:7398` — add `industry: Some("Consulting".to_owned()),` as the last field of each.

- [x] **Step 5: The HTTP body.** In `RegistrationBody` after `address: AddressBody,`:

```rust
    /// The business's industry — `Consulting`, `Retail`, `Beauty`. It goes in
    /// the ZATCA certificate request as the business category, so it is
    /// required: no industry, no certificate.
    industry: String,
```

Add `"industry": "Consulting",` to the struct's `#[schema(example = json!({…}))]` after `"identifier"`. In `register`, add to the `crate::Registration { … }` literal after `identifier`:

```rust
        industry: Some(body.industry.trim().to_owned()),
```

In the `registration` GET handler's `Ok(Json(RegistrationBody { … }))` add after `identifier: registration.identifier,`:

```rust
        industry: registration.industry.unwrap_or_default(),
```

In `crates/erp-api/tests/http.rs` `register_with_zatca` JSON add `"industry": "Consulting",` after `"identifier": "1010101010",`.

- [x] **Step 6: Run the guards**

```bash
cargo nextest run -p tax_sa --lib an_industry_given_empty_is_refused
cargo nextest run -p tax_sa --test tax_sa registration
cargo nextest run -p erp-api --test http register
```
Expected: all pass.

- [x] **Step 7: Falsify.** Comment out the six-line `industry` check in `check()`; run the unit test → FAIL (`Ok(())` where `Err` expected). Restore; run → PASS.

---

### Task 2: The sample documents take `Issues`, not a `Unit`

The samples read nothing from the unit but which document kinds it declares. The worker will not hold a `Unit`, so the parameter becomes the thing actually used. Refactor only; the existing tests are the guard.

**Files:**
- Modify: `modules/tax_sa/src/zatca/samples.rs` (`compliance_documents` ~line 43; tests ~lines 160–300)
- Modify: `modules/tax_sa/src/zatca/onboarding.rs` (`pass_compliance_checks` ~line 468, `compliance_submissions` ~line 666)
- Modify: `modules/tax_sa/tests/tax_sa.rs` (three `pass_compliance_checks(` calls ~lines 2318, 2400, 2432), `modules/tax_sa/tests/sandbox.rs:376`, `modules/tax_sa/src/http.rs` `activate` (~line 1490, the `pass_compliance_checks` call — rewritten in Task 7; make it compile now)

**Interfaces:**
- Produces: `pub fn compliance_documents(registration: &Registration, issues: Issues, at: Timestamp) -> Vec<Document>`; `pub fn compliance_submissions(registration, issues: Issues, signer, at)`; `Onboarder::pass_compliance_checks(&self, registration: &Registration, issues: Issues, environment: Environment, at: Timestamp)`.

- [x] **Step 1: Change the signatures.** `samples.rs`:

```rust
use super::csr::Issues;
```
(drop `Unit` from that import) and

```rust
pub fn compliance_documents(
    registration: &Registration,
    issues: Issues,
    at: Timestamp,
) -> Vec<Document> {
    let mut kinds: Vec<Kind> = Vec::new();
    if issues.standard {
        kinds.push(Kind::Standard);
    }
    if issues.simplified {
        kinds.push(Kind::Simplified);
    }
```

`onboarding.rs`: `pass_compliance_checks(&self, registration: &crate::taxpayer::Registration, issues: Issues, environment: Environment, at: Timestamp)` and inside it `compliance_submissions(registration, issues, &signer, at)`; `compliance_submissions(registration: &crate::taxpayer::Registration, issues: Issues, signer: &super::signing::Signer, at: Timestamp)` and inside it `super::samples::compliance_documents(registration, issues, at)`. Add `Issues` to the `use super::csr::{…}` line.

- [x] **Step 2: Fix the callers.** In `samples.rs` tests: delete the `fn unit(issues: Issues) -> Unit` helper and run

```bash
python3 - <<'EOF'
import re
p='modules/tax_sa/src/zatca/samples.rs'; s=open(p).read()
n=len(re.findall(r'&unit\(', s))
s=re.sub(r'&unit\(((?:[^()]|\([^()]*\))*)\)', r'\1', s)
open(p,'w').write(s); print("replaced", n)
EOF
```
Expected: `replaced 6`. In `modules/tax_sa/tests/tax_sa.rs`, in each of the three `.pass_compliance_checks(` calls replace the argument line `&unit(),` with `Issues::both(),`. In `modules/tax_sa/tests/sandbox.rs:376` replace `&unit,` with `unit.issues,`. In `modules/tax_sa/src/http.rs` `activate`, replace `.pass_compliance_checks(&registration, &unit, environment, now)` with `.pass_compliance_checks(&registration, unit.issues, environment, now)`.

- [x] **Step 3: Build and run the existing guards**

```bash
cargo clippy -p tax_sa --all-targets -- -D warnings
cargo nextest run -p tax_sa compliance
```
Expected: clean; every `compliance` test passes (the samples unit tests, the three module tests).

---

### Task 3: Checks and refusals are facts in the log, and another environment starts over

**Files:**
- Modify: `modules/tax_sa/src/onboarded.rs` (events, aggregate, tests)
- Modify: `modules/tax_sa/src/commands.rs` (after `record_csid`, ~line 383)
- Modify: `modules/tax_sa/src/projections.rs` (`Onboardings::apply` ~line 211, `Onboarded` ~line 385, `onboarding()` ~line 398)
- Modify: `modules/tax_sa/schema/install.sql` (table `onboarding` ~line 72)
- Modify: `modules/tax_sa/src/lib.rs:69` (exports)
- Typecheck database: `ALTER TABLE` by hand (see step 6)

**Interfaces:**
- Produces:
  - `OnboardingEvent::ChecksPassed { certificate_serial: String, submitted: usize, at: Timestamp }`
  - `OnboardingEvent::Refused { step: Step, detail: String, version: String, at: Timestamp }`
  - `pub enum Step { ComplianceChecks, ProductionCertificate }` with `as_str()` → `"compliance_checks" | "production_certificate"`
  - `pub struct Refusal { pub step: Step, pub detail: String, pub version: String, pub at: Timestamp }`
  - `Onboarding.checks_passed_for: Option<String>`, `Onboarding.refused: Option<Refusal>`
  - `pub(crate) async fn record_checks_passed(db, certificate_serial: &str, submitted: usize, at, metadata)`
  - `pub(crate) async fn record_refusal(db, step: Step, detail: &str, version: &str, at, metadata)`
  - `Onboarded` gains `checks_serial: Option<String>, checks_submitted: Option<i32>, checks_passed_at: Option<Timestamp>, refused_step: Option<String>, refused_detail: Option<String>, refused_version: Option<String>, refused_at: Option<Timestamp>`
  - `tax_sa::{Refusal, Step}` exported.

- [x] **Step 1: Write the failing aggregate tests** in `modules/tax_sa/src/onboarded.rs` `mod tests`. Replace the `issued` helper with one that takes the environment, keep the old name as a shorthand:

```rust
    fn issued_in(environment: Environment, stage: Stage, serial: &str) -> OnboardingEvent {
        OnboardingEvent::CsidIssued {
            stage,
            environment,
            request_id: "1234".to_owned(),
            subject: "C=SA, CN=EGS1".to_owned(),
            serial: serial.to_owned(),
            not_before: "Jan  1 00:00:00 2026 GMT".to_owned(),
            not_after: "Jan  1 00:00:00 2031 GMT".to_owned(),
            at: Timestamp::UNIX_EPOCH,
        }
    }

    fn issued(stage: Stage, serial: &str) -> OnboardingEvent {
        issued_in(Environment::Simulation, stage, serial)
    }

    /// **Another environment is a fresh start.** Live in simulation says nothing
    /// about production, and reading it as "production" would send real
    /// invoices with simulation's certificate.
    #[test]
    fn a_certificate_for_another_environment_starts_over() {
        let mut onboarding = Onboarding::default();
        onboarding.apply(&issued(Stage::Compliance, "01"));
        onboarding.apply(&issued(Stage::Production, "02"));
        onboarding.apply(&issued_in(Environment::Production, Stage::Compliance, "03"));

        assert_eq!(onboarding.stage, Some(Stage::Compliance));
        assert_eq!(onboarding.environment, Some(Environment::Production));
    }

    /// **Checks and refusals belong to the certificate they were for.** A new
    /// compliance certificate has to earn its own passed checks, and clears
    /// whatever ZATCA refused about the old one.
    #[test]
    fn checks_and_refusals_belong_to_the_certificate_they_were_for() {
        let mut onboarding = Onboarding::default();
        onboarding.apply(&issued(Stage::Compliance, "01"));
        onboarding.apply(&OnboardingEvent::ChecksPassed {
            certificate_serial: "01".to_owned(),
            submitted: 6,
            at: Timestamp::UNIX_EPOCH,
        });
        onboarding.apply(&OnboardingEvent::Refused {
            step: Step::ProductionCertificate,
            detail: "REJECTED: not yet".to_owned(),
            version: "0.1.0".to_owned(),
            at: Timestamp::UNIX_EPOCH,
        });
        assert_eq!(onboarding.checks_passed_for.as_deref(), Some("01"));
        assert_eq!(
            onboarding.refused.as_ref().map(|r| r.step),
            Some(Step::ProductionCertificate)
        );

        onboarding.apply(&issued(Stage::Compliance, "02"));
        assert_eq!(onboarding.checks_passed_for, None);
        assert_eq!(onboarding.refused, None);
    }
```

- [x] **Step 2: Run, expect compile errors**

```bash
cargo nextest run -p tax_sa --lib onboarded
```

- [x] **Step 3: The events and the aggregate.** In `onboarded.rs`, after the `CsidIssued` variant inside `OnboardingEvent`:

```rust
    /// Every compliance sample ZATCA was shown was accepted — what the
    /// production certificate is asked for on the strength of.
    ChecksPassed {
        /// The compliance certificate the samples were signed with. Checks
        /// passed for one certificate are no evidence for the next.
        certificate_serial: String,
        submitted: usize,
        at: Timestamp,
    },
    /// ZATCA refused a step the worker cannot argue with.
    ///
    /// Recorded rather than retried: a refused sample is this software's fault
    /// and the same samples would be refused again. The build version says
    /// which software was refused, so a new build gets one more try.
    Refused {
        step: Step,
        /// ZATCA's words — or the first refused document and its first error.
        detail: String,
        /// `CARGO_PKG_VERSION` of the build that was refused.
        version: String,
        at: Timestamp,
    },
```

`NAMES` becomes three, and `event_name` maps them:

```rust
    pub const NAMES: [&'static str; 3] = [
        "tax_sa.zatca.csid_issued",
        "tax_sa.zatca.checks_passed",
        "tax_sa.zatca.refused",
    ];
```
```rust
        crate::name(match self {
            Self::CsidIssued { .. } => Self::NAMES[0],
            Self::ChecksPassed { .. } => Self::NAMES[1],
            Self::Refused { .. } => Self::NAMES[2],
        })
```

Below `OnboardingEvent`:

```rust
/// The steps the worker takes after the certificate, and can be refused at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// The six sample documents.
    ComplianceChecks,
    /// The production certificate request.
    ProductionCertificate,
}

impl Step {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ComplianceChecks => "compliance_checks",
            Self::ProductionCertificate => "production_certificate",
        }
    }
}

/// What ZATCA last refused, standing against the current certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub step: Step,
    pub detail: String,
    pub version: String,
    pub at: Timestamp,
}
```

`Onboarding` gains two fields after `issued_at`:

```rust
    /// The compliance certificate whose samples all passed, if any.
    pub checks_passed_for: Option<String>,
    /// What ZATCA refused about the current certificate, if anything.
    pub refused: Option<Refusal>,
```

`apply` becomes:

```rust
    fn apply(&mut self, event: &Self::Event) {
        match event {
            OnboardingEvent::CsidIssued {
                stage,
                environment,
                serial,
                not_after,
                at,
                ..
            } => {
                // **Another environment is a fresh start**: what was reached in
                // simulation says nothing about production. Within one, a
                // renewal of the production certificate must not put the tenant
                // back to `compliance`, and a compliance certificate re-issued
                // after going live must not either.
                let moved = self.environment.is_some_and(|had| had != *environment);
                if moved || *stage == Stage::Production || self.stage.is_none() {
                    self.stage = Some(*stage);
                }
                if *stage == Stage::Compliance {
                    // A new certificate has to earn its own passed checks.
                    self.checks_passed_for = None;
                }
                // Any certificate answers whatever was refused about the last.
                self.refused = None;
                self.environment = Some(*environment);
                self.serial = Some(serial.clone());
                self.not_after = Some(not_after.clone());
                self.issued_at = Some(*at);
            }
            OnboardingEvent::ChecksPassed {
                certificate_serial, ..
            } => self.checks_passed_for = Some(certificate_serial.clone()),
            OnboardingEvent::Refused {
                step,
                detail,
                version,
                at,
            } => {
                self.refused = Some(Refusal {
                    step: *step,
                    detail: detail.clone(),
                    version: version.clone(),
                    at: *at,
                });
            }
        }
    }
```

Export in `lib.rs:69`: `pub use onboarded::{Onboarding, OnboardingEvent, Refusal, Step, onboarding_id};`

- [x] **Step 4: Run the aggregate tests**

```bash
cargo nextest run -p tax_sa --lib onboarded
```
Expected: the two new tests and the two existing ones pass.

- [x] **Step 5: The commands.** In `modules/tax_sa/src/commands.rs` after `record_csid`:

```rust
/// Records that every compliance sample signed with this certificate passed.
///
/// Recorded once per certificate: the worker that crashed between ZATCA's
/// answer and this write repeats the samples, and the second write is nothing.
pub(crate) async fn record_checks_passed(
    db: &TenantDb,
    certificate_serial: &str,
    submitted: usize,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<Committed<crate::onboarded::OnboardingEvent>, CommandError<TaxError>> {
    db.execute::<crate::onboarded::Onboarding, _, TaxError>(
        &crate::onboarded::onboarding_id(),
        crate::upcasters(),
        metadata,
        |loaded| {
            if loaded.aggregate.checks_passed_for.as_deref() == Some(certificate_serial) {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(
                crate::onboarded::OnboardingEvent::ChecksPassed {
                    certificate_serial: certificate_serial.to_owned(),
                    submitted,
                    at,
                },
            ))
        },
    )
    .await
}

/// Records that ZATCA refused a step, so the worker stops asking until
/// something changes — a new build, or a new certificate.
pub(crate) async fn record_refusal(
    db: &TenantDb,
    step: crate::onboarded::Step,
    detail: &str,
    version: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<Committed<crate::onboarded::OnboardingEvent>, CommandError<TaxError>> {
    db.execute::<crate::onboarded::Onboarding, _, TaxError>(
        &crate::onboarded::onboarding_id(),
        crate::upcasters(),
        metadata,
        |loaded| {
            // The same refusal twice is one refusal.
            if loaded.aggregate.refused.as_ref().is_some_and(|standing| {
                standing.step == step && standing.detail == detail && standing.version == version
            }) {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(crate::onboarded::OnboardingEvent::Refused {
                step,
                detail: detail.to_owned(),
                version: version.to_owned(),
                at,
            }))
        },
    )
    .await
}
```

- [x] **Step 6: The read model.** `modules/tax_sa/schema/install.sql`, inside `CREATE TABLE IF NOT EXISTS onboarding (…)` after `recorded_at TIMESTAMPTZ NOT NULL`:

```sql
    recorded_at TIMESTAMPTZ NOT NULL,
    -- **The worker's half.** Which compliance certificate's samples all passed,
    -- and what ZATCA last refused. A new compliance certificate clears them:
    -- checks passed for one certificate are no evidence for the next.
    checks_serial     TEXT,
    checks_submitted  INTEGER,
    checks_passed_at  TIMESTAMPTZ,
    refused_step      TEXT,
    refused_detail    TEXT,
    refused_version   TEXT,
    refused_at        TIMESTAMPTZ
```

(Deployed tenants get columns through the projection rebuild, `erp_projection::rebuild_swap`; that is the existing mechanism, there is no `ALTER` in any `install.sql`.) Then the typecheck database, by hand:

```bash
psql "postgres://postgres:postgres@localhost:55432/erp_typecheck" -c "ALTER TABLE proj_tax_sa.onboarding ADD COLUMN IF NOT EXISTS checks_serial TEXT, ADD COLUMN IF NOT EXISTS checks_submitted INTEGER, ADD COLUMN IF NOT EXISTS checks_passed_at TIMESTAMPTZ, ADD COLUMN IF NOT EXISTS refused_step TEXT, ADD COLUMN IF NOT EXISTS refused_detail TEXT, ADD COLUMN IF NOT EXISTS refused_version TEXT, ADD COLUMN IF NOT EXISTS refused_at TIMESTAMPTZ"
```

`modules/tax_sa/src/projections.rs`, replace the body of `Onboardings::apply` from the `let OnboardingEvent::CsidIssued { … } = ctx.decode…` down to the end of the function with:

```rust
        let event = ctx
            .decode::<OnboardingEvent>(envelope)
            .map_err(|source| ProjectionError::Decode {
                event_name: envelope.event_name.as_str().to_owned(),
                position: envelope.position,
                source,
            })?;

        match event {
            OnboardingEvent::CsidIssued {
                stage,
                environment,
                serial,
                not_after,
                at,
                ..
            } => {
                // **Another environment is a fresh start**; within one, the
                // furthest stage reached, because a production certificate does
                // not un-issue the compliance one. A new compliance certificate
                // starts its checks clean, and any certificate clears a refusal.
                sqlx::query(
                    "INSERT INTO onboarding
                         (id, stage, environment, serial, not_after, issued_at, recorded_at)
                     VALUES ('self', $1, $2, $3, $4, $5, $6)
                     ON CONFLICT (id) DO UPDATE
                        SET stage = CASE
                                WHEN onboarding.environment <> EXCLUDED.environment THEN EXCLUDED.stage
                                WHEN onboarding.stage = 'production' THEN 'production'
                                ELSE EXCLUDED.stage
                            END,
                            environment      = EXCLUDED.environment,
                            serial           = EXCLUDED.serial,
                            not_after        = EXCLUDED.not_after,
                            issued_at        = EXCLUDED.issued_at,
                            recorded_at      = EXCLUDED.recorded_at,
                            checks_serial    = CASE WHEN EXCLUDED.stage = 'compliance' THEN NULL ELSE onboarding.checks_serial END,
                            checks_submitted = CASE WHEN EXCLUDED.stage = 'compliance' THEN NULL ELSE onboarding.checks_submitted END,
                            checks_passed_at = CASE WHEN EXCLUDED.stage = 'compliance' THEN NULL ELSE onboarding.checks_passed_at END,
                            refused_step     = NULL,
                            refused_detail   = NULL,
                            refused_version  = NULL,
                            refused_at       = NULL",
                )
                .bind(stage.as_str())
                .bind(environment.as_str())
                .bind(serial)
                .bind(not_after)
                .bind(at)
                .bind(ctx.event_time())
                .execute(&mut *conn)
                .await?;
            }
            OnboardingEvent::ChecksPassed {
                certificate_serial,
                submitted,
                at,
            } => {
                sqlx::query(
                    "UPDATE onboarding
                        SET checks_serial = $1, checks_submitted = $2, checks_passed_at = $3,
                            recorded_at = $4
                      WHERE id = 'self'",
                )
                .bind(certificate_serial)
                .bind(i32::try_from(submitted).unwrap_or(i32::MAX))
                .bind(at)
                .bind(ctx.event_time())
                .execute(&mut *conn)
                .await?;
            }
            OnboardingEvent::Refused {
                step,
                detail,
                version,
                at,
            } => {
                sqlx::query(
                    "UPDATE onboarding
                        SET refused_step = $1, refused_detail = $2, refused_version = $3,
                            refused_at = $4, recorded_at = $5
                      WHERE id = 'self'",
                )
                .bind(step.as_str())
                .bind(detail)
                .bind(version)
                .bind(at)
                .bind(ctx.event_time())
                .execute(&mut *conn)
                .await?;
            }
        }

        Ok(())
```

Update the `Onboardings` doc comment's last sentence to mention the environment reset. `Onboarded` gains, after `issued_at`:

```rust
    /// The compliance certificate whose samples all passed.
    pub checks_serial: Option<String>,
    pub checks_submitted: Option<i32>,
    pub checks_passed_at: Option<Timestamp>,
    /// What ZATCA last refused, standing against the current certificate.
    pub refused_step: Option<String>,
    pub refused_detail: Option<String>,
    pub refused_version: Option<String>,
    pub refused_at: Option<Timestamp>,
```

and `onboarding()` selects them:

```rust
    let row = sqlx::query!(
        r#"SELECT stage as "stage!", environment as "environment!", serial as "serial!",
                  not_after as "not_after!", issued_at as "issued_at!",
                  checks_serial, checks_submitted, checks_passed_at,
                  refused_step, refused_detail, refused_version, refused_at
             FROM proj_tax_sa.onboarding
            WHERE id = 'self'"#,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|r| Onboarded {
        stage: r.stage,
        environment: r.environment,
        serial: r.serial,
        not_after: r.not_after,
        issued_at: r.issued_at,
        checks_serial: r.checks_serial,
        checks_submitted: r.checks_submitted,
        checks_passed_at: r.checks_passed_at,
        refused_step: r.refused_step,
        refused_detail: r.refused_detail,
        refused_version: r.refused_version,
        refused_at: r.refused_at,
    }))
```

- [x] **Step 7: Build**

```bash
cargo clippy -p tax_sa --all-targets -- -D warnings
```
Expected: clean. (`record_checks_passed`/`record_refusal` are `pub(crate)` and unused until Task 5; if clippy reports dead code, add `#[allow(dead_code)]` **temporarily** and remove it in Task 5.)

- [x] **Step 8: Falsify the aggregate guards.** In `apply`, change `if moved || *stage == Stage::Production || self.stage.is_none()` to `if *stage == Stage::Production || self.stage.is_none()` → `a_certificate_for_another_environment_starts_over` FAILS. Restore. Delete the `self.checks_passed_for = None;` line → `checks_and_refusals_belong_to_the_certificate_they_were_for` FAILS. Restore; both PASS. (The projection's own reset is falsified by Task 4's module test.)

---

### Task 4: A certificate for another environment forgets the old production credentials

**Files:**
- Modify: `modules/tax_sa/src/zatca/onboarding.rs` (`accept_certificate` ~line 641; new helper below `store`)
- Test: `modules/tax_sa/tests/tax_sa.rs` (after `going_live_without_a_compliance_certificate_is_refused`)

**Interfaces:**
- Consumes: `tax_sa::onboarding(&mut conn) -> Result<Option<Onboarded>, sqlx::Error>` (Task 3), `erp_eventlog::secrets::forget(conn, key)`.

- [x] **Step 1: Write the failing module test** in `modules/tax_sa/tests/tax_sa.rs`:

```rust
/// **Onboarding into another environment starts from compliance.** Live in
/// simulation, then a production OTP: simulation's production credentials
/// cannot clear a real invoice, and the submit sweep would try with them. So
/// they go, and the read model says compliance in production — not production
/// anywhere.
#[tokio::test]
async fn onboarding_into_another_environment_starts_from_compliance() {
    let fixture = Fixture::new().await;
    let zatca = FakeZatcaCa::new();
    let sealing = sealing();
    let onboarder = Onboarder::new(&fixture.db, &sealing, &zatca);

    onboarder
        .onboard(
            &unit(),
            Environment::Simulation,
            &otp(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("onboards in simulation");
    onboarder
        .go_live(
            Environment::Simulation,
            on("2026-01-02"),
            &Metadata::default(),
        )
        .await
        .expect("goes live in simulation");
    fixture.project().await;

    onboarder
        .onboard(
            &unit(),
            Environment::Production,
            &otp(),
            on("2026-02-01"),
            &Metadata::default(),
        )
        .await
        .expect("onboards in production");
    fixture.project().await;

    assert_eq!(
        tax_sa::zatca::onboarding::reached(&fixture.db)
            .await
            .expect("reads"),
        vec![Stage::Compliance],
        "simulation's production credentials are gone"
    );
    let mut conn = fixture.db.read().await.expect("a connection");
    let row = tax_sa::onboarding(&mut conn)
        .await
        .expect("reads")
        .expect("a row");
    drop(conn);
    assert_eq!((row.stage.as_str(), row.environment.as_str()), ("compliance", "production"));

    fixture.cleanup().await;
}
```

- [x] **Step 2: Run, expect FAIL** on the `reached` assertion (production still there):

```bash
cargo nextest run -p tax_sa --test tax_sa onboarding_into_another_environment
```

- [x] **Step 3: Forget before storing.** In `accept_certificate`, after `let issued = accept(csid, &key, stage, environment)?;` and before `store(…)`:

```rust
    if stage == Stage::Compliance {
        forget_another_environments_production(db, environment).await?;
    }
```

Below `store`:

```rust
/// A compliance certificate for another environment makes the production
/// credentials on file somebody else's: simulation's certificate cannot clear
/// a real invoice, and the submit sweep would try. So they go before the new
/// certificate is stored, and the tenant is at compliance until it earns
/// production again. Read from the projection (L7), which describes the
/// certificates this one is replacing.
async fn forget_another_environments_production(
    db: &TenantDb,
    environment: Environment,
) -> Result<(), OnboardError> {
    let mut conn = db.read().await?;
    let onboarded = crate::onboarding(&mut conn).await?;
    drop(conn);
    let Some(onboarded) = onboarded else {
        return Ok(());
    };
    if onboarded.environment == environment.as_str() {
        return Ok(());
    }

    let mut conn = db.acquire().await?;
    erp_eventlog::secrets::forget(&mut conn, PRODUCTION_SECRET).await?;
    drop(conn);
    Ok(())
}
```

- [x] **Step 4: Run → PASS.** Same command.

- [x] **Step 5: Falsify twice.** (a) Comment out the two-line `if stage == Stage::Compliance {…}` call → FAIL on `reached`. Restore. (b) In `projections.rs`, change the `CASE` back to `GREATEST(onboarding.stage, EXCLUDED.stage)` for `stage` → FAIL on `("compliance", "production")`. Restore. Run → PASS.

---

### Task 5: `finish` — the worker's half

**Files:**
- Create: `modules/tax_sa/src/zatca/finish.rs`
- Modify: `modules/tax_sa/src/zatca/mod.rs:51` (`pub mod finish;` after `pub mod csr;` alphabetically: after `pub mod csr;`)
- Modify: `modules/tax_sa/src/zatca/onboarding.rs` (`OnboardError` gains `NotRegistered`)
- Test: `modules/tax_sa/tests/tax_sa.rs` (`FakeZatcaCa` gains `production_unanswered`, `checks()`; three tests)

**Interfaces:**
- Consumes: `Onboarder::pass_compliance_checks(registration, Issues, environment, at)` (Task 2), `record_checks_passed`, `record_refusal`, `Onboarded` fields (Task 3).
- Produces:
  - `pub enum Due { Checks, Production }`
  - `pub fn due(onboarded: &crate::Onboarded) -> Option<Due>`
  - `pub struct Finished { pub checks: Option<ComplianceChecks>, pub production: Option<Issued>, pub refused: Option<Step> }` with `did_something()`
  - `pub async fn finish(db: &TenantDb, sealing: &SealingKey, registrar: &dyn Registrar, now: Timestamp, metadata: &Metadata) -> Result<Finished, OnboardError>`

- [x] **Step 1: Extend the fake.** In `FakeZatcaCa` add a field and accessor:

```rust
    /// How many production requests to leave unanswered before issuing —
    /// ZATCA down for a moment, which is not a refusal.
    production_unanswered: std::sync::Mutex<u32>,
```
initialise `production_unanswered: std::sync::Mutex::new(0),` in `new()`, add

```rust
    /// How many compliance documents it has been shown.
    fn checks(&self) -> usize {
        self.checked.lock().expect("not poisoned").len()
    }
```
and at the top of `production_csid` (before the two `assert!`s):

```rust
        {
            let mut left = self.production_unanswered.lock().expect("not poisoned");
            if *left > 0 {
                *left -= 1;
                return Err(tax_sa::zatca::wire::Unanswered::Unavailable(
                    "connection reset".to_owned(),
                ));
            }
        }
```

- [x] **Step 2: Write the three failing module tests** (add `use tax_sa::zatca::finish::{Finished, finish};` and `use tax_sa::Step;` to the onboarding `use` block):

```rust
/// **One OTP, and the worker does the rest.** The route stops at the
/// compliance certificate; this is everything after it, in one pass. A second
/// pass finds nothing to do and sends nothing.
#[tokio::test]
async fn the_worker_finishes_what_one_otp_started() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    let zatca = FakeZatcaCa::new();
    let sealing = sealing();
    Onboarder::new(&fixture.db, &sealing, &zatca)
        .onboard(
            &unit(),
            Environment::Simulation,
            &otp(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("onboards");
    fixture.project().await;

    let finished = finish(
        &fixture.db,
        &sealing,
        &zatca,
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("finishes");
    let checks = finished.checks.expect("the samples were submitted");
    assert_eq!((checks.submitted, checks.passed), (6, 6));
    assert_eq!(
        finished.production.expect("went live").stage,
        Stage::Production
    );
    assert_eq!(finished.refused, None);
    assert_eq!(zatca.checks(), 6);
    assert_eq!(
        tax_sa::zatca::onboarding::reached(&fixture.db)
            .await
            .expect("reads"),
        vec![Stage::Compliance, Stage::Production]
    );

    fixture.project().await;
    let mut conn = fixture.db.read().await.expect("a connection");
    let row = tax_sa::onboarding(&mut conn)
        .await
        .expect("reads")
        .expect("a row");
    drop(conn);
    assert_eq!(row.stage, "production");
    assert_eq!(row.checks_submitted, Some(6));
    assert!(row.checks_passed_at.is_some());
    assert_eq!(row.refused_step, None);

    // Live: nothing more to do, and nothing more is sent.
    let again = finish(
        &fixture.db,
        &sealing,
        &zatca,
        on("2026-01-03"),
        &Metadata::default(),
    )
    .await
    .expect("finishes");
    assert!(!again.did_something(), "{again:?}");
    assert_eq!(zatca.checks(), 6, "the samples were not sent again");

    fixture.cleanup().await;
}

/// **A refused sample is recorded, not retried.** The samples are generated
/// here, so the same build would be refused again; the refusal names the build,
/// and the same build asking again sends nothing.
#[tokio::test]
async fn a_refused_sample_is_recorded_and_waits_for_a_new_build() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    let mut zatca = FakeZatcaCa::new();
    zatca.refuse_checks = true;
    let sealing = sealing();
    Onboarder::new(&fixture.db, &sealing, &zatca)
        .onboard(
            &unit(),
            Environment::Simulation,
            &otp(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("onboards");
    fixture.project().await;

    let finished = finish(
        &fixture.db,
        &sealing,
        &zatca,
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("finishes");
    assert_eq!(finished.refused, Some(Step::ComplianceChecks));
    assert!(finished.production.is_none());
    assert_eq!(
        tax_sa::zatca::onboarding::reached(&fixture.db)
            .await
            .expect("reads"),
        vec![Stage::Compliance]
    );

    fixture.project().await;
    let mut conn = fixture.db.read().await.expect("a connection");
    let row = tax_sa::onboarding(&mut conn)
        .await
        .expect("reads")
        .expect("a row");
    drop(conn);
    assert_eq!(row.stage, "compliance");
    assert_eq!(row.refused_step.as_deref(), Some("compliance_checks"));
    assert!(
        row.refused_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("BR-KSA-99")),
        "{row:?}"
    );
    assert_eq!(
        row.refused_version.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );

    // The same build asks again: nothing is sent.
    let again = finish(
        &fixture.db,
        &sealing,
        &zatca,
        on("2026-01-02"),
        &Metadata::default(),
    )
    .await
    .expect("finishes");
    assert!(!again.did_something(), "{again:?}");
    assert_eq!(zatca.checks(), 6);

    fixture.cleanup().await;
}

/// **Passed checks are not resent** when the production request is what
/// failed. They are recorded, and the next pass starts from step 4.
#[tokio::test]
async fn passed_checks_are_not_resent_when_going_live_fails() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    let zatca = FakeZatcaCa::new();
    *zatca.production_unanswered.lock().expect("not poisoned") = 1;
    let sealing = sealing();
    Onboarder::new(&fixture.db, &sealing, &zatca)
        .onboard(
            &unit(),
            Environment::Simulation,
            &otp(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("onboards");
    fixture.project().await;

    let first = finish(
        &fixture.db,
        &sealing,
        &zatca,
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await;
    assert!(
        matches!(
            first,
            Err(tax_sa::zatca::onboarding::OnboardError::Unanswered {
                step: "requesting a production certificate",
                ..
            })
        ),
        "{first:?}"
    );
    assert_eq!(zatca.checks(), 6);
    fixture.project().await;

    let second = finish(
        &fixture.db,
        &sealing,
        &zatca,
        on("2026-01-02"),
        &Metadata::default(),
    )
    .await
    .expect("finishes");
    assert!(second.checks.is_none(), "the samples were not resubmitted");
    assert_eq!(zatca.checks(), 6);
    assert_eq!(
        second.production.expect("went live").stage,
        Stage::Production
    );

    fixture.cleanup().await;
}
```

`Finished` needs `Debug` for the `{again:?}` assertions.

- [x] **Step 3: Run, expect compile errors** (`finish` does not exist):

```bash
cargo nextest run -p tax_sa --test tax_sa finish
```

- [x] **Step 4: Write `modules/tax_sa/src/zatca/finish.rs`:**

```rust
//! What the worker does once a tenant holds a compliance certificate.
//!
//! # Why this is the worker's, and the OTP exchange is not
//!
//! The route spends the OTP: it is the taxpayer's proof of who they are for
//! about an hour, and the one call that needs it is answered while they wait.
//! Everything after that — six signed samples and the production request —
//! needs only the compliance certificate, which is sealed here, so nothing
//! about it needs the taxpayer or a request handler holding a connection
//! through seven network calls. The worker runs it, retries what ZATCA did not
//! answer, and records what it refused.
//!
//! # What a pass reads, and what it writes
//!
//! It reads the onboarding read model (law L7: a worker loads no aggregate)
//! and decides from it what is [`due`]. It writes through the commands:
//! `ChecksPassed` once the samples are accepted, `Refused` when ZATCA says no,
//! and `CsidIssued` for the production certificate. All three are idempotent,
//! so a pass that crashes between ZATCA's answer and the write repeats the
//! step and writes nothing the second time.
//!
//! # Why a refusal waits for a new build
//!
//! The samples are generated here. If ZATCA refuses one, the same build would
//! be refused again on the next pass, every pass, for as long as the tenant
//! waits — so the refusal records the build version and the same version does
//! not ask again. A deploy that fixes the samples tries once more without
//! anybody remembering to.

use erp_eventlog::{Metadata, SealingKey};
use erp_tenant::TenantDb;
use erp_types::Timestamp;

use super::csr::{Environment, Issues};
use super::onboarding::{ComplianceChecks, Issued, OnboardError, Onboarder, Registrar, Stage};
use crate::onboarded::Step;

/// The step a tenant at compliance is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// The six samples have not passed for the current certificate.
    Checks,
    /// They have; the production certificate has not been asked for, or was
    /// not answered.
    Production,
}

/// What a tenant needs next, or nothing: live already, never onboarded, or
/// waiting on a refusal this build cannot change.
#[must_use]
pub fn due(onboarded: &crate::Onboarded) -> Option<Due> {
    if onboarded.stage != Stage::Compliance.as_str() {
        return None;
    }
    if onboarded.refused_version.as_deref() == Some(env!("CARGO_PKG_VERSION")) {
        return None;
    }
    if onboarded.checks_serial.as_deref() == Some(onboarded.serial.as_str()) {
        Some(Due::Production)
    } else {
        Some(Due::Checks)
    }
}

/// What one pass did.
#[derive(Debug, Default)]
pub struct Finished {
    /// The samples, when this pass submitted them.
    pub checks: Option<ComplianceChecks>,
    /// The production certificate, when this pass obtained it.
    pub production: Option<Issued>,
    /// The step ZATCA refused, when this pass recorded a refusal.
    pub refused: Option<Step>,
}

impl Finished {
    #[must_use]
    pub const fn did_something(&self) -> bool {
        self.checks.is_some() || self.production.is_some() || self.refused.is_some()
    }
}

/// Takes a tenant from a compliance certificate as far as ZATCA lets it.
///
/// Returns `Err` only for what nobody decided — ZATCA unreachable, the database
/// down — and the caller tries again next pass. A refusal is a decision, and
/// comes back as `Ok` with [`Finished::refused`] set.
pub async fn finish(
    db: &TenantDb,
    sealing: &SealingKey,
    registrar: &dyn Registrar,
    now: Timestamp,
    metadata: &Metadata,
) -> Result<Finished, OnboardError> {
    let mut conn = db.read().await?;
    let onboarded = crate::onboarding(&mut conn).await?;
    let registration = crate::registered(&mut conn).await?;
    drop(conn);

    let Some(onboarded) = onboarded else {
        return Ok(Finished::default());
    };
    let Some(mut step) = due(&onboarded) else {
        return Ok(Finished::default());
    };
    let environment: Environment = onboarded
        .environment
        .parse()
        .map_err(OnboardError::Certificate)?;
    let onboarder = Onboarder::new(db, sealing, registrar);
    let version = env!("CARGO_PKG_VERSION");
    let mut finished = Finished::default();

    if step == Due::Checks {
        let registration = registration.ok_or(OnboardError::NotRegistered)?;
        let checks = onboarder
            .pass_compliance_checks(&registration, Issues::both(), environment, now)
            .await?;
        if !checks.all_passed() {
            // **Ours, not the tenant's.** The samples are generated here, so a
            // refusal is a bug in this software: the whole list to the log,
            // the first one to the record.
            tracing::error!(
                submitted = checks.submitted,
                passed = checks.passed,
                failures = ?checks.failures,
                "ZATCA refused a compliance document"
            );
            crate::commands::record_refusal(
                db,
                Step::ComplianceChecks,
                &first_failure(&checks),
                version,
                now,
                metadata,
            )
            .await?;
            finished.checks = Some(checks);
            finished.refused = Some(Step::ComplianceChecks);
            return Ok(finished);
        }
        crate::commands::record_checks_passed(
            db,
            &onboarded.serial,
            checks.submitted,
            now,
            metadata,
        )
        .await?;
        finished.checks = Some(checks);
        step = Due::Production;
    }

    debug_assert_eq!(step, Due::Production);
    match onboarder.go_live(environment, now, metadata).await {
        Ok(issued) => finished.production = Some(issued),
        Err(OnboardError::NotIssued {
            disposition,
            detail,
        }) => {
            crate::commands::record_refusal(
                db,
                Step::ProductionCertificate,
                &format!("{disposition}: {detail}"),
                version,
                now,
                metadata,
            )
            .await?;
            finished.refused = Some(Step::ProductionCertificate);
        }
        Err(other) => return Err(other),
    }
    Ok(finished)
}

/// The first refused document and its first error, as one line.
fn first_failure(checks: &ComplianceChecks) -> String {
    checks
        .failures
        .first()
        .map(|(document, errors)| {
            let first = errors
                .first()
                .map_or_else(String::new, |e| format!("{}: {}", e.code, e.message));
            format!("{document} — {first}")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_compliance() -> crate::Onboarded {
        crate::Onboarded {
            stage: "compliance".to_owned(),
            environment: "simulation".to_owned(),
            serial: "01".to_owned(),
            not_after: "Jan  1 00:00:00 2031 GMT".to_owned(),
            issued_at: Timestamp::UNIX_EPOCH,
            checks_serial: None,
            checks_submitted: None,
            checks_passed_at: None,
            refused_step: None,
            refused_detail: None,
            refused_version: None,
            refused_at: None,
        }
    }

    /// **What is due follows the row, and a refusal binds only the build that
    /// was refused.** A new build sees the same row and tries again.
    #[test]
    fn a_refusal_holds_this_build_and_releases_the_next() {
        let mut row = at_compliance();
        assert_eq!(due(&row), Some(Due::Checks));

        row.refused_version = Some(env!("CARGO_PKG_VERSION").to_owned());
        assert_eq!(due(&row), None, "this build was refused");

        row.refused_version = Some("0.0.0-before".to_owned());
        assert_eq!(due(&row), Some(Due::Checks), "a later build tries once more");

        row.refused_version = None;
        row.checks_serial = Some("01".to_owned());
        assert_eq!(due(&row), Some(Due::Production));

        row.checks_serial = Some("00".to_owned());
        assert_eq!(
            due(&row),
            Some(Due::Checks),
            "checks passed for another certificate are no evidence"
        );

        row.stage = "production".to_owned();
        assert_eq!(due(&row), None);
    }
}
```

Add to `OnboardError` in `onboarding.rs`, after `NotYet`:

```rust
    /// The samples are issued by the business being onboarded, and there is
    /// no business registered to issue them as.
    #[error("this tenant has no ZATCA registration to sign the compliance samples as")]
    NotRegistered,
```

`modules/tax_sa/src/zatca/mod.rs`: add `pub mod finish;` after `pub mod csr;`. Remove any temporary `#[allow(dead_code)]` from Task 3.

- [x] **Step 5: Run the guards**

```bash
cargo clippy -p tax_sa --all-targets -- -D warnings
cargo nextest run -p tax_sa --test tax_sa finish
cargo nextest run -p tax_sa --lib finish
```
Expected: clippy clean; three module tests and the unit test pass.

- [x] **Step 6: Falsify three ways, restoring after each.** (a) In `due`, delete the `refused_version` check → `a_refused_sample_is_recorded_and_waits_for_a_new_build` FAILS (twelve checks) and the unit test FAILS. (b) In `due`, replace the `checks_serial` branch with `Some(Due::Checks)` → `passed_checks_are_not_resent_when_going_live_fails` FAILS. (c) Delete the `record_checks_passed` call → the same test FAILS. Restore; all PASS.

---

### Task 6: The worker job

**Files:**
- Modify: `crates/erp-worker/src/bin/worker.rs` (`SubmitToZatca` ~line 1044; `zatca_jobs` ~line 1113; names test ~line 1724)

**Interfaces:**
- Consumes: `tax_sa::onboarding`, `tax_sa::zatca::finish::{due, finish}`, `tax_sa::zatca::http::Fatoora::new`, `by_the_platform()` (exists in this file).
- Produces: job named `tax_sa.onboard`.

- [x] **Step 1: Extend the names test** (`a_deployment_with_a_sealing_key_both_signs_and_submits`):

```rust
        assert!(names.contains(&"tax_sa.onboard"), "{names:?}");
```
after the `tax_sa.submit` assertion. Run → FAIL:

```bash
cargo nextest run -p erp-worker --bin worker a_deployment_with_a_sealing_key
```

- [x] **Step 2: The job.** After the `SubmitToZatca` impl:

```rust
/// **Finishes an onboarding the route started.** A tenant holding a compliance
/// certificate is due six sample documents and a production certificate, and
/// nothing about either needs the taxpayer. `finish` records what ZATCA refused
/// and leaves what it did not answer for the next pass.
struct FinishOnboarding {
    sealing: erp_eventlog::SealingKey,
}

#[async_trait::async_trait]
impl erp_worker::Job for FinishOnboarding {
    fn name(&self) -> &'static str {
        "tax_sa.onboard"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(tax_sa::module_id())
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        // The read model says whether anything is due (L7), and reading it is
        // all an idle tenant costs: never onboarded, live, or waiting on a
        // refusal this build cannot change.
        let mut conn = db.read().await?;
        let onboarded = tax_sa::onboarding(&mut conn).await?;
        drop(conn);
        let Some(onboarded) = onboarded else {
            return Ok(Activity::Idle);
        };
        if tax_sa::zatca::finish::due(&onboarded).is_none() {
            return Ok(Activity::Idle);
        }
        let environment: tax_sa::zatca::csr::Environment = onboarded
            .environment
            .parse()
            .map_err(erp_worker::BoxError::from)?;

        let zatca = tax_sa::zatca::http::Fatoora::new(environment)?;
        let finished = tax_sa::zatca::finish::finish(
            db,
            &self.sealing,
            &zatca,
            chrono::Utc::now(),
            &by_the_platform(),
        )
        .await?;

        if let Some(step) = finished.refused {
            tracing::warn!(
                tenant = %db.tenant(),
                step = step.as_str(),
                "ZATCA refused an onboarding step; waiting for a new build or a new OTP"
            );
        }
        if finished.production.is_some() {
            tracing::info!(tenant = %db.tenant(), "ZATCA onboarding finished; the tenant is live");
        }
        Ok(if finished.did_something() {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}
```

In `zatca_jobs`, add after the `SubmitToZatca` entry:

```rust
        Arc::new(FinishOnboarding {
            sealing: sealing.clone(),
        }),
```

- [x] **Step 3: Run → PASS**

```bash
cargo clippy -p erp-worker --all-targets -- -D warnings
cargo nextest run -p erp-worker --bin worker a_deployment_with_a_sealing_key
```

- [x] **Step 4: Falsify.** Remove the `FinishOnboarding` entry from `zatca_jobs` → FAIL. Restore → PASS.

---

### Task 7: The routes — OTP in, certificate out, status that says where it stands

**Files:**
- Modify: `modules/tax_sa/src/messages.rs`
- Modify: `modules/tax_sa/src/http.rs`: `OnboardingRequest` (~line 941), `begin_onboarding` (~line 1066), `OnboardingView` (~line 1021), `onboarding_status` (~line 1185), old `unit_for` (~line 1233) and `unit_from` (~line 1583), `ActivationRequest`/`ActivationView`/`activate` (~lines 1372–1543)
- Test: `crates/erp-api/tests/http.rs` (new test after `a_certificate_is_checked_against_the_key_it_is_meant_for`; the two existing onboarding tests keep working because unknown request fields are ignored)

**Interfaces:**
- Consumes: `Registration.industry` (Task 1), `Onboarded` fields (Task 3), `finish` (Task 5, from the HTTP test).
- Produces: request bodies `OnboardingRequest { environment, branch? }`, `ActivationRequest { environment, otp, branch? }`; response `ActivationView { compliance, state, checks_expected }` with **202**; `OnboardingView` gains `state`, `checks`, `refusal`; message codes `tax_sa.no_industry` (400), `tax_sa.already_live` (409).

- [x] **Step 1: Messages.** In `messages.rs` add the constants after `NO_SUCH_DOCUMENT`, add both to `CODES`, and append to `ENTRIES` before the closing `];`:

```rust
pub const NO_INDUSTRY: MessageCode = MessageCode::new("tax_sa.no_industry");
pub const ALREADY_LIVE: MessageCode = MessageCode::new("tax_sa.already_live");
```
```rust
    (
        NO_INDUSTRY,
        Locale::English,
        Template::Simple(
            "The registration has no industry, and ZATCA's certificate names one. Add the industry to the registration first.",
        ),
    ),
    (
        NO_INDUSTRY,
        Locale::Arabic,
        Template::Simple(
            "لا يتضمن التسجيل مجال النشاط، وشهادة هيئة الزكاة والضريبة والجمارك تذكره. أضِف مجال النشاط إلى التسجيل أولًا.",
        ),
    ),
    (
        ALREADY_LIVE,
        Locale::English,
        Template::Simple(
            "This business is already live with ZATCA in {environment}. Onboarding again would replace the key its certificate is bound to; renew or replace the certificate through the manual onboarding route instead.",
        ),
    ),
    (
        ALREADY_LIVE,
        Locale::Arabic,
        Template::Simple(
            "هذه المنشأة مفعّلة لدى هيئة الزكاة والضريبة والجمارك في بيئة {environment} بالفعل. إعادة التسجيل تستبدل المفتاح المرتبط بشهادتها؛ جدِّد الشهادة أو استبدلها عبر مسار التسجيل اليدوي.",
        ),
    ),
```

- [x] **Step 2: Write the failing HTTP test** in `crates/erp-api/tests/http.rs` after `a_certificate_is_checked_against_the_key_it_is_meant_for`. It needs a registrar that issues; put it after `sign_certificate`:

```rust
/// A ZATCA that issues whatever it is shown, for driving the worker's half of
/// onboarding without a network. The route's half makes one real call and is
/// covered by the module tests with the same kind of fake.
#[derive(Debug, Default)]
struct IssuingZatca {
    checked: std::sync::Mutex<usize>,
}

fn issued_over(subject: &openssl::x509::X509NameRef, key: &openssl::pkey::PKey<openssl::pkey::Public>, request_id: &str) -> tax_sa::zatca::onboarding::CsidResponse {
    tax_sa::zatca::onboarding::CsidResponse {
        request_id: Some(serde_json::json!(request_id)),
        disposition: Some("ISSUED".to_owned()),
        token: Some(base64_encode(&sign_certificate(subject, key))),
        secret: Some("the-csid-secret".to_owned()),
        errors: None,
    }
}

#[async_trait::async_trait]
impl tax_sa::zatca::onboarding::Registrar for IssuingZatca {
    async fn compliance_csid(
        &self,
        _environment: tax_sa::zatca::csr::Environment,
        _otp: &tax_sa::zatca::onboarding::Otp,
        request: &tax_sa::zatca::onboarding::ComplianceRequest,
    ) -> Result<tax_sa::zatca::onboarding::CsidResponse, tax_sa::zatca::wire::Unanswered> {
        let csr = openssl::x509::X509Req::from_pem(&base64_decode(&request.csr)).expect("a CSR");
        Ok(issued_over(csr.subject_name(), &csr.public_key().expect("a key"), "compliance-1"))
    }

    async fn check_compliance(
        &self,
        _environment: tax_sa::zatca::csr::Environment,
        _compliance: &tax_sa::zatca::onboarding::Csid,
        _submission: &tax_sa::zatca::wire::Submission,
    ) -> Result<tax_sa::zatca::wire::Verdict, tax_sa::zatca::wire::Unanswered> {
        *self.checked.lock().expect("not poisoned") += 1;
        Ok(tax_sa::zatca::wire::Verdict::Accepted {
            warnings: vec![],
            stamped: None,
        })
    }

    async fn production_csid(
        &self,
        _environment: tax_sa::zatca::csr::Environment,
        compliance: &tax_sa::zatca::onboarding::Csid,
        _request: &tax_sa::zatca::onboarding::ProductionRequest,
    ) -> Result<tax_sa::zatca::onboarding::CsidResponse, tax_sa::zatca::wire::Unanswered> {
        // Over the same key: the production certificate replaces the
        // compliance one for the unit that earned it.
        let certificate = compliance.certificate().expect("a certificate");
        Ok(issued_over(
            certificate.subject_name(),
            &certificate.public_key().expect("a key"),
            "production-1",
        ))
    }

    async fn renew_csid(
        &self,
        _environment: tax_sa::zatca::csr::Environment,
        _production: &tax_sa::zatca::onboarding::Csid,
        _otp: &tax_sa::zatca::onboarding::Otp,
        _request: &tax_sa::zatca::onboarding::ComplianceRequest,
    ) -> Result<tax_sa::zatca::onboarding::CsidResponse, tax_sa::zatca::wire::Unanswered> {
        unreachable!("nothing is renewed in these tests")
    }
}
```

The test:

```rust
/// **One OTP, over HTTP, and the status says where it stands.** The
/// registration carries the industry; the route derives the unit and stops at
/// the compliance certificate; the worker finishes; asking again is refused
/// before a key is touched.
#[expect(clippy::too_many_lines, reason = "one story, told once, from registration to live")]
#[tokio::test]
async fn a_tenant_goes_live_from_one_otp_and_the_status_says_so() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_selling_only(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };
    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");

    // A registration from before the industry existed: no certificate can be
    // asked for until it is added, and the answer says so.
    tax_sa::register_taxpayer(
        &db,
        tax_sa::Registration {
            vat_number: "310122393500003".to_owned(),
            name: "أكمي للتجارة".to_owned(),
            name_latin: None,
            scheme: tax_sa::IdScheme::Crn,
            identifier: "1010101010".to_owned(),
            address: tax_sa::Address {
                street: "طريق الملك فهد".to_owned(),
                building: "2322".to_owned(),
                additional: None,
                district: "العليا".to_owned(),
                city: "الرياض".to_owned(),
                postal_code: "12211".to_owned(),
                country: "SA".to_owned(),
            },
            industry: None,
        },
        chrono::Utc::now(),
        &erp_eventlog::Metadata::default(),
    )
    .await
    .expect("registers");
    fixture.project_tax(tenant).await;
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/tax_sa/zatca/onboarding"))
                .body(Body::from(r#"{"environment":"simulation"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "tax_sa.no_industry");

    // With the industry, and six digits or nothing before anything is generated.
    fixture.register_with_zatca(&token).await;
    fixture.project_tax(tenant).await;
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/tax_sa/zatca/onboarding/activate"))
                .body(Body::from(r#"{"environment":"simulation","otp":"12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.not_an_otp");

    // The route's half, by hand — its one network call is a fake's job in the
    // module tests. The unit is derived: the registered name is the O and, with
    // no branch given, the OU; the common name is minted.
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/tax_sa/zatca/onboarding"))
                .body(Body::from(r#"{"environment":"simulation"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["compliance_documents"], 6);
    let request =
        openssl::x509::X509Req::from_pem(&base64_decode(body["csr"].as_str().expect("a CSR")))
            .expect("a certificate request");
    let subject: Vec<String> = request
        .subject_name()
        .entries()
        .map(|e| {
            format!(
                "{}={}",
                e.object().nid().short_name().expect("a name"),
                e.data().as_utf8().expect("utf8")
            )
        })
        .collect();
    assert!(subject.contains(&"O=أكمي للتجارة".to_owned()), "{subject:?}");
    assert!(subject.contains(&"OU=أكمي للتجارة".to_owned()), "{subject:?}");
    assert!(
        subject.iter().any(|e| e.starts_with("CN=EGS-")),
        "{subject:?}"
    );
    let (status, body, _) = fixture
        .send(
            bearer(Request::put("/v1/tax_sa/zatca/onboarding/certificate"))
                .body(Body::from(
                    serde_json::json!({
                        "stage": "compliance",
                        "environment": "simulation",
                        "token": certificate_over(&request),
                        "secret": "the-csid-secret",
                        "request_id": "compliance-1"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    fixture.project_tax(tenant).await;

    let (status, body, _) = fixture
        .send(
            bearer(Request::get("/v1/tax_sa/zatca/onboarding"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "checking");
    assert_eq!(body["live"], false);
    assert!(body["checks"].is_null());
    assert!(body["refusal"].is_null());

    // The worker's half.
    let zatca = IssuingZatca::default();
    let finished = tax_sa::zatca::finish::finish(
        &db,
        &erp_eventlog::SealingKey::new("test", &[5u8; 32]).expect("32 bytes"),
        &zatca,
        chrono::Utc::now(),
        &erp_eventlog::Metadata::default(),
    )
    .await
    .expect("finishes");
    assert_eq!(
        finished.checks.as_ref().map(|c| (c.submitted, c.passed)),
        Some((6, 6))
    );
    assert!(finished.production.is_some());
    fixture.project_tax(tenant).await;

    let (status, body, _) = fixture
        .send(
            bearer(Request::get("/v1/tax_sa/zatca/onboarding"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "live");
    assert_eq!(body["live"], true);
    assert_eq!(body["checks"]["submitted"], 6);
    assert!(!body["checks"]["passed_at"].is_null());
    assert!(body["refusal"].is_null());
    assert_eq!(
        body["reached"],
        serde_json::json!(["compliance", "production"])
    );

    // Live: asking again is refused before any key is touched — and before
    // ZATCA is called, which is why this test can ask.
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/tax_sa/zatca/onboarding/activate"))
                .body(Body::from(r#"{"environment":"simulation","otp":"123456"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "tax_sa.already_live");

    fixture.cleanup().await;
}
```

(The fixture's sealing key is `SealingKey::new("test", &[5u8; 32])` at `crates/erp-api/tests/http.rs:125`; `finish` must unseal with the same one.)

- [x] **Step 3: Run, expect FAIL** (first on `tax_sa.no_industry`: the manual route still takes the old body):

```bash
cargo nextest run -p erp-api --test http a_tenant_goes_live_from_one_otp
```

- [x] **Step 4: Slim the request bodies.** Replace `OnboardingRequest` and its example with:

```rust
#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "environment": "simulation",
    "branch": "الفرع الرئيسي"
}))]
struct OnboardingRequest {
    /// `sandbox`, `simulation` or `production`. **Not a default** — the only
    /// visible difference is a string in the request, and a mistake onboards
    /// into the wrong authority rather than failing.
    environment: String,
    /// Only when this business wants its invoices distinct per branch: the
    /// branch this unit belongs to (for a VAT group member, their own
    /// 10-digit TIN). Absent, the unit is the whole business and carries its
    /// registered name.
    #[serde(default)]
    branch: Option<String>,
}
```

Delete `const fn yes()` if nothing else uses it (grep `yes()`). Replace `ActivationRequest` and `ActivationView`:

```rust
#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "environment": "simulation",
    "otp": "123456"
}))]
struct ActivationRequest {
    /// `sandbox`, `simulation` or `production` — whichever portal the OTP was
    /// generated in.
    environment: String,
    /// **The six digits the taxpayer generates in the Fatoora portal.** Valid
    /// for about an hour, used once, and never stored here.
    otp: String,
    /// Only when this business wants its invoices distinct per branch. Absent,
    /// the unit is the whole business.
    #[serde(default)]
    branch: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct ActivationView {
    /// The certificate the OTP bought. It can sign the compliance samples and
    /// nothing else.
    compliance: CertificateView,
    /// `checking`: the worker now submits the samples and asks for the
    /// production certificate. Watch `GET /v1/tax_sa/zatca/onboarding`.
    state: &'static str,
    /// How many sample documents the worker will submit.
    checks_expected: usize,
}
```

- [x] **Step 5: One way to build the unit.** Delete the old `async fn unit_for(tenant, body, locale)` (~line 1233–1275) and `fn unit_from(registration)` (~line 1583–1601). Add, near `registered_unit`:

```rust
/// The unit, from the registration and at most a branch.
///
/// The VAT number, the legal name, the address and the industry are the
/// registration's — a second endpoint restating them is how the certificate
/// ends up naming a different business from the invoices. Both document types
/// are always declared. The serial and the common name are minted here: they
/// identify this unit to ZATCA and nobody has a better name for it.
fn unit_for(
    registration: &crate::Registration,
    branch: Option<&str>,
    locale: Locale,
) -> Result<crate::zatca::csr::Unit, Problem> {
    let industry = registration
        .industry
        .as_deref()
        .map(str::trim)
        .filter(|industry| !industry.is_empty())
        .ok_or_else(|| {
            Problem::new(
                StatusCode::BAD_REQUEST,
                &erp_i18n::Message::new(crate::messages::NO_INDUSTRY),
                locale,
                &CATALOG,
            )
        })?;
    let branch = branch
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .unwrap_or(registration.name.as_str());
    // The random tail of a v7 id: twelve hex characters, unique enough for the
    // one unit a tenant has, and nothing a person has to think up.
    let hex = uuid::Uuid::now_v7().simple().to_string();
    let serial = hex[hex.len() - 12..].to_owned();

    Ok(crate::zatca::csr::Unit {
        vat_number: registration.vat_number.clone(),
        organization: registration.name.clone(),
        branch: branch.to_owned(),
        common_name: format!("EGS-{serial}"),
        solution: SOLUTION.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        serial,
        address: format!(
            "{} {} {}",
            registration.address.street,
            registration.address.city,
            registration.address.postal_code
        ),
        industry: industry.to_owned(),
        issues: crate::zatca::csr::Issues::both(),
    })
}
```

In `begin_onboarding`, replace `let unit = unit_for(&tenant, &body, locale).await?;` with:

```rust
    let registration = registered_unit(&tenant, locale).await?;
    let unit = unit_for(&registration, body.branch.as_deref(), locale)?;
```

- [x] **Step 6: The onboarding read, shared.** Extract from `onboarding_status` the read of the row into a helper (used by the status and the guard):

```rust
/// The onboarding row, or a 500 that is ours: a read model this module owns
/// failing is not something a caller can act on.
async fn onboarding_row<C: erp_web::Capability>(
    tenant: &Allowed<C>,
    locale: Locale,
) -> Result<Option<crate::Onboarded>, Problem> {
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let onboarded = crate::projections::onboarding(&mut conn)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "reading the onboarding read model failed");
            Problem::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                &erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
                locale,
                &CATALOG,
            )
        })?;
    drop(conn);
    Ok(onboarded)
}

/// **409 when this business is already live in that environment.** Asking again
/// would seal a new key and orphan the production certificate that clears its
/// invoices; a renewal or a key replacement is an operator's act through the
/// manual path. Read from the projection because the destructive step comes
/// before any command handler runs; two activations racing past it both get
/// valid certificates and the later one wins, which is harmless.
async fn refuse_if_live(
    tenant: &Allowed<ManageTenant>,
    environment: crate::zatca::csr::Environment,
    locale: Locale,
) -> Result<(), Problem> {
    let live_here = onboarding_row(tenant, locale).await?.is_some_and(|o| {
        o.stage == crate::zatca::onboarding::Stage::Production.as_str()
            && o.environment == environment.as_str()
    });
    if live_here {
        return Err(Problem::new(
            StatusCode::CONFLICT,
            &erp_i18n::Message::new(crate::messages::ALREADY_LIVE).with(
                "environment",
                erp_i18n::MessageArg::text(environment.as_str().to_owned()),
            ),
            locale,
            &CATALOG,
        ));
    }
    Ok(())
}
```

- [x] **Step 7: `activate`.** Replace the handler, its doc comment and its `utoipa::path` with:

```rust
/// Start taking this business live with ZATCA, from a Fatoora OTP.
///
/// This request spends the OTP: a key pair and a certificate request are
/// generated here, the OTP buys the compliance certificate, and both are
/// sealed. **The worker does the rest** — one signed sample of every document
/// type, then the production certificate — and `GET
/// /v1/tax_sa/zatca/onboarding` says where it stands. Nothing after this
/// request needs the taxpayer or a second OTP.
///
/// The unit is the registration's: VAT number, legal name, address and
/// industry. Both document types are declared. A `branch` is only for a
/// business that wants its invoices distinct per branch.
#[utoipa::path(
    post,
    path = "/v1/tax_sa/zatca/onboarding/activate",
    tag = "tax_sa",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = ActivationRequest,
    responses(
        (status = ACCEPTED, description = "The compliance certificate is sealed; the worker is finishing. Watch the status.", body = ActivationView),
        (status = BAD_REQUEST, description = "An OTP that is not six digits, an unknown environment, or a registration with no industry", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, the module is not enabled, or nothing is registered with ZATCA yet", body = Problem),
        (status = CONFLICT, description = "Already live in this environment. Renew or replace the certificate through the manual route.", body = Problem),
        (status = BAD_GATEWAY, description = "ZATCA refused the OTP, or could not be reached. Nothing was stored.", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "This deployment has no sealing key", body = Problem),
    ),
)]
async fn activate(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<ActivationRequest>,
) -> Result<(StatusCode, Json<ActivationView>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let sealing = sealing(&state, locale)?;
    let environment = environment_of(&body.environment, locale)?;

    let otp = body
        .otp
        .parse::<crate::zatca::onboarding::Otp>()
        .map_err(|_| {
            // The value itself never reaches the message: an OTP in a log is a
            // certificate somebody else can obtain for an hour.
            bad_request(erp_web::messages::NOT_AN_OTP, "otp", "", locale)
        })?;
    let registration = registered_unit(&tenant, locale).await?;
    let unit = unit_for(&registration, body.branch.as_deref(), locale)?;
    refuse_if_live(&tenant, environment, locale).await?;

    let fatoora = crate::zatca::http::Fatoora::new(environment).map_err(|source| {
        onboarding_problem(
            &crate::zatca::onboarding::OnboardError::Unanswered {
                step: "building a client",
                source,
            },
            locale,
        )
    })?;
    let compliance = crate::zatca::onboarding::Onboarder::new(&tenant.db, sealing, &fatoora)
        .onboard(&unit, environment, &otp, Utc::now(), &metadata(&tenant))
        .await
        .map_err(|e| onboarding_problem(&e, locale))?;

    // So the worker finishes within a visit rather than on its schedule.
    nudge(&state, tenant.db.tenant()).await;

    Ok((
        StatusCode::ACCEPTED,
        Json(ActivationView {
            compliance: certificate_view(compliance),
            state: "checking",
            checks_expected: unit.issues.compliance_documents(),
        }),
    ))
}
```

`erp_web::messages::COMPLIANCE_REFUSED` is no longer used here; leave the constant in `erp-web`.

- [x] **Step 8: The status.** `OnboardingView` gains, after `issued_at`:

```rust
    /// `none`, `checking` (the worker is submitting samples or asking for the
    /// production certificate), `refused` (ZATCA said no — see `refusal`), or
    /// `live`.
    state: &'static str,
    /// The samples, once they all passed for the current certificate.
    checks: Option<ChecksView>,
    /// What ZATCA refused about the current certificate, if anything.
    refusal: Option<RefusalView>,
```
with

```rust
#[derive(Debug, Serialize, ToSchema)]
struct ChecksView {
    submitted: i32,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    passed_at: Timestamp,
}

#[derive(Debug, Serialize, ToSchema)]
struct RefusalView {
    /// `compliance_checks` or `production_certificate`.
    step: String,
    /// ZATCA's words, or the first refused document and its first error.
    detail: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    at: Timestamp,
}
```

In `onboarding_status`, replace the inline read with `let onboarded = onboarding_row(&tenant, locale).await?;` and build:

```rust
    let state = match &onboarded {
        Some(o) if o.stage == crate::zatca::onboarding::Stage::Production.as_str() => "live",
        Some(o) if o.refused_step.is_some() => "refused",
        Some(_) => "checking",
        None => "none",
    };
    let checks = onboarded.as_ref().and_then(|o| {
        Some(ChecksView {
            submitted: o.checks_submitted?,
            passed_at: o.checks_passed_at?,
        })
    });
    let refusal = onboarded.as_ref().and_then(|o| {
        Some(RefusalView {
            step: o.refused_step.clone()?,
            detail: o.refused_detail.clone()?,
            at: o.refused_at?,
        })
    });
```
and add `state, checks, refusal,` to the `OnboardingView { … }` literal. (If clippy asks for `?` inside `and_then` closures to be written differently, use `o.checks_submitted.zip(o.checks_passed_at).map(|(submitted, passed_at)| ChecksView { submitted, passed_at })`.)

Update the `onboarding_status` doc comment to mention `state`.

- [x] **Step 9: Run**

```bash
cargo clippy -p tax_sa -p erp-api --all-targets -- -D warnings
cargo nextest run -p erp-api --test http a_tenant_goes_live_from_one_otp a_tenant_can_generate_a_signing_key a_certificate_is_checked_against
cargo nextest run -p tax_sa --test tax_sa onboard
```
Expected: clean and passing. The two older HTTP tests still send `common_name`/`serial`/`industry` in the body; `serde` ignores unknown fields, and their `compliance_documents == 6` assertion still holds.

- [x] **Step 10: Falsify.** (a) In `refuse_if_live`, change `if live_here` to `if false` → FAIL on `CONFLICT`. Restore. (b) In `unit_for`, replace the `ok_or_else(…)?` on `industry` with `.unwrap_or("Services")` → FAIL on `tax_sa.no_industry`. Restore. (c) In `onboarding_status`, hard-code `state: "checking"` → FAIL on `"live"`. Restore; run → PASS.

---

### Task 8: Documents, generated files and gates

**Files:**
- Modify: `docs/IMPLEMENTATION.md` (status row ~line 2077; new §45 inserted **above** `### 44 ·` at ~line 654 — the write-ups run newest-first from §26)
- Modify: `docs/RUNNING.md` ("ZATCA, end to end", ~lines 382–408)
- Modify: `modules/tax_sa/src/zatca/onboarding.rs` header (lines 19–24)
- Regenerate: `docs/openapi.json`, `docs/openapi.baseline.json`, `.sqlx/`

- [x] **Step 1: `onboarding.rs` header.** Replace the paragraph beginning `**Steps 2 and 4 are separate calls here, and separate on purpose.**` (through `would hide that.`) with:

```text
//! **Step 2 is the route's and steps 3 and 4 are the worker's.** The OTP is the
//! taxpayer's proof of who they are for about an hour, and the one call that
//! needs it is answered while they wait. Everything after needs only the
//! compliance certificate sealed here, so [`finish`](super::finish) runs it from
//! the worker, retrying what ZATCA did not answer and recording what it
//! refused. [`Onboarder`] still exposes every step on its own, because the
//! manual path and the tests drive them one at a time.
```

- [x] **Step 2: `docs/RUNNING.md`.** In the registration `curl`, add `"industry":"Consulting",` after `"identifier":"1010101010",`. Replace step 2's comment and body with:

```bash
# 2. The OTP. This request generates the key, buys the compliance certificate
#    and answers 202; the worker submits the six samples and obtains the
#    production certificate on its next visit.
curl -s -X POST $API/v1/tax_sa/zatca/onboarding/activate -H "$H" -H "$A" -H 'content-type: application/json' -d '{
  "environment":"simulation","otp":"123456"}'
```
and after step 3's first `curl` add a line: `# `state` goes checking → live; `refusal` says what ZATCA refused, if anything.`

- [x] **Step 3: `docs/IMPLEMENTATION.md`.** Status row (line ~2077) becomes:

```markdown
- [x] Onboarding: key pair, CSR, OTP, compliance checks, production certificate.
      The route spends the OTP and the worker finishes (§45); the industry
      lives on the registration
```

Insert above `### 44 ·`:

```markdown
### 45 · The OTP is typed once, and the worker finishes the onboarding

**`activate` did everything in one request, and that was the problem.** Ten
network calls inside a request handler, five unit details beside the OTP, and
no way to resume: a failure after the compliance certificate left the tenant
half-way and the only way forward was a new OTP. Every other outbound call in
this system is the worker's, made after a route records the request; onboarding
was the exception.

**The split follows the credential.** The OTP is the taxpayer's proof of who
they are for about an hour, and the one call that needs it — the compliance
certificate — is answered while they wait, so the OTP is still never stored.
Everything after needs only that certificate, which is sealed here, so
`zatca::finish` runs the six samples and the production request from the
worker (`tax_sa.onboard`), reading the onboarding read model to decide what is
due (L7) and writing two new facts to the log: `ChecksPassed`, per certificate,
and `Refused`, naming the step, ZATCA's words and the build version. What ZATCA
did not answer is retried next pass; what it refused waits for a new build,
because the samples are generated here and the same build would be refused
again. `the_worker_finishes_what_one_otp_started`,
`a_refused_sample_is_recorded_and_waits_for_a_new_build` and
`passed_checks_are_not_resent_when_going_live_fails` are the tests; the build
version rule is `a_refusal_holds_this_build_and_releases_the_next`.

**The unit is derived, not typed.** The industry moved onto the registration,
where the VAT number, name and address already were; both document types are
always declared; the serial and common name are minted; and a branch is only
for a business that wants its invoices distinct per branch (per-branch units,
with their own keys and chains, are not built). The request is `environment`,
`otp` and at most `branch`. Both routes build the unit through one function, so
the manual CSR path takes the same body.

**Another environment starts over.** The read model kept the furthest stage
ever reached, so a tenant live in simulation that onboarded to production read
as "production" while holding simulation's credentials, and the submit sweep
would have sent real invoices with them. A compliance certificate for another
environment now resets the stage and forgets the old production credentials,
in the aggregate, the projection and the secret store.
`onboarding_into_another_environment_starts_from_compliance` and
`a_certificate_for_another_environment_starts_over` hold it.

**Over HTTP**, `a_tenant_goes_live_from_one_otp_and_the_status_says_so`
registers with an industry, refuses a registration without one, derives the
unit into the certificate's subject, drives the worker with a fake ZATCA, reads
`state`, `checks` and `refusal` from the status, and is refused with 409 when
it asks again while live. The route's one real call is covered by the module
tests with the same kind of fake.

**Two compatibility breaks, taken deliberately with `just baseline`:** the
activate response no longer carries the production certificate and the check
counts, and the registration body requires `industry`.
```

- [x] **Step 4: Generated files and gates.** In this order:

```bash
just openapi
cargo nextest run -p erp-api --test compatibility
```
Expected: the compatibility test **fails** naming `activate`'s response fields and `industry`. Then, the deliberate act the user approved:

```bash
just baseline
cargo nextest run -p erp-api --test openapi --test compatibility
cargo nextest run -p erp-api --test http every_role_against_every_endpoint
just prepare
SQLX_OFFLINE=true cargo check --workspace --all-targets
cargo fmt --all -- --check
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
```
Expected: all clean; the role matrix still counts 216.

- [x] **Step 5: Stop and summarize.** List what was built, every falsification with the line reverted, the two baseline breaks, and hand over `just check`. Do not commit.

---

## Self-review against the spec

- §1 registration `industry` → Task 1. §2 derived unit → Task 7 step 5. §3 route (validation order, 409, 202, manual route shares the body) → Task 7. §4 events/aggregate/commands → Task 3. §5 `finish`, `due`, worker job, manual-path tenants finished by the same job (the job keys on the row's stage, however it got there) → Tasks 5, 6. §6 read model → Task 3 step 6. §7 environment change → Tasks 3, 4. §8 status → Task 7 step 8. Error table → `finish` returns `Err` for unanswered, records refusals; route answers as today. Compatibility → Task 8 step 4. Tests → Tasks 1, 3, 4, 5, 6, 7 (names match the spec; the spec's `the_registration_needs_an_industry` is `an_industry_given_empty_is_refused`). Docs → Task 8.
- Types: `Step` and `Refusal` live in `onboarded.rs` and are re-exported from `tax_sa`; `finish.rs` imports `crate::onboarded::Step`; tests use `tax_sa::Step`. `Onboarded` field names are identical in Task 3, Task 5's unit test and Task 7. `pass_compliance_checks` takes `Issues` from Task 2 onward and every caller is listed.
- No placeholder text remains.
