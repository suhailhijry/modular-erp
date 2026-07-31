//! Discounts, split into two layers that must not be conflated:
//!
//! - RULES (this module's aggregates): admin-authored, condition-based,
//!   living independently of any invoice. Rules are what an admin
//!   creates, edits, deactivates.
//!
//! - APPLIED DISCOUNTS (`AppliedDiscount`): concrete minor-unit amounts
//!   with provenance, computed by the evaluation service at invoice
//!   creation and recorded ON the invoice. The invoice never stores or
//!   re-evaluates rules - it stores results, so the document is a
//!   complete audit record of exactly what was granted, from which
//!   source, and why, even if the rule is later edited or deleted.
//!
//! Loyalty-points discounts (client point balances usable "as payment"
//! but accounted as a discount) are a FIFTH source, deliberately left
//! as a marked extension point until specified - see
//! `DiscountSource::LoyaltyPoints` placeholder note at the bottom.

use crate::event_sourcing::*;
use serde::{Deserialize, Serialize};

// =======================================================================
// Value: every discount is either a fixed amount or a percentage.
// =======================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscountValue {
    /// Minor units (halalas/cents).
    Fixed(i64),
    /// Basis points: 1000 = 10%.
    Percentage(i32),
}

impl DiscountValue {
    /// Concrete amount against a base, capped at the base - a discount
    /// can never exceed what it discounts.
    pub fn amount_against(&self, base_minor: i64) -> i64 {
        let raw = match self {
            DiscountValue::Fixed(v) => *v,
            DiscountValue::Percentage(bp) => base_minor * (*bp as i64) / 10_000,
        };
        raw.clamp(0, base_minor)
    }
}

// =======================================================================
// Conditions for AUTOMATIC discounts - a serializable predicate tree,
// evaluated against facts the evaluation service assembles. The tree
// shape means "buying in April, in the morning, in 2026, AND loyal for
// 3+ years" is data, not code.
// =======================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Condition {
    All(Vec<Condition>),
    Any(Vec<Condition>),
    Not(Box<Condition>),

    // --- client facts ---
    /// Full years since the client's first purchase.
    ClientLoyaltyYearsAtLeast(u32),
    /// Lifetime spend, minor units.
    ClientTotalSpentAtLeast(i64),
    /// Share of past invoices paid by their due date, in basis points
    /// (9000 = 90%). "Payment history" conditions build on this.
    ClientOnTimePaymentRatioAtLeastBp(i32),
    ClientInvoiceCountAtLeast(u32),

    // --- time-of-purchase facts (evaluated in the business's local tz,
    //     supplied by the evaluation context - NOT re-derived per node,
    //     so one evaluation sees one consistent instant) ---
    YearIs(i32),
    MonthIn(Vec<u32>),     // 1..=12
    DayOfWeekIn(Vec<u32>), // 1=Mon..7=Sun
    HourBetween {
        from: u32,
        to_exclusive: u32,
    }, // 0..24; from > to wraps midnight
    DateBetween {
        start: chrono::NaiveDate,
        end_inclusive: chrono::NaiveDate,
    },
}

/// The facts a condition tree is judged against. Assembled ONCE per
/// evaluation by the service (client facts come from read models),
/// so every node in the tree sees the same consistent snapshot.
#[derive(Debug, Clone)]
pub struct EvaluationFacts {
    pub client_loyalty_years: u32,
    pub client_total_spent_minor: i64,
    pub client_on_time_ratio_bp: i32,
    pub client_invoice_count: u32,
    /// Purchase instant in the business's local timezone.
    pub local_datetime: chrono::NaiveDateTime,
}

