# The rules engine — design

**Phase 5b, first of three.** This spec covers **the artifact and the
evaluator, with pricing on it and nothing else changed.** Authorization,
authoring levels and rule packs are later specs and are named at the end.

## The problem this is the answer to

A tenant cannot change a business rule without a deploy. The codebase names the
rules it wants and cannot express:

- *"A bookkeeper may post entries under ten thousand riyals, or only to their
  own branch"* — `crates/erp-tenant/src/roles.rs:11`. `Role::allows` is a
  compiled `match` over four roles and no facts.
- *"An invoice over 50,000 needs a second approval"* — ARCHITECTURE §5.6's
  `Rule<ApprovalChain>`. Does not exist.

And one it *can*: *"Thursday evenings are 25% dearer"* —
`modules/booking/src/pricing.rs`, tenant-authored, versioned, first-match,
frozen onto the line at decision time.

**That last one is the engine already, in miniature and for one consumer:**

```rust
pub struct Band { name: String, when: Availability, uplift: i32 }
//                ↑ identity     ↑ the condition     ↑ the consequence
```

Against ARCHITECTURE §5.6's `Rule<E> { when: DynCondition, then: E, … }`, the
only difference is that `when` is one concrete type instead of a language.

**The payoff is not any one rule.** It is that the next kind of rule costs a
type parameter rather than a subsystem, and inherits authoring, versioning,
replay-safety and explanation from the first.

## Why now, and why this was right to defer

Phase 5's preamble deferred the engine because *"it had one real consumer and no
concrete rules to describe it from — pricing does not exist"*. Pricing exists.
The deferral's own condition is met, by the consumer the plan does not claim —
the four it does claim were audited on 2026-09-09 and are **one and a half**
(§53).

Two consumers with real authored conditions is a thin but genuine basis. Four,
two of them fictional, was not.

## 1 · `DynCondition`, and the one variant that is not uniform

```rust
pub enum DynCondition {
    /// Matches everything. The base rate, the default rule.
    Always,
    All(Vec<DynCondition>),
    Any(Vec<DynCondition>),
    Not(Box<DynCondition>),
    /// A named fact against a value.
    Is { fact: FactName, op: Op, value: Value },
    /// **A span falls inside a repeating window.**
    Covers(Availability),
}
```

### Why `Covers` carries a whole type

The other variants compare a scalar fact. `Covers` does not, and forcing it to
would be the one destructive thing this spec could do.

`Availability::covers(span, calendar)` walks every day a booking touches, gives
each end of a daylight-saving change its own offset, and is deliberate about
minute-versus-second boundaries so that 16:59:30 is inside a window closing at
17:00. Its representation is bit-packed — `months: u16`, `weekdays: u8`,
`days: u32`, where **zero means every**, which is what makes the common rule
short. None of that has a natural spelling as scalar facts and operators, and
re-expressing it would be reimplementing working, tested code in a weaker form.

So the language is **deliberately not uniform**: one variant is a rich type with
its own evaluator. The alternative — a `span` fact with `covers`/`overlaps`
operators — hides the same code behind an operator and gains only the appearance
of symmetry.

### What `Is` covers

`Op` is `Eq | Ne | Lt | Lte | Gt | Gte | In`. `Value` is `Int(i64) |
Text(String) | Bool(bool) | Money(Money)`. Money compares only against Money and
only in the same currency; a mismatch is a **validation** failure at authoring
time, not a false at evaluation time.

## 2 · `Facts` and `FactRegistry`

```rust
pub struct Facts { /* named scalars, plus the span when there is one */ }

pub struct FactRegistry { /* which facts exist, and of what type */ }
```

**The registry's job is to fail at authoring time, not at a user's request.** A
condition naming a fact nobody supplies, or comparing Money to Text, is refused
when the rule is written. That is the architecture's *"validated against
`FactRegistry` at authoring time"*, and it is the difference between a rules
engine and a way to store broken rules.

**Two disjoint groups, and that is expected.** Pricing's facts are time;
authorization's will be amount, branch and role. They share nothing, and a
registry of two disjoint groups is the honest state of a system with two
unrelated rule kinds. It is not evidence the abstraction is wrong; it is
evidence there are two consumers.

## 3 · `Rule<E>` — smaller than §5.6, and each omission argued

