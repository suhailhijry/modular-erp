//! The event log's catalog must be complete in every supported language.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use spa_eventlog::{AppendError, CATALOG};
use spa_i18n::{Catalog, Locale, Localize};
use spa_types::{AggregateId, DomainName, Sequence, StreamId};

#[test]
fn the_catalog_is_complete() {
    spa_i18n::testing::assert_complete(&CATALOG);
}

#[test]
fn every_error_variant_maps_to_a_translated_code() {
    let errors = [
        AppendError::Conflict {
            stream: StreamId::new(
                DomainName::new("ledger_account").unwrap(),
                AggregateId::new("1000").unwrap(),
            ),
            expected: Sequence::ZERO,
        },
        AppendError::Empty,
    ];

    for error in errors {
        let message = error.message();
        for locale in Locale::ALL {
            assert!(
                CATALOG.template(locale, &message.code).is_some(),
                "{error} produced {} with no {} translation",
                message.code,
                locale.code()
            );
        }
    }
}
