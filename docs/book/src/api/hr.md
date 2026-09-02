# hr

The org chart, and the claims that travel up it.

**Depends on:** `branches`, and the core.
**Depended on by:** nothing yet — `payroll` and attendance are the next things
to stand on it.

## Why this module is an authorization structure and not a directory

Every other module here answers a question about money or capacity. This one
answers **who may decide**.

The reporting line is not a decoration on an employee record. It is the
structure authority travels along, and §9b's one line is the whole of it:

```text
claims(node) = own(node) ∪ ⋃ claims(child) for each child
```

A manager automatically holds everything their reports hold. The reason is
operational and good: a manager has to be able to cover for anyone beneath them,
and nobody should have to remember that giving a new clerk a permission also
means giving it to their supervisor. Granting *downward* is the arrangement that
produces the ticket **"the branch manager cannot approve what her own cashier
can"**.

Everything below is a consequence of that line. Each one is a decision.

## The root is a superuser, and that is intended

The definition makes the top node hold the union of every claim in the company.

That is settled rather than tolerated: the person nobody reports to is the owner
of the business, and a business owner who could not approve something happening
in their own company would be a surprising product.

Somebody who must sit *outside* it — an external auditor, a bookkeeper on
retainer — **is not an employee and does not go in the tree**. They are a
platform membership with a role, which is the other axis entirely and is exactly
what the plane decision below keeps separate.

## A grant at a leaf is not a local act

Giving a junior something powerful is the cheapest way to escalate every
ancestor, silently. So the API cannot fail to say so:

```
POST /v1/hr/employees/{employee}/claims
→ { "holders": ["EMP-CLERK", "EMP-MANAGER", "EMP-OWNER"], "propagates": true }
```

`holders` is **everyone who now has it**, not an acknowledgement. A screen that
showed only the person being granted would hide exactly what somebody needs to
see before they click, and putting the list in the response is what makes
omitting it impossible rather than merely discouraged.

## Segregation of duties, and the flag that saves it

The control every accounting system is measured on is that the person who raises
an invoice is not the person who approves its payment. Under a bottom-up union
their shared manager holds both — automatically, silently, the moment the org
chart says so. That fails a Saudi statutory audit.

So a claim can be **non-propagating**, and `hr::SEGREGATED` is the list that must
be:

```rust
pub const SEGREGATED: &[&str] = &[
    "purchases.approve_payment",
    "sales.approve_credit_note",
    "hr.approve_timesheet",
];
```

A constant and **not configuration**, because what an auditor requires is not a
preference a tenant expresses — a business that could switch it off would have a
design that passes an audit only when nobody has touched the settings.

`grant` refuses to propagate one **even when asked to**, and the response says
`propagates: false` rather than quietly doing something other than what was
sent. Matching is by prefix, so a module can segregate a whole family, and
`segregation_matches_a_family_and_not_a_lookalike` is what stops
`purchases.approve_payments` being caught by `purchases.approve_payment`.

## These claims never leave the tenant

**The load-bearing decision of the phase.** Authorization in the control plane
answers *"may you reach this endpoint at all"* — four coarse capabilities, per
identity per tenant, cached across nodes in Redis. These claims answer *"may you
approve this particular thing"*, and they are checked **inside module commands**
where the decision is made.

So `Capability` and `Allowed<C>` are untouched, and:

- no `hr` type appears in `erp-control`, `erp-web` or `erp-tenant`;
- nothing in `hr` reaches for `Invalidate`, `forget` or `ControlPlane`;
- nothing in `claims.rs` names a `proj_` table.