```rust
pub struct Rule<E> {
    /// What the business calls it. Printed beside the outcome.
    pub name: String,
    pub when: DynCondition,
    pub then: E,
}

pub struct Rules<E> { /* ordered; first match wins */ }
```

ARCHITECTURE §5.6 also lists `id`, `version`, `priority`, `effective` and
`origin`. None is in this spec:

| Field | Why not yet |
|---|---|
| `priority` | The list is ordered and first match wins, which is what `Tariff::band_for` already does. A priority *and* an order is two ways to say one thing, and they disagree eventually |
| `effective: DateRange` | `Availability` already carries `from`/`until`. A second date range on the rule would be a second answer to the same question |
| `origin` | Records which authoring level produced a rule. There is one level today — a JSON body — so it would have one value. **Since delivered, and not as a field**: `Authored<A>` wraps the artifact, because a form-authored rule has no condition of its own to carry — it has answers, and the condition is rebuilt from them |
| `id`, `version` | The whole rule set is one versioned configuration entry with an `ETag`, which is what pricing already uses. Per-rule versioning is for when rules are edited individually |

Each arrives with the consumer that needs it. Adding them now means five fields
nothing reads, which is the shape of every abstraction that later has to be
unpicked.

## 4 · `explain`, and why it is in the first cut

```rust
pub struct Explained<'a, E> {
    pub matched: Option<&'a Rule<E>>,
    /// Every rule tried, in order, and why each failed.
    pub considered: Vec<Considered<'a>>,
}
```

Not deferred, for one reason: **a rules engine whose refusals cannot be
interrogated generates the support tickets it was built to remove.** "Why was
this priced at the peak rate" and "why may I not post this entry" are the two
questions a configurable system creates, and answering them after the fact is
much harder than emitting them from the evaluator that already knows.

`evaluate` and `explain` are **one function**: `explain` is the whole answer and
`evaluate` returns `matched` from it. They cannot disagree about which rule won,
which is the same property `preview_chart` and `install_chart` have.

## 5 · What changes in `booking`

`Band` becomes `Rule<Uplift>`; `Tariff` holds `Rules<Uplift>`. `band_for`
becomes an `explain` whose `matched` is used.

**The wire shape does not change.** A tenant's stored tariff deserialises to the
same JSON: `{ name, when: {...}, uplift }`, with `when` now a `DynCondition`
whose `Covers` variant serialises as today's `Availability` fields. An upcaster
handles the old shape, and a golden case pins it — a tariff already configured
must keep pricing identically, or this refactor silently repriced somebody's
salon.

## 6 · Testing, and how each guard is falsified

| Test | Falsified by |
|---|---|
| A condition naming an unregistered fact is refused **at authoring time** | Making validation return `Ok` — a broken rule stores and fails at a user's request instead |
| Comparing Money to Text is refused at authoring time | Same |
| `All`/`Any`/`Not` compose, including the empty cases: `All([])` is true, `Any([])` is false | Swapping the two — the classic identity bug, and it silently inverts a rule |
| `Covers` matches exactly what `Availability::covers` matches, over the existing pricing fixtures | Making `Covers` evaluate the span's start instant instead of the span |
| First match wins, and a later matching rule does not | Returning the last match |
| `explain` names every rule considered, in order, with a reason for each miss | Returning only the match — the engine works and cannot be interrogated |
| `evaluate` and `explain` agree on the winner, always | Giving `evaluate` its own loop |
| **A tariff stored before this change prices identically after it** | Any change to `Covers` serialisation; this is the one that protects a live tenant |

## What this spec does not cover

- **Authorization on the engine.** Its three facts, and the narrowing rule that
  a fact-based override refines `Role::allows` and never widens it. Second spec.
- **Authoring levels 0–3 and `origin` round-tripping.** Third spec, and the one
  that decides whether most tenants ever see JSON. **Built 2026-09-10** —
  `erp_rules::authoring`, with the design recorded in that module and in
  ARCHITECTURE §5.6 rather than in a separate spec: one question needed
  deciding (whether the answers or the condition are the truth) and the answer
  is the whole shape.
- **Rule packs as blueprints.** Rides on 4d's pipeline, whose preview step now
  exists.
- **Per-request fact assembly with startup coverage assertions.** Needs a
  consumer whose facts are assembled per request, which authorization is and
  pricing is not.
