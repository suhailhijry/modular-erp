//! Reading events written by older versions of this system.
//!
//! # The constraint
//!
//! A stored event can never be migrated. The log is append-only by definition,
//! so `ALTER TABLE`'s equivalent does not exist here — the bytes written in 2026
//! are the bytes read in 2030. What moves forward is the *interpretation*.
//!
//! So every event carries the `schema_version` it was written under, and reading
//! one at an older version runs it through a chain of small transformations
//! until it matches what this build expects. `v1 → v2 → v3`, composed, each step
//! doing one thing.
//!
//! # Why the chain, rather than one function per old version
//!
//! Adding a fourth version otherwise means writing three new functions
//! (`v1→v4`, `v2→v4`, `v3→v4`) and getting all three right. With a chain it means
//! writing one (`v3→v4`), and the older paths keep working because they compose
//! through it. The number of things that can be wrong grows linearly instead of
//! quadratically.
//!
//! # Events from the future
//!
//! An event whose version is *newer* than this build understands is refused, not
//! guessed at (law L6). That happens during a rolling deploy if a new pod writes
//! v3 while an old pod is still serving, which is why the deploy order is fixed:
//!
//! 1. Deploy the build that can **read** v3 (upcaster registered) but still
//!    writes v2.
//! 2. Deploy the build that **writes** v3.
//!
//! Two deploys, same as every other expand/contract change in the system. Doing
//! it in one is how a rollback becomes unreadable data.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use spa_types::{EventName, SchemaVersion};

#[derive(Debug, thiserror::Error)]
pub enum UpcastError {
    /// The event was written by a newer build than this one.
    ///
    /// Never guessed past. See the module docs on deploy ordering.
    #[error(
        "{event_name} is at schema version {stored}, but this build only understands \
         up to {current} — it was written by a newer version"
    )]
    FromTheFuture {
        event_name: EventName,
        stored: SchemaVersion,
        current: SchemaVersion,
    },
    /// A step in the chain is missing, so an old event cannot be brought forward.
    #[error("no upcaster from version {from} for {event_name}; the chain is broken")]
    MissingStep {
        event_name: EventName,
        from: SchemaVersion,
    },
    /// The event name has no declared current version.
    #[error("{0} is not registered; declare its current schema version at startup")]
    Undeclared(EventName),
    #[error("upcasting {event_name} from version {from} failed: {reason}")]
    StepFailed {
        event_name: EventName,
        from: SchemaVersion,
        reason: String,
    },
    #[error("{event_name} at version {version} does not match the expected shape: {reason}")]
    Decode {
        event_name: EventName,
        version: SchemaVersion,
        reason: String,
    },
}

/// One step forward: version `n` to version `n + 1`.
///
/// A plain function pointer rather than a closure, so a registry is buildable in
/// a `const`/`static` context and an upcaster cannot accidentally capture
/// mutable state — which would make it non-deterministic and break replay.
pub type UpcastStep = fn(serde_json::Value) -> Result<serde_json::Value, String>;

/// What this build knows about every event's history.
#[derive(Debug, Default)]
pub struct Upcasters {
    /// Event name → the version this build reads and writes.
    current: BTreeMap<String, SchemaVersion>,
    /// (event name, from-version) → the step that produces from-version + 1.
    steps: BTreeMap<(String, i64), UpcastStep>,
}

impl Upcasters {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares the version of an event that this build produces and expects.
    ///
    /// Every event type registers exactly once. An event read at a lower version
    /// is upcast to this; one read at a higher version is refused.
    #[must_use]
    pub fn declare(mut self, event_name: &EventName, current: SchemaVersion) -> Self {
        self.current.insert(event_name.as_str().to_owned(), current);
        self
    }

    /// Registers the step from `from` to `from + 1`.
    #[must_use]
    pub fn step(mut self, event_name: &EventName, from: SchemaVersion, step: UpcastStep) -> Self {
        self.steps
            .insert((event_name.as_str().to_owned(), from.get()), step);
        self
    }

    /// Every gap in every chain, from version 1 to each declared current.
    ///
    /// Run at startup. A missing step means some event already in some tenant's
    /// log cannot be read — and finding that out during a replay, months later,
    /// is the worst possible time.
    #[must_use]
    pub fn gaps(&self) -> Vec<String> {
        let mut gaps = Vec::new();
        for (name, current) in &self.current {
            for version in 1..current.get() {
                if !self.steps.contains_key(&(name.clone(), version)) {
                    gaps.push(format!(
                        "{name}: no upcaster from version {version} (declared current is {current})"
                    ));
                }
            }
        }
        gaps
    }

    /// The version this build reads and writes for an event.
    pub fn current_version(&self, event_name: &EventName) -> Option<SchemaVersion> {
        self.current.get(event_name.as_str()).copied()
    }