impl Condition {
    pub fn evaluate(&self, facts: &EvaluationFacts) -> bool {
        match self {
            Condition::All(cs) => cs.iter().all(|c| c.evaluate(facts)),
            Condition::Any(cs) => cs.iter().any(|c| c.evaluate(facts)),
            Condition::Not(c) => !c.evaluate(facts),

            Condition::ClientLoyaltyYearsAtLeast(n) => facts.client_loyalty_years >= *n,
            Condition::ClientTotalSpentAtLeast(v) => facts.client_total_spent_minor >= *v,
            Condition::ClientOnTimePaymentRatioAtLeastBp(bp) => {
                facts.client_on_time_ratio_bp >= *bp
            }
            Condition::ClientInvoiceCountAtLeast(n) => facts.client_invoice_count >= *n,

            Condition::YearIs(y) => {
                use chrono::Datelike;
                facts.local_datetime.year() == *y
            }
            Condition::MonthIn(months) => {
                use chrono::Datelike;
                months.contains(&facts.local_datetime.month())
            }
            Condition::DayOfWeekIn(days) => {
                use chrono::Datelike;
                days.contains(&facts.local_datetime.weekday().number_from_monday())
            }
            Condition::HourBetween { from, to_exclusive } => {
                use chrono::Timelike;
                let h = facts.local_datetime.hour();
                if from <= to_exclusive {
                    h >= *from && h < *to_exclusive
                } else {
                    // wraps midnight: e.g. 22..6 = late night
                    h >= *from || h < *to_exclusive
                }
            }
            Condition::DateBetween {
                start,
                end_inclusive,
            } => {
                let d = facts.local_datetime.date();
                d >= *start && d <= *end_inclusive
            }
        }
    }

    /// Non-short-circuiting evaluation for the admin "explain" mode:
    /// every node gets a verdict, even branches an All/Any would have
    /// skipped in production. Production paths must keep using
    /// `evaluate` (which short-circuits); this exists so an admin can
    /// see WHY a rule did or didn't fire.
    pub fn explain(&self, facts: &EvaluationFacts) -> ConditionVerdict {
        match self {
            Condition::All(cs) => {
                let children: Vec<ConditionVerdict> = cs.iter().map(|c| c.explain(facts)).collect();
                let result = children.iter().all(|v| v.result);
                ConditionVerdict {
                    node: "All".into(),
                    detail: None,
                    result,
                    children,
                }
            }
            Condition::Any(cs) => {
                let children: Vec<ConditionVerdict> = cs.iter().map(|c| c.explain(facts)).collect();
                let result = children.iter().any(|v| v.result);
                ConditionVerdict {
                    node: "Any".into(),
                    detail: None,
                    result,
                    children,
                }
            }
            Condition::Not(c) => {
                let child = c.explain(facts);
                let result = !child.result;
                ConditionVerdict {
                    node: "Not".into(),
                    detail: None,
                    result,
                    children: vec![child],
                }
            }
            leaf => {
                // Leaves: reuse the real evaluation, and describe both
                // the requirement and the actual fact it was judged
                // against, so a false verdict is self-explanatory.
                let result = leaf.evaluate(facts);
                let detail = Some(match leaf {
                    Condition::ClientLoyaltyYearsAtLeast(n) => format!(
                        "requires >= {n} loyalty years; client has {}",
                        facts.client_loyalty_years
                    ),
                    Condition::ClientTotalSpentAtLeast(v) => format!(
                        "requires >= {v} lifetime spend (minor); client has {}",
                        facts.client_total_spent_minor
                    ),
                    Condition::ClientOnTimePaymentRatioAtLeastBp(bp) => format!(
                        "requires >= {bp}bp on-time ratio; client has {}bp",
                        facts.client_on_time_ratio_bp
                    ),
                    Condition::ClientInvoiceCountAtLeast(n) => format!(
                        "requires >= {n} invoices; client has {}",
                        facts.client_invoice_count
                    ),
                    Condition::YearIs(y) => format!(
                        "requires year {y}; purchase is {}",
                        facts.local_datetime.format("%Y")
                    ),
                    Condition::MonthIn(m) => {
                        format!("requires month in {m:?}; purchase month is {}", {
                            use chrono::Datelike;
                            facts.local_datetime.month()
                        })
                    }
                    Condition::DayOfWeekIn(d) => format!(
                        "requires weekday in {d:?} (1=Mon); purchase weekday is {}",
                        {
                            use chrono::Datelike;
                            facts.local_datetime.weekday().number_from_monday()
                        }
                    ),
                    Condition::HourBetween { from, to_exclusive } => format!(
                        "requires hour in [{from}, {to_exclusive}); purchase hour is {}",
                        {
                            use chrono::Timelike;
                            facts.local_datetime.hour()
                        }
                    ),
                    Condition::DateBetween {
                        start,
                        end_inclusive,
                    } => format!(
                        "requires date in [{start}, {end_inclusive}]; purchase date is {}",
                        facts.local_datetime.date()
                    ),
                    _ => unreachable!("branch nodes handled above"),
                });
                ConditionVerdict {
                    node: leaf.node_name().into(),
                    detail,
                    result,
                    children: vec![],
                }
            }
        }
    }

