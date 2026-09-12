//! Who may do what, and who inherits it.
//!
//! # The rule, in one line
//!
//! ```text
//! claims(node) = own(node) ∪ ⋃ claims(child) for each child
//! ```
//!
//! A manager automatically holds everything their reports hold. The reason is
//! operational: a manager has to be able to cover for anyone beneath them, and
//! nobody should have to remember that giving a new clerk a permission also
//! means giving it to their supervisor. Granting *downward* is the arrangement
//! that produces the ticket *"the branch manager cannot approve what her own
//! cashier can"*.
//!
//! Every consequence below follows from that one line. Each is a decision.
//!
//! # The root holds everything
//!
//! That is the definition, not a defect — but it means **the org chart is the
//! authorization model and the top node is a superuser by construction**. It is
//! intended: the person nobody reports to is the owner of the business, and a
//! business owner who could not approve something happening in their own
//! company would be a surprising product.
//!
//! Somebody who must sit *outside* that — an external auditor, a bookkeeper on
//! retainer — is not an employee and does not go in the tree. They are a
//! platform membership with a role, which is the other axis entirely and is
//! what §9c kept separate.
//!
//! # A grant at a leaf is not a local act
//!
//! Giving a junior something powerful is the cheapest way to escalate every
//! ancestor, silently. So [`grant`] returns **everyone who gained it**, and the
//! screen that grants a claim is expected to show that list. A grant that
//! showed only the person being granted is the interface that would make this
//! design dangerous rather than convenient.
//!
//! # Segregation of duties, and the flag that saves it
//!
//! The control every accounting system is measured on is that the person who
//! raises an invoice is not the person who approves its payment. Under a
//! bottom-up union their shared manager holds both, automatically, the moment
//! the org chart says so — which fails a Saudi statutory audit.
//!
//! So a claim can be granted **non-propagating**: it applies to the person
//! named and travels nowhere. [`SEGREGATED`] is the list that must be, and
//! [`grant`] refuses to propagate one even if a caller asks.
//!
//! # Why this is not a projection
//!
//! A command deciding *"may this person approve this"* cannot read a read model
//! that may be a second behind. A claim revoked a moment ago has to bite now.
//! So the effective set is write-side state in the tenant migration chain,
//! maintained in the same transaction as the org event that changed it — see
//! `migrations/tenant/0008_org_claims.sql`.

use erp_types::AggregateId;
use sqlx::PgConnection;

/// Claims that must never travel up the tree.
///
/// # Why a claim is `module:verb` and not `module.verb`
///
/// Because `module.verb` is what an *error code* looks like in this API, and a
/// document naming `hr.approve_leave` as an example claim was read by the
/// openapi guard as a code that did not exist. That is a real ambiguity and not
/// a false alarm: two namespaces sharing a shape is two things somebody will
/// eventually confuse. A colon separates them at a glance.
///
/// **This is the segregation-of-duties list**, and it is a constant rather than
/// configuration because what an auditor requires is not a preference a tenant
/// expresses. A business that could switch it off would have a design that
/// passes an audit only when nobody has touched the settings.
///
/// Prefix matching, so a module's whole family can be segregated at once —
/// `sales.approve.` covers everything under it.
pub const SEGREGATED: &[&str] = &[
    // The classic pair. Raising a document and approving the money for it must
    // not land in one pair of hands, and under this union they otherwise would
    // the moment the two people share any manager.
    "purchases:approve_payment",
    "sales:approve_credit_note",
    // Approving your own timesheet is the same shape one module over.
    "hr:approve_timesheet",
];

/// The claim that lets somebody other than the owner reset a colleague's second
/// factor. `module:verb`, like every claim, and `hr`'s because it is a fact
/// about people rather than about any book.
///
/// **Not segregated**, so it travels up the chart like the document limit's: a
/// manager covering for a supervisor who may get a cashier back into their
/// account may do it too. It is checked with [`actor_holds`], which means a
/// caller with no employee record holds it whatever else they are.
pub const RESET_SECOND_FACTOR: &str = "hr:reset_second_factor";

/// Whether a claim is one the union must not carry.
#[must_use]
pub fn is_segregated(claim: &str) -> bool {
    SEGREGATED
        .iter()
        .any(|listed| claim == *listed || claim.starts_with(&format!("{listed}.")))
    // The `.` suffix is deliberate and not a leftover: a family is
    // `purchases:approve_payment.over_limit`, so the *namespace* separator is
    // the colon and the *hierarchy* separator inside it stays a dot.
}

