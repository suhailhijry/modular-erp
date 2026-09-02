//! This module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NO_SUCH_RUN: MessageCode = MessageCode::new("payroll.no_such_run");
pub const APPROVED: MessageCode = MessageCode::new("payroll.approved");
pub const NOBODY_TO_PAY: MessageCode = MessageCode::new("payroll.nobody_to_pay");
pub const NOT_PAYABLE: MessageCode = MessageCode::new("payroll.not_payable");
pub const NOT_A_PERIOD: MessageCode = MessageCode::new("payroll.not_a_period");
pub const AMOUNT_OUT_OF_RANGE: MessageCode = MessageCode::new("payroll.amount_out_of_range");
pub const DATABASE: MessageCode = MessageCode::new("payroll.database");

pub const CODES: &[MessageCode] = &[
    NO_SUCH_RUN,
    APPROVED,
    NOBODY_TO_PAY,
    NOT_PAYABLE,
    NOT_A_PERIOD,
    AMOUNT_OUT_OF_RANGE,
    DATABASE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NO_SUCH_RUN,
        Locale::English,
        Template::Simple("There is no payroll run {id}."),
    ),
    (
        NO_SUCH_RUN,
        Locale::Arabic,
        Template::Simple("لا توجد مسيرة رواتب {id}."),
    ),
    (
        APPROVED,
        Locale::English,
        Template::Simple("Payroll run {id} has been approved and cannot be changed."),
    ),
    (
        APPROVED,
        Locale::Arabic,
        Template::Simple("تم اعتماد مسيرة الرواتب {id} ولا يمكن تعديلها."),
    ),
    (
        NOBODY_TO_PAY,
        Locale::English,
        Template::Simple("A payroll run needs somebody to pay."),
    ),
    (
        NOBODY_TO_PAY,
        Locale::Arabic,
        Template::Simple("مسيرة الرواتب تحتاج إلى من تُصرف له."),
    ),
    (
        NOT_PAYABLE,
        Locale::English,
        Template::Simple("{id} is not on the books, or has no salary recorded."),
    ),
    (
        NOT_PAYABLE,
        Locale::Arabic,
        Template::Simple("{id} ليس على رأس العمل، أو لا يوجد راتب مسجَّل له."),
    ),
    (
        NOT_A_PERIOD,
        Locale::English,
        Template::Simple("{period} is not a month. Use YYYY-MM."),
    ),
    (
        NOT_A_PERIOD,
        Locale::Arabic,
        Template::Simple("{period} ليس شهرًا. استخدم صيغة YYYY-MM."),
    ),
    (
        AMOUNT_OUT_OF_RANGE,
        Locale::English,
        Template::Simple("That amount is outside the range this system can hold."),
    ),
    (
        AMOUNT_OUT_OF_RANGE,
        Locale::Arabic,
        Template::Simple("المبلغ خارج النطاق الذي يستطيع النظام تمثيله."),
    ),
    (
        DATABASE,
        Locale::English,
        Template::Simple("Payroll could not be read. Try again."),
    ),
    (
        DATABASE,
        Locale::Arabic,
        Template::Simple("تعذّرت قراءة الرواتب. أعد المحاولة."),
    ),
];
