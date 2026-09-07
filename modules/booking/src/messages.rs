//! The booking module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NO_SUCH_EMPLOYEE_TO_ROSTER: MessageCode = MessageCode::new("booking.no_such_employee");
pub const MAY_NOT_WORK: MessageCode = MessageCode::new("booking.may_not_work");
pub const NO_SUCH_BRANCH: MessageCode = MessageCode::new("booking.no_such_branch");
pub const NOTHING_TO_BOOK: MessageCode = MessageCode::new("booking.nothing_to_book");
pub const NO_NAME: MessageCode = MessageCode::new("booking.no_name");
pub const RESOURCE_HAS_NO_NAME: MessageCode = MessageCode::new("booking.resource_has_no_name");
pub const NO_SUCH_CUSTOMER: MessageCode = MessageCode::new("booking.no_such_customer");
pub const NO_SUCH_RESOURCE: MessageCode = MessageCode::new("booking.no_such_resource");
pub const WITHDRAWN: MessageCode = MessageCode::new("booking.withdrawn");
pub const NOT_OFFERED: MessageCode = MessageCode::new("booking.not_offered");
pub const BARRED: MessageCode = MessageCode::new("booking.barred");
pub const NO_REASON_TO_BAR: MessageCode = MessageCode::new("booking.no_reason_to_bar");
pub const NO_SUCH_RESERVATION: MessageCode = MessageCode::new("booking.no_such_reservation");
pub const SECURED: MessageCode = MessageCode::new("booking.secured");
pub const NOT_LAPSED: MessageCode = MessageCode::new("booking.not_lapsed");
pub const OVER: MessageCode = MessageCode::new("booking.over");
pub const CANNOT_MOVE: MessageCode = MessageCode::new("booking.cannot_move");
pub const NO_SUCH_LINE: MessageCode = MessageCode::new("booking.no_such_line");
pub const RESERVED_NAME: MessageCode = MessageCode::new("booking.reserved_name");
pub const INVALID_REFERENCE: MessageCode = MessageCode::new("booking.invalid_reference");
pub const UNKNOWN_STAGE: MessageCode = MessageCode::new("booking.unknown_stage");
pub const UNKNOWN_KIND: MessageCode = MessageCode::new("booking.unknown_kind");
pub const NO_SUCH_TRADE: MessageCode = MessageCode::new("booking.no_such_trade");
pub const NOT_A_RATE: MessageCode = MessageCode::new("booking.not_a_rate");
pub const NOT_A_FRACTION: MessageCode = MessageCode::new("booking.not_a_fraction");
pub const NOTHING_TO_BILL: MessageCode = MessageCode::new("booking.nothing_to_bill");
pub const NOTHING_CHARGED: MessageCode = MessageCode::new("booking.nothing_charged");
pub const NOT_AN_ALLOWANCE: MessageCode = MessageCode::new("booking.not_an_allowance");
pub const ALLOWANCE_TOO_LARGE: MessageCode = MessageCode::new("booking.allowance_too_large");
pub const MIXED_CURRENCIES: MessageCode = MessageCode::new("booking.mixed_currencies");
pub const AMOUNT_OUT_OF_RANGE: MessageCode = MessageCode::new("booking.amount_out_of_range");

pub const NOT_A_PHONE: MessageCode = MessageCode::new("booking.not_a_phone");
pub const CODE_TOO_SOON: MessageCode = MessageCode::new("booking.code_too_soon");
pub const CODE_NOT_VALID: MessageCode = MessageCode::new("booking.code_not_valid");

/// Which sentence a verification failure gets.
#[must_use]
pub fn code_for(error: &crate::verification::VerificationError) -> MessageCode {
    use crate::verification::VerificationError;
    match error {
        VerificationError::NotANumber(_) => NOT_A_PHONE,
        VerificationError::TooSoon => CODE_TOO_SOON,
        VerificationError::NotValid | VerificationError::Database(_) => CODE_NOT_VALID,
    }
}

