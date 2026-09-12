//! **How large a document a member may issue.**
//!
//! A tenant's owner sets one amount at `PUT /v1/sales/document-limit`, and says
//! whether it is compared with a document's total before VAT or after it. From
//! then on every invoice, every credit note (a whole cancellation or part of an
//! invoice) and every refund **a member issues** is refused when it comes to
//! more — at the sales routes, at the till, from the booking desk and when a
//! gateway refund is asked for. Supplier bills are not sales documents and are
//! not limited. With nothing set there is no limit, which is how every tenant
//! starts.
//!
//! # Who is not limited
//!
//! - **The owner, always.** The house rule of §52: a control must not strand the
//!   person who switched it on.
//! - **Anybody the org chart gives [`EXCEED_DOCUMENT_LIMIT`]**, in the branch the
//!   request names — asked with `hr::actor_holds`, so the claim reaches the
//!   employee it was granted to and everybody above them, the way every claim
//!   travels. A member with no employee record can hold no claim, and is
//!   limited.
//! - **Nobody acting.** A customer paying a deposit online, the worker billing a
//!   completed booking, a gateway confirming a refund somebody already asked
//!   for. [`Authority::System`] says so, in so many words, at each of those
//!   calls. Where a member started what the gateway later settles — a deposit
//!   charged to a saved card, a refund — the member was judged when they asked,
//!   by [`may_issue`] and [`crate::may_refund`], which lives beside the
//!   commands because it loads the invoice (L7).
//!
//! # Why in the command and not a permission limit
//!
//! `erp_tenant::Limits` judges a *capability* at the edge, from facts the edge
//! has — the role, the branch, and an amount only where the route can read one
//! before anything is written, which is the ledger's own two. The total of an
//! invoice exists only once this module has priced its lines at the tenant's
//! rate, taken off its discounts and deducted its deposit, inside the
//! transaction that writes it. So this is judged there, after the totals, by
//! the functions every issuing path goes through: `issue_in`, the credit-note
//! roots and `refund_in`. A permission limit is for what a *role* may do; this
//! is for how large a *document* may be.
//!
//! # Who is asking is an argument, with no default
//!
//! Every one of those functions takes an [`Authority`]. Not an `Option`, and not
//! something read off the connection: a new path that issues a document does
//! not compile until it has said whether a member is behind it, which is the
//! only way "we forgot to check" stops being a thing a later change can do.

use erp_eventlog::{ExecuteError, Metadata};
use erp_types::Money;
use serde::{Deserialize, Serialize};

use crate::SalesError;
use crate::invoice::Invoice;

/// The claim that lifts the limit. `module:verb`, like every claim; it
/// travels up the org chart like any claim not in `hr::SEGREGATED`, because a
/// manager covering for a cashier who may ring a large sale may ring one too.
pub const EXCEED_DOCUMENT_LIMIT: &str = "sales:exceed_document_limit";

/// Who is issuing a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authority {
    /// Somebody signed in to this tenant — a person, or an API key acting with
    /// a role (§22). `owner` exempts them; anybody else is judged by the org
    /// chart, through the request's actor and branch.
    Member { owner: bool },
    /// Nobody: a customer's own payment, a worker's pass, a gateway's answer.
    /// **Never the default for anything**; the callers that pass it say why.
    System,
}

impl Authority {
    /// The member behind a request's handle.
    ///
    /// **Never [`Self::System`].** A handle with nobody behind it is a member
    /// who is not the owner, so the worst a mistaken caller gets is a refusal.
    #[must_use]
    pub fn of(db: &erp_tenant::TenantDb) -> Self {
        Self::Member {
            owner: db.role() == Some(erp_tenant::Role::Owner),
        }
    }
}

/// Which of a document's totals a limit is compared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// What it comes to before tax. A refund's is its share of the invoice's,
    /// at the invoice's own proportion.
    BeforeVat,
    /// What the customer pays, or is handed back.
    AfterVat,
}

/// **The most one document may come to**, and on which total.
///
/// Cannot exist unchecked: [`Self::new`] refuses an amount that is not
/// positive, and reading a stored one checks the same way, so a row this build
/// cannot use is refused where it is read (L6) rather than treated as no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Unchecked")]
pub struct DocumentLimit {
    limit: Money,
    basis: Basis,
}