/// One claim, somewhere.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Claim {
    /// The granting module's own vocabulary — `hr:approve_leave`. Never parsed.
    pub name: String,
    /// Where it applies. `None` is company-wide.
    ///
    /// **Not the same as "some branch".** Payroll and an end-of-service
    /// calculation are company-wide by nature, and a claim that had to name a
    /// branch could not express them.
    pub branch: Option<String>,
}

/// A claim somebody holds, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub claim: Claim,
    /// Themselves, or somebody in their subtree. **The first question anybody
    /// asks of an inherited permission is where it came from.**
    pub source: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    #[error("{0} cannot report to somebody in their own team")]
    Cycle(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl erp_i18n::Localize for ClaimError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::Cycle(id) => Message::new(messages::CYCLE).with("id", MessageArg::text(id)),
            Self::Database(_) => Message::new(messages::DATABASE),
        }
    }
}

/// Records where somebody sits, and rebuilds what that changes.
///
/// Called from inside the command's transaction, so the tree and the effective
/// set commit together with the event that caused them. There is no window in
/// which the log says one thing and an authorization check says another.
pub async fn place(
    conn: &mut PgConnection,
    employee: &AggregateId,
    reports_to: Option<&AggregateId>,
    branch: Option<&AggregateId>,
) -> Result<(), ClaimError> {
    if let Some(parent) = reports_to {
        // **A cycle is refused, and not because it is untidy.** `A → B → A` is
        // what two well-meaning edits a week apart produce, and the union above
        // would not terminate on one.
        if parent == employee || is_beneath(conn, parent, employee).await? {
            return Err(ClaimError::Cycle(employee.to_string()));
        }
    }

    sqlx::query(
        "INSERT INTO org_reporting_line (employee, reports_to, branch)
         VALUES ($1, $2, $3)
         ON CONFLICT (employee) DO UPDATE
             SET reports_to = EXCLUDED.reports_to, branch = EXCLUDED.branch",
    )
    .bind(employee.as_str())
    .bind(reports_to.map(AggregateId::as_str))
    .bind(branch.map(AggregateId::as_str))
    .execute(&mut *conn)
    .await?;

    rebuild(conn).await
}

/// Whether `candidate` is anywhere beneath `node`.
///
/// A recursive walk **down** rather than up, because that is the direction a
/// cycle would close in: making `A` report to somebody already in `A`'s own
/// subtree is what creates one.
async fn is_beneath(
    conn: &mut PgConnection,
    candidate: &AggregateId,
    node: &AggregateId,
) -> Result<bool, sqlx::Error> {
    let found: Option<i32> = sqlx::query_scalar(
        "WITH RECURSIVE subtree AS (
             SELECT employee FROM org_reporting_line WHERE employee = $1
             UNION
             SELECT l.employee
               FROM org_reporting_line l
               JOIN subtree s ON l.reports_to = s.employee
         )
         SELECT 1 FROM subtree WHERE employee = $2",
    )
    .bind(node.as_str())
    .bind(candidate.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(found.is_some())
}

