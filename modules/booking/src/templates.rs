//! **Ready-made price bands, with blanks a business fills in.**
//!
//! Level 1 of the four authoring levels (`erp_rules::authoring`). A tenant
//! picks a template, answers two or three questions in their own language, and
//! gets a [`Band`] — without meeting a bitmask of weekdays, a minute count past
//! midnight, or basis points.
//!
//! # Why a form is worth having here
//!
//! `Band` speaks the engine's language: `opens_at: 1020`, `uplift: 2500`. A
//! salon owner says "Thursday evenings, a quarter more". The template is the
//! translation, and it is the *only* translation — the answers are stored and
//! the band is rebuilt from them on every read, so what the screen shows and
//! what the price engine uses cannot come apart.
//!
//! # Why this build ships no presets
//!
//! A preset is a template with no blanks, and the mechanism supports one. There
//! is none here because the one number that matters — how much dearer — is the
//! most business-specific choice on the screen. A preset that picked 25% for a
//! salon would be inventing their pricing, and they would have to un-pick it.
//!
//! Presets earn their place where the *shape* is the answer and no number is
//! being guessed at. Pricing is not that.
//!
//! # Why every template asks for a name
//!
//! The band's name is printed beside the price on a receipt, so it is the
//! customer's language and not the build's. Deriving it — "Thursday peak" — is
//! how a shipped English string ends up on a Riyadh salon's bill.

use erp_recurrence::Availability;
use erp_rules::{Answers, Field, Kind, Template, Unfillable, Value};

use crate::messages;
use crate::pricing::Band;

pub(crate) const NAME: Field = Field {
    key: "name",
    label_en: "What to call it",
    label_ar: "الاسم",
    kind: Kind::Text,
};

/// **Percent, not basis points.** "25" is what a business says; `2500` is what
/// the arithmetic needs, and doing that conversion here is most of the reason
/// this file exists.
pub(crate) const PERCENT: Field = Field {
    key: "percent",
    label_en: "Percent dearer (negative is cheaper)",
    label_ar: "نسبة الزيادة (بالسالب للتخفيض)",
    kind: Kind::Int,
};

pub(crate) const WEEKDAY: Field = Field {
    key: "weekday",
    label_en: "Day of the week, 1 is Monday and 7 is Sunday",
    label_ar: "يوم الأسبوع، ١ الاثنين و٧ الأحد",
    kind: Kind::Int,
};

pub(crate) const FROM_HOUR: Field = Field {
    key: "from_hour",
    label_en: "From this hour until midnight, 0 to 23",
    label_ar: "من هذه الساعة حتى منتصف الليل، ٠ إلى ٢٣",
    kind: Kind::Int,
};

/// Every price-band template this build ships.
///
/// Nothing reads [`Template::id`] but the lookup, so another template is an
/// entry in this list and a function, and no code at all anywhere else.
pub static TARIFF_TEMPLATES: &[Template<Band>] = &[
    Template {
        id: "weekday",
        name_en: "A day of the week costs more",
        name_ar: "يوم من الأسبوع بسعر أعلى",
        fields: &[NAME, WEEKDAY, PERCENT],
        build: whole_weekday,
    },
    Template {
        // The architecture's own worked example: "make Thursday evenings 25%
        // dearer".
        id: "weekday_evening",
        name_en: "An evening costs more",
        name_ar: "أمسية بسعر أعلى",
        fields: &[NAME, WEEKDAY, FROM_HOUR, PERCENT],
        build: weekday_evening,
    },
];

fn whole_weekday(answers: &Answers) -> Result<Band, Unfillable> {
    band(answers, 0)
}

fn weekday_evening(answers: &Answers) -> Result<Band, Unfillable> {
    band(answers, hour(answers, FROM_HOUR)?)
}

