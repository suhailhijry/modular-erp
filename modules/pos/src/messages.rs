//! The pos module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NO_SUCH_SHIFT: MessageCode = MessageCode::new("pos.no_such_shift");
pub const CLOSED: MessageCode = MessageCode::new("pos.closed");
pub const NOT_A_FLOAT: MessageCode = MessageCode::new("pos.not_a_float");
pub const NOTHING_SOLD: MessageCode = MessageCode::new("pos.nothing_sold");
pub const TENDERS_DO_NOT_MATCH: MessageCode = MessageCode::new("pos.tenders_do_not_match");
pub const NOT_AN_AMOUNT: MessageCode = MessageCode::new("pos.not_an_amount");
pub const UNKNOWN_METHOD: MessageCode = MessageCode::new("pos.unknown_method");
pub const AMOUNT_OUT_OF_RANGE: MessageCode = MessageCode::new("pos.amount_out_of_range");

pub static CODES: &[MessageCode] = &[
    NO_SUCH_SHIFT,
    CLOSED,
    NOT_A_FLOAT,
    NOTHING_SOLD,
    TENDERS_DO_NOT_MATCH,
    NOT_AN_AMOUNT,
    UNKNOWN_METHOD,
    AMOUNT_OUT_OF_RANGE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NO_SUCH_SHIFT,
        Locale::English,
        Template::Simple("There is no shift {id}."),
    ),
    (
        NO_SUCH_SHIFT,
        Locale::Arabic,
        Template::Simple("لا توجد وردية {id}."),
    ),
    (
        CLOSED,
        Locale::English,
        Template::Simple("Shift {id} has been closed and cannot take any more."),
    ),
    (
        CLOSED,
        Locale::Arabic,
        Template::Simple("الوردية {id} أُغلقت ولا يمكنها استقبال المزيد."),
    ),
    (
        NOT_A_FLOAT,
        Locale::English,
        Template::Simple("An opening float cannot be negative."),
    ),
    (
        NOT_A_FLOAT,
        Locale::Arabic,
        Template::Simple("رصيد الافتتاح لا يمكن أن يكون سالبًا."),
    ),
    (
        NOTHING_SOLD,
        Locale::English,
        Template::Simple("A sale needs at least one line on it."),
    ),
    (
        NOTHING_SOLD,
        Locale::Arabic,
        Template::Simple("البيع يحتاج إلى بند واحد على الأقل."),
    ),
    (
        TENDERS_DO_NOT_MATCH,
        Locale::English,
        Template::Simple(
            "The tenders come to {tendered} and the sale is {total}. A till sale is paid in full at the counter: less would leave a balance owing, and change handed back is not recorded.",
        ),
    ),
    (
        TENDERS_DO_NOT_MATCH,
        Locale::Arabic,
        Template::Simple(
            "مجموع المدفوعات {tendered} والبيع {total}. بيع الكاشير يُسدَّد بالكامل عند الصندوق: الأقل يترك رصيدًا مستحقًا، والباقي المعاد لا يُسجَّل.",
        ),
    ),
    (
        NOT_AN_AMOUNT,
        Locale::English,
        Template::Simple("An amount here must be more than nothing."),
    ),
    (
        NOT_AN_AMOUNT,
        Locale::Arabic,
        Template::Simple("المبلغ هنا يجب أن يكون أكثر من صفر."),
    ),
    (
        UNKNOWN_METHOD,
        Locale::English,
        Template::Simple("{method} is not a way money arrives. Use cash, card or transfer."),
    ),
    (
        UNKNOWN_METHOD,
        Locale::Arabic,
        Template::Simple("{method} ليست طريقة استلام. استخدم cash أو card أو transfer."),
    ),
    (
        AMOUNT_OUT_OF_RANGE,
        Locale::English,
        Template::Simple("That amount is outside the range this system can hold."),
    ),
    (
        AMOUNT_OUT_OF_RANGE,
        Locale::Arabic,
        Template::Simple("هذا المبلغ خارج النطاق الذي يستطيع النظام حفظه."),
    ),
];
