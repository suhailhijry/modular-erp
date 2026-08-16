//! The Saudi tax module's messages, in every supported language.

use spa_i18n::{Locale, MessageCode, Template};

pub const EMPTY_PERIOD: MessageCode = MessageCode::new("tax_sa.empty_period");
pub const ALREADY_FILED: MessageCode = MessageCode::new("tax_sa.already_filed");
pub const INVALID_PERIOD: MessageCode = MessageCode::new("tax_sa.invalid_period");

pub static CODES: &[MessageCode] = &[EMPTY_PERIOD, ALREADY_FILED, INVALID_PERIOD];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        EMPTY_PERIOD,
        Locale::English,
        Template::Simple("A period must end after it starts. `until` is exclusive."),
    ),
    (
        EMPTY_PERIOD,
        Locale::Arabic,
        Template::Simple("يجب أن تنتهي الفترة بعد بدايتها. تاريخ الانتهاء غير شامل."),
    ),
    (
        ALREADY_FILED,
        Locale::English,
        Template::Simple(
            "The period {period} was filed on {on}. Correcting a filed return is an amendment, not a second filing.",
        ),
    ),
    (
        ALREADY_FILED,
        Locale::Arabic,
        Template::Simple(
            "قُدِّم إقرار الفترة {period} بتاريخ {on}. تصحيح إقرار مُقدَّم يكون بتعديل وليس بإقرار ثانٍ.",
        ),
    ),
    (
        INVALID_PERIOD,
        Locale::English,
        Template::Simple("{period} cannot be used as a period identifier."),
    ),
    (
        INVALID_PERIOD,
        Locale::Arabic,
        Template::Simple("لا يمكن استخدام {period} كمعرّف فترة."),
    ),
];
