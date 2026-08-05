//! Dynamic facts + conditions, shared by discounts and authorization.
//!
//!   - Facts:        a typed key-value bag producers fill.
//!   - DynCondition: generic comparators over keys - evaluation is
//!                   permanently generic, no per-fact match arms.
//!   - FactRegistry: the REGISTERED VOCABULARY - each domain declares
//!                   its keys (name, type, description) and can extend
//!                   the registry from anywhere. Authoring-time
//!                   validation checks conditions against it, which is
//!                   what keeps "dynamic" from meaning "stringly-typed
//!                   and silently broken": an unknown key or a
//!                   type-incompatible op is rejected WHEN THE RULE IS
//!                   WRITTEN, with the full problem list.
//!
//! Evaluation-time rule fail-closed: a condition referencing a
//! key the producer didn't supply evaluates false. The registry makes
//! that a deliberate semantic (fact absent for this request) rather
//! than a typo trap (validation already proved the key exists).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// =======================================================================
// Values & facts
// =======================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum FactValue {
    Int(i64),
    Str(String),
    Bool(bool),
    Date(chrono::NaiveDate),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    Int,
    Str,
    Bool,
    Date,
}

impl FactValue {
    pub fn kind(&self) -> FactKind {
        match self {
            FactValue::Int(_) => FactKind::Int,
            FactValue::Str(_) => FactKind::Str,
            FactValue::Bool(_) => FactKind::Bool,
            FactValue::Date(_) => FactKind::Date,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Facts {
    values: BTreeMap<String, FactValue>,
}

impl Facts {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(mut self, key: impl Into<String>, value: FactValue) -> Self {
        self.values.insert(key.into(), value);
        self
    }
    pub fn get(&self, key: &str) -> Option<&FactValue> {
        self.values.get(key)
    }
    /// Convenience: derive the standard time facts (year, month,
    /// weekday 1=Mon..7, hour 0..23, date) from one local instant, so
    /// every producer derives them identically.
    pub fn with_local_time(self, dt: chrono::NaiveDateTime) -> Self {
        use chrono::{Datelike, Timelike};
        self.set("year", FactValue::Int(dt.year() as i64))
            .set("month", FactValue::Int(dt.month() as i64))
            .set(
                "weekday",
                FactValue::Int(dt.weekday().number_from_monday() as i64),
            )
            .set("hour", FactValue::Int(dt.hour() as i64))
            .set("date", FactValue::Date(dt.date()))
    }
}

// =======================================================================
// Conditions
// =======================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactOp {
    Eq(FactValue),
    Ne(FactValue),
    /// Int/Date only.
    AtLeast(FactValue),
    AtMost(FactValue),
    /// Inclusive both ends; Int/Date only.
    Between {
        from: FactValue,
        to_inclusive: FactValue,
    },
    /// Membership; homogeneous list.
    In(Vec<FactValue>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DynCondition {
    All(Vec<DynCondition>),
    Any(Vec<DynCondition>),
    Not(Box<DynCondition>),
    Fact { key: String, op: FactOp },
}

fn cmp_values(a: &FactValue, b: &FactValue) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (FactValue::Int(x), FactValue::Int(y)) => Some(x.cmp(y)),
        (FactValue::Date(x), FactValue::Date(y)) => Some(x.cmp(y)),
        _ => None, // ordering undefined across kinds / for Str+Bool
    }
}

impl FactOp {
    fn evaluate(&self, actual: &FactValue) -> bool {
        use std::cmp::Ordering::*;
        match self {
            FactOp::Eq(v) => actual == v,
            FactOp::Ne(v) => actual != v,
            FactOp::AtLeast(v) => matches!(cmp_values(actual, v), Some(Greater | Equal)),
            FactOp::AtMost(v) => matches!(cmp_values(actual, v), Some(Less | Equal)),
            FactOp::Between { from, to_inclusive } => {
                matches!(cmp_values(actual, from), Some(Greater | Equal))
                    && matches!(cmp_values(actual, to_inclusive), Some(Less | Equal))
            }
            FactOp::In(vs) => vs.iter().any(|v| v == actual),
        }
    }

    fn describe(&self) -> String {
        match self {
            FactOp::Eq(v) => format!("== {v:?}"),
            FactOp::Ne(v) => format!("!= {v:?}"),
            FactOp::AtLeast(v) => format!(">= {v:?}"),
            FactOp::AtMost(v) => format!("<= {v:?}"),
            FactOp::Between { from, to_inclusive } => format!("in [{from:?}, {to_inclusive:?}]"),
            FactOp::In(vs) => format!("in {vs:?}"),
        }
    }
}

impl DynCondition {
    /// Fail-closed: an absent fact makes the leaf false.
    pub fn evaluate(&self, facts: &Facts) -> bool {
        match self {
            DynCondition::All(cs) => cs.iter().all(|c| c.evaluate(facts)),
            DynCondition::Any(cs) => cs.iter().any(|c| c.evaluate(facts)),
            DynCondition::Not(c) => !c.evaluate(facts),
            DynCondition::Fact { key, op } => {
                facts.get(key).is_some_and(|actual| op.evaluate(actual))
            }
        }
    }

    /// Non-short-circuiting annotated evaluation for admin explain
    /// endpoints - fully generic, so new facts get explain support for
    /// free.
    pub fn explain(&self, facts: &Facts) -> ConditionVerdict {
        match self {
            DynCondition::All(cs) => {
                let children: Vec<_> = cs.iter().map(|c| c.explain(facts)).collect();
                ConditionVerdict {
                    node: "All".into(),
                    detail: None,
                    result: children.iter().all(|v| v.result),
                    children,
                }
            }
            DynCondition::Any(cs) => {
                let children: Vec<_> = cs.iter().map(|c| c.explain(facts)).collect();
                ConditionVerdict {
                    node: "Any".into(),
                    detail: None,
                    result: children.iter().any(|v| v.result),
                    children,
                }
            }
            DynCondition::Not(c) => {
                let child = c.explain(facts);
                let result = !child.result;
                ConditionVerdict {
                    node: "Not".into(),
                    detail: None,
                    result,
                    children: vec![child],
                }
            }
            DynCondition::Fact { key, op } => {
                let (result, actual_desc) = match facts.get(key) {
                    Some(actual) => (op.evaluate(actual), format!("{actual:?}")),
                    None => (false, "ABSENT".to_string()),
                };
                ConditionVerdict {
                    node: key.clone(),
                    detail: Some(format!("requires {} ; has {}", op.describe(), actual_desc)),
                    result,
                    children: vec![],
                }
            }
        }
    }

    /// Authoring-time validation against the registered vocabulary.
    /// Returns EVERY problem. This - not the closed enum - is now where
    /// typo-safety and type-safety live.
    pub fn validate(&self, registry: &FactRegistry, limits: &ConditionLimits) -> Vec<String> {
        let mut problems = Vec::new();
        let mut nodes = 0usize;
        self.validate_inner(1, registry, limits, &mut nodes, &mut problems);
        if nodes > limits.max_nodes {
            problems.push(format!(
                "condition has {nodes} nodes, exceeding max {}",
                limits.max_nodes
            ));
        }
        problems
    }

    fn validate_inner(
        &self,
        depth: usize,
        registry: &FactRegistry,
        limits: &ConditionLimits,
        nodes: &mut usize,
        problems: &mut Vec<String>,
    ) {
        *nodes += 1;
        if depth > limits.max_depth {
            problems.push(format!("condition exceeds max depth {}", limits.max_depth));
            return;
        }
        match self {
            DynCondition::All(cs) | DynCondition::Any(cs) => {
                if cs.is_empty() {
                    problems.push("empty All/Any branch - vacuous condition".into());
                }
                for c in cs {
                    c.validate_inner(depth + 1, registry, limits, nodes, problems);
                }
            }
            DynCondition::Not(c) => c.validate_inner(depth + 1, registry, limits, nodes, problems),
            DynCondition::Fact { key, op } => {
                let Some(decl) = registry.get(key) else {
                    problems.push(format!(
                        "unknown fact '{key}' - known facts: {}",
                        registry.keys().collect::<Vec<_>>().join(", ")
                    ));
                    return;
                };
                // op/type compatibility
                let check_kind = |v: &FactValue, problems: &mut Vec<String>| {
                    if v.kind() != decl.kind {
                        problems.push(format!(
                            "fact '{key}' is {:?}, but the condition compares against {:?}",
                            decl.kind,
                            v.kind()
                        ));
                    }
                };
                match op {
                    FactOp::Eq(v) | FactOp::Ne(v) => check_kind(v, problems),
                    FactOp::AtLeast(v) | FactOp::AtMost(v) => {
                        check_kind(v, problems);
                        if !matches!(decl.kind, FactKind::Int | FactKind::Date) {
                            problems.push(format!(
                                "ordering op on non-orderable fact '{key}' ({:?})",
                                decl.kind
                            ));
                        }
                    }
                    FactOp::Between { from, to_inclusive } => {
                        check_kind(from, problems);
                        check_kind(to_inclusive, problems);
                        if !matches!(decl.kind, FactKind::Int | FactKind::Date) {
                            problems.push(format!(
                                "Between on non-orderable fact '{key}' ({:?})",
                                decl.kind
                            ));
                        }
                        if cmp_values(from, to_inclusive) == Some(std::cmp::Ordering::Greater) {
                            problems.push(format!("inverted Between range on '{key}'"));
                        }
                    }
                    FactOp::In(vs) => {
                        if vs.is_empty() {
                            problems.push(format!("empty In list on '{key}' - never true"));
                        }
                        for v in vs {
                            check_kind(v, problems);
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConditionVerdict {
    pub node: String,
    pub detail: Option<String>,
    pub result: bool,
    pub children: Vec<ConditionVerdict>,
}

pub struct ConditionLimits {
    pub max_depth: usize,
    pub max_nodes: usize,
}

impl Default for ConditionLimits {
    fn default() -> Self {
        Self {
            max_depth: 10,
            max_nodes: 100,
        }
    }
}

// =======================================================================
// Registry - the extensible vocabulary. Same composition pattern as
// ResourcePolicyRegistry: domains contribute, duplicates panic at
// startup.
// =======================================================================

#[derive(Debug, Clone)]
pub struct FactDecl {
    pub kind: FactKind,
    /// Shown in validation errors and admin UIs - what this fact means
    /// and who supplies it.
    pub description: &'static str,
}

#[derive(Default)]
pub struct FactRegistry {
    decls: BTreeMap<&'static str, FactDecl>,
}

impl FactRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with(mut self, entries: Vec<(&'static str, FactDecl)>) -> Self {
        for (key, decl) in entries {
            if self.decls.insert(key, decl).is_some() {
                panic!("fact '{key}' registered twice - wiring bug");
            }
        }
        self
    }
    pub fn get(&self, key: &str) -> Option<&FactDecl> {
        self.decls.get(key)
    }
    pub fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.decls.keys().copied()
    }
}

/// The standard time facts every registry should include (matching
/// Facts::with_local_time).
pub fn time_facts() -> Vec<(&'static str, FactDecl)> {
    vec![
        (
            "year",
            FactDecl {
                kind: FactKind::Int,
                description: "calendar year of the local instant",
            },
        ),
        (
            "month",
            FactDecl {
                kind: FactKind::Int,
                description: "1-12",
            },
        ),
        (
            "weekday",
            FactDecl {
                kind: FactKind::Int,
                description: "1=Mon..7=Sun",
            },
        ),
        (
            "hour",
            FactDecl {
                kind: FactKind::Int,
                description: "0-23, local time",
            },
        ),
        (
            "date",
            FactDecl {
                kind: FactKind::Date,
                description: "local calendar date",
            },
        ),
    ]
}
