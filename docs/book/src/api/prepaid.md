# prepaid

Everything the customer has already paid for: packages, courses, deposits,
subscriptions and loyalty. One module, because it is **one accounting problem
wearing five names** — money received now for value delivered later.

**Depends on:** `ledger`, `crm`, plus the core.
**Depended on by:** nothing yet.

## Why this is one module and not five

Building them separately would write deferred revenue five times, and law L3
would then forbid the one screen every one of these businesses actually wants:
*what does this customer have with us?* Tables in different projection groups
never read each other, so five modules would mean five reads at five checkpoints
that can disagree while somebody is taking money against the answer.

The name is `prepaid` and not `entitlements` because `entitlement` already means
something here — the control plane's table of which **modules** a tenant has
switched on.

## This module posts the deferral, not the sale

A divergence from the plan, and the reason is ZATCA.

The plan said the sale is `Dr cash / Cr deferred revenue`. That skips the tax
invoice, and a Saudi business selling a gym year cannot skip one: it is a supply,
it needs an invoice, and the invoice has to be cleared or reported. `sales`
already does all of that, including the hash chain and the QR.

So the sale is an ordinary invoice and `sales` posts it — `Dr receivable`,
`Cr revenue`, `Cr VAT payable`. What `prepaid` adds is the fact that the revenue
is not earned yet:

| when | entry |
|---|---|
| Granted | `Dr revenue`, `Cr deferred revenue` — the reclassification |
| Redeemed, or a month served, or a point honoured | `Dr deferred revenue`, `Cr revenue` |
| Expired unused | `Dr deferred revenue`, `Cr revenue` — breakage |
| Revoked | `Dr deferred revenue`, `Cr revenue`, and `sales` credits the invoice |

Two things follow, and both are why this shape was chosen over the plan's.

