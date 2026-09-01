//! The booking module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NOTHING_TO_BOOK: MessageCode = MessageCode::new("booking.nothing_to_book");
pub const NO_NAME: MessageCode = MessageCode::new("booking.no_name");
pub const RESOURCE_HAS_NO_NAME: MessageCode = MessageCode::new("booking.resource_has_no_name");
pub const NO_SUCH_CUSTOMER: MessageCode = MessageCode::new("booking.no_such_customer");
pub const NO_SUCH_RESOURCE: MessageCode = MessageCode::new("booking.no_such_resource");
pub const WITHDRAWN: MessageCode = MessageCode::new("booking.withdrawn");
pub const NOT_OFFERED: MessageCode = MessageCode::new("booking.not_offered");
pub const NO_SUCH_RESERVATION: MessageCode = MessageCode::new("booking.no_such_reservation");
pub const OVER: MessageCode = MessageCode::new("booking.over");
pub const CANNOT_MOVE: MessageCode = MessageCode::new("booking.cannot_move");
pub const NO_SUCH_LINE: MessageCode = MessageCode::new("booking.no_such_line");
pub const RESERVED_NAME: MessageCode = MessageCode::new("booking.reserved_name");
pub const INVALID_REFERENCE: MessageCode = MessageCode::new("booking.invalid_reference");
pub const NOT_A_WINDOW: MessageCode = MessageCode::new("booking.not_a_window");
pub const NOT_A_TIME_OF_DAY: MessageCode = MessageCode::new("booking.not_a_time_of_day");
pub const BACKWARDS_DATES: MessageCode = MessageCode::new("booking.backwards_dates");
pub const NOT_A_DAY_OF_THE_MONTH: MessageCode = MessageCode::new("booking.not_a_day_of_the_month");
pub const NOT_A_MONTH: MessageCode = MessageCode::new("booking.not_a_month");
pub const NOT_A_WEEKDAY: MessageCode = MessageCode::new("booking.not_a_weekday");
pub const UNKNOWN_STAGE: MessageCode = MessageCode::new("booking.unknown_stage");
pub const UNKNOWN_KIND: MessageCode = MessageCode::new("booking.unknown_kind");
pub const NOT_AN_OFFSET: MessageCode = MessageCode::new("booking.not_an_offset");
pub const NO_SUCH_TRADE: MessageCode = MessageCode::new("booking.no_such_trade");
pub const NOT_A_RATE: MessageCode = MessageCode::new("booking.not_a_rate");
pub const NOTHING_CHARGED: MessageCode = MessageCode::new("booking.nothing_charged");
pub const NOT_AN_ALLOWANCE: MessageCode = MessageCode::new("booking.not_an_allowance");
pub const ALLOWANCE_TOO_LARGE: MessageCode = MessageCode::new("booking.allowance_too_large");
pub const MIXED_CURRENCIES: MessageCode = MessageCode::new("booking.mixed_currencies");
pub const AMOUNT_OUT_OF_RANGE: MessageCode = MessageCode::new("booking.amount_out_of_range");

