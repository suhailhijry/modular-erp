//! This module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NO_SUCH_EMPLOYEE: MessageCode = MessageCode::new("hr_sa.no_such_employee");
pub const NO_SALARY: MessageCode = MessageCode::new("hr_sa.no_salary");
pub const NOT_LEFT: MessageCode = MessageCode::new("hr_sa.not_left");
pub const UNKNOWN_FOOTING: MessageCode = MessageCode::new("hr_sa.unknown_footing");
pub const UNKNOWN_LEAVING: MessageCode = MessageCode::new("hr_sa.unknown_leaving");
pub const AMOUNT_OUT_OF_RANGE: MessageCode = MessageCode::new("hr_sa.amount_out_of_range");
pub const DATABASE: MessageCode = MessageCode::new("hr_sa.database");

pub const CODES: &[MessageCode] = &[
    NO_SUCH_EMPLOYEE,
    NO_SALARY,
    NOT_LEFT,
    UNKNOWN_FOOTING,
    UNKNOWN_LEAVING,
    AMOUNT_OUT_OF_RANGE,
    DATABASE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NO_SUCH_EMPLOYEE,
        Locale::English,
        Template::Simple("There is no employee {id}."),
    ),
    (
        NO_SUCH_EMPLOYEE,
        Locale::Arabic,
        Template::Simple("لا يوجد موظف {id}."),
    ),
    (
        NO_SALARY,
        Locale::English,
        Template::Simple("No salary is recorded for {id}."),
    ),
    (
        NO_SALARY,
        Locale::Arabic,
        Template::Simple("لا يوجد راتب مسجَّل لـ {id}."),
    ),
    (
        NOT_LEFT,
        Locale::English,
        Template::Simple("{id} is still employed, so there is no end of service to compute."),
    ),
    (
        NOT_LEFT,
        Locale::Arabic,
        Template::Simple("{id} ما زال على رأس العمل، فلا توجد نهاية خدمة لاحتسابها."),
    ),
    (
        UNKNOWN_FOOTING,
        Locale::English,
        Template::Simple("{footing} is not a GOSI footing. Use saudi or non_saudi."),
    ),
    (
        UNKNOWN_FOOTING,
        Locale::Arabic,
        Template::Simple("{footing} ليس تصنيفًا في التأمينات. استخدم saudi أو non_saudi."),
    ),
    (
        UNKNOWN_LEAVING,
        Locale::English,
        Template::Simple(
            "{reason} is not a reason for leaving. Use dismissed, resigned, in_full or for_cause.",
        ),
    ),
    (
        UNKNOWN_LEAVING,
        Locale::Arabic,
        Template::Simple(
            "{reason} ليس سببًا لانتهاء الخدمة. استخدم dismissed أو resigned أو in_full أو for_cause.",
        ),
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
        Template::Simple("That could not be read. Try again."),
    ),
    (
        DATABASE,
        Locale::Arabic,
        Template::Simple("تعذّرت القراءة. أعد المحاولة."),
    ),
];