/// Grants a claim, and reports **everyone who gained it**.
///
/// The second half is not a convenience. A grant at a leaf escalates every
/// ancestor, and an interface that showed only the person being granted would
/// hide exactly the thing somebody needs to see before they click.
///
/// `propagates` is the caller's request and not the last word: a claim on
/// [`SEGREGATED`] never travels, whatever is asked.
pub async fn grant(
    conn: &mut PgConnection,
    employee: &AggregateId,
    claim: &Claim,
    propagates: bool,
) -> Result<Vec<String>, ClaimError> {
    let propagates = propagates && !is_segregated(&claim.name);

    // Delete-then-insert rather than `ON CONFLICT`, because the uniqueness is
    // two *partial* indexes — one for a branch, one for company-wide — and a
    // conflict target can only name one of them. `IS NOT DISTINCT FROM` is what
    // makes `NULL` match `NULL`, which is the case `=` would silently miss.
    sqlx::query(
        "DELETE FROM org_claim_granted
          WHERE employee = $1 AND claim = $2 AND branch IS NOT DISTINCT FROM $3",
    )
    .bind(employee.as_str())
    .bind(&claim.name)
    .bind(claim.branch.as_deref())
    .execute(&mut *conn)
    .await?;

    sqlx::query(
        "INSERT INTO org_claim_granted (employee, claim, branch, propagates)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(employee.as_str())
    .bind(&claim.name)
    .bind(claim.branch.as_deref())
    .bind(propagates)
    .execute(&mut *conn)
    .await?;

    rebuild(conn).await?;
    who_holds(conn, claim).await
}

/// Takes a claim back, and reports everyone who lost it.
pub async fn revoke(
    conn: &mut PgConnection,
    employee: &AggregateId,
    claim: &Claim,
) -> Result<Vec<String>, ClaimError> {
    let before = who_holds(conn, claim).await?;

    sqlx::query(
        "DELETE FROM org_claim_granted
          WHERE employee = $1 AND claim = $2 AND branch IS NOT DISTINCT FROM $3",
    )
    .bind(employee.as_str())
    .bind(&claim.name)
    .bind(claim.branch.as_deref())
    .execute(&mut *conn)
    .await?;

    rebuild(conn).await?;
    let after = who_holds(conn, claim).await?;

    Ok(before.into_iter().filter(|w| !after.contains(w)).collect())
}

/// **Whether this person may do this, here.**
///
/// The question every command asks, answered from write-side state so a claim
/// revoked a moment ago already bites.
///
/// A company-wide claim answers yes for any branch, which is what company-wide
/// means. A claim scoped to Olaya does not answer for Malaz — collapsing them
/// would grant a branch manager authority in a branch they have never seen.
pub async fn holds(
    conn: &mut PgConnection,
    employee: &AggregateId,
    claim: &str,
    branch: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM org_claim_effective
          WHERE employee = $1 AND claim = $2
            AND (branch IS NULL OR branch IS NOT DISTINCT FROM $3)
          LIMIT 1",
    )
    .bind(employee.as_str())
    .bind(claim)
    .bind(branch)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(found.is_some())
}

/// **Whether the claim system is switched on in this tenant.**
///
/// A tenant that has never granted a claim is not asking for the control, and
/// enforcing one against them would refuse work nobody was refusing before.
/// The first `place` anywhere turns it on for every module that checks — which
/// is what "grant a claim and it starts mattering" has to mean if the grant is
/// not to be decorative.
///
/// # Errors
/// If the database does.
pub async fn any_claim_placed(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    // **`org_claim_granted`, the grants themselves** — not
    // `org_claim_effective`, which is the union derived from them. A grant that
    // reaches nobody because the org chart is empty is still a tenant saying
    // they want the control.
    let found: Option<i32> = sqlx::query_scalar("SELECT 1 FROM org_claim_granted LIMIT 1")
        .fetch_optional(&mut *conn)
        .await?;
    Ok(found.is_some())
}

/// What [`may_for`] decided, and why.
///
/// A reason rather than a `bool` because the two refusals are different
/// sentences to the person reading them: one says *ask somebody who holds it*,
/// the other says *ask somebody else entirely*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Permitted,
    /// The caller does not hold the claim here.
    NoClaim,
    /// The caller holds it, and the subject is the caller. **Segregation of
    /// duties is not about authority; it is about two people.**
    WouldBeSelfApproval,
}

/// **May this caller do this, here, to this person?**
///
/// [`may`] with a subject. Everything [`may`] answers, plus: somebody who holds
/// the claim still may not exercise it **on themselves**.
///
/// # Why the owner is exempt from this too
///
/// A one-person business is its own only employee. Refusing self-approval
/// outright would stop a sole trader recording a single day worked, with nobody
/// on earth able to do it for them — the lockout shape again. An owner
/// overriding a control they own is the accepted residual risk in every
/// accounting system; a control that stops the business working is not.
///
/// # Errors
/// If the database does.
pub async fn may_for(
    conn: &mut PgConnection,
    claim: &str,
    subject: Option<&AggregateId>,
    metadata: &erp_eventlog::Metadata,
    access: Option<&erp_tenant::Access>,
) -> Result<Approval, sqlx::Error> {
    if !any_claim_placed(&mut *conn).await? {
        return Ok(Approval::Permitted);
    }
    if access.is_some_and(|a| a.role == erp_tenant::Role::Owner) {
        return Ok(Approval::Permitted);
    }
    if metadata.actor.is_none() {
        // A worker, a reaper or provisioning. Nobody to check, and refusing
        // would stop background work the moment a tenant granted a claim.
        return Ok(Approval::Permitted);
    }
    let Some(id) = claimant(&mut *conn, metadata).await? else {
        return Ok(Approval::NoClaim);
    };
    if !holds(&mut *conn, &id, claim, metadata.branch()).await? {
        return Ok(Approval::NoClaim);
    }
    // **Checked after the claim, not before.** Somebody who does not hold the
    // claim at all should be told that, not told they cannot sign their own —
    // the second message would imply they could sign somebody else's.
    if subject.is_some_and(|subject| *subject == id) {
        return Ok(Approval::WouldBeSelfApproval);
    }
    Ok(Approval::Permitted)
}