**There is no tax anywhere in this module.** The tax was settled by whatever
recognised the revenue in the first place, so there is no second opinion to keep
consistent and no VAT question to answer here. That claim is load-bearing, and
[what is refused](#an-open-value-gift-card-which-is-refused) is what keeps it
true.

**The reclassification is visible.** An auditor sees revenue booked and then
deferred, which is what happened, rather than a sale that never appeared in the
sales ledger at all.

## The files

| File | What is in it |
|---|---|
| [`entitlement.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/prepaid/src/entitlement.rs) | `Entitlement`, `EntitlementEvent`, `Reason`, `Balance`, `Closed` |
| [`subscription.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/prepaid/src/subscription.rs) | `Subscription`, `SubscriptionEvent`, the recognition arithmetic |
| [`loyalty.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/prepaid/src/loyalty.rs) | `Loyalty`, `LoyaltyEvent`, `Mechanic`, `Scheme`, `Tier`, `allocate` |
| [`posting.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/prepaid/src/posting.rs) | `PostingAccounts`, `entry_for_deferral`, `entry_for_release` |
| [`commands.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/prepaid/src/commands.rs) | Fourteen commands, and the errors |
| [`projections.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/prepaid/src/projections.rs) | The `Prepaid` group, entitlements, subscriptions, cards, `outstanding` |
| [`http.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/prepaid/src/http.rs) | The routes |

## Three aggregates, and why not one

| shape | liability | revenue recognised |
|---|---|---|
| Package, ten sessions | yes | when each session is **delivered** |
| Deposit against a booking | yes | when it is drawn, or forfeited |
| Subscription, a gym year | yes | **ratably over the term**, attended or not |
| Loyalty points | yes | when they are **honoured**, or expire as breakage |
| Coupon | **no** | never — no consideration was received |

This is the part that is an accounting error if it is got wrong. A gym
subscription recognises monthly whether or not the member appears; a ten-session
package recognises per session. Treating them alike misstates revenue every
month, in one direction or the other.

So the split follows the recognition model and nothing else. A package and a
deposit differ in being counted rather than an amount, and in naming what they
are held against — which is not a difference in *when the revenue is earned*, so
they are one aggregate. A subscription earns by the clock, so it is another. A
card accumulates rather than draws down, so it is a third.

## Entitlement

```rust
pub enum Reason { Bought, GiftedByCustomer, GrantedByBusiness, FreeFromCoupon }
impl Reason { pub const fn was_paid_for(self) -> bool; }
```

**It decides the accounting, not the wording.** Two of these were paid for and
two were not, and that is the whole of what the enum is for: a grant nobody paid
for creates no liability, so it posts nothing and has nothing to release.

A coupon is a discount at the point of sale and **not** a liability. Rekaz has a
full coupon model and no coupon liability account, which is correct and worth not
undoing. `GiftedByCustomer` exists because somebody did pay — who holds it is a
different question from who funded it, and keeping both answerable a year later
is what the field buys.

```rust
pub struct Balance { pub uses: Option<u32>, pub value: Money }
impl Balance {
    pub fn worth_of_one_use(self) -> Result<Money, MoneyError>;
    pub fn is_spent(&self) -> bool;
}
```

Two numbers rather than one, because a package is counted and a deposit is not,
and because the value has to be tracked in money regardless — it *is* the
liability, and the liability is what has to come out right.

### Why the last use takes the remainder

Ten sessions of a 100 riyal package is 10 riyals each and divides exactly. Three
sessions of 100 is 33.33, and three of those is 99.99 — a halala stranded in a
liability account for ever, on a package the customer has finished.

So a use is worth `remaining value / remaining uses`, recomputed each time. When
one use is left, that is the whole remaining value, which is the definition
rather than a special case. `Money::apportioned` is exact at `n/n` for the same
reason.

```rust
pub enum EntitlementEvent {
    Granted { customer, what, uses, value, reason, against, expires_at, at },
    Redeemed { reference, uses, value, at },
    Expired { value, at },
    Revoked { why, value, at },
}

pub struct Entitlement { … }
impl Entitlement {
    pub const fn exists(&self) -> bool;
    pub fn is_live(&self) -> bool;
    pub fn has_lapsed(&self, at: Timestamp) -> bool;
    pub fn has_redemption(&self, reference: &str) -> bool;
    pub fn outstanding(&self) -> Option<Money>;
}
```

`expires_at` is an instant and not `none | days | months`. The rule that produced
the date belongs to whoever sold it; storing the date is what makes a replay
reproduce the decision instead of recomputing it under this year's policy. L5.

`has_lapsed` takes the moment rather than reading a clock, for the same reason.

`against` is an opaque id — the booking a deposit secures. This module does not
know what a booking is, so there is no foreign key and could not be one: it would
point into another projection group.

## An open-value gift card, which is refused

A card spendable on anything is a **multi-purpose voucher**. What it buys is not
known when it is sold, so neither is the rate it should have been taxed at, and
the tax point moves to redemption.

Every shape here is single-purpose: a package counts uses of a named service, a
deposit names the booking it secures, and both settle the tax question at the
sale. `grant` refuses an amount that is neither — no uses and nothing to be held
against — with `PrepaidError::OpenValue`.

**The refusal is a guard and not a note.** It was a documentation claim first,
and nothing enforced it; a caller could construct exactly an open-value card.
Making it a check is what keeps "there is no tax anywhere in this module" true by
construction rather than by hoping callers cooperate. If open-value cards are
ever wanted, the classification belongs to the *product* and not to a tenant
setting, and the sale has to settle its own tax question first.

## Subscription

```rust
pub enum SubscriptionEvent {
    Started { customer, plan, price, from, until, at },
    Recognised { through, value, at },
    Frozen { why, at },
    Resumed { until, at },
    Renewed { price, from, until, at },
    Cancelled { why, at },
}

pub struct Subscription { … }
impl Subscription {
    pub fn admits(&self, at: Timestamp) -> bool;
    pub fn served(&self, through: Timestamp) -> Option<(i64, i64)>;
    pub fn earned_by(&self, through: Timestamp) -> Option<Result<Money, MoneyError>>;
    pub fn outstanding(&self) -> Option<Money>;
    pub const fn is_frozen(&self) -> bool;
}
```

### Recognition is a cumulative total, not a sum of instalments

`earned_by` answers *how much of this term has been earned by this moment*, as a
total: `price × served / term`. `Recognised` then carries the **difference**
between that and what has already been recognised, and `through` records what the
total was computed at.

Two properties follow, and neither survives the obvious alternative of adding up
monthly instalments. Running a month-end job twice recognises nothing the second
time, because the cumulative has not moved — which is what makes it safe to
retry. And the last day of the term brings the total to exactly the price, with
no rounding drift accumulated over twelve postings.

### Freeze

Freezing earns everything up to that moment and stops the clock. Resuming pushes
the term out by exactly the time it was stopped for, which is why `Resumed`
carries the new `until` rather than deriving it later.

How *long* a freeze may run is not decided here. Rekaz's own copy concedes those
rules are policy-dependent, and a limit invented in this module would be wrong
for somebody.

### admits

**This is what a gym door asks:** started, not cancelled, inside its term, and
not frozen. It is answered from aggregate state rather than from a projection,
because a turnstile cannot wait for a read model that may be a second behind.

## Loyalty

Points, stamps and visits are one aggregate. They differ in **what produces the
count** — a rate on spend, a named item, an attendance — and in nothing after it.
All three accumulate, all three are redeemed for something, and all three carry
the same obligation.

```rust
pub enum Mechanic { Points, Stamps, Visits }
```

So `Mechanic` is fixed when a card is opened and read by the business, and
nothing branches on it. It is recorded because a stamp card and a points balance
are different things to the person holding one. Rekaz models the three separately
and pays for it in three earning paths and three balances; this is the lesson
packages and deposits already taught.

### IFRS 15, and the shortcut that is not available

Points are a **separate performance obligation**. A sale that awards them has not
delivered everything it was paid for, so part of its price belongs to the points
and is deferred until they are honoured or expire:

```text
allocated = spend × (count × worth) / (spend + count × worth)
```

That is IFRS 15's relative standalone selling price, with the sale's own price
standing as the goods' standalone price and `Scheme::worth` as the points'.

A hundred riyals awarding a hundred points worth ten halalas each defers
**9.09, not 10.00**. The common SMB shortcut accrues the liability at redemption
value instead — the whole ten — which overstates the liability and charges the
difference to expense.

Saudi requires IFRS. **Only the rigorous treatment is implemented, and there is
no setting that selects the other:** what an accountant may not choose, a tenant
may not either. The allocation is frozen into the event, so a scheme that changes
`worth` next year does not restate what was deferred last year (L5).

### What this needs from the sale, and what it does not

It needs the sale's **price**, because the allocation is a fraction of it. It does
not need the sale: `Earning::from` is an opaque id, the same reconciliation
surface a deposit uses, and for the same reason — `sales` and `prepaid` are
siblings and neither may depend on the other.

That the invoice and the deferral are two transactions is deliberate and is this
module's existing bargain. The canary below is what catches a pair that came
apart.

```rust
pub struct Scheme { pub worth: Money, pub rate_bp: u32, pub tiers: Vec<Tier> }
pub struct Tier { pub name: String, pub from: u32, pub rate_bp: u32 }

impl Scheme {
    pub const KEY: &'static str;          // "prepaid.loyalty_scheme"
    pub async fn resolve(conn) -> Result<Option<Self>, ConfigError>;
    pub fn rank_at(&self, lifetime: u32) -> Option<&Tier>;
    pub fn rate_at(&self, lifetime: u32) -> u32;
    pub fn counts_for(&self, spend: Money, lifetime: u32) -> u32;
}
```

**There is no default scheme.** Account codes have a conventional value that
every chart ships; what a point is worth does not — it is a business decision
with no defensible fallback, and guessing one would put a number nobody chose
into the allocation that decides deferred revenue. A tenant who has not
configured a scheme cannot earn, and gets `PrepaidError::NoScheme` rather than a
quiet default. That is L6.

`counts_for` rounds **down**: a business that promised a point per riyal has not
promised one for eighty halalas, and rounding up would award a point nobody paid
for and defer revenue against it.

### Tiers are ranks, and they are not spent

Rekaz calls this `Membership`, which is easy to misread as a gym membership; it
is a rank, and what it changes is the earning rate.

```rust
pub struct Loyalty { pub lifetime: u32, pub balance: Option<Balance>, … }
```

`lifetime` counts everything ever earned and **never decreases**. Spending points
does not cost a rank, and neither does breakage: what was earned was earned. That
is the whole reason it is tracked separately from the redeemable balance.

The rate is read at the rank a card has *already* reached, so the movement that
crosses a threshold earns at the old rate and the next one at the new. Any other
reading makes the award depend on itself.

```rust
pub enum LoyaltyEvent {
    Opened { customer, mechanic, at },
    Earned { reference, count, allocated, from, at },
    Redeemed { reference, count, value, toward, at },
    Expired { count, value, at },
}
```

Redeeming reuses the same drawdown a package uses — each count is worth what is
left divided by what is left to spend — so the deferred pool empties to exactly
zero and no halala is stranded.

**The card survives its own breakage.** Points running out is not the end of the
card; it can earn again the next day. That is the one place this aggregate is not
shaped like `Entitlement`.

A card remembers the last sixty-four movement keys rather than all of them. A
card is earned on at every visit for years, so remembering every key would grow
without bound; a retry arrives within seconds, and anything arriving sixty-four
movements later is a new movement and not a retry.

## Posting

```rust
pub struct PostingAccounts { pub deferred: AggregateId, pub revenue: AggregateId }
impl PostingAccounts {
    pub const KEY: &'static str;          // "prepaid.posting_accounts"
    pub async fn resolve(conn) -> Result<Self, ConfigError>;
    pub fn conventional() -> Self;        // 2400 and 4000
}
```

Two accounts rather than three: there is no cash account, because this module
never touches cash.

Unlike the loyalty scheme, these *do* have a conventional value — every chart in
`ledger::CHARTS` ships `2400 Deferred revenue` — so a tenant who never opens the
settings gets it. One who has configured something unusable gets an error rather
than the default: silently posting a year of deferrals to the wrong account is
found at an audit and not before.

## Commands

```rust
// entitlements
pub async fn grant(db, id, &Grant, meta) -> Outcome<EntitlementEvent>;
pub async fn redeem(db, id, &Redemption, meta) -> Outcome<EntitlementEvent>;
pub async fn expire(db, id, at, meta) -> Outcome<EntitlementEvent>;
pub async fn revoke(db, id, why, at, meta) -> Outcome<EntitlementEvent>;

// subscriptions
pub async fn start_subscription(db, id, &Term, meta) -> Outcome<SubscriptionEvent>;
pub async fn recognise_through(db, id, through, meta) -> Outcome<SubscriptionEvent>;
pub async fn freeze(db, id, why, at, meta) -> Outcome<SubscriptionEvent>;
pub async fn resume(db, id, at, meta) -> Outcome<SubscriptionEvent>;
pub async fn renew_subscription(db, id, price, until, at, meta) -> Outcome<SubscriptionEvent>;
pub async fn cancel_subscription(db, id, why, at, meta) -> Outcome<SubscriptionEvent>;

// loyalty
pub async fn open_card(db, id, &Card, meta) -> Outcome<LoyaltyEvent>;
pub async fn earn(db, id, &Earning, meta) -> Outcome<LoyaltyEvent>;
pub async fn redeem_points(db, id, &PointsRedemption, meta) -> Outcome<LoyaltyEvent>;
pub async fn expire_points(db, id, at, meta) -> Outcome<LoyaltyEvent>;
```

**Every command writes an event and a journal entry, together.** The same reason
`sales` does: a liability that exists in one place and not the other is a state
nobody could explain and nothing would clean up. So none of these use
`TenantDb::execute`, which runs exactly one aggregate — each opens its own
transaction and posts inside it.

**The posting is derived from what was recorded, not from what was asked.** Each
command reads `Committed::events` back and posts for what is actually in them. A
decision that recorded nothing posts nothing, so a retried command is silent all
the way down to the ledger — which is the property that makes a month-end
recognition job safe to run twice.

That distinction was not free. `renew_subscription` posted the release of the old
term and never the new term's deferral, and the read model carried a liability
the books did not. What found it was the canary, after the test was extended to
renew.

Every command that takes a `reference` is idempotent under it (L8): a repeated
redemption, earning or payment key records nothing the second time and therefore
posts nothing.

## Read models

One group, `proj_prepaid`, with three tables — and one group rather than three
because the screen this module exists for shows all of them at once. A group is
the unit of consistency (L3).

```rust
pub async fn entitlements(conn, customer, include_closed, limit, after) -> Page<EntitlementSummary>;
pub async fn entitlement(conn, id) -> Option<EntitlementSummary>;
pub async fn subscriptions(conn, customer, limit, after) -> Page<SubscriptionSummary>;
pub async fn subscription(conn, id) -> Option<SubscriptionSummary>;
pub async fn cards(conn, customer, limit, after) -> Page<CardSummary>;
pub async fn card(conn, id) -> Option<CardSummary>;
pub async fn outstanding(conn) -> Vec<Money>;
```

`CardSummary::deferred` is optional because a card that has never earned has no
currency to say a zero in.

### The canary

`outstanding` is **the number the ledger's deferred revenue account has to agree
with**: every unredeemed entitlement, every unearned subscription month and every
unhonoured loyalty count, per currency.

It returns a number instead of checking it, and that is L3 rather than laziness.
The comparison needs the ledger's account balance, which lives in `proj_ledger` —
a different projection group, which this module may not read. It is the same
reason `crm` cannot show a customer's invoices.

So this is one half of a canary. The half that compares belongs to something that
declares both groups: a report module, or a test.
`a_liability_agrees_with_the_ledger` is that test today, and it is the same class
of check as `ledger::imbalances` — if the two disagree, the pipeline is broken
rather than the arithmetic. It runs the comparison after a grant, a redemption,
a deposit drawn, recognition, a freeze, a resume, a renewal, points earned and
spent, a revocation, a cancellation and breakage.

## What is deliberately absent

**Open-value gift cards**, refused rather than missing — see above.

**Any tax.** Not an omission: it is the property the module is arranged to keep.

**Breakage estimated in advance.** IFRS 15 allows recognising expected breakage
in proportion to the pattern of redemption. This recognises it at expiry, which
is the simpler treatment and the one a business that has just watched a package
lapse expects.

**A limit on how long a freeze may run**, and **a link from an entitlement to the
invoice that sold it.** The second is a reconciliation surface rather than a
foreign key, for the reason every cross-module reference here is.

## Routes

| Method | Path | Capability |
|---|---|---|
| `GET` `POST` | `/v1/prepaid/entitlements` | Read / PostEntries |
| `GET` | `/v1/prepaid/entitlements/{entitlement}` | Read |
| `POST` | `/v1/prepaid/entitlements/{entitlement}/redemptions` | PostEntries |
| `POST` | `/v1/prepaid/entitlements/{entitlement}/expiry` | PostEntries |
| `POST` | `/v1/prepaid/entitlements/{entitlement}/revocation` | PostEntries |
| `GET` `POST` | `/v1/prepaid/subscriptions` | Read / PostEntries |
| `GET` | `/v1/prepaid/subscriptions/{subscription}` | Read |
| `POST` | `/v1/prepaid/subscriptions/{subscription}/recognition` | PostEntries |
| `POST` `DELETE` | `/v1/prepaid/subscriptions/{subscription}/freeze` | PostEntries |
| `POST` | `/v1/prepaid/subscriptions/{subscription}/renewal` | PostEntries |
| `POST` | `/v1/prepaid/subscriptions/{subscription}/cancellation` | PostEntries |
| `GET` `POST` | `/v1/prepaid/cards` | Read / PostEntries |
| `GET` | `/v1/prepaid/cards/{card}` | Read |
| `POST` | `/v1/prepaid/cards/{card}/earnings` | PostEntries |
| `POST` | `/v1/prepaid/cards/{card}/redemptions` | PostEntries |
| `POST` | `/v1/prepaid/cards/{card}/expiry` | PostEntries |
| `GET` | `/v1/prepaid/outstanding` | Read |
| `GET` `PUT` | `/v1/prepaid/posting-accounts` | Read / ManageAccounts |
| `GET` `PUT` | `/v1/prepaid/loyalty-scheme` | Read / ManageAccounts |

Resuming a frozen subscription is a `DELETE` on its freeze, which is the same
shape `booking` uses for putting a withdrawn resource back: removing the thing
*is* the operation.
