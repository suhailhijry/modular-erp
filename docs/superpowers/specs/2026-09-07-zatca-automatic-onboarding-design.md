# ZATCA onboarding that finishes itself

*Design, 2026-09-07. Decided with the user in conversation; every section below
was approved before it was written down.*

## Goal

The taxpayer types the six-digit OTP from the Fatoora portal and nothing else.
The system takes the tenant from that OTP to a production certificate without
another request, reports where it stands, and never needs a second OTP because
something after the certificate failed.

## What exists

`POST /v1/tax_sa/zatca/onboarding/activate` already makes all four ZATCA calls
(compliance certificate, six compliance samples, production certificate) but:

- inside the HTTP request, holding the connection through ten network calls;
- with the OTP **plus** environment, branch, common name, serial and industry
  in the body;
- with no way to resume: a failure after the compliance certificate leaves the
  tenant half-way, and calling again generates a new key and needs a new OTP.

Every other outbound call in this system is made by the worker after a route
records the request. Onboarding is the exception.

## Decisions

| question | decision |
|---|---|
| Where the four calls run | **The route spends the OTP** (key, CSR, compliance certificate) and answers at once. **The worker finishes** (samples, production certificate). The OTP is never stored. |
| What the request carries | `environment`, `otp`, optional `branch`. Nothing else. |
| Industry | Part of the company registration (`PUT /v1/tax_sa/registration`), required. |
| Document types | Always both standard and simplified. No choice. |
| Branch | Optional. Absent means one unit for the whole company and the registered name is the certificate's OU. Per-branch units (separate keys, chains) are **not** built. |
| Environment | Stays in the request. The taxpayer chose it when they generated the OTP in either the simulation or the production portal. |
| Refusals | Recorded in the log with the step, ZATCA's reason and the build version. Not retried until the build version changes or a new activation happens. |
| Unanswered calls | Not recorded. Tried again on the next worker pass. |
| Changing environment | A compliance certificate for another environment resets the stage to compliance and forgets the old production credentials. |
| Never-pays / other tails | Out of scope. |

## Components

### 1. Registration gains `industry`

- `Registration.industry: Option<String>` with `#[serde(default)]` so events
  recorded before this decode. `Registration::check()` refuses an empty industry
  when one is given.
- `RegistrationBody.industry: String`, **required** on `PUT`. `GET` renders an
  absent industry as `""`.
- The registration is stored as JSONB, so no schema change.

### 2. The unit is derived, in one place

`unit_for(registration, branch: Option<&str>) -> Unit` in `modules/tax_sa/src/http.rs`,
used by both `activate` and the manual `begin_onboarding`:

| field | source |
|---|---|
| `vat_number`, `organization`, `address`, `industry` | the registration |
| `branch` (OU) | the request's `branch`, else the registered name |
| `serial` | 12 lowercase hex characters from a fresh UUID v4 |
| `common_name` | `EGS-<serial>` |
| `solution`, `version` | this software, as today |
| `issues` | `Issues::both()` |

