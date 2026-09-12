//! What a rule may ask about, and what it is worth.

use std::collections::BTreeMap;

use erp_types::Money;
use serde::{Deserialize, Serialize};

/// A fact's name, as a condition spells it. Never parsed for meaning here — the
/// registry says which are real.
pub type FactName = String;

/// What a fact is worth, and what a condition compares against.
/// **Adjacently tagged**, not internally: serde cannot put a tag *inside* a
/// newtype variant wrapping a primitive, so `{"type": "int", "of": 3}` rather
/// than `{"type": "int", ...}`. Found by the round-trip test, which is why it
/// exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "of", rename_all = "snake_case")]
pub enum Value {
    Int(i64),
    Text(String),
    Bool(bool),
    /// **Money is not an integer**, because two amounts in different currencies
    /// are not comparable and an integer would compare them happily.
    Money(Money),
}

impl Value {
    /// Which kind this is, for the registry to check a condition against.
    #[must_use]
    pub const fn kind(&self) -> Kind {
        match self {
            Self::Int(_) => Kind::Int,
            Self::Text(_) => Kind::Text,
            Self::Bool(_) => Kind::Bool,
            Self::Money(_) => Kind::Money,
        }
    }

    /// Orders two values of the same kind.
    ///
    /// `None` when they are different kinds — which the registry refuses when a
    /// rule is written, so reaching it means a fact was supplied with the wrong
    /// kind — or two amounts in different currencies, which nothing can refuse
    /// earlier: a rule's currency is known when it is written, an amount's only
    /// when one is asked about. `DynCondition::decide` carries the `None` up.
    #[must_use]
    pub fn compare(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => Some(a.cmp(b)),
            (Self::Text(a), Self::Text(b)) => Some(a.cmp(b)),
            (Self::Bool(a), Self::Bool(b)) => Some(a.cmp(b)),
            // Same currency or no answer. **Not a false**: "is 100 SAR under
            // 100 USD" has no truth value, and answering one invents a rate.
            (Self::Money(a), Self::Money(b)) if a.currency() == b.currency() => {
                Some(a.minor().cmp(&b.minor()))
            }
            _ => None,
        }
    }
}

/// A fact's type, which is what a condition is checked against when written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Int,
    Text,
    Bool,
    Money,
}

/// **What the caller knows when a rule is evaluated.**
///
/// Scalars by name, and the span when there is one. A span is not a named fact
/// because nothing compares it to a value — `DynCondition::Covers` asks a
/// question about it that no operator expresses. See `condition.rs`.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    values: BTreeMap<FactName, Value>,
    #[cfg(feature = "spans")]
    span: Option<(erp_occupancy::Span, erp_types::Calendar)>,
}

impl Facts {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with(mut self, name: impl Into<FactName>, value: Value) -> Self {
        self.values.insert(name.into(), value);
        self
    }

    /// The window this decision is about, on the tenant's clock.
    #[cfg(feature = "spans")]
    #[must_use]
    pub fn over(mut self, span: erp_occupancy::Span, calendar: erp_types::Calendar) -> Self {
        self.span = Some((span, calendar));
        self
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.values.get(name)
    }

    #[cfg(feature = "spans")]
    #[must_use]
    pub const fn span(&self) -> Option<&(erp_occupancy::Span, erp_types::Calendar)> {
        self.span.as_ref()
    }
}

/// **Which facts exist, and of what type.**
///
/// # Why this exists at all
///
/// So a rule that can never be true is refused **when it is written**, not when
/// somebody's request hits it. A condition naming a fact nobody supplies, or
/// comparing an amount to a word, is a mistake the author can still fix; the
/// same mistake discovered at a till is a support call about a price that
/// looks wrong.
///
/// A registry is per rule *kind*, not global: pricing's facts and
/// authorization's share nothing, and one list of both would let a pricing rule
/// name a fact only an authorization check supplies.
#[derive(Debug, Clone, Default)]
pub struct FactRegistry {
    known: BTreeMap<FactName, Kind>,
    /// The only values some text facts ever take. See [`Self::one_of`].
    values: BTreeMap<FactName, Vec<String>>,
    /// Whether conditions here may ask about a span.
    spans: bool,
}

impl FactRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn fact(mut self, name: impl Into<FactName>, kind: Kind) -> Self {
        self.known.insert(name.into(), kind);
        self
    }

    /// Declares a text fact that only ever takes one of `values`.
    ///
    /// **So a misspelt value is refused when it is written.** Without the list,
    /// `capability == "post_entires"` has the right fact and the right kind,
    /// validates, and is never once true. Nothing orders such a fact either:
    /// `role < "clerk"` compares spellings, not anything the business means.
    #[must_use]
    pub fn one_of(
        mut self,
        name: impl Into<FactName>,
        values: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let name = name.into();
        self.known.insert(name.clone(), Kind::Text);
        self.values
            .insert(name, values.into_iter().map(Into::into).collect());
        self
    }

    /// Declares that a decision here is about a window of time, so
    /// `DynCondition::Covers` is available.
    #[must_use]
    pub const fn over_spans(mut self) -> Self {
        self.spans = true;
        self
    }

    #[must_use]
    pub fn kind_of(&self, name: &str) -> Option<Kind> {
        self.known.get(name).copied()
    }

    /// The only values a fact declared with [`Self::one_of`] takes.
    #[must_use]
    pub fn values_of(&self, name: &str) -> Option<&[String]> {
        self.values.get(name).map(Vec::as_slice)
    }

    #[must_use]
    pub const fn takes_spans(&self) -> bool {
        self.spans
    }

    /// Every fact this registry knows, for an error that can name them.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.known.keys().map(String::as_str).collect()
    }
}