    fn node_name(&self) -> &'static str {
        match self {
            Condition::All(_) => "All",
            Condition::Any(_) => "Any",
            Condition::Not(_) => "Not",
            Condition::ClientLoyaltyYearsAtLeast(_) => "ClientLoyaltyYearsAtLeast",
            Condition::ClientTotalSpentAtLeast(_) => "ClientTotalSpentAtLeast",
            Condition::ClientOnTimePaymentRatioAtLeastBp(_) => "ClientOnTimePaymentRatioAtLeastBp",
            Condition::ClientInvoiceCountAtLeast(_) => "ClientInvoiceCountAtLeast",
            Condition::YearIs(_) => "YearIs",
            Condition::MonthIn(_) => "MonthIn",
            Condition::DayOfWeekIn(_) => "DayOfWeekIn",
            Condition::HourBetween { .. } => "HourBetween",
            Condition::DateBetween { .. } => "DateBetween",
        }
    }

    /// Structural + semantic validation, run at rule Create/Update time
    /// (NOT at evaluation time - a rule that passed validation once is
    /// trusted thereafter; stored rules never re-validate on the draft
    /// hot path). Returns every problem found, not just the first, so
    /// an admin fixes the rule in one round trip.
    pub fn validate(&self, limits: &ConditionLimits) -> Vec<ConditionProblem> {
        let mut problems = Vec::new();
        let mut node_count = 0usize;
        self.validate_inner(1, limits, &mut node_count, &mut problems);
        if node_count > limits.max_nodes {
            problems.push(ConditionProblem::TooManyNodes {
                count: node_count,
                max: limits.max_nodes,
            });
        }
        problems
    }

    fn validate_inner(
        &self,
        depth: usize,
        limits: &ConditionLimits,
        node_count: &mut usize,
        problems: &mut Vec<ConditionProblem>,
    ) {
        *node_count += 1;
        if depth > limits.max_depth {
            problems.push(ConditionProblem::TooDeep {
                depth,
                max: limits.max_depth,
            });
            return; // don't recurse further into an already-too-deep branch
        }
        match self {
            Condition::All(cs) | Condition::Any(cs) => {
                if cs.is_empty() {
                    // All([]) is vacuously true, Any([]) vacuously false -
                    // both are almost certainly authoring mistakes.
                    problems.push(ConditionProblem::EmptyBranch {
                        node: self.node_name(),
                    });
                }
                for c in cs {
                    c.validate_inner(depth + 1, limits, node_count, problems);
                }
            }
            Condition::Not(c) => c.validate_inner(depth + 1, limits, node_count, problems),

            Condition::MonthIn(months) => {
                if months.is_empty() {
                    problems.push(ConditionProblem::EmptyBranch { node: "MonthIn" });
                }
                for m in months {
                    if !(1..=12).contains(m) {
                        problems.push(ConditionProblem::InvalidMonth(*m));
                    }
                }
            }
            Condition::DayOfWeekIn(days) => {
                if days.is_empty() {
                    problems.push(ConditionProblem::EmptyBranch {
                        node: "DayOfWeekIn",
                    });
                }
                for d in days {
                    if !(1..=7).contains(d) {
                        problems.push(ConditionProblem::InvalidWeekday(*d));
                    }
                }
            }
            Condition::HourBetween { from, to_exclusive } => {
                if *from > 23 || *to_exclusive > 24 {
                    problems.push(ConditionProblem::InvalidHourRange {
                        from: *from,
                        to_exclusive: *to_exclusive,
                    });
                }
                if from == to_exclusive {
                    // Ambiguous: empty window or full day? Refuse; the
                    // admin should say what they mean (omit the node for
                    // "always", or fix the range).
                    problems.push(ConditionProblem::AmbiguousHourRange(*from));
                }
            }
            Condition::DateBetween {
                start,
                end_inclusive,
            } => {
                if start > end_inclusive {
                    problems.push(ConditionProblem::InvertedDateRange {
                        start: *start,
                        end_inclusive: *end_inclusive,
                    });
                }
            }
            Condition::ClientTotalSpentAtLeast(v) => {
                if *v < 0 {
                    problems.push(ConditionProblem::NegativeAmount(*v));
                }
            }
            Condition::ClientOnTimePaymentRatioAtLeastBp(bp) => {
                if !(0..=10_000).contains(bp) {
                    problems.push(ConditionProblem::InvalidRatioBp(*bp));
                }
            }
            // Remaining leaves have no invalid states beyond their types.
            Condition::ClientLoyaltyYearsAtLeast(_)
            | Condition::ClientInvoiceCountAtLeast(_)
            | Condition::YearIs(_) => {}
        }
    }
}

