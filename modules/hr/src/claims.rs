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
