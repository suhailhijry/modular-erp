# Code review — the whole codebase, as of `b329e35` (2026-09-05)

A single reviewer read the write-side of every module, the security boundary
(`erp-web`, `erp-control`, `erp-api`), the kernel (`erp-eventlog`,
`erp-projection`, `erp-tenant`, `erp-occupancy`), the worker, and the outbox,
with targeted greps across all 103k lines of source for the defect classes that
turned up in the first pass. Tests were read only where a finding depended on
what they do or do not cover.

Every finding names a file and line in the tree at `b329e35`, says what breaks
and how, and says what would fix it. **Confirmed** means the mechanism was read
end to end and the trigger is ordinary use; **Plausible** means the mechanism
is real and the trigger needs timing, configuration, or a runtime test to
demonstrate.

The last section lists what was checked and found sound, so nobody re-chases
it.

Severity is about consequence, not effort: **High** loses money, data, or lets
a stranger do something; **Medium** produces wrong books, a wrong tax return,
a stuck state, or a real operational hole; **Low** is a foot-gun, a stale
promise, or a limit worth writing down.

---

## Status after the fixing passes (2026-09-05 and 2026-09-06; §G on 2026-09-06)

Every fix below was **falsified**: the fix was reverted, the named test was run
and watched fail, the fix was restored. A finding marked *fixed* has a test
that goes red if the defect comes back. Line numbers in the findings are still
those of `b329e35`.

| # | Status | Guard |
|---|--------|-------|
| A1 | fixed | `every_public_route_is_rate_limited`, `one_account_cannot_be_guessed_at_from_many_addresses` (erp-api `http`) |
| A2 | fixed | `one_address_cannot_cause_texts_to_many_numbers` |
| A3 | fixed | `a_caller_cannot_mint_addresses_by_prepending_to_the_forwarded_chain`; the limiter is shared through Redis when it is there |
| A4 | fixed | `register_device` is the owner's in `every_role_against_every_endpoint`; `a_device_can_only_belong_to_an_employee_or_a_customer` (messaging) |
| A5 | fixed | `a_held_field_cannot_be_set_on_a_customer_that_does_not_exist` |
| A6 | fixed | `verify_domain` reads the DNS TXT record the claim named (`_erp-challenge.<domain>`); `allow_origin` takes `https://<host>[:port]` under a *proved* domain only — `a_domain_is_proved_by_the_record_it_was_told_to_publish` (erp-control), `a_domain_is_proved_only_by_its_published_record`, `an_origin_must_be_https_under_a_proved_domain` (erp-api) |
| A7 | fixed | a proved domain serves the whole API on any host under it (`tenant_by_host`), and CORS carries the session, `If-Match`, `X-Branch` and every method — `a_proved_domain_serves_the_api_on_every_host_under_it`, `a_preflight_is_answered_at_the_edge` |
| A8 | fixed | `a_secret_moved_to_another_row_does_not_unseal`, `a_value_sealed_by_the_first_format_still_unseals` (erp-eventlog) |
| A9 | fixed (sweep) | `expired_sessions_are_swept_and_live_ones_are_not` (erp-control); per-identity revocation already existed |
| A10 | fixed | the attempts update is `?` — no test, because it needs a failing database |
| A11 | fixed | `an_unknown_route_and_a_wrong_method_answer_in_problem_json` |
| B1 | fixed, **confirmed** | `a_claimed_tenant_is_not_claimable_again_by_its_own_worker`, `a_tenant_is_visited_by_one_visit_at_a_time` — the falsification showed the double visit, so this was real, not plausible |
| B2 | fixed | `a_lease_is_renewed_only_by_its_holder_and_only_while_it_holds` |
| B3 | fixed | `one_failing_job_does_not_stall_the_others` |
| B4 | fixed | `a_dead_letter_can_be_requeued_and_is_then_delivered_under_its_own_key`; `GET /v1/effects/dead`, `POST /v1/effects/dead/{id}/requeue`; sixteen attempts capped at an hour |
| B5 | **refuted** | `claim_tenants` selects `status = 'active'`; a suspended tenant is never visited — see the note under the finding |
| B6 | fixed | a per-claim lease token (`leased_by`) and a heartbeat renewing it while the handler runs; settlement conditional on it — `a_delivery_slower_than_the_lease_is_renewed_and_not_claimed_again` |
| B7 | fixed | the lapse/secure tests in booking; the worker secures from `payments::settled_advances` |
| C1 | fixed | the refund tests in `modules/payments/tests/payments.rs`: a refund is a request, the worker calls the gateway, and the payment records what came back |
| C2 | fixed | `filing_a_period_closes_it_to_backdated_documents` (tax_sa) |
| C3 | fixed | `erp_types::Calendar` is the one clock: stamped on every event, read by projections from the event, by commands from configuration; tax periods are local dates; `an_instant_becomes_a_day_only_through_the_calendar` (erp-eventlog `write_side`) refuses every other conversion; `PUT /v1/tenant/calendar` sets it |
| C4 | fixed | `a_return_of_one_line_credits_that_line_and_leaves_the_rest`, `a_partial_return_whose_tenders_do_not_match_its_lines_is_refused` (pos) |
| C5 | fixed | `a_closed_till_refuses_a_return` |
| C6 | fixed | `a_package_with_no_uses_is_refused` (prepaid) |
| C7 | fixed | the deposit tests in payments: the amount comes from the aggregate, the kept net is apportioned, one deposit per booking through `Awaiting` |
| C8 | fixed | `approve_run` takes `at`; `no_module_reads_the_wall_clock` (erp-eventlog `write_side`) refuses `Utc::now()` outside a module's HTTP layer |
| D1 | fixed | `a_conditional_write_lands_only_on_the_version_it_read` (erp-eventlog), `a_stale_settings_write_is_refused_and_a_fresh_one_lands` (erp-api); every settings `GET` answers `ETag`, every `PUT` takes `If-Match` |
| D2 | fixed | `retention_forgets_old_receipts_and_keeps_young_ones_and_open_promises` (erp-worker), plus one sweep test per crate |
| D3 | fixed | advisory locks in `otp::request_code` and `booking::verification::issue` |
| D4 | fixed | the compatibility walker's cycle guard is per branch — `a_component_referenced_twice_is_walked_under_both_paths` |
| D5 | measured | kept by decision; `throughput.rs` prints the ceiling (~470 appends/s per tenant on the dev machine) and asserts a floor; recorded under L1 in ARCHITECTURE.md |
| D6 | fixed | doc corrected |
| E1 | fixed | `public_booking_settings_can_be_set_and_a_deposit_over_the_price_cannot` |
| E2 | fixed | docs corrected |
| E3 | left | by decision: the handbook is maintained by hand |
| E4 | fixed | `a_document_cannot_be_attached_to_a_record_that_does_not_exist` (files), `every_owner_kind_names_the_domain_its_module_uses` (erp-api `seams`) |
| G1 | fixed | `Returns.notification` is where a provider reports, never a page; Tamara sends it only when given and reads both of its bodies — `no_notification_url_means_none_is_sent`, `a_webhook_and_a_notification_are_both_read_and_told_apart` (erp-payments `tamara`), `a_tamara_callback_of_either_shape_is_accepted_and_told_apart` (erp-api `http`) |
| G2 | fixed | one `refusal` for every provider: a `404` about a named payment is the only absence — `only_a_404_about_a_named_payment_is_no_such_payment` (erp-payments), `a_fetch_is_no_such_payment_only_on_a_404` (`tamara`, `tabby`) |
| G3 | fixed | `a_partial_capture_reports_what_was_captured_not_the_order` (`tamara`), `closed_means_paid_only_when_something_was_captured` (`tabby`) |
| G4 | fixed | `Gateway::capture`/`refund` take the caller's reference; the sweep sends `<payment>.<reference>` — `a_refund_is_keyed_on_the_reference_and_not_on_the_amount` (`tabby`), `a_refund_carries_the_reference_as_its_comment` (`tamara`), `a_refund_is_carried_to_the_gateway_before_the_books_record_it` (payments) |
| G5 | fixed | `a_basket_line_totals_its_quantity_and_has_its_own_id` (`tamara`) |
| G6 | fixed | `Buyer` carries `registered_since` and `purchases`, `Basket` a `deliver_to`, `Item` a `category`; the body is built from them — `the_amount_goes_out_as_a_quoted_decimal_in_major_units` (`tabby`) |
| G7 | fixed | `transport::worth_retrying` (`408`, `429`, `5xx`) answers for every transport — `a_busy_relay_is_retried_and_a_refusing_one_is_not`, `credentials_are_permanent_and_an_outage_is_not` (messaging) |
| G8 | fixed | `a_401_forgets_the_access_token_and_the_retry_mints_another` (messaging `fcm`) |
| G9 | fixed | `erp_types::phone` is the one rule; `a_phone_number_is_read_in_one_place` (erp-eventlog `write_side`) refuses a second parser; `a_number_reaches_taqnyat_the_way_it_documents_them` (messaging) |
| G10 | documented | neither provider offers one; `Transport::send` says so, and delivery stays at-least-once by construction |
| G11 | fixed | `Revenue` has a `Credited` arm and `credited` is one row per credit note — `a_partial_credit_leaves_revenue_and_reconciles_like_a_document` (reports) |
| G12 | fixed | `a_rescheduled_booking_moves_its_count_to_the_month_it_went_to` (reports) |
| G13 | fixed | `two_lines_on_one_resource_are_one_booking_with_both_lines_minutes` (reports) |
| G14 | fixed | `undocumented` reconciles invoices and credit notes as one list of documents; the same test as G11 tampers with the credit's row and watches it found. Fixing it found the entry name was wrong too: `sales` posts `cn.<invoice>.<reference>`, and the report looked for the statutory number |
| G15 | fixed | `a_language_the_client_refused_is_never_chosen` (erp-i18n) |
| G16 | fixed | `arabic_arguments_are_bidi_isolated_in_english` (erp-i18n `catalog`) |
| G17 | fixed | `Seeded.colleague_password` is minted per demo and printed — `the_demo_has_somebody_who_cannot_do_everything` (erp-demo) signs in with it and is refused with the owner's |
| G18 | fixed | the demo asks for a saved-card charge instead — `the_demo_starts_no_payment_the_gateway_never_issued` (erp-demo) |

