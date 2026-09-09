//! When a rule applies.

use erp_recurrence::Availability;
use serde::{Deserialize, Serialize};

use crate::fact::{FactName, FactRegistry, Facts, Kind, Value};

/// How a fact is compared to a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
}

/// **When a rule applies.**
///
/// # Why one variant is not like the others
///
/// [`Self::Is`] compares a scalar fact to a value, and four combinators build
/// on it. [`Self::Covers`] does neither: it carries a whole
/// [`Availability`] and asks whether a *span* falls inside a repeating window.
///
/// That asymmetry is deliberate. `Availability::covers` walks every day a
/// booking touches, gives each end of a daylight-saving change its own offset,
/// and is careful enough that 16:59:30 is inside a window closing at 17:00. Its
/// representation is bit-packed — months, weekdays and days-of-month where
/// **zero means every**, which is what makes the common rule short. None of
/// that has a natural spelling as facts and operators.
///
/// The uniform alternative — a span fact with `covers`/`overlaps` operators —
/// hides the same code behind an operator and buys only the appearance of
/// symmetry. This way the working evaluator stays whole and is one kind of
/// condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "when", rename_all = "snake_case")]
pub enum DynCondition {
    /// Matches everything: the base rate, the default rule, the last line of a
    /// list.
    Always,
    /// **True when empty**, which is the identity for "and" and is what makes
    /// `All(conditions)` behave when a form submits none.
    All {
        of: Vec<DynCondition>,
    },
    /// **False when empty**, the identity for "or".
    Any {
        of: Vec<DynCondition>,
    },
    Not {
        of: Box<DynCondition>,
    },
    Is {
        fact: FactName,
        op: Op,
        value: Value,
    },
    /// A span falls inside a repeating window.
    Covers {
        window: Availability,
    },
}

/// Why a condition could not be written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    #[error("no fact called {name}; this rule kind knows {}", known.join(", "))]
    NoSuchFact { name: String, known: Vec<String> },
    #[error("{name} is {declared:?} and is compared to {given:?}")]
    WrongKind {
        name: String,
        declared: Kind,
        given: Kind,
    },
    #[error("{name} is {kind:?} and {op:?} orders nothing of that kind")]
    NotOrderable { name: String, kind: Kind, op: Op },
    #[error("this rule kind is not about a window of time")]
    NoSpans,
}

impl DynCondition {
    /// **Checks a condition against what the caller will actually know.**
    ///
    /// Called when a rule is *written*, so a rule that can never be true is
    /// refused while its author is still looking at it.
    ///
    /// # Errors
    /// The first problem found, which is the one to show.
    pub fn validate(&self, registry: &FactRegistry) -> Result<(), Invalid> {
        match self {
            Self::Always => Ok(()),
            Self::All { of } | Self::Any { of } => of.iter().try_for_each(|c| c.validate(registry)),
            Self::Not { of } => of.validate(registry),
            Self::Covers { .. } => {
                if registry.takes_spans() {
                    Ok(())
                } else {
                    Err(Invalid::NoSpans)
                }
            }
            Self::Is { fact, op, value } => {
                let declared = registry.kind_of(fact).ok_or_else(|| Invalid::NoSuchFact {
                    name: fact.clone(),
                    known: registry.names().iter().map(|n| (*n).to_owned()).collect(),
                })?;
                if declared != value.kind() {
                    return Err(Invalid::WrongKind {
                        name: fact.clone(),
                        declared,
                        given: value.kind(),
                    });
                }
                // **`Bool` orders nothing.** "Is refundable greater than true"
                // is a question with no answer, and refusing it here is the
                // difference between a typo caught now and a rule that is
                // quietly never true.
                if declared == Kind::Bool && !matches!(op, Op::Eq | Op::Ne) {
                    return Err(Invalid::NotOrderable {
                        name: fact.clone(),
                        kind: declared,
                        op: *op,
                    });
                }
                Ok(())
            }
        }
    }

    /// Whether this holds, given what the caller knows.
    ///
    /// **A fact that is missing is not true.** An unsupplied fact means the
    /// caller could not answer, and a rule that fires on a question nobody
    /// answered is worse than one that does not fire.
    #[must_use]
    pub fn holds(&self, facts: &Facts) -> bool {
        match self {
            Self::Always => true,
            Self::All { of } => of.iter().all(|c| c.holds(facts)),
            Self::Any { of } => of.iter().any(|c| c.holds(facts)),
            Self::Not { of } => !of.holds(facts),
            Self::Covers { window } => facts
                .span()
                .is_some_and(|(span, calendar)| window.covers(*span, *calendar)),
            Self::Is { fact, op, value } => {
                let Some(known) = facts.get(fact) else {
                    return false;
                };
                let Some(order) = known.compare(value) else {
                    return false;
                };
                match op {
                    Op::Eq => order.is_eq(),
                    Op::Ne => order.is_ne(),
                    Op::Lt => order.is_lt(),
                    Op::Lte => order.is_le(),
                    Op::Gt => order.is_gt(),
                    Op::Gte => order.is_ge(),
                }
            }
        }
    }
}
