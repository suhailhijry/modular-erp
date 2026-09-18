# Implementation plan

Phased build order for the architecture in [ARCHITECTURE.md](./ARCHITECTURE.md).

Ordering is by **dependency**, not by value. Each phase ends somewhere that
compiles, tests green, and is worth having on its own. Durations assume one
focused engineer and are the least reliable thing in this document.

Type-safety work (§4 of the architecture) is deliberately spread across phases
rather than batched — it is cheapest applied to code as it is written.

**Legend:** `[ ]` todo · `[~]` in progress · `[x]` done

**A box can also be closed by deciding.** `[x]` with **decided against** or
**deliberately not built** means the question was asked and the answer was no:
the reasoning *is* the deliverable, and it sits beside the box so it is not
re-litigated. `Idempotency-Key` in Phase 3c has been marked that way since it was
written.

So an unticked box is one of exactly two things, and says which: **outstanding
work**, or work **waiting on a named condition** that has not happened yet.

This matters more than bookkeeping. Reading a deliberate non-build as a gap has
twice led to nearly building something this codebase had already decided against
on good grounds — the WPS file below being the sharpest case, where guessing at
an unverifiable specification is the *worst* available option and two other
documents said so while this one did not.

**Where this stands:** 1,792 tests green as of 2026-09-16, clippy, fmt and `cargo deny` clean.
**Priority 1 of [Road to selling](#road-to-selling) is complete** (§79–§83); in Priority 2 the
statements item (§84–§87) and the customer-holds item (§88 the print, held B2B and public
links; §89 PDF/A-3 with the XML attached) are done. The per-phase test
counts below are the numbers *at the time that phase was met* and are left as
written; they are history, not status. What is not yet true is collected under
[What needs work now](#what-needs-work-now) at the end, and what blocks selling
the system, in order, under [Road to selling](#road-to-selling).

---

## Road to selling

**What blocks selling the system, in order of importance.** Written 2026-09-14
from three audits — the "what is left" audits of 2026-09-12 and 2026-09-14, and a
six-lens sales-readiness audit checked by a completeness critic — and the product
owner's answers the same day. It sets the order of near-term work;
[Build order](#build-order) records how the product was sequenced before it.

The tiers, most important first:

1. **Stop-ship** — a security hole, lost data, or a way to put a customer in breach. Mostly hours.
2. **Needed to sell** — a demo that converts, and a contract to sign.
3. **Needed to go live** — what a paying customer needs to run on it safely.
4. **Self-hosting** — only when a customer hosts it themselves.
5. **Growth** — after the first few customers.
6. **Later phases.**

Sizes are estimates, not measurements. References name files and functions rather
than line numbers, because nothing checks line numbers in this document and they
drift with every change.

### Decided 2026-09-14

- **The generic data-driven `Document`/`StateMachine` is not needed.** Phase 6 and
  ARCHITECTURE §8 record it.
- **The interface is a separate project** built against these APIs. Screens are not
  this repository's work; what the APIs lack is.
- **Segments:** appointment businesses (salons, clinics, gyms) and counter
  businesses (cafés, retail, restaurants), both from the start.
- **Hosting:** Hetzner first, outside the Kingdom. Move to Riyadh when a local
  provider or an AWS Riyadh region is cheaper. Customers may also self-host.
- **Branches and payroll** are sold at launch.
- **Pricing:** a fixed price per user, with an annual discount. POS users, service
  providers such as masseurs, and other workers who do not need every feature are
  cheaper seats than main users.
- **Payment gateways:** each customer signs its own Moyasar, Tabby or Tamara
  agreement and only enters its credentials; the platform collects nothing on its
  behalf. `PUT /v1/payments/gateways/{provider}` already takes them, sealed.
- **Claims switch on per claim.** Granting one claim must not arm the others.
- **Charts for launch:** a food & beverage chart and a healthcare chart, beside
  services, retail and real estate.
- **Reports refuse when their figures disagree with the books.**
- **Retention:** a tenant's books are kept as long as the retention law of the
  tenant's country requires, with a compressed copy of its data kept so it can be
  handed over after a suspension or before deletion.
- **Erasure:** deleting a customer or a staff member keeps only legally binding
  records (invoices, contracts and the like) and erases the rest.
- **Recurring invoicing lives in `sales`**, beside the other invoices. **A rental
  unit is a booking unit** (a `booking::Resource`).
- **BSL everywhere.** The API document said `AGPL-3.0-or-later`; it says `BUSL-1.1`
  now, as `LICENSE` and `Cargo.toml` always did.
- **Suspension drains before it stops.** A tenant being suspended keeps having its
  issued documents signed and reported to ZATCA until none is pending, and only
  then is nothing run for it. The 2026-09-11 rule that nothing runs while
  suspended stands for everything else.
- **An API key issued with the owner's role is an ordinary member**: held to the
  document limit, and refused credit notes and payment approvals wherever the
  tenant uses claims, because a machine can hold no claim. No keys have been
  issued yet.
- **Signup is closed unless the deployment says `SIGNUP=open`.** Platform staff
  create a tenant after it has paid, and the owner is mailed the confirmation link.
  **Modules are not what a tenant pays for** — seats are — so the owner keeps
  switching modules on and off, and no trial exists: the public demo is the trial.
- **A second-factor reset is limited to three per target an hour**, counted
  through the shared limiter.
- **Cluster capacity is required**: the migrator refuses to declare the primary
  cluster without `PRIMARY_CLUSTER_CAPACITY`.
- **Branch membership is a list on the membership**, in the control plane beside
  the per-module roles; no list means every branch. A member with one branch need
  not send `X-Branch`; with several, they must. It bounds what a request reads as
  well as what it writes, and API keys are bound like people. Designed so a
  self-hosted deployment, which runs its own control plane, carries it.
- **Dependency scanning is `cargo-deny`.**
- **Two ways in, one billing record** (decided 2026-09-14, evening). A bank
  transfer is what the staff order is for; a smaller tenant pays by card and is
  provisioned without staff. Both record what was bought — seats by type and a
  period — and a staff order may carry a negotiated price outside the list, for
  a large customer. Card checkout is Moyasar's hosted form; renewal charges the
  saved card token; a failed payment gives 14 days' grace with an email and a
  bell notification, then suspension. **The platform issues its own tax
  invoices from this system**, keeping its books as a tenant of itself. Its
  Moyasar secret is sealed under `SEALING_KEY` and refused if absent, like every
  other secret. Self-serve billing moves from growth to before the demo.
- **Seat types are rows platform staff define, not a table in code**: a name,
  a monthly and an annual price, and what the seat can reach as a scope set in
  the vocabulary API keys already use (`module:capability`, wildcards), judged
  at the door where a key's scopes are, before the role. Four ship as defaults
  — main (`*:*`), POS, service provider, worker — and staff may edit them or add
  one for a deal. The owner buys counts per type and assigns members to seats;
  every member holds one, the owner a main seat; assignment beyond the bought
  count is refused. A staff order may grant a type **without limit** where the
  deal says so. Module-by-capability is the grain for now; widen if a deal needs
  finer.
- **The period close is two acts** (decided 2026-09-14, night). A *period* close
  moves the watermark, in order; a *year* close **books** the year — one closing
  entry per currency, every trading account with a balance to the
  retained-earnings account for that currency (`ledger.closing_accounts`, `3100`
  by default), dated the year's last day and flagged so the profit and loss
  leaves it out while balances, the sheet and the journal keep it. Any period may
  be the first ever closed; the history before it closes with it and is never
  booked. Reopening runs in reverse: only the latest closed period, a year only
  while no later one is booked, and a booked year's periods only after the year.
  A year close is refused while a trading currency has no closing account, and
  years need not be booked in order. The VAT return still moves the watermark
  and never books. `PUT /v1/ledger/books` goes — one way to close, by period.
  The calendar becomes **segments**: a new one starts on a fiscal-year boundary
  of the last, in open time; closed years keep the periods they were closed
  under; no short transition years yet.
- **FX is deferred** (decided 2026-09-15). The business handles its foreign
  currency itself: the ledger keeps per-currency books, a tenant that converts
  on receipt records riyals, one that keeps dollars keeps a dollar book beside.
  The provider, the functional currency, translated statements and revaluation
  are dropped from Priority 2 to *when a tenant asks*; the first thing asked
  for will be the exchange itself — one entry with two currency legs at a stated
  rate — which needs a functional currency to balance against and no provider.
  **The one thing not left to the business:** a Saudi tax document must be in
  riyals, because ZATCA states the tax in riyals and this build cannot state a
  riyal tax amount for a dollar invoice — `tax_sa` seeds the rule, `sales`
  refuses (§86).
- **Cost centers are a per-line dimension** (decided 2026-09-15, all as
  recommended). A ledger aggregate like an account — open, rename, close; a
  closed one refuses lines — sharing the id namespace with branches, so an open
  branch is a cost center without being opened. `Line.cost_center` is optional,
  never mandatory, and a line without one takes the entry's branch; a P&L by
  cost center shows the rest as unassigned. Manual journal entries and purchase
  bill lines carry one per line; invoices, till sales, payroll and prepaid get
  the branch default. `?cost_center=` on the P&L and the journal beside
  `?branch=`, and a `by-cost-center` P&L with one column per center; the
  balance sheet stays company-wide. Confinement is unchanged and separate:
  a confined member's reads stay pinned to `posting.branch`. Reassigning a
  posted line is a reversal and a repost; `reports` does not get the dimension
  yet.
- **What a customer holds** (decided 2026-09-16). Print-ready **HTML** on the
  server — the QR inline as SVG, nothing fetched, an 80 mm receipt for a
  simplified invoice and an A4 page for a standard one and a credit note —
  **bilingual always**, Arabic first. PDF/A-3 with the XML embedded is the next
  step. The worker keeps signing and submitting on the nudged visit and **the
  print route waits** for the signature and, on a standard invoice, for ZATCA's
  clearance, then renders the stamped document ZATCA returned; the private key
  stays in the worker. **Tokenised public links**: the number and an HMAC of it
  under a secret the business keeps, opened with no sign-in and bounded like
  every open route. `qrcode` is the one dependency added.
- **PDF/A-3 through typst** (decided 2026-09-16). The standard invoice's sharing
  format is PDF/A-3 with the XML embedded; `typst` and `typst-pdf` as a library
  do the shaping, the bidi, the tables, the font subsetting and the PDF/A-3b
  output with the attachment — a larger binary is worth not taking weeks over
  it. The face is **IBM Plex Sans Arabic** (OFL, vendored), Arabic and Latin in
  one family. `GET …/documents/{number}/pdf` for staff and `?format=pdf` on the
  public link; both wait and refuse as the print does, and receipts get PDFs
  from the same route. Conformance 3b; the attachment is the document the
  `xml` route serves, as `{number}.xml`, relationship `data`.
- **Products a till can use** (decided 2026-09-16, not yet built). A `Product`
  aggregate **in `sales`**, at `/v1/sales/products` — in `sales` because
  inventory is an optional module and a salon or clinic with no stock still
  has a menu. **Price stored net of VAT** in the document currency, so the
  till's `net` is the product's and VAT keeps being computed on the band
  subtotal. **Names in both languages, the tenant's primary language required**
  and the other optional; the line's description is the primary-language name.
  A till or invoice line names the product's id and the product fills
  description, net and VAT where the line leaves them out; **a `net` that
  differs from the product's price needs a claim** (`sales:override_price`,
  checked as `sales:approve_credit_note` is). **Barcode** on the product,
  optional, unique among live products in the tenant; the till caches the
  catalogue and matches barcodes itself, `?barcode=` on the list for everyone
  else — serials stay inventory's, one per unit, resolved on the server.
  **Editing** is one `PUT` with the full shape and one event, plus **retire**,
  which hides the product from the till and keeps every line that named it.
  **Optional inventory link** (`product`, validated to exist); a line naming
  the product depletes stock through it. Writes for the roles that manage
  sales settings, reads for anyone who can ring a sale.
  **A tenant primary language** (decided the same day): the per-request locale
  is the front-end user's; the tenant's language is for what the business
  itself writes — product names first. It does not exist yet and is built
  with the products. **Products map `prepaid` to the catalogue** (decided the
  same day): a product may grant an entitlement when sold — a package of
  uses, a subscription term — so the sale issues the tax invoice (VAT due
  now), `prepaid` defers the value and recognises it use by use or ratably,
  as its table already says; a package of seven sessions at 1,000 recognises
  a seventh per session delivered, the discount being the transaction price
  and not a line. Barcodes are per product, for physical products that have
  one; serials stay per unit in inventory. **Settled the same day:**
  `tenant.language` as a tenant setting, seeded Arabic, changeable only while
  every live product has its name and description in the new language;
  packages, subscriptions **and sold coupons** as products — a business sells
  coupons to a reseller at a discount, a customer buys one for somebody else —
  deposits and loyalty not; the grant in the sale's write and the revocation
  in the cancellation's, through in-transaction variants `prepaid` gains as
  `inventory` has; redemption at the till **explicit**, the cashier choosing
  the entitlement, and **on booking completion** (`Stage::Completed`) in this
  build; a validity in days or months on a prepaid product, the expiry from
  the sale date; a subscription is a price and a term, no uses; **kits of
  physical products in this build**. **And the same evening:** a description
  is optional, and the language change is refused for a missing one exactly
  as for a missing name; a **coupon is a bearer entitlement with a code**,
  attached to a customer at first redemption, packages and subscriptions
  named to the buyer; a coupon may be **tied to a branch, a product, a
  service, or all**; **no open-value gift cards**; the **issuer's side only**
  of a coupon resale — the tenant sells coupons for its own product to a
  business customer at a discount; **redemptions never appear on a receipt**;
  a **kit** lists components with quantities, one level deep, and its revenue
  is **split across the components and kept per kit**, so the kit's sales and
  each component's sales at the kit's discount are both readable; **booking
  lines name a product**, not free text. **Closed the same evening:** three
  coupon shapes on one basic model — a **sold coupon** (paid for, a bearer
  entitlement in `prepaid`, always naming a product), a **promotional
  discount** and a **promotional code** for influencers and paid advertising
  (neither paid for, so no liability; a percentage or an amount off, scoped
  to a branch, a product or all, applied at the till as a line allowance
  without the price-override claim); **a service is a product** with no
  inventory link; **every booking line names a product** and the bookable's
  published rate *is* the product's price — nothing is deployed, so `what`
  is replaced rather than kept beside; kit revenue allocated **in proportion
  to standalone prices** (IFRS 15), the rounding remainder on the last
  component. **And last:** a promotional code carries **optional numeric use
  limits**, per customer and in total; a code **may carry no discount**, for
  attribution alone; **stacking is the tenant's configuration, per kind of
  discount or promotion** — loyalty-point redemption beside an ordinary
  discount or not at all, several promotions or a maximum count (two, say),
  unlimited, or unlimited up to a specified amount. That cap is **an amount
  or a percentage**, the tenant's choice per rule, and there are two kinds of
  cap: the tenant's **stacking cap is per sale**, across the basket; a
  **promotion's own cap follows its scope** — per line when the promotion is
  constrained to a product, per sale when it applies to all. Nothing on this
  item is open; it is ready to build after the charts.
- **Charts for launch, decided in detail** (2026-09-16): `food_beverage`
  "Restaurants and cafés" and `healthcare` "Clinics and medical practices",
  each the services baseline with vertical names on the conventional codes and
  the additions listed in the products conversation of that day — food,
  beverage and delivery-platform sales, tips payable, packaging, platform
  commissions, spoilage, kitchen equipment with depreciation; consultations,
  procedures, laboratory and imaging, pharmacy apart for its zero rating,
  insurance claims receivable apart from patients, patient advances on `2400`,
  medicines and supplies, outsourced laboratories, medical waste, licences,
  claim rejections, malpractice insurance, medical equipment with
  depreciation. Four guards in the property chart's style: tips are a
  liability; platform sales and commissions are separate; insurance
   receivable is apart from patients; medicines are apart from consultations.
- **Per-product accounts** (decided 2026-09-16, not yet built). Every product
  carries a **revenue account**, and stocked ones an **inventory** and a **cost
  account**, all defaulting to the conventional codes — without it the vertical
  charts' revenue split has nothing posting to it.
- **Classifications instead of cost centers** (decided 2026-09-18, not yet
  built; researched in §90). DualEntry's model: tenant-defined **types**, each
  a list of **values that nest** — one tree per type, reports rolling up —
  tagged on **lines**, so the chart stays small. §87's cost center becomes one
  type and is reworked, nothing being deployed; the branch stays its own
  column because confinement reads it. **Tags ride the revenue, expense and
  cost lines**; receivable, payable, VAT and cash lines carry none and the
  balance sheet stays company-wide. **A tag arrives in this order:** what the
  line says, a rule over the operation's facts, the default on what the line
  names, nothing. **Re-tagging history is an event** that changes tags and
  never amounts, in open periods only. **Rule and default changes apply
  forward only.** **Custom fields and allocation templates are in this item.**
  Learned suggestions are not: they come later through TypeSafe's AI model,
  Jev, and are that integration's problem. **Settled the same day:**
  **default tags** live on products, customers, suppliers, employees, tills,
  promotional codes and accounts; a tag may be **required per kind of
  operation and per account**. **Custom fields** on invoices, credit notes,
  supplier bills, customers, suppliers, products, employees and bookings —
  text, number, date, yes/no, a pick list of the tenant's own values, and
  **file uploads** (through `files`); no formulas; a field may be required,
  appears in the record's API and lists, can be filtered on, is never summed
  in a financial report, and may be marked **printed** — on the page only,
  ZATCA's signed XML having no place for it. **Allocation templates** on
  supplier bill lines, manual journal lines **and sales lines**, splitting by
  **percentages, fixed amounts, or a mix**; applying one replaces the line with
  several tagged lines on the document itself — visible, editable before
  posting, frozen after. **A mixed template takes its fixed amounts first**
  and divides what remains by percentages that total 100; fixed amounts past
  the line refuse it. **Both this and approvals stay in Priority 5.**
- **Approval workflows** (decided 2026-09-18, not yet built; §90). Rules are
  authored per kind of operation — comparisons on amounts and percentages —
  and **every rule that matches is triggered and must be satisfied**. A rule
  names a **chain of supervisors**: ordered steps, each a list of **named
  users** — roles were considered and removed — satisfied by *any* or by
  *all*, with **"no self-approval" as an option on the rule**. **Held means the operation is stored** as the
  request and replayed on approval; nothing exists until then. Researched the
  same day: Business Central, NetSuite and Zoho Books all keep a *visible
  document* in a pending status that cannot post, and tills (Lightspeed,
  Square, Toast) take a manager's PIN on the spot — so the stored request is
  shown to its approver as a **preview by dry run**, the way `preview_chart`
  runs the real install and rolls it back, and listed to its requester as
  pending. **The owner is never held and may always approve.** **First kinds
  of operation:** discounts and price overrides, invoices, credit notes,
  refunds, supplier bills, supplier payments, manual journal entries, stock
  write-offs and count adjustments, payroll runs, period close and reopen —
  and **sensitive permission changes**. **Each rule says whether it applies to
  API keys, on by default.** Managing the rules is a **new permission** held
  by the owner and manager roles and grantable; **editing approval rules can
  never itself be put behind a rule**. **Staleness:** the replay recomputes the
  facts; a failure is the requester's to hear; a fact the matching rule names
  that grew past what was approved is held again; an unanswered request lapses
  after the tenant's number of days, fourteen by default. **At the till the
  supervisor approves from their own phone** — told by push, which `messaging`
  already sends through FCM — because a supervisor walking to a till in a
  crowded supermarket is the thing to avoid; **a PIN on the cashier's device
  is the fallback** for a phone that is off or has no connection. **Approval
  comes before payment:** what is held at the till is the discount's
  authorisation for that basket, and the sale carries it once the customer
  pays. **The till parks a basket** so the cashier serves the next customer
  while one waits. **System jobs are never held.** **A rejection by
  anyone on any chain ends the whole request, every rejection carries a
  reason, and the requester may withdraw at any time.** **Sensitive kinds:**
  granting or changing a role, granting a claim, issuing an API key, changing
  the second-factor policy, editing permission limits, changing posting
  accounts, **exporting client data, exporting financial data, and granting
  any permission that exports sensitive data**. **Escalation:** a named person
  who neither approves nor rejects is passed over to **their immediate
  manager** on the org chart, and the escalation is logged for reports; the
  **rule's author sets the interval, with no default**; it **keeps climbing**,
  manager after manager; with **nobody above, it goes to the owner**. **Chains
  of several matching rules start together**, and **one person's approval of a
  request counts everywhere they appear on it**. **A rule cannot be saved
  without its escalation interval.** **The PIN is six digits** — proposed with
  it and not objected to: set by the supervisor, good only for approving at a
  till and never for signing in, locked after five wrong attempts until they
  sign in and reset it, and the approval marked as given by PIN at that till.
  **A third answer beside approve and reject: "needs modification".** The
  request goes back to its requester with the supervisor's note instead of
  ending; they edit and send it again, and **the new version is shown with
  its changes highlighted** against the one before — the request's fields and
  the dry-run preview both. **A resubmission restarts every chain from its
  first step**: an approval was of a version that no longer exists. Nothing on
  approvals is open.

### Waiting on the product owner

- **The legal entity** — being registered. It fills `LICENSE` and signs the terms.
- **Retention periods per country.** Counsel confirms Saudi Arabia's.
- **ZATCA registration details and the simulation OTP.** Onboarding to simulation,
  then production, runs when they arrive.
- **Party roles for property** (later). Recommended: roles every module shares, such
  as owner or supplier, in `crm`; lease-only roles, such as tenant or guarantor, on
  the lease.

### Priority 1 · Stop-ship

- [x] **The branch comes from a header the caller writes.** `X-Branch` is checked for
      shape only, and branch-scoped claims and inventory shelves both trust it; §68
      even says "the claim is branch-scoped already". Needs a record of which
      branches each person belongs to. 1–2 weeks, and more urgent because branches
      are sold at launch. **Done 2026-09-14** (§83): a list per membership, judged
      at the door and on every branch-filtered read
- [x] **An owner-role API key counts as the owner.** `sales::Authority::of` reads the
      handle's role, so an integration key walks past the document limit and the
      credit-note claim. Half a day. **Done 2026-09-14** (§79): `Access::is_owner`
      is the one question, and it is false for a machine
- [x] **Suspending a tenant stops its ZATCA reporting**, so a suspension of more than
      a day can push the customer past the 24-hour reporting window. Decided: a
      `suspending` state that drains signing and reporting, then stops. 1–2 days.
      **Done 2026-09-14** (§81)
- [x] **Claims switch on per claim** (decided above). Today the first grant of any
      claim arms all of them, so till staff with no employee record lose returns.
      Hours. **Done 2026-09-14** (§79): `hr::claim_placed` asks about one claim
- [x] **Behind the proxy, every anonymous rate limit is one bucket for the whole
      internet.** `TRUST_X_FORWARDED_FOR` is off and compose does not set it, so
      sign-in, signup, one-time codes and public booking all key on the proxy's
      address. Document `PRIMARY_CLUSTER_CAPACITY`, `FLEET_CONCURRENCY` and `DEMO_*`
      at the same time. Hours. **Done 2026-09-14** (§79)
- [x] **Cluster capacity silently defaults to 10,000** when it is not set, so
      placement overfills the cluster. Refuse a missing value. Hours. **Done
      2026-09-14** (§79): `erp_control::declared_capacity`, and `bin/demo` no
      longer re-declares the cluster at its own placeholder
- [x] **The API ignores SIGTERM**: it waits on Ctrl-C only, so every deploy cuts
      requests in flight. Hours. **Done 2026-09-14** (§79): one
      `erp_control::shutdown_signal` for both processes
- [x] **The request path accepts a read model newer than the build** (`<` where the
      projection runner uses `!=`), so old pods serve new-shaped tables during a
      rolling deploy. 1 hour. **Done 2026-09-14** (§79)
- [x] **Audit entries are written after the commit**, so a crash between the two
      loses the record. 1 day at the root. **Done 2026-09-14** (§80): `record`
      takes the transaction, 31 sites pass theirs, a scan refuses anything else
- [x] **The second-factor reset has no rate limit.** Each call ends the target's
      sessions and sends mail. Under an hour. **Done 2026-09-14** (§79): three per
      target an hour, both routes on one budget
- [x] **Anyone can sign up and run the system for free.** `SIGNUP=open|closed`,
      closed when unset; a staff route that creates a tenant after payment and mails
      the owner the confirmation link, which then asks for a password; and a source
      scan that every module route calls `require_module`. Modules stay the owner's
      to switch on — seats are what is paid for. 3–5 days. **Done 2026-09-14** (§82)
- [x] **Two documents promise what the code does not do.** ARCHITECTURE.md says the
      migrator enforces a backup before upgrade, and it has no backup code;
      `docs/book/src/deployment.md` says a customer-hosted tenant keeps running
      without our control plane, and every request checks sessions against it.
      Correct both. Minutes. **Done 2026-09-14** (§79)
- [x] **No dependency vulnerability scanning in CI** (`cargo audit` or `cargo deny`).
      Hours. **Done 2026-09-14** (§79): `cargo-deny`, `deny.toml`, a CI job and
      `just deny`

### Priority 2 · Needed to sell

- [ ] **Legal**, once the entity exists: fill `LICENSE` (BSL, decided); terms
      of service, privacy notice, data processing agreement, sub-processor list,
      records of processing, a breach procedure, and an exit clause promising the
      compressed export. Record terms acceptance at signup (half a day of
      engineering)
- [ ] **Financial statements:** profit and loss, balance sheet, balances at a date,
      and a journal listing. Today the trial balance is per-currency totals and
      account balances are all-time. **Decided 2026-09-14 (evening):** exclusive
      far ends as the VAT return and `closed_before` use; a journal listing at
      `GET /v1/ledger/entries` with lines, filterable by range, account and
      branch; the balance sheet refuses (503) when the postings to that instant
      do not balance; zero rows hidden unless `?all=true`. **Three directions
      that widen it** (open questions in the same note): fiscal periods are the
      tenant's — monthly, 4-4-5 and its variants, yearly — with a **formal period
      closure**; **FX rates** are wanted, with a functional currency; and a
      business may run branches as **cost centers** on journal lines rather than
      as metadata on the entry. **Settled the same evening, all as
      recommended:** a tenant-set fiscal calendar — a start date and a pattern
      (monthly, quarterly, 4-4-5 and its variants with the 53rd week in the last
      period, yearly) — with periods closing in order by moving the watermark,
      the year's last close posting revenue and expense to `3100`, reopening
      reversing it, and the pattern changeable only from the next open year;
      FX as tenant-entered daily rates with an import route (fetching later),
      a functional currency, statements presented in it with a translation
      difference, multi-currency entries balanced at a stated rate with the
      difference to an FX account, and revaluation with the period close;
      cost centers on each line, defaulting to the entry's branch, every branch
      one automatically, P&L by cost center and the balance sheet company-wide.
      **Order:** statements on the calendar (~2 weeks) → formal closure (~1) →
      FX levels i and ii (~2–3) → cost centers (~1–2) → revaluation (~1).
      **Statements on the calendar built 2026-09-14 (§84), the formal closure
      the same night (§85); FX deferred 2026-09-15 (Decided above, §86); cost
      centers built the same day (§87)**. Done but for revaluation, which goes
      with FX
- [ ] **Receipts and invoices a customer can hold.** A till sale's response carries
      no QR, and the full nine-field QR exists only after the worker signs; a B2B
      invoice is not held back until ZATCA clears it, and handing one over uncleared
      is a breach. Rendering (QR image, Arabic, PDF) can live in the interface or
      the server. Backend days; server rendering 1 week; PDF/A-3 1–2 weeks more.
      **Built 2026-09-16 (§88):** print-ready HTML on the server, the print and
      the XML waiting for the signature and, on a standard invoice, the clearance;
      a customer's link behind an HMAC; **PDF/A-3 with the XML embedded the same
      day (§89)**, through typst. Done
- [ ] **Food & beverage and healthcare charts** (decided above), each installing into
      a fresh tenant and carrying every account the modules' conventional postings
      name. Days
- [ ] **Products a till can use:** price, barcode, VAT category, editing, and an
      Arabic name. 1–2 weeks
- [ ] **Self-serve card billing** (decided above; moved up from growth). One billing
      record for both ways in: seats by type and a period, at list prices for a card
      checkout on Moyasar's hosted form, or at a negotiated price on a staff order.
      Renewal on the saved card token; 14 days' grace on a failed payment with an
      email and a bell notification, then suspension (§81's drain makes that safe).
      The platform's own tax invoices issued from its own books as a tenant of
      itself, through `sales` and `tax_sa`. `PLATFORM_MOYASAR_SECRET` sealed under
      `SEALING_KEY`, refused if absent. Waits on the seat-type definitions and on
      the legal entity for the merchant account. 3–5 weeks
- [ ] **A demo a salesperson can use:** a small hosted environment, staff routes to
      create, reset and convert demo tenants, a clinic booking template, and salon,
      clinic and café seeds with two branches each. 2–3 weeks
- [ ] **Settle error status codes before the interface depends on them.** `purchases`
      and `hr` refusals answer 400 where `sales` answers 403, and credit refusals
      answer 400 at `/v1/sales` but 422 at the till. 1–2 hours
- [ ] **Consent and a privacy notice captured on public booking**, where clinics
      collect health data. A few days
- [ ] *Dependency outside this repository:* the interface project calling these APIs

### Priority 3 · Needed to go live

- [ ] **Payroll correctness**, because payroll is sold at launch. GOSI rates verified
      against the official schedule; a schedule for people hired after the 2024
      pension reform, whose deductions are wrong today; part-month pay, since a
      mid-month joiner stops the month's run; payslips the interface can render.
      About 1 week once the rules are confirmed. The WPS salary file still needs a
      real bank or Mudad specification. **Do not switch payroll on for a customer
      before this lands**
- [ ] **Seat types for per-user pricing** (decided above). Staff-defined rows with a
      scope set, four shipped; the owner assigns members within the bought counts,
      or without limit where a staff order says so; the seat's scopes are judged in
      `Allowed::from_request_parts` where a key's are. Must exist before self-serve
      billing. 1–2 weeks
- [ ] **A legal basis for keeping personal data on Hetzner outside the Kingdom**, with
      extra care for clinics' health data. Business and legal
- [ ] **Production environment on Hetzner:** TLS, including to Postgres; security
      headers; encryption at rest; point-in-time backups that include uploaded files
      and keep control-plane and tenant data consistent; backup before upgrade,
      enforced by the migrator; failover; readiness checks and metrics; alerts for a
      stalled worker and overdue ZATCA reports; the reaper and migrator scheduled;
      mail with SPF, DKIM and DMARC; playbooks for database loss, provider outages,
      a ZATCA outage and a data breach. 3–5 weeks or more
- [ ] **Reports refuse when their figures disagree with the books** (decided above).
      The message `DOES_NOT_RECONCILE` exists in English and Arabic, and nothing
      raises it. Days
- [ ] **Refuse invoices until the tenant is onboarded with ZATCA.** An invoice issued
      before onboarding can never be reported. Days
- [ ] **First live calls** with a pilot's own Moyasar, Tabby or Tamara account, and
      the Taqnyat SMS sender name. Days once the accounts exist
- [ ] **Importing a customer's data:** products, opening stock, opening balances, and
      a path for open receivables that does not create ZATCA documents. 2–3 weeks
- [ ] **Stock transfers between branches**, because branches are sold at launch.
      1–2 weeks
- [ ] **Posting-accounts routes for `purchases` and `payments`.** A tenant with its
      own chart cannot record a supplier bill or a gateway payment at all. About 6
      hours
- [ ] **Booking:** confirmation and cancellation messages (days); customers cancel or
      reschedule their own bookings (days); WhatsApp with approved templates
      (2–4 weeks)
- [ ] **Packages and memberships:** redeem a package from a booking (1–2 weeks);
      renew and charge memberships automatically (2–3 weeks); gym check-in (not
      sized)
- [ ] **Erasure under the policy above:** customers, whose name and phone sit in
      append-only events (2–4 weeks); a route to erase staff (1–2 days); audit-trail
      retention (days)
- [ ] **A compressed whole-tenant export**, kept for a suspended or departing tenant
      until deletion. Days
- [ ] **Staff can find a tenant by slug and a person by email.** Every staff action
      takes a UUID today. Half a day
- [ ] **Sign-in, password and second-factor events in the audit trail.** Payment
      gateways' security questionnaires ask for it. 1–3 days
- [ ] **A VAT return that matches the form:** zero-rated boxes split, reverse charge,
      credit notes shown as adjustments, credit carried forward, prior-period
      corrections (1–2 weeks); and a test for cancelling and reissuing a B2B invoice
      ZATCA refuses (2–3 days)

### Priority 4 · Self-hosting

- [ ] A self-hosting agreement — none exists — once the licence is settled
- [ ] A tenant's install that keeps running without our control plane
- [ ] An install and upgrade a stranger can run, and compose refusing its all-zero
      `SEALING_KEY` default

### Priority 5 · Growth

- [x] SaaS billing — **moved to Priority 2** on 2026-09-14 as self-serve card
      billing; trials stay deliberately unbuilt (the public demo is the trial)
- [ ] More than one database cluster (the cluster registry only knows `PRIMARY_*`),
      which is also the path from Hetzner to Riyadh; a database login per tenant
- [ ] ZATCA registration per branch or per till. **Confirm during the simulation
      run**: if ZATCA expects a unit per branch, this moves to Priority 3
- [ ] Purchasing: suppliers, purchase orders, reorder points, stock valuation
- [ ] Payables ageing, customer statements, bank reconciliation, sales by item,
      quotations and sales orders, debit notes, and recurring invoices in `sales`
- [ ] Offline till; card-terminal integration
- [ ] Restaurants: tables, kitchen orders, modifiers, recipes. 6–10 weeks
- [ ] **Approval workflows** (researched 2026-09-18, §90; decisions open). A
      tenant writes rules naming the operations that need approval and who gives
      it; a matching operation is held, its approvers are told, and it runs when
      they say yes. A gate inside the command, where `hr::may` is called today,
      not the generic document state machine declined on 2026-09-14. 3–4 weeks
      after the shared fact vocabulary (1 week)
- [ ] **Automatic accounting classifications** (researched 2026-09-18, §90;
      decisions open). **Classifications instead of cost centers**, as the
      dimensional-accounting systems have them: tenant-defined types, each a
      list of values, tagged on every line that reaches the journal — by
      defaults from what the line names and by rules over the operation's
      facts — frozen on the event, so the chart stays small and the P&L, the
      journal and the sales reports cut by any of them. §87's cost center
      becomes one type. **With custom fields and allocation templates** (decided
      2026-09-18). ~5–6 weeks after the same fact vocabulary
- [ ] Security polish: rate limits and lockout for signed-in users; API key expiry;
      session idle timeout and a list of active sessions; virus scanning of uploads;
      keys scoped `*:manage_tenant` must not rewrite permission limits; the remaining
      second-factor gaps (a failed enrolment mail is invisible, a suspended tenant's
      member cannot be freed, a pending factor is renamed by kind); internal cluster
      names in the owner's audit trail; a reused sealing-key id; a support-entry
      route; a tenant switcher for accountants
- [ ] Amount limits only reach the ledger's routes: supplier bills, payroll runs and
      pay-outs have none. Say so in the interface until they do
- [ ] Lists that take no branch — invoices, entries, bookings — still span every
      branch for a member confined to one (§83 bounds the header, `?branch=` and
      `?scope=all`). Bounding them row by row is a change to every list query
- [ ] Actions the code defines with nothing to perform them: suspending and
      reinstating a person, deleting a tenant after its retention period, changing a
      cluster's status, sweeping webhooks from unknown providers, and dismissing a
      tenant's dead letters
- [ ] Inventory leftovers: a one-unit delivery naming its serial twice is accepted;
      the lot a line named is not shown on the invoice; goods received are not
      matched to bills line by line; supplier bill lines do not keep their product
- [ ] Housekeeping: code and the book still cite the retired `docs/ERRORS.md`, and
      nothing checks that this plan's `file:line` references still resolve
- [ ] Two tests flaked once each under a full parallel run on 2026-09-14 and pass
      alone and in their own binaries: `support_requeues_and_dismisses…` (the
      dispatcher dead-lettered one email of two) and `leases::a_claim_is_bounded…`
      (a claim found one due tenant of three). Both compare a stored `now()` with a
      later one, and both failed in the direction a backwards clock step produces;
      the machine is WSL2. Suspect the clock before the code, and pin `now()` in
      the tests if it recurs. **Four worker tests were a different kind of flake
      and are fixed (2026-09-16):** `one_failing_job_does_not_stall_the_others`
      failed the product owner's full run (1,444 s against 1,250 s the run before,
      the compose `api`/`worker` containers live on the same Postgres), and running
      the binary five times turned up `a_tenant_being_suspended_runs_only_its_drain_jobs…`
      and `a_tenant_is_visited_by_one_visit_at_a_time` too; `shutdown.rs`'s
      `a_failing_job_stalls_one_tenant…` had the same shape. All four ran the
      worker for a fixed while — 200 to 600 ms — and asserted what had happened,
      which a loaded machine misses. Each now waits for the condition it asserts
      (`wait_until`, bounded at ten seconds, the helper `shutdown.rs` already had),
      and both binaries passed five runs in a row. **A fifth, fixed the same day:**
      `a_print_waits_for_the_worker_and_a_link_opens_it` and `print.rs`'s
      `a_link_opens_its_document_and_nothing_else` forged a link by writing `0`
      over the MAC's last hex digit — which one run in sixteen it already was, so
      the "forgery" was the real link and opened (a 503 where a 404 was expected,
      after the full twenty-second wait). Both now write a digit that differs

### Priority 6 · Later phases

- [ ] **FX, when a tenant asks** (deferred 2026-09-15, §86). First the exchange
      itself — one entry with two currency legs at a stated rate, the difference
      to an FX account — which needs a functional currency on the tenant and
      per-line amounts in it; then translated statements; a rate provider and
      revaluation last, if ever. Until then the business keeps per-currency
      books and Saudi tax documents are riyals only
- [ ] Property (Phase 20). The party-model plan must be updated for read-model
      versions (§65) before it is followed
- [ ] Marketing (Phase 18)
- [ ] OIDC and single sign-on

---

## For review — decisions I made without you

Written across 2026-09-01/02 while you were away: a gap-closing pass, then
Phase 17, then Phase 9, then Phase 10, then Phase 11's channels and links. Each item below is a judgement call I
took rather than stopping on, and each is reversible. Read them, and delete this
section once you have.

**What landed, in order:** the closable gaps and two defects they exposed; the
public booking API with per-tenant CORS, rate limiting and an API-compatibility
guard (Phase 17); Phase 9 — the org chart with claims travelling up it, work
documents, skills, shifts, attendance, leave, payroll with commission, and the
Saudi statutory arithmetic; Phase 10 — `modules/reports`, which subscribes
to the log rather than reading four groups, and reconciles to the books; and
Phase 11a/b/c/e — short links; `modules/messaging`, with templates that fetch
their own data, audiences resolved rather than frozen and SMS billed by the
segment; `modules/files`, where a document's record is a key and a checksum rather
than a URL, and now an S3 engine behind the same trait, round-tripped against a
real bucket, plus the Taqnyat and FCM gateways behind `Transport`; 11d — every list is a spreadsheet on `Accept: text/csv`, and an
import takes the good rows and reports the bad ones; and Phase 12b/12c/12d/12e — verified inbound
callbacks, API keys in pairs, a version a client can be refused on by name, and
signing in with a phone number.

**Phase 9 is complete except two items, and both are blocked on something
outside the repository**: the WPS file needs a specification this build cannot
verify, and the email reminder for an expiring document needs a tenant-plane
outbox handler that does not exist. Both are written up where they sit.

**The one to read first is §5**, because it is about numbers that come out of
this system and go to a government. **§10 is a decision I stopped on** rather
than took, and it blocks the last buildable piece of Phase 9.

### 1 · Two views over the invoice, rather than one

`invoice_status` grouped every invoice in the tenant to return one page of
twenty. Measured on 200,000 invoices and 400,000 payments: **410 ms and 443,000
buffers**. Rewriting it to correlate makes that **0.3 ms and 118 buffers** — but
makes the receivables report and the overpayment health check about **3× slower**
(292 → 760 ms, 273 → 945 ms), because each of the 200,000 invoices then costs an
index lookup.

So there are now two views over the same numbers: `invoice_status` for readers
that scan, `invoice_row` for readers that want one invoice or one page. A test
asserts they agree, since two shapes of one rule is exactly how a rule drifts.

**The alternative I did not take** is a maintained `paid` column on `invoice`,
which would be fastest for everything. The schema comment argues against it
deliberately — "a second thing that can be wrong" — and overriding a documented
decision on the strength of a benchmark I wrote myself seemed like your call.

### 2 · Matching a customer to an old invoice is re-matchable

Phase 7a's reconciliation is built. `attach_customer` sets the *reference* and
never the printed name.

The judgement: **re-matching to a different record is allowed.** A match made to
the wrong Ahmed has to be correctable, and the log keeps every attachment so the
correction is visible. The stricter alternative — refuse once matched — leaves
no way to fix a mistake at all, which seemed worse. Say the word and it becomes
a refusal with a `sales.already_matched` code.

`attach_customer` is scoped to **owner** (`ManageTenant`), on the grounds that
re-pointing a document at a different customer changes what a report says about
that customer. If a clerk should be doing the backlog, it wants `PostEntries`.

### 3 · Four flaky tests I could not reproduce

All four pass in isolation and each failed once under a full-workspace run. The
first two are around leases and killed backends; the second two arrived later
and look like load rather than a shared cause:

- `erp-eventlog::crash a_crash_during_a_claim_leaves_the_effect_owed` — failed
  once, then passed 5/5 in isolation, 3/3 under `-j 16`, and in several later
  full runs. It kills a backend with `pg_terminate_backend`, which signals
  rather than waits, so a timing window is plausible.
- `erp-control::leases re_claiming_your_own_tenant_renews_it` — failed once
  under a full run and passed immediately in isolation. Same family: a lease
  whose timing assumption is tighter than a loaded machine honours. **Seen
  again 2026-09-04**, on the settlement run: failed once at position 300 of
  1,148, passed 4/4 in isolation, and the next full run was clean. Twice now is
  not "once", and it is the one of these four worth actually fixing.

- `erp-worker::modules an_invitation_is_promised_by_the_control_plane_and_delivered_by_the_worker`
  — failed once on the run that added API keys, passed immediately in isolation
  and on the next full run.
- `erp-demo::demo every_module_is_enabled_and_answering` — the same, on the same
  run. The demo now builds sixteen modules' worth of data in one test, which is
  the slowest thing in the suite.

Left as-is rather than papered over with a retry. If any recurs, the suspect in
the first two is a wait that assumes the database has finished something it was
only told to start; in the second two it is more likely that a loaded machine
took longer than a timeout allows, and the honest fix is a longer timeout with
the reason written down rather than a retry that hides it.

### 4 · Phase 9 decisions I took without you

The org chart went in overnight, and four calls in it are product decisions
rather than technical ones.

**The root is a superuser.** The union makes the top of the tree hold every
claim in the company. I settled that as *intended* — the person nobody reports
to is the owner — and put anybody who must sit outside it (an auditor, a
bookkeeper on retainer) outside the tree entirely, as a platform membership.
The alternative is an explicit "outside the hierarchy" flag on `Employee`.

**`SEGREGATED` is a constant, not configuration.** A tenant cannot switch off
the segregation-of-duties list, on the grounds that what an auditor requires is
not a preference a business expresses. If a customer needs a different list, it
becomes tenant configuration and the argument above stops being true.

**An empty skill list means *anything*.** The alternative — empty means nothing
— would refuse every assignment in every existing tenant the day the module is
switched on. The cost is that recording the first skill starts restricting,
which is why the API takes the whole set at once and offers no way to add one.

**A part-period joiner refuses the payroll run.** Pro-rating is real arithmetic
and Saudi contracts differ on working days versus calendar days, so I stopped
rather than guessed. That means a business hiring mid-month cannot run payroll
for that month until somebody builds pro-rating. If you would rather it paid a
full month, or a simple calendar-day proration, say which.

### 5 · The GOSI defaults ship, and I am not certain they are current

`hr_sa::gosi::Schedule::default()` carries 9.75% employee / 11.75% employer for
Saudis, 2% employer for non-Saudis, and a 45,000 ceiling. Those are the
long-standing figures. **I could not verify them against the authority's current
schedule from here**, and the 2024 pension reform put new entrants on a
different and rising scale that this shape does not express at all — it has one
rate per footing, not one per cohort.

What I did about it: the rates are configuration rather than constants, the
schedule read answers `configured` so a tenant can see nobody has confirmed
them, and it is said in the module docs, the book and this document.

**What you may want instead**, and I would understand either:

- **No defaults.** `resolve` refuses until a tenant configures, so a payroll run
  cannot happen on numbers nobody checked. More consistent with L6 — stop rather
  than degrade — and it is money withheld from people's pay and remitted to a
  government, which is not the same as an account code that a reclassification
  entry can fix.
- **A cohort on the employee.** If new entrants really are on a separate scale,
  `Footing` needs a third case or a date, and that is a change to the shape
  rather than to the numbers.

### 6 · The overnight commits are unsigned

`gpg` needs a pinentry TTY for your key's passphrase and I have neither, so the
gap-closing commits went in with `--no-gpg-sign`. Re-sign them when you are
back:

```
git rebase --exec 'git commit --amend --no-edit -S' -i <the commit before them>
```

Nothing else about them differs. **Every commit from the gap-closing pass
onward** is unsigned, not just two — the count in this heading is out of date
the moment another lands, so treat it as "the overnight run".

### 7 · Phase 17's deposits are recorded, not charged

`booking.public` carries a `deposit_bp`, the public booking response reports it,
and **nothing collects it** — card payments are Phase 12a and there is no
gateway. The alternative was to leave the setting out until the gateway lands,
which means it arrives configured by nobody.

If you would rather the field did not exist until it works, say so and it comes
out; the argument for keeping it is that the shape is known and a site can
honestly tell a customer what will be asked for.

### 8 · Public booking writes are gated on an opt-in I invented

Nothing in the plan asked for `booking.public`. The plan's answer to abuse of a
public write is the deposit, and the deposit does not work yet — so a public
booking that anybody can make would let a script fill a salon's week with
appointments nobody intends to keep, bounded only by a rate limiter.

So it is off unless a business turns it on. That is a product decision I made
rather than a technical one, and it is the one thing in this phase I would most
expect you to want changed.

### 9 · The "5,000 tenants" prose was not stale

I had this on the gap list from an earlier session. It is wrong: `ARCHITECTURE.md`,
`pools.rs` and `placement.rs` all quote 5,000, and this document's own target is
2,000–5,000. Sizing against the top of the stated range is correct. Nothing
changed; the item is struck.


### 10 · The recurrence crate move, and two renames I took

You said to record blockers and keep going, so I took both.

**`booking::Availability` moved to `crates/erp-recurrence`**, because `hr`
shifts are the same shape and `booking` already depends on `hr`. Its seven error
codes moved with it: `booking.not_a_window` is `recurrence.not_a_window` now.
`Calendar::KEY` went from `booking.calendar` to `tenant.calendar`, since a
business has one clock and both the diary and the rota read it.

**`hr` claims changed separator**, from `hr.approve_leave` to
`hr:approve_leave`. That one was not planned: the openapi guard read a claim
name in the documentation as an error code that did not exist, which is a real
ambiguity rather than a false alarm — two namespaces sharing `module.verb` is
two things somebody eventually confuses. The colon separates them at a glance,
and the dot stays as the *hierarchy* separator inside a claim
(`purchases:approve_payment.over_limit`).

Both are breaking changes to client-facing identifiers, both are free today
because nothing is released, and neither would have been in six months. Reverse
either and it is a `sed`.

**A third thing fell out of it.** Composing the recurrence catalogue into
`erp_api::CATALOG` made me look at that list, and `hr`, `payroll` and `hr_sa`
were not in it — mounted, routed and tested, with every refusal they can make
absent from `docs/ERRORS.md`. Nothing broke at runtime, because a module renders
through its own smaller composite; what was missing was the reference a client
reads. `Registered` carries a `catalog` now, so there is nowhere to add a module
that does not also say what it can say, and `every_module_reaches_the_reference`
fails if one slips through.

### 11 · A report module reads its own tables while projecting, and that needed a rule

`crates/erp-projection/tests/purity.rs` says a projection **writes and does not
read**, for three reasons: an N+1 per event, an undeclared ordering constraint
between projections, and — the one that matters — a dependence on rows that may
be absent mid-replay, which is L2 lost.

`reports` cannot obey it. It subscribes to *other modules'* events and cannot put
anything into them: `sales.invoice.cancelled` carries the credit note and not the
invoice's amounts, and `pos.shift.sold` carries tenders and not the operator —
both correctly, because neither has changed. Netting a credit off, or grouping
takings by person, means the report has to have remembered.

**What I did instead of exempting the module:** refined the rule and
strengthened the guard.

- A read of your **own group's** working table, written by **this same
  projection earlier in log order**, is allowed — it costs the N+1 and costs
  neither of the other two, and the shadow replay proves it. It must be declared
  on the line above it with `// projection-read:` and a sentence saying which
  table and why.
- The guard now follows the helpers `apply` calls, transitively. It previously
  scanned only the inline body, so it was blind to a read one function away.

That second change found **four undeclared reads that already existed** in
`tax_sa` — the ZATCA chain's previous link, the invoice a credit note points at,
the document a signature applies to, and the registration in force at that point
in the log. All four are the same legitimate shape, all four are now declared,
and none of them was visible to the guard before.

Reverse it by deleting the marker rule and the helper hop; the four `tax_sa`
reads go back to being invisible, which is what they were.

### 12 · `sales` now publishes the names of the entries it posts

`sales::issue_entry_of` and `sales::credit_entry_of` are public. The §10b
reconciliation is *"the debits of the entry this invoice posted equal what the
invoice came to"*, and naming that entry is the only way to ask the question
without a cross-group read.

The alternative was for `reports` to reimplement `si.{invoice}`, which is the
kind of copy that stays right until somebody changes the prefix. A unit test
asserts the published names and the private derivation agree.

### 13 · What the reconciliation compares, and what it deliberately does not

It compares **per document**, not per account. I tried the account-level version
first — reported revenue against the revenue accounts' balance — and it produces
false alarms: `prepaid` moves money in and out of revenue as packages are
granted and redeemed, and a manual journal to `4000` is a legitimate thing a
business does. **An invariant that fires on something normal is one somebody
switches off**, so it is per-document and account-agnostic.

It also skips the last invoice in the log. An invoice and its journal entry
commit together and take consecutive positions, but a projection batch may end
between them — reporting that as "made no entry" would be reporting a batch
boundary as a broken ledger. `invoiced.position` is what excludes it.

**What that leaves uncovered**, honestly: a journal entry posted by hand that
should not exist, and a document from before this module was enabled. Neither is
reachable from what a report can see, and inventing an answer would be worse
than the gap.

### 14 · Three of Phase 10a's figures are not built, and one is a duplicate

- **Revenue by product** needs invoice lines, which is a working table the width
  of every line ever issued. Not built; the same question is answerable per
  document today.
- **Headcount and expiring documents** are answered by `hr` from its own group.
  No cross-group total is involved, so a copy in `reports` would be duplication
  for its own sake. Deliberately absent rather than pending.
- **"Against what was banked"** is `takings.paid_out` — cash that left the
  drawer and was not a refund. Nothing in this system has seen a bank statement,
  so it is named for what it is rather than claiming a reconciliation to one.

### 15 · The providers are chosen — *answered 2026-09-03*; two of three built

**SMS: Taqnyat — built.** `modules/messaging/src/taqnyat.rs`. `POST /v1/messages`
with a bearer token and the three fields the OpenAPI spec marks required. Three
things the documentation makes non-obvious, and all three are tested:

- `recipients` is an array of **unquoted JSON numbers**. Every example Taqnyat
  publishes — the spec, the docs curl, both of their own SDKs — sends
  `[966500000000]`, and whether a quoted string is accepted is documented
  nowhere. A number written the Saudi way, `0500000000`, is refused here rather
  than sent: parsing it as an integer drops the leading zero and addresses
  `500000000`, which is a different number that might exist.
- **A `201` is not a send.** The body carries `accepted` and `rejected`, and a
  rejected recipient still comes back `201`. Reporting that as delivered would
  be the worst kind of wrong, so the body is read on success too. Both fields
  are strings shaped `"[966500000000,]"` — bracketed, trailing comma, not JSON.
- **Exactly one documented 400 is retryable** (`SMS-API not responding`).
  Everything else — an empty balance, an unregistered sender, an unauthorised
  IP — is permanent, and retrying an empty balance on a timer never becomes
  money.

**Push: FCM — built.** `modules/messaging/src/fcm.rs`. The legacy
`Authorization: key=…` API was shut down from July 2024, so this is HTTP v1 and
authenticates with a short-lived OAuth 2.0 access token: an RS256 JWT signed
with the service account's key, exchanged at Google's token endpoint, cached to
fifty-five minutes. Google's own documentation says to use their client library;
their library is not available here without a second TLS stack, and the flow is
one signed assertion over a POST. The test verifies the signature it produces
against the public half of the key that signed it, which is what Google's server
does with it.

**One decision worth reading**: `UNREGISTERED` retires a device token and
**`SENDER_ID_MISMATCH` does not**. The second means the credentials in this
process belong to a different Firebase project — one wrong environment variable
— and retiring on it would erase every push token a tenant has, unrecoverably,
because of a deployment mistake. Google's own token-management guidance names
only `UNREGISTERED` and `INVALID_ARGUMENT` as invalid-token signals, and
`INVALID_ARGUMENT` also covers "your payload is broken". So one code retires,
and it is the one that means the app is gone.

**WhatsApp: not built, and the reason is architectural rather than effort.** See
§26.

`Relay` stays, and is now the escape hatch it was always meant to be rather than
a stand-in: a named gateway wins over the relay for its channel, and a channel
with neither leaves its effects in the outbox.

**What is still not proven.** No account exists for either provider, so the
first live call is the operator's. What is tested is the bytes each client puts
on the wire and the answer it makes of every documented reply — against a
hand-written server that shows those bytes rather than a mock that agrees by
construction. That is the same honesty the ZATCA client carries.

### 15a · Two push tokens that look identical

Found writing the FCM adapter. `push::tokens` returned `Vec<String>` — the
platform column was dropped on the way out — so a transport was handed an opaque
string with no way to tell an Apple token from a Firebase one by looking. FCM
would have answered `INVALID_ARGUMENT`, which reads exactly like a payload bug
and is one of the two codes Google names as an invalid-token signal.

`Outbound` now carries `platform: Option<Platform>`, set on push and nothing
else, and the FCM adapter refuses an `apns` token with a sentence somebody can
act on. Optional rather than defaulted, so push effects enqueued before this
field existed still deserialize instead of being dead-lettered on a rolling
deploy.

### 16 · `crm` could not change a customer's phone number

`amend_customer` decided *nothing moved* by comparing the name and the VAT
number — the only two fields the aggregate kept — so an amendment that changed
the phone number, the email, the address or the Latin spelling wrote **no event
and did nothing at all**. The caller got `Ok`.

The comment defending it said the projection holds the rest and re-writing an
identical row is harmless, which is true and beside the point: the check ran
before anything looked at the rest.

Found on the first day something depended on a customer's number being current,
which is `messaging`'s first premise. The aggregate now holds every field the
event carries, because an aggregate cannot answer "did anything move" about a
field it does not have. `changing_anything_an_amendment_carries_writes_an_event`
is the regression test, and it fails on the old code for four separate fields.

### 17 · The tenant dispatcher had no handlers at all until now

Worth stating plainly because it changes what §9e's note meant. The reason `hr`'s
expiring-document reminder is a health finding rather than an effect was not that
a finding is better — it was that an effect enqueued from a module would have sat
in the outbox for ever, because nothing claimed tenant-plane effects.

`messaging` registers the first four. The `hr` reminder can become a real message
now, and it should: it is a template about an employee addressed to their branch
manager, and every piece of that exists. **Not done in this pass** — it belongs
with the rest of 11's producers rather than smuggled into the module that made it
possible.

### 18 · The S3 engine — *answered 2026-09-03, and built*

`object_store`, and not `aws-sdk-s3`. The deciding argument was TLS: this build
links exactly one stack, OpenSSL, and says so in four places in `Cargo.toml`.
`aws-sdk-s3` offers no native-tls option at all, so taking it would have put
rustls and `aws-lc-rs` in the same process permanently. `object_store`'s `aws`
feature does the same — its `aws-base` feature does not, at the price of
supplying a `CryptoProvider`, which is SHA-256 and HMAC-SHA256 over the OpenSSL
that is already here.

Measured, not estimated: **+8 compiled crates** (`object_store`, `quick-xml`,
`crc-fast`, `itertools`, `humantime`, `h2`, `fnv`, `spin`), and `reqwest`
0.12 → 0.13, which `object_store` requires. `rustls` and `ring` appear in
`Cargo.lock` as unenabled optional dependencies of `reqwest` and are never
compiled — `cargo tree -e normal -i rustls` prints nothing.

**One trap worth writing down.** In `reqwest` 0.12 the feature to ask for was
`default-tls` and it meant native-tls; in 0.13 `default-tls` means *rustls*. The
manifest line that was correct before the bump would now quietly pull in the
stack the whole comment above it exists to keep out, so the backend is spelled
out as `native-tls`.

And it is tested against a real bucket rather than a mock: `compose.yaml` runs
MinIO, and `crates/erp-storage/tests/s3.rs` round-trips five bytes, four
megabytes, a nested key, a missing key and an overwritten one through it. A mock
agrees with you by construction, and everything that can go wrong here — the
SigV4 signature, the wire format, path-style addressing — is on the wire.

**What is deliberately not built:** presigned URLs. Every byte goes through the
API process, which is the shape the rest of this system has: the route that
serves a file is the route that knows who is asking. Handing a browser a signed
URL is a different authorization story. It is also the one thing that would
change the answer above — the SDK's presigner and its per-provider quirk
coverage would be worth forty-six crates the day tenants upload direct.

**What this does not prove:** Hetzner or Contabo specifically. MinIO is a
faithful S3 implementation and is neither of them, and no credentials for either
exist in this build. Contabo's gateway answers JSON where S3 answers XML, and
Hetzner documents a `CopyObject` caveat; neither is reachable from here.

### 18a · Two tenants shared a storage key, and one bucket makes that fatal

Found while wiring the engine. `files::key_for` produced
`invoice/INV-1/doc-1` — no tenant anywhere in it. One process serves every
tenant and holds **one** `Storage`, and invoice numbers are unique inside a
tenant and nowhere else, so two companies both having an `INV-1` — the normal
case, not an exotic one — meant the second upload overwrote the first and the
first read returned the other company's contract.

`Local` hid it behind the fact that nobody runs two tenants against one
directory on purpose. A bucket does not hide it.

The key is now `{tenant}/{kind}/{owner}/{file}`, with the tenant's **id** rather
than its subdomain: a company that renames itself has not moved any of its
documents. `two_tenants_with_the_same_invoice_number_do_not_share_a_key` is the
regression test.

No migration is written, because nothing has stored a file under the old shape
outside a test. If that stops being true before release, it needs one.

### 19 · The body limit moved from the binary into the router

`MAX_BODY` was in `bin/api.rs`, so `erp_api::router()` — what the test harness
and any second binary use — had axum's own default instead of the API's. Nothing
depended on it until a file upload needed a different one, and then the
difference was a test that passed for the wrong reason.

It is `erp_api::routes::MAX_JSON_BODY` now, applied inside `router()`, with
`modules/files` raising it for its two upload routes and nothing else.
`the_raised_body_limit_applies_to_uploads_and_nowhere_else` asserts both halves.

Every route with a body now declares `413`, which the contract check caught
before I did.

### 20 · The asynchronous export is not built, and the reason is a cap

Every list in this API is capped at a page — 200 rows, 500 in a couple of places
— so no export takes a minute and none holds a connection long enough to matter.
Building "generate, store, send a link" now would be building it against a
problem that does not exist, and the shape would be guessed rather than measured.

What changed is that it is no longer a *design* question. A file is
`modules/files`, an effect is the outbox, and a link is `erp-links`; all three
landed this pass. When a list becomes unbounded — an all-invoices export is the
obvious one — it is a job that composes three things that already work.

**The synchronous half is done and is the useful half today**: `Accept:
text/csv` on any list, as one layer, so a list added tomorrow is exportable
without anybody remembering to make it so.

### 21 · An API key's secret is digested, not Argon2

The plan says "the same posture as a password". A key is shown once and stored
hashed, which is the posture; what it is **not** is Argon2, and the difference is
deliberate.

A password is short and chosen by a person, so the slow hash is what stands
between a stolen dump and a dictionary. A key is 256 bits from the OS: there is
no dictionary, and Argon2 would put roughly 50ms on **every request** an
integration makes — which for the thing most likely to be called in a loop is
the wrong end of the trade.

This is the argument `0004_authentication.sql` already makes for session tokens,
in the same schema: *"a token is 256 bits of entropy, so unlike a password there
is nothing to brute-force and no need for a slow hash; the point is only that a
leaked database dump cannot be replayed."*

The comparison is constant-time regardless.

### 22 · A key acts as a machine identity, not as the person who made it

Issuing a key creates an identity with no password and joins it to the tenant.
The alternative — the key carries its creator's identity — has two failures a
business meets: the key dies when they leave, and everything it did reads in the
audit trail as theirs.

`created_by` records who asked, which is a different and equally necessary fact.

The pleasant consequence is that a key needs no new code downstream. Membership,
roles, `enter`, the audit trail and every module's `metadata(&tenant)` work
unchanged — and a key for one tenant is nothing on another's subdomain because
its identity is a member of exactly one, which is a check nobody had to remember
to write.

### 23 · Webhooks are received and nothing handles them yet

The inbound half is built and verified: a signature checked before the body is
read, a replay window that a kept copy cannot get around, and the provider's own
id as the idempotency key so three deliveries are one effect.

What is not built is a **handler**. `webhook.<provider>` is promised to the
outbox, and nothing registers for it — payments are 12a and a delivery receipt is
`messaging`'s to claim. Until one does, a verified callback is recorded and its
effect waits, which is the dispatcher's documented behaviour for a kind nobody
handles, and `GET /v1/hooks/{provider}/events` is what makes it visible.

The polling reconciliation in the same section needs a provider to poll, which is
the same decision. **Both unblock together**, and the decision is §15's: which
gateway, and whether an SDK may be added.

### 24 · HMAC is fifteen lines here rather than a dependency

It is a 1997 specification, it is verified against RFC 4231's published vectors —
including case 6, a key longer than the block size, which is the line every
hand-written HMAC gets wrong — and the alternative is a crate whose correctness
is checked exactly as much.

Say the word and it becomes `hmac`; the tests do not change either way, which is
the point.

### 25 · Phase 12a — the gateways are chosen, *answered 2026-09-03*

**Cards: Moyasar. Buy-now-pay-later: Tabby and Tamara.** All three document a
public HTTP API, so none needs an SDK on the dependency list.

Everything around 12a already exists:

- **The inbound half.** A verified, deduplicated webhook that promises
  `webhook.<provider>` and waits for a handler (12b).
- **Effects with a retry policy, a lease and a dead letter** (D9, Phase 2).
- **The ledger seam.** `sales::pay_in` and `ledger::post_entry_in` already
  compose inside one transaction, which is what settlement and fees will need.
- **A shared secret per provider**, sealed (12b).

**Tabby and Tamara are not Moyasar wearing different branding**, and the plan
already says why: the provider pays the merchant and collects from the buyer, so
the receivable is settled by a third party and the entries differ. Getting that
wrong shows up as a debtor who has already paid. They share a `Gateway` trait
and they do not share posting rules.

The parts of this that are **domain rather than vendor** — a refund against a
cleared invoice is a credit note; a payout reconciles to payments minus fees; a
fee is an expense and never a smaller revenue; a BNPL receivable is settled by a
third party — are testable here in full. The parts that are vendor are testable
against a fake HTTP layer and not against a live account, which is the same
position §15 leaves the messaging adapters in.

**Built 2026-09-03: `crates/erp-payments` is the vendor half, and all three
gateways are in it.** The split is the one `erp-storage` makes — a thin crate
that knows an amount, a buyer and three providers' HTTP APIs, and knows nothing
about invoices or accounts. `modules/payments` is the domain half and is next.

**Money on the wire is the expensive disagreement.** All three want something
different, and getting it wrong charges a hundred times too much:

| | `amount` |
|---|---|
| Moyasar | JSON **integer**, minor units — `1.00 SAR` is `100` |
| Tabby | JSON **string**, major units — `"100.00"` |
| Tamara | JSON **number**, major units — `100.50` |

Moyasar's is exactly what `Money` stores. The other two go through
`erp_payments::decimal`, which is integer division and remainder — floating
point is forbidden here and `100.50` has no exact binary representation anyway,
so a round trip through `f64` would be a rounding step in the middle of somebody's
bill. **Tamara forced the body to be built as text**: an unquoted decimal cannot
be produced by `serde_json` without an `f64`, so that one client writes its own
JSON and a test asserts the result still parses.

Reading is the same hazard backwards, and it is the one that loses a halala:
`"amount": 300.50` parsed into a float and multiplied by a hundred. Responses
are read as raw text and parsed as digits.

**Idempotency: one out of three.** Moyasar's `given_id` is a real one — a UUID
the caller supplies that *becomes* the payment id — so the client refuses a
reference that is not a UUID rather than silently omitting it, because without
it every network timeout is a possible double charge. Tabby has `reference_id`
on capture and refund and nothing on checkout. **Tamara has none at all.** That
is a property of the world, written down rather than discovered.

**A card number has nowhere to go.** `Source` has two variants — a gateway token
and "the provider hosts the page" — and neither holds a PAN. Moyasar's terms
make sending one to the merchant backend grounds for terminating the agreement.

**What each provider's lifecycle actually means**, since none of the three
agrees and two of them mislead:

- **Tabby's `CLOSED` is not "paid".** It is the terminal state for captured in
  full, cancelled without capture, *and* partially captured then closed. So the
  adapter reports `Paid` only when something was actually captured, and a
  payment closed with nothing captured is `Voided`. A partial capture also
  leaves the payment `AUTHORIZED` for ever — the leftover is not released on its
  own, and Tabby's own backstop is *"after 21 days, Tabby may capture the
  remaining amount in full on its side"*.
- **Tamara's `approved` is not `authorised`.** When the customer comes back they
  have paid the first instalment and **the merchant still has to act**; an order
  left at `approved` expires after 72 hours. So `approved` reads as
  `Initiated` — somebody still has to do something — and `capture` authorises
  first, reading the `auto_captured` flag so an account that captures on
  authorise is not captured twice.
- **Only a capture is money for either of them.** *"Orders NOT captured are NOT
  settled to your account."* Tamara's phrase about `authorised` — "you can
  consider the order as paid" — is about credit risk, not cash, and the adapter
  does not repeat it.

**Neither BNPL provider will take an anonymous charge.** Both score the buyer
before they will lend, so `Charge` carries an optional `Buyer` and `Basket` and
the adapters refuse without them, naming the missing field. A shop assistant can
act on "we need their mobile number"; they cannot act on a Tabby validation
error.

### 53 · Phase 5b is unblocked — by a consumer the plan never claimed

**Audit finding, 2026-09-09.** The plan says four consumers now exist and the
rules engine's deferral condition is met. Checked one by one, **the count is
really one and a half** — and the phase is unblocked anyway, for a different
reason.

| Claimed consumer | Verified |
|---|---|
| Booking automations — reminders, no-show handling, recall follow-ups | **Do not exist.** `automation`, `recall` and `follow up` return zero hits across every `.rs`. `booking` does not depend on `messaging` or `notifications` at all. `Stage::NoShow` is a manual status with no fee, no rebooking and no rule |
| HR document expiry | **Real, and trivial** — the whole rule is `DOCUMENT_WARNING_DAYS: i32 = 60` |
| §9b's claim union | **Real as data, and a live consumer since 2026-09-09.** It was inert when this table was written — see §52 — and three commands check a claim now, each inside its own transaction, which is the constraint §9c derives |
| Authorization | **Real**, and the only one whose seam is written into the code |

**The real second consumer is pricing, which shipped and which the plan still
says does not exist.** `modules/booking/src/pricing.rs` is 524 lines:

```rust
pub struct Band { name, when: Availability, uplift }
```

That is `Rule<Uplift>` with `when: Availability` in place of `when:
DynCondition` — a **tenant-authored, serialized, first-match condition language
over one fact type**, configured over HTTP with ETag/If-Match and evaluated
inside the booking's own transaction, frozen onto the line (L5). And crucially
it is *not* our own aggregate standing in for demand, which is the error §8's
reconciliation caught: **a tenant writes the bands.**

#### What this changes about how 5b should be specified

Not "build `Facts`, `DynCondition` and `FactRegistry`, then move two things onto
them". The honest first box is **"generalise the one fact in `Availability` and
keep everything else"** — there is a working evaluator, a working authoring
surface and a working freeze-at-decision-time story, over a vocabulary of one.
Authorization's vocabulary is identity and role; pricing's is time. Two thin but
real fact types is a basis. Four, two of them fictional, is not.

#### And half of one 5b box is already built

`explain`-backed dry run and effective-permission inspection are one box.
**Inspection ships twice**: `modules/hr/src/claims.rs:485` `effective` returns
claim, branch *and source*; `GET /v1/members` returns each member's tenant role
plus module exceptions. `explain` and `dry_run` return zero hits — and the dry
run needs the *same* rolled-back-transaction primitive 4d's preview needs, so
the two should be built once rather than twice.

### 54 · A chart that would have failed its owner's first invoice

**Caught by CI on 2026-09-09, and the interesting part is what the fix found.**

The `real_estate` chart shipped without `4000`. Every other chart puts its
principal revenue there — `services` service revenue, `retail` sales — and
`sales::PostingAccounts::conventional()` maps revenue to it. A landlord
installing that chart would have had **their first invoice fail to post**.

`sales::conventional_codes_exist_in_every_shipped_chart` caught it, which is
exactly the job it was written for. Rent moved to `4000`, where it belongs
anyway: for a letting business rent *is* the principal revenue, and service
charges and commission sit above it at `4210` and `4300`.

#### What the fix uncovered

**Five modules claimed the guard and two had it.** `pos`, `prepaid` and
`purchases` each carried the sentence *"the codes every chart in
`ledger::CHARTS` ships"* on their `conventional()` with **no test enforcing
it**. The claim was true when written and nothing kept it true.

Adding the missing three immediately caught a second hole in the same chart:
`pos` maps a till's difference to `5910`, and `real_estate` had no `5910`.

**And the guard that existed was incomplete.** `payments` checked four of its
five conventional accounts — `forfeited` (`4910`) was in the mapping and not in
the assertion, so a chart could ship without it and the test would pass.

So one failing test in CI led to: one real defect, three missing guards, and one
guard that did not cover its own subject. All six now bite, proved by removing
each account in turn and watching the right module fail.

#### The process lesson, which is the same one as before

`ledger` was the crate I changed and `sales` was the crate that failed. I ran
`-p ledger` and stopped. **This is the third time the same shape has caught
me** — the pooler test in `erp-control` after adding two modules, and the
`erp-control` migration guard after adding one.

The rule that would have caught all three: **after changing anything in a crate
that others depend on, run the dependents, not the crate.**
`grep -rn "ledger::CHARTS"` takes five seconds and names them exactly.

### 52 · Claims are granted, displayed, and enforced nowhere

**Found by audit 2026-09-09. This is a defect in shipped behaviour, not a
planning inaccuracy, and it had no box anywhere.**

`modules/hr/src/claims.rs` is complete and correct. It places claims on an org
chart, unions them upward, refuses the segregated ones, and answers who holds
what:

- `grant`, `revoke`, `place`, `withdraw` — maintained transactionally in
  `migrations/tenant/0008_org_claims.sql`
- `effective` — claim, branch and *source*, so a screen can say why
- `holds(conn, employee, claim, branch)` — **the check**
- `SEGREGATED` — `purchases:approve_payment`, `sales:approve_credit_note`,
  `hr:approve_timesheet`, whose stated purpose is segregation of duties
- `GET /v1/hr/employees/{employee}/claims` — a tenant can see the result

**`holds` has exactly one caller in the entire repository, and it is
`modules/hr/tests/hr.rs:171`.** No module imports it. No command consults it. A
tenant can grant `purchases:approve_payment`, see it on the screen, put it in
front of an auditor — and no code path will ever ask.

The demo does exactly this: `crates/erp-demo/src/lib.rs:1682,1696` grant
`sales:apply_discount` and `purchases:approve_payment`, and nothing checks
either. Combined with `modules/hr/src/http.rs:1768` — *"a claim name is checked
for shape and never for meaning"* — the system will accept and display any
control a customer invents, including one it does not implement.

**Why the documentation did not catch it.** §9b and §9c both assert the checks
exist. §9c says the claims are checked *"inside commands, not at the edge …
inside a transaction that is holding a connection"*, and derives a performance
constraint from it. **That code does not exist.** The passage describes a design
that was specified, argued about, and never wired up.

#### What to do, and why it is small — *done 2026-09-09*

One command calling `hr::holds` before it approves something. `purchases`'
payment approval was the obvious first, because `purchases:approve_payment` is
already in `SEGREGATED` and already granted by the demo.

**Two decisions were taken by asking rather than guessing**, and both are the
stronger reading:

- **The control switches on with the first grant anywhere in the tenant**, not
  per claim. Granting one claim makes every checking module start checking.
- **An owner is exempt.** The question asked was about somebody with no employee
  record; it applies to **any owner**, staff or not — put back to you on
  2026-09-10 and confirmed. Exempting only non-staff strands an owner who is
  *also* on the org chart the moment they grant the claim.

  **The residual risk is accepted, not overlooked**: an owner can approve their
  own payment, credit note and timesheet. The alternative is a control that
  stops a one-person business working at all.

**All three `SEGREGATED` claims are now checked** — the other two wired the same
day, each a small diff because `hr::may` already existed:

| Claim | Guards | Where |
|---|---|---|
| `purchases:approve_payment` | Paying a supplier | `purchases::pay_bill` |
| `sales:approve_credit_note` | Cancelling *or* partly crediting an invoice, and asking a gateway for a refund that will leave one owing | `sales::may_credit`, in `cancel_in`, `credit_part_in` and `may_refund` — the roots every credit note goes through, since §70 |
| `hr:approve_timesheet` | Recording a day worked | `hr::record_day` |

*Since §68 a fourth claim is checked*, and not a segregated one:
`sales:exceed_document_limit` lifts the owner's per-document limit in
`sales::issue_in`, the credit-note roots and `refund_in`. It is asked with
`hr::actor_holds`, not `hr::may`, because the limit, not the first grant,
switches that control on.

**One helper for both credit paths**, because a full cancellation and a partial
credit are the same authority and two copies of the check would eventually
differ. `sales` gained an `hr` dependency for it, the same sibling edge
`purchases` took; `hr` needed none, since it owns claims.

**The check sat before the retry loop** in both credit paths, so the answer
could not change between optimistic-concurrency attempts. §70 moved it to the
top of each root, which is inside that loop: a contended credit note asks once
per attempt, and an uncontended one — every one in practice — asks exactly once,
as before. The *question* is still asked there; §70's review moved the
**refusal** into the decision, after the retry check, so that a retry answers
with the document it issued rather than with a 403 once the claim is revoked.

**Not claim-judged, deliberately:** a credit note with `Authority::System`
behind it, which is what `payments` passes `sales::credit_what_is_clear` when a
gateway refund lands. That is a consequence of a refund that already happened,
not a person approving a credit note — the same reasoning that lets a worker
with no actor through. Until §70 the *function* was the exemption; now the
authority is, so the same function called by a member — a refund at
`/v1/sales` that clears an invoice — does ask. A member asking a *gateway* for
one is asked when they ask, in `may_refund`, because by the time the gateway
answers there is nobody left to ask.

#### Self-approval — *decided and built, 2026-09-10*

A person holding `hr:approve_timesheet` could approve **their own**.
`SEGREGATED` stops a claim travelling *up* the org chart; it never stopped
self-approval, while the comment beside the list — *"approving your own
timesheet is the same shape one module over"* — read as though it did.

`hr::may_for` is `may` with a **subject**, and returns a reason rather than a
`bool`, because the two refusals are different sentences: *ask somebody who
holds it* and *ask somebody else entirely*. `may` is now the no-subject case of
it, so there is one policy and not two.

**The order matters.** Self-approval is checked *after* the claim, so somebody
who does not hold it at all is told that — being told "not your own" would imply
they could sign somebody else's.

**And the owner exemption carries through, which is the part worth arguing.** A
sole trader is their own only employee. Refusing self-approval outright would
stop them recording a single day worked, with nobody on earth able to do it for
them — the lockout shape a third time. An owner overriding a control they own is
the accepted residual risk in every accounting system; a control that stops the
business working is not. There is a test named for that case.

Four guards, all falsified — and the last of them only after a falsification
that **passed** revealed the "employee without the claim" path was never
reached: the existing test used a login that named no employee, so it exited
before the claim lookup.

#### A house rule, arrived at three times

**Switching a control on must not be the act that strands you.** Three controls
in two days landed on it independently:

- **Tenant second-factor** refuses to be *enabled* by somebody who has not
  enrolled one, and can always be *disabled* without one.
- **Claim checks** exempt an owner, so granting the first claim cannot lock the
  person granting it out of their own books.
- **Self-approval** exempts an owner too, because a sole trader is their own
  only employee and the segregation the control asks for is arithmetically
  impossible for them.

Worth stating once rather than rediscovering: a control whose failure mode is
"the business cannot operate" is worse than the risk it removes, and the owner
is where that pressure is always relieved.

It is also the thing that unblocks Phase 5b honestly — see §53.

- [x] **Wired 2026-09-09.** `purchases::pay_bill` consults `hr::may` before
      anything is written, and refuses with `PurchaseError::NotApproved` in both
      languages. `hr::holds` now has a production caller.

      **In the command, not at the route**, as §9c says: a job paying a bill is
      still approving a payment, and a route-layer check would not see it.

      **The policy lives in `hr::may`**, not in `purchases`, so the next caller
      inherits it rather than re-deriving it. Three answers in order: the tenant
      has granted no claim at all → permitted, and nothing changes for anyone
      who does not use claims; the caller owns the tenant → exempt; otherwise
      they must be an employee holding the claim in the branch they named.

      **A caller with no employee record is refused.** No claim can reach
      somebody outside the org chart, and passing them through would be a hole
      the size of "make a second login".

      Five guards, all falsified — including one that only started biting after
      a *failed* falsification revealed the non-staff path was untested.
- [ ] Decide what a claim on a module that has no check means: today it is
      accepted and displayed. Refusing an unknown claim name needs the registry
      `http.rs:1767` deliberately declined; **surfacing** unenforced claims on
      the read model may be the cheaper honest answer. *Still open after §68*,
      which adds a fourth name with a check (`sales:exceed_document_limit`);
      a misspelling of it is accepted and lifts nothing, which fails closed
- [x] **§9b and §9c corrected 2026-09-10.** Both now say when they became true
      rather than describing enforcement in the present tense while none
      existed. Every factual claim in the corrected text was checked back
      against the three call sites — `purchases/commands.rs:369`,
      `sales/commands.rs:46` (from both credit paths, as they stood then; §70
      moved that check into the credit-note roots, and it is `:69` now) and
      `hr/commands.rs:732`

### 90 · Researched, not built: operations held for approval, and classifications set by rules

**Asked for 2026-09-18.** Two features, one finding: both are rules over the
facts of an operation, and the rules crate already has the machinery —
`DynCondition`, `Facts`, a validated `FactRegistry`, `Rules<E>` with first
match and `explain`, authoring templates. What neither has is the *vocabulary*:
the registry that exists (`erp_tenant::limits::registry`) knows four facts —
`amount`, `branch`, `capability`, `role` — and the amount reaches it only on
the ledger's hand-posted entries, which Priority 5 already lists as a gap. So
the first piece of work is shared: **the facts of an operation, computed inside
the command where the total exists** — operation, module, amount, currency,
branch, requester, requester's role and claims; then product, category,
customer, payment method, discount. One vocabulary, three consumers: the
permission limits (refuse), approvals (hold), classifications (tag). This is
the rules phase's deferred half with two real consumers behind it, which is
what §5b said it was waiting for.

#### Approvals — what exists

Approval today is a **gate, not a workflow**. Five claims
(`sales:approve_credit_note`, `sales:exceed_document_limit`,
`purchases:approve_payment`, `hr:approve_timesheet`,
`hr:reset_second_factor`) are checked by `hr::may` at command time: the person
*doing* the thing must hold the claim, or it is refused. Only payroll runs and
timesheets have two steps, and those are compiled into their aggregates.
Nothing can say *"hold this and ask somebody"*. The owner exemption, the
self-approval rule and the house rule — *switching a control on must not be the
act that strands you* — are all decided and carry over.

**Reconciling with 2026-09-14.** The generic `StateMachine` driving document
workflows and approval routing was declined: documents stay compiled types.
This proposal keeps that. No document gains a generic state; the *operation*
is what waits.

#### Approvals — the proposed shape

**The gate gains a third answer.** Where a command calls `hr::may` today it
calls a gate with the operation's facts and gets *proceed*, *refuse* or *hold*.
The tenant's approval rules are `Rules<Approvers>` — the `Rule<ApprovalChain>`
the rules crate's own doc anticipated. On *hold* the command writes nothing of
its own: an `Approval` aggregate records the request **as the request** — route,
body, the caller, the facts as they stood — under the write's own
`Idempotency-Key`, and the route answers 202 with the approval's id. Approvers
hear by bell and email. On approval the API replays the stored request with
the same key; the command runs again, the gate finds the approval and proceeds,
and the event's metadata carries who approved. **Nothing exists until then** —
no draft, no number taken, nothing posted — so there is no gap in a ZATCA
series and no half-document in a list.

**Staleness is handled by running again, not by trusting the snapshot.** The
replay computes fresh facts. If the operation now fails — the stock went, the
period closed — the requester is told why. If the facts exceed what was
approved — the amount grew — it is held again. Pending requests expire.

#### Classifications — what exists

The cost center (§87): one dimension, per line, optional, defaulting to the
entry's branch, carried by manual entries and purchase bill lines only, cut on
the P&L and the journal; reassigning is a reversal and a repost; `reports`
does not have it. Custom fields, which the 2026-09-14 note mentions, do not
exist in code.

#### Classifications — the proposed shape

**Tenant-defined dimensions beside the cost center** — channel, department,
project, doctor, campaign — each a list of values opened and closed like cost
centers. A line on a document carries its values, the posting lines a module
derives from it inherit them, and they are frozen on the event (L5). **Rules
set them:** per dimension, `Rules<Value>` over the same facts, first match; a
value sent on the line wins; `explain` says which rule tagged it. It is the
rule the accounting-rules conversation of 2026-09-16 ended on — *the engine
decides the label, code decides the entry* — and per-product accounts are its
first instance. Rule changes apply forward.

#### What the product owner added, 2026-09-18

**Approvals are authored, per kind of operation.** Somebody with the
permission opens *Approval workflows*, picks a kind of operation — discounts,
invoices — and adds a rule: *if the total discount is X% of the invoice,
require authorisation by supervisor X*; *if the invoice total is 50,000 SAR or
more, require approval by supervisor X*. Comparisons on amounts and
percentages; a **list of supervisors satisfied by any or by all**; and **"no
self-approval" as an option on the rule**. One engine, used by every section
of the system.

What that settles: rules are grouped by operation kind, so **each kind of
operation declares the facts it supplies** — its own small registry, which is
also what the authoring page reads to offer fields. The crate needs no new
operator: it has `eq ne lt lte gt gte`, `all/any/not`, and `Int`, `Text`,
`Bool`, `Money`; a percentage is an `Int` in basis points, as VAT rates are.
The consequence is `Approvers { who: Vec<user>, mode: any | all, own: bool }`.
One addition is needed: an operation can match **more than one** rule — a
discount rule and a total rule on the same invoice — and the crate's
`evaluate` is first match.

**Classifications are tags on whatever produces a journal entry.** The branch's
cost center is attached today; beside it go tags such as the point of sale,
the department, the specialist, the campaign — added automatically, by the
tenant's configuration. So a tag takes its value one of two ways: **bound** to
something the operation already knows (the till that rang it, the specialist
on the booking line, the campaign behind the promotional code, the product's
category), or **decided by a condition** over the operation's facts, producing
a value from the tenant's own list.

**Staleness, confirmed by the product owner the same day:** the replay
recomputes the facts as of the approval; a failure is the requester's to hear;
a fact *the matching rule names* that has grown past what was approved is held
again, and one the rule never mentioned is not; an unanswered request lapses
after the tenant's number of days, fourteen by default.

#### Classifications, researched against DualEntry (2026-09-18)

The product owner meant DualEntry's classifications — **instead of** cost
centers, not beside them. Their documentation's own words: *"Classifications
(also called dimensions) are tags you attach to individual transaction
lines"*; each **type** (Department) *"holds a list of values"* (Engineering,
Sales); they give *"granular reporting without inflating your chart of
accounts"* — one expense account, each line tagged with its department;
*"you can require a classification on certain transaction types so users
cannot post a line without tagging it"*; reports *"group by classification …
show either as a details column, and filter on them"*. A classification tags
**a line**, where their custom field describes a whole record. Their product
pages add **unlimited nesting** of values (against a single-level class or a
five-dimension ceiling elsewhere), allocation templates by percentage or
amount, and categorisation that suggests tags on bank-feed, scanned and
imported lines. It is the dimensional model of Sage Intacct, NetSuite's
segments and Business Central's dimensions, and the last of those documents
the part DualEntry's pages leave out — **how a tag arrives without anybody
typing it**: default dimensions on master records (customer, vendor, item,
account, employee), a priority order when two defaults disagree, and
per-account rules that a value is mandatory, fixed, or forbidden.

**What it means here.** There is no cost-center concept in that model: a cost
center is one classification type. Nothing is deployed, so §87 is reworked
rather than kept beside — the `CostCenter` aggregate becomes types and values,
`Line.cost_center` becomes the line's tags, `?cost_center=` and the
by-cost-center P&L become any type's. **The branch stays its own column**:
confinement reads it, and access control is not a reporting dimension. A
"Branch" type bound to it gives the same cut. **A tag arrives**, in order: what
the line says; a rule over the operation's facts; the default on what the line
names — the product, the customer, the specialist, the till, the promotional
code's campaign; nothing. Frozen on the event, explained on request.
**Decided the same day:** nesting, tags on revenue, expense and cost lines
only, that order of arrival, re-tagging by an event in open periods, forward-only
rules; **custom fields and allocation templates join this item**, and learned
suggestions wait for TypeSafe's Jev.

#### Open decisions, put to the product owner the same day

Approvals: the gate-and-replay shape itself; nothing existing until approved;
the first approvable operations; who approves (named users, claim holders in
the branch, the requester's manager); steps and whether any one approver
satisfies a step; owner never held and always able to approve, nobody
approving their own; expiry; API keys held and system jobs never; approval at
the till over the real-time stream. Classifications: nesting of values; which
posting lines carry tags; the order in which a tag arrives; required per kind
of operation and per account; which records carry defaults; re-tagging history
by an event that never touches amounts, or by reversal as §87 does;
forward-only rules. And where both sit in the road map — filed under Priority
5 until told otherwise.

### 89 · PDF/A-3, the XML attached

**Built 2026-09-16**, closing the customer-holds item, from three decisions
taken that day: typst over a hand-rolled writer (a larger binary is worth not
taking weeks), IBM Plex Sans Arabic (OFL, vendored under `modules/tax_sa/fonts/`
with its licence), a staff route and `?format=pdf` on the public link.

**What exists now.** `tax_sa::pdf::pdf(&Document, qr, xml)` — a typst `World`
serving exactly four files (`document.json` written from the stored document,
`qr.svg`, the XML under its attachment name, `invoice.typ`) and two faces, no
filesystem; `typst::compile` to a `PagedDocument`, `typst_pdf::pdf` with
`PdfStandard::A_3b`, the PDF id set to the number and the date to the
document's day on the business's clock. The template `print/invoice.typ` reads
JSON rather than being built from strings, so a `#` in a description is text,
not code; receipt or A4 by `d.receipt`, bilingual, the QR drawn from the same
SVG the page uses, `pdf.attach(d.attachment, relationship: "data", mime-type:
"application/xml")`. `GET …/documents/{number}/pdf` and the public link's
`?format=pdf` render off the request thread (`spawn_blocking`) and wait and
refuse exactly as the print does. The HTML print's formatting helpers are
shared, so the two never disagree.

**Proven by reading the bytes back** (`lopdf`, dev-only): PDF 1.7, XMP with
`pdfaid:part` 3 and conformance `B`, an `OutputIntents` entry, the catalog's
`AF`, exactly one `EmbeddedFile` whose decompressed bytes are ZATCA's stamped
document byte for byte, every `FontDescriptor` carrying a font programme.
veraPDF is not in CI: `write_a_sample_pdf` (ignored) writes a receipt, a
cleared invoice and a credit note of each kind to `target/` for a manual run,
and all four were handed over for one.

**Guards, both falsified** (revert → `the_pdf_is_pdf_a_3_with_the_xml_attached`
fails → restore): the standard set to plain PDF 1.7 instead of A-3b, and the
XMP identification goes; `pdf.attach` taken out of the template, and the
catalog has no associated file.

**Six typst-transitive advisories, ignored with reasons in `deny.toml`:**
`quick-xml` 0.38 twice (pinned by `citationberg` under `hayagriva`, typst's
bibliography — never called here; no upgrade in range), `bincode` 1 and
`yaml-rust` unmaintained (under `syntect` and `two-face`, typst's code
highlighting — never called here), and `rustybuzz` and `ttf-parser`
"unmaintained" by the archived-repository rule — typst's shaper and font
parser, which *are* used, pure Rust over fonts this product embeds itself, with
no successor to move to. All to be revisited at every typst release.

**Not done here:** a "not ready yet" page for a customer's browser; nothing
else on this item.

### 88 · What a customer holds: the print, and a link to it

**Built 2026-09-16**, the second Priority 2 item, from six decisions taken that
morning and recorded above.

**What exists now.** `tax_sa::print` — `deliverable(&Stored)` is the one rule
for whether a document may be handed over (a simplified invoice once signed, a
standard one once ZATCA cleared it and then ZATCA's stamped document with
ZATCA's QR read back out of it; refused and pre-registration documents never);
`html(&Document, qr)` renders the page, an 80 mm receipt or an A4 invoice,
Arabic first and English beside it, the QR inline as SVG through `qrcode`;
`LinkSecret` under `tax_sa.link_secret`, made on first use with OpenSSL's
random bytes, whose `token` is `INV-00001.<32 hex of HMAC-SHA256>` and whose
`opens` verifies in constant time. `Stored` gained the document JSON the
renderer works from (`.sqlx` regenerated).

**Routes.** `GET /v1/tax_sa/zatca/documents/{number}/print` (`text/html`) and
`…/xml` (`application/xml`, as a file) wait up to twenty seconds (`?wait=`)
for the worker's visit — the one the sale nudged — before 503
`tax_sa.not_yet_signed` (retry) or 409 `tax_sa.awaiting_clearance` /
`tax_sa.document_refused` / `tax_sa.not_deliverable`. `POST …/{number}/link`
hands staff the customer's path; `GET /v1/tax_sa/zatca/public/{token}` opens
it with no sign-in under `erp_web::Public` (bounded per caller and per
business, declared deliberate in `only_the_deliberately_public_routes_are_public`)
and 404 `tax_sa.no_such_link` for a byte off. The document view carries
`deliverable`.

**Guards, all falsified** (revert → the named test fails → restore): a receipt
handed over unsigned; a standard invoice handed over uncleared; a link opening
without its MAC; the print no longer waiting. Tests: the rule state by state,
the link, and the helpers (unit); the whole path in the module test — refused
before the signature, the nine-tag QR after it, the receipt's title, line and
bare amount, the standard invoice held until a recorded clearance and then
printing ZATCA's QR and document, the secret made once; over HTTP the 503, the
real wait, the 409 on the standard invoice, the XML, the 404, and the link
opened with no bearer and refused when forged.

**Not done here:** PDF/A-3 with the XML embedded, which is what a standard
invoice is *shared* as under ZATCA's rules (built the same day, §89); a "not
ready yet" page for a customer's browser (the link answers problem+json
meanwhile).

### 87 · A cost center is a dimension a line carries

**Built 2026-09-15**, the last of the Priority 2 statements item, from the five
decisions taken that day — all as recommended.

**What exists now.** `ledger::CostCenter`, an aggregate like an account
(`ledger.cost_center.opened/renamed/closed`; a closed one refuses lines), with
`open_cost_center` refusing an open branch's id as a duplicate because a branch
*is* a cost center. `Line.cost_center: Option<AggregateId>` (serde default, no
upcaster), checked per line in the one posting path — an open cost center, or
an open branch through the branches log, else 422 `ledger.no_such_cost_center`
/ `ledger.cost_center_closed`. The posting row's `cost_center` is what the line
named **or the entry's branch**, decided in the projection so old entries land
in their branch on the rebuild — read-model **version 3**, pin updated. A
reversal keeps the line's cost center. `cost_center` table and `CostCenters`
projection; `cost_centers` listing; `profit_and_loss` takes `cost_center`;
`profit_and_loss_by_cost_center` gives one row per (cost center, account),
unassigned last; `JournalFilter.cost_center`, `JournalLine.cost_center`.

**Who carries one.** Manual entries (`NewEntryLine.cost_center`) and purchase
bill lines (`BillLine.cost_center`, `NewBillLine.cost_center`, threaded through
`entry_for_bill`); everything else defaults to the branch, as decided.

**Routes.** `GET`/`POST /v1/ledger/cost-centers`, `PUT …/{id}` (rename), `POST
…/{id}/close`, `GET /v1/ledger/statements/profit-and-loss/by-cost-center`
(one column per cost center, the same per-currency shape inside each), and
`?cost_center=` on the P&L and the journal beside `?branch=`. Confinement
untouched: a confined member's reads stay pinned to `posting.branch`.

**Guards, all falsified** (revert → the named test fails → restore): the
per-line check removed; a line no longer falling to its branch; a reversal
dropping the cost center; a bill line dropping its cost center; a branch's id
opened as a cost center. Three tests: the ledger module (every rule: naming,
the branch default, unassigned, unknown, closed, the branch as a cost center,
the reversal, both cuts, the journal filter and the list); purchases (a bill
line charges its department, the other is unassigned); HTTP (every route and
both refusals, the by-cost-center order with unassigned last).

### 86 · FX is deferred, and a Saudi tax document is in riyals

**Decided 2026-09-15.** The FX ladder — provider, functional currency, translated
statements, cross-currency entries, revaluation — was built for a case this
market barely has: the riyal is pegged to the dollar, most tenants are
riyal-only, and the ones that touch dollars already know what they did with
them. It is deferred to *when a tenant asks*, and what they will ask for first
is the exchange itself (one entry, two currency legs, a stated rate), which
needs a functional currency and no provider. What the ledger promises meanwhile
is exact: per-currency books and per-currency statements; a tenant that
converts on receipt records riyals; one that keeps dollars keeps a dollar book
beside; the gain or loss on an exchange has nowhere to go and sits in a clearing
account per currency until then.

**The one thing not left to the business.** ZATCA states the tax on an
e-invoice in riyals whatever the invoice is in. `sales` let an invoice be in any
currency, the VAT return sums per currency, and `tax_sa` has no riyal tax amount
for a dollar document — a compliance hole, quiet. Now `tax_sa` **seeds**
`tax.document_currency = SAR` (`schema/seed.sql`, `DO NOTHING`, so a tenant's
own value survives a re-install like the rate does) and `sales::issue_in` reads
`sales::DocumentCurrency` in the issuing transaction and refuses any other
currency — 422 `sales.document_currency` naming both — before anything is
written. Every document ZATCA sees is issued through `issue_in` (the till's
included), so the one check covers them; a credit note takes its invoice's
currency. A tenant under no tax module has no rule.

**Guards, both falsified:** the check removed → the sales and HTTP tests fail;
the seed removed → the tax module's and the HTTP tests fail. Three tests: the
sales module (refused before anything is written, riyals still issue), `tax_sa`
(seeded at install, a tenant's dirhams survive a re-install), HTTP (a Saudi
tenant's dollar invoice is a 422 naming both currencies, the riyal one a 201).

### 85 · A period closes in order, and a year books into retained earnings

**Built 2026-09-14**, the second Priority 2 item, from eight decisions taken
that night and recorded above — all as recommended.

**What exists now.** Two acts in `ledger::period`. `close_period_in` moves the
watermark to a period's end, in order (the period to close is the one the
watermark is in; the first ever may be any, and swallows the history before it);
`reopen_period_in` moves it back, only for the latest closed period and never
while its year is booked. `close_year_in` **books** a year once every period of
it is closed: per currency, every trading account's balance over the year is
posted away and the result to the retained-earnings account for that currency —
`ClosingAccounts` under `ledger.closing_accounts`, `3100` serving any currency
it holds unconfigured — dated the year's last day on the tenant's clock, id
`closing-{year}-{CUR}-{n}`. `reopen_year_in` reverses those entries and is
refused while a later year is booked. `Books` gained `years`, a map of
`BookedYear { booked, closes, entries }`, written with the version it was read
at so two accountants closing at once conflict. The VAT return's
`close_through` is unchanged and never books.

**The flag.** `JournalEntryEvent::Posted` carries `closing: bool` (serde default,
so no upcaster), `proj_ledger.posting` a `closing` column — read-model
**version 2**, so the migrator rebuilds the ledger group on deploy — and the
profit and loss is the one query that leaves closing postings out; balances,
the sheet and the journal count them, which is what keeps `3100` and the
computed prior-years line from both holding the result. Closing entries are the
one posting allowed into closed time, through the same `post_in` every posting
uses, and `reverse_in` refuses one by hand (409 `ledger.closing_entry`).

**The calendar is segments.** `FiscalCalendars { segments }` under the old key,
reading a single stored calendar as a list of one. `with` appends a calendar
whose start is in open time and on a fiscal-year boundary of the previous
segment (400 `ledger.not_a_year_start`, 409 `ledger.calendar_locked`), dropping
later pending segments and always keeping the first — the closed history was
closed under it, even when its anchor is on or after the new start, which is
why `on`/`for_year` give the first segment everything before the second
regardless of its own anchor. Nothing closed: replace, as before.

**Routes.** `PUT /v1/ledger/books` is gone. `POST /v1/ledger/periods/{p}/close`
and `…/reopen` (204), `GET /v1/ledger/years/{y}` and `POST …/close` and
`…/reopen` (200 with the year and its closing entries), `GET`/`PUT
/v1/ledger/closing-accounts` (`{ "USD": "3900" }`), `GET /v1/ledger/books` now
lists the booked years, the journal shows `closing`. Every change is
`ManageAccounts`. Refusals: 409 `period_out_of_order` (naming the next),
`period_not_latest`, `year_booked`, `year_open`, `later_year_booked`,
`closing_needs_account`; 503 `read_model_behind` when the ledger group has not
projected to the head, because the figures a close posts come from it.

**A bug the test found.** The first version posted each currency's entry as it
went and refused on the second currency — on a bare connection in the module
test, the riyal entry had already landed. Every currency's entry is now built
and every refusal found before anything is posted, so a refusal never depends
on the caller's rollback.

**Three standing guards fired on the full run, each rightly.** The read-model
pin in the migrator (`ledger` re-pinned at 2 with its new install hash); the
role matrix, which found `GET /v1/ledger/years/{year}` answering axum's
plain-text 400 for a non-numeric year — the year is parsed in the handler now,
400 `ledger.not_a_year` as problem+json, the `effects.rs` precedent; and the
`/v1` compatibility guard on the removal of `PUT /v1/ledger/books`, which is
the decided removal and **stays red until `just baseline` accepts it** — the
product owner's action, not mine.

**Guards, all falsified** (revert → the named test fails → restore): the closing
flag dropped by the projection; a closing entry reversible by hand; an
out-of-order close accepted; a stale read model accepted; an off-boundary
segment accepted; posting before every currency is checked. The module test
runs one year through every rule — order, no-ops on retry, the dollar refusal,
the entry's lines and date, reopen order, a second booking's fresh ids, the
later-year hold, the read-model check; the HTTP test drives the routes and the
calendar's new segment on a 4-4-5 → monthly switch.

**Not done here:** cost centers, next by the decided order. FX and the
revaluation that went with it were deferred the next morning (§86).

### 84 · Statements are read by the tenant's fiscal calendar

**Built 2026-09-14**, the first Priority 2 item, from the decisions taken that
evening: the calendar is the tenant's, every figure is a sum at the instant
asked, and a balance sheet that does not balance is refused rather than shown.

**What exists now.** `ledger::fiscal` — a `FiscalCalendar { starts_on, pattern }`
under `ledger.fiscal_calendar` (monthly from 2000-01-01 when never set) with
`Pattern::{Monthly, Quarterly, FourFourFive, FourFiveFour, FiveFourFour,
Yearly}`; periods are *generated*, never stored: month patterns clamp to short
months, week patterns start each year on the start date's weekday nearest its
anniversary and put the 53rd week in the last period, and a year is named by the
calendar year it starts in (`2026-P03`). Five queries in `projections.rs` —
`balances_at`, `profit_and_loss` (with a branch), `balance_sheet` (returning the
lines and, per currency, the trading result split at the fiscal year's start and
the postings' difference), `journal` paged on `(occurred_on, entry_id)` and
`journal_entry`. Its OpenAPI line is `StatementLineView`, because `sales` already
owns `LineView` with another shape and the schema-clash scan said so. Seven routes: `GET`/`PUT /v1/ledger/fiscal-calendar`,
`GET /v1/ledger/periods`, `GET /v1/ledger/balances`, the two statements under
`/v1/ledger/statements/`, and `GET /v1/ledger/entries[/{entry}]`. A period is
resolved on the tenant's own `Calendar`, so `2026-P02` starts at Riyadh
midnight; revenue, liability and equity are presented in their natural sign.

**What was refused on purpose.** `PUT` on the calendar while the books are
closed at all (409 `ledger.calendar_locked`) — stricter than the decided
"from the next open year", because the watermark is still one instant and the
formal close is what will make it per period. The balance sheet at 503
`ledger.sheet_does_not_balance` naming the currency and the difference. A
profit and loss with one end of its range (400 `ledger.not_a_range`), a period
the calendar does not have (400 `ledger.no_such_period`), a pattern this build
does not know (400 `ledger.not_a_pattern`).

**A bug the test found, fixed at its root.** `?limit=3` on the journal was a
400: `erp_web::After` is `#[serde(flatten)]`ed into a route's own query struct,
serde buffers a flattened struct's values as text, and `Option<i64>` does not
read text. Every route that folds `After` in — branches, crm, pos,
notifications, booking's two — had the same 400 on `limit`, untested because no
test had ever passed one; the audit routes, which take `Query<After>` whole,
were fine. `After::limit` now reads either shape
(`wire.rs::limit_however_written`), with a unit test on the flattened case.

**Guards, all falsified.** The 503 (check removed → the HTTP test reads a 200
sheet), the flattened `limit` (deserializer removed → 400 over HTTP and the unit
test fails), the fiscal-year split (the sheet bound `as_at` for the year's start
→ both the module test and the HTTP test see the wrong current and prior
results). The ledger's own test drives four entries across two years and two
branches through every query; the HTTP test does the same through the router
on a 4-4-5 calendar, closes 2025, and slips a posting in behind the projection
to see the sheet refused.

**Not done here, by the decided order:** the formal period close (the closing
entry to `3100`, reopening reversing it, the calendar changeable from the next
open year), FX, cost centers, revaluation.

### 83 · A branch is something a member belongs to

**Built 2026-09-14**, the last Priority 1 item, from four decisions taken the
same day: the record is a list on the membership, a member with one branch need
not name it, reads are bounded as well as writes, and a key is bound like a
person. `X-Branch` was a header the caller wrote: `Allowed` parsed it for shape
and handed it to the capability check as a fact, `ledger::post_entry_in` checked
it named an open branch, and branch-scoped claims, inventory shelves and every
posting trusted it. Nothing recorded which branches a person belonged to, so
nothing could refuse one they did not.

#### The record

`membership_branch` (`0024_membership_branches.sql`) sits beside
`membership_module_role` and is shaped like it: the tenant's own branch
identifier as text, no foreign key, cascading with the membership. No rows is
every branch, which is what every membership had before the table existed.
`ControlPlane::set_member_branches` (`crates/erp-control/src/members.rs`)
replaces the list in one transaction with its `membership.branches_changed`
entry; `revoke_membership` drops it with the per-module roles, so a re-added
member starts unconfined. `live_access` loads it in the same round trip as the
roles — an `array_agg` subquery, not a second join, which would multiply
modules by branches — into `Access::branches`, and `members()` lists it.

**Self-hosting.** The product owner asked that the design carry to a customer
who hosts it themselves. It does: a self-hosted deployment runs its own control
plane beside its tenant, so the list lives where the membership it bounds
lives; and the whole-tenant export (Priority 3) must take the membership tables
with it, which this adds one to.

#### The rule, in one place

`Access::branch_for` (`crates/erp-tenant/src/roles.rs`) is the whole of it,
pure and unit-tested as a table: an unconfined member gets what they asked for,
named or not; a confined member may name one of theirs, gets their one branch
when they name none, and is refused `BranchRefusal::NameOne` when they belong
to several — choosing for them would pick a place they did not mean. Naming a
branch that is not theirs is `NotTheirs`, whatever else they hold.
`TenantDb::branch_for` answers it for a handle, and a handle with nobody behind
it — the public, maintenance — gets what it asked for.

`Allowed::from_request_parts` (`crates/erp-web/src/extract.rs`) asks it for the
header before the capability check reads the branch as a fact, so every write in
the system is judged without forty handlers remembering to. The refusals are
`403 access.wrong_branch`, naming the branch, and `403 access.name_a_branch`,
listing theirs. A key comes through the same extractor with its own membership,
and is bound by it.

#### Reads

`Allowed::branch_scope` applies the same rule to `?branch=`: stock, lots and the
shelf summary (`modules/inventory/src/http.rs`) narrow to a confined member's
branch when they name none and refuse another; the bookable list and the
employee list, whose default is the header's branch, judge a named branch the
same way; and `Allowed::may_span_branches` refuses `?scope=all` on the org
chart to a confined member. Lists that take no branch at all — invoices,
entries — still span branches; bounding them row by row is a wider change and
is listed under growth.

#### The route

`PUT /v1/members/{identity}/branches` (`crates/erp-api/src/members.rs`), the
owner's like every membership change, checks each id is an open branch through
the `branches` module's read model — the control plane holds no domain and
cannot — refusing `400 request.no_such_branch`, and an empty list lifts the
confinement. `GET /v1/members` shows each member's list. The tenant route matrix
names the route as owner-only.

#### Proved

`a_confined_member_acts_in_their_branches_and_nowhere_else` pins the rule as a
table. `a_members_branches_are_loaded_with_the_membership_and_gone_with_it`
(`crates/erp-control/tests/control_plane.rs`) proves the list is loaded,
replaced, lifted, dropped with the membership, refused for a stranger and on
the record. `a_member_confined_to_a_branch_acts_and_reads_there_and_nowhere_else`
(`crates/erp-api/tests/http.rs`) runs a clerk through every door: Malaz
refused, no header is Olaya, shelves and summary Olaya alone where the owner sees
both, the org chart refused company-wide, two branches needing a name, a key
bound through its membership, an unknown branch refused, and the list lifted.

### 82 · Signup is closed, and staff set a company up once it has paid

**Built 2026-09-14**, from the product owner's answers: signup closed unless
the deployment opens it, a staff route after payment, no trial (the public demo
is the trial), and modules staying the owner's to switch on because seats — not
modules — are what a company pays for. Five sub-decisions were put and taken
the same day: billing and superadmin create tenants; a closed form answers
`403 signups.closed`; an owner whose address already has an account proves its
password at the link; the link refuses a surplus password rather than ignoring
it; and the scan exempts handlers that take `Anonymous`.

#### The door

`AppState::signup_open` (`crates/erp-web/src/state.rs`) is `false` unless
`bin/api.rs` reads `SIGNUP=open`; `closed` and unset are the same, and any other
word refuses to start, so a typo cannot open it. `POST /v1/signups` checks it
before it charges a budget or reads a handle. The confirmation route is not
what is closed — it is token-gated, and a staff-created owner arrives through
it. `compose.yaml` opens the development stack; the HTTP fixture opens its
router; and `erp_demo::with_deployment` opens the demo's own in-process
router, which never listens on a socket, so a demo builds wherever it runs.

#### The order

`POST /v1/platform/tenants` (`crates/erp-api/src/platform.rs`), under the new
`PlatformPower::CreateTenants`, files the same `pending_signup` row a
self-signup does through one private `file_request`
(`crates/erp-control/src/signup.rs`), with two differences the row records:
no password on file, and `created_by` naming the staff member. `0023` adds the
column and widens `pending_signup_names_one_owner` to allow a row with neither
identity nor hash when staff filed it. The owner is mailed
`invited_signup_messages` — "your company is ready", not "somebody asked" —
and `POST /v1/signups/{token}` now takes an optional `password`:
`claim_and_build` reads the live row **before** claiming it and decides what
the link needs — required for a staff order, refused for a self-signup, and
for an address that already has an account, proved with `authenticate` the
way an invitation is accepted — so a refusal leaves the link live and only a
failed build unclaims. The request is on the platform record as
`signup.requested` under the staff member's name.

#### The scan

`every_module_route_requires_its_module` (`crates/erp-api/tests/entitlement.rs`)
reads every `modules/*/src/http.rs`, finds each handler under a
`#[utoipa::path]`, and refuses one whose body never calls `require_module`
unless its signature takes `Anonymous` — today exactly the two catalogue routes
a signup form reads before a company exists. It counts over two hundred
handlers, so a moved file cannot pass it vacuously.

#### Proved

`signup_is_closed_unless_the_deployment_opens_it` builds a router without the
switch and watches the form refused and the link route still answer; leaving
the door open fails it. `billing_sets_a_company_up_and_the_owner_chooses_a_password_at_the_link`
runs the order from support's refusal to the owner signed in — no password,
short password, the right one, then an existing account with the wrong and the
right password, and the audit entry under billing's name; changing the
required-password arm fails it. Commenting one `require_module` out of
`purchases` fails the scan. The platform matrix test names the twelfth
operation and billing's second power. Control plane 41/41 (provisioning,
migrations, audit, staff), demo 9/9, clippy clean; `just prepare` regenerated
`.sqlx`, and `just openapi` the document.

### 81 · A suspension drains before it stops

**Built 2026-09-14**, from the product owner's answer to the Road to selling
decisions: drain the issued invoices and report them to ZATCA on suspension.
The 2026-09-11 rule — nothing runs while suspended — stands for everything
else, and the two rules meet in a state between them.

#### Two halves

`TenantStatus::Suspending` sits between `Active` and `Suspended`
(`crates/erp-control/src/model.rs`). `suspend_tenant` moves a tenant there,
with the reason and the instant staff wrote, and every door treats it as
suspended: `is_enterable` is false, members and the public get
`access.tenant_unavailable`, the owner still reads the audit trail, support
still gets in. What differs is the worker. `claim_tenants` and `renew_lease`
answer for `active` and `suspending`, so the tenant keeps being visited; the
visit reads the status it was claimed under (`Visit::suspending`,
`crates/erp-worker/src/worker.rs`) and runs only the jobs that say
`drains_a_suspension()`. After a clean round it asks each of those `drained(db)`,
and when every one answers yes it calls `ControlPlane::finish_suspension`, which
moves `suspending` → `suspended` and records `tenant.suspension_complete` under
the system's name. From there `claim_tenants` skips the tenant and
`renew_lease` says stop, as before. A visit whose drain job failed — ZATCA
down — leaves the tenant `suspending`, shut and retried, until a visit in which
it does not. `reinstate_tenant` works from either half; `finish_suspension`
answers `false` rather than an error for a tenant reinstated mid-drain, since
the worker that asked simply has nothing to finish.

#### What drains, and what "drained" means

`Job` gained two methods (`crates/erp-worker/src/job.rs`), both defaulting to
the answer that keeps every other job out: `drains_a_suspension` is `false` and
`drained` is `true`. `SignZatcaDocuments` and `SubmitToZatca`
(`crates/erp-worker/src/bin/worker.rs`) say `true`, and each answers `drained`
from what its tick would find: `tax_sa::awaiting_signature` and
`tax_sa::awaiting_submission` (`modules/tax_sa/src/submit.rs`) count the
rows, and a tenant with rows but nothing to sign them with or nowhere to send
them — one that never finished onboarding — counts as drained, because holding
its suspension open would hold it for ever. Everything else — saved-card
charges, reminders, projections, the outbox — stops from the moment staff act.

#### The schema

`0022_tenant_suspending.sql` widens `tenant_status_check` to admit
`suspending` and `tenant_suspension_is_complete` to require the reason on both
halves; both drop-and-add, exempted in `migrations/EXEMPTIONS` as widenings the
previous build's writes all satisfy. `just prepare` regenerated `.sqlx` for the
five queries whose text changed and the two new counts.

#### Proved

`a_tenant_being_suspended_runs_only_its_drain_jobs_and_is_suspended_once_drained`
(`crates/erp-worker/tests/modules.rs`) runs a real worker against a suspended
tenant with a kernel job and a fake drain job that reports three documents:
the kernel job never ticks, the drain job empties, the tenant ends
`suspended` with one `tenant.suspension_complete` on the record. Removing the
job skip fails it on the first assertion; removing the flip fails it on the
status. `a_suspended_tenant_is_not_visited_and_its_visit_stops`
(`crates/erp-control/tests/leases.rs`) now proves a suspending tenant is
claimed and kept, and a suspended one is neither; the three control-plane
suspension tests cover both halves, the reinstatement from each, and that the
reason survives the move. `docs/RUNNING.md`, the platform route documents and
the control-plane chapter describe the two halves.

### 80 · An audit entry commits with the change it records

**Built 2026-09-14**, the first of the four larger Priority 1 items after §79.
`ControlPlane::record` ran on the pool, after each caller's own commit, so a
crash between the two left an act that stood — a tenant suspended, a key issued,
a member removed — with no record that it had happened. No test could catch it:
every test runs to completion.

#### The root, not the sites

`record` takes `&mut PgConnection` now (`crates/erp-control/src/lib.rs`), so an
entry written outside the change's transaction stops compiling by accident
rather than by review. Of the 33 callers: five already ran in a transaction and
recorded after `commit` — `invite`, `reset_second_factor_by`, `move_staff`,
`abandon`, `request_signup` — and now record before it; twenty-four ran a bare
statement on the pool and then recorded, and each now opens a transaction around
both. The three status moves go through `moved`, which takes the transaction
the `UPDATE` ran on, records, commits, and only then forgets the entry cache — a
forget before the commit could be refilled by a racing read with the old row.
`dealt_with` (dead letters) takes its transaction the same way. Cache
invalidation moved after every commit for that reason.

**Two acts have no control-plane write of their own** and acquire a bare
connection, saying `audit-only:` beside it with why: support entering a tenant
(`enter_for_support`), where the entry is written before the door opens and a
failure to write it keeps the door shut; and `signup.confirmed`, a summary of a
build that is many transactions across two databases, each with its own entry.

**Where one transaction cannot reach**, the record sits with the last
control-plane write of the act: `issue_key_replacing` creates the identity and
its membership as their own audited acts, then the authenticator, the key and
`api_key.issued` commit together; `reap_demo` and `abandon` drop the tenant's
database first, by design, and the row's deletion and its entry commit together
after.

#### The guard

`an_audit_entry_is_written_on_the_transaction_of_its_change`
(`crates/erp-control/tests/audit.rs`) scans the crate for every `.record(` and
refuses one whose first argument is not `&mut *tx` unless an `audit-only:`
comment sits within eight lines above it. It asserts thirty-plus sites, so a
renamed call cannot pass it vacuously. Proved: breaking one marker fails it,
restoring passes. The control-plane suite (224) and every audit-reading HTTP
test (52) pass unchanged, which is the other half of the proof — nothing about
what is recorded moved, only when.

### 79 · The stop-ship holes that were hours

**Built 2026-09-14**, from the product owner's answers to the Road to selling
decisions the same day, in the order agreed: everything in Priority 1 sized in
hours or a half-day, first. The four larger items — the branch record, the
suspension drain, audit entries inside their transaction, and closed signup —
follow in their own sections. Nothing here changed an event, a projection or a
migration, and no route moved. References name files and functions; nothing
checks a line number in this document.

#### The licence is BSL everywhere

`crates/erp-api/src/routes.rs` declared the API under `AGPL-3.0-or-later` while
`LICENSE` and `Cargo.toml` said Business Source License 1.1. It says `BUSL-1.1`
now, and `just openapi` rewrote `docs/openapi.json` to match.
`docs/openapi.baseline.json` still carries the old name until `just baseline` is
run deliberately, which nothing here does. `crates/erp-rules/Cargo.toml` was the
one crate with no `license` field at all, which `cargo-deny` found first.

#### A claim switches on when *it* is granted

`hr::any_claim_placed` asked whether the tenant had granted *any* claim, and
`may_for` and `actor_holds` read a yes as "this control is on". So a café that
granted `hr:approve_timesheet` to a supervisor found `sales:approve_credit_note`
asked at the till the same afternoon, and a clerk with no employee record could
no longer take a return. It is `hr::claim_placed(conn, claim)` now
(`modules/hr/src/claims.rs`), one claim at a time, and both callers pass the
claim they are about to judge. `granting_one_claim_does_not_arm_another`
(`modules/hr/tests/hr.rs`) grants the credit-note claim and asks about the
timesheet one. The prose that said "some claim" — `docs/RUNNING.md`,
`docs/book/src/api/pos.md`, the doc comments on `may_credit` and
`pay_bill` — says "that claim".

#### A key issued the owner's role is not the owner

Three controls exempt "the owner", and each read the role off the handle:
`sales::Authority::of`, `hr::may_for`, and the second-factor reset route. An
integration key issued `role: owner` therefore walked past the document limit,
the credit-note claim and the payment-approval claim. The fix is one question
in one place: `Access::is_owner` (`crates/erp-tenant/src/roles.rs`) is true for
a person holding the owner's role and false for a machine, `Access::machine`
records which, and `Tenant::from_request_parts` (`crates/erp-web/src/extract.rs`)
— the one door every key comes through — marks the handle with
`TenantDb::acting_as_machine`. The three controls ask `is_owner`. A machine is
on no org chart, so wherever a claim is the way past a control it is refused.
Confirmed by the product owner: no keys have been issued yet.
`a_clerk_over_the_document_limit_is_refused_and_the_worker_is_not`
(`crates/erp-api/tests/http.rs`) now issues an owner-role key and watches the
limit refuse it; `a_key_with_the_owners_role_is_not_the_owner`
(`modules/hr/tests/hr.rs`) does the same to `may_for`.

#### Three resets of one person an hour

`POST /v1/members/{identity}/second-factor-reset` and
`POST /v1/platform/identities/{identity}/second-factor-reset` each end every
session the target holds and mail them, with no bound. `RESETS_PER_TARGET`
(`crates/erp-web/src/rate.rs`) is three an hour, and `charge_for_a_reset`
(`crates/erp-web/src/extract.rs`) charges it **on the target**, so the owner's
three and support's fourth are one budget. Charged after the caller has proved
they may — a stranger's refusal costs the target nothing — and before the control
plane, so the fourth ends no session. Through the shared limiter when Redis is
there, per node when it is not, like every other limit. The fourth answers 429
`request.too_many_requests` with the seconds, and both routes document it.
`a_persons_factor_is_reset_at_most_three_times_an_hour` runs the owner out and
then watches support refused.

#### Behind the proxy, every caller is themselves

`compose.yaml` runs nginx in front of two API replicas and nginx appends the
caller to `X-Forwarded-For`, but nothing set `TRUST_X_FORWARDED_FOR`, so every
anonymous rate limit keyed on the proxy's address: one budget for sign-in, signup,
one-time codes and public booking, shared by the whole internet. The `api`
service sets it now, and only it. `docs/RUNNING.md` documents the variable, why it
is dangerous anywhere but behind a proxy you run, and the three that were
undocumented: `PRIMARY_CLUSTER_CAPACITY`, `FLEET_CONCURRENCY` and the `DEMO_*`
family.

#### The cluster's capacity is a number somebody chose

`bin/migrator`'s `register_primary` defaulted `PRIMARY_CLUSTER_CAPACITY` to a
placeholder of ten thousand — and worse, `bin/demo` re-registered the cluster
afterwards at its own ten thousand through `ON CONFLICT … DO UPDATE`, so even a
deployment that set the variable lost it the first time somebody built a demo.
`erp_control::declared_capacity` (`crates/erp-control/src/placement.rs`) is the
one rule: unset, blank, zero, negative or not a number is refused with a message
naming D13. The migrator and the demo both call it; `erp_demo::bootstrap` takes
the capacity as an argument and a test passes a number. `compose.yaml` and
`just demo` declare 100, which is room for a demo on one box and says so.
`the_capacity_is_required_and_a_count` pins the rule.

#### The API drains on SIGTERM

`bin/api.rs` waited on `tokio::signal::ctrl_c` alone. An orchestrator stops a
pod with SIGTERM, which that future never resolves on, so every deploy killed the
API with requests in flight while the worker beside it drained politely.
`shutdown_signal` moved from `erp-worker` to `erp-control`
(`crates/erp-control/src/shutdown.rs`), which both binaries depend on; the worker
re-exports it and the API's graceful shutdown waits on its token.
`the_api_drains_on_the_shared_shutdown_signal` (`crates/erp-api/tests/shutdown.rs`)
is a source scan — the property is one line, and a test that sends a real
SIGTERM to a spawned API needs a database and a race.

#### A read model newer than the build is refused too

`ControlPlane::read_model_behind` compared with `<`, so during a rolling deploy
a pod still on the old build served tables the migrator had already swapped to
the next release's shape, by rules that no longer described them. The projection
runner always used `!=`; the request path does now.
`a_module_whose_read_model_is_newer_than_the_build_answers_503_too` stamps a
group one version ahead and watches the 503.

#### Two documents say what the code does

`docs/ARCHITECTURE.md` §1.17 said backup before upgrade "is enforced by the
migrator"; the migrator has no backup code, and the sentence now says so and
points at the go-live item. `docs/book/src/deployment.md` said the control plane
can go down without stopping a tenant; every request checks its session against
it, and the page now says that, and that the licence handshake it describes does
not exist yet.

#### The dependency tree is checked on every push

`cargo-deny`, configured in `deny.toml`: advisories, dependency licences against
an allow list of permissive ones, duplicate crates as a warning, and crates.io as
the only source. A `deny` job in `.github/workflows/check.yml` runs it beside
`check`, and `just deny` runs it locally. One advisory is ignored with its id and
reason — `paste`, an unmaintained proc-macro `utoipa-axum` expands with, no
vulnerability and nothing to move to — and one warning stands: `chacha20 0.10.1`
is yanked, reachable through `hickory-resolver`, and `cargo update -p chacha20`
is the operator's call, not this section's.

### 78 · A tenant reads its shelves at a glance, and the lists say what they list

**Built 2026-09-14**, from **D-D**, which the product owner took beside D-C
after looking at what a tenant can see: a summary of the shelves per branch, and
a branch filter and the product's name on the stock and lot lists. Read side
only. Nothing posts, nothing moves, and no event or projection changed.

#### What the summary answers

**`GET /v1/inventory/summary`** (`modules/inventory/src/http.rs:875`) answers
one row per branch with a shelf, or just the one `branch` names:

- **`value`**: what the shelves are carried at, **one amount per currency**.
- **`products`**: how many products have a shelf there.
- **`expiring`** and **`expired`**: open lots going off within the tenant's
  window, and lots past their date that are still on the shelf.
- **`below_zero`**: each shelf below zero, named, with what it **`owes`**.

Around the rows the response gives **`today`**, the day the dates were read
against, and **`expiring_through`**, the last day the window reaches. A count
of lots going off means nothing to a reader who does not know from when.

**`branch` is a query parameter and not the request's `X-Branch`.** The header
says where a request comes from, and a manager at head office asking about Malaz
is not at Malaz. Leaving it out means every branch. The stock and lot lists
already ignored the header, and the three routes now read `branch` the same way.
A branch with no shelf is not in the answer, and an empty tenant gets
`"branches": []` with a 200.

**Readable by every role, like the lists** (`crates/erp-api/tests/http.rs:2312`).
The summary is `GET /v1/inventory/stock` and `GET /v1/inventory/lots` added up.
A viewer can page through both and do the sum, so withholding the total protects
nothing. It would only make one screen harder to build for the person at the
counter.

#### Added up when asked, never kept

**Nothing in it is stored.** §71 argued that a projection may not read while it
applies, which is why a movement row carries a signed change rather than a
running total. A total the projection kept would be one more number it could not
check against anything. `inventory::summary`
(`modules/inventory/src/projections.rs:1289`) sums the rows the projection
already writes, at the moment it is asked:

- **Worth is per currency.** A shelf is kept in its inventory account's currency,
  and a tenant that moves that account to dollars holds the old stock in riyals
  and the new stock in dollars. A single total would add riyals to dollars. The
  test builds that state through the product's own commands: a dollar asset
  account and a dollar holding account, chosen as the posting accounts, then a
  delivery.
- **A product counts once it has a shelf, whatever is on it.** A product that
  sold out is still one the branch stocks, and a shelf below zero is counted too,
  and also listed. A count of shelves with something on them would drop a product
  the moment it sold out, and would disagree with the stock list it summarises.
- **What a shelf owes is its debt, not its value.** It is the sum of the shelf's
  movement rows with no lot, with the sign flipped. A shortfall takes units off
  with no lot, and whatever a return or a count settles puts units back with
  none. That sum is `Stock::owed`, rebuilt from rows. It is not the shelf's
  value, because a delivery does not pay the debt (decision 16, §74). Take two
  bags at 5.00 each, five sold, then one more bag at 7.00: the shelf holds −2,
  is worth −8.00, and owes 15.00 — which is what a count of the shelf settles,
  at what the sale charged the three bags out at.
- **One snapshot.** The three reads run in one `REPEATABLE READ READ ONLY`
  transaction, so the value, the lot counts and the shelves below zero come from
  one moment of the read model and not three. `TenantDb::read` hands out a plain
  pooled connection, and a replica serves the transaction as well as the primary
  does.

#### Going off: one rule, and the tenant's day

**The route works out today; the read never does.** `stock_summary` reads the
tenant's `ExpiryWindow` and calendar and takes `calendar.day(Utc::now())`
(`http.rs:892`). The read takes `today` and `expiring_through` as arguments, so
nothing below the route reads a clock, and the tests can choose their day.

**The window's far end is worked out in one place.** `warns_until` was a private
function in the worker binary. It is now `ExpiryWindow::warns_until`
(`modules/inventory/src/expiry.rs:81`), and it is the same arithmetic. The
worker's `read_before` and `going_off` call it, and so does the summary route.
Without the move there would be two `today + days`, and the bell and the summary
could disagree about the last day of the window. A lot is **`expired`** when its
date is before today, and **`expiring`** from today through
`expiring_through`: a batch dated today is still good today, which is how
`going_off` reads it. An undated lot is neither. A lot with nothing left on it is
neither, because the read counts only lots with something remaining, as the lot
list does.

#### The lists: a branch and a name

`inventory::stock` and `inventory::lots` take a `branch`, and
`GET /v1/inventory/stock` and `GET /v1/inventory/lots` pass it through. Movements
do not; nobody asked.

**Every stock and lot row carries the product's `name`**, joined at read time
from the module's own `product` table: the same schema and the same projection
group, read by a route rather than by a projection applying an event, so it
crosses no group and L3 has nothing to say about it. When the table has no declaration for
the product, the row shows **`null`** and is still listed. Inside one group that
state is not reachable today: a product is declared before anything moves
(`accepts_movements` asks the log), and the group applies events in log order.
The join does not rely on that, so a name can never become a 500 or a missing
row. `inventory::lot`, the single read `messaging` uses, names the product too,
because `LotRow` gained the field.

#### Costs

**SQL**: `stock`, `lots` and `lot` gained the join, and `stock` and `lots` gained
the filter; `summary` is three new queries. `just prepare` regenerated `.sqlx`.
It also removed `query-380776…`, the `configuration::get` from before §77 added
`set_at`, which no code has used since. **No install script changed, and no
projection writes anything differently**, so `proj_inventory` stays at version 4
and the `READ_MODELS` pin stands. A version bump is for what a rebuild would write
differently, and nothing here is written.

**One route**, `stock_summary`, in `PERMISSIONS` for every role. The count of
role-scoped operations went from two hundred and fifty-nine to two hundred and
sixty, and `just openapi` regenerated the document. The route adds optional
`branch` parameters and new response fields. No baseline was taken.

**Signatures**: `inventory::stock` and `inventory::lots` take `branch`, and the
worker's two calls and the inventory tests pass `None`. `LotRow` and `StockRow`
gained `name`, so the worker's test literal says `None`. There is no new refusal
and no new message: a branch with no shelves is an empty answer, not an error.

**Tests.** In `inventory`, against a tenant:
`a_summary_is_the_lists_it_summarises_added_up`
(`modules/inventory/tests/inventory.rs:2574`) checks that an empty tenant gets an
empty summary. It then builds six shelves at two branches and at none, in two
currencies, and checks each branch's worth, currency by currency, and its product
count against the stock list, and its lot counts against the lot list read
before today and through the window. It also checks the numbers themselves, so
the agreement is not nothing agreeing with nothing, then the beans shelf below
zero owing 15.00 while it is worth −8.00, the sugar sold to exactly nothing
counted as stocked and absent from `below_zero`, the branch filter on all three
reads, and the name on every row.
`a_lot_is_going_off_from_today_through_the_windows_last_day` (`:2855`) sets
batches dated yesterday, today, tomorrow, the window's last day and the day after,
plus an undated batch and one written off to nothing, and reads them under a
thirty-day window, a window of none and the widest window. In `erp-api`,
`stock_is_summarised_a_branch_at_a_time` (`crates/erp-api/tests/http.rs:15535`)
starts from an empty summary. **It sets the tenant's calendar to whichever of
UTC−12 and UTC+14 is on a different day from UTC when the test runs, and will
stay on that day for the next hour** (one of them always is) and stocks one
branch with a batch dated the tenant's today and the other with one dated the
day before. Read by UTC's day, one of the two batches lands in the wrong column,
whichever way the zone differs. **It sets the window to two days through its
route**, and Olaya also holds batches dated the window's last day and the day
after, so the default thirty days would count one more. Beans sold three short at
Olaya put an entry in `below_zero`. The test then checks `today`,
`expiring_through`, the `below_zero` entry field by field, `branch` on the
summary, the stock and the lots, and the name on both lists.

**Falsified.** Each fix broken in Rust, the guard run and watched to fail on an
assertion, the file restored from a copy (sha256-identical, then touched) and the
guard run again and watched to pass:

| broke | failed |
|---|---|
| the summary adds a branch's currencies into one amount | `a_summary_is_the_lists_it_summarises_added_up`: *Olaya worth what its shelves are, a currency at a time* — *left* `[("SAR", 34200)]`, *right* `[("SAR", 4200), ("USD", 30000)]` |
| the summary skips the shelves of no branch | same: *a row per branch with a shelf, the shelves of no branch first* — *left* `[Malaz, Olaya]` |
| `summary` ignores the branch it is given | same: asked for Malaz, *left* all three rows |
| `stock` ignores the branch it is given | same: *left* 5 shelves at Olaya, *right* 3 |
| `lots` ignores the branch it is given | same: *left* five lots at Malaz, *right* the one received there |
| a stock row carries no name | same: *left* `None`, *right* `Some("حليب طازج")` |
| a lot row carries no name | same, at the lot at Malaz |
| `ExpiryWindow::warns_until` one day short | `a_lot_is_going_off_from_today_through_the_windows_last_day`: *today, tomorrow and the 10th of May going off* — *left* `(2, 1)`, *right* `(3, 1)`; and the worker's `a_lot_warns_inside_the_window_and_not_outside_it`: *left* `Some(2026-05-09)` |
| a lot dated today counted as gone | `a_lot_is_going_off_from_today_through_the_windows_last_day`: *left* `(2, 2)`, *right* `(3, 1)` |
| the route reads today off UTC's calendar | `stock_is_summarised_a_branch_at_a_time`: *the tenant's day in Etc/GMT+12, not UTC's 2026-09-14* — *left* `"2026-09-14"`, *right* `"2026-09-13"` |
| the stock route drops `branch` | `stock_is_summarised_a_branch_at_a_time`: `?branch=` Malaz — *left* 2 shelves, *right* 1 |
| the lots route drops `branch` | same: `?branch=` Olaya — *left* 2 lots, *right* 1 |
| the summary route drops `branch` | same: `?branch=` Malaz — *left* both rows, *right* Malaz's |
| the stock view drops the name | same: *left* `null`, *right* `"حليب طازج"` |
| the lot view drops the name | same, on the lot list |
| the summary route asks `ManageTenant` rather than `Read` | `every_role_against_every_endpoint`: *accountant → GET /v1/inventory/summary (stock_summary) answered 403 Forbidden, and the table says allowed* |
| what a shelf owes read off its value (`-s.value`), built against the type-check database | `a_summary_is_the_lists_it_summarises_added_up`: *three bags no lot covered, at the 5.00 the last delivery cost* — *left* `owes: 800`, *right* `owes: 1500` |
| an emptied lot counted (`remaining > 0` dropped), the same way | `a_lot_is_going_off_from_today_through_the_windows_last_day`: *left* `(3, 2)`, *right* `(3, 1)` |
| *(review)* the summary route reads `ExpiryWindow::DEFAULT`, not the tenant's window | `stock_is_summarised_a_branch_at_a_time`: *the tenant's two days, not the default thirty* — *left* `"2026-10-13"`, *right* `"2026-09-15"` |
| *(review)* a shelf at zero read as below zero (`on_hand <= 0`), against the type-check database | `a_summary_is_the_lists_it_summarises_added_up`: *…the empty sugar shelf owes nothing* — *left* the sugar at 0 owing 0 and the beans, *right* the beans |
| *(review)* the product count skips empty shelves (`AND on_hand <> 0`), the same way | same: *Olaya: a product for every shelf the list shows* — *left* 3, *right* 4 |
| *(review)* `owes` on the wire read off `on_hand` | `stock_is_summarised_a_branch_at_a_time`: *three bags owed at 5.00* — *left* `"minor": -3`, *right* `"minor": 1500` |

**Prose the change made false, and fixed.** `expiry.rs`'s *What reads it* named
only the worker, and the description of `GET /v1/inventory/expiry-window` did
too. The book's read-model signatures had no branch, and its route table had no
summary. §76 and §77 named `warns_until` as the worker's own function.

#### What review found

**Four findings, all in the tests and none in the code.** Each was a way the
code could go wrong while every guard still passed. They share one root: the
tests gave the route inputs under which the thing a guard should pin could not
show. No batch sat at a window's edge, no shelf was sold to exactly nothing,
nothing was below zero over HTTP, and a day was captured once and read twice.

- **The summary never used the tenant's own window** (medium). The HTTP test set
  no window, and its batches were dated today and yesterday, so every window gave
  the same counts. `ExpiryWindow::DEFAULT` in place of `resolve` passed every
  guard. The test now sets two days, dates batches at the window's last day and
  the day after, and checks `expiring_through` and the count.
- **No shelf stood at exactly zero** (low). With `on_hand <= 0`, a sold-out shelf
  would show in `below_zero` owing nothing. A `held` read that skipped empty
  shelves would stop counting a sold-out product, which breaks the rule this
  section states. The inventory test now sells sugar to nothing at Olaya.
- **The wire shape of `below_zero` was not tested** (low). The only HTTP
  assertion was `[]`, so `owes` read off `on_hand`, or left out, would reach a
  client. No stock route sells, so the HTTP test sells beans short through
  `inventory::consume_in`, the call `sales` makes, and checks the entry.
- **The HTTP test could fail on correct code at noon UTC** (low). It took the
  tenant's day from its own clock before seconds of setup, and the route read
  the clock again afterwards. UTC−12 turns its day at 12:00 UTC, a boundary the
  zone choice could straddle. The test now takes a zone whose day will not turn
  within the hour. UTC−12 is on another day than UTC before noon, and UTC+14 is
  from ten; each turns at one of those two hours, so any margin under two hours
  leaves one of them. **This fix could not be falsified in Rust**, because the
  route's clock cannot be moved. Instead, the rule was run against tzdata for
  every minute of two days, and no minute was left without a zone.

**Left open.**

- **A tenant with shelves at a branch and shelves at none cannot filter to the
  shelves at none.** Leaving `branch` out means every branch. This is a business
  that traded without `X-Branch` and then opened branches, and its old shelves
  show under `branch: null` in the summary.
- **`below_zero` is not paged.** Every shelf below zero at a branch comes back in
  one response. A café has a few; a store with thousands of plain products never
  counted would have many.
- **A shelf below zero sums its whole movement history to find its debt**
  (`ponytail:` at the query). Only shelves below zero are read that way, and a
  count clears them. A column the projection keeps would need a version bump,
  and nothing has asked for one yet.
- **Units sold short before anything was received owe nothing.** No cost was
  booked for them. If a delivery later gives the shelf a currency, they read as
  owing 0.00 in it, which is what the books hold and may still surprise a reader.
- **The movement list has neither the filter nor the name.** D-D asked for the
  stock and lot lists only.
- **A run that crosses midnight UTC does not guard the route's day.** In the
  hour before it UTC+14 is chosen, and once UTC turns, its day and the tenant's
  agree, so a route reading UTC's day would pass that one run. Correct code
  still passes.

### 77 · Stock going off rings the bell, and operators watch that it rang

**Built 2026-09-14**, from three decisions the product owner took after §76's
review and a look at what a tenant can see: **D-A**, a plain product's named lot
refuses; **D-B**, the expiry warning reaches the tenant's notification bell, once
per lot, for the people who may write stock off; and **D-C**, which corrects
D-B's *the warning moves*: the operators' per-lot finding is not removed but
**replaced** by a check that the bell works. An earlier run of this slice was
stopped when the product owner questioned removing the health check; it left
nothing in the tree, and nothing of it is here.

#### No module rings the bell, so the worker does

§47 is the rule: announcing resolves an audience through `messaging`, which reads
the domain modules, so a module announcing closes a cycle cargo refuses.
`inventory` is now one of the modules `messaging` reads — a notification about a
lot has to say what the lot is — so the cycle is literal: `inventory →
notifications → messaging → inventory`. The warning is raised where §47 puts
every producer, **a job in `crates/erp-worker/src/bin/worker.rs`**:
`AnnounceExpiringStock` (`:409`), a scan with no cursor that runs on every visit,
like `AnnounceExpiringDocuments` beside it.

It reads the tenant's `ExpiryWindow`, the tenant's calendar and the open lots,
and puts them through `going_off` — the classifier §76 wrote and tested, kept
unchanged along with `warns_until` and `read_before`. (§78 moved `warns_until`,
unchanged, onto `ExpiryWindow`, so the summary counts by it too.) A lot reaching its date
inside the window is **`stock_expiring`**; one past its date and still on the
shelf is **`stock_expired`**. Undated lots are never either, and neither is a lot
with nothing on it, however it emptied — sold, counted or written off — so a lot
that empties after it was told is never told again.

#### Once per lot, by construction

**The notification's id is derived from its kind and its subject**
(`notifications::announce::derived_id`), and here the subject is the lot and the
kind is its state (L8). So a second run finds the aggregate and writes nothing,
and a window widened to reach a lot already told writes nothing either: the
window is not in the id. No cursor, no table of what was said.

**Passing its date earns a second, distinct notification, also once.** The two
ask for different acts. *Going off soon* is an order to rotate, mark down or send
back while the batch can still be sold; *gone and still on the shelf* is stock
that must come off it today — a write-off with reason `expired`. That is the
split `WorkDocumentExpiry` has between an expiring document and a lapsed one, and
§76 kept for the same reason; a person who read the first and did nothing is
exactly who the second is for, and folding it into the first would make it a
repeat nobody reads.

#### Who is told is who may write it off — which no audience can say

The write-off route is `POST /v1/inventory/stock/{product}/write-offs`, whose
`PERMISSIONS` row is owner, accountant and clerk and whose extractor is
`Allowed<PostEntries>` under `/v1/inventory`. **That decision is asked again, per
member**, by `who_may_write_off` (`:533`):

- **The capability is named once**, as the type the handler takes:
  `inventory::http::WritesOff` (`modules/inventory/src/http.rs:707`), with
  `WRITE_OFF` its value. Change the route's extractor and whoever is told
  changes with it.
- **The role that applies in `inventory`** — a module role over the tenant-wide
  one, `Access::allows`.
- **The tenant's permission limits**, with the lot's branch as the request's
  branch and that role as the `role` fact. So that this is the route's decision
  and not a copy of it, the two halves `Allowed` and `TenantDb::permits` ran
  inline are functions now: `erp_tenant::limits::facts_at` builds the facts the
  edge knows (`crates/erp-tenant/src/limits.rs:253`), used by the extractor and
  by `still_permits` (`crates/erp-web/src/extract.rs:808`); `Limits::permit` is
  the whole decision for one person (`:185`), and `TenantDb::permits` calls it
  once it has read the limits (`crates/erp-tenant/src/db.rs:178`). The route's
  answer did not change; it is computed in one place.
- **A suspended login is not told**: it cannot sign in to write anything off.

**This is a departure from §47, and the reason is §47's own.** Every kind before
these resolves an `Audience` through `messaging`: an employee linked to a login,
by where they work and whom they report to. §47 built that link to answer *which
bell rings* and said, in so many words, that nothing about what somebody **may
do** reads it — 9c refused to bridge the planes for authorization. *Who may write
stock off* is exactly what somebody may do: a login's role in a tenant and the
tenant's limits, control-plane, which nothing `notifications` or `messaging`
holds can ask, and which a tenant with no `hr` records could not answer through
an audience at all. So the one place holding both planes — the composition root,
where §47 already puts every producer — resolves the logins and names them.
**`Kind::told_by_caller`** (`modules/notifications/src/kind.rs:113`) says which
kinds are addressed that way — the two stock kinds, exactly the ones with no
audiences, and `every_kind_is_addressed_to_an_audience_its_topic_has` (`:157`)
now asserts one or the other — and `Announcing::to`
(`modules/notifications/src/announce.rs:43`) carries the names, read for those
kinds and no other. An empty `to` is `AnnounceError::Unreachable`, as an
audience that resolves to nobody always was. The control plane is asked only
when a page holds a lot nobody has been told about. What §47 forbids still
holds: no module announces, and the id is still derived.

#### What the notification says

`messaging::Topic::Lot` (`modules/messaging/src/audience.rs:40`) is what the
notification is about, with bindings `lot.id`, `lot.code`, `lot.expires_on`,
`lot.remaining`, `product.name` and `branch.name` (`bindings.rs:210`,
`template.rs:234`). `messaging` depends on `inventory` to answer them —
`inventory::lot` and `inventory::product`, two single-row reads — and declares it
in `reading`, which `a_modules_reads_are_its_crate_dependencies` insists on. An
untracked lot's `lot.code` is its id, which is how the lot list names it.

**Where the lot is, and never a hole.** A notification's wording is rendered once
and frozen in `Announced`, and its id is derived, so a sentence recorded with
`{{ branch.name }}` in it says so for ever. Two lots have no branch name to give:

- **A lot at no branch** — a business with one shelf, `X-Branch` optional — has
  no branch at all. Each stock kind has a second sentence without *at {branch}*,
  and `notifications::copy::of` (`modules/notifications/src/copy.rs:206`) takes
  **the first of a kind's sentences whose every name the subject answers**. A
  kind with one sentence renders as it always did.
- **A lot at a branch the `branches` read model has not caught up with** is named
  by the key it was received under. The branch is in the log — the ledger
  refuses a receipt's posting at a branch that was never opened
  (`branches::accepts_documents`) — so only a read model behind leaves it
  unnamed; the key is how the lot list names a branch, as the id is how it names
  an untracked batch; and a key in the sentence beats braces in it for ever.

The copy (`copy.rs:132`), both languages, neither a translation of the other:

| kind | en | ar |
|---|---|---|
| `stock_expiring` | *{product}, batch {code} at {branch}, is good until {date}.* | *{product}، الدفعة {code} في {branch}، صالحة حتى {date}.* |
| `stock_expired` | *…was good until {date} and is still on the shelf. Write off what cannot be sold or sent back.* | *…كانت صالحة حتى {date} وما زالت على الرف. اشطب من المخزون ما لا يمكن بيعه أو إرجاعه.* |

*Good until*, because a batch dated today is still good today — the reading
`going_off` and `expiring_before` take. A tenant who wants other words writes an
in-system template named after the kind, like every kind.

**The bell and nothing else.** The people named are logins, and a login has no
email address or phone number, so no paid channel can reach them. A grid asking
a stock kind for one would be saved and never obeyed, so `set_preferences`
refuses it (`NotificationError::InSystemOnly`, `notifications.in_system_only`,
en + ar, 400 at `PUT /v1/notifications/preferences`).

A topic is not only a notification's: `lot` is now a topic a `messaging` template
and a `conversations` thread may name, addressed to the branch manager or an
operator. The descriptions that listed four topics list five.

#### The check operators see: did the bell ring?

**`StockExpiry` is gone.** Operators were being shown every tenant's expiring
lots, which none of them can do anything about. **`StockBellRings`** (`:620`,
registered as `stock_bell` at `:140`) reports instead **a lot the announcer should
have told somebody about, with no notification of the kind its state calls for,
for longer than the grace**. That is a fault somebody can fix: the job not
running, the bell's read model behind, a limit or a suspension leaving nobody who
may write stock off where the lot is, or a notification that cannot be recorded.
**It names the lots by id and nothing else** (`describe_lots`, `:694`): what is on
a tenant's shelf, how much and until when, is the tenant's. It reads two groups —
the lots out of `proj_inventory`, what was announced out of `proj_notifications`
— in the composition root for the reason `StockValueAgrees` does, and the rule is
a pure function, **`unannounced`** (`:655`):

- A lot is due `stock_expired` from the start of the tenant's day after its
  date, and `stock_expiring` from the start of the day its window first reaches
  its date (`Calendar::start_of`, never by hand).
- **Or from when it came onto the shelf, if later** — a delivery that lands
  already inside its window was due from then. `LotRow` gained `recorded_at` for
  this: `received_at` is the caller's, and a delivery entered today but dated
  last week would have been due, by that, before anything could have told
  anybody. **A return that puts units back on a lot that had emptied moves it**
  (`put_back`, `modules/inventory/src/projections.rs:662`): that lot was off the
  listing and nobody could have been told about it either.
- **Or, going off, from when the window was last set, if later.** A window
  widened this morning reaches lots nothing has had a visit to announce yet.
  `erp_eventlog::configuration::Configured` gained `set_at`, which the
  `configuration` table has always kept (`crates/erp-eventlog/src/config.rs:76`).
  Passing a date does not depend on the window, so it does not move that.

**The grace is argued from how often the announcer runs** (`bell_grace`,
`:639`). It runs on every visit, so the ordinary wait between a lot falling due
and being told is the wait for the next visit — at the longest, for a tenant
doing nothing else, the schedule's ceiling plus the jitter spread across it. That
bound had no name; it is `WorkSchedule::longest_idle_delay` now
(`crates/erp-control/src/leases.rs:120`), computed from the same arithmetic
`next_idle_delay` uses, and with the shipped schedule it is six hours of ceiling
and two of jitter, less a millisecond. **One health interval on top**: the
notification reaches the bell's read model the round after it is written and the
check only looks every five minutes, so a finding means a whole visit came and
went without it. Derived from the schedule the worker runs, not a constant beside
it: raise the ceiling and the grace follows.

**Silent without a bell.** A tenant with `inventory` and without
`notifications` has no path to be broken, and the check says nothing — see
*Left open*, where it is a question for the product owner.

The value-on-hand invariant, `StockValueAgrees`, is untouched.

#### D-A, recorded

The open question in §76 is closed: **a named lot on a plain product refuses**
when it cannot cover the line, as a tracked one does. It is the one way a plain
sale is refused for stock, and deliberate — the line asked for that lot. No code
changed. `a_named_lot_that_cannot_cover_the_line_refuses`
(`modules/sales/tests/sales.rs:6657`) already pinned the plain case beside the
tracked one; its doc now says it guards a product-owner decision, and the book's
*What an invoice takes off the shelf* says the same.

#### Costs

**SQL**: `configuration::get` reads `set_at`; `inventory::lots` reads
`recorded_at`; `inventory::lot` and `inventory::product` are new. `just prepare`
regenerated `.sqlx`. **No install script changed shape**, so the pinned hash
stands, but **`proj_inventory` moved to version 4** and `READ_MODELS` is re-pinned
at it: a return reopening a lot now writes `recorded_at`, which a rebuild under
the old projection would not. The module has not shipped, so no tenant rebuilds.
**No route**, so `PERMISSIONS` is untouched. New notification kinds and a new
topic are new values in string fields, and `notifications.in_system_only` is a
new 400 for those new values only, which the compatibility test does not pin;
`just openapi` regenerated the descriptions and no baseline was taken.
`Announcing` gained a field and `announce_all` a parameter, so every caller —
four worker jobs, the demo seed, the notifications tests and one `erp-api` test —
passes `&[]` or `Vec::new()`. `erp-worker` depends on `erp-tenant` directly, for
`Limits` and `facts_at`.

**Tests.** Pure, in the worker binary:
`whoever_may_write_stock_off_is_told_and_nobody_else`,
`a_lot_due_a_notification_is_a_finding_until_its_own_one_exists`,
`a_lot_is_silent_inside_the_grace_from_whenever_it_fell_due`; §76's
`a_lot_warns_inside_the_window_and_not_outside_it` and
`an_undated_or_emptied_lot_never_warns` stand, and its cap test went with the
cap. **Against a tenant**, in the same binary — the harness §72 and §76 found
missing: `a_lot_going_off_is_told_once_to_whoever_may_write_it_off` (owner and
clerk told, viewer not; once; English and Arabic naming the product, batch,
branch and date; a lot outside the window, an undated lot and one written off to
nothing never told; ninety days reaching the next lot without telling the first
again; the check silent),
`a_lot_at_no_branch_or_an_unknown_one_is_told_in_a_whole_sentence` and
`a_lot_past_its_date_nobody_was_told_about_is_a_finding` (a delivery entered
now, two days past its date and dated three days back: silent inside the grace
because it counts from when it was recorded, a finding past it naming the lot's
id and not its batch, `stock_expired` once told, silent after). In
`erp-control`: `no_tenant_waits_longer_than_the_longest_idle_delay`. In
`notifications`: `a_kind_told_to_logins_takes_no_channel_but_in_system`. In
`inventory`: `a_return_after_a_count_cleared_the_debt_puts_the_units_back` now
also pins when a reopened lot and an open one came onto the shelf.

**Falsified.** Each fix broken in Rust, the guard run and watched to fail, the
file restored from a copy (sha256-identical, then touched) and the guard run
again and watched to pass:

| broke | failed |
|---|---|
| `who_may_write_off` stops skipping suspended logins | `whoever_may_write_stock_off_is_told_and_nobody_else`: *a suspended owner* among those told |
| the role asked with no module instead of `inventory` | same: *a clerk who only views stock* told, *a viewer who is a clerk for stock* not |
| the lot's branch left out of the facts | same, at Malaz: *left* every clerk, *right* `["owner", "accountant"]` — the limit never saw the branch |
| `WritesOff` is `ManageAccounts` | same: *left* `["owner", "accountant"]` — the route's extractor changed and who is told followed |
| `Limits::permit` stops adding the `role` fact | same, at Malaz: the limit on clerks never matched |
| `facts_at` passes the branch as an empty string | same, at Malaz |
| `unannounced` treats a lot already told as untold | `a_lot_due_a_notification_is_a_finding_until_its_own_one_exists`: a finding though the bell rang |
| a lot past its date looked up as `stock_expiring` | same assertion |
| no grace (`now >= due`) | `a_lot_is_silent_inside_the_grace_from_whenever_it_fell_due`: *the ordinary wait for a visit* |
| the window's `set_at` ignored | same: a lot a window reached an hour ago is a finding |
| due counted from `received_at` instead of `recorded_at` | same, at the delivery an hour old; and `a_lot_past_its_date_nobody_was_told_about_is_a_finding`: *recorded a moment ago, however long ago it says it arrived, so still inside the grace* |
| the grace from the ceiling alone | same: *left* 21900s, *right* 29099.999s — *six hours of ceiling, two of jitter, five minutes of health interval* |
| `longest_idle_delay` without the jitter | `no_tenant_waits_longer_than_the_longest_idle_delay`: *left* 21600s, *right* 28799.999s |
| the job hands the bell nobody (`&to[..0]`) | `a_lot_going_off_is_told_once_to_whoever_may_write_it_off`: the first tick *left* `Idle`, *right* `Worked` |
| `announce` reads `to` only when it is empty | same |
| the window in the notification's subject | same: *left* `lot.….dn-soon.w30`, *right* `lot.….dn-soon` |
| the window in the subject once it is not thirty days, and the cheap first pass skipped then | same, at ninety days: *left* 3, *right* 2 — `dn-soon` told a second time |
| the check reads a lot as told `stock_expired` only when nothing was, and it was told `stock_expiring` | `a_lot_past_its_date_nobody_was_told_about_is_a_finding`: *told, and still a finding* |
| `product.name` not bound | `a_lot_going_off_is_told_once_to_whoever_may_write_it_off`: *does not say* the product |
| the Arabic sentence at a branch drops the branch | same: *ar does not say* العليا |
| `told_by_caller` forgets `stock_expired` | `every_kind_is_addressed_to_an_audience_its_topic_has`: *stock_expired must name an audience or be told to whom the caller names* |
| a plain line's short named lot falls back to the picking rule (D-A) | `a_named_lot_that_cannot_cover_the_line_refuses`: *two in a plain lot and three asked for of that lot* |

**Prose the change made false, and fixed.** `inventory`'s module doc and
`expiry.rs` said the worker's `stock_expiry` check reports; the expiry-window
route's description said lots are *reported as health findings*; the book's
*The expiry warning* described two health findings; §71's closed entry and §76's
*The expiry warning* described `StockExpiry` as current. The notifications
crate's layering diagram and its `Cargo.toml` listed `messaging`'s reads without
`inventory`. The notification view's `kind` and `docs/RUNNING.md` listed five kinds, and
every description of a topic listed four.

#### What review found — a harness that never named a branch, and a hole frozen into the bell

Seven findings: six fixed, one left to the product owner.

- **The main guard had never passed.** `a_lot_going_off_is_told_once_to_whoever_may_write_it_off`
  failed on its first wording assertion — *en does not say العليا: حليب طازج,
  batch B-SOON at {{ branch.name }}* — because the harness projected `inventory`
  and `notifications` and never `branches`. Everything after that line had not
  run: the Arabic, the clerk, the viewer, the ninety-day window, the silent
  check. The harness projects `branches` now, and the test is green for the
  first time; every falsification of it above was run after the fix.
- **That failure was a production bug, not only a harness one.** A lot at no
  branch took the business's signing name as its `branch.name`, and that
  defaults to empty and nothing outside `messaging` sets it; a lot at a branch
  the read model lagged on had none either. Either way the bell recorded
  `at {{ branch.name }}`, frozen, under a derived id no later run replaces. The
  business fallback is gone; the stock kinds have a sentence without a branch,
  chosen when the subject answers no branch; a lagging branch is named by its
  key — see *Where the lot is, and never a hole*. Guard:
  `a_lot_at_no_branch_or_an_unknown_one_is_told_in_a_whole_sentence`, with no
  business name set and `branches` left unprojected.
- **This section did not exist**, while §76, the D-A guard's doc and the check's
  rustdoc cited it, and the departure from §47 was argued nowhere. It is written,
  with that argument under *Who is told is who may write it off*.
- **The book said operators do not see a tenant's lots**, and the finding logged
  five lots' quantity, product, branch, batch and date at `error`. The finding
  names ids only now (`describe_lots`), which is what D-C's objection was about;
  the book says what an operator does see.
- **`PUT /v1/notifications/preferences` saved SMS for a stock kind** and answered
  204, though the people those kinds reach have no address (L6). Refused now,
  in the command rather than at the route, so every writer meets it.
- **A lot reopened by a return kept its receipt's `recorded_at`**, so a lot that
  emptied before its window, came back after it and had never been told was due
  from long ago, and the health job — registered ahead of the announcer — could
  report it before the announcer's turn in the same visit. `put_back` moves
  `recorded_at` when the lot had emptied; `proj_inventory` is at version 4.
- **A tenant with `inventory` and without `notifications` is told by nobody**,
  and the check is silent for it: the per-lot finding was the only thing that
  spoke for such a tenant, and D-C replaced it with a check of a bell that tenant
  does not have. **Decided 2026-09-14 (D-E): the summary is enough** — see
  *Left open*.

**And one the verification run found.** `erp-web`'s
`every_declared_fact_is_assembled_somewhere` looked for `limits::BRANCH` and
`limits::ROLE` at the code that supplies them, and with that code moved into
`facts_at` and `Limits::permit` it found neither. The facts are still supplied;
the scan was looking in the old place. It also reads `limits.rs`'s own code now
(`crates/erp-web/tests/facts.rs`), above its tests, which build facts of their
own.

**Falsified**, the same way:

| broke | failed |
|---|---|
| `copy::of` always takes a kind's first sentence | `a_lot_at_no_branch_or_an_unknown_one_is_told_in_a_whole_sentence`: *leaves a hole* — `{{ branch.name }}` for the lot at no branch |
| a branch the read model lags on left unnamed rather than named by its key | same: *does not say BR-OLAYA* — the sentence without a branch, for a lot at one |
| the finding names batch codes again | `a_lot_past_its_date_nobody_was_told_about_is_a_finding`: *…or the bell cannot record one: B-GONE* |
| `set_preferences` refuses a stock kind's grid only when it is empty | `a_kind_told_to_logins_takes_no_channel_but_in_system`: *SMS for stock going off was saved, and could never be sent* |
| `put_back` keeps a reopened lot's `recorded_at` | `a_return_after_a_count_cleared_the_debt_puts_the_units_back`: *the reopened batch still says it came onto the shelf with its receipt* |
| `put_back` moves it for a lot still open too | same: *a unit joining a lot still open moved when that lot came onto the shelf* — *left* `…21.109778Z`, *right* `…21.051383Z` |
| `facts_at` puts the branch in as `capability` | `every_declared_fact_is_assembled_somewhere`: *the registry declares `branch` and nothing anywhere supplies it* |
| `Limits::permit` stops adding the `role` fact | same: *the registry declares `role` and nothing anywhere supplies it* |

The harness fix is not in the table: its guard is the test it repaired, and every
row above that names `a_lot_going_off_is_told_once_to_whoever_may_write_it_off`
was run after it.

**Left open.**

- ~~**A tenant with `inventory` and without `notifications` is told by nobody.**~~
  **Decided 2026-09-14 (D-E): the summary is enough, and the docs say so.** Such
  a tenant is not pushed and `stock_bell` stays silent for it, but it sees every
  expiring and expired lot in `GET /v1/inventory/summary`, counted against the
  same window. `inventory` does not require `notifications`: nothing was built,
  and the module doc (`modules/inventory/src/lib.rs`), the expiry-window
  setting's description and the book page now state it.
- **Enabling `notifications` on a tenant whose lots are already due** can raise
  one `stock_bell` finding if the health job reaches the tenant before the
  announcer does on that visit: when a module was enabled is not something a
  `TenantDb` knows. It clears on the same visit.
- **A tenant's own in-system template for a stock kind can still leave braces.**
  The first-answered rule is for the compiled copy; a template is checked against
  the vocabulary when it is saved, and `branch.name` is in the vocabulary,
  though a lot at no branch has none. Refusing to announce until a template's
  every name is answered would hold a notification for ever over an optional
  name.
- **Every open lot inside the window is read on every visit**, a page at a
  time, by the announcer and by the check. Cheap for a café; a pharmacy with
  thousands of dated lots and a six-month window may want a watermark.
- **A lot whose id is not a valid `AggregateId`** — longer than 128 characters,
  which a long product key and branch key together can make — cannot name a
  notification. The announcer logs it and the check reports it; neither drops it
  quietly. Capping what `lot_of` can produce is `inventory`'s, and nothing has
  produced one.
- **A member who has not enrolled a second factor the tenant requires** is told,
  though they cannot sign in until they do. They are one enrolment from writing
  the stock off, which is closer to *may* than to *may not*.

### 76 · A line names its lot, a return names its units, and a lot going off is somebody's to act on

**Built 2026-09-13**, after Phase 19's four boxes, from what they left open:
§74's *a line that names a lot* and *naming which units come back*, §71's
*nothing reads the expiry window* and *no book page*. Decision 9 had all of it —
picking is earliest expiry first **and overridable**, a serial is an identity,
expiry is **warned early and written off by hand** — and only the seams existed.

#### A line may name its lot

`pick` already honoured a named lot (`modules/inventory/src/picking.rs:290`):
refused when it is not open on the shelf, refused when it holds fewer than was
asked, never topped up from the next. Nothing on an invoice could say which.
**`lot` is now on the draft line and on the stored line**
(`modules/sales/src/invoice.rs:175`, `:280`), and `deplete` hands it to
`consume_in` (`modules/sales/src/commands.rs:677`). On the stored line and not
only the draft, because since §74's review `deplete` reads the lines off the
`Issued` event the transaction wrote, never off the request; `#[serde(default)]`,
so every line already written decodes as one that let the picking rule choose,
which is what it did. It rides `issue_in`, so the till has it too — both wire
lines carry it (`modules/sales/src/http.rs:211`, `modules/pos/src/http.rs:110`).

- **The lot's id, not its code.** Codes repeat across deliveries (decision 19);
  the id is what `GET /v1/inventory/lots` lists and what a write-off already
  takes.
- **A lot at another branch is refused by construction.** A shelf is one
  aggregate per product per branch and only its own lots are in it, so Olaya's
  batch named on a sale at head office is `inventory.no_such_lot` — even when
  head office holds milk the line could have had. No new check; a test that says
  so.
- **A lot named with no product is refused** (`commands.rs:1782`,
  `SalesError::LotWithoutAProduct`, `sales.lot_without_a_product`, new, en +
  ar), in `priced_lines` beside the rule a line naming serials follows. Dropped,
  nothing would take the units off that batch while the customer was promised
  it (L6).
- **A plain product that names a lot refuses when the lot is short**, like a
  tracked one. Naming a lot is a claim about that lot whatever the tracking, and
  `consume_in` already said so; only R1's shortfall is a plain product's, and it
  is for the lines that name nothing. *That is the lead's reading of decision 9
  against R1, not the product owner's — see What review found and Left open.*

The invoice's read model stores its lines as columns and has none for `lot`, so
the batch a line named is on the event and on the stock movement
(`proj_inventory.stock_movement.lot`) and not in `GET /v1/sales/invoices/{id}`.
No SQL changed, and no `VERSION` moved. See *Left open*.

#### A return names the units that came back

A return of part of a named-unit movement was refused, because which of three
phones came back is not a thing to guess (decision 17), and a credit line had
nowhere to say. **`CreditLine` gained `serials`** (`commands.rs:1961`), and so did
both wire shapes (`modules/sales/src/http.rs:777`, `modules/pos/src/http.rs:192`)
and `inventory::Restoration` (`modules/inventory/src/commands.rs:363`).

**Only units this invoice sold, and each of them once.** The question is not
whether the name is on the shelf — a unit sold again since is off the shelf as
well, so a shelf check would let the first invoice bring it back a second time
while the second customer is holding it. **It is whether that unit is still out
on that sale**, and §74 already built the thing that answers it:
`inventory::stock::Returning` follows the consumption through the whole stream,
and `WentOut::give_back` takes each returned name off what is still out
(`modules/inventory/src/stock.rs:332`). That line was written for whole returns
and is load-bearing now. `named_back` (`commands.rs:958`) takes each name off the
portion it went out on, at that portion's share of what the sale froze, and
refuses a name that is not there — never taken, or already back — or given twice:
**`InventoryError::NotOut`** (`:145`, `inventory.not_out`, new, en + ar, 422).
Nothing is decided from the shelf's bounded window, which §74's review found
wrong twice. The shelf's own `lands` check still runs after it.

**Names come with their count.** `priced_for_credit` refuses names without a
quantity of that many (`modules/sales/src/commands.rs:2269`): a credit line with
names and no quantity is skipped by `came_back`, which only returns what a
quantity says, so the money would credit and nothing would come back. It reuses
`sales.named_units`, reworded to fit a credit line as well as an invoice line.
`inventory` checks its own input the same way — names disagreeing with a
`Restoration`'s quantity are `NeedsSerials` (`commands.rs:958`) — which is the
split `deplete` already has with `leaving`: each layer refuses the shape it is
handed. Only `sales` calls `restore_in`, so that one is pinned by a unit test on
the pure function rather than through an invoice.

Names on two credit lines against one invoice line add up with their quantities
(`ComingBack`, `commands.rs:726`, now a quantity and the names), and trimmed the
way a sale trimmed them (`restore_in`, `modules/inventory/src/commands.rs:767`),
so a name matches itself. A return of named units by quantity alone still only
works for the whole of what is out, and
`inventory.named_units_come_back_whole` now says to name them.

#### The expiry warning

*Superseded by §77.* The warning is a notification for whoever may write stock
off now (D-B), and `StockExpiry` is gone: what operators see is a check that the
notification was raised (D-C). `going_off`, `warns_until` and `read_before`
survive it unchanged — `warns_until` as `ExpiryWindow::warns_until` since §78. What follows is what this slice built.

**`StockExpiry`** (`crates/erp-worker/src/bin/worker.rs:388`, registered at
`:140`) is a health invariant in the shape `WorkDocumentExpiry` has: it reads the
tenant's `ExpiryWindow`, the tenant's calendar and the open lots, and reports two
findings — `stock_expired` for lots past their date and still on the shelf,
`stock_expiring` for lots that reach their date within the window. Two and not
one, for `hr`'s reason: stock past its date should be off the shelf today, and
stock about to be is an order to rotate.

**It posts nothing, writes nothing off and moves nothing.** A date is not a
smell: a batch may go back to its supplier or sell at a markdown until its last
good day, so what leaves the shelf leaves through a write-off somebody enters
with a reason. The check holds a read connection, reads one setting, the
calendar and one listing, and hands the rows to a pure function.

**The rule is `going_off`** (`:429`), tested without a database, with the
window's last day in `warns_until` (`:399`):

- A lot with **no date never warns** — nothing on it spoils — and an **emptied**
  lot never warns, whatever its date: nothing is left to throw away.
- **Gone** is a date behind today. A batch dated today is still good today, the
  reading `expiring_before` already takes.
- **Soon** is a date no later than today plus the window. A window of none still
  reports what has gone, and what goes today.
- **Today is the tenant's day** (`erp_eventlog::configuration::calendar`), not
  UTC's — at one in the morning on the 12th in Riyadh it is still the 11th in
  UTC, so `Utc::now().date_naive()` would call a batch dated the 11th good for
  three more hours after the shop's own day said it had gone.

The listing is narrowed with `expiring_before` set to the day after
`warns_until` (`read_before`, `:413`) and capped at 200 lots, soonest first
(`EXPIRY_LOTS`, `:393`), reading one more so a finding can say whether the cap
cut the list and where (`expiry_findings`, `:459`). `lots` already excludes
emptied and, under `expiring_before`, undated lots — that narrowing is the
listing's own, and `going_off` is still the rule, so a change to the query
cannot start warning about either. It can stop warning, by reading less, which
is why `read_before` has a test of its own (see *What review found*).

**"Changes nothing" is by construction, not by test.** No command is reachable
from the check, and the classifier takes borrowed rows. Proving it against a
database would need a tenant harness in the worker binary, which §72 noted it
does not have.

#### The book page

`docs/book/src/api/inventory.md` in the shape of the other module pages, listed
in `SUMMARY.md` beside `ledger`, and an `inventory` entry in `modules.md` —
above `sales`, because that page orders modules by what they depend on, and
`sales` and `purchases` depend on this. It describes what is built: the tracking
modes, lots and the picking rule, serials, receipts and `2010`, write-offs,
counts, what an invoice takes and a credit note puts back, the expiry warning,
the posting accounts, and what is not here. `module.md` still said the registry
carried four modules, that three tests guarded it, and that `docs/ERRORS.md` is
generated from the catalogue; it now shows one entry per module, says what the
registry's tests guard, and names `erp_i18n::testing::assert_complete`
(`crates/erp-api/src/catalog.rs:69`), which is what fails a code missing a
language since `ERRORS.md` was retired.

#### Costs

No SQL changed and no `.sqlx` file moved: the sales read model has no column for
a line's lot, and `proj_inventory` already stored a movement's lot and a unit's
name. **No `VERSION` bump**, no `READ_MODELS` re-pin, no route, so `PERMISSIONS`
is untouched. An `Issued` event's line may carry `lot`, and a `Restored` event's
portions carried serials already. Four optional request fields —
`NewInvoiceLine.lot`, the till's `NewLine.lot`, `NewCreditLine.serials`,
`ReturnedLine.serials` — so the compatibility test passed untouched and no
baseline was taken; `just openapi` regenerated them and the route descriptions.
The credit-note and till-return handlers built their lines inline; each is now a
function with a test at the seam (`credit_lines`, `modules/sales/src/http.rs:1676`;
`returned`, `modules/pos/src/http.rs:837`), because §74 found a field dropped
between JSON and a command is invisible to every test that calls the command.
Every `DraftLine`, `InvoiceLine` and `CreditLine` literal in the workspace gained
the new field.

**Falsified.** Each fix broken in Rust, the guard run and watched to fail, the
file restored from a copy (sha256-identical) and the guard run again and watched
to pass:

| broke | failed |
|---|---|
| `deplete` passes `lot: None` | `an_invoice_naming_a_lot_takes_from_that_lot`: *two came off the batch the line named — left `Some(3)`, right `Some(1)`*; and `a_named_lot_that_cannot_cover_the_line_refuses`: *three in the batch and four asked for: `Ok(…)`* — the invoice issued off the early batch |
| the lot-without-product check removed from `priced_lines` | `a_named_lot_that_cannot_cover_the_line_refuses`: *a lot of nothing: `Ok(…)`* — issued with `product: None, lot: Some(…)` |
| `from_one` falls back to picking when the named lot is not on the shelf | `a_lot_from_another_branch_refuses`: *Olaya's batch is not on head office's shelf: `Ok(…)`* |
| `named_back` ignores a name no portion holds | `a_serial_return_names_only_what_the_invoice_sold_and_only_once`: *SN-3 went out on another invoice: `Ok(Numbered { … "CN-00001" })`* — the money credited, nothing came back |
| `give_back` stops taking returned names off what is still out | same test: *SN-2 has already come back on that invoice: `Ok(Numbered { … "CN-00002" })`* — sold again, it came back a second time |
| `came_back` drops the names on a credit line | same test: *`NamedUnitsComeBackWhole { taken: 2 }`* where `NotOut("SN-3")` was expected |
| `priced_for_credit` stops requiring a quantity beside names | same test: *a name with no quantity would have credited the money and put nothing back: `Ok(…)`* |
| `named_back` stops checking names against the quantity | `a_name_that_is_not_out_is_refused`: *three coming back and one named* |
| `named_back` stops refusing a name given twice | same test: *one phone named twice is one phone* |
| `going_off` stops skipping emptied lots | `an_undated_or_emptied_lot_never_warns`: *`["emptied-and-gone"]`* |
| `going_off` counts an undated lot as soon | same test: *`["undated"]`* |
| the window's last day falls outside (`<` for `<=`) | `a_lot_warns_inside_the_window_and_not_outside_it`: *inside the window — left `["today"]`, right `["today", "last-day"]`* |
| a lot dated today counts as gone (`<=` for `<`) | same test: *past its date and on the shelf — left `["yesterday", "today"]`, right `["yesterday"]`* |
| the sales route drops a credit line's `serials` | `a_credit_line_carries_its_units_to_the_command`: *left [], right ["SN-2"]* |
| the sales route drops a line's `lot` | `a_line_carries_its_product_and_its_units_to_the_draft`: *left None, right Some("lot.x")* |
| the till drops a line's `lot` | `a_till_line_carries_its_product_and_its_units_into_the_draft`: *left None, right Some("lot.x")* |
| the till drops a return's `serials` | `a_till_return_carries_its_units_to_the_credit_line`: *left [], right ["SN-2"]* |

**Prose the change made false, and fixed.** `inventory`'s module doc listed
*naming which units come back* and *the expiry warning itself* as not here, and
said nothing reads the window; `expiry.rs`'s module doc was headed *Nothing reads
this yet*; the expiry-window route's description — in `docs/openapi.json` — said
the check *"is a later slice"*. `restore_in`'s rustdoc and
`NamedUnitsComeBackWhole` said part of a named movement cannot come back at all.
The credit-note routes' and the till's descriptions now name `inventory.not_out`,
the lot refusals and `sales.lot_without_a_product`, and the invoice route no
longer says a plain product never refuses for stock without *"that names no
lot"*. `sales.md` in the book said a stock line names a product and its serials
and nothing about a lot or a credit line's units. §71's and §74's *Left open*
entries are struck through and pointed here.

#### What review found — two doors, a window's far end, and a call nobody signed

Four findings, three fixed and one left to the product owner.

- **The till answered a malformed line 422 while its own description said
  400.** The till's `problem_for` sent every `PosError::Sale` to 422 but a
  malformed *shelf* refusal, so `sales.lot_without_a_product` — added by this
  slice and listed under the till sale's 400 — was a 422 at the till and a 400
  at `/v1/sales`, and so were `sales.named_units` and `sales.not_a_quantity`.
  The till return's descriptions named neither refusal its new `serials` made
  reachable. **One decision now, `SalesError::is_malformed`**
  (`modules/sales/src/commands.rs:314`): a quantity that is not one, names that
  do not match their line, a lot with no product, or whatever
  `InventoryError::is_malformed` says. `sales_problem`
  (`modules/sales/src/http.rs:1723`) and the till's `problem_for`
  (`modules/pos/src/http.rs:916`) both ask it, so neither door can answer the
  same refusal differently; `/v1/sales` answers exactly what it did. Both till
  routes' 400 descriptions name the codes, in `docs/openapi.json` too. Guard:
  `a_malformed_sales_line_is_a_bad_request_at_the_till_too`
  (`modules/pos/src/http.rs:1087`).
- **Half of the window's last day was an expression nothing tested.** The
  horizon was `warns_until(..).and_then(succ_opt)` inline in `check`. The
  reviewer dropped the `succ_opt`: the listing stopped reading the last day's
  lots, so they never warned, and all eleven worker tests passed. The section
  said the boundary was in one place, and it was in two. It is `read_before`
  now (`crates/erp-worker/src/bin/worker.rs:413`), asserted in
  `a_lot_warns_inside_the_window_and_not_outside_it`.
- **The cap's note was wrong both ways.** `Page::of` calls a page that fills
  its limit one that *may* have more (`crates/erp-types/src/page.rs:116`), so a
  tenant with exactly 200 dated lots was told the list was cut. And since every
  lot already gone sorts before every lot going soon, 200 gone lots filled the
  page and `stock_expiring` did not appear at all, which reads as nothing
  expiring. The check reads 201 now, and `expiry_findings` (`:459`) looks at
  the one past the cap: none, and nothing says it was cut; one going soon, and
  the expiring finding says *and more*; one already gone, or the cap falling
  exactly between the gone and the soon, and the expired finding says nothing
  expiring within the window was looked at. Guard:
  `the_expiry_cap_says_it_cut_only_when_it_did_and_where`.
- **A plain product's named lot refuses when it is short or closed** —
  plausible, and not changed. R1 lets a plain product go negative; this slice
  made a plain line that names a lot refuse the invoice when that lot cannot
  cover it, and wrote it down as settled. It stays because decision 9 lets a
  line name a lot whatever the tracking, and a named lot is a claim about *that*
  stock: topping it up from the next lot breaks the claim, and recording a
  shortfall on it turns a wrong input into a wrong count (L6, and decision 17's
  reasoning about identities). The other answer is refusing `lot` on a plain
  line outright, which takes back something decision 9 gave. Neither is the
  lead's to pick, so it is under *Left open* and in the book's *What an invoice
  takes off the shelf*, and `a_named_lot_that_cannot_cover_the_line_refuses`
  now pins the plain case beside the tracked one — it had only milk, which is
  lot-tracked — so an answer the other way is a deliberate change.

**Falsified**, each file restored sha256-identical and the guard watched to
pass again:

| broke | failed |
|---|---|
| the till's `problem_for` back to the shelf-only arm | `a_malformed_sales_line_is_a_bad_request_at_the_till_too`: *`sales.lot_without_a_product` — left 422, right 400* |
| `SalesError::is_malformed` forgets `LotWithoutAProduct` | same test, same assertion |
| `read_before` without `succ_opt` | `a_lot_warns_inside_the_window_and_not_outside_it`: *left `Some(2026-05-10)`, right `Some(2026-05-11)`* |
| `expiry_findings` never looks past the cap | `the_expiry_cap_says_it_cut_only_when_it_did_and_where`, at the cut among the soon |
| a full page read as a cut — `Page::of`'s reading | same test: *exactly the limit is not a cut* — *"1 lot expires within 30 days, and more (only the 2 soonest were read)"* |
| the cap falling between gone and soon goes unsaid | same test, at the expired finding |
| a plain line's short named lot falls back to the picking rule | `a_named_lot_that_cannot_cover_the_line_refuses`: *two in a plain lot and three asked for of that lot: `Ok(Committed { … "INV-00001" … })`* — issued, one bag short |

**Left open.**

- ~~**Whether a plain product's named lot may refuse.**~~ **Decided by the
  product owner on 2026-09-14 (D-A), recorded in §77**: it refuses
  (`inventory.lot_is_short`, `inventory.no_such_lot`) as a tracked one does, and
  `a_named_lot_that_cannot_cover_the_line_refuses` says it guards that decision.
- **`/v1/sales` and the till still answer four credit refusals differently.**
  `sales.no_such_line`, `sales.credit_too_large`, `sales.already_credited` and
  `sales.not_a_stock_line` are a 400 at `/v1/sales/invoices/{id}/credit-notes`
  and a 422 at the till's return, under *the sale is not one that can be
  credited*. They refuse on the state of the invoice, not the shape of the
  request, so the till's is arguably right; they predate this slice, and moving
  the sales routes' statuses is a compatibility question of its own.

- **The lot a line named is not served back on the invoice.** It is on the
  `Issued` event and on the stock movement, not in `proj_sales.invoice_line`.
  A column, a response field, a `sales` group `VERSION` bump and a `READ_MODELS`
  re-pin — additive, and deferred because nothing has asked to read it off the
  invoice rather than off the movement.
- ~~**An expiring lot is a health finding until somebody acts.**~~ **Closed by
  §77**: the product owner put the warning on the tenant's bell (D-B) and gave
  operators a check that the bell rang instead (D-C). The classifier did not
  change.
- ~~**No database test for the check.**~~ **Closed by §77**, which builds the
  tenant harness in `erp-worker`'s binary for the announcer and for the check
  that replaced this one.
- **A credit line's names are not stored on the credit note.** Like its
  quantity, they reach the shelf — `Restored` carries them — and not the
  `Credited` event, which records money. A statement of which phone came back
  reads the movement.
- **A write-off or a count still cannot name a unit on another shelf**, and a
  line still cannot take one invoice line from two branches. §74's transfer, not
  this.
- **Other book pages still cite `docs/ERRORS.md`** — `erp-web.md`, `http.md`,
  `erp-i18n.md`, `erp-api.md` — and so does the rustdoc on
  `crates/erp-api/src/modules.rs`'s catalogue field. Only `module.md` was this
  slice's to fix.

### 75 · A count counts the shelf, and settles what the shelf owes

**Built 2026-09-13**, Phase 19's last box, on revision **R2**. Until this slice
a count named one lot (§71): counting a shelf of four deliveries was four
counts, a serial-tracked product could not be counted at all, and the debt a
plain product's sale leaves (§74) sat on no lot, so no count could reach it. A
business that oversold read a negative number nothing in the product could
correct.

**`Count.lot` is optional, and that is the interface change**
(`modules/inventory/src/commands.rs:373`) — the one §73's review predicted. No lot
counts the shelf; a lot counts that batch, which stays available for someone
counting batches. `serials` is new. On the wire `lot` went from required to
optional and `serials` arrived optional, so the compatibility test passed
untouched and no baseline was taken.

**One rule for a shelf and for a lot** (`counted`, `commands.rs:1594`). What was
counted is the lots in scope — every open lot, or the one named — and they are
taken to `declared`:

- **A shortage goes through `pick`**, with `Wanted::Quantity`: the function a
  sale takes stock by, so there is no second ordering to drift. Earliest expiry
  first, undated oldest-first, each portion at its own lot's cost.
- **An overage joins the lot that goes out last** — the far end of the order a
  shortage walks (`earliest_first(scope).pop()`, `:1635`; *as built it was
  `scope.last()`, the end of the list, which review found disagreeing with the
  picking order — see below*), at that
  lot's own unit cost, through `OpenLot::gives` — now `pub(crate)`
  (`modules/inventory/src/picking.rs:92`), because the found units are one more
  portion of that lot, only going on rather than off. **Why that cost, and not
  the last unit cost or the shelf's average**: a lot carries a value for a
  quantity, and joined at any other price its unit cost becomes a blend, so
  every later portion off it is charged an average of two prices — the thing
  lots exist not to do. The last-out lot's own cost and the last delivery's are
  usually the same number; where they differ (an undated lot a return reopened
  counts as the newest, or the lot has been drawn down to a rounded remainder)
  the lot's own is the one that keeps it honest.
- **More than the lots hold with no lot open is refused** (`NoLotToJoin`,
  `commands.rs:151`, new, en + ar). The count would have to invent a lot: a found carton
  has no delivery behind it to say what it cost, and on a lot-tracked product no
  batch — R1's phantom unit. It comes in as a receipt. See *Left open*.

**The event carries where the variance landed** (`modules/inventory/src/stock.rs:219`).
`Counted` gained `taken`, `joined` and `settles`, and `lot` and `value` became
optional; `apply` draws the portions down, puts the joined one back and settles
the debt (`stock.rs:539`), so the aggregate replays what the decision froze and
nothing is recomputed. `Stock::correct`, which set a lot to `declared`, is gone
— it was a second way for a lot to move. **`taken` has no serde default, on
purpose**: a count written before this slice, decoded as one with no portions,
would replay as a count that moved nothing while the read model had moved the
lot. It fails to decode instead (L6), and no tenant has one — the module has not
shipped.

#### What a count does to the debt, and to the books

A plain product sold short owes units, and the sale credited `1300` for them at
the last unit cost it knew (§74). **A count of the shelf clears the whole debt**
(`settles`, `commands.rs:1604`): what is on the shelf is what was counted, and a
shelf cannot hold less than nothing. `expected` is **on hand** — the open lots
less the debt, which is `Stock::on_hand` and the number a screen showed the
counter *(corrected by review: this said on hand less the debt, which is the
debt taken off twice)* — and the value is what `joined` and
`settles` put back less what `taken` was carried at. It posts through §72's
entry unchanged: short `Dr 5900 / Cr 1300`, over the reverse, nothing on zero.
**A count of one lot leaves the debt alone**; the debt is on no lot.

Two cases, both in `a_count_settles_what_a_plain_product_owes`:

- **The delivery that had not been typed in.** Ten croissants received at 3.00,
  thirteen sold: the shelf owes three, charged out at 9.00, and `1300` reads
  −9.00. The late delivery lands — five at 4.00 — and does not pay the debt
  (§74). The tray holds two, which is what the books say, so the variance is
  **zero** — and the count still settles the three at 9.00 and takes the three
  they stood for off the real lot at 12.00. **It posts 3.00**, the gap between
  the guess and the price. That is the price variance §72 and §74 both refused
  to book on a receipt, because a delivery cannot know what the debt was; a
  person standing at the tray can.
- **The empty tray.** Sold short again — three at 4.00 — and counted at nothing:
  the variance is +3, the count settles 12.00, `Dr 1300 / Cr 5900`, and the
  asset stops being negative. The credit goes to the variance account and not
  back to cost of goods sold: the invoice's cost is what that invoice cost, and
  what a count finds is a control finding, which is §72's reason for the account.

So **"a count equal to on hand posts nothing" holds for a shelf that owes
nothing**. On a shelf that owes, a count that finds exactly on hand still clears
the debt, and posts what the debt was charged out at against what the units
covering it cost — zero when the two prices agree, and the right number when
they do not.

#### A serial-tracked product names what it found

`tally` (`commands.rs:1559`) decides the request's shape before the shelf is loaded,
as `delivery` does for a receipt: serials only on a serial-tracked product, and
`declared` equal to the distinct names or `NeedsSerials` (400). Then `counted`
refuses a name that is not on hand in scope — `NoSuchSerial`, 422, decision 17 —
and what is **missing** is the scope's names that were not given, taken through
`pick` with `Wanted::Serials`, each at its own lot's cost. The read model marks
them `missing`, a new state (`modules/inventory/schema/install.sql:132`), because
`written_off` says somebody threw a unit away and gave a reason. The count is a
row per lot the names came off (`modules/inventory/src/projections.rs:447`).
`SerialsAreNotCounted` and `inventory.serials_are_not_counted` are gone.

#### One serial on a shelf once

The low finding §74's review left open. `receive` refused a serial the shelf is
holding, and **`restore_in` asked nothing**: a phone sold, received again under
its own name — back from repair, bought back — and then its sale's credit note
put a second `SN-1` on the shelf. Two copies of one identity, and the next
write-off by name takes both off the lot's list while taking one off its
quantity: §71's unnameable unit, by another door. **Fixed in one place**: `lands`
(`commands.rs:1686`) is the check a named unit arriving on a shelf goes through, and
both ways a unit arrives call it — `decide_receipt` (`:495`) and the
restoration's decision (`:785`), after the retry check. The refusal is the one
that already existed, `inventory.serial_already_held`, so no new message.

#### Costs

`proj_inventory` is at **version 2** for the `missing` state — **3** since
review, below — (`projections.rs:43`, pinned at `crates/erp-worker/src/bin/migrator.rs:810`) — free, because
no tenant has the group. `just prepare` changed no `.sqlx` file: no checked query
moved. No route was added, so `PERMISSIONS` is untouched; `just openapi`
regenerated the count's description, its body and its statuses. The HTTP
walk-through now counts **the shelf**, naming no lot, and first sends a count
naming the crate that was thrown out, which is refused `inventory.no_such_lot` —
so both shapes are proven to reach the command over the wire; the demo still
counts one lot. And the route's translation is a function with a test at the seam
(`modules/inventory/src/http.rs:132`, `:1123`), which is §74's lesson from the till and the
sales line: a field dropped between JSON and a command is invisible to every test
that calls the command.

**Falsified.** Each fix broken in Rust, the guard run and watched to fail, the
file restored byte for byte (hash-checked) and the guards run again and watched
to pass:

| broke | failed |
|---|---|
| the shortage walks the lots in received order — `pick` over the lots with their dates stripped, a second ordering | `a_shelf_count_takes_a_shortage_off_the_lots_in_picking_order`: *left `[(rcv-1, -12, -6000), (rcv-2, -4, -2400)]`, right `[(rcv-2, -12, -7200), (rcv-1, -4, -2000)]`* |
| `count`'s retry check dropped | same test: a third row, `(None, 0, 0, Some(20), Some(20))` — the retry counted again |
| an overage joins the oldest lot (`.first()`) | `an_overage_joins_the_newest_lot_at_its_own_cost`: *left 71200, right 71600* — the found grams at 0.06, the older sack's price |
| a count of the shelf settles nothing | `a_count_settles_what_a_plain_product_owes`: *the count cleared the debt — left `Some(Shortfall { quantity: 3, cost: 9.00 })`, right `None`* |
| the value leaves out what the debt was charged out at | same test: *12.00 of croissants against the 9.00 guess they were sold at — left 1200, right 300* |
| `Stock::apply` ignores a count's `settles` | same test: *the count cleared the debt* — the aggregate still owed three |
| `Stock::apply` ignores a count's `joined` | `an_overage_joins_the_newest_lot_at_its_own_cost`: *a count that found the books right posted something — left 73200, right 71600*; the shelf forgot the grams and the level count found them again |
| a named serial that is not on hand is accepted | `a_serial_count_names_what_it_found_and_the_rest_leave`: *a unit nobody received was counted into existence* |
| a serial count takes nothing off for the units it did not name | same test: *left 3, right 1* |
| a serial count's `declared` need not agree with its names | same test: `NeedsSerials { units: 2, named: 1 }` not returned |
| a count of one lot counts the whole shelf | `what_a_count_found_is_frozen`: *left `Some(10000)`, right `Some(5000)`* |
| an overage with no lot open lands nowhere instead of refusing | `a_count_settles_what_a_plain_product_owes`: `NoLotToJoin { found: 1 }` not returned |
| `restore_in` stops asking `lands` | `a_restoration_cannot_put_a_serial_on_the_shelf_twice`: *a credit note put a second SN-1 on the shelf* |
| `decide_receipt` stops asking `lands` | `a_serial_that_is_not_on_the_shelf_is_refused`: a second unit was received as `SN-3` |
| the projection does not put an overage on the lot it joined | `an_overage_joins_the_newest_lot_at_its_own_cost`: *left `(rcv-2, 5000, 40000)`, right `(rcv-2, 5200, 41600)`* |
| the projection writes no row for the debt a count settled | `a_count_settles_what_a_plain_product_owes`: *left `(None, 0, 0, Some(-3), Some(0))`, right `(None, 3, 1200, Some(-3), Some(0))`* |
| the projection leaves a unit the count did not find `on_hand` | `a_serial_count_names_what_it_found_and_the_rest_leave`: *the read model still lists a machine the count did not find — left `[["SN-1", "SN-2"]]`* |
| the zero filter removed from `two_sided` | `an_overage_joins_the_newest_lot_at_its_own_cost`: *counts: `Unbalanced(ZeroLine { index: 0 })`* — the level count |
| the route's count drops `serials` | `a_count_carries_its_lot_and_its_serials_to_the_command`: *left [], right ["A-1", "A-2"]* |
| the route's count drops `lot` | same test: *left None, right Some("lot.x")* |
| the same, against the HTTP walk-through | `stock_is_declared_received_written_off_and_counted`: *left 201, right 422* — a count of the thrown-out crate counted the shelf instead |

**Prose the change made false, and fixed.** `inventory`'s module doc said *"a
count counts one lot — for now"* and listed the shelf-wide count and settling a
debt as not here; the `count` command's rustdoc argued why a count took a lot
and not yet the shelf; the route's description (in `docs/openapi.json`) said a
shelf-wide count is *"decided and not built"* and a serial-tracked product is
refused, and `NewStockCount.lot` that a count is of one batch. `MovementRow.lot`
still said *"nothing writes"* a lot-less row, stale since §74, and `kind` did
not name `consumed`. `install.sql` described `counted` as *"somebody counted a
lot"*, and `entry_for_variance` valued a count *"at what that lot is carried
at"*. The *Left open* entries in §71–§74 that waited on this slice are struck
through and pointed here, and Phase 19's third box is ticked.

#### What review found — a debt with two owners, and an order with two ends

Three findings, all verified against the code before anything changed: two
behavioural, one prose.

**A return settled a debt a count had already cleared** (high). The shelf's
debt, `Stock.owed`, and what each consumption still owes, `WentOut.shortfall`,
are one number read two ways, and until this slice only a return moved them —
both at once. A count of the shelf became a second way to clear `owed`, and
`Returning`, the fold a return decides from, never heard it. So a credit note
after a count *settled* units the shelf no longer owed: `Stock::settle` floors at
nothing, the units vanished from the aggregate, and `1300` and `stock_item` still
counted them. The reviewer's case: ten received at 30.00, thirteen sold, counted
empty, the invoice credited whole — the aggregate held 10 at 30.00 while the
books and the read model held 13 at 39.00, a rebuild reproduced it, and the next
count of the thirteen real croissants booked the same 9.00 gain a second time.

**Fixed in the fold.** `Returning::apply` (`modules/inventory/src/stock.rs:424`)
now hears a count that settles: whatever the followed consumption still owed
moves from `shortfall` to `WentOut::counted` (`stock.rs:301`). It is not a debt any
more, because a count clears all of it, and it cannot become one again, because a
shortfall is only ever added by the consumption itself. `coming_back`
(`modules/inventory/src/commands.rs:856`) settles only what is still owed and
**lands what a count cleared as stock** (`:918`): a portion on a lot of its own,
`returned_lot_of(shelf, taken_on)` (`:1139`, prefixed `back.` so no receipt
reference can name it), at what the sale charged those units out at.
`WentOut::give_back` takes a return landing there off `counted`, so a second
credit note can only return what the first left.

**Why a lot, and not the refusal review offered.** The finding suggested capping
what a return settles at what the shelf owes and refusing the rest (L6). That
refuses the whole cancellation — `cancel_in` restores every product line — of
every invoice whose short sale was later counted, for good: §74's finding about
statutory cancellations, by another door, met by any shop that oversells, counts
at close and then cancels an invoice. Dropping the cleared units from what is
still out instead would restore ten of thirteen without saying so, which is the
degrading L6 forbids. And the two reasons `NoLotToJoin` refuses a found carton do
not hold here: these units have a frozen cost, and they are on a plain product —
the only kind that can sell short (R1) — so there is no batch or date to invent.
One lot per consumption, so two returns of one sale land at one unit cost and two
sales never blend into an average.

**Units nothing was ever paid for** come back at nothing: a product sold before
anything was received carries a shortfall with no cost. On a shelf with no
currency either — a bakery that bakes what it sells and never receives it — the
zero is stated in the currency the inventory account is kept in
(`ledger::posting_currency`, `commands.rs:775`), the only currency `receive` would
ever let a delivery land in. That is one account load per return. Where the
inventory account does not exist, such a unit is refused `NoSuchAccount`, which
is what any posting to it says; nothing else is refused for it.

**The read model follows** (`put_back`, `modules/inventory/src/projections.rs:658`).
An `UPDATE` became an upsert, so the lot a return opens gets a row — positioned at
the return, with the return's quantity — and a lot that exists keeps its receipt's.
`proj_inventory` is at **version 3**. `install.sql`'s shape did not change, so the
pinned hash stayed and only the version moved
(`crates/erp-worker/src/bin/migrator.rs:810`); its comment on `lot` now names the
one row no receipt makes.

**An overage joined the end of the list, and the list has two orders** (low).
`scope.last()` took the last lot in `Stock::lots`, which is received order —
except that `Stock::put_back` pushes a lot a return reopens onto the end. So after
a credit note reopened an older batch, a count's overage joined that batch: its
code, and a date that may already have passed. **Fixed by giving the count the one
order the module has.** `earliest_first` (`modules/inventory/src/picking.rs:249`,
now `pub(crate)`) is what a shortage walks, and the overage takes its other end
(`commands.rs:1635`). For dated stock that is the latest expiry, whatever the list
says. For undated stock it is the last received — or a lot a return put back,
which `put_back` already calls the newest on its shelf and which a sale reaches
last. That half is a choice, not an accident; see *Left open*. `put_back`'s rustdoc
said a reopened lot at the end *"changes nothing for a dated one"*, which the
overage had made false; it is true again.

**`expected` was documented as on hand less the debt** (low). `Stock::on_hand`
already takes the debt off, so read literally that is the debt twice; the code
takes it off once (`commands.rs:1605`). The event's doc (`stock.rs:227`) and the
paragraph above now say what the code does.

**Prose the fixes made false, and fixed.** `restore_in`'s rustdoc said a return
takes *"the debt first"* and that this is *"what keeps the count and the books
agreeing"*, and §74 said a returned unit settles what the shelf owes before it
lands on a lot; both now say the debt is settled only while it is owed. The
`commands` module doc said a return goes back *"on the lots it left"*. The count's
rustdoc, `StockEvent::Counted::joined`, `inventory`'s module doc and the route's
description (in `docs/openapi.json`) said *"the newest lot"*; they now name the
far end of the picking order.

**Falsified.** Each fix broken in Rust, the guard run and watched to fail, the
file restored from a copy (byte-identical by `cmp`) and the guard run again and
watched to pass:

| broke | failed |
|---|---|
| `Returning::apply` leaves a cleared shortfall owed (`counted` never set) | `a_return_after_a_count_cleared_the_debt_puts_the_units_back`: *the units the customer brought back vanished from the shelf — left `[(rcv-1, 10, 3000)]`, right `[(back….inv-1.line-1, 3, 900), (rcv-1, 10, 3000)]`* |
| `give_back` does not take a return landing on `lands_on` off `counted` | same test: *or came back twice — left `[(back…, 5, 1500), (rcv-1, 10, 3000)]`* |
| the projection's `put_back` only updates a lot that exists, as before | same test: *projected: the read model's lots — left `[(rcv-1, 10, 3000)]`* |
| no fallback to the inventory account's currency | same test: *three come back at nothing: `Ledger(NoSuchAccount("1300"))`* |
| the overage joins `scope.last()` again | `an_overage_joins_the_lot_that_goes_out_last_after_a_return`: *the found bottle joined the batch expiring first — left `(Some(2), Some(12))`, right `(Some(1), Some(13))`* |

**Left open.**

- **An undated lot a return reopened goes out last, so a plain shelf's overage
  joins it** at its old unit cost rather than the latest delivery's. Review asked
  for received order. That means remembering the order of lots that have closed —
  an ordinal frozen on every portion — and it would also change the order returned
  stock is sold in, which §74 chose. The dated case, where the batch and the date
  are the harm, is fixed.
- **A count's price gap is not reversed by a return.** When a count settles a debt
  that a late delivery covered, it books the gap between the guess and the price
  (3.00 in the croissant case). A return of those units comes back at the guess,
  on a lot of its own, and the gap stays booked.
- **Found stock with no lot to join.** Refused. A tenant counting stock that
  arrived with no receipt enters the receipt, which is right when there is
  paperwork and awkward when there is none. The alternative is a lot derived
  from the count at the last unit cost — and on a lot-tracked product that is a
  batch with no code, which R1 refuses everywhere else.
- **An overage on a lot-tracked shelf takes the code and date of the batch that
  goes out last.**
  That is what R2 says, and it is a guess about which batch the found bottles
  came in. A counter who knows counts that lot.
- **A serial count cannot find a unit the shelf never held.** Refused, not
  received: a count does not invent identities, and the unit comes in as a
  receipt with its cost.
- **A delivery that names one serial twice for one unit** is accepted:
  `delivery` counts distinct names against the quantity and keeps the list as
  sent, so the lot carries the name twice. Found by reading while writing
  `tally`, which dedupes; not tested and not fixed here — it is §71's class of
  finding, and harmless today because a unit leaves by name.
- **A count is not segregated** (§71), unchanged.

### 74 · An invoice depletes the shelf, and books what it sold

**Built 2026-09-13**, Phase 19 boxes 2 and 4. Everything before this slice was a
shelf nobody could sell off: `inventory::consume_in` picked lot by lot and built
a cost-of-goods-sold entry, and **no route reached it**, so every margin in the
system was the invoice's net and the cost of the goods was zero.

**The hook is in `sales::issue_in`** (`modules/sales/src/commands.rs:436`),
which is decision 2 and is the whole of why it is one line of code rather than
three. Every invoice this system issues goes through that function — the
`/v1/sales` route, `pos::sell` at the till, `erp-api`'s booking bill,
`payments`' deposit — so putting the depletion there means a till sale and a
counter invoice cannot disagree about what came off a shelf, because there is no
second place for them to disagree in. It runs **in the invoice's own
transaction**, after the journal entry, for the same reason the entry is in that
transaction: an invoice that exists without the movement that supplied it is not
a state this system can reach. **And only off the `Issued` event that
transaction wrote**, so a retried invoice depletes nothing — review found a retry
reaching the shelf; see *What review found*, below.

**A line names a product and a quantity, and both are optional.** A required
field breaks every client that exists and would have demanded a new baseline; an
optional one does not, and the compatibility test passed untouched. `DraftLine`
and `InvoiceLine` gained `product`, `quantity`, `unit` and `serials`
(`modules/sales/src/invoice.rs`), all `#[serde(default)]`, so every line ever
written decodes as what it was: a bare total, quantity one.

**The line total is computed and never divided back out**
(`modules/sales/src/commands.rs:1736`, decision 3). Given a price and a
quantity, `priced_lines` multiplies; the unit price is stored beside the
quantity rather than recovered from the total later, because recovering it is a
division that does not always land on a whole halala — and `cbc:PriceAmount` has
to be exact or the tax document stops balancing.

**Which is the other half of this slice.** `modules/tax_sa/src/zatca/ubl.rs:664`
printed `<cbc:InvoicedQuantity>1</cbc:InvoicedQuantity>` on every line ever
issued, and BT-131 —

```text
BT-131 = quantity × (BT-146 / base quantity) + charges − Σ BT-136
```

— balanced only because there was nothing to multiply by. The quantity is now
the line's own and `cac:Price` is **one unit's**, so the rule holds for a line
of three the way it did for a line of one
(`modules/tax_sa/src/zatca/ubl.rs:734`). A line with no factors still renders as
one unit at what it came to, which is byte-identical to what it rendered
yesterday.

#### What the shelf cannot cover: one branch, two answers

Revision **R1**, at `modules/inventory/src/commands.rs:1546`. A **lot- or
serial-tracked** product refuses: what is on that shelf is meant to be known
exactly, a phantom carton has no batch and no expiry, and a named unit that is
not there was never there. A **plain** product sells anyway — a till does not
stop for a bad count — and what no lot could cover is recorded as a
[`Shortfall`] at the shelf's last known unit cost (decision 16). The refusal
takes the invoice with it, because it is raised inside the invoice's
transaction.

**The refusal sits after the already-heard check**, inside the decision
(`modules/inventory/src/commands.rs:691`). This repo has been bitten by exactly
the other order — it is where `sales::cancel_in` puts its claim and its limit —
and the failure is ugly: a client whose request times out retries, and the
second attempt is refused for stock the first attempt already took.
*(Corrected by review.)* This paragraph named
`the_same_invoice_sent_twice_takes_the_stock_once` as the test that pins the
order, and said breaking it made the shelf read four instead of seven. It did
not pin it: that test sells a plain product with stock to spare, which neither
order refuses, and four is what *removing* the check does. The guard is now
`a_retried_consumption_is_not_refused_for_the_stock_it_already_took`
(`modules/inventory/tests/inventory.rs`), on a lot-tracked shelf the sale
empties. `issue_in` no longer reaches `consume_in` on a retried invoice at all,
so the order is `consume_in`'s promise to any caller rather than the invoice's
only defence.

**A shortfall is now in an event, and that needed a decision §72 declined to
make.** `StockEvent::Consumed` gained `shortfall`, and `Stock` gained `owed`
(`modules/inventory/src/stock.rs:460`) — which `on_hand` and `value` both come
down by, because the books already have: the sale credited `1300 Inventory` for
the shortfall when it sold it. A shelf that owed without saying so would
disagree with that account for ever, and §72's own invariant would have reported
it. The open question was what a later receipt does to the debt. **The answer is
that it does nothing**: the shortfall was costed at a guess, and netting a real
delivery against it would have to put the difference between the guess and what
the delivery actually cost into a price-variance account nobody has opened. A
count settles it, which is decision 16's own word for it.

#### A return puts the goods back where they came from

`inventory::restore_in` (`modules/inventory/src/commands.rs:756`), called from
both credit-note roots: `credit_part_in` for the lines a client says came back,
and `cancel_in` for the whole of an invoice it undoes.

**Where it lands and at what was the question this slice had to answer**, and it
had two rules against it: the read model may not be consulted (L3), and a cost
may not be guessed (L6). The answer is the shelf's **own stream**:
`inventory::stock::Returning` is the shelf seeded with the reference of the
consumption being undone and folded through **the whole** stream, in the
transaction that appends the return, so it reads the consumption event itself
rather than a second record of it. *(As built, a return read the consumption
out of the shelf's bounded window of movements heard; review found a sale older
than the window could then never be returned or cancelled. See below.)* So a
return puts
back the lots that sale took, at what that sale froze — not at today's cost,
which would restate a margin already reported, and never at a share of what was
credited (decision 12), because the money and the goods are two statements and
dividing one into the other does not always land.

**A lot that the sale emptied reopens as itself.** `Portion` gained `code` and
`expires_on` (`modules/inventory/src/picking.rs:137`), frozen at consumption for
the same reason the cost already was: a lot that empties closes and leaves the
aggregate, so by the time the customer brings the carton back there is nothing
left to ask. Without them a returned batch would come back undated and go out
*last*, which is the opposite of what an expiry rule is for.

**The debt is paid before the shelf is refilled** (`coming_back`,
`modules/inventory/src/commands.rs:856`): a returned unit settles what the shelf
owes before it lands on a lot, because a phantom unit is not stock and the count
and the books have to agree at the end of it. *(Corrected by §75's review: only
while the shelf still owes it. Once a count of the shelf has cleared the debt,
those units come back as stock on a lot of their own; settling them paid off
nothing and lost them from the shelf.)* **Part of a movement whose units
have names is refused** — which of three phones came back is not a thing to
guess (decision 17). *(Since §76, unless the credit line names them.)*

**A credit note's quantity is the client's and its amount is another statement.**
`CreditLine` gained an optional `quantity`; a line without one puts nothing
back (and since review, one of nothing, or one against a line that sold no
product, is refused rather than dropped), which is what a goodwill credit means and what `credit_what_is_clear`
means when it spreads a *refund* across lines — it knows the money and nothing
about the goods.

**And that is exactly why the goods need their own cap.** `credit_part_in`
already refuses more *money* than a line has left, and a review of this slice
found that cap does not stand in for the units: crediting 10.00 twice off a
75.00 line is legal, and a client claiming three sacks each time would have got
six back. So what a consumption still has out **shrinks as returns land against
it** (`WentOut::give_back`, applied as `Returning` folds each `Restored` naming
that consumption), and `StockEvent::Restored` carries the movement it undoes so
the fold can find it.
A second credit note can then only return what the first one left, and the
refusal says which: *"1 of that movement is still out and 2 are coming back"*.

#### Every other caller of `issue_in`, and what it passes

- **`pos::sell`** (`modules/pos/src/commands.rs:725`) passes `basket.lines`
  straight through: a till sale is a `sales` invoice and the till decides
  nothing about stock. Its `NewLine` gained the same three optional fields and
  `ReturnedLine` gained `quantity`, so a return at the counter restores.
- **`erp-api::billing`** (`crates/erp-api/src/billing.rs:150`) passes `None`. A
  booking charges for a slot, not for a thing; a room and an hour are not on a
  shelf, and `booking`'s own lines have no product to carry.
- **`payments`** (`modules/payments/src/commands.rs:391`) passes `None`. A
  deposit is money taken **before** the supply and nothing leaves a shelf when
  it is taken; the final invoice carries the product lines.

#### Costs paid, and the one that was not

**`sales` now depends on `inventory`.** The same edge `purchases` took a slice
earlier, and the arrow only points one way: `inventory` depends on `ledger` and
`branches` and on nothing that depends on it. `inventory` is added to `sales`'
`reading(…)` set; a tenant without the module has no products, so no line can
name one.

**No `VERSION` bump and no fleet rebuild**, which is not luck. §71 declared the
`consumed` kind, the `sold` state and the nullable `lot` a shortfall leaves
empty a slice before anything wrote one, on the argument that a shape change
after shipping is a rebuild and adding a field to an *event* is not. A
restoration writes `kind = 'received'` — "bought in, or put back", which that
file already said — so the whole of this slice landed inside the existing
shape.

**Falsified.** Each fix broken in Rust, the test watched to fail, the file
restored, the test watched to pass:

| broke | failed |
|---|---|
| `issue_in` stops calling `consume_in` | `an_invoice_takes_its_lines_off_the_shelf_and_books_what_they_cost`: *three sacks went out — left 10, right 7* |
| ~~the already-heard check moves after the shelf is judged~~ | ~~`the_same_invoice_sent_twice_takes_the_stock_once`: *not four — left 4, right 7*~~ — **wrong, found by review**: moving the check leaves that test passing; see *What review found* |
| R1's branch dropped, so a plain product refuses too | `a_plain_product_the_shelf_cannot_cover_sells_and_records_the_shortfall`: panicked on `.expect("a till does not stop")` |
| the whole R1 guard dropped, so nothing refuses | `a_tracked_product_the_shelf_cannot_cover_refuses_and_leaves_no_invoice`: the short sale was accepted and the document exists |
| `cost_of` ignores the shortfall | same test: *two off the lot and one at the last unit cost — left 20.00, right 30.00* |
| `Stock::value` forgets what the shelf owes | same test: *and the value owes what it was charged out at — left Some(0.00), right Some(-10.00)* |
| `priced_lines` stops multiplying price by quantity | `an_invoice_takes_its_lines_off_the_shelf…`: *left 25.00, right 75.00* |
| `came_back` puts nothing back | `a_returned_line_puts_the_stock_back_and_the_cost_with_it`: *two sacks are back — left 0, right 2* |
| a reopened lot loses its code and date | `a_returned_batch_comes_back_with_its_code_and_its_date`: *the batch it was — left None, right Some("B-2026-04")* |
| a whole cancellation restores nothing | `cancelling_an_invoice_puts_everything_back_on_the_shelf`: *left 6, right 10* |
| the ZATCA line goes back to quantity one | `a_line_priced_per_unit_states_both_factors_and_still_balances`: the `cbc:InvoicedQuantity` assertion, `ubl.rs:1732` |
| `cbc:PriceAmount` prints the line total again | same test: *the price is one unit's, not the line's*, `ubl.rs:1736` |
| the shelf stops taking a return off what the movement still has out | `a_second_credit_note_can_only_return_what_the_first_one_left`: *only one sack is still out: Ok(Numbered { … credit_note: "CN-00002" … })* — three sacks sold and four back |
| the till stops carrying its lines' products into the draft | `a_till_sale_takes_the_stock_off_the_shelf_and_a_return_puts_it_back`: *three sacks left the shop — left 10, right 7* |
| the till's **wire** line drops its product | `a_till_line_carries_its_product_and_its_units_into_the_draft`: *left None, right Some("f81d4fae-…")* |
| the till's wire line drops its serials | same test: *left [], right ["A-1", "A-2", "A-3"]* |
| the sales route drops the product off the draft line | `a_line_carries_its_product_and_its_units_to_the_draft`: *left None, right Some("f81d4fae-…")* |
| the sales route drops the quantity off the draft line | same test: *left None, right Some(3)* |

**The last four are there because a falsification did not falsify.** Breaking
`modules/pos/src/http.rs`'s `lines` — the one that turns a till's JSON into a
draft — left `a_till_sale_takes_the_stock_off_the_shelf_and_a_return_puts_it_back`
**passing**, because that test calls `pos::sell` with a `Basket` and never goes
near the wire. A grep then found that **nothing in the repo** had ever sent a
`product`, a `quantity` or a `serials` on a sales or a till line over HTTP: not
`crates/erp-api/tests/http.rs`, which has no `/v1/pos` test at all, and not the
demo, whose till rings coffee and whose stock is bought rather than sold. Both
translations were new, both were on the path a shop's stock actually takes, and
both were unguarded. The guard is one test each at the seam itself
(`modules/sales/src/http.rs:1774`, `modules/pos/src/http.rs:1001`) — the shape
`purchases::http` and `pos::http` already use for their own translation
decisions — asserting the three fields arrive and that `net` is still **one
unit's**, because multiplying twice is a division nobody can undo. A second
test each pins the line that names nothing: that is what every invoice issued
before these fields existed is.

#### What review found — one window doing two jobs, and a key only one invoice owned

Nine findings, all verified against the code before anything changed, and they
came from three roots.

**Root one: the shelf's window of movements heard was answering two questions.**
It was built for *has this movement already happened*, where forgetting is
harmless because a client does not retry a request from two hundred movements
ago. §74 then asked it *what did that sale take*, where forgetting is not
harmless at all — and a busy product rolls its window in an afternoon.

- **A retried invoice took the stock twice** (high). `issue_in` called
  `deplete` on every retry, trusting the shelf's window to recognise the line.
  The invoice is idempotent for ever through `try_create`; the window is not. A
  reviewer received ten sacks, sold three, received two hundred more and sent
  the same invoice again: `Ok` with no events, while the shelf went from 207 to
  204 and the cost entry — whose id is derived — posted nothing, so the shelf
  stopped agreeing with `1300 Inventory`. On a tracked product the same retry was
  refused `not_enough_stock`. **Fixed in one place**: `deplete` takes what
  `try_create` committed and reads the lines off the `Issued` event it wrote,
  the way `came_back` reads `Credited` — no event, no depletion, so a retry
  cannot reach a shelf by construction. (A first cut gated the call on
  `committed.at.is_some()` in `issue_in`; clippy's line limit on that function
  pushed it into the shape it has, which is the better one anyway.) Nothing
  needed healing: no invoice has ever been written without its depletion,
  because both are one transaction. `deplete`'s rustdoc, which said it ran on a
  retry *"and the shelf recognises the movement's reference"*, now says the
  opposite and why.
- **An invoice for a busy product could never be cancelled** (high). A return
  read the consumption out of the window (`Stock::taken_on`), and `cancel_in`
  restores every product line, so this was not *"a return older than the
  window"* — as *Left open* put it — but the statutory cancellation refused for
  good with `inventory.not_consumed`. **Fixed by taking the second job away from
  the window.** `inventory::stock::Returning` is the shelf plus one followed
  consumption: seeded with the reference being undone, it folds the **whole**
  stream, records that consumption when it meets it and takes each later
  `Restored` naming it off what is still out. `restore_in` decides from it
  through `erp_eventlog::try_execute_from`, a new seam that is `try_execute` with
  a seed instead of `Default` — `try_execute` is now that call with
  `A::default()` — so the load, the decision and the optimistic append are still
  one pass and what the decision saw is what the append is checked against. It
  costs nothing a load was not already paying: a load reads the whole stream.
  The window is back to references only (`Heard` and `Stock::taken_on` are
  gone), which is also what decision 20 wanted of it.
- **The guard for "the refusal sits after the retry check" did not guard it**
  (medium). The reviewer moved the check below the shelf's judgement and
  `the_same_invoice_sent_twice_takes_the_stock_once` still passed: a plain
  product with stock to spare is refused by neither order. The falsification
  table above claimed otherwise; that row is struck through. The guard is now
  `a_retried_consumption_is_not_refused_for_the_stock_it_already_took`, at
  `consume_in` itself, because after the first fix no invoice retry reaches it.

**Root two: a return was keyed on something that was not unique where it
landed.**

- **Two credit notes with one client reference on two invoices shared a return**
  (high). `return_reference` was `r.{client reference}.{line}`, and a client
  reference is only unique per invoice — `has_credit` lives on the invoice, and
  the credit entry's id carries the invoice beside it. Invoice B's unit was
  heard as a retry of invoice A's and never came back, while B's money credited
  in full. The same held for `cancel_in` and for a till return's `reference`.
  **Fixed by keying on the credit note's number** (`r.CN-00001.0`), the
  tenant's own gapless series, which cannot repeat. Adding the invoice id
  instead, as the finding suggested, would not have fitted: a UUID invoice, a
  UUID reference and a branch beside a UUID product overrun `AggregateId`'s 128
  characters. Stock only moves in the transaction that issues the credit note,
  so the number is final whenever it is used.
- **Two lines of one credit note against one invoice line put back one**
  (medium). `priced_for_credit` allows it; each became a return under the same
  reference and the second was a silent no-op (L6). **Fixed** by making what
  comes back a `BTreeMap` keyed by product and invoice line
  (`sales::commands::ComingBack`): quantities add up, one invoice line is one
  return by construction, and iterating the map is the product order decision
  14 already asked for. Summing rather than keying on the credit line's own
  position also means two lines returning one phone each of a two-phone sale is
  the whole line coming back, not two refused halves.

**Root three: the shelf was the request's, not the sale's** (medium, *plausible*
in review and confirmed by its test). `restore_in` took the branch off the
credit note's request, so a cancellation raised at head office for a sale rung
at Olaya looked for the consumption on head office's shelf and was refused.
**Fixed**: `Restoration` carries `branch`, and `sales::restore` reads it off the
invoice's own `Issued` event metadata — the branch the sale happened at, which
is the shelf it depleted. The cost reversal still posts under the credit note's
request, exactly as `ledger::reverse_in` does for the money.

**And three on their own.**

- **A quantity on a line that sold no product was dropped** (low), and a
  quantity of nothing was never checked. `priced_for_credit` now refuses both,
  inside the decision and after the retry check: `sales.not_a_stock_line` (new,
  en + ar) and `sales.not_a_quantity`.
- **The documented statuses did not name the stock refusals** (low), and the
  till answered `inventory.needs_serials` with 422 where `/v1/sales/invoices`
  answers 400. `pos::http::problem_for` now sends a malformed stock refusal to
  400 through `InventoryError::is_malformed`, the same split the sales and
  inventory routes use. The `utoipa` descriptions on the invoice, both credit
  note routes and the till's sale and return name the stock codes. **Two of the
  descriptions being rewritten were already false**: the partial credit-note
  route documented `409` for an invoice already cancelled and `422` for no such
  line and more than is left, and `sales_problem` answers all three with 400;
  they say so now. *Not done:* the till still answers every other `sales`
  refusal with 422 where the sales routes split 400 from 422, which predates this
  slice and would change statuses nobody reported. *(Since §76's review a
  malformed sales refusal is a 400 at the till too, through
  `SalesError::is_malformed`; four credit refusals still differ — §76's
  Left open.)*
- **`zatca::Line`'s rustdoc** (low). `units()` and `price()` had been inserted
  between `before_allowances`' doc comment and its function, so `units()`
  documented BT-146 and `before_allowances` had none. Moved back.

**Costs.** `erp-eventlog` gains `try_execute_from`; `inventory` stops exporting
`Heard` and exports `Returning` and `WentOut`; `sales` takes `branches` as a
dev-dependency for the cross-branch test. No event changed shape, no SQL
changed, no `VERSION` bump. A return's movement reference is now the credit
note's number, which is also what a manager reading the movement list would
look for.

**Falsified.** Each fix broken in Rust, the guard watched to fail, the file
restored (hash-checked) and the guard watched to pass:

| broke | failed |
|---|---|
| `issue_in` hands `deplete` an `Issued` built from the draft whether or not one was written | `a_retried_invoice_takes_nothing_once_the_shelf_has_moved_on`: *the sale took its three once — left 204, right 207* |
| `Returning` forgets a consumption once it leaves the window — the old bound, reproduced | `an_invoice_can_be_cancelled_long_after_its_shelf_has_moved_on`: *cancels: `Rejected(Stock(NotConsumed("INV-OLD-CANCEL.0")))`* |
| a partial return keyed on the client's reference again | `one_client_reference_on_two_invoices_puts_both_back`: *one back from each — left 7, right 8* |
| a second credit line against one invoice line overwrites the first instead of adding | `two_credit_lines_against_one_invoice_line_put_both_back`: *both units are back — left 8, right 9* |
| `restore_in` takes the shelf from the request's branch | `a_cancellation_from_another_branch_puts_the_goods_back_where_they_were_sold`: *head office cancels a sale rung at Olaya: `Rejected(Stock(NotConsumed("INV-OLAYA.0")))`* |
| `sales::restore` drops the branch it read off `Issued` | same test, same refusal |
| both credit-line quantity checks dropped | `units_coming_back_that_cannot_land_are_refused`: *an hour of consultancy has no shelf: `Ok(Numbered { … credit_note: "CN-00001" … })`* |
| `consume_in` judges the shelf before the retry check | `a_retried_consumption_is_not_refused_for_the_stock_it_already_took`: *`NotEnoughStock { held: 0, wanted: 3 }`* |
| the till's malformed-stock arm removed | `a_malformed_stock_line_is_a_bad_request_at_the_till_too`: *left 422, right 400* |
| only the `quantity <= 0` check dropped | same test: *nothing coming back is not a quantity: `Err(… Stock(NotAQuantity))`* — still refused, but by the shelf after the credit note was decided, rather than inside the decision |

**Prose the change made false, and fixed.** `inventory`'s module doc, its
`commands` and `posting` docs, `stock.rs`'s event docs and `schema/install.sql`
all said in four places that nothing called `consume_in`; `install.sql` also
said what is on hand could never go below zero and that no event could carry a
shortfall, and `StockEvent::Received` still said a receipt posts nothing, which
§73 had already made untrue. `MovementView.lot`, `StockView.on_hand` and
`StockView.value` reach `docs/openapi.json` and said the same things. The book's
`sales.md` said there is no quantity or unit price and that adding them would be
an upcaster; it was neither, because both fields default. And `sales::http`'s
`drafted`, split out of `issue_invoice` in this slice, had been given
`sales_problem`'s doc comment along with its own — a function that parses lines
does not *"map a command failure onto a status"*; the paragraph is back where it
belongs.

**Left open.**

- ~~**Settling what a shelf owes.**~~ **Closed by §75**: a count of the shelf
  clears the debt at what the sale charged it out at, and takes the units it
  stood for off the lots at theirs. As it stood here: a plain product sold below
  zero carried the debt until a **shelf-wide** count cleared it, and only a
  per-lot count existed. Counting the lot the units came off did not help: the
  debt is not on a lot.
- ~~**Naming which units come back.**~~ **Closed by §76**: a credit line and a
  till return carry `serials`, and each name has to be still out on the sale
  being undone. As it stood here: a return of *part* of a movement whose units
  have names was refused rather than guessed at, and `CreditLine` had no field
  for serials.
- ~~**A return older than the window.**~~ **Closed by review, and it was worse
  than this said:** `cancel_in` restores every product line, so it was not an
  old return that was refused but the statutory cancellation of any invoice
  whose shelf had moved on. A return now follows the sale through the whole
  stream.
- ~~**A line that names a lot.**~~ **Closed by §76**: a sales line and a till
  line carry `lot`, and `deplete` hands it to `consume_in`. As it stood here:
  `Consumption` took an optional `lot` and `pick` honoured it, and no sales line
  could say so.
- **Two branches, one document.** A line depletes the shelf at *the request's*
  branch, which is right for a till and wrong for a warehouse shipping one
  invoice out of two places. That is a transfer, which is its own command.

### 73 · Receiving posts, and the supplier's bill clears it

**Built 2026-09-13**, Phase 19 slice 3, on decision **R3** — which supersedes
decision 8. §72 shipped a check that was correct and unusable: it compared what
the shelves are worth against `1300 Inventory`, and because a receipt posted
nothing and the *bill* debited the asset, every delivery made the two disagree
until somebody typed the invoice in. `HealthJob` logged `invariant violated` at
error level every five minutes for the length of that window, which is the
ordinary state of a business. An alert that fires on the normal case is the one
that gets muted.

**So the model moved, not the alert.** A receipt debits inventory and credits a
new liability, and the supplier's bill relieves that liability instead of the
asset:

```text
delivery  Dr 1300 Inventory              Cr 2010 Goods received, not invoiced
bill      Dr 2010 Goods received…        Cr 2000 Accounts payable  (+ 1200 input VAT)
```

Between the two, `2010` is exactly what has arrived and nobody has billed for.
After both it is zero. And **the stock is on the balance sheet from the moment
it lands**, which is the accounting answer as well as the operational one: the
goods are in the building, they can be sold and they can spoil.

**One code, three charts, and it is a liability.** `2010` sits beside `2000
Accounts payable` in `services`, `retail` and `real_estate`, in both languages
(`modules/ledger/src/charts.rs:172`, `:357`, `:573`) — the way `1300` and `5010`
went in a slice earlier. `the_conventional_accounts_exist_in_every_shipped_chart`
catches a chart that is missing it, and a second test catches the subtler
mistake: `what_is_received_and_not_invoiced_is_owed`
(`modules/inventory/src/posting.rs:309`) asserts the account is a **Liability**
in every chart, because a code that landed in the 1000s would put a negative
asset on the balance sheet and still balance — invisible to `TrialBalance`,
which is the same argument `money_that_is_held_rather_than_earned_is_a_liability`
makes for the property chart.

**`receive` grew the loop the other three movements already had**
(`modules/inventory/src/commands.rs:431`): `begin` / `try_execute` / `post` /
`settle`, the shape `pos::close_shift` has, because a transaction spanning this
module's event and the ledger's cannot leave the retry inside either. It was the
one command that did not need it and now it is not. The decision moved out into
`decide_receipt` (`:495`) only because the retry borrows the receipt on every
attempt and a closure that moved it could run once. The entry is named `ir.` plus
the shelf and the movement's own reference, derived like the other three (L8) —
so re-posting a retried delivery is the ledger's no-op as well as the shelf's,
which `a_delivery_debits_the_shelf_and_credits_what_is_not_yet_invoiced` asserts
by receiving the same key twice.

**`PostingAccounts` grew a fifth field, and that is a config shape change**
(`modules/inventory/src/posting.rs:94`). A tenant who had stored the §72
shape gets a `ConfigError` from `resolve` rather than a silent default, which is
what every module does and the right answer (L6) — and nobody has that shape,
because `inventory` has not shipped to a tenant. `GET`/`PUT
/v1/inventory/posting-accounts` carry it; no route was added, so `PERMISSIONS`
and its count are untouched. The `PUT` body gained a **required** field, which
would break clients — it does not, because `inventory`'s operations are not in
`docs/openapi.baseline.json` yet: they are new operations, which the
compatibility test treats as additive. Nothing was accepted by hand.

**A bill line names a product, and the product is optional.** `BillLine.product`
(`modules/purchases/src/bill.rs`) is `Option<AggregateId>`, `#[serde(default)]`
on the wire — a **required** field there would have turned every existing
client's request into a 400, and the compatibility test would have demanded
`just baseline` for a change nobody asked for. Absent, everything behaves
exactly as it did; `a_line_with_no_product_on_it_is_untouched` is the test that
fails if the substitution ever leaks into rent.

**The arrow points `purchases` → `inventory`, and it had to.** The bill is the
document that posts, so the decision has to be taken where the entry is built —
`inventory` cannot reach in and change it. `inventory` depends on `ledger` and
`branches` and on nothing that depends on it, so there is no cycle for cargo to
refuse, and `sales` will need the same edge the day a line depletes a shelf. The
seam is `inventory::accepts_movements`, already public and written for exactly
this, asked of the **log** and inside the bill's transaction, the way `sales`
asks `crm::accepts_documents`: the read model lags, and a product declared a
moment ago would otherwise be reported as not existing. Nothing new was
published from `inventory`; `PostingAccounts::resolve` was already public too.

The edge costs one thing and it is the system's own rule: `reads` is what a
route is refused on, so `/v1/purchases` now answers 503 while `proj_inventory`
is older than the build. That is named in the `Cargo.toml` beside the dependency
rather than discovered later.

**A line that names a product nobody declared is refused**
(`purchases::PurchaseError::NoSuchProduct`, with its own message in English and
Arabic). A product id is one typo away from another one, and falling back to the
account the line named would leave the stock account and the shelves disagreeing
until somebody reconciled a year of them (L6). What is **not** required is a
delivery: a bill that beats its goods is recorded, `2010` sits as a debit, and
that reads as *invoiced, not yet received* —
`an_invoice_that_arrives_before_its_goods_is_recorded_anyway` receives the goods
a week later and watches the account come back to zero. Nothing blocks entering
a supplier invoice.

**The stored line records where it posted, not what the request asked for**
(`modules/purchases/src/commands.rs:353`, `stocked`). The substitution happens
once, before the event and the entry are built from the same list, so
`proj_purchases.bill_line.account` and the journal cannot disagree about where
the money went. `entry_for_bill` is untouched: it is still summation over the
accounts it is handed. **Review found this claim unguarded** — the balances
alone cannot tell a substituted line from a substituted *entry* — so
`a_bill_line_that_names_a_stocked_product_clears_what_the_delivery_owed` now
reads the line back out of `proj_purchases` and asserts the account it stored.

**`usable_shelf` stayed, and two thirds of it are now belt-and-braces.** It asks
three things of a shelf before a delivery lands — a branch nobody opened, a name
too long to be an `AggregateId`, a currency the inventory account is not kept in.
A receipt posts now, so the first two would be refused by `post_entry_in` a
moment later **with the same error**: `NoSuchBranch` is the ledger's own, and the
receipt builds its own entry id anyway. The currency is the one that stays load-
bearing — from a posting it comes back as `MixedCurrencies`, which names neither
the shelf nor the cause, which is the finding §72 wrote down. Deleting the other
two would also delete `inventory`'s `branches` dependency and the prose around
it; it is one door, three questions, and the diff for splitting it is bigger than
the thing it removes.

**`StockValueAgrees` kept its name and changed its meaning**
(`crates/erp-worker/src/bin/worker.rs:245-283`). It no longer compares two modules'
halves: `inventory` writes `1300` at both ends, so a difference means a movement
that did not post, an entry somebody made against the stock account by hand, or a
read model behind the log. The doc says that, and says what it used to say and
why it was wrong, because the next person to read it will be reading it after an
alert.

**And its blind spot is closed.** `value_on_hand` groups `stock_item` rows, so a
tenant with a balance on the stock account and nothing on any shelf produced no
row and therefore no finding — precisely the case the finding's own text names,
*a debit that never reached a shelf*. The comparison is now
`stock_disagreements` (`worker.rs:292`), a pure function over the two sides that
adds the account's own currency when no shelf holds it, and it is **unit-tested
in the worker binary** — `a_stock_account_with_no_shelf_behind_it_is_a_finding`
and `a_shelf_the_account_cannot_be_compared_with_is_still_reported`. §72 said
this wanted a harness these invariants do not have; it wanted a pure function
instead.

**The demo buys its stock by product now.** `ap-2260`'s four lines name
`PROD-BEANS`, `PROD-MILK`, `PROD-PASTRY` and `PROD-GRINDER`
(`crates/erp-demo/src/lib.rs`), so each debits `2010` back to zero against the
deliveries that credited it. `seed_bills` moved **after** `seed_inventory`,
because a line naming a product nobody has declared is refused — the delivery
still need not exist, only the product. `the_shelf_was_counted` asserts `2010` is
zero beside its existing `1300`-against-the-shelves assertion: the café was
billed for exactly what it received, and that is one account to read rather than
a health finding to interpret.

**Review found four things; three are fixed here and the fourth is older than
this slice.**

1. **A typo'd product answered 400 and the route's own description promised
   422.** `purchase_problem` maps the rejections it names and defaults the rest
   to `BAD_REQUEST`, so `NoSuchProduct` — added a few lines above in the same
   diff — fell through the catch-all while the regenerated `docs/openapi.json`
   listed it under 422. A client that retries never on a 400 and shows a 422 as
   a business refusal put a malformed-request error in front of somebody who had
   mistyped a product id. The refusal is well formed and the *product* is what
   is missing, which is how `inventory` reads the identical error
   (`InventoryError::is_malformed`, `modules/inventory/src/commands.rs:262`), so
   the variant joined the 422 arm beside `NotRecorded` and `Ledger`
   (`modules/purchases/src/http.rs:533`). Pinned by a pure unit test on the
   mapping (`:580`) — the shape `payments` already uses for the one status a
   route test would not explain.
2. **The stored account was unguarded**, above.
3. **`product` is write-only, deliberately.** See *Left open*.
4. **A count still counts one lot** (revision R2; *built by §75*), and the prose said an
   allocation rule was one "nobody has agreed to" — which stopped being true the
   day R2 agreed to it. Nothing about counting changed in this slice; the module
   doc, the `count` operation's description and the Phase 19 box did, because
   all three were arguing from the superseded position. The box is open again.
   The same correction went to the negative-stock paragraph, which cited
   decision 7 after R1 had narrowed it to plain products.

**Falsified.** Each fix broken in Rust, the test watched to fail, the file
restored, the test watched to pass:

| broke | failed |
|---|---|
| `2010` ships as an `Asset` in the charts | `what_is_received_and_not_invoiced_is_owed`: *chart "services" makes 2010 a Asset, and goods arrived and not billed are owed for* |
| `receive` stops posting its pair | `a_delivery_debits_the_shelf_and_credits_what_is_not_yet_invoiced`: *onto the shelf — left 0, right 30000* |
| `entry_for_receipt` credits `variance` instead of the holding account | `a_closed_holding_account_refuses_the_delivery`: *a delivery with nowhere to put what it owes: ()* — it was accepted, and the goods landed with the liability unrecorded |
| `stocked` returns the lines unchanged | `a_bill_line_that_names_a_stocked_product_clears_what_the_delivery_owed`: *the bill cleared what the delivery owed — left -70000, right 0* |
| `stocked` substitutes the account on a line with no product on it | `a_line_with_no_product_on_it_is_untouched`: *rent is not stock and lands where the line said — left 0, right 150000* |
| the `accepts_movements` check dropped from `stocked` | `a_line_naming_a_product_nobody_declared_is_refused`: *got Ok(Committed … account: AggregateId("2010"), product: Some(AggregateId("PROD-BEENS")))* — a typo posted to the holding account |
| the account's own side dropped from `stock_disagreements` | `a_stock_account_with_no_shelf_behind_it_is_a_finding`: *left [], right [(0.00, Some(700.00))]* — the blind spot, reported by nothing |
| the demo's beans line stops naming its product and debits `1300` again | `the_demo_passes_every_invariant`: *the demo's books and its shelves disagree about what the stock is worth — left 277100, right 207100* — exactly the 700.00 of beans debited twice |
| **(review)** `NoSuchProduct` dropped back out of the 422 arm | `a_line_naming_a_product_nobody_declared_is_refused_on_the_state_of_the_world`: *left 400, right 422* |
| **(review)** the event keeps `draft.lines` while the entry posts the substituted ones | `a_bill_line_that_names_a_stocked_product_clears_what_the_delivery_owed`: *the stored line records where it posted — left ["5010"], right ["2010"]* — the balances all still passed, which is the blind spot |

**Left open.**

- ~~**Nothing consumes, still.**~~ **Closed by §74**: `sales::issue_in` calls
  `consume_in` for every invoice line that names a product.
- **`2010` is one account for every product**, exactly as `1300` is. Account
  determination by item group is the Phase 6 seam `sales::PostingAccounts`
  already names, and it is not this module's to open.
- **Nothing reconciles the holding account line by line.** A non-zero `2010` says
  *some* delivery is unbilled or *some* bill has no goods behind it; which one
  takes a screen nobody has asked for, and `reports` is where it would go. The
  account balance is at least a number a bookkeeper already knows how to read,
  which the health finding was not.
- **A bill line is not matched to a receipt.** Nothing says which delivery a
  line pays for: four receipts and one line for 700.00 net off against each other
  in the account balance and nowhere else. Matching needs the receipt named on
  the bill line, which is the goods-receipt document this build does not have.
- **A partial or over-billed delivery leaves a balance and nobody is told.** A
  supplier who bills 720.00 for goods received at 700.00 leaves 20.00 sitting in
  `2010` for ever. That is visible on the balance sheet and in no alert; the
  check above compares the *stock* account, and this is the other one.
- **A tenant who re-points `goods_received` mid-stream strands what is in the old
  account.** The same shape every posting-account change has (entries keep the
  accounts they were posted to, L5), and worth knowing before somebody changes it
  between a delivery and its bill.
- **`product` never comes back out.** A bill line accepts it, it decides where
  the money posts, and then `bill_line` does not store it and `BillLineView`
  does not serve it: a client POSTs `PROD-BEANS`, gets 201, and reads back
  `account: "2010"` with nothing saying which product made it so. The account
  itself is the signal, and it is a thin one. Storing the product is a column, a
  response field, a `purchases` group `VERSION` bump and a `READ_MODELS`
  re-pin — additive and cheap, and deferred because nothing has asked to read
  it. It is also what the line-by-line reconciliation above would be built on:
  no query today can list the lines that were treated as stock.
- ~~**A count counts one lot, and R2 says it should count a shelf.**~~ **Closed
  by §75**, which made `Count.lot` optional as predicted here. As it stood: §72
  closed that Phase 19 box and this slice's review re-opened it. The rule is decided —
  a shortage through `pick`'s order, an overage onto the newest lot, a
  serial-tracked count naming the serials found — and `picking.rs` already
  implements the ordering half, so this is a command that makes `Count.lot`
  optional rather than a design question. Until it ships a counter files one
  count per lot, and a serial-tracked product cannot be counted at all.
- **`purchases` is refused while `proj_inventory` rebuilds.** New, correct by the
  system's own rule, and a cost nobody has felt yet because no tenant has the
  module. §72's *Left open* said the same about `pos` and `sales`; this is the
  first edge that actually exists.

### 72 · Stock costs money: what a shelf posts, and the account it has to agree with

**Built 2026-09-13**, Phase 19 slice 2. §71 built the shelf and posted
nothing; this books everything that leaves one. A count's discrepancy, a
write-off's loss and a document's cost of goods sold are journal entries now,
each written **in the transaction that writes the movement** — so a shelf that
came down without its entry is not a state this system can reach.

**The accounts were already in all three charts, and were not the work.** §71
put `1300 Inventory` and `5010 Cost of goods sold` into `services` and
`real_estate` beside the ones retail already carried, with retail's codes and
names in both languages (`modules/ledger/src/charts.rs:151`, `:343`, `:544`).
Decision 4 was closed before this slice opened. What was missing was anything
that posted to them.

**Four accounts, and two of them default to the same code.** `PostingAccounts`
(`modules/inventory/src/posting.rs:86`) is `inventory`, `cogs`, `variance` and
`waste`. The last two are separate fields because they are two numbers a
manager acts on differently: a write-off is somebody standing in front of the
goods saying *this is spoiled*, a cost of doing business the buyer controls; a
count variance is stock that left without anybody saying so, which is a control
failure and a different meeting. Netting them makes both unreadable, which is
`pos`'s argument for keeping `5910` out of `5900`.

They default to the same `5900` anyway, and that is not a contradiction. The
shipped charts have one general loss account between them, and adding a second
code to charts every tenant installs would be a guess about businesses nobody
has asked — a guess that fails on the first posting, which is exactly what
`the_conventional_accounts_exist_in_every_shipped_chart` catches. So the *seam*
is two and the *default* is one: a tenant who wants spoilage apart from
shrinkage splits them in one `PUT` with no code change, and a tenant who does
not gets one number that is still correct. The HTTP test does that split
(`crates/erp-api/tests/http.rs`, the `posting-accounts` block) rather than
leaving the claim to a comment.

**This renames a stored configuration.** `shrinkage` became `variance` and
`waste`, so a tenant who had stored the slice-1 shape gets a `ConfigError` from
`resolve` rather than a silent default — which is the behaviour every module
already has and the right one (L6). Nobody has that shape: §71 shipped the same
day and no tenant has the module.

**Everything posts through `ledger::post_entry_in`, inside the movement's own
transaction**, and the accounts are read inside it too, with the configuration
generation stamped on the metadata (`commands.rs:1235`, the shape
`sales::resolve_accounts` has). Reading them outside would be marginally
cheaper and would let an account be closed between the check and the append.
`write_off` (`commands.rs:560`) and `count` (`commands.rs:1052`) grew the
`begin`/`try_execute`/`post`/`settle` loop `pos::close_shift` has, because a
transaction spanning this module's event and the ledger's cannot leave the
retry inside either.

**A count that found exactly what the books said posts nothing.** Not a pair of
zero lines and not a refusal: `two_sided` returns `None` on zero
(`posting.rs:242`) before `BalancedLines` is built, because the ledger refuses a
zero line and a count that balanced is not an error. Filtered before the lines
exist, as `sales::entry_for_issue` filters its tax line.

**The consumption path is written and nothing calls it.** *(Superseded by §74:
`sales::issue_in` calls it for every line that names a product. What follows
describes the build as it stood here.)* `consume_in`
(`commands.rs:661`) takes the caller's connection rather than a `TenantDb`,
because the sale that will call it owns the transaction its invoice commits in
— `pos` composes `sales::issue_in` the same way. **No route in this build
reaches it.** It is said in the module doc, in `posting.rs`'s header, in
`schema/install.sql` beside the `consumed` kind and the `sold` serial state, and
in the function's own doc, because a module whose `consume_in` exists reads as a
module that depletes on a sale. It does not. The cost of goods sold a margin
needs is still zero for every line rung.

It was written now because the posting is the hard half and it is cheaper to
test against a shelf than against a shelf and an invoice —
`a_consumption_posts_exactly_what_the_lots_it_took_cost`
(`modules/inventory/tests/inventory.rs:1489`) takes six hundred grams across two
roasts at two prices and asserts 38.00, naming the 36.67 an average would have
charged. That is the whole argument for per-lot costing, in one entry.

**The COGS entry is named by a published function** and the other two are not.
`cost_entry_of` (`commands.rs:1198`) is public for the reason
`sales::issue_entry_of` is: a consumption belongs to a *document*, so a report
asked *which postings did this invoice line make* has to name the entry without
reimplementing the prefix. A write-off and a count belong to nothing outside
this module. All three are derived from the shelf and the movement's own
reference and never minted (L8), and a shelf and reference that will not fit in
one `AggregateId` are **refused** rather than truncated — two movements sharing
a truncated name would share an entry and the second would post nothing.

**A shortfall is still refused, on both paths.** *(Superseded by §74: a plain
product's sale records it (R1) and a count, not a delivery, settles it; a
write-off still refuses it.)* Decision 16 says a sale may
take a shelf below zero at the last known unit cost; recording one means
deciding what the next receipt does to the units the shelf owes, and nobody has
decided. `taken` (`commands.rs:1521`) is the one place both ways out refuse it,
so that decision has one place to land. Guessing now would be writing the answer
down before the question was asked — §71's own *Left open*, unchanged.

**The check decision 8 makes load-bearing.** *(Superseded by §73 the same day:
receiving posts now, and the bill clears a holding account. What follows
describes the build as it stood here.)* Receiving does not post: the supplier's
bill debits `1300` on the line that bought the goods and this module only ever
credits it. So the asset account is written by two modules that cannot see each
other, and nothing but a comparison says the two halves still agree.
`inventory::value_on_hand` (`modules/inventory/src/projections.rs:1098`) is this
module's half; `StockValueAgrees` (`crates/erp-worker/src/bin/worker.rs:283`) is
the other, and it lives in the composition root for the reason `TrialBalance`
and `ReportsReconcile` do — the comparison needs `proj_ledger` beside
`proj_inventory`, and L3 forbids a module from reading across projection groups.
It resolves the *tenant's* inventory account rather than the conventional code,
so a business that pointed stock at `1310` is not reported broken for ever.

**The demo buys its own stock.** A bill from a coffee supplier
(`crates/erp-demo/src/lib.rs:1252`) debits `1300` line for line with what
arrives in `stock_arrives`; the crate of milk, the damaged grinder and the three
missing croissants credit it. `the_shelf_was_counted` asserted `1300` was zero
and now asserts it equals what the shelves are worth
(`crates/erp-demo/tests/demo.rs:958`), and `the_demo_passes_every_invariant`
makes the same comparison, because its own first line claims to be every
invariant the platform checks.

**A posting is dated to a branch, and that changed a test.** Stock is per branch,
so every movement carries one, and `ledger::post_entry_in` checks it against the
log — so a write-off at a branch nobody opened is now refused. The HTTP test
opens `BRANCH-OLAYA` through `/v1/branches` before receiving into it. That is
the right answer and it is new: in §71 nothing inventory did reached the ledger.
Review then found the other half of it, below: a *receipt* has to ask the same
question, and `branches` is a dependency of the module rather than of its tests.

**No new route.** The `GET`/`PUT /v1/inventory/posting-accounts` pair §71
shipped grew two fields; the permissions table and its count are untouched.

**Falsified.** Each fix broken in Rust, the test watched to fail, the file
restored, the test watched to pass:

| broke | failed |
|---|---|
| `entry_for_write_off` debits the count's account | `a_write_off_and_a_count_can_land_in_different_accounts`: *left 0, right 1200* |
| the zero filter removed from `two_sided` | `nothing_that_moved_nothing_posts`: `balances: ZeroLine { index: 0 }` — the ledger refuses the pair of zeroes the filter exists to avoid |
| `waste` defaulted to `5920` | `the_conventional_accounts_exist_in_every_shipped_chart`: *chart "services" has no account 5920* |
| `count` stops posting its variance | `a_count_books_what_it_found_and_nothing_when_it_found_nothing`: *the loss is booked — left 0, right 900* |
| `cost_of` sums only the first portion | `a_consumption_posts_exactly_what_the_lots_it_took_cost`: *left 3000, right 3800* — one roast's cost for a movement that took two |
| `post` swallows the ledger's refusal | `a_closed_account_refuses_the_movement_rather_than_posting_elsewhere`: the write-off committed against a closed account |
| `entry_for_write_off` credits `cogs` instead of `inventory` | `the_value_on_hand_agrees_with_the_inventory_account`: *left 6400, right 5600* |
| the demo's bill buys beans as an expense, not an asset | `every_module_is_enabled_and_answering`: *left 137100, right 207100* — exactly the 700.00 of beans that never reached the asset |
| the branch check dropped from `usable_shelf` | `a_shelf_no_movement_could_ever_leave_takes_no_delivery`: *a delivery to a branch that was never opened: ()* — it was accepted |
| the account-currency check dropped from `usable_shelf` | same test: *a delivery priced in a currency the books do not keep: ()* |
| the `entry_id` check dropped from `usable_shelf` | same test: *a delivery onto a shelf no entry could be named for: ()* |
| `NotAReference` carries an empty shelf | same test: *the refusal has to name the shelf that overflowed, not only the key: NotAReference { shelf: "", reference: "rcv-3" }* |

**What review found, and what changed.** Five findings, two roots. Four of them
are one root — **a receipt posts nothing, so it accepted shelves that nothing
could ever post off** — and it is the shape §71's review had too: a name or a
guard that was supposed to identify one thing and covered less than it looked
like it did.

- **Stock could land where it could never leave.** Three ways. A branch nobody
  opened: `erp-web` deliberately does not validate `X-Branch`
  (`crates/erp-web/src/extract.rs:783-785`) because *"`ledger::post_entry_in`
  refuses one that names no open branch, and every posting in the system arrives
  there"* — and a receipt is the one movement that arrives nowhere, so
  `WAREHOUSE-2` took a delivery and every write-off, count and consumption at it
  was then refused for ever. A currency the inventory account is not kept in:
  `receive` compared the delivery against the *shelf*, so the first USD delivery
  onto a fresh shelf under a SAR chart was accepted and `post_entry_in` refused
  every line off it afterwards with `MixedCurrencies`, which names neither the
  shelf nor the cause. And a branch id long enough that `iw.{shelf}.{reference}`
  cannot be an `AggregateId` at all: receipts succeeded because `lot_of` returns
  a plain `String` with no length rule, while every exit was refused before its
  loop opened. All three are the same mistake and they are fixed in one place —
  `usable_shelf` (`modules/inventory/src/commands.rs:1719`), asked by `receive`
  because a receipt is the only door onto a shelf. The currency question is
  `ledger::posting_currency` (`modules/ledger/src/commands.rs:535`), new and
  public for the reason `accepts_postings` is: a module told the account's
  currency before it accepts a document can refuse the document, rather than
  leaving the refusal to a posting days later. The branch answer is
  `ledger`'s own `NoSuchBranch` rather than a message of this module's: a check
  that answers differently from the command it guards is worse than no check.
  `branches` moved from a dev-dependency to a dependency, the arrow pointing the
  way `ledger`'s does, and `setup()` reads it.
- **And the refusal blamed the wrong half.** `inventory.not_a_reference` read
  *"{reference} is too long … use a shorter key"* when a branch id can eat the
  whole budget on its own and the key may already be one character.
  `NotAReference` now carries the shelf **and** the reference and the message
  names both in English and Arabic (`messages.rs:374-387`). `consume_in` builds its
  entry through the same `entry_id` as the other two rather than its own
  `AggregateId::new` (`modules/inventory/src/commands.rs:677`), so the name a caller can predict from
  `cost_entry_of` is the name that posts.
- **The doc on `setup()` said `ledger` was checked and not posted to.** *"which
  is the one thing this module does with `ledger` today"* — true when §71 shipped
  and false the moment this slice did, while the `Cargo.toml` beside it had
  already been corrected. Rewritten (`modules/inventory/src/lib.rs:256-260`): every movement out books an
  entry in the transaction that writes it, so a closed account or a closed period
  refuses the stock movement itself.
- **Two route descriptions still said the count and the write-off post
  nothing**, and they are the half of the module a caller reads rather than
  compiles: `count_stock` ended *"Posts nothing yet. Booking the discrepancy is
  the next slice"* and `write_off_stock` *"the ledger has not been told"*, both
  shipped in `docs/openapi.json`. The module doc, `posting.rs` and the command
  docs had all been rewritten; these two had not, and a client integrating
  against the published document would have built around a ledger that is
  written. Both now say what posts and in which transaction
  (`modules/inventory/src/http.rs:484`, `:557`), their `422` descriptions name
  the ledger's refusal beside the module's, and `just openapi` regenerated the
  document — `the_document_matches_the_router` is what would have caught the
  regeneration being skipped, not the staleness itself.
- **`StockValueAgrees` treated the receive-to-invoice window as a violation**
  — *the finding §73 closed, and the reason it exists*;
  and it is the ordinary state of a business between a delivery arriving and the
  supplier's invoice being entered — decision 8 puts the debit on the bill, so
  the two differ for those days by exactly what has been received and not billed,
  and `HealthJob` logs `invariant violated` at error level every five minutes for
  the whole window. Closing it properly takes a goods-received-not-invoiced
  account that the bill relieves, which is a chart decision this build has not
  taken and `purchases` would have to post; it is **not** taken here. What
  changed is that the check stops implying otherwise: the finding names the
  window as one of its three causes and the doc says the check is expected to
  fire during it (`crates/erp-worker/src/bin/worker.rs:261-270`). The alert that
  fires on the normal case is the one that gets muted, so this is on the product
  owner's desk rather than closed.
- **And it hid a currency it could not compare.** The check filtered out any
  shelf whose currency was not the account's, so a tenant holding stock the books
  had no room for produced no finding at all. The filter is gone: a shelf in a
  second currency is stock somebody has to be told about. Untested, and said out
  loud — the invariant needs a tenant database and the worker binary has no
  harness for one, and the state now takes two `PUT`s and two receipts to reach
  rather than one mistake.

**Left open.**

- ~~**Nothing consumes.**~~ **Closed by §74**: the hook is in `sales::issue_in`,
  so a till sale, a booking bill and a `/v1/sales` invoice all deplete through
  one path, with the products in a fixed order (sorted by id — decision 14).
- ~~**Negative stock, still.**~~ **Closed by §74** for a plain product's sale: it
  records the shortfall and a count settles it, not a delivery. As it stood here:
  both ways out refused a shortfall. The sale that
  records one decides what the next receipt does to the units the shelf owes: is
  the delivery costed against the debt, or does a count settle it? The second
  answer did not work then, because a count counted one lot and a shortfall has
  no lot — which is §71's first *Left open* becoming load-bearing rather than
  tidy. *(§75 built the count of the shelf that does it.)*
- **A write-off's reason does not choose an account.** Expired and damaged both
  land in `waste`; the reason is on the movement and in the entry's memo. A
  tenant who wants spoilage and breakage as separate expense lines needs a third
  field, and nobody has asked for one.
- **No route reads an entry's memo**, so `a_write_off_books_its_loss_and_keeps_its_reason`
  asserts the account and the movement's reason rather than the memo it posted
  under. A reader for `proj_ledger.posting.memo` is the ledger's to add.
- ~~**The invariant compares rows, so an empty shelf is not compared.**~~
  **Closed by §73**, which drove the comparison from both sides through a pure
  `stock_disagreements` and unit-tested it in the worker binary.
- **The invariant is a finding, not a report.** `StockValueAgrees` says a tenant
  is unhealthy and names the two numbers. Which delivery is missing a bill, or
  which bill pointed at the wrong account, takes a reconciliation screen nobody
  has asked for — `reports` is where it would go.
- ~~**A consumption of a serial marks it `sold`** and nothing can reach that
  state yet.~~ **Reachable since §74**, from an invoice line that names its
  serials.
- **`1300` is one account for every product.** Account determination by item
  group — beans to one asset, equipment to another — is the Phase 6 seam
  `sales::PostingAccounts` already names, and it is not this module's to open.
- ~~**Goods received and not yet invoiced have nowhere to sit.**~~ **Closed by
  §73 the same day.** The product owner took it (decision R3): `2010 Goods
  received, not invoiced` ships in all three charts, a receipt credits it and a
  bill line naming a stocked product debits it back.
- **Closing a branch strands the stock on its shelves.** A receipt now refuses a
  branch nobody opened, but a branch closed *after* the stock arrived refuses
  every write-off and count at it — `post_entry_in`'s rule, which `sales` and
  `pos` live under too. Reopening clears it. What a business actually wants is a
  transfer between branches, and no verb moves stock from one shelf to another.
- **A count that finds exactly what the books said still posts nothing**, so it
  is the one movement that does not reach the ledger and the one that a closed
  branch or account does not refuse. Harmless — it records that somebody counted
  — and the asymmetry is worth knowing about before a report is built on it.
  *(Since §75, only on a shelf that owes nothing: a count of a shelf that owes
  clears the debt, and may post.)*

### 71 · The shelf exists: `modules/inventory`, lots, and stock that moves by hand

**Built 2026-09-13**, Phase 19 slice 1. The first of the phase's four boxes and
none of the other three: products, lots and movements are events now; **nothing
posted to the ledger and nothing depleted on a sale.** Both were said out loud —
in the module doc, in the file a reader opens expecting entries
(`modules/inventory/src/posting.rs`), and in the route descriptions — because a
module that looks finished and is not is how a tenant discovers a gap at an
audit. §72 closed the first half the same day and §73 the rest of it — receiving posts
too now — and §74 closed the second: an invoice line that names a product
depletes its shelf.

**Costing is per lot, and that replaced a weighted average that was already
written.** A first version of this module carried two integers per shelf and
drew every movement at `value ÷ on hand`; the product owner's revision added
lots, expiry and serials, and a weighted average cannot answer *which delivery
is this*, which is the whole of expiry and the whole of a recall. The two
integers went; `OpenLot` (`modules/inventory/src/picking.rs:57`) came.

**Every receipt is a lot, whatever the tracking mode.** A product declares how
closely it is watched — `none | lot | serial`, frozen at declaration beside the
unit (`modules/inventory/src/product.rs:39-70`) — and the mode decides what a
delivery must *say*, not whether lots exist. An untracked product's lots are
FIFO layers nobody names, which is what lets one costing method serve all three
and stops a second one being needed the day somebody wants batches.

**The picking rule is one pure function** (`modules/inventory/src/picking.rs:225`)
and everything that leaves a shelf leaves through it. Dated lots before undated,
earlier date first, received order among equals — so a pharmacy's stock goes out
in the order it will spoil and a hardware shop's goes out in the order it
arrived, **down the same path**. The sort is three lines
(`picking.rs:249-253`): `(expires_on.is_none(), expires_on)`, stable over a
received-order list. Naming a lot overrides it, and a named lot that cannot
cover what was asked is **refused** rather than topped up from the next — naming
one is a claim about that lot, and the caller is holding the goods.

**A portion costs what its own lot cost.** `value × units ÷ quantity` through
`Money::apportioned` (`picking.rs:81`), which is exact at `n/n`, so the movement
that empties a lot takes whatever is left and the lot closes on exactly zero
rather than stranding a halala. Pinned by
`the_last_portion_of_a_lot_takes_the_remainder` (`picking.rs:483`) and by
`each_portion_costs_what_its_own_lot_cost` (`modules/inventory/src/stock.rs:812`),
which is the test that would have passed under the old design and now names the
number an average would have charged.

**Open lots only live in the aggregate** (`stock.rs:442-443`). A lot that empties
leaves; a café receiving beans every morning for three years carries the four it
can still pour from. The window of references already heard is bounded the same
way `conversations::Thread` bounds its. The read model keeps the history: `lot`
holds every delivery ever made, closed or not
(`modules/inventory/schema/install.sql:54`).

**A serial is an identity and is never invented.** It comes from the caller at
receipt (L8), one per unit; a movement names the units it takes; and a serial
that is unknown, already gone, or named twice in one movement is **refused**
(`picking.rs:312-339`). A *delivery* giving two units one name is refused too, and
separately — the serials are counted distinct in `delivery`
(`modules/inventory/src/commands.rs:1407-1429`), before the shelf is loaded,
because a name repeated inside the incoming list is a fact about the request and
no shelf can see it. Review found that gap; see below. That is the one place
this module refuses for stock, and
the reason is written down where the split is made
(`modules/inventory/src/picking.rs:314-317`): decision 7's *"a sale never
refuses for stock"* governs quantities, and a count corrects a quantity. Nothing
corrects a unit that was never on the shelf. *(R1 later narrowed decision 7 to
plain products: a lot- or serial-tracked sale the shelf cannot cover refuses
too. See §74.)*

**A count counts one lot, and a serial-tracked product is not counted at all.**
The smaller honest thing, and the argument was in `count`'s rustdoc until
§75 rewrote it: a shelf-wide variance has to be put
somewhere, and putting it somewhere is an allocation rule nobody has agreed to —
three kilos missing off four deliveries did not go missing evenly, and charging
them to the batch the picking rule would take next is a guess dressed as
arithmetic. A lot is the smallest thing a person can stand in front of and
count; `GET /v1/inventory/lots` is what they read first. For a serial-tracked
product a quantity that came up one short cannot say *which* name is gone, so
the count is refused and the correction is a write-off that names the unit.
`expected`, `declared`, the variance and what the variance was worth are frozen
into the event (`stock.rs:207-255`), exactly as `pos::ShiftEvent::Closed` freezes
the drawer's.

**Corrected 2026-09-13, later: the rule was agreed.** Revision R2 says a count
takes a shelf and the shortage comes off the lots in picking order — so "nobody
has agreed to one" was true when this was written and stopped being true that
afternoon. The code is unchanged and the Phase 19 box is open again; §73's review
carries it. *(Superseded by §75: a count takes the shelf, a serial-tracked product
is counted by naming what was found, and `SerialsAreNotCounted` is gone.)*

**A write-off exists and, in this slice, did not post** — §72 books its loss.
A reason — `expired` or `damaged`, not
free text, because a reason nobody can group by is a reason nobody reads
(`stock.rs:71-83`) — and more than the shelf holds is **refused**
(`commands.rs:1521`). This is somebody holding the goods, not a till: a till may
not stop for a bad count, and a person looking at a shelf may be told the shelf
says otherwise. `Shortfall` (`picking.rs:150`) is computed and carries the last
known unit cost, and the only caller in this build refuses it — so it is a
reason to refuse and nothing else. **No event carried one** *(§74 gave `Consumed` one, for a plain product's sale)*. The first draft put
the field on `WrittenOff` "so the sale slice does not reshape the event"; review
showed the claim was hollow in both directions, and it is gone (`stock.rs:195-206`).
See below.

**A shelf is per branch**, keyed `{product}.{branch}` from the request's own
metadata (`stock.rs:714`), with `parts` (`stock.rs:727`) taking the halves back
out so no event repeats what its key already says. The seam is the first `.`,
unambiguous because a product's id is its `Idempotency-Key` and therefore a
UUID — and a product id carrying a dot is refused there rather than guessed at.

**And a lot id carries its shelf too**: `lot.{shelf}.{reference}`
(`commands.rs:1128`). An idempotency key is only promised to be unique to the
client that sent it, and `lot.id` is a primary key across the whole tenant —
review found the two facts meeting. See below.

**Twelve operations across nine paths under `/v1/inventory`**, taking `PERMISSIONS`
(`crates/erp-api/tests/http.rs:2156`) from 247 to 259. Reading the shelf, the
lots and the movements is every role's. Receiving, counting and **writing off**
are `PostEntries` — the person who finds the milk past its date is the person
holding it, and a loss nobody may record is a loss that goes in the bin
unrecorded. Declaring a product is the owner's, because the unit and the
tracking mode are frozen at declaration.

**The lots listing is the order stock goes out in**, and that is not decorative.
`lots` (`modules/inventory/src/projections.rs:914`) sorts by
`COALESCE(expires_on, '9999-12-31'), position` — the sentinel is how *undated
last* survives a keyset page, and `position` is the receipt's log position and
therefore the received order. Which is why a count **does not** bump a lot's
`position` (`projections.rs:492`, `schema/install.sql:83-88`): a screen that
reordered itself after a stocktake would stop matching the rule it exists to
preview. The index is built on the same expression (`install.sql:102-104`).
`expiring_before` is **exclusive** (`projections.rs:946`) — a batch dated the day
asked about is still good that day — which is what the parameter's own name says
and is not what it did.

**`1300 Inventory` and `5010 Cost of goods sold` are in all three shipped charts
now** (`modules/ledger/src/charts.rs:151`, `:250`, `:343`, `:433`, `:544`,
`:676`). They were in `retail` alone, and the demo installs `services`. The
precedent is `2300 Zakat payable`, which ships everywhere for a calculation
nothing performs. `PostingAccounts::conventional()`
(`modules/inventory/src/posting.rs:127`) names them plus `5900 Other expenses` for
shrinkage — **not** `5910`, which is the drawer's. The chart test
(`posting.rs:286`) is worth having before anything posts, because the first
posting is otherwise where a missing account is discovered, by the tenant.

**Both settings ship with writers, and in this slice nothing read either** —
§72's postings read the accounts.
`purchases` and `payments` each ship a `PostingAccounts` with no route, which
strands a tenant who closes one of the accounts it names. This one has
`GET`/`PUT /v1/inventory/posting-accounts`, ETag-guarded, `ManageAccounts` for
the write the way `sales` argues it, and each code is checked with
`ledger::accepts_postings` against the **log** before it is stored
(`modules/inventory/src/http.rs:898`). The expiry warning window
(`modules/inventory/src/expiry.rs`) is the same bet a slice earlier: a tenant who
has chosen their window before the first warning arrives never sees a wrong one.
`GET /v1/inventory/lots?expiring_before=` takes a day per request and
deliberately does **not** fall back to the setting — a listing that hid lots by
default is a listing nobody could trust, and the check that reads the window is
not built *(§76 built it)*.

**One event can be several movement rows.** A write-off off two lots is two
portions costed on their own lots, and flattening them would lose exactly the
fact this module exists to keep — so `seq` completes the movement key
(`install.sql:188-245`) and the projection writes one row per portion
(`projections.rs:252-342`). Each row is a **signed delta**, never a running
total, because a running total is a read and a projection may not read while
applying (L2). Which makes the canary a property rather than a reconciliation:
**what is on hand is the sum of the movements that produced it, lot by lot** —
asserted against a real tenant
(`modules/inventory/tests/inventory.rs:505`, projected *and* rebuilt), over HTTP
(`crates/erp-api/tests/http.rs:15354`) and in the demo
(`crates/erp-demo/tests/demo.rs:1022`).

**Cross-module existence reads the log.** `accepts_movements`
(`commands.rs:1151`) loads the `Product` aggregate rather than reading
`proj_inventory.product`, three lines, the way `crm::accepts_documents` is
written — and it is public because the invoice slice has to ask the same
question the same way. `tracking_of` (`commands.rs:1160`) asks on its own
connection rather than inside the movement's transaction, and says why: a
product is declared once and nothing un-declares one, so the answer is
monotonic and a check that passed cannot stop being true while the movement
commits.

**Every registration place.** `REGISTERED` (`crates/erp-api/src/modules.rs:135`,
after `ledger`), the catalogue (`crates/erp-api/src/catalog.rs:50`),
`module_jobs` (`crates/erp-worker/src/bin/worker.rs:2188`), the migrator's
`rebuild` arm (`crates/erp-worker/src/bin/migrator.rs:543`), `READ_MODELS`
(`:810`), `REBUILDABLE` (`:982`), the demo's seed and projection run
(`crates/erp-demo/src/lib.rs:283`, `:394`, `:1882`) and its replay witness
(`crates/erp-demo/tests/demo.rs:452`), plus the two source-scan allowlists
(`crates/erp-eventlog/tests/write_side.rs:56`,
`crates/erp-control/tests/pooler.rs:42`). The demo seeds one product of each
tracking mode — two roasts of beans, a crate of milk that went off and was
thrown out, a tray of pastry counted three short, and two grinders of which one
arrived damaged and left **by name** — because the three modes are three
products and a demo with only the middle one leaves expiry and identity with
nothing behind them.

**Re-pinned at version 1** (`migrator.rs:810`). The module has not shipped, so no
tenant has `proj_inventory` to rebuild and a shape change costs one line. After
it ships the same edit is a fleet rebuild, which is why every table the next
slices need is already in `install.sql` — including the `consumed` kind, the
`sold` serial state, and the nullable `lot` a shortfall would leave empty. The
`serial` key gained the shelf under review, and re-pinning it was the same one
line (`migrator.rs:810`).

**What review found, and what changed.** Five findings, four roots. Every one of
them is a name that was supposed to identify one thing and did not, or a doc
sentence the code did not back.

- **A delivery could give two units one serial.** `delivery` checked only that
  the count of names matched the quantity, and `receive`'s shelf guard looks for
  names *already on the shelf* — neither can see a repeat inside the incoming
  list. `["SN-1", "SN-1"]` for two units was accepted; writing `SN-1` off then
  took **both** copies off the lot (`Stock::draw_down` retains by name) while
  taking one off the quantity, leaving a unit with no name that nothing can ever
  move: not by name, not by quantity — a serial product refuses a quantity
  write-off — and not by counting, which a serial product refuses outright. The
  fix is one line where the shape of a delivery is already decided: count the
  names **distinct**, trimmed the way `label` trims (`commands.rs:1407-1429`). The
  refusal is the one that already existed and already reads right — *two units,
  one named* — so no new message code, and it is a 400 because it is a fact
  about the request.
- **A lot id was derived from the idempotency key alone.** `lot.{reference}`,
  while `lot.id` is `PRIMARY KEY` across the tenant. A key is unique only to the
  client that sent it; one that keys a retry loop on a batch rather than on a
  row sends the same key for two rows, and two shelves each hear it for the
  first time and both record a delivery. The read model's `ON CONFLICT (id) DO
  NOTHING` then dropped the second lot, and the next write-off on it decremented
  the **first** one — the canary, silently, on the other product. The shelf is
  now in the id (`commands.rs:1128`), which is what tells the two apart; within
  one shelf the reference already did. `lot_of` takes the pair the way
  `sales::credit_entry_of` does.
- **The `serial` row was first-write-wins, and tenant-wide.** `ON CONFLICT
  (product, serial) DO NOTHING`, but `receive` refuses only a serial the shelf is
  **holding** — so a machine that comes back from repair is a legitimate second
  delivery, and the screen went on saying `written_off` on a closed lot while the
  log said the unit was on hand in a new one. The insert now updates
  (`projections.rs:180-199`): a receipt is the newest fact about a named unit,
  and a replay re-applies events in order so the last one still wins. The key
  gained the shelf (`schema/install.sql:147`) for the same reason `Stock` is per
  branch — two aggregates, neither able to see the other's serials, so a
  read-model uniqueness across the tenant would have been claiming something no
  command enforces — and `gone` is scoped to the stream to match
  (`projections.rs:724`), so Olaya throwing a machine away no longer takes
  Malaz's off the screen.
- **`WrittenOff::shortfall` was a field nothing could write.** The write side's
  `apply` ignored it, the projection subtracted it, and when its cost was `None`
  with no portions beside it the projection dropped the **whole event** — no
  movement row to explain a number that had moved. It is deleted, not repaired
  (`stock.rs:195-206`, `projections.rs:265-278`). The doc said it was there "so the
  sale slice does not reshape the event", and that was never load-bearing: an
  `Option` behind `#[serde(default)]` costs nothing to add later, and the slice
  that adds it has to decide what a later receipt does to units the shelf owes —
  a decision nobody has taken. Writing the field now was writing the answer down
  before the question was asked.
- **`expiring_before` filtered inclusively.** `expires_on <= $4` under a name and
  a description that both say *before*. Harmless on a screen, and a day early on
  every dated lot in the tenant the moment the expiry worker reads the same
  function. Now `<` (`projections.rs:946`), with the boundary in the test.

The longer id and the new `serial` key ripple exactly three places and each one
failed loudly rather than quietly: the HTTP walk-through asserts the lot it was
given (`crates/erp-api/tests/http.rs:15189`), the demo names the lot it counts
(`crates/erp-demo/src/lib.rs:2031`), and the read-model pin is the same one line
it always was (`migrator.rs:810`, version still 1 — the module has not shipped,
so nothing rebuilds).

**Falsified.** Each break, the failure it produced, then the file restored and
the test watched to pass again.

| broke | failed |
|---|---|
| `earliest_first` stops sorting | `the_earliest_expiry_goes_out_first`: *left `[l1, l2, l3]`, right `[l2, l3, l1]`* — received order, not expiry order |
| undated lots sorted by plain `Option` ordering | `an_undated_lot_waits_and_then_goes_oldest_first`: the undated lots came out before the dated one |
| a named lot no longer capped at what it holds | `naming_a_lot_overrides_the_order`: *left `Ok(Picked { … quantity: 11 … })`, right `Err(LotIsShort { held: 10, wanted: 11 })`* |
| a portion costs the whole lot (`apportioned(1, 1)`) | `the_last_portion_of_a_lot_takes_the_remainder`: *left 10000, right 3333* |
| the shortfall arm removed | `what_the_lots_cannot_cover_is_a_shortfall`: *left None, right `Some(Shortfall { quantity: 3, cost: Some(75.00) })`* |
| the repeated-serial check removed | `a_serial_is_found_or_refused`: `["SN-1", "SN-1"]` took two units off a lot holding one of them |
| an emptied lot stays in the aggregate | `a_depleted_lot_closes_and_the_rest_stays`: *left 2 lots, right 1* |
| the already-held serial check removed from `receive` | `a_serial_that_is_not_on_the_shelf_is_refused`: a second unit was received under a name already on the shelf |
| the `NotEnoughStock` guard removed from `write_off` | `a_write_off_takes_no_more_than_is_there`: 2,500 came off a shelf of 2,000 |
| a serial-tracked product allowed to be counted | the same test: the count committed instead of being refused |
| `count`'s already-heard check removed | `a_movement_is_recorded_once_however_often_it_is_sent`: *left 4 movements, right 3* |
| `shelf_of` ignores the request's branch | `a_shelf_belongs_to_a_branch`: Olaya's shelf held nothing |
| a lot-tracked delivery no longer needs its batch code | `a_delivery_has_to_look_like_the_product_it_is_of`: the codeless crate committed |
| the count event freezes `declared` as `expected` | `what_a_count_found_is_frozen`: *left `Some(4800)`, right `Some(5000)`* |
| a count values its variance at the whole lot | the same test: *left 30000, right -1200* |
| the projection records a count's `declared` instead of its variance | `a_quantity_on_hand_agrees_with_the_movements`: *left 58, right 40* |
| a write-off stops drawing its lot's remainder down | the same test: *the emptied batch should have closed, left 3 right 2* |
| a count bumps its lot's `position` | `what_a_count_found_is_frozen`: *left `["lot.rcv-2", "lot.rcv-1"]`, right `["lot.rcv-1", "lot.rcv-2"]`* — the listing stopped matching the picking order |
| the lot id stops being derived from the receipt | `stock_goes_out_of_the_lot_that_expires_first`: two deliveries landed on one lot |
| `1300` taken out of the `services` chart | `the_conventional_accounts_exist_in_every_shipped_chart`: named the chart and the code |
| the Arabic for `serials_are_not_counted` replaced by a second English entry | `every_crates_messages_render_through_the_composite` |
| a shape change to `install.sql` without a re-pin | `a_read_model_change_bumps_its_version`, with the tuple to paste |
| `module_jobs` entry removed | `every_module_has_a_projection_job`: *a module is offered and never projected. Missing: `["inventory"]`* — and nothing else in the workspace noticed |
| `REBUILDABLE` entry removed | `every_module_can_be_rebuilt` |
| `write_side.rs` allowlist entry removed | `an_aggregate_is_loaded_only_while_handling_a_command` |
| `pooler.rs` allowlist entry removed | `no_session_scoped_set_outside_a_ddl_path` |
| a `fetch_optional` added inside `apply` | `a_projection_does_not_read_while_applying` |
| the demo's replay witness removed | `the_demo_replays_to_exactly_what_is_live`: *a projection group is not covered by shadow replay* |
| the demo's `advance::<Inventory>` removed | `every_module_is_enabled_and_answering`: *inventory has its products, left 0 right 4* |
| `declare_product` relaxed to `ALL_ROLES` in the table | `every_role_against_every_endpoint`: the accountant's 403 named `manage_tenant` |
| `list_lots` taken out of the table | the same test: *served and untabled: `["list_lots"]`* |

And the five review fixes, the same way:

| broke | failed |
|---|---|
| `delivery` counts the serials it was sent instead of the distinct ones | `a_delivery_has_to_look_like_the_product_it_is_of`: `["SN-1", "SN-1"]` for two units committed instead of refusing `NeedsSerials { units: 2, named: 1 }` |
| `lot_of` ignores the shelf and derives from the reference alone | `a_shelf_belongs_to_a_branch`: *left `[("lot.rcv-1", 5000)]`, right `[("lot.rcv-1", 5000), ("lot.rcv-1", 1000)]`* — one key at two branches made one lot, and the read model kept the first |
| the `serial` insert back to `DO NOTHING` | `a_named_unit_that_comes_back_is_on_hand_again`: the lot the machine came back onto listed no serials — *left `[…("…OLAYA.rcv-3", [])]`, right `[…("…OLAYA.rcv-3", ["SN-1"])]`* |
| `gone` matches on the product alone, not on the shelf | the same test: *left `[(Some("BRANCH-MALAZ"), 1, [])]`, right `[(Some("BRANCH-MALAZ"), 1, ["SN-1"])]`* — Olaya's write-off took Malaz's machine off the screen |
| `expiring_before` back to `expires_on <= $4` | `stock_goes_out_of_the_lot_that_expires_first`: *left 1, right 0* — the batch that goes off **on** the day asked about came back as already gone |

Deleting `WrittenOff::shortfall` has no guard test and cannot have one: nothing
could write the field, so no behaviour changed and the compiler is the check.
What it bought is that the three docs describing it are now true.

**Left open.**

- **Nothing posts.** ~~A write-off's loss and a count's discrepancy are the next
  slice.~~ **Closed by §72 the same day**: both post now, in the transaction
  that writes the movement, and the demo asserts `1300 Inventory` against what
  the shelves are worth rather than against zero. **And the receipt by §73**,
  which is the one this section said would never post: it debits the asset and
  credits `2010 Goods received, not invoiced`, and the supplier's bill clears
  that account instead of the asset.
- ~~**Nothing consumes.**~~ **Half closed by §72** — `consume_in` picks lot by
  lot and posts cost of goods sold — **and closed by §74**, which calls it from
  `sales::issue_in` for every line that names a product.
- ~~**Negative stock is unreachable, and no event can record it.**~~ **Closed by
  §74** for a plain product's sale, which records the shortfall on `Consumed`;
  a count settles it, not a delivery. As it stood here: decision 7 says
  a sale never refuses for stock and `pick` already computes what the lots
  cannot cover — but the only movement out in this build is a write-off, which is
  refused rather than taken below zero, so `Shortfall` is a reason to refuse and
  nothing else. The slice that records one adds the field (or its own variant)
  once it has decided what the next receipt does to the units the shelf owes: is
  the delivery costed against the debt, or does the count settle it? Nobody has
  chosen, and the earlier draft's field was that choice made silently.
- ~~**A shelf-wide count.**~~ **Closed by §75.** Counting *all the milk* without
  saying which batch needs an allocation rule for the variance, and nobody had
  agreed to one. *(Revision R2 agreed to one later the same day — picking order
  for a shortage, the newest lot for an overage — and §75 built it.)*
- ~~**Nothing reads the expiry window.**~~ **Closed by §76**: the worker's
  `stock_expiry` check reads it, reports lots past their date and lots inside
  the window, and changes nothing — and §77 turned that report into a
  notification for whoever may write stock off. As it stood here: the setting and its route
  shipped, and the check that warns did not exist.
- **A lot-tracked delivery may leave the expiry empty.** The code is required
  and the date is not, because a batch with no shelf life is a real thing and
  the picking rule already has a rule for undated lots. If a tenant wants dates
  compulsory that is a per-product flag, and nobody has asked.
- **A count is not segregated.** `inventory:approve_count` beside
  `sales:approve_credit_note` is the auditor's answer for a tenant who wants the
  duties split; it costs a claim, a check in the root and a two-step flow.
- **Two lots of one product cannot be merged or transferred**, and neither can
  stock between branches. Both are additive over the three events here.
- **A serial is unique to its shelf, not to the tenant.** Two branches are two
  `Stock` aggregates and neither can see the other's names, so the same number
  may be on hand at Olaya and at Malaz — and the read model is keyed to say so
  rather than to claim otherwise. Refusing it tenant-wide means a second stream
  read inside the decision (L3) or an index nothing writes; the honest version is
  a transfer command, and nobody has asked for one.
- **A reference repeated on one shelf after it has fallen out of the heard
  window** still lands on one lot id. The window is 200 movements and the repeat
  is a client bug; what it costs is a lot row the projection keeps rather than
  replaces. Narrowing it further means an id that no caller could derive, which
  is what `lot_of` exists to avoid.
- **Rebuilding `inventory` takes the counter down**, now that `sales` reads it
  (§74): the group is in the read-model closure of `sales` and `pos`, so a stale
  `proj_inventory` answers 503 on their routes. Correct by design.
- ~~**Two tills on one basket can deadlock.**~~ **Closed by §74**: `deplete`
  takes the movements in a fixed order, sorted by product id, after
  `sales::issue_in` has reserved the numbering counter.
- ~~**No book page.**~~ **Closed by §76**, with `module.md`'s correction. As it
  stood here: `docs/book/src/api/inventory.md`, `SUMMARY.md` and `modules.md`
  were the paperwork slice's, and `module.md` said the registry carried four
  modules and cited a retired `docs/ERRORS.md`.

### 70 · One rule everywhere for the credit-note claim

**Built 2026-09-12**, Round 3c decision. §68's own *Left open* said it: a till
return did not ask for `sales:approve_credit_note`. The claim was checked in
`may_credit`, and `may_credit` was called by `cancel_invoice` and
`credit_invoice_part` — the two wrappers the `/v1/sales` routes use.
`pos::take_back` calls the roots directly (`credit_in` → `cancel_in`, and
`credit_part_in`), so a clerk refused a credit note on the sales screen could
hand the same money back at the counter and get the same document out of it.
The product owner's call is **one rule everywhere**, and the change at the till
is deliberate.

**The check moved into the roots.** `may_credit` (`sales/src/commands.rs:69`) is
asked at the top of `cancel_in` (`:1366`) and `credit_part_in` (`:1736`) — the
two functions every credit note in this system passes through — and both wrapper
calls are gone, so there is one enforcement point and not two. Each root asks it
beside §68's `limit::binding`, on the root's own connection and so inside the
caller's transaction: a till return is judged in the same write that hands the
money back. The *refusal* sits further in, beside the limit's own comparison —
see **What review found** below.

**The branch is §68's, exactly**: `Authority::Member { owner: false }`, the same
`let … else` that opens `limit::binding` (`limit.rs:241`). Two consequences,
both deliberate:

- **`Authority::System` is not claim-judged**, the way it is not limit-judged. A
  gateway's confirmed refund (`payments::refund_in` → `credit_what_is_clear`), a
  worker's sweep and a customer's own deposit have nobody to ask, and by then
  the money has moved.
- **The owner is exempt**, as they are exempt from the document limit and from
  `hr::may`'s own reading of the handle. The exemption comes off `Authority` now
  rather than off `erp_tenant::Access`, because a root has no handle — and it is
  the same bit: `Authority::of` (`limit.rs:78`) reads `db.role() ==
  Some(Role::Owner)`, which is the field `hr::may` compared. `may_credit`
  therefore passes `None` for `access`, and nothing else in `hr::may` consults
  it.

**A tenant that has granted nothing still works, and that is the load-bearing
part.** `hr::may` answers *permitted* when `any_claim_placed` is false, and that
first answer is what makes the control opt-in: every till that worked yesterday
works today, in every tenant that has never granted a claim — which is nearly
all of them. It is also why this asks `hr::may` and not §68's `hr::actor_holds`:
`actor_holds` reads "nobody has granted anything" as "not held", which is right
for a control the *owner* switches on by setting a limit and exactly wrong for
one a *grant* switches on. Swapping them refuses every existing tenant's till
(see *Falsified*). A tenant selling with no `hr` at all never reaches the read
model either, because `any_claim_placed` reads `org_claim_granted` in the
tenant's own migration chain and stops there.

**What changed for whom.**

| Path | Before | Now |
|---|---|---|
| `POST /v1/sales/invoices/{invoice}/credit-note`, `…/credit-notes` | claim asked | unchanged |
| `POST /v1/pos/shifts/{shift}/sales/{sale}/returns` | **not asked** | asked |
| `POST /v1/sales/invoices/{invoice}/refunds` that clears the invoice | **not asked** | asked |
| `POST /v1/payments/{payment}/refunds` that will leave a credit note owing | **not asked** | asked, of the member, when they ask |
| a gateway's confirmed refund, a worker's sweep, a customer's own deposit | not asked | unchanged |
| the owner, anywhere | not asked | unchanged |
| a tenant that has granted no claim | not asked | unchanged |

The third row is a second door, and it was not in the decision's words.
`refund_invoice` calls `credit_what_is_clear` with the member's authority, so a
refund that leaves an invoice holding nothing issues a whole-invoice credit note
— which now needs the claim that credit note needs. It is the till's hole one
function over, and closing one without the other would have left the rule in two
pieces again. §52's *not guarded, deliberately* was about the **function**
`credit_what_is_clear`; it is about the **authority** now, which is what makes
it one rule rather than a list of exempt functions.

**The claim does not travel.** `sales:approve_credit_note` is on `hr::SEGREGATED`
(`hr/src/claims.rs:78`), so `grant` refuses to propagate it whatever the screen
asks: the boss above a supervisor who holds it does not hold it, and the clerk
beneath never would. That is the opposite of §68's
`sales:exceed_document_limit`, which is not segregated and does travel up. The
two claims read alike and behave differently, so the till test pins both
directions.

**Status and message are unchanged.** `SalesError::NotApproved`,
`sales.not_approved` in en and ar, and 403 through
`SalesError::refuses_the_caller` (`commands.rs:261`), which the till's
`problem_for` already consulted — so the counter answers exactly what the sales
screen answers, with no new mapping. The till's and the refunds route's
`FORBIDDEN` descriptions gained the code, so `just openapi` ran.

**Tests**, through the product path.

- `a_till_return_needs_the_credit_note_claim` (`modules/pos/tests/pos.rs:1206`).
  Three sales rung at one till. With no `hr` and no grants, the clerk's return
  goes through — the opt-in case, and the one that would break every existing
  tenant. Then an org chart, boss ← supervisor ← clerk, and the claim granted to
  the supervisor: the clerk's return is refused, and `sales::cancel_invoice`
  refuses the same clerk the same way in the same test, which is the item in one
  assertion. The boss above the holder is refused too, the supervisor is not,
  and once the clerk is granted it their return goes through. The drawer is
  level at the end: three sales, three returns, and the refusals moved nothing.
- `crediting_an_invoice_needs_the_claim_once_the_tenant_uses_claims`
  (`modules/sales/tests/sales.rs:4817`) gained the three cases this branch
  needs: an owner is never refused, a `System` credit note is not judged at all,
  and a member's refund that clears an invoice is refused for the claim. Its
  first case — a tenant that has granted nothing — was already there and is now
  the second opt-in guard.
- `a_partial_credit_needs_the_claim_too` (`:4922`) is unchanged except that both
  calls say `MEMBER` where they said `System`. They said `System` because the
  check used to read the *handle* and ignore the authority; under one rule
  `System` is not judged, so the old spelling would have made the test prove
  nothing.
- `the_credit_note_claim_refuses_the_caller_at_the_till_too`
  (`modules/pos/src/http.rs:931`), beside the document limit's.
- `a_gateway_refund_is_asked_for_the_claim_when_it_will_credit`
  (`modules/sales/tests/sales.rs:4983`), from review. Five invoices paid in
  full, then `may_refund` asked the way `payments::request_refund_in` asks it:
  before any grant the member is permitted; after one they are refused both for
  a refund that clears the invoice and for a 23-riyal part of it (20 net at
  15%, so a partial credit note lands exactly); the claim's holder, the owner
  and `System` are not refused. A 10-riyal refund of the same invoice is **not**
  asked, because no net comes to 10 at 15% and so no credit note follows —
  which is what keeps this a control on issuing a document rather than on
  handing money back.
- `a_retry_answers_with_its_credit_note_after_the_claim_is_revoked` (`:5055`),
  from review. Khalid cancels one invoice and partly credits another while he
  holds the claim; it is then revoked and granted to Sara, so the tenant still
  uses claims and the control stays on. A *new* credit note is refused him, and
  both retries — the same references — answer with the documents they issued,
  `did_nothing()` and the same numbers.

`pos` gained `hr` as a **dev**-dependency for the chart the test grants on. Not
a dependency: `pos` composes `sales`, and `sales` owns the edge to `hr`.

#### What review found — two doors, one root

Review put two things to this section, and they are the same mistake twice: the
claim was given half the shape of the limit it now sits beside. §68's limit is
three things — an async *question* (`limit::binding`), a sync *comparison*
applied inside the decision and after the retry check, and a *pre-judgement*
where a member asks for something a gateway will later write as `System`
(`may_refund`, `may_issue`). The claim had the question and nothing else.

**A member could still refund their way to a credit note.**
`payments::request_refund_in` (`payments/src/commands.rs:711`) judged the member
with `sales::may_refund`, which asked the limit and not the claim; the document
itself is then written by `payments::refund_in` → `credit_what_is_clear` with
`Authority::System`, which no claim judges. So the member this section refuses
at the credit-note route and at the till could ask a gateway for a refund and
get the same credit note out of it. The asymmetry was the tell — the same
pre-judgement already enforced the *limit* on that member.

`may_refund` (`sales/src/commands.rs:783`) now asks `may_credit` beside the
limit (`:791`) and refuses on the credit note the refund will leave owing
(`:813`). **Whole *or* part.** Review proposed the whole-invoice arm alone,
since that is the arm the limit judges separately; but a partial refund of a
single-band invoice issues a *partial* credit note through `credit_part_in`,
also with `System`. The limit can skip that arm — the money that went back is
exactly what the note credits, and that was judged a line earlier — while a
claim about whether you may issue a credit note at all cannot. A refund that
leaves no credit note owing (`Owed::Nothing`, or a multi-band invoice's
`Owed::Overstated`) is not asked for one, and that is pinned rather than
assumed. The invoice is loaded only when there is something to judge, so a
tenant with no limit whose member holds the claim reads nothing it did not
read before.

**And a retry answered 403 where the book promises a no-op.** The claim refused
at the top of each root, before `try_execute`, while both retry arms live
*inside* the decision — `state.cancelled_by == Some(reference)` and
`state.has_credit(&reference)`. So a till return whose response was lost and
which the client resent after the claim was revoked was refused, though
`reference` is documented as the client's key for that return and sending it
again as a no-op (`docs/book/src/api/http.md`), and §68 put the limit's
comparison after the retry check for precisely this reason (`limit.rs`,
`binding`). The 403 is not the worst of it: a clerk told *not approved* rings the
return again under a new reference, and **that** one is a second credit note.

So `may_credit` (`:69`) answers rather than refuses — `Result<bool, …>`, the
shape `limit::binding` has, for the same reason: the question is async and a
decision is not. Each root applies the answer inside its decision, after its
retry arm (`:1396`, `:1766`). There is still one function that decides; what
moved is where its answer is spent. Somebody who never held the claim is refused
exactly as before, which two of the five falsifications below are the old guards
re-run to prove.

**Falsified.** Each break, the failure it produced, then the file restored byte
for byte (checked with `md5sum`) and the tests watched to pass again.

| broke | failed |
|---|---|
| `cancel_in`'s `may_credit` removed | `a_till_return…`: the clerk's return went through; `crediting_an_invoice…`: *sara does not hold the claim, got Ok(Numbered { … CN-00002 … })*. `a_partial_credit…` still passed, which is what says the two roots are guarded separately |
| `credit_part_in`'s `may_credit` removed | `a_partial_credit…`: *a partial credit is still a credit note, got Ok(…)*, and the other two still passed |
| `hr::may` swapped for §68's `hr::actor_holds` | `a_till_return…`: *the door that opened yesterday opens today: NotApproved*; `crediting_an_invoice…`: *no claims in this tenant means no control: NotApproved* — the opt-in gone, and with it every existing tenant's till |
| the owner no longer exempt (`owner: _`) | `crediting_an_invoice…`: *an owner is never refused their own credit note: NotApproved* |
| `System` claim-judged too | `crediting_an_invoice…`: *a credit note nobody issued is not claim-judged: NotApproved*. `a_members_refund_is_judged…` **passed** under this break and is not a witness: its tenant grants no claim, so `hr::may` permits everything in it |
| review: `may_refund`'s refusal removed (`commands.rs:813`) | `a_gateway_refund…`: *clearing the invoice issues a whole credit note: Ok(())* — the member every other door refuses walks through this one |
| review: `cancel_in` refuses before the retry check again (the placement this section shipped) | `a_retry_answers…`: the retry of a credit note Khalid *was* allowed to issue answered `NotApproved` instead of its own number |
| review: `credit_part_in` refuses before the retry check again | `a_retry_answers…`: the same, one assertion further on, for the partial credit note |

**Left open.**

- ~~**A gateway refund that clears an invoice still issues its credit note
  unasked.**~~ Found by review and closed below: `may_refund` asks the claim
  beside the limit it already judged there, for the whole-invoice credit note
  *and* the partial one.
- **`purchases.not_approved` and `hr.not_approved` still answer 400.** §68's
  item, still true: only sales moved to 403.
- **A contended credit note asks twice.** The question sat before the retry loop
  and now sits inside it, so every optimistic-concurrency attempt asks again.
  One indexed lookup per attempt, and the answer cannot change between them.
  A refused caller also reserves a credit-note number before being refused,
  because the refusal now sits after the reservation; the transaction rolls it
  back, so no number is spent and the series stays gapless.
- **A member with no employee record may credit nothing** once the tenant uses
  claims. That is `hr::may`'s rule rather than this item's, but the till is where
  it will be felt: a shop may well run its counter on logins nobody has put on
  the org chart, and the first grant anywhere turns the control on for all of
  them.

### 69 · Resetting somebody else's second factor, and the link that is the only way back

**Built 2026-09-12**, Round 3b. §67 closed the hole where a member could drop a
factor their company required, and its Left-open bullet named the cost: somebody
who loses the authenticator *and* all ten recovery codes is locked out, and
nobody — not their owner, not support — could undo it. This is the undoing, and
almost all of its design is about the state it leaves behind, because taking a
factor away hands the account to whoever enrols next.

**Two routes, one root.** `POST /v1/members/{identity}/second-factor-reset`
(`erp-api/src/members.rs:253`) is the company's; `POST
/v1/platform/identities/{identity}/second-factor-reset` (`platform.rs:488`) is
platform support's, with a required reason. Both reach
`ControlPlane::reset_second_factor_by` (`second_factor.rs:745`), which is
private and is the only thing that removes somebody else's factor. In one
transaction it deletes `totp`, `totp_pending` and `recovery`, stamps
`identity.second_factor_reset_at`, writes a hashed link row, deletes every
session of the person, and enqueues the email (D9, as
`request_password_reset` does); the other nodes' session caches are cleared
after the commit, the way `log_out` and `confirm_second_factor` do it. The audit
entry is `second_factor.reset` under the resetter, on the tenant for the
company's route and on no tenant, carrying the reason, for support's.

**This is not `disable_second_factor` and shares no line with it.** That one is
a person dropping their *own* factor, costs a code, and is refused outright
wherever a factor is required of them (§67). This one takes no code, because the
person it exists for has nothing left to prove with — and it does not ask
`second_factor_required_by`, because a member of a tenant that requires a factor
is exactly who needs it. They are refused entry until they enrol again, which is
the same answer a new member gets. **No new way to remove your own appears:**
`by == target` is refused at *both* routes (`:639`, `:697`), so a staff member
cannot reach through support's route for the removal `auth.staff_keeps_second_factor`
denies them.

**Link-only, and it is a fact about the account rather than a timer.** This is
the part worth the migration. After a reset, enrolment refuses unless the caller
presents a live link — and **that holds after the link expires**, because if it
lapsed with the link, the move for somebody holding a stolen password would be
to wait an hour. So `0021_second_factor_reset.sql` adds two things, not one:
`second_factor_reset` is the *permission* (token digest, an hour, `used_at`,
swept like a password reset), and `identity.second_factor_reset_at` is the
*state*, cleared by a confirmed enrolment and by nothing else. Sweeping an
expired link therefore takes nothing away — it means asking for another.
`ControlPlane::enrolment_permitted` (`:556`) is the one check, and both
`begin_second_factor` and `confirm_second_factor` ask it first.
`confirm_second_factor` clears the column and spends **every** outstanding link
of the identity in its own transaction (`:342`): two resets in a row leave two
live links, and enrolling once must not leave the second one able to enrol
again. An account that never had a factor has the column NULL and is untouched —
first enrolment by whoever holds the password stays the accepted gap (decision B).

**A fresh link is the same route run again**, not a second one. It removes a
factor that is already gone, ends sessions that are already ended, mails another
link and records another entry. There was nothing for a second route to do
differently, and one more route is one more thing to authorise. Griefing is
bounded by who may call it at all — the owner, a claim holder, or support — and
every call is on the record under their name; the cross-tenant rule below stops
one company aiming it at somebody who mostly works elsewhere.

**Who may, at the company's route.** The owner always, or a member holding
`hr:reset_second_factor` (`hr/src/claims.rs:96`) — and **never an API key**
(`members.rs:267`), which is review's and is the sharp edge of the door being
`Allowed<Read>`. The extractor has to be `Read`, because the claim lives in the
tenant's own database and `erp-web` is below it; but the key-scope gate asks for
*the door's* capability (`extract.rs:913`), so a credential issued `*:read` with
the owner's role cleared a gate that answers `keys.out_of_scope` at every
sibling member route, and the role its machine identity holds was all the
handler then asked for. It is refused with `keys.not_a_person`, the answer the
personal audit trail already gave a key, through one shared
`erp_web::not_a_person` (`extract.rs:1007`). A wider scope is not the way in
either: `claimant` finds no employee record for a machine, so no key can hold
the claim.

The real check, for a person, is the handler's next line: not the owner and not
holding the claim is `403 access.not_permitted` naming `manage_tenant`, through
a now-public `erp_web::not_permitted` (`extract.rs:987`) so there is one shape
for "you may not" in the API. The `PERMISSIONS` row is therefore `OWNER`, and it
is honest: in a tenant that has granted nothing, the owner is the only one who
gets through. The claim is asked with `hr::actor_holds` (`members.rs:317`), not
`hr::may`, for the reason §68 gives — "nobody has granted anything" must read as
*not held*, or a tenant that uses no claims would let every clerk reset every
colleague. It is not in `hr::SEGREGATED`, so it travels **up** the chart: a
grant to a supervisor reaches the boss above them and not the clerk beneath,
which the test pins in both directions.

**Four targets the company's route refuses**, each with its own code in en and
ar, checked in an order that says nothing about anybody the caller cannot
already list: not a live member here is the same `404 members.not_a_member`
`remove_member` gives, and only then yourself
(`second_factor.reset_yourself`), the owner (`…reset_the_owner`), platform staff
(`…reset_platform_staff`) and — the cross-tenant rule — anybody with a live
membership of another tenant (`…reset_another_company`). That last one exists
because a factor is the *account's* everywhere, not one company's: acme may not
weaken somebody's sign-in at globex. Its message says to contact support, and
the test asserts the word is in it. A fifth, `second_factor.reset_no_login`,
refuses an account with no password login at all — there would be nowhere to
send the link, and a reset with no link is a lockout with extra steps (L6).

**The platform power.** `PlatformPower::ResetSecondFactors` (`staff.rs:63`),
support and superadmin, with `erp_web::ResetSecondFactors` as its marker
(`extract.rs:1139`). **Resetting a staff account needs `ManageStaff` on top**
(`second_factor.rs:705`), so support cannot reset a superadmin's factor —
otherwise the narrower role would be the route to the wider one. The reason is
1–500 characters, refused blank (`second_factor.reset_reason`), and is the only
record anybody will ever have of why a person's sign-in was weakened.

**No `attempts` column, and the migration says why.** `password_reset` has one
because the link gates six digits, and twenty bits is where guessing goes.
Nothing is gated behind this link: presenting it *is* the permission, and the
code that confirms the enrolment comes from the app its holder is enrolling. A
column nothing increments would be a claim the code does not make. Single use,
an hour, and the fact that only three kinds of caller can cause a row are what
bound it instead.

**`POST /v1/sessions/second-factor` gained an optional body, and nothing broke.**
It had none, so a required one would have turned every existing caller's request
into a 415. `erp_web::Json` now implements `OptionalFromRequest` (`wire.rs:67`):
**an empty body is `None`**, with or without a `Content-Type`, because a client
that always sets the header still sends no bytes when it has nothing to say. A
body that is *there* goes through `Json`'s own rules, so the wrong content type
is still 415 and bad JSON still 400 — reading either as "nothing was sent" is
how an ignored field becomes a hole. `confirm_second_factor`'s body already
existed, so its `link` is purely additive.

**The matrix:** `reset_member_second_factor`, OWNER, which makes 247 tenant
operations; `reset_any_second_factor` with `reset_second_factors`, which makes
11 platform operations. `STAFF_POWERS` gained the power for superadmin and
support, and `PlatformPower::ALL` is six.

**Tests.** Through the product path, in `crates/erp-api/tests/http.rs`.

- `an_owner_resets_a_members_factor_and_the_link_is_the_only_way_back`
  (`:3154`). acme requires a factor. A clerk's session dies with the reset, the
  password still signs in, `GET /v1/sessions/second-factor` shows nothing
  enrolled and no codes left, and the tenant answers
  `403 auth.tenant_requires_second_factor` until they enrol. Both enrolment
  calls are `403 auth.enrolment_link_required` on the password alone — the
  confirmation too, holding a valid code. The link enrols, hands back ten fresh
  recovery codes, and a replacement afterwards needs no link, which is the state
  having been cleared. A second reset mints a second link; the first is dead.
  The tenant's trail names the owner and the clerk, twice.
- `waiting_out_an_enrolment_link_does_not_reopen_password_only_enrolment`
  (`:3374`). The link is wound back an hour — the way
  `an_expired_session_is_swept` winds one — and `sweep_enrolment_links` deletes
  it. Enrolment is still refused, with the stale token and without one. Running
  the reset route again is what sends a working link.
- `the_claim_lets_somebody_other_than_the_owner_reset_a_factor` (`:3449`). Boss
  ← supervisor ← clerk on a real org chart. With nothing granted, all three are
  refused. Granted to the supervisor, the supervisor and the boss may and the
  clerk still may not; a member with no employee record never can. The owner's
  factor and your own are both refused, and a stranger is a 404. An API key is
  refused before any of that (review's): `*:read` reaches the handler and gets
  `keys.not_a_person`, `*:manage_tenant` does not even clear the scope gate, and
  the target's session is still alive afterwards.
- `somebody_who_works_for_two_companies_is_platform_supports_to_reset`
  (`:3657`). A clerk in acme and globex is `422 second_factor.reset_another_company`
  at acme, and the message says support. Support's route refuses no reason and a
  blank one, resets them, and the platform trail carries the reason under
  support's name with no tenant. Support is refused a superadmin
  (`403 access.not_permitted` naming `manage_staff`) and a superadmin is not;
  nobody resets their own; an account with no login is refused; and acme cannot
  reach a staff member of its own through its route.

**What review found, and what changed.** Three findings, two of them real.

1. **A read-only API key could reset anybody's factor.** The serious one, and
   the price of lowering the door to make room for the claim: a key issued
   `*:read` with the owner's role — what a tenant hands a reporting vendor —
   deleted a colleague's `totp` and ended every session they held, while the
   same key got `keys.out_of_scope` on `DELETE /v1/members/{identity}`. Refused
   now rather than scoped differently, for the reason above; the audit entry
   that would have named a machine goes with it.
2. **A database fault stated a fact about somebody's account.**
   `password_handle`'s error was discarded into `ResetError::NoLogin`, so a
   dropped connection answered `422 second_factor.reset_no_login` — *"that
   account has no email login, nothing was changed"* — about a colleague who
   plainly signs in by email, and the caller who believed it would not retry.
   `no_address` (`second_factor.rs:139`) matches the variant: only a missing
   password row is `NoLogin`, and a fault is a 500 (L6).
3. **Not a bug: the "`hr` disabled, so `actor_holds` 500s" reading.** It cannot
   happen — the Left-open bullet below says why — so no code changed. The prose
   that invited the reading did: `actor_holds` "asks the grants before the read
   model" was doing the work of a guarantee it does not give, and both
   `members.rs:313` and the paragraph above now say the thing that is actually
   true, which is that a tenant with a grant has had `hr` on and a disabled
   module keeps its read models.

**Falsified.** For each row I broke the fix, watched the named tests fail,
restored the file byte for byte and watched them pass. The breaks inside
`query!` text were built against the type-check database (`SQLX_OFFLINE=false`).

| broke | failed |
|---|---|
| `enrolment_permitted` always `Ok` | `an_owner_resets…`: `201` where `403 auth.enrolment_link_required` was due at `begin`; `waiting_out…`: *expiring the link reopened enrolment* |
| the link not spent on a confirmed enrolment | `an_owner_resets…`: *a spent link enrolled again*, 201 |
| `second_factor_reset_at` not cleared by a confirmation | `an_owner_resets…`: *the link-only state outlived the enrolment that was supposed to clear it*, 403 |
| the reset's session `DELETE` neutered | `an_owner_resets…`: *the reset left a session alive*, 403 where 401 was due |
| the reset's authenticator `DELETE` neutered | `an_owner_resets…`: the login still demanded a code, 401 |
| the cross-tenant check removed | `somebody_who_works…`: `204` where `422 second_factor.reset_another_company` was due |
| the owner check removed | `the_claim_lets…`: `204` where `…reset_the_owner` was due |
| `by == target` removed, tenant route | `the_claim_lets…`: *the supervisor reset their own* |
| `by == target` removed, platform route | `somebody_who_works…`: `204` where `…reset_yourself` was due |
| the platform-staff check removed, tenant route | `somebody_who_works…`: `204` where `…reset_platform_staff` was due |
| `staff_may(ManageStaff)` removed from the platform route | `somebody_who_works…`: support reset the superadmin, `204` |
| the no-login check removed | `somebody_who_works…`: an account with no address was reset, `204` |
| the reason check removed | `somebody_who_works…`: a blank reason accepted, `204` |
| the claim check bypassed | `the_claim_lets…`: *boss reset a factor with no claim* |
| the route asking for `hr:approve_timesheet` instead | `the_claim_lets…`: *supervisor was refused*, 403 |
| `support` given `ManageStaff` | `somebody_who_works…`: support reset the superadmin |
| `billing` given `ResetSecondFactors` | `every_role_may_exactly_what_it_should`: *billing / reset_second_factors*; `every_platform_role_against_every_platform_endpoint`: billing reached the route |
| the key refusal deleted (review) | `the_claim_lets…`: *a key scoped `["*:read"]` reset a colleague's factor* — `204` and no body, where `403 keys.not_a_person` was due |
| `no_address` collapsed back to `NoLogin` for every error (review) | `a_database_fault_is_not_an_account_with_nowhere_to_mail`: *a database fault was reported as an account with no email login* |

**Docs.** RUNNING gained *When somebody loses the phone and the paper*, with
both routes and the four refusals; its platform-trail paragraph said the trail
does **not** record second factors enrolled or removed, which is now only true
of the person's own, so it says so and names `second_factor.reset`. The book's
`http.md` gained both routes in their tables and a section under *The second
factor*; `erp-control.md` gained the two entry points, the root, the link-only
column and the widened power matrix; `erp-web.md` gained the `ResetSecondFactors`
marker; `erp-worker.md` and RUNNING's reaper paragraph name the new sweep.
`just openapi` regenerated the document for two new routes, the new `link`
fields and the `403` on both enrolment calls. §67's Left-open bullet about the
lockout is struck through and points here.

**Left open.**

- **A person in several companies whose mailbox is also gone is still stuck.**
  Support's route mails the link to the account's own address, which is the only
  address this system has. If that address is what they lost, nothing here
  reaches them; somebody has to change the login handle first, and no route
  does that. This is the honest limit of a mailbox-based recovery.
- **If the mail never arrives, nothing says so.** A dead letter shows in
  `GET /v1/platform/effects/dead` under `enrolment:`, and support can requeue
  it — but the *resetter* gets a `204` either way and learns nothing. The
  person's factor is already gone by then, so a failed send leaves them worse
  off than before they asked. Telling the caller the send is only promised, or
  surfacing undelivered enrolment mail to the tenant, is not built.
- **A reset is not undoable.** Nothing restores the old factor or the ten
  recovery codes — they are deleted, not archived — so a mistaken or malicious
  reset costs the person an enrolment they must redo, and the audit entry is
  the whole of the remedy.
- **Nothing rate-limits the route.** An owner or a claim holder can reset the
  same colleague every minute, ending their sessions each time, and the only
  cost is a row in the trail. A per-target cooldown like
  `RESET_INTERVAL_SECONDS` would bound it. Nobody asked, and the caller is
  already somebody the tenant trusts with its books.
- **The reset and the enrolment are two acts, and the account is exposed
  between them.** From the moment of the reset the account is password-only in
  every sense that matters *except* enrolment, so a tenant that does not require
  a factor lets whoever holds the password sign in. That was already true of
  anybody without a factor; this makes it true of somebody who had one.
- **The audit entry is written after the transaction commits**, as
  `move_staff`'s is, so a crash in between leaves the reset done and unrecorded.
  §61 named this seam and it is unchanged.
- **`identity.second_factor_reset_at` survives a restore of an older dump.** A
  control-plane restore to before a reset would bring back the old `totp` row
  and clear the state with it, which is right; a restore to *after* one, with a
  tenant database from before, changes nothing here. Neither is tested.
- **A missing `proj_hr` would 500 this route, and nothing in the product makes
  one.** This bullet used to say that disabling `hr` after granting the claim
  would leave `actor_holds` reading a read model that is not there. Review
  checked it and it is wrong: `disable_module` (`erp-control/src/lib.rs:2242`)
  marks the entitlement and **never drops a module's tables**, so that a tenant
  who downgrades and comes back finds their data; `proj_hr` is created by
  `hr::install`; a grant is only ever written through a route that needs `hr`
  on; and the one `DROP SCHEMA` of a live read model is inside `rebuild_swap`'s
  transaction, which renames the replacement in before it commits. So the 500 is
  unreachable and no code changed. It stops being unreachable the day something
  learns to drop a read model — a storage reclaim, a rebuild that half-fails —
  and then both callers of `actor_holds`, this route and §68's limit, answer
  500. Neither is tested, because neither has a way in.

### 68 · How large a document a member may issue, and the claim that lifts it

**Built 2026-09-12**, Round 3 decision C. §63 left a bookkeeper limited to ten
thousand riyals free to issue a fifty-thousand invoice, because a permission
limit is judged at the edge and an invoice has no total there. The decision is a
second, separate control: a per-document limit on every invoice, credit note
and refund a member issues, anywhere a member issues one, with the owner always
exempt and an `hr` claim as the allow list.

**The setting** is `sales.document_limit`, one typed configuration key owned by
`sales`: `DocumentLimit { limit: Money, basis: before_vat | after_vat }`
(`modules/sales/src/limit.rs`). Absent, or stored as `null`, is no limit.
`DocumentLimit::new` refuses an amount that is not positive, and
`#[serde(try_from)]` makes reading a stored row check the same way, so a row a
later build cannot use is a 500 on the `GET` and a refusal on every member's
document (L6), never "no limit". The routes are `GET`/`PUT
/v1/sales/document-limit` in `sales`' own `http.rs` (`:1518`, `:1573`), beside
its posting accounts, copied from the calendar: the version as `ETag`,
`If-Match` on the write, `set_by` on the row. Both take `Allowed<ManageTenant>`,
as decision 9 did for permission limits. The wire shape is `{ "limit": { "amount":
{minor, currency}, "basis" } | null }`, so a basis cannot be sent without an
amount. `erp_web::Amount` gained `Serialize` so the `GET` answers in the shape
the `PUT` takes. Every write stamps the whole configuration version into the
document's metadata as before (L5), so which limit was in force when a document
was judged stays answerable.

**Who is asking is an argument with no default.** `sales::Authority` is
`Member { owner: bool }` or `System` (`limit.rs:62`). Every root a document
passes through takes one: `issue_in`, `cancel_in` (behind `cancel_invoice` and
`credit_in`), `credit_part_in`, `refund_in` and `credit_what_is_clear`, and so
does every public wrapper over them, `pos::sell`, `pos::take_back`,
`erp_api::billing::bill_reservation` and `payments::request_refund_in`. A new
path does not compile until it says. `Authority::of(db)` (`:78`) is how a route
says it, and it can never answer `System`: a handle with nobody behind it is a
member who is not the owner, so the worst a mistaken caller gets is a refusal.

**Every path, and what it passes.** I traced every caller of the five roots.

| Path | Caller | Authority |
|---|---|---|
| `POST /v1/sales/invoices`, `…/refunds`, `…/credit-note`, `…/credit-notes` | member | `Authority::of` |
| `POST /v1/pos/shifts/{shift}/sales`, `…/returns` | member at the till | `Authority::of` |
| `POST /v1/booking/reservations/{r}/invoice` | the desk | `Authority::of` |
| `bill_completions`, the worker's pass (`billing.rs:232`) | nobody; the owner turned billing on completion on | `System` |
| `settle_in` → `bill_the_deposit` (`payments/src/commands.rs:371`) | the gateway settling a customer's own deposit | `System` |
| `payments::refund_in` → `sales::refund_in`, `credit_what_is_clear` (`:672`, `:903`) | the worker, recording what the gateway already refunded | `System` |
| `POST /v1/payments/{payment}/refunds` → `request_refund_in` | member | `Authority::of`, judged by `sales::may_refund` (`:769`) |
| `POST /v1/payments`, `…/cards/{card}/charges` with a deposit → `start_in`, `request_in` | member | `Authority::of`, judged by `sales::may_issue` (after review) |
| the customer's own deposit (`erp-api/src/deposits.rs:379`), the three gateway sweeps | nobody; what they start was judged when it was asked for | `System` |

A gateway refund needed the fourth row. The member asks, the worker tells the
gateway, and `payments::refund_in` records what the gateway confirms, by which
time the money has gone and refusing to record it would only make the books
wrong. So the member is judged when they ask. `sales::may_refund` (`commands.rs:780`)
applies the rule `refund_in` does, and the one `credit_what_is_clear` does, and `request_refund_in` calls it only when
the request is new, after it is recorded, so a retry answers the way the first
one did. A refusal returns an error and the route's transaction takes the
request back out. `PaymentsError::Refused(SalesError)` carries it as `sales`
said it, instead of the `Sales(String)` that flattens every other sales failure
into a 400 with English inside.

**Where it is judged.** `limit::binding` (`:236`) answers the limit this caller
is held to: `None` for `System`, for the owner, when nothing is set, or when
`hr::actor_holds` says the caller holds `sales:exceed_document_limit`. It runs
in the command's transaction before the decision, because a claim lookup is a
query and a decision closure is not async. The comparison, `DocumentLimit::judge`
(`:182`), runs inside the decision after the totals and after the retry check:

- `issue_in` (`commands.rs:496`) on the invoice's own totals, after its
  discounts and after any deposit is deducted, which is what the document
  charges.
- `cancel_in` (`:1400`) on the whole invoice, which is what a cancellation
  credits. It sits after the `HasPayments` check on purpose: an invoice still
  holding money is refused for that, not for its size.
- `credit_part_in` (`:1773`) on the credit note's own totals.
- `refund_in` (`:1060`) on what goes back. After VAT that is the money itself.
  Before VAT it is the invoice's own proportion, `refunded × net ÷ gross`, with
  `Money::apportioned`, the way `payments::retain_in` splits a kept deposit.
  Refunding a whole invoice is therefore judged on exactly the totals issuing it
  was.

Because the comparison follows the retry check, a retry of a document issued
before the limit was lowered answers with its number rather than a refusal.
Equal to the limit is within it.

A refund is judged twice when it issues a credit note: once as money, once as
the document. That is what stops a till return being split across two tenders
under the limit to carry an 11,500 credit note, and what refuses a 5,000 refund
that clears a part-paid 23,000 invoice, since clearing it issues a
whole-invoice credit note — at `/v1/sales` because `credit_what_is_clear` runs
with the member, and through a gateway because `may_refund` judges the same
document when the member asks. `refund_in`'s own check is not redundant: a
partial refund of a multi-band invoice issues no credit note at all (§44), and
the first version of the refund test passed with that check removed, because
each case it had also issued one. It now refunds part of a two-band invoice.

**Another currency is refused.** `judge` counts an amount it cannot compare as
over, the three-valued reading §63 settled on: counting it as under would let
anybody past a riyal limit by invoicing in dollars. The message is its own,
`sales.document_limit_currency`, because "over 100 SAR" is not what 5 USD is.

**The claim.** `sales:exceed_document_limit`, `module:verb` like the others. It
is not in `SEGREGATED`, so it travels up the chart: a manager holds it when
somebody beneath them does. The decision's wording, "a position above them",
reads the other way. The org chart has no positions, only employees, and a claim
has never travelled down, so a clerk is not exempted by a grant to their
supervisor. The test pins both directions. It is asked with a new
`hr::actor_holds` (`hr/src/claims.rs:455`), `hr::may` without its two passes. `may`
answers yes when the tenant has granted no claim at all and when there is no
actor, which is right for a control that a grant switches on. Here the owner
switches the control on and the claim is the way past it, so "nobody has granted
anything" must not mean "nobody is limited". The employee lookup is shared with
`may_for` through a private `claimant`, so the two cannot drift. Like `may`, it
takes the branch from the request's `X-Branch`.

**My first draft had a 500 in it.** `actor_holds` found the employee through
`hr`'s read model, and a tenant selling without `hr` enabled has none, so the
first limited clerk at its till would have got `relation "proj_hr.employee" does
not exist`. I saw it writing the till test, whose fixture has no `hr`. `may`
never hits this, because it stops when no claim is granted. `actor_holds` now
does the same: it asks the grants, which live in the tenant's own migration
chain, before it asks the read model. Taking that check out again produces
exactly that error in the till and payments tests (see *Falsified*).

**The refusal** is `SalesError::OverDocumentLimit { limit, amount }` with the
amount on the limit's basis, `sales.over_document_limit` (or
`sales.document_limit_currency`) in en and ar, naming the limit, the amount and
the claim. A 403 everywhere it surfaces. The status is decided by one method,
`SalesError::refuses_the_caller` (`commands.rs:261`), which the sales routes,
the till, the booking desk and the payments route all ask. It also covers
`NotApproved`, which used to fall into the sales routes' catch-all and answer
400. Now both "who you are" refusals are 403, and the four routes' `FORBIDDEN`
descriptions say which codes.

**The matrix:** `document_limit` and `set_document_limit`, both OWNER. That
makes 246.

**Tests.** Through the product path. The limit is written with the same typed
`configuration::set` the `PUT` calls in the module tests, the way
`configure_exemption_reasons` seeds rates, and through the `PUT` itself over
HTTP.

- `a_clerk_is_held_to_the_document_limit_and_the_owner_is_not`
  (`modules/sales/tests/sales.rs:5266`). With no limit a clerk's 20,000 goes
  through. With 10,000 after VAT, the retry of that invoice still answers, 9,200
  goes through, and 11,500 is refused and leaves no document. The owner's 57,500
  and a `System` 57,500 go through. A login with no employee record is refused.
  A dollar invoice is refused, and the refusal names dollars.
- `the_basis_decides_which_total_is_held_to_the_limit` (`:5170`). Net 9,500 is
  10,925 gross: refused after VAT, allowed before. 10,000.01 net is refused
  before VAT.
- `the_claim_lifts_the_limit_for_whoever_holds_it_and_everyone_above` (`:5201`).
  Boss ← supervisor ← clerk. A grant to the supervisor exempts the supervisor and
  the boss, not the clerk. A grant to the clerk then exempts the clerk.
- `a_credit_note_over_the_limit_is_refused_whole_or_in_part` (`:5245`). A whole
  cancellation of 23,000 is refused, a partial of 11,500 is refused, and one of
  5,750 goes through.
- `a_refund_over_the_limit_is_refused` (`:5301`). 11,500 back is refused after
  VAT. 11,000 back from a two-band invoice, which issues no credit note, is
  refused. The same 11,500 is 10,000 of net and goes through before VAT. A 5,000
  refund that clears a part-paid invoice is refused on its whole-invoice credit
  note.
- `a_till_holds_a_clerk_to_the_document_limit` (`modules/pos/tests/pos.rs:1026`).
  This tenant has no `hr`. A clerk's 11,500 sale is refused and rings nothing,
  and the owner's goes through. The clerk's return of it in two 5,750 tenders is
  refused on its 11,500 credit note. The drawer moved for neither refusal.
- `a_members_refund_is_judged_when_asked_and_the_gateways_answers_are_not`
  (`modules/payments/tests/payments.rs:3277`). Under a 50 limit two customer
  deposits of 115 settle and are billed. A clerk asking for 115 back is refused
  and nothing is recorded. 40 is accepted. The gateway's confirmed 115 refund is
  recorded and credited.
- After review, `a_members_deposit_charge_is_judged_on_the_invoice_it_will_raise`
  (`:3487`). Under a 50 limit a clerk's saved-card deposit of 115 is refused and
  leaves no payment, so is the same deposit recorded at `POST /v1/payments`, a
  46 charge stands, and the customer's own 115 deposit is not judged.
- After review, `a_refund_that_clears_an_invoice_is_judged_on_its_whole_credit_note`
  (`:3530`). A 115 invoice part-paid by 40 at a gateway: a clerk asking for the
  40 back is refused under a 50 limit, because clearing it issues the 115 credit
  note; asking for 30 back, which leaves it holding 10, stands.
- `the_document_limit_is_the_owners_versioned_setting`
  (`crates/erp-api/tests/http.rs:14804`). `limit: null` and `ETag "0"` at first.
  0 SAR is `400 sales.document_limit_not_positive`, a bad currency is `400
  request.unknown_currency`, and neither is stored. A stale `If-Match` is 412.
  The limit reads back as written, and `null` removes it.
- `a_clerk_over_the_document_limit_is_refused_and_the_worker_is_not` (`:14878`).
  The owner sets 100 SAR over HTTP. The clerk's 115 invoice is `403
  sales.over_document_limit` naming `115.00 SAR`, and the owner's is 201. At the
  booking desk the clerk's 230 final invoice is 403. The worker's pass bills the
  same booking.
- Unit tests in `limit.rs` (`:299`, `:317`, `:327`), and one each in the till's
  and the payments route's status mapping (`pos/src/http.rs:931`,
  `payments/src/http.rs:1553`). Neither route has an HTTP test that reaches the
  refusal.

**Falsified.** For each row I broke the fix in Rust, watched the named tests
fail, restored the file, and watched them pass.

| broke | failed |
|---|---|
| `issue_in`'s `judge` removed | `a_clerk_is_held…`: `None` where 11,500 was due; `the_basis_decides…held`: `None` where 10,925 was due; `the_claim_lifts…`: the clerk not refused; `a_till_holds…`: the sale rang; the HTTP test: 201, not 403 |
| `Basis::BeforeVat` compared the gross | `the_basis_decides_which_total_is_compared`; `…held_to_the_limit`: 9,500 net refused; `a_refund_over…`: the 10,000-net refund refused |
| the owner not exempt in `binding` | `a_clerk_is_held…`, `a_till_holds…`: the owner refused; the HTTP test: *the owner is never limited*, 403 |
| `binding` asking for another claim | `the_claim_lifts…`: the supervisor, granted it, refused |
| `actor_holds` answering yes for any employee | `the_claim_lifts…`: *a claim does not travel down the chart* |
| `actor_holds` without its grants check | `a_till_holds…` and `a_members_refund…`: `relation "proj_hr.employee" does not exist` in place of the refusal |
| an incomparable currency counted as under | `another_currency…`; `a_clerk_is_held…`: the dollar invoice went on to the ledger |
| `cancel_in`'s `judge_whole` removed | `a_credit_note_over…`: the 23,000 cancellation went through; `a_refund_over…`: the refund that clears a part-paid invoice; `a_till_holds…`: the split return |
| `credit_part_in`'s `judge` removed | `a_credit_note_over…`: the 11,500 partial went through |
| `refund_in`'s `judge_refund` removed | `a_refund_over…`: the two-band refund went through. **It passed before that case was added** |
| `refund_invoice` passing `System` to `credit_what_is_clear` | `a_refund_over…`: the clearing refund went through |
| `take_back` passing `System` to its credit note | `a_till_holds…`: the split return went through |
| `request_refund_in` judging as `System` | `a_members_refund…`: the 115 request accepted |
| `bill_the_deposit` as a member | `a_members_refund…`: *a customer's own deposit is billed* failed to settle |
| `payments::refund_in` as a member | `a_members_refund…`: the gateway's confirmed refund refused |
| `bill_completions` as `Authority::of(db)` | the HTTP test: *nobody at the desk*, billed 0 |
| the booking desk's 403 mapping | the HTTP test: 409, not 403 |
| the sales routes' 403 mapping | the HTTP test: 400, not 403 |
| the till's and the payments route's 403 mapping | their unit tests: 422 and 400 |
| `DocumentLimit::new` accepting zero | `a_limit_of_nothing…`; the settings test: 204, not 400 |
| `#[serde(try_from)]` removed | `a_limit_of_nothing…`: a stored −5 read back |
| the `GET` on `Allowed<Read>` | `every_role_against_every_endpoint`: *accountant → GET /v1/sales/document-limit answered 200* |

**Review found two more places a member reaches a document, and one doc line
that was not true.** Both holes had the same root: a document a member *asks
for* and a gateway *causes* is issued later with `System`, and only one of the
two such paths was judged when the member asked.

- **A deposit a member charges was not judged at all.** A clerk holds
  `PostEntries`, so they can call `POST /v1/payments/cards/{card}/charges` or
  `POST /v1/payments` with a `deposit`. The worker charges the card, `settle_in`
  calls `bill_the_deposit`, and that raises the prepayment invoice with
  `System` — a 57,500 riyal document from a clerk who is refused at
  `/v1/sales/invoices` for the same number. The Left-open item excusing this
  said judging the request would need the deposit's gross at `request_in`, and
  that was simply wrong: `Collection.amount` **is** the gross, because
  `bill_the_deposit` refuses any settlement whose invoice does not come to
  exactly what was charged (`payments/src/commands.rs:425`), and `Advance.net`
  is the net. So the member is judged when they ask, as they already were for a
  refund: `sales::may_issue` (`limit.rs:273`) is `binding` plus `judge` on those
  two numbers, and `payments::may_bill` (`commands.rs:797`) calls it from
  `request_in` (`:1429`) and `start_in` (`:203`), each only when something was
  written, so a retry answers as the first call did. Both now take an
  `authority` with no default, like every other root: the two routes pass
  `Authority::of` (`payments/src/http.rs:318`, `:1194`), the customer's own
  deposit route passes `System` (`erp-api/src/deposits.rs:379`), and the three
  sweeps pass `System`, since what they start was asked for and judged already.
  Settlement still issues the invoice with `System`, which is what stops a
  charge the customer has paid from going undeclared.
- **A gateway refund that clears an invoice issues a whole-invoice credit note,
  and `may_refund` judged only the money.** A clerk asking for 5,000 back
  against a part-paid 23,000 invoice was accepted, and `payments::refund_in`
  then issued a 23,000 credit note with `System`. The same refund at
  `/v1/sales` is refused, because there `credit_what_is_clear` runs with the
  member. The root is that "which credit note does this refund leave owing" was
  answered in two places. It is one now: `commands::owed`
  (`sales/src/commands.rs:951`) decides `Nothing`, `Whole`, `Part(net)` or
  `Overstated(why)` from the invoice and what it holds once the refund is
  recorded. `credit_what_is_clear` (`:854`) carries that decision out instead of
  calling `credit_in` and reading `HasPayments` and `AlreadyCancelled` back out
  of it, and `may_refund` (`:783`) judges what it says: the whole
  invoice when the refund clears it. A partial credit note credits exactly what
  went back, which `judge_refund` has already judged, so that arm judges
  nothing twice.
- **`actor_holds`' doc said it "does not ask whether the tenant uses claims"**,
  and its first statement asks exactly that — it reads "nobody has granted
  anything" as "not held" rather than as a pass, which is the whole point of
  the function. A reader trusting the sentence would have deleted the call as
  dead and brought back the 500 this section describes. Reworded
  (`hr/src/claims.rs:437`).

`sales.md`, `http.md` and the `GET /v1/sales/document-limit` rustdoc now name
the deposit routes among the places a member is judged, and say the two refund
answers come from one decision. The two payments routes' `FORBIDDEN` lines say
which code, so `just openapi` ran again.

| broke | failed |
|---|---|
| `request_in`'s `may_bill` removed | `a_members_deposit_charge…`: `Ok(())` where the saved-card deposit was due a refusal |
| `start_in`'s `may_bill` removed | `a_members_deposit_charge…`: `Ok(())` where the recorded charge was due one |
| `may_refund`'s `Owed::Whole` arm answering `Ok(())` | `a_refund_that_clears…`: `Ok(())` where the clearing refund was due one |

**The round's gate found `may_refund` breaking L7, and it moved.** The L7 scan,
`an_aggregate_is_loaded_only_while_handling_a_command`
(`crates/erp-eventlog/tests/write_side.rs`), allows `erp_eventlog::load` only in
a module's `commands.rs`, because that is where the convention puts command
handling. `may_refund` loads the invoice and was written in `limit.rs`, so the
whole-workspace run failed on it. It *is* command handling — it runs inside
`payments::request_refund_in`'s transaction and decides from history what that
write may do, the same argument the allowlist already makes for
`payments/src/commands.rs` — so the fix is where it lives, not an allowlist
entry that would stop the rule meaning what it says. `may_refund` moved whole
to `sales/src/commands.rs` (`:780`), next to the `owed` it judges by and the
`credit_what_is_clear` it mirrors; `lib.rs` re-exports it from `commands`
instead of `limit`, so `sales::may_refund` is unchanged for every caller.
`limit.rs` keeps everything that loads nothing, and its module doc says why the
one function left. Falsified by moving the function back into `limit.rs`,
watching the scan name `modules/sales/src/limit.rs:293`, restoring both files
byte for byte (checked with `md5sum`) and watching it pass.

**Doc corrections.** `roles.rs` said an invoice or a till sale "posts with no
amount for it to judge". It now says why an amount rule cannot reach one and
names this control. `limits.rs`, the book's `erp-tenant.md` and `http.md`
(permission limits) say which control is for what. So does ARCHITECTURE §5.6: a
permission limit for what a role may do, the document limit for how large one
sales document may be. `sales.md` gained the section, the signatures and the
route. `pos.md` gained the signatures and a paragraph. `http.md` gained the
routes and a *Document limit* section. §63's Left-open item and §52's table point
here.

**Left open.**

- ~~**A till return does not ask for `sales:approve_credit_note`.**~~ Put to the
  product owner and closed by §70: `may_credit` moved into the roots behind
  `Authority::Member`, which is the one-place fix this bullet named.
- **`purchases.not_approved` and `hr.not_approved` still answer 400.** Only
  sales moved to 403.
- ~~**A member who starts a card charge for a deposit is not limited.**~~ Found
  by review, fixed above: judged at `request_in` and `start_in`, where the net
  and the gross both already are.
- **A deposit refused at `POST /v1/payments` may already exist at the gateway.**
  That route records a charge the caller's browser created, so a refusal leaves
  a charge this system has not written down. It is the right way round — the
  alternative is a prepayment invoice of any size — but the client has to void
  it. The saved-card route has no such window: nothing is charged until the
  worker's pass, and the refusal happens before it.
- **A gateway's answer is judged on the invoice as it stood when the member
  asked.** A payment, refund or credit note that lands in between can change
  which credit note the gateway's answer issues, and that one is not judged
  again — it cannot be, because by then the money has moved.
- **An owner-role API key is exempt.** `Authority::of` reads the handle's role,
  and a key can be issued with any role. That is the same rule `hr::may`
  applies.
- **A tenant that disables `hr` after granting claims** would hit the missing
  read model in `actor_holds`, as `may_for` already would. Disabling a module
  with live grants is not guarded anywhere.
- **The limit is one amount for every document kind and every branch.** Nobody
  asked for per-kind or per-branch limits. The claim is branch-scoped already.
- **A stored limit this build cannot read refuses every member's document** until
  the owner writes a new one, which the `PUT` can always do. There is no
  read-back of the raw row, as for permission limits.

### 67 · A second factor a tenant requires can be replaced, never removed

**Built 2026-09-11**, Round 3 decision B. §58 stopped platform staff turning
their second factor off, because an account with a password and no factor gets
its next factor from whoever enrols first, and that may be somebody holding
only the password. It left the same window open for a tenant that requires a
factor: a member could turn theirs off and whoever enrolled next was let in.
The decision is that a member of such a tenant can replace their factor but
never remove it, by the same rule as staff.

**One function decides it.** `ControlPlane::second_factor_required_by`
(`crates/erp-control/src/second_factor.rs:513`) returns
`Option<FactorRequiredBy>`. It is `Staff` for any live platform row, whatever
its role, which is the query `disable_second_factor` used to run inline. It is
`Tenant` for a live (`revoked_at IS NULL`) membership of a tenant with
`requires_second_factor` whose status is not `deleted`. Staff is the answer
when both hold. `disable_second_factor` asks it first (`:469`), before it
looks at the code, so a recovery code sent with a refused request is not
spent.

**Which tenant statuses count.**

- **Suspended counts.** A suspended tenant is reinstated with its requirement
  still set. A member who dropped their factor during the suspension would come
  back password-only, which is the gap this closes.
- **Provisioning counts.** It becomes active with its flag. Nothing sets the
  flag on a tenant that is still provisioning today, so this is the same rule
  rather than a case anybody hits.
- **Deleted does not count.** Nobody enters it again, and `tenants_for_identity`
  leaves it out for the same reason. No product path sets `deleted` today, so
  this part has no test. Reaching it would take raw SQL.

The function reads the database, not the tenant cache that `enter` uses.
Switching the requirement on binds the next removal attempt on every node.
Switching it off frees removal at once.

**The error is generalised.** `AuthError::StaffKeepsSecondFactor` became
`AuthError::SecondFactorKept(FactorRequiredBy)` (`auth.rs:40`). Staff keep
their code, `auth.staff_keeps_second_factor`, so no client that already matches
on it breaks. Tenant members get `auth.tenant_keeps_second_factor`, in en and
ar (`messages.rs:84`). Its text tells them the way out: ask the organisation's
owner to remove them, or to stop requiring it. Both are 403
(`erp-web/src/error.rs:75`), because signing in again would not change the
answer. The `DELETE /v1/sessions/second-factor` 403 now describes both codes
and says the code is not spent (`routes.rs:903`).

**It refuses even when only a pending enrolment exists.** A required member who
began an enrolment and never confirmed it cannot drop the pending row through
this route either. That costs them nothing, because starting another enrolment
replaces the pending one. It keeps the rule to one line.

**Replacement never passes through a state without a factor, and it could.**
I checked every write that removes `totp` or `recovery` rows:

- `verify_second_factor` spends one recovery code. The `totp` row stays.
- Erasure deletes the identity, and the rows go with it by cascade.
- `reseal_second_factors` only updates the sealed part, and only when that
  part still matches what it read.
- `passwords.rs` touches only the `password` row.
- `begin_second_factor` touches only `totp_pending`.
- `confirm_second_factor` deletes the old factor and renames the pending row in
  one transaction. That looks safe, but it was not under concurrency.

Two confirmations of the same replacement can both pass every check before
either writes. Each proves the old factor with a different code inside the
drift window, and both find the pending row. If the second one's transaction
starts after the first commits, its `DELETE` sees the first's new `totp` and
new recovery codes and deletes them. Its `UPDATE` renames nothing, because the
pending row is already gone. Its ten `INSERT`s no longer collide, and it
commits. Both callers get `201` with recovery codes, and the account has
recovery rows and no `totp`. `has_second_factor` then says no, `start_session`
lets the password alone in, and the next person to enrol holds the factor.
Nothing asks `second_factor_required_by` on that path. A double-submitted
confirmation form is enough to cause it. In the other order, where the second
transaction starts first, it failed with a unique violation, which showed as a
500.

The fix is at the rename (`second_factor.rs:293`). The `UPDATE` must move
exactly one pending row, or the function returns `InvalidCredentials` and the
transaction rolls back, taking the `DELETE` with it. The renamed row stays
locked until commit, so whatever commits has a factor. The second confirmation
now gets the same answer as confirming with nothing pending, whichever order
the two run in.

**Tests.**

- `a_second_factor_a_company_requires_is_replaced_never_removed`
  (`crates/erp-api/tests/http.rs:2995`). The owner switches the requirement on
  through `PUT /v1/members/second-factor-policy`. A clerk in acme, which
  requires a factor, and globex, which does not, sends a valid recovery code to
  `DELETE` and gets `403 auth.tenant_keeps_second_factor`. `GET` still shows
  the clerk enrolled with 10 codes left. The clerk then replaces the factor,
  using that same unspent code as `previous`. The owner is refused the same
  way. The owner removes the clerk from acme over HTTP, which leaves the clerk
  only in globex, and the clerk's `DELETE` answers 204. The owner switches the
  requirement off, and their own `DELETE` answers 204.
- `a_suspended_tenant_still_keeps_its_members_factor`
  (`crates/erp-control/tests/second_factor.rs:602`). The requirement is on and
  the tenant is suspended. The member is refused with
  `SecondFactorKept(Tenant)` and the recovery code is not spent.
- `two_confirmations_at_once_never_leave_the_account_without_a_factor`
  (`:653`). The test holds three of the pool's four connections, so the two
  confirmations share one. The second one's transaction can then only begin
  after the first commits, which is exactly the losing order. It asserts that
  a factor is left, that exactly one confirmation succeeded, that the other got
  `InvalidCredentials`, and that the new app verifies.
- `the_requirement_can_always_be_switched_off` (`:551`) used to reach "owner
  locked out" by having the owner turn off their own factor while the
  requirement held. That is now refused, so the test uses a second owner who
  never enrolled. They are refused entry, switch the requirement off without a
  factor, and get in.
- Staff are still refused. `every_platform_role_against_every_platform_endpoint`
  is unchanged and still expects `auth.staff_keeps_second_factor`.

**Review found the refusal pointing at a door that does not exist.** The new
message, the `DELETE` 403 text in `openapi.json`, RUNNING, the book's `http.md`
and this section told a member the way out was to leave the organisation. No
route lets a member leave. The only removal is the owner's
`DELETE /v1/members/{identity}` (`remove_member`, `Allowed<ManageTenant>`,
`members.rs:433`). The en and ar text now says to ask the owner to remove you or
to stop requiring two-step sign-in (`messages.rs:614`). The 403 description
(`routes.rs:903`) names the owner's route and says a member cannot leave on
their own, and `just openapi` regenerated the document. RUNNING, `http.md`, the
code's rustdoc and the HTTP test's doc say the same. No behaviour changed, so
there is no new test. The code the member gets is still pinned by
`a_second_factor_a_company…`.

**Falsified.** For each row I broke the fix, watched the test fail, restored
it, and watched it pass. The breaks inside `query!` text were built against
the type-check database (`SQLX_OFFLINE=false`). After them, `just prepare`
rewrote `.sqlx/`.

| broke | failed |
|---|---|
| the `Tenant` arm only when also staff (`row.tenant && row.staff`) | `a_second_factor_a_company…`: `(204, None)` where `(403, auth.tenant_keeps_second_factor)` was due; `a_suspended_tenant…`: `Ok(())` |
| the refusal moved after the code check | `a_second_factor_a_company…`: *the refusal took the factor or spent the code*, 9 ≠ 10; `a_suspended_tenant…`: *the refusal spent a recovery code*, 9 ≠ 10 |
| `m.revoked_at IS NULL` dropped (SQL) | `a_second_factor_a_company…`: 403 where 204 was due for the clerk removed from acme |
| `AND t.requires_second_factor` dropped (SQL) | the same line: globex, which requires nothing, still refused |
| `t.status <> 'deleted'` narrowed to `= 'active'` (SQL) | `a_suspended_tenant…`: `Ok(())` |
| the `Tenant` case rendered with the staff code | `a_second_factor_a_company…`: `auth.staff_keeps_second_factor` where the tenant code was due |
| the `Staff` arm only when also a tenant member | `every_platform_role…`: `(204, None)` where `(403, auth.staff_keeps_second_factor)` was due |
| the rename check loosened to `renamed > 1` | `two_confirmations…`, 3 runs of 3: *two confirmations left the account password-only*, both `Ok` with recovery codes; passes 3 of 3 restored |

**Docs.** I updated these:

- RUNNING's second-factor section and the book's `http.md` now describe both
  refusals.
- `erp-control.md` now names the new error and the function.
- `Tenant::requires_second_factor` and the `SecondFactorPolicy.required`
  schema text said the requirement refuses entry "and nothing else", which is
  no longer true. They now say it also keeps members' factors, and `just
  openapi` regenerated the document.
- §58's paragraph about the tenant gap now points here.
- `set_second_factor_requirement`'s rustdoc began with the first paragraph of
  `request_visit`'s doc, which had been pasted above it. I moved that paragraph
  back to `request_visit`.

**Left open.**

- **First enrolment is an accepted gap** (decision B). An account that has
  never had a factor can have one enrolled by whoever holds the password. A
  member who joins a requiring tenant without a factor is refused entry until
  they enrol, and whoever enrols first holds it. Nothing here changes that.
- ~~**Losing the authenticator and all the recovery codes is a lockout, and
  nobody can undo it.**~~ Closed by §69, Round 3b: the tenant's owner, a member
  holding `hr:reset_second_factor`, or platform support resets it, and the
  person enrols again through a link that is mailed to them. A required member
  still cannot turn their *own* factor off, and nobody can reset their own
  either, so the rule this section is about is untouched.
- **The check and the delete are not serialised against a concurrent
  switch-on.** If the owner switches the requirement on while a member's
  `DELETE` is between its check and its delete, the member ends up without a
  factor in a tenant that requires one. The result is the same as if the member
  had removed the factor a moment before the switch, which is allowed, so no
  lock was added. They are refused entry until they enrol again.
- **Deleted tenants are excluded without a test.** No product path produces
  one.
- **A suspended tenant's member waits for reinstatement.** Both of the owner's
  ways out, removing the member and switching the requirement off, go through
  `enter`, which answers a suspended tenant 503. Until it is reinstated nobody
  can free the member's factor, and the message does not say so, because the
  refusal does not look at which tenant holds it.
- **A replacement racing a new `begin_second_factor`** still commits whichever
  pending secret is in the row at rename time. That may not be the one whose
  code was checked. It needs a live session, and the begin has to land in the
  milliseconds between the check and the rename. Keying the rename on the
  secret that was verified would close it. It is outside this item.

### 66 · An old pod's audit entries find their tenant, and a scan makes it a rule

**Built 2026-09-11.** §62 gave `audit_entry` a `tenant_id` and left one hole
open. During a deploy, a pod still on the build before `0019` inserts in the
shape it knows, with no `tenant_id`, and the append-only trigger forbids
filling it in afterwards. Those entries would have been missing from their
tenant's trail for good. Round 3 decision A sets zero-downtime deploys as a
goal: catch up at write time, through one SQL function the backfill and a
trigger share, and make the rule general.

**`0019` is amended, not followed by a new migration.** It is untracked in git
and has been applied only to databases that are thrown away. I checked. The
`.env` database `erp_control` is at control migration 10. `erp_typecheck` is
rebuilt by `just prepare`. `scan_bench` and `spa_backend` have no
`_sqlx_migrations`. Every other control-chain database on the one cluster is
an `erp_test_*` clone or an `erp_tmpl_control_*` template, and the testkit names
templates by a fingerprint of the migrations, so a changed file builds a new
one. Amending in place means the trigger exists from the moment the column
does. The migration is one transaction, and the `ALTER TABLE` holds its lock
until commit, so an old pod's insert waits and then meets the trigger. There
is no window.

**One function, two users** (`0019_audit_tenant.sql:31`).
`audit_entry_tenant(subject_type, subject_id, detail)` returns the tenant from
the three places the backfill looked, in the backfill's order:

1. a subject that is a tenant;
2. `detail`'s `tenant`;
3. the key's tenant when the subject is an API key.

Otherwise it returns NULL. It is plpgsql with one `IF` per rule, not a `CASE`
in SQL, so no rule's cast runs on a row it does not apply to. That matters
because a `handle` subject is an email address, and `'x@y'::uuid` raises. The
backfill is now one `UPDATE` through the function (`:50`), where it was three.
`audit_entry_fills_tenant()` (`:66`) sets `NEW.tenant_id` from it only when
the insert left it NULL, and `audit_entry_tenant_on_insert` (`:75`) runs it
`BEFORE INSERT`.

**Ordering with the append-only trigger:** there is none to get wrong.
`audit_entry_no_update` is `BEFORE UPDATE OR DELETE` and the new one is
`BEFORE INSERT`, so no statement fires both. The re-pinned
`audit_entry_is_append_only` still compares `tenant_id`, so filling it in
after the insert is still refused, and
`the_audit_trail_is_still_append_only_for_everything_else` passes unchanged.

**This build's `None` stays NULL.** A trigger cannot tell an old pod's missing
column from this build's `None`. What matters is that the rules find nothing
in what the `None` writers write, and I read every one of them:

- staff changes and platform memberships: subject `identity`, `"tenant": null`;
- dead letters: `effect`;
- `identity.*`;
- `cluster.*`;
- `signup.requested`: `handle`;
- the test action: `thing`.

None of them matches a rule. `record()`'s doc (`lib.rs:2301`) said an entry
that names no tenant "is not in that tenant's trail whatever its subject or
detail says". That is no longer true of a `None` with a tenant subject. The
doc now says what the trigger does, and that no current `None` writer trips
it. The `ponytail:` note under it is gone. The book's paragraph on `record`
said the same thing and is rewritten, and `tenant_audit`'s doc names the
trigger.

**Tests** (`crates/erp-api/tests/http.rs`):

- `a_pod_on_the_build_before_0019_files_its_entries_under_their_tenant`
  (`:4402`) makes four inserts shaped exactly like the pre-`0019` `record()`
  (from `git show HEAD`), with no `tenant_id` column. They are the test's only
  SQL writes, and its doc says they simulate an old build. Three of them show
  up in the owner's `GET /v1/audit` under acme, with the owner named:
  - `tenant.origin_revoked`, found by its subject;
  - `membership.role_changed`, found by `detail`;
  - `api_key.revoked`, for a key issued over HTTP, found by the key.

  The fourth, `identity.suspended`, does not show up.
- `the_database_keeps_the_tenant_its_writer_gave_and_the_none` (`:4488`). A
  superadmin makes an acme clerk support staff over HTTP, then billing. Both
  entries are about a tenant member, and both stay NULL. `record(Some(acme),
  "test.filed", "tenant", globex, {"tenant": globex})` stays acme.
- The dead-letter test (`support_requeues_and_dismisses…`, `:4012`) now also
  asserts that no `effect.*` entry has a tenant.

**The general guard** is
`a_column_added_to_an_append_only_table_is_filled_on_insert`
(`crates/erp-control/tests/migrations.rs:427`). It finds the append-only
tables in the migrations themselves: the ones with a trigger that runs a
function named `*_is_append_only`. Today those are `event` (tenant `0001`) and
`audit_entry` (control `0001`), keyed by chain. The test fails if either one
stops being found, so the convention cannot quietly stop checking anything.

Then it fails any `ALTER TABLE` on one of those tables that has an `ADD`
clause with no `DEFAULT`, or only `DEFAULT NULL` (`gives_a_default`, `:306`).
`COLUMN` is optional in that clause, and constraints don't count. There are
two ways out:

- the same migration creates a `BEFORE INSERT … FOR EACH ROW` trigger on the
  same table. The scan sees that the trigger exists, not that it fills the
  column; that part is review's;
- `migrations/EXEMPTIONS` gives the migration the new rule
  `append-only-column`, with a reason.

`no_migration_carries_an_exemption_it_does_not_need` knows the new rule and
refuses it where nothing needs it. `the_append_only_check_refuses_what_it_claims_to`
pins the parser on thirteen synthetic statements, among them:

- `ADD` without `COLUMN`;
- `DEFAULT NULL`, and `ON DELETE SET DEFAULT` with no default;
- a constraint;
- the other chain's `event`;
- an `AFTER INSERT` trigger, which runs too late to set `NEW`;
- a statement-level `BEFORE INSERT` trigger, written out or left to
  Postgres's default, which has no `NEW` at all;
- a row-level `BEFORE INSERT` trigger on a different table.

**Review found four holes in the scan, all fixed.** It counted any clause with
the word `default` in it as filled, so `DEFAULT NULL`, which is exactly no
default, passed. So did a clause whose only `default` is in `ON DELETE SET
DEFAULT`. `gives_a_default` now wants a `default` that is not after `set` and
is followed by something other than `null`. `a_new_column_is_never_mandatory_without_a_default`
had the same word match (`NOT NULL DEFAULT NULL` passed), so its predicate is
now `mandatory_without_a_default` (`:297`) on the same helper, and its self-check
calls that predicate instead of asserting on `statements()`. `trigger()`
counted a statement-level `BEFORE INSERT` trigger as filling, though it has no
`NEW`; it now also wants `for each row` (`:326`). Nothing tested that the
trigger had to be on the same table as the column: loosening the match to "any
`BEFORE INSERT` trigger in the migration" passed all six tests. Last, the
ARCHITECTURE row said the trigger "fills it" and left out the `EXEMPTIONS`
way out. It now lists all three ways and says the scan checks only that the
trigger exists.

No existing migration needed an exemption. The expand-only rule was not
described in ARCHITECTURE at all, so §7's table now has a "Deploy overlap" row
covering both rules. The `EXEMPTIONS` header names the new rule.

**Falsified.** The guards in `0019` are SQL, so I broke each one in the
unshipped file and touched `lib.rs` so `sqlx::migrate!` embedded it again. I
ran the tests and watched them fail. Then I restored the file byte for byte
(checked with `cmp`) and watched them pass.

| broke (SQL, in `0019`) | failed |
|---|---|
| deleted the `CREATE TRIGGER` | `a_pod_on…`: *no tenant.origin_revoked in* the owner's trail; and `a_column_added…`: *control/0019_audit_tenant.sql adds a column to `audit_entry`, which is append-only, with no default* |
| deleted rule 1 (subject) | `a_pod_on…`: *no tenant.origin_revoked* |
| deleted rule 2 (`detail`) | `a_pod_on…`: *no membership.role_changed* |
| deleted rule 3 (API key) | `a_pod_on…`: *no api_key.revoked* |
| the trigger sets the tenant unconditionally | `the_database_keeps…`: `test.filed` under globex, not acme |
| a fourth rule: an `identity` subject's tenant membership | `the_database_keeps…`: both staff entries under acme; `a_pod_on…`: *an entry about a person was filed under a tenant* |
| unmatched rows get a fixed UUID instead of NULL | the dead-letter test: *a dead letter was filed under a tenant*, 2 ≠ 0; and `the_database_keeps…` |

| broke (Rust, in the scan) | failed |
|---|---|
| an `AFTER` trigger counts as filling | `the_append_only_check…`: *too late to set NEW* |
| `constraint` dropped from the not-a-column list | `the_append_only_check…`: the `ADD CONSTRAINT` case, 1 ≠ 0 |
| only `ADD COLUMN` recognized | `the_append_only_check…`: *COLUMN is optional* |
| the detector looks for a misspelled function name | `a_column_added…`: *("tenant", "event") is no longer found append-only* |
| an `append-only-column` exemption added for `0019` | `no_migration_carries…`: *exempts `append-only-column` and does nothing that needs it* |
| after review: `gives_a_default` accepts `DEFAULT NULL` | `the_append_only_check…`: *DEFAULT NULL is no default*, 0 ≠ 1; `the_check_refuses…`: `mandatory(… NOT NULL DEFAULT NULL)` |
| after review: `gives_a_default` accepts `SET DEFAULT` | `the_append_only_check…`: *SET DEFAULT is no default*, 0 ≠ 1 |
| after review: `mandatory_without_a_default` back on the word match | `the_check_refuses…`: `mandatory(… NOT NULL DEFAULT NULL)` |
| after review: `trigger()` without the `for each row` test | `the_append_only_check…`: *a statement-level trigger has no NEW*, 0 ≠ 1 |
| after review: any `BEFORE INSERT` trigger in the migration counts (`!filled.is_empty()`) | `the_append_only_check…`: *a trigger on another table fills nothing here*, 0 ≠ 1 |

The broken template databases are left on the test cluster for `just
clean-databases`. No `query!` text changed, so `just prepare` wrote nothing
new to `.sqlx/`. RUNNING had no prose about the deploy gap, so it is
unchanged. §62's Left-open bullet is struck through, and its note on the
untested backfill now says the rules are tested through the trigger.

**Left open.**

- **The backfill statement itself is untested.** The rules are tested through
  the trigger. The one `UPDATE` that applies them is not, because the testkit
  migrates empty databases.
- **The append-only convention is a name.** If a future append-only table's
  guard function is not called `*_is_append_only`, the scan doesn't see it. The
  two known tables are pinned, so renaming either one's function fails loudly.
  A third table has to follow the name.
- **The scan reads statements split on `;`**, as the expand-only scan always
  has. It does not see an `ALTER TABLE` inside a `DO` block, or a trigger made
  by dynamic SQL.
- **The trigger escape is structural, not semantic.** The scan wants a
  row-level `BEFORE INSERT` trigger on the table. It does not read the
  function, so a trigger that never touches the new column passes, and so does
  one whose `WHEN` skips the rows that need it. `gives_a_default` reads words:
  `DEFAULT (NULL)` or an expression that yields NULL passes as a default.
- **A future `None` writer with a tenant-shaped subject** would be filed under
  that tenant by the trigger. `record()`'s doc says so. Nothing enforces it
  beyond review and the examples in the two tests.
- **An old pod's `api_key.revoked` for a key whose tenant is gone** stays NULL,
  because the key row was deleted along with the tenant (cascade). The
  backfill has the same limit.

### 65 · Read models say which build made them, and a stale one answers 503

**Built 2026-09-11.** Nothing recorded which read model built a tenant's
tables. `install.sql` is `IF NOT EXISTS` throughout, provisioning's checkpoint
insert was `DO NOTHING`, and a changed read model reached existing tenants only
if an operator remembered `migrator refresh <module>` for that module. `check`
said "uniform" while projections were on an old shape. A module disabled and
enabled again kept its old tables and nobody could tell. ARCHITECTURE §7 listed
"schema version = target, per module" as a continuously asserted invariant, and
nothing asserted it. `provision.rs` cited a test,
`a_module_has_exactly_one_projection_group`, that did not exist. Decision 7 sets
what a user sees while a tenant is behind: 503 for that module's routes, with a
new code in both languages, and never numbers from a shape the build has
replaced.

**The declaration and the record.** `ProjectionGroup::VERSION` (`group.rs`,
default 1) is the shape and meaning of a group's tables. `ModuleSetup.groups`
is now `(name, schema, version)`, and every module copies the third field from
its group type the way it copies the other two. Tenant migration
`0016_read_model_version.sql` adds `projection_checkpoint.read_model_version`,
`SMALLINT NOT NULL DEFAULT 0` with a `>= 0` check. It is expand-only, so the
migration test passes with no exemption. 0 means "built before this was
recorded", below every real version. It is deliberately not 1: nothing knows
whether an old tenant's tables match today's script, and stamping them current
is the silent fallback L6 forbids. The cost is that the first deploy rebuilds
every group on every tenant once, through the swap, with no outage.

**Four writers stamp it, and nothing else writes it.**

- `ensure_group` (`runner.rs`) stamps a new row. It takes the version now, and
  `ensure_group_schema::<G>` passes `G::VERSION`.
- Provisioning's `install_schema` (`provision.rs:1265`) stamps a new row too,
  and keeps `DO NOTHING`. An existing row means existing tables the DDL did not
  reshape, so the old stamp is the truth about them. That closes the
  disable-then-enable hole.
- `rebuild_schema` (`:1357`) sets the version with the rewind, because it
  really does rebuild.
- `rebuild_swap`'s swap (`shadow.rs:405`) sets it in the transaction that
  renames staging over live, so the stamp always names the live tables.

**The runner projects only into its own build's tables.** `run_once_in`
(`runner.rs:215`) reads the version with the lease, which costs no extra query,
and returns the new `RunError::OtherReadModel { group, installed, expected }`
when the row is not `G::VERSION`. That rides the worker's existing "job failed;
it is stalled" path, once per visit, per group. This is the continuous check §7
promised. It was `<` as first built, and review changed it to `!=` (below): a
build still draining must not write its old rules into tables the migrator has
stamped new. The price is that every changed group waits for the new build's
workers during a rollout, and the old ones log it as stalled until they are
gone.

**The deploy step rebuilds; `check` gates.** `ControlPlane::survey_read_models`
(`fleet.rs`) mirrors `survey_event_versions`: every tenant with a database,
suspended ones too, reading the tenant's checkpoints and not its entitlements.
A disabled module's groups are surveyed because its tables have to be current
the moment it is enabled again. The migrator judges it with a pure `stale`
(`bin/migrator.rs:291`). A group not at the build's version is `Rebuild`.
`!=` is right here: a group ahead is a rolled-back deploy's leftover, and read
models are derived, so rebuilding down costs only a replay. A group that no
module in the build declares is `Undeclared`, the read-model twin of "an event
nothing can read". The bare command runs `read_models` (`:205`) after
`migrate_fleet`, since the column is one of the migrations, and calls the
existing `rebuild` for each finding. `check` lists the same findings without
touching anything, as `acme: sales at 1, this build projects 2`. Either one
exits 1 if anything is left, and an unreachable tenant counts as left. The
calls go into the `Mode` match that §64 built. `refresh <module>` stays as the
manual rebuild.

**Forgetting to bump the version fails CI.** `a_read_model_change_bumps_its_version`
(`bin/migrator.rs:860`) pins `(group, version, sha256)` for every module's
`install.sql`, with comments and whitespace squeezed out. It uses the same
normalization as `tests/migrations.rs`, so a comment-only edit never trips it,
and `a_comment_is_not_a_change_of_shape` checks that. If the script changes and
the version doesn't, it fails with the bump and the new pin written out. It
also fails if the version moves without the pin, and on a pin that no module
declares. `sha2` and `hex` are new dev-dependencies of `erp-worker`, both
already in the workspace. The citation in `provision.rs` now names a test that
exists: `a_module_has_at_most_one_projection_group` (`erp-api/src/modules.rs`).
The name says "at most" because `messaging` and `hr_sa` have none.

**The request path: 503 while a read model a route serves from is behind.**
The check sits in the `Tenant` extractor, which every `Allowed<C>` goes
through, and in `Public`: `read_models_current` (`extract.rs:384`), after
entry. So only a caller who may be there learns that the module is being
rebuilt. It works on the module `module_of` finds, which is the answer the
capability check already uses, and answers
`503 request.read_model_rebuilding` with `args.module` (en and ar,
`erp-web/src/messages.rs`). Two things it needed:

- **Which groups a route serves from.** One module's own group was not enough.
  Every read across groups goes through the other module's crate, because L3
  leaves no other way: a scan found no qualified `proj_x.` reference to another
  module outside comments. `tax_sa`'s VAT return reads `sales` and `purchases`
  tables through `sales::vat_return` and `purchases::input_tax`, so a stale
  `sales` must stop the return too. `ModuleSetup::reads` (`.reading(&[..])`)
  names the modules a crate depends on, and
  `a_modules_reads_are_its_crate_dependencies` holds it equal to the crate's
  `[dependencies]`. `erp_api::modules::read_models` takes the closure, which is
  how `notifications` sees `booking` through `messaging`, a module with no
  tables. Review found that `erp-api`'s own routes under `/v1/booking` run
  more than `booking`'s crate does, and `COMPOSED` adds them (below). `erp_api::router` puts the result in `AppState::read_models`
  (`routes.rs:441`), from the list that mounts the routes. That way no server
  built there can refuse by one list and serve by another. `requires` would
  not do: it is what a tenant must *have*, and `sales` reads `crm` without
  requiring it.
- **Low cost per request.** `ControlPlane::read_model_behind`
  (`lib.rs:1269`) does one cache read per group, and one query on the
  tenant's read connection for the groups not known current.
  **It caches only "current"**, for the entry TTL. A behind answer is never
  kept. Every request for that module reads the checkpoint again, and the
  first request after the swap is served. That is the entry caches' own rule,
  where a refusal is not stored so a grant works at once. It is also why the
  cache needs no invalidation when a rebuild swaps tables in another process.
  The one way a group goes backwards is a restore or a rolled-back migrator,
  and the TTL bounds it. Misses are not counted in `entry_cache_stats`,
  because they cost the tenant's database and not the control plane.

The document's conventions (`routes.rs:352`) add the 503 to every route of a
module with read models, appended to whatever that route's 503 already said.
214 operations changed, and `just openapi` regenerated the document. A new
status is compatible, and the compatibility test agrees. I considered a router
layer instead of the extractor. It would run before authentication and put a
tenant query on unauthenticated requests.

**Falsified.** Each was broken in Rust, then the named test was watched fail,
restored and watched pass.

| broke | failed |
|---|---|
| the runner's guard disabled | `a_group_built_for_an_older_read_model_is_not_projected_into`: a newer build projected into the old shape |
| the swap leaves `read_model_version` as it was | `a_rebuild_brings_a_group_up_to_this_builds_read_model`: `Behind { installed: 1, expected: 2 }` (the variant's name then) after its own rebuild; and the HTTP test: files still 503 after the rebuild |
| `install_schema` stamps 0 | `a_new_tenant_is_stamped_with_the_read_model_it_was_built_from`: `Some(0)`, not 3; `re_enabling_over_an_old_shape…`: `Some(0)` |
| `install_schema` `DO UPDATE SET read_model_version = EXCLUDED…` | `re_enabling_over_an_old_shape_does_not_claim_the_new_one`: `Some(2)` over the old tables |
| `rebuild_schema` keeps the old version | `refreshing_a_module_rebuilds_its_schema_and_rewinds_its_checkpoint`: `Some(1)`, not 2 |
| `stale` skips a group ahead (`to <= from`) | `a_group_behind_or_ahead_of_this_build_is_rebuilt_and_a_current_one_is_not`: `[]` for toy at 3 |
| `stale` drops an undeclared group | `a_group_no_module_declares_is_reported`: `[]` |
| `crm`'s pinned hash changed (what editing its `install.sql` does) | `a_read_model_change_bumps_its_version`: *crm's install.sql changed shape but crm's read-model version is still 1*, with the pin to write |
| `normalized` keeps comments | `a_comment_is_not_a_change_of_shape`, and the pin: every module "changed shape" |
| `files` given a second group | `a_module_has_at_most_one_projection_group`: *files declares 2* |
| `crm` dropped from `sales`' `reading` | `a_modules_reads_are_its_crate_dependencies`: `["hr", "ledger"]`, with the `.reading(..)` to write |
| `read_models` without the closure | `a_modules_read_models_are_its_closure_over_reads`: `notifications` lacks `booking` |
| `Tenant` without `read_models_current` | the HTTP test: files 200 while behind |
| `Public` without it | the HTTP test: the public services list 200 while booking is behind |
| a behind group cached like a current one | the HTTP test: the second request while behind was 200 |
| `router` does not fill `read_models` | the HTTP test: files 200 while behind |
| the document convention off | `the_document_matches_the_router` |

**Falsification found a hole in the guard test, fixed.** The HTTP test first
asked once while behind. With a behind group cached as current, it still
passed, because nothing asked a second time before the rebuild. It asks twice
now. Caching the *behind* answer is not something I could break into: the
cache's value is `()`, "current", so it cannot hold one. One run of the pin
falsification passed at first, because the edit landed in the same mtime tick
as the previous build and cargo did not rebuild. The rerun, with the file
newer than the build, failed as shown, and every row above was run the same
way.

**One test seeds state around the product, and says so.**
`a_module_whose_read_model_is_older_than_the_build_answers_503_until_it_is_rebuilt`
sets a checkpoint to 0 with raw SQL (`built_before_versions`). This build writes
only its own version. What it simulates is what `0016` leaves on every older
tenant, or what a restore brings back. The same test calls `clear_caches`
after that out-of-band change, which is that function's documented purpose.
It does not call it after the rebuild, and that absence is the point.
Everything else is reached through the product. The re-enable test signs up
on v1, disables the module and enables it on v2.

**Doc corrections.** `ARCHITECTURE` §1.17 says how the rebuilds are chosen,
and §7 names the mechanism behind "schema version = target". RUNNING's
"Before a deploy" says the bare command rebuilds and `check` gates. It says
what a stalled group and the 503 look like, and that the bare command runs
once more after a rollout that changed a read model. The restore steps and the
checkpoint query mention the version. The justfile comment is updated. The
migrator's header describes the new bare mode. In `provision.rs`,
`install_module`, `refresh_module` and the one-group citation are updated. The
book is updated for `erp-projection` (`VERSION`, `OtherReadModel`, `ensure_group`'s
new argument, the swap stamp), `erp-tenant` (`groups`, `reads`), `erp-control`
(`survey_read_models`, `read_model_behind`, `install_module`/`refresh_module`),
`erp-web` (the refusal, `AppState::read_models`), `erp-worker` (the bare mode,
the pin), `erp-api` (the router, and the adding-a-module steps) and `http.md`'s
status table.

**Review found three things, and all three were real.**

- **`erp-api`'s own routes under `/v1/booking` were judged by `booking`'s
  closure alone.** That closure is `booking`, `branches`, `crm` and `hr`, built
  from module crates' `reads`. But `bill_reservation_route`
  (`POST /v1/booking/reservations/{r}/invoice`, `billing.rs`) works out the
  prepayment deduction through `sales::invoice` and `sales::bands_of`, then
  issues a final tax invoice. With `sales` stale on one tenant, `/v1/sales/*`
  answered 503 and this route still issued a legal document from the old
  tables. The deposit routes (`deposits.rs`) call `payments`, `ledger`,
  `messaging` and `tax_sa::registered` the same way. §65 had named the deposit
  routes as left open, but not the invoice, and ARCHITECTURE, RUNNING and
  `VERSION`'s doc claimed every route was covered. The fix is at the one place
  the closure is made. `COMPOSED` (`erp-api/src/modules.rs:272`) names, per
  module path, what this crate's routes under it run, and `closure` (`:237`)
  seeds the walk with it, at the root only, since another module reading
  `booking` runs `booking`'s crate and not these routes. `read_models` is
  built from `closure`, and so is the guard,
  `composed_routes_run_only_what_their_module_is_refused_on`. It scans every
  file in `erp-api/src` that declares a `path = "/v1/{m}/…"`, and fails on a
  registered module named there as `x::` (comment lines aside) that is not in
  `m`'s closure. Now every `/v1/booking` route answers 503 while `sales`,
  `payments`, `tax_sa` or what they read is stale. The OpenAPI document did
  not change, because `booking` already had read models and its routes
  already said 503.
- **The runner's `<` let the draining build write its old rules into tables
  stamped new.** After the migrator swaps `sales` to 2, a v1 worker still
  leased the group and projected. With a new column that has a default, or a
  bump for a change of meaning only, its rows land in v2 tables and the
  checkpoint passes them. The v2 workers continue from there, the migrator
  sees 2 == 2, the request path sees current, and those rows keep v1's rules
  for good. The expand/contract rule I had cited covers a draining pod
  *selecting* columns, not a draining worker *writing* rows. The runner is
  `!=` now. The API's check stays `<`, because reading a newer shape is what
  expand/contract does make safe. The variant is renamed `OtherReadModel`,
  because `Behind { installed: 2, expected: 1 }` said the opposite of what
  happened. Its message no longer says `just migrate-fleet` fixes it: on a
  draining pod the fix is the rollout finishing, and after a rollback it is
  the *old* build's migrator. RUNNING now says the old pods log the changed
  groups as stalled during a rollout, and that `consistent_after` reads
  answer `not_caught_up` meanwhile.
- **"That module answers 503" was false for the first rollout of this
  change.** A tenant provisioned by a pre-`0016` pod has no
  `read_model_version` column. `read_model_behind`'s query fails,
  `AccessError::Database` is a 500, and since every module is in
  `AppState::read_models`, every module route of that tenant answers 500.
  `check` lists it behind on migrations, and `unreachable` in the read-model
  survey, not as a stale group. The bare command after the rollout repairs
  it: it migrates, then rebuilds from 0. Only the prose was wrong, and the
  Left-open bullet and RUNNING say this now. I did not map the missing column
  to the 503. That would special-case one migration's first rollout in the
  request path, for a state the documented deploy order already ends.

| broke (review) | failed |
|---|---|
| `tax_sa` dropped from `COMPOSED` | `composed_routes_run_only_what_their_module_is_refused_on`: *deposits.rs serves routes under /v1/booking/ that run ["tax_sa"]* |
| `closure` ignores `COMPOSED` | the same test: deposits.rs runs `["payments", "ledger", "tax_sa", "messaging"]`; and `a_modules_read_models_are_its_closure_over_reads`: `booking` lacks `sales` |
| `COMPOSED` applied to every root, not only its own | `a_modules_read_models_are_its_closure_over_reads`: `files` is no longer just `["files"]` |
| the runner's `!=` made `<` again | `an_older_build_does_not_project_into_a_newer_shape`: `Ok(Advanced { events: 3, .. })` for the draining build |

Dropping `sales` alone from `COMPOSED`, as review suggested, does not fail
the scan. `payments`, `messaging` and `tax_sa` each read `sales`, so it stays
in the closure, which is correct. `tax_sa` is the entry nothing else brings
in.

**Left open.**

- **A tenant that signs up on an old pod during a rollout that changed a read
  model** is built with the old read model. On new pods, that module answers
  503 until the bare command runs again. RUNNING says to run it after the
  rollout. The worker does not rebuild for itself. On the rollout that brings
  in `0016` itself it is worse: such a tenant lacks the column, so every
  module route answers 500, and `check` lists it behind on migrations (above).
  Every later tenant migration has the same hazard for the same reason.
- **The composed-route scan is per file.** A route in one `erp-api` file that
  reaches another module through a helper in a different file is not followed.
  `realtime.rs` calls `deposits::public_settings`, which reads only `booking`.
- **Refused per module path, not per route.** Every `/v1/booking` route now
  answers 503 while `sales`, `payments`, `tax_sa` or `purchases` (through
  `tax_sa`) is stale, including the ones that read only `booking`.
- **Wider than strictly needed.** A crate dependency used only for event types
  counts as a read. `reports` depends on `sales`, `booking`, `pos` and
  `payroll` for their events, and so answers 503 while any of them is stale.
  That is the safe direction, and it only costs availability during a failed
  rebuild.
- **A disabled module's stale group still refuses the modules that read it.**
  The migrator rebuilds disabled groups with the rest, so this happens only
  after a rebuild failed.
- **The pin's loophole:** re-pinning the hash without bumping passes, in plain
  sight in a diff. A projection change with no DDL change is caught only by
  judgement, and `VERSION`'s doc says so.
- **Rebuilds run one at a time** across the fleet (`ponytail:` note), and the
  first deploy after this rebuilds every group on every tenant once.
- `ControlPlane::refresh_module` and `rebuild_schema` still have only test
  callers. They are stamped here, not deleted.

### 64 · Sealing keys that can actually be rotated

**Built 2026-09-11.** A column, an index, two doc comments and RUNNING all said
`SEALING_KEY` could be rotated: `module_secret.sealed_with` was there "so a
rotation can find what it has not re-sealed yet", and `SealingKey::parse` said
"a rotation means two keys existing at once". The code made rotation
impossible. `parse` took one key, `unseal` tried that one key, and `get` never
read `sealed_with`. The control plane's TOTP secrets recorded no key at all.
Changing the variable would have made every ZATCA key, gateway key, card
token, webhook secret and authenticator app unreadable at once. Decision 11
sets the scope: rotation, plus an incident playbook in RUNNING, and no reminder.

**The ring** (`crates/erp-eventlog/src/secrets.rs`). `SealingKey` (`:114`) is
now a current key and a list of previous ones. It keeps its name, so its ~40
callers did not change. `parse` (`:201`) takes `<id>:<hex>[,<id>:<hex>…]`, and
the first entry seals. It refuses an empty entry or id, a repeated id, and the
same bytes under two ids, a "rotation" that only renamed the compromised key.
Its errors name the entry or the id and never contain hex. The old one-key
error echoed the first eight characters of a malformed value, which could be
four bytes of the key. A single key parses as before, so no deployment has to
change anything. `Debug` shows the ids, and the loaders in `api.rs` and
`worker.rs` log it, so an operator can see a rollout take effect.

**A row is opened with the key it names** (`unseal`, `:299`). The id comes
from the column: `get` now passes `sealed_with`. An id the ring does not hold is
the new `SecretError::UnknownKey`. The row is refused, not tried under whatever
keys happen to be there, because a key that opens it is not the key the row
says sealed it (L6). `UnknownKey` is kept separate from `Unsealable` because
the fix differs: put the key back. `None` means a value sealed before its id
was recorded. Only authenticator rows enrolled before `0020` are like that,
and they are tried under every held key, which GCM makes a refusal rather than
a guess. A `ponytail:` note there says when that arm can go.

**The id lives in the column, not the envelope.** A new format byte carrying
the id would have made everything written after the deploy unreadable to a
rolled-back build. It could also collide with first-format values whose random
first nonce byte happens to match it. The envelope stays `0x02`.

**The control plane got the column.** `migrations/control/0020_authenticator_sealed_with.sql`
adds `authenticator.sealed_with`, nullable and with no constraint, which keeps
it expand-only. `begin_second_factor` writes it on both arms of its upsert
(`second_factor.rs:85`). The second arm matters: a re-enrolment after a
rotation that replaced the secret and kept the old id would name a key the
blob is not under, and the person could never confirm it. `stored_secret`
passes the column to `unseal`. Platform staff's factors are the same rows, so
everything here covers them.

**The sweep.** `secrets::reseal` (`:440`) moves one tenant database's rows off
every key but the current one. `reseal_second_factors` (`second_factor.rs:397`)
does the same for the control plane. The spent-code marker `remember_spent`
appends after a pipe is kept: the compare-and-swap is on the sealed part only.
`ControlPlane::reseal_fleet` (`fleet.rs:607`) runs the control plane, then
every tenant from `tenants_with_databases`: active and suspended, on every
cluster, through `maintenance_options`, at `fleet_concurrency`. Failures are
collected the way `migrate_fleet` collects them. Every row is its own
compare-and-swap on the value it read, so a `put` racing the sweep wins, the
row is left for the next run, and a rerun resumes. `updated_at` is not touched,
because resealing does not replace the secret. A row nothing opens goes in
`Census::unsealable`, by name only, and is left exactly as it was. Both modes
unseal every row, those already under the current id included, and write only
the stale ones, so the look-only mode reports what a real run would fail on.
`SealingPlan::is_settled` (`fleet.rs:591`) is the gate for retiring a key:
everything under the current key, nothing unopenable, and every tenant reached.

**The operator's command** is `migrator reseal` and `migrator reseal check`
(`bin/migrator.rs:359`). It needs `SEALING_KEY`, it does not migrate or
register a cluster, and it exits 1 until the plan is settled. It is not in the
reaper, which holds no sealing key and runs scheduled tidying. This is a
one-off step somebody decides to take.

**The migrator ran any word it did not know as the bare, applying command.**
`check_only = mode == "check"` sent everything else to `control.migrate()`
and `migrate_fleet()`. So `reseal check` run on an image from before this
change would have migrated the fleet, and so would a typo. RUNNING's restore
section told operators to run `migrator -- survey`. No such mode exists, so
that instruction migrated the fleet too. The fix is at the root: `mode`
(`:335`) matches the exact argument lists it knows into a closed `Mode` enum,
and `main` refuses anything else with the usage and exit 2 before it connects
to anything. RUNNING's restore section now says `check`, then the bare command,
then `reseal check`.

**Falsified.** Each was broken in Rust and the named test watched fail, then
restored and watched pass. Three rows break SQL text inside `query!` rather than
a migration. Those were compiled against the `just prepare` type-check database
(`SQLX_OFFLINE=false`) so `.sqlx/` was never touched, and are marked *(query
text)*.

| broke | failed |
|---|---|
| `seal` uses the last key in the ring | `a_value_sealed_under_the_previous_key_unseals_after_rotation`: the new-only ring could not open what the ring sealed |
| `unseal(Some(id))` ignores the id and tries every key | `a_value_under_a_key_the_ring_does_not_hold_is_refused_by_name`: the renamed key opened it |
| `unseal(None)` tries only the current key | `an_unrecorded_value_opens_under_any_held_key` |
| `parse` without the same-bytes check / without the empty-id check | `a_configured_ring_is_the_current_key_first`: *the same bytes under two ids was accepted* / *an empty id was accepted* |
| `Debug` prints the previous key's bytes | `the_key_is_not_in_its_own_debug_output` |
| `Census::is_settled` ignores unsealable rows | `a_census_is_settled_only_when_nothing_is_left_behind` |
| `mode` falls back to `Apply` | `only_the_modes_the_migrator_knows_are_accepted`: *["chek"] was accepted* |
| `reseal_fleet` without the control plane | `a_rotation_reseals_every_tenant_on_every_cluster_and_the_control_plane`: `{"old": 3}`, not 4 |
| `reseal_fleet` walks only `primary` | the same test: `{"old": 3}` |
| `reseal_fleet` skips suspended tenants | the same test: `{"old": 3}` |
| look-only writes (`if apply \|\| true`) | the same test: `{"new": 3, "old": 1}` after looking |
| the tenant swap keeps the old `sealed_with` | the same test: `{"new": 1, "old": 3}` after applying |
| `get` passes no id | the same test: *looking moved a secret* (`Unsealable`, not `UnknownKey`) |
| *(query text)* the control swap compares the whole secret, marker included | the same test: `{"new": 3, "old": 1}` because the used factor never moved |
| per-tenant failures dropped | `an_unreachable_tenant_keeps_the_rotation_unsettled`: `failed` 0, not 1 |
| an unopenable row not recorded / `forget` on it | the same test: `unsealable` empty / `{"new": 1}` with no `stranger` row left |
| control sweep skips unrecorded rows | `a_factor_enrolled_before_keys_were_recorded_is_read_and_stamped`: resealed 0 |
| `stored_secret` reads a NULL id as the current key | the same test: the unrecorded secret did not open |
| *(query text)* the control swap drops the spent-code marker | the same test: *resealing forgot which code was spent* |
| *(query text)* the enrolment upsert keeps the old `sealed_with` | `re_enrolling_after_a_rotation_records_the_new_key`: the confirm failed under the new key |
| `SealingPlan::is_settled` without `failed.is_empty()` | `an_unreachable_tenant_keeps_the_rotation_unsettled`: *a tenant nobody reached let the old key go* |
| the tenant sweep skips rows under the current id before unsealing | `a_current_id_over_the_wrong_bytes_keeps_the_rotation_unsettled`: `unsealable` held only the control row |
| the control sweep skips rows under the current id before unsealing | the same test: `unsealable` held only `acme: tax_sa.csid` |
| either sweep rewrites rows already under the current id | `a_rotation_reseals_every_tenant_on_every_cluster_and_the_control_plane`: the second run resealed 3 (tenants) / 1 (control), not 0 |

**Review found two holes in the gate, both fixed.** First, no test covered the
unreachable-tenant half of `SealingPlan::is_settled`. The only test with an
unreachable tenant also held an unopenable row, and that row alone kept the
plan unsettled. With `failed.is_empty()` deleted, the suite stayed green. That
test is now two: `what_a_rotation_cannot_finish_is_reported_and_left_alone`
keeps the unopenable row and no ghost, and
`an_unreachable_tenant_keeps_the_rotation_unsettled` has a ghost beside a fleet
whose census is settled. Second, neither sweep opened a row that was already
under the current id: the tenant query was `WHERE sealed_with <> $1`, and the
control loop `continue`d before unsealing. So `reseal check` said more than it
checked. Reuse an id for new bytes — rotate inside the month RUNNING's
`$(date +%Y-%m)` names, and rename the old entry `2026-09-leaked` because
`parse` refuses a repeated id — and every row names `2026-09` but is sealed by
bytes that id no longer means. Every sign-in, ZATCA signature and settlement
fails, and yet the sweep found nothing stale, `reseal check` exited 0, and the
leak playbook said to drop the only key that opened everything. Both sweeps now
open every row and skip only the write for current ones (`secrets.rs:452`,
`second_factor.rs:412`), so those rows land in `unsealable` and the gate stays
shut. RUNNING now says an id names its bytes for good, and that a leak in the
same month as the last rotation still needs a new id.

**One test seeds state around the product, and says so.**
`a_factor_enrolled_before_keys_were_recorded_is_read_and_stamped` nulls
`sealed_with` with raw SQL. That is what every enrolment made before `0020`
looks like, and this build cannot write one. The test's doc says it simulates
a control plane upgraded with such rows in it. Everything else is written
through the product: secrets through `enter_for_maintenance` and
`secrets::put`, and factors through `begin`/`confirm_second_factor`. The fleet
fixture now registers a second cluster at the same server, so a walk that
missed one cluster would show.

**Checked by hand as well.** `migrator chek`, `migrator reseal chek` and
`migrator check now` each print the usage and exit 2. Against a scratch control
database, `migrator` then `SEALING_KEY=new,old migrator reseal check` exits 0.
Without `SEALING_KEY` it exits 1 and names the variable. A list with one key
twice is refused at parse.

**Doc corrections.** RUNNING's environment line and SEALING_KEY paragraph now
describe the list. It has a "Rotating the sealing key" procedure, with the
retirement gate, the run once after this deploy, keeping retired keys for as
long as backups, and no rollback mid-rotation. It has "If the sealing key
leaks", decision 11's playbook: ZATCA through the manual route, since
`activate` answers 409 to a live tenant; gateway keys and card tokens at each
provider; webhook secrets; everyone's authenticator app, staff first. The
restore section's `survey` is fixed. The book is updated: `erp-eventlog`
(`seal` had lost its `key` argument in the listing, plus `unseal`, the ring,
`Census`, `reseal`), `erp-control` (`reseal_fleet`), `erp-worker` (the modes
and the refusal), `erp-api` (the variable). `secrets::forget` said a rotation
used it. No rotation does, so it now says what does. `enter_for_maintenance`'s
note on what reaches a suspended tenant names `reseal_fleet`. Tenant `0006`'s
comments ("so a rotation can find…", "What a rotation sweeps") could not be
edited, and they are now true.

**Left open.**

- **Other clusters.** `ClusterRegistry::from_env` knows only `PRIMARY_*`, so
  a tenant on any other cluster lands in `failed`, and the gate stays shut
  until the binaries learn more cluster URLs. `migrate_fleet` has the same
  limit. The result is a refusal, not a silent skip.
- **Two rollouts per rotation.** Skipping the read-only step costs visible
  500s on processes not yet updated, as RUNNING says. Nothing is lost.
- **Out of reach of the sweep:** databases no tenant row claims, and backups.
  A restored backup from before a rotation is refused until the old key is back
  in the list and `reseal` has run.
- **No test races a `put` against the swap.** Forcing the interleaving needs a
  hook the product does not have. The swap's `WHERE sealed = $old` is the
  guard, and the loser is a no-op that the next run finishes.
- **`fetch_all` per database.** There is a `ponytail:` note: page it if one
  tenant ever holds ~10^5 secrets.
- **The unrecorded arm** stays until every deployment's `reseal check` shows
  no `(unrecorded)` row.
- **Nothing stops an id being given to new bytes.** `parse` sees one list,
  not the ids of the past. The gate now catches it, since every row under the
  reused id is listed as unopenable, but the outage lasts until somebody puts
  the id back on its old bytes.
- Nothing reminds anyone to rotate (decision 11). There is no KMS and there
  are no per-tenant data keys. Both would change only `SealingKey`.

### 63 · Permission limits get a writer, and cannot lock out the one who writes them

**Built 2026-09-11.** Since Phase 5b every capability check has read
`tenant.permission_limits` (`TenantDb::permits`, `crates/erp-tenant/src/db.rs`),
and nothing wrote it: no route, no seeder, no test. `erp-rules`' crate doc said
so ("nothing writes it"); `roles.rs`, `limits.rs` and the 5b box said the
bookkeeper example worked. It could not have, twice over: nothing stored it, and
no fact said who was asking, so "entries over ten thousand are refused" refused
the owner too. Decision 9 of the day shapes the routes: both are owner-only,
and a 403 names only the capability.

**The routes** are `GET`/`PUT /v1/tenant/permission-limits`
(`crates/erp-api/src/permission_limits.rs`), a copy of the calendar's: the
version as `ETag`, `If-Match` on the write, `set_by` on the row, one wire type
`LimitsView { rules }` both ways. Both take `Allowed<ManageTenant>`. The tenant's
settings live in its own `configuration` table, so **nothing is recorded in the
control-plane audit trail**, as §62 already lists; `set_by` is the record of who
wrote it last, and there is no history.

**A writer copied from the calendar would have locked the owner out.** The
`PUT` runs `permits(ManageTenant, ..)`, so a stored `{ when: always, then:
refuse }` refuses the write that removes it. So would a stored row this build
can no longer decode: `permits` returns an error, `Allowed` turns it into 503,
and the repair route is behind it. The fix is one line in the one door every
check goes through: `if !allowed || !limits::narrows(capability)` returns the
role's answer before the read (`db.rs:156`). `narrows` (`limits.rs:85`) is false
for `ManageTenant` alone. It sits **before the read on purpose**, which is what
closes the second path. It is safe because only an owner holds `ManageTenant`,
`LastOwner` keeps one, and `/v1/tenant/*` belongs to no module, so no module
override reaches it. The price is that an owner cannot limit their own
administration of the tenant by branch or amount; nobody asked. I rejected a
write-time check ("refuse rules that would refuse the writer") because it only
holds while the edge supplies no fact the owner might lack. It also closes §62's
open item: `ManagesTenant` and `Allowed<ManageTenant>` now agree.

**Limits cannot exist unchecked.** `Limits::new` (`limits.rs:138`) returns
`Result<_, Unusable>`, where `Unusable { rule, why }` names the rule the tenant
wrote rather than its index. `#[serde(try_from, into)]` (`:115`) replaces
`transparent`, so reading a row validates too. The stored JSON is the same bare
array, so no migration. The old `validate()` had no production caller and is
gone. So a row a later build cannot use is refused where it is read (503, L6),
and the owner can still replace it. That makes `registry()` expand-only, and its
doc says so.

**A misspelt value is refused, not stored.** `capability == "post_entires"` had
the right fact and the right kind, validated, and never fired: exactly what
`facts.rs` exists to prevent. `FactRegistry::one_of` (`erp-rules/src/fact.rs:158`)
declares a text fact with its only values. `DynCondition::validate` refuses any
other value with the new `Invalid::NoSuchValue`, and refuses ordering one
(`condition.rs:134`), because `role < "clerk"` compares spellings. `capability`
lists `Capability::ALL` (new, `roles.rs:95`) filtered by `narrows`, so naming
`manage_tenant` is refused when written instead of stored to never fire.

**The `role` fact** is added in `permits` from `access.role_in(module)`
(`db.rs:178`): the role `allows_in` just checked, a module's own where the
tenant set one. It is supplied there because nothing else knows it, and
`facts.rs` finds it there. "Deliberately three" became four.

**Refusals** are three new request codes with en and ar text:
`request.no_such_fact` (with the facts a rule may name), `request.no_such_fact_value`
(with the values) and `request.rule_cannot_compare`. A `covers` condition
deserialises in the API build, because feature unification turns `spans` on
there. It gets the unknown-fact code, which is true: no permission check
supplies a window of time.

**The matrix:** `permission_limits` and `set_permission_limits`, both OWNER.
That makes 244.

**Falsified**, each by breaking Rust and watching the named test fail, then
restoring it and watching it pass:

| broke | failed |
|---|---|
| `permits` without the `narrows` short-circuit | `no_limit_locks_the_owner_out_of_its_limits`: the owner's `GET` got *403 manage_tenant*; `limits_this_build_cannot_read_lock_nobody_out_of_repairing_them`: 503 |
| the short-circuit moved after the read | `limits_this_build_cannot_read…` alone: 503, not 500 (the always-refuse test still passed, which is why both exist) |
| `permits` without the `role` fact | `a_bookkeeper_is_refused_an_entry_over_the_limit_the_owner_wrote`: the 20,000 entry got 200; `every_declared_fact_is_assembled_somewhere`: *declares `role` and nothing anywhere supplies it* |
| the ledger's `still_permits` call removed | `a_bookkeeper_is_refused…`: 200. The first end-to-end test of the narrowing engine |
| `Limits::new` skips validation | `permission_limits_are_a_versioned_setting_that_refuses_impossible_rules`: 204, not 400; three `limits.rs` unit tests |
| `registry()` without the `narrows` filter | `a_rule_naming_a_capability_that_does_not_exist_or_cannot_be_limited_is_refused_when_written`: *never narrowed: Ok*; the HTTP test's `manage_tenant` row: 204 |
| `capability` declared as plain text | the same two: *a typo: Ok*, and 204 |
| `#[serde(transparent)]` back | `stored_limits_that_no_longer_validate_are_refused_when_read`; `limits_this_build_cannot_read…`: **200**, the unlimited answer |
| the value-list branch in `validate` | `a_text_fact_with_known_values_refuses_one_it_does_not_know`: `Ok(())` |
| listed facts left orderable | the same test: *Lt should not order a listed fact* |
| `GET` on `Allowed<Read>` | `every_role_against_every_endpoint`: *accountant → GET /v1/tenant/permission-limits answered 200* |

**One test seeds state around the product, and says so.**
`limits_this_build_cannot_read…` writes an undecodable row with
`configuration::set` through maintenance entry, which is how
`configure_vat_reasons` already seeds settings. No SQL is involved. It stands in
for rules a build with a larger registry saved, which this build cannot
produce. The e2e tests write limits only through the `PUT`.

**Doc corrections.** `erp-rules/src/lib.rs` ("nothing writes it"), `roles.rs`
(now names the route, the `accountant` role, and why "their own branch" still
waits), `limits.rs` ("Deliberately three"; a new section on the one capability
a limit cannot touch, and one on expand-only), the 5b boxes (the bookkeeper
claim is marked *false until §63*), `Allowed::still_permits` ("the three"),
`ManagesTenant`'s "limits are not consulted", `facts.rs`, the book (`erp-tenant`,
`erp-web`, `erp-api`, `http.md`) and ARCHITECTURE §5.6. `erp-api` now depends on
`erp-tenant` and `erp-rules` directly, as `erp-web` already did.

**What review found: the bookkeeper could walk around the limit it names.**
Twice, both failing open (L6).

- **By currency.** `Value::compare` has no answer for two currencies, and
  `holds` turned no answer into *false*, so `amount >= 10,000 SAR` did not
  match 5,000,000 USD. An accountant holds `manage_accounts`, opens two dollar
  accounts, and posts. The first draft listed this as "a rule in `USD` never
  fires on an entry in `SAR`", a mistake of the author's; it was a hole for the
  person limited. The root is that a condition had two answers where it needs
  three. `DynCondition::decide` (`erp-rules/src/condition.rs`) answers
  `Option<bool>`: a missing fact is still `false` (the edge supplies no amount,
  and must not be refused for that), an incomparable one is `None`, and `All`,
  `Any` and `Not` carry `None` up the three-valued way, so `Not` of no answer
  is not a yes. `holds` is `decide == Some(true)`, unchanged except for `Not`
  over an incomparable amount, which only limits ever compare. Who acts on a
  rule decides what no answer means: `Rules::explain_undecided`
  (`rule.rs`) is the one walk, and `explain` is it with *does not apply*.
  `Limits::explain` (`limits.rs`) passes `then == Refuse`, and `narrow` reads
  its answer from there, so the two cannot disagree. A refusal that cannot be
  judged refuses; an exception that cannot be judged excepts nothing. A tenant
  with dollar and riyal books writes an `allow` for dollars above the riyal
  refusal, and `an_exception_in_another_currency_leaves_the_refusal_below_it_to_decide`
  pins that it works. I rejected refusing, at write time, any amount not in
  the tenant's currency: `Limits::new` does not know that currency, and the
  entry would still arrive in dollars.
- **By reversal.** `reverse_entry` took `Allowed<PostEntries>` and never asked
  again with an amount, so the owner's 20,000 went through backwards. The
  check both routes share is now `within_limits` (`modules/ledger/src/http.rs`),
  and the reversal route reads the original first with `posted_lines`
  (`commands.rs`), outside the reversal's transaction, which is safe because a
  posted entry's lines never change. An entry that does not exist is 422 there,
  as it was.

The fix corrected `Value::compare`'s doc, which said the registry refuses two
currencies at authoring time; it cannot, because an amount's currency is known
only when one is asked about. `http.md`, the route's schema doc, `roles.rs`,
`limits.rs`, the 5b box and the book's `erp-tenant` and `ledger` pages now say
which checks supply an amount and what an unjudgeable one does. The HTTP guard
is `a_bookkeeper_cannot_walk_around_the_limit_by_reversal_or_currency`, which
shares its setup with the first test (`a_bookkeeper_limited_to_ten_thousand`)
because the two together outgrew clippy's function length.

| broke | failed |
|---|---|
| `Limits::explain` back on `Rules::explain` (no answer does not apply) | `an_amount_in_a_currency_the_limit_does_not_name…`: *no answer is not under it*; `an_exception_in_another_currency…` on 5,000,000 USD; `a_bookkeeper_cannot_walk_around_the_limit_by_reversal_or_currency`: **200** *in another currency* |
| an undecided `allow` counted in too (`\|_\| true`) | `an_exception_in_another_currency…`: the riyal 20,000 was let through by the dollar exception |
| `All` answering `false` on an unknown part | the same two unit tests, `an_amount_in_another_currency_is_no_answer_and_stays_one` (`Some(false)`, not `None`), and `a_bookkeeper_cannot_walk_around…`: 200 |
| `Not` back on `!holds` | `an_amount_in_another_currency_is_no_answer…`: *not of no answer* `Some(true)` |
| the reversal's `within_limits` call removed | `a_bookkeeper_cannot_walk_around…`: *20,000 backwards* 200. The bookkeeper reversing their own 4,000 is the contrast |

**Left open.**

- **No per-member branch.** "Only their own branch" needs a record of which
  branch is somebody's; the `branch` fact is the request's `X-Branch`, which the
  caller chooses.
- **A 403 does not say which rule refused** (decision 9). `Limits::explain`
  exists; returning its name changes `permits`' signature and both callers.
- **An unusable stored row cannot be read back.** `GET` answers 500 and the
  owner replaces the rules blind. Showing the raw JSON would need a second,
  unvalidated read path.
- **Keys scoped `*:manage_tenant` can write limits**, as they can the calendar.
- **Only the ledger's two routes supply an amount.** Every other module that
  posts — sales, purchases, prepaid, pos, payroll, payments and more — does it
  through `post_entry_in` or `reverse_in`, from routes that never call
  `still_permits`, so a bookkeeper limited to ten thousand riyals can still
  issue a fifty-thousand invoice. That is a
  question per module — what an invoice's amount is, and whether issuing one
  is "posting an entry" — and `post_entry_in` cannot answer it, because it has
  a connection and no `Access`. `http.md` says so. *Answered for sales documents
  by §68, with a different control*: the owner's per-document limit, judged
  inside the `sales` roots where an invoice's total exists, stops the
  fifty-thousand invoice, credit note or refund. Permission limits still judge
  only the ledger's two routes, and supplier bills, payroll and pay-outs are
  still unlimited by amount.
- **A tenant with a refusal in riyals is refused every dollar entry that rule
  would judge** until it writes the dollar `allow` above it. Refused, visibly,
  and fixed by the owner, which is the direction L6 asks for.
- **No dry run**, still the 5b box it was.

### 62 · The audit trail gets readers, and a column that says whose it is

**Built 2026-09-11.** The control plane has recorded every change to members,
keys, domains, modules, staff and a tenant's status since Phase 1, and nothing
read it. The only readers were raw SQL in tests. So ARCHITECTURE §1.9's "visible
to the tenant" was not true, §59's suspension reason went to an owner who had no
way to see it, and "who suspended this person" was a question for psql.
Decisions 5, 6 and 12 of that day shape this: handles stay in the trail, a
person reads their own, and a suspended tenant's owner reads why.

**The root: a row did not say which tenant it concerned.** "The entries about
tenant T" meant an OR across the subject, a `tenant` key some writers put in
`detail`, and a join to `api_key`. `api_key.revoked` and `api_key.rotated` had
no tenant anywhere in the row, so no query could find them. Any new writer that
left the tenant out of `detail` would have dropped out of the tenant's view.
`migrations/control/0019_audit_tenant.sql` adds `tenant_id`, with no foreign
key, because the entries outlive the tenant: `tenant.abandoned` and
`tenant.demo_reaped` are the only record a deleted tenant existed. It backfills
the old rows from the three places the tenant used to be, and adds an index
for the tenant view and one on `on_behalf_of` (see below). `record()`
(`lib.rs:2311`) takes `tenant: Option<TenantId>` after the actor, so every one
of the 31 writers now states it. The compiler found them all, batch 1's
included: `moved()` passes the tenant, `tenant.abandoned` passes it, staff
changes and `dealt_with` in `dead_letters.rs` pass `None`. The detail shapes are
unchanged, so old and new rows read alike.

**The column would have opened a hole in the trigger.** `0007`'s function
allows exactly one UPDATE, an actor nulled, and it lists the columns that must
not change. It did not know `tenant_id`. So an UPDATE that moved an entry into
another tenant's trail, or out of every tenant's trail, would have passed. `0019`
re-pins the function with `tenant_id` in the list, after the backfill (which
relies on the old function to pass).

**One query, three readers** (`lib.rs:2346`–`2445`), newest first and
keyset-paged on the entry's id. The id is an identity column allocated in
order; `at` is a transaction timestamp and skews at least as much.

- `tenant_audit`: `tenant_id = T`. Entries about a person alone are not in it,
  even when the person is a member, because an account's suspension may be
  about another company. The members list already shows `suspended`.
- `identity_audit`: entries whose subject is the person, or whose actor or
  on-behalf-of is them. That OR needed the third column indexed, or it scans
  the whole trail.
- `platform_audit`: either filter, both (the entries in each), or neither
  (everything, including what concerns no tenant: staff changes, dead letters,
  clusters, signups nobody confirmed).

**Who is named.** On the first two readers, an actor's login is filled in only
where they are, or were, a member of the tenant the entry concerns.
`membership` keeps revoked rows, so somebody who has left is still named for
what they did. Staff who are not, and never were, members of the tenant show
up on `tenant.support_access` and `tenant.suspended` by id alone. A staff
member who also is, or was, a member of it is a co-member like any other and is
named. Decision 6 asks exactly that for the personal view, and it
is the same rule for the tenant's. Staff see every actor's login. An erased
actor comes back as a null actor, which is what a system action looks like.

**The owner's route cannot go through `enter`,** and §59 said why:
`Allowed<ManageTenant>` goes through `Tenant` and `enter`, which answers any
tenant that is not active with 503. `enter`'s checks now live in a private
`admitted(identity, tenant, serving)` (`lib.rs:495`). `enter` calls it with
`serving` true and then opens a connection, as before, in the same order. The
new `admit` (`lib.rs:485`) calls it with `serving` false and opens nothing. So
the answer to "may this person act here" is still written once, with the
identity, the membership and the tenant's second-factor rule. Only "is it
serving" and the connection are left out.

`erp_web::ManagesTenant` (`extract.rs:1034`) is the door: `admit`, then
`Allowed`'s two gates in `Allowed`'s order, a key's scopes and then the role,
with the same two 403s. Those 403s are now the functions `not_permitted` and
`out_of_scope` (`:981`, `:995`), which `Allowed` uses too, so the wording
cannot drift. It hands out no `TenantDb`. It does not consult limits, because
they are kept in the tenant's database, which it does not open. Its one route is
`GET /v1/audit`, beside `GET /v1/sessions/current/audit`, which takes
`Authenticated` and refuses a key, in `crates/erp-api/src/audit.rs`. `GET /v1/platform/audit`
(`platform.rs:422`) takes `Staff<ReadAuditTrail>`, the fourth `Power` marker,
so support and superadmins read it. All three answer in one `AuditView`, paged, and share
`resume`, which reads a cursor through `erp_control::audit_position`. A cursor
that is not one part holding an integer is `400 request.invalid_cursor`, never
the first page again. The matrices got their rows: `audit_trail` OWNER and
`my_audit_trail` ALL_ROLES (242), and `platform_audit_trail` `read_audit_trail`
(ten).

**A key needs `*:manage_tenant`** to read a tenant's trail: the wildcard
`Allowed` asks of every route outside a module. A key of a suspended tenant that has that
scope can still read it, as the owner can. Keys never checked the tenant's
status; `enter` did that for them.

**Handles stay (decision 5).** `invitation.created`, `invitation.accepted` and
`signup.confirmed` carry the handle in `detail`, `signup.requested` uses it as
the subject, and erasure nulls actors only. `erase_identity`'s rustdoc said the
trail stays "with this person's name removed from it". It now says the link is
removed and the address is not, and that this is the product owner's call to
keep a legal record of who was given access. The book says the same. Nothing is
redacted.

**What the trail does not record,** so nobody reads its silence as an answer:
signing in and out, passwords changed or reset (`passwords.rs`), second
factors enrolled or removed (`second_factor.rs`), and a tenant's second-factor
rule (`set_second_factor_requirement`, which changes the tenant row and records
nothing). A tenant's settings, permission limits among them, live in its own
`configuration` table with its own `set_by`. Its business lives in its event
log. RUNNING's platform section says this too.

**Doc corrections.** `record()` said the table refuses `UPDATE` "so this is the
only way its contents change". That has been false since `0007`, and it now
names the one exception. `access()` said it was "the same answer `enter`
decides on", but it is the cached membership alone, without identity status or
the second-factor rule. It now says so and points at `admit`. §59 and the texts
it left said "nothing shows it yet": the suspend route's doc, `http.md` and
RUNNING now name `GET /v1/audit`. Both also said a suspended tenant's members and
keys get 503 "from the next request", and they now say everywhere but the trail.
The book's "every power but `ReadAuditTrail` has a door" is gone, and ARCHITECTURE
§1.9 says the tenant sees support access. `0018`'s column comment ("Also
recorded in the audit trail as tenant.suspended") was already true and stays.

**Falsified**, each by breaking Rust and watching the named test fail:

| broke | failed |
|---|---|
| `admit` refuses a tenant that is not serving (`admitted(.., true)`) | `the_owner_of_a_suspended_tenant_reads_why`: *get /v1/audit answered 503* |
| `moved()` records `None` for the tenant | the same test: the newest entry was `membership.granted`, not `tenant.suspended` |
| `revoke_key` records `None` | `an_owner_reads_their_tenants_trail_and_nobody_elses`: *no api_key.revoked* |
| `tenant_audit` drops its tenant | the same test: *not acme's*, a platform `membership.granted` |
| `tenant_audit` names every actor | the same test: *a customer was shown a staff member's address*, `support@erp.test` |
| `ManagesTenant` skips the key's scopes | the same test: `*:read` answered 200 |
| `ManagesTenant` skips the role | `every_role_against_every_endpoint`: *accountant → GET /v1/audit answered 200*; `…reads_why`: the clerk got 200 |
| `identity_audit` drops its person | `a_person_reads_what_was_done_to_them_and_by_them`: *not about sara, nor by her* |
| `identity_audit` names every actor | the same test: *a staff member's address was shown to a customer* |
| `platform_audit` names only co-members | `support_reads_the_whole_trail_narrowed_by_tenant_or_person`: billing's handle null; `a_person_reads…`: admin's null |
| `platform_audit` drops the tenant filter | `support_reads…`: an entry with no tenant in globex's view |
| `ReadAuditTrail` asks for `SuspendTenants` | `every_platform_role_against_every_platform_endpoint`: *billing → GET /v1/platform/audit answered 200* |
| `audit_position` reads a bad cursor as the top | `the_audit_trail_pages_without_losing_or_repeating_entries`: *not a number*, 200 |
| the keyset resumes at `before + 1` | the same test: *paging lost, repeated or reordered an entry* |

**One SQL falsification, for review.** The trigger's new line is SQL, and SQL
cannot be broken in Rust. I deleted the line from `0019`, touched `lib.rs`, and
watched `the_audit_trail_is_still_append_only_for_everything_else` fail with
*an entry was taken out of its tenant's trail*. Then I restored the file byte
for byte, and the test passed. No `query!` text changed, so `.sqlx/` was not
involved. The broken template database is left on the test cluster for `just
clean-databases`.

**Not tested: the backfill.** The testkit migrates empty databases, and seeding
pre-`0019` rows would take SQL the product can no longer produce. A cast that
fails aborts the migration rather than leaving a row out. Every writer stating
a tenant is enforced by the compiler, not a test. (§66 made the backfill one
`UPDATE` through `audit_entry_tenant`, the function the insert trigger uses,
and the trigger is tested, so the rules are tested even though this statement
is not.)

**What review found.** Two things, both fixed.

- *An API key could read the personal trail.* The route took plain
  `Authenticated`, which admits keys. A key's identity is the subject of the
  `membership.granted` that issuing it writes, with the issuing owner as actor
  and the tenant set, so the co-member rule named the owner. A key scoped only
  `booking:read`, refused `/v1/members`, got the owner's address from
  `/v1/sessions/current/audit`. The route now answers a key `403
  keys.not_a_person` (`audit.rs:151`), a new code with en and ar text that
  names no scope because none reaches it. The check sits in the one handler
  that needs it, as `Staff<P>` refuses keys in its extractor; the other
  `/v1/sessions/*` routes still take keys, and are noted below.
- *The docs said a customer never sees a staff member's address.* The query
  names any actor who is, or was, a member of the entry's tenant, and staff can
  be members: `a_person_reads…` grants support to an acme clerk. The docs
  (`lib.rs:2383`, `model.rs:186`, `audit.rs`, this section, ARCHITECTURE §1.9,
  the book) now say staff who never were members appear by id alone, and one
  who was is named like any co-member. That is decision 6's rule, so the code
  stays.

Falsified: with the key check in `my_audit_trail` made unreachable,
`a_key_reads_no_personal_trail` failed with *a key read a person's trail*:
status 200, and the owner's address in the body.

**No state is seeded by SQL.** Every entry the new tests read was written by
the product: role changes, keys, staff grants and a suspension over HTTP,
`enter_for_support` through the control plane. The extended append-only test
uses raw `UPDATE`s only to show that the database refuses them.

**Left open.**

- ~~**Rolling deploys.** A pod on the build before `0019` inserts without a tenant
  during the overlap, and the trigger forbids filling it in later. There is a
  `ponytail:` note at `record()` naming the fix (a BEFORE INSERT default from
  `subject_type`).~~ Closed by §66: `0019` itself fills the tenant on insert,
  by the backfill's own rules, so there is no window.
- **Not atomic.** `record()` still runs on the pool after the act commits, as
  §61 noted, so a crash between the two leaves an unrecorded act.
- **Infrastructure names reach customers.** `tenant.registered` carries its
  cluster in `detail`, and `detail` is returned raw. The owner's view shows it.
  It is low-sensitivity; filter it per action if it matters.
- **No retention.** The trigger refuses every DELETE, so a limit on how long
  entries are kept would need a new permitted shape and a sweep.
- **Filters, and a way to find a tenant.** The tenant and personal views take no
  action or subject filter. Staff still find a tenant's id in psql.
- ~~**Limits and decision 9.** `ManagesTenant` consults no limits, while
  `Allowed<ManageTenant>` does. Until the limits item makes `ManageTenant`
  unnarrowable, a limit written against it narrows `/v1/members` and not
  `/v1/audit`.~~ Closed by §63: no limit narrows `ManageTenant`, so the two
  agree.
- `signup.requested` names a handle, not an identity, so it is in no personal
  view.
- **Keys on the other personal routes.** `/v1/sessions/second-factor*`,
  `/v1/sessions/current/password` and `DELETE /v1/sessions/current` take
  `Authenticated` too, and so admit a key acting on its own identity. None
  reveals anybody else, which is why only the trail refuses one. A `Person`
  extractor for all of them is the root fix if that changes.

### 61 · The control plane's outbox gets the watchers a tenant's has

**Built 2026-09-11.** The control plane has had an outbox since invitations
started sending email, and it now carries every signup confirmation,
invitation, reset link and sign-in code. It got the dispatcher and none of the
three things that watch a tenant's outbox:

- **Nothing checked it.** `outbox_health` had one caller, `kernel_findings`,
  which takes a `TenantDb`. A deployment with no `SMTP_URL` held every
  top-of-funnel email unsent, and said so once, in a warning at start-up.
  `dispatch.rs:258` promised that "the backlog-age health check fires", which
  was true for tenants only, and RUNNING's SMTP paragraph leaned on it.
- **Nothing handled its dead letters.** `GET /v1/effects/dead` and its requeue
  sit behind `Allowed<ManageTenant>` and read `tenant.db`. For the control
  plane the way back was hand-written SQL, the defect `effects.rs` was written
  to remove.
- **Nothing forgot its receipts.** `sweep_delivered` ran only inside
  `Retention::sweep(&TenantDb)`, so every delivered email and text, each an
  address and an expired credential, was kept for ever.

**All three are the existing functions, pointed at the control pool.** They
already take a bare connection, and the two tables are the same
(`the_two_outboxes_are_the_same_table`), so nothing is copied.

- **Health.** The outbox's two findings moved into `outbox_findings(conn)`
  (`health.rs:197`). The tenant check calls it as before, and
  `HealthJob::control_findings` (`:146`) calls it on `control.pool()`.
  `HealthJob` is also a `PlatformJob` now, `control.health` (`:257`),
  registered beside its tenant registration (`bin/worker.rs:146`). Platform jobs
  run every claim cycle, a quarter of a second apart on an idle fleet, so it
  goes through the same `claim_turn`, now keyed by `Option<TenantId>` with
  `None` for the control plane. A tenant being checked cannot use up the
  control plane's turn. A finding logs the tenant check's `invariant violated`
  with `plane = "control"`, so an alert on that message catches it. Every
  worker process runs the check, so N workers log a finding N times per
  interval; a `ponytail:` comment says when to give it a lease.
- **Retention.** `Retention::sweep_control(control, now)` (`retention.rs:63`)
  is `sweep_delivered` with the tenant plane's thirty days, and the reaper
  calls it after the reset links (`reaper.rs:89`). It is not a platform job:
  `outbox` has no index on `delivered_at`, and a sequential scan four times a
  second on every worker is not a sweep. It also bounds the table the new
  health check counts.
- **Dead letters**, over HTTP (decisions 1, 2 and 10). Three routes on
  `Staff<HandleDeadLetters>`, a new `Power` marker, so support and superadmins:
  `GET /v1/platform/effects/dead`, `POST …/{id}/requeue` and `DELETE …/{id}`
  (`platform.rs:309`, `:343`, `:379`). They answer in the tenant route's
  `DeadLetterView`, the same schema, and share its id parsing and its
  `404 request.no_such_dead_letter` (`effects.rs`), so no new message codes.
  There is no nudge: platform jobs run every cycle anyway. Behind them,
  `crates/erp-control/src/dead_letters.rs` calls the `erp_eventlog` functions
  on the control pool and records `effect.requeued` or `effect.dismissed`
  naming the staff member, with `{kind, idempotency_key}` as the detail and
  nothing else. The payload is never recorded: it holds the address and, for a
  reset or a code, the credential. The key says which row the effect was about
  (`reset:<id>`), and that id is the row's, not the token.

**`erp_eventlog::dismiss` is new** (`effect.rs:307`), beside `requeue`. It
deletes a row only when `dead_at IS NOT NULL` and returns its kind and key. It
exists because a sign-in code expires in five minutes and a reset link in an
hour, while the platform dispatcher's retries (16 attempts, backoff capped at
an hour) take about four hours to give up. A code or reset letter that died of
an outage had expired long before it died. Until now the only way to clear one
was to requeue it, which mails a dead credential, and one left uncleared keeps
`no_dead_letters` firing every five minutes for good, which teaches people to
ignore it. The requeue route's doc, the book and RUNNING all say to dismiss
those. `requeue` now returns `Option<Handled>` (kind and key) instead of
`bool`, so its caller can record what it touched; the tenant route reads
`.is_some()` and behaves as before. The tenant surface does not get a dismiss
route here, but the function is there for it.

**Recorded after the act, not in its transaction**, like every other audited
write in the control plane (`suspend_tenant`, `grant_staff`), because `record()`
takes the pool. A failure between the two leaves an unrecorded requeue or
dismissal behind a 500. For review; the audit item changes `record()` anyway.

**Doc corrections.** `effect.rs` had a stray first line on `DeadLetter`'s doc,
"Counts an operator, and the per-tenant health check, cares about"; it belongs
to `OutboxHealth` and now says either plane. `dispatch.rs:258` is true now and
says so. ARCHITECTURE §7 says the two outbox invariants are asserted for the
control plane too. RUNNING's SMTP paragraph says what gets logged, the reaper
paragraph names the new sweep, and the platform section says how to handle the
control plane's dead letters. The book said `HandleDeadLetters` had no door. The
comment in `migrations/control/0008_outbox.sql` calling dead letters "a
per-tenant health assertion" stays, because sqlx checksums migration files.

**Falsified**, each by breaking Rust and watching the named test fail:

| broke | failed |
|---|---|
| `dismiss` falls back to deleting the row whatever its state | `only_a_dead_letter_can_be_dismissed`: *the delivered effect is not a dead letter, and was dismissed anyway*; `support_requeues_and_dismisses_…`: dismissing the pending one answered 204 |
| a dismissal not recorded | `support_requeues_and_dismisses_…`: the audit trail held only `effect.requeued` |
| the dismiss route records `Actor::system()` | the same test: `effect.dismissed` with no actor |
| the dismiss route on `Staff<SuspendTenants>` | `every_platform_role_against_every_platform_endpoint`: *billing → DELETE … answered 400, and the table says refused* |
| `control_findings` answers `Ok(vec![])` | `a_dead_letter_in_the_control_plane_is_a_finding`: `[]`, not `["no_dead_letters"]` |
| the tenant tick takes the control plane's turn (`claim_turn(None)`) | `the_control_plane_has_its_own_turn_on_an_interval`: the first platform tick answered `Idle` |
| the platform tick without `claim_turn` | the same test: the second tick checked again and failed |
| `sweep_control` without `- DELIVERED_EFFECTS` | `the_control_plane_forgets_what_it_delivered_and_nothing_else`: *a receipt younger than the window is kept*, 1 not 0 |
| `sweep_control` answers `Ok(0)` | the same test: *the delivered one goes*, 0 not 1 |
| the English 404 back to "already requeued, or never given up on" | `support_requeues_and_dismisses_…`: the requeue after a dismissal read *already requeued*, with no *dismissed* in it |
| the Arabic 404 without أو حُذفت | the same test, on the Arabic detail |

**No state is seeded by SQL.** Dead letters are made the product's way: a
relay that refuses (`MailError::Refused`, which is permanent) in `erp-worker`,
and a handler that answers `Permanent` in `erp-api` and `erp-eventlog`. The
SQL in these tests only reads. The own-turn test closes the control pool, so
that a check which runs is an error; that is how it sees whether the check ran.

**Review found three places that still said a dead letter is only ever
requeued**, one root: `dismiss` added a way out and the text written before it
was not reread.

- The `404 request.no_such_dead_letter` both new routes reuse said "it was
  already requeued, or never given up on", in English and Arabic
  (`erp-web/src/messages.rs:679`, `:686`). After a dismissal that reason is
  false. It now says "requeued or dismissed" (أو حُذفت), which is still true of
  the tenant route. The test asserted only the code, so it passed on the wrong
  text; it now reads the detail after a dismissal in both languages.
  `requeue`'s own rustdoc said the same and is fixed.
- Both outbox migrations (`control/0008:96`, `tenant/0003:84`) say of
  `dead_at`: "Never deleted — a dead letter is evidence". `dismiss` deletes
  them. The files stay, for the checksums; `dismiss`'s rustdoc and the book's
  dead-letters section now say it is the one way a dead letter is deleted, that
  the migrations predate it, and that the kind and key survive in the caller's
  record (`effect.dismissed` on the control plane).
- The book said the tenant's and the control plane's dead-letter routes "are
  the same three functions". The tenant surface has no dismiss route. It now
  says the platform uses all three and the tenant the first two.

The two prose fixes have no test; the one behaviour under them, that `dismiss`
touches only a dead row, is `only_a_dead_letter_can_be_dismissed`.

**Left open.** A dismiss route on the tenant surface. The N identical log
lines from N workers. Nothing shows `effect.*` entries until the audit reader
lands *(since §62: `GET /v1/platform/audit` does)*. The platform matrix sends the two id routes a tenant UUID, so an allowed
caller gets a 400 rather than a 404; the matrix only asks whether it was a 403.

### 60 · Signups cut off mid-build

**Built 2026-09-11.** The plan box asked for a sweeper for tenants stuck in
`provisioning`, on the premise that a crash strands them. A crash does, but the
common cause was our own API: `bin/api` wraps every route in a 30-second
`TimeoutLayer`, and a client can close the tab sooner. Either one drops the
handler's future, and `provision` ran inside it. So the compensation in
`provision` and the unclaim in `confirm_signup` were dropped with it. The tenant
stayed `provisioning` for ever, often with a migrated database behind it. Its
slug stayed taken, because `request_signup` checks the slug whatever its status,
and the customer's link stayed spent. Decision 8 says to finish the build
anyway, and to keep the sweeper for real crashes.

**The build runs on a task the request only waits for.** `confirm_signup`
(`signup.rs:409`) now takes `self: &Arc<Self>`, spawns the claim, the build, the
compensation and the audit entry onto a task of their own, and awaits the
handle. A dropped request drops only the wait. The task still ends in one of the
two outcomes it would have reached anyway: an active company that the person
logs into with their password, or a failure undone and the link unclaimed. The
spawn is in the control plane, not the handler, because that is where the
compensation lives. The only production path to `provision` is that handler,
and the demo seeder reaches it over HTTP too. Invitations build nothing. The
handler's doc, and so `openapi.json`, says that a timeout is not a failure.

**The unclaimed link did not work, and the new test found it.** For a new
account, the first attempt creates the identity and its login. After a failure
the link was put back, but the next click tried to create the login again and
got `HandleTaken` for its own handle. So "the link still works" was true only
for an address that already had an account. `build_signup` now writes the new
identity onto the request as soon as the identity exists (`signup.rs:507`),
setting `identity_id` and clearing `password_hash`, which the one-owner CHECK
allows. A retry then takes the existing-account path. A side effect: a
confirmation that got as far as the account no longer leaves a password hash on
the request.

**`abandon` is safe to call with a stale value, so it is now public**
(`provision.rs:500`). It had trusted the `Tenant` it was given: if the status
said provisioning, it dropped the database first and then deleted the row with a
status filter. A sweep that read a tenant just before it activated would have
dropped a live company's database and kept a row pointing at nothing. Now:

- **The row is the lock.** `abandon` re-reads the row `FOR UPDATE` while it is
  still `provisioning`, and holds it until the `DELETE` commits.
  `activate_tenant`'s `UPDATE` waits on it, and so do the foreign-key checks
  behind `enable_module` and `grant_membership`: `FOR UPDATE` conflicts with the
  `KEY SHARE` they take. If the row is not there, `abandon` returns `Ok(false)`
  and touches nothing. A provisioner that loses the race is refused at
  activation. That was already true through §59's `moved()`, which answers
  `NoSuchTenant` for a row that is gone, so this item's own change to
  `activate_tenant` was not needed.
- **The database is asked what is in it**, through the same `occupancy_of` the
  orphan sweep uses. If it has events or a setting a person chose, `abandon`
  refuses with `Corrupt`. A provisioning row over a database like that is a
  control plane restored to a point behind its database, not a dead signup. A
  database it cannot open is refused too (L6).
- **A database that does not exist is empty.** `occupancy_of` answers `Ok(None)`
  for SQLSTATE 3D000 on connect (`provision.rs:973`), which is the crash before
  `CREATE DATABASE`. For the orphan sweep this changes nothing: a database
  listed a moment ago and gone now was going to be dropped with `IF EXISTS`
  anyway.
- `abandon` records `tenant.abandoned`. It used to leave no entry.

**The sweep** is `reap_stuck_provisioning(grace, limit)` (`provision.rs:781`).
It has the same shape as `reap_expired_demos`: it lists, abandons each one, and
logs a failure and moves on. The reaper runs it after the demo sweep and before
the orphans (`reaper.rs:102`). The order does not matter for correctness, because
it drops a database and the row that names it together.
`PROVISIONING_GRACE_SECONDS` is 15 minutes (`provision.rs:840`). The doc says
what the grace is for: **not safety** (the lock and the look inside provide
that), but not failing a signup that is still building. That means a spawned
build, or a statement still running on the server after the process that sent
it died. A stuck name is held for up to the grace plus the reaper's schedule,
and RUNNING says to schedule it at least hourly. A link spent by a build that
died stays spent after the sweep (decision 13); the person asks again.

**Several docs said something false.** §56 and the orphan doc in `provision.rs`
both said *"a provisioning that dies leaves a row with no database — which
`abandon` handles"*. That was wrong twice: a cut-off after `CREATE DATABASE`
leaves the database too, and `abandon` only ran in-process. RUNNING said the
same. `provision.rs`'s module doc said every step is idempotent, *"so 'recover'
and 'retry' are the same operation"*. Nothing retries: each `provision`
registers a new `TenantId`. That bullet is replaced by one about the sweep. The
book said *"Signup returns immediately and the provisioner works in the
background"* and *"`sign_up` is what `confirm_signup` calls"*. Neither was true;
both are corrected.

**Two tests drop the request, at the control plane**, because only there can a
test pass a module that sleeps. Over HTTP, modules are resolved from the
build's own list. `confirm_and_cut_off` drops `confirm_signup` inside a
`select!` at the moment the tenant row exists. That is mid-build by
construction, because the module sleeps after the row is written. It is the
same drop the `TimeoutLayer` does.

**Falsified**, each by breaking Rust and watching the named test fail:

| broke | failed |
|---|---|
| `confirm_signup` builds inline rather than on a task | `a_confirmation_cut_off_mid_build_still_ends_in_a_working_company`: *the tenant activating never happened*; `…that_fails_is_still_compensated`: *the link working again never happened* |
| the new identity not written onto the request | `…that_fails_is_still_compensated`: *the same link builds the company: `Auth(HandleTaken("owner@acme.test"))`* (also the failure before the fix) |
| the sweep's loop skips `abandon` | `a_build_that_died_mid_provision_is_swept_and_its_name_freed`: reaped 0, not 1 |
| `WITH (FORCE)` removed from `drop_database` | the same test: reaped 0; the killed build's `pg_sleep` backend was still in the database |
| the 3D000 arm removed | `a_provisioning_that_never_got_a_database_is_swept`: *a database that does not exist holds nothing*, 0 not 1 |
| `tenant.abandoned` recorded under another name | the same test: *an abandonment is recorded*, 0 not 1 |
| the grace bound as 0 | `a_provisioning_younger_than_the_grace_is_left_alone`: reaped 1 |
| the locked re-read ignored | `a_tenant_that_activated_after_the_sweep_read_it_survives`: *an active tenant is not abandoned* |
| `activate_tenant` tells `moved()` a row changed | `activating_a_tenant_the_sweep_abandoned_is_refused`: activation answered `Ok` |
| the occupancy refusal made a pass-through | `a_provisioning_tenant_with_events_is_never_dropped`: `abandon` answered `Ok` |
| *(review)* the locked re-read run on the pool, outside the transaction | `an_activation_waits_while_abandon_looks_and_then_finds_nothing`: *activated while abandon was looking: `Ok(Ok(()))`* |
| *(review)* the cannot-look-inside refusal made a pass-through | `a_provisioning_tenant_whose_database_cannot_be_opened_is_never_dropped`: `abandon` answered `Ok(true)` and dropped it |

**Review found two guards nothing tested, and three kinds of stale prose.**

- **The row lock.** The only test of it passed `abandon` a value that was stale
  before the call, so moving the `FOR UPDATE` onto the pool — where it
  autocommits and lets go at once — still passed all nine related tests. And the
  lock is all that stands between `abandon` and a live company, because its
  `DELETE` has no status filter. `an_activation_waits_while_abandon_looks_and_then_finds_nothing`
  now holds the race open: it locks the tenant's `event` table so `abandon`
  stops inside its look, starts `activate_tenant`, and waits until
  `pg_stat_activity` shows the activation waiting on a lock. Then it lets go and
  expects `abandon` to answer `true` and the activation `NoSuchTenant`.
- **The L6 refusal for a database `abandon` cannot open** had no test either;
  the one `ALLOW_CONNECTIONS false` test covers the orphan sweep.
  `a_provisioning_tenant_whose_database_cannot_be_opened_is_never_dropped` is
  its sibling for `abandon`. Both new tests reach a stuck tenant the way the
  existing sweep test does, by dropping `sign_up` mid-build, now a
  `died_mid_build` helper the three share.
- **Comments that needed a retry.** With the "recover and retry" bullet gone,
  four places still leaned on it: the entitlement-before-schema comment in
  `provision` and its mirror on `install_module` (and in the book) justified the
  order by "retry visibility"; the `42P04` arm called itself "the idempotent
  case"; the module doc said idempotency solved partial failure; and
  `provisioning_the_same_tenant_twice_is_safe`'s doc said recovery and retry
  were the same operation. Nothing reads a provisioning tenant's entitlements
  back and nothing re-runs `provision`. Each now gives the real reason: the
  order is harmless because nobody can see the tenant yet, and a module's
  install is re-run by `install_module` on a live tenant.
- **The reaper was still optional.** Its header and the book said a deployment
  with no demos can leave it unscheduled, while RUNNING said at least hourly.
  With no reaper, a signup whose build died holds its name for ever. Both now
  say it must be scheduled.
- **`ORPHAN_GRACE_SECONDS` said nothing deletes**, which stopped being true
  when the reaper started passing it to `drop_empty_orphans`. It now says the look inside, not
  the age, is what makes that drop safe.

**Raw SQL, for review:** `a_provisioning_tenant_with_events_is_never_dropped`
sets an active tenant back to `provisioning` by hand. The product cannot reach
that state, since nobody can write an event before activation. It simulates a
restore, as `a_tenant_whose_control_row_was_lost_is_never_dropped` does with a
`DELETE`, and its doc says so. `a_provisioning_tenant_whose_database_cannot_be_opened_is_never_dropped`
closes the database with `ALTER DATABASE … ALLOW_CONNECTIONS false`, an
operator's lock during a restore, as the orphan test does.

**Left open.** Nothing resumes a half-built tenant; the sweep only finishes the
compensation, and the customer asks again.

### 59 · Suspending a tenant, and the doc that said the worker kept going

**Built 2026-09-11.** Nothing in the product could suspend a tenant. The only
status write was `activate_tenant`, and the one test with a suspended tenant
(`tests/fleet.rs`) set it by hand with a raw `UPDATE`. Decisions 1, 4 and 12 of
that day shape it: staff do it over HTTP, nothing runs while a tenant is
suspended, and the members get the same generic message everybody gets.

**Two routes**, in `crates/erp-api/src/platform.rs:249` and `:279`: `POST
/v1/platform/tenants/{id}/suspend` with a required `reason`, and `POST
…/reinstate`, both `Staff<SuspendTenants>` — the new `Power` marker next to
`ManageStaff`, so billing and superadmins. Each records the staff member as the
actor. The suspend route's docs say the reason is written for the tenant's
owner and goes into the audit trail about their tenant, so nobody types an
internal note into it. **Nothing shows it to the owner yet**: the reader is the
audit-trail item, which is still to land. *(Since §62 the owner reads it at
`GET /v1/audit`, which answers while the tenant is suspended.)* Reinstating takes no reason; the
entry names who and when. There is no `bin/operator` command for either.

**One place judges a status change.** The three moves (provisioning → active,
active → suspended, suspended → active) each run `UPDATE … WHERE status =
<where the move starts>` and hand the row count to `moved()` (`lib.rs:1909`).
Zero rows is refused: `WrongTenantStatus { status, expected }` naming the status
the tenant is actually in, or `NoSuchTenant`, and never an `Ok` that did nothing.
A change forgets the tenant from every entry cache and is recorded. Suspending
twice is a 409, and so is reinstating an active tenant or suspending one still
provisioning. The message is staff-facing, `tenants.wrong_status`, en and ar.

**`activate_tenant` was that bug already.** It ignored the row count, so called
on a tenant that was not provisioning it changed nothing, answered `Ok`, and
wrote a `tenant.activated` entry anyway. Once reinstating exists, somebody
reaching for "activate" to bring a tenant back would have got a silent no-op and
an audit trail that lied. It goes through `moved()` now (`lib.rs:1808`). Its one
product caller, `sign_up`, only reaches it while the row is provisioning.

**A suspension has a reason, and the schema says so.**
`migrations/control/0018_tenant_suspension.sql` adds `suspended_reason` and
`suspended_at` with `tenant_suspension_is_complete`, the rule `identity` has had
since 0001 plus a length bound `identity`'s lacks: suspended means a reason of 1
to 500 characters after trimming plus a time, and any other status means neither. The explicit
`suspended_reason IS NOT NULL` is load-bearing. `length(btrim(NULL))` is NULL,
and a CHECK that comes out NULL passes. `suspend_tenant` does not check the
reason itself. It maps that constraint's refusal to `SuspensionReason`, a 400
`tenants.suspension_reason` (the `tenant_slug_key` → `SlugTaken` pattern), so
the rule is written down once. The audit entry is the history; the columns are
the current state and what the rule hangs on. The migration fills in any row
suspended by hand before it adds the constraint. It carries an `EXEMPTIONS`
entry because the previous build never writes `suspended`.

**Nothing runs while suspended, including a visit already under way.** Every
door already refused a suspended tenant: `enter` and `enter_for_the_public` both
answer `503 access.tenant_unavailable`, gateway callbacks come in through
`Public`, and `claim_tenants` selects only active tenants. The gap was a visit
that started before the suspension. `renew_lease` did not look at the status,
so that visit ran every remaining job, saved-card charges included. It now has
`AND status = 'active'` (`lib.rs:1028`), and the worker's renewal before every job
stops the visit. The warning text in `worker.rs:367` says why it can stop now.
REVIEW B5 gets a line for it. Sessions are not ended, because they belong to
people who may work for other tenants. This node refuses the tenant on the next
request, and other nodes within five seconds, or at once where they share
Redis.

**The doc that said the worker kept going.** `enter_for_maintenance` said
*"Suspended tenants still need their projections driven"*, and
`enter_for_the_public` repeated it. Neither was true: the worker, the only
production caller of `enter_for_maintenance`, never claims a suspended tenant.
The fleet migrator and module refresh do reach suspended tenants, but through
their own connections (`maintenance_options`), not this door. The comment now
says what is true (`lib.rs:606`): a suspended tenant is not refused there, and
nothing runs for it anyway. The book's lease
section and ARCHITECTURE §1.14 both said *"claiming and renewing are the same
call"*, which stopped being true when `renew_lease` was split out. Both are
corrected.

**`tests/fleet.rs` suspends through `suspend_tenant`**, not raw SQL. The one raw
`UPDATE` this section adds is in
`a_suspension_says_why_and_is_audited_and_the_schema_refuses_one_that_does_not`,
and it is there to prove the database refuses a hand edit.

**Falsified**, each by breaking Rust and watching the named test fail:

| broke | failed |
|---|---|
| `forget` removed from `moved()` | `a_suspended_tenant_is_refused_at_every_door_and_reinstated_at_once`: *a member got into a suspended tenant: Ok(())* |
| `moved()`'s zero-rows refusal disabled | `a_tenant_moves_only_from_the_status_it_is_in`: suspending a provisioning tenant answered `Ok(())` |
| `activate_tenant` alone told a row changed | the same test: activating a suspended tenant answered `Ok(())` |
| the constraint → `SuspensionReason` mapping keyed on another name | `a_suspension_says_why…`: *a 3-character reason was not refused* |
| `record` removed from `moved()` | the same test: *an audit entry was written: RowNotFound* |
| `renew_lease` back to its old text, with its old `.sqlx` file restored for the build | `a_suspended_tenant_is_not_visited_and_its_visit_stops`: *a visit went on running jobs for a suspended tenant* |
| `WrongTenantStatus` out of the 409 arm | `billing_suspends_and_reinstates_a_tenant_under_their_own_name`: 500 where 409 was due |
| the suspend route asks for `ManageStaff` | the HTTP matrix: billing's `POST …/suspend` answered 403 |
| `WrongTenantStatus` rendered with an uncatalogued code | `every_error_variant_maps_to_a_known_code` |
| the `EXEMPTIONS` entry removed | `every_migration_is_expand_only` names 0018 |

**Not falsified:** the CHECK itself. Breaking it means editing a migration,
which is SQL. The hand-edit assertion is what holds it.

**Review found the docs promising a reader that does not exist.** The suspend
route's doc called the audit trail *"a trail that is theirs to read"*, and it is
published in `openapi.json`. The migration's column comment said *"where the
tenant's owner reads it"*. Nothing reads the audit trail, so both claims were
false. They now say the reason is recorded for the owner and is to be shown to
them, and that nothing shows it yet (`platform.rs:230`, the `0018` comment, the
book's `http.md` and RUNNING). The instruction to write it for the owner stays.
No behaviour changed, so no new test. The property the future reader will depend
on is the entry itself: `tenant.suspended`, subject the tenant, actor the staff
member, `detail.reason` trimmed. `a_suspension_says_why…` already pins that
entry. Re-falsified by recording `{}` as the detail: it failed on the detail
assertion.

**Left open.** No route finds a tenant's id by its name, so staff take it from
the control plane for now (RUNNING says how). While a tenant is suspended its
ZATCA reporting stops too, so a suspension longer than a day can push invoices
issued just before it past their 24 hours. That is decision 4's cost, and
RUNNING says so.

**A constraint for the audit-trail reader.** Decision 12 wants the owner to
read the reason through the audit trail, and the reader as designed cannot meet
that. The owner's reader, `GET /v1/audit`, is a tenant-host route. Its
`Allowed<ManageTenant>` goes through `Tenant` and `enter`
(`erp-web/src/extract.rs:258`), and `enter` answers 503 for any tenant that is
not active. So the owner of a suspended tenant would get the same 503 there,
and could read the reason only while the tenant is active, when nobody needs
it. The self-access route (`GET /v1/sessions/current/audit`, decision 6) would
miss it too as designed: it selects entries whose subject, actor or
on-behalf-of is the person, and `tenant.suspended` has the tenant as its
subject and staff as its actor. Either route needs a path that does not go
through `enter`. One option: the session route includes tenant-subject entries
for tenants where the caller holds a live owner membership, whatever the
tenant's status. The trail is control-plane data, so reading it never needed
the tenant's database. Its guard test should suspend a tenant and read the
reason as its owner. *(Met in §62 by the other option: `GET /v1/audit` stays on
the tenant's host behind `ManagesTenant`, which asks `admit` — `enter`'s checks
bar the status — and `the_owner_of_a_suspended_tenant_reads_why` is that test.)*

### 58 · Platform staff: roles, one door, and a way to make the first one

**Built 2026-09-11**, as the foundation the suspend, dead-letter and audit-reader
items stand on. Decisions 1–3 of that day are the product owner's: staff act over
HTTP, roles split by job, and a CLI makes the first superadmin.

**What was there.** A `membership` row with `scope_kind = 'platform'` and a
free-text role (`migrations/control/0001_initial.sql:88`), a cache of *whether*
somebody was staff, and `enter_for_support`, which let in anybody with a live
platform membership. Nothing in the product could grant one. Its doc said what
staff may do was decided by the path — *"audited and time-boxed"* — and nothing
was time-boxed; that sentence is gone.

**The vocabulary is closed and has one table.** `PlatformRole { Support,
Billing, Superadmin }` and `PlatformPower { SuspendTenants, HandleDeadLetters,
ReadAuditTrail, EnterForSupport, ManageStaff }` live in
`crates/erp-control/src/staff.rs`, shaped like the tenant's `Role` and
`Capability`, and `PlatformRole::may` (`staff.rs:93`) is the matrix: superadmin
everything, billing suspends, support reads the trail, handles dead letters and
enters for support. A stored role this build does not know is
`AccessError::Corrupt` — `UnknownRole` reused rather than a second copy — so a
`root` row locks nobody in and nobody out silently. The cache now holds the
parsed `Option<PlatformRole>`.

**One door.** `ControlPlane::staff_may(identity, power)` (`staff.rs:212`) is what
every platform door asks: active identity, a role that `may`, and a second
factor. `enter_for_support` asks it for `EnterForSupport` (`lib.rs:551`), so
**billing can no longer open a tenant's books** — before this, any platform row
could. `erp_web::Staff<P>` asks it for `P::POWER`; it is to `Power` what
`Allowed<C>` is to `Capability`, and taking one is the check. Refusals are 403
either way: `access.not_permitted` naming the power, the same shape a tenant
role gets, or `access.staff_second_factor_required`. An API key is refused
before the door (`extract.rs:1147`) — belt and braces, since a key's machine
identity has no handle to be granted by. That line has no test.

**The door asks whether a factor is enrolled, and that has to mean the session
went through it.** Two things make it mean that — both added after review, see
below: confirming an enrolment ends every other session, and staff cannot turn
their factor off. **`grant_staff` refuses an account with no factor**: a staff
account with only a password is one somebody holding that password could enrol
their own factor on, and then they would hold the only one. Whose phone the
factor is on is the granting superadmin's to know; nothing here can.

**Staff over HTTP**, in `crates/erp-api/src/platform.rs`: `GET`/`POST
/v1/platform/staff`, `PATCH`/`DELETE /v1/platform/staff/{identity}`, all
`Staff<ManageStaff>`. The field is `platform_role`, not `role`: `name_the_roles`
appends the tenant's list to every `role` in the document, and a staff route
offering `owner` is the drift `every_role_the_document_names_exists` was written
against. It now names each vocabulary on its own field and that test checks
both. Every change is a `membership.*` audit entry with the superadmin as actor,
and forgets the platform cache at once.

**The last live superadmin cannot leave over HTTP**, by demotion or removal,
their own or anybody's — `LastOwner`'s rule, with "live" meaning an active
identity, so a suspended superadmin is not somebody who can grant another.
Unlike `is_last_owner`, the check is **in one transaction that locks every live
superadmin row first** (`staff.rs:360`), in id order: two superadmins removing
each other at once otherwise both see the other standing and both succeed. The
tenant's `is_last_owner` has the same race and I did not touch it.

**A grant race too, found by writing the test for it.** `grant_membership`
answers success for a membership that is already live, deliberately and without
changing the role. Two superadmins granting one person `support` and `billing`
at once were both told 201. `grant_staff` now reads the role back
(`staff.rs:314`) and tells the one who did not get theirs `staff.already_staff`.

**`bin/operator`** (`crates/erp-worker/src/bin/operator.rs`): `grant-staff
<email> <role>` through `grant_staff`, and `revoke-staff <email>` through
`revoke_membership(Scope::Platform)`, which has no last-superadmin guard — the
break-glass path decision 3 asks for, and which also ends the account's
sessions. Both record as the system. It opens no cluster
(`ClusterRegistry::new()`), so it needs `CONTROL_DATABASE_URL`, plus `REDIS_URL`
where the API has one.
Anything but those two exact shapes is the usage and exit 2, before it connects.
Wired into the Dockerfile's build, `cp` and `COPY` lines, a `just operator`
recipe, and RUNNING's binary count.

**The matrix test.** `role_scoped_operations` now leaves out everything under
`/v1/platform/`, and `platform_operations` collects exactly that for
`every_platform_role_against_every_platform_endpoint`: `PLATFORM` tables each
route's power, `STAFF_POWERS` types decision 2 out rather than reading `may`,
and every staff role, a tenant owner with a second factor who is not staff, and
a superadmin with no factor (made by `grant_membership` directly, the one way
left to be one) are sent at every route. The tenant matrix stays at 240.

**Falsified**, each by breaking Rust and watching the named test fail:

| broke | failed |
|---|---|
| `Support` may `ManageStaff` | `every_role_may_exactly_what_it_should`; the HTTP matrix (support → `GET /v1/platform/staff` answered 200) |
| `Staff<P>` asks for `ReadAuditTrail` whatever `P` is | the HTTP matrix (billing's 403 named the wrong power) |
| second-factor check removed from `staff_may` | the HTTP matrix (factorless superadmin got 200); `support_access_needs_the_power_and_a_second_factor_and_is_audited` |
| `enter_for_support` back to "any platform role" | the support-access test: *billing entered a tenant's books* |
| unknown stored role read as `Support` | `an_unknown_stored_role_is_corrupt_data_not_a_guess`; the support-access test |
| `forget` removed from `move_staff` | `staff_are_managed_over_http…`: *a promotion waited for the cache* |
| `platform.rs` records `Actor::system()` | the same test: three audit rows with no actor |
| grant's second-factor check removed | the same test: 201 where 422 `staff.no_second_factor` was due |
| last-superadmin guard disabled | `the_last_superadmin_cannot_leave…`; `two_superadmins_removing_each_other…` |
| superadmin rows read off the transaction (lock released) | `two_superadmins_removing_each_other…`, 5 runs of 5; passes 5 of 5 restored |
| grant read-back disabled | `two_grants_of_one_person_at_once_tell_the_loser`, 5 of 5 |
| `revoke-staff` arm accepts any first word | `only_the_two_commands_are_read…` |
| `platform_role` described with the tenant's roles | `every_role_the_document_names_exists` |
| platform routes left in `role_scoped_operations` | `every_role_against_every_endpoint`: four untabled operations |

Two of those need saying. **The race tests first passed with the lock
removed**: one side spent its turn opening a fourth pool connection while the
other ran to the end. Both now open every connection the pool will give before
racing, and then fail every time without the fix. And **"live" is a SQL clause**
(`i.status = 'active'`), which the Rust-only rule cannot break; I broke it with
the build pointed at the type-check database, watched
`the_last_superadmin_cannot_leave…` fail on the suspended-superadmin step, and
restored the text, leaving `.sqlx/` as `just prepare` wrote it.

#### What review found

**A session from before the factor walked through the door.** `staff_may` asks
whether a factor is enrolled *now*. A reviewer signed in with a password, then
enrolled, then was granted superadmin — and the first, password-only session
answered `200` at `GET /v1/platform/staff`. In life: a password phished at nine,
the owner enrolling at ten and made superadmin at five past, and the attacker's
session managing staff until it expired twelve hours later without ever holding
the factor. The paragraph this section used to have said *"a stolen session is a
stolen session either way"*; this one was not stolen, it was minted from a
stolen password, which is exactly what the factor is there to stop.

The fix is at the enrolment, not the grant, because the grant is only one of the
places this shows: **`confirm_second_factor` now ends every other session of the
identity in its own transaction** (`second_factor.rs:214`), keeping only `keep`,
the session that confirmed and so just proved the factor. With `start_session`
refusing password-only sessions once a factor exists, a session alive while a
factor exists either went through it or confirmed it — for tenants that require
a factor as much as for staff. The route passes its own token; the Rust callers
in tests pass `None`.

**And a staff member could turn the factor off**, which reopened the window the
grant check closes: the account is password-only again, and whoever enrols next
holds the factor — a password-holder signing in (allowed, no factor exists)
could enrol first. **`disable_second_factor` now refuses any identity with a live
platform row** (`second_factor.rs:342`), before it checks the code so a recovery
code is not spent on a refusal: `403 auth.staff_keeps_second_factor`, en and ar.
Replacing a factor still works; dropping one means coming off the staff first.
**A tenant that requires a factor has the same window and I did not close it**:
its members can still turn theirs off, and whoever enrols next is let in. That
is a product call about tenants, not part of this item. *Closed by §67: the
product owner made that call, and the staff refusal became the general one.*

**`operator revoke-staff` told no API node.** It built its control plane without
`.sharing(...)`, so the revocation forgot the platform cache in its own
short-lived process and every API node kept the revoked superadmin for up to
five seconds — the window `move_staff`'s own comment calls *"exactly what was
just taken away"*. It now shares when `REDIS_URL` is set, as `api` and `worker`
do (`operator.rs:91`), and since this is the command for a compromised account,
it also ends that account's sessions (`operator.rs:120`), which the HTTP
revocation deliberately does not. The book's *"invalidates the platform cache at
once"* now says where.

| broke | failed |
|---|---|
| `confirm_second_factor`'s session delete skipped | `a_session_from_before_the_factor_does_not_reach_a_platform_door`: the phished session answered 200 where 401 was due |
| staff refusal in `disable_second_factor` skipped | `every_platform_role_against_every_platform_endpoint`: `(204, None)` where `(403, auth.staff_keeps_second_factor)` was due |
| operator's `control_plane` drops the `Shared` | `revoke_staff_closes_the_door_on_every_node_and_ends_the_sessions`: *the API node still let a revoked superadmin manage staff* |
| operator's `log_out_everywhere` removed | the same test: *the revoked superadmin's session still works on the API node* |

The operator test runs an API-shaped control plane and the operator's over one
database and one Redis, warms the API node's cache first, and needs `REDIS_URL`
the way `erp-control`'s `tests/shared.rs` does — failing without it, not
skipping.

**Not here, deliberately:** a support-entry route (`enter_for_support` stays a
function), a staff screen, and doors for the other three powers — suspend, dead
letters and the audit reader are the next items and each adds one `Power`
marker. ARCHITECTURE §1.9's "time-boxed, visible to the tenant, attributed to
both parties" is still design, and now says so.

### 57 · The leftovers, reclaimed where they are made

**Built 2026-09-11.** The production sweep (§56) refuses to touch a test
cluster's leftovers, and rightly: they are unclaimed, most have events, and one
occupied database refuses the whole run. So the accumulation carried on — 65 to
235 to 324 across a single day's runs.

**The harness already had this mechanism.** `erp_testkit::sweep_once` drops
leftover `erp_test_*` databases once per process, on two conditions that were
already exactly right: no active connections, and older than a grace. It simply
did not cover `erp_tenant_*`, because those are made by `provision` rather than
by the harness and carry no timestamp the harness put there. I had written a
second sweep from scratch before finding it — the reuse rung of the ladder,
missed.

The extension is one more query with the same two conditions. What differs is
the grace, and the reason is worth stating: for an `erp_test_*` database the
connection check *is* the guard, because a pool is held for as long as its test
runs. An `erp_tenant_*` database is provisioned and may then sit untouched for
minutes inside a test that is very much still using it, so there the **age** is
the guard — and it has to be wider than a whole **run**, not wider than a gap,
because `nextest` starts binaries throughout a run and a late one can see an
early one's databases. Two hours against a twenty-minute suite.

**The age rule moved to `erp_types::TenantId::named_in_database`**, beside the
id it reads. It was in `erp-control` and the harness needed the same answer for
a different reason; two copies of "is this one of ours, and how old" is the
second declaration that eventually disagrees with the first. Its exclusions are
tested where it now lives: the operator's `erp_tenant_backup_before_upgrade`,
the wrong length, the hyphenated form `Uuid::parse_str` accepts, and **v1 and v6
— which carry timestamps that convert to plausible times**, so the version has
to be checked rather than inferred from "did a timestamp come back".

**Three of my own tests failed on the first full run afterwards**, and the cause
was mine: they fabricated forty-eight-hour-old names to look like old orphans,
and the new harness sweep ate them mid-test. The fix made them smaller — they
already pass a grace of zero, so age was never what made those fixtures
candidates, and the backdating was decoration that happened to be dangerous.

Measured rather than asserted: three six-hour-old databases seeded by hand,
gone after one test binary ran; `erp_tenant_backup_before_upgrade` and a
v1-named database beside them, untouched.

### 56 · Databases nothing accounts for — and a sweep that was wrong at the root

**Built and then reversed, 2026-09-11**, which is the part worth writing down.

A development cluster had accumulated **1139** `erp_tenant_*` databases and the
contention made a suite run fail. Every sweep this system has iterates *rows*,
so a database no row claims is in none of them. The obvious fix was a sweep that
finds and drops them, and that is what was built: five conditions, each
falsified, a day of grace, no `WITH (FORCE)`.

**A review found the premise backwards.** The sweep was justified by "a run that
dies between `CREATE DATABASE` and the row that names it". `provision` writes the
row **first** (`provision.rs:167`) and creates the database **second** (`:195`),
and `abandon` drops the database before deleting the row. A provisioning that
dies leaves a row with no database — which `abandon` handles **— false
twice**, see §60 — and never a database with no row. **The window the whole design was calibrated against does
not exist**, and the grace protected the wrong thing: it measures age since
*signup*, so the older and larger a tenant, the less protection it had.

So an unclaimed tenant database has one realistic cause and it is the opposite
of rubbish: **a control plane that has lost rows.** A restore to a point before a
tenant existed, a stale replica, a mis-pointed DSN. This repo already names that
state — `restore.rs::a_tenant_database_without_its_control_row_is_unreachable`
asserts the events are all still there, *"which is what makes this dangerous"* —
and the sweep converted that recoverable incident into permanent loss of the one
thing `RUNNING.md` calls irreplaceable, on a schedule, while an operator is
mid-restore. A reviewer reproduced it on a scratch server at the production
grace: a three-day-old unclaimed database, destroyed.

**It asks the database now.** The control plane cannot tell a dead provisioning
from a lost row — they are the same absence — so the question goes to the one
party that is not in doubt. `find_orphaned_databases` classifies each unclaimed
database as `Empty`, `Occupied` or `Unreadable`, and `drop_empty_orphans` acts
only on the first.

**Two tables answer it.** `event`, because `RUNNING.md` calls the log the thing
"nothing else can reconstruct" — one row and this is somebody's business. And
`configuration` *excluding what a module seeded*: an install writes
`set_by = 'module:tax_sa'` and `refresh_module` writes it again, so those come
back by themselves, while a rate a business corrected does not and nothing else
remembers it.

**One doubtful database refuses the whole cluster.** A tenant with data and no
row does not mean one row was lost; it means the control plane is not a
description of this cluster, and the next name in the list is not evidence of
anything either. `Unreadable` counts as doubtful: **not knowing is not knowing
it is empty**, which is worth a whole variant and is the arm that fails safe.

Every way looking can fail is one `Err`, so there is one place that decides what
not-knowing means — it cannot be got right for the connection and wrong for the
query. That collapse came out of falsification: with two arms, only the
connect-failure one had a test.

**Two of my own tests were the bug in miniature.**
`a_claimed_database_survives_a_sweep_with_no_grace_at_all` ran a destructive
sweep at zero grace against the shared test cluster, so it deleted *other tests'*
tenant databases whenever the suite ran in parallel — and passed, because it only
asserted about the two databases it knew of. A reviewer reproduced that too:
three seeded databases destroyed, test green.

**What survived the reversal**, because it was right: `orphan_age_seconds` reads
the age out of the UUIDv7 in the name rather than any clock or catalogue; it
requires **version 7 specifically**, since `get_timestamp` answers for v1 and v6
as well and both convert to plausible times; and it refuses the hyphenated UUID
form, which `Uuid::parse_str` otherwise accepts. The version check was missing
from the first version — documented as a condition and never enforced, which is
the fourth time this session a comment claimed something the code did not do.

**What still is not claimed:** that a production orphan exists. None of this
makes one, because the row precedes the database. The population this actually
serves is leftovers from crashed test runs and from control-plane clones — and
for those, `Empty` is exactly what they are. If a real one ever appears it will
be `Occupied`, and the sweep will refuse and say so.

### 55 · No way back in, and a way around the second factor

**Built 2026-09-10.** Two things, and the second was found while designing the
first.

#### The gap: nothing could write a password

`register_login` was the only writer of `authenticator.secret` and its two
callers were signup and invitation-acceptance. There was no change-password and
no reset — no route, no function, no recipe. The phone-code path is not a
recovery path either: for a number with no `phone` authenticator, `otp.rs` mints
a **brand-new identity with no memberships**, so an owner who signed up with
email and forgot the password got a fresh empty account rather than their
tenant. Nobody is above a tenant owner, so it was permanent.

`POST /v1/password-resets`, `POST /v1/password-resets/redemption`,
`POST /v1/sessions/current/password`. The design is in
`crates/erp-control/src/passwords.rs`; four choices are worth repeating here.

**A reset issues no session.** `DELETE /v1/sessions/second-factor` takes nothing
but a live one, so a reset that handed a session back would be a two-call factor
removal: open the link, get a session, delete the enrolment. A reset ends with
"changed" and the person logs in through the same gate as everybody else.

**It demands the second factor, and that strands nobody.** The mailbox is
exactly what a second factor exists to survive. Requiring it here closes no door
that was open — anybody who cannot present it cannot log in with it either — so
it is consistent with *switching a control on must not be the act that strands
you*. The recovery codes are the path.

**No "one live link per address."** `pending_signup` and `invitation` both have
one and copying it would have been a permanent denial of recovery: request a
reset for the victim once a minute, each request cancelling the last, and their
mail arrives holding a dead link. The law does not transfer — an invitation is
revocable access and a reset link is neither revoked nor access. A cooldown, an
hour of life and the sweep bound the rows instead.

**An address with no account is answered identically and mailed nothing.**
Mailing every address anybody types would be an unauthenticated mail cannon
aimed at strangers, and the cost is not their inbox — it is the complaint rate
on the sending domain that carries every tenant's signup and invitation mail.
That is the vector `0010_signups.sql` closed, and one `INSERT` of timing is not
worth reopening it for.

#### The bigger one: three paths minted sessions without the second factor

`log_in` carried the promise — *"a path which never heard of a second factor
cannot issue a session that skipped one"* — and `start_session` had five
production callers, of which **three never asked**: `otp::verify_code`, and both
signup paths for an address that already has an account.

The chain, verified end to end: an attacker holding the victim's password and
mailbox — the exact pair a second factor exists to survive — is refused at
`POST /v1/sessions`, then signs up a throwaway company under the victim's
address. `request_signup` calls `authenticate`, which is the credential half
with no factor in it. Confirming the link calls `start_session` directly and
hands back **a full session as the victim**. `DELETE /v1/sessions/second-factor`
then takes the enrolment and all ten recovery codes.

**The gate is inside `start_session` now**, and `issue_session` — private to
`auth.rs` — is the only way round it. A promise every caller has to keep is one
a caller eventually breaks; this is the same move `evaluate` made when it
started taking its answer from `explain`. Privacy is the primary guard: a third
module *cannot* call it, which is a compile error rather than a test failure.
`only_two_paths_may_issue_a_session_without_checking_the_second_factor` counts
the ways round for anything added inside `auth.rs`.

**Both signup paths check before they build, not after.** Reaching the gate with
the tenant already provisioned answers `500` through `Corrupt` and leaves an
orphan database, an unclaimed confirmation link and a slug taken forever — a
worse bug than the one being fixed. The falsification reproduces exactly that:
`Corrupt("this account needs its second factor")`.

#### And the last step of the chain, closed the same day

`disable_second_factor` and `confirm_second_factor` both took a live session and
nothing else. Both ask for the factor now.

**Turning it off asks for the factor being turned off** — not the password.
Somebody who worked their way to a session may well have the password; that is
usually how they got close. Only the factor is evidence they do not have.

**Replacing one was the worse of the two.** `confirm_second_factor` deleted the
live enrolment *and all ten recovery codes* with no proof of either, so one
transient session pointed the account at the attacker's authenticator app and
destroyed the paper that would have let the owner back in — leaving them
*holding* a factor rather than merely dropping one. It proves the old factor
before anything is deleted, and a recovery code counts, because losing the phone
is the case re-enrolling exists for.

**Neither strands anybody**, which is what keeps both inside *switching a
control on must not be the act that strands you*: a person who can present
neither a code nor a recovery code could not have logged in to reach either
route. `the_requirement_can_always_be_switched_off` still holds and now says so
in those words.

**Two guards, both falsified by putting the hole back:**
`turning_it_off_needs_the_factor_being_turned_off` and
`replacing_a_factor_needs_the_one_being_replaced`.

### 51 · A second factor, and the state it refuses to have

**Built 2026-09-09.** Half of Phase 3's MFA box, and the reason it was worth
doing tonight is that it needs nothing from the outside world: RFC 6238 is
specified to the byte and publishes test vectors, so it can be proved right
here rather than guessed at — the exact opposite of the WPS file and the
Taqnyat callback, both of which were skipped the same day for want of a
verifiable specification.

#### There is no half-authenticated state

The usual shape is a challenge: check the password, hand back a short-lived
token, exchange that plus a code for a session. It needs a table of
part-way-there states, and **every query that reads a session then has to
remember to exclude them.** One that forgets is a login that never needed the
second factor.

So `log_in` takes the password, and refuses with `SecondFactorRequired` if a
factor is enrolled. `log_in_with_second_factor` takes both and **creates nothing
until both pass**. The cost is that a client holds the password while the person
types six digits. The gain is that "a session that skipped a factor" is not a
state this system can be in — not a bug that is unlikely, a bug that cannot be
written.

The refusal lives in `log_in` itself rather than in the API route, so a path that
never heard of a second factor — an invitation acceptance, a test helper —
cannot issue a session past one.

#### Four kinds, no new table

`0004_authentication.sql` predicted that "OIDC and API keys are more rows, not
more tables". §50's sibling finding was that the prediction was **half wrong**
for API keys, which needed their own table. Here it is exactly right:

| kind | `secret` holds | `handle` holds |
|---|---|---|
| `password` | an Argon2id PHC string | the login address |
| `totp_pending` | the sealed shared secret | the identity's id |
| `totp` | the same, once proved | the identity's id |
| `recovery` | SHA-256 of one single-use code | `<identity>:<n>` |

**Why `totp_pending` is a kind and not a `confirmed_at` column.** A pending
enrolment must never satisfy a login. A row that is simply *not there* under
`kind = 'totp'` cannot be missed by a query that forgot to check a nullable
column — and somebody who scans a QR and wanders off is not locked out of their
account.

#### Three things that are easy to get wrong

**The secret is sealed, not hashed.** It has to be recoverable to compute a
code, so hashing is not available; it is AES-256-GCM under the deployment's
`SealingKey`, **bound to the identity**. A row moved onto another account does
not open — there is a test that moves one.

**A code cannot be used twice.** TOTP has no memory, so the same six digits work
for the whole thirty-second window, including for somebody who read them over a
shoulder. The last accepted code's digest is recorded beside the secret and a
repeat is refused, which closes the replay without a second table.

**Re-enrolling retires everything.** A new enrolment deletes the old secret
*and* the old recovery codes. Somebody whose phone was stolen needs one action
that makes everything they had useless.

#### SHA-1, deliberately

RFC 6238 permits SHA-256 and SHA-512. Every authenticator worth naming computes
SHA-1 and ignores the `algorithm` parameter in the provisioning URI, so a build
that chose SHA-256 would be more modern and would produce codes nobody's phone
agrees with. HMAC-SHA1 is not broken for this: the attack on SHA-1 is
collisions, and HMAC does not rest on collision resistance.

#### Proved rather than asserted

Seventeen tests. Eight are arithmetic against **published vectors** — RFC 6238
Appendix B for the codes, RFC 4648 §10 for base32 and base64 — so if this build
ever disagrees with them, every authenticator app in the world disagrees with
it. Nine are against a real control database, and **all six guards were
falsified**: the password gate, the pending-enrolment gate, the replay refusal,
the recovery-code spend, the re-enrolment purge, and the identity binding.

#### And a tenant can require it — *answered 2026-09-09, and built*

The three questions this was held on were put and answered:

**What happens to somebody already signed in?** Their session stays valid and
they are refused at the next *entry to that tenant*, with a message telling them
to enrol. Not logged out, and their other tenants are untouched — an owner
turning this on asked to protect their own business, not to sign somebody out of
somebody else's.

**How does an owner avoid locking themselves out?** Switching it **on** is
refused unless the person doing it has enrolled one. One rule, no special cases,
and it guarantees at least one person can still get in. Switching it **off**
carries no such condition, because needing a second factor to remove the
requirement is the trap it exists to prevent.

**Where does the flag live?** On the **tenant row in the control plane**, not in
the tenant's own configuration. `enter` already reads a cached tenant, so the
check costs nothing — and a flag inside the tenant database would have to be
read *after* deciding whether the caller may reach that database, which is the
wrong way round. Setting it forgets the cached row, and there is a test that
fails if it does not.

`AccessError::SecondFactorRequired` answers **403, not the 404** every other
access failure gets. The enumeration argument does not apply: reaching it means
already holding a session *and* a live membership, so there is nothing left to
discover — and a 404 tells a member to give up when the one thing they can do is
enrol.

Four more guards, all falsified: the entry check, the lockout guard, that
switching off never needs a factor, and that the cache is forgotten.

### 50 · Exemption reasons: the invoice that called rent a financial service

**Built 2026-09-09.** Phase 4e's open box, and it turned out to be a defect
rather than a gap — which is only visible once you ask what the code actually
did rather than what the box said.

#### What it was doing

`modules/tax_sa/src/zatca/mod.rs` derived the ZATCA exemption reason **from the
VAT category**:

```rust
pub const fn exemption_reason(category: VatCategory) -> Option<(&str, &str)> {
    VatCategory::Zero => Some(("VATEX-SA-32", "Export of goods")),
    VatCategory::Exempt => Some(("VATEX-SA-29", "Financial services mentioned in Article 29")),
}
```

A category says *that* a supply carries no tax. Only the taxpayer knows *why*.
So every exempt line this system has ever produced told ZATCA it was a financial
service, and every zero-rated line told ZATCA it was an export. For a bank and
an exporter that was right. For a landlord letting residential property —
`VATEX-SA-30`, real estate transactions — it was a **false statement to a tax
authority on every invoice**, and it would have been the property vertical's
first compliance failure.

The `ponytail:` comment above it had predicted the shape of the fix and
mis-stated the risk: *"neither gets a reason that is wrong."* That holds only
while every tenant is a bank or an exporter.

#### Where the reason had to go, and why not a setting read

The obvious answer — read the tenant's configured reason where the document is
built — is wrong twice over. `ubl::render` runs inside `ZatcaDocuments::apply`
(`modules/tax_sa/src/documents.rs:308`), so a settings read there breaks **L7**
(no reads while applying) and **L2**: a rebuild would stamp today's article onto
an invoice issued under last year's.

So the reason is resolved **at issue time**, in the command's own transaction,
and carried on the event — exactly where the *rate* already is, and for exactly
the same reason `sales::Vat` documents: *"the rate that applied when it was
issued"*. `Vat` gained a third field and nothing else moved.

#### Four places, one idea

- **`ledger::Rates`** gains `zero_reason` and `exempt_reason` — opaque strings.
  `ledger` does not own the list and must not: a country-neutral module that
  enumerated Saudi articles would need every country's. This is the
  `crm::TaxRegistration.scheme` precedent, which carries ZATCA's `schemeID`
  without `crm` knowing what is in it.
- **`sales::Vat`** carries the code, stamped by `Vat::at` from those rates.
- **`tax_sa::ExemptionReason`** is the enum: sixteen `VATEX-SA-*` codes, each
  with its article text and the category it belongs to. The country module owns
  the country's list.
- **`sales` refuses at issue** (L6) when a non-standard line has no configured
  article, naming the treatment in both languages. Not a default — a default is
  what caused this.

#### Two things that came out of it

**An unknown code never reaches the authority.** `exemption_reason` parses the
stamped string and yields nothing if it is not a code ZATCA published, so a typo
in a settings field produces a document with no reason rather than a document
asserting a fiction.

**Nothing is better than a guess.** A document issued before this existed stamps
no code, and now renders without the element. That is strictly better than what
it did yesterday, which was to render `VATEX-SA-29`.

#### Verified rather than remembered

The code list was **looked up**, not recalled: ZATCA's four tax categories
(`S`/`Z`/`E`/`O`) and all sixteen `VATEX-SA-*` codes, cross-confirmed against two
independent sources. The `VRBL:SA:` prefix some tooling documents is that tool's
namespace and is **not** in the XML ZATCA receives. Recorded in
`docs/AMBIGUITIES.md` §0 so it is not re-derived.

One rule found there is worth carrying into Phase 20a: **if the reason is
`VATEX-SA-EDU` or `VATEX-SA-HEA`, the buyer's ID is mandatory and must be a
national ID.** That is the `Identification { scheme: NationalId }` field §49
designed for Ejar, wanted independently by a second obligation.

#### Left open

`VatCategory::OutsideScope` (ZATCA's `O`) and its `VATEX-SA-OOS` free-text
reason are **not added**. Nothing in this system produces an out-of-scope supply,
and a fourth category with no producer is a variant every `match` must handle to
no purpose. The code list is complete for the three categories that exist.

### 49 · Property management — *answered 2026-09-09: a new vertical for this product*

**Asked on 2026-09-09, before picking up the backlog. Checked against the code,
not remembered — every claim below names the file that shows it.**

The direct answer is **no**. `grep -rniE "\b(propert(y|ies)|lease|tenancy|landlord|rent(al)?|real.?estate)\b" modules/ crates/ --include=*.rs`
returns no domain hit; every match is incidental — a `Rent` line in a
chart-of-accounts template, `current` matching `rent`. There is no unit, no
lease, no owner, no tenancy, and no phase that plans one.

What is worth writing down is how much of the substrate *does* carry, because it
is more than it looks, and the pieces that do not carry are specific rather than
general.

#### What carries as it stands

**A rentable unit is already a `Resource`.** `modules/booking/src/resource.rs:29`
— `Kind` is `Person | Place | Thing`, and its own comment is why this works:
*"Display and filtering only. No rule in this module branches on it, and the
moment one does the engine has stopped being general."* An apartment, a shop, a
parking bay is a `Place`, and Phase 8b's six fixtures are the evidence the engine
does not care what it is holding.

**A lease term is expressible as a window.**
`modules/booking/src/reservation.rs:425` — `starts_at`/`ends_at` are plain
`Timestamp` with no upper bound anywhere. Twelve months is a legal reservation.

**Saudi VAT for real estate is already modelled.** `modules/ledger/src/vat.rs:31`
— `VatCategory` is `Standard | Zero | Exempt`, and `Zero` and `Exempt` are
separate variants *because of* `input_is_reclaimable()` (`vat.rs:47`). That is
exactly the distinction residential rent (exempt, input tax not reclaimable) and
commercial rent (15%) require, and it is load-bearing rather than decorative.

**An owner statement has a precedent, and it is not a new problem.**
`modules/reports/src/lib.rs:1` sets out why a module that must see leases,
invoices, payments and maintenance *at once* subscribes to the log and keeps its
own group rather than reading four — L3, and the honest cost: *"It keeps its own
copies of what it needs."* An arrears report and an owner disbursement statement
are that module's shape, not a new one.

**Attaching the signed lease is solved.** `modules/files/src/file.rs:34` — an
`Owner` is an opaque `(kind, id)` pair, and `files` depends on no module.

#### Three closed enums a property module would have to open from below

This is the friction the layering rule creates, and it is worth naming before
rather than during:

| Enum | Where | What it costs |
|---|---|---|
| `messaging::Topic` | `modules/messaging/src/audience.rs:33` | Four variants. Rent reminders and expiry notices need a `Lease` |
| `files::OwnerKind` | `modules/files/src/file.rs:34` | Seven variants, each pinned by `every_owner_kind_names_the_domain_its_module_uses` |
| `notifications::Kind` | `modules/notifications/src/kind.rs:22` | Five variants, each carrying compiled bilingual copy |

None is hard. All three are edits to a module *below* the one being added, which
is the opposite direction from how every module so far has landed — and all
three have a test that fails if the edit is half-done, which is the mitigation.

#### What is simply not there

- **Fixed assets.** No capitalisation, no depreciation schedule, no disposal, no
  gain or loss on sale. `grep -rniE "depreciat|amorti[sz]|fixed.asset|capitali[sz]"`
  over `modules/` and `crates/` finds three false positives and nothing else.
  **This is what kills *buying and selling* outright** — a property bought is an
  asset carried and depreciated, and a property sold is a disposal against
  carrying value. Rent could ship without it; a purchase could not.
- **Recurring invoicing.** Nothing in `modules/sales` or `modules/payments`
  schedules anything; every invoice comes from a command. And `erp-recurrence` is
  **not** this — it is 432 lines of `availability.rs`, weekly patterns for
  booking, and the crate name flatters it.
- **Arrears.** No dunning, no overdue tracking, no ageing. Chasing rent is most
  of what property management software actually does.
- **Paying money out.** `modules/purchases` books supplier bills, but there is no
  payment run; an owner disbursement and a contractor payment both need one.

#### Two things it would be a mistake to reuse

**`booking`'s deposit is not a security deposit.**
`modules/booking/src/reservation.rs:27` — `Deposit { net: Money, due_by: Timestamp }`,
and `net` is documented as *"Before tax. What the prepayment invoice will be
raised for."* That is a prepayment against a future invoice: revenue in advance.
A rental security deposit is a **liability** — money held and not earned,
returned at the end of the term less deductions, sitting on the balance sheet the
whole time. Reusing the word would produce a set of books that overstates revenue
by the entire deposit balance, and the trial-balance invariant would not catch it
because it would balance.

**The branch dimension is the wrong carrier for a property.**
`crates/erp-eventlog/src/envelope.rs:68` says what it is: *"a fact about where the
request came from"*, folded in from an `X-Branch` header so that every event of
one request agrees. A property is not that — an agent at head office raises
January rent for forty units they are not standing in. The dimension has to be on
the document. Mechanically a second key is free, because `extra` is a generic bag
(`crates/erp-eventlog/src/aggregate.rs:500`); what is not free is
`proj_ledger.posting`'s dedicated `branch` column
(`modules/ledger/src/projections.rs:135`) and `branch_balance` being a
single-dimension rollup. A contained change to `ledger`, but a real one.

#### The party model is the one genuine architectural gap

`modules/crm/src/customer.rs:31` — `CustomerKind` is `Person | Company`. There is
no role, and no relationship between parties. Property needs one person to be the
**owner** of unit A, the **tenant** of unit B and the **guarantor** on unit C at
the same time. Custom fields do not reach it either: `modules/crm/src/fields.rs:60`
offers `Text | Number | Date | Choice | Flag` and has no reference type, so "owns
unit X" cannot be a field.

This is the piece that is a design question rather than a build task, and it is
the one worth settling before anything else is drawn.

#### Saudi specifics

- Residential rent exempt, commercial 15% — **expressible today** (`vat.rs:31`).
- Per-line exemption reasons — ~~an open box in Phase 4e~~ **built 2026-09-09,
  §50**. A landlord configures `VATEX-SA-30` (real estate transactions, Article
  30) once and every exempt line carries it. This was the gate, and it is open.
- **RETT at 5% on a sale** — not VAT, and there is no non-VAT tax anywhere in the
  code.
- **Ejar**, the mandatory rental-contract registration — not built, and it would
  not have to be invented from nothing: §45's ZATCA onboarding is the pattern for
  an external regulatory registration with a credential, a submission and a
  worker that finishes it.

#### Sizing, honestly

For calibration, by lines of `src/` plus schema: `branches` 1,399 ·
`conversations` 1,837 · `notifications` 2,287 · `prepaid` 5,479 · `booking` 8,028.

- **A lease module that bills rent** — units, leases, a rent schedule, invoicing
  from it — is `prepaid`-sized. Call it 4–6 weeks, plus the party model and the
  three enum edits.
- **Arrears, security deposits held as a liability, and owner statements** roughly
  doubles that.
- **Buying and selling** needs fixed assets first, which is its own module and
  appears in no phase. Another 3–4 weeks before the property side of it starts.
- A **competitive** product also wants maintenance work orders, service-charge
  reconciliation, utility recharging, agent commission and the sale pipeline.
  That is a vertical, not a module.

#### The answer — *2026-09-09*

**A new vertical for this product.** Not a separate product.

The question was whether property is a seventh Phase 8b fixture or a different
system. It is the seventh fixture: the engine holds up, a unit is a `Place`, and
a lease term is a window the reservation model already allows. What does not
carry is the money — rent accrues monthly, is chased when late, and sits against
an owned asset.

So the work is **not** a property module. It is four cross-cutting pieces that
this product wants anyway, and a module that composes them. Every one of the four
has a second consumer already in the building, which is what makes this a vertical
rather than a bolt-on:

| Piece | Second consumer that already exists |
|---|---|
| A **party model** with roles and relationships (`crm`) | §9b's claim union asks *who reports to whom*; a booking client who is also a supplier |
| **Recurring invoicing** | `prepaid` subscriptions bill on a cycle and nothing schedules them either |
| **Arrears** — ageing, dunning, chasing | Every unpaid invoice in `sales`, today, is chased by nobody |
| **Fixed assets** — capitalisation, depreciation, disposal | Any tenant that owns equipment; §19's inventory sits next to it |

That table is the argument. If any of those four had exactly one consumer it
would be the wrong thing to build, by the same reasoning that deferred 5b for a
year.

**What this does not settle**, and what Phase 20 is written to keep separable:
whether the first release is *letting* only, or letting **and** buying and
selling. Letting can ship without fixed assets; buying and selling cannot. See
Phase 20.

### 48 · Conversations: what a reply answers, and how it is known

**`messaging` had to start remembering.** Everything about that module is
fire-and-forget by design — resolve late, render late, promise an effect, hold
nothing — and that is right for sending and useless for receiving. An SMS reply
arrives as a number, a body and the gateway's id; *what it answers* exists
nowhere unless what was sent to that number was written down. So `message_sent`
joins the meter and the device tokens in the tenant migration chain, for the
same stated reason both of those are there: a send is an **effect promise, not
an event**, nothing in the log says one happened, and a rebuild must not destroy
it. The module still owns no projections.

**Correlation is against the reply's own instant, never the clock.** This is the
whole trick of the phase. Asked against *now*, "what does this answer" gives a
different answer every time it is asked — a reply landing on one booking this
minute and another the next, as later reminders go out. Asked as of when they
replied, the answer never moves; so the sweep that lands inbound messages can
re-run over an overlapping window for ever, with **no cursor and no checkpoint
table**, and the same webhook always lands in the same place. It is the same
shape 13c uses with derived ids: do not remember what you did, make what you do
deterministic.

**Three questions in order.** What was last said to that number; whose number it
is; and failing both, the tray. The number is matched **exactly** — no
normalisation and no last-nine-digits heuristic, because a heuristic that
resolves `+966500000001` and `0500000001` to each other also eventually resolves
two people to one, and putting one customer's reply into another customer's
conversation is a worse failure than not matching. The tray is what catches the
rest, and it moves what has arrived without binding the number: the fix that
stops it recurring is the number on the customer record, which is `crm`'s.

**A thread is named by what it is about.** `v5("{topic}:{id}")`, so opening the
conversation about a booking is a computation, nothing is created before the
first note, and two people opening it at once open one thread. The projection
gets something for free from that: it can *recompute* an id to tell which
derivation produced the one it is looking at — which is how it knows a thread is
the tray for a number rather than a real subject's, without a flag on the event
or a column to keep in step.

**Two of the four channels are refused, and say why.** WhatsApp takes
pre-approved templates outside a 24-hour service window (§26) and every message
typed into a thread is outside one, so promising it would be this system
pretending; push addresses a device rather than a person. `sms` and `email` are
what a person may pick, and a customer with no address on the one they picked is
a refusal naming it.

**Reading a thread needs `PostEntries`** — the one place in this API where
reading is not the most permissive capability. A thread holds staff's private
notes about a customer, and `Read` is the role for an external accountant at
year end: every reason to see the books, none to see what the front desk wrote.

### 47 · The bell: who a notification is for, and why no module may ring it

**A notification had nowhere to live.** `messaging` reaches people well and
records nothing: a reminder that went out at 09:00 exists in an outbox row and
somebody else's gateway logs. So anything the system noticed and nobody was
waiting for was lost — an iqama expiring in three weeks was logged as a *health
finding*, at `error`, beside "the event log is not contiguous", because there
was nowhere else to put it.

**The two planes made "who" the hard part.** An audience resolves to an
*employee*, which is tenant-plane; a bell is read by whoever *logs in*, which is
a control-plane identity. 9c deliberately refused to bridge those for
authorization, and that stands — nothing about what somebody may do reads this.
What 13c adds is one optional field, `hr.employee.identity`, that answers a
different question: **which bell rings**. It grants nothing, and the reader
still needs the module role to see anything at all. Identity ids were already in
the tenant log — `Metadata.actor` has carried one on every event a person caused
since the log existed — so this crosses nothing that was not already crossed.

**A domain module can never announce, and that is the dependency graph talking.**

```text
notifications  →  messaging  →  booking, crm, hr, sales
        ↑
erp-api, bin/worker.rs   (the composition roots)
```

Announcing resolves an audience, which is `messaging`'s work, which reads the
domain modules' read models. `booking → notifications` would close that loop and
cargo would refuse to build it. So announcements are raised from **above** the
modules — worker jobs — which is where three of the four producers already were.
It costs nothing in practice: a bell tells somebody about something they were
*not* doing, and what a person did themselves needs no announcement.

**Every producer is a scan that may run twice.** The notification's aggregate id
is `Uuid::new_v5` over the kind and the subject, so announcing the same thing
again loads an aggregate that exists and writes nothing. That single fact
removes the queue this would otherwise need: no cursor, no checkpoint table, no
exactly-once delivery to get wrong. Each producer sweeps a six-hour window every
tick and announces all of it; the overlap is free.

**A document belongs to the business, and that was a bug found by building on
it.** `Audience::BranchManager` resolved through the branch a subject is at —
and an invoice has no branch, because where its postings landed is `ledger`'s
and a different projection group. So three of the five kinds would have resolved
to nobody for ever, silently, and a template about an invoice addressed to a
branch manager had *already* been in that state since Phase 11. `messaging` now
reads a subject with no branch as the business itself: whoever reports to nobody
at all. A record that is not there still resolves to nobody, which is the
distinction that keeps the two apart.

**What a person can say for themselves** is a grid of kind × channel, defaulting
to the bell and nothing billable. A default that spends money is a default
nobody chose.

**The expiring-document finding stays.** It now has a bell as well, and that is
not duplication: the finding is the operator's channel and the bell is the
tenant's, and a tenant without the module — or with nobody linked to a login —
would otherwise be told by nobody at all.

### 46 · The signal stream: two screens and a phone agree, and nobody polled

**Every read was a poll, and the phase said so.** A booking from a phone had to
reach every counter screen without a refresh, and `pg_notify` was refused by D4.
What replaces polling is the shape 13a wrote down before it was built: a
server-sent-event stream that carries *group `booking` is queryable through N*
and nothing else, published by the worker **after** the projection commit —
the `Advanced` arm in `jobs.rs`, the one moment the guarantee is true — and
fanned out over the Redis channel `shared.rs` already had. The client
re-fetches through the ordinary API with `consistent_after`, which already does
authorization, localization and paging. A stream holds a receiver, a deadline
and a list of module ids; never a connection.

**Two surfaces, apart on purpose.** Staff watch a tenant on `GET /v1/events`;
a customer's phone watches one reservation on
`GET /v1/booking/public/reservations/{id}/events`, keyed by the id only the
phone holds, behind the public limiter. They are different populations —
dozens against thousands — so the hub keeps them in separate registries with
separate caps (`REALTIME_STAFF_STREAMS_PER_TENANT`,
`REALTIME_PUBLIC_STREAMS_PER_TENANT`), and a public signal is never routed
through a staff sender. For the phone to be woken only for its own booking
without a database read per open stream, the signal names the streams a batch
touched — `Progress::Advanced.streams`, bounded at 256, `None` for "many".

**Reconnect is the reconcile.** Every stream's first event is `ready`, the
checkpoint of every visible group; every stream ends at ten minutes with
`reconnect`, so authorization is re-run by the reconnect rather than outlived;
and a watcher behind the 64-signal buffer is sent `reconnect` too, because
`TenantDb` is deliberately not `Clone` and the cheapest fresh snapshot that
touches no database from inside a stream is the next `ready`. The deposit
status gained `consistent_after` so the phone's re-fetch has the same
guarantee a screen's has.

**The exit criterion is a test:**
`two_screens_and_a_phone_agree_within_a_second_and_nobody_polled` books through
the public route, plays the worker the way `ProjectionJob::tick` does, and reads
`advanced` at the committed position on two staff streams and the phone's, then
reads the reservation at that position. Beside it:
`ready_names_every_group_the_tenant_may_see_and_nothing_else`,
`a_signal_for_a_module_the_tenant_lacks_is_not_delivered`,
`opening_a_stream_asks_for_a_visit`, `without_redis_nothing_can_be_watched`,
`the_caps_refuse_the_stream_past_them`,
`a_stream_ends_after_its_lifetime_with_reconnect`,
`a_phone_hears_only_its_own_reservation`; the worker's
`a_projection_that_advances_signals_once_with_the_committed_position`; the
runner's `an_advance_names_the_streams_it_touched`; the hub's four unit tests;
and `an_advance_published_is_received_by_a_subscriber` over a real Redis.

**Left by decision:** 13d conversations; `Last-Event-ID` replay; per-branch
filtering; public streams for anything but a reservation, which the subject
registry is ready for. 13c is built — see §47, which rides on this one: a
notification reaches a screen as *group `notifications` is queryable through N*
and nothing else.

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

### 44 · The final invoice after a deposit, and the credit note for part of one

**Nothing billed a booking.** The deposit raised its prepayment invoice when
it settled, because receiving consideration is a tax point; the service was
delivered; and then nothing — the counter or a person typed an invoice, or
did not, and the deposit's tax stood declared with no document for the rest.
ZATCA's shape for the rest is a final invoice (388) that shows the whole
supply, names the prepayment invoice on a line of its own with what it
declared, deducts that in `PrepaidAmount`, and declares tax on the remainder.

**The deduction is `sales`'.** `Draft::prepaid` names the prepayment invoice
and carries its bands; `issue_in` subtracts them band by band
(`Totals::less`) and refuses a band the supply does not have or a prepayment
larger than it. The event keeps the whole supply on its lines and **the
remainder in its totals** — what this document charges, posts and declares —
so the ledger, the revenue report, the reconciliation and the return all keep
working unchanged and the deposit's tax is declared once. Only the ZATCA
rendering has to know both: `Document::supply_gross` for the tax-inclusive
total and the QR, the prepayment line from `PrepaidRef`, and the payable
amount from the totals. `the_final_invoice_after_a_deposit_charges_and_declares_only_the_rest`
and `a_final_invoice_after_a_deposit_shows_the_supply_and_deducts_the_prepayment`
are the tests; the rendering is built from ZATCA's published fields and the
sandbox call is still the operator's.

**The composition is `erp_api::billing`, and it runs twice.** The desk asks at
`POST /v1/booking/reservations/{reservation}/invoice`; the worker asks for
every completed, priced, unbilled booking when `PUT /v1/booking/billing` has
`on_completion` on — the user's decision, both, with a per-tenant switch. One
function reads what `booking` and `sales` say in their read models (a route
or a worker may load no aggregate), derives the invoice id from the booking
(`bk-<reservation>`), and issues the invoice and records `Billed` on the
booking in one transaction. Asked twice, or by the desk and the worker at
once, it raises one document: `sales` answers the existing invoice on a
retry, `booking` records once, and the worker counts only what was actually
raised — the first version counted a read model that had not caught up as
work, and the test caught it. `a_completed_booking_is_billed_with_its_deposit_deducted`
runs the whole story over HTTP, ZATCA document included.

**The partial credit note.** §35 left a partial refund of a deposit with the
money back and the prepayment invoice standing whole. `credit_what_is_clear`
now takes what went back: when the invoice still holds money and has **one
tax band**, it finds the net that, taxed at that rate the way the invoice
was, comes to exactly the refund — checked forwards rather than divided
backwards, because at 15% no net comes to 10.00 — spreads it over the lines
and issues a partial credit note keyed on the refund's reference, so a
retried refund credits once. A multi-band invoice, or a gross no net lands
on, is left as it was and says so in the log. `part_of_a_deposit_can_be_returned`
and `a_partial_refund_credits_the_part_when_the_invoice_has_one_band` are
the tests.

**Not built, by decision:** the never-pays blacklist. A public booking has no
customer record to bar, and keying it on a phone number was offered and
declined in favour of waiting for customer accounts.

### 43 · A lender's checkout, and the price a stranger could not send

**Found by writing the test, not by reading.** §39 said the loop ran end to
end: a public booking records what holding the slot costs, the customer pays,
the worker settles. It did in the module tests, which price the booking the
way the counter does. Over HTTP it never had: `public_lines` set `charge:
None` on every line — a stranger may not send a price, and nothing else
supplied one — so `deposit_for` found nothing to take a fraction of, the
deposit route answered *not waiting to be paid* to every booking ever made
through the site, and neither Tabby nor Tamara had a caller anywhere.

**A published rate, not a catalogue.** Phase 8d refused a price list on the
server because nothing had asked for one. A public booking asks: the price
has to come from the business, and the smallest thing that is the business's
own is one optional field on a bookable — `rate`, before tax, shown on
`GET /v1/booking/public/services` and stamped onto a public line as its
charge. `what` stays opaque, the counter still sends what it charges, and the
tenant's bands still decide when it costs more. A service with no rate books
unpriced, which is a business that bills elsewhere and asks for no deposit.

**A lender is told everything up front, and the worker opens the page.** A
card is paid in the browser against this system's id. A lender hosts its own
page and has to be told who is buying, where the service is delivered, and
where to send them afterwards — Tabby's schema marks the buyer's history and
a shipping address required, and both providers want landing pages. So the
deposit route takes a `provider`, and for a lender the email, the phone (the
booking's own by default) and three `return_to` pages, each of which must be
on an origin the business has allowed — the same list CORS answers from, so a
lender never sends a customer back to a page somebody else named. The
address is the branch the first resource belongs to, or the business's ZATCA
registration; with neither the request is refused by name rather than filled
with dashes, because a placeholder is one more thing the lender scores.

All of it is frozen on the request as `payments::Checkout` (L5): the worker
may load no aggregate and may not name `booking` or `branches` (L7), so the
`Requested` event carries what the lender will be told, and what it was told
is what this system recorded telling it. Opening the checkout is an outbound
call, so it is the worker's — `open_checkouts`, beside the saved-card pass —
and the page it answers with is recorded as `pay_at` for the `GET` beside the
route to hand the waiting customer. A tick, not a request handler holding a
connection for as long as somebody else's server takes.

**Capture is the sweep's.** Both lenders authorise when the customer commits
and settle only what the merchant captures; nothing in this system asked. So
`settle_pending` captures an authorised payment in full and once, under
`<payment>.capture`, before anything is posted — and not at all when the
gateway authorised a different figure, which is failed with both numbers
rather than captured and refused a line later. Reading Tamara's `approved`
as "waiting on the customer" would have left every Tamara order there until
it expired; it is the merchant's turn, and the adapter now says so and asks
the order where it stands before authorising, because authorising an
already-authorised order is a `409`.

**What a retry costs.** A lender's id for a session is not known until it is
created, so there is no fetch-before-charge here the way there is for a card:
a pass that died between creating a session and recording it opens a second
one next tick. That is an unpaid page, not a second charge, and the customer
is sent to the one that was recorded.

**Still ahead:** a sandbox call against either lender, which is the
operator's. The 388 and the partial credit note followed in §44.

### 42 · The specialist blacklist, as a constraint the command enforces

§41 ended by saying this was the one worth building next, and by saying why it
is not a custom field: *"never book this customer with that stylist"* written in
a text box is a line a manager believes the system is holding and it has never
looked at. So it is checked in the command, against the log, and a reservation
that would break it is **refused**. There is no override — a bar a click can
step past is a note again, and these are not set for the reasons notes are.

**An aggregate rather than a row**, for the reason `branches` and
`crm::accepts_documents` are: a bar raised a moment ago has to stop the very next
booking, and a projection is another checkpoint that can lag. The window where
the rule is set and not yet enforced is exactly the window somebody raising one
in a hurry is standing in. The projection exists, and nothing reads it to decide
anything — it is the screen.

**One aggregate per customer, not per pair.** A reservation names several
resources at once, so answering "may this be booked" has to be one load.

#### Three doors, not one

Checking `reserve` alone leaves two ways round it, and both are ordinary use:

- **Reschedule.** Book on a stylist who is clear, then move the appointment onto
  the barred one.
- **Assign.** Book *"any stylist"* — a pool names nothing barred — and pick the
  barred one out of it afterwards.

Both are checked, against the reservation's **own** customer read from the log
rather than one the caller passes, and each is falsified separately:
removing the check from any one of the three makes a test fail.

#### Only somebody the system can recognise

Bars are keyed on a `crm` customer, so a walk-in booked under a typed-in name is
barred from nothing. That is a real limit and there is a test named for it
rather than a silence: barring somebody means recognising them next time, and a
name in a box is not a recognition.

**And that includes the public site**, where §39 sets `customer: None` on every
booking on purpose — a stranger does not get to name which customer record they
are, and the business matches it afterwards. So a barred customer can still book
through the form. Saying it plainly is better than the alternative, which is a
business finding out: what closes it is public bookings resolving to a record,
which is the customer-account work §41 gestures at and not something a check in
`reserve` can do.

#### Two smaller decisions

**The reason is required, and never reaches the customer.** Whoever is refusing
a booking at the desk has to be able to explain it, so `why` is mandatory and
refused when blank. The refusal a booking gets names the resource and nothing
else — repeating the reason back would say to somebody's face what a colleague
wrote about them.

**Lifting carries no reason.** Who lifted it is the event's actor and when is
`at`, which is what the question afterwards is about — and a field no screen can
fill is a field that is always empty. The bar is lifted rather than deleted,
because "was there ever one" is the question somebody asks later.

**Both ends are checked before a bar is written.** A typo in either id would
otherwise be accepted, written, and enforce nothing — and the person who typed it
would go away believing a rule was in place. A **withdrawn** resource is still
barrable on purpose: a stylist on leave is exactly who a complaint arrives
about, and the bar has to be waiting when they come back.

**A bar is about the next booking, not the ones already made.** Raising one does
not cancel Tuesday's appointment. What to do about a booking that already exists
is a conversation somebody has to have, and a system that quietly emptied the
diary would be taking that decision off them — so there is a test named for it
rather than a surprise discovered on Tuesday.

**Not the other blacklist.** §39 leaves *"blacklisting a customer who
repeatedly books and never pays"* still ahead. That one is automatic, counts
something, and is about the whole business; this one is a person's decision
about one pair and counts nothing. They share a word and no mechanism.

**Raising and lifting are the owner's.** Not because reading a complaint is
above a clerk, but because there is no override: this refuses every booking
between the two and nobody can click past it, which is a heavier lever than
taking a chair out of service — already the owner's. Reading the list is
everybody's, because a receptionist has to know why the diary just refused them.

### 41 · Custom fields on a customer, typed, and erasable by construction

**Typed, not a blob, and the reasons are three concrete failures.** A blob
cannot be *validated* — a date typed as `03/04` stores happily and is discovered
a year later. It cannot be *filtered* — "everybody allergic to latex" is a scan
and a guess about spelling. And it cannot be *shown properly*, because nothing
knows whether a value is a date, a number or a choice from a list somebody
agreed on. Declaring the field settles all three at the one moment a person is
around to answer them.

Five kinds — text, number, date, choice, flag — and each is a column and a
check. A kind that could be neither validated on the way in nor filtered on the
way out would be a blob wearing a name.

**No decimals**, and that is the workspace rule rather than laziness: a height is
centimetres and a weight is grams, and the unit belongs in the label where a
person reads it.

**The choice kind is the one that earns its keep.** "Blood group" as free text is
nine spellings of four answers.

#### Why the values are not in the event log

Because an append-only log cannot forget, and this data has to be forgettable. A
wellness centre's fields hold a person's health details; under the PDPL those
carry a right to erasure, and this document has already had to fix one place
where the answer was *"our schema will not let us"*.

So the split is §30's, applied again: **the shape is declared and replayed —
`crm.fields`, versioned configuration — and the values live in a table where a
delete is a delete.** A rebuild does not reproduce them, which is correct rather
than unfortunate: a rebuild is a function of the log, and if it could reproduce
them the delete would not have been one.

**What is not lost is the history.** A value is superseded rather than
overwritten, so "what did this say in March, and who changed it" is answerable —
and erasing takes the superseded rows with it, because a deletion that left the
old value behind would not be one.

#### The three ways this quietly loses data, and the guard for each

- **A field removed with its values still in the table** is exactly how a
  business comes to hold health details it has forgotten about. Removing one is
  refused while anybody holds a value, `GET /v1/crm/fields/orphaned` finds any
  that slipped through, and there is a route to erase them.
- **A field redefined under its own values** — text to date — leaves every stored
  value unreadable. Refused the same way, and `held` skips a value whose stored
  shape and declared shape disagree rather than showing a date as a number.
- **Half a form stored.** Every value is checked before any is written, so one
  bad date keeps what was there rather than writing the rest.

#### Required is a worklist, not a gate

A tenant who adds a required field has a thousand customers missing it that
instant, and refusing to amend any of them until somebody filled it in would
make the field impossible to add. So it is reported — `Fields::missing_from` —
and the one thing it does refuse is *deliberately clearing* one, which is a
different act.

#### Two of the three examples were not custom fields

Asked for alongside this were private notes and a specialist blacklist. Notes
are a notes feature — an author, a timestamp, and who may read it — and modelling
them as a field loses all three. **A specialist blacklist is not a field at
all**: "never book this customer with that stylist" is a constraint the booking
command has to enforce, and as a text box nobody enforces it, which is worse than
not having it — a manager reads it and believes the system is holding a line it
has never looked at. Neither is built here, and the blacklist is the one worth
building next — it is §42.

#### And a guard caught the routes in the wrong place

The first version mounted the value routes at `/v1/customers/{customer}/fields`,
outside `crm`'s own name. `every_modules_routes_live_under_its_own_name` failed
immediately, and the reason is the one that matters: `Allowed<C>` reads the
module out of the path, so a route outside its module's namespace is judged on
the **tenant-wide** role instead of the module-scoped one — the more permissive
answer, arrived at silently. They are `/v1/crm/customers/...` now.

Worth recording because nothing about writing the route suggests it. The
attribute is in the module, the handler is in the module, the capability is
named on the extractor, and the whole thing is wrong in a way only a test that
knows how authorization resolves could see.

#### Falsified rather than trusted

Narrowing the erasure to current values leaves the superseded row behind and
`erasing_a_customers_fields_takes_the_history_with_it` fails. Checking values one
at a time instead of all first stores half the form and
`a_bad_value_stores_none_of_the_others` fails. Both pass with the code restored.

### 40 · Verifying a phone is a setting, and it is not the anti-abuse control

**`PublicBooking.verify_phone`, off by default.** A business that must reach
whoever booked — a clinic, anyone whose no-show costs a slot they cannot resell
— turns it on; one taking a deposit on every booking already has what it needs,
and a second step before a stranger can book costs them bookings.

**The default is a judgement, not a shrug.** What stops a booking form being
spammed is the **deposit**: a slot that cannot be held without paying for it
cannot be spammed by anybody, verified or not. What a verified number buys is
being able to *reach* somebody, which is a real need and a different one. Saying
so is the whole reason this is a setting rather than a rule.

**And a setting that gates nothing would be `deposit_bp` all over again**, which
this document has now criticised three times. So the mechanism is built:
`POST /v1/booking/public/verifications` sends a code, and the reservation route
spends it — in the request that takes the booking, so one code cannot hold two
slots.

**It is deliberately not the control plane's `one_time_code`.** That one signs
somebody in: verifying it mints a session and, for an unknown number, an
identity. Wrong here in two ways. A customer is a `crm` record and not an
account, so a booking form that created identities would fill the fleet's
identity table with people who have no business in it. And worse — a public form
that can request a **sign-in** code for any number is a way to make a staff
member's phone buzz with a real code that a caller can then talk them into
reading out. Codes here live in the tenant's own table, and nothing but a
booking reads one.

The rest is `erp_control::otp`'s shape because it earned it: two limiters
because they fail differently — a cooldown per number against somebody using the
form to send texts, attempts per code against guessing — six digits, five
minutes, single use, and one answer for wrong, expired, used and never-issued,
because telling them apart tells a guesser which half of the pair they got right.

**Where each half lives.** `booking` owns issuing and claiming, which need no
messaging; `erp-api` owns the route, because sending does. The code and its text
are promised in one transaction (D9), so a code stored and never sent is not a
state this can reach.

**Falsified rather than trusted**: dropping `used_at IS NULL` from the claim
lets one code hold two bookings, and `a_verification_code_holds_one_booking_and_no_more`
fails. Restoring it passes.

### 39 · Deposits at booking, and the public surface that takes them

**The loop, end to end.** A public booking records what holding the slot costs;
the customer pays that charge in their own browser; the worker asks the gateway
whether they have; settling raises the prepayment invoice and tells the diary
its slot is paid for; anything nobody paid for lapses on its own.

**What a stranger cannot decide is the amount.** It is worked out from the
booking — the fraction the business set, of what the booking was priced at, with
tax at the rate the tenant configured — and the request carries no money at all.
That is what makes an unauthenticated write that touches money safe to have: the
worst it can do is create a charge the caller would have to pay themselves.

**And they cannot choose which payment.** The `Idempotency-Key` becomes the
charge's id and is passed to the gateway as its own, so the browser pays *this*
charge rather than reporting back about one it picked. Without that, anybody who
learned a payment id could attach a stranger's money to their own booking — and
a gateway id is not a secret.

**Neither module may name the other, so two seams carry it.**

- *Into* `booking`: `Secured` takes an **opaque payment id** and asks nothing.
  Whether money is real is not a question a diary can answer, so the caller that
  knows both tells it. The same shape `prepaid` uses for what an entitlement is
  held against.
- *Out of* `payments`: `Swept::secured` **reports** which deposits settled and
  what they were held against, and acts on none of it.

The worker joins them, because it depends on both and neither may depend on the
other — `requires` is a hard AND, so one direction forces a diary on every shop
that takes a card and the other a gateway on every salon.

**It is a repair, not a step, and that is deliberate.** The join cannot be in the
settling transaction: they are different modules' aggregates, and the money must
commit whatever the diary says. So `secure_in` runs for anything settled on the
pass and is a no-op on a booking already told — a failure leaves a paid deposit
on a booking that still looks unpaid, which the next tick fixes.

**The expiry job asks one question of one group.** "Still reserved, a deposit was
asked for, past its deadline, nothing paid it" — four columns on
`proj_booking.reservation`, because whether the money arrived is a fact the
module was *told*. A job joining `proj_booking` to `proj_payments` would be
reading two checkpoints that can disagree, and the disagreement it would hit is
exactly the one that matters: a deposit settled a moment ago whose booking has
not heard yet. The ordering only fails safe — `Secured` is written first, so the
worst case is a slot released a tick late rather than one released after it was
paid for.

**The route lives in `erp-api`**, which is the exception "modules ship their own
routes" needed: it is about the seam rather than about a module, and `erp-api` is
where everything is already assembled.

**What is deliberately not here: verified phone numbers.** The plan called for
public bookers to be registered with a verified phone before they can book, and
that is a customer-identity system — the OTP machinery is control-plane and
issues codes to *staff*. It is also not what stops abuse here: **the deposit is**.
A booking that cannot be held without paying for it cannot be spammed, which is
what deposits are for. Verifying a phone is about being able to *reach* somebody,
which is a real need and a different one.

Still ahead, and named: a public route that answers what a booking is waiting to
be paid (the deposit route creates the charge but there is no read beside it),
and blacklisting a customer who repeatedly books and never pays, which needs the
`crm` additions §18 parks.

### 38 · Two of the credit-note tests asserted nothing, and falsification said so

**The question was whether a fully credited invoice refuses the next credit
note.** It does, and it always did — but it was not tested, and writing the test
turned up something worse.

The new tests passed on the first run, which is the moment to be suspicious
rather than pleased. So both caps were removed in turn to see which tests
noticed.

**Removing the band cap** failed the two tests that name it. Good.

**Removing the line cap failed nothing.** All three tests that claimed to
exercise it still passed, because every case they set up was *also* over the
band:

- `credits_are_capped_per_line_and_not_by_the_total` used one standard-rated
  line and one zero-rated one, so crediting 1,000 of standard against a band of
  500 was caught by the band.
- `one_credit_note_cannot_take_a_line_twice` used a single line, so 60 + 60
  against a band of 100 was caught by the band.

Both were renamed and rebuilt around **two lines at the same rate**, which is the
only shape where the line cap is the thing doing the work: 500 off a line of 300
sits inside a band of 500 and is still more of that item than was ever sold.
Removing the line cap now fails both, and restoring it passes both.

**The lesson is the one this document keeps relearning**: a test that passes the
first time has not been shown to test anything. The cheap way to find out is to
break the code it names and watch. It cost ten minutes and it caught two tests
that would have sat there looking like protection.

**What was actually asked, answered:** a fully credited invoice refuses the next
credit note, on either route to exhaustion — line by line where there is no
document discount, and by the band where there is, since a discounted invoice
still has line room left over once it has been credited for what it charged.
`a_fully_credited_invoice_refuses_another_credit_note` and
`a_discounted_invoice_credited_to_its_band_refuses_the_line_room_left_over` are
the two, and the second is the one that fails if the band cap is ever dropped as
redundant.

### 37 · A credit note names a line, and the compatibility gate had a hole

**One mode, not two.** A credit line used to carry a description, an amount and
a treatment, all typed by the caller — so a credit note could describe something
the invoice never sold, at a rate it never charged, and the only thing stopping
the second was a band lookup. It now names a **line**, and the description and
the rate come off that line. Crediting a treatment the invoice never carried
stopped being a refusal and became unrepresentable.

**Both caps, because they catch different things.** The line cap stops a credit
note taking back more of an item than was sold. The band cap is still there and
still needed: a document discount comes off the *band*, so an invoice's lines
sum to more than its bands whenever it carried one. Two lines of 100 with 50 off
the document were charged 150 — the per-line cap would allow both in full, and
the band cap is what refuses the extra 50 and the 7.50 of VAT never collected on
it. `the_band_cap_still_bites_when_a_document_discount_shrank_the_invoice` is
that test, and it is the one worth reading.

Two lines of one credit note naming the same invoice line are capped on what
they come to together, which is a separate test because it is a separate way to
get it wrong.

**And the gate that should have caught the API break did not.**
`compatibility.rs` reported nothing when `against` appeared as a required field.
Its `required_of` read only the **top level** of a request schema, so a required
field appearing inside a nested object — a new field on each element of `lines`
— was invisible. That is exactly the defect the response side had, found and
fixed when a rename of `ServiceView::name` sailed through; the two halves had
simply drifted apart and only one was ever repaired.

It walks now, with one rule the response side does not need: **it descends only
into properties that are themselves required.** A field required inside an
*optional* object breaks nobody, because a caller who omits the object omits the
field with it — and reporting those trains people to run `just baseline` without
reading it, which is worse than not having the gate.

Verified by falsification rather than by argument: with a baseline doctored to
hold the old line shape, it reports *"credit_invoice_part now requires
`lines.against` in its body"*, and without the fix it reports nothing.

**No baseline was accepted.** The credit-note path is newer than the last
`just baseline`, so there was no promise to break — which is also why the hole
went unnoticed at the time.

### 36 · A discount can belong to a line, and ZATCA has always said so

**Checked, because I had it wrong twice in a row.** §35 was written on a forum
quote — *"once you provide discounts at line level, there is no need to provide
a sub-total or aggregation of line level discount amount again anywhere else"* —
passed on as guidance without reading it against the standard, which is the same
mistake as guessing at a tax point. The correction came from being asked whether
line discounts must still reach the document summaries. They must, and they do:

| | |
|---|---|
| **BT-131** line net amount | already **after** the line's own allowances |
| **BR-CO-10** | `LineExtensionAmount` (BT-106) = Σ BT-131 |
| **BR-CO-11** | `AllowanceTotalAmount` (BT-107) = Σ **document-level** allowances only |
| **BR-CO-13** | BT-109 = BT-106 − BT-107 + BT-108 |

So a line allowance reaches the totals by making its line smaller, and putting
it in BT-107 as well would double-count it. The forum was right about BT-107 and
the sentence read as though line discounts never touch the totals, which is
false.

**And the line-level allowance is a different element from the document one.**
`cac:AllowanceCharge` inside `cac:InvoiceLine` takes an indicator, an amount and
a reason, and has **no `cac:TaxCategory`** — the line it sits in already says how
it is taxed. The document-level one has no line to inherit from, so it must name
the category and the rate or the taxable amounts do not add up. That asymmetry is
now the difference between `sales::Allowance` and `sales::Discount`, and it is
why one writer emits a category and the other does not.

**What changed.** `DraftLine` and `InvoiceLine` carry `allowances`, and
`InvoiceLine::net` is the amount *after* them — BT-131, which is what `vat::total`
was already summing, so the band arithmetic needed no change at all. `tax_sa`
emits them inside the line, before `cac:TaxTotal` because the sequence is the
schema's, and `cbc:PriceAmount` became the figure *before* the allowances so
BT-131 holds. Both are `#[serde(default)]`, so every invoice issued before this
decodes as one with none — which is what it was.

**What this unblocks.** A credit note that names a line. §32's cap is on the
band because a line's face value was not what it was charged once a document
discount existed; a line that carries its own allowances *is* what it was
charged, so capping per line becomes exact. That is the follow-on, not done here.

**Still to confirm against a sandbox**, and named rather than assumed: the
per-category taxable amount in `cac:TaxSubtotal` with line allowances present.
The rendering is right by the rules above; the only way to know ZATCA agrees is
to submit one, and that needs a real certificate.

### 35 · The deposit is a document, and that undid most of §33 and §34

**Three questions, one mistake.** Asked in this order: if the deposit is taxed,
why is it not already an invoice; is a cancellation not just a credit note and a
refund; and if not, why derive the tax out of a total instead of keeping it from
the start. All three were right, and all three were the same error seen from a
different side.

**The tax point is receipt.** VAT falls due on the earliest of supply, invoice
and consideration received, so a deposit is taxable the day the customer pays.
§33 held it in a liability with no document and declared nothing — and §34 then
declared it at *retention*, which is whatever quarter the customer failed to turn
up in. The amount came out right eventually; the period did not, which is the
same defect §31 fixed pointing the other way.

So `settle_in` raises a **prepayment invoice** — ZATCA type 386, a `sales`
invoice with `prepayment: true` — in the transaction that records the money.
Everything after that is an ordinary invoice payment.

**And that is what makes a cancellation just a credit note and a refund.**
`refund_in` no longer has two paths: a deposit has an invoice like anything else,
so giving one back is `sales::refund_in` plus the credit note §31 already wired.
The `Advance` case is the only place that still knows a deposit is different, and
only for as long as it takes to raise the document.

**The rounding was self-inflicted.** `net_of_gross` existed to divide a gross
deposit back into a net and a tax, and could not always land — at 15% no net
comes to exactly 10.00. But the net was never unavailable:
`booking::pricing::Charged` has carried a net and a gross since 8d. So `Advance`
carries the net, the tax runs forwards from it the way it does on every other
invoice, and the function and its residue account are gone. Writing careful code
to recover information already thrown away is the shape of the error, and it is
the part that should have been caught while writing it.

**What §34's setting became.** It cannot decide the tax — that was settled by
the tax point, and the guidance is not to reverse a prepayment that was kept. So
`Retention` decides whether the money is a **sale**: the default leaves it as one
and posts nothing at all, and a business whose adviser disagrees moves the net
into `4910 Forfeited deposits` while the tax stays declared. The field is
`supply`, not `taxable`, because a name that claims to move tax and does not is
worse than no setting at all.

**What went in the bin:** `2410 Customer deposits received` and every entry that
posted to it, `net_of_gross` and its residue, `entry_for_advance`,
`entry_for_advance_refund`, `entry_for_applying_advance`, and the invoice-raising
half of `retain_in`. Two turns' work, most of it deleted by getting the tax point
right.

**One gap left, and it is named.** A *partial* refund of a deposit gives the
money back and leaves the prepayment invoice standing for the full amount,
because crediting part of an invoice needs to know which band the part came out
of — the allocation §32 refuses to guess. Every deposit is a single-band invoice,
where there is only one answer, so teaching `credit_what_is_clear` to issue a
partial credit note in that case is the next thing to build.

### 33 · A payment can collect against something that is not an invoice

**The assumption ran all the way through.** `Started` carried an invoice,
`settle_in` called `sales::pay_in` with it, and every entry cleared a
receivable. Right for a customer paying a bill; wrong for the one that pays for
a booking before there is a bill at all — and 12a was built entirely on the
first.

`Collects` is the widening: an invoice, or an **advance** against something
opaque. Settling an invoice payment is unchanged. Settling an advance posts
`Dr clearing / Cr 2410` and calls `sales` not at all, because there is no
receivable to clear and no supply to recognise.

**Two `Option`s on the wire, one enum in the domain.** `invoice` went from
required to optional and `advance_for` arrived beside it, both
`#[serde(default)]` — so every payment written before advances existed decodes
as exactly what it was, with no upcaster and no event version two. `Collects::of`
is where the pair becomes a decision, and it refuses *neither* and *both*; the
`payment_collects_one_thing` check says the same in the database, because a
row that means nothing is worse than one that is refused.

**Corrected by §35.** This section's first version held the money in a
liability and declared no tax, which put the output tax in the wrong period. A
deposit is billed by a prepayment invoice the moment it settles, and
`Collects::Advance` survives as what it should always have been: the state
between a charge being created and the gateway confirming it, and no longer.

**What this does not do.** It does not take a deposit at booking. It makes the
money-shaped half work: a gateway payment, a saved-card charge or a callback can
name a booking, the document is raised when the money is real, and a refund is a
credit note like any other. The rest — a public surface a stranger can pay on, a
phone they have verified, and a hold that expires — is still ahead.

### 32 · Partial credit notes, and why they needed a table

**The thing that unblocked them was research, not code.** A cancellation policy
that keeps half a deposit needs a credit note for half an invoice, and this
system could only credit whole ones. ZATCA's shape settled how: a credit note
(381) is a document with its own number, its own tax point and **its own
lines**, and the authority computes its VAT from those rather than from the
invoice it references.

That is what made the old `ponytail:` note in `install.sql` right — *"partial
credit notes would carry their own bands rather than borrowing the invoice's,
which is a table of their own"*. Three tables now: `credit_note`,
`credit_note_line`, `credit_note_tax`, mirroring the invoice's. The `vat_entry`
view unions them; a whole-invoice cancellation still borrows the invoice's bands
because it credits every one of them, and that arm is unchanged.

**It posts its own entry rather than reversing the invoice's.** `cancel_in`
calls `ledger::reverse_in`, which is exactly right for a whole invoice and
impossible for part of one — there is no reversing half a journal entry. So
`entry_for_credit` is `entry_for_issue` with the sides swapped, written out
rather than expressed as a negation, the way `entry_for_refund` already is.

**The rates come off the invoice, never today's configuration.** A caller sends
a treatment and an amount; the rate is looked up in the invoice's own bands.
That single lookup is both halves of the guard: an invoice issued at 5% is
credited at 5% for ever (L5), and a category the invoice never carried has no
rate to be given — which is `DiscountWithoutABand`'s argument one step along,
and the difference between crediting a supply and reclaiming tax nobody charged.

**Capped per band, cumulatively, not by the total.** An invoice of 500
standard-rated and 500 zero-rated credited 1,000 standard-rated has a gross that
agrees perfectly and reclaims 75 riyals of VAT that was never charged.
`credits_are_capped_per_band_and_not_by_the_total` is the test.

**The two shapes are mutually exclusive**, in both directions, and both refuse
with `sales.already_credited`. They share the credit-note series, because both
produce a credit note and the authority does not care which shape made one.

**One bug the tests found.** The first version set `invoice.credit_note` on a
partial credit and answered a retry from it. That is wrong the moment an invoice
has two — "the credit note" stops being a question with one answer. The
aggregate now keeps `(reference, number)` pairs and a retry is answered with the
number *its own reference* was given; `invoice.credit_note` stays what it always
meant, which is the cancellation's.

**What this unblocks.** A cancellation policy that keeps a fraction, which is
the shape every one of these businesses actually wants and which could only
express 0% or 100% before. That is the next piece, and §33 has yet to be
written.

### 31 · A refund issues the credit note, and the work found a second bug

**The gap was one missing call.** Everything a credit note needs already
existed — `sales::credit_in` reverses the issue entry and takes a number from
the gapless series, `tax_sa::documents` builds a ZATCA credit note from
`sales.invoice.cancelled`, the VAT return nets it, and the signing and
submission jobs carry it. `pos::take_back` had been composing refund-then-credit
since Phase 15. Neither refund path did, so a refunded invoice stayed cleared at
the full amount and kept declaring output tax on a supply that had been unwound.

`a_refund_takes_its_supply_out_of_the_vat_return` is the test worth reading: it
asserts the return goes to zero, which is where a tax authority would have
noticed and nothing in this system would have.

**Both paths, not just the one asked about.** `payments::refund_in` was the path
named, and `sales::refund_invoice` — the non-gateway route — had the same gap.
Fixing one would have left the other quietly wrong.

**The rule lives in `sales`, once.** `credit_what_is_clear` calls `credit_in`
and reads two refusals as "not yet, and that is fine": `HasPayments`, which
after a partial refund is simply true, and `AlreadyCancelled`, where the
document already exists. `payments` calls it rather than keeping a second copy
of which refusals mean what.

It is deliberately **not** folded into `sales::refund_in`. That one is a
per-money-movement primitive — a till calls it once per tender — and a credit
note is per document; crediting there would issue one against a single tender's
reference and then try again for every other.

**A partial refund still gets no document.** A credit note for part of an
invoice carries tax bands of its own, and how a refund of an arbitrary amount
divides across a standard-rated line and a zero-rated one is not something this
system may guess. `sales` recorded that as an open item in Phase 3d and it
stays open. What has changed is that the case is tested and named rather than
indistinguishable from success.

**And the bug the work exposed.** A retried refund was *refused*, not repeated:
a fully refunded payment is no longer collectable, so the second attempt got
`NotCollectable` — and the client that timed out on the first had no way to tell
that from a real failure. The route's own documentation said "sending it again
is a retry, not a second refund", which was not true of the code. It matters
more now than it did: that path was the one thing that could have issued a
**second credit note**, which is a statutory document that must not exist twice.
`Payment` now keeps the references it has already refunded — a seen-list, the
same shape `pos` grew after a till return took the drawer down twice — and
`a_retried_refund_issues_one_credit_note` holds it.

### 30 · A saved card lives in `payments`, and its token is not in the log

Three calls, all reversible, all made while building 12a's last piece.

**The token is sealed in `module_secret`, not written to an event.** Everything
else in this module is event-sourced and this half deliberately is not. The log
holds what a person recognises the card by — whose it is, `visa`, `4242`, `05/28`
— and the thing that charges it sits in the vault beside the gateway
credentials, under `payments.card.{id}`.

The weaker reason is that a token is a payment credential. It is much weaker
than a card number — a gateway acts on one only alongside the secret key — but
the credentials that build a client are already sealed for exactly this reason,
and a payment credential in the clear in a table that is copied into every
shadow schema, every rebuild and every demo is not where one belongs.

The reason that actually decides it: **"forget my card" has to mean it.** An
event log is append-only by design, so a token written into one is a token this
system holds for ever, and projecting it away changes nothing about that. Sealed
in a table, forgetting is a delete. `CardEvent::Forgotten` records *that* it
happened — history somebody may have to answer for — while the thing that could
charge it goes away. `forgetting_a_card_deletes_the_token_and_keeps_the_history`
is the test, and it asserts both halves.

The cost is that a rebuild does not restore tokens. That is correct rather than
unfortunate: a rebuild is a function of the log, and if it could restore a token
the delete would not have been one.

**It is a `payments` aggregate, not the `crm` record the plan called for.** The
plan said "a `crm` record pointing at a gateway token, and it belongs with the
domain half", which are two different places. The domain half won, for a reason
the plan could not have known when it was written: whatever holds the token is
read by the thing that charges it, and if the record lived in `crm` then
`payments` would be reading `crm`'s projection group to charge a card — the
cross-group read L3 exists to forbid. So `Card` is keyed on a `crm` customer id
and stores it as a reference nothing joins on, the same way a payment names an
invoice without depending on `sales`.

**Charging happens in the worker, and the route answers `202`.** A saved-card
charge is an outbound call to a third party, and this system already decided
where those go: ZATCA submissions and the settlement sweep are both worker jobs,
because a handler that waits on somebody else's server holds a database
connection for as long as that server feels like taking. So the route records
`PaymentEvent::Requested` and answers; `payments.settle` grew a charge pass that
runs before its settle pass, so a card charged on a tick settles on the same
tick.

Two consequences worth having written down. The `Idempotency-Key` **is** the
payment id **and** the gateway's — it is passed as Moyasar's `given_id` — so a
retry cannot become a second charge at any layer, and the route refuses a key
that is not a UUID rather than letting the worker discover Moyasar's rule later.
And the charge pass `fetch`es before it charges, so a pass that died between
charging and recording picks the payment up instead of sending it again. That
costs one extra call per saved-card payment, once. Moyasar's `given_id` is
supposed to make the retry safe without it, and probably does; what a duplicate
`given_id` actually answers is not something this build has verified, and a
double charge is not the place to find out.

**What this does not do.** A saved-card charge that raises a 3-D Secure
challenge cannot complete — there is no customer watching. It stays `pending`
with the gateway's challenge URL, which somebody can send to the customer, and
it shows up on `payment_pending` like anything else that has not resolved. That
is a property of charging a card with nobody present, not of this design, and
the honest thing is that it is visible rather than silently retried.

### 29 · Settlement reconciles, and the source of a settlement report does not exist yet

**What is built.** A payout is what a gateway actually sent; the payments it
covers say what it should have. `POST /v1/payments/payouts` records the transfer
and posts `Dr bank / Cr clearing`, and `GET /v1/payments/settlement` answers the
question underneath it: per provider, what has been settled and not yet paid
over. That number is what the clearing account should be holding, and a
disagreement between the two means a payment posted and its payout did not, or
the other way round.

**The difference posts.** It will not always agree — a chargeback, a fee the
settlement report explains and the payment did not, a rounding difference on a
conversion. Booking it to its own account (`5420`, in every chart) rather than
refusing the payout is the same call `pos` makes about a till that counts short:
a payout that cannot be recorded leaves the books saying the gateway still holds
money it has already sent, for ever, and the next reconciliation inherits that.
Its own account rather than lumped into fees, because a chargeback and a
processing fee are different facts about a business.

**A payout naming a payment that never settled is refused**, not skipped. The
arithmetic would otherwise run against a smaller set than the operator thinks,
and the missing amount would look like the gateway paying short — a
reconciliation failure invented by the reconciler.

**A payout with no list is allowed and reconciles nothing.** Somebody typing
from a bank statement has an amount and a date and no transaction list. It
posts, `covered` is zero, and the payments it did not name stay in
`awaiting_payout` — so the clearing account and the payouts now visibly
disagree, which is the honest answer rather than a reconciliation that always
agrees.

**What is not built, and this is the gap worth naming.** The plan said this
would be "the bank statement matching from Phase 8 pointed at a different
source". There is no such machinery: §10a already records that nothing in this
system has ever seen a bank statement, which is why `takings.paid_out` is named
for what it is. So a payout gets here because a person records one — from the
gateway's dashboard or their bank. Two ways to close that, neither guessed at
here:

- **A settlement-report import.** Every provider publishes one as a CSV, and
  11d's importer already exists. This is the small one, and it needs one file
  format per provider that somebody has actually downloaded.
- **The gateways' own payout APIs.** The research behind §25 covered payments
  and deliberately did not cover settlements, so what those endpoints are is
  genuinely unknown here. Writing them from memory is the failure mode this
  build keeps refusing.

### 28 · A webhook cannot settle a payment, so a sweep does

The callback surface built in 12b promises `webhook.{provider}` and waits for a
handler. Writing that handler is the obvious way to close the loop, and it does
not work: **`EffectHandler::deliver` is handed an effect and nothing else.** The
dispatcher holds no connection — a documented property, and the reason a slow
provider cannot exhaust a tenant's pool — so a handler cannot write a
settlement. `messaging` retires a push token from a sweep for the same reason.

That is the mechanical answer. The better reason is that **a callback is not a
reliable trigger**. Moyasar retries six times over about four hours and then
drops the message; Tamara documents no retry policy at all. A system that
settled only on callback would lose payments quietly, in the direction of a
customer who was charged and an invoice saying they were not.

So the callback is a **doorbell**: authenticated, recorded, acknowledged —
`payments::Doorbell` exists only so those effects do not pile up in the outbox
waiting for a handler that should not exist. `payments.settle` answers the door,
asking the gateway about everything still pending, whether or not the bell rang.

Three properties it has, each tested:

- **It stops rather than degrading.** An unreachable gateway ends that tenant's
  sweep and says so; the payments stay pending and the next tick tries again.
  Marking them anything else would be inventing a fact about somebody's money.
- **One bad answer does not strand the batch.** A gateway reporting an amount
  other than what was started is refused, logged loudly — that is exactly what
  somebody needs to look at — and the sweep moves on.
- **A sweep only asks its own provider's payments.** Asking Moyasar about a
  Tabby id would be a `NoSuchPayment` on every tick, for ever.

Credentials are sealed per provider under `payments.{provider}`, mirroring the
`webhooks.{provider}` key the callback secret already uses — per provider so
rotating one key does not rewrite the others, and so a sweep unseals only what
it is about to use. Without a `SEALING_KEY` the job is **not registered at
all**, which is louder than one that runs every tick and finds it can do
nothing.

**What this still does not do:** nothing abandons a payment. A checkout the
customer walked away from stays pending until the gateway itself reports it
expired or abandoned, which all three do. If that turns out not to be true for
one of them, the `payment_pending` index is where it will show.

### 27 · No payment gateway signs its webhooks, and 12b assumed one did

The inbound surface built in 12b verifies an HMAC-SHA256 over
`<timestamp>.<body>` in `x-webhook-signature`. That is the right shape, and it
is this system's own contract for a relay somebody writes.

**None of the three chosen gateways does anything like it.** Moyasar puts a
shared secret *inside the JSON body* as `secret_token` and signs nothing at all;
Tabby has no signature scheme whatsoever, offering at most a static header the
merchant names at registration; Tamara sends a JWT that authenticates the sender
rather than the payload. So the route as built would have rejected every real
callback.

Tamara's is worth its own sentence, because their documentation is wrong about
it. The notification token is HS256 over its own header and claims — `iss`,
`iat`, `exp` and nothing else. Their docs say this "ensures the payload was sent
from Tamara without any modifications"; **it does not**, because the token
commits to no part of the body, and it is also sent in the query string where it
lands in access logs. Anybody who captures one can replay it with a body of
their choosing for the rest of its fifteen minutes. Two things this build does
that their own SDK does not: the algorithm is **pinned** to HS256 rather than
read from the token — a verifier that trusts `alg` accepts `none` — and `iss` is
checked.

Tabby's header name is the merchant's to choose and Tabby fixes none, so this
system fixes it: `erp_payments::SECRET_HEADER`, in one constant, so the value a
tenant is told to register and the value this code looks for cannot drift.

`POST /v1/hooks/{provider}` now authenticates **per provider**: one
`erp_payments` knows is checked its way, and everything else falls back to the
signed contract, which is still right for a relay.

**The deeper consequence is the interesting one.** Because none of these bodies
is signed, a callback proves *nothing about the amount* — anybody who learns the
URL can post a plausible one. So `erp_payments::authenticate` returns the
**payment id and nothing else**, and its Moyasar implementation deserializes
only `{secret_token, data: {id}}`: there is nowhere for a body to put a number.
What happened is then asked of the gateway over an authenticated connection, and
the amount **and currency** are compared against what was expected before
anything is recorded. That is what Moyasar's own reference plugin does.

Two smaller rules fall out of the same argument, and both are tested:

- **The secret is checked before the body means anything**, and a malformed body
  answers "not authentic" rather than "unreadable". A caller who can tell those
  apart has an oracle. `Unreadable` is kept for a body that *did* authenticate
  and still made no sense, where the answer helps whoever is on call.
- **`callback_url` is not a channel.** The `id`, `status` and `message` query
  parameters a gateway appends are followed by the *customer's own browser* and
  are therefore theirs to edit. It is where somebody lands, not how anything is
  learned.

### 26 · WhatsApp cannot take a finished string, and that is a design decision

**The adapter is not built and should not be built as an adapter.** This is the
one place where the provider's model and this system's disagree, and writing the
client anyway would produce a file that passes its tests and fails every real
message.

Meta's rule, quoted: *"When the window closes, you can only send pre-approved
template messages."* A 24-hour **customer service window** opens when the user
messages or calls the business, and only then. Free-form `type: "text"` is
accepted inside it and refused outside it with error `131047` — *"More than 24
hours have passed since the recipient last replied to the sender number."*

Every WhatsApp message this system would send is a reminder or a notification
the business initiates. All of them are outside the window, always.

**The obvious escape hatch does not exist.** A "passthrough" template whose body
is a single variable — hand it our rendered string and let Meta send it — is
rejected at *template creation*: error `2388299`, "Leading or trailing
parameters not allowed", and `2388293`, "Parameters words ratio exceeds limit".
Templates are structurally slot-based with a static-text-to-variable floor, by
design. Verified against Meta's own error-code reference.

**So the work is not a transport, it is a template model.** What it needs:

- A **provider template** per message template and locale — name plus language
  code, matching one a human created and Meta approved in WhatsApp Manager.
  Approval is per name+language and takes hours to days.
- **Structured parameters on `Outbound`**, not a rendered body. Meta's current
  form takes named parameters (`parameter_name`), which is a near-exact fit for
  the `{placeholder}` names `Template::placeholders` already returns — so the
  render step would emit the values map instead of collapsing it to a string.
- **Approval state**, because sending against an unapproved or paused template
  is a permanent failure per message, and the tenant needs to be told which of
  their templates are live before the reminders start.

That is a phase, not an afternoon, and it puts an operational obligation on
every tenant — someone has to create and get each template approved.

**Answered 2026-09-03: left as it stands, for now.** The `whatsapp.send` effect
kind and the channel stay; there is no adapter, and `WHATSAPP_RELAY_URL` is the
way in for a deployment that has built the template mapping outside this system.
Nothing is decided against — the template model above is still the design when
somebody wants the channel for real. What this costs today is nothing, and what
it defers is a lot of per-tenant onboarding for a channel SMS already reaches.

Two smaller things settled while looking, worth keeping either way:

- **`HTTP 200` is not delivery.** The response can carry
  `messages[0].message_status: "accepted"`, and the message can still fail
  later, arriving as a `failed` status webhook with its own error code. Whichever
  design wins, the code→classification table has to be fed from both the POST
  response and the webhook — which is 11a's deferred delivery-receipt item, and
  12b is now the surface it can land on.
- **No idempotency key**, on any of the three providers. Not Meta, not Google,
  not Taqnyat — searched for specifically in each. A delivery that times out
  after the gateway accepted it is sent, retried and billed twice. The
  alternative is treating a timeout as permanent, which loses real messages to a
  slow network; losing a reminder is worse than sending it twice. Written down
  because it is a property of the world, not a bug to find later.

---

## Phase 1 — Foundations · 3–4 weeks

Nothing above this is worth building until the handle types and the test harness
exist, because everything after inherits them.

### 1a · Workspace skeleton
- [x] Cargo workspace, crate layout per architecture §6
- [x] `rust-toolchain.toml` pinning the toolchain
- [x] Shared lint configuration (`workspace.lints`), warnings denied
- [x] Retire the prototype `src/` tree (preserved at `f2e8acd`)

### 1b · `erp-types`
- [x] Newtype macro: `Display`, `FromStr`, serde, sqlx, validation
- [x] Identifiers: `TenantId`, `IdentityId`, `AggregateId`, `StreamId`
- [x] Distinct position types: `LogPosition` (global) vs `Sequence` (per-aggregate)
- [x] `CurrencyCode` with ISO-4217 minor-unit exponent
- [x] `Money` — runtime currency, no `Add`, `checked_add -> Result`
- [x] `NonEmpty<T>`
- [x] Unit tests including the "these two types cannot be confused" cases

### 1c · `erp-testkit`
- [x] Template-database-per-test fixture (`CREATE DATABASE … TEMPLATE`)
- [x] Measured: ≈280 ms to acquire, ≈140 ms to drop (local Postgres 18)
- [x] Parallel-safe (unique names, automatic teardown)
- [x] Fault-injection hooks at transaction boundaries *(landed in Phase 2, where the transactions exist)*

### 1d · Control plane
- [x] Schema: identities, memberships, tenants, entitlements
- [x] Append-only audit trail, enforced by trigger (D2).
      *Read since §62: a tenant's owner at `GET /v1/audit`, whatever the
      tenant's status; a person their own at `GET /v1/sessions/current/audit`;
      support and superadmins all of it at `GET /v1/platform/audit`*
- [x] Connection manager: LRU pools, `min = 0`, global budget as a semaphore
- [x] `TenantDb` with no public constructor; `ControlPlane::enter`
- [x] Tenant registry carries `(cluster, database)` from day one
- [x] Support access as a separate audited path — no `is_system` bypass.
      *Since §58 it needs the `EnterForSupport` power (support or superadmin)
      and a second factor; billing is refused*
- [x] Authenticators and sessions *(landed in 3a, with the rest of auth)*

### 1e · Build hygiene
- [x] `.sqlx/` offline query data; `SQLX_OFFLINE=true` in `.cargo/config.toml`
- [x] Verified: `cargo build` succeeds with no `DATABASE_URL` and no server
- [x] Verified: a query that drifts from the committed schema fails the build
- [x] `justfile` with `check`, `prepare`, `clean-databases`
- [x] `docs/DATABASE_SETUP.md`

### 1f · Exit criteria
- [x] Two-tenant isolation test: no code path reaches across
- [x] `cargo test --workspace` green (60 tests), `clippy -- -D warnings` clean,
      `cargo fmt --check` clean
- [x] **Soak test** (`erp-control/tests/soak.rs`, run with `--ignored`).
      Measured: open connections track *active tenants × per-tenant pool*, busy
      connections track the lane budget, neither tracks request count, entry
      cache hit rate 99.9%. 22,169 ops/s across 40 tenants with 256 workers.
- [x] Per-operation connection permits with per-lane bulkheads
- [x] Entry-path cache (four cold lookups, zero warm)
- [x] Read-replica seam (`TenantDb::read`, falls back to primary)

### 1g · Localization (D12)
- [x] `erp-i18n`: `Locale`, `MessageCode`, `MessageArg`, `Message`, `Localize`
- [x] CLDR plural rules — six categories for Arabic, two for English
- [x] Bidi isolation of Latin arguments inside RTL text
- [x] `Accept-Language` negotiation with quality values and regional subtags
- [x] English + Arabic for every control-plane message
- [x] Completeness test — a missing translation fails the build (verified by
      deleting one and watching three tests fail)
- [x] User-facing messages never leak tenant existence, internal detail, or
      cluster topology

### 1h · Multi-cluster (D13)
- [x] `cluster` table; credentials by env-var name, never stored
- [x] `cluster_load` view — counts from the tenant table, not a drifting counter
- [x] `ClusterStatus`: available / draining / full / offline
- [x] `PlacementPolicy`: balanced and packed, deterministic tie-breaking
- [x] Utilization in integer basis points, max of the two limits
- [x] `register_tenant` places automatically; `register_tenant_on` pins
- [x] Foreign key so a tenant cannot be placed on a nonexistent cluster, and a
      cluster holding tenants cannot be deleted
- [x] Typed `SlugTaken` — a normal signup outcome, not a database failure

---

## Phase 2 — Event core with reproducibility built in · 3–4 weeks

Replay guarantees are structural. They cannot be added later without rewriting
every projection, so they land before the first projection exists.

- [x] Gapless append — **a counter row, not an advisory lock** (see notes)
- [x] Concurrent-append test asserting contiguity and commit order
- [x] A test proving the naive implementation *does* lose events, so the one
      above cannot pass vacuously
- [x] Append-only enforced by trigger; `integrity()` for the continuous check
- [x] `Envelope`, `Metadata` with `config_version` (L5), optimistic concurrency
- [x] Reads: `read_since` (tailer), `read_stream`/`read_stream_since` (rebuild)
- [x] `Aggregate` / `DomainEvent` traits; `load`, `load_since`, `append_events`
- [x] `execute` — load, decide, append, retry on conflict, give up as `Contended`
- [x] Upcaster chain (`v1 → v2 → v3`), gap check, events-from-the-future refused
- [x] Golden files: every stored shape decodes on every build
- [x] **Snapshots — decided against, not pending.** An optimization for
      aggregates with long histories, and which of those exist is still not
      known. `load_since` (`crates/erp-eventlog/src/aggregate.rs:239`) is the
      seam and it is load-bearing rather than aspirational: `load` *is*
      `load_since(.., A::default(), Sequence::ZERO)`, so a snapshot supplies a
      non-default aggregate and a non-zero start and nothing else moves
- [x] Crash tests: a rolled-back append returns its positions; a crash between
      the append and the promise leaves neither
- [x] Projection groups, one Postgres schema each, `search_path` isolation (L3)
- [x] `ProjectionCtx` — no clock, no RNG, no pool; `derive_id` instead (L2)
- [x] Checkpoint-in-transaction; the row lock doubles as the lease (L4)
- [x] `run_once` / `run_to_head`; `Progress::Busy` rather than blocking
- [x] `replay_shadow` + table differ (`EXCEPT ALL` both ways)
- [x] The differ is itself tested against a clock-reading projection, so a clean
      diff means something
- [x] Outbox schema; `Effect` as a value; `enqueue` in the command's transaction
- [x] `Decision` (events + effects) and `Committed` (position, version, effects)
- [x] Dispatcher: claim under `SKIP LOCKED`, deliver with no connection held,
      settle separately; exponential backoff, dead letters, health counters
- [x] Effects whose kind has no registered handler are **not claimed**, so a
      staggered deploy cannot dead-letter a tenant's work
- [x] Per-visit tenant leases (`claim_tenants`), `next_visit_at` scheduling with
      per-tenant jitter, `request_visit` as the seam for the push path
- [x] `enter_for_maintenance` — background access with no identity, own lane
- [x] `erp-worker`: `Job` trait, `ProjectionJob`, `OutboxJob`, `bin/worker`
- [x] `CancellationToken` + `TaskTracker` drain; leases released on the way out
- [x] SIGTERM-mid-batch test, verified by shadow differ rather than by assertion
- [x] Fault injection at transaction boundaries (carried from 1c) —
      `pg_terminate_backend`, not a simulated failure
- [x] Shadow replay against the demo tenant, on every run *(Phase 4b — all four
      groups, and the list is checked against `erp_api::modules()` rather than
      trusted, so a module added without a line there fails the test)*

**Exit:** a projection can be written, replayed into a shadow schema, and proven
identical by a differ rather than by assertion. **Met.** 215 tests.

---

## Phase 3 — The request path · 2–3 weeks

**Resequenced.** The original Phase 3 was "build what modules are built on"
before any module existed: a `Module` trait with one implementation, capability
permits with nothing to permit, a configuration system with nothing configurable.
Every one of those is an abstraction invented from a guess about its consumer.

So Phase 3 is now the **spine** — the shortest path from an HTTP request to a
tenant's data — and the kernel abstractions are deferred to 3c, where two real
modules can say what shape they need.

### 3a · Authentication and sessions *(carried from 1d)*
- [x] `authenticator` table; Argon2id, PHC strings so cost is raisable without a
      migration
- [x] Opaque 256-bit session tokens; only the SHA-256 is stored
- [x] Constant-time-ish login: an unknown handle costs the same as a wrong
      password, so the API is not an account-enumeration oracle
- [x] `SessionToken`'s `Debug` is redacted — a token in a log line is a working
      credential
- [x] `log_out`, `log_out_everywhere`, `sweep_sessions`
- [x] **API keys — built in Phase 12c**, and the prediction beside this box was
      half wrong. It said "more rows in `authenticator`, not more tables"; a key
      *is* a row in `authenticator` (`crates/erp-control/src/keys.rs:260`) **and**
      needed its own table for the public half, the scopes and the rotation
      (`migrations/control/0012_api_keys.sql`). Left visible rather than
      rewritten: the guess was reasonable and the correction is the useful part.
- [x] **MFA — built 2026-09-09 (§51).** TOTP, as an authenticator app computes
      it, with recovery codes. **The prediction held this time**: four kinds in
      `authenticator` and no new table — `password`, `totp_pending`, `totp`,
      `recovery`. `migrations/control/0015_second_factor.sql` widens one CHECK
      and adds nothing else
- [ ] OIDC. *(Unscheduled. **The prediction is the shaky part, not the
      status**: the login is more rows in `authenticator`, but provider config —
      issuer, client id and secret, JWKS — has nowhere to live, and
      `authenticator.secret` is `NOT NULL` for a login that has no secret. This
      is the case likeliest to break "more rows, not more tables" the way API
      keys already did.)*

### 3b · The HTTP surface
- [x] `erp-api` on axum; `bin/api` with body limit, timeout, graceful shutdown
- [x] problem+json (RFC 9457) plus `code` and `args` — a client branches on the
      code, never on the prose
- [x] `Accept-Language` honoured on every response including failures
- [x] Composite catalog across crates, with a duplicate-code test — layered
      since modules ship their own routes: `erp_web::CATALOG` is what any route
      can answer with, a module adds its own and its dependencies', and
      `erp_api::CATALOG` is the union `docs/ERRORS.md` comes from
- [x] `Tenant` extractor: the only route to a `TenantDb`, so "did we check the
      membership?" is answered by the signature
- [x] Status mapping in one place; a tenant that exists but is not yours is
      byte-identical to one that does not exist
- [x] Nine tests through the real router, including the two enumeration oracles

### 3e · Provisioning
- [x] `ControlPlane::provision` — registers, creates and migrates the database,
      installs modules, grants the owner, activates. Idempotent throughout.
- [x] Compensation: a failure drops the database and the row, so the name is
      free again — proved by signing the same name up successfully after
- [x] `ControlPlane::sign_up` — the whole thing, plus the account and a session
- [x] `ModuleSetup`: a module *describes* itself — install SQL, seed SQL,
      projection groups, event versions, dependencies — rather than the control
      plane knowing what a ledger is
- [x] **`POST /v1/signups`** — a request, a confirmation email, and then
      `POST /v1/signups/{token}`, which is where the caller gets a working
      system they are already logged into with the ledger installed and usable.
      Two calls since item 5; nothing is built by the first
- [x] A sweeper for tenants stuck in `provisioning`. **Built 2026-09-11, §60**:
      `reap_stuck_provisioning` in the reaper, through an `abandon` made safe
      against a stale read and a database with data in it. The commonest cause
      was not a crash but the API's 30-second timeout or a closed tab dropping
      the build; a confirmation now builds on a task the request only waits
      for. *(**Audited 2026-09-09:
      the premise is true and the reason is wrong.** Signup is synchronous end
      to end — `crates/erp-api/src/signup.rs` → `confirm_signup` → `provision`,
      nothing spawned or enqueued. But compensation is **best-effort**:
      `crates/erp-control/src/provision.rs:255` logs and swallows a failed
      `abandon`, and a crash between `register_tenant` and `activate_tenant`
      compensates nothing at all. The row sits at `provisioning` and its slug is
      held for ever, so `request_signup` answers `SlugTaken` to a customer who
      never got an account. Needed for that, not for the async reason.)*

### 3c · Kernel services

Built only where the ledger produced a second consumer.

- [x] `Job::module()` — a tenant that declined a module is not worked on its
      behalf, which is what "modular" has to mean to be worth anything
- [x] `Invariant` trait + `HealthJob` — architecture §7's per-tenant checks
      actually run, on an interval. Four kernel invariants; the ledger
      contributes the trial balance through the composition root
- [x] `bin/worker` composes the kernel and the modules; neither crate depends on
      the other
- [~] `Module` trait and registry *(the **registry** half is done and was the
      load-bearing one: `modules::REGISTERED` is one entry per module carrying
      both its `ModuleSetup` and its router, so neither can be added without the
      other. The **router** half is done too — `erp-web` sits below the modules,
      so a module ships `http::routes()` itself.
      What is left is the reason a trait is still not worth writing: a module's
      **worker jobs** are registered in `bin/worker.rs` and cannot move, because
      a module must not depend on `erp-worker` and the kernel must not know what
      a ZATCA document is. A trait with two of its three methods implemented
      somewhere else describes nothing)*
- [x] `Idempotency-Key` — **decided against**, see architecture L8. Mutations take
      client-chosen ids and the log's uniqueness constraint refuses the repeat, so
      a header plus a key/response store would rebuild a property the design
      already has. `erp-api/tests/idempotence.rs` enforces what makes it true.
- [x] **`ETag`/`If-Match` — built**, and the condition this box was waiting on
      was met by settings. `crates/erp-web/src/extract.rs:992` extracts the
      version a write is conditional on, `crates/erp-web/src/messages.rs:673`
      refuses a malformed one in both languages, and every settings `PUT` takes
      it (`modules/messaging/src/http.rs:446`). The box said "no update-in-place
      endpoint yet"; settings became one and nobody came back to tick this.
      **If it meant conditional requests on domain resources too**, that is a
      different and larger thing — logged in `docs/AMBIGUITIES.md` §3 rather
      than assumed either way
- [x] `?consistent_after=<position>` — read your own write, with the write
      nudging the worker so the wait is a claim cycle rather than the idle backoff
- [x] Cursors — keyset paging on the columns each list is ordered by, an
      opaque cursor, and `next` absent meaning **the list ended**. Every list
      used to stop at 200 and say nothing. A cursor this build cannot read is
      refused rather than ignored
- [x] Roles and capabilities: `Role::allows` is the one place authorization is
      decided, and `Allowed<C>` in a handler's signature *is* the check
- [x] Authorization matrix as a test, written out rather than derived — over
      HTTP, every role against every endpoint
- [x] A 403 names the capability, in the caller's language
- [x] Member management: list, add, change role, remove — `ManageTenant`, and
      the last owner cannot remove or demote themselves
- [x] Invitations — a link the inviter passes on, single-use, expiring,
      revocable. The recipient sets their own password and the owner never sees
      it. No email: sending one is an outbox effect and belongs with the first
      real handler
- [x] **Tenant-local, fact-derived authorization — built in §9c, and
      deliberately *not* a projection.** `hr` claims are write-side state in the
      tenant migration chain (`migrations/tenant/0008_org_claims.sql:22`), under
      a heading that answers this box directly — *"Why it is here and not in a
      projection schema … a read model may be a second behind; an authorization
      answer may not."* Platform roles stay in the control plane and `Allowed<C>`
      reads only those. **But nothing checks a claim — see §52**, which is a
      defect rather than a deferral
- [x] Configuration — the **store**, not the system. A versioned key-value table
      in the tenant database, a typed surface on top, and posting accounts as
      its first and only consumer. Declarations, layers and resolution rules are
      Phase 6's, and they are what §6 describes; this is what sits underneath
      them
- [x] Numbering (gapless per-tenant document sequences) — `erp_eventlog::numbering`
      and `migrations/tenant/0005_numbering.sql`. A counter row read `FOR UPDATE`
      and advanced in the same transaction as the document, so a refusal, a
      retry, or a crash releases the number rather than burning it. Saudi VAT
      Implementing Regulations Article 53 requires the sequence to have no holes
      in it, which a Postgres `SEQUENCE` cannot give: `nextval` survives a
      rollback by design
- [x] `docs/ERRORS.md` — every error code in both languages, generated from the
      catalog the API renders from, with a CI drift check (`just errors`)
- [x] `docs/openapi.json` — every route, generated from the router that serves
      them, with a CI drift check (`just openapi`) and served at
      `GET /v1/openapi.json`. `utoipa-axum` registers the axum route *from* the
      `#[utoipa::path]` attribute, so a served route cannot be undocumented; the
      hand-written half (which status carries what) is checked by validating
      every response in `tests/http.rs` against the published schema
- [x] Authorization matrix tests — every role against **every** role-scoped
      endpoint, with the endpoint list taken from `erp_api::openapi()` so a route
      added tomorrow appears whether or not anybody remembers, and an operation
      the table does not name fails the test rather than defaulting to untested

**Exit for 3a+3b:** a person signs in and reads their own tenant over HTTP, in
Arabic, and cannot read anybody else's. **Met.** 230 tests.

## Phase 3d — `modules/ledger` · 2–3 weeks

The first real module, and the proof that the module seam works. Built as a
module from the start rather than extracted from the kernel later.

**This is what 3c waits on.** Writing the ledger against the bare spine shows
which kernel services are genuinely shared and which were invented — and the
first `Idempotency-Key` and `ETag` have a real mutation to attach to.

- [x] Accounts (open, rename, close) and journal entries
- [x] `BalancedLines` as a proof-carrying event payload — revalidates on
      `Deserialize`, so a stored unbalanced entry will not decode
- [x] Signed amounts rather than a debit/credit pair: "debits equal credits"
      becomes "sums to zero", one check on one number
- [x] Trial balance and account balances as **views**, not maintained tables
- [x] `imbalances()` — the health check this module contributes
- [x] Property tests on `BalancedLines`, plus a generated command sequence whose
      stored postings must still sum to zero
- [x] Shadow replay proves the ledger rebuilds identically
- [x] HTTP routes; the same `Tenant` extractor, so isolation is inherited
- [x] Reversals — an entry posted in error is undone by posting its opposite,
      both in one transaction, refused if already undone
- [x] Credit notes — an invoice issued in error is cancelled by crediting it,
      which reverses its journal entry. Whole-invoice, and refused while
      payments stand against it — **and mutually exclusive with the partial
      credit notes below**: reversing the whole issue entry on an invoice
      already partly credited would take the credited part back twice
- [x] Fiscal periods — `ledger::period`, one watermark, checked in
      `post_entry_in` where every posting in the system arrives. An entry dated
      into a closed period is refused whether it is a hand-written journal entry,
      a reversal, an invoice's tax point, a payment, or a credit note
- [~] **Partial credit notes — built.** A credit note against part of an
      invoice, as a document with lines, bands and a tax point of its own:
      `POST /v1/sales/invoices/{invoice}/credit-notes`. It posts its own entry
      rather than reversing the invoice's, because there is no such thing as
      reversing part of a journal entry — and the rates come off the invoice's
      own bands, so a 2019 invoice is credited at 5% for ever. Capped **per
      band** and cumulatively, which is the check that protects the tax. See §32.

      Drafts and multi-currency entries with FX are still open.
- [ ] An entry-level read model *(a `proj_ledger.entry` table would show which
      entry reversed which, and let entries be listed at all — but adding a
      table to a module's install script needs the fleet-wide module refresh
      that nothing needs yet, and nothing displays the link today)*
      *(each needs someone to want it before its shape is decided)*
- [x] Chart-of-accounts templates — `services` and `retail`, bilingual, with
      Saudi VAT and Zakat accounts in both
- [x] `GET /v1/ledger/charts` (unauthenticated — a signup form has to show the
      choices) and `POST /v1/ledger/chart`
- [x] Installing is idempotent, so a half-finished install is fixed by retrying,
      and `retail` on top of `services` opens only the difference

**Exit:** a correct ledger behind an API a third party could integrate against —
and a module a tenant can decline. **Met.** 256 tests.

---

## Phase 4 — Modules, blueprints, provisioning, demo · 4–5 weeks

### 4a · `modules/sales`

The second module: invoicing with Saudi VAT, posting to the ledger.

- [x] `Invoice` aggregate — issue and record payment, both idempotent by
      client-chosen id
- [x] VAT as `standard` / `zero` / `exempt`, with the **rate resolved at issue
      and stored on the line** so a future rate change cannot restate a filed
      return
- [x] Tax computed per band, not per line — which is what the authority
      computes, and provably different from summing line-level rounding
- [x] Rounding half **away from zero**, so an invoice and its exact credit
      reverse without leaving a halala in VAT payable
- [x] The customer is a snapshot on the document, not a foreign key
- [x] `sales → ledger` in **one transaction** (`ledger::post_entry_in`), rather
      than by event through the outbox — see the running note and the amendment
      in architecture §1.11
- [x] `sales::requires()` — signup refuses sales without the ledger
- [x] Module gating on routes: a tenant that did not enable a module gets a 404,
      not a 500 from a missing table
- [x] A VAT return — output tax by rate for a half-open period, per currency,
      with credited invoices excluded
- [x] ZATCA clearance and reporting — the whole chain: onboarding by OTP, the
      key and CSR, the XAdES signature, the QR, the invoice hash chain, the
      transport, and the worker sweeps that sign and submit. Nine documents
      accepted by ZATCA's sandbox with zero warnings. **Not** an outbox handler
      in the end — it is two worker jobs, because a submission has to read a
      sealed private key and the outbox carries values, not secrets *(4e)*
- [x] The input-tax side — `modules/purchases`, and the whole return composed in
      the API from both modules' own reads, because their projection groups never
      see each other (L3) and nothing below the composition root could produce
      the figure
- [x] Credit notes and cancellation — a `POST`, not a `DELETE`: the invoice
      stays, its journal entry is reversed, and the books show both
- [x] Statutory gapless numbering, on its own series per document type
- [x] **Receivables** — `GET /v1/sales/receivables`, aged by due date and falling
      back to the issue date, grouped by `(customer, currency)`, biggest debtor
      first. The system could issue and settle invoices but not answer *what does
      this customer owe*, which is the question an AR clerk asks every morning.
      Needed no schema change: `invoice_status` already carried `outstanding`,
      and its own comment already anticipated the report.
- [x] Customers as records — `modules/crm` (§16, §41)
- [x] Partial credit notes — `sales::credit_part_in`, and since §44 a partial
      refund issues one (2026-09-07)
- [x] **Quantities and unit prices — decided, and waiting on a customer rather
      than on us.** **The trigger this box named has already passed** — `modules/tax_sa/src/zatca/ubl.rs:659` emits
      `cbc:InvoicedQuantity` (hardcoded `1`, with the reason written in) and
      `:727` emits `cac:Price`, both sandbox-accepted. So they do not "land with
      ZATCA's line-level fields"; those landed. The upcaster now waits on a
      customer who sells by the unit. *Deliberately not stored:
      `modules/sales/src/invoice.rs` keeps a line's net only, because a client
      that shows "3 × 250.00" already computed the 750.00 it sends. They land
      with ZATCA's line-level fields, as an upcaster.*

### 4b · The demo tenant

- [x] `erp-demo`: a tenant with every module enabled, filled **through the
      public HTTP API** — so a demo built out of internal calls cannot be
      perfect while the API a customer would use is broken
- [x] Deterministic: fixed dates, amounts and identifiers, so the numbers can be
      screenshotted and a CI failure is never "did the data change?"
- [x] Something in every state a screen has to render: settled, part-paid and
      untouched invoices; all three VAT treatments; expenses as well as sales
- [x] **Part of `cargo test --workspace`**, which builds the whole demo and
      asserts every module answers, every group replays identically, and every
      invariant is clean. Called "a required CI check" in an earlier draft of
      this document: ~~**there is no CI**, here or anywhere in the repo~~ — **false since 2026-08-27**, when `.github/workflows/check.yml` landed: `just check` on every push to main and every PR, against pinned Postgres 18.3, Redis 8 and MinIO, plus an `offline-data` job. Left visible because a *ticked* box nobody re-reads is the worst place for a false claim, and **at least two open boxes are still written against the no-CI premise**. It was a
      required *test*, and nothing runs it but a person
- [x] **Shadow replay against the demo** *(carried from Phase 2 — it needed the
      demo tenant, and now has one. All four groups, with the coverage itself
      asserted; each group names a table the demo must have filled, because
      `EXCEPT ALL` between two empty tables is clean)*
- [x] `erp_api::modules()` — the module set in one place, so "every module
      enabled" is true by construction rather than by a second list agreeing
- [x] `bin/demo` + `just demo <password>`, so the demo is a thing a person signs
      into rather than a thing a test builds
- [x] `erp_demo::bootstrap` — migrates and registers the cluster, because a
      demo is usually the first thing pointed at an empty database
- [x] Demo tenant TTL and reaper — `set_demo_expiry`, `expired_demos`,
      `reap_demo`, and `bin/reaper` (`just reap`). Demos expire by default;
      `DEMO_TTL_DAYS=0` opts out

### 4c · Entitlements a tenant can change

- [x] `POST /v1/modules` and `DELETE /v1/modules/{module}` — a tenant buys a
      module on a Tuesday and it works immediately. No `{slug}`: the tenant is
      the subdomain
- [x] `ControlPlane::install_module` — read models **and** entitlement, because
      either alone is a tenant that 500s
- [x] `ModuleSetup::requires` and `requires_any` — dependencies declared once
      and read by all three places that ask: signing up, enabling later, refusing
      to disable. `requires` is an AND list; `requires_any` is a group satisfied
      by any member, which is what `tax_sa` needs — a VAT return wants a source
      for one side or the other and does not care which
- [x] Disabling deletes nothing. The entitlement is marked off; the events and
      read models stay, so a tenant who downgrades and returns finds their data
- [x] `GET /v1/catalogue` — unauthenticated and on the apex, because a pricing
      page needs it before anyone has an account or a subdomain. Carries both
      kinds of dependency, so a picker can grey out impossible combinations.
      (`GET /v1/modules` is the tenant's own list; the two collided when the
      tenant moved to the subdomain and the router refused to start)
- [x] The test fixture installs modules through `install_module` rather than by
      hand, so it can no longer be right while the product is wrong
- [ ] `ModuleEnabled<M>` capability tokens. **By this box's own threshold, the
      moment has arrived**: `require_module` has **206 call sites**, three routes
      assert it by hand in `crates/erp-api/tests/http.rs`, and nothing catches a
      handler that forgot it — a route serving a module the tenant is not paying
      for. **The cheap half of the fix is a source scan, not a token**:
      `crates/erp-api/tests/creates.rs:84` already has `handlers()`, which splits
      a module's `http.rs` into handler bodies and runs two other laws off it.
      *(`require_module` is a runtime check
      at the top of each handler; the token makes a disabled module's handler
      unconstructable. Worth it when a module has enough routes that remembering
      the call is the weak link)*

### 4d · The rest

- [x] A second business module *(shipped as 4a — and it changed how
      cross-module integration works, which is the point of building one)*
- [x] **Blueprints: browse, parameterize, preview and install ship three
      times** — `ledger::CHARTS`, `booking::TRADES` and, since 2026-09-10,
      `booking::PACKS`. Preview shipped the same day (the box below).

      **Edit before install stays declined**, and the reason at
      `modules/ledger/src/charts.rs:24` is unchanged: every account is
      renameable and closeable afterwards, so editing first is worth building
      when somebody asks to change a thing they cannot already change after
      installing it. Packs decline it on the same ground and add one of their
      own — every band a pack writes is an ordinary form-authored band, so the
      form *is* the edit surface
- [x] **Preview executes in a rolled-back transaction and reports resulting
      state — built 2026-09-10.** `POST /v1/ledger/chart/preview`, taking `Read`
      because it writes nothing.

      **The seam, as scoped.** `ledger::open_account_in` and
      `install_chart_in` take a `&mut PgConnection` instead of owning a
      transaction, the pattern `post_entry_in` established. No retry inside
      them: the caller's transaction owns that decision, and retrying inside
      somebody else's would re-run their earlier steps.

      **Preview and install are the same function.** `preview_chart` rolls the
      transaction back and `install_chart` commits it; nothing else differs.
      That is the property the box asked for — a *predicted* preview is a second
      implementation of the install's rules, and two implementations drift.
      `a_preview_says_exactly_what_the_install_does` is the test, and it
      falsifies.

      **The rollback happens on the error path too.** A preview that failed
      halfway and left accounts behind would be the worst version of this.

      `Installed` now names the accounts rather than counting them, because
      "twenty-six would be opened" is not an answer anybody can check — and the
      preview and the install answer in the same shape, from the same run.
- [x] **Chart-of-accounts templates — three ship, and two of the five this box
      named should not exist.** `services` and `retail` were already built when
      this box was written; `real_estate` was added 2026-09-09 for Phase 20 and
      is where a security deposit is a **liability** rather than revenue.

      **"SOCPA-aligned" is not a real artefact** — looked up rather than
      assumed. SOCPA endorses IFRS and IFRS for SMEs, which are *accounting
      standards*, and neither prescribes a chart of accounts; Saudi Arabia
      mandates none for private companies. Shipping an invented chart under that
      name would imply an endorsement that does not exist, to the one audience
      qualified to notice.

      **"Empty" was already decided against, in the code**: *"not installing one
      is already that, and a template that creates nothing is a menu item that
      does nothing."*

      **"Generic IFRS"** is what `services` is — a chart with no industry
      accounts in it. A second one under a standards-body name would be the same
      accounts and a stronger claim
- [ ] **A food & beverage chart and a healthcare chart.** *Decided 2026-09-14:*
      cafés and clinics are sold from launch, so these are the next industries
      rather than a guess. Tracked under [Road to selling](#road-to-selling),
      Priority 2
- [ ] Self-service signup as a durable workflow. *(**Already two-phase and
      compensating** — `pending_signup` plus an outbox email in one transaction,
      `confirm_signup` unclaims on `Err`, `provision` drops the database and
      frees the slug, and `CREATE DATABASE` treats 42P04 as idempotent. What is
      missing is surviving a **crash** rather than an `Err`, not holding a
      request open across `CREATE DATABASE` and a whole migration chain, and
      resuming rather than tearing down.)*
- [ ] Template databases per module combination. *(**Half the premise is
      stale.** The harness exists — `crates/erp-testkit/src/template.rs` clones
      per test from a template keyed by migration-set fingerprint — but has no
      key for a module *combination*: only `control` and `tenant` exist, and
      module schemas are installed per test on top of the clone. And "built in
      CI" is wrong twice: CI exists now, and templates are built lazily by the
      first test that asks.)*
- [x] **Demo blueprint with every module enabled, as a required CI check**
      *(4b)*
- [x] Fleet migrator — `survey_fleet` looks, `migrate_fleet` applies, `bin/migrator`
      (`just migrate-fleet [check]`) is the deploy step. Exits non-zero when the
      fleet is not uniform, so `check` gates a deploy
- [x] Per-tenant health checks including the trial-balance invariant *(shipped
      with `HealthJob` in Phase 3c; sales added the overpaid-invoice check)*
- [x] Module schema refresh across the fleet — `refresh_module`,
      `refresh_module_fleet`, `just migrate-fleet refresh <module>`. Drop the
      schema, install it again, rewind the checkpoint, let the worker replay

### 4e · Saudi e-invoicing, and the seams it broke

Not in the original plan at all — it was one line in 4a. It is a phase.

- [x] `modules/tax_sa` — the VAT return netting output against input, filed
      returns, and the first module standing on two others
- [x] The invoice hash chain (PIH/ICV), the QR as TLV, the canonical UBL
- [x] Onboarding: key pair, CSR, OTP, compliance checks, production certificate.
      The route spends the OTP and the worker finishes (§45); the industry
      lives on the registration
- [x] The XAdES signature, and the transport (`reqwest` over the OpenSSL stack
      sqlx already links)
- [x] Sealed module secrets — `SEALING_KEY`, and anything that would store a
      private key **refuses** without one rather than storing it in the clear.
      *Rotatable only since §64: until then one key, and changing it lost
      every secret; now a list, and `migrator reseal`*
- [x] Worker sweeps: `tax_sa.sign` and `tax_sa.submit`, registered only when the
      deployment has a sealing key
- [x] `CertificateExpiry` invariant — sixty days' warning, because renewal needs
      a human reading an OTP off the Fatoora portal and nothing here can do it
- [x] Verified against ZATCA's **sandbox** with a real certificate: nine
      documents, zero warnings
- [ ] Verified against **simulation**, and then production *(needs a real
      taxpayer's OTP — see [What needs work now](#what-needs-work-now))*
- [x] **Exemption reasons — built 2026-09-09, and they closed a defect rather
      than a gap.** A line that carries no tax now names the ZATCA article it is
      untaxed under, stamped at issue time from the tenant's configured
      `ledger::Rates` (L5) and carried on the event, so a replay reproduces what
      was declared. Before this, the reason was derived from the *category*:
      every exempt line in the system was declared to ZATCA as `VATEX-SA-29`,
      **financial services** — right for a bank, and a false statement to a tax
      authority for a landlord. See §50
- [x] **Per-band, not per-line**, and that is the standard's shape rather than a
      simplification: `cbc:TaxExemptionReasonCode` sits in `cac:TaxCategory`
      inside `cac:TaxSubtotal`, which is one per (category, rate). Lines in a
      band cannot disagree — they took the code from the same configuration in
      the same transaction
- [ ] Per-till device certificates

### 4f · Modules that are actually modules

- [x] `erp-web` — extractors, problem+json, paging and the request-level messages
      moved *below* the modules, so a module can name what its routes are built
      from without depending on `erp-api`
- [x] Every module ships `http::routes()`; `erp-api` mounts them and writes none
- [x] `ModuleSetup::seeding` — a module's data is a step of its own, not a rider
      on its DDL
- [x] PDPL erasure — `audit_entry`'s trigger permits exactly one shape of update,
      one that nulls an actor and changes nothing else. An identity that had ever
      acted could not be deleted at all, and "our schema will not let us" is not
      a lawful ground for refusing
- [x] **An HTTP endpoint for erasure — deliberately absent.** **Who may erase
      whom** is a policy question, and answering it while fixing a schema bug
      would answer it badly. The erasure itself works
      (`erp-control/src/lib.rs:1301`); what is missing is the decision, and it
      is three cases: self, an owner erasing a colleague, and platform staff
      erasing a customer
- [x] `docs/RUNNING.md` — bringing the API and workers up by hand

**Exit:** someone signs up online, picks a chart of accounts, and gets a working
system.

---

## Phase 5 — Granular permissions · 3–4 weeks

**Resequenced.** The rule engine was to be built first and authorization moved
onto it. But it had one real consumer and no concrete rules to describe it from
— pricing did not exist **— false since `modules/booking/src/pricing.rs` shipped**, see §53 — so building `Facts` and `DynCondition` now meant
inventing which facts exist. Instead: the smallest real granularity gap first,
and let two working cases describe the engine.

### 5a · A different role in a different module

- [x] `Access` — a tenant-wide role plus per-module exceptions, so "Sara does
      the invoicing, Khalid does the books" is expressible
- [x] The module comes from the **request path**, so a module route added
      tomorrow is scoped without anybody remembering to scope it
- [x] The tenant's own surface — members, invitations, entitlements — is nobody's
      module and uses the tenant-wide role
- [x] Clearing an exception restores the tenant-wide role, which is a different
      thing from setting `viewer`
- [x] Removing somebody clears their exceptions with their membership
- [x] Invalidated on the spot, like every other authorization change

### 5b · The engine, once there is something to describe it

- [x] **`erp-rules`: `Facts`, `DynCondition`, `FactRegistry`, `Rule<E>` — built
      2026-09-10.** Spec at
      `docs/superpowers/specs/2026-09-10-rules-engine-design.md`.

      **`Rule<E>` is smaller than §5.6 on purpose**, and each omission is an
      argument: `priority` duplicates list order, `effective` duplicates
      `Availability`'s `from`/`until`, and `id`/`version` are per-rule editing
      the config store already covers at set level. `origin` did arrive — as a
      wrapper rather than a field, for the reason below. Each arrives with the
      consumer that needs it.

      **One variant of `DynCondition` is not uniform**, deliberately.
      `Covers { window: Availability }` carries the working span evaluator
      whole — day-walking, per-end DST offsets, the 16:59:30-is-inside-17:00
      boundary, and a bit-packed representation where zero means *every*. The
      uniform alternative hides the same code behind an operator and buys only
      symmetry.

      **`explain` is in the first cut, not deferred.** A rules engine whose
      refusals cannot be interrogated makes the support tickets it was built to
      remove. `evaluate` takes its answer from `explain`, so they cannot
      disagree about which rule won — the property `preview_chart` and
      `install_chart` also have
- [x] **Pricing is on it.** `Tariff::band_for` takes its answer from the
      engine, and `Tariff::explain` says which bands were tried and which won.

      **A `Band` is still `{ name, when, uplift }`.** What a tenant writes did
      not change when the engine took over evaluating it, and did not change
      again when templates arrived — which is what "all producing the same
      artifact" means. What *did* change is the envelope: the stored shape is
      `TariffAsWritten { bands: Vec<Authored<Band>> }`, and `Tariff` is now what
      resolving it produces. Everything downstream works on the resolved form,
      so how a band was authored is a question only the settings screen asks.
      `the_engine_picks_the_band_the_old_matcher_would_have` is the guard that
      protects a live tenant from being silently repriced
- [x] **Authorization on it — built 2026-09-10.** `erp_tenant::Limits` is
      `Rules<Verdict>` over three facts — `amount`, `branch`, `capability` —
      stored as tenant configuration under `tenant.permission_limits`, empty by
      default so roles decide alone until somebody says otherwise.
      **Nothing wrote it until §63 (2026-09-11)**, which added the owner's
      `GET`/`PUT /v1/tenant/permission-limits`, a fourth fact, `role`, and made
      `ManageTenant` impossible to narrow.

      **A limit narrows and never widens**, and it is a shape rather than a
      convention: `Limits::narrow(allowed, facts)` takes the role's own answer
      as its input and returns early when it is already `false`. No arrangement
      of rules can grant a capability a role withholds, so the worst a
      misconfigured limit does is refuse work — visible, and fixed by an owner.
      `a_limit_never_widens_what_a_role_allows` is the guard.

      **`Verdict::Allow` exists so an exception can sit above a refusal**, first
      match winning — "Olaya is exempt" above "everything else over ten
      thousand". It only ever restores what the role already permitted.

      **The example `roles.rs` named now works** — *false until §63*: *"a
      bookkeeper may post entries under ten thousand riyals"* could not name the
      bookkeeper, because no fact said who was asking, so the rule refused the
      owner too; and no route could store it. With §63's `role` fact it is one
      rule, and it leaves reading alone because the capability is a fact.
      *Until §63's review it could also be walked around*: an entry in dollars
      was never "at least ten thousand riyals", and a reversal supplied no
      amount. Both are closed; it still reaches only the ledger's own entries,
      not an invoice or a till sale — see §63's Left open.

      **`erp-rules` grew a `spans` feature for this.** A permission is not about
      a window of time, so the authorization kernel does not compile the booking
      calendar to answer one; `booking` asks for the feature and `erp-tenant`
      does not
- [x] **Per-request fact assembly, and the coverage assertion — built
      2026-09-10.**

      **In two places, because an amount is not knowable at the edge.**
      `Allowed<C>` decides before the request body is read, so it supplies what
      the edge knows — `capability`, and `branch` from `X-Branch`, which had to
      move above the check to be a fact at all. A handler that has parsed a body
      supplies the rest through `Allowed::still_permits`, and
      `ledger::post_entry` is the first: it narrows on the entry's total debits,
      which is exactly *"a bookkeeper may post entries under ten thousand
      riyals"* — the example `roles.rs` named and could not express.

      **A refusal costs nothing to serve.** `TenantDb::permits` returns before
      reading anything when the role already said no, which is possible only
      because a limit can never widen.

      **Unusable limits are refused, not ignored.** A tenant who configured
      limits and stored something this build cannot parse gets a `503`, not the
      unlimited answer.

      **The coverage assertion is `crates/erp-web/tests/facts.rs`**, and it is
      the phase's own words — *"an unsatisfiable condition fails the build, not
      a user's request"*. A fact declared and never assembled lets a tenant
      author a rule that validates, stores, reads back, and **is never once
      true**: the worst kind of broken, because every part of it looks like it
      works. The reverse is checked too, so a fact assembled and never declared
      is work nobody asked for.

      It scans the **whole workspace**, not the extractor — which its first run
      taught me, by correctly failing on `amount` while I was scanning one of
      the two places facts come from
- [x] **Authoring levels 0–3 with `origin` round-tripping — built
      2026-09-10.** `erp_rules::Authored<A>` is the four levels, and `booking`
      is the consumer: `GET /v1/booking/tariff/templates` draws the form,
      `PUT /v1/booking/tariff` accepts any level, `GET` reads it back.

      **A preset is a form with no blanks**, so levels 0 and 1 are one
      mechanism rather than two. `Template::fields` being empty is the whole
      difference, and the same code renders, checks and builds both.

      **The answers are the truth.** A templated rule stores its answers and
      *nothing else*; the artifact is rebuilt from them on every read. There is
      no second copy to fall behind, which makes drift structurally impossible
      rather than something a future reader has to remember to avoid.
      `a_form_stores_its_answers_and_not_what_they_build` asserts the absence —
      it fails the moment a built artifact appears in the stored JSON.

      **So editing a templated rule's artifact is not an edit**: it replaces the
      rule with a `Raw` one and drops the form. That is the honest outcome — a
      rule somebody hand-edited is no longer that form's rule, and rendering it
      as one is how a settings screen starts lying.

      **Generic over the artifact, not the consequence.** `A` is
      `booking::Band`, not `Rule<i32>`: a module that already has a
      configuration shape keeps it, and `authoring.rs` never learns what a
      condition is. `a_band_filled_into_a_form_prices_exactly_as_one_written_out`
      books the same hour of the same Thursday twice, one band written each
      way, and compares the price — which is "all producing the same artifact"
      checked against a real booking.

      **`booking` ships two forms and no presets.** How much dearer is the one
      number a business must choose for itself; a preset that picked 25% would
      be inventing their pricing. Presets earn their place where the *shape* is
      the answer.

      **A withdrawn template refuses rather than vanishing** (L6). A tariff
      silently missing its peak band is a month of underbilling nobody notices,
      so `TariffAsWritten::resolve` returns `ConfigError::Invalid` naming the
      key, and the booking stops.

      **The stored shape moved and nothing carries an old one across.** A
      `booking.tariff` written before this reads back as a `500`, not a
      degraded tariff. That is deliberate for a pre-launch system with a
      disposable database; the alternative — deserialising a bare band as
      `Raw` — costs a second declaration of the same enum, which is the drift
      this codebase argues against everywhere else. If a tenant's tariff ever
      needs carrying, that is the shape of the fix.

      **Two things this found on the way.** `Tariff::band_for` looked the
      winner up *by name*, so two bands a tenant called the same thing would
      have priced the second at the first one's rate — a mistake forms make
      much easier to commit. It takes the winner by position now.
      And `the_same_answers_serialise_the_same_way` was a tautology that could
      not be falsified: it compared two values equal by construction
- [x] **Rule packs as blueprints — built 2026-09-10.** `booking::PACKS` is five
      ready-made tariffs; `GET /v1/booking/tariff/packs` browses them,
      `POST /v1/booking/tariff/packs/preview` says what one would do, and
      `POST /v1/booking/tariff/packs` takes it. The third blueprint kind this
      build ships, after charts of accounts and trades.

      **A pack is a form already filled in.** Each step is a template id and
      the answers to fill it with — the same pair `PUT /v1/booking/tariff`
      takes from a person — so `Authored::written` builds every band and a pack
      cannot write one the form would have refused. Which is also why this may
      ship numbers while `templates.rs` still ships no presets: **a preset's
      number is unreachable** without abandoning the preset, and a pack's lands
      in the box a business already edits numbers in.

      **D8, honestly.** Half of "a versioned, parameterized list of commands"
      is literally true here and half is not, and the module doc says which:
      the steps really are the authoring calls the screen makes, but a tariff
      is one configuration value and writing it is not a command — where a
      chart is eighteen independently refusable `open_account_in` calls against
      the log. So this is a blueprint of the *catalogue* shape. What survives
      and matters is that `TariffAsWritten::write` is the one write and both
      paths go through it: "a pack writes what the screen writes" is the call
      graph, not a comment.

      **Preview shares one implementation with install without a transaction.**
      `Pack::onto` is pure and returns the tariff an install stores, so preview
      *is* that function and install is that function plus one write — the
      property `preview_chart` buys with a rollback. It buys it without one on
      purpose: a chart install is eighteen commands that can each refuse and
      only a real run is honest about it, while a tariff is one value. And a
      rolled-back transaction would burn a `configuration_version` per preview,
      because `nextval` does not roll back.

      **A pack goes underneath.** First match wins, so appending is the only
      position that cannot reprice an hour the business already decided.
      Bands whose hours something already prices are skipped **by window, not
      by name** — a business that renamed the band covering Thursday evening
      has still answered the question the pack was about to ask, and keying on
      the name is the bug `band_for` already paid for.

      **Installing is a read-modify-write, so it can never write blind.** It
      hands `write` the generation it read, and a caller's own `If-Match` is
      answered *before* the nothing-to-do shortcut — somebody who asked "only
      if it is still at N" and got `200` back would have been handed a tariff
      at `N+1` they had never seen. That second part was a real bug, found by
      the test that expected a `412` and got a `200`.

      **Blueprint validity, and a claim that was false for two kinds of three.**
      ARCHITECTURE asks that every shipped blueprint be previewed against a
      fresh tenant in CI. Only `TRADES` had it: `real_estate` had never once
      been installed against a database — the chart the whole property vertical
      posts into — and four of the five packs went nowhere near the install
      path. Both loops exist now
      (`every_shipped_chart_installs_into_a_fresh_tenant_and_twice_is_harmless`,
      `every_shipped_pack_installs_into_a_fresh_tariff_and_twice_is_harmless`),
      and each was falsified: a chart declaring an account code the domain
      refuses, and an install that stores something other than what it
      returned.

      `every_pack_builds_every_band_it_promises` stays, and covers a different
      thing: it is pure, so it proves the answers make bands without a
      database. The database test proves they survive `configuration`'s JSONB
      and come back the same — which is the half the pure test cannot see.

      **No Ramadan or Eid pack**, which the market would rank first. Both are
      Hijri and drift about eleven days a Gregorian year, and nothing here can
      compute one: `erp_types::Calendar` is a time zone and nothing else —
      checked, not assumed. A pack with the dates typed in would work for one
      year and be wrong the next.

      **No taking a pack back out**, declined in the shape `charts.rs` declines
      editing before install: every band a pack writes is an ordinary band the
      moment it exists, and `PUT /v1/booking/tariff` already replaces the whole
      list. What it would cost is named so it is not re-derived — provenance
      beside each band, because neither cheap key works: not the name (two
      bands may share one), not the content (an edited band stops matching,
      which is when somebody most wants it gone). And that decision has a
      deadline: the stored shape moves for free today and will not after launch
- [x] **`explain` — built** for pricing, and generic: `Rules::explain` names
      every rule tried, in order, with whether each matched. Effective-permission
      inspection already shipped twice (§53). The `explain`-backed **dry run**
      is what remains, and it now has the primitive it was waiting for — 4d's
      rolled-back transaction seam

**Exit:** one engine behind every rule, and a surface most tenants never leave.

---

## Phase 6 — Configured domain · 5–7 weeks

- [x] **Account determination and posting rules — built, by a route this box did
      not imagine.** Every module that touches the ledger carries a typed,
      versioned `PostingAccounts` configuration with its own key and route:
      `sales` (`modules/sales/src/posting.rs:23`), `pos`, `payroll`, `prepaid`,
      `purchases`, `payments`. *(**Corrected 2026-09-14:** `purchases` and
      `payments` carry the typed configuration but have no route, so a tenant with
      its own chart cannot use either; see [Road to selling](#road-to-selling),
      Priority 3.)* The box was phrased as a *mechanism* — a rules
      engine — and the outcome arrived as per-module typed configuration
      instead, which is simpler and refuses at compile time what a rules engine
      would refuse at runtime
- [x] **The business module's ledger path migrated onto them — built**, by the
      same evidence: **there is no module left to migrate.** All six that post
      already resolve their accounts this way, inside the command's own
      transaction so a configuration change cannot land mid-write (L5)
- [x] ~~`StateMachine` as data driving document workflows and approval routing~~
      **Declined 2026-09-14 by the product owner**, with the two boxes below: the
      generic `Document` is not needed. Documents stay compiled types, and
      tenants shape them through settings, claims and custom fields
- [x] ~~`DocumentType` as versioned data; generic `Document` aggregate~~ Declined,
      as above
- [x] ~~One document type ported end to end~~ Declined, as above
- [x] ~~Remaining modules, each landing with its own blueprint and demo
      participation~~ **Stale: it named nothing.** Nineteen modules ship, and the
      demo test requires every registered module to replay and answer

**Before starting:** resolve the open question in architecture §8 about whether
the generic `Document` aggregate is right for the real document mix. *Resolved
2026-09-14: it is not needed, so this phase has nothing left to start.*

**Audited 2026-09-09, and two things came out of it.**

**Two of the six boxes were already built** — see above. Four remain, and all
four are the `Document`/`StateMachine` half, which is exactly the half §8 says
not to start.

**And "What Phases 7–13 unblock" contradicts §8 about whether that question is
answered.** It claims a reservation, a service request, a leave request and a
payroll run are "four documents with genuinely different workflows — which is
the evidence §8 asked for". **They are not that evidence**, and the distinction
is the whole question:

- Those four are **ours**. They are compiled Rust aggregates, they will stay
  compiled, and no tenant defines a fifth. That the *product* has varied
  workflows says nothing about whether a *tenant* needs to author one.
- §8 asks whether tenants "need genuinely different workflows" or "the same
  handful of documents with different fields". A tenant who wants a *field*
  added is already served — `crm`'s custom fields (§41) do exactly that, and
  nobody has asked for more.
- The only real signal so far points the *other* way: the one configuration
  request this build has actually met was fields, not workflows.

So §8 stands as written and this phase stays blocked on customer conversations.
The four documents are evidence about the product, not about the market, and
the paragraph in "What Phases 7–13 unblock" has been corrected to say so.

**Exit:** tenants configure charts, documents, posting and approvals without a
deploy.

---

---

## Build order

*For near-term work, [Road to selling](#road-to-selling) sets the order now; this
section records how the product was sequenced before it.*

**Phase numbers below are historical.** They record the order things were
designed in, and the cross-references in this document depend on them, so they
stay. What follows is the order the work is actually done in, which is a
different thing and changes as the market answers back.

It changed once already, after reading Rekaz's 73 tools and Qoyod's, Wafeq's and
Daftra's pricing. The finding that reordered it is one line long: **three of
Rekaz's tools are accounting integrations, to Qoyod, Odoo and Daftra.** Nobody
builds three of those with a ledger of their own. So the competitor with the
booking product has no books, and the competitors with books have no booking,
and a salon today buys both and reconciles them by hand.

That is the wedge, and everything below is ordered by how directly it serves it.

| | what | why it is here | serves |
|---|---|---|---|
| 1 | **7a · `modules/crm`** | Nothing else can start. A booking is made *by* somebody, a package belongs to somebody, points accrue to somebody | everyone |
| 2 | **7b · the occupancy engine** | The one piece with no substitute. Write-side state, capacity, guards | appointments |
| 3 | **8a–8b · `modules/booking`** | The half of the wedge the accounting vendors do not have | salons, clinics, gyms, studios |
| 4 | **14 · `modules/prepaid`** | Deferred revenue, and the five shapes that share it | wellness, gyms, academies |
| 5 | **15 · `modules/pos`** | A coffee shop cannot open without a till. Unlocks a whole segment that needs no booking at all | cafés, restaurants, retail |
| 6 | **16 · branches** | Does not exist today. Blocks per-branch reporting and the segments that have more than one | multi-site anything |
| 7 | **17 · the public booking API** | How a customer reaches the business. The site itself is a separate React project; this is the surface it calls | appointments |
| 8 | **18 · marketing** | Segments and campaigns over the log | growth |
| 9 | **19 · `modules/inventory`** | Restaurants and retail count stock | restaurants, retail |
| | *then* | Phases 10–13 as written: reports, channels, payments, real time | |

**Two products share one spine.** Appointment businesses need CRM, occupancy,
booking and prepaid. Counter businesses need CRM, POS, inventory and loyalty and
never touch a calendar. Both need the ledger and ZATCA, which exist. Ordering
appointments first is a judgement that salons, clinics and gyms are the larger
and better-served segment; if the first ten sales conversations say otherwise,
POS moves to position 3 and nothing else changes.

---

## Phase 7 — Customers, and the occupancy engine · 4–5 weeks

**Where this comes from.** A working booking ERP was read end to end — a
Laravel system of roughly 407k lines, 74 aggregates, 863 event classes and 230
tables, serving salons and spas. It is called *that system* below. The parts
worth taking are named against it, and so are the parts worth refusing: its own
comments record its bugs, which is the most useful documentation in it.

**The finding that shapes this phase.** Its reservation aggregate writes the
same lifecycle three times — `SeatActivated` / `ShowerActivated` /
`ServiceActivated`, and again for start, end, notes, cancel and restore. That is
most of its seventy reservation events. But its `slot_occupancy` table is
already generic: `(resource_type, resource_id, [start, end), owner_type,
owner_id)`. Somebody found the abstraction, applied it to the write path, and
never took it back into the domain model. **A seat, a shower, a room, a hall and
a person who does the work are one concept**, and this phase builds that
concept once.

### 7a · `modules/crm` — customers as records

The gap the receivables report already exposes: an invoice freezes the buyer's
name (L5), so two spellings are two rows and nothing can answer "everything for
this customer". Booking cannot start without it, because a reservation is made
*by* somebody.

- [x] `Customer` — name, contacts, addresses, tax registration, the fields ZATCA
      needs on a B2B invoice
- [x] An invoice references a customer **and still freezes what it printed**.
      Both, not either: the reference is for the customer list, the frozen copy
      is what the law requires the document to say. Validated against `crm`'s
      *log* and not its projection, because a projection lags and an invoice
      would be refused to a customer created a moment earlier
- [x] Backfill: existing invoices name a customer that no record matches, so the
      first migration is a reconciliation surface, not a foreign key.
      `unmatched_customers` is the worklist — one row per frozen spelling,
      largest backlog first, because the job is matching *people* and forty
      invoices for one name is one decision. `attach_customer` works through it,
      writing the **reference** and never the printed name: what the document
      says about its buyer was frozen at issue and a reconciliation does not get
      to restate a filed document. Validated against `crm`'s log, so a record
      created a moment ago can be matched at once. Re-matching is allowed and is
      itself an event — see [For review](#for-review--decisions-i-made-without-you)
- [x] Receivables groups by customer id where one exists and by name where none
      does, and says which — `AgedCustomer::identified`

**`sales` does not require `crm`.** It was made to, and three tests said no: the
reference is optional, so a till issuing simplified invoices to walk-ins must
not be forced to keep a customer list. The crate dependency and the entitlement
dependency are different things.

### 7b · The occupancy engine

Not a projection. Occupancy is **write-side state** — the read side can be
rebuilt, a booking that was accepted cannot be un-accepted. That system says
the same of its own table — write-side state, never truncated and never rebuilt
by a replay — and it is right.

Built as `crates/erp-occupancy` with its tables in `migrations/tenant/0007`,
which is where that argument lands: a module's `install_sql` is what
`rebuild_schema` drops, and these rows must be somewhere it cannot reach. Same
shape as `erp_eventlog::numbering` for the same reason. Nobody enables
`occupancy`; a tenant enables `booking`, and `booking` links it.

- [x] `Resource` — a person, a place or a thing. Carries a **capacity**.
      Capacity 0 is legal and means out of service, which is retirement without
      a second column and without losing the claims already against it
- [x] `Claim` — one resource, one half-open interval, and a **quantity**
- [x] The conflict test is capacity and not an existence check. **It is a peak
      and not a sum**, which is a correction to the line this plan used to
      carry: `SUM(quantity) over overlaps` counts claims that never coexist,
      so a room type with eight units and eight one-night stays across a week
      turns away a guest asking for the week while seven rooms stand empty
      every night of it. The claims become `+q` at each start and `-q` at each
      end, and the largest the running total reaches is what is held at once
- [x] A guard row per `(resource, date)`, taken with `FOR UPDATE` **in sorted
      order** before the probe. Unsorted, two multi-resource bookings touching
      the same two resources in opposite orders deadlock — their bug, recorded.
      The insert is sorted too: `ON CONFLICT DO NOTHING` waits on a conflicting
      insert that has not committed, so the deadlock is reachable before a
      single `FOR UPDATE` runs
- [x] The batch is checked **against itself**. Theirs was not, and the defect
      it caused is recorded in its own source: one request naming the same
      resource twice at the same hour found nothing already held, wrote both
      claims, and double-booked that resource against itself. Fixed
      structurally by writing each claim before probing the next, so the second
      sees the first and there is no separate self-check to forget
- [x] Times normalised on construction — truncated to whole seconds, in a type
      whose constructor is the only way to build one. Theirs: *"comparing those
      unnormalised is how an overlap check silently passes"*
- [x] Release is by owner and idempotent, so a retried handler is harmless (L8)
- [x] A reschedule ignores the rows it is about to release, so a booking never
      conflicts with its own previous position

**Not modelled, deliberately.** Slot granularity — store instants; fifteen-minute
slots are validation and display, one configuration key. Buffers — a cleaning or
setup allowance widens the claimed interval at claim time, so the probe stays one
comparison.

**All or nothing is the caller's transaction, not the engine's.** `take` writes
as it goes, so a batch refused on its third claim leaves the first two in the
caller's transaction. Rolling back is what makes the booking atomic, exactly as
it is for `sales::issue_in`, and it matters most in `reschedule`: committing
over a refused reschedule gives up the slot the booking already had.

**Exit:** capacity 1 and capacity N both hold under a concurrent test
(`only_one_of_two_bookings_racing_for_the_last_place_gets_it`), a deadlock is
unreachable under one (`a_deadlock_is_not_reachable`), and the engine knows
nothing about what a resource is for.

Left for 8a, where it belongs: **availability and downtime**. When a resource is
*offered* is a recurrence, not a claim, and this engine only answers whether one
more fits.

## Phase 8 — Reservations, and the verticals that prove it is general · 5–6 weeks

**The criterion for "generic".** Four businesses that share no vocabulary must
be configurable without a code change. A hotel that needs a patch means the
engine is still written for one trade.

### 8a · `modules/booking`

- [x] `Reservation` — a customer, a time, and lines. Each line claims resources
- [x] One lifecycle, once: `reserved → confirmed → arrived → in service →
      completed`, with `cancelled` and `no-show` as ends. That system reached
      the same list independently, which is a reason to trust it.
      `ReservationEvent::Moved` is the single event that walks it, and skipping
      forwards is allowed — a walk-in arrives without ever being confirmed
- [x] Typestate, per architecture §4. **As an exhaustive `match` on a pair of
      stages, not phantom types.** Every command starts from a `load`, so the
      stage is only ever known at run time and phantom types would buy one
      boundary check that is this same match with seven zero-sized types on top.
      What the match does buy is real: an eighth stage is a compile error in the
      one place the rules live. Nothing else in this codebase carries phantom
      typestate either, and `Permit<C>` is where it earns its keep
- [x] `Availability` as a recurrence — **and this is the second place the plan
      was wrong.** The specified shape was cron: months, weekdays, days, hours
      and minutes as bit fields. Cron cannot say "half past nine". Its hours and
      minutes are independent sets, so "open 09:30 to 17:00" needs minutes
      `{30..59} ∪ {0..29}`, which is every minute and therefore also matches
      09:05. There is no assignment of those two fields that means what a salon
      means. The calendar half stays as bit fields, which is what made theirs
      compact and indexable; the clock half became the interval it actually is,
      half-open like everything else here
- [x] The customer is claimable as a resource, so "already in another chair"
      needs no special case. Held at capacity one under a reserved `customer.`
      prefix, in the same engine as every chair. **Once per distinct span, not
      once per line** — four seats at one showing is one person at one time and
      must be allowed; a haircut at ten and a massage at half past is one person
      in two places and must not be
- [x] Fungible pools: book the **type**, assign the unit later. The pool holds
      the count and the unit holds the identity, so assigning takes a second
      claim on a different resource and nothing is counted twice
- [x] Add `erp_occupancy::CATALOG` to `erp_api::CATALOG`

**Local time, and the ceiling on it.** A rota is local and an instant is not, so
`booking.calendar` is a fixed offset defaulting to `+03:00`. Exact for Saudi
Arabia and the Gulf, which have no daylight saving. A market that does needs
`chrono-tz` and a zone name, and that is a change to `calendar.rs` and to
nothing else.

**A defect the tests found in 7b.** `occupancy_claim` was keyed on
`(owner, resource, starts_at)`, which made a legal booking impossible: three
lines of one reservation each taking one place in the same class at the same
hour is one owner holding three, and it arrived as a primary-key violation
reading `duplicate key value violates unique constraint`. The key now includes
`ends_at` and a repeat accumulates through `ON CONFLICT DO UPDATE`. Covered by
`one_owner_asking_twice_for_the_same_span_holds_two_of_it`.

**No money, deliberately.** A reservation carries no price, no tax and no ledger
posting. Pricing is 8d and one pure function; invoicing a completed booking is
after that. A number on a line now would mean writing the pricing rules twice.

**Exit:** the diary and the rota over HTTP, the engine holding what the diary
says, and a replay reproducing both.

### 8b · Six fixtures, one engine

Written as `modules/booking/src/trades.rs` — six `const` blueprints — and
`modules/booking/tests/fixtures.rs`, which fits a tenant out from each one and
books the thing that is characteristic of that trade.

- [x] **Salon** — person plus chair, minutes, capacity 1, a named person. Two
      stylists and two chairs refuse the third booking without anybody writing
      "a salon has two chairs" in the code
- [x] **Restaurant** — table with covers as capacity, a sitting as duration.
      Four at a table for six leaves two, a party of four will not sit at a
      table for two, and the later sitting takes the same table
- [x] **Hotel** — room type with N units, nights, assignment deferred. Booking
      the type leaves every room untouched; check-in claims one, and the pool is
      not charged twice
- [x] **Class** — instructor plus room, capacity N, many customers in one slot
- [x] **Gym** — no slot at all. The rota holds the classes and nothing for the
      floor, the door or the changing rooms, and the diary of a gym operating
      normally is empty. The membership itself is Phase 14
- [x] **Ticketed slot** — a museum sells 500 places at 10:00 with no named
      resource. A family of four, a coach party of two hundred, and nothing
      assigned to anybody
- [x] Each is a blueprint (D8), not a branch in the code

**No code change was needed.** All six compiled and passed against `booking` as
8a left it. Nothing in the module reads a trade's id, so a seventh trade is an
entry in `TRADES` and no code at all.

**What the class fixture found.** Written first with one customer booked twelve
times, and refused on the second: `customer.c1 holds 1 of 1 then`. Not a bug —
the "already in another chair" rule doing its job — but it pins down what a
class booking may look like. **Twelve places is either twelve customers, or one
customer on one booking.** A parent bringing four children is one reservation
with four places; twelve strangers are twelve reservations. What is refused is
one person holding twelve *separate simultaneous* bookings, and it has to be:
a system that allowed it would have nothing left to catch the salon
double-booking with, because they are the same query.

**Where the six came from.** Rekaz sells to salons, clinics, gyms, studios,
museums, event ticketing and horse stables. Those seven need four shapes between
them: capacity one with a named person, capacity N in one slot, a pool of
interchangeable units, and pure capacity with nobody assigned. Restaurants add
covers-as-capacity and the gym adds the case where there is no slot at all.

**Exit:** six trades demonstrable from blueprints, and `fit_out` is the same two
commands a person clicking through the screens would run.

### 8c · moved

Packages grew into [Phase 14 · `modules/prepaid`](#phase-14--modulesprepaid--everything-the-customer-has-already-paid-for--56-weeks)
once it became clear that packages, subscriptions, gift cards, deposits and
loyalty points are one accounting problem wearing five names.

### 8d · Pricing, once and pure

- [x] One `price` function. No database, no settings, no clock — so it is
      testable and cannot drift with configuration. `modules/booking/src/pricing.rs`
- [x] **Time-based pricing.** Peak and off-peak, which Rekaz sells and every
      salon wants. It is an argument to `price`, resolved from configuration at
      the moment of booking and frozen onto the line (L5), never read again.
      A band's *when* is an `Availability` — the same recurrence that says when
      a resource is offered, because "open Thursday evening" and "dearer
      Thursday evening" are one shape and a tenant should learn it once
- [x] **Tax-exclusive discounts**: an allowance comes off the net and tax is
      charged on what remains. **No tax is computed in `booking`**, and that is
      the point: a reservation is not a tax document, so the allowances travel
      with the line to `sales` when it is invoiced and reduce the band they
      come off there. The tax-exclusive property falls out rather than being
      something two modules each have to remember
- [x] `Money`, never a float

**The rounding rule moved before it could be duplicated.** That system's engine
takes floating-point amounts and its own docblock records three implementations
that disagreed, every fixed discount differing by exactly the tax on it. The
half-away-from-zero rule was private to `sales::vat`; `booking` needed the same
one for a peak-hour uplift. It is now `Money::scaled_by`, which is the only
place in the workspace a rate is applied to an amount, and `sales`' fifty-four
tax tests are what keep it honest.

**The order of operations is written down, because it is not free.** The band
moves the **rate**, then quantity multiplies, then allowances come off the
total. Banding the total instead gives a different answer wherever the rounding
bites, and it bites at the prices businesses actually use: a 33.33 service at a
quarter more is 41.66 each, so four are 166.64 — banding the total gives 166.65
and a customer who checks finds a halala nobody can explain.

**Bands, not prices.** What a service costs is the caller's to send; *when* it
costs more is the tenant's to configure. Putting the price list on the server
would need a service catalogue, which nothing has asked for and which `what`
being opaque is currently buying us. A client cannot decide its own peak rate,
which is the half that had to be server-side.

*Amended by Phase 17 (§43).* A stranger booking through the public site cannot
send a price either, and a deposit is a fraction of one — so a bookable now
carries an optional **published rate**, and a public line is priced at it.
That is one field, not a catalogue: `what` stays opaque, the counter still
sends what it charges, and the bands still do the rest.

**A whole span, not its start.** A treatment beginning before peak and running
into it is charged at the base rate. The alternative is the answer a customer
argues with, and a business that wants the other rule splits the booking, which
is what they would do at the till anyway.

**Exit:** six verticals demonstrable from blueprints, and one pricing path.

---

## Phase 14 — `modules/prepaid` — everything the customer has already paid for · 5–6 weeks

**One module, six aggregates, one liability.** Packages, subscriptions, gift
cards, wallets, deposits and loyalty points are the same accounting problem:
money received now for value delivered later. Building them as separate modules
would write deferred revenue five times, and law L3 would then forbid the one
screen every one of these businesses wants: *what does this customer have with
us?* Tables in different projection groups never read each other, so four
modules means four reads at four checkpoints that can disagree with each other
while somebody is taking money against the answer.

The name is `prepaid` and not `entitlements` because `entitlement` already means
something here: the control plane's table of which **modules** a tenant has
switched on. Two meanings for one word in a codebase that renamed itself to
avoid exactly that.

**Where the shape comes from.** That system's chart of accounts had worked most
of this out already: `deferred_revenue` and `loyalty_liability` as liabilities,
with `loyalty_earned`, `loyalty_granted`, `loyalty_redeemed` and
`loyalty_expired` as unconstrained counterparts. Its `ServicePackage` carries
`expiration_type` (none/days/months), `expires_after`, `activation_count` and
both `rank_points` and `walaa_points`. Its `ClientPackage.type` records
bought / gifted-by-client / gifted-by-business / free-from-coupon, which is what
makes "who actually paid for this" answerable a year later.

### 14a · The two recognition models, which are not interchangeable

This is the part that is an accounting error if it is got wrong, and Rekaz
splits its own product along the same line, which is evidence the distinction is
real and not theoretical.

| shape | liability | revenue recognised |
|---|---|---|
| Package (10 sessions) | yes | when each session is **delivered** |
| Subscription (monthly gym) | yes | **ratably over the period**, attended or not |
| Gift card / wallet | yes | when spent |
| Deposit against a booking | yes | when the booking is served, or forfeited |
| Loyalty points | yes | when redeemed, or expired as breakage |
| **Coupon** | **no** | never. No consideration was received |

- [x] A gym subscription recognises monthly whether or not the member appears.
      A ten-session package recognises per session. Treating them alike
      misstates revenue every month in one direction or the other. Two
      aggregates, `Entitlement` and `Subscription`, and the split is the reason
- [x] A coupon is a discount at the point of sale and **not** a liability.
      `Reason::was_paid_for` is the whole of it: a grant nobody paid for carries
      no value, posts nothing, and recognises nothing when it is delivered

### 14b · Packages and subscriptions

- [x] `Package` — N of a service, with the balance that remains. **A deposit is
      the same aggregate**: it differs in being an amount rather than a count
      and in naming what it is held against, and in nothing else. Redeeming
      against a reservation line is an opaque id; `prepaid` does not know what
      a booking is
- [x] `Subscription` — a period, a price, a renewal, and **freeze**. Freezing
      earns everything up to that moment and stops the clock; resuming pushes
      the term out by exactly the time it was stopped for. How *long* a freeze
      may run is not decided here, because Rekaz's own copy concedes those
      rules are policy-dependent
- [x] Expiry, and breakage. An entitlement carries an expiry instant rather than
      `none | days | months`: the rule that produced the date belongs to
      whoever sold it, and storing the date is what makes a replay reproduce
      the decision instead of recomputing it
- [x] `type` on every grant: bought, gifted by a customer, granted by the
      business, free from a coupon. It decides the accounting, not the wording
- [x] Entry validation: `Subscription::admits` answers *is this live right now*
      from state rather than from a projection that may be a second behind,
      which is what a gym door needs

### 14c · Loyalty, in three mechanics

Rekaz rewards by **points, stamps, or visits**, ties rewards to specific
services, and puts the card in Apple Wallet. Stamps are the coffee-shop punch
card and are not a points balance with a different label.

- [x] Points — a balance earned at a rate, redeemed at a value
- [x] Stamps — N of a specific thing buys one free. The café mechanic
- [x] Visits — count of attendances, independent of spend
- [x] Tiers, which that system calls `Membership`: points_start, points_end, an
      earning rate. Easy to misread as a gym membership; it is a rank

**Both open questions were answered by the owner, and this is what was built.**

- [x] **IFRS 15, and no shortcut.** The answer was *always IFRS, without
      shortcuts*, so `Scheme` has no setting that selects the other treatment
      and there is no code path for it. What is deferred is a fraction of the
      sale by relative standalone selling price —
      `spend × (count × worth) / (spend + count × worth)` — and not the reward's
      face value. A hundred riyals awarding a hundred points worth ten halalas
      defers **9.09 and not 10.00**, which is the difference the shortcut hides.
      `points_defer_a_fraction_of_the_sale_and_not_the_reward` is that number,
      asserted against the ledger

- [x] **Multi-purpose vouchers are disallowed for now, and it is a guard rather
      than a note.** The claim "every shape here is single-purpose" was until
      now only in the docs: nothing stopped a caller granting an amount with no
      uses and nothing to hold it against, which is exactly an open-value gift
      card. `grant` now refuses that shape (`PrepaidError::OpenValue`), so what
      keeps this module out of tax is a check and not a hope.
      `an_amount_that_names_no_purpose_is_refused` grants the refused shape and
      the allowed one — a deposit, which differs by naming the booking it
      secures — one after the other

**Three divergences from what this section assumed, and the reasons.**

- **One aggregate for the three mechanics, not three.** They differ in what
  produces the count — a rate on spend, a named item, an attendance — and in
  nothing after it. `Mechanic` is fixed at open and read by the business;
  nothing branches on it. Rekaz models them separately and pays for it in three
  earning paths and three balances. The same lesson packages and deposits
  taught in 14b

- **`earn` does not need the sale, only its price.** The allocation is a
  fraction of the transaction price, so the caller passes `spend`; `from` is an
  opaque id and a reconciliation surface, exactly as `against` is for a deposit.
  A tighter coupling would make `prepaid` depend on `sales`, which siblings may
  not do. The cost is that the invoice and the deferral are two transactions —
  the module's existing bargain, and `a_liability_agrees_with_the_ledger` is
  what catches a pair that came apart

- **A rank is read from `lifetime`, which never decreases.** Spending points
  does not cost a rank, and neither does breakage: what was earned was earned.
  The movement that *crosses* a threshold earns at the old rate and the next one
  at the new, because any other reading makes the award depend on itself

- **There is no default scheme.** Account codes have a conventional value every
  chart ships; what a point is worth does not. A tenant who has not configured
  one cannot earn (`PrepaidError::NoScheme`) rather than earning against a
  number nobody chose (L6)

- **A card survives its own breakage.** Points running out is not the end of the
  card — it can earn again the next day — which is the one place this aggregate
  is not shaped like `Entitlement`

**What the answers leave open, recorded rather than guessed at.** If open-value
cards are ever wanted, the classification is a property of the *product* and not
a tenant setting, and the sale has to settle its own tax question first: the
refusal above is where that decision lands, and `Reason::Bought` still assumes
the sale carried its own tax.

### 14d · One ledger integration

- [x] Every shape posts through `ledger::post_entry_in`, in the same
      transaction as its own event, exactly as `sales` does
- [x] The chart templates gain the liability account, `2400 Deferred revenue`,
      in every template for the reason VAT and Zakat are in every template
- [x] The invariant: **the deferred revenue balance equals the sum of
      unredeemed value.** `a_liability_agrees_with_the_ledger` asserts it after
      grants, redemptions, recognition, a freeze, a resume, a renewal, a
      revocation and a cancellation

**This module posts the deferral, not the sale — a divergence, and the reason is
ZATCA.** The plan said *"sale is Dr cash / Cr deferred revenue"*. That skips the
tax invoice, and a Saudi business selling a gym year cannot skip one: it is a
supply, it needs an invoice, and the invoice has to be cleared or reported.
`sales` already does all of that. So the sale is an ordinary invoice and `sales`
posts it; `prepaid` adds the fact that the revenue is not earned yet, with
Dr revenue / Cr deferred at the grant and the reverse as it is delivered.

Two things follow. **No tax anywhere in this module**, so there is no second
opinion to keep consistent. And **the reclassification is visible** — an auditor
sees revenue booked and then deferred, which is what happened, rather than a
sale that never appeared in the sales ledger.

**A bug the canary was blind to, found by a different test.** `renew_subscription`
posted the release of the term that ended and never the deferral of the term
that began, so the read model carried a liability the books did not. The canary
had not renewed anything; it does now.

**Recognition is a cumulative total, never a sum of instalments.** Each step
computes what *should* have been earned by a date and posts the difference, so
running a month-end job twice posts nothing the second time, and
`Money::apportioned` being exact at `n/n` means the last day of a term brings
the liability to exactly zero. Summing instalments would strand a halala on
almost every term, in the account that is supposed to be the canary.

**Exit:** a gym sells a frozen-then-resumed annual membership and it reconciles
to the trial balance. The café's tenth coffee is 14c and waits on the answer
below.

---

## Phase 9 — People, and what the Kingdom requires · 7–9 weeks

**Why this is a phase and not a module.** Payroll touches the ledger, attendance
touches booking, and documents touch the outbox. It is the first thing that uses
three existing modules at once, which is the real test of whether extension by
subscription holds.

**It is also the first phase that changes who may do what**, which is why it
grew: §9b makes the org chart an authorization structure, and §9c is the
decision about which plane that structure is allowed to reach into. Neither is
payroll, and both have to be settled before `Employee` has a field.

### 9a · `modules/hr` — the org chart

- [x] `Employee`. `Position`, `Department` and `Contract` are not built: the
      tree and the claims are what everything else in this phase stands on, and
      three more aggregates before the authorization model was proved would
      have been three more things to change when it moved
- [x] Skills: which services a person may perform. `booking::assign` reads it,
      which is why `hr` lands below `booking` and not beside it.

      **An empty list means anything, not nothing**, or every existing tenant's
      rota would be refused the day the module is switched on. The edge is that
      recording the *first* skill starts restricting, which is why the API takes
      the whole set at once and offers no way to add one — and why the read
      answers `restricted` rather than leaving `[]` to be read either way.

      `eligible_for` is **one question**: employment, documents and skill
      together, because a caller who had to ask both would eventually ask one
- [x] Shifts, on Phase 8's recurrence. The same problem, so the same type — and
      the type moved to `crates/erp-recurrence` to make that possible.

      `hr` could not reach `booking::Availability`: `booking` already depends on
      `hr`, because a bookable resource names an employee and a lapsed work
      document stops the rota, so the other direction closes a cycle. It moves
      below both, which is the argument that made `erp-occupancy` a crate — and
      which `erp-occupancy` itself half-anticipated: *"when a resource is
      offered is a recurrence, and it belongs in `booking`"* was true while
      booking was the only thing that needed it.

      **The error codes came with it**, `booking.not_a_window` becoming
      `recurrence.not_a_window` and six more. A code is a client-facing
      identifier and this API tells clients to branch on it, so that is a
      breaking change — free because nothing is released, and it would not have
      been in six months. `Calendar::KEY` is `tenant.calendar` now rather than
      `booking.calendar`, because a business has one clock and both the diary
      and the rota read it.

      **A shift refuses nothing**, and that is deliberate: it says when somebody
      is *scheduled*, and people cover, swap and stay late. A system telling a
      manager she cannot ask somebody to stay is not a rule it gets to make — a
      lapsed iqama is, because the law says so, and that stays `may_work_on`.

      Empty means *no pattern recorded*, not "never works", for the reason an
      empty skill list means anything; the read answers `rostered` so `[]`
      cannot be taken the wrong way round.

- [x] Attendance and leave. **A day is recorded whole**, not clocked in and out:
      a half-recorded day is somebody who forgot, somebody who left early, or a
      device that lost power, and nothing can tell which — so it is recorded
      when it is *known*, which is what approving a timesheet is.

      Zero minutes is an absence somebody recorded, which is a different fact
      from a day with no row. The same day again with different minutes is a
      correction, and the aggregate keeps a **bounded** window of recent days
      only to tell that from a retry — a career's attendance on an aggregate
      would grow without bound, which is `prepaid::Loyalty`'s problem and answer.

      Leave is whole days, **inclusive at both ends**, and the count is stored
      because an inclusive range is exactly the arithmetic somebody gets wrong
      by one. Reads find leave that *touches* a window rather than starts in it,
      or a rota for April would show somebody who is on a beach for the first
      week of it.

      **"Balances that accrue" is both halves, in the two modules they belong
      in.** `hr::leave_taken` says what has gone, per kind, which is the same in
      every country. `hr_sa::annual_entitlement` says what was owed — Article
      109's 21 days rising to 30 after five years, pro-rated, with the step
      landing mid-year where it falls — and `hr_sa::sick_days` splits a stretch
      of illness across Article 117's pay bands, where a second illness in the
      same year does not start again at full pay.

      `GET .../leave-entitlement` puts them together, and **`annual_left` may be
      negative**: somebody who took three weeks in January and left in March has
      overdrawn, and clamping it would hide money the business is owed back.

      Half-days are not modelled: the rounding argument every business has about
      them is a decision nobody has asked for

**The org is a tree, and it is the point.** Employees are not a flat list with a
`manager_id` decoration; the reporting line is the structure, and everything
below depends on being able to walk it. So: one `reports_to` edge per employee,
a single root per tenant, and **cycles refused at the command** — not because a
cycle is untidy but because the claim union below would not terminate.

- [x] `Employee::reports_to`, and `Reparented` as an event of its own. Moving a
      person moves everything they carry, which makes it the operation an
      auditor asks about — and an `Amended` that quietly changed a parent would
      not answer them. `amending_details_cannot_move_somebody_in_the_chart` is
      what keeps the two apart
- [x] A cycle is refused, in `claims::place`, by a recursive walk **down** —
      because that is the direction one closes in: making `A` report to somebody
      already in `A`'s subtree is what creates it

### 9b · Claims, and why they travel upward

**The rule the owner asked for: a manager automatically holds everything their
reports hold.** Formally, for each node in the tree:

```text
claims(node) = own(node) ∪ ⋃ claims(child) for each child
```

The reason is operational and good: a manager has to be able to cover for anyone
beneath them, and nobody should have to remember that giving a new clerk a
permission also means giving it to their supervisor. Granting downward is the
arrangement that produces the support ticket *"the branch manager cannot approve
what her own cashier can"*.

**Every consequence below follows from that one line, and each is a decision
rather than an accident.**

- [x] **The root holds the union of every claim in the company.** Settled as
      intended: the person nobody reports to is the owner, and a business owner
      who could not approve something in their own company would be a
      surprising product.

      Somebody who must sit *outside* it — an external auditor, a bookkeeper on
      retainer — is **not an employee and does not go in the tree**. They are a
      platform membership with a role, which is the other axis entirely and is
      exactly what §9c kept separate
- [x] **A grant at a leaf is not a local act**, and the API makes that
      impossible to omit: `POST .../claims` answers with `holders` — *everyone*
      who now has it — rather than an acknowledgement. A screen cannot fail to
      show what it was handed
- [x] **Segregation of duties breaks, and this is the part that fails an audit.**
      The control every accounting system is measured on is that the person who
      raises an invoice is not the person who approves its payment. Under a
      bottom-up union their shared manager holds both, automatically, the moment
      the org chart says so.

      So a claim is markable **non-propagating**, and `hr::SEGREGATED` is the
      list that must be — a constant and not configuration, because what an
      auditor requires is not a preference a tenant expresses. `grant` refuses
      to propagate one **even when asked to**, and the response says
      `propagates: false` rather than silently doing something else.
      `a_segregated_claim_travels_nowhere` is the test.

      **Non-propagation is half of segregation, and this bullet read as though
      it were all of it.** Stopping a claim travelling *up* the tree does not
      stop the holder exercising it *on themselves* — a supervisor granted
      `hr:approve_timesheet` could sign their own hours, which is the same
      control failing for a different reason. Refused since 2026-09-10 by
      `hr::may_for`, which takes the subject; `nobody_approves_their_own_timesheet`
      is that test. An owner is exempt, deliberately — §52
- [x] **It is not computed on demand.** `org_claim_effective` is maintained when
      the org changes and read as one indexed lookup when a command asks.

      *Until 2026-09-09 no command asked.* The table was correct, indexed and
      unread — see §52. Three commands ask now.*

      The recomputation is **the whole set, not an increment**, and that is
      deliberate: an incremental update would be a second implementation of the
      union rule, free to disagree with the first. This codebase has already
      been bitten by a rule written twice — `pos`'s drawer — so the union exists
      once, in one recursive SQL statement, and every change re-runs it. Marked
      `ponytail:` with the condition for changing it

**What 9a still owes**, and it is additive: skills, shifts, attendance, leave,
positions, departments and contracts. Each of them hangs off `Employee` and
none of them changes the authorization model, which is why the tree and the
claims went first — three more aggregates written before the model was proved
would have been three more things to change when it moved.

### 9c · Which plane the claims live on — **decided: they do not leave the tenant**

Authorization today is *control-plane*: `Access { role, overrides }` per identity
per tenant, four coarse `Capability` values, checked by `Allowed<C>` at the edge
and cached across nodes in Redis. Employees are *tenant-plane*. A hierarchy in
one granting permissions checked in the other crosses the boundary that D15 and
the two-plane split exist to keep clean — so it does not.

- [x] **Decided (a): hr claims are domain claims.** They answer *"may you approve
      this leave request, discount beyond ten percent, sign off this timesheet"*,
      and are checked **inside module commands** where the decision is made.
      `Capability` and `Allowed<C>` are untouched: the platform keeps answering
      *"may you reach this endpoint at all"*, and `hr` answers *"may you do this
      particular thing"*.

      **Corrected 2026-09-10: the decision was right and the wiring did not
      exist.** This box was ticked, and this paragraph was written in the
      present tense, while **no command anywhere consulted a claim** — `holds`
      had one caller and it was a test. An audit found it (§52); the tenses were
      aspirational for months.

      It is true now, and here is where: `purchases::pay_bill`,
      `sales::may_credit` (both credit paths) and `hr::record_day`, each through
      `hr::may` or `hr::may_for`, each inside the command and before anything is
      written.
- [x] **Two lines that must stay true**, now three tests in
      `modules/hr/tests/planes.rs`: no `hr` type appears in `erp-control`,
      `erp-web` or `erp-tenant` and none of them depends on the crate; nothing
      in `hr` reaches for `Invalidate`, `forget` or `ControlPlane`; and nothing
      in `claims.rs` names a `proj_` table.

      They are source-scanning and crude, which is the point: the decision is
      not enforced by any type, so what enforces it is a test that reads what a
      reviewer would have to read

**What it buys.** No plane is crossed. Nothing has to invalidate a session cache
when somebody is promoted — `shared.rs` warns that a stale *logout* is the one
thing that cache must not serve, and a stale *promotion* would have joined it. A
tenant's own org chart cannot widen what the platform believes about that tenant.

**Rejected: (b), the claims feeding the control-plane role.** It needs an
employee → identity mapping and a tenant-wide session invalidation on every
re-parent, and it puts a customer-editable tree in front of the platform's own
authorization. It is also the harder direction to leave: claims that turn out to
belong at the edge can be promoted later, whereas a control-plane hierarchy
shipped first cannot be quietly demoted.

**Where the effective claim set lives, which (a) settles.** Not in a projection.
A command that has to know *"may this person approve this"* cannot read a read
model that may be a second behind — the same reason `sales` checks a customer
against the log rather than `proj_crm`, and the same reason a claim revoked a
moment ago must already bite.

But the union is a subtree walk, and loading every employee aggregate to compute
one inside a command is not viable either.

- [x] The effective set is **write-side state in the tenant migration chain**
      — `migrations/tenant/0008_org_claims.sql` — maintained in the same
      transaction as the org event that changed it. `proj_hr` exists alongside
      it for the screen that *draws* the chart, and is not what any check reads.

      **Half true, found 2026-09-10 while correcting this section.** The *claim*
      is read write-side, as written. But a command knows its caller as an
      **identity**, and turning that into an employee goes through
      `hr::employee_by_login`, which is `SELECT … FROM proj_hr.employee`
      (`modules/hr/src/projections.rs:640`). So one half of every check does
      read a model that may be a second behind.

      **Which direction that fails matters, and it fails the safe way for the
      dangerous case.** A login linked a moment ago does not resolve until the
      projection catches up, so the answer is *refused* — fail-closed. A login
      **un**linked a moment ago still resolves, so there is a brief window where
      somebody who has just been detached could still act. That window is
      bounded by the control plane, which revokes the session and the membership
      and is the authoritative gate for somebody leaving; the claim is a second
      layer beneath it.

      **And the plane test passes on the letter.** `planes.rs` scans `claims.rs`
      for `proj_`, and finds none, because the read is one function call away in
      `projections.rs`. The test enforces the rule it states and not the one it
      means.
- [x] **Decided: it stays read-side, and the reason is written down.**
      It is the only projection read on an authorization path. Moving it is a
      column on the org-claim chain maintained beside the link event, in the
      same transaction — the same shape the effective set already uses. Not
      urgent, because the failure direction is fail-closed where it counts, and
      not free, because it is a migration. Deferred with the reason written
      down rather than discovered again

      **A design error the first run caught.** `PRIMARY KEY (employee, claim,
      branch)` cannot hold a nullable column, and company-wide is exactly
      `branch IS NULL` — so the key would have forced every claim to name a
      branch, which payroll cannot. It is two partial unique indexes instead,
      and the second is not redundant with the first because Postgres treats
      NULLs as distinct

### 9d · Branch scoping, now that branches exist

Phase 16 built the dimension. Everything in `hr` carries it: `Employee`,
`Department`, `Position`, `Contract`, shifts and attendance.

- [x] **A record's branch is not the request's branch, and conflating them is
      the bug waiting here.** Phase 16's branch travels in `Metadata` and means
      *where this request happened*; `Employee.branch` means *where this person
      works*. They differ legitimately and often — an Olaya manager visiting
      Malaz records attendance for a Malaz shift — and a report that read one
      where it meant the other would be wrong in a way nobody notices for a
      quarter
- [x] **A filter, not a wall.** `ledger::post_entry_in` *refuses* a document
      dated to a branch that is not open; `hr` reads must **default** to the
      caller's branch and widen on an explicit parameter. It cannot be a wall:
      payroll, the org chart and an end-of-service calculation are company-wide
      by nature, and a boundary that refused them would make the feature
      unusable in the first month
- [x] Every list endpoint states which of the two it is. `GET /v1/hr/employees`
      says it in the operation summary and takes `?scope=all`, which is what a
      payroll run and an org chart both want
- [x] **Claims carry their branch up the tree.** The union is over
      `(claim, branch)` pairs. `a_claim_carries_its_branch_up_the_tree` asserts
      both halves: the regional manager accumulates Olaya *and* Malaz, and the
      Olaya manager does not gain Malaz

### 9e · Documents that expire

An expired iqama stops a person working. The module warns before the date and
refuses to roster anyone whose document has lapsed.

- [x] Identity documents, work permits, medical certificates, professional
      licences — each with an expiry. A **date and not an instant**: an iqama
      expires on a day in Riyadh, not at an hour in UTC, and storing an instant
      would make the answer depend on which side of midnight somebody asked.

      Four variants and not a free-text kind, because a rule that reads a string
      is a rule that silently does nothing when somebody types `Iqama`
- [~] The reminder. `hr::expiring` is the read — what has gone *and* what is
      going, soonest first, because burying the lapsed ones below the upcoming
      ones is how they stay buried.

      **The outbox producer is not built, and the reason is worth recording:**
      the tenant dispatcher has *no handlers registered at all*
      (`bin/worker.rs`: "an empty dispatcher claims nothing"). Email lives on
      the control plane, because the things that send it are control-plane rows,
      and a module cannot reach it. So an effect enqueued here would sit in the
      outbox for ever.

      What reaches somebody today is a `HealthJob` invariant —
      `WorkDocumentExpiry`, beside `CertificateExpiry` and for the same reason.
      **A lapsed document is a separate finding from an expiring one**, not a
      louder version of it: one is somebody to remind, the other is somebody who
      must come off the rota today, and collapsing them into one severity is how
      the second gets treated like the first.

      The *email* still needs a tenant-plane handler that does not exist
- [x] Escalation: a document that lapses is not a warning that was ignored, it
      is a person who may not be rostered. `booking::assign` refuses them, at
      the moment they would be — against `hr`'s **log**, so an iqama renewed
      this morning counts now.

      The link is `ResourceEvent::Declared { employee }`, **set once** like
      `branch` and **optional**: a business that keeps a diary and no staff
      records is unaffected, which is what makes this additive rather than a
      migration. `booking` gains a crate dependency on `hr` and **not** an
      entitlement one — the same distinction `sales` and `crm` already draw.

      Somebody with no documents recorded may work. A business that has not
      started recording them must not find its whole rota refused the day the
      module is switched on

### 9f · `modules/payroll`

- [x] Salary structure: basic, allowances, deductions — on `Employee`, because
      every question anybody asks of it is asked about a *person*, and the log
      keeps the history a contract aggregate would have been for.

      **Amounts, not percentages.** A housing allowance quoted as 25% of basic
      is stored as the riyals it came to, because a rate stored here would be
      recomputed on every run and a basic-pay rise would silently restate last
      month's payslip
- [x] A run produces a journal entry — **posted inline through
      `ledger::post_entry_in`, not by subscription.** The plan said subscription,
      pointing at `tax_sa → sales`; that direction is right for *reading* a
      module's events and wrong for money. A run that existed and had not posted
      yet is a state nobody could explain, so it commits with its entry the way
      an invoice does.

      **Drafting and approving are two steps.** Drafting posts nothing, so a
      business reads it, fixes the two people whose overtime is wrong, and runs
      it over. A single-step run would have posted the first attempt before
      anybody looked.
- [x] Commission from booking: a person earns on the services they performed,
      which is where the three modules meet.

      **`booking::performed`** says who completed which priced lines and what
      they came to — only `completed` bookings, because a commission paid on a
      no-show is money given away twice, and only priced ones, because a line
      with no charge is a business that bills elsewhere rather than one that
      charged zero.

      **`Salary::commission_bp`** is the rate, on the employee. A rate and not
      an amount, which is the opposite of every allowance beside it and for a
      reason: an allowance is a sum somebody agreed, and a commission is a share
      of a number that changes every month.

      **The split is the design.** The caller sends the *basis* — who is in the
      run and what they did are facts a person assembles, and a caller could get
      either wrong — and `payroll` applies the rate from the employee's own
      record. So a caller can be wrong about what somebody performed and never
      about what they earned. `a_basis_without_a_rate_earns_nothing` is the test.

      Commission is part of **gross**, because statutory contributions and
      end-of-service are computed from what somebody earned rather than from the
      predictable part of it. The payslip carries what it was earned on, because
      "five per cent of 24,000" is a figure somebody will query

**Three things this got wrong first, all caught by a test.**

- `5100` is **Rent** in every shipped chart, and the first `PostingAccounts`
  used it for wages. The kind of default that posts a year of salary somewhere
  nobody looks until an audit. `2210 Payroll deductions` did not exist and now
  ships in every chart, for the reason `2400` and `5910` do
- `try_create` plus catching `AlreadyExists` looked like a way to detect an
  existing run. That variant is about a *different request* reusing an id, so
  with no fingerprint a second draft looked like a retry and redrafting silently
  did nothing. Fixed by not needing the distinction: a run is named by the
  caller and redrafting is the operation, so it is `try_execute`
- `salary_for` took a day and **ignored it** — the compiler said so. Employment
  was answered as "now", which would have refused a leaver their last month and
  paid a full month to somebody who started on the 20th. It is
  `was_employed_throughout` now, and a part-period joiner refuses the run rather
  than being guessed at

### 9g · `modules/hr_sa` — a country module, mirroring `tax_sa`

- [x] **GOSI** contributions, employee and employer shares.

      **The rates are configuration and the shipped ones are a starting point**,
      which is the load-bearing decision: the authority sets the schedule and
      has changed it — most recently for people entering after the 2024 pension
      reform, who are on a different and rising scale. A build that hard-coded a
      percentage would be quietly wrong for some employees from the day it
      shipped. `GET .../gosi/schedule` answers with `configured`, so nobody
      discovers on a payslip that they were on numbers nobody had checked.

      The ceiling caps the **base**, not the contribution — capping the
      contribution would give the employee and the employer different effective
      bases, which is the subtle version of the bug and the one a payslip does
      not show. Whether somebody is Saudi is **stated, not inferred**: nothing
      here can work it out from a name
- [x] **WPS — deliberately not built, and the reason is verifiability.** The
      monthly salary file the Ministry mandates has a specification — field
      order, encoding, and each bank's own variations — that this build cannot
      verify from where it stands. **A file that is almost right is one the bank
      rejects on the day wages are due**, which makes guessing worse than
      absence. It is the same position `tax_sa` was in before there was a
      sandbox to submit against.

      Shape-wise it is the ZATCA submission again — a generated document, a
      schema, a transmission, a receipt, a status — so it is not a design
      problem, and building it is a short job **the day somebody has a real bank
      file to test against**. `docs/book/src/api/hr_sa.md` has recorded this as
      deliberate for a while; this box had not, and read as outstanding work.

      **Deliberately not guessed at.** The specification — field order,
      encoding, each bank's own variations — is not something this build can
      verify from where it stands, and a file that is *almost* right is one the
      bank rejects on the day wages are due. Same position `tax_sa` was in
      before somebody had a sandbox to submit against
- [x] **End-of-service benefit** — Articles 84 and 85, as a pure function.
      Exact, and the workspace's ban on floating-point arithmetic is what made
      it so: the first version computed months as an `f64` and the lint refused
      it, which turned out to be a precision *improvement* as well as a rule —
      912 days is 1.24931506… months, not the 1.2493 the decimal version gave.

      One numerator over one denominator, so the rounding happens once and away
      from zero. **365 days and not 360**: the Labour Law speaks in years and a
      calendar year is what a court would read, and a tenant who wants the other
      convention should have to argue for it.

      Why somebody left is **stated and not inferred** — Article 87's marriage
      and childbirth cases and Article 80's dismissal for cause are facts about
      *why*, which nothing here can work out

**Exit:** a payroll run posts, a WPS file validates, and an expiring document
reaches somebody.

### 9g's shape, and why `hr_sa` has no schema

It holds **no state at all**. Every function is arithmetic over what it is
given, and the one thing it stores — the GOSI schedule — is a configuration
value in the shared store. So `install` does nothing and there is no projection
group.

Two guards in the worker had to learn that: *every module has a projection job*
and *every module can be rebuilt* now key off `setup.groups.is_empty()` rather
than a list of exceptions, so the next arithmetic-only module needs no edit
there. That is the second time this week a guard was right and the fix was to
teach it the rule rather than add a name to it.

**And the org half, which is a separate claim and needs its own: met.**
`a_claim_travels_up_the_reporting_line` has a branch manager holding what her
cashier holds without a second grant; `a_segregated_claim_travels_nowhere` keeps
the invoice raiser and the payment approver apart;
`moving_somebody_moves_what_their_team_holds` re-parents and asserts both the
gain and the loss; and `GET /v1/hr/employees` defaults to the caller's branch
with `?scope=all` for the company-wide read a payroll run needs.

**Exit, as it stands.** Two of three, and the third is blocked rather than
undone. A payroll run posts and the trial balance
is still square (`a_payroll_run_posts_and_the_books_balance`); an expiring
document reaches somebody (`WorkDocumentExpiry`). **The WPS file does not
validate, because it is not built** — see §9g for why that is a refusal rather
than an omission.

**What is left in the phase, and both are blocked on something outside it.**

- **The WPS file** (§9g). Its specification — field order, encoding, each bank's
  own variations — is not something this build can verify from where it stands,
  and a file that is *almost* right is one the bank rejects on the day wages are
  due. Same position `tax_sa` was in before somebody had a sandbox.
- **The email reminder** for an expiring document (§9e). The tenant dispatcher
  has no handlers at all — email is control-plane, because the things that send
  it are control-plane rows — so an effect enqueued from `hr` would sit in the
  outbox for ever. `WorkDocumentExpiry` is what reaches somebody meanwhile, and
  it does satisfy the exit criterion.

Everything else in Phase 9 is built, tested and documented.

---

## Phase 10 — Reporting that agrees with the books · 3–4 weeks

**The architectural point, stated before the work.** A dashboard mixing sales,
bookings and payroll looks like it must read three projection groups. L3 forbids
that, and it is exactly the mistake that system made — its projectors declare
which other projections they read, and it needed a bespoke check to police the
rebuild order that created.

**A report module subscribes to the log; it does not read another group.** It
consumes `sales.invoice_issued`, `booking.reservation_completed`,
`payroll.run_posted` and maintains its **own** group: one checkpoint, internally
consistent, L3 satisfied.

### 10a · `modules/reports`

- [~] Sales: revenue by period, branch and product; tax summary — period,
      branch, net, tax, documents and credits. **Product is not built**: it needs
      a working table the width of every invoice line ever issued, and the same
      question is answerable per document today. See review §14
- [x] Booking: utilisation, no-show rate, lead time, revenue per resource-hour —
      `booked`/`completed`/`no_shows`/`cancelled`, `minutes` as the
      resource-hour denominator, and lead time in **domain** time, not commit
      time
- [~] People: headcount, cost, documents about to expire — cost is built, from
      **approved** runs only. Headcount and expiry are `hr`'s own group and no
      cross-group total is involved, so a copy here would be duplication. See
      review §14
- [x] Cash: takings by method and by person, against what was banked — taken,
      refunded, variance, `paid_out`. Nothing here has seen a bank statement, so
      the last column is named for what it is

### 10b · The invariant that makes a report trustworthy

- [x] A report group reconciles to the trial balance, asserted the way
      `an_unbalanced_entry_is_refused` is asserted — `reports::reconciles`, and
      `every_figure_agrees_with_the_books` / `the_demo_passes_every_invariant`
- [x] A discrepancy is a **failure**, not a coloured cell. L6 — the worker's
      `reports_reconcile` health check makes the tenant unhealthy, and
      `a_figure_that_disagrees_with_the_books_is_a_failure` proves the check can
      fail
- [x] The warning from that system, taken seriously: its customer statement is
      built from invoices rather than from the ledger, because the ledger was
      unfinished and its books were going to be deleted and rebuilt. Two
      financial truths that disagree is what this section exists to prevent

**Exit:** every figure on a dashboard is derivable from the log, and reconciles.
**Met**, with the three deliberate gaps in 10a above. The module subscribes to
`sales`, `booking`, `pos`, `payroll` and `ledger` events, keeps one checkpoint,
reads no other group — `this_module_names_no_other_projection_group` — and
replays to exactly what is live.

---

## Phase 11 — Channels, documents, and moving data in and out · 5–6 weeks

**What Phases 7–10 assumed and did not build.** They describe a domain and no way
to reach anybody in it. The system has exactly **one** effect kind — `email.send`
— which is the entire outbound surface. For a product sold in this market the
channel is not plumbing; a reminder that does not arrive is a chair that stays
empty.

Everything here hangs off machinery that exists. D9 already gives an effect a
transaction, a retry policy, a lease and a dead letter, and `two_dispatchers_never_deliver_the_same_effect`
already passes. A channel is a `Handler`, and that is the whole integration.

### 11a · Channels as effects

**Taqnyat (SMS) and FCM (push) are built** — review §15. **WhatsApp is not**, and
that is a design decision rather than a gap: Meta accepts only pre-approved
templates outside a 24-hour window opened by the customer, and this system hands
a transport a finished string. See §26 for what a correct WhatsApp integration
needs and why it is your call. `Relay` stays as the escape hatch for any
provider without an adapter.

- [x] `sms.send`, `push.send`, `whatsapp.send` beside `email.send` — one effect
      kind per channel, so a worker without an SMS relay leaves SMS in the
      outbox rather than dead-lettering it during a rollout
- [x] One `Recipient` resolved at send time, never a phone number frozen into an
      event — a person who changes their number should get the next message. An
      **audience** (`client`, `worker`, `branch_manager`, `operator`) resolved
      against the read model minutes before the send
- [x] **Delivery receipts — assessed 2026-09-09 and deliberately not built.**
      No provider's callback shape is verifiable (Taqnyat's own documentation
      states not one field name), and correlating one needs a change to
      `EffectHandler`, a kernel trait with four implementors, because a handler
      gets no database connection. Building that to feed adapters that cannot be
      written correctly is how unexercised machinery gets in.
      `docs/AMBIGUITIES.md` §1 and §1b. **One captured callback reopens this.**
      The original text follows, and its last line is superseded.
  > ~~Delivery receipts land back as inbound events, so "sent" and "delivered"
  > stay different words.~~ *(The superseded original, kept for the trail — not
  > an open box. Was deferred to Phase 12 because there was no verified
  > inbound surface; Phase 12 built one (`crates/erp-api/src/hooks.rs`, a
  > signed callback per provider), and payments use it. Messaging does not
  > yet — but the box above is the standing verdict: no provider publishes a
  > callback shape that can be written correctly.)*
- [x] **Metering.** SMS is billed per segment, and a message that silently
      becomes three costs three times. Segment counting is part of sending
      (GSM 03.38 against UCS-2, so Arabic is billed at 70 characters), and a
      per-tenant budget refuses rather than overspends (L6)
- [x] Push tokens expire. Cleaning them up is scheduled work, not an
      afterthought — retired by the platform's own rejection, swept after a
      grace period by `messaging.retire_push_tokens`. FCM's `UNREGISTERED` is
      that rejection; `SENDER_ID_MISMATCH` deliberately is **not** — see §15

### 11b · Templates that fetch their own data

The system read for Phase 7 has **two** template systems that do not meet: a
database aggregate whose parameters the caller fills in by hand, and hardcoded
classes with the copy, the business name and the gendered wording compiled in.
Changing a reminder's wording there is a deploy. Both problems have one cause —
a template cannot ask for anything, so somebody must hand it everything.

- [x] A template names an **audience**, not an address: the client, the employee
      on the booking, the manager of that branch, an operator. The recipient is a
      query against the read model at send time. The branch manager comes off the
      org chart — whoever at that branch reports to nobody at that branch —
      rather than a field `branches` does not have
- [x] A template declares **bindings** — `{{ reservation.starts_at }}`,
      `{{ customer.name }}` — resolved from the read models when it is rendered,
      so the caller supplies a subject and nothing else
- [x] Bindings are declared, so an unresolvable one fails **when the template is
      saved**, not when a customer is waiting for a message.
      `GET /v1/messaging/vocabulary` is what an editor shows, and
      `every_binding_in_the_vocabulary_can_be_resolved` stops the two lists
      drifting
- [x] Arabic and English are the same template with two bodies, per D12. Neither
      is a translation of a compiled string, and a template missing one is
      refused rather than falling back
- [x] Rendering happens in the worker, at send time. A reminder for a booking
      that moved says the new time — in the reminder **job**, minutes before the
      send, because the dispatcher deliberately holds no connection while it
      delivers

### 11c · Files, and where they actually live

- [x] `Storage` as a trait: local disk and S3-compatible object storage to start,
      and the tenant chooses. A self-hosted tenant (D15) keeps its own files, and
      that is the point rather than a configuration detail. Both engines ship:
      `Local` against a directory, `S3` against any S3-compatible endpoint —
      round-tripped against MinIO in `compose.yaml`, not against a mock. **No
      presigned URLs**; every byte goes through the API process. See review §18
- [x] An event stores `(engine, key, checksum, size, media_type)` and **never a
      URL**. A URL is where a file is today; a key is what it is
- [x] The checksum is verified on read. A document that comes back different from
      what was stored is a failure, not a warning — `erp_storage::fetch` refuses,
      and the route answers `500` with `storage.corrupt`
- [~] Attachments are polymorphic — a document belongs to an invoice, a booking,
      an employee record — and the owner is what authorizes reading it. **The
      first half is built and the second is not**: "may this person see this
      invoice's attachments" is the same question as "may this person see this
      invoice", and this system answers neither per record yet. That is Phase 5

### 11d · Spreadsheets, both directions

- [x] Export any list the API can page. It is the same query, a different
      encoder, so a new list is exportable the day it exists — `Accept:
      text/csv`, as **one layer** in `erp_api::router`, so no handler knows it
      happened
- [x] **Large exports — decided against, until something takes a minute.**
      They would be effects rather than requests: generate, store (11c), then
      send a link (11e), because a report that takes a minute must not hold a
      connection. **Every list in this API is capped at a page and none takes a
      minute**, so there is nothing to move off the request path. The machinery
      — a file, an effect and a link — all exists, so this is a job rather than
      a design the day a report earns it. See review §20
- [x] Import with **partial failure as a first-class outcome**. A thousand-row
      file with three bad rows imports 997 and returns the three, with the row
      number and what was wrong — the row number counting the header as row 1,
      which is what the person's editor is showing them
- [x] An import is a command per row under one idempotency key, so a re-upload of
      a corrected file does not duplicate the 997 — `erp_web::importing` folds
      the row's own identity into the file's key, and
      `an_import_takes_the_good_rows_and_reports_the_bad_ones` re-uploads a
      corrected file and counts the events

### 11e · Short links for anything

- [x] A link points at an internal target or an external URL, and anything can
      make one in a line. SMS is billed by length, which is the practical reason
      — `erp_links::shorten`, one call in the caller's own transaction
- [x] Optional expiry, optional single use, and a visit record — the record is a
      count and the two ends rather than a row per hit, which is what a person
      asks for first and what does not grow without a reader
- [x] Infrastructure, not domain (D11) — it holds no business meaning and every
      module may use it. `crates/erp-links`, with its table in the tenant
      migration chain where a rebuild cannot reach it

**Exit:** a booking reminder reaches a customer in Arabic, on SMS, with a link,
having asked the read model for everything it says. **Met to the last hop**:
`BookingReminders` in the worker resolves the audience, renders the template,
shortens the link and promises the effect, and
`a_reminder_says_what_is_true_now_and_reaches_where_somebody_is_now` asserts all
of it. What is not verified is the gateway itself, because this build has an
account with none — see review §15.

---

## Phase 12 — Taking money, and letting other systems in · 5–7 weeks

**The distinction this phase exists for.** The system **records** payments. It
has never **taken** one. Those are different problems: a recorded payment is a
fact somebody asserts, and a taken payment is a conversation with a third party
that can time out halfway. Everything here is the second kind.

It is also the first inbound surface. Every integration so far has been the
system talking; a gateway talks back, and a callback that is trusted without
being verified is somebody else's command executed under your authority.

### 12a · Payments

**Providers chosen 2026-09-03** (review §25): Moyasar for cards, Tabby and
Tamara for buy-now-pay-later. Public HTTP APIs, so no SDK on the dependency
list. **`crates/erp-payments` is the vendor half and Moyasar is built**; see
§25 for what carries over to the other two, and §27 for what the webhook
research changed about 12b.

- [x] A card gateway, and **saved cards** — the token is the gateway's, never a
      card number, and it belongs to a customer rather than to a session. The
      gateway half is `crates/erp-payments` (Moyasar): charge, fetch, capture,
      refund, void, and callback authentication. `Source` has one variant and it
      holds a token, because Moyasar's terms make sending a card number to the
      merchant backend grounds for termination.

      **Saved cards are built**, as a `payments::Card` aggregate keyed on a
      `crm` customer — not a `crm` record, and §30 says why. The token is sealed
      in `module_secret` and **never written to the log**, so "forget my card"
      is a delete rather than a projection that looks away. Charging one is a
      worker pass, not a route: `POST /v1/payments/cards/{card}/charges` answers
      `202` and `payments.settle` sends it
- [x] Buy-now-pay-later, which is **not a card gateway wearing different
      branding**: the provider pays the merchant and collects from the buyer, so
      the receivable is settled by a third party and the entries differ. Getting
      this wrong shows up as a debtor who has already paid. Both clients are
      built (Tabby and Tamara) with their lifecycles read correctly — see §25
      for what `CLOSED` and `approved` actually mean. **And the entries differ**:
      `Settlement::of` sends a card to `1150` and an instalment provider to
      `1160`, so what a lender owes is never mixed with what a card processor
      is holding. `an_instalment_provider_owes_the_money_and_not_the_card_gateway`
      is the test
- [x] Capture is idempotent under retry (L8). A timeout is not a failure — it is
      an unknown, and the resolution is a query against the gateway, never a
      second capture. **`payments.settle` is that caller**: it asks
      `Gateway::fetch` about everything still pending and records what comes
      back, so an answer this system missed is one it goes and gets. The charge
      carries Moyasar's `given_id` so a retried *create* lands on the same
      payment
- [x] Refunds, partial refunds, and what a refund does to a cleared tax invoice.
      ZATCA has an opinion (`tax_sa`) and it is a credit note. **A refund that
      leaves the invoice holding nothing now issues one**, in the same
      transaction, on both refund paths — the gateway one and `sales`' own. The
      pipeline behind it already existed: `tax_sa::documents` builds a credit
      note from `sales.invoice.cancelled`, the VAT return nets it, and the
      signing and submission jobs carry it. Nobody was asking.
      `a_refund_takes_its_supply_out_of_the_vat_return` is the test that says
      why it mattered. **A partial refund still gets no document** — see §31 and
      the partial-credit-note item in 3d
- [~] **Settlement.** A gateway pays out in batches, net of fees, days later. The
      reconciliation is: this payout equals these payments minus this fee — and
      it posts to `ledger`. **Built**, and it is *not* the Phase 8 bank-statement
      machinery pointed at a different source, because that machinery does not
      exist — nothing in this system has ever seen a bank statement (§10a says
      so). What is built is the arithmetic and the accounts; where a settlement
      report comes from is still a person or a spreadsheet. See §29
- [x] Fees are an expense, not a smaller revenue. A tenant that nets them cannot
      answer what it actually sold — and the VAT return it files is wrong, which
      is the half that costs money. `5400 Payment processing fees` is an
      `Expense` in **every** chart template, guarded by
      `every_chart_can_settle_a_gateway_payment`, and `entry_for_fee` is what
      posts to it when a payout settles

### 12b · Inbound webhooks

- [x] Signature verified before the body is read. An unverified callback is not
      a slow path, it is a refused one — HMAC-SHA256 over `<timestamp>.<body>`,
      written here and checked against RFC 4231's published vectors including
      the key-longer-than-a-block case
- [x] Delivered more than once, out of order, and replayed by an attacker who
      kept a copy — so a webhook is a **command with the provider's id as its
      idempotency key**, and arriving twice does nothing twice. The timestamp is
      *inside* the signature, so a kept copy cannot be re-sent with a fresh one
- [x] Accepted fast, processed as an effect. A provider that times out retries,
      and a retry storm is self-inflicted — `202` after the row and the promise
      commit together
- [x] Providers that go quiet: reconcile by polling what the provider says it
      sent. A payment confirmed by a webhook nobody received is money the tenant
      cannot see. **`payments.settle` is that poll**: it asks `Gateway::fetch`
      about every payment still pending and records what comes back, so a
      callback is a doorbell and never the mechanism — which is also the only
      shape that works, since a webhook handler is handed no database
      connection. See §28

### 12c · API keys, in pairs

- [x] A **public key** identifies and is safe in a mobile app or a browser. A
      **private key** authenticates, is shown once, and is stored hashed — the
      same posture as a password. **Digested rather than Argon2**, for the
      reason `session` already gives about its own tokens; see review §21
- [x] Scopes per key, so an integration that reads bookings cannot post journal
      entries — and they **narrow only**, so a key given the owner's role by
      mistake still cannot
- [x] Rotation with an overlap window, because a key that cannot be rotated
      without downtime is a key nobody rotates — seven days by default, and zero
      is legitimate for a key that is known to have leaked
- [x] Rate limits per key, and this is the primitive
      [item 5](#5-signup-is-public-unlimited-and-creates-a-database) has been
      waiting for — the same `erp_web::rate::Limiter` the public surface uses,
      keyed by the public key

### 12d · API version compatibility

Not app version gating. A client — mobile, web, or somebody else's system — was
**compiled against a stated API version**, and the server decides whether it can
still be served.

- [x] A request declares the version it was built against. The server publishes
      the range it supports and refuses outside it, naming the version to build
      against — the same shape as `MIGRATION_FLOOR` refusing an old tenant, and
      the same reasoning as D17's two majors. `x-api-version` in,
      `x-api-current` and `x-api-minimum` out, on **every** response including
      refusals
- [x] The refusal is a typed error a client can act on, not a 500 —
      `request.api_version_too_old` / `_too_new`, with the numbers in `args`
- [x] A version inside the range but behind is served, and says so in a header.
      Deprecation that arrives as a surprise is an outage. **There has only ever
      been one contract**, so the mechanism is unit-tested at versions this build
      does not have rather than only at `1..=1` — a range nobody has exercised
      is a range that does not work

### 12e · Signing in without a password

- [x] One-time codes over SMS, for a market where a phone number is the identity
      and an email address often is not. The text is promised in the same
      transaction as the code (D9), on the **same effect kind** `messaging`
      uses — so one handler answers for a sign-in code and a booking reminder
- [x] Two rate limiters, not one: requesting a code and verifying a code fail
      differently and must be limited separately. A cooldown per number against
      somebody using this to send texts, and attempts per code against guessing.
      Both in the database, so they hold across pods
- [x] A code is single use, short lived, and constant-time compared — claimed in
      **one statement**, so two requests racing with the same code resolve to
      one. `0013_one_time_codes.sql` says plainly what the stored digest is and
      is not worth for six digits
- [x] Cookie sessions for a browser and bearer tokens for everything else, over
      one session model — two authentication surfaces, one authorization answer.
      `HttpOnly; SameSite=Strict; Secure`, and the bearer wins when both are
      sent

**Exit: met.** A customer pays with a saved card — `a_saved_card_is_charged_by_the_worker_and_settles`
runs the whole loop with no browser and no callback in it. The webhook confirms
it once however often it arrives (12b). The payout reconciles to the ledger
(§29). And an outdated client is told what to build against (12d), over the keys
and passwordless sign-in it all runs on (12c, 12e).

**What is left in the phase is not the mechanism.** A refund against a cleared
tax invoice still owes ZATCA a credit note, and where a settlement report comes
from is still a person with a spreadsheet (§29). Both are named above; neither
is a gap in the money path.

---

## Phase 13 — Real time · 3–4 weeks

**This is a requirement, not a refinement.** A customer books from a phone and
the schedule on every counter screen must show it, without anybody refreshing.
Two people looking at the same grid, one of them holding a phone, is how a slot
gets sold twice — and while Phase 7's guard refuses the second write, a screen
that still showed the slot as free has already cost a conversation.

Every read in this system is a poll today. `pg_notify` was refused by D4 and
stays refused; this is the mechanism that replaces the polling it would have
optimised.

### 13a · The event stream

Designed already, not built. The shape matters more than the transport, and
three parts of it are not obvious.

- [x] Server-sent events over the tenant's own log. Not WebSockets: the traffic
      is one-directional, and SSE reconnects by itself
- [x] **It carries a signal, not the data.** *"Group `booking` is queryable
      through position N."* The client re-fetches through the ordinary API, which
      already does authorization, localization and paging. A payload stream would
      need all of that again, in a second dialect — and would make the log a
      query engine, which L7 forbids
- [x] **Published when the projection advances, never when the event is
      appended.** An event is committed and visible before its projection has
      applied it; signal on the append and the client re-fetches, reads a lagging
      read model, sees nothing new, and stops. The hook is the `Advanced` arm
      after its commit, because that is the moment the guarantee becomes true
- [x] **A stream holds no database connection.** Fan-out is the Redis channel
      `shared.rs` already uses for cache agreement. A per-stream poll would
      multiply connection demand by open browser tabs, against a budget sized in
      `pools.rs` for tenants rather than tabs
- [x] Opening a stream calls `request_visit`. A quiet tenant has backed off to a
      six-hour interval, and a stream onto a dormant tenant is silent until
      somebody gives up
- [x] Streams are capped and reconnect. A stream held for hours outlives the
      authorization checked when it opened, and reconnection re-runs the
      extractor for free

### 13b · The live grid

- [x] A booking made anywhere reaches every screen watching that branch and day
      — and the phone that booked, on its own stream (§46)
- [x] Filtered by what the watcher may see — a signal naming a group a viewer has
      no module for is not sent
- [x] The grid reconciles on reconnect rather than trusting a delta it may have
      missed. `?consistent_after=` already expresses "wait for at least this"

### 13c · Notifications inside the system

- [x] A notification is a **durable record first** and a live signal second. One
      that only existed on a socket did not happen for whoever was at lunch
- [x] Read state per person, and it survives a rebuild — so it is a projection of
      an event, not a flag set on a row
- [x] The same audiences as Phase 11b: the client, the employee, the manager, an
      operator. One audience model, four channels — in-system, email, SMS, push —
      and a preference per person. **The client is the exception and had to be**:
      an inbox belongs to a login and a customer has none (§47)

### 13d · Conversations

- [x] A thread against a subject: a booking, an invoice, a customer. Not a chat
      room, which nobody can find afterwards. **Its id is derived from its
      subject**, so opening one is a computation rather than a search (§48)
- [x] Inbound messages (Phase 12b) land in the thread, so a customer replying to
      a reminder is answering a person and not a void — correlated against what
      was last said to that number **as of the moment they replied**
- [x] Internal notes and customer-visible messages in one thread, distinguished —
      the private-note distinction the system read for Phase 7 found necessary
      enough to build twice

**Exit:** two browsers and a phone agree about a schedule within a second of a
booking, and nobody polled.

---

## Out of band — identity moves to `Idempotency-Key`

**Not a phase. A defect found by inspection, fixed before release.**

The API took each record's identity from an `id` in the request body, and that
identifier was doing two jobs: naming the record, and telling a retry from a new
request. It did the second badly. `INV-0001` chosen at one till collides with
`INV-0001` chosen at another, and **five creates resolved that collision by
ignoring the second write and returning success** — losing a document and
reporting it saved. The worst of them, `sales::issue_invoice`, handed the second
till the *first* invoice's statutory number.

- [x] **`erp_eventlog::try_create`**, beside `try_execute`. Empty stream →
      create. Taken, and the request's fingerprint matches → a retry: nothing is
      written and the original is reported. Taken, and it differs →
      `ExecuteError::AlreadyExists`, a 409. The rule is the kernel's, so no
      module writes it and none can forget it
- [x] **The fingerprint travels in `Metadata`**, which every command already
      takes, so **no command signature mentions it**. A create is written
      exactly as before and gains the rule by calling `try_create`
- [x] **`erp_web::IdempotencyKey`**, required on every create and refused unless
      it parses as a UUID. It **is** the aggregate id, so deduplication falls out
      of the log's own `UNIQUE (stream_domain, stream_id, sequence)`: no keys
      table, no TTL, no sweeper, no Redis, and idempotency that is permanent
      rather than lasting a day
- [x] `id` is gone from every create body. The identity a human reads is the
      server's — an invoice `number` from a gapless statutory series
- [x] `TenantDb::create`, the retry loop for creates, mirroring `execute`
- [x] The five silent no-ops replaced: `sales::issue_invoice`,
      `purchases::record_bill`, `ledger::post_entry`, `booking::declare_resource`
      and `booking::reserve`
- [x] `CrmError::AlreadyExists` and `PrepaidError::AlreadyGranted` /
      `AlreadyStarted` / `AlreadyOpen` deleted. Four modules had written their
      own version of one rule; now none has

**Two identities keep their own names, deliberately.** An account code and a
bookable resource are named by the business and referenced by that name — you
book `chair-1`, you post to `4000` — so those creates keep the id in the body
and use the key only as the fingerprint. The rule still applies: a *different*
resource claiming a taken name is refused rather than swallowed.

**A consequence worth knowing.** A list paginated on `(timestamp, id)` used to
tie-break in creation order, because clients numbered their own keys
sequentially. Ids are UUIDs now, so rows sharing a timestamp come back in an
order that is stable but arbitrary. The cursor's actual guarantee — no row
skipped, none repeated — is unchanged, and
`a_list_longer_than_one_page_can_be_read_to_the_end` asserts that rather than the
sequence it used to assert. If invoice lists should sort by document number
within a day, that is a separate change to `sales`, and a defensible one.

This reverses the L8 note in `docs/ARCHITECTURE.md` §3, which argued a header
"buys nothing without a store of keys and prior responses beside it". Right about
the store, wrong about the conclusion: the store is unnecessary once the key is
the stream id.

---

## Phase 15 — `modules/pos` — the counter · 4–5 weeks

**The segment that needs no calendar.** A coffee shop, a restaurant and a retail
shop never take a booking, and until this exists there is nothing to sell them.
It is also the cheapest phase to be confident about, because the hard half is
already built: every till transaction is a ZATCA **simplified** invoice, and
this system builds, hashes, chains, signs and reports those today.

- [x] **`Sale` is not an aggregate, and that is the phase's one real decision.**
      A till transaction *is* a ZATCA simplified invoice, and `sales` already
      builds, numbers, hashes, chains, signs and reports one. A second document
      model here would duplicate VAT, discounts, numbering and the ZATCA chain —
      and give revenue two sources of truth, so the VAT return and the till
      report could disagree with nobody able to say which was right.

      So `pos` **composes**: `sell` writes the shift's event, `sales::issue_in`
      and `sales::pay_in` in **one transaction**, the same seam `sales` itself
      uses on `ledger`. The two functions were already there and private;
      making them public was the whole of the change to `sales`
- [x] `Shift` — open, sell, pay out, count, close. The cash-drawer domain: an
      opening float, takings by tender, a declared count, and **the variance**,
      which is the number a manager actually reads and the only one this module
      posts
- [x] A sale is a simplified invoice unless the buyer gives a VAT number. Not
      re-decided here: it is `sales`' rule, reached by passing the customer
      through, which is what composing buys
- [x] **Returns and refunds — the prerequisite was built, then this was.**
      `sales::cancel_invoice` refused an invoice that had payments, and **every
      till sale is paid the instant it happens**, so no till sale could be
      credited through any route.

      The refusal was not wrong so much as too blunt. What a credit note may not
      do is undo a supply while the business keeps the cash — so `sales` gained
      a **refund** (`Refunded`, `refund_invoice`, `refund_in`,
      `entry_for_refund`), and the rule became *"nothing is still held"*:
      `Invoice::held()` is paid less refunded, and a credit note needs it at
      zero. `pos::take_back` then hands the money back and credits the document
      in one transaction, which is the only order in which the books are never
      briefly wrong.

      A refund projects as a **negative** row in `invoice_payment` rather than a
      table of its own, so `paid` stays one sum and no read has to remember to
      consult a second place before saying what an invoice is holding
- [x] **Offline is deliberately out of scope.** Unchanged, and the reason is
      unchanged: a till that queues sales locally is a second write path with
      its own ordering problem, and L1 is not negotiable

**Two more divergences, and the reasons.**

- **The float does not post.** Cash moved from a safe to a drawer is still
  `1000 Cash on hand`, so the business is no richer and there is no entry. It
  follows that a shift's `expected` — what the drawer should physically hold — is
  a *larger* number than what the shift added to the ledger, and that the two
  answer different questions. The variance is what reconciles them
- **`5910 Cash over and short` is new in every chart**, for the reason `2400`
  was added in Phase 14: a till that records a shortage and cannot post it
  leaves the books saying the drawer holds what it does not, for ever

**Two weak tests, found by falsification and not by review.** Both passed
against code that was wrong, which is the failure mode a test suite is worst at
noticing about itself.

- The drawer rule was written **twice**: the aggregate matched on `Method::Cash`
  and the projection asked `is_in_the_drawer`. Making every card sale count into
  the drawer left every test green, because the aggregate never consulted the
  rule being broken. `Takings::in_the_drawer` is now the one place it is applied
- The variance test closed one till short and one over by the same amount and
  asserted the expense account netted to zero — which is also what posting
  *neither* looks like, and what posting them backwards looks like. It now
  asserts the shortage on its own before the overage exists

Seven mutations after those fixes, seven caught: a card in the drawer, a
variance unposted, a variance inverted, tenders that need not match the sale, a
retried sale ringing twice, a shut till still selling, and a pay-out counted
twice.

**Exit: met.** `a_cafe_opens_sells_and_closes_level` opens a shift, rings forty
coffees, checks all forty statutory numbers are distinct, closes level, and
asserts the drawer, revenue and VAT payable in the ledger — which `pos` never
posted, because `sales` did.

---

## Phase 16 — Branches · 2–3 weeks

**They do not exist.** The only `branch` in this codebase is a free-text string
on the ZATCA EGS certificate. There is no entity, no scoping, no reporting
dimension. Every competitor meters them (Qoyod charges SAR 40 each, Rekaz caps
them at five even on its top tier), which makes unlimited branches a real
differentiator and means the concept has to exist first.

- [x] `Branch` — a place, with an address. `modules/branches`, a **leaf that
      depends on nothing**, which is what makes it safe for everything else to
      sit on. Opening, amending, closing and reopening are events, because a
      dimension edited in place rewrites history: a report for Olaya run in
      March and again in June would differ with nothing able to say why
- [x] **A dimension on every document, from one mechanism.** The branch travels
      in `Metadata`, folded in by `erp_web::Allowed` from an `X-Branch` header —
      so *every* event a request produces carries it, and no module threads a
      field through. `ledger`'s `posting` table reads it off the envelope, and
      `branch_balances` splits the chart by it
- [x] **Validated once**, in `ledger::post_entry_in`. Every posting in the
      system arrives there — it is already where a closed period is enforced —
      so one check covers `sales`, `purchases`, `prepaid` and `pos` without any
      of them repeating it
- [x] **Opening hours — decided against.** Nothing would read them.
      `booking` already keeps availability per *resource*, which is finer than a
      branch and is what a diary needs; branch hours are something the booking
      site would *display*, and that site is a separate React project reading
      this API — Phase 17. A rule nobody applies is wrong by the time somebody
      applies it
- [x] **Resources belong to a branch**, which is what makes "book at Olaya"
      work. `ResourceEvent::Declared` carries one — **set once, like `kind`**,
      because a resource that changed branch would retroactively re-attribute
      every booking it ever held to a place it was not at. If a chair physically
      moves, declaring a new one is the honest record.

      Checked at declaration against the `branches` log, and **not** inherited
      from `post_entry_in` like every other branch reference, because declaring
      a resource posts nothing and so has no journal entry to carry the check.
      The rota narrows to the caller's `X-Branch` and `?branch=` overrides it —
      a default, not a wall, for the reason §9d gives
- [ ] A person scoped to one. Not built, but **the seam is placed**:
      `Allowed::branch` sits beside the capability check, which is where a
      person's scope would be enforced, and the doc comment says so
- [ ] ZATCA per-branch EGS units. Not built, and unchanged: `taxpayer_id()` is
      still one stream per tenant

**A footgun found by closing the gap, and removed.** `sales::issue_in` took the
journal entry's id as an argument, and `cancel_in` reverses that entry by
*rebuilding the same name*. So a caller that chose a different one — `pos` did,
reasonably, using its own prefix — issued an invoice that could never be
credited. The name is now derived inside `sales` by `issue_entry` and
`money_entry`, and is no longer a parameter anybody can get wrong. It was
invisible until a second caller existed, which is the argument for having one.

**Two decisions worth keeping.**

- **A per-branch trial balance does not have to balance, and nothing here
  pretends it does.** Debits equal credits per *currency* — that is the
  invariant `ledger` asserts and it is untouched. Moving cash between branches
  debits one and credits the other, so each side is out by the transfer until
  inter-branch clearing accounts exist. What this phase delivers is that each
  branch can be **reported** separately and that the branches are a *partition*
  of the whole. Claiming more would report a normal transfer as a broken ledger
- **A fourth `Address`.** `crm`, `sales` and `tax_sa` each already define one.
  Collapsing them into `erp-types` is worth doing and was not done here, because
  the three are event schemas that are equal by coincidence rather than by rule —
  ZATCA adding a field to the invoice one is what would separate them again. The
  duplication is named in `branches::Address` rather than left unexplained

**One guard had to be relaxed, and it was right to check.**
`every_modules_routes_live_under_its_own_name` required every path to start
`/v1/{module}/`. `branches` is the first module whose resource *is* its name —
`/v1/branches`, with nothing after it — which `module_of` already scopes
correctly. The guard now accepts the module root as well as paths beneath it.

**Exit: met.** `two_branches_report_separately_and_sum_to_one_trial_balance`
opens two branches, rings two tills at them through `pos`, and asserts each
branch's revenue on its own, that the per-branch rows sum to the unsplit chart,
and that the trial balance still balances.

---

## Phase 17 — The public booking API · 3–4 weeks

**The site is a separate project.** React and shadcn, its own repository, its own
deployment, talking to this backend over HTTP. So this phase builds **no pages**:
what it owes that project is an unauthenticated API surface, the security around
it, and a contract it can be built against.

That is a smaller phase than "the booking site" was, and a sharper one. The
pages, the Arabic and English rendering, the embed snippet and the Instagram-bio
link all leave this repository. What arrives in their place is the thing a
same-origin server-rendered site would never have needed.

- [x] **The surface itself.** `erp_web::Public` is the first thing in this build
      to open a tenant with **no person behind it**, and what makes it safe is
      construction rather than care: the handle carries no access, so
      `TenantDb::role()` is `None` and every capability check refuses it. A
      public handler cannot reach a guarded command by forgetting something — it
      would have to call a module function directly, which is a visible line
      rather than a missing one.

      It is also the first caller of **`Lane::Client`**, which has existed since
      Phase 1 and been used only by tests. "A tenant's customers, through their
      app or website. The flood" is what the lane was written for, and using it
      is what stops a bot on a booking form starving the counter staff serving
      people in the shop.
- [x] **Three public routes, deliberately narrower than their authenticated
      counterparts.** `services` never shows a withdrawn resource and never its
      capacity; `availability` answers one number; `reservations` takes a
      booking with **no price and no customer id on it** — a stranger choosing
      their own rate, or naming which of a business's customer records they are,
      are both things the counter's shapes allow and this one must not.
- [x] **CORS, which did not exist here at all.** Allowed origins are per tenant
      and checked through the control plane's entry cache — one staleness story,
      not two. Written here rather than configured from `tower-http` because
      that layer decides an origin with a *synchronous* predicate and this
      answer is an `await`; feeding a sync predicate would need a second cache
      refreshed on its own schedule.

      Never a wildcard, never credentials, and **never a suffix match** — a
      tenant that allows `https://salon.com` must not admit
      `https://salon.com.attacker.example`. Verified by falsification: writing
      the check as `ends_with` fails the test.
- [x] **Tenant resolution: settled as recommended.** The site calls
      `salon.erp.com`, so the subdomain stays the single source of tenant
      identity and `extract.rs`'s safety argument stays true unchanged. CORS
      does the cross-origin work, which is what it is for. `tenant_label` is now
      shared between the extractor and the middleware, because two
      implementations of "which tenant is this host" is how one of them comes to
      admit `a.b.acme.erp.com`.
- [x] **Domain verification**, and custom domains otherwise gone as planned. A
      domain is claimed and proved; only a proved domain licenses origins, so
      adding `https://www.salon.com` after `https://salon.com` is a row and not
      a second proof. The verification token is minted **inside the control
      plane** so no caller can choose a predictable one — the same lesson
      `sales` learned about the journal entry id.

      **What proves it is not built**, and the module says so: reaching out to
      DNS or a well-known URL is an outbound call, an outbound call is an outbox
      effect (D9), and that handler does not exist. `POST .../verification` is
      an operator recording that the check was made by hand, audited as
      `tenant.domain_verified`.
- [x] **Rate limiting, which stopped being deferrable.** Two fixed windows per
      node: per (business, origin) and per business. Charged in the extractor
      rather than in each handler, so a public route added tomorrow is bounded
      without anybody remembering to bound it — and charged *after* the tenant
      resolves, so a flood aimed at names that do not exist cannot consume a
      real tenant's budget.

      **Honestly per node**, and the numbers are chosen knowing it. Fleet-wide
      means Redis on the request path: failing open would be exactly the
      degradation L6 refuses, and failing closed makes a cache outage an outage.
      Per-node is the honest third answer, and the sharper key — a thing the
      caller *holds* rather than asserts — is Phase 12c's API key.
- [~] **Deposits at booking.** The money-shaped half is built: a gateway
      payment can collect against a **booking** rather than an invoice, and a
      settled one is a liability (`2410`) rather than revenue or a cleared
      receivable — see §33. A saved card can be charged for one, and giving one
      back clears the liability and issues no credit note.

      **And the booking-shaped half is built** (§39): a public booking records
      what holding the slot costs, a public route creates the charge the
      customer pays in their own browser, the worker asks the gateway whether
      they have and tells the diary, and a hold nobody paid for lapses. The
      document is a **386 prepayment invoice** raised when the money is real.

      A **verified phone** is a setting the business decides (§40), off by
      default because the deposit is what stops abuse and a verified number is
      about being able to reach somebody. The 388 with the advance deducted,
      when the service is finally delivered, is still ahead.

      **Keeping one is built and is the business's call** — a sale by default,
      `payments.retention` to say otherwise. The tax is not what it decides:
      that was settled when the deposit was billed. See §34 and §35.

      **And the loop now runs over HTTP, through a lender as well as a card**
      (§43). Writing the end-to-end test found the public path had never been
      priced — a public line carried no charge, so no deposit was ever asked
      for through the site — and that neither buy-now-pay-later provider could
      be reached at all. A bookable carries a published rate; the deposit
      route takes a provider, and for a lender the email, phone and landing
      pages it needs; the worker opens the checkout and captures what the
      customer authorises; and `GET` on the same path hands the waiting
      customer the page. `a_public_deposit_is_paid_through_a_lender_at_the_published_price`
      is the test.

      **The 388 with the advance deducted is built** (§44): the desk raises it
      at `POST /v1/booking/reservations/{reservation}/invoice`, or the worker
      does on completion when `PUT /v1/booking/billing` asks; either way one
      invoice, showing the whole supply, naming the prepayment invoice and
      charging the rest. **And a partial refund of a deposit gets its credit
      note** (§35's next thing): a single-band invoice has one honest net for
      what went back, and `credit_what_is_clear` issues it.

      **Left by decision:** blacklisting a customer who repeatedly books and
      never pays waits for customer accounts, because a public booking has no
      record to bar (§42).
- [x] **`docs/openapi.json` as a contract.** `docs/openapi.baseline.json` is what
      clients may rely on, and `tests/compatibility.rs` fails on a change that
      would break one: an operation that disappears or is renamed, a required
      request field that appears, a response field that vanishes, a path that
      gains a parameter. Accepting a break takes `just baseline`, which is the
      point — a break somebody typed a command to accept is a break somebody
      knows about.

      It checks four shapes and **not** type narrowing, enum members or
      `format`; a full structural diff is a much larger piece of work, and what
      is here catches the ones a normal refactor causes by accident.

      **The first version of it was useless and the falsification is what said
      so.** It compared only top-level response properties — and almost every
      list here answers `Paged<T>`, whose top level is `items` and `next`, so
      renaming `ServiceView::name` sailed straight through. It walks three
      levels now, bounded and cycle-guarded, and the same rename is reported as
      `public_services no longer returns items.name`.

**One decision worth keeping.** Online booking is **off until a business turns
it on**, and the absence of the setting is a no. The two public *reads* are safe
by their nature — a shop's front page is what they are. A public **write** claims
a real slot in a real diary, and a salon that never asked for online booking must
not find their week full of appointments nobody intends to keep. The rate limiter
bounds how fast that can happen; it does not make it something the business
agreed to.

Refusing is a **404 and not a 403**, because "forbidden" would confirm the route
would work for somebody else, which is neither true nor the caller's business.

---

## Phase 18 — Marketing · 3–4 weeks

**Audited 2026-09-09: all five boxes are genuinely open.** Nothing here exists,
and the one thing that looks like it does is a false positive worth naming —
`segments` in `modules/messaging/src/budget.rs` is an **SMS billing segment**,
the unit a long message is charged in, and has nothing to do with a marketing
segment. There is no analytics or tracking-pixel configuration of any kind.


- [ ] **Segments**, and the architectural constraint that shapes them: a segment
      like *"booked in the last 90 days and spent over 5,000"* spans booking and
      sales, and **L3 forbids reading across projection groups**. So marketing
      subscribes to the log and maintains its own group, exactly as Phase 10
      specifies for reports. That is what stops the campaign list disagreeing
      with the invoice list
- [ ] **Campaigns** — a segment, a template, a channel and a schedule. Every one
      of those exists after Phase 11; a campaign is the thing that composes them
- [ ] Tracking pixels: Meta, TikTok, Snapchat, Google Ads, GTM, Analytics,
      Clarity. **Client-side, and the client is the React project now** — so
      what this repository owes is the configuration: which ids a tenant has set,
      readable by the site. The pixels themselves are not built here
- [ ] Reviews, and the request that asks for one after a visit
- [ ] Abandoned bookings, which is the retargeting case that actually pays

---

## Phase 19 — `modules/inventory` · 3–4 weeks

**Audited 2026-09-09: all four boxes are genuinely open.** The only hits in the
tree are chart-of-accounts entries — `1300 Inventory` and `Cost of goods sold`
in the **retail** chart, which anticipate this module without implementing any
of it. That the accounts are already there is convenient and is not progress.

**2026-09-13: the first box is closed and the other three are not.** §71 built
the module — products, lots, movements, counts and the read model behind them —
and deliberately posts nothing and depletes nothing. Both accounts are now in
all three shipped charts, which was the thing blocking the first posting rather
than progress towards it.

**The product owner's revision landed the same day**: costing is **per lot**,
not weighted average, and expiry and serial tracking are in scope. A lot answers
*which delivery is this*, which is the whole of expiry and the whole of a
recall; an average cannot.

**2026-09-13, later: two of the four boxes are closed and two are half.** §72
booked everything that leaves a shelf — a count's discrepancy, a write-off's
loss and a document's cost of goods sold — each in the transaction that writes
the movement, and added the invariant that proves the inventory account still
agrees with what the shelves are worth. The two open boxes are one piece of
work: **nothing calls `consume_in`**, so no sale depletes a shelf and no cost of
goods sold is ever posted. Both wait on the hook in `sales::issue_in`.

**2026-09-13, later still: receiving posts** (§73, decision R3 superseding
decision 8). A delivery debits `1300` and credits a new `2010 Goods received,
not invoiced`; the supplier's bill line that names a stocked product debits
`2010` back instead of the account it carries. Stock is on the balance sheet
from the moment it lands, and the value-on-hand check stops calling the ordinary
gap between a delivery and its invoice a violation. It closes no box on its own
— the two open ones are still the `sales::issue_in` hook — and it makes the
fourth one honest: the account those postings are checked against is now written
by this module at both ends.

**And its review re-opened the counting box.** Revision R2 says a count takes a
shelf, with the shortage allocated through the picking rule; only a per-lot count
exists, and the module's prose was still arguing that no such rule had been
agreed. The prose is corrected and the box is open again — **one box closed,
three open**. Nothing about counting changed in the code.

**2026-09-13, last: the invoice hook lands** (§74), and with it boxes 2 and 4.
`sales::issue_in` calls `inventory::consume_in` for every line that names a
product, in the invoice's own transaction and with the products sorted by id, so
a till sale, a booking bill and a `/v1/sales` invoice all deplete through one
path and cost of goods sold is posted lot by lot as the goods leave. A
lot- or serial-tracked product that the shelf cannot cover **refuses and takes
the document with it** (R1); a plain one sells and records the shortfall. A
credit note's returned line goes back onto the lots it left, at what the
consumption froze — read out of the shelf's own stream, because the read model
may not be consulted (L3) and a cost may not be guessed (L6). The ZATCA document
carries the real quantity and unit price, which BT-131 had been getting away
without. **Three boxes closed, one open**: the shelf-wide count.

**2026-09-13, and the last box: a count counts the shelf** (§75, revision R2).
The counter enters what is there; a shortage comes off the lots through `pick`,
the order a sale takes stock in, and an overage joins the newest lot at that
lot's own cost. A serial-tracked product is counted by naming the units found,
and a name that is not on hand is refused. A count of the shelf is also what
clears a plain product's debt, which §74 left for it. Counting one lot still
works. **All four boxes closed.**

**2026-09-13, and what the boxes left open** (§76). A sales or till line may
name the lot it takes from; a credit note or a till return of serial-tracked
stock names the units that came back, and each has to be still out on that sale
— followed through the stream, not read off the shelf; the worker warns about
lots past or near their date and changes nothing; and the module has its book
page.

- [x] **Products, quantities, and stock movements as events.** Built 2026-09-13
      (§71): `modules/inventory`, two aggregates and three events, **per-lot
      costing behind one pure picking rule** (earliest expiry first, undated
      last, oldest received among equals), expiry and serials, stock per product
      per branch, and a read model whose movements sum to what is on hand **lot
      by lot**
- [x] **Consumption on sale, so a POS line depletes stock.** Built 2026-09-13
      (§74): the hook is in `sales::issue_in`, so every invoice this system
      issues depletes through one path — the till, the booking bill, the
      `/v1/sales` route. Products in a fixed order (decision 14), the refusal
      after the already-heard check, R1's split between a tracked product that
      refuses and a plain one that records a shortfall, and a credit note that
      puts back what the client says came back, onto the lots it left and at
      what the consumption froze (decision 12). What the next receipt does to
      the units a shelf owes is decided and it is **nothing**: the debt was
      costed at a guess, so a count settles it rather than a delivery
- [x] **Counts and the discrepancy a count finds, which is the number that
      matters.** Built 2026-09-13 (§72, §75): short books the loss against
      inventory, over books the reverse, and a count that moved no value posts
      nothing. Since §75 a count takes a **shelf** (R2) — a shortage off the lots
      in picking order at each lot's cost, an overage onto the newest lot at its
      cost, a serial-tracked count naming the units found — and clears what a
      plain product sold short owes. Counting one named lot still works, for
      someone counting batches. *(As it stood after §73's review: half done, one
      lot per count and no serial count at all.)*
- [x] **Cost of goods sold, posted to `ledger`.** Built 2026-09-13 (§72, §73,
      §74): valued lot by lot, with a write-off's loss beside it under its own
      account, and now **triggered** — by the invoice that sold the goods, in
      that invoice's transaction. A plain product sold below zero books the
      shortfall too, at the last unit cost the shelf saw, so the goods that left
      the building are in the books whatever the count says; a credit note
      reverses the entry at what the consumption froze. The check that the
      inventory account and the shelves still agree is a comparison of like with
      like at both ends

---

## Phase 20 — Property · the vertical, in four prerequisites and a module

**Decided 2026-09-09 (§49): a new vertical for this product.** §49 is the
assessment and is not repeated here; this is only the order, and the order is
what the assessment was for.

Read the two halves separately. **20a–20e are lettings**, and they ship a
product: a landlord or an agency that collects rent. **20f–20g are ownership**,
and they need a module that does not exist in any phase. Nothing in the first
half depends on the second, which is the point of splitting them.

### 20a · The party model *(a design question, and it comes first)*

- [ ] Roles and relationships between parties, so one person is the **owner** of
      unit A, the **tenant** of unit B and the **guarantor** on unit C at once.
      `CustomerKind` is `Person | Company` today (`modules/crm/src/customer.rs:31`)
      and there is no relationship at all
- [x] **Not custom fields — a decided constraint on the party model, not work.**
      `FieldKind` is `Text | Number | Date | Choice | Flag`
      (`modules/crm/src/fields.rs:60`) with no reference type, and adding one
      would make a typed-and-erasable field into a foreign key that erasure
      cannot honour
- [ ] Decide whether a role is a `crm` concept or a property one. A guarantor is
      property-specific; an owner is not

**This is the one piece that is a design question rather than a build task.**
Everything below assumes an answer to it.

### 20b · The three enums opened from below

- [ ] `messaging::Topic` — a `Lease` variant (`modules/messaging/src/audience.rs:33`)
- [ ] `files::OwnerKind` — a `Lease` variant, plus its domain name, which
      `every_owner_kind_names_the_domain_its_module_uses` pins
- [ ] `notifications::Kind` — rent due, rent late, lease expiring, each with
      compiled Arabic and English copy and its ordered `audiences()`

Mechanical, and each has a test that fails if the edit is half-done. Listed
because they are edits to modules *below* the one being added, which is the
opposite direction from every module so far.

### 20c · Per-line VAT exemption reasons *(already an open box in 4e)*

- [x] **Built — and the box was already stale when it was written.** §49 was
      right that this gates the vertical: residential rent is VAT-exempt and a
      ZATCA invoice for an exempt supply must carry a reason code. It landed as
      §50: the codes are **tenant configuration** (`ledger::vat::Rates::reason`)
      rather than a constant, because declaring every exempt line
      `VATEX-SA-29` — financial services — would be a false statement to a tax
      authority on a landlord's behalf. `SalesError::NoExemptionReason` refuses
      rather than guessing (L6), and the reason travels to the ZATCA document
      through `tax_sa::documents::reason_of`

### 20d · Recurring invoicing

- [ ] A schedule that raises an invoice — "the 1st of every month for 12
      months". Nothing in `sales` or `payments` schedules one today.

      **Two constraints, decided, and not separate work.** The ids are derived
      over (schedule, period), so the producer is a scan re-running over an
      overlapping window with no cursor — the shape §47 and §48 both landed on,
      and which `BillCompletedBookings` (`worker.rs:752`) already implements for
      the identical problem, so it is the template rather than a new pattern.
      And **not `erp-recurrence`**, which is 432 lines of weekly booking
      patterns that the crate name flatters
- [x] ~~Where it lives is a decision: `sales` owns invoices, but `prepaid`
      subscriptions want it too~~ **Decided 2026-09-14: in `sales`**, beside the
      other invoices

### 20e · `modules/property` — units, leases, rent

- [x] A **unit** is a `booking::Resource` of kind `Place`, not its own aggregate —
      **decided 2026-09-14**. §49 found the engine does not care what it holds
      (`modules/booking/src/resource.rs:29`), so this was a choice, not a constraint
- [ ] A **lease**: parties, term, rent, escalation, break clause, renewal,
      termination
- [ ] A **security deposit as a liability** — money held and not earned, returned
      at term end less deductions. **Explicitly not `booking`'s `Deposit`**, which
      is a prepayment against a future invoice (`modules/booking/src/reservation.rs:27`).
      Reusing it would overstate revenue by the deposit balance, and the trial
      balance would still balance
- [ ] **Property as a second posting dimension.** The `extra` bag is generic
      (`crates/erp-eventlog/src/aggregate.rs:500`) so a key is free; the work is
      `proj_ledger.posting`'s dedicated `branch` column and `branch_balance` being
      a single-dimension rollup. **Not** the `X-Branch` header, which carries
      *where the request came from* — head office raises January rent for forty
      units it is not standing in
- [ ] **Arrears**: ageing, overdue, dunning. Log-subscribing with its own group,
      the way `modules/reports` argues for (`modules/reports/src/lib.rs:1`)
- [ ] **Owner statements and disbursements**, which need a payment run —
      `modules/purchases` books supplier bills and nothing pays them
- [ ] Maintenance requests as `conversations` threads, once `Topic` has a variant
- [ ] **Ejar** — the mandatory rental-contract registration. §45's ZATCA
      onboarding is the pattern: a credential, a submission, and a worker that
      finishes it

**Exit for the letting half:** an agency collects rent, chases it, and pays
owners, with a compliant invoice for both a residential and a commercial tenancy.

### 20f · Fixed assets *(its own module, and it blocks the second half)*

- [ ] Capitalisation, depreciation schedules, disposal, gain or loss on sale.
      **None of this exists**: `grep -rniE "depreciat|amorti[sz]|fixed.asset|capitali[sz]"`
      over `modules/` and `crates/` returns three false positives and nothing else
- [ ] Posting to `ledger`, and a health check that carrying value agrees with the
      books, in the shape §10b sets

### 20g · Buying and selling

- [ ] A property as an owned asset, acquired and disposed
- [ ] **RETT at 5%** — not VAT, and there is no non-VAT tax anywhere in the code
- [ ] The pipeline: offer, contract, title transfer, agent commission

**Exit:** a property is bought, held, depreciated and sold, and the books agree.

### What is deliberately not in this phase

Service-charge and CAM reconciliation, utility sub-metering and recharging,
sub-letting, valuation, and mortgage or finance on a property. Each is real and
none is needed to collect rent; they are what turns the vertical into a
competitive product, and they should be sequenced from customers rather than
from this list.

---

## What Phases 7–13 unblock

**Phase 5b finally has its second consumer.** The rule engine was deferred
because authorization alone could not describe it — one consumer means inventing
which facts exist. Booking automations (reminders, no-show handling, recall
follow-ups) and HR document expiry are two more, independent of each other and of
authorization. The engine can be specified from three working cases instead of
guessed at from one.

**And a fourth, which is the one that will shape it.** §9b's claim union is a
rule over facts the engine would have to name anyway — *who reports to whom*,
*which branch*, *is this claim propagating*. It arrives with a concrete question
already asked, which is exactly what the deferral was waiting for.

It also carries the sharpest constraint of the four, and §9c is what sets it. The
claims are checked **inside commands**, not at the edge, so the engine is not on
the hot path of every request — but it is inside a transaction that is holding a
connection, which is a worse place to be slow than it looks. Whatever the engine
evaluates for a claim has to have been settled when the org changed, not when
somebody asked.

**D14's push path finally attaches.** Architecture §1.14 says of
`request_visit` that it "pulls a tenant forward, which is where a push path
attaches when the API can tell a worker directly that a tenant just wrote
something: polling becomes the floor rather than the mechanism, and nothing
downstream changes." `ControlPlane::request_visit` and
`tests/leases.rs` both name the same seam. Phase 13 is that push path, and the sentence
was written to be collected.

**Item 5 gets its primitive.** Item 5 is closed — signup builds nothing until the
address answers — but the piece it deferred is still outstanding, and it was
never a signup-specific one: rate limiting per caller. `REQUEST_INTERVAL` caps
mail per address and cannot do more than that. Phase 12c builds the real one for
API keys, and signup is the second user of it.

~~**Phase 6's open question gets an answer.**~~ **Withdrawn 2026-09-09 — this was
wrong, and it contradicted the architecture.** It argued that a reservation, a
service request, a leave request and a payroll run are "four documents with
genuinely different workflows — which is the evidence §8 asked for".

§8 asks what **tenants** need, and says to decide from customer conversations.
Those four documents are **ours**: compiled Rust aggregates that no tenant
authors and no tenant will. That this product has varied workflows is evidence
about the product, not about the market — and the only configuration request
this build has actually met was for *fields* (§41's custom fields), which is the
answer §8 says would make the generic aggregate **not** worth its cost.

Phase 6 remains blocked on customer conversations, exactly as §8 says.

---

## What needs work now

Written after reading this document against the code, in the order I would do
them. Everything here was **checked**, not remembered — the file and the command
that shows it are named.

### 1. ~~The outbox has no producers and no handlers~~ — done

Was: `grep` for `with_effect|enqueue(` outside `erp-eventlog`'s own tests
returned **nothing**, and `bin/worker.rs` built its dispatcher with no
`.register(...)` after it. Every piece of D9 was finished, tested, and reaching
nothing; the concrete cost was that an invitation was a link somebody copied out
of an API response by hand.

Email is the handler, invitations are the producer, and the shape of the fix was
decided by a fact nobody had noticed: **the outbox only existed in tenant
databases, and invitations are control-plane rows.** See the running note.

The original text follows, because the reasoning in it is what led to the
control-plane outbox rather than to a sweep.

#### The original finding

`grep -rn "with_effect\|enqueue(" crates/ modules/` outside `erp-eventlog`'s own
tests returns **nothing**, and `bin/worker.rs:54` builds
`Dispatcher::new(RetryPolicy::default())` with no `.register(...)` after it.

So: the outbox schema, effects-as-values, claim-under-`SKIP LOCKED`, exponential
backoff, dead letters, the at-least-once idempotency key, the crash tests that
prove a lost delivery record replays with the same key — all built, all tested,
and **nothing in the product uses any of it**. D9 is the architecture's answer to
"how does anything reach the outside world", and the answer currently reaches
nothing.

The concrete cost is one feature short: an invitation is a link the inviter has
to copy and pass on by hand, because sending an email is an outbox effect and no
handler exists. That was the right call when it was written — "belongs with the
first real handler" — and ZATCA turned out **not** to be that handler, because a
submission reads a sealed private key and an effect is a value in a table.

So email is the first real handler, and it is the one that unblocks invitations,
password reset, and every notification after. Until it exists the outbox is the
largest piece of finished, unexercised machinery in the build — and unexercised
machinery is where the next silent bug lives. Two of them have already been found
this way (the ZATCA sweeps had no caller; `ON DELETE SET NULL` on `audit_entry`
was unreachable from the day it was written).

### 2. ~~There is no CI~~ — done, with one job still missing

`.github/workflows/check.yml`: `just check` against Postgres 18.3 and Redis 8 as
service containers, and a second job that runs `just prepare` and fails if
`.sqlx` moved. Verified the way it needed to be — the whole suite against a
**freshly created empty Postgres**, which is what a runner gets and which is the
only way to find setup that lives in a shell history rather than the repository.
705 passed there, identical to local.

Redis is a required service, not an optional one: `shared.rs` refuses rather
than skipping when it is absent (L6), so a runner without it fails four tests
instead of quietly covering less than the badge claims.

**Still missing:** the job D17 actually wants — upgrading a realistic N-1
database on every build. It needs a seeded corpus at the previous major, and
there is no previous major yet (`MIGRATION_FLOOR` is 0 for all of the first).
Build the corpus at the first major release, not before.

The original finding follows.

#### The original finding


No `.github`, no `.gitlab-ci.yml`, nothing. `just check` exists and runs
`fmt-check`, `clippy -D warnings` and `cargo test --workspace`; a person has to
remember to run it.

This document claimed "a required CI check" in two places, which is how a claim
like that survives — it was true of the intent and never of the repository. Both
are corrected above.

What CI has to run, and why each one is not optional: `just check`
and `just openapi` (both regenerate a committed file and fail on drift), and
`just migrate-fleet check` / `versions` against a scratch database (the two
pre-deploy gates). The soak test and the ZATCA sandbox tests are `#[ignore]`d and
need credentials; they belong on a schedule, not on a push.

### 3. ~~Shadow replay covers two groups of four~~ — done

Was: `ledger` and `sales` were replayed, `purchases` and `tax_sa` were not, while
the test's own doc comment said "every group". `tax_sa` was the one that mattered
most — its projection builds the ZATCA hash chain, and a rebuild producing a
different document produces a different hash, breaking a chain **the tax
authority validates**.

All four now, and the list is no longer trusted: the group names replayed are
compared against every group `erp_api::modules()` declares, so a module added
without a line there fails rather than becoming the next one nobody watches.

Each group also names a table the demo must have filled, because `EXCEPT ALL`
between two **empty** tables is clean — a group whose read models happen to be
empty was "reproducible" the way a blank page is correct.

Falsified four ways: dropping `tax_sa` from the list fails the coverage
assertion; dropping a projection from the group empties its witness table; and a
`clock_timestamp()` in either the `tax_sa` or `purchases` insert is caught by the
differ (7 documents and 4 bills respectively).

### 4. ~~Nothing says how to deploy this, or how to get a tenant back~~ — half done

**Getting a tenant back** is done and is a test, not prose:
`crates/erp-control/tests/restore.rs` dumps a tenant, destroys it, restores it,
and compares the log row for row. A second test pins the failure an operator
actually hits — the two planes restored to different points, where *neither*
direction reports an error and one of them silently loses events.
`docs/RUNNING.md` documents the procedure those tests execute.

**Still open:** deployment beyond compose, and Postgres failover. Neither is a
test, and neither should be claimed until it is rehearsed the same way.

The original finding follows.

#### The original finding


The target is 2,000–5,000 tenants self-managed on Hetzner. The repository has no
container image, no unit files, no scheduling for `bin/reaper` or `bin/migrator`
(both of which exit when done and are meant to be scheduled), and no backup or
restore procedure — tested or otherwise.

A database per tenant makes restore *the* operational question, not a footnote:
restoring one tenant to a point in time must not touch the other 4,999, and the
control plane's row for that tenant has to agree with whatever the database
became. Nothing in the code or the docs addresses it, and "we will work it out
when it happens" is a bad plan for the day it happens.

`docs/RUNNING.md` covers running it by hand, which is a different question.

### 5. ~~Signup is public, unlimited, and creates a database~~ — done

Was: one unauthenticated request ran `CREATE DATABASE` and a full migration
chain, so a shell loop from the open internet cost the attacker one HTTP request
and cost the operator a disk.

**Signup is two calls with a mailbox in between now.** `POST /v1/signups` writes
one `pending_signup` row and one outbox effect and answers `202`; nothing else
happens until `POST /v1/signups/{token}`, which is where the account, the tenant,
the database and the session are built. The email is the same producer the
invitation flow uses, which is the other half of why item 1 came first.

The second half of the finding was not in the original text and is the worse of
the two. Signing up wrote an **authenticator** under whatever address was named,
with a password of the attacker's choosing, so signing up as `ceo@bigcorp.example`
locked the real owner out of ever signing up: they would have to prove a password
they never set. Nothing is written to `authenticator` now until the address
answers; the hash waits in `pending_signup` and moves across on confirmation.

Six tests, each falsified by breaking the code it covers:

| what it pins | broken by |
|---|---|
| a request builds no tenant, no database, no authenticator, and hands back no token | returning the token; registering the login early |
| a link works once | unclaiming on success |
| an unissued token is refused | (paired with the above, which proves the route can succeed) |
| one address gets one message a minute | dropping the interval check |
| a name taken meanwhile does not burn the link | dropping the unclaim on failure |
| the mail is written in the language of the form | rendering in the default locale |

**What is deliberately still missing: a rate limit.** `REQUEST_INTERVAL` caps mail
per *address*, which is what stops the new flow being a way to fill one mailbox,
and it is all this endpoint can do alone. Limiting per *caller* needs a notion of
caller that does not exist yet, and Phase 12c builds it for API keys — the
sequencing this document already described, unchanged.

**Also deliberately still missing: the slug is not reserved.** A unique index on
a pending slug reads like the kinder behaviour and would make squatting free, one
throwaway address per name. So the name is checked when it is requested and again
when it is confirmed, and first to *confirm* wins. `a_name_taken_while_you_were_reading_your_mail_does_not_burn_the_link`
pins the case that creates.

The original finding follows.

#### The original finding

`POST /v1/signups` is `security()` — unauthenticated by definition, which is
correct. What is on the API is `RequestBodyLimitLayer`, `TimeoutLayer` and
`TraceLayer` (`bin/api.rs:68`). There is no rate limit, no proof of work, no
captcha, and no email verification.

Every call that gets past validation runs `CREATE DATABASE` and a full migration
chain. A script can therefore exhaust a cluster's disk from the open internet
with no account, and each attempt costs the attacker one HTTP request and costs
the operator a database. Email verification before provisioning would fix the
abuse case and the "is this address real" case at once — and it needs the outbox
handler from item 1, which is part of why that is first.

### 6. ZATCA is proven in sandbox only

Nine documents accepted with zero warnings, against **sandbox**. Simulation and
production are untested, and both need a real taxpayer's OTP from the Fatoora
portal — so this is blocked on access, not on code. Simulation is the one that
matters: it is the environment ZATCA requires a solution to pass before
production, and its certificate template differs from sandbox's (found the hard
way — see the running note).

*Updated 2026-09-07 (§45):* the run is now one request. Register with an
`industry`, `POST /v1/tax_sa/zatca/onboarding/activate` with `{environment:
"simulation", otp}`, and watch `GET /v1/tax_sa/zatca/onboarding`: `state` goes
`checking` while the worker submits the six samples and asks for the production
certificate, then `live`. If ZATCA refuses a sample, `refusal` names the
document and the rule, and the worker will not ask again until a new build —
so a refused simulation run is a bug report, not a retry loop. The exact
`curl`s are in `docs/RUNNING.md`.

Renewal is a five-year deadline with a sixty-day warning and no automation
possible, because it needs a human with an OTP. That is written down here so it
is a known limitation rather than a surprise in 2031.

### 7. ~~Sequential upgrades are a policy with nothing enforcing them~~ — done

Was: `FleetPlan::is_current` bounded the top and nothing bounded the bottom, so
`migrate_fleet` would take a tenant from migration 2 to 42 in one hop. Now
`MIGRATION_FLOOR` plus `below_floor` refuse it, and the error names the release
to install first. The predicate is separate from the constant so the rule is
tested against a chosen floor — testing it against the current constant would
prove nothing while it is zero, which it is for all of the first major.

Also done in the same pass: L1's documented mechanism corrected (it described an
advisory lock the code deliberately does not use), L7 enforced and its two
violations fixed, and L8 corrected to the mechanism that actually holds.

The original finding follows.

#### The original finding


D17 says upgrades are sequential and that we support two majors. Nothing in the
tree refuses a skip. Checked:

```
grep -rn "floor\|minimum_version\|min_version\|too old\|MIN_MIGRATION" crates/ migrations/
```

returns four hits, none of them about schema versions. `FleetPlan::is_current`
(`crates/erp-control/src/fleet.rs:45`) is `self.version == Some(latest)` — an
upper bound only — so `migrate_fleet` will take a tenant from migration 0002 to
0042 in a single hop today, which is exactly what D17 forbids.

What it needs:

- a `MIGRATION_FLOOR` constant, bumped to the previous major's final migration
  at each major release;
- a refusal in `walk_fleet`'s `visit` when `applied_version < MIGRATION_FLOOR`,
  whose error **names the release to install first** — "too old" with no next
  step makes an operator guess, which is the failure being prevented;
- `None` (never migrated) still allowed: that is fresh provisioning, not a skip;
- a test that an out-of-range tenant is refused *and* that a fresh one is not,
  since a floor that also blocks provisioning would be found in production.

Related and separate: a test that no registered event name loses a step in its
upcaster chain. The support window bounds which builds we patch, not which
events we must read — a v1 event is readable forever or the log is corrupt.

Both are small. Neither is done, and D17 is marked accordingly in the decision
index.

### 8. Commands that existed and no route could reach — done

Found by auditing the API surface against the module exports rather than by
using either feature, which is the only way this class shows up.

`pos::take_back` and `sales::refund_invoice` were both built, tested, exported —
and mounted nowhere. A till could take a return from Rust and not over HTTP, and
a refund outside a till had no route at all. `POST /v1/pos/shifts/{shift}/sales/{sale}/returns`
and `POST /v1/sales/invoices/{invoice}/refunds` now exist.

The same audit found `ShiftEvent::Refunded` had been unreachable since Phase 15,
which is what prompted looking.

**The lesson worth keeping:** the role matrix in `crates/erp-api/tests/http.rs`
caught both the moment they were mounted, because it fails on a served operation
with no row. Nothing caught them while they were *unmounted* — a command with no
route is invisible to every guard in the build. The nearest cheap check would be
a test that every `pub async fn` taking `&TenantDb` is named by some handler.

### 9. A retried till return took the drawer down twice — done

`pos::take_back` checked `Shift::has_pay_out` for its idempotency, and
`ShiftEvent::Refunded` recorded no key at all. So a retry deduplicated perfectly
in `sales` — the credit note and the money are keyed by reference there — while
the shift appended a second `Refunded` every time. Three retries of a 17.25
return left a drawer that should have held nothing holding **−34.50**.

`a_retried_return_is_harmless` passed throughout, because it asserted the ledger
balances and the ledger was the half somebody else was already protecting.

`Refunded` now carries its own `reference`, `Shift::has_return` answers for it,
and the test asserts the drawer as well as the books. Verified by falsification:
reverting the one-line fix fails the test with the −34.50.

Two seen-lists rather than one shared list, because a banking run and a return
are different caller namespaces.

### 10. A full entry cache refused the traffic that was arriving — done

`TtlCache::put` skipped the insert when the map was full and nothing had expired.
The comment defended it — "a cache that thrashes is worse than one that
occasionally misses" — and it is the wrong call here: what survives is whatever
arrived first, so the cache sits at capacity serving a working set it has stopped
tracking, and every request that is actually happening misses.

Under a five-second TTL the oldest entry is one that was about to expire anyway,
so evicting it is the expiry sweep running a moment early rather than a thrash.
It now evicts the oldest tenth. `a_full_cache_makes_room_for_what_is_arriving`
is the test.

The capacity itself (`ENTRY_CACHE_CAPACITY = 50_000`, five caches) is unchanged.
It was on the gap list as "undersized", but the number is not the defect — the
behaviour at the boundary was.

### 11. The book documented four of nine modules — done

`modules.md` described `ledger`, `sales`, `purchases` and `tax_sa` and stopped
there; `crates.md` listed the same four plus no `erp-occupancy`; the API index
said "All 105 operations" when there were 120, and had no chapter for `pos` or
`branches` at all.

All nine modules are now in both, `pos` and `branches` have chapters, `http.md`
has their route sections and the `X-Branch` header it had never documented, and
the count is generated rather than remembered.

**One thing the pass corrected rather than added:** `sales.md` still said a
credit note is refused on an invoice that "has payments", which stopped being
true when refunds landed — the rule is *still holding*, paid less refunded. A
test's doc comment said the same. Prose that was true when written is the kind
of stale that survives, because nothing compiles it.

### 12. What is next (2026-09-07)

Written after the ZATCA onboarding landed (§45) and the sixteen CI failures
behind it were run down. In the order I would take them:

1. **Prove it against the real world.** Item 6 — one simulation run with a real
   OTP now exercises the whole onboarding, the worker's half included — and
   Tabby's and Tamara's sandboxes, which §43 built for and nobody has called.
   Everything below is guesswork about the hardest interfaces until this is done.
2. ~~**Phase 13, real time.**~~ **Done** — all fifteen boxes, across 13a–13d
   (`0b086c9`, `274517e`, `e947280`). See §46, §47 and §48.
3. **Blueprints (4d).** Browse → parameterize → preview in a rolled-back
   transaction → install, and the chart-of-accounts templates. Still the nearest
   customer-visible payoff, as the closing paragraph says.
4. **The singles.** The WPS salary file (Phase 9, the same shape as the ZATCA
   submission); large exports as outbox effects (Phase 11); messaging delivery
   receipts through `hooks.rs`, now that the inbound surface exists (Phase 11);
   MFA and OIDC as more rows in `authenticator` (Phase 3).
5. **Item 4's other half.** Deployment beyond compose and Postgres failover —
   rehearsed as tests, the way restore was, before either is claimed.
6. **Then, and only then** — below, unchanged: 5b and 6 wait for a second
   consumer; 18 and 19 are new modules and come after.

### 13. What is next (2026-09-09)

Written after Phase 13 closed and after §49 answered a question asked before the
backlog was picked up. The order below **supersedes the one above**, which was
correct when written and is now one item stale.

1. **Prove it against the real world.** Unchanged, and now unambiguously first.
   Item 6 — one ZATCA simulation run with a real OTP — plus Tabby's and Tamara's
   sandboxes, which §43 built for and which nothing has ever called. Every
   estimate below is guesswork about the hardest interfaces until this is done,
   and it is the only item no amount of code advances.
2. ~~**Answer §49.**~~ **Answered the same day: a new vertical for this product.**
   What it leaves behind is **Phase 20a, the party model** — one person who is the
   owner of unit A, the tenant of unit B and the guarantor on unit C. It is a
   design question, it gates every other box in Phase 20, and it is worth settling
   while the rest of the phase is still on paper rather than after.
3. **Blueprints (4d).** Five boxes, and now the largest half-built phase: browse →
   parameterize → preview in a rolled-back transaction → install, plus the
   chart-of-accounts templates. Still the nearest customer-visible payoff — and
   with §49 answered it is no longer a detour, because a real-estate chart is one
   of the templates and Phase 20 will want it.
4. **The singles.** Audited on 2026-09-09; **most of the list was not work at
   all** — see the legend. Where it stands after that day:
   - ~~Per-line VAT exemption reasons~~ — **built (§50)**, and it was a defect
     rather than a gap: every exempt line was declaring itself a financial
     service to ZATCA. The property vertical's gate, and it is open.
   - ~~Messaging **delivery receipts**~~ — **skipped, and recorded**. No
     provider's callback shape is verifiable (Taqnyat's own documentation does
     not state a single field), and correlating one needs a change to
     `EffectHandler`, a kernel trait with five implementors. Building that to
     feed adapters that cannot be written is how unexercised machinery gets in.
     `docs/AMBIGUITIES.md` §1, §1b. **One captured callback settles it.**
   - MFA and OIDC (Phase 3a) — genuinely wanted, genuinely unscheduled, and now
     the largest remaining single.

   Removed from this list, because they are decisions rather than tasks and the
   boxes now say so: **WPS** (unverifiable specification — guessing is worse than
   absence), **large exports** (nothing takes a minute), **opening hours**
   (nothing would read them). **API keys** was removed for the opposite reason —
   it shipped in 12c and the box had not noticed.
5. **Item 4's other half.** Deployment beyond compose, and Postgres failover.
   Rehearsed as tests the way restore was, before either is claimed.
6. **Phase 5b now has its consumers, and that is new.** The rules engine was
   deferred for want of a second consumer to describe it from. "What Phases 7–13
   unblock" already names four — booking automations, HR document expiry, §9b's
   claim union, authorization — and the deferral's own condition is therefore met.
   It is no longer correct to file this under "waiting"; it is correct to file it
   under "ready, and unscheduled".
7. **The audit that keeps paying.** Every phase examined against the code has
   closed boxes that were never work — Phase 9 (WPS was a decision), Phase 3
   (API keys and `ETag` had shipped), Phase 4e (exemption reasons were a defect,
   not a gap), Phase 4d (two chart templates should not exist as named), Phase 6
   (account determination was built by another route). **Phases 2, 3d, 5, 7, 8,
   10–17 have never been audited this way.** It costs about an hour a phase and
   has so far been the cheapest work available.
8. **Phase 20 proper** — 20b through 20e, the letting half. Sized in §49 at
   `prepaid`-scale plus its prerequisites. **Where it goes in this list is a call I
   have not made**: it is a vertical, not a feature, and whether it starts before
   or after blueprints depends on whether there is a customer waiting for it. The
   phase is written so the answer changes the order and nothing else.
9. **Then Phase 18 and 19**, both audited on 2026-09-09 and both genuinely
   unbuilt, and 20f–20g — fixed assets and the buying-and-selling half — after
   them or alongside, since nothing in the letting half depends on either.
   **Phase 6 is not in this list any more**: four of its six boxes remain and
   all four are blocked on customer conversations, which is a thing to do rather
   than a thing to build.

**`a_tailer_never_skips_an_event` is fixed** (reported 2026-09-09). It had failed
once at 399 of 400 positions on 2026-09-08; my leaked-database explanation was
wrong and was withdrawn at the time. What actually fixed it is not recorded here
because I did not make the change — worth one line in §3 when whoever did says
what it was.

### Then, and only then

~~Phase 5b (the rules engine) and Phase 6 (configured domain) are still correctly
sequenced: both wait for a second real consumer to describe them, and neither has
one yet.~~ **Half of that stopped being true and nobody noticed.** Phase 5b's
condition is *met*: "What Phases 7–13 unblock" names four consumers — booking
automations, HR document expiry, §9b's claim union, and authorization — and it
has said so since Phase 13 was planned. 5b is not waiting; it is ready and
unscheduled, which is a different thing and belongs in a different list. Phase 6
still waits, and correctly. Blueprints (4d) remain the nearest thing with a
customer-visible payoff.

The smaller deferrals — snapshots, `Idempotency-Key`, `ETag`, `ModuleEnabled<M>`,
an entry-level read model, quantities and unit prices on a line — each name the
condition that should trigger them, and none of those conditions has been met.
They are fine where they are. (Partial credit notes and customers as records used
to be on this list; both are built — §32, §44, §16.)


## Running notes

- **"Implementation of `Send` is not general enough" is diagnosable — from the
  right place.** An axum handler's future must be `Send`, and rustc reports a
  failure at the *route table*, naming borrows in files that look unrelated
  (rust-lang/rust#102211). `#[axum::debug_handler]` finds nothing. A whole day
  went into chasing it from the handler, moving the error around without ever
  closing it.

  What actually worked was one line, put in the crate that owns the code:

  ```rust
  const _: fn() = || {
      fn assert_send<T: Send>(_: T) {}
      fn probe(control: &ControlPlane, modules: Vec<ModuleSetup>) {
          assert_send(control.sign_up(/* … */));
      }
  };
  ```

  With the error landing next to the cause, bisecting took twenty minutes. Four
  distinct triggers, each one enough on its own:

  1. **A helper `async fn` taking several references.**
     `install_modules(&Tenant, &[ModuleSetup], &mut PgConnection)` — three
     elided lifetimes. Inlined.
  2. **A borrowed iterator held across an await.** `for setup in &modules`
     carries a `slice::Iter<'_, _>`; indexing does not.
  3. **A closure capturing by reference across an await.** `|e| f(locale)`
     needed `move`; `&Locale` alone broke the proof.
  4. **`Migrator::run`, generic over `Acquire<'_>`.** `Box::pin` does not help —
     the opaque future still carries the bound. sqlx ships `run_direct` for
     exactly this, marked `#[doc(hidden)]` with the comment *"getting around the
     annoying `implementation of Acquire is not general enough` error"*.

  Two structural improvements fell out and were kept: DDL now goes through
  helpers that **take and return the connection by value**, so the `Acquire`
  bound never reaches a caller's future; and the control plane no longer depends
  on `erp-projection`, because creating a projection group turned out to be two
  statements it can run itself.

  **The lesson is the diagnostic, not the fixes.** When an error names types from
  a file you are not editing, assert the property where the code lives.

- **Submit-then-refresh was broken, and the fix was mostly a call nobody made.**
  Projections are driven by a worker, so a read taken immediately after a write
  can legitimately miss it. Every write already returned its log position and
  nothing could be done with it.

  `?consistent_after=<position>` waits for the projection to reach it — but the
  real bug was underneath: `request_visit` had existed since the lease work and
  *nothing called it*, so a tenant that had been quiet waited out its thirty-second
  idle backoff before anything projected the write. `consistent_after` would have
  timed out on a perfectly healthy system.

  On timeout the read is a 503, not stale data with a shrug. The caller asked for
  a guarantee this response cannot make; answering anyway is the behaviour that
  made the feature necessary. A read that does *not* ask never waits, and a test
  asserts it does not pay for the option.

  ponytail: one control-plane round trip per write, and the update is a no-op for
  an already-due tenant. If write rate makes it hot, batch the ids in the API
  process and flush on a timer — the call site does not change.

- **`Path<String>` silently 404s any route with two parameters.** The `Tenant`
  extractor pulled the slug positionally, which works for
  `/tenants/{slug}` and fails for `/tenants/{slug}/members/{identity}` — the
  shape every nested route in this API will have. It surfaced as a 404 on a
  route that plainly existed, which is the most misleading failure available.
  Extracting by name from a `HashMap` fixes it for every route, present and
  future.

- **A capability with no endpoint is a capability nobody has thought about.**
  `ManageTenant` shipped last round with nothing behind it — by the standard
  written two paragraphs above its own definition. Member management is what it
  was for: a tenant had exactly one user, forever.

  The owner sets a colleague's password and hands it over. The polished flow is
  an emailed invitation link, which needs email delivery, which needs an outbox
  handler nothing has written — so it would look finished and deliver nothing.
  What shipped is how small businesses actually onboard staff, and the invitation
  flow calls the same `add_member` once someone accepts.

  Two rules are worth naming. A demotion invalidates the cache immediately
  rather than waiting out the five-second TTL, because five seconds is five
  seconds of someone doing what they were just told they cannot. And the last
  owner cannot remove or demote themselves: a tenant with no owner has nobody
  who can add one, and the only fix is a support ticket.

- **The `role` column was written and never read.** Every member of a tenant
  could do everything — post entries, close accounts, install charts — while
  `grant_membership` dutifully recorded "owner" or "clerk" for nobody. A stored
  field that no code path consults is worse than an absent one: it reads like a
  control.

  Four roles, four capabilities, and `Role::allows` is the only place the
  decision is made — which is what makes the rule engine (Phase 5) a change to
  one function rather than an audit of every handler. `Allowed<C>` in a
  handler's signature is the check, for the same reason `TenantDb` has no public
  constructor: `tenant.require(…)?` on the first line fails by *omission*, which
  is silent and invisible in review.

  Clippy found the one honest mistake in the design: `Admin` and `Accountant`
  had identical bodies. With the capabilities that exist they were the same
  role, and a role that is a synonym for another is a support question with no
  answer. Dropped.

- **Platform staff are not tenant members with a different role.** Making the
  membership cache hold a parsed tenant `Role` broke support access immediately,
  because platform memberships store `support` and `superadmin` — a different
  vocabulary. Forcing them through the same enum would let "support" answer
  questions about what someone may do inside a tenant's books. Two caches now,
  each with the type its question actually has.

  A stored role this build does not recognise is a 500, not a default.
  Defaulting down locks someone out silently; defaulting up lets them in
  silently. A test asserts the 500.

- **A chart of accounts is a template, not a fixture.** The architecture
  describes blueprints as browse → parameterize → materialize → edit → preview →
  install. What shipped is browse, preview and install: every account a template
  creates is an ordinary account that can be renamed, closed, and posted to from
  the moment it exists, so "edit before installing" solves a problem that only
  exists if installing were irreversible. It is not.

  The two decisions that took thought were bilingual account names — telling a
  Saudi bookkeeper to rename eighteen accounts is a chore, not a starting point —
  and putting VAT and Zakat in *every* chart rather than an "advanced" one,
  because a Saudi business without them has to fix the chart before its first
  invoice. A test asserts both, including that the Arabic is actually Arabic
  rather than a copied English string.

  Installing skips accounts that already exist rather than refusing. Eighteen
  accounts is eighteen commands and the fifteenth can fail; refusing would make
  the retry — the obvious next move — fail immediately and leave the chart
  half-built forever.

- **The system did not actually work outside its tests.** After the ledger
  landed, a user could post an entry over HTTP and nothing would ever project it
  — `bin/worker` had no jobs registered, so the read models only moved when a
  test drove them by hand. Worth naming because every individual piece was
  tested and green: the gap was in the composition, which is the one thing unit
  tests structurally cannot cover.

- **Listing an invariant is not checking it.** Architecture §7 has named five
  per-tenant invariants since the first draft, and nothing ran any of them. They
  run now, on an interval — in memory rather than in a table, because losing the
  schedule on a deploy costs one extra check per tenant, which is cheaper than
  the table that would avoid it.

- **`Invariant` is a trait; the kernel checks are not.** The four kernel
  invariants apply to every tenant and there is nothing to register, so they are
  written directly. The trait exists because the *ledger's* trial balance must
  not be knowable to the kernel (D11) and the worker must not be knowable to the
  module — so they meet in `bin/worker`, in three lines. That is the shape a
  `Module` trait will take when there is a second module to describe it.

- **`Money` could not be decoded out of a stored event.** `CurrencyCode`'s
  `Deserialize` took `&str`, which only works when the deserializer can point
  into its input — true for `from_str`, false for `from_value`, which is how
  every event payload is decoded. Every unit test passed; the first real event
  carrying an amount failed. Now `Cow<str>`, with a test on the `from_value`
  path specifically. The lesson generalizes: a type's serde impl must be tested
  on the path production uses, not the path that is convenient to write.

- **Two sqlx migrators cannot share one database.** The ledger's read models
  were a numbered migration chain, which failed with `VersionMissing(2)` because
  `_sqlx_migrations` already belonged to the tenant schema. The fix is not a
  second table — it is noticing that a module's read models are *derived*, so
  there is no data to preserve across a change and no chain to be in. They are
  an idempotent install script now, and a module that eventually needs real
  migrations will need its own version table then.

- **The retry loop had to move to where the connection budget is.** `execute`
  begins a transaction per attempt, and a transaction needs a permit from the
  tenant's lane — so a version taking a bare `PgPool` either hands out an
  unmetered connection or holds one permit across every attempt. `try_execute`
  is one attempt in the caller's transaction; `TenantDb::execute` is the loop.
  Same split as `run_once`/`run_once_in`, for the same reason.

- **The ledger's route layer lives in `erp-api`, not in the module.** With one
  module, a `Module` trait that mounts routers is a trait with one
  implementation. When the second module lands, what the two route layers have
  in common *is* the trait — described rather than guessed. The module still
  owns everything that matters: aggregates, the invariant, the read models.

- **Balances are views.** A maintained balance table is a second thing that can
  be wrong, and keeping it in step is the projection code most likely to
  double-count. `sum(amount)` is exact and needs no code. Marked with the
  ceiling: fine to millions of postings, wrong at hundreds of millions.

- **Phase 3 was building abstractions before their consumers existed.** A
  `Module` trait with one implementation is a factory for one product; capability
  permits with nothing to permit are a guess about what a module will ask for;
  a configuration system with nothing configurable is the largest guess of the
  three. All of it was scheduled *before* the first module, which is the one
  thing that could tell us the shape. Phase 3 is now the request path — sign in,
  enter a tenant, get an answer in your language — and the kernel services wait
  for the ledger to say what they should be.

- **The extractor is the authorization.** `Tenant` in a handler's signature is
  the check, because its only constructor is `ControlPlane::enter` and there is
  no other route to a `TenantDb`. "Did we verify the membership?" stops being a
  question you answer by reading the handler body.

- **Two enumeration oracles closed, both tested.** A tenant that exists but is
  not yours returns a response byte-identical to one that does not exist; an
  unknown login handle costs the same time and returns the same bytes as a wrong
  password. The second needs a dummy Argon2 verification on the miss path —
  identical error messages do not hide a 50ms/50µs timing difference.

- **Sessions are the one entry-path lookup that is not cached.** Everything else
  tolerates five seconds of staleness. A logged-out token that keeps working for
  five seconds does not, so `session()` hits the database every request. That is
  the cost of a revocation that means something.

- **Effects are written by commands, not derived by projections.** A projection
  deriving effects from the stream would get exactly-once for free from L4, which
  makes it the tempting design. It is wrong for one reason that settles it:
  projections are rebuildable, and a rebuild would re-derive every effect and
  re-send years of email. Command-time effects mean a rebuild sends nothing,
  which is what makes `replay_shadow` something you can run in production. It
  also matches L5 — an effect records a decision taken under the configuration in
  force at the time, and re-deriving it later would resolve against today's.

- **A missing handler must not be a delivery failure.** The first design claimed
  every due effect and failed the ones it could not handle, which backs off and
  eventually dead-letters. That turns an ordinary staggered rollout — some
  workers have a module's handler, some do not yet — into a dead-letter storm for
  every tenant using that module. The claim now filters on the kinds the
  dispatcher knows, so an unrecognised effect is simply left for a worker that
  can take it, and "nobody can handle this" surfaces through the backlog-age
  alarm instead. `effects_with_no_registered_handler_are_left_alone` is the test.

- **`impl Into<Decision>` made a rejection-only command handler uninferable.**
  `execute` first took anything convertible into a `Decision`, so a command with
  no effects could return a bare `Vec`. A closure whose only branch is
  `Err(...)` then has no way to name the `Ok` type, and the compiler's complaint
  points at `Result` rather than at the real problem. It now takes `Decision`
  directly: one fewer generic parameter, always inferable, and `Decision::one(…)`
  at every call site puts the D9 vocabulary where a reader will see it.

- **`FOR UPDATE` with `OFFSET` locks the rows the offset skipped.** A test that
  held a lock on "the second outbox row" via `ORDER BY id LIMIT 1 OFFSET 1 FOR
  UPDATE` was locking the first row as well, because discarded rows still pass
  through the `LockRows` node. It measured the wrong thing and failed for the
  right reason. Pick the id first, then lock by primary key.

- **The lease is per *visit*, not per tenant.** Two workers processing one
  projection group is already refused by the checkpoint lock (L4), so a tenant
  lease is not what makes concurrency safe — it is what stops two workers opening
  connections to the same tenant at the same moment to learn there is nothing to
  do. That reframing removed the renewal loop, the rebalancing, and the
  membership protocol: one statement claims what is due, and the mark lapses
  afterwards.

- **Idle tenants are throttled by `next_visit_at`, not by the lease.** The
  measured sizing rule is `connections ≈ active_tenants × per_tenant_pool`, so
  visiting every tenant constantly would make every tenant active. A visit that
  finds nothing pushes its tenant out by an interval, and per-tenant pools hold
  no connection in between. The jitter is derived from the tenant's own id rather
  than a random source, so it is a pure function and a restart does not reshuffle
  the fleet — without it, a batch claimed together stays synchronized forever.

- **`run_once` had to give up ownership of its transaction.** The worker takes
  its connections from `TenantDb`, which has no public pool accessor by design —
  so a runner that begins its own transaction from a `&PgPool` cannot be driven
  by a worker without breaking the boundary that makes cross-tenant access a type
  error. `run_once_in` takes the caller's connection and does everything L4 needs
  inside it; `run_once` is now a thin wrapper for tests. The obligation moves to
  the caller, and it fails safe: forgetting to commit loses a batch, and there is
  no ordering in which a caller can commit part of one.

- **Fault injection kills the connection rather than simulating a failure.**
  Returning an error from a fake proves the code's own rollback path works, which
  was never in doubt. `erp_testkit::kill_connection` issues
  `pg_terminate_backend` from a second connection, so Postgres does the rollback
  and the code finds out the way it would in production. That is what makes
  `a_crash_mid_batch_leaves_neither_rows_nor_a_moved_checkpoint` an L4 test
  rather than an error-handling test.

- **The outbox test suite asserts at-least-once, not exactly-once.** Delivery and
  the record of it are separate commits, so a crash between them redelivers.
  Asserting "exactly once" would assert something the design does not provide,
  and would first fail in production rather than in CI. The test asserts the two
  deliveries carry the *same* idempotency key, which is the property that makes
  at-least-once survivable.

- **A five-millisecond backoff made an assertion a race.** A test asserted an
  effect was *not yet* due immediately after failing delivery, with the backoff
  set to 5ms so the suite would stay quick. Under the full parallel suite it lost
  that race about one run in six — and a flake that says the code is broken when
  the test is is worse than a failure, because the response to it is to rerun.
  Fixed twice over: the assertion now reads `next_attempt_at` from the row rather
  than inferring it from a second dispatch, and waiting for a backoff polls for
  due-ness instead of sleeping a guessed duration. Verified across eight
  consecutive runs of the file and three of the workspace.

- **`just prepare` did not work from a clean checkout either.** Same class of bug
  as `cargo test` in Phase 1: `just` does not read `.env` by default, so a
  developer whose Postgres wants a password got `no password supplied` from a
  recipe while their tests passed. `set dotenv-load := true`, and both the
  type-check and admin URLs are now derived from `DATABASE_URL` so credentials
  live in one place.

- **L3 isolation caught its first bug immediately — mine.** The shadow rebuild
  set `search_path` to the shadow schema *before* reading the log, which put the
  `event` table out of scope. The isolation was working exactly as designed; the
  sequencing was wrong. `run_once` had it right: read the batch, then narrow the
  path, then apply.

- **The differ needs its own proof.** A clean shadow diff is ambiguous between
  "replay is reproducible" and "the differ does not work", and the second is
  indistinguishable from the first until it matters. So there is a projection
  that deliberately writes `now()`, and a test asserting the differ catches it —
  same discipline as the naive-event-log test in Phase 2a.

- **A concurrency test can pass without ever hitting the thing it tests.** The
  16-task retry test was asserted to exercise the retry path; counting decision
  invocations showed *16 decisions for 16 successes* — the transactions were
  short enough that they never overlapped. The retry loop now has a test that
  injects the competing write from inside the decision closure, so the conflict
  is caused rather than hoped for.

- **Do not block inside an async task to coordinate a test.** The first attempt
  at the above held two tasks at a spin-wait until both had loaded. It deadlocked
  — a spinning task starves the worker that would run the task it is waiting for
  — and then *passed* on a rerun with different scheduling, which is worse than
  failing. `block_in_place` is the supported escape hatch; better still, arrange
  for one task to need no coordination.

- **`cargo test` did not work from a clean checkout.** Cargo does not read
  `.env`, so a developer whose Postgres requires a password got
  `password authentication failed` even with a correct `.env` — the test binary
  never saw the variable. I had been masking this by exporting `.env` manually in
  every command, which is exactly how a setup bug survives to the first new
  contributor. `erp-testkit` now loads `.env` itself, and a connection failure
  reports what it tried, where the setting came from, and the password redacted.

- **L1 needs a counter row, not an advisory lock.** The architecture said
  `pg_advisory_xact_lock` per tenant. Two corrections: database-per-tenant means
  the log is already tenant-scoped, so there is nothing to key a lock on; and an
  advisory lock over a sequence gives commit *ordering* but not gaplessness,
  because a rolled-back transaction burns its number. A counter row updated with
  `UPDATE ... RETURNING` gives both — the row lock serializes, and the counter is
  transactional so a rollback returns the position. That turns the contiguity
  check from a warning into a real integrity assertion.

- **Localization completeness is now a shared audit.** `erp_i18n::testing::audit`
  checks translation coverage, plural categories per language, non-empty
  rendering, Arabic-actually-in-Arabic, and code shape. Each crate's test is one
  line, and when `Module` gains `messages()` the registry can run it across every
  module — which is what makes it impossible to ship a module without
  translations.

- **Missing translations warn and fall back; they do not fail the request.** The
  deliberate exception to L6: a Saudi user reading one English sentence is
  inconvenienced, one reading a 500 is blocked. CI is where a missing string is
  found; the runtime fallback is what happens if one slips past.

Decisions taken during implementation that amend the architecture are recorded
here and folded back into ARCHITECTURE.md.

- **Money is not generic over currency.** Recorded as D10. Currencies are tenant
  configuration and cannot be type parameters; the guarantee is preserved by
  omitting `Add` rather than by a phantom type.

- **Validated string newtypes get no `sqlx::Type` derive.** `#[sqlx(transparent)]`
  generates a `Decode` that skips the validating constructor, so a value read
  back from the database would bypass its own invariant — which is precisely
  where it matters, since that is where data written by older versions arrives.
  Callers bind with `as_str()` and decode through `new()`.

- **Test-database acquisition measures ≈280 ms, not the ~200 ms first claimed.**
  Teardown is another ≈140 ms but is off the critical path. Numbers from
  `cargo test -p erp-testkit --test harness cloning_is_fast -- --nocapture`.

- **The kernel holds no business domain; accounting is a module.** Recorded as
  D11. The earlier placement confused a universal *invariant* (debits equal
  credits) with a large *domain* (chart of accounts, statement formats, posting
  rules, fiscal calendars, multi-GAAP) — and made the most saleable module
  unremovable. Phase 3 is now kernel services only; the ledger is Phase 3b, built
  as a module from the start rather than extracted from the kernel later.

- **The connection permit was scoped to the request; it is now scoped to the
  operation.** Holding a permit across business logic caps *concurrent requests*
  at the budget, when what needs capping is *concurrent database operations* —
  ~400 connections at 10k req/s instead of ~120. `TenantDb` now holds only pools;
  `acquire`/`begin`/`read` take the permit for the duration of the operation.

- **The lane budget does not bound open connections.** The soak test refuted
  that: with a budget of 32 across 40 tenants, peak open connections was 95. A
  connection returned to a tenant's pool stays open until the idle timeout, so
  connections accumulate across every tenant touched in that window. Halving
  `max_connections_per_tenant` halved the peak; setting it to 1 produced exactly
  the tenant count. The real rule is
  `connections_per_cluster ≈ active_tenants × max_connections_per_tenant`, and it
  means **cluster count is sized by concurrently-active tenants, not by tenant
  count or request rate**. Defaults changed accordingly (per-tenant 8 → 4, idle
  timeout 30s → 10s).

- **Throughput rose as the per-tenant pool shrank** — 7.7k → 22.2k ops/s going
  from 4 connections per tenant to 1 — because connection churn cost more than
  the extra parallelism bought. A bigger pool is not automatically faster.

- **`enter()` had to be cached before it could serve real load.** Four
  control-plane queries per request is 40,000 queries/second at 10k req/s against
  a database that cannot be sharded. A 5-second TTL cache with local invalidation
  on writes brings that to ~0.1% of requests. The cost is a bounded staleness
  window on revocation, documented in `cache.rs`; shortening it below a few
  seconds needs out-of-band invalidation, which is a Phase 3 decision.

- **The second module changed the answer, which is why it was worth building.**
  The plan said "a second business module, proving cross-module integration *by
  event*" — sales would emit, the outbox would carry a promise, and a handler
  would post to the ledger a moment later. Two things fell out of trying it.

  The first was mechanical: `EffectHandler::deliver` takes a `PendingEffect` and
  nothing else. It cannot reach a `TenantDb`, because `TenantDb` lives in
  `erp-control` and `erp-control` depends on `erp-eventlog`, not the other way
  round. Making it possible meant a context type parameter threaded through
  `Dispatcher`, `OutboxJob` and their tests — about a hundred lines of kernel
  churn for a mechanism whose first genuine user (email, ZATCA clearance) does
  not exist yet.

  The second was the real one. The outbox exists because delivering to something
  outside this process cannot be atomic with the commit. Between two aggregates
  in the *same database* it can be, and choosing not to would mean an invoice
  can exist without its journal entry — a state needing a dead-letter queue, a
  sweeper, and an operator to explain it. That trade is worth taking against an
  email server. It is not worth taking against a table two schemas over.

  So `ledger::post_entry_in` is a new seam: one attempt, in the caller's
  transaction, account checks included. `post_entry` became the retry loop
  around it, which also moved those checks *inside* the transaction and closed a
  TOCTOU nobody had noticed. `a_failed_posting_leaves_no_invoice_behind` is the
  test that holds the line.

  What the original design was actually protecting — that a module should not
  hardcode which accounts a sale moves — survives intact, as
  `sales::PostingAccounts`. Phase 6 supplies that value from configuration. It
  was never the asynchrony that provided the decoupling.

- **Two fixtures were installing a module without entitling the tenant to it.**
  Adding the 404-if-not-enabled check to the module routes failed nine tests
  that had been green. `enable_ledger` in `tests/http.rs` created `proj_ledger`
  and never called `enable_module`, so every tenant in those tests had the
  ledger's tables and no entitlement to use them — a discrepancy nothing could
  detect until something read the entitlement. The gap was in the test harness,
  but a harness that cannot represent a tenant without a module is a harness
  that cannot test declining one.

- **The demo is a client, and that is the whole value of it.** Every step goes
  through the public API: sign up, install a chart, issue an invoice, take a
  payment. A seeder that called commands directly would have been shorter and
  would have proved nothing — the failure it needs to catch is an API that
  cannot do what the internals can.

  It caught one immediately. Signing the demo up with `["ledger"]` alone fails
  at `POST /v1/tenants/demo/sales/invoices` with `request.module_not_enabled`,
  which is how the "every module enabled" requirement went from a sentence in a
  document to something that cannot be quietly false. That experiment is worth
  repeating whenever this check changes: a demo that passes because it asks
  nothing is worse than no demo.

- **One list, because two things needed it.** `erp_api::modules()` replaced the
  `match` in signup. Not a `Module` trait: a trait would also have to carry the
  routes and the worker'"'"'s jobs, and neither can cross that boundary — a module
  must not depend on `erp-api` or `erp-worker`. So each composition root still
  lists what it composes, and only the *set* is shared. That is the part the
  demo needed to make "all of them" mean something.

- **`just demo` failed the first time a person ran it, and the second.** Two
  bugs, both of the same kind: a method with no caller.

  `relation "cluster" does not exist`. `bin/demo` assumed a migrated control
  plane; `ControlPlane::migrate` had existed since Phase 1 and *nothing called
  it*. Exactly the shape of the `request_visit` bug, and for the same reason —
  the tests all start from a migrated template, so the one path that starts from
  nothing was the one path nothing exercised.

  Then `duplicate key value violates unique constraint "cluster_pkey"`, found by
  the regression test written for the first bug. `register_cluster` was a bare
  `INSERT`, so a second run against the same deployment failed — and, worse,
  there was no way at all to change a cluster's capacity after registering it.
  Registering now declares configuration: `ON CONFLICT DO UPDATE` on the
  capacity and DSN columns, and deliberately **not** on `status`, so
  re-declaring a draining cluster does not put it back into service.

  The regression test builds on `Schema::sql("empty", &[])` — a database that
  has run nothing — and asserts it really is bare before bootstrapping it. A
  bootstrap test against an already-migrated fixture passes for the wrong
  reason.

- **Two halves of "enable a module", and only one of them existed.**
  `ControlPlane::enable_module` wrote the entitlement. Installing the read
  models was a separate step, inlined in `provision`. So enabling a module on a
  live tenant — which nothing could do over HTTP anyway — would have produced a
  tenant entitled to a module whose tables did not exist: routes found, every
  one of them failing on a missing relation.

  The test fixture had been papering over it, doing both steps by hand. That is
  the tell worth naming: **a harness with its own install path is a harness that
  can be right while the product is wrong.** `install_module` now does both, and
  the fixture calls it.

  The two orderings differ on purpose. `provision` entitles *before* installing,
  because the tenant is invisible until activation and early entitlement buys
  retry visibility. `install_module` installs *before* entitling, because the
  tenant is live and the entitlement is the thing that makes the routes
  reachable.

- **The dependency moved onto the module.** `sales::setup().requiring(&["ledger"])`
  replaced a hardcoded `if requested.contains("sales")` in signup. Three places
  needed the same answer — signing up, enabling later, and refusing to disable
  something another module is standing on — and two of them did not exist when
  the first was written. A `requires` field is not an abstraction; it is the
  question being asked in one place instead of three.

- **The one place that deletes a live tenant has three guards.** `reap_demo`
  refuses anything without an expiry in Rust, re-reads the row under
  `demo_expires_at <= now()` before dropping anything, and repeats the condition
  in the final `DELETE`. The gap between a sweep and a reap is exactly where a
  demo becomes a paying customer, and a test converts one in that gap to prove
  the re-read matters.

  The expiry instant is computed by Postgres, not by the process — same
  reasoning as event times. Two machines' clocks disagree, and the one deciding
  when a database is destroyed should be the one everybody already agrees with.

  Database first, row second. The other order leaves a database no row points
  at, which nothing would ever find again; this order leaves a row pointing at
  nothing, which the next sweep retries and `DROP ... IF EXISTS` absorbs.

- **A green test run was leaking three tenant databases, and had been for a
  while.** The API fixture recorded databases as `provision()` created them —
  but tenants born from `POST /v1/signups` were never on that list, and each
  signup test tried to drop `erp_tenant_acme`, a name that stopped being right
  when database names became id-derived rather than slug-derived. Dead cleanup
  that silently did nothing.

  The fix is to stop remembering: `cleanup` reads `SELECT database_name FROM
  tenant` out of the test's own control database and drops what is actually
  there. Asking cannot drift the way recording can. Found by counting
  `pg_database` before and after a run — worth doing again after anything that
  creates tenants.

- **An unauthenticated account takeover, found while wiring invitations up.**
  `set_password` ended with `ON CONFLICT (kind, handle) DO UPDATE SET secret`.
  That is the right shape for *changing your own password* and a full takeover
  for *registering a new one* — and the function had both kinds of caller.

  Signing up with somebody else's email overwrote their password and left the
  authenticator row pointing at **their** identity. Proved before fixing:

  ```
  signup with victim@acme.test          → 201
  login as victim, attacker's password  → 201
  read the victim's tenant as owner     → 200
  victim's own password                 → 401
  ```

  Public endpoint, no credential, complete loss of the account and everything
  the account owned.

  The fix is that the two operations are different operations.
  `register_login` inserts and refuses a taken handle; a future "change my
  password" gets its own function whose `WHERE identity_id = $1` is the clause
  that makes the difference. Signing up with an address that already has an
  account now has to prove it — which also turns out to be the *right product
  behaviour*, because the same person opening a second company should not need
  a second email address. `log_in` split into `authenticate` + `start_session`
  so signup and invitation-acceptance can check a password at exactly the cost
  a login checks one.

  What is worth taking from this: **the bug was in a function with a name that
  described neither caller correctly.** "Set the password" is true of both, and
  the difference between them is the whole of the security property. It sat
  there through the auth phase, the API phase and an authorization-matrix test
  suite, and was only found because a third caller needed the same code and its
  semantics had to be stated out loud.

- **Invitations without email.** The link is returned to the inviter, once, and
  they pass it on however they already talk to that person — which for a small
  business in this market is frequently better than mail, and does not wait on a
  decision about a provider. Sending it by email is an outbox effect (D9); the
  control plane has no outbox table, and adding one to carry a handler nobody
  has written would be building the mechanism before the need.

  What the link cannot do is become somebody else's account. Acceptance always
  binds to the invited address: an existing account for it must prove itself
  with its password, and a new one is created under that address and no other.
  Wrong password does not burn the invitation — a typo should not become a
  support ticket.

- **The takeover was a class, so the class got audited.** Every `ON CONFLICT DO
  UPDATE` in the codebase, every write to `session`, `membership` and
  `identity`, and every privileged control-plane method checked for a route that
  reaches it. Two more findings, one real:

  **Removing a member made them permanently un-addable.** The unique constraint
  on `(identity_id, tenant_id)` covers revoked rows, and `grant_membership` was
  a plain `INSERT` — so re-adding anyone hit a 500 that named nothing. An
  employee who leaves and comes back is not an edge case.

  Granting now revives a revoked membership, and the `WHERE membership.revoked_at
  IS NOT NULL` on the `DO UPDATE` is the whole safety of it: without that clause
  the same statement would be a way around `change_role`'s last-owner guard.
  That is the lesson from `set_password` applied on the spot — an upsert that
  means two things is the thing to be suspicious of.

  **Managing a stranger answered 204.** Changing or removing an identity that
  belongs to a different tenant updated no rows and reported success. Isolation
  held — a test asserts the other tenant's membership was untouched, and it
  passed before the fix — but an owner who mistyped an id was told something had
  happened. Now a 404.

  `suspend_identity`, `log_out_everywhere` and `Actor::impersonating` turned out
  to have no HTTP route at all, which is the right answer for all three.

- **Nothing had ever migrated an existing tenant.** `provision` runs the
  tenant-plane migrations when it builds a database, and that was the only
  caller. The day `migrations/tenant/0004_*.sql` shipped, new tenants would get
  it and every existing one would not — while the code that needs it deployed to
  all of them. Queries would compile, because they are checked against a
  database that *has* the migration, and fail at runtime per tenant across the
  live fleet.

  `survey_fleet` answers "who is behind?" without writing anything, which is the
  thing to run *before* a deploy. `migrate_fleet` does the work. It does not
  stop on a failure — one unreachable cluster must not leave the rest of the
  fleet un-migrated — and it is idempotent, so a partial run is resumed by
  running it again. Suspended tenants are included: a suspended tenant is one
  that may come back, and coming back to a schema three versions behind is the
  whole failure.

- **And underneath it, a worse one: adding a migration did not trigger a
  rebuild.** Found immediately, by adding a real migration to a real two-tenant
  fleet and watching `just migrate-fleet check` report everything current.

  `sqlx::migrate!` embeds the files at compile time. With no build script, cargo
  never learns the directory is an input — so the binary keeps an old migrator
  baked in and reports a fleet that is up to date with a migration it has never
  heard of. That is precisely the silent failure the fleet migrator exists to
  prevent, sitting one layer below it.

  Two `build.rs` files fix it, each emitting `rerun-if-changed` for its
  migrations directory **and every file in it** — the directory alone covers
  adding and removing files but not editing one in place.

  Verified end to end afterwards: two seeded tenants, add a migration, `check`
  reports both behind and exits 1, apply, the table is confirmed present in both
  tenant databases by `psql`, `check` exits 0.

- **Configuration, built for the one thing that needed it.** `PostingAccounts`
  was a constant with a comment saying it was the seam. It is now resolved from
  a tenant's own configuration, and everything else that looked configurable on
  inspection turned out not to be: the VAT rate is statutory, session and
  invitation lifetimes are platform decisions, currency is chosen per chart and
  carried by each invoice. One consumer, so one key.

  The store is versioned by a shared sequence, which makes `max(version)` a
  single number describing a tenant's whole configuration at a moment — and that
  number is what finally fills `Metadata.config_version`, declared in Phase 2
  and never written until now.

  **Resolved inside the command's transaction.** What the invoice posted to and
  what the tenant had configured cannot disagree, and the generation is stamped
  on the event. The values themselves go into the journal entry, so changing the
  configuration changes the next invoice and nothing before it (L5) — there is a
  test that changes it between two invoices and then replays the ledger to prove
  the earlier one still rebuilds.

  The mechanism is key-value; the *surface* is not. `PUT .../sales/posting-accounts`
  is typed, validated, and behind `ManageAccounts`. A generic "set any key to any
  JSON" endpoint would make every reader's decode the only thing between a typo
  and a broken module.

- **A guard that disagreed with the command it guarded.** The first version of
  the posting-accounts validation checked the accounts against
  `proj_ledger.account` — a read model driven by a worker. A tenant who
  installed a chart and immediately configured where to post it was told the
  accounts did not exist. `ledger::accepts_postings` now asks the log, which is
  the same question `post_entry_in` asks and asks it the same way.

- **Process note: two edits silently did nothing.** A scripted
  `str.replace` whose target had already been reflowed by `cargo fmt` is a no-op
  that reports success, and I spent three rounds debugging a handler that still
  contained the old code. The tell was `Finished in 0.10s` — a test run that did
  not rebuild after an edit did not edit anything. Every scripted replacement in
  this codebase should assert its target matched.

- **The rule engine was deferred, and the deferral is the interesting part.**
  Phase 5 was to build `Facts`, `DynCondition`, `FactRegistry` and `Rule<E>`,
  then move authorization onto them. Authorization is its only real consumer —
  pricing did not exist **— false since `modules/booking/src/pricing.rs` shipped**, see §53 — and no concrete rule had been asked for, so every
  decision about *which facts exist* would have been a guess dressed as an
  interface.

  What was concrete: the second module created a permissions gap a real business
  has. One role per tenant means the person who does the invoicing must also be
  an accountant for the books. That is a describable problem with a bounded fix,
  and it produces the first genuine case for the engine rather than a
  hypothetical one.

- **Authorization now reads the URL, on purpose.** `Allowed<C>` derives the
  module from the request path, because the URL namespace *is* the module
  namespace by construction — every module mounts under its own name.

  The alternative, an explicit marker per handler, fails the wrong way: a
  handler that forgets it silently gets the tenant-wide role, which is the more
  permissive answer. **Forgetting must never be the permissive option.** The
  price is that authorization depends on URL shape, paid by
  `module_paths_are_what_they_look_like` — a route that moves changes a test
  rather than changing permissions quietly.

  An unrecognised path segment is *not* treated as a module, which matters more
  than it looks: if it were, somebody held back in every module they have could
  reach `/v1/tenants/acme/anything/…` and fall back to a role they were
  deliberately not given.

- **`JournalEntryEvent::Reversed` had existed since Phase 3 with no command that
  produced it.** Declared, named, upcast-registered, applied by the aggregate,
  written by nothing — the fourth instance this session of a thing that exists
  and has no caller, and the pattern behind three of the real bugs found so far.

  It mattered more than the others: without it, **a mistake was permanent.** An
  entry posted for the wrong amount could not be corrected by any route, which
  is not a missing feature in an accounting system so much as a missing
  premise.

  `reverse_entry` posts the opposite lines and marks the original, both in one
  transaction — an entry marked reversed with no reversal to show for it is a
  hole in the trial balance, and a reversal with nothing marked is a
  double-count. Reversing again with the *same* id is a no-op, so a retry is
  safe; with a different one it is refused and says what already undid it,
  because the second attempt would swing the balance the other way.

  The aggregate now keeps its lines, so undoing one does not need a second
  place that knows how a `Posted` event is shaped.

- **What was deliberately not built with it.** The obvious companion is a
  `proj_ledger.entry` table showing which entry reversed which. It was left out:
  adding a table to a module's install script means re-running it across the
  fleet and replaying the group, which is the module-refresh machinery deferred
  in Phase 4 — and nothing displays the link today, because there is no
  entry-list endpoint either. The correction works, the books balance, and the
  read model arrives with the screen that wants it.

- **A changed read model needed the refresh, so the refresh got built.** Credit
  notes have to be *visible* — a cancelled invoice still showing as outstanding
  is worse than no cancellation at all, because somebody chases a customer for
  money that was credited back. That meant a new column on
  `proj_sales.invoice`, and `install.sql` is `CREATE TABLE IF NOT EXISTS`
  throughout, so re-running it would never have added one.

  This is the trigger the Phase 4 deferral was waiting for. `refresh_module`
  drops the module's schema, installs it again and rewinds its checkpoint — all
  in one transaction, holding the same checkpoint lock a projection run takes,
  so a run in flight finishes rather than finding its tables gone mid-batch.
  Resetting the checkpoint in that same transaction matters just as much: there
  is no moment where the tables are gone and the checkpoint still claims they
  are current, which a worker would read as "nothing to do".

  Proved against a real fleet rather than only in tests: seed a tenant on the
  old schema (5 invoices, no `cancelled_on`), ship the new one, run
  `just migrate-fleet refresh sales`, watch the column appear and the tables
  empty with the checkpoint at zero, start the worker, watch all five invoices
  come back. A second refresh is harmless.

  It also caught a mistake in its own reporting: the first version exited
  non-zero after a *successful* rebuild, because it asked `is_uniform()` — and
  for a refresh, "tenants that were behind and have now been rebuilt" is the
  success case, not the failure one.

- **Crediting an invoice reuses the seam reversal created.** `ledger::reverse_in`
  is to cancelling what `post_entry_in` was to issuing: the ledger owns what
  undoing a posting means, sales owns when. Both events commit in one
  transaction, for the same reason as before.

  Refused while payments stand against it. The money is somewhere, cancelling
  the document without moving it back would leave cash on the books against a
  sale that no longer exists, and there is no way to model a refund yet.
  Refusing says so; guessing would not.

- **The VAT return is the module's commercial reason to exist, and it was a
  view and a query.** Everything it needs was already stored: the tax bands, the
  rate that applied, the tax point. That is what banding by rate at issue bought
  — the return is `GROUP BY`, not a recomputation, and it cannot disagree with
  what the invoice printed.

  The period is **half-open**, `[from, until)`. "31 March inclusive" is a
  comparison somebody gets wrong once a quarter, and two consecutive returns
  built that way either double-count the boundary day or drop it. The test
  states that as a property rather than as arithmetic: the two quarters
  together equal the whole span. An earlier version asserted a hand-computed
  total instead, which passed and said nothing.

  Credited invoices leave the return, which is right when the credit lands in
  the same period and wrong across a boundary — a credit note in a *later*
  period is a supply and then an adjustment, and each belongs in the period it
  happened. That needs the credit note to be a document with its own tax point,
  which is the partial-credit-note work. The view says so rather than pretending
  otherwise.

- **The demo had stopped demonstrating.** Credit notes, reversals and per-module
  roles all shipped without reaching the seeder, so the CI check that proves the
  system works end to end was proving a subset of it. That is the omission my own
  note predicted — "what still needs a person is teaching the seeder to use it" —
  arriving three increments later.

  It now issues an invoice against the wrong customer and credits it, posts a
  utilities entry for the wrong amount and reverses it, and adds a second person
  who does the invoicing and not the books. Each is asserted, so the next feature
  that skips the seeder fails a test rather than quietly narrowing the demo.

  A demo of an accounting system in which nothing was ever *wrong* is not a demo
  of an accounting system. It is also the first thing a prospective customer asks
  about and the last thing a description is convincing about.

- **Fifty-five error codes, and nothing listed them.** A client branches on
  `code` — that is the contract — and finding out what the codes are meant
  reading Rust across five crates.

  `docs/ERRORS.md` is generated from the same catalog the API renders from, so it
  cannot claim a code that does not exist, and a test fails when the codebase
  grows one the document does not mention. Checked by adding a code and watching
  the check fail, then removing it.

  Generated rather than written for the usual reason: a hand-maintained list is
  wrong within a month, and wrong in the direction that costs an integrator a
  day — a code that exists and is undocumented looks like a bug in their client.

- **I wrote a vacuous test, and the check for vacuity is what caught it.**
  `refresh_module` takes the checkpoint lock before dropping anything, so a
  projection run in flight is not left mid-transaction with its tables gone. I
  had asserted that in a comment. The first test asserted only that the refresh
  *had not finished* while a run held the lease — and it **passed with the lock
  removed**.

  It passed for the wrong reason. Without the explicit lock the refresh still
  blocks, at the checkpoint `UPDATE` — but that comes *after* `DROP SCHEMA`, so
  by the time it blocks the damage is done. "The refresh waits" was never the
  property. "The run's tables are still there while it is in flight" is.

  The rewritten test has the in-flight batch write a row after the refresh has
  started, and fails when the lock is removed. Verified both ways, which is the
  only thing that distinguishes a test from a comment that compiles.

  Worth stating plainly: the window is the *start* of a batch, when a run has
  taken its lease and written nothing, so it holds no lock on the tables
  themselves. That is the moment every projection run passes through.

- **A performance instinct that measurement refused.** `Allowed<C>` derives the
  module from the request path, which calls `modules::available()` — and
  `ModuleId` holds a `String`, so that is two allocations and two validations on
  every authenticated request, on the authorization path. It looked like an
  obvious regression to fix with a `OnceLock`.

  Measured first: 82ns for `available()`, 35ns for `module_id()`. Against a
  request that makes several database round trips that is roughly a hundredth of
  a percent, and three small allocations next to the ones JSON parsing already
  makes. Left alone.

  Recorded because the reflex was wrong, not because the outcome was
  interesting: "allocation on a hot path" is a shape, not a measurement, and
  this codebase has a soak test precisely so the difference can be settled.

- **The OpenAPI document found three defects, and none of them was in the
  document.** Generating it was supposed to be a writing job. What it produced
  was a list of places where the server and the promise had quietly diverged:

  1. **`payments` carried two types on the same resource.** `InvoiceView` has a
     `payments` **count**; `InvoiceDetailView` flattens `InvoiceView` and adds a
     `payments` **array**. serde writes the flattened fields first, so on the
     detail endpoint the array silently overwrote the count — `GET .../invoices`
     answered `"payments": 2` and `GET .../invoices/INV-1` answered
     `"payments": [ … ]`. Every generated client would have broken on it, and no
     test noticed because each endpoint was only ever read on its own. The count
     is now `payment_count`.
  2. **The most common client mistake did not get the documented error shape.**
     axum's `Json` rejection is `text/plain` with no `code`, so "every failure is
     `application/problem+json` with a stable code" was untrue for a malformed
     body — on exactly the request where a client most needs the message. Fixed
     at the root with `wire::Json` and `wire::Query`, which keep axum's status
     (400 / 415 / 422) and replace only the body. Six imports changed; nothing
     else did.
  3. **Undocumented statuses.** A 500 can come back from any route and was
     declared on none.

  All three surfaced the same way: the document's hand-written half — which
  status carries what — was checked against real responses.

- **What is structural and what had to be checked.** `utoipa-axum` registers the
  axum route *from* the `#[utoipa::path]` attribute, so path and method are one
  string rather than two that agree today; a handler with no attribute does not
  compile inside `routes!`, and schemas come from the wire types by derive. That
  covers everything except the `responses(…)` blocks, which are prose about
  types the compiler never relates to the handler's return value.

  So `tests/http.rs` validates **every response it receives** against the schema
  the document publishes for that path, method and status — sixty-five tests and
  three thousand lines become contract coverage for the cost of one call in
  `Fixture::send`. The validator is a hand-written subset of JSON Schema rather
  than a dependency, and it carries its own two guards:
  `every_schema_keyword_is_understood` fails when the document starts using a
  keyword nobody implemented (a constraint silently unchecked is how a
  hand-rolled validator becomes a test that looks at nothing), and
  `the_validator_is_not_vacuous` proves it says no to a missing, renamed,
  retyped, or extra field. Both were confirmed by breaking the code.

- **Conventions belong in one place, applied to the finished document.** The
  bearer scheme, `Accept-Language` on every path, what each status means, and the
  responses every operation can give regardless of what it does — all of it is
  uniform, and declaring it per-handler would be thirty chances to leave one out.

  utoipa's `modifiers(…)` is the wrong hook: it runs on `ApiDoc::openapi()`,
  *before* any route is registered, so a modifier that walks `paths` walks
  nothing. The first version did exactly that and reached zero operations —
  caught by `every_path_takes_accept_language`, which is there because a
  convention that silently applies to nothing is worse than one never written.
  `Conventions` now runs on the document `split_for_parts` produces.

- **Three routes were called `list`.** utoipa derives `operationId` from the
  handler's name, and handler names are unique *per module* — `members::list`,
  `invitations::list` and `modules::list` all became `list`. A generator handed
  that emits three `list()` functions and drops two, or refuses the document
  outright. Found by checking the generated file against the OpenAPI Object
  schema by hand, which is also where the 413/504 gap came from; both are now
  tests. The handlers are `list_members`, `list_invitations` and `list_modules`,
  which is what the rest of the crate already did (`list_accounts`,
  `list_charts`).

- **Two failures are outside the document, and it says so.** `bin/api` wraps the
  router in a body limit and a timeout, so a body over 1 MB is a 413 and a
  request still running after thirty seconds is a 504 — both refused at the edge,
  before anything the document describes, and both with no body at all. Writing
  them into the operations would be a lie (they carry no `Problem`); leaving them
  out entirely would be a different one. The `info` description names them.

- **The one thing a document must not be wrong about is authentication.**
  `only_the_deliberately_public_routes_are_public` lists the eight open
  operations and fails on any other that opts out. It caught `POST /v1/signups`,
  which is public and was documented as needing a session — harmless in that
  direction, and the same test is what catches the harmful one.

- **A heading that had outrun its test.** `every_role_can_do_exactly_what_it_should`
  was documented as "every role against every endpoint" and checked three ledger
  routes. It could not have done better while the endpoint list lived in the test
  body: it only grew when somebody remembered to grow it, and a route added
  without that thought is a route nobody checked. Twenty-seven role-scoped
  operations existed; four were covered.

  The document fixed that as a side effect. The endpoint list now comes from
  `erp_api::openapi()` — the same value the router is built from — and the
  permission table has to name every operation under `/v1/tenants/{slug}` or the
  test fails. **Adding a route now forces the decision instead of allowing it.**

  The table itself is still written out rather than derived from `Role::allows`:
  a test that asks the code what it does can only agree with it. This one asks
  whether that is what we meant, and a change in permissions has to be typed into
  a diff somebody reviews.

  108 checks, all green — the code and the intent agree everywhere, which is the
  outcome worth having and not the one worth assuming. Both halves were confirmed
  by breaking them: a table entry the code does not grant, and a served route the
  table does not name.

  The mechanism that makes a garbage body safe is worth stating: `Allowed<C>` is
  a `FromRequestParts` extractor and the first parameter of all twenty-seven
  handlers, so authorization runs *before* the body is parsed. `{}` gets a 403
  when the role is refused and a 400 when it is not — the exact distinction being
  measured — and the matrix cannot mutate the tenant out from under itself.

- **`manager` was never a role.** I published it in three field descriptions and
  an example; the roles are `owner`, `accountant`, `clerk`, `viewer`. A client
  copying that example gets a 400.

  `role` is a `String` on the wire on purpose — an unknown one should get a
  localized `request.unknown_role`, not a serde rejection — which leaves the list
  a client reads as prose, in eight places. So `Conventions` now generates it from
  `Role::ALL` onto every `role` field, and the doc comments that listed it are
  gone. One list, and it is the enum's.

- **Gapless numbering: what a sequence cannot do, and why the client had to give
  something up.** Saudi law requires a tax invoice to carry "a sequential number
  which uniquely identifies the invoice" (VAT Implementing Regulations, Article
  53), and ZATCA's e-invoicing rules require the counter to advance by exactly
  one so the cryptographic chain has no holes. Not *unique*. Not *mostly
  ordered*. **Gapless** — an auditor counts them, and a missing 4,108 is a
  question the business has to answer.

  A Postgres `SEQUENCE` cannot do it, and not by accident: `nextval` is
  deliberately transaction-independent, because that is what lets concurrent
  writers take numbers without blocking. Every rolled-back issue would burn one.
  So the counter is an ordinary row read `FOR UPDATE` and advanced in the
  transaction that writes the document, and a rollback releases the number
  because it was never really taken.

  **The cost is real and cannot be engineered away.** Issuing serializes per
  (tenant, series). "Gapless" and "concurrent" are the same contradiction
  whatever holds the counter; the honest answer when a tenant outgrows it is more
  series — per branch, per point of sale — which is how the paper world solved it
  too.

- **Two calls, because the document might not be written.** `reserve` takes the
  row lock without moving the counter; `consume` moves it. A single
  `nextval`-shaped call would burn a number on every idempotent retry — and a
  client whose request timed out and repeated it is the *normal* case, not an
  edge one. Putting a gap in a business's invoice sequence because their network
  blinked would be this feature failing at the one thing it exists to do.

  `re_issuing_does_not_move_the_series` is the test for exactly that pairing,
  which the module cannot enforce from inside itself.

- **The client gave up choosing the number, and got a key instead.** `id` on a
  new invoice used to be the invoice number. It is now the client's own
  reference: what makes a retry a no-op, and what addresses the document
  afterwards. The number is allocated here and comes back as `number`.

  That is not a preference. A number a client picks cannot be gapless — two
  clients cannot coordinate, and a client that skips one has no way to know. The
  same split applies to credit notes, which ZATCA numbers separately from the
  invoices they credit, and which now have their own series.

  A retried request is told the number the document **already has**, which costs
  one extra aggregate load on that path only. Telling a client "done" and nothing
  else would leave it to guess, and the guess would be a number that does not
  exist.

- **The number is in the event, not derived on read.** Architecture L5, and here
  it is load-bearing: a number derived at projection time would mean replaying a
  tenant's log renumbers every document they have ever issued — including the
  ones customers hold copies of. `a_replay_reproduces_the_numbers_it_issued_under`
  checks the rebuild is identical *and* that the counter did not move.

  For the same reason `document_number` is not in a `proj_*` schema. It is not
  derived from the log; the log depends on it. A module refresh drops and rebuilds
  projection schemas, and a tenant whose series restarted at one afterwards would
  reissue numbers that are already printed. That is now asserted in
  `refreshing_a_module_rebuilds_its_schema_and_rewinds_its_checkpoint`, and
  confirmed by making a refresh delete the counters and watching it fail.

- **Old invoices keep the numbers they were issued under.** `Issued.number` is an
  `Option`, not a version bump with an upcaster — and that is the honest shape
  rather than a shortcut. An upcaster sees the payload and not the stream it came
  from, so there is nowhere for an old number to come from; and an invoice issued
  before this existed genuinely had none allocated. Its number *was* its
  client-chosen id, and the projection resolves `number.unwrap_or(id)`, so every
  such invoice keeps exactly the number on the copy somebody holds.

- **The demo taught the wrong thing for about ten minutes.** Its invoices were
  seeded with ids like `INV-2026-001`, which now sit beside numbers like
  `INV-00001` — two strings that look like invoice numbers, side by side, in the
  one artifact that exists to explain the system. The seeded ids read like a
  CRM's references now (`crm-4471`), which is what `id` actually is.

- **A comment that promised a guard nobody had written.** `taxable_supply`
  carried this, in the schema, next to the view it describes:

  > *"…so this view is honest about being the simple case and `vat_return`
  > refuses to span one silently."*

  `vat_return` did no such thing. There was no check anywhere — the sentence
  described an intention that never became code, and it read as reassurance for
  months. The `ponytail:` note above it was accurate about the *problem* and
  wrong about the mitigation, which is the worse of the two ways to be wrong.

- **What it was hiding: a filed VAT return could quietly restate itself.**
  `taxable_supply` excluded cancelled invoices outright. That is right in exactly
  one case — a credit note raised in the same period as the invoice, netting out
  before anything is filed.

  Across a boundary it is wrong. An invoice issued in February and credited in
  April *was* a supply in Q1: the return was filed and the tax paid. Dropping the
  invoice retrospectively meant re-running the Q1 return produced a smaller
  number than the one filed, with nothing anywhere recording why — and the credit
  appeared in no period at all.

  Both documents are now entries on their own tax point (`proj_sales.vat_entry`),
  so Q1 keeps the supply and Q2 carries the adjustment. Same-period credits still
  net to zero; cross-period ones no longer reach back.

- **The numbering work is what unblocked it.** The `ponytail:` note said the fix
  needed "the credit note to be a document with its own tax point", and it was
  right. A credit note now has its own number, from its own statutory series, and
  `on` is its tax point — so there *is* a document to date the adjustment by. The
  feature that made it possible was built for a different reason entirely.

- **The old test could not have caught it.** `a_credited_invoice_drops_out_of_the_return`
  credited within a single period, so it passed under both the wrong rule and the
  right one — it never touched the boundary, which is the only place they differ.
  Worse, its `credit()` helper dated the credit note in **2023** against invoices
  from 2026, and that had no effect at all, because the old view ignored a credit
  note's date entirely. A field the code never read cannot be wrong in a test.

  The replacement is named for what it checks, and
  `a_credit_note_is_declared_in_its_own_period_not_the_invoices` fails against the
  old rule — confirmed by putting the old rule back and watching a filed 150
  become 0.

- **Bands count both kinds of document.** A period where a supply was invoiced
  and credited shows `invoices: 2, credit_notes: 1, tax: 0` rather than vanishing.
  A return that showed nothing would be hiding that a supply happened and was
  reversed, which is precisely what an auditor is looking for.

- **One check, at the seam everything already routes through.** Closing the books
  has to refuse a back-dated journal entry, a back-dated reversal, an invoice
  with a back-dated tax point, a payment, and a credit note. That is five call
  sites, in two modules, and a check per call site is a check somebody forgets —
  where the one forgotten is the one that mattered.

  There is exactly one: `ledger::post_entry_in`. Sales writes an invoice and its
  journal entry in the same transaction, so every sales command arrives there;
  `reverse_in` calls it too. **Sales never mentions a fiscal period and inherits
  the refusal anyway**, which is the seam earning its keep rather than being
  asserted about. `modules/sales/tests/sales.rs` tests it from that side, because
  a guarantee only one module knows about is a guarantee that decays.

- **`closed_before`, not "closed through".** The watermark is the first instant
  still *open*, so closing January is `2026-02-01T00:00:00Z`. The same convention
  as the VAT return's `until`, for the same reason: "closed through 31 January"
  is a comparison somebody gets wrong once a month, and gets wrong by exactly one
  day. `the_instant_named_is_the_first_one_still_open` pins both sides of the
  boundary.

- **One instant rather than a table of periods.** Books close in order — nobody
  closes March while February is open, because the March numbers are built on the
  February ones. So the whole state is a scalar, stored in the configuration the
  command already reads inside its own transaction. A `ponytail:` note names the
  upgrade: a locked prior year with one adjustment period open inside it is a
  table of ranges, and this becomes its newest row.

- **Reopening is allowed, on purpose.** An accountant who closes the wrong month
  has to be able to put it right, and a system that refuses is one they route
  around by editing the database — which is strictly worse, because then nothing
  records it at all. What it must not be is quiet, which is what `set_by` and
  `set_at` are for.

- **What this makes safe.** The VAT return's period rule says an adjustment
  belongs to the period of its own tax point. Without a close, somebody could
  still date a credit note into a quarter that has been declared and paid — which
  would put the return back exactly where it was before `vat_entry`, able to
  restate itself after filing. `a_credit_note_cannot_be_dated_into_a_closed_period`
  is the test that the two features hold the line together.

- **The third module is a different job from the second.** `sales` answered "how
  do two modules meet". `purchases` answered the question that could not be asked
  until there was a third: *was that a general answer, or did it just happen to
  fit sales?*

  All of the mechanism generalised, unchanged. An aggregate, events at version 1,
  a projection group nobody else reads, `ModuleSetup`, `requires`, a
  rejection-to-status mapping, and a command writing its document and its journal
  entry in one transaction through `ledger::post_entry_in`. The closed-period
  check arrived **for free**: `purchases` never mentions a fiscal period and
  cannot post into one, because every posting goes through the same seam.
  `a_bill_cannot_be_dated_into_a_closed_period` is a rule written in another
  module before this one existed.

  Exactly one thing had to move: `VatCategory`, from `sales` to `ledger`. Two
  sibling modules must not depend on each other, so what they share has to live
  in the one they both stand on. That is a rule only a third module could test.

- **What is genuinely different is the domain, not the plumbing.** Sales
  *computes* tax; purchases *records* it. Input VAT is reclaimed against the
  supplier's tax invoice, so the figure in the books has to be the figure on the
  document you hold — a recomputation landing a halala away produces a reclaim
  that does not match its own evidence, and the evidence is what an inspector
  asks to see. So there is no `vat::total` here and no rounding: the module
  checks the stated tax is *possible* and stores what it was told.

  Three consequences fall out of that one fact, and each is a rule rather than a
  simplification:

  1. **No gapless numbering.** We did not issue the document. The supplier's own
     number is recorded, and a duplicate of it against the same supplier is
     refused — recording one bill twice is a duplicate reclaim.
  2. **Tax without the supplier's VAT number is refused.** A bill from an
     unregistered supplier is not evidence of a reclaim.
  3. **Exempt input tax never reaches `1200 Input VAT`.** It is irrecoverable, so
     it is a cost of the purchase and rides on the line's own account. In
     practice suppliers charge no tax on an exempt supply — but "rarely" is not
     "never", and a rule that only holds for the common case is the one that
     produces an unexplainable balance.

- **A VAT return spans two modules, and neither can compute it.** `proj_sales`
  and `proj_purchases` are separate groups and neither may read the other (L3). A
  third module reading both would be exactly the cross-group read the law exists
  to prevent.

  So it is composed in the API, from each module's own answer — which is not a
  workaround but where cross-module composition is supposed to happen, the same
  place `erp-worker` composes jobs and `modules.rs` composes the catalogue. A
  tenant with only one module gets zeroes for the other side rather than a 404: a
  business that has not enabled purchases genuinely reclaimed nothing, and that is
  a return they can file.

  `GET /v1/tenants/{slug}/sales/vat-return` is **gone**. It answered half the
  question, and half a VAT return is a number nobody files.

- **Two guards earned their keep during this.** `no_two_operations_share_an_id`
  caught the new `/vat-return` colliding with the sales one the moment it was
  registered — which is what forced the decision to delete rather than rename.
  And the authorization matrix refused to pass until all five new operations were
  in its table, so "who may record a bill" was a decision typed into a diff rather
  than one that defaulted.

- **`just clean-databases` left the two halves disagreeing.** It drops
  `erp_tenant_%` and never touched the control plane, so the `tenant` rows
  outlived their databases — and the next `just demo` failed with `slug_taken`
  against a tenant whose database was gone. Found by running it, which is the
  only way a developer-tool bug gets found. The recipe now clears rows whose
  database no longer exists.

- **Five composition roots list every module, and one of them was unchecked.**
  Adding a module means editing `erp_api::modules()`, the message catalog, the
  routes, the worker's job list, and the demo's projection advance. The question
  worth asking after a third module is not "can this be a trait" but **"what
  happens if somebody forgets one of the five?"**

  So I removed `purchases` from two of them and ran the workspace. The catalog
  omission was caught immediately — by the `docs/ERRORS.md` drift check, which
  turns out to cover it for free. The **worker omission was caught by nothing at
  all**: 476 tests green with a module whose read models would never fill.

  That is the worst failure this system has. The events still commit, the ledger
  still balances, the module still accepts writes and posts correctly — and
  `proj_purchases` stays permanently empty. No bill list, no input tax, and a VAT
  return quietly under-reporting what can be reclaimed, so a business pays tax it
  does not owe. Nothing about it looks like a bug from the inside.

- **The fix is a list a test can look at.** `bin/worker.rs` built its jobs with a
  chain of `with_job` calls, which nothing could inspect. They are a
  `module_jobs()` function now, and two tests read it: every module in
  `erp_api::modules()` has a job, and every job is scoped to its module — because
  a `for_module` somebody forgot looks identical until a tenant is billed for
  projections they declined.

  The other two roots are checked behaviourally rather than structurally, which
  is better where it is available: `every_module_has_routes` reads the OpenAPI
  document (a module a tenant can enable and find nothing behind), and the demo
  test asserts its bills were projected (which is what a missing advance looks
  like from outside). All four were confirmed by breaking them.

- **This is the registry the plan asked for, and not the trait.** The `Module`
  trait is still blocked on a genuine contradiction — it would carry a router,
  and a module must not depend on `erp-api`. What three modules made clear is
  that the *registry* half was the load-bearing part, and it did not need a trait
  to be closed. The trait can wait for the router problem to have an answer
  rather than being built around a guess at one.

## Deploying without an outage

Three of the four zero-downtime pieces landed. Build-then-swap projection
rebuilds is the fourth and is its own increment — see the note at the end.

- **Expand-only migrations, enforced.** A migration that removes something an
  old pod still uses turns the overlap window into an outage — and it is an
  outage nobody sees in staging, because staging deploys one pod. Eleven rules,
  each a phrase that only appears in an `ALTER`-shaped statement (`set not null`
  is; the `not null` in every `CREATE TABLE` is not), checked over both migration
  chains with comments stripped first, because half these files explain in prose
  why something is *not* dropped.

  One migration in the repo needed an exemption: `0002_clusters.sql` adds a
  foreign key, which is unsafe on a live table and was entirely safe there
  because it ran before the system had a tenant.

- **The exemption mechanism broke the rule it enforces.** First version put the
  marker in a comment in the migration. `just demo` then failed with
  `VersionMismatch(2)`: sqlx checksums migration files, so editing one — *even to
  add a comment* — strands every database that already ran it.

  A rule about not changing what is already deployed, enforced by changing what
  was already deployed. Exemptions live in `migrations/EXEMPTIONS` now, and the
  reason that file is separate is written at the top of it.

- **The pre-deploy version gate.** `erp_eventlog::upcast` refuses an event from a
  newer build rather than guessing (L6), which is right — and means a build
  deployed out of order does not fail *at deploy time*. It fails later, when a
  projection reaches the first event it cannot read and stops, by which point the
  pods are up and the read models are silently falling behind.

  `just migrate-fleet versions` asks the fleet first, and reports two different
  failures: an event at a version higher than this build declares (somebody is
  deploying backwards) and an event name this build declares nothing for at all
  (a module was dropped rather than deprecated). Verified by planting one of each
  in the demo tenant's log and watching both fire — and the log refused the
  cleanup `DELETE`, which is the append-only trigger doing its job.

- **The gate made `upcasters` part of a module's declaration.** Comparing needed
  the union of every module's event versions, and building that by hand would
  have been a *sixth* place listing modules — in the increment whose whole
  finding was that the fifth was wrong. `ModuleSetup::new` now takes it, as a
  required argument rather than a builder method: a module that forgot it would
  be invisible to the gate, and invisible is exactly the answer that lets a bad
  build ship.

- **Modules are deprecated, never removed.** `ModuleSetup::deprecated(why)`.
  Signing up for one and enabling one are refused; **disabling one is not**, and
  neither is managing who uses it — a tenant on a deprecated module has to be
  able to get off it, and refusing there would trap them. The catalogue carries
  the reason so a picker can hide it. It leaves the build when the last
  entitlement does, which is a fact somebody can check rather than a date
  somebody guessed.

- **A third silent-corruption bug from writing Rust through Python.** Two format
  strings came out with twenty spaces in the middle: `\`-continuations in a
  Python heredoc are consumed by *Python*, so the Rust source never had them and
  the indentation landed inside the string literal. Third time. The tell is a run
  of spaces inside a quoted string, and it is now something to grep for after any
  scripted edit.

- **Build-then-swap projection rebuilds.** Done — see below.

## Rebuilding read models without an outage

- **The blocker was an asymmetry nobody had noticed.** Projections wrote
  unqualified (`INSERT INTO invoice`) through `search_path`; the install SQL that
  *created* those tables named `proj_sales.invoice` outright. So the DDL could
  only ever build one schema — the live one — and a rebuild had no choice but to
  drop it first.

  The install SQL is schema-relative now, and `install_schema` aims it by setting
  `search_path` to the group's schema, which is exactly what the projections
  already did. `just prepare` does the same for the type-check database, because
  the reads are still qualified and the tables have to land where they expect.

- **`rebuild_swap` builds beside, then exchanges.** Staging schema, install,
  replay from zero while the live tables keep serving. Then one transaction: take
  the checkpoint's `FOR UPDATE` lock (so a projection run in flight finishes),
  pin the log head, catch staging up to it, drop the live schema, rename staging
  over it, set the checkpoint. Postgres makes DDL transactional, so a failure
  anywhere leaves live exactly as it was — asserted rather than assumed.

  **Readers block only for the drop-and-rename**, two catalogue updates, because
  the catch-up happens before it rather than after.

- **The catch-up window is where a rebuild silently loses data**, and it is the
  test worth having. Events appended *between* the build finishing and the swap
  happening would be missing from the new tables while the checkpoint claimed
  they were there — permanently. Confirmed by disabling the catch-up and watching
  five events vanish.

- **A guard against the mistake this whole change invites.** If a module's
  install SQL is still schema-qualified, it builds into the *live* schema and
  leaves staging empty — and the swap would then rename an empty schema over a
  working one, deleting a tenant's read models. So staging is checked for tables
  before the swap, and the refusal says why. Confirmed by removing the check and
  watching the failure become an unrelated "relation does not exist".

- **Wired, not left as a library.** `just migrate-fleet refresh <module>` uses
  it. That needed `ControlPlane::maintenance_pool` — `TenantDb` deliberately
  exposes no pool because it is the request path, and a rebuild is not a request:
  no member behind it, several transactions, and the same trust level as
  `enter_for_maintenance`. Verified on the real demo tenant: six invoices before,
  six after, checkpoint unmoved at 55, no leftover staging schema. Under the old
  path that was six, then zero, then six.

  `refresh_module_fleet` is **deleted** — the migrator does the loop now, and a
  function with no callers is the bug class this project keeps finding.
  `refresh_module` stays as the fallback for a caller with no projections, with
  its cost written on it.

- **Deleting nearly took a hard-won test with it.** The first cut removed
  `a_refresh_does_not_drop_tables_under_a_projection_run` — the one proved
  non-vacuous several increments ago — because it sat between the dead test and
  the helper they shared. Caught by the compiler, not by me. The redo asserts
  what it is *not* allowed to remove before writing anything, which is what a
  scripted deletion should have done in the first place.

- **`every_module_can_be_rebuilt`.** `rebuild_swap` is generic over the
  projection group, and a group is a type, so the migrator matches on the module
  name — one more place a module can be left out, and leaving one out means a
  change to its read models could never be deployed. Same shape and same reason
  as `every_module_has_a_projection_job`.

## The rate is the tenant's, not the build's

- **A business outside Saudi Arabia could not issue a correct invoice.**
  `VatCategory::rate_now()` returned `1_500` — 15% since July 2020 — from the
  *accounting kernel*. A fact about one country, in the code every country would
  use, on the write path. The UAE charges 5% and there was no way to say so.

  Moving `VatCategory` into `ledger` last time was right; moving the **rate**
  with it was not. `ledger` keeps the shape — that a line has a treatment and a
  rate — and has no opinion about the number.

- **Rates are configuration, and a country module is what will seed them.** This
  is the answer to "where does a country module put its rates" without a sibling
  dependency: it writes data, and `sales` reads data. The shipped default is
  Saudi Arabia's, with a `ponytail:` note saying it belongs to `tax_sa` the
  moment there is a second country — and that the seam is already here, because
  seeding a config key is all that module has to do.

- **Resolved in the command's transaction, not by the handler.** The API used to
  stamp `Vat::current(category)` onto each line before calling the command. That
  put a database-backed decision outside the transaction that writes it, so a
  rate changed in between would leave an invoice carrying one that was never
  current — the exact argument `resolve_accounts` already makes about posting
  accounts.

  The fix separates two shapes that were one: `DraftLine` is what a client sends
  (a treatment), `InvoiceLine` is what was issued (a treatment *and* the rate it
  was issued under). The second goes in the event, so L5 holds and a rate change
  cannot restate a filed return — asserted by issuing at 15%, changing to 5%,
  issuing again, and checking both the invoices and a shadow replay.

- **One positive rate, deliberately.** KSA and the UAE each have exactly one, so
  `Rates { standard }` is the whole of it. A jurisdiction with reduced rates
  needs a *category* per rate rather than a second field — two lines at different
  positive rates are not the same classification — and that is a `VatCategory`
  change, noted rather than guessed at.

- **The rate is validated where it is set**, between 0 and 10000 basis points. A
  negative one would credit VAT payable on every sale; one over 100% would charge
  more tax than the supply.

## `tax_sa` — the first module that stands on two

- **A country is a module, and this is the first one.** Saudi Arabia has ZATCA
  and 15%; the UAE has Peppol PINT AE and 5%. The rate, the return's shape, the
  clearance protocol and the fields an invoice must print all change at the
  border. `ledger` owns that a line *has* a treatment and a rate; `tax_sa` owns
  what the number is, and seeds it when a tenant enables the module.

- **The VAT return moved out of `erp-api`, where I had put domain that does not
  belong there.** I composed it in the API two increments ago and wrote that
  cross-module composition belongs in the composition root. The core/module model
  says otherwise, and the test it gives settles it: *can a tenant disable it?* A
  business with neither sales nor purchases had a VAT return endpoint.

  It is composed in a module that **declares both** now:
  `tax_sa → {sales, purchases} → ledger`. Nothing reaches sideways, and it is
  still not a cross-group read — each module's own read function is called and
  the answers are netted in Rust, exactly as before. What changed is who owns it.

- **`requires` was wrong, and a test said so.** I gave it
  `requiring(&["sales", "purchases"])`, which reads sensibly and forces a
  business that only sells to enable a purchases module they do not use in order
  to declare tax they do owe. `requires` is an AND list and the rule that
  actually describes this is "at least one of" — which it cannot express.

  So it requires nothing. The **crate** depends on both; the **entitlement**
  depends on neither, and each side reports zero when the tenant has not enabled
  it. That is not a fallback: a business that has not enabled purchases genuinely
  reclaimed nothing.

- **A filing is recorded, not inferred.** Every other guarantee here makes
  re-running a period give the number that was filed — documents on their own tax
  point, closed periods refusing back-dated writes. Those are properties of the
  *arithmetic*. `tax_sa.return.filed` puts the numbers that went to ZATCA in the
  log with the date they went, so "does the system still agree with what we
  filed?" is a comparison rather than an argument — and it survives a rebuild
  because it is an event rather than a derivation, which
  `a_filing_replays_to_exactly_what_it_recorded` checks.

  Filing a period twice is a **conflict**, not a no-op: the second one is an
  amendment, which is a different document with its own rules.

- **`ModuleId` accepts what the database refuses.** `tax-sa` constructed fine,
  passed every test that does not touch the control plane, and failed at the
  moment a tenant enabled it — `entitlement.module_id` is `^[a-z][a-z0-9_]{0,47}$`
  and the type allows `.` and `-`. A terrible place to find that out.

  Everything is `tax_sa` now, one spelling from crate to URL, and
  `every_module_id_satisfies_the_entitlement_constraint` catches the next one at
  build time. The honest fix is for `ModuleId` to carry the narrower rule so the
  type refuses what the schema will; that is a `erp-types` change with no
  consumer asking for it yet.

- **Seeding rides on the only hook a module has.** The rate is an `INSERT … ON
  CONFLICT DO NOTHING` inside the schema install, which is idempotent so a
  rebuild re-running it is harmless — and `DO NOTHING` is what stops enabling a
  country module stamping over a rate a tenant corrected, which
  `re_installing_does_not_overwrite_a_rate_the_tenant_set` pins. A module wants a
  `seed` step distinct from its DDL; noted rather than invented.

- **The demo files a return.** A tax module nobody has filed with demonstrates an
  arithmetic exercise rather than the thing being bought.

## The tenant is the subdomain

- **`/v1/tenants/{slug}/sales/invoices` was wrong, and it was wrong in a way
  every route repeated.** A tenant is a company, and a company on this platform
  is `bassat.erp.com` — not a path segment that every handler has to remember to
  scope by. The slug moved into the `Host` header, and the paths became what they
  describe: `/v1/login`, `/v1/sales/invoices`, `/v1/tax_sa/vat-return`.

  It is the smaller diff *and* the stronger guarantee. A path parameter is
  something a handler can forget to use; a subdomain is resolved once, in the
  `Tenant` extractor, before any handler runs — and a handler that does not take
  `Tenant` cannot reach a tenant database at all, because `TenantDb` has no
  public constructor.

- **Exactly one label under the configured domain.** `demo.erp.test` is a tenant;
  `erp.test` is not, `a.demo.erp.test` is not, and neither is anything under a
  domain this build was not told about. The port is stripped, a trailing dot is
  stripped, and the comparison is lowercase, because all three arrive in real
  `Host` headers. `PUBLIC_DOMAIN` configures it and defaults to `localhost`, so
  `demo.localhost` works in a browser with no hosts-file editing.

- **Two collisions, and both were the paths telling me something.** With the
  slug gone, `GET /v1/invitations/{invitation}` — a manager reading an invitation
  they sent — collided with `GET /v1/invitations/{token}`, which is a stranger
  holding a link and belongs to no tenant. They were never the same resource; the
  public one is `/v1/join/{token}` now. `GET /v1/modules` collided the same way:
  the catalogue of what is *offered* is not the list of what a tenant *has*, and
  it is `/v1/catalogue`.

- **`just clean-databases` left the people behind.** It already deleted tenant
  rows whose database was gone; identities outlived them, so the next `just demo`
  with a different password failed with `invalid_credentials` against an account
  whose company no longer existed. The same bug one table over, and the recipe
  deletes memberless identities now.

  Getting there turned up something worth knowing: `audit_entry` refuses UPDATE
  and DELETE by trigger, which makes the `ON DELETE SET NULL` on its actor
  columns **unreachable** — an identity that has ever acted cannot be deleted at
  all. Fine for the dev recipe, which truncates. Not fine for a Saudi PDPL
  erasure request, and that is a decision to take deliberately: an audit trail
  that keeps a name forever, or actor columns that can be nulled by a path the
  trigger allows.

## ZATCA — clearance and reporting

- **Two obligations, and one field decides which.** A buyer who gives a VAT
  number gets a **standard** invoice, which ZATCA has to *clear* **before the
  buyer is given it**. Everyone else gets a **simplified** one, handed over at
  the till and *reported* within twenty-four hours. `Kind::of` is the only place
  that decision is taken, and everything downstream — the endpoint, the
  `InvoiceTypeCode` subtype, the deadline, the two counts in the standing report
  — follows from it.

- **The document is a projection, and that is the first real extension module.**
  Nothing in the issuing transaction can build a ZATCA document: `sales` issues
  the invoice and must not know Saudi Arabia exists, because the dependency runs
  `tax_sa → sales` and inverting it would put ZATCA in the sales module of every
  tenant in every country.

  So `tax_sa` **subscribes** to `sales.invoice.issued` and
  `sales.invoice.cancelled` in its own projection group. Three things the kernel
  already had made it work with no new mechanism: a projection reads the whole
  log rather than its own module's slice, a projection group is the unit of
  consistency so this writes only into `proj_tax_sa`, and — the one addition —
  `Upcasters::also`, which folds `sales`' event history into `tax_sa`'s so a
  version `sales` adds next year is readable here without a second copy of its
  chain that could disagree with the first.

  **This is the answer to "how does a module extend another?"** The module being
  extended does not know. There is no registry, no hook, and nothing in `sales`
  to change.

- **The registration had to be an event, and the reason is the hash chain.**
  Every other tenant setting here is configuration, read inside the command's
  transaction and stamped onto the event. That mechanism was unavailable — the
  issuing command cannot read a ZATCA registration — and the obvious fallback,
  reading `configuration` inside the projection, is a **silent** disaster:
  rebuild after the business moves offices and every historic invoice renders
  with the new address, hashes differently, and breaks the chain. Each document
  on its own would still look fine.

  So `tax_sa.taxpayer.registered` is a fact in the log with a position, like
  everything else the projection reads. An invoice issued in March renders under
  the registration that was current in March whatever happened in April, and
  `a_correction_applies_from_where_it_was_made` pins it.

- **The XML is written by hand, already canonical.** ZATCA hashes the
  *canonicalised* document (C14N 1.1) and the seller signs that hash, so a
  serialiser that reorders an attribute or collapses `<a></a>` into `<a/>`
  invalidates the signature. The usual pipeline is DOM → serialise → XSL strip →
  canonicalise → hash: four places to be wrong, and three dependencies.

  Writing canonical form directly makes canonicalisation the identity function,
  so `hash(bytes) == hash(c14n(bytes))`. The rules that keeps true are checked by
  a scanner in the test module that walks the output with a tag stack and knows
  nothing about how it was produced — balanced tags, no empty-element form, no
  undeclared prefix, namespaces on the root in C14N's order (**the default one
  first**, because it has no local name and so sorts least), attributes sorted,
  `&`/`<`/`>` escaped in text. `the_scanner_refuses_what_it_claims_to` breaks
  each of those and watches it say no.

  What this does *not* prove is byte-equality with a real C14N 1.1
  implementation, and there is not one in this workspace — `xmllint --c14n11`
  would settle it, and Python's stdlib canonicaliser is C14N **2.0**, which
  rewrites namespace declarations down to where they are used and answers a
  different question. The one that matters is ZATCA's own SDK, and that needs a
  certificate.

- **The first link in the chain is encoded differently from every other one, and
  that is not a bug.** ZATCA's genesis PIH is `base64(hex(sha256("0")))` — 88
  characters encoding the *text* `5feceb66…` — while every subsequent PIH is
  `base64(sha256(bytes))`, 44 characters. A chain that "fixes" the inconsistency
  is rejected at the first invoice, so `the_first_link_is_zatcas_odd_one_out`
  pins the literal.

- **A refusal and a failure to ask are different facts.** ZATCA saying *no* is
  about the document and is final. A timeout, a 503 or an expired certificate is
  about us — nothing was decided, so nothing is appended, the document stays
  pending, and the next sweep tries again. Collapsing them marks a perfectly good
  invoice permanently refused because a token expired, and it marks *every*
  invoice in the batch, which is why the sweep stops on the first `Unanswered`
  rather than working through the rest.
  `an_outage_marks_nothing_refused_and_stops_the_sweep` breaks it and watches.

- **What cannot be built here, and what was built instead.** Submitting needs a
  production CSID — a certificate ZATCA issues after onboarding a specific
  solution for a specific taxpayer — and an XAdES signature made with it. There
  is no honest way to have that in this repository, and a fake that pretended
  would be worse than none, because the whole point of a clearance record is that
  it happened.

  So the seam is `wire::Submitter`, one method, and everything up to the socket
  is here and tested: the request and response bodies, both endpoints, the
  `Clearance-Status` header, and above all `Verdict::of` — which reads an HTTP
  status and a body into *cleared with warnings*, *refused*, or *no verdict at
  all*, including the case where ZATCA answers `200` with `NOT_CLEARED` and the
  status line has to beat the HTTP code.

  The sweep is a function rather than a worker job for the same reason: a job
  registered with nothing behind it is code with no caller, which is the failure
  this codebase keeps finding. `submit_pending` has a real caller in its tests,
  and wrapping it in a job is three lines the day there is a certificate.

- **Documents issued before registration are recorded, not skipped.** The chain
  starts at onboarding, so they have no place in it and cannot be cleared
  retrospectively — but a business needs to know they exist, and silently
  dropping them is the "quietly under-reporting" failure the worker docs warn
  about. They sit at `unregistered` and the standing report counts them.

- **The QR carries no hash yet, deliberately.** Tags 1–5 are what a build without
  a certificate can honestly produce. Tag 6 is the invoice hash and 7–9 are the
  stamp; a QR carrying a hash and no signature claims more than it can show, and
  fails validation for it. The encoder takes them as `Option`s and the length
  byte is a **byte** count, which is the mistake an Arabic seller name makes
  expensive — `the_length_is_bytes_and_not_characters` asserts the two counts
  differ, so it cannot pass by accident.

## ZATCA onboarding: the OTP, the key, and the certificate

- **"Certificate generation" is a key pair and a CSR, and the private key never
  moves.** A taxpayer never issues their own certificate: ZATCA signs the request
  and returns one. What this generates is an ECDSA key pair and a PKCS#10
  request, and the key stays sealed in the tenant's database for the rest of its
  life.

- **secp256k1, which is one character from the curve everything else defaults
  to.** ZATCA specifies the Koblitz curve; almost every X.509 stack reaches for
  secp256r1/P-256. A CSR on the wrong one is refused at onboarding with no
  useful message, so `the_curve_is_the_koblitz_one_and_not_the_usual_one` reads
  the curve back out of the encoded request rather than trusting the constant.

- **OpenSSL was already linked, so key generation cost nothing.** `sqlx` builds
  against it for Postgres TLS, which means the process already had `EcKey`,
  `X509Req`, `X509Extension` and AES-GCM. The alternative was four RustCrypto
  crates for the same capability and a second TLS-adjacent stack in the binary.

- **The two extensions are written as DER by hand.** ZATCA reads the EGS unit's
  identity out of `subjectAltName` as a `directoryName`, and reads which
  environment to onboard into out of `1.3.6.1.4.1.311.20.2`. Neither is
  expressible through openssl-rs's config-string API — `X509Extension::new`
  wants an OpenSSL config section and the Rust binding cannot build one — so both
  are `new_from_der` over sixty bytes this build writes itself. That also makes
  them assertable: the tests decode the finished request and look for the pipe-
  separated EGS serial and the UID OID.

  A whole DER library for sixty bytes would have been the other answer, and the
  encoder here is a tag, a length and somebody else's bytes.

- **The environment is not a default.** Sandbox, simulation and production differ
  by one string in one extension. Getting it wrong does not fail — it succeeds
  against the wrong authority, which is why `Environment` is a required argument
  everywhere it appears.

- **A module had nowhere to keep a secret, so core grew one.** The three places a
  module had were all wrong for a private key: the event log is immutable and
  replayed forever, `configuration` is read by anything that can read the tenant,
  and `proj_<module>` is **dropped and rebuilt** by an ordinary maintenance
  operation. `module_secret` is a core table holding opaque sealed bytes under a
  module's own key — the same shape as `configuration` and `document_number`,
  which are also core tables only modules use.

  Sealed with AES-256-GCM under a key from the deployment's environment, nonce
  and tag inside the value so a row is self-describing. `the_sealed_bytes_do_not_
  contain_the_plaintext` and `a_tampered_value_is_refused_rather_than_decrypted`
  are the two that matter.

- **No sealing key means refusal, not a plaintext fallback.** `SEALING_KEY` is
  optional and its absence is not a degraded mode: the endpoint answers 503 and
  stores nothing. Law L6, applied to the one place where degrading would mean
  writing a signing key to disk in the clear because an environment variable was
  missing.

- **The certificate is checked against our key before it is stored.** ZATCA
  returning a certificate over somebody else's key is not a smaller problem than
  returning none — it is accepted, stored, and then every invoice signed with it
  is rejected at clearance, on a document a customer is waiting for, with an
  error that says nothing about why. `accept` compares the public keys and
  refuses; `a_certificate_that_is_not_for_our_key_is_refused` breaks it.

- **The OTP is never stored, never logged, and never in an event.** It is the
  taxpayer's proof of identity for about an hour. `Otp`'s `Debug` prints
  `<withheld>` so it cannot reach a log through a panic or a `tracing` field, and
  `the_log_records_the_certificate_and_never_the_key` greps the actual event
  payloads for it.

- **What goes in the log is the certificate's identity.** Subject, serial,
  validity, environment, stage — everything a person needs to answer "which
  certificate signed this invoice?" three years later, and nothing secret. The
  secret material is sealed beside it, where it can be rotated; a secret in the
  log could never be.

- **Onboarding works today with no HTTP client.** It is a once-per-tenant act
  with a human in the middle — somebody has to log into Fatoora and read six
  digits off a screen — so the two halves separate cleanly:
  `POST /v1/tax_sa/zatca/onboarding` generates the key and hands back the
  request, and `PUT …/certificate` takes what ZATCA issued. An operator can carry
  the middle step with `curl`.

  That is not a workaround for the missing transport. It is the path a deployment
  falls back to **when the automated one breaks**, which for a once-per-tenant
  act guarded by an hour-long credential is worth having regardless. `Registrar`
  is the automated path's seam, and `Onboarder` drives both halves through it.

- **Two things here are reconstructed from ZATCA's specification and want a
  sandbox round-trip before anyone relies on them**: whether `csr` carries the
  base64 of the whole PEM or of the DER body, and which of those two
  `binarySecurityToken` comes back as. The second is handled by accepting both —
  they are distinguishable, since DER starts with a `SEQUENCE` tag and base64
  text does not. The first is a guess documented at
  `Generated::csr_for_zatca`, and it is the first thing a deployment's sandbox
  onboarding will confirm or deny.

## The XAdES signature

- **Signing cannot happen in a projection, and the reason is ECDSA.** Every
  signature over the same bytes with the same key is different, because a fresh
  `k` goes into each one. A projection that signed would produce different tables
  on every rebuild — in the column a tax authority holds a copy of. It would also
  need the private key, and a projection that could read `module_secret` is a
  projection that could leak it.

  So signing happens once, outside, and `tax_sa.zatca.signed` records the result.
  The projection **replays** a signature rather than recomputing one, which is
  the same argument as recording what ZATCA said, applied to something we did
  ourselves. `a_rebuild_reproduces_the_signature_it_cannot_recompute` breaks it
  by recomputing and watches the shadow differ.

- **Two sweeps, because they fail for different reasons.** `sign_pending` needs a
  certificate; `submit_pending` needs ZATCA to answer. A document needs the first
  even when the second cannot happen — a simplified invoice's QR carries the
  cryptographic stamp, and that receipt goes to the customer at the till whether
  or not the network is up. `pending` hands out only signed documents, so an
  unsigned one is never submitted to be told what we already know.

- **The signature covers `ds:SignedInfo`, which covers two digests.** The invoice
  digest is the one already in the chain — ZATCA hashes the document with the
  extensions, the signature and the QR reference removed, and this build never
  renders those into the bytes it hashes. The second is over
  `xades:SignedProperties`, which carries the signing time and the certificate.
  Breaking the first (signing the invoice digest directly instead) makes the
  signature fail to verify under its own certificate, which is what the test
  checks.

- **The submitted document is rendered, not spliced.** `ubl::signed` is the same
  renderer with the three parts put back in UBL's required order — extensions as
  the first child of the root, the QR reference after the chain, `cac:Signature`
  after the last `AdditionalDocumentReference` and before the parties. Splicing
  strings at a marker would be a second parser that has to agree with the first
  about where a document's parts are.

- **Three things here are reconstructed from ZATCA's specification and deviate
  from the standards they are built on.** Each is one named function, marked
  `UNCONFIRMED`, so a sandbox round-trip changes one line:

  1. `certificate_digest` hashes the certificate's **base64 text**, not its DER.
     XAdES says DER. ZATCA's SDK says text. The same class of quirk as the
     genesis PIH, and pinned by a test that asserts the two differ.
  2. The ECDSA signature goes in as **DER**; XML-DSig specifies the raw `r ‖ s`
     pair. ZATCA's published samples decode to DER.
  3. The whitespace inside `xades:SignedProperties` is **inside the digest**, so
     it cannot be normalised away and the element is one string rather than a
     builder. An editor reflowing that function changes every signature it makes.

- **The demo stays unsigned, deliberately.** It has no ZATCA certificate, and
  inventing one would be inventing a compliance record — the same reason there is
  no fake `Submitter`. `just demo` says so in as many words, and the standing
  report's `unsigned` count is the number that tells a real tenant they are not
  live yet whatever else looks fine.

## The transport, and the compliance checks

- **reqwest with `default-tls`, not `rustls-tls`.** sqlx already links native-tls
  against OpenSSL for Postgres, and every piece of cryptography here — key
  generation, the CSR, signing, sealing — is OpenSSL too. Choosing rustls would
  have put a second TLS implementation in the same process for no benefit. No
  cookies, no gzip, no blocking client: six JSON endpoints on one host.

- **The client is tested against a real socket, not a mock.** A forty-line
  one-request HTTP server in the test module accepts a connection, reads the
  bytes, and hands them back as a string — so the assertions are on **what
  actually went out**: `OTP: 123456` in a header, `Clearance-Status: 1` for
  clearance and `0` for reporting, the basic auth built from the CSID, the JSON
  field names. Removing the OTP header or fixing `Clearance-Status` to one value
  both fail. A mocked client would have asserted on the mock.

- **Failing to ask is still not a verdict, now that there is a socket to fail
  at.** A timeout, a refused connection and a 5xx all become `Unanswered`; a
  `400` with a body is parsed, because that body is where ZATCA explains itself.
  `a_connection_that_fails_is_not_a_verdict` points the client at a closed port
  and checks which of the two it produced.

- **A client for one environment refuses a call for another.** The sandbox and
  production differ by a hostname and a string in a certificate, and a mismatch
  otherwise succeeds against the wrong authority.

- **The compliance samples are invented, and their chain is thrown away.** ZATCA
  wants one of every document type the CSR declared — six for a unit that issues
  both kinds — before it will issue a production certificate. There are no real
  invoices to send, because a business onboards before it issues, so the samples
  are synthetic: the taxpayer's own registration, one line, one riyal, a number
  starting `COMPLIANCE-`.

  They chain among themselves from ZATCA's genesis value and **never touch the
  tenant's counter**, which has not started yet and must start at one when it
  does. Deriving them from the real chain would either burn six positions or
  leave a gap where the samples were — and a gap is the one thing the chain
  exists to make impossible. Collapsing them onto one position fails
  `the_chain_is_broken`.

- **`POST /v1/tax_sa/zatca/onboarding/activate` is the whole flow.** Key pair and
  CSR, the OTP exchange, the six signed samples, and the production certificate —
  four calls to ZATCA in the order it requires, from one request carrying six
  digits. Nothing is stored unless the step that produced it succeeded.

- **A refused compliance sample answers 502, not 400.** The samples are generated
  here, so ZATCA refusing one is a fault in this software and not in the caller's
  request — the message says so, in both languages, and the full failure list
  goes to the log where somebody can act on it. Getting that backwards would tell
  a business to fix something they did not do.

- **The manual path still exists, and is still the one to reach for when the
  automated one breaks.** `POST …/onboarding` hands back a CSR and
  `PUT …/onboarding/certificate` takes what comes back, so an operator with
  `curl` can complete an onboarding whose automated attempt failed at any step.

## What live traffic to ZATCA proved, and what it did not

The first run against `gw-fatoora.zatca.gov.sa` was worth more than the tests it
passed. In order:

- **The sandbox issued a real certificate to a CSR this build generated.**
  `POST /e-invoicing/developer-portal/compliance` answered `ISSUED` with a
  `binarySecurityToken`, which the flow then read, checked against the private
  key, and sealed — serial `01A00C782957`. That settles the parts that were
  reconstructed from the specification: the curve, the two extensions, the
  subject, the base64 wrapping of the CSR, and the shape of the request body are
  **all accepted by ZATCA**.

- **Simulation and production accept the same request and reject only the OTP.**
  Both answered `{"code":"Invalid-OTP"}` to a made-up six digits, which is the
  furthest a build without a real taxpayer's portal login can get.

- **`dispositionMessage` is sometimes absent.** ZATCA returns an invalid-OTP
  refusal with nothing but `errors`, and an earlier version of this rendered that
  as an empty pair of brackets. The recorded body is now a test.

- **Two diagnostics were wrong, and only live traffic showed it.** The first
  failure reported "ZATCA could not be reached" — and the certificate had in fact
  been issued and stored; what failed was the *compliance check*, three steps
  later. Onboarding makes four calls that all fail the same way, so the error now
  names the step, and `Verdict::of` puts the HTTP status and the first 200 bytes
  of the body in the message. The same failure now reads:

  > ZATCA could not be reached while **submitting a compliance document**:
  > ZATCA's answer could not be read: expected value at line 1 column 1 —
  > **HTTP 400, 15 bytes: Invalid Request**

  An hour of bisecting a flow that talks to a tax authority, turned into one
  line. This is the failure mode worth designing against: not that a call fails,
  but that the failure names the wrong thing.

- **What is still unconfirmed: the signed document itself.** The sandbox answers
  `/compliance/invoices` with the non-JSON string `Invalid Request` — and it
  answers deliberate garbage the same way, so it does not distinguish. It appears
  to be a stub for certificate issuance rather than a validating endpoint.
  Settling the `XAdES` signature, the certificate digest and the DER-vs-`r ‖ s`
  question needs the **simulation** environment with a real taxpayer's OTP, which
  is the first thing to do with one.

## What a real certificate settled

A working sandbox CSID from another implementation, plus the key it was issued
against, turned every open question here into an answer. `modules/tax_sa/tests/
sandbox.rs` submits a document built by the ordinary renderer and signed by the
ordinary signer; ZATCA now accepts both a standard and a simplified invoice
**with no warnings and no errors**. Five things came out of it.

- **The hashed document carries the whitespace the removed elements left
  behind, and this was the one that mattered.** ZATCA hashes what it receives
  after an XSL transform removes `ext:UBLExtensions`, `cac:Signature` and the QR
  reference — and a transform removes *elements*, not the whitespace text nodes
  around them. Removing an element from

  ```text
    <Invoice …>\n  <ext:UBLExtensions>…</ext:UBLExtensions>\n  <cbc:ProfileID>
  ```

  leaves the `"\n  "` before it *and* the `"\n  "` after it. This build never
  renders those elements, so it now emits their leftovers deliberately. Without
  them the answer was `invalid-invoice-hash`; with them, accepted. There was no
  way to deduce that from the specification, and no unit test that could have
  caught it.

- **The certificate digest and the DER signature were both right.** Two guesses
  that deviate from `XAdES` and XML-DSig in ZATCA's direction, and both hold.

- **`BR-KSA-EN16931-09`: a second, bare `cac:TaxTotal`.** When
  `cbc:TaxCurrencyCode` is present, ZATCA wants one tax total *with* subtotals
  and one *without*. It warns rather than refusing, which is how the first
  accepted document still had something wrong with it.

- **The QR timestamp has no `Z`.** ZATCA's QR specification shows one; its
  validator compares the value against `cbc:IssueDate` + `T` + `cbc:IssueTime`,
  which carries no zone, and answers `invoiceTimeStamp_QRCODE_INVALID`. The
  specification and the validator disagree, and the validator is the one that
  decides.

- **A buyer needs an address, so `sales::Customer` grew one.** ZATCA wants
  street, city and country on a standard invoice (BT-50, BT-52, BT-55) and warns
  without them. Optional and `#[serde(default)]`, so every invoice issued before
  the field existed still decodes — an absent address is exactly what those had,
  which is why it needs no upcaster. Snapshotted onto the invoice like the name,
  for the same reason.

- **And the template name was wrong.** Their CSR carries
  `PREZATCA-Code-Signing` where this build had `TSTZATCACA-Code-Signing` for
  both sandbox and simulation. The sandbox issues a certificate against any of
  the three, so the mistake would have surfaced at the first *simulation*
  onboarding and nowhere before it. Three environments, three template names,
  now pinned to their literals.

## The compliance checks, against the sandbox

Step 3 now passes: **all six documents accepted, no warnings and no errors** —
standard and simplified, invoice, credit note and debit note. Two more things
came out of running them, and neither was deducible from the specification.

- **A credit note is an `<Invoice>`, not a UBL `<CreditNote>`.** Generic UBL has
  a separate document type for one, and this build used it. ZATCA's schema is
  UBL's *Invoice* schema throughout: a credit note is an `<Invoice>` whose
  `cbc:InvoiceTypeCode` says 381, with `cac:InvoiceLine` and
  `cbc:InvoicedQuantity` like any other.

  The tell was the *shape* of the refusal. A `<CreditNote>` root came back as
  `HTTP 400 Invalid Request` — fifteen bytes of plain text from the gateway,
  before the validator ran, with nothing to say what was wrong. Every other
  rejection in this exercise was JSON naming a BR-KSA rule. **A refusal that
  does not name a rule means the document was never read**, which is a different
  class of problem and worth recognising on sight.

- **`BR-KSA-17`: the reason goes in KSA-10, which is
  `cac:PaymentMeans/cbc:InstructionNote`.** This build had it in `cbc:Note`,
  which is BT-22 — a general note, and not what the rule reads. Every credit and
  debit note was refused for it. `cac:PaymentMeans` also needs a
  `cbc:PaymentMeansCode`, which is not meaningful on a note and is required by
  UBL wherever the element appears at all.

  Only notes carry it: an ordinary invoice is accepted without one, and adding
  it there would be inventing a payment method nobody chose.

- **The generator is separable from the driver**, which is what made this
  quick. `compliance_submissions` builds, chains and signs the six with no
  database and no network; `pass_compliance_checks` loads the sealed credentials
  and submits them. The part that had to be right could be run against ZATCA on
  its own.

## Document-level discounts

- **A discount was a negative line, and that is invisible on the document.** The
  invoice showed a smaller total and nothing said why. ZATCA models one as
  `cac:AllowanceCharge` — an amount, a reason and the tax treatment it comes off
  — and prints it as its own figure, so a customer sees what they were charged
  *and* what they were let off. The demo had exactly this problem: an "Early
  settlement discount" line of −1,500.00 that no reader could distinguish from a
  refund or a mistake.

- **The tax comes off with it, which is the whole difference from a credit
  note.** A discounted invoice was never for the larger amount, so the smaller
  one is what is taxed and what is declared: 100.00 less 15.00 is taxed on
  85.00, and the tax is 12.75. A credit note, by contrast, reverses an invoice
  that really was issued for the larger amount. `total()` subtracts the
  discounts before it works out any tax, which is the one line that makes this
  true.

- **A discount names the band it comes off, and only that one.** UBL puts a tax
  category on the allowance itself, and it has to: discounting the exempt part
  of a mixed invoice must not reduce the tax on the standard-rated part, because
  that tax was charged. Discounting at a rate the invoice does not carry is
  refused outright (`DiscountWithoutABand`) — it would reclaim tax that never
  existed.

- **Three monetary totals where there used to be one number.**
  `LineExtensionAmount` is what the lines came to, `AllowanceTotalAmount` what
  was taken off, `TaxExclusiveAmount` what is taxed — and ZATCA checks they
  agree. `Totals` records what the lines came to rather than deriving it
  downstream, because a second computation is a second thing that has to match.

- **No upcaster, because absent means what it says.** `Totals::discount` is
  `Option<Money>` and `Issued::discounts` is `#[serde(default)]`, so an invoice
  issued before any of this existed decodes as one with no discount — which is
  what it was. An event written today with no discount is byte-identical to one
  written last year, and `no_discount_is_absent_rather_than_zero` decodes a
  verbatim older payload to prove it.

  `Option<Money>` rather than a zero because a `serde` default cannot see the
  rest of the struct, and there is no zero without a currency. Inventing a
  sentinel currency to have somewhere to put it would have been the other
  answer.

- **Confirmed against ZATCA.** A discounted invoice built and signed by this
  code is accepted by the sandbox with no warnings — the ninth document in
  `modules/tax_sa/tests/sandbox.rs` to be.

## Wiring, erasure, and paging

- **The ZATCA sweeps had no caller.** Signing, submission, the Fatoora client —
  all of it worked in tests and none of it ran in production: an invoice was
  issued, a document was built and chained, and then nothing happened to it ever
  again. They are worker jobs now, registered only when `SEALING_KEY` is set,
  because they read a tenant's private key and a job that runs and finds it can
  do nothing is quieter than one that was never registered.

  They go in `bin/worker.rs` — the composition root, beside `TrialBalance` — for
  the reason that file already gives: the kernel must not know what a ZATCA
  document is, and a module must not depend on the worker.

  `zatca_jobs()` is a function rather than two `with_job` calls for the same
  reason `module_jobs()` is: a test can look at what a deployment would run.
  `no_module_job_runs_for_tenants_that_declined_it` covers them, and it matters
  more here than for a projection — a submit job with no `module()` opens a
  connection to a tax authority for every tenant on the platform.

- **A five-year certificate is a deadline nobody has a reminder for.** Renewal
  needs a human — the taxpayer reads an OTP off the Fatoora portal — so nothing
  here can do it. What it can do is stop the lapse being a surprise:
  `CertificateExpiry` reports sixty days out. It parses the date the certificate
  states, in OpenSSL's format, where the day has a **leading space** for single
  digits — a `%d` parse would work for three weeks in four and report an
  unreadable certificate on the ninth of the month.

- **An identity that had ever acted could not be deleted at all.**
  `audit_entry`'s trigger refuses UPDATE, and its own foreign keys declare
  `ON DELETE SET NULL` — which is an UPDATE. The clause was unreachable from the
  day it was written and nothing noticed, because nothing had ever tried.

  Under Saudi PDPL that is a right that cannot be honoured, and "our schema will
  not let us" is not a lawful ground for refusing. The trigger now permits
  **exactly one** shape of update: one that nulls an actor and changes nothing
  else. The trail keeps what it is for and loses only the link to a person —
  which is the shape it has always had for a system-initiated action.

  What is deliberately absent is an endpoint. **Who may erase whom** is a policy
  question, and answering it in passing while fixing a schema bug would be
  answering it badly.

- **Lists silently truncated at 200.** A tenant with 201 invoices saw 200 and was
  told nothing; the response was indistinguishable from a complete one. Keyset
  paging on the columns each list is ordered by, an opaque cursor, and `next`
  absent meaning **the list ended**.

  Keyset rather than `OFFSET` because offset is wrong under concurrent writes:
  an invoice issued while somebody pages shifts every later row by one, so a row
  can be skipped or seen twice. The test pages five invoices two at a time, some
  sharing a tax point so the cursor's second part is what separates them, and
  asserts nothing was lost or repeated. Ignoring the cursor makes it fail with
  `INV-00005 came back twice`.

  A cursor this build cannot read is **refused**, not ignored — silently
  starting over would hand a caller the first page again and read as the list
  restarting.

- **`ModuleId` accepted what the database refused.** `tax-sa` constructed fine
  and failed at the moment a tenant enabled it, because
  `entitlement.module_id` is `^[a-z][a-z0-9_]{0,47}$`. The type carries that
  rule now, so a module id that cannot be stored is one that cannot be built.

- **The crash-test flake was mine.** `just clean-databases` drops with `FORCE`,
  and I ran it while a background suite was still going: a test's database
  vanished mid-run, its outbox came back empty, and two fault-injection tests
  failed an assertion about something else entirely. It took an afternoon to
  find because the failure names the wrong thing.

  The recipe refuses now when anything is connected to a test database. The
  hazard is real for anyone running the suite in one terminal and cleaning up in
  another.

## Modules ship their own routes

Four files — `ledger_routes.rs`, `sales_routes.rs`, `purchases_routes.rs`,
`tax_sa_routes.rs`, about 3,900 lines — lived in `erp-api`. So a module's HTTP
surface was written by something the module could not see, adding an endpoint
meant editing two crates, and "read the sales module" meant reading two
directories. They are `modules/*/src/http.rs` now, next to the aggregates and
read models they serve.

- **What was actually in the way was the furniture.** Extractors, problem+json,
  the JSON and query rejections, paging, the request-level messages — all in
  `erp-api`, which names every module, so a module reaching for `Json` or
  `require_module` would have closed a cycle. Those moved *down* into a new
  crate, `erp-web`, below the modules. What is left in `erp-api` is the core's
  own routes — sessions, the tenant, members, invitations, signup, module
  management — and the composition.

  The split falls where the architecture already said it did: `erp-web` holds no
  business domain (D11) and cannot be given one without becoming a module.

- **One list, two views.** `modules::REGISTERED` carries each module's
  `ModuleSetup` *and* its router. `available()` is the first view — what the
  control plane, the worker and the migrator read — and `mounted()` is the
  second. A module cannot be added to the platform and have its routes
  forgotten, because there is nowhere to add it that does not also mount them.

- **Authorization reads the path, and the path list moved.** `Allowed<C>` decides
  which role applies by taking the module out of `/v1/{module}/…`, and it used to
  check that segment against the *build's* module list — which is above
  `erp-web` now. It checks the **tenant's** list instead, which is the better
  answer: a segment naming a module the tenant does not have is judged on the
  tenant-wide role, exactly as `/v1/members` is, and the handler's own
  `require_module` then answers 404. A request for a module they do not have
  cannot reach data by any route, and the reply does not confirm what they are
  not paying for.

  What the old check bought — "no module's routes are silently judged
  tenant-wide" — is now `every_modules_routes_live_under_its_own_name`, which
  walks each module's own OpenAPI paths. Pointing one route at `/v1/posting-accounts`
  fails it. `no_two_modules_claim_the_same_path` turns a startup panic into a
  build failure with both module names in it.

- **The catalog had to be split, and getting it wrong was silent.** A module
  renders its failures through a composite of its own catalog, its dependencies',
  and `erp_web::CATALOG`; `erp_api::CATALOG` is the complete union and is still
  what `docs/ERRORS.md` comes from.

  `ApiError::into_problem` rendered through a fixed catalog, which was fine while
  every caller was in one crate. After the move, `POST /v1/ledger/entries` with
  an unbalanced entry answered `"detail": "ledger.does_not_balance"` — the bare
  code, no sentence. `an_unbalanced_entry_is_refused_with_the_difference` caught
  it.

  `into_problem` takes the catalog now, and **there is no `IntoResponse for
  ApiError`**: it could not name one, so `?` on an `ApiError` in a handler would
  have taken the same wrong turn just as quietly. Nothing was using it.

- **`docs/openapi.json` is byte-identical** across the move but for one
  description, which was a stale comment about paging that does not exist any
  more. Nothing a client can see changed.

## A module seeds separately from its DDL

The Saudi VAT rate rode on `tax_sa`'s schema install, because that was the only
hook a module had. It worked — the insert is `ON CONFLICT DO NOTHING`, so
re-running it is harmless — and it made two different things look like one: a
tenant's *data* written by a step named "install schema".

`just prepare` was already somewhere that mattered. It installs every module's
DDL into a throwaway type-check database, where a `configuration` row is noise;
it globbed `schema/*.sql` and would have picked up a seed file too. It runs
`install.sql` only now, which is a distinction the recipe can only make because
the two are separate files.

`ModuleSetup::seeding(sql)`, run after the install and under the same
`search_path` — so a seed can write both the module's own tables and the
tenant's `public` ones, which is what the rate does.

`a_modules_seed_runs_when_a_tenant_gets_the_module` proves the ordering by
inserting into a table the DDL creates: run it first and it fails on a missing
relation. `a_rebuild_seeds_again_without_overwriting_the_tenants_own_value`
covers the other half — a refresh drops the module's schema, so its seed has to
run again, and the tenant's `configuration` is *not* dropped, so it meets a row
it already wrote. Skipping the seed step fails both.

The existing `enabling_the_module_seeds_the_saudi_rate` did **not** catch it: it
calls `tax_sa::install()`, a test helper that had its own copy of the two
statements. That helper reads `setup()` now, so it installs what production
installs.

## `requires` learns "at least one of"

`tax_sa` computes a VAT return, which nets output tax against input tax. It
needs a source for one side or the other and does not care which: a business
that only sells still files, and so does one that has bought but not yet sold.

`requires` is an AND list, so neither answer was available. Naming both would
force a shop with no supplier bills to enable `purchases` in order to declare
tax they do owe. So `tax_sa` named **neither**, with a comment saying what it
actually meant — and that let a tenant turn on a VAT return with nothing on
either side, and disable the last module feeding it without a word.

`ModuleSetup::requires_any`: one group, satisfied by any member.
`.requiring(&["ledger"]).requiring_any(&["sales", "purchases"])` is now the whole
sentence. One group and not a list of groups — "ledger AND (sales OR purchases)"
is what this system needs, a second disjunction has no consumer, and the nested
shape can arrive with the module that wants one.

`ledger` is named explicitly even though either alternative brings it: `tax_sa`
reads `ledger::Rates` itself, and a dependency relied on directly is one to
declare rather than inherit.

**The interesting half is disabling.** A tenant with sales, purchases and
`tax_sa` may turn either side off; the *second* one is refused, because a return
with nothing on either side is not a downgrade, it is a module that cannot
answer. `dependent_on` asks whether `name` is the last enabled member of a
dependent's `requires_any`, which is three distinct ways to be wrong:

- skip the enable check → `tax_sa` goes on with only the ledger
- treat `requires_any` as AND when disabling → *neither* side can ever be turned
  off, which is worse than the bug being fixed
- ignore `requires_any` when disabling → the last side goes and the return is
  left mute

Each fails `one_of_several_is_enough_and_none_of_them_is_not` with its own
message. `a_module_needing_one_of_several_takes_either_and_refuses_neither`
walks the same path over HTTP, in Arabic, and asserts the refusal reads *one of*
— `request.module_requires_one_of` is a separate code from
`request.module_requires`, because "needs sales, purchases" and "needs at least
one of sales, purchases" are different sentences and a client rendering its own
has to be able to tell them apart.

The list arrives in `args.required` comma-separated. A comma separates a list in
both English and Arabic, so neither language needs a conjunction built in the
code — which is how "sales و purchases" ends up in an English sentence.

`GET /v1/catalogue` and `GET /v1/modules` both carry `requires_any`, so a picker
can grey out what is impossible rather than let somebody discover it.

## The first outbox handler, and the plane it had to go in

The outbox was finished in Phase 2 and joined to nothing for the whole of Phases
3 and 4. Effects as values, claim under `SKIP LOCKED`, leases, exponential
backoff, dead letters, the at-least-once idempotency key, crash tests proving a
lost delivery record replays with the *same* key — all of it real, all of it
tested, and **no producer anywhere in the product and no registered handler**.
An effect enqueued by hand in a test was the only effect this system had ever
seen.

Email is the first handler. Invitations are the first producer.

- **The outbox was in the wrong database for its first real user.** It was built
  where commands and events are, which is a tenant database. But the things that
  most need to reach the outside world do not happen in a tenant database at all:
  an invitation is a control-plane row, and so is a signup, and so is a password
  reset.

  Writing the invitation in the control plane and its email into the tenant's
  outbox would be two databases, therefore two transactions, therefore a window
  where the invitation exists and the email was never promised — with nothing
  recording that it was owed. That window is the exact thing D9 exists to close,
  so closing it in one plane and leaving it open in the other is not a design.

  So the control plane has an outbox now, and it is **byte-for-byte** the tenant
  one: `Dispatcher` and `enqueue` are compile-time-checked against a table named
  `outbox` with those columns, and reusing them costs one obligation — the two
  files must not drift. Nothing in the compiler can see that obligation, because
  sqlx validates against a single type-check database where the two are the same
  table. A column added to one chain would type-check perfectly and fail at
  runtime in the other plane. `the_two_outboxes_are_the_same_table` compares them
  column by column and constraint by constraint; adding one column to either
  fails it.

  `just prepare` now loads the tenant chain **first**, so the tenant definition is
  the one sqlx checks against and the control migration is a no-op there.

- **`PlatformJob`, because a `Job` is handed a `TenantDb`.** The control-plane
  outbox has no tenant. Running it as a per-tenant job would be *safe* —
  `SKIP LOCKED` sees to that — and still wrong: the work would scale with the
  number of tenants rather than with the amount of work, under a tenant's lease,
  N times a cycle.

  It runs once per claim cycle, inline, **before** tenants are claimed, so an
  idle deployment with nothing due still pumps the queue. A failure is logged and
  stepped over rather than propagated: one unreachable relay must not stall every
  projection on the fleet.

- **SMTP, not a vendor's HTTP API.** Every provider speaks it and so does a
  Postfix somebody runs themselves, which for a self-managed fleet means changing
  provider is one environment variable rather than a code change. `lettre` on
  `native-tls` — the OpenSSL sqlx already links for Postgres TLS, the same call
  made for `reqwest`, and the reason this build has one TLS stack rather than
  two. Not hand-rolled, because an Arabic subject line needs RFC 2047
  encoded-words and getting that subtly wrong produces mojibake in exactly the
  clients this market uses.

- **The interesting part of the handler is not SMTP.** It is which failures are
  worth retrying. A refused connection, a TLS handshake, a 4xx greylisting, a
  timeout — the relay having a moment, and dead-lettering an invitation because a
  mail server was restarting would be losing it. A 5xx, or an address that will
  not parse — permanent, and retrying spends four attempts and two minutes of
  backoff to arrive at the same answer. A payload this build cannot read is
  permanent too, rather than retried three times first with a misleading error
  left on the row.

  That is why there is a `Mailer` trait: a test of it has to make the transport
  fail on demand, and must never send real mail by accident.

- **The text is rendered when the invitation is written, not when it is sent.**
  L5: the effect records a resolved decision. The recipient has no account and
  therefore no stored language, so what exists at the moment of inviting — the
  language the *inviter* was working in — is the best signal there will ever be,
  and it is gone by the time a worker picks the row up. It also means editing the
  catalog does not silently change what an already-issued invitation says, which
  is the same reason an invoice stores its VAT rate.

- **No relay configured is not an error.** With `SMTP_URL` unset the handler is
  not registered, and an effect whose kind has no handler is *not claimed* — so
  the email waits as an undelivered promise rather than being attempted and given
  up on. Configure a relay a month later and everything already promised goes
  out. Same call `SEALING_KEY` makes for the ZATCA sweeps.

- **What the tests pin.** `inviting_somebody_promises_them_an_email` asserts the
  row is there after the request, in Arabic, carrying the invitation's own token
  — and then *uses* that token against `/v1/join/`, because a body with a
  plausible-looking URL that 404s is worse than no email.
  `an_invitation_is_promised_by_the_control_plane_and_delivered_by_the_worker`
  runs the platform pass and asserts the message reached the mailer and the row
  says delivered, then runs it again and asserts nothing is sent twice.

  Falsified: removing the `enqueue` empties the outbox; a batch limit of zero
  never delivers; an unregistered handler claims nothing; a drifted column fails
  the parity test.

- **A second `git checkout` cost a file.** `git checkout <path>` restores from
  the *index*, and a test appended but never staged is not in the index — so
  reverting a deliberate falsification deleted the test along with it. Second
  time this session. The habit is now `git add -A` before falsifying anything.

## Containers, a standby, and the cache that had nobody to talk to

Three things that turned out to be one thing: **every one of them was a seam
written for a second instance, in a system that had only ever run one.**

### The replica routing was reachable only from a unit test

`TenantDb::read` has routed to a replica since Phase 1 — "adding replicas later
is a configuration change, not a code change" — and `ClusterRegistry::with_replica`
has existed just as long. **No binary ever called it.** Five composition roots,
each registering a primary and stopping there, so the entire read path was
exercised by one unit test and nothing else.

`ClusterRegistry::from_env` now reads `PRIMARY_CLUSTER_URL` and
`PRIMARY_REPLICA_URL`, and all five binaries use it. Two things came out of
writing it:

- **`with_replica` on an unknown cluster was a silent no-op.** `with_replica("primry", …)`
  returned `Ok` with the replica dropped: every read would go to the primary, the
  deploy would look correct, and the only symptom would be a primary carrying
  twice the load somebody sized it for. It is an error now, and the error names
  what was not found.
- **Blank is absent.** A compose file that declares the variable and leaves it
  empty is the ordinary way to say "no replica here"; treating `""` as a URL
  fails at parse with a message about nothing.

`from_urls` is `from_env` with the environment already read, so the decision is
testable without `set_var` — which this workspace could not do anyway, because
it denies `unsafe`, and which races every other test in the same binary
regardless.

### Redis is not a second cache. It is agreement between nodes.

The obvious reading of "cache the hot paths with Redis" is wrong here, and the
existing code says why: the entry-path cache answers from process memory at a
99.9% hit rate, and moving it to Redis would replace a memory read with a network
round trip. It stays exactly where it is.

What Redis buys is the two things a per-process cache structurally cannot do.

**Sessions.** `ControlPlane::session` runs on every authenticated request and was
the one hot lookup with no cache at all — deliberately, for a reason `cache.rs`
states plainly: *a stale membership for five seconds is survivable, a stale
logout is not*. So the busiest query in the system went to the control database
every time, the database that cannot be sharded. A **shared** cache resolves the
objection, because a logout deletes the entry for every node at once.

**Invalidation.** The entry caches invalidate locally on write. `cache.rs` named
the fix when it was written — "out-of-band invalidation, which is a Phase 3
decision" — and this is it. Every `invalidate` in the crate now goes through one
`forget`, which drops the key locally and publishes what changed; every node
applies what it receives.

The failure policy is the part worth arguing about, and it is: **every path
degrades to exactly the behaviour of the build before this existed.** A session
read falls through to Postgres. A write is skipped. An invalidation that cannot
be published still happened locally, so other nodes fall back to the TTL window
that was always documented. That is not L6 being bent — L6 is about not degrading
a *guarantee*, and none of these was a guarantee without a documented bound.

The one exception is stated where it lives: a logout that cannot reach Redis
leaves that token usable until the cached entry expires. `SESSION_TTL` is one
minute, and it is the blast radius of that failure rather than a performance
knob.

Two smaller findings:

- **serde cannot internally tag a newtype variant wrapping a string**, and half
  the `Invalidate` variants are exactly that. Adjacent tagging
  (`{"what":…,"which":…}`) instead. The shape matters more than it looks: during
  a rolling deploy two builds are live, and a message an old node cannot read has
  to be a loud failure rather than a silently ignored one — silently ignored is a
  node serving stale authorization with nothing in the log to say so.
- **The subscriber holds a `Weak`.** A background task with a strong
  `Arc<ControlPlane>` would keep the pools open through shutdown and the drain
  would never finish.

The tests run **two `ControlPlane`s over one control database**, which is what two
API replicas are. Reverting `forget` to a local `invalidate` fails
`a_role_change_on_one_node_reaches_the_others`; dropping the Redis delete from
`log_out` fails `a_logout_on_one_node_ends_the_session_on_the_other`.

One test of mine was wrong before the code was: I demoted with `grant_membership`,
which **deliberately refuses to change a live member's role** because doing so
would be a way around the last-owner guard. The database was right and my
assertion was not.

### The compose file is the point, not the Dockerfile

One image with five binaries, because five images are five things to keep at one
version and "the worker is a deploy behind the API" is a failure this system has
a pre-deploy gate for. No `ENTRYPOINT`, so `docker run erp worker` puts the
worker at PID 1 and SIGTERM reaches it directly — which the graceful shutdown and
the lease-releasing drain are both written against.

Cache mounts rather than the usual dummy-sources-then-real-sources trick. That
trick works by making cargo believe the dependency layer is current, and it fails
quietly: a stale `liberp_control.rlib` built from an empty `lib.rs` links cleanly
and contains none of the code.

What the stack runs is deliberately not one of everything — two API replicas, two
workers, and a streaming standby — because one of anything tests none of what the
last two sections were about.

### What running it actually found

Four bugs, none of which any test could have caught, because every one of them
was about a second instance or a real service:

- **Postgres 18's image moved the data directory.** Mounting the old
  `/var/lib/postgresql/data` makes it refuse to start with a long message about
  finding data in an unused volume. The mount is `/var/lib/postgresql` and the
  cluster lives in a version subdirectory, so `pg_upgrade --link` can cross
  versions without a mount boundary in the way.
- **`pg_basebackup` was refused: `no pg_hba.conf entry for replication`.** The
  image writes a `pg_hba.conf` for ordinary connections and not for replication
  ones. There is no environment variable for it and it is not an `ALTER SYSTEM`
  setting, so it goes in `/docker-entrypoint-initdb.d`. Scoped to private
  ranges, not `all`.
- **`smtp://…?tls=none` is not a thing.** lettre refuses it at start-up; plain
  SMTP is `smtp://host:port` with no `tls` parameter at all. This was in the
  compose file *and* in `docs/RUNNING.md`, written from memory and wrong in
  both. A four-line probe over `Smtp::new` settled which forms are accepted.
- **Two API replicas cannot both publish one host port.** The one that lost the
  race exited with a networking error. That is why there is an nginx in front:
  without it two replicas is a fiction, because every request would land on
  whichever container won. It also has to pass `Host` through unchanged — the
  tenant *is* the subdomain, and nginx rewrites `Host` to the upstream name by
  default, which would turn every tenant-scoped request into a 404.

And one gap in the product rather than the packaging: **`migrator` never applied
the control-plane migrations.** Nothing but `erp_demo::bootstrap` ever called
`ControlPlane::migrate`, so a fresh deployment could only get its control schema
by building a demo tenant first — backwards for the thing this document calls
the deploy step. It applies them now, in the apply mode only: `check` and
`versions` are the pre-deploy gates and are look-only by contract, and a gate
that writes is one you cannot run against production before deciding to deploy.

### Verified against the running stack, not asserted

- `pg_stat_replication` on the primary reports `walreceiver | streaming | async`;
  `pg_is_in_recovery()` on the standby is `t`.
- The demo builds through the containers: 6 invoices, 4 bills, a filed VAT
  return, 7 ZATCA documents chained.
- A session created through the proxy is a `erp:session:…` key in Redis; logging
  out once makes **every** subsequent request 401 regardless of which replica
  answers.
- Inviting somebody in Arabic puts a message in Mailpit with the subject
  correctly RFC 2047 encoded and the bidi isolation marks around the Latin part
  of the company name — and the link in that body, pasted back at
  `GET /v1/join/…`, returns the invitation.
- `PATCH /v1/members/{id}` publishes
  `{"what":"membership","which":{"identity":…,"tenant":…}}` on `erp:invalidate`,
  and both replicas answer with the new role immediately afterwards.

## Standards for a pooler, and three costs it made visible

Read Supavisor's two write-ups first, on the principle that setting standards now
is cheaper than a rewrite later. The load-bearing facts: **transaction mode** is
the mode, 400 direct connections served 250,000 clients, one node holds the
direct connections per database while others relay, reads spread across the
cluster and writes go to the primary, and prepared statements work by parsing SQL
and broadcasting `PREPARE` across the pool.

The audit that followed was the most useful hour of it. Transaction pooling
forbids session state surviving between transactions, so:

| hazard | found |
|---|---|
| `SET search_path` at session scope | **12 sites, every one DDL or install** |
| session advisory locks | only `erp-testkit` |
| `LISTEN`/`NOTIFY` | none — D4 banned it years ago |
| temp tables, `WITH HOLD` cursors | none |

The projection hot path already used `SET LOCAL`, deliberately and with a comment
saying why. So the codebase was one decision away from pooler-ready, and the
decision is the one Supabase ships: **two connection strings per cluster.**

- **`Role::Direct`, and `PRIMARY_DIRECT_URL` behind it.** Provisioning, fleet
  migration and schema rebuilds ask for it; everything else goes through the
  primary, which may be a pooler. **It falls back to the primary when unset**,
  which is what makes this a variable rather than a flag day — nothing changes
  for a deployment that never adopts one.

  `maintenance_options` is the one line that did it, because every DDL path in
  the system already went through that function.

- **The rule is a test, not a comment.** `tests/pooler.rs` walks every `.rs` and
  `.sql` in the workspace and fails on a session-scoped `SET`, a session advisory
  lock, or a `LISTEN` outside an allow-list of DDL files. Changing the projection
  runner back to plain `SET` fails it; adding a `pg_notify` anywhere fails it.

  Its first run found **itself** — the file names every pattern it hunts for.
  Which at least proved the walk reaches that far.

- **`POOL_STATEMENT_CACHE`.** sqlx prepares by default and caches per connection.
  Poolers answer this differently and both answers are the pooler's business; what
  this crate owes a deployment is a knob it can turn without a rebuild.

### And three things the same audit exposed

**Lane budgets were compiled constants.** 100 + 240 + 60 = 400 per process, and
four processes make 1,600 against a 200-connection server. Nothing had ever
failed, because the per-tenant pool cap hid it. They read from the environment
now, and `report_budget` states the arithmetic at start-up — which promptly
warned on the compose stack, exactly as intended, until the stack was sized:
`this_process_at_most: 30, server_max_connections: 200`.

**The fleet walk was sequential**, and its own comment conceded it. Bounded
concurrency now, `FLEET_CONCURRENCY` (16). Measured over 40 tenants:

| concurrency | elapsed |
|---|---|
| 1 | 225 ms |
| 4 | 67 ms |
| 16 | 64 ms |
| 32 | 36 ms |

Bounded and not unbounded because each visit opens a connection on the **direct**
route — the one that bypasses any pooler, and therefore the one with the smallest
budget.

**A quiet tenant was visited for ever at a fixed thirty seconds.** Five thousand
tenants is 167 visits a second, in perpetuity, almost all finding nothing — and
each one opens a connection, runs every enabled module's projection query, and
writes a row back. It was the largest standing cost the platform had, spent
entirely on tenants doing nothing.

Consecutive idle visits now back off exponentially to a six-hour cap:

```text
100 active + 4,900 dormant  ≈  3.5 visits/s   (was 167)
```

Three details that are not obvious:

- **Waking was already built.** `request_visit` pulls `next_visit_at` back to
  now and every write calls it, so a dormant tenant that receives a request is
  current within a claim cycle. Without that the backoff would be a latency bug
  rather than a saving, which is what `a_request_wakes_a_dormant_tenant_immediately`
  is for.
- **Jitter has to scale with the interval.** Ten seconds of spread across a
  six-hour interval leaves five thousand tenants landing in the same ten-second
  window every six hours — a thundering herd with a long fuse, and one that only
  shows up in production.
- **The streak is not on `Tenant`.** How many times a scheduler looked and found
  nothing is the scheduler's business; a domain model carrying it invites code
  that reads it for something else. `Claimed { tenant, idle_visits }` instead.

### Two more gaps found by running it

**`migrator` never registered a cluster.** Only `erp_demo::bootstrap` ever called
`register_cluster`, so a fresh compose stack had migrations applied, an empty
`cluster` table, and every signup failing with a 500 that said
`no cluster has capacity (0 at their limit)` — which names a capacity problem and
is a missing row. Same shape as the control migrations, found the same way: bring
it up clean and post a signup. It declares `primary` now, with a capacity that
says in the log that it is a placeholder to be sized from measurement.

**A falsification that did not falsify.** Editing `leases.rs` by pattern found
nothing, because `cargo fmt` had split the line across three; the test passed and
`git checkout` reported "Updated 0 paths", which is the tell. Worth knowing: a
scripted edit that reports success and a `git checkout` that reports no change
are contradictory, and the second one is right.

## Thirty-six findings, ten roots, and the guards that hold them

Written 2026-09-05, after the whole-codebase review in
[REVIEW.md](./REVIEW.md) and the pass that fixed it. The review found
thirty-six things; sorting them by what they had in common gave ten roots, and
the fixes were made at the root rather than at the symptom, each with a test
that was **falsified** — the fix reverted, the test watched fail, the fix
restored — before it counted. The status table at the top of REVIEW.md says
which test guards which finding.

### The roots, and what each became

**No identity for an unauthenticated request** (A1–A3). Every public and
sign-in route now goes through `Anonymous` or `Public`, both of which know the
caller's address — the last hop of `X-Forwarded-For` when the deployment says
to trust it, the socket otherwise — and charge a limit against it before doing
any work. Limits are `Limit { count, window }` constants in `erp_web::rate`,
per caller and per target (a handle, a phone number, a tenant), and counted in
Redis when it is there so a second pod does not double them. The contract test
walks every operation in the OpenAPI document and refuses any unauthenticated
one that is not limited; the two that are unbounded by design are named.

**Liveness state that was never refreshed** (B1–B3). A claim on a tenant now
pushes its `next_visit_at` out to the lease end, the visit renews the lease
between jobs and stops if it has lost it, and one failing job no longer stalls
the rest. B1 was filed as *plausible*; the falsification produced the double
visit, so it was real.

**"Recorded" standing in for "done"** (C1, B7). A gateway refund is a request
the worker takes to the gateway and settles from what the gateway says, the
same shape as a saved-card charge. A settled deposit is repaired into `booking`
from `payments::settled_advances` on every visit, so hold expiry can no longer
cancel a paid booking.

**Irreversible actions taken from lagging state** (C7). The deposit amount
comes from the payment aggregate, not the projection; the kept net is
apportioned from the advance rather than re-taxed at today's rate; and one
deposit per booking is a `payments` aggregate keyed on the thing the deposit is
against, so two tabs cannot both pay.

**Time facts without an anchor** (C2). Filing a VAT return closes the ledger
through the end of the period in the same transaction, so a backdated document
into a filed quarter is a refusal rather than a silent change to the return.

**Promises without a compiler** (E1, E2, A11, D6). `PUT /v1/booking/public-settings`
exists, bounds the deposit at the whole price, and is in the role matrix;
unknown routes and wrong methods answer in `problem+json`; the stale docs say
what the code does.

**Nothing forgets** (D2, A9). `erp_worker::Retention` is a kernel job every
tenant gets: delivered effects at thirty days, webhook payloads at ninety,
past occupancy claims at a hundred and eighty, dead short links at thirty past
their death; `SweepSessions` is the control-plane half. Nothing pending, dead
or permanent is ever swept, and the worker test plants one old and one young
row of each kind to prove which side of the line each falls.

**Failure handling that did too much or too little** (B4, A10). A dead letter
is a queue, not a grave: `GET /v1/effects/dead` lists them and
`POST /v1/effects/dead/{id}/requeue` puts one back with its attempts reset and
its idempotency key intact; the default retry schedule is sixteen attempts
capped at an hour, so a provider outage over lunch is not a permanent loss. An
OTP attempts update that fails now refuses the guess instead of allowing
unlimited guessing.

**The till and the prepaid ledger one step short** (C4–C6). A return names its
lines and credits only those, and its tenders must come to what the lines
credit, read off the credit note `sales` issued rather than recomputed; a
closed till refuses a return the way it refuses a sale; a package with zero
uses is refused.

**Settings with no optimistic concurrency** (D1). `configuration::set` takes
the version the caller read and refuses with `Conflict` if the key has moved.
Every settings `GET` answers with the version as `ETag`; every settings `PUT`
takes `If-Match` and answers `412` when it is stale — `erp_web::IfMatch`,
`erp_web::Versioned`, and one `config_problem` mapping for all of them.
Templates, which share one key, do the compare-and-swap internally and retry.

### Guards that name the seam rather than the symptom

Two tests are about facts one crate states about another. `files` names the
event-log domain of every record a document can go on as a string, because
depending on six modules for one fact each would make it require all of them;
`every_owner_kind_names_the_domain_its_module_uses` in `erp-api` — the one
crate with everything in scope — pins each string to the aggregate that owns
it. `recipient_exists` and `owner_exists` both read the log rather than a
projection, so a customer registered a moment ago is somebody already.

### The second pass (2026-09-06)

The seven that were left open were closed the next day, each with a guard:

- **The tenant's clock is a kernel type** (C3, C8). `erp_types::Calendar` —
  an IANA zone, `Asia/Riyadh` by default, settable at `PUT /v1/tenant/calendar`,
  with daylight saving where the zone has it —
  is stamped onto every event's metadata by `append`, so a projection reads
  the clock an event was written under (`ctx.calendar()`) and a rebuild
  reproduces what was live after the setting changes; a command reads it from
  configuration. Tax periods are local dates on the wire; reports' months, the
  till's, payroll's posting date, messages' times, ZATCA's `IssueDate` and the
  rota's day all go through it. Two source-scanning tests keep it so:
  `an_instant_becomes_a_day_only_through_the_calendar` refuses `date_naive()`,
  `and_hms`, and `format("%Y-%m…")` anywhere but the calendar itself, and
  `no_module_reads_the_wall_clock` refuses `Utc::now()` outside a module's
  HTTP layer.
- **A domain is proved, not declared** (A6, A7). Claiming a domain names a
  DNS TXT record; `verify_domain` resolves it (behind a `DomainProver`, so
  tests publish to a fake) and only a match proves the zone. An origin must be
  `https://<host>[:port]` under a proved domain. A proved domain then serves
  the whole API on any host under it — `tenant_by_host` beside `tenant_by_slug`
  — and CORS offers the session, `If-Match`, `X-Branch` and every method to
  those origins, because they are the tenant's own app.
- **A slow delivery keeps its lease** (B6). Every claim mints a `leased_by`
  token; a heartbeat renews the lease every third of it while the handler runs,
  and settlement is conditional on still holding it. A second dispatcher
  polling through a one-second delivery on a 300 ms lease claims nothing.
- **The compatibility gate sees every branch** (D4): the cycle guard is the
  path, not the walk.
- **The append ceiling is a number** (D5): about 470 appends/s per tenant on
  the development machine, printed by `throughput.rs` and recorded under L1.
- `ERRORS.md`, its drift test and `just errors` are gone by decision; the
  catalogs themselves stay test-guarded. E3 stays open by decision.

### The second pass's own findings (§G, 2026-09-06)

The parts the first pass did not read — the payment adapters, the message
transports, the reports projections, `erp-i18n`, the demo — got eighteen
findings and the same treatment. What each became:

- **A provider reports to the hook, and the hook reads what it sends** (G1).
  `Returns` gained `notification`, which is `POST /v1/hooks/<provider>` on the
  tenant's host and never a page a person lands on; Tamara sends it when it is
  given. Tamara talks back in two shapes under one token — a registered
  webhook's `{order_id, event_type, data}` and a checkout notification's
  `{order_id, order_status}`; its own SDK reads them through two services —
  and neither carries an `id`. So `erp_payments::authenticate` now answers a
  `Callback { payment, event, kind }`: the adapter that read the body names the
  delivery (Moyasar's event number; Tabby's payment and status; Tamara's order,
  what was said, and the capture or refund id), and the hook route stops
  guessing at a gateway's body. Both Tamara bodies land, each is its own
  delivery, and a resend of either is a duplicate.
- **One meaning for a refusal** (G2, G7). `erp_payments::refusal` decides once
  for every gateway: `401`/`403` are the account, a `404` *about a named
  payment* is the only absence, `408`/`429`/`5xx` are the moment. The two
  BNPL adapters had read every `4xx` as "no such payment", which is what the
  saved-card sweep charges again on. `messaging::transport::worth_retrying`
  does the same for the three transports, so a rate-limited relay is retried
  rather than dead-lettered.
- **Paid means what was captured** (G3), and **a refund carries its own key**
  (G4): `Gateway::capture` and `refund` take the caller's reference, the sweep
  sends `<payment>.<reference>`, Tabby puts it in `reference_id` — its only
  idempotency key — and Tamara in the refund's `comment`, which is the only
  field of the merchant's it keeps. Moyasar has nowhere for it and says so.
- **The checkout bodies say what the lenders ask** (G5, G6). `Buyer` carries
  `registered_since` and `purchases`, `Basket` a `deliver_to`, `Item` a
  `category`; Tabby's `buyer_history` and `shipping_address` are built from
  them rather than sent empty, and Tamara's lines total the unit price times
  the quantity and each carry an id of their own. Asked of the caller rather
  than invented in an adapter, because a placeholder is one more thing the
  lender scores.
- **A phone number is read in one place** (G9). `erp_types::phone::normalise`
  is the rule the control plane's codes, the booking door and the SMS
  transport all read by; the transport had refused the dashes the other two
  stripped. `a_phone_number_is_read_in_one_place` refuses a fourth copy.
- **A refused FCM token is forgotten** (G8), so the retry mints another.
- **Credit notes are documents to the report** (G11, G14). `Revenue` takes a
  partial credit's lines out in the period the credit was dated to; `credited`
  is one row per credit note, and the reconciliation lists invoices and credit
  notes together and checks each against the entry it posted. Doing that found
  the name was wrong: `sales` posts a credit's entry as
  `cn.<invoice>.<reference>` — the number is minted inside the transaction the
  entry is posted in — and the report had looked for the statutory number, in
  a column nothing read.
- **A booking is one booking** (G12, G13). `held` is one row per resource with
  every line's minutes and the notice it was counted with; a reschedule gives
  the old month back its `booked` and its lead and counts the new one, with
  the notice measured from the move.
- **`q=0` is a refusal** (G15) and **an argument's own direction decides its
  isolation** (G16): an Arabic name in an English sentence is isolated, read
  off the Unicode bidi classes rather than a list of scripts.
- **The demo's colleague has a password of her own** (G17), printed with the
  rest, and **the demo starts no payment the gateway never issued** (G18): it
  asks for a saved-card charge, which is a state this system owns.