/// **May this caller do this, here?** The question a command asks.
///
/// Three answers, in order:
///
/// 1. **Nobody has granted a claim in this tenant** — the control is not on,
///    and everything is permitted exactly as it was before claims existed.
/// 2. **The caller owns the tenant** — exempt. See below.
/// 3. **Otherwise** the caller must be an employee who holds the claim, in the
///    branch the request named. No employee record means no claim can reach
///    them, and that is a refusal rather than a pass.
///
/// # Why an owner is exempt even when they are staff
///
/// **Asked and confirmed, 2026-09-10.** The narrower rule — exempt only
/// somebody with no employee record — has a trap: an owner who is *also* on the
/// org chart, which is the ordinary case in a small business, locks themselves
/// out the moment they grant the claim to somebody else.
///
/// So the exemption is **by role, not by whether a record exists**.
/// `Role::Owner` is documented as "everything", and a control an owner cannot
/// lift is a support call.
///
/// This is the third control in this system to land on the same rule, and it is
/// worth naming as one: **switching a control on must not be the act that
/// strands you.** The tenant second-factor requirement refuses to be enabled by
/// somebody unenrolled for it; that requirement can always be switched off
/// without one for it; and this exemption exists for it.
///
/// **The residual risk is deliberate and known**: an owner can approve their
/// own payment, credit note and timesheet. Every accounting system accepts
/// that, because the alternative is a control that stops a one-person business
/// working at all — see `a_sole_trader_may_record_their_own_days`.
///
/// # Errors
/// If the database does.
pub async fn may(
    conn: &mut PgConnection,
    claim: &str,
    metadata: &erp_eventlog::Metadata,
    access: Option<&erp_tenant::Access>,
) -> Result<bool, sqlx::Error> {
    Ok(may_for(&mut *conn, claim, None, metadata, access).await? == Approval::Permitted)
}

/// **Whether the person behind this request holds a claim here**, and nothing
/// else.
///
/// [`may`] without its two passes. It asks whether the tenant has granted any
/// claim, but reads "none" as "not held" rather than as a pass, and a request
/// with no actor holds nothing. That is the question for
/// a control something *other* than a grant switches on — `sales`' document
/// limit is set by the owner, and a claim is the way past it — where "nobody
/// has granted anything yet" must not mean "nobody is limited". Whether the
/// caller owns the tenant is the caller's question; this answers only for the
/// org chart, which an owner may not even be on.
///
/// **A tenant that has granted nothing is answered from the grants alone**,
/// which live in the tenant's own migration chain: finding the employee reads
/// `hr`'s read model, and a tenant selling without `hr` enabled has none.
///
/// # Errors
/// If the database does.
pub async fn actor_holds(
    conn: &mut PgConnection,
    claim: &str,
    metadata: &erp_eventlog::Metadata,
) -> Result<bool, sqlx::Error> {
    if !any_claim_placed(&mut *conn).await? {
        return Ok(false);
    }
    match claimant(&mut *conn, metadata).await? {
        Some(id) => holds(&mut *conn, &id, claim, metadata.branch()).await,
        None => Ok(false),
    }
}

/// The employee whose login made this request, if one did. **No employee
/// record means no claim can reach them**, which every caller treats as not
/// holding one.
async fn claimant(
    conn: &mut PgConnection,
    metadata: &erp_eventlog::Metadata,
) -> Result<Option<AggregateId>, sqlx::Error> {
    let Some(actor) = metadata.actor.as_deref() else {
        return Ok(None);
    };
    Ok(crate::employee_by_login(&mut *conn, actor)
        .await?
        .and_then(|employee| AggregateId::new(employee.id).ok()))
}