pub static CODES: &[MessageCode] = &[
    NOTHING_TO_BOOK,
    NO_NAME,
    RESOURCE_HAS_NO_NAME,
    NO_SUCH_CUSTOMER,
    NO_SUCH_RESOURCE,
    WITHDRAWN,
    NOT_OFFERED,
    NO_SUCH_RESERVATION,
    OVER,
    CANNOT_MOVE,
    NO_SUCH_LINE,
    RESERVED_NAME,
    INVALID_REFERENCE,
    NOT_A_WINDOW,
    NOT_A_TIME_OF_DAY,
    BACKWARDS_DATES,
    NOT_A_DAY_OF_THE_MONTH,
    NOT_A_MONTH,
    NOT_A_WEEKDAY,
    UNKNOWN_STAGE,
    UNKNOWN_KIND,
    NOT_AN_OFFSET,
    NO_SUCH_TRADE,
    NOT_A_RATE,
    NOTHING_CHARGED,
    NOT_AN_ALLOWANCE,
    ALLOWANCE_TOO_LARGE,
    MIXED_CURRENCIES,
    AMOUNT_OUT_OF_RANGE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NOTHING_TO_BOOK,
        Locale::English,
        Template::Simple("A booking needs at least one thing being booked."),
    ),
    (
        NOTHING_TO_BOOK,
        Locale::Arabic,
        Template::Simple("يحتاج الحجز إلى خدمة واحدة على الأقل."),
    ),
    (
        NO_NAME,
        Locale::English,
        Template::Simple("A booking needs a name to put in the diary."),
    ),
    (
        NO_NAME,
        Locale::Arabic,
        Template::Simple("يحتاج الحجز إلى اسم يظهر في المفكرة."),
    ),
    (
        RESOURCE_HAS_NO_NAME,
        Locale::English,
        Template::Simple("Give it a name people will recognise on the calendar."),
    ),
    (
        RESOURCE_HAS_NO_NAME,
        Locale::Arabic,
        Template::Simple("امنحه اسمًا يتعرف عليه الناس في التقويم."),
    ),
    (
        NO_SUCH_CUSTOMER,
        Locale::English,
        Template::Simple("There is no customer {customer}."),
    ),
    (
        NO_SUCH_CUSTOMER,
        Locale::Arabic,
        Template::Simple("لا يوجد عميل {customer}."),
    ),
    (
        NO_SUCH_RESOURCE,
        Locale::English,
        Template::Simple("There is nothing bookable called {resource}."),
    ),
    (
        NO_SUCH_RESOURCE,
        Locale::Arabic,
        Template::Simple("لا يوجد شيء قابل للحجز باسم {resource}."),
    ),
    (
        WITHDRAWN,
        Locale::English,
        Template::Simple("{resource} is out of service."),
    ),
    (
        WITHDRAWN,
        Locale::Arabic,
        Template::Simple("{resource} خارج الخدمة."),
    ),
    (
        NOT_OFFERED,
        Locale::English,
        Template::Simple("{resource} is not open at that time."),
    ),
    (
        NOT_OFFERED,
        Locale::Arabic,
        Template::Simple("{resource} غير متاح في ذلك الوقت."),
    ),
    (
        NO_SUCH_RESERVATION,
        Locale::English,
        Template::Simple("There is no booking {reservation}."),
    ),
    (
        NO_SUCH_RESERVATION,
        Locale::Arabic,
        Template::Simple("لا يوجد حجز {reservation}."),
    ),
    (
        OVER,
        Locale::English,
        Template::Simple("This booking is already {stage}, and nothing more can happen to it."),
    ),
    (
        OVER,
        Locale::Arabic,
        Template::Simple("هذا الحجز {stage} بالفعل، ولا يمكن تغييره بعد ذلك."),
    ),
    (
        CANNOT_MOVE,
        Locale::English,
        Template::Simple("A booking cannot go from {from} to {to}."),
    ),
    (
        CANNOT_MOVE,
        Locale::Arabic,
        Template::Simple("لا يمكن نقل الحجز من {from} إلى {to}."),
    ),
    (
        NO_SUCH_LINE,
        Locale::English,
        Template::Simple("This booking has no line {line}."),
    ),
    (
        NO_SUCH_LINE,
        Locale::Arabic,
        Template::Simple("لا يحتوي هذا الحجز على البند {line}."),
    ),
    (
        RESERVED_NAME,
        Locale::English,
        Template::Simple("Names beginning with \"customer.\" are kept for customers' own diaries."),
    ),
    (
        RESERVED_NAME,
        Locale::Arabic,
        Template::Simple("الأسماء التي تبدأ بـ \"customer.\" محجوزة لمفكرات العملاء."),
    ),
    (
        INVALID_REFERENCE,
        Locale::English,
        Template::Simple("{reference} cannot be used as a reference."),
    ),
    (
        INVALID_REFERENCE,
        Locale::Arabic,
        Template::Simple("لا يمكن استخدام {reference} كمرجع."),
    ),
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
        UNKNOWN_STAGE,
        Locale::English,
        Template::Simple("{value} is not a stage a booking can be in."),
    ),
    (
        UNKNOWN_STAGE,
        Locale::Arabic,
        Template::Simple("{value} ليست مرحلة يمكن أن يكون الحجز فيها."),
    ),
    (
        UNKNOWN_KIND,
        Locale::English,
        Template::Simple("{value} is not a person, a place or a thing."),
    ),
    (
        UNKNOWN_KIND,
        Locale::Arabic,
        Template::Simple("{value} ليس شخصًا ولا مكانًا ولا شيئًا."),
    ),
    (
        NOT_AN_OFFSET,
        Locale::English,
        Template::Simple("A timezone offset is minutes from UTC, between -{limit} and {limit}."),
    ),
    (
        NOT_A_RATE,
        Locale::English,
        Template::Simple("A price cannot be negative."),
    ),
    (
        NOT_A_RATE,
        Locale::Arabic,
        Template::Simple("لا يمكن أن يكون السعر بالسالب."),
    ),
    (
        NOTHING_CHARGED,
        Locale::English,
        Template::Simple("A priced line must be for at least one."),
    ),
    (
        NOTHING_CHARGED,
        Locale::Arabic,
        Template::Simple("يجب أن يكون البند المسعّر لواحد على الأقل."),
    ),
    (
        NOT_AN_ALLOWANCE,
        Locale::English,
        Template::Simple("A discount is the amount taken off, so it is a positive number."),
    ),
    (
        NOT_AN_ALLOWANCE,
        Locale::Arabic,
        Template::Simple("الخصم هو المبلغ المحسوم، لذا يجب أن يكون رقمًا موجبًا."),
    ),
    (
        ALLOWANCE_TOO_LARGE,
        Locale::English,
        Template::Simple("A discount cannot be larger than what it comes off."),
    ),
    (
        ALLOWANCE_TOO_LARGE,
        Locale::Arabic,
        Template::Simple("لا يمكن أن يتجاوز الخصم المبلغ المحسوم منه."),
    ),
    (
        MIXED_CURRENCIES,
        Locale::English,
        Template::Simple("Every amount on a booking must be in the same currency."),
    ),
    (
        MIXED_CURRENCIES,
        Locale::Arabic,
        Template::Simple("يجب أن تكون جميع المبالغ في الحجز بالعملة نفسها."),
    ),
    (
        AMOUNT_OUT_OF_RANGE,
        Locale::English,
        Template::Simple("That amount is too large to record."),
    ),
    (
        AMOUNT_OUT_OF_RANGE,
        Locale::Arabic,
        Template::Simple("هذا المبلغ أكبر من أن يُسجَّل."),
    ),
    (
        NO_SUCH_TRADE,
        Locale::English,
        Template::Simple("There is no ready-made rota called {trade}."),
    ),
    (
        NO_SUCH_TRADE,
        Locale::Arabic,
        Template::Simple("لا توجد قائمة موارد جاهزة باسم {trade}."),
    ),
    (
        NOT_AN_OFFSET,
        Locale::Arabic,
        Template::Simple("فرق التوقيت هو عدد الدقائق عن التوقيت العالمي، بين -{limit} و {limit}."),
    ),
];
