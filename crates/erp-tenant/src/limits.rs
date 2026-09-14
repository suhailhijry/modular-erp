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
//! comment says a fact-based override belongs. The first half is one rule now,
//! naming the `accountant` role — on the entries the ledger posts by hand and
//! their reversals, the only checks that supply an amount; the second needs a
//! member's own branch, which nothing records yet. The size of an invoice,
//! credit note or refund is not judged here: `sales`' document limit does that
//! inside the command, where the total exists.
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
//!
//! # The one capability a limit cannot touch
//!
//! "Fixed by an owner" needs the owner to still be able to fix it. Writing
//! limits takes [`Capability::ManageTenant`], so a limit that could refuse it —
//! `{ when: always, then: refuse }` is enough — would refuse the write that
//! removes it, and nothing in the product could undo that. So [`narrows`] says
//! `ManageTenant` is never narrowed, `TenantDb::permits` answers it before it
//! reads a limit at all, and [`registry`] does not list `manage_tenant` as a
//! value a rule may name. Only an owner holds it, and there is always an owner.
//!
//! The price is that an owner cannot limit their own administration of the
//! tenant — members, keys, modules, settings — by branch or by amount. Nobody
//! has asked for that.
//!
//! # Checked when written, and when read
//!
//! A [`Limits`] cannot exist unchecked: [`Limits::new`] refuses a rule that
//! can never be true, and so does deserialising one. So a row this build cannot
//! use is refused where it is read — a 503, not the unlimited answer — rather
//! than quietly never firing. Which makes [`registry`] **expand-only**: remove a
//! fact or a value from it and every tenant that named it is refused until an
//! owner saves their limits again.

use erp_rules::{FactRegistry, Facts, Kind, Rules};
use serde::{Deserialize, Serialize};

use erp_types::ModuleId;

use crate::roles::{Access, Capability, Role};

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
/// **Deliberately four.** Every one is something a request already knows, and
/// a fact the caller cannot supply is a rule that silently never fires. More
/// arrive when a consumer supplies them.
pub const AMOUNT: &str = "amount";
pub const BRANCH: &str = "branch";
pub const CAPABILITY: &str = "capability";
/// The role that applies where the check is made — a module's own, when the
/// tenant gave this person a different one there. Supplied by
/// `TenantDb::permits`, which is where the role is known.
pub const ROLE: &str = "role";

/// Whether a limit may narrow this capability. See the module docs: everything
/// but the one that writes limits.
#[must_use]
pub const fn narrows(capability: Capability) -> bool {
    !matches!(capability, Capability::ManageTenant)
}

/// Which facts a permission rule may name, and of what type.
///
/// **Expand-only** — see the module docs.
#[must_use]
pub fn registry() -> FactRegistry {
    FactRegistry::new()
        .fact(AMOUNT, Kind::Money)
        .fact(BRANCH, Kind::Text)
        .one_of(
            CAPABILITY,
            Capability::ALL
                .into_iter()
                .filter(|c| narrows(*c))
                .map(Capability::as_str),
        )
        .one_of(ROLE, Role::ALL.map(Role::as_str))
}

/// **A tenant's own restrictions on what their roles may do.**
///
/// Stored as configuration, like a tariff. An empty set is the shipped default
/// and means roles decide alone, which is what every tenant starts with.
///
/// Serialised as the bare list of rules, and **checked on the way in**: see
/// [`Limits::new`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Rules<Verdict>", into = "Rules<Verdict>")]
pub struct Limits {
    rules: Rules<Verdict>,
}

/// A rule that can never be true, named, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("rule {rule:?}: {why}")]
pub struct Unusable {
    /// What the tenant called it — the name on their screen, not an index.
    pub rule: String,
    pub why: erp_rules::Invalid,
}

impl Limits {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "tenant.permission_limits";

    /// **The only way to have limits**, and it checks every rule against the
    /// facts a request can supply.
    ///
    /// # Errors
    /// The first rule that can never be true, and why.
    pub fn new(rules: Rules<Verdict>) -> Result<Self, Unusable> {
        rules.validate(&registry()).map_err(|(at, why)| Unusable {
            rule: rules
                .as_slice()
                .get(at)
                .map(|rule| rule.name.clone())
                .unwrap_or_default(),
            why,
        })?;
        Ok(Self { rules })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
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
        match self.explain(facts).matched.map(|rule| rule.then) {
            Some(Verdict::Refuse) => false,
            // No rule matched, or one matched and permits: the role's answer
            // stands, and the role's answer here is already `true`.
            Some(Verdict::Allow) | None => true,
        }
    }

