//! The four authoring levels: what each stores, and what it refuses.

use erp_i18n::MessageCode;
use erp_rules::{Answers, Authored, Field, Kind, Template, Unfillable, Value};

const BROKEN: MessageCode = MessageCode::new("test.broken");

/// The artifact under test: a made-up rule shape, so nothing here depends on
/// what booking happens to ship.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Charge {
    name: String,
    percent: i64,
}

const NAME: Field = Field {
    key: "name",
    label_en: "Name",
    label_ar: "الاسم",
    kind: Kind::Text,
};

const PERCENT: Field = Field {
    key: "percent",
    label_en: "Percent",
    label_ar: "النسبة",
    kind: Kind::Int,
};

static TEMPLATES: &[Template<Charge>] = &[
    Template {
        id: "flat",
        name_en: "A flat ten percent",
        name_ar: "عشرة بالمئة ثابتة",
        fields: &[],
        build: flat,
    },
    Template {
        id: "custom",
        name_en: "A percentage you choose",
        name_ar: "نسبة تختارها",
        fields: &[NAME, PERCENT],
        build: custom,
    },
];

// The signature a `Template` needs, not the one this body wants.
#[expect(
    clippy::unnecessary_wraps,
    reason = "a preset that cannot fail is still a build fn"
)]
fn flat(_: &Answers) -> Result<Charge, Unfillable> {
    Ok(Charge {
        name: "Service".to_owned(),
        percent: 10,
    })
}

fn custom(answers: &Answers) -> Result<Charge, Unfillable> {
    let Some(Value::Text(name)) = answers.get("name") else {
        return Err(Unfillable::Impossible(BROKEN));
    };
    let Some(Value::Int(percent)) = answers.get("percent") else {
        return Err(Unfillable::Impossible(BROKEN));
    };
    if *percent < 0 {
        return Err(Unfillable::Impossible(BROKEN));
    }
    Ok(Charge {
        name: name.clone(),
        percent: *percent,
    })
}