/// Result tree from `explain` - serialize straight to JSON for the
/// admin dry-run endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct ConditionVerdict {
    pub node: String,
    /// For leaves: "requires X; client/purchase has Y".
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
        // Covers any sane business rule; blocks stack-blowing or
        // evaluation-slowing trees on the draft hot path.
        Self {
            max_depth: 10,
            max_nodes: 100,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error, Serialize)]
pub enum ConditionProblem {
    #[error("condition tree exceeds max depth {max} (found depth {depth})")]
    TooDeep { depth: usize, max: usize },
    #[error("condition tree has {count} nodes, exceeding max {max}")]
    TooManyNodes { count: usize, max: usize },
    #[error("{node} has no children/entries - vacuous conditions are almost certainly a mistake")]
    EmptyBranch { node: &'static str },
    #[error("invalid month {0} (must be 1-12)")]
    InvalidMonth(u32),
    #[error("invalid weekday {0} (must be 1=Mon..7=Sun)")]
    InvalidWeekday(u32),
    #[error("invalid hour range [{from}, {to_exclusive}) (from must be 0-23, to 0-24)")]
    InvalidHourRange { from: u32, to_exclusive: u32 },
    #[error("hour range [{0}, {0}) is ambiguous - omit the node for 'always', or fix the range")]
    AmbiguousHourRange(u32),
    #[error("date range starts {start} after it ends {end_inclusive}")]
    InvertedDateRange {
        start: chrono::NaiveDate,
        end_inclusive: chrono::NaiveDate,
    },
    #[error("negative amount {0}")]
    NegativeAmount(i64),
    #[error("on-time ratio {0}bp out of range (0-10000)")]
    InvalidRatioBp(i32),
}

// =======================================================================
// Aggregate 1: automatic discount rules (admin-created, conditional)
// =======================================================================

#[derive(Debug, Clone, Serialize, Deserialize, DomainEvent)]
#[event(prefix = "discount_rule")]
pub enum DiscountRuleEvent {
    Created {
        name: String,
        value: DiscountValue,
        condition: Condition,
        combinable: bool,
    },
    Updated {
        name: String,
        value: DiscountValue,
        condition: Condition,
        combinable: bool,
    },
    Deactivated,
    Reactivated,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, AggregateMeta)]
#[aggregate(type = "discount_rule")]
pub struct DiscountRule {
    id: String,
    version: u64,
    name: String,
    value: Option<DiscountValue>,
    condition: Option<Condition>,
    /// Whether this rule may combine with other discounts. Non-combinable
    /// rules compete: the evaluation picks whichever single option
    /// benefits the client most vs. the combinable stack (default
    /// policy, adjustable in the evaluator).
    combinable: bool,
    active: bool,
}

