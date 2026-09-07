//! Facts one crate states about another, checked where both are in scope.
//!
//! `files` names the event-log domain of every record a document can go on,
//! as a string, because depending on six modules for one fact each would make
//! it require all of them. This is where the strings meet the aggregates.

use erp_eventlog::Aggregate as _;
use files::OwnerKind;

/// **Every owner kind names the domain its module actually writes to.** A
/// renamed aggregate would otherwise make every document of that kind
/// unattachable — `owner_exists` would find an empty stream — with nothing at
/// compile time to say so.
#[test]
fn every_owner_kind_names_the_domain_its_module_uses() {
    for kind in OwnerKind::ALL {
        let expected = match kind {
            OwnerKind::Invoice => Some(sales::Invoice::domain()),
            OwnerKind::Bill => Some(purchases::Bill::domain()),
            OwnerKind::Reservation => Some(booking::Reservation::domain()),
            OwnerKind::Customer => Some(crm::Customer::domain()),
            OwnerKind::Employee => Some(hr::Employee::domain()),
            OwnerKind::Entry => Some(ledger::JournalEntry::domain()),
            OwnerKind::Tenant => None,
        };
        assert_eq!(
            kind.domain(),
            expected.as_ref().map(erp_types::DomainName::as_str),
            "{kind:?} names a domain its module does not write to"
        );
    }
}
