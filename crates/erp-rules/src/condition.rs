//! When a rule applies.

#[cfg(feature = "spans")]
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
    /// A span falls inside a repeating window. See the type docs on why this
    /// one variant is not like the others.
    #[cfg(feature = "spans")]
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
    /// A bool, or a fact that takes one of a list of values, compared by order.
    #[error("{name} is {kind:?} and {op:?} orders nothing of that kind")]
    NotOrderable { name: String, kind: Kind, op: Op },
    /// A value the fact never takes — see [`FactRegistry::one_of`].
    #[error("{name} is never {value:?}; it is one of {}", known.join(", "))]
    NoSuchValue {
        name: String,
        value: String,
        known: Vec<String>,
    },
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
            #[cfg(feature = "spans")]
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
                // quietly never true. A fact with a list of values orders
                // nothing either, for the reason `FactRegistry::one_of` gives.
                let listed = registry.values_of(fact);
                if (declared == Kind::Bool || listed.is_some()) && !matches!(op, Op::Eq | Op::Ne) {
                    return Err(Invalid::NotOrderable {
                        name: fact.clone(),
                        kind: declared,
                        op: *op,
                    });
                }
                if let (Some(known), Value::Text(given)) = (listed, value)
                    && !known.contains(given)
                {
                    return Err(Invalid::NoSuchValue {
                        name: fact.clone(),
                        value: given.clone(),
                        known: known.to_vec(),
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
    ///
    /// A condition that [cannot be told](Self::decide) does not hold either.
    #[must_use]
    pub fn holds(&self, facts: &Facts) -> bool {
        self.decide(facts) == Some(true)
    }

    /// Whether this holds, or `None` when it **cannot be told**: a fact was
    /// supplied that the condition's value does not compare with — an amount
    /// in another currency.
    ///
    /// Not a false: *"is 5,000,000 USD at least 10,000 SAR"* has no answer,
    /// and `Not` of a false would fire. So the unknown travels up — `All` is
    /// false if any part is false, `Any` true if any part is true, and
    /// otherwise an unknown part makes the whole unknown — and whoever acts on
    /// the rule decides what an unknown means. See `Rules::explain_undecided`.
    #[must_use]
    pub fn decide(&self, facts: &Facts) -> Option<bool> {
        match self {
            Self::Always => Some(true),
            Self::All { of } => {
                let mut answer = Some(true);
                for part in of {
                    match part.decide(facts) {
                        Some(false) => return Some(false),
                        None => answer = None,
                        Some(true) => {}
                    }
                }
                answer
            }
            Self::Any { of } => {
                let mut answer = Some(false);
                for part in of {
                    match part.decide(facts) {
                        Some(true) => return Some(true),
                        None => answer = None,
                        Some(false) => {}
                    }
                }
                answer
            }
            Self::Not { of } => of.decide(facts).map(|holds| !holds),
            #[cfg(feature = "spans")]
            Self::Covers { window } => Some(
                facts
                    .span()
                    .is_some_and(|(span, calendar)| window.covers(*span, *calendar)),
            ),
            Self::Is { fact, op, value } => {
                let Some(known) = facts.get(fact) else {
                    return Some(false);
                };
                let order = known.compare(value)?;
                Some(match op {
                    Op::Eq => order.is_eq(),
                    Op::Ne => order.is_ne(),
                    Op::Lt => order.is_lt(),
                    Op::Lte => order.is_le(),
                    Op::Gt => order.is_gt(),
                    Op::Gte => order.is_ge(),
                })
            }
        }
    }
}