    /// Brings a stored payload forward to the current version.
    pub fn upcast(
        &self,
        event_name: &EventName,
        stored: SchemaVersion,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, UpcastError> {
        let current = self
            .current_version(event_name)
            .ok_or_else(|| UpcastError::Undeclared(event_name.clone()))?;

        if stored > current {
            return Err(UpcastError::FromTheFuture {
                event_name: event_name.clone(),
                stored,
                current,
            });
        }

        let mut value = payload;
        let mut version = stored;
        while version < current {
            let step = self
                .steps
                .get(&(event_name.as_str().to_owned(), version.get()))
                .ok_or_else(|| UpcastError::MissingStep {
                    event_name: event_name.clone(),
                    from: version,
                })?;
            value = step(value).map_err(|reason| UpcastError::StepFailed {
                event_name: event_name.clone(),
                from: version,
                reason,
            })?;
            version = version.next();
        }

        Ok(value)
    }

    /// Upcasts and then decodes into the current Rust type.
    pub fn decode<E: DeserializeOwned>(
        &self,
        event_name: &EventName,
        stored: SchemaVersion,
        payload: serde_json::Value,
    ) -> Result<E, UpcastError> {
        let current = self.upcast(event_name, stored, payload)?;
        let version = self
            .current_version(event_name)
            .unwrap_or_else(|| SchemaVersion::ZERO.next());
        serde_json::from_value(current).map_err(|e| UpcastError::Decode {
            event_name: event_name.clone(),
            version,
            reason: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name() -> EventName {
        EventName::new("test.thing").expect("valid")
    }

    fn v(n: i64) -> SchemaVersion {
        SchemaVersion::new(n).expect("valid")
    }

    /// v1 → v2: add a field with a default.
    fn add_currency(mut value: serde_json::Value) -> Result<serde_json::Value, String> {
        let object = value.as_object_mut().ok_or("payload is not an object")?;
        object.insert("currency".into(), serde_json::json!("SAR"));
        Ok(value)
    }

    /// v2 → v3: rename a field.
    fn rename_name_to_label(mut value: serde_json::Value) -> Result<serde_json::Value, String> {
        let object = value.as_object_mut().ok_or("payload is not an object")?;
        let previous = object.remove("name").ok_or("expected a `name` field")?;
        object.insert("label".into(), previous);
        Ok(value)
    }

    fn registry() -> Upcasters {
        Upcasters::new()
            .declare(&name(), v(3))
            .step(&name(), v(1), add_currency)
            .step(&name(), v(2), rename_name_to_label)
    }

    #[test]
    fn a_current_event_passes_through_untouched() {
        let payload = serde_json::json!({ "label": "Cash", "currency": "SAR" });
        assert_eq!(
            registry().upcast(&name(), v(3), payload.clone()).unwrap(),
            payload
        );
    }

    #[test]
    fn an_old_event_is_carried_forward_through_every_step() {
        let v1 = serde_json::json!({ "code": "1000", "name": "Cash" });
        let upcast = registry().upcast(&name(), v(1), v1).unwrap();
        assert_eq!(
            upcast,
            serde_json::json!({ "code": "1000", "label": "Cash", "currency": "SAR" }),
            "v1 should compose through both steps to reach v3"
        );
    }

    #[test]
    fn an_intermediate_version_enters_the_chain_partway() {
        let v2 = serde_json::json!({ "code": "1000", "name": "Cash", "currency": "USD" });
        let upcast = registry().upcast(&name(), v(2), v2).unwrap();
        assert_eq!(
            upcast["currency"],
            serde_json::json!("USD"),
            "an already-set field must not be overwritten by an earlier step"
        );
        assert_eq!(upcast["label"], serde_json::json!("Cash"));
    }

    /// Law L6. An event from a newer build is refused, not interpreted.
    #[test]
    fn an_event_from_the_future_is_refused() {
        let result = registry().upcast(&name(), v(4), serde_json::json!({}));
        assert!(
            matches!(result, Err(UpcastError::FromTheFuture { .. })),
            "got {result:?}"
        );
    }

    #[test]
    fn an_unregistered_event_is_refused() {
        let unknown = EventName::new("test.unknown").unwrap();
        let result = registry().upcast(&unknown, v(1), serde_json::json!({}));
        assert!(matches!(result, Err(UpcastError::Undeclared(_))));
    }

    /// The startup check: a chain with a hole means some stored event is
    /// already unreadable.
    #[test]
    fn gaps_in_a_chain_are_reported_before_anything_reads_them() {
        let broken = Upcasters::new()
            .declare(&name(), v(3))
            .step(&name(), v(1), add_currency);
        // v2 → v3 is missing.

        let gaps = broken.gaps();
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert!(gaps[0].contains("version 2"), "{}", gaps[0]);

        // And a complete chain reports nothing.
        assert!(registry().gaps().is_empty());

        // A v1 event genuinely cannot be read through the broken chain.
        let result = broken.upcast(&name(), v(1), serde_json::json!({ "name": "Cash" }));
        assert!(matches!(result, Err(UpcastError::MissingStep { .. })));
    }

    #[test]
    fn a_failing_step_reports_which_one() {
        // A v1 payload missing the field the v2→v3 step renames.
        let result = registry().upcast(&name(), v(2), serde_json::json!({ "code": "1000" }));
        match result {
            Err(UpcastError::StepFailed { from, reason, .. }) => {
                assert_eq!(from, v(2));
                assert!(reason.contains("name"), "{reason}");
            }
            other => panic!("expected StepFailed, got {other:?}"),
        }
    }

    #[test]
    fn decoding_produces_the_current_rust_type() {
        #[derive(Debug, PartialEq, serde::Deserialize)]
        struct Current {
            code: String,
            label: String,
            currency: String,
        }

        let v1 = serde_json::json!({ "code": "1000", "name": "Cash" });
        let decoded: Current = registry().decode(&name(), v(1), v1).unwrap();
        assert_eq!(
            decoded,
            Current {
                code: "1000".into(),
                label: "Cash".into(),
                currency: "SAR".into(),
            }
        );
    }

    #[test]
    fn a_payload_that_does_not_match_after_upcasting_is_reported() {
        #[derive(Debug, serde::Deserialize)]
        struct Current {
            #[allow(dead_code)]
            required_field: String,
        }

        let result: Result<Current, _> =
            registry().decode(&name(), v(3), serde_json::json!({ "label": "Cash" }));
        assert!(matches!(result, Err(UpcastError::Decode { .. })));
    }
}