A registration with no industry answers **400** `tax_sa.no_industry` ("register
the industry first").

### 3. The route

`POST /v1/tax_sa/zatca/onboarding/activate`, body
`{ environment, otp, branch? }`:

1. module enabled, sealing key present (503 otherwise, as today);
2. OTP is six digits (400 `NOT_AN_OTP`), environment is known (400);
3. registration exists (404 `NOT_REGISTERED`) and has an industry (400);
4. **409 `tax_sa.already_live`** when the onboarding read model says
   production in this environment. A live tenant asking again is a renewal or a
   key replacement, both operator acts through the manual path. A tenant at
   compliance, or refused, may activate again with a fresh OTP: the new
   certificate starts the checks clean (§6). The guard reads the projection
   because the destructive step, sealing a new key, comes before any command
   handler runs; two activations racing past it both produce valid
   certificates and the later one wins, which is harmless;
5. `Onboarder::onboard` as today: seal the key, send the CSR with the OTP,
   check the certificate against the key, seal it, record `CsidIssued`;
6. `nudge` the worker;
7. **202 Accepted** with the compliance certificate (`CertificateView`) and
   `checks_expected: 6`.

`POST /v1/tax_sa/zatca/onboarding` (manual CSR) takes the same slim body
`{ environment, branch? }` and the same `unit_for`. `PUT …/certificate` is
unchanged.

### 4. Two new events on the onboarding aggregate

```rust
OnboardingEvent::ChecksPassed { certificate_serial, submitted, at }
OnboardingEvent::Refused { step, detail, version, at }
// step: "compliance_checks" | "production_certificate"
```

`Onboarding` (the aggregate) gains `checks_passed_for: Option<serial>` and
`refused: Option<Refusal>`. Commands in `commands.rs`:

- `record_checks_passed` writes nothing when already recorded for that serial;
- `record_refusal` writes nothing when the same step, detail and version stand;
- `record_csid` unchanged, except: a **compliance** certificate for a different
  environment than the one on record resets `stage` to compliance (see §7).

Both events carry no secret. The OTP appears nowhere, as before.

### 5. The worker finishes

New file `modules/tax_sa/src/zatca/finish.rs`:

```rust
pub struct Finished { pub checks: Option<ComplianceChecks>, pub production: Option<Issued> }

pub async fn finish(db, sealing, registrar: &dyn Registrar, now, metadata)
    -> Result<Finished, OnboardError>
```

Reads the onboarding read model (L7), then:

1. stage is not `compliance` → nothing;
2. a refusal stands for this certificate and `version == CARGO_PKG_VERSION` →
   nothing;
3. checks not yet passed for this certificate → `pass_compliance_checks`
   (which now takes `Issues`, not a `Unit`, because the samples use nothing
   else); all passed → `record_checks_passed`; any refused →
   `record_refusal("compliance_checks", first failure, version)` and stop, full
   failure list to tracing as today;
4. `go_live`; `NotIssued` → `record_refusal("production_certificate", …)`;
   `Unanswered` → return the error, the next pass retries.

Worker job `FinishOnboarding` (`tax_sa.onboard`, module `tax_sa`) in
`zatca_jobs()` beside sign and submit: reads the environment from the read
model like `SubmitToZatca`, builds `Fatoora::new(environment)`, calls `finish`,
returns `Worked` when anything was recorded. A tenant that reached compliance
through the manual path is finished by the same job.

### 6. Read model

Columns appended to `proj_tax_sa.onboarding`:

```sql
checks_serial      TEXT,          -- the compliance certificate the checks passed for
checks_submitted   INTEGER,
checks_passed_at   TIMESTAMPTZ,
refused_step       TEXT,
refused_detail     TEXT,
refused_version    TEXT,
refused_at         TIMESTAMPTZ
```

Projection: `ChecksPassed` sets the first three; `Refused` sets the last
four; `CsidIssued { Compliance }` **clears all seven** (a new certificate starts
clean) and, for a different environment, sets `stage = 'compliance'` instead of
`GREATEST(…)`. `CsidIssued { Production }` clears the refusal.

`Onboarded` (the Rust row) gains the same fields as `Option`s.

### 7. Changing environment

In `accept_certificate` for `Stage::Compliance`: if the read model has a
production certificate for a **different** environment, `secrets::forget`
the production credentials before recording. The aggregate and projection
reset the stage (§4, §6). `SubmitToZatca` then finds no production credentials
and idles until the new environment goes live, instead of sending real invoices
with simulation credentials.

### 8. Status

`GET /v1/tax_sa/zatca/onboarding` keeps every field and adds:

```json
"state": "none" | "checking" | "refused" | "live",
"checks": { "submitted": 6, "passed_at": "…" } | null,
"refusal": { "step": "compliance_checks", "detail": "COMPLIANCE-388-1 — BR-KSA-…", "at": "…" } | null
```

`state` is derived from the row: `live` if stage is production, `refused` if a
refusal stands, `checking` if stage is compliance, `none` otherwise. Nothing is
unsealed.

## Error handling

| failure | where | what happens |
|---|---|---|
| OTP wrong / expired | route, step 5 | 502 `CSID_NOT_ISSUED` with ZATCA's reason, as today. Nothing stored except an orphan key the next attempt overwrites. |
| ZATCA unreachable at the OTP exchange | route | 502 `ZATCA_UNREACHABLE` naming the step. Caller retries with the same OTP while it is valid. |
| a sample refused | worker | `Refused` recorded, status says `refused` with the first reason, full list in the log. Retried on the next build. |
| production request refused | worker | same, step `production_certificate`. |
| ZATCA unreachable in the worker | worker | logged, nothing recorded, retried next pass. Checks already passed are not resent. |
| worker crashes between a ZATCA answer and recording it | worker | the next pass repeats the step; `record_*` commands are idempotent. |

## Compatibility

Two breaks, accepted by the user, taken with `just baseline`:

- `activate`'s response loses `checks_submitted`, `checks_passed`, `production`.
- `PUT /v1/tax_sa/registration` gains a required `industry`.

Compatible: fields removed from request bodies (`serde` ignores unknown
fields), new response fields, the 202 and 409 statuses.

## Tests (each falsified: revert the fix, watch it fail, restore)

Module, `modules/tax_sa/tests/tax_sa.rs`, with the existing `FakeZatcaCa`:

- `the_worker_finishes_what_one_otp_started` — `onboard` then `finish`:
  six samples submitted, production issued, status row live; a second `finish`
  does nothing and submits nothing.
- `a_refused_sample_is_recorded_and_waits_for_a_new_build` — fake refuses one
  sample: `Refused` recorded with the version, second pass submits nothing.
- `passed_checks_are_not_resent_when_going_live_fails` — fake's production call
  is unanswered once: pass 1 records `ChecksPassed`, pass 2 submits no sample and
  goes live.
- `onboarding_into_another_environment_starts_from_compliance` — live in
  simulation, compliance in production: stage is compliance, production
  credentials gone.
- `the_registration_needs_an_industry` (taxpayer): empty industry refused.

HTTP, `crates/erp-api/tests/http.rs`:

- `a_tenant_goes_live_from_one_otp_and_the_status_says_so` — register with
  industry, `activate` with a bad OTP shape → 400, without industry → 400,
  manual certificate path to compliance, `finish` with the fake registrar (the
  same way the lender test drives the worker), status → `live`, `checks` 6,
  then `activate` again → 409.
- `every_role_against_every_endpoint`: no new operations. `just openapi`,
  `just baseline`.

Worker: the `zatca_jobs` names test gains `tax_sa.onboard`.

## Documentation

- `docs/IMPLEMENTATION.md`: status row for Phase 12 ZATCA onboarding, and §45
  recording what was built and why, with the test names.
- `docs/RUNNING.md` "ZATCA, end to end": the new request body, the 202, and
  watching the status.
- `modules/tax_sa/src/zatca/onboarding.rs` header: remove the stale "step 3 is
  the piece this build cannot finish".

## Out of scope, by decision

- Per-branch EGS units.
- Renewal over HTTP (the `renew` function exists; the certificate subject holds
  the unit details a renewal needs).
- Retrying a refusal on a timer.
- A deployment-level allow-list of environments.
