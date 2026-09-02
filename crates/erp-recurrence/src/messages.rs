//! This crate's messages, in every supported language.
//!
//! # Why these are `recurrence.` and not `booking.`
//!
//! They were `booking.` when only `booking` had a repeating calendar. `hr` needs
//! the same shape for a shift, and a shift refused with `booking.not_a_window`
//! would name a module the tenant may not have enabled — so the codes move with
//! the type.
//!
//! **A code is a client-facing identifier and this API tells clients to branch
//! on it**, so renaming one is a breaking change. It is free here because
//! nothing is released; it would not have been in six months.

use erp_i18n::{Locale, MessageCode, Template};

pub const NOT_A_WINDOW: MessageCode = MessageCode::new("recurrence.not_a_window");
pub const NOT_A_TIME_OF_DAY: MessageCode = MessageCode::new("recurrence.not_a_time_of_day");
pub const BACKWARDS_DATES: MessageCode = MessageCode::new("recurrence.backwards_dates");
pub const NOT_A_DAY_OF_THE_MONTH: MessageCode =
    MessageCode::new("recurrence.not_a_day_of_the_month");
pub const NOT_A_MONTH: MessageCode = MessageCode::new("recurrence.not_a_month");
pub const NOT_A_WEEKDAY: MessageCode = MessageCode::new("recurrence.not_a_weekday");
pub const NOT_AN_OFFSET: MessageCode = MessageCode::new("recurrence.not_an_offset");

pub const CODES: &[MessageCode] = &[
    NOT_A_WINDOW,
    NOT_A_TIME_OF_DAY,
    BACKWARDS_DATES,
    NOT_A_DAY_OF_THE_MONTH,
    NOT_A_MONTH,
    NOT_A_WEEKDAY,
    NOT_AN_OFFSET,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NOT_A_WINDOW,
        Locale::English,
        Template::Simple(
            "Opening hours must close after they open. A window that runs past midnight is two windows.",
        ),
    ),
    (
        NOT_A_WINDOW,
        Locale::Arabic,
        Template::Simple(
            "يجب أن تنتهي ساعات العمل بعد بدايتها. النافذة التي تتجاوز منتصف الليل هي نافذتان.",
        ),
    ),
    (
        NOT_A_TIME_OF_DAY,
        Locale::English,
        Template::Simple("A time of day is minutes past midnight, from 0 to {most}."),
    ),
    (
        NOT_A_TIME_OF_DAY,
        Locale::Arabic,
        Template::Simple("وقت اليوم هو عدد الدقائق بعد منتصف الليل، من 0 إلى {most}."),
    ),
    (
        BACKWARDS_DATES,
        Locale::English,
        Template::Simple("The last day these hours apply is before the first."),
    ),
    (
        BACKWARDS_DATES,
        Locale::Arabic,
        Template::Simple("آخر يوم تسري فيه هذه الساعات يسبق أولها."),
    ),
    (
        NOT_A_DAY_OF_THE_MONTH,
        Locale::English,
        Template::Simple("{value} is not a day of any month."),
    ),
    (
        NOT_A_DAY_OF_THE_MONTH,
        Locale::Arabic,
        Template::Simple("{value} ليس يومًا في أي شهر."),
    ),
    (
        NOT_A_MONTH,
        Locale::English,
        Template::Simple("{value} is not a month."),
    ),
    (
        NOT_A_MONTH,
        Locale::Arabic,
        Template::Simple("{value} ليس شهرًا."),
    ),
    (
        NOT_A_WEEKDAY,
        Locale::English,
        Template::Simple("{value} is not a weekday. Monday is 1 and Sunday is 7."),
    ),
    (
        NOT_A_WEEKDAY,
        Locale::Arabic,
        Template::Simple("{value} ليس يومًا من أيام الأسبوع. الاثنين هو 1 والأحد هو 7."),
    ),
    (
        NOT_AN_OFFSET,
        Locale::English,
        Template::Simple("A timezone offset is minutes from UTC, between -{limit} and {limit}."),
    ),
    (
        NOT_AN_OFFSET,
        Locale::Arabic,
        Template::Simple("فرق التوقيت هو عدد الدقائق عن التوقيت العالمي، بين -{limit} و {limit}."),
    ),
];