impl DiscountRule {
    pub fn is_active(&self) -> bool {
        self.active
    }
    pub fn value(&self) -> Option<DiscountValue> {
        self.value
    }
    pub fn condition(&self) -> Option<&Condition> {
        self.condition.as_ref()
    }
    pub fn combinable(&self) -> bool {
        self.combinable
    }
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Clone)]
pub enum DiscountRuleCommand {
    Create {
        name: String,
        value: DiscountValue,
        condition: Condition,
        combinable: bool,
    },
    Update {
        name: String,
        value: DiscountValue,
        condition: Condition,
        combinable: bool,
    },
    Deactivate,
    Reactivate,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscountRuleError {
    #[error("rule already exists")]
    AlreadyExists,
    #[error("rule does not exist")]
    NotFound,
    #[error("rule is already in the requested state")]
    NoChange,
    #[error("invalid condition tree: {}", problems.iter().map(|p| p.to_string()).collect::<Vec<_>>().join("; "))]
    InvalidCondition { problems: Vec<ConditionProblem> },
}

impl Aggregate for DiscountRule {
    type Event = DiscountRuleEvent;
    type Command = DiscountRuleCommand;
    type Error = DiscountRuleError;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            DiscountRuleEvent::Created {
                name,
                value,
                condition,
                combinable,
            }
            | DiscountRuleEvent::Updated {
                name,
                value,
                condition,
                combinable,
            } => {
                self.name = name.clone();
                self.value = Some(*value);
                self.condition = Some(condition.clone());
                self.combinable = *combinable;
                if matches!(event, DiscountRuleEvent::Created { .. }) {
                    self.active = true;
                }
            }
            DiscountRuleEvent::Deactivated => self.active = false,
            DiscountRuleEvent::Reactivated => self.active = true,
        }
        self.version += 1;
    }

    fn handle(&self, command: Self::Command) -> Result<Vec<Self::Event>, Self::Error> {
        match command {
            DiscountRuleCommand::Create {
                name,
                value,
                condition,
                combinable,
            } => {
                if self.version != 0 {
                    return Err(DiscountRuleError::AlreadyExists);
                }
                let problems = condition.validate(&ConditionLimits::default());
                if !problems.is_empty() {
                    return Err(DiscountRuleError::InvalidCondition { problems });
                }
                Ok(vec![DiscountRuleEvent::Created {
                    name,
                    value,
                    condition,
                    combinable,
                }])
            }
            DiscountRuleCommand::Update {
                name,
                value,
                condition,
                combinable,
            } => {
                if self.version == 0 {
                    return Err(DiscountRuleError::NotFound);
                }
                let problems = condition.validate(&ConditionLimits::default());
                if !problems.is_empty() {
                    return Err(DiscountRuleError::InvalidCondition { problems });
                }
                Ok(vec![DiscountRuleEvent::Updated {
                    name,
                    value,
                    condition,
                    combinable,
                }])
            }
            DiscountRuleCommand::Deactivate => {
                if !self.active {
                    return Err(DiscountRuleError::NoChange);
                }
                Ok(vec![DiscountRuleEvent::Deactivated])
            }
            DiscountRuleCommand::Reactivate => {
                if self.active {
                    return Err(DiscountRuleError::NoChange);
                }
                Ok(vec![DiscountRuleEvent::Reactivated])
            }
        }
    }
}

// =======================================================================
// Aggregate 2: targeted discounts - on an item, or on an item FOR a
// specific client. One aggregate, scope carried in the id AND the
// state: the aggregate id is "item:{item_id}" or
// "item:{item_id}:client:{client_id}", so lookup at evaluation time is
// two direct loads per line, no scanning.
// =======================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetScope {
    /// Applies to every sale of this item/service/package.
    Item { item_id: String },
    /// Applies only when THIS client buys THIS item - a negotiated,
    /// client-specific price agreement.
    ClientItem { item_id: String, client_id: String },
}

impl TargetScope {
    pub fn aggregate_id(&self) -> String {
        match self {
            TargetScope::Item { item_id } => format!("item:{item_id}"),
            TargetScope::ClientItem { item_id, client_id } => {
                format!("item:{item_id}:client:{client_id}")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, DomainEvent)]
#[event(prefix = "targeted_discount")]
pub enum TargetedDiscountEvent {
    Set {
        scope: TargetScope,
        value: DiscountValue,
        combinable: bool,
    },
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, AggregateMeta)]
#[aggregate(type = "targeted_discount")]
pub struct TargetedDiscount {
    id: String,
    version: u64,
    scope: Option<TargetScope>,
    value: Option<DiscountValue>,
    combinable: bool,
    active: bool,
}

impl TargetedDiscount {
    pub fn active_value(&self) -> Option<DiscountValue> {
        if self.active { self.value } else { None }
    }
    pub fn combinable(&self) -> bool {
        self.combinable
    }
}

#[derive(Debug, Clone)]
pub enum TargetedDiscountCommand {
    Set {
        scope: TargetScope,
        value: DiscountValue,
        combinable: bool,
    },
    Remove,
}

#[derive(Debug, thiserror::Error)]
pub enum TargetedDiscountError {
    #[error("no discount set for this target")]
    NotSet,
}

impl Aggregate for TargetedDiscount {
    type Event = TargetedDiscountEvent;
    type Command = TargetedDiscountCommand;
    type Error = TargetedDiscountError;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            TargetedDiscountEvent::Set {
                scope,
                value,
                combinable,
            } => {
                self.scope = Some(scope.clone());
                self.value = Some(*value);
                self.combinable = *combinable;
                self.active = true;
            }
            TargetedDiscountEvent::Removed => self.active = false,
        }
        self.version += 1;
    }

    fn handle(&self, command: Self::Command) -> Result<Vec<Self::Event>, Self::Error> {
        match command {
            // Set is create-or-replace by design: "the discount on this
            // item is now X" is idempotent intent.
            TargetedDiscountCommand::Set {
                scope,
                value,
                combinable,
            } => Ok(vec![TargetedDiscountEvent::Set {
                scope,
                value,
                combinable,
            }]),
            TargetedDiscountCommand::Remove => {
                if !self.active {
                    return Err(TargetedDiscountError::NotSet);
                }
                Ok(vec![TargetedDiscountEvent::Removed])
            }
        }
    }
}

