//! **Permissions narrowed by facts.**
//!
//! # What this is for
//!
//! `Role::allows` answers *"may this role do this at all"* from four coarse
//! roles. `roles.rs` names what a tenant actually wants and it cannot say:
//!
//! > a bookkeeper can be allowed to post entries **under ten thousand riyals**,
//! > or only to **their own branch**
//!
//! Those are the same question with facts attached, and this is where the facts
//! attach — beside the capability check, exactly where that module's own
//! comment says a fact-based override belongs.
//!
//! # The one property everything here rests on
//!
//! **A limit narrows and never widens.** It can turn a role's *yes* into a
//! *no*; it can never turn a *no* into a *yes*. So the worst a misconfigured
//! limit can do is refuse work — annoying, visible, and fixed by an owner —
//! rather than grant somebody an authority nobody gave them.
//!
//! That is not a convention this file hopes callers honour. [`Limits::narrow`]
//! takes the role's answer as its input and can only ever return `false` when
//! it was already `true`, which is a shape the compiler enforces and
//! `a_limit_never_widens_what_a_role_allows` pins.

use erp_rules::{FactRegistry, Facts, Kind, Rules};
use serde::{Deserialize, Serialize};

use crate::roles::Capability;

/// What a matching limit decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Refuse, even though the role permits it.
    Refuse,
    /// Permit — which only ever *restores* what the role already allowed, and
    /// exists so a narrow exception can sit above a broad refusal.
    Allow,
}

/// The facts a permission rule may ask about.
///
/// **Deliberately three.** Every one is something a request already knows, and
/// a fact the caller cannot supply is a rule that silently never fires. More
/// arrive when a consumer supplies them.
pub const AMOUNT: &str = "amount";
pub const BRANCH: &str = "branch";
pub const CAPABILITY: &str = "capability";

/// Which facts a permission rule may name, and of what type.
#[must_use]
pub fn registry() -> FactRegistry {
    FactRegistry::new()
        .fact(AMOUNT, Kind::Money)
        .fact(BRANCH, Kind::Text)
        .fact(CAPABILITY, Kind::Text)
}

/// **A tenant's own restrictions on what their roles may do.**
///
/// Stored as configuration, like a tariff. An empty set is the shipped default
/// and means roles decide alone, which is what every tenant starts with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Limits {
    rules: Rules<Verdict>,
}

impl Limits {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "tenant.permission_limits";

    #[must_use]
    pub const fn new(rules: Rules<Verdict>) -> Self {
        Self { rules }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Checks every rule against the facts a request can supply.
    ///
    /// # Errors
    /// The index of the first rule that can never be true, and why.
    pub fn validate(&self) -> Result<(), (usize, erp_rules::Invalid)> {
        self.rules.validate(&registry())
    }

    /// **Narrows a role's answer. Never widens it.**
    ///
    /// `allowed` is what [`crate::Access::allows`] already decided. A limit is
    /// only consulted when that was `true`, so no arrangement of rules can
    /// grant a capability the role withholds.
    #[must_use]
    pub fn narrow(&self, allowed: bool, facts: &Facts) -> bool {
        if !allowed || self.rules.is_empty() {
            return allowed;
        }
        match self.rules.evaluate(facts).map(|rule| rule.then) {
            Some(Verdict::Refuse) => false,
            // No rule matched, or one matched and permits: the role's answer
            // stands, and the role's answer here is already `true`.
            Some(Verdict::Allow) | None => true,
        }
    }

    /// **Why**, in the same shape [`erp_rules::Rules::explain`] gives — for the
    /// question a refused person asks.
    #[must_use]
    pub fn explain(&self, facts: &Facts) -> erp_rules::Explained<'_, Verdict> {
        self.rules.explain(facts)
    }
}

