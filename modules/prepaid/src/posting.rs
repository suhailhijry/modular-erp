//! Where money that has been taken but not yet earned is held.
//!
//! # This module posts the deferral, not the sale
//!
//! **A divergence from the plan, and the reason is ZATCA.** The plan says
//! *"sale is Dr cash / Cr deferred revenue"*. That skips the tax invoice, and a
//! Saudi business selling a gym year cannot skip one: it is a supply, it needs
//! an invoice, and the invoice has to be cleared or reported. `sales` already
//! does all of that, including the hash chain and the QR.
//!
//! So the sale is an ordinary invoice and `sales` posts it — Dr receivable,
//! Cr revenue, Cr VAT payable. What `prepaid` adds is the fact that the revenue
//! is not earned yet:
//!
//! | when | entry |
//! |---|---|
//! | Granted | Dr revenue, Cr deferred revenue — the reclassification |
//! | Redeemed, or a subscription month served | Dr deferred revenue, Cr revenue |
//! | Expired unused | Dr deferred revenue, Cr revenue — breakage |
//! | Revoked | Dr deferred revenue, Cr revenue, and `sales` credits the invoice |
//!
//! Two things follow, and both are why this shape was chosen over the plan's:
//!
//! - **No tax anywhere in this module.** The tax was settled by whatever
//!   recognised the revenue in the first place, so there is no second opinion
//!   to keep consistent and no VAT question to answer here.
//! - **The reclassification is visible.** An auditor can see revenue booked and
//!   then deferred, which is what happened, rather than a sale that never
//!   appeared in the sales ledger at all.
//!
//! # What is not checked, deliberately
//!
//! That an invoice exists for the value being deferred. `prepaid` does not
//! depend on `sales` — two sibling modules must not — and the reference is a
//! reconciliation surface rather than a foreign key, the same argument 7a made
//! for an invoice naming a customer.

use erp_types::{AggregateId, Money};

/// The two accounts this module moves value between.
///
/// Two rather than three: there is no cash account here, because this module
/// never touches cash. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PostingAccounts {
    /// What is owed to customers for things not yet delivered. Credited when
    /// value is deferred and debited as it is earned.
    ///
    /// **This is the canary.** Its balance must equal the sum of every
    /// unredeemed entitlement and every unearned subscription month, and
    /// `crate::unreconciled` is what checks it.
    pub deferred: AggregateId,
    /// Where earned value lands, and where the reclassification takes it from.
    pub revenue: AggregateId,
}

impl PostingAccounts {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "prepaid.posting_accounts";

    /// What this tenant has configured, or what ships.
    ///
    /// A tenant who never opens the settings gets [`Self::conventional`]. One
    /// who *has* configured it and stored something unusable gets an error
    /// rather than the default — silently posting a year of deferrals to the
    /// wrong account is found at an audit and not before.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or_else(Self::conventional, |configured| configured.value))
    }

    /// The codes every chart in `ledger::CHARTS` ships.
    #[must_use]
    pub fn conventional() -> Self {
        Self {
            deferred: code("2400"),
            revenue: code("4000"),
        }
    }
}

impl Default for PostingAccounts {
    fn default() -> Self {
        Self::conventional()
    }
}

/// Money taken for something not yet delivered: out of revenue, into the
/// liability.
pub(crate) fn entry_for_deferral(
    value: Money,
    accounts: &PostingAccounts,
) -> Result<ledger::BalancedLines, ledger::Unbalanced> {
    two_sided(&accounts.revenue, &accounts.deferred, value)
}

/// Value delivered, forfeited or reversed: out of the liability, into revenue.
///
/// The same entry for all three, because a ledger sees debits and credits and
/// the difference is in the memo and in the event. A read model that needs to
/// tell earned revenue from a reversed deferral reads the events, not this.
pub(crate) fn entry_for_release(
    value: Money,
    accounts: &PostingAccounts,
) -> Result<ledger::BalancedLines, ledger::Unbalanced> {
    two_sided(&accounts.deferred, &accounts.revenue, value)
}

/// Debit one, credit the other, by the same amount.
fn two_sided(
    debit: &AggregateId,
    credit: &AggregateId,
    value: Money,
) -> Result<ledger::BalancedLines, ledger::Unbalanced> {
    let opposite = value.checked_neg()?;
    ledger::BalancedLines::new(vec![
        ledger::Line::new(debit.clone(), value),
        ledger::Line::new(credit.clone(), opposite),
    ])
}

/// Panics only on a literal in this crate that breaks `AggregateId`, which is a
/// build bug with no runtime recovery.
#[expect(
    clippy::expect_used,
    reason = "a malformed literal is a build bug, not a runtime condition"
)]
fn code(literal: &str) -> AggregateId {
    AggregateId::new(literal).expect("account codes in this crate are valid literals")
}
