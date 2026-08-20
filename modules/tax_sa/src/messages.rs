//! The Saudi tax module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const EMPTY_PERIOD: MessageCode = MessageCode::new("tax_sa.empty_period");
pub const ALREADY_FILED: MessageCode = MessageCode::new("tax_sa.already_filed");
pub const INVALID_PERIOD: MessageCode = MessageCode::new("tax_sa.invalid_period");
pub const INVALID_REGISTRATION: MessageCode = MessageCode::new("tax_sa.invalid_registration");
pub const INVALID_DOCUMENT: MessageCode = MessageCode::new("tax_sa.invalid_document");
pub const NOT_REGISTERED: MessageCode = MessageCode::new("tax_sa.not_registered");
pub const NO_SUCH_DOCUMENT: MessageCode = MessageCode::new("tax_sa.no_such_document");

pub static CODES: &[MessageCode] = &[
    EMPTY_PERIOD,
    ALREADY_FILED,
    INVALID_PERIOD,
    INVALID_REGISTRATION,
    INVALID_DOCUMENT,
    NOT_REGISTERED,
    NO_SUCH_DOCUMENT,
];

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
    (
        INVALID_REGISTRATION,
        Locale::English,
        Template::Simple(
            "That ZATCA registration cannot be used: {reason}. It is checked here because a standard invoice cannot be given to a buyer until ZATCA has cleared it.",
        ),
    ),
    (
        INVALID_REGISTRATION,
        Locale::Arabic,
        Template::Simple(
            "لا يمكن استخدام بيانات التسجيل لدى هيئة الزكاة والضريبة والجمارك: {reason}. تُفحص هنا لأن الفاتورة الضريبية لا تُسلَّم للمشتري قبل اعتمادها.",
        ),
    ),
    (
        INVALID_DOCUMENT,
        Locale::English,
        Template::Simple("{document} cannot be used as a document identifier."),
    ),
    (
        INVALID_DOCUMENT,
        Locale::Arabic,
        Template::Simple("لا يمكن استخدام {document} كمعرّف مستند."),
    ),
    (
        NOT_REGISTERED,
        Locale::English,
        Template::Simple(
            "This business has no ZATCA registration yet, so no invoice can be cleared or reported. Register one first.",
        ),
    ),
    (
        NOT_REGISTERED,
        Locale::Arabic,
        Template::Simple(
            "لا يوجد تسجيل لدى هيئة الزكاة والضريبة والجمارك لهذه المنشأة بعد، فلا يمكن اعتماد أي فاتورة أو الإبلاغ عنها. سجِّل البيانات أولًا.",
        ),
    ),
    (
        NO_SUCH_DOCUMENT,
        Locale::English,
        Template::Simple("There is no ZATCA document numbered {document}."),
    ),
    (
        NO_SUCH_DOCUMENT,
        Locale::Arabic,
        Template::Simple("لا يوجد مستند برقم {document}."),
    ),
];