/// The facts a capability check can supply about itself.
///
/// A caller adds what it knows — an amount, a branch — with
/// [`erp_rules::Facts::with`].
#[must_use]
pub fn facts_for(capability: Capability) -> Facts {
    Facts::new().with(
        CAPABILITY,
        erp_rules::Value::Text(capability.as_str().to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_rules::{DynCondition, Op, Rule, Value};
    use erp_types::{CurrencyCode, Money};

    fn sar() -> CurrencyCode {
        "SAR".parse().unwrap()
    }

    fn riyals(n: i64) -> Money {
        Money::from_minor(n * 100, sar())
    }

    /// "A bookkeeper may post entries under ten thousand riyals" — the example
    /// `roles.rs` names and could not express.
    fn under_ten_thousand() -> Limits {
        Limits::new(Rules::new(vec![Rule {
            name: "Entries over ten thousand".to_owned(),
            when: DynCondition::All {
                of: vec![
                    DynCondition::Is {
                        fact: CAPABILITY.to_owned(),
                        op: Op::Eq,
                        value: Value::Text("post_entries".to_owned()),
                    },
                    DynCondition::Is {
                        fact: AMOUNT.to_owned(),
                        op: Op::Gte,
                        value: Value::Money(riyals(10_000)),
                    },
                ],
            },
            then: Verdict::Refuse,
        }]))
    }

    /// **The property everything rests on.** A limit may turn a yes into a no
    /// and must never turn a no into a yes — so the worst a misconfiguration
    /// does is refuse work, not grant authority.
    #[test]
    fn a_limit_never_widens_what_a_role_allows() {
        let always_allow = Limits::new(Rules::new(vec![Rule {
            name: "Allow everything".to_owned(),
            when: DynCondition::Always,
            then: Verdict::Allow,
        }]));
        let facts = facts_for(Capability::ManageTenant);

        // The role said no. Nothing here may change that.
        assert!(!always_allow.narrow(false, &facts));
        assert!(!under_ten_thousand().narrow(false, &facts));
        assert!(!Limits::default().narrow(false, &facts));
    }

    #[test]
    fn an_amount_over_the_limit_is_refused_and_one_under_is_not() {
        let limits = under_ten_thousand();

        let big = facts_for(Capability::PostEntries).with(AMOUNT, Value::Money(riyals(10_000)));
        assert!(!limits.narrow(true, &big), "ten thousand is not under it");

        let small = facts_for(Capability::PostEntries).with(AMOUNT, Value::Money(riyals(9_999)));
        assert!(limits.narrow(true, &small));
    }

    /// **A limit about posting must not refuse reading.** The capability is a
    /// fact so one rule set can hold rules about different capabilities.
    #[test]
    fn a_limit_on_one_capability_leaves_the_others_alone() {
        let limits = under_ten_thousand();
        let big = Value::Money(riyals(50_000));

        // **The contrast is the test.** Asserting only that reading is
        // permitted passes even if the capability is not a fact at all, because
        // a rule that never matches also never refuses. The pair is what makes
        // the capability load-bearing — found by a falsification that passed.
        let reading = facts_for(Capability::Read).with(AMOUNT, big.clone());
        let posting = facts_for(Capability::PostEntries).with(AMOUNT, big);
        assert!(limits.narrow(true, &reading), "the rule is about posting");
        assert!(!limits.narrow(true, &posting), "and posting is refused");
    }

    /// **A fact the caller could not supply does not fire a rule.** A check
    /// with no amount is not "an amount over ten thousand".
    #[test]
    fn a_check_that_supplies_no_amount_is_not_refused_by_an_amount_rule() {
        let limits = under_ten_thousand();
        assert!(limits.narrow(true, &facts_for(Capability::PostEntries)));
    }

    /// First match wins, which is what lets a narrow exception sit above a
    /// broad refusal.
    #[test]
    fn an_exception_above_a_refusal_restores_what_the_role_allowed() {
        let limits = Limits::new(Rules::new(vec![
            Rule {
                name: "Olaya is exempt".to_owned(),
                when: DynCondition::Is {
                    fact: BRANCH.to_owned(),
                    op: Op::Eq,
                    value: Value::Text("olaya".to_owned()),
                },
                then: Verdict::Allow,
            },
            Rule {
                name: "Everything else over ten thousand".to_owned(),
                when: DynCondition::Is {
                    fact: AMOUNT.to_owned(),
                    op: Op::Gte,
                    value: Value::Money(riyals(10_000)),
                },
                then: Verdict::Refuse,
            },
        ]));

        let at_olaya = facts_for(Capability::PostEntries)
            .with(AMOUNT, Value::Money(riyals(50_000)))
            .with(BRANCH, Value::Text("olaya".to_owned()));
        assert!(limits.narrow(true, &at_olaya));

        let at_malaz = facts_for(Capability::PostEntries)
            .with(AMOUNT, Value::Money(riyals(50_000)))
            .with(BRANCH, Value::Text("malaz".to_owned()));
        assert!(!limits.narrow(true, &at_malaz));
    }

    #[test]
    fn no_limits_at_all_means_roles_decide_alone() {
        let none = Limits::default();
        assert!(none.is_empty());
        for capability in [
            Capability::Read,
            Capability::PostEntries,
            Capability::ManageAccounts,
            Capability::ManageTenant,
        ] {
            assert!(none.narrow(true, &facts_for(capability)));
            assert!(!none.narrow(false, &facts_for(capability)));
        }
    }

    #[test]
    fn a_rule_naming_a_fact_no_request_supplies_is_refused_when_written() {
        let nonsense = Limits::new(Rules::new(vec![Rule {
            name: "Nonsense".to_owned(),
            when: DynCondition::Is {
                fact: "phase_of_the_moon".to_owned(),
                op: Op::Eq,
                value: Value::Text("waxing".to_owned()),
            },
            then: Verdict::Refuse,
        }]));
        assert!(matches!(
            nonsense.validate(),
            Err((0, erp_rules::Invalid::NoSuchFact { .. }))
        ));
        under_ten_thousand()
            .validate()
            .expect("a real rule validates");
    }

    /// A refused person asks why, and the answer is the evaluator's own.
    #[test]
    fn explain_names_the_rule_that_refused() {
        let limits = under_ten_thousand();
        let facts = facts_for(Capability::PostEntries).with(AMOUNT, Value::Money(riyals(20_000)));
        let why = limits.explain(&facts);
        assert_eq!(
            why.matched.map(|r| r.name.as_str()),
            Some("Entries over ten thousand")
        );
    }

    #[test]
    fn limits_round_trip_through_json() {
        let limits = under_ten_thousand();
        let written = serde_json::to_string(&limits).unwrap();
        let back: Limits = serde_json::from_str(&written).unwrap();
        assert_eq!(back, limits);
    }
}
