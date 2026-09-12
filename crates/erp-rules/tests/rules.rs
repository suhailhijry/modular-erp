//! The engine, against the two things it must never get wrong: a rule that
//! cannot be true must be refused when it is written, and the rule that wins
//! must be the one `explain` says won.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use erp_rules::{DynCondition, FactRegistry, Facts, Invalid, Kind, Op, Rule, Rules, Value};
use erp_types::{CurrencyCode, Money};

fn sar() -> CurrencyCode {
    "SAR".parse().expect("a currency")
}

fn riyals(n: i64) -> Money {
    Money::from_minor(n * 100, sar())
}

fn registry() -> FactRegistry {
    FactRegistry::new()
        .fact("amount", Kind::Money)
        .fact("branch", Kind::Text)
        .fact("count", Kind::Int)
        .fact("refundable", Kind::Bool)
}

fn is(fact: &str, op: Op, value: Value) -> DynCondition {
    DynCondition::Is {
        fact: fact.to_owned(),
        op,
        value,
    }
}

// --- validation: a rule that cannot be true is refused when it is written ---

#[test]
fn a_condition_naming_a_fact_nobody_supplies_is_refused() {
    let refused = is("porpoise", Op::Eq, Value::Int(1)).validate(&registry());
    assert!(
        matches!(refused, Err(Invalid::NoSuchFact { ref name, .. }) if name == "porpoise"),
        "got {refused:?}"
    );
}

#[test]
fn the_refusal_names_the_facts_that_do_exist() {
    let Err(Invalid::NoSuchFact { known, .. }) =
        is("porpoise", Op::Eq, Value::Int(1)).validate(&registry())
    else {
        panic!("expected a refusal naming the alternatives");
    };
    // A message that says "no such fact" and stops is a support ticket.
    assert!(known.contains(&"amount".to_owned()));
    assert!(known.contains(&"branch".to_owned()));
}

#[test]
fn comparing_an_amount_to_a_word_is_refused() {
    let refused = is("amount", Op::Lt, Value::Text("lots".to_owned())).validate(&registry());
    assert!(
        matches!(
            refused,
            Err(Invalid::WrongKind {
                declared: Kind::Money,
                given: Kind::Text,
                ..
            })
        ),
        "got {refused:?}"
    );
}

/// **`Bool` orders nothing.** "Is refundable greater than true" has no answer,
/// and a rule asking it would simply never fire.
#[test]
fn ordering_a_yes_or_no_is_refused() {
    for op in [Op::Lt, Op::Lte, Op::Gt, Op::Gte] {
        let refused = is("refundable", op, Value::Bool(true)).validate(&registry());
        assert!(
            matches!(refused, Err(Invalid::NotOrderable { .. })),
            "{op:?} should not order a bool, got {refused:?}"
        );
    }
    for op in [Op::Eq, Op::Ne] {
        is("refundable", op, Value::Bool(true))
            .validate(&registry())
            .expect("equality is fine");
    }
}

/// **A value the fact never takes is refused**, and so is ordering one. Without
/// the list, `capability == "raed"` validates and is never once true.
#[test]
fn a_text_fact_with_known_values_refuses_one_it_does_not_know() {
    let listed = FactRegistry::new().one_of("capability", ["read"]);

    let refused = is("capability", Op::Eq, Value::Text("raed".to_owned())).validate(&listed);
    assert_eq!(
        refused,
        Err(Invalid::NoSuchValue {
            name: "capability".to_owned(),
            value: "raed".to_owned(),
            known: vec!["read".to_owned()],
        })
    );
    is("capability", Op::Eq, Value::Text("read".to_owned()))
        .validate(&listed)
        .expect("a value it takes is fine");
    for op in [Op::Lt, Op::Lte, Op::Gt, Op::Gte] {
        let refused = is("capability", op, Value::Text("read".to_owned())).validate(&listed);
        assert!(
            matches!(refused, Err(Invalid::NotOrderable { .. })),
            "{op:?} should not order a listed fact, got {refused:?}"
        );
    }
    // A text fact with no list still takes anything, and orders.
    is("branch", Op::Gt, Value::Text("anything".to_owned()))
        .validate(&registry())
        .expect("an unlisted text fact is open");
}

#[test]
fn validation_reaches_inside_the_combinators() {
    let nested = DynCondition::All {
        of: vec![
            DynCondition::Always,
            DynCondition::Not {
                of: Box::new(is("porpoise", Op::Eq, Value::Int(1))),
            },
        ],
    };
    assert!(matches!(
        nested.validate(&registry()),
        Err(Invalid::NoSuchFact { .. })
    ));
}

