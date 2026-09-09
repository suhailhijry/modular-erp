# Ambiguities — things I did not decide

Started 2026-09-09, on instruction: *"don't assume when something is tricky or
ambiguous, keep a log of ambiguous findings"*, refined to *"decide sensibly, and
when things are TRULY ambiguous, record it and skip it"* and *"when something
needs verification … research it, look it up on the internet. And if it's still
undecidable, then log it and continue with other work."*

Every entry names what is unclear, what I did **instead** of guessing, and what
it would take to settle. Nothing here is blocking — each was worked around in a
way that is reversible.

**The standing rule I applied:** if a question could be a tenant **setting**, it
is one. A setting is not a way of avoiding a decision; it is the right answer
whenever two tenants could legitimately want different things. What is logged
here is what a setting *cannot* answer.

---

## 0 · Research log — what was looked up, and what it settled

| Question | Researched | Outcome |
|---|---|---|
| Taqnyat DLR callback payload | `dev.taqnyat.sa/ar/doc/sms/` (primary source) + search | **Still undecidable.** The primary source documents *that* a webhook exists and how to configure it, and does not document a single field name, type or status value. See §1, §1b — feature skipped |
| ZATCA VAT categories and VATEX-SA exemption codes | Vertex e-invoicing docs + ClearTax + ZATCA rule discussion | **Decided.** Cross-confirmed by two independent sources; built (§50 of the implementation plan) |
| TOTP algorithm and test vectors | RFC 6238 / RFC 4226 / RFC 4648, published vectors | **Decided.** Specified to the byte with vectors the build now runs; built (§51) |
| Is there a SOCPA or Saudi-mandated chart of accounts? | IFRS Foundation jurisdiction profile, IFAC, SOCPA guidance | **Decided: there is none.** SOCPA endorses IFRS and IFRS for SMEs — accounting *standards*, not a chart — and Saudi Arabia mandates no chart of accounts for private companies. The plan's "SOCPA-aligned" template was therefore dropped rather than invented; see §7 |

### What the ZATCA research settled

**Four tax categories**, not three. This build has `Standard | Zero | Exempt`;
ZATCA also defines **`O` — services outside the scope of tax**. Not added — §4.

**The exemption reason codes**, with the category each belongs to:

| Category | Code | Meaning |
|---|---|---|
| **E** | `VATEX-SA-29` | Financial services (VAT Regulations, Article 29) |
| **E** | `VATEX-SA-29-7` | Life insurance services (Article 29) |
| **E** | **`VATEX-SA-30`** | **Real estate transactions (Article 30)** — the property vertical's code |
| Z | `VATEX-SA-32` / `-33` | Export of goods / of services |
| Z | `VATEX-SA-34-1` … `-34-5` | International transport |
| Z | `VATEX-SA-35` / `-36` | Medicines and medical equipment / qualifying metals |
| Z | `VATEX-SA-EDU` / `-HEA` | Private education / healthcare to a citizen |
| Z | `VATEX-SA-MLTRY` / `-DIPLOMAT` | Qualified military goods / diplomatic |
| O | `VATEX-SA-OOS` | Free text, given by the taxpayer case by case |

**The codes carry no vendor prefix.** Vertex's documentation shows
`VRBL:SA:VATEX-SA-30`; the `VRBL:SA:` part is Vertex's own namespace and is not
in the XML ZATCA receives.

**One rule worth carrying forward**, and it validates the party-model work:
> if the exemption reason code is `VATEX-SA-EDU` or `VATEX-SA-HEA`, the buyer's
> other ID (BT-46) is **mandatory and must be a national ID**.

That is exactly the `Identification { scheme: NationalId }` field designed in
`docs/superpowers/specs/2026-09-09-party-model-design.md`, arrived at
independently for Ejar. Two unrelated obligations want the same field.

---

## 1 · Which providers actually send delivery receipts, and in what shape

**Where:** Phase 11, messaging delivery receipts.

**The ambiguity.** A receipt's payload shape is the provider's, and none is
verifiable from here:

- **Taqnyat (SMS)** — sends a DLR, but the callback body, the field naming the
  provider's message id, and the status vocabulary are undocumented at the
  primary source. Checked directly, not assumed.
- **FCM (push)** — sends no delivery callback in the usual sense.
- **Email** — a bounce arrives against a `Message-ID`, and there is no email
  transport in this build to attach one to.
- **WhatsApp** — Meta sends `sent`/`delivered`/`read`, through a Business API
  account nobody here has.

**To settle it:** one real callback from each provider, captured verbatim.

## 1b · …and therefore delivery receipts is **skipped**, not half-built

**The deciding fact, found while designing it.** Correlating a receipt needs the
provider's own message id, and `Transport::send` discards it. Capturing it is not
a messaging change: an effect handler is given **no database connection** by
design (`crates/erp-eventlog/src/outbox/dispatch.rs` —
`deliver(&self, effect) -> Result<(), DeliveryError>`), so the only component
that *could* record what a provider returned is the dispatcher. That means
changing `EffectHandler`, a **kernel trait with five implementors**, plus a new
outbox column.

**Why that settles it.** The kernel change would exist solely to feed provider
adapters that §1 says cannot be written correctly. If a real callback later
forces a different correlation, the kernel change was wrong — and it is the most
expensive kind of wrong to undo, because four unrelated handlers moved with it.