pub static CODES: &[MessageCode] = &[
    NOTHING_TO_BILL,
    NOT_A_PHONE,
    CODE_TOO_SOON,
    CODE_NOT_VALID,
    NO_SUCH_BRANCH,
    NO_SUCH_EMPLOYEE_TO_ROSTER,
    MAY_NOT_WORK,
    NOTHING_TO_BOOK,
    NO_NAME,
    RESOURCE_HAS_NO_NAME,
    NO_SUCH_CUSTOMER,
    NO_SUCH_RESOURCE,
    WITHDRAWN,
    NOT_OFFERED,
    BARRED,
    NO_REASON_TO_BAR,
    NO_SUCH_RESERVATION,
    SECURED,
    NOT_LAPSED,
    OVER,
    CANNOT_MOVE,
    NO_SUCH_LINE,
    RESERVED_NAME,
    INVALID_REFERENCE,
    UNKNOWN_STAGE,
    UNKNOWN_KIND,
    NO_SUCH_TRADE,
    NOT_A_RATE,
    NOT_A_FRACTION,
    NOTHING_CHARGED,
    NOT_AN_ALLOWANCE,
    ALLOWANCE_TOO_LARGE,
    MIXED_CURRENCIES,
    AMOUNT_OUT_OF_RANGE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NOTHING_TO_BILL,
        Locale::English,
        Template::Simple("Booking {reservation} has no priced line to bill."),
    ),
    (
        NOTHING_TO_BILL,
        Locale::Arabic,
        Template::Simple("الحجز {reservation} لا يحتوي على بند مُسعَّر لإصدار فاتورة به."),
    ),
    (
        NO_SUCH_EMPLOYEE_TO_ROSTER,
        Locale::English,
        Template::Simple("There is no employee {id}."),
    ),
    (
        NO_SUCH_EMPLOYEE_TO_ROSTER,
        Locale::Arabic,
        Template::Simple("لا يوجد موظف {id}."),
    ),
    (
        MAY_NOT_WORK,
        Locale::English,
        Template::Simple(
            "{id} may not be rostered: a work document has lapsed, or they have left.",
        ),
    ),
    (
        MAY_NOT_WORK,
        Locale::Arabic,
        Template::Simple("{id} لا يمكن إسناده: انتهت صلاحية وثيقة عمل، أو لم يعد على رأس العمل."),
    ),
    (
        NO_SUCH_BRANCH,
        Locale::English,
        Template::Simple(
            "There is no open branch {branch}. A resource can only be placed at a branch that exists and is still trading.",
        ),
    ),
    (
        NO_SUCH_BRANCH,
        Locale::Arabic,
        Template::Simple("لا يوجد فرع مفتوح {branch}. المورد لا يُسنَد إلا لفرع قائم وما زال يعمل."),
    ),
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
    // **Names the resource and not the reason.** The reason is written by one
    // member of staff about a customer, and a refusal that read it back would
    // say it to whoever is standing at the desk.
    (
        BARRED,
        Locale::English,
        Template::Simple("{resource} cannot be booked for this customer."),
    ),
    (
        BARRED,
        Locale::Arabic,
        Template::Simple("لا يمكن حجز {resource} لهذا العميل."),
    ),
    (
        NO_REASON_TO_BAR,
        Locale::English,
        Template::Simple("Say why, so whoever has to refuse a booking can explain it."),
    ),
    (
        NO_REASON_TO_BAR,
        Locale::Arabic,
        Template::Simple("اذكر السبب، ليتمكن من يرفض الحجز من توضيحه."),
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
        SECURED,
        Locale::English,
        Template::Simple("Booking {reservation} has been paid for and cannot lapse."),
    ),
    (
        SECURED,
        Locale::Arabic,
        Template::Simple("الحجز {reservation} مدفوع ولا يمكن إسقاطه."),
    ),
    (
        NOT_LAPSED,
        Locale::English,
        Template::Simple("Booking {reservation} is not an unpaid hold past its deadline."),
    ),
    (
        NOT_LAPSED,
        Locale::Arabic,
        Template::Simple("الحجز {reservation} ليس حجزًا غير مدفوع تجاوز موعده."),
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
        NOT_A_FRACTION,
        Locale::English,
        Template::Simple(
            "A deposit is a fraction of the booking: at most 10000 basis points, not {deposit_bp}.",
        ),
    ),
    (
        NOT_A_FRACTION,
        Locale::Arabic,
        Template::Simple(
            "العربون جزء من قيمة الحجز: 10000 نقطة أساس على الأكثر، وليس {deposit_bp}.",
        ),
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
        NOT_A_PHONE,
        Locale::English,
        Template::Simple("That is not a phone number we can send a code to."),
    ),
    (
        NOT_A_PHONE,
        Locale::Arabic,
        Template::Simple("هذا ليس رقم جوال يمكن إرسال رمز إليه."),
    ),
    (
        CODE_TOO_SOON,
        Locale::English,
        Template::Simple("A code was sent a moment ago. Wait before asking for another."),
    ),
    (
        CODE_TOO_SOON,
        Locale::Arabic,
        Template::Simple("تم إرسال رمز قبل قليل. ي\u{64f}رجى الانتظار قبل طلب رمز آخر."),
    ),
    // **One sentence for every way a code can fail.** Wrong, expired, used,
    // never issued — telling them apart tells somebody guessing which half of
    // the pair they got right.
    (
        CODE_NOT_VALID,
        Locale::English,
        Template::Simple("That code is not valid. Ask for a new one."),
    ),
    (
        CODE_NOT_VALID,
        Locale::Arabic,
        Template::Simple("الرمز غير صالح. ي\u{64f}رجى طلب رمز جديد."),
    ),
];