// --- evaluation ---

#[test]
fn an_amount_is_compared_in_its_own_currency_and_never_across_two() {
    let facts = Facts::new().with("amount", Value::Money(riyals(100)));
    assert!(is("amount", Op::Lt, Value::Money(riyals(200))).holds(&facts));
    assert!(!is("amount", Op::Lt, Value::Money(riyals(50))).holds(&facts));

    // **Not a false because 100 < 200.** Comparing across currencies invents a
    // rate, so it is no answer, and no answer does not fire a rule.
    let dollars = Money::from_minor(20_000, "USD".parse().expect("a currency"));
    assert!(!is("amount", Op::Lt, Value::Money(dollars)).holds(&facts));
}

/// **No answer stays no answer on the way up.** Were it a false, `Not` would
/// turn it into a true and fire. An unknown part decides the whole only when
/// the other parts do not.
#[test]
fn an_amount_in_another_currency_is_no_answer_and_stays_one() {
    let facts = Facts::new().with("amount", Value::Money(riyals(100)));
    let across = is(
        "amount",
        Op::Lt,
        Value::Money(Money::from_minor(
            20_000,
            "USD".parse().expect("a currency"),
        )),
    );
    let yes = is("amount", Op::Gt, Value::Money(riyals(1)));
    let no = is("amount", Op::Gt, Value::Money(riyals(1_000)));
    let not = |c: &DynCondition| DynCondition::Not {
        of: Box::new(c.clone()),
    };
    let all = |a: &DynCondition, b: &DynCondition| DynCondition::All {
        of: vec![a.clone(), b.clone()],
    };
    let any = |a: &DynCondition, b: &DynCondition| DynCondition::Any {
        of: vec![a.clone(), b.clone()],
    };

    assert_eq!(across.decide(&facts), None);
    assert_eq!(not(&across).decide(&facts), None, "not of no answer");
    assert!(!not(&across).holds(&facts), "does not fire");
    assert_eq!(all(&no, &across).decide(&facts), Some(false));
    assert_eq!(all(&yes, &across).decide(&facts), None);
    assert_eq!(any(&yes, &across).decide(&facts), Some(true));
    assert_eq!(any(&no, &across).decide(&facts), None);

    // Whoever acts on the rule says what no answer means.
    let rules = Rules::new(vec![Rule {
        name: "Across".to_owned(),
        when: across,
        then: 1,
    }]);
    assert!(
        rules.evaluate(&facts).is_none(),
        "by default, it does not apply"
    );
    assert_eq!(
        rules
            .explain_undecided(&facts, |_| true)
            .matched
            .map(|r| r.then),
        Some(1)
    );
}

/// **A fact nobody supplied is not true.**
#[test]
fn a_missing_fact_does_not_satisfy_anything() {
    let nothing = Facts::new();
    for op in [Op::Eq, Op::Ne, Op::Lt, Op::Lte, Op::Gt, Op::Gte] {
        assert!(
            !is("count", op, Value::Int(0)).holds(&nothing),
            "{op:?} fired on a missing fact"
        );
    }
}

/// **The identities, which are the classic silent inversion.** Swapping these
/// flips every rule authored through a form that submitted no conditions.
#[test]
fn an_empty_all_is_true_and_an_empty_any_is_false() {
    let nothing = Facts::new();
    assert!(
        DynCondition::All { of: vec![] }.holds(&nothing),
        "All([]) is and's identity"
    );
    assert!(
        !DynCondition::Any { of: vec![] }.holds(&nothing),
        "Any([]) is or's identity"
    );
}

#[test]
fn the_combinators_compose() {
    let facts = Facts::new()
        .with("amount", Value::Money(riyals(100)))
        .with("branch", Value::Text("olaya".to_owned()));

    assert!(
        DynCondition::All {
            of: vec![
                is("amount", Op::Lte, Value::Money(riyals(100))),
                is("branch", Op::Eq, Value::Text("olaya".to_owned())),
            ],
        }
        .holds(&facts)
    );

    let either = DynCondition::Any {
        of: vec![
            is("branch", Op::Eq, Value::Text("malaz".to_owned())),
            is("amount", Op::Gt, Value::Money(riyals(1))),
        ],
    };
    assert!(either.holds(&facts));
    assert!(
        !DynCondition::Not {
            of: Box::new(either)
        }
        .holds(&facts)
    );
}