    /// **The whole decision, for one person**: the role that applies in the
    /// module, and then these limits, with that role added as the `role` fact.
    ///
    /// What `TenantDb::permits` answers for whoever is asking once it has read
    /// the limits, and what anybody asking *who else may do this* asks per
    /// member — the worker telling whoever may write stock off — so the two
    /// cannot disagree about who may.
    #[must_use]
    pub fn permit(
        &self,
        access: &Access,
        capability: Capability,
        module: Option<&ModuleId>,
        facts: &Facts,
    ) -> bool {
        let allowed = access.allows(capability, module);
        if !allowed || !narrows(capability) {
            return allowed;
        }
        let facts = facts.clone().with(
            ROLE,
            erp_rules::Value::Text(access.role_in(module).as_str().to_owned()),
        );
        self.narrow(allowed, &facts)
    }

    /// **Why**, in the same shape [`erp_rules::Rules::explain`] gives — for the
    /// question a refused person asks. [`Self::narrow`] reads its answer here,
    /// so the two cannot disagree.
    ///
    /// **A refusal it cannot judge refuses.** Whether 5,000,000 USD is over
    /// 10,000 SAR has no answer, and treating it as *no* let a bookkeeper post
    /// any amount past the limit by opening accounts in another currency. An
    /// exception it cannot judge restores nothing, so the rules below it decide.
    /// Both are the narrower answer (L6). A tenant that posts in two currencies
    /// writes an `allow` per currency above the refusal.
    #[must_use]
    pub fn explain(&self, facts: &Facts) -> erp_rules::Explained<'_, Verdict> {
        self.rules
            .explain_undecided(facts, |rule| rule.then == Verdict::Refuse)
    }
}

impl TryFrom<Rules<Verdict>> for Limits {
    type Error = Unusable;

    fn try_from(rules: Rules<Verdict>) -> Result<Self, Unusable> {
        Self::new(rules)
    }
}

impl From<Limits> for Rules<Verdict> {
    fn from(limits: Limits) -> Self {
        limits.rules
    }
}

/// The facts a capability check can supply about itself.
///
/// A caller adds what it knows — an amount, a branch — with
/// [`erp_rules::Facts::with`]. The role is added by `TenantDb::permits`.
#[must_use]
pub fn facts_for(capability: Capability) -> Facts {
    Facts::new().with(
        CAPABILITY,
        erp_rules::Value::Text(capability.as_str().to_owned()),
    )
}

