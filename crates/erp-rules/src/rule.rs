//! A rule, an ordered set of them, and the answer to "why".

use serde::{Deserialize, Serialize};

use crate::{
    condition::{DynCondition, Invalid},
    fact::{FactRegistry, Facts},
};

/// **One rule: when it applies, and what follows.**
///
/// # Why this is smaller than the architecture's sketch
///
/// ARCHITECTURE §5.6 also lists `id`, `version`, `priority`, `effective` and
/// `origin`. None is here, and each omission is an argument rather than an
/// oversight:
///
/// - **`priority`** — [`Rules`] is ordered and the first match wins, which is
///   what pricing already does. A priority *and* an order is two ways to say
///   one thing, and they disagree eventually.
/// - **`effective`** — [`Availability`](erp_recurrence::Availability) already
///   carries `from` and `until`. A second date range on the rule would be a
///   second answer to the same question.
/// - **`origin`** — records which authoring level produced a rule. There is one
///   today, so it would have one value.
/// - **`id`/`version`** — a whole rule set is one versioned configuration entry
///   with an `ETag`. Per-rule versions are for editing rules individually.
///
/// Each arrives with the consumer that needs it. Five fields nothing reads is
/// the shape of every abstraction that later has to be unpicked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule<E> {
    /// What the business calls it. Printed beside the outcome, so "Peak" and
    /// "Ramadan evenings" rather than an index.
    pub name: String,
    pub when: DynCondition,
    pub then: E,
}

/// **An ordered set of rules. The first that matches wins.**
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Rules<E> {
    rules: Vec<Rule<E>>,
}

impl<E> Default for Rules<E> {
    fn default() -> Self {
        Self { rules: Vec::new() }
    }
}

/// One rule that was tried, and how it went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Considered<'a> {
    pub name: &'a str,
    /// Whether this one matched. At most one is true, and it is the last
    /// entry — the evaluator stops there.
    pub matched: bool,
}

/// **What the rules decided, and what they considered on the way.**
///
/// Returned by [`Rules::explain`], from which [`Rules::evaluate`] takes its
/// answer. They are one function for the same reason `preview_chart` and
/// `install_chart` are: two implementations of "which rule wins" would
/// eventually disagree, and the disagreement would surface as a price nobody
/// could account for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explained<'a, E> {
    pub matched: Option<&'a Rule<E>>,
    /// Every rule tried, in order, up to and including the one that matched.
    pub considered: Vec<Considered<'a>>,
}

impl<E> Rules<E> {
    #[must_use]
    pub const fn new(rules: Vec<Rule<E>>) -> Self {
        Self { rules }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    #[must_use]
    pub fn as_slice(&self) -> &[Rule<E>] {
        &self.rules
    }

    /// **Checks every condition against what a caller will know.**
    ///
    /// Run when a rule set is *written*. See [`FactRegistry`].
    ///
    /// # Errors
    /// The index of the first rule that cannot be true, and why.
    pub fn validate(&self, registry: &FactRegistry) -> Result<(), (usize, Invalid)> {
        for (at, rule) in self.rules.iter().enumerate() {
            rule.when.validate(registry).map_err(|e| (at, e))?;
        }
        Ok(())
    }

    /// **Which rule applies, and what was considered getting there.**
    #[must_use]
    pub fn explain(&self, facts: &Facts) -> Explained<'_, E> {
        let mut considered = Vec::new();
        for rule in &self.rules {
            let matched = rule.when.holds(facts);
            considered.push(Considered {
                name: &rule.name,
                matched,
            });
            if matched {
                return Explained {
                    matched: Some(rule),
                    considered,
                };
            }
        }
        Explained {
            matched: None,
            considered,
        }
    }

    /// Which rule applies. [`Self::explain`] with the reasoning dropped.
    #[must_use]
    pub fn evaluate(&self, facts: &Facts) -> Option<&Rule<E>> {
        self.explain(facts).matched
    }
}