/// One weekday, from an hour until midnight.
///
/// Both templates are this function; they differ only in where the evening
/// starts, and a second copy of the weekday and percent handling is a second
/// place for them to disagree.
fn band(answers: &Answers, from_hour: u16) -> Result<Band, Unfillable> {
    let name = text(answers, NAME)?;
    if name.trim().is_empty() {
        return Err(Unfillable::Impossible(messages::NO_NAME));
    }
    let weekday = int(answers, WEEKDAY)?;
    let weekday = u8::try_from(weekday)
        .ok()
        .filter(|d| (1..=7).contains(d))
        .ok_or(Unfillable::Impossible(messages::NOT_A_WEEKDAY))?;

    // Below -100% the business pays the customer to come in; above 100× the
    // number stops being a price and starts being a typo. Both are refused
    // where the person can still see what they typed.
    let percent = int(answers, PERCENT)?;
    if !(-100..=10_000).contains(&percent) {
        return Err(Unfillable::Impossible(messages::NOT_A_RATE));
    }

    Ok(Band {
        name,
        when: Availability::from_parts(&[], &[weekday], &[], from_hour * 60, 24 * 60, None, None)
            // Unreachable: the hour is bounded above and the window always
            // closes at midnight. Refused rather than unwrapped, because
            // "unreachable" is a claim about today's `from_parts`.
            .map_err(|_| Unfillable::Impossible(messages::NOT_A_WINDOW))?,
        // The conversion the form exists for.
        uplift: i32::try_from(percent * 100)
            .map_err(|_| Unfillable::Impossible(messages::NOT_A_RATE))?,
    })
}

/// An hour of the local day.
fn hour(answers: &Answers, field: Field) -> Result<u16, Unfillable> {
    u16::try_from(int(answers, field)?)
        .ok()
        .filter(|h| *h <= 23)
        .ok_or(Unfillable::Impossible(messages::NOT_AN_HOUR))
}

/// The kinds are checked before `build` is called, so reaching the error arm
/// means this file's own field list and its builder disagree.
///
/// **Our bug, not the tenant's**, and it says so rather than blaming their
/// answers — and it is still a refusal rather than a panic, because a broken
/// template must not take a booking down with it.
fn int(answers: &Answers, field: Field) -> Result<i64, Unfillable> {
    match answers.get(field.key) {
        Some(Value::Int(n)) => Ok(*n),
        _ => Err(Unfillable::Impossible(messages::TEMPLATE_BROKEN)),
    }
}

fn text(answers: &Answers, field: Field) -> Result<String, Unfillable> {
    match answers.get(field.key) {
        Some(Value::Text(t)) => Ok(t.clone()),
        _ => Err(Unfillable::Impossible(messages::TEMPLATE_BROKEN)),
    }
}

/// The message a refusal from these templates carries.
///
/// Every arm is one of this module's own codes, so a caller renders it the way
/// it renders any other refusal.
#[must_use]
pub fn refusal(why: &Unfillable) -> erp_i18n::Message {
    match why {
        Unfillable::NoSuchTemplate(id) => erp_i18n::Message::new(messages::NO_SUCH_TEMPLATE)
            .with("template", erp_i18n::MessageArg::text(id.clone())),
        Unfillable::Unanswered { template, field } => erp_i18n::Message::new(messages::UNANSWERED)
            .with("template", erp_i18n::MessageArg::text(template.clone()))
            .with("field", erp_i18n::MessageArg::text(field.clone())),
        Unfillable::WrongKind { field, .. } | Unfillable::NotAsked { field } => {
            erp_i18n::Message::new(messages::NOT_AN_ANSWER)
                .with("field", erp_i18n::MessageArg::text(field.clone()))
        }
        Unfillable::Impossible(code) => erp_i18n::Message::new(code.clone()),
    }
}