/// Everything one person effectively holds, and where each came from.
pub async fn effective(
    conn: &mut PgConnection,
    employee: &AggregateId,
) -> Result<Vec<Held>, sqlx::Error> {
    let rows: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT claim, branch, source FROM org_claim_effective
          WHERE employee = $1 ORDER BY claim, branch NULLS FIRST, source",
    )
    .bind(employee.as_str())
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(name, branch, source)| Held {
            claim: Claim { name, branch },
            source,
        })
        .collect())
}

/// Everyone who effectively holds a claim. What [`grant`] reports back.
async fn who_holds(conn: &mut PgConnection, claim: &Claim) -> Result<Vec<String>, ClaimError> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT employee FROM org_claim_effective
          WHERE claim = $1 AND branch IS NOT DISTINCT FROM $2
          ORDER BY employee",
    )
    .bind(&claim.name)
    .bind(claim.branch.as_deref())
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows)
}

/// Takes away everything somebody was granted, when they leave.
///
/// **Their grants, not their inheritance.** What they held because their team
/// holds it goes with the same recomputation; what they were given directly is
/// deleted, because authority ends when somebody walks out even though their
/// record does not.
///
/// Their reporting line stays. Their team still reports to them until the
/// business moves it, which is a decision a resignation does not get to make —
/// silently re-parenting a whole team to the departed manager's manager would
/// hand somebody a subtree nobody chose to give them.
pub async fn withdraw(conn: &mut PgConnection, employee: &AggregateId) -> Result<(), ClaimError> {
    sqlx::query("DELETE FROM org_claim_granted WHERE employee = $1")
        .bind(employee.as_str())
        .execute(&mut *conn)
        .await?;
    rebuild(conn).await
}

/// Recomputes the whole effective set.
///
/// # Why the whole thing, and why that is not the wrong answer
///
/// An incremental update would touch only the ancestors of what changed, which
/// is fewer rows — and would be a second implementation of the union rule,
/// living beside the first and free to disagree with it. This codebase has
/// already been bitten by a rule written twice (`pos`'s drawer), so the union
/// exists once, in SQL, and every change re-runs it.
///
/// It is one recursive query over a table whose size is the number of employees
/// a company has. A thousand-person business is a thousand rows, and this runs
/// when somebody is hired, moved or granted something — not when a claim is
/// *checked*, which is the operation that had to be fast and is a single
/// indexed lookup.
///
/// ponytail: rebuild-the-world. Make it incremental when a customer has enough
/// employees for it to show, and prove the two agree before you do.
async fn rebuild(conn: &mut PgConnection) -> Result<(), ClaimError> {
    sqlx::query("DELETE FROM org_claim_effective")
        .execute(&mut *conn)
        .await?;

    // `descendants` is every (ancestor, descendant) pair including the node
    // itself, which is what makes the union one join: somebody holds a claim if
    // it was granted to anyone at or beneath them — and, unless it is theirs,
    // only if it propagates.
    sqlx::query(
        "INSERT INTO org_claim_effective (employee, claim, branch, source)
         WITH RECURSIVE descendants AS (
             SELECT employee AS ancestor, employee AS descendant
               FROM org_reporting_line
             UNION
             SELECT d.ancestor, l.employee
               FROM org_reporting_line l
               JOIN descendants d ON l.reports_to = d.descendant
         )
         SELECT DISTINCT d.ancestor, g.claim, g.branch, g.employee
           FROM descendants d
           JOIN org_claim_granted g ON g.employee = d.descendant
          WHERE d.ancestor = d.descendant OR g.propagates
         ON CONFLICT DO NOTHING",
    )
    .execute(&mut *conn)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The segregation list is prefix-matched so a module can segregate a whole
    /// family, and must not catch a claim that merely starts with the same
    /// letters.
    #[test]
    fn segregation_matches_a_family_and_not_a_lookalike() {
        assert!(is_segregated("purchases:approve_payment"));
        assert!(is_segregated("purchases:approve_payment.over_limit"));
        assert!(
            !is_segregated("purchases:approve_payments"),
            "a different claim was segregated because it shared a prefix"
        );
        assert!(!is_segregated("purchases:record_bill"));
    }
}