**No partial mechanism built either.** Building the correlation column and the
landing sweep with nothing able to feed them is unexercised machinery, which
this codebase has been bitten by twice ("What needs work now", item 1).

---

## 2 · Whether a failed delivery should reach a person, and whom

**The ambiguity.** A rent reminder that silently fails matters; a marketing blast
that fails for one recipient does not.

**What I would have done.** Made it a **setting**: failures always recorded, and
whether they additionally raise an in-system notification a `notify_on_failure`
flag on `messaging`'s existing `Settings`, defaulting to off.

**Moot for now**, because §1b skips the feature. Recorded because the setting is
the right answer whenever this is picked up.

---

## 3 · Two stale boxes found while auditing, and whether "stale" is the right word

**Where:** Phase 3, the request path.

Two boxes described work as outstanding that had shipped: **API keys** (Phase
12c) and **`ETag`/`If-Match`** (a real extractor with bilingual failures, used on
every settings write). Both ticked.

**The ambiguity.** Whether the `ETag` box meant something *broader* —
conditional requests on domain resources rather than on settings. No route
outside settings does update-in-place, so there is nothing else it could apply
to today. If it was meant broadly, it should be reopened with the broader
meaning written down.

---

## 4 · ZATCA's fourth tax category, `O` — decided, not built

Nothing in this system produces an out-of-scope supply, and a fourth
`VatCategory` variant means every `match` in six modules grows an arm that can
never be reached. The sixteen codes modelled cover the three categories that
exist.

**When it should change:** the first supply genuinely outside scope. `O` and the
free-text `VATEX-SA-OOS` arrive together — the code is useless without the
category and the category unreachable without the code.

---

## 5 · Whether one invoice may carry two different reasons for the same category

**The ambiguity.** The reason lives on `cac:TaxCategory` inside
`cac:TaxSubtotal`, and there is one subtotal per (category, rate). An invoice
with an exported good (`VATEX-SA-32`) *and* a medicine (`VATEX-SA-35`) — both
zero-rated — has two reasons and one place to put them. ZATCA's own developer
forum carries an open thread titled *"Multiple Tax Exemption Reason Codes For The
Same Tax Category"*, which is the evidence that this is not settled.

**What I did instead of guessing.** Made the reason a **tenant-level setting per
category**, so the situation cannot arise: every zero-rated line on an invoice
carries the same article, because they all read the same configuration in the
same transaction. Correct for the businesses this serves, and it encodes no
answer to the question above.

**What it forecloses:** a business with genuinely mixed zero-rated supplies
cannot state both articles; they would issue two invoices, which may be what
ZATCA expects. A per-line override is a small change once the answer is known.

---

## 6 · ~~Whether a tenant may *require* a second factor of its members~~ — **answered and built, 2026-09-09**

**Where:** Phase 3, MFA (§51).

**The tension, which is structural rather than a matter of taste.** Enrolment is
per **identity**, because an identity spans tenants and a login happens before
any tenant has been chosen. A tenant-level requirement is not a setting on the
login at all — it is a condition on `ControlPlane::enter`, with consequences:

- Somebody already signed in would start being refused mid-session when an owner
  turns it on.
- A member of two tenants has one identity and one factor; turning it on for
  tenant A silently changes tenant B's login too.
- An owner could lock every member out with one toggle, including themselves.

**Why not simply "make it a setting".** The *enforcement point* is the open
question, not the flag. A flag in `configuration` without deciding what happens
to live sessions and multi-tenant members would encode an answer nobody asked
for.

**Answered directly rather than logged.** Refuse at next tenant entry (session
stays valid, other tenants unaffected); switching it on is refused unless the
owner is enrolled; switching it off never needs one. Built and falsified — see
§51 of the implementation plan.

**This entry is kept as the record of the reasoning, not as an open question.**
From 2026-09-09 these go to the user directly instead of into this file.

---

## 7 · "SOCPA-aligned chart of accounts" — researched, and it does not exist

**Where:** Phase 4d, chart-of-accounts templates.

**The finding.** The box asked for five templates including a *"SOCPA-aligned"*
one. Looked up rather than assumed: SOCPA requires IFRS Accounting Standards as
endorsed in Saudi Arabia, with IFRS for SMEs the default for private companies
that are not public interest entities. Those are **accounting standards** —
recognition, measurement, disclosure. Neither prescribes a chart of accounts,
and Saudi Arabia mandates none for private companies.

**Why this is not merely a naming quibble.** A chart shipped as "SOCPA-aligned"
claims an alignment with a professional body that has issued nothing to align
with. The audience for that label is Saudi accountants and auditors — precisely
the people who would know it is not a thing, and the ones whose trust the label
is trying to borrow. Inventing a chart is fine; naming it after a standards body
is not.

**What was built instead.** `real_estate`, which is named after what it is for,
alongside the existing `services` and `retail`. "Generic IFRS" was also dropped:
`services` already *is* a chart with no industry accounts in it, and a second
copy under a standards-body name would be the same accounts and a stronger
claim.

**Not ambiguous, so not blocking** — recorded because the plan asked for
something specific and the honest answer was "that is not a thing", which is
worth having written down the next time somebody reads that box.

