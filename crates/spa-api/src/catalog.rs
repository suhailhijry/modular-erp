//! Every crate's messages, behind one lookup.
//!
//! Codes are globally unique by their `domain.` prefix, so "first catalog that
//! has it" is unambiguous. A module's catalog is added here when the module is;
//! forgetting to is caught by [`Catalog::codes`] disagreeing with what the
//! module claims, not by a user seeing a bare code.

use spa_i18n::{Catalog, Locale, MessageCode, StaticCatalog, Template};

/// Ordered by nothing in particular — lookups are by code, which is unique.
static PARTS: &[&StaticCatalog] = &[
    &crate::REQUEST_CATALOG,
    &spa_control::CATALOG,
    &spa_eventlog::CATALOG,
    &ledger::CATALOG,
    &sales::CATALOG,
    &purchases::CATALOG,
];

/// The catalog the API renders from.
pub static CATALOG: Catalogs = Catalogs;

#[derive(Debug)]
pub struct Catalogs;

impl Catalog for Catalogs {
    fn template(&self, locale: Locale, code: &MessageCode) -> Option<Template> {
        PARTS.iter().find_map(|c| c.template(locale, code))
    }

    fn codes(&self) -> &'static [MessageCode] {
        // Concatenated once and leaked. The alternative — returning an empty
        // slice — would make every completeness audit silently pass.
        static ALL: std::sync::OnceLock<&'static [MessageCode]> = std::sync::OnceLock::new();
        ALL.get_or_init(|| {
            Box::leak(
                PARTS
                    .iter()
                    .flat_map(|c| c.codes().iter().cloned())
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_crates_messages_render_through_the_composite() {
        spa_i18n::testing::assert_complete(&CATALOG);
    }

    #[test]
    fn no_two_crates_claim_the_same_code() {
        // A duplicate would make "first catalog that has it" depend on the order
        // of `PARTS`, which is exactly the kind of thing nobody notices until
        // one of them is translated differently.
        let mut codes: Vec<_> = CATALOG.codes().to_vec();
        let before = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(before, codes.len(), "duplicate message codes across crates");
    }
}
