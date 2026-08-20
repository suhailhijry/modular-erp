//! How a line is treated for tax.
//!
//! # Why this is in the ledger and not in `sales`
//!
//! It started in `sales`, which was right while sales was the only module that
//! knew about VAT. `purchases` classifies input tax by the same three
//! categories, and two sibling modules must not depend on each other — so it
//! moved to the module they both already depend on.
//!
//! That is not a filing decision. The ledger is this system's accounting kernel
//! for a jurisdiction: it ships the Saudi chart templates, and every one of them
//! carries a `1200 Input VAT` and a `2100 VAT payable` account. The tax
//! treatment of a line belongs beside the accounts that treatment posts to.
//!
//! What stayed in `sales` is the part only sales does: resolving the statutory
//! rate at issue and computing tax per band. A purchase does not compute its
//! tax — see `purchases::BillLine`.
//!
//! # Why zero and exempt are not the same thing
//!
//! Both are 0%. On a return they are different lines, and the difference is
//! money: input tax attached to a zero-rated supply is reclaimable and input tax
//! attached to an exempt one is not. Collapsing them is a decision that cannot be
//! undone later without asking a bookkeeper to reclassify every historic line.

use serde::{Deserialize, Serialize};

/// How a line is treated for tax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VatCategory {
    /// The ordinary rate. 15% since July 2020.
    Standard,
    /// Taxable at 0% — exports, qualifying medicines. Input tax is reclaimable.
    Zero,
    /// Outside the tax — residential rent, some financial services. Input tax
    /// attached to it is **not** reclaimable, which is why this is not `Zero`.
    Exempt,
}

impl VatCategory {
    /// Every category, for a picker and for tests that must cover all of them.
    pub const ALL: [Self; 3] = [Self::Standard, Self::Zero, Self::Exempt];

    /// Whether input tax on a purchase in this category can be reclaimed.
    ///
    /// The whole reason `Zero` and `Exempt` are separate variants, expressed as
    /// the question that separates them.
    #[must_use]
    pub const fn input_is_reclaimable(self) -> bool {
        !matches!(self, Self::Exempt)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Zero => "zero",
            Self::Exempt => "exempt",
        }
    }
}

impl std::str::FromStr for VatCategory {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "standard" => Ok(Self::Standard),
            "zero" => Ok(Self::Zero),
            "exempt" => Ok(Self::Exempt),
            other => Err(format!("unknown VAT category {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_category_round_trips_through_its_wire_form() {
        for category in VatCategory::ALL {
            assert_eq!(
                category.as_str().parse::<VatCategory>(),
                Ok(category),
                "{category:?} does not survive the wire"
            );
        }
        assert!("nonsense".parse::<VatCategory>().is_err());
    }

    /// The distinction the two zero-rate variants exist to carry.
    #[test]
    fn exempt_input_tax_is_the_one_that_cannot_be_reclaimed() {
        assert!(VatCategory::Standard.input_is_reclaimable());
        assert!(VatCategory::Zero.input_is_reclaimable());
        assert!(!VatCategory::Exempt.input_is_reclaimable());
    }
}

// ---------------------------------------------------------------------------
// What the rates actually are
// ---------------------------------------------------------------------------

/// The rates a business charges, as its jurisdiction sets them.
///
/// # Why this is configuration and not a constant
///
/// It used to be `VatCategory::rate_now()`, returning 1500 — Saudi Arabia's 15%
/// since July 2020 — from the accounting kernel. That is a fact about one
/// country living in the code every country would use, and a business in the UAE
/// (5%) could not issue a correct invoice at all.
///
/// So the rate is a value a tenant holds, and a country module is what seeds it.
/// `ledger` keeps the *shape* — that a line has a treatment and a rate — and has
/// no opinion about the number.
///
/// # Why the shipped default is Saudi Arabia's
///
/// ponytail: because it is the only market this build has been written for, and
/// a tenant who never opens the settings has to get *something*. It belongs to a
/// `tax_sa` module the moment there is a second country, and the seam is already
/// here: that module sets this key at signup and this default goes away.
///
/// # One positive rate
///
/// ponytail: `standard` is the only configurable rate, because KSA and the UAE
/// each have exactly one. A jurisdiction with reduced rates needs a *category*
/// per rate rather than a second number here — `VatCategory` is what a line is
/// classified as, and two lines at different positive rates are not the same
/// classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rates {
    /// The standard rate, in basis points. 1500 is 15%.
    pub standard: i32,
}

impl Rates {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "ledger.vat_rates";

    /// 15%, since July 2020.
    #[must_use]
    pub const fn saudi_arabia() -> Self {
        Self { standard: 1_500 }
    }

    /// The rate a category carries under these.
    ///
    /// Zero-rated and exempt are both 0% by definition and not by configuration
    /// — a jurisdiction that taxed an exempt supply would not call it exempt.
    #[must_use]
    pub const fn of(self, category: VatCategory) -> i32 {
        match category {
            VatCategory::Standard => self.standard,
            VatCategory::Zero | VatCategory::Exempt => 0,
        }
    }

    /// What this tenant has configured, or what ships.
    ///
    /// **Read inside the command's transaction**, for the same reason
    /// `sales::PostingAccounts` is: a rate that changed between the read and the
    /// write would leave an invoice stamped with one that was never current.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or_else(Self::saudi_arabia, |configured| configured.value))
    }
}

impl Default for Rates {
    fn default() -> Self {
        Self::saudi_arabia()
    }
}