None of that is enforced by a type, so it is enforced by
[`tests/planes.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/hr/tests/planes.rs)
— source-scanning and crude, which is the point: the tests read what a reviewer
would have to read, and the decision would otherwise erode one reasonable-looking
commit at a time.

**What it buys**: no plane is crossed, nothing invalidates a session when
somebody is promoted, and a tenant's own org chart cannot widen what the platform
believes about that tenant.

## Where the effective set lives

Not in a projection.

A command deciding *"may this person approve this"* cannot read a read model that
may be a second behind — the same reason `sales` validates a customer against
`crm`'s log rather than `proj_crm`, one layer along and with more at stake. A
claim revoked a moment ago has to bite **now**.

So `org_claim_granted`, `org_claim_effective` and `org_reporting_line` are in
`migrations/tenant/0008_org_claims.sql`, beside `occupancy_claim` and for the
same reason: `rebuild_schema` drops and rebuilds `proj_*` and must never come
near them. `proj_hr` exists alongside, for the screen that *draws* the chart.

### A design error worth recording

`PRIMARY KEY (employee, claim, branch)` cannot hold a nullable column — and
company-wide is exactly `branch IS NULL`. The key would have forced every claim
to name a branch, which payroll and an end-of-service calculation cannot.

It is two **partial** unique indexes instead. The second is not redundant with
the first: Postgres treats NULLs as distinct in a unique index, so without it two
company-wide grants of the same claim would both be admitted.

### Why the recomputation is the whole set

```rust
async fn rebuild(conn: &mut PgConnection) -> Result<(), ClaimError>
```

An incremental update would touch only the ancestors of what changed — fewer
rows, and **a second implementation of the union rule**, living beside the first
and free to disagree with it. This codebase has already been bitten by a rule
written twice (`pos`'s drawer rule, which a mutation test found because nothing
ever asked the aggregate and the projection the same question). So the union
exists once, in one recursive SQL statement, and every change re-runs it.

It runs when somebody is hired, moved, or granted something — not when a claim is
*checked*, which is the operation that had to be fast and is a single indexed
lookup. Marked `ponytail:` with the condition for changing it.

## The tree

```rust
pub enum EmployeeEvent {
    Hired { …, reports_to, branch, at },
    Amended { … },              // never the reporting line
    Reparented { reports_to, why, at },
    Transferred { branch, at },
    Left { why, at },
    Rehired { at },
}
```

**Moving somebody is its own event**, because it moves everything they carry:
every claim in their subtree stops reaching their old manager and starts reaching
their new one. That is the operation an auditor asks about, and an `Amended` that
quietly changed a parent alongside a phone number would not answer them.
`amending_details_cannot_move_somebody_in_the_chart` is what keeps them apart.

**A cycle is refused**, and not because it is untidy: the union would not
terminate. `A → B → A` is what two well-meaning edits a week apart produce. The
check is a recursive walk **down** — that is the direction one closes in, because
making `A` report to somebody already in `A`'s subtree is what creates it.

**A leaver keeps their record and loses their claims.** They are on last year's
payroll and whatever they approved; what ends is authority. Their team keeps
reporting to them until the business moves it, which is a decision a resignation
does not get to make — silently re-parenting a whole team to the departed
manager's manager would hand somebody a subtree nobody chose to give them.

## Branches: a filter, not a wall

`Employee.branch` is **where this person works**. The branch in `Metadata` is
**where this request happened**. They differ legitimately and often — an Olaya
manager visiting Malaz records attendance for a Malaz shift — and a report that
read one where it meant the other would be wrong in a way nobody notices for a
quarter.

The union is over `(claim, branch)` pairs, not bare claims. A regional manager
over two branches accumulates both; a branch manager does not gain a branch they
have never seen. `None` is company-wide and answers for any branch, which is not
the same as "some branch" — payroll could not be expressed otherwise.

Reads **default** to the caller's branch and widen on request. It cannot be a
wall the way `ledger::post_entry_in` is one: payroll, the org chart and an
end-of-service calculation are company-wide by nature, and a boundary that
refused them would make the module unusable in its first month. `?scope=all` is
how a caller says so.

## What is deliberately not here yet

Skills, shifts, attendance, leave, positions, departments and contracts. Each
hangs off `Employee` and none changes the authorization model — which is why the
tree and the claims went first. Three more aggregates written before the model
was proved would have been three more things to change when it moved.

## Routes

See [The HTTP API](./http.md#the-org-chart-and-what-people-may-decide).
