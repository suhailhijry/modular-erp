//! Every crate's messages, behind one lookup.
//!
//! Codes are globally unique by their `domain.` prefix, so "first catalog that
//! has it" is unambiguous. A module's catalog is added here when the module is;
//! forgetting to is caught by `codes()` disagreeing with what the module claims,
//! not by a user seeing a bare code.
//!
//! # Why this is not the only composite
//!
//! It is the *complete* one, and it can only exist here — this is the only crate
//! that names every module. A module renders its own failures through a smaller
//! composite of its catalog and [`erp_web::CATALOG`], because it cannot name its
//! siblings and has no reason to. This one is what `docs/ERRORS.md` is generated
//! from and what the completeness audit runs against, so a code missing from any
//! part is still a failing build.

use erp_i18n::Composite;

/// Ordered by nothing in particular — lookups are by code, which is unique.
///
/// `erp_web::CATALOG` is itself a composite, and carries the request-level, the
/// control plane's and the event log's. What is added here is what only this
/// crate can name: every module.
pub static CATALOG: Composite = Composite::new(&[
    &erp_web::CATALOG,
    // Not a module. `booking` surfaces the occupancy engine's refusals — "that
    // stylist is already holding one of one" is the most common thing this API
    // says no to — so its codes have to render from here like any other.
    &erp_occupancy::CATALOG,
    // Nor is this. A repeating calendar is one idea two modules need — the
    // diary's opening hours and the rota's shifts — so it sits below both and
    // its refusals render from here for the same reason occupancy's do.
    &erp_recurrence::CATALOG,
    // Nor is this. A short link is infrastructure every module may use, and the
    // three refusals it can make are the only messages in this system a total
    // stranger reads — somebody tapping a URL in a text message.
    &erp_links::CATALOG,
    // Nor this. Where a file lives is infrastructure, and "that came back
    // different from what was stored" is a refusal an API has to be able to
    // render.
    &erp_storage::CATALOG,
    &booking::CATALOG,
    &crm::CATALOG,
    &ledger::CATALOG,
    &sales::CATALOG,
    &branches::CATALOG,
    &prepaid::CATALOG,
    &pos::CATALOG,
    &hr::CATALOG,
    &payroll::CATALOG,
    &hr_sa::CATALOG,
    &purchases::CATALOG,
    &tax_sa::CATALOG,
    &reports::CATALOG,
    &messaging::CATALOG,
    &files::CATALOG,
]);

#[cfg(test)]
mod tests {
    use super::*;
    use erp_i18n::Catalog;

    #[test]
    fn every_crates_messages_render_through_the_composite() {
        erp_i18n::testing::assert_complete(&CATALOG);
    }

    #[test]
    fn no_two_crates_claim_the_same_code() {
        // A duplicate would make "first catalog that has it" depend on the order
        // of the parts, which is exactly the kind of thing nobody notices until
        // one of them is translated differently.
        let mut codes: Vec<_> = CATALOG.codes().to_vec();
        let before = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(before, codes.len(), "duplicate message codes across crates");
    }
}