#[derive(Deserialize)]
struct Unchecked {
    limit: Money,
    basis: Basis,
}

impl TryFrom<Unchecked> for DocumentLimit {
    type Error = NotALimit;

    fn try_from(unchecked: Unchecked) -> Result<Self, NotALimit> {
        Self::new(unchecked.limit, unchecked.basis)
    }
}

/// A limit of nothing, or less, which would refuse every document there is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a document limit must be more than nothing, and {0} is not")]
pub struct NotALimit(pub Money);

impl erp_i18n::Localize for NotALimit {
    fn message(&self) -> erp_i18n::Message {
        erp_i18n::Message::new(crate::messages::DOCUMENT_LIMIT_NOT_POSITIVE)
            .with("limit", erp_i18n::MessageArg::text(self.0.to_string()))
    }
}

impl DocumentLimit {
    /// Where a tenant's limit is stored. `null` there is no limit, which is
    /// how an owner removes one.
    pub const KEY: &'static str = "sales.document_limit";

    /// # Errors
    /// [`NotALimit`] for an amount that is not positive.
    pub fn new(limit: Money, basis: Basis) -> Result<Self, NotALimit> {
        if limit.is_positive() {
            Ok(Self { limit, basis })
        } else {
            Err(NotALimit(limit))
        }
    }

    #[must_use]
    pub const fn limit(&self) -> Money {
        self.limit
    }

    #[must_use]
    pub const fn basis(&self) -> Basis {
        self.basis
    }

    /// What this tenant has set, or `None` for no limit.
    ///
    /// # Errors
    /// A stored value this build cannot use, or the database.
    pub async fn resolve(
        conn: &mut sqlx::PgConnection,
    ) -> Result<Option<Self>, erp_eventlog::ConfigError> {
        Ok(
            erp_eventlog::configuration::get::<Option<Self>>(conn, Self::KEY)
                .await?
                .and_then(|configured| configured.value),
        )
    }

    /// **Refuses a document over this limit.** `net` and `gross` are the
    /// document's own totals; the basis picks one, and equal to the limit is
    /// within it.
    ///
    /// **A document in another currency is refused.** Its total cannot be
    /// compared with the limit, and no answer is not "under it" — the
    /// three-valued reading §63 gave permission limits, for the reason it gave
    /// there: counting it as under would let anybody past the limit by
    /// invoicing in dollars. The owner or the claim is the way through.
    pub(crate) fn judge(self, net: Money, gross: Money) -> Result<(), SalesError> {
        let amount = match self.basis {
            Basis::BeforeVat => net,
            Basis::AfterVat => gross,
        };
        match amount.checked_cmp(self.limit) {
            Ok(std::cmp::Ordering::Less | std::cmp::Ordering::Equal) => Ok(()),
            Ok(std::cmp::Ordering::Greater) | Err(_) => Err(SalesError::OverDocumentLimit {
                limit: self.limit,
                amount,
            }),
        }
    }

    /// A whole invoice, as a cancellation credits it.
    pub(crate) fn judge_whole(self, state: &Invoice, invoice: &str) -> Result<(), SalesError> {
        let (net, gross) = whole(state, invoice)?;
        self.judge(net, gross)
    }

    /// Money handed back against an invoice: the refund itself after VAT, and
    /// before it the same share of the invoice's net — `refunded × net ÷
    /// gross`, the invoice's own proportion, the way `payments` apportions a
    /// deposit it keeps. Refunding a whole invoice is judged on exactly the
    /// totals issuing it was.
    pub(crate) fn judge_refund(
        self,
        state: &Invoice,
        invoice: &str,
        refunded: Money,
    ) -> Result<(), SalesError> {
        let (net, gross) = whole(state, invoice)?;
        let share = refunded
            .apportioned(net.minor(), gross.minor())
            .map_err(|e| SalesError::Tax(e.into()))?;
        self.judge(share, refunded)
    }
}