// --- rules: first match, and explaining it ---

fn tiers() -> Rules<i32> {
    Rules::new(vec![
        Rule {
            name: "Large".to_owned(),
            when: is("amount", Op::Gte, Value::Money(riyals(1_000))),
            then: 2_500,
        },
        Rule {
            name: "Medium".to_owned(),
            when: is("amount", Op::Gte, Value::Money(riyals(100))),
            then: 1_000,
        },
        Rule {
            name: "Base".to_owned(),
            when: DynCondition::Always,
            then: 0,
        },
    ])
}

#[test]
fn the_first_match_wins_and_a_later_one_does_not() {
    let facts = Facts::new().with("amount", Value::Money(riyals(5_000)));
    let rules = tiers();
    let won = rules.evaluate(&facts).expect("something matches");
    assert_eq!(won.name, "Large");
    assert_eq!(won.then, 2_500);
}

#[test]
fn a_catch_all_last_is_what_makes_a_base_rate() {
    let facts = Facts::new().with("amount", Value::Money(riyals(1)));
    let rules = tiers();
    assert_eq!(rules.evaluate(&facts).expect("matches").name, "Base");
}

#[test]
fn nothing_matches_when_no_rule_does() {
    let narrow = Rules::new(vec![Rule {
        name: "Large".to_owned(),
        when: is("amount", Op::Gte, Value::Money(riyals(1_000))),
        then: 1,
    }]);
    let facts = Facts::new().with("amount", Value::Money(riyals(1)));
    assert!(narrow.evaluate(&facts).is_none());
}

/// **A rules engine whose refusals cannot be interrogated makes the support
/// tickets it was built to remove.**
#[test]
fn explain_names_every_rule_tried_in_order_and_stops_at_the_winner() {
    let facts = Facts::new().with("amount", Value::Money(riyals(500)));
    let rules = tiers();
    let explained = rules.explain(&facts);

    assert_eq!(explained.matched.expect("matches").name, "Medium");
    let names: Vec<_> = explained.considered.iter().map(|c| c.name).collect();
    assert_eq!(names, ["Large", "Medium"], "Base was never reached");
    assert_eq!(explained.considered.iter().filter(|c| c.matched).count(), 1);
    assert!(explained.considered.last().expect("one").matched);
}

#[test]
fn explain_lists_everything_when_nothing_matches() {
    let narrow = Rules::new(vec![Rule {
        name: "Large".to_owned(),
        when: is("amount", Op::Gte, Value::Money(riyals(1_000))),
        then: 1,
    }]);
    let nothing = Facts::new();
    let explained = narrow.explain(&nothing);
    assert!(explained.matched.is_none());
    assert_eq!(explained.considered.len(), 1);
    assert!(!explained.considered[0].matched);
}

/// They are one function. Two implementations of "which rule wins" would
/// disagree eventually, and the disagreement would be a price nobody could
/// account for.
#[test]
fn evaluate_and_explain_never_disagree() {
    for amount in [1_i64, 99, 100, 999, 1_000, 100_000] {
        let facts = Facts::new().with("amount", Value::Money(riyals(amount)));
        let rules = tiers();
        assert_eq!(
            rules.evaluate(&facts).map(|r| r.name.clone()),
            rules.explain(&facts).matched.map(|r| r.name.clone()),
            "at {amount}"
        );
    }
}

#[test]
fn validating_a_set_names_which_rule_is_wrong() {
    let rules = Rules::new(vec![
        Rule {
            name: "fine".to_owned(),
            when: DynCondition::Always,
            then: 0,
        },
        Rule {
            name: "broken".to_owned(),
            when: is("porpoise", Op::Eq, Value::Int(1)),
            then: 1,
        },
    ]);
    let Err((at, Invalid::NoSuchFact { .. })) = rules.validate(&registry()) else {
        panic!("expected the second rule to be refused");
    };
    assert_eq!(at, 1, "the index is what a screen highlights");
}

#[test]
fn a_condition_round_trips_through_json() {
    let condition = DynCondition::All {
        of: vec![
            is("amount", Op::Lt, Value::Money(riyals(100))),
            DynCondition::Not {
                of: Box::new(is("branch", Op::Eq, Value::Text("olaya".to_owned()))),
            },
        ],
    };
    let written = serde_json::to_string(&condition).expect("serializes");
    let back: DynCondition = serde_json::from_str(&written).expect("decodes");
    assert_eq!(back, condition);
}