/// **What the edge knows**: the capability, and the branch the request is for
/// when it names one.
///
/// The facts `Allowed` checks a route against before its handler has read a
/// body — one function, so that asking the same question from somewhere with
/// no request builds the same facts.
#[must_use]
pub fn facts_at(capability: Capability, branch: Option<&str>) -> Facts {
    let facts = facts_for(capability);
    match branch {
        Some(branch) => facts.with(BRANCH, erp_rules::Value::Text(branch.to_owned())),
        None => facts,
    }
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

    fn is(fact: &str, value: Value) -> DynCondition {
        DynCondition::Is {
            fact: fact.to_owned(),
            op: Op::Eq,
            value,
        }
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    /// What `TenantDb::permits` supplies for somebody who is an accountant here.
    fn as_accountant(capability: Capability) -> Facts {
        facts_for(capability).with(ROLE, text("accountant"))
    }

    /// "A bookkeeper may post entries under ten thousand riyals" — the example
    /// `roles.rs` names. The bookkeeper is the `accountant` role, and without
    /// the role fact this rule would refuse the owner too.
    fn under_ten_thousand() -> Limits {
        Limits::new(Rules::new(vec![over_ten_thousand()])).expect("a real rule validates")
    }

    fn over_ten_thousand() -> Rule<Verdict> {
        Rule {
            name: "Entries over ten thousand".to_owned(),
            when: DynCondition::All {
                of: vec![
                    is(ROLE, text("accountant")),
                    is(CAPABILITY, text("post_entries")),
                    DynCondition::Is {
                        fact: AMOUNT.to_owned(),
                        op: Op::Gte,
                        value: Value::Money(riyals(10_000)),
                    },
                ],
            },
            then: Verdict::Refuse,
        }
    }

    fn dollars(n: i64) -> Value {
        Value::Money(Money::from_minor(n * 100, "USD".parse().unwrap()))
    }

    /// **An amount the limit cannot judge is refused, not waved through.** A
    /// bookkeeper holds `manage_accounts`, so one who opens accounts in dollars
    /// must not post five million of them past a limit in riyals. The owner is
    /// the contrast: the rule is still not about them, in any currency.
    #[test]
    fn an_amount_in_a_currency_the_limit_does_not_name_is_refused_not_waved_through() {
        let limits = under_ten_thousand();
        let five_million = as_accountant(Capability::PostEntries).with(AMOUNT, dollars(5_000_000));
        assert!(
            !limits.narrow(true, &five_million),
            "no answer is not under it"
        );

        let owner = facts_for(Capability::PostEntries)
            .with(ROLE, text("owner"))
            .with(AMOUNT, dollars(5_000_000));
        assert!(limits.narrow(true, &owner), "the rule is about accountants");
    }

    /// **An exception it cannot judge restores nothing.** How a tenant that
    /// posts in two currencies says what it means: an `allow` for dollars above
    /// the refusal in riyals. The riyal entry over the limit is the line that
    /// matters — counting the dollar exception in would let it through.
    #[test]
    fn an_exception_in_another_currency_leaves_the_refusal_below_it_to_decide() {
        let limits = Limits::new(Rules::new(vec![
            Rule {
                name: "Dollars under 2,700".to_owned(),
                when: DynCondition::Is {
                    fact: AMOUNT.to_owned(),
                    op: Op::Lt,
                    value: dollars(2_700),
                },
                then: Verdict::Allow,
            },
            over_ten_thousand(),
        ]))
        .expect("validates");
        let posting = |amount| as_accountant(Capability::PostEntries).with(AMOUNT, amount);

        assert!(limits.narrow(true, &posting(dollars(100))));
        assert!(!limits.narrow(true, &posting(dollars(5_000_000))));
        assert!(limits.narrow(true, &posting(Value::Money(riyals(5_000)))));
        assert!(
            !limits.narrow(true, &posting(Value::Money(riyals(20_000)))),
            "the riyal rule decides a riyal entry"
        );
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
        }]))
        .expect("validates");
        let facts = as_accountant(Capability::PostEntries);

        // The role said no. Nothing here may change that.
        assert!(!always_allow.narrow(false, &facts));
        assert!(!under_ten_thousand().narrow(false, &facts));
        assert!(!Limits::default().narrow(false, &facts));
    }

    #[test]
    fn an_amount_over_the_limit_is_refused_and_one_under_is_not() {
        let limits = under_ten_thousand();

        let big = as_accountant(Capability::PostEntries).with(AMOUNT, Value::Money(riyals(10_000)));
        assert!(!limits.narrow(true, &big), "ten thousand is not under it");

        let small =
            as_accountant(Capability::PostEntries).with(AMOUNT, Value::Money(riyals(9_999)));
        assert!(limits.narrow(true, &small));
    }

    /// **The rule is about the bookkeeper, not about everybody.** The owner
    /// posting the same entry is not refused — the half that needs the role.
    #[test]
    fn a_limit_on_one_role_leaves_the_others_alone() {
        let big = Value::Money(riyals(50_000));
        let owner = facts_for(Capability::PostEntries)
            .with(ROLE, text("owner"))
            .with(AMOUNT, big.clone());
        let accountant = as_accountant(Capability::PostEntries).with(AMOUNT, big);
        assert!(
            under_ten_thousand().narrow(true, &owner),
            "the rule is about accountants"
        );
        assert!(!under_ten_thousand().narrow(true, &accountant));
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
        let reading = as_accountant(Capability::Read).with(AMOUNT, big.clone());
        let posting = as_accountant(Capability::PostEntries).with(AMOUNT, big);
        assert!(limits.narrow(true, &reading), "the rule is about posting");
        assert!(!limits.narrow(true, &posting), "and posting is refused");
    }

    /// **A fact the caller could not supply does not fire a rule.** A check
    /// with no amount is not "an amount over ten thousand".
    #[test]
    fn a_check_that_supplies_no_amount_is_not_refused_by_an_amount_rule() {
        let limits = under_ten_thousand();
        assert!(limits.narrow(true, &as_accountant(Capability::PostEntries)));
    }

    /// First match wins, which is what lets a narrow exception sit above a
    /// broad refusal.
    #[test]
    fn an_exception_above_a_refusal_restores_what_the_role_allowed() {
        let limits = Limits::new(Rules::new(vec![
            Rule {
                name: "Olaya is exempt".to_owned(),
                when: is(BRANCH, text("olaya")),
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
        ]))
        .expect("validates");

        let at_olaya = facts_for(Capability::PostEntries)
            .with(AMOUNT, Value::Money(riyals(50_000)))
            .with(BRANCH, text("olaya"));
        assert!(limits.narrow(true, &at_olaya));

        let at_malaz = facts_for(Capability::PostEntries)
            .with(AMOUNT, Value::Money(riyals(50_000)))
            .with(BRANCH, text("malaz"));
        assert!(!limits.narrow(true, &at_malaz));
    }

    #[test]
    fn no_limits_at_all_means_roles_decide_alone() {
        let none = Limits::default();
        assert!(none.is_empty());
        for capability in Capability::ALL {
            assert!(none.narrow(true, &facts_for(capability)));
            assert!(!none.narrow(false, &facts_for(capability)));
        }
    }

    fn one_rule(when: DynCondition) -> Result<Limits, Unusable> {
        Limits::new(Rules::new(vec![Rule {
            name: "Nonsense".to_owned(),
            when,
            then: Verdict::Refuse,
        }]))
    }

    #[test]
    fn a_rule_naming_a_fact_no_request_supplies_is_refused_when_written() {
        let refused = one_rule(is("phase_of_the_moon", text("waxing")));
        assert!(
            matches!(
                &refused,
                Err(Unusable { rule, why: erp_rules::Invalid::NoSuchFact { .. } }) if rule == "Nonsense"
            ),
            "refused by name: {refused:?}"
        );
    }

    /// **A misspelt capability, or the one a limit cannot touch, is refused
    /// when written** — not stored to never fire. `manage_tenant` is refused
    /// because `TenantDb::permits` never narrows it, so a rule about it would
    /// be exactly that. The last line is the contrast.
    #[test]
    fn a_rule_naming_a_capability_that_does_not_exist_or_cannot_be_limited_is_refused_when_written()
    {
        for (value, why) in [
            ("post_entires", "a typo"),
            ("manage_tenant", "never narrowed"),
        ] {
            let refused = one_rule(is(CAPABILITY, text(value)));
            assert!(
                matches!(
                    refused,
                    Err(Unusable {
                        why: erp_rules::Invalid::NoSuchValue { .. },
                        ..
                    })
                ),
                "{why}: {refused:?}"
            );
        }
        assert!(matches!(
            one_rule(is(ROLE, text("bookkeeper"))),
            Err(Unusable {
                why: erp_rules::Invalid::NoSuchValue { .. },
                ..
            })
        ));
        one_rule(is(CAPABILITY, text("post_entries"))).expect("a capability a limit narrows");
    }

    /// **Read is checked too.** A stored row this build cannot use must not
    /// come back as limits that silently never fire — `TenantDb::permits`
    /// turns the decode error into a 503. The JSON here is input to a pure
    /// decode, not seeded state.
    #[test]
    fn stored_limits_that_no_longer_validate_are_refused_when_read() {
        let stored = serde_json::json!([{
            "name": "x",
            "when": {
                "when": "is", "fact": "phase_of_the_moon", "op": "eq",
                "value": { "type": "text", "of": "waxing" }
            },
            "then": "refuse"
        }]);
        assert!(serde_json::from_value::<Limits>(stored).is_err());
    }

    /// A refused person asks why, and the answer is the evaluator's own.
    #[test]
    fn explain_names_the_rule_that_refused() {
        let limits = under_ten_thousand();
        let facts =
            as_accountant(Capability::PostEntries).with(AMOUNT, Value::Money(riyals(20_000)));
        let why = limits.explain(&facts);
        assert_eq!(
            why.matched.map(|r| r.name.as_str()),
            Some("Entries over ten thousand")
        );
    }

    #[test]
    fn limits_round_trip_through_json() {
        let limits = under_ten_thousand();
        let written = serde_json::to_value(&limits).unwrap();
        assert!(written.is_array(), "stored as the bare list: {written}");
        let back: Limits = serde_json::from_value(written).unwrap();
        assert_eq!(back, limits);
    }
}
