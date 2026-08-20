//! What a Saudi business declares for a period.
//!
//! # Why this is a module and not a function in the API
//!
//! It was one, until the shape of the system said otherwise. Netting output tax
//! against input tax is **domain** — VAT law, and Saudi VAT law specifically —
//! and it was sitting in `erp-api`, the crate that is supposed to hold none. The
//! test that catches it is the one the model gives: *can a tenant disable it?*
//! A business with neither sales nor purchases had a VAT return endpoint, which
//! is the answer.
//!
//! # Why it is not a cross-group read
//!
//! `proj_sales` and `proj_purchases` are separate projection groups and neither
//! may read the other (architecture L3). Nothing here does: it calls each
//! module's own read function and nets the answers in Rust. What L3 protects is
//! that a group is the unit of consistency, and the protection is unchanged —
//! two groups at different checkpoints would give a return that mixes a caught-up
//! quarter with one that is not, which is what `?consistent_after=` waits out.
//!
//! # Why every document is reported on its own tax point
//!
//! An invoice on its issue date, a credit note on its credit date, a bill on the
//! date the supplier stated. Re-running a filed period gives the number that was
//! filed — see `modules/sales/schema/install.sql` on the boundary case that
//! forced it.

use ledger::VatCategory;
use erp_types::{CurrencyCode, Money, Timestamp};

/// One rate band, on one side of the return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Band {
    pub category: VatCategory,
    pub basis_points: i32,
    pub net: Money,
    pub tax: Money,
    /// Documents with a tax point in this period. Invoices and credit notes on
    /// the output side; bills on the input side.
    pub documents: i64,
}

/// One side of the return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    pub bands: Vec<Band>,
    pub net: Money,
    pub tax: Money,
}

impl Side {
    fn empty(currency: CurrencyCode) -> Self {
        Self {
            bands: Vec::new(),
            net: Money::zero(currency),
            tax: Money::zero(currency),
        }
    }
}

/// What gets declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Return {
    pub from: Timestamp,
    /// **Exclusive**, so consecutive returns neither overlap nor leave a day out.
    pub until: Timestamp,
    pub currency: CurrencyCode,
    /// What was charged on sales, net of credit notes with a tax point in this
    /// period.
    pub output: Side,
    /// What was paid on purchases, and the reclaimable part of it.
    pub input: Side,
    /// **The number that gets paid, or reclaimed.** Output tax less input tax;
    /// negative means ZATCA owes the business.
    pub payable: Money,
}

/// What a business has and has not enabled.
///
/// A tenant with only one of the two modules gets zeroes for the other side
/// rather than an error: a business that has not enabled purchases genuinely
/// reclaimed nothing, and that is a return they can file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sides {
    pub sells: bool,
    pub buys: bool,
}

/// The return for a period.
pub async fn vat_return(
    conn: &mut sqlx::PgConnection,
    sides: Sides,
    currency: CurrencyCode,
    from: Timestamp,
    until: Timestamp,
) -> Result<Return, sqlx::Error> {
    let output = if sides.sells {
        let filed = sales::vat_return(&mut *conn, currency, from, until).await?;
        Side {
            bands: filed
                .bands
                .iter()
                .map(|b| Band {
                    category: b.category,
                    basis_points: b.basis_points,
                    net: b.net,
                    tax: b.tax,
                    documents: b.invoices + b.credit_notes,
                })
                .collect(),
            net: filed.net,
            tax: filed.tax,
        }
    } else {
        Side::empty(currency)
    };

    let input = if sides.buys {
        let paid = purchases::input_tax(&mut *conn, currency, from, until).await?;
        Side {
            bands: paid
                .bands
                .iter()
                .map(|b| Band {
                    category: b.category,
                    basis_points: b.basis_points,
                    net: b.net,
                    tax: b.tax,
                    documents: b.bills,
                })
                .collect(),
            net: paid.net,
            tax: paid.tax,
        }
    } else {
        Side::empty(currency)
    };

    let payable = output
        .tax
        .checked_sub(input.tax)
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

    Ok(Return {
        from,
        until,
        currency,
        output,
        input,
        payable,
    })
}
