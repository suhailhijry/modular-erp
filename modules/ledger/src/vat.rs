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

    /// The rate this category carries **today**, in basis points.
    ///
    /// Called once, when a document is issued. Everything afterwards reads the
    /// rate stored on the line.
    #[must_use]
    pub const fn rate_now(self) -> i32 {
        match self {
            // ponytail: a constant, not configuration. The rate is national and
            // changes by royal decree roughly once a decade; when it next moves,
            // this becomes a date-keyed table and old documents are unaffected
            // because they carry their own rate.
            Self::Standard => 1_500,
            Self::Zero | Self::Exempt => 0,
        }
    }

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