/// The template with this id, if this build ships one.
#[must_use]
pub fn find(id: &str) -> Option<&'static Template<Band>> {
    TARIFF_TEMPLATES.iter().find(|t| t.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_i18n::Catalog as _;
    use erp_rules::{Authored, Value};

    fn answers(pairs: &[(&str, Value)]) -> Answers {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    fn thursday_evening() -> Answers {
        answers(&[
            ("name", Value::Text("ذروة الخميس".to_owned())),
            ("weekday", Value::Int(4)),
            ("from_hour", Value::Int(17)),
            ("percent", Value::Int(25)),
        ])
    }

    /// The architecture's own worked example: "make Thursday evenings 25%
    /// dearer", written by somebody who has never met a basis point.
    #[test]
    fn a_form_builds_the_band_the_business_described() {
        let band = Authored::written(TARIFF_TEMPLATES, "weekday_evening", thursday_evening())
            .expect("filled in")
            .rule(TARIFF_TEMPLATES)
            .expect("builds");

        assert_eq!(band.name, "ذروة الخميس");
        assert_eq!(band.uplift, 2_500, "25 percent is 2500 basis points");
        assert_eq!(band.when.weekdays(), vec![4]);
        assert_eq!(band.when.opens_at(), 17 * 60);
        assert_eq!(
            band.when.closes_at(),
            24 * 60,
            "an evening runs to midnight"
        );
    }

    /// **Percent in, basis points out.** Doing this conversion in one place is
    /// most of why a form is worth having.
    #[test]
    fn a_discount_is_a_negative_percentage() {
        let mut cheaper = thursday_evening();
        cheaper.insert("percent".to_owned(), Value::Int(-10));

        let band = Authored::written(TARIFF_TEMPLATES, "weekday_evening", cheaper)
            .expect("filled in")
            .rule(TARIFF_TEMPLATES)
            .expect("builds");

        assert_eq!(band.uplift, -1_000);
    }

    #[test]
    fn a_whole_weekday_runs_from_midnight_to_midnight() {
        let band = Authored::written(
            TARIFF_TEMPLATES,
            "weekday",
            answers(&[
                ("name", Value::Text("Friday".to_owned())),
                ("weekday", Value::Int(5)),
                ("percent", Value::Int(15)),
            ]),
        )
        .expect("filled in")
        .rule(TARIFF_TEMPLATES)
        .expect("builds");

        assert_eq!(band.when.opens_at(), 0);
        assert_eq!(band.when.closes_at(), 24 * 60);
        assert_eq!(band.when.weekdays(), vec![5]);
    }

    /// **Below -100% the business pays the customer to come in.**
    #[test]
    fn a_percentage_that_would_reverse_the_payment_is_refused() {
        let mut absurd = thursday_evening();
        absurd.insert("percent".to_owned(), Value::Int(-101));

        let why = Authored::written(TARIFF_TEMPLATES, "weekday_evening", absurd)
            .expect_err("nobody is paid to turn up");

        assert_eq!(why, Unfillable::Impossible(messages::NOT_A_RATE));
    }

    /// An uplift large enough to overflow the band is a typo, not a price.
    #[test]
    fn a_percentage_far_beyond_a_price_is_refused() {
        let mut absurd = thursday_evening();
        absurd.insert("percent".to_owned(), Value::Int(10_001));

        let why = Authored::written(TARIFF_TEMPLATES, "weekday_evening", absurd)
            .expect_err("that is not a price");

        assert_eq!(why, Unfillable::Impossible(messages::NOT_A_RATE));
    }

    #[test]
    fn a_day_outside_the_week_is_refused() {
        for day in [0, 8, -1, i64::from(u16::MAX)] {
            let mut wrong = thursday_evening();
            wrong.insert("weekday".to_owned(), Value::Int(day));

            let why = Authored::written(TARIFF_TEMPLATES, "weekday_evening", wrong)
                .expect_err("there is no such day");

            assert_eq!(
                why,
                Unfillable::Impossible(messages::NOT_A_WEEKDAY),
                "{day}"
            );
        }
    }

    #[test]
    fn an_hour_outside_the_day_is_refused() {
        for at in [24, -1, 1_000] {
            let mut wrong = thursday_evening();
            wrong.insert("from_hour".to_owned(), Value::Int(at));

            let why = Authored::written(TARIFF_TEMPLATES, "weekday_evening", wrong)
                .expect_err("there is no such hour");

            assert_eq!(why, Unfillable::Impossible(messages::NOT_AN_HOUR), "{at}");
        }
    }

    /// The name is printed beside the price, so a blank one is a band nobody
    /// can recognise on a receipt.
    #[test]
    fn a_band_with_no_name_is_refused() {
        let mut nameless = thursday_evening();
        nameless.insert("name".to_owned(), Value::Text("   ".to_owned()));

        let why = Authored::written(TARIFF_TEMPLATES, "weekday_evening", nameless)
            .expect_err("a band needs a name");

        assert_eq!(why, Unfillable::Impossible(messages::NO_NAME));
    }

    /// **Every refusal these templates make can be shown to the person who
    /// caused it.**
    ///
    /// Two ways that fails and neither is loud: `refusal` names a code the
    /// catalog does not have, so a settings screen shows `booking.not_a_rate`;
    /// or it names one whose sentence has a `{placeholder}` it forgot to fill,
    /// so the screen shows the brace. Whether the *translation* exists is a
    /// different question, and `the_catalog_is_complete` asks it.
    #[test]
    fn every_refusal_renders_as_a_sentence_rather_than_a_code() {
        let refusals = [
            Unfillable::NoSuchTemplate("seasonal".to_owned()),
            Unfillable::Unanswered {
                template: "weekday".to_owned(),
                field: "percent".to_owned(),
            },
            Unfillable::WrongKind {
                field: "percent".to_owned(),
                declared: Kind::Int,
                given: Kind::Text,
            },
            Unfillable::NotAsked {
                field: "percentage".to_owned(),
            },
            Unfillable::Impossible(messages::NOT_A_RATE),
            Unfillable::Impossible(messages::NOT_A_WEEKDAY),
            Unfillable::Impossible(messages::NOT_AN_HOUR),
            Unfillable::Impossible(messages::NOT_A_WINDOW),
            Unfillable::Impossible(messages::NO_NAME),
            Unfillable::Impossible(messages::TEMPLATE_BROKEN),
        ];
        for why in refusals {
            for locale in [erp_i18n::Locale::English, erp_i18n::Locale::Arabic] {
                let said = crate::CATALOG.render_or_code(locale, &refusal(&why));
                assert!(
                    !said.trim().is_empty() && !said.contains('{') && !said.starts_with("booking."),
                    "{why:?} in {locale:?} rendered as {said:?}"
                );
            }
        }
    }

    /// Every field a template declares is one its builder actually reads.
    ///
    /// **The pair that goes wrong quietly**: a field renamed in the list and
    /// not in the builder leaves the old answer unread and the new one
    /// unanswered, and `TEMPLATE_BROKEN` is what a tenant would see.
    #[test]
    fn every_declared_field_is_one_the_builder_reads() {
        for template in TARIFF_TEMPLATES {
            let filled: Answers = template
                .fields
                .iter()
                .map(|field| {
                    let answer = match field.kind {
                        Kind::Text => Value::Text("Peak".to_owned()),
                        // In range for every numeric field these templates
                        // declare: a weekday, an hour and a percentage.
                        Kind::Int => Value::Int(1),
                        Kind::Bool => Value::Bool(true),
                        Kind::Money => panic!("no band template asks for an amount"),
                    };
                    (field.key.to_owned(), answer)
                })
                .collect();

            assert!(
                Authored::written(TARIFF_TEMPLATES, template.id, filled).is_ok(),
                "{} declares a field its builder does not read",
                template.id
            );
        }
    }

    /// Two templates cannot share an id, or one of them is unreachable.
    #[test]
    fn every_template_has_its_own_id() {
        let mut ids: Vec<_> = TARIFF_TEMPLATES.iter().map(|t| t.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();

        assert_eq!(ids.len(), count, "two templates share an id");
    }
}
