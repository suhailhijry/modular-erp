//! This module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NO_NAME: MessageCode = MessageCode::new("hr.no_name");
pub const NO_CONTACT: MessageCode = MessageCode::new("hr.no_contact");
pub const NO_SUCH_EMPLOYEE: MessageCode = MessageCode::new("hr.no_such_employee");
pub const NO_SUCH_MANAGER: MessageCode = MessageCode::new("hr.no_such_manager");
pub const NO_SUCH_BRANCH: MessageCode = MessageCode::new("hr.no_such_branch");
pub const LEFT: MessageCode = MessageCode::new("hr.left");
pub const CYCLE: MessageCode = MessageCode::new("hr.cycle");
pub const DATABASE: MessageCode = MessageCode::new("hr.database");
pub const NOT_A_CLAIM: MessageCode = MessageCode::new("hr.not_a_claim");
pub const NO_DOCUMENT_NUMBER: MessageCode = MessageCode::new("hr.no_document_number");
pub const UNKNOWN_DOCUMENT: MessageCode = MessageCode::new("hr.unknown_document");
pub const NOT_A_SALARY: MessageCode = MessageCode::new("hr.not_a_salary");
pub const DEDUCTIONS_EXCEED_PAY: MessageCode = MessageCode::new("hr.deductions_exceed_pay");

pub const CODES: &[MessageCode] = &[
    NO_NAME,
    NO_CONTACT,
    NO_SUCH_EMPLOYEE,
    NO_SUCH_MANAGER,
    NO_SUCH_BRANCH,
    LEFT,
    CYCLE,
    DATABASE,
    NOT_A_CLAIM,
    NO_DOCUMENT_NUMBER,
    UNKNOWN_DOCUMENT,
    NOT_A_SALARY,
    DEDUCTIONS_EXCEED_PAY,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NO_NAME,
        Locale::English,
        Template::Simple("An employee needs a name."),
    ),
    (
        NO_NAME,
        Locale::Arabic,
        Template::Simple("الموظف يحتاج إلى اسم."),
    ),
    (
        NO_CONTACT,
        Locale::English,
        Template::Simple("An employee needs a phone number or an email address."),
    ),
    (
        NO_CONTACT,
        Locale::Arabic,
        Template::Simple("الموظف يحتاج إلى رقم هاتف أو بريد إلكتروني."),
    ),
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
        NO_SUCH_MANAGER,
        Locale::English,
        Template::Simple("There is no employee {id} to report to."),
    ),
    (
        NO_SUCH_MANAGER,
        Locale::Arabic,
        Template::Simple("لا يوجد موظف {id} ليكون مسؤولًا."),
    ),
    (
        NO_SUCH_BRANCH,
        Locale::English,
        Template::Simple("There is no open branch {branch}."),
    ),
    (
        NO_SUCH_BRANCH,
        Locale::Arabic,
        Template::Simple("لا يوجد فرع مفتوح {branch}."),
    ),
    (
        LEFT,
        Locale::English,
        Template::Simple("Employee {id} has left."),
    ),
    (
        LEFT,
        Locale::Arabic,
        Template::Simple("الموظف {id} لم يعد على رأس العمل."),
    ),
    (
        CYCLE,
        Locale::English,
        Template::Simple("{id} cannot report to somebody in their own team."),
    ),
    (
        CYCLE,
        Locale::Arabic,
        Template::Simple("{id} لا يمكن أن يكون تابعًا لأحد أفراد فريقه."),
    ),
    (
        DATABASE,
        Locale::English,
        Template::Simple("The org chart could not be read. Try again."),
    ),
    (
        DATABASE,
        Locale::Arabic,
        Template::Simple("تعذّرت قراءة الهيكل التنظيمي. أعد المحاولة."),
    ),
    (
        NO_DOCUMENT_NUMBER,
        Locale::English,
        Template::Simple("A document needs its number."),
    ),
    (
        NO_DOCUMENT_NUMBER,
        Locale::Arabic,
        Template::Simple("الوثيقة تحتاج إلى رقمها."),
    ),
    (
        NOT_A_SALARY,
        Locale::English,
        Template::Simple("A salary needs positive basic pay, and every part in one currency."),
    ),
    (
        NOT_A_SALARY,
        Locale::Arabic,
        Template::Simple("الراتب يحتاج إلى أساسي موجب، وكل بند بعملة واحدة."),
    ),
    (
        DEDUCTIONS_EXCEED_PAY,
        Locale::English,
        Template::Simple("What is taken off comes to more than what is paid."),
    ),
    (
        DEDUCTIONS_EXCEED_PAY,
        Locale::Arabic,
        Template::Simple("مجموع الاستقطاعات يتجاوز المستحق."),
    ),
    (
        UNKNOWN_DOCUMENT,
        Locale::English,
        Template::Simple("{kind} is not a kind of document this system tracks."),
    ),
    (
        UNKNOWN_DOCUMENT,
        Locale::Arabic,
        Template::Simple("{kind} ليس نوع وثيقة يتتبعه هذا النظام."),
    ),
    (
        NOT_A_CLAIM,
        Locale::English,
        Template::Simple("{claim} is not usable as a permission name."),
    ),
    (
        NOT_A_CLAIM,
        Locale::Arabic,
        Template::Simple("{claim} غير صالح كاسم صلاحية."),
    ),
];