// =======================================================================
// Applied discounts - what actually lands on the invoice. Concrete
// amounts + provenance; never re-evaluated after creation.
// =======================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DiscountSource {
    /// An automatic rule matched. rule_id + name snapshot so the
    /// document stays self-explanatory even if the rule changes later.
    Automatic { rule_id: String, rule_name: String },
    /// Item-level catalog discount.
    Item { item_id: String },
    /// Client-specific price agreement on this item.
    ClientItem { item_id: String },
    /// Ad-hoc, granted at invoice creation. `reason` is mandatory -
    /// an unexplained manual discount is an audit hole.
    Custom { reason: String },
    // LoyaltyPoints { .. } - EXTENSION POINT, deliberately absent until
    // specified: it interacts with a per-client points-account aggregate
    // (balance, accrual, redemption) and with payment recording, and
    // guessing its shape now would bake in wrong assumptions. Adding a
    // variant later is additive - existing stored events stay valid.
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedDiscount {
    pub source: DiscountSource,
    pub value: DiscountValue,
    /// The concrete amount this discount removed, minor units, computed
    /// at application time against its base. Totals just sum these -
    /// no percentage math at read time, no drift.
    pub amount_minor: i64,
}

// =======================================================================
// Evaluation service - assembles facts, gathers candidates, resolves
// stacking, produces concrete AppliedDiscounts per line + header.
// =======================================================================

/// Facts provider - backed by read models (client stats projector).
/// Trait so tests can stub it and so the facts source can move (e.g. to
/// a dedicated client-stats service) without touching evaluation logic.
#[async_trait::async_trait]
pub trait ClientFactsProvider: Send + Sync {
    async fn facts_for(
        &self,
        client_id: &str,
        now_local: chrono::NaiveDateTime,
    ) -> anyhow::Result<EvaluationFacts>;
}

pub struct LineDiscountInput<'a> {
    pub item_id: &'a str,
    /// quantity * unit_price, minor units - the base targeted discounts
    /// apply against.
    pub line_gross_minor: i64,
}

pub struct EvaluatedDiscounts {
    /// Per input line, same order as the input.
    pub per_line: Vec<Vec<AppliedDiscount>>,
    /// Header-level (automatic rules + custom), applied pro-rata across
    /// lines for tax-base purposes by the invoice's totals computation.
    pub header: Vec<AppliedDiscount>,
}