---

## Common roots

Thirty-six findings, ten causes. Each fix below is aimed at the cause, so that
the class of bug becomes unwritable rather than the instance being patched.
The finding ids in brackets are the ones each root accounts for.

**R1 · Nothing knows who an unauthenticated caller is.** No code path ever
derives a client address; the one rate limiter keys on the caller-chosen
`Origin` header and lives in a single process; login, signup, invitation
acceptance and OTP requests have no limiter at all, so the only defences on
those surfaces are per-handle cooldowns that are themselves check-then-insert.
[A1, A2, A3, D3] *Fix:* a `Caller` derived from a trusted proxy header or the
socket; a limiter shared through the Redis layer that already exists; an
extractor every `security()` route **must** take, enforced by a test that
hammers each public operation until it sees a 429; issuance caps on codes per
caller and per platform.

**R2 · A guard keyed on an identity the caller typed.** The handler trusts a
body or query field to say *whose* record this is, when the check it needed
(`accepts_documents`, the session identity, `hr::exists`) is one call away and
used everywhere else. [A4, A5, E4, A6] *Fix:* the check moves into the
command, where every other caller of the same command gets it too, with a
test per route.

**R3 · Liveness state written once and never refreshed.** The worker lease,
`next_visit_at`, and the outbox lease are all set at claim time and touched
again only at completion, so anything slower than the timeout — or the very
next loop iteration — becomes a second concurrent actor. [B1, B2, B6] *Fix:*
claiming advances `next_visit_at`; renewal is an explicit call between jobs;
a visit that loses its lease stops.

**R4 · An irreversible action decided from transient or lagging state.**
The booking↔payments join is derived from one pass's in-memory result; hold
expiry cancels from a projection with no aggregate guard; `collect_awaited`
overwrites the amount it should have checked against; the deposit route reads
a lagging projection to decide whether a charge already exists. [B7, C7]
*Fix:* repair sets come from durable queries; the refusal lives in the
aggregate command (`lapse_in`); `start_in` keeps what it was asked; one
deposit charge per reservation, by construction.

**R5 · "Recorded" standing in for "done".** The refund route writes the
books and the credit note and never calls the gateway, while all three
gateway adapters implement `refund`. [C1] *Fix:* the same shape a charge
already has — record the intent, execute from the worker, settle from `fetch`.

