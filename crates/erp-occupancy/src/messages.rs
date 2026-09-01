//! The occupancy engine's messages, in every supported language.
//!
//! Every code here is a refusal a person caused and can act on: pick another
//! time, pick another chair, or ask for fewer places. There is one exception,
//! [`INTERNAL`], and it is the same exception the event log makes.

use erp_i18n::{Locale, MessageCode, Template};

pub const OVERBOOKED: MessageCode = MessageCode::new("occupancy.overbooked");
pub const NO_SUCH_RESOURCE: MessageCode = MessageCode::new("occupancy.no_such_resource");
pub const NOTHING_CLAIMED: MessageCode = MessageCode::new("occupancy.nothing_claimed");
pub const EMPTY_SPAN: MessageCode = MessageCode::new("occupancy.empty_span");
pub const SPAN_TOO_LONG: MessageCode = MessageCode::new("occupancy.span_too_long");
pub const INTERNAL: MessageCode = MessageCode::new("occupancy.internal_error");

pub static CODES: &[MessageCode] = &[
    OVERBOOKED,
    NO_SUCH_RESOURCE,
    NOTHING_CLAIMED,
    EMPTY_SPAN,
    SPAN_TOO_LONG,
    INTERNAL,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    // Three numbers, so `MessageArg::Int` throughout and no plural form: a
    // template can only agree with one count, and this one names three.
    (
        OVERBOOKED,
        Locale::English,
        Template::Simple(
            "{resource} is already holding {held} of {capacity} at that time, so {wanted} more will not fit.",
        ),
    ),
    (
        OVERBOOKED,
        Locale::Arabic,
        Template::Simple(
            "{resource} محجوز بمقدار {held} من {capacity} في ذلك الوقت، ولا يتسع لـ {wanted} إضافية.",
        ),
    ),
    (
        NO_SUCH_RESOURCE,
        Locale::English,
        Template::Simple("There is nothing here called {resource}."),
    ),
    (
        NO_SUCH_RESOURCE,
        Locale::Arabic,
        Template::Simple("لا يوجد مورد باسم {resource}."),
    ),
    (
        NOTHING_CLAIMED,
        Locale::English,
        Template::Simple("A booking has to be for at least one place."),
    ),
    (
        NOTHING_CLAIMED,
        Locale::Arabic,
        Template::Simple("يجب أن يكون الحجز لمكان واحد على الأقل."),
    ),
    (
        EMPTY_SPAN,
        Locale::English,
        Template::Simple("A booking has to end after it starts."),
    ),
    (
        EMPTY_SPAN,
        Locale::Arabic,
        Template::Simple("يجب أن ينتهي الحجز بعد بدايته."),
    ),
    (
        SPAN_TOO_LONG,
        Locale::English,
        Template::Plural {
            zero: None,
            one: Some("A booking may not run longer than one day."),
            two: None,
            few: None,
            many: None,
            other: "A booking may not run longer than {n} days.",
        },
    ),
    (
        SPAN_TOO_LONG,
        Locale::Arabic,
        Template::Plural {
            zero: Some("لا يمكن أن يمتد الحجز أي يوم."),
            one: Some("لا يمكن أن يمتد الحجز أكثر من يوم واحد."),
            two: Some("لا يمكن أن يمتد الحجز أكثر من يومين."),
            few: Some("لا يمكن أن يمتد الحجز أكثر من {n} أيام."),
            many: Some("لا يمكن أن يمتد الحجز أكثر من {n} يومًا."),
            other: "لا يمكن أن يمتد الحجز أكثر من {n} يوم.",
        },
    ),
    (
        INTERNAL,
        Locale::English,
        Template::Simple("Something went wrong on our side. The problem has been recorded."),
    ),
    (
        INTERNAL,
        Locale::Arabic,
        Template::Simple("حدث خطأ لدينا. تم تسجيل المشكلة."),
    ),
];
