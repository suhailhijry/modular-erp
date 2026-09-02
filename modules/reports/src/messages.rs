//! This module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NOT_A_PERIOD: MessageCode = MessageCode::new("reports.not_a_period");
pub const RANGE_TOO_LONG: MessageCode = MessageCode::new("reports.range_too_long");
pub const BACKWARDS: MessageCode = MessageCode::new("reports.backwards");
pub const DOES_NOT_RECONCILE: MessageCode = MessageCode::new("reports.does_not_reconcile");
pub const DATABASE: MessageCode = MessageCode::new("reports.database");

pub const CODES: &[MessageCode] = &[
    NOT_A_PERIOD,
    RANGE_TOO_LONG,
    BACKWARDS,
    DOES_NOT_RECONCILE,
    DATABASE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NOT_A_PERIOD,
        Locale::English,
        Template::Simple("{period} is not a month. Write it as 2026-05."),
    ),
    (
        NOT_A_PERIOD,
        Locale::Arabic,
        Template::Simple("{period} ليس شهرًا. اكتبه هكذا: 2026-05."),
    ),
    (
        RANGE_TOO_LONG,
        Locale::English,
        Template::Plural {
            zero: None,
            one: Some("A report may cover at most one month."),
            two: None,
            few: None,
            many: None,
            other: "A report may cover at most {n} months.",
        },
    ),
    (
        RANGE_TOO_LONG,
        Locale::Arabic,
        Template::Plural {
            zero: Some("لا يغطي التقرير أي شهر."),
            one: Some("لا يغطي التقرير أكثر من شهر واحد."),
            two: Some("لا يغطي التقرير أكثر من شهرين."),
            few: Some("لا يغطي التقرير أكثر من {n} أشهر."),
            many: Some("لا يغطي التقرير أكثر من {n} شهرًا."),
            other: "لا يغطي التقرير أكثر من {n} شهر.",
        },
    ),
    (
        BACKWARDS,
        Locale::English,
        Template::Simple("A report ends after it starts. {from} is later than {until}."),
    ),
    (
        BACKWARDS,
        Locale::Arabic,
        Template::Simple("ينتهي التقرير بعد أن يبدأ. {from} بعد {until}."),
    ),
    // **Not a coloured cell.** See `reconcile.rs`: a report that disagrees with
    // the books is worse than no report, because somebody acts on it.
    (
        DOES_NOT_RECONCILE,
        Locale::English,
        Template::Plural {
            zero: None,
            one: Some(
                "These figures do not agree with the books: one discrepancy. \
                 Nothing is shown until it is resolved.",
            ),
            two: None,
            few: None,
            many: None,
            other: "These figures do not agree with the books: {n} discrepancies. \
                    Nothing is shown until they are resolved.",
        },
    ),
    (
        DOES_NOT_RECONCILE,
        Locale::Arabic,
        Template::Plural {
            zero: Some("هذه الأرقام تطابق الدفاتر."),
            one: Some("هذه الأرقام لا تطابق الدفاتر: فرق واحد. لا يُعرض شيء حتى تتم تسويته."),
            two: Some("هذه الأرقام لا تطابق الدفاتر: فرقان. لا يُعرض شيء حتى تتم تسويتهما."),
            few: Some("هذه الأرقام لا تطابق الدفاتر: {n} فروق. لا يُعرض شيء حتى تتم تسويتها."),
            many: Some("هذه الأرقام لا تطابق الدفاتر: {n} فرقًا. لا يُعرض شيء حتى تتم تسويتها."),
            other: "هذه الأرقام لا تطابق الدفاتر: {n} فرق. لا يُعرض شيء حتى تتم تسويتها.",
        },
    ),
    (
        DATABASE,
        Locale::English,
        Template::Simple("That could not be read. Try again."),
    ),
    (
        DATABASE,
        Locale::Arabic,
        Template::Simple("تعذّرت القراءة. أعد المحاولة."),
    ),
];