/// Stacking policy (default, deliberately simple and stated):
/// - all COMBINABLE eligible discounts stack (sum);
/// - each NON-combinable eligible discount is an exclusive alternative;
/// - the client gets whichever is worth more: the combinable stack, or
///   the single best non-combinable.
/// Custom discounts granted by a human always apply on top - a person
/// explicitly granting a discount outranks rule plumbing.
pub async fn evaluate_discounts(
    store: &dyn EventStore,
    facts_provider: &dyn ClientFactsProvider,
    client_id: &str,
    now_local: chrono::NaiveDateTime,
    lines: &[LineDiscountInput<'_>],
    active_rule_ids: &[String], // from a small read model listing active rule ids
    custom: Option<(DiscountValue, String)>, // (value, mandatory reason)
) -> anyhow::Result<EvaluatedDiscounts> {
    let facts = facts_provider.facts_for(client_id, now_local).await?;

    // --- per-line: targeted discounts, client-specific beats catalog ---
    let mut per_line = Vec::with_capacity(lines.len());
    for line in lines {
        let mut applied = Vec::new();

        let client_scope = TargetScope::ClientItem {
            item_id: line.item_id.to_string(),
            client_id: client_id.to_string(),
        };
        let item_scope = TargetScope::Item {
            item_id: line.item_id.to_string(),
        };

        let client_specific =
            load_aggregate::<TargetedDiscount>(store, &client_scope.aggregate_id()).await?;
        let catalog = load_aggregate::<TargetedDiscount>(store, &item_scope.aggregate_id()).await?;

        // Client-specific agreement supersedes the catalog discount for
        // that client - they negotiated a price, they don't ALSO get the
        // shelf discount on top unless both are combinable.
        match (client_specific.active_value(), catalog.active_value()) {
            (Some(v), Some(cv)) if client_specific.combinable() && catalog.combinable() => {
                applied.push(AppliedDiscount {
                    source: DiscountSource::ClientItem {
                        item_id: line.item_id.to_string(),
                    },
                    value: v,
                    amount_minor: v.amount_against(line.line_gross_minor),
                });
                let remaining =
                    line.line_gross_minor - applied.iter().map(|a| a.amount_minor).sum::<i64>();
                applied.push(AppliedDiscount {
                    source: DiscountSource::Item {
                        item_id: line.item_id.to_string(),
                    },
                    value: cv,
                    amount_minor: cv.amount_against(remaining),
                });
            }
            (Some(v), _) => {
                applied.push(AppliedDiscount {
                    source: DiscountSource::ClientItem {
                        item_id: line.item_id.to_string(),
                    },
                    value: v,
                    amount_minor: v.amount_against(line.line_gross_minor),
                });
            }
            (None, Some(cv)) => {
                applied.push(AppliedDiscount {
                    source: DiscountSource::Item {
                        item_id: line.item_id.to_string(),
                    },
                    value: cv,
                    amount_minor: cv.amount_against(line.line_gross_minor),
                });
            }
            (None, None) => {}
        }
        per_line.push(applied);
    }

    // Base for header-level discounts: gross minus per-line discounts.
    let gross_after_line: i64 = lines.iter().map(|l| l.line_gross_minor).sum::<i64>()
        - per_line
            .iter()
            .flatten()
            .map(|a| a.amount_minor)
            .sum::<i64>();

    // --- automatic rules: evaluate conditions, resolve stacking ---
    let mut combinable_stack: Vec<AppliedDiscount> = Vec::new();
    let mut best_exclusive: Option<AppliedDiscount> = None;

    for rule_id in active_rule_ids {
        let rule = load_aggregate::<DiscountRule>(store, rule_id).await?;
        if !rule.is_active() {
            continue;
        }
        let (Some(value), Some(condition)) = (rule.value(), rule.condition()) else {
            continue;
        };
        if !condition.evaluate(&facts) {
            continue;
        }
        let candidate = AppliedDiscount {
            source: DiscountSource::Automatic {
                rule_id: rule_id.clone(),
                rule_name: rule.name().to_string(),
            },
            value,
            amount_minor: value.amount_against(gross_after_line),
        };
        if rule.combinable() {
            combinable_stack.push(candidate);
        } else if best_exclusive
            .as_ref()
            .map_or(true, |b| candidate.amount_minor > b.amount_minor)
        {
            best_exclusive = Some(candidate);
        }
    }

    let stack_total: i64 = combinable_stack.iter().map(|a| a.amount_minor).sum();
    let mut header: Vec<AppliedDiscount> = match &best_exclusive {
        Some(exclusive) if exclusive.amount_minor > stack_total => vec![best_exclusive.unwrap()],
        _ => combinable_stack,
    };

    // --- custom: human-granted, applies on top, reason mandatory ---
    if let Some((value, reason)) = custom {
        let base = gross_after_line - header.iter().map(|a| a.amount_minor).sum::<i64>();
        header.push(AppliedDiscount {
            source: DiscountSource::Custom { reason },
            value,
            amount_minor: value.amount_against(base),
        });
    }

    Ok(EvaluatedDiscounts { per_line, header })
}