/// What an issued invoice came to, before and after tax.
fn whole(state: &Invoice, invoice: &str) -> Result<(Money, Money), SalesError> {
    let unissued = || SalesError::NotIssued(invoice.to_owned());
    let gross = state.gross.ok_or_else(unissued)?;
    let net = Money::checked_sum(state.bands.iter().map(|band| band.net), gross.currency())
        .map_err(|e| SalesError::Tax(e.into()))?;
    Ok((net, gross))
}

/// **The limit this caller is held to**, or `None`.
///
/// Asked before the decision, in the same transaction, because the claim is a
/// query and a decision is not async. The comparison is inside the decision,
/// after the totals and after the retry check, so the retry of a document
/// issued before the limit was lowered answers with that document.
pub(crate) async fn binding(
    conn: &mut sqlx::PgConnection,
    authority: Authority,
    metadata: &Metadata,
) -> Result<Option<DocumentLimit>, ExecuteError<SalesError>> {
    let Authority::Member { owner: false } = authority else {
        return Ok(None);
    };
    let Some(limit) = DocumentLimit::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(SalesError::Config(e)))?
    else {
        return Ok(None);
    };
    if hr::actor_holds(&mut *conn, EXCEED_DOCUMENT_LIMIT, metadata)
        .await
        .map_err(ExecuteError::Database)?
    {
        return Ok(None);
    }
    Ok(Some(limit))
}

/// **Refuses an invoice this caller may not have issued**, before anything
/// asks for it to be issued.
///
/// For a deposit a member charges: the prepayment invoice is raised by
/// [`issue_in`] when the gateway settles, by which time the customer has paid
/// and refusing it would leave the money undeclared. So the member who asks
/// for the charge is judged then, on the totals the invoice will have — the
/// deposit's `net`, and the `gross` the customer is charged, which settlement
/// refuses to bill unless the invoice comes to exactly that.
///
/// # Errors
/// [`SalesError::OverDocumentLimit`], or reading the limit or the claim.
///
/// [`issue_in`]: crate::issue_in
pub async fn may_issue(
    conn: &mut sqlx::PgConnection,
    net: Money,
    gross: Money,
    authority: Authority,
    metadata: &Metadata,
) -> Result<(), ExecuteError<SalesError>> {
    match binding(&mut *conn, authority, metadata).await? {
        Some(limit) => limit.judge(net, gross).map_err(ExecuteError::Rejected),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_types::CurrencyCode;

    fn sar(minor: i64) -> Money {
        Money::from_minor(minor, CurrencyCode::new("SAR").expect("SAR"))
    }

    /// **The basis is the whole question at the boundary.** Net 9,500 comes
    /// to 10,925 with 15% on it: under a 10,000 limit before VAT, over it
    /// after.
    #[test]
    fn the_basis_decides_which_total_is_compared() {
        let (net, gross) = (sar(950_000), sar(1_092_500));
        let before = DocumentLimit::new(sar(1_000_000), Basis::BeforeVat).expect("a limit");
        let after = DocumentLimit::new(sar(1_000_000), Basis::AfterVat).expect("a limit");

        assert!(before.judge(net, gross).is_ok());
        assert!(matches!(
            after.judge(net, gross),
            Err(SalesError::OverDocumentLimit { limit, amount })
                if limit == sar(1_000_000) && amount == gross
        ));
        assert!(
            after.judge(sar(1_000_000), sar(1_000_000)).is_ok(),
            "equal to the limit is within it"
        );
    }

    #[test]
    fn another_currency_is_refused_rather_than_counted_as_under() {
        let limit = DocumentLimit::new(sar(1_000_000), Basis::AfterVat).expect("a limit");
        let dollars = Money::from_minor(100, CurrencyCode::new("USD").expect("USD"));
        assert!(matches!(
            limit.judge(dollars, dollars),
            Err(SalesError::OverDocumentLimit { .. })
        ));
    }

    #[test]
    fn a_limit_of_nothing_cannot_be_made_or_read() {
        assert_eq!(
            DocumentLimit::new(sar(0), Basis::AfterVat),
            Err(NotALimit(sar(0)))
        );
        let stored = serde_json::json!({
            "limit": { "minor": -5, "currency": "SAR" },
            "basis": "after_vat"
        });
        assert!(serde_json::from_value::<DocumentLimit>(stored).is_err());
    }
}