fn answers(pairs: &[(&str, Value)]) -> Answers {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn filled() -> Answers {
    answers(&[
        ("name", Value::Text("Peak".to_owned())),
        ("percent", Value::Int(25)),
    ])
}

/// **The whole point of level 0 being level 1 with no blanks.**
#[test]
fn a_template_with_no_blanks_is_a_preset() {
    let written =
        Authored::written(TEMPLATES, "flat", Answers::new()).expect("a preset needs no answers");

    assert_eq!(
        written,
        Authored::Preset {
            template: "flat".to_owned()
        }
    );
    assert_eq!(written.level(), "preset");
    assert_eq!(
        written.rule(TEMPLATES).expect("it builds"),
        Charge {
            name: "Service".to_owned(),
            percent: 10
        }
    );
}

#[test]
fn a_template_with_blanks_is_a_form_and_keeps_the_answers() {
    let written = Authored::written(TEMPLATES, "custom", filled()).expect("filled in");

    assert_eq!(written.level(), "form");
    assert_eq!(written.template(), Some("custom"));
    let Authored::Form { answers, .. } = &written else {
        panic!("a form")
    };
    assert_eq!(answers.get("percent"), Some(&Value::Int(25)));
}

/// **Nothing stores the built artifact beside its answers**, which is what
/// makes the two unable to disagree.
///
/// Serialised, because that is where a second copy would be visible — and
/// where a later reader would be tempted to trust it.
#[test]
fn a_form_stores_its_answers_and_not_what_they_build() {
    let written = Authored::written(TEMPLATES, "custom", filled()).expect("filled in");
    let json = serde_json::to_value(&written).expect("serialises");

    assert_eq!(json["level"], "form");
    assert_eq!(json["answers"]["percent"]["of"], 25);
    assert!(
        json.get("rule").is_none(),
        "a templated rule holds no artifact: {json}"
    );
}

/// Reading it back builds it again, every time.
#[test]
fn a_form_rebuilds_its_artifact_on_every_read() {
    let written = Authored::written(TEMPLATES, "custom", filled()).expect("filled in");
    let once = written.rule(TEMPLATES).expect("builds");
    let twice = written.rule(TEMPLATES).expect("builds again");

    assert_eq!(once, twice);
    assert_eq!(
        once,
        Charge {
            name: "Peak".to_owned(),
            percent: 25
        }
    );
}

/// **A rule somebody hand-edited is no longer that form's rule.**
#[test]
fn hand_editing_a_form_drops_it() {
    let written = Authored::written(TEMPLATES, "custom", filled()).expect("filled in");
    let edited = Authored::Raw {
        rule: Charge {
            name: "Peak".to_owned(),
            percent: 40,
        },
    };

    assert_eq!(written.template(), Some("custom"));
    assert_eq!(edited.template(), None, "the form is gone, not stale");
    assert_eq!(edited.level(), "raw");
    assert_eq!(edited.rule(TEMPLATES).expect("builds").percent, 40);
}

/// A hand-written rule needs no templates at all, which is what lets a
/// consumer with no templates still evaluate.
#[test]
fn a_written_rule_reads_back_without_the_templates() {
    let rule = Charge {
        name: "Peak".to_owned(),
        percent: 40,
    };
    for written in [
        Authored::Raw { rule: rule.clone() },
        Authored::Builder { rule: rule.clone() },
    ] {
        assert_eq!(written.rule(&[]).expect("no templates needed"), rule);
    }
}

#[test]
fn a_missing_answer_is_refused_rather_than_defaulted() {
    let why = Authored::written(
        TEMPLATES,
        "custom",
        answers(&[("name", Value::Text("Peak".to_owned()))]),
    )
    .expect_err("percent was not answered");

    assert_eq!(
        why,
        Unfillable::Unanswered {
            template: "custom".to_owned(),
            field: "percent".to_owned()
        }
    );
}

#[test]
fn an_answer_of_the_wrong_kind_is_refused() {
    let why = Authored::written(
        TEMPLATES,
        "custom",
        answers(&[
            ("name", Value::Text("Peak".to_owned())),
            ("percent", Value::Text("25".to_owned())),
        ]),
    )
    .expect_err("a percentage is not text");

    assert_eq!(
        why,
        Unfillable::WrongKind {
            field: "percent".to_owned(),
            declared: Kind::Int,
            given: Kind::Text
        }
    );
}

/// **An answer nobody asked for is a mistake, not spare data.**
///
/// It is how a renamed field goes unnoticed: the old answer sits there unread
/// and the new one is missing, and only one of those is otherwise reported.
#[test]
fn an_answer_the_template_does_not_ask_for_is_refused() {
    let mut spare = filled();
    spare.insert("percentage".to_owned(), Value::Int(25));

    let why = Authored::written(TEMPLATES, "custom", spare).expect_err("no such field");

    assert_eq!(
        why,
        Unfillable::NotAsked {
            field: "percentage".to_owned()
        }
    );
}

/// The answers were all present and describe nothing that can exist.
#[test]
fn answers_that_describe_no_rule_are_refused_while_they_are_being_written() {
    let why = Authored::written(
        TEMPLATES,
        "custom",
        answers(&[
            ("name", Value::Text("Peak".to_owned())),
            ("percent", Value::Int(-5)),
        ]),
    )
    .expect_err("a negative charge");

    assert_eq!(why, Unfillable::Impossible(BROKEN));
}

#[test]
fn a_template_this_build_does_not_ship_is_refused() {
    let why = Authored::written(TEMPLATES, "seasonal", Answers::new())
        .expect_err("there is no such template");

    assert_eq!(why, Unfillable::NoSuchTemplate("seasonal".to_owned()));
}

/// **A deploy that withdraws a template does not silently drop the rule.**
///
/// The rule stops being readable and says why, which is the refusal L6 asks
/// for rather than a tariff quietly missing its peak band.
#[test]
fn a_form_whose_template_is_withdrawn_refuses_rather_than_vanishing() {
    let written = Authored::written(TEMPLATES, "custom", filled()).expect("filled in");

    let why = written.rule(&TEMPLATES[..1]).expect_err("custom is gone");

    assert_eq!(why, Unfillable::NoSuchTemplate("custom".to_owned()));
}

/// A template that gains a blank leaves every rule written before it
/// unfillable, and says which blank.
#[test]
fn a_template_that_grows_a_field_refuses_the_answers_written_before_it() {
    static GREW: &[Template<Charge>] = &[Template {
        id: "custom",
        name_en: "A percentage you choose",
        name_ar: "نسبة تختارها",
        fields: &[
            NAME,
            PERCENT,
            Field {
                key: "branch",
                label_en: "Branch",
                label_ar: "الفرع",
                kind: Kind::Text,
            },
        ],
        build: custom,
    }];
    let written = Authored::written(TEMPLATES, "custom", filled()).expect("filled in");

    let why = written.rule(GREW).expect_err("branch was never answered");

    assert_eq!(
        why,
        Unfillable::Unanswered {
            template: "custom".to_owned(),
            field: "branch".to_owned()
        }
    );
}

#[test]
fn every_level_round_trips_through_json() {
    let rule = Charge {
        name: "Peak".to_owned(),
        percent: 40,
    };
    let levels = [
        Authored::Preset {
            template: "flat".to_owned(),
        },
        Authored::Form {
            template: "custom".to_owned(),
            answers: filled(),
        },
        Authored::Builder { rule: rule.clone() },
        Authored::Raw { rule },
    ];

    for level in levels {
        let json = serde_json::to_string(&level).expect("serialises");
        let back: Authored<Charge> = serde_json::from_str(&json).expect("and comes back");
        assert_eq!(back, level, "{json}");
    }
}

/// **Answers serialise in key order, not insertion order**, so the same form
/// saved twice is the same bytes — which is what an `ETag` on a settings entry
/// and a diff of a tenant's configuration both depend on.
#[test]
fn answers_serialise_in_key_order_whatever_order_they_arrived_in() {
    let mut backwards = Answers::new();
    backwards.insert("percent".to_owned(), Value::Int(25));
    backwards.insert("name".to_owned(), Value::Text("Peak".to_owned()));

    let json = serde_json::to_string(&Authored::<Charge>::Form {
        template: "custom".to_owned(),
        answers: backwards,
    })
    .expect("serialises");

    assert!(
        json.find(r#""name""#) < json.find(r#""percent""#),
        "answers came back in insertion order: {json}"
    );
}

#[test]
fn a_template_names_itself_in_both_languages() {
    let custom = &TEMPLATES[1];
    assert_eq!(
        custom.name(erp_i18n::Locale::English),
        "A percentage you choose"
    );
    assert_eq!(custom.name(erp_i18n::Locale::Arabic), "نسبة تختارها");
    assert_eq!(NAME.label(erp_i18n::Locale::Arabic), "الاسم");
    assert!(custom.is_form());
    assert!(!TEMPLATES[0].is_form(), "a preset asks nothing");
}
