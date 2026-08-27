//! Golden-file tests: every event shape that has ever existed still decodes.
//!
//! The apparatus in `upcast.rs` is unit-tested against synthetic payloads. This
//! file tests it against *files*, because that is the failure mode that matters:
//! a build that can no longer read data already sitting in a tenant's log.
//!
//! A unit test only proves the upcaster does what its author expected. A golden
//! file proves it does that to the bytes actually stored.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

use erp_eventlog::{DomainEvent, UpcastError, Upcasters};
use erp_types::{EventName, SchemaVersion};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// The event, at its current shape.
//
// A fixture rather than a real domain event: D11 keeps business domain out of
// this crate, and a synthetic type that has been through two shape changes
// exercises the machinery better than a stable real one would.
//
// History:
//   v1  { code, name }
//   v2  + currency          (added, defaulted for existing events)
//   v3  name -> label       (renamed)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AccountOpened {
    code: String,
    label: String,
    currency: String,
}

fn account_opened() -> EventName {
    EventName::new("account.opened").expect("valid")
}

fn v(n: i64) -> SchemaVersion {
    SchemaVersion::new(n).expect("valid")
}

impl DomainEvent for AccountOpened {
    fn event_name(&self) -> EventName {
        account_opened()
    }
    fn schema_version(&self) -> SchemaVersion {
        v(3)
    }
}

/// v1 → v2: everything before multi-currency was in the home currency.
fn add_currency(mut value: serde_json::Value) -> Result<serde_json::Value, String> {
    let object = value.as_object_mut().ok_or("payload is not an object")?;
    object.insert("currency".into(), serde_json::json!("SAR"));
    Ok(value)
}

/// v2 → v3: `name` was ambiguous next to the account holder's name.
fn rename_name_to_label(mut value: serde_json::Value) -> Result<serde_json::Value, String> {
    let object = value.as_object_mut().ok_or("payload is not an object")?;
    let previous = object.remove("name").ok_or("expected a `name` field")?;
    object.insert("label".into(), previous);
    Ok(value)
}

fn upcasters() -> Upcasters {
    Upcasters::new()
        .declare(&account_opened(), v(3))
        .step(&account_opened(), v(1), add_currency)
        .step(&account_opened(), v(2), rename_name_to_label)
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn golden(file: &str) -> serde_json::Value {
    let path = golden_dir().join(file);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------

/// **The load-bearing test.** Every stored shape still decodes into the current
/// type.
#[test]
fn every_golden_version_decodes_into_the_current_shape() {
    let upcasters = upcasters();
    let expected = AccountOpened {
        code: "1000".into(),
        label: "Cash".into(),
        currency: "SAR".into(),
    };

    for version in 1..=3 {
        let stored = golden(&format!("account.opened.v{version}.json"));
        let decoded: AccountOpened = upcasters
            .decode(&account_opened(), v(version), stored)
            .unwrap_or_else(|e| panic!("v{version} no longer decodes: {e}"));

        assert_eq!(
            decoded, expected,
            "v{version} decoded to something different from the other versions; \
             an upcaster is losing or changing data"
        );
    }
}

/// Every golden file must be reachable. A file added without registering its
/// version, or a version registered without a file, are both gaps in the
/// coverage this suite claims to provide.
#[test]
fn every_golden_file_is_covered_and_every_version_has_a_file() {
    let mut found: Vec<i64> = std::fs::read_dir(golden_dir())
        .expect("golden directory exists")
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().to_string_lossy().into_owned();
            let version = name
                .strip_prefix("account.opened.v")?
                .strip_suffix(".json")?;
            version.parse().ok()
        })
        .collect();
    found.sort_unstable();

    let current = upcasters()
        .current_version(&account_opened())
        .expect("declared");
    let expected: Vec<i64> = (1..=current.get()).collect();

    assert_eq!(
        found, expected,
        "golden files must cover every version from 1 to the current one ({current}); \
         a missing file means an untested shape, an extra one means a version that \
         was removed rather than superseded"
    );
}

/// The chain has no holes, checked the way a startup check would.
#[test]
fn the_upcaster_chain_is_complete() {
    let gaps = upcasters().gaps();
    assert!(gaps.is_empty(), "incomplete upcaster chain:\n  {gaps:?}");
}

/// What a `DomainEvent` says it writes must match what the registry expects to
/// read. If they drift, this build writes events its own reader rejects.
#[test]
fn the_event_and_the_registry_agree_on_the_current_version() {
    let sample = AccountOpened {
        code: "1000".into(),
        label: "Cash".into(),
        currency: "SAR".into(),
    };
    assert_eq!(
        Some(sample.schema_version()),
        upcasters().current_version(&sample.event_name()),
        "DomainEvent::schema_version disagrees with the registry's declared current"
    );
}

/// An event written by a newer build is refused rather than misread.
#[test]
fn an_event_from_a_newer_build_is_refused() {
    let result: Result<AccountOpened, _> = upcasters().decode(
        &account_opened(),
        v(4),
        serde_json::json!({ "code": "1000", "label": "Cash", "currency": "SAR" }),
    );
    match result {
        Err(UpcastError::FromTheFuture {
            stored, current, ..
        }) => {
            assert_eq!(stored, v(4));
            assert_eq!(current, v(3));
        }
        other => panic!("expected FromTheFuture, got {other:?}"),
    }
}

/// Serializing the current type produces exactly the newest golden file.
///
/// Catches the other direction: a field added to the Rust type without bumping
/// the version and adding a file. Without this, new events would be written in a
/// shape no golden file covers.
#[test]
fn the_current_type_serializes_to_the_newest_golden_file() {
    let current = AccountOpened {
        code: "1000".into(),
        label: "Cash".into(),
        currency: "SAR".into(),
    };
    assert_eq!(
        serde_json::to_value(&current).unwrap(),
        golden("account.opened.v3.json"),
        "the current type no longer matches the newest golden file — bump the \
         schema version, add an upcaster, and add a new golden file"
    );
}