**R6 · Facts about time with no anchor.** Filing a return does not fence the
period it filed; periods are labelled from UTC instants in a UTC+3 market; a
command stamps the wall clock; configuration writes carry a version nobody
checks. [C2, C3, C8, D1] *Fix:* filing closes the books through the period
end in the same transaction; `configuration::set` takes the version the
caller read; commands take `at`. The timezone model is a design decision
(where does a tenant's fiscal zone live?) and is deferred with that said.

**R7 · Failure handling that stops too much or too little.** One failing job
stalls every job for a tenant; a dead-lettered effect is dead for ever; a
failed attempts-counter update is `let _`; suspended tenants keep having their
cards charged. [B3, B4, A10, B5] *Fix:* stall per job; dead letters listable
and requeueable with a longer tail; count attempts or refuse; jobs declare
whether they run while suspended.

**R8 · Nothing forgets.** Delivered outbox rows, webhook payloads, expired
sessions, past occupancy claims and expired short links all grow without a
sweep. [D2, A9] *Fix:* retention sweeps on the worker, one per table.

**R9 · Promises without a compiler.** A setting with no setter; route docs
describing a previous phase; a handbook nothing tests; a plain-text 404 on an
API that promises `problem+json`; a limiter doc that says there is no limiter.
[E1, E2, E3, D6, A7, A11] *Fix:* the setter, the fallback handler, and the
doc corrections; the handbook needs a test that reads it, which is its own
piece of work.

**R10 · The till and the prepaid ledger stop one step short.** A return
cannot be partial; a refund can land on a closed shift; a paid entitlement
can be granted with zero uses. [C4, C5, C6] *Fix:* line references on a
return, the same open-shift check `pay_out` already has, and a validation.

---

## A · Security boundary

### A1 · High · Unauthenticated Argon2 endpoints have no rate limit at all — `crates/erp-api/src/routes.rs:443`, `crates/erp-control/src/signup.rs:202`

`POST /v1/sessions` (`log_in`), `POST /v1/signups` (which calls
`authenticate` when the address already has an account), and
`POST /v1/join/{token}` all verify a password with Argon2 and none of them
sits behind a limiter. The only limiters in the system are per-API-key
(`Authenticated`) and per-public-origin (`Public`); the login and signup
handlers take `State` and `Json` and nothing else.

*Consequence.* Two attacks, both trivial. **Credential stuffing**: an
attacker tests `(email, password)` pairs against real accounts at network
speed; `authenticate` is careful to be constant-time so it does not leak
*which* half was wrong, but nothing bounds *how many* guesses. **CPU
exhaustion**: every guess costs the server one Argon2 hash (tens of
milliseconds of CPU); a small botnet against `/v1/sessions` starves every
other request. The plan defers "a per-caller rate limit on signup" but does
not mention login, which is the worse surface.

*Fix.* A per-address limiter in front of all three (the `Limiter` already
exists — it needs a client-address key, see A3), plus an account-level
lockout or exponential delay after N failures on one handle.

### A2 · High · SMS pumping: codes can be requested for unlimited distinct numbers — `crates/erp-control/src/otp.rs:140`, `crates/erp-api/src/codes.rs:106`

`request_code` enforces a 60-second cooldown **per number** and nothing else.
`POST /v1/codes` has no limiter (not even `Public`). An attacker who owns, or
has a revenue-share on, a block of premium numbers requests a code for each of
them once a minute, and the platform pays for every text. This is SMS-pumping
fraud, a common and expensive attack on any OTP form.

The same shape exists on `POST /v1/booking/public/verifications`, bounded
there only by the tenant's `Public` budget of 600 requests a minute — which
is 600 texts a minute at the tenant's expense.

*Fix.* A per-caller and a global cap on code issuance (numbers per hour per
address; texts per hour per tenant and per platform), and a circuit breaker
on the messaging spend meter, which already exists for tenants.

### A3 · Medium · The public rate limiter is keyed on an attacker-controlled header and lives in one process — `crates/erp-web/src/extract.rs:349`, `crates/erp-web/src/rate.rs`, `crates/erp-web/src/state.rs:58`

`Public` charges the limiter with `Origin` (or `"anonymous"` when absent).
`Origin` is whatever the client sends, so a flood rotates it and gets a fresh
60/minute per fake origin; a flood that omits it shares one 60/minute bucket
with every legitimate non-browser caller. The only real bound is the
per-tenant 600/minute — enough to take a tenant's booking site offline from
one laptop. No client IP is used anywhere (nothing reads `X-Forwarded-For` or
`Forwarded`), and the `Limiter` is an in-memory `RwLock<HashMap>` per API
process, so N pods multiply every limit by N.

The doc comment on `Public` (`extract.rs:306`) still says *"Nothing here
rate-limits"* and promises scoping "by origin, address and the tenant";
address never arrived.

*Fix.* Trust a configured proxy's forwarded address, key the per-caller bucket
on it, and back the limiter with the Redis `Shared` layer that already exists
for cache invalidation.

### A4 · Medium · Any member can point another person's push notifications at their own device — `modules/messaging/src/http.rs:681`, `modules/messaging/src/push.rs:80`

`register_device` takes `Allowed<Read>` and stores `(token, recipient)` with
`recipient` straight from the body ("whoever the device belongs to, in
whatever id space the caller uses"). Nothing checks that the caller *is* that
recipient or may act for them. `ON CONFLICT (token) DO UPDATE SET recipient`
even lets a token be re-pointed.

*Consequence.* A viewer registers their phone's token with
`recipient: "CUST-0001"` (or an operator's id) and receives that person's
reminders and staff notifications — including the booking short links inside
them. A push template addressed to *the operator* goes to whoever claimed the
operator's id first.

*Fix.* Bind `recipient` to the caller's identity for operator devices; for
customer devices, require a proof the customer holds (a verified phone, a
signed-in customer account once one exists) rather than a bare id.

### A5 · Medium · Custom-field values can be stored under a customer that does not exist — `modules/crm/src/http.rs:1104`

Reported in the range review and repeated here because it is the same class
as A4 and E4: `set_held_fields` writes health/personal values for any string
that parses as an `AggregateId`, `held` never shows them, `orphaned` (which
checks field keys, not customers) never finds them. A PDPL erasure request
cannot find data filed under a typo.

*Fix.* `crm::accepts_documents` inside the same transaction, as every other
command that names a customer already does.

### A6 · Low · `allow_origin` accepts any string and does not require the domain to be verified — `crates/erp-control/src/lib.rs:665`

The origin is lowercased and stored; there is no check that it is
`scheme://host[:port]`, that it belongs to `domain`, or that `domain` passed
`verify_domain` (which, per the plan, currently verifies nothing). Today this
only widens which browsers may call the **public** routes, which anybody can
call without a browser, so the blast radius is small — but the moment CORS
serves authenticated routes (A7) it becomes the whole tenant.

### A7 · Low · CORS only works for the public surface — `crates/erp-web/src/cors.rs:53`

`ALLOWED_HEADERS` omits `authorization` and `x-branch`, preflight allows only
`GET, POST`, and `Access-Control-Allow-Credentials` is never set. A tenant's
own web app on an allowed origin therefore cannot call any authenticated
route (cookie or bearer, PUT/PATCH/DELETE all fail preflight). If that is the
intent — CORS is for the booking site and nothing else — say so in the
`origins` route docs; today the allow-list reads as if it opens the API.

### A8 · Low · Sealed secrets are not bound to their key name — `crates/erp-eventlog/src/secrets.rs:137`

AES-256-GCM with a random nonce and an **empty AAD**. `module_secret` rows are
`(key, sealed, sealed_with)`, so anyone with SQL write access can copy the
blob from `payments.card.A` onto `payments.card.B` and it unseals. Passing the
row key as AAD costs nothing and makes a blob usable only under the name it
was sealed for.

### A9 · Low · Sessions never shrink and cannot be revoked in bulk — `crates/erp-control/src/auth.rs:437`

`sweep_sessions` exists and nothing calls it (the worker sweeps one-time
codes, not sessions), so expired rows accumulate forever. `log_out_everywhere`
and `suspend_identity` have no HTTP route (known, plan line 4787). A
suspended identity's live sessions still pass `Authenticated`; only `enter`
refuses them, so bare-`Authenticated` routes (`DELETE /v1/sessions/current`)
still answer — harmless today, worth remembering when the next such route is
added.

### A10 · Low · A database error silently disables OTP attempt counting — `crates/erp-control/src/otp.rs:257`

The `attempts = attempts + 1` update after a wrong guess is `let _ =`. Under
database trouble the guess is refused but not counted, so the five-guess
limit stops binding exactly when the system is degraded.

### A11 · Info · Unknown routes and wrong methods answer in plain text — `crates/erp-api/src/routes.rs:339`

No `.fallback` or method-not-allowed handler, so axum's defaults return a
bare `404`/`405` with no body. Every other failure in the API is
`application/problem+json`, a promise the contract test checks for routes it
knows about.

---

## B · Worker and background execution

### B1 · High (Plausible) · The worker re-claims tenants whose visits are still running — `crates/erp-control/src/lib.rs:759`, `crates/erp-worker/src/worker.rs:157`

`claim_tenants` returns tenants with `next_visit_at <= now()` whose lease is
free **or held by this owner** (`OR worker_lease_owner = $1`, deliberately, so
"renewing and claiming are the same call"). But `next_visit_at` is only moved
forward at the *end* of a visit (`Visit::reschedule`), and the worker's `run`
loop goes straight back to `claim_tenants` after spawning visits, with no
pause while claims are non-empty.

So: loop 1 claims tenant T and spawns a visit. Loop 2, milliseconds later,
finds T still due (`next_visit_at` unchanged), still leased to *this* owner,
and returns it again — a second concurrent visit of T in the same process.
This repeats until the `concurrency` semaphore is full of duplicate visits of
the same few tenants.

*Consequence.* Every job runs N times concurrently per tenant. Projections
are safe (`FOR UPDATE NOWAIT` → `Busy`); `settle_in` is idempotent; but
`charge_requested`'s fetch-before-charge guard is a read-then-act across two
gateway calls and two concurrent passes can both see `NoSuchPayment` and both
`charge()` — the double charge the guard exists to prevent. `ExpireUnpaidHolds`
and `BookingReminders` race the same way (harmless but wasteful).

*Verify.* A worker test with one due tenant, `concurrency: 4`, and a job that
records its entry count; the first pass should show more than one visit.

*Fix.* On claim, set `next_visit_at = now() + lease` (so a claimed tenant is
not due again until its lease lapses), and let the in-visit renewal be an
explicit `renew_lease` rather than a side effect of the claim query.

### B2 · Medium · Leases are never renewed during a visit — `crates/erp-worker/src/worker.rs`, `crates/erp-control/src/leases.rs:80`

The lease is 30 s and nothing extends it while a visit runs. A visit is up to
16 ticks of every job; `payments.settle` alone makes up to three gateway calls
per payment for 25 payments per provider, each with its own HTTP timeout. A
slow gateway pushes one tick past 30 s, at which point *another* worker
legitimately claims the tenant and the same concurrent-visit race as B1
occurs across pods. `claim_tenants`'s own comment ("recovered from by the
lease expiring — there is nothing to detect") is right for a dead worker and
wrong for a slow one.

*Fix.* Renew the lease between jobs inside `Visit::work`, and abort the visit
if renewal fails (the lease was lost).

### B3 · Medium · One failing job stalls every job for that tenant — `crates/erp-worker/src/worker.rs:373`

`Visit::work` returns on the first `Err` from any job ("this tenant is
stalled until it is fixed"). That is L6 applied at tenant granularity, and
the blast radius is wide: a tenant whose Tabby secret no longer parses
(`configured()` → `credentials.client()?` → `Err`) stops hold expiry, booking
reminders, ZATCA signing and submission, and the invariant checks — none of
which have anything to do with Tabby. The jobs are ordered so external
dependencies come late, which limits it, but `secrets::get` failures and
config decode errors can happen in any of them.

*Fix.* Stall per job (skip the failing one, keep a per-job failure record the
standing report shows), or at minimum keep running the jobs that come *before*
the failed one on the next tick rather than none.

### B4 · Medium · Dead letters are terminal and nothing can resurrect them — `crates/erp-eventlog/src/outbox/dispatch.rs:91`

`max_attempts: 8` with backoff capped at one minute means an effect is
dead-lettered after roughly eight minutes of failure. A provider outage
longer than that permanently dead-letters every email, SMS, WhatsApp message,
push, and payment callback enqueued during it. The only reader of `dead_at`
in the codebase is a `count(*)` in the outbox stats; there is no route, job,
or CLI that lists dead effects or requeues them. Recovery is hand-written
SQL.

*Fix.* A "dead letters" listing and a requeue command (set `dead_at = NULL,
attempts = 0, next_attempt_at = now()`), and a longer tail on the backoff
(hours) for kinds that are provider-dependent.

### B5 · Low–Medium · Background jobs run for suspended tenants — `crates/erp-control/src/lib.rs:459`

**Refuted on a second reading.** `enter_for_maintenance` does accept a
`Suspended` tenant, but nothing reaches it for one: the worker finds tenants
through `claim_tenants`, whose query selects `status = 'active'`, so a
suspended tenant is never claimed and none of its jobs run. The finding below
is kept as written because the mechanism it describes is one line away from
being true — a second caller of `enter_for_maintenance` that did not go through
`claim_tenants` would have exactly this problem.

*(2026-09-11, IMPLEMENTATION §59.)* It was one visit away from true once
something could suspend a tenant: a visit already under way ran every remaining
job, because `renew_lease` did not look at the status. It does now, and the
visit stops before its next job.

`enter_for_maintenance` refuses only `Deleted` and `Provisioning`. A
`Suspended` tenant (non-payment, abuse) keeps having its saved cards charged,
its deposits collected, its reminders sent, and its documents submitted to
ZATCA. Continuing ZATCA submissions is arguably a legal obligation;
continuing to *charge customers' cards* on behalf of a suspended business is a
decision that should be taken on purpose. `enter_for_the_public` and `enter`
both refuse suspended tenants, so the interactive and public surfaces go
dark while the money keeps moving.

### B6 · Low · Outbox redelivery on a slow provider — `crates/erp-eventlog/src/outbox/dispatch.rs`

The outbox lease is 30 s and delivery of an email or SMS can exceed it; a
second dispatcher then claims and redelivers. At-least-once is documented and
the idempotency key prevents *re-enqueueing*, but an SMS handler cannot be
idempotent at the provider, so the customer gets two texts. Worth a longer
lease for message kinds, or a lease renewal inside long deliveries.

### B7 · High · Settled deposits are never repaired into `booking`; hold expiry can cancel a paid booking — `crates/erp-worker/src/bin/worker.rs:1212`, `:747`

Reported in the range review; listed here because it is a worker defect. The
join from `payments` to `booking::secure_in` runs only over the payments
settled on *this* pass, so a failed `secure_in` is never retried and the
booking expires as unpaid. `ExpireUnpaidHolds` decides from the projection
and `move_to(Cancelled)` has no `secured_by` guard, so a deposit that settled
between the read and the write is cancelled anyway.

---

## C · Money and tax

### C1 · Medium · A gateway refund is recorded but never executed — `modules/payments/src/http.rs:372`, `crates/erp-payments/src/lib.rs:306`

`POST /v1/payments/{payment}/refunds` "**records and posts; it does not ask
the gateway**" — the doc is honest about it. `Gateway::refund` is implemented
for Moyasar, Tabby and Tamara and is called from nowhere in `modules/`. So
the product's only refund path writes `Refunded`, posts the ledger entry, and
issues the ZATCA credit note, while the customer's card is untouched unless an
operator separately refunds in the provider's dashboard and then comes here to
record it. Nothing in the route enforces that order, and a clerk who does it
in the other order has a set of books saying money went back when it did not
— the exact failure the doc warns about.

*Fix.* Either call `gateway.refund()` in the worker (the same shape as
`charge_requested`: record the intent, execute out of band, settle from
`fetch`), or take the `reference` as the provider's refund id and *verify it*
against `fetch` before recording.

### C2 · Medium · Filing a VAT return does not fence the period — `modules/tax_sa/src/commands.rs:131`, `modules/ledger/src/period.rs:92`

`file_in` computes the return, writes `Filed`, and refuses to file the same
period twice. It does not close the ledger period. `post_entry_in` enforces
`period::close`'s watermark for every posting, so *if* somebody runs
`ledger::close` the fence holds — but nothing ties the two together. A credit
note issued next month with `on` inside the filed quarter (the client supplies
`on`), or an invoice with `issued_on` in it, changes the `vat_entry` rows the
filed return was computed from, silently.

*Fix.* Filing should close the books through the end of the period in the
same transaction, or refuse to file while the period is open — one of the
two, chosen deliberately.

### C3 · Medium · Fiscal periods and tax points have no timezone — `modules/tax_sa/src/commands.rs:55`

Every tax point is a UTC instant and every period boundary is whatever
instant the caller sends. `period_id` labels the period with
`from.format("%Y-%m-%d")` in UTC. Saudi Arabia is UTC+3 with no DST, so a
quarter that starts at local midnight starts at 21:00Z the previous day: a
caller that passes `2026-04-01T00:00:00Z` (a natural mistake) puts every
invoice issued between midnight and 03:00 Riyadh on 1 April into the wrong
return. Nothing in `reports`, `sales`, or `tax_sa` uses `AT TIME ZONE` or a
tenant calendar; `booking` has a `Calendar` offset for opening hours that the
tax code does not share.

*Fix.* A tenant fiscal timezone (the booking `Calendar` is the obvious home),
periods expressed as local dates and converted once, and `period_id` labelled
from those dates.

### C4 · Medium · The till cannot do a partial refund — `modules/pos/src/commands.rs:364`, `:415`

`take_back` refunds the tenders through `sales::refund_in` and then calls
`sales::credit_in`, which is the **whole-invoice** cancellation and refuses
`HasPayments` while any money is still held. Returning one of two items
therefore rolls the whole transaction back with an error about payments that
has nothing to do with what the cashier did. `credit_part_in` exists, but
`take_back` has no line references to give it.

*Fix.* Take lines on `Return` and call `credit_part_in`; or, when the return
is for the full amount, `credit_what_is_clear`.

### C5 · Low–Medium · Refunds are accepted against a closed shift — `modules/pos/src/commands.rs:364`

`pay_out` refuses a closed shift; `sell` allows only a retry of an existing
sale; `take_back` checks nothing. A refund after `close_shift` changes the
drawer's expected cash after the variance entry has been posted, and the
variance is never revisited. Either refuse, or record it against the open
shift on the same till.

### C6 · Low · A paid entitlement can be granted with zero uses — `modules/prepaid/src/commands.rs:209`

`grant` refuses a negative or zero *value* for a paid grant but accepts
`uses: Some(0)`. The money is deferred and can never be redeemed
(`draw` refuses `left < wanted` with `wanted ≥ 1`); it is released only by
`revoke` or by `expire` if an expiry was set.

### C7 · High / Medium · Deposit amount check defeated; forfeit tax at today's rate; duplicate deposit charges — `modules/payments/src/sweep.rs:327`, `modules/payments/src/commands.rs:728`, `crates/erp-api/src/deposits.rs:186`

Reported in the range review; listed for completeness. `collect_awaited`
writes the gateway's amount onto `Started`, so `settle_in` compares the
gateway to itself; `tax_on_kept` resolves the *current* VAT rate instead of
using the `advance.net` already in hand; every fresh `Idempotency-Key` on the
public deposit route creates another charge against the same reservation.

### C8 · Low · A command stamps the wall clock — `modules/payroll/src/commands.rs:193`

`approve_run` writes `at: chrono::Utc::now()` inside the decide closure. Every
other command in the codebase takes `at` from the caller so a retried request
produces the same event. Harmless here because approval is idempotent, but it
is the one exception to a rule the rest of the code keeps.

---

## D · Kernel and infrastructure

### D1 · Medium · Configuration writes have no optimistic concurrency — `crates/erp-eventlog/src/config.rs:91`

`configuration::set` is an unconditional `ON CONFLICT (key) DO UPDATE`.
`Configured` carries a `version`, and nothing lets a caller say "only if the
version is still N". Every settings screen — tariff, deposit policy, custom
field definitions, message templates, GOSI schedule, chart, messaging budget
— is last-write-wins when two people edit at once, and the loser is never
told. For `crm.fields` this is the window in which one admin's removal of a
field and another's redefinition of it can interleave.

*Fix.* `set(conn, key, value, set_by, expected_version: Option<i64>)` with a
`WHERE version = $n` clause and a `Conflict` error; every route passes the
version it read.

### D2 · Low–Medium · Unbounded tables with no sweep

| table | grows by | swept by |
|---|---|---|
| `outbox` (delivered rows) | every effect ever sent | nothing |
| `webhook_event` (`crates/erp-api/src/hooks.rs:257`) | every provider callback, payload included | nothing |
| `session` (`crates/erp-control/src/auth.rs:437`) | every login | `sweep_sessions` exists, never called |
| `occupancy_claim` | every booking, forever | nothing (plan: "a retention policy") |
| `short_link` | every reminder | nothing after expiry |
| `customer_field` superseded rows | every edit | by design (history); erasure only |

None of these affect correctness — the partial indexes keep lookups fast —
but every one of them is a table that only ever grows, on a per-tenant
database that is cloned and backed up.

### D3 · Low · Cooldowns are check-then-insert — `crates/erp-control/src/otp.rs:150`, `modules/booking/src/verification.rs:82`

Both `request_code` and `verification::issue` `SELECT` the newest row and
then `INSERT`, with no lock or unique constraint on the handle. Two
concurrent requests for one number both pass the cooldown and both send. Fix
with `INSERT … WHERE NOT EXISTS` in one statement or an advisory lock on the
handle.

### D4 · Low · The compatibility gate's `seen` set is global, not per branch — `crates/erp-api/tests/compatibility.rs:320`

Reported in the range review: a component referenced from two properties has
its subtree walked once, so removed or newly-required fields under the second
occurrence are invisible to the gate.

### D5 · Info · Every append in a tenant serialises on one row — `crates/erp-eventlog/src/append.rs:110`

`UPDATE event_log_position … WHERE id` takes a row lock held to commit. That
is the L1 mechanism — commit-ordered, gapless positions — and it means a
tenant's write throughput is one transaction at a time across *all*
aggregates, not just contended ones. Documented as a design choice; named here
as the ceiling to measure before a large tenant finds it.

### D6 · Info · Stale doc on `Public` — `crates/erp-web/src/extract.rs:306`

"Nothing here rate-limits" — it does now (A3 says how well).

---

## E · Product and contract drift

### E1 · Medium · `PublicBooking` has no setter — `modules/booking/src/pricing.rs:107`

Reported in the range review. `booking.public` (`open`, `deposit_bp`,
`hold_minutes`, `verify_phone`) is only ever read; no route writes it, so the
public site, deposits, hold expiry and phone verification are unreachable in
the product. When a setter is added, bound `deposit_bp ≤ 10_000`.

### E2 · Low · Stale public-reservation docs — `modules/booking/src/http.rs:663`

"It takes no money … Phase 12a" and "not collected by this build" ship in
`openapi.json` after the deposit route was built.

### E3 · Low · The handbook has drifted and nothing guards it — `docs/book/`

`docs/book/src/api/http.md:3` says eighty operations across fifty-seven
paths; the generated document has 227 across 168. There is no `payments`
module chapter and `roadmap.md:59` says the system has never taken a payment.
`openapi.json` is test-guarded (`ERRORS.md` was retired: the catalogs are what matter, and they are); the handbook is the one
client-facing document nothing compiles.

### E4 · Low · Attachments can be filed against a record that does not exist — `modules/files/src/http.rs:252`

`upload_file` takes `owner_kind`/`owner_id` from the query and stores under
`{tenant}/{kind}/{owner}/{file}` without checking the owner exists. Same
class as A5; the consequence is orphaned files rather than orphaned PII.

---

## F · Checked and found sound

Listed so the next reviewer does not re-chase them.

- **Event log.** `append` reserves positions under a single row lock, so
  positions commit in order and `read_since (position > $1)` cannot skip a
  slower writer (L1). `try_create` treats a repeated create as a retry only
  when the request fingerprint matches; a *different* request against an
  existing id is `AlreadyExists`. Schema-version truncation to `i16` is
  bounded.
- **Projections.** `run_once_in` locks the checkpoint `FOR UPDATE NOWAIT`,
  applies the batch, and moves the checkpoint in one transaction (L4);
  `SET LOCAL search_path` plus a schema-qualified checkpoint update.
  `rebuild_swap` builds into a staging schema under the same lock and
  renames.
- **Numbering.** `reserve` is a lock, not an increment (`DO UPDATE SET next
  = next`); `consume` increments. An unconsumed reservation in a committed
  transaction leaves no gap — so the swallowed `HasPayments` inside a
  committed refund does not burn a credit-note number.
- **Money.** One rounding rule (`div_round_half_away`), half away from zero,
  symmetric; `apportioned(n, n)` is exact; no floats anywhere near money.
- **Credit notes.** Per-line and per-band caps both enforced; `cancel_in`
  refuses a partly-credited invoice, so cancellation-after-partials cannot
  double-negate `vat_entry`; `prepayment` is hardcoded `false` on the
  client-facing issue route; `post_entry_in` enforces the ledger period close
  for client-supplied `on`/`issued_on`.
- **UBL.** Line-level `cac:AllowanceCharge` before `cac:TaxTotal`, no
  `cac:TaxCategory`, `PriceAmount` = before allowances, `LineExtensionAmount`
  = after. `der()` in `csr.rs` handles the short form, `0x81`, and `0x82`
  correctly.
- **Auth.** Argon2 with a dummy hash on the miss path (no enumeration
  oracle); constant-time compare for API-key secrets and webhook HMACs;
  session cookie is `HttpOnly; SameSite=Strict; Secure`; bearer wins over
  cookie; `sk_` prefix separates key and session token spaces; API-key
  scopes narrow and never widen; module-scoped keys cannot reach non-module
  routes; `/v1/keys` is judged on the tenant-wide role.
- **Signup / invitations.** `register_hashed_login` is `DO NOTHING` +
  `HandleTaken` (the account-takeover path is closed); the pending-signup
  token is a digest; confirm-and-build resets `confirmed_at` on failure so the
  link survives a transient error; the last owner cannot be demoted or
  removed.
- **Webhooks.** Provider-specific authentication with a generic HMAC
  fallback; replay window of 300 s; dedup on `(provider, event_id)`; the
  callback is a doorbell and moves no money.
- **Secrets at rest.** AES-256-GCM, fresh nonce per seal, tag verified;
  `sealed_with` recorded. (AAD missing — A8.)
- **Storage.** `check_key` before `root.join` refuses `..`; downloads are
  always `attachment` with an RFC 5987 filename and never inline; body limit
  on the files router.
- **Occupancy.** Guards are inserted and locked in sorted order
  (deadlock-free); capacity checked against the peak of overlapping claims
  inside the lock.
- **Outbox.** `FOR UPDATE SKIP LOCKED` claim; attempts counted on claim;
  permanent-vs-retryable classification present in every handler; idempotency
  key dedup at enqueue.
- **Tenant isolation.** One database per tenant; `TenantDb` has no public
  constructor; `Public` yields a handle with no role so a public handler
  cannot reach a guarded command by omission; `module_of` judges unknown path
  segments on the tenant-wide role and the handler's `require_module` answers
  404.
- **Provisioning DDL.** Every `format!`ed identifier goes through
  `quote_ident`, which validates before quoting.
- **Booking bars.** All three doors (`reserve`, `reschedule`, `assign`)
  checked against the log; each falsified separately.

---

## G · Second pass — the parts the first pass did not read

Read on 2026-09-06: the three payment-gateway adapters' request bodies and
status mappings (`tamara.rs`, `tabby.rs`, `moyasar.rs`), the message
transports (`taqnyat.rs`, `fcm.rs`, `transport.rs`), the `reports` read models
and reconciliation, `erp-i18n` in full, `erp-recurrence` in full, and
`erp-demo`. Test files were scanned mechanically for tests that assert nothing
(two, both deliberate "does not panic" checks) and for `#[ignore]` (none).

One fact frames the gateway findings: **the hosted checkouts are not wired.**
`Gateway::charge` is called from exactly one place, the saved-card sweep
(`modules/payments/src/sweep.rs:437`), and only ever with a card token, which
Tamara and Tabby refuse by design. Their `charge` bodies are therefore latent —
wrong today, and the day somebody wires a "pay by instalments" button they
become the first thing that fails. `fetch`, `refund` and `void` on those two
*are* live, for payments recorded under their provider name.

### G1 · Medium (latent) · Tamara's webhook is sent to the customer's success page — `crates/erp-payments/src/tamara.rs:171`

`merchant_url.notification` is set to `charge.returns.success`. That is the
URL the *customer's browser* is sent back to; Tamara posts order-status
notifications to it, so every callback lands on the tenant's booking site
instead of `POST /v1/hooks/tamara`. Nothing breaks visibly because the
settle-from-fetch sweep catches up, but the webhook path — the fast one —
is dead for Tamara from the first order. `Returns` has no notification
field; it needs one, filled with the API's hook URL by whoever builds the
`Charge`.

### G2 · Medium · A gateway's "no" on any 4xx becomes "no such payment" — `crates/erp-payments/src/tamara.rs:224`, `crates/erp-payments/src/tabby.rs:230`

Both `fetch` implementations map *every* `Refused` — any 4xx other than
401/403/429, including a 400 for a malformed id or a 422 the gateway raises
on its own state — to `NoSuchPayment`. Moyasar's `fetch` maps only a 404.
The pending-settlement sweep treats `NoSuchPayment` as "still pending, warn";
the saved-card sweep treats it as "never created, charge it". A Tamara or
Tabby payment can never reach the saved-card path today, but the mapping is
the wrong shape for a code that callers branch on: only a 404 is an absence.

### G3 · Medium · A partial capture is recorded as fully paid — `crates/erp-payments/src/tamara.rs:490`, `crates/erp-payments/src/tabby.rs:402`

Tamara `partially_captured` maps to `Status::Paid` with `Charged.amount =
total_amount` (the order total, with `captured_amount` only a fallback);
Tabby `CLOSED` with any capture maps to `Paid` with `amount` = the payment's
authorised amount, the captures summed and then discarded. `settle_pending`
compares `Charged.amount` with what the invoice expects and posts the
receipt: a capture of 60 of 100 posts a receipt for 100. For a Paid status
the amount reported has to be the captured sum.

### G4 · Medium · A refund carries no reference of ours, so two equal refunds collide — `crates/erp-payments/src/lib.rs:306`, `crates/erp-payments/src/tabby.rs:266`, `:249`

`Gateway::refund(id, amount)` has no place for the refund request's own
`reference`, so Tabby derives its idempotency key from the amount
(`ref-{id}-{minor}`) and Tamara sends `"comment":"refund"`. Two refunds of
the same amount against one Tabby payment — a common shape: two of the same
item returned on different days — share a key; Tabby replays the first, the
gateway's `refunded` total does not move, and the sweep's reconciliation
(`modules/payments/src/sweep.rs:553`) refuses the second with
"the gateway reports … refunded in total", which is true and unhelpful.
Captures have the same shape. The trait should take the reference and each
adapter should send it as its idempotency key.

### G5 · Low (latent) · Tamara's basket lines total the unit price, whatever the quantity — `crates/erp-payments/src/tamara.rs:537`

`"quantity":{quantity},"unit_price":{price},"total_amount":{price}` —
a line of three at 50 is sent as total 50. Tamara validates that the lines
sum to the order; any basket with a quantity above one is refused at
checkout. Every item also carries the *basket's* reference as its
`reference_id` and `sku`, so all lines share one id.

### G6 · Low (latent) · Tabby's checkout body is thinner than Tabby's schema — `crates/erp-payments/src/tabby.rs:194`

`buyer_history: {}`, `order_history: []`, `shipping_address: {}`, and items
without a `category` — Tabby's documented request marks several of these
required, and its pre-scoring uses them. Whether Tabby answers 400 or scores
the buyer as unknown cannot be settled without a sandbox call; either way the
body should be filled from what `crm` knows (registration date, order count)
before this is wired.

### G7 · Low–Medium · A rate-limited message relay is a dead letter — `modules/messaging/src/transport.rs:252`, `modules/messaging/src/taqnyat.rs:233`

`Relay::send` maps every 4xx but 410 to `Refused`, which the handler turns
into `DeliveryError::Permanent`; Taqnyat's `refusal` does the same unless the
body says "SMS-API not responding". A `429 Too Many Requests` or a `408` is
therefore permanent: the text is dead-lettered on a busy minute. FCM's
`verdict` gets this right (429 and 401 are retryable); the other two should
match it.

### G8 · Low · An expired FCM token is retried, not replaced — `modules/messaging/src/fcm.rs:395`, `:231`

A `401` is classed retryable, but the cached access token is not invalidated
on it, so every retry within the token's remaining lifetime sends the same
rejected token. The five-minute refresh margin bounds the damage; a token
revoked early loops until it would have expired anyway. On 401, clear
`minted` before returning.

### G9 · Low · Taqnyat refuses numbers the rest of the system accepts — `modules/messaging/src/taqnyat.rs:162`

`msisdn` rejects punctuation (`+966-50-000-0000`) that
`booking::verification::normalise` strips. A customer phone entered with
dashes in `crm` reaches the transport as written and is refused
permanently. One normaliser, shared, would do.

### G10 · Info · Neither transport passes its idempotency key on — `modules/messaging/src/taqnyat.rs`, `modules/messaging/src/fcm.rs`

Both ignore `_key`, because neither provider offers idempotent sends. Delivery
is at-least-once by construction (the outbox docs say so); B6's lease
renewal removed the *slow-delivery* duplicate, and the crash-between-deliver-
and-settle duplicate remains, as documented. Noted so nobody expects
`idempotency_key` to reach the provider.

### G11 · Medium · Revenue reports ignore partial credit notes — `modules/reports/src/projections.rs:153`

`Revenue` handles `Issued` and `Cancelled` and drops everything else. Since
partial credits exist (`InvoiceEvent::Credited`, `sales::credit_part_in`, the
till's line returns), a credited line is revenue the report still counts:
net, tax and the `credited` count are all overstated, and the VAT figure on
the revenue report disagrees with the return `tax_sa` files. A `Cancelled`
after partial credits would then subtract the whole original again. The
projection needs a `Credited` arm that subtracts `totals` and, for
`Cancelled`, subtracts what remains.

### G12 · Low–Medium · A rescheduled booking's counts stay in its old month — `modules/reports/src/projections.rs:318`

`Rescheduled` deletes the `held` rows and re-holds with `Taken::Already`, so
no `booked` bump is made for the new period while the old period keeps the
one it got at `Reserved`. A booking moved from March to April shows March
booked +1 and April completed +1: utilisation for April exceeds 100% of what
was booked, and March shows a booking that never happened there. The old
period's `booked` should be decremented and the new one's incremented.

### G13 · Low · Two lines on one resource in a booking count once — `modules/reports/src/projections.rs:366`

`held` is keyed `(reservation, resource)` and the upsert replaces `minutes`,
so a booking with two services on the same stylist records the second line's
minutes only. Sum on conflict, or key by line.

### G14 · Low · Reconciliation never checks the credit entry — `modules/reports/src/reconcile.rs:163`

`credited()` stores `credit_entry` on `invoiced`, but `undocumented` only
compares the *issue* entry's debits with the document. A credit note whose
posting disagrees with its document — or that posted nothing — passes the
invariant.

### G15 · Low · A language the client refused can be chosen — `crates/erp-i18n/src/lib.rs:135`

`Accept-Language: ar;q=0, fr` picks Arabic: `q=0` means "not acceptable"
(RFC 9110 §12.4.2) but the first candidate is taken whatever its quality.
Skip candidates with `q=0`.

### G16 · Low · Bidi isolation depends on the message's language, not the argument's — `crates/erp-i18n/src/catalog.rs:183`

Arguments are wrapped in FSI…PDI only when the *locale* is Arabic. An
English message carrying an Arabic customer name (every `NoSuchCustomer`
with `{id}` a name) renders unisolated, and the surrounding punctuation
reorders. Isolate any argument that contains strong RTL characters, whatever
the locale.

### G17 · Info · The demo's second account shares the first's password — `crates/erp-demo/src/lib.rs:1811`

`seed_colleague` creates the viewer with `DEMO_PASSWORD`. Fine for a demo
that expires; worth a second variable the day a demo is shown to a prospect
with the owner's password on the screen.

### G18 · Info · The demo records a gateway payment nothing can settle — `crates/erp-demo/src/lib.rs:1360`

`seed_gateway_payment` records a Moyasar payment with an invented gateway id.
A worker pointed at the demo would `fetch` it, get Moyasar's 404
(`NoSuchPayment`), and log "the gateway has no record of a payment this
system started" on every visit for ever. Harmless in the demo's own tests,
which run no worker; noisy in a deployment that runs one against a demo
tenant.

### Checked in this pass and found sound

- `decimal.rs`: the round trip is exact for every exponent, refuses excess
  precision rather than rounding, and refuses overflow.
- `secrets_match` is length-then-constant-time; the Moyasar callback secret,
  the Tabby header and the Tamara HS256 JWT (`alg` pinned, `iss` and `exp`
  checked) all go through it.
- `Availability::covers` walks local days on the tenant's calendar and
  refuses a span that runs past closing rather than answering for the part
  that fits; `from_parts` bounds every field.
- `erp-i18n`'s plural rules match CLDR for Arabic at every boundary;
  `render_or_code` never panics and `audit` proves every code has both
  languages; duplicate codes across modules are caught by
  `crates/erp-api/src/catalog.rs`.
- `reconcile::unbalanced` and the `Book` projection agree on sign conventions,
  so a balanced ledger reconciles to zero per currency.
- The demo drives the real router with real sessions, reads the confirmation
  link from the control-plane outbox rather than bypassing signup, and refuses
  to run without `DEMO_PASSWORD`.

## What was not read

After the second pass: the demo's seed data line by line (its shape is
exercised end to end by `crates/erp-demo/tests/demo.rs`), and the strength of
individual test assertions beyond the mechanical scan for tests that assert
nothing. The provider request bodies were rebuilt from the providers' published
schemas (Tamara's checkout and webhook references, Tabby's checkout session
schema) and are asserted byte for byte against a fake server, but no sandbox
call has been made; the first live checkout on either is the operator's, as
the adapters' own docs say. Tamara's two callback shapes are taken from its
webhook reference and its SDK's two notification services.
