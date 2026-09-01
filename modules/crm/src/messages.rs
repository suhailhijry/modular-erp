//! The crm module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NO_NAME: MessageCode = MessageCode::new("crm.no_name");
pub const NAME_TOO_LONG: MessageCode = MessageCode::new("crm.name_too_long");
pub const NO_CONTACT: MessageCode = MessageCode::new("crm.no_contact");
pub const NO_SUCH_CUSTOMER: MessageCode = MessageCode::new("crm.no_such_customer");
pub const ARCHIVED: MessageCode = MessageCode::new("crm.archived");
pub const NOT_A_VAT_NUMBER: MessageCode = MessageCode::new("crm.not_a_vat_number");
pub const PERSON_WITH_VAT_NUMBER: MessageCode = MessageCode::new("crm.person_with_vat_number");
pub const UNKNOWN_KIND: MessageCode = MessageCode::new("crm.unknown_kind");

pub static CODES: &[MessageCode] = &[
    NO_NAME,
    NAME_TOO_LONG,
    NO_CONTACT,
    NO_SUCH_CUSTOMER,
    ARCHIVED,
    NOT_A_VAT_NUMBER,
    PERSON_WITH_VAT_NUMBER,
    UNKNOWN_KIND,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NO_NAME,
        Locale::English,
        Template::Simple("A customer needs a name."),
    ),
    (
        NO_NAME,
        Locale::Arabic,
        Template::Simple("يحتاج العميل إلى اسم."),
    ),
    (
        NAME_TOO_LONG,
        Locale::English,
        Template::Plural {
            zero: None,
            one: Some("A name may not be longer than one character."),
            two: None,
            few: None,
            many: None,
            other: "A name may not be longer than {n} characters.",
        },
    ),
    (
        NAME_TOO_LONG,
        Locale::Arabic,
        Template::Plural {
            zero: Some("لا يمكن أن يتجاوز الاسم أي حرف."),
            one: Some("لا يمكن أن يتجاوز الاسم حرفًا واحدًا."),
            two: Some("لا يمكن أن يتجاوز الاسم حرفين."),
            few: Some("لا يمكن أن يتجاوز الاسم {n} أحرف."),
            many: Some("لا يمكن أن يتجاوز الاسم {n} حرفًا."),
            other: "لا يمكن أن يتجاوز الاسم {n} حرف.",
        },
    ),
    (
        NO_CONTACT,
        Locale::English,
        Template::Simple("A customer needs a phone number or an email address."),
    ),
    (
        NO_CONTACT,
        Locale::Arabic,
        Template::Simple("يحتاج العميل إلى رقم جوال أو بريد إلكتروني."),
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
        ARCHIVED,
        Locale::English,
        Template::Simple("Customer {customer} is archived. Restore them first."),
    ),
    (
        ARCHIVED,
        Locale::Arabic,
        Template::Simple("العميل {customer} مؤرشف. استعده أولًا."),
    ),
    (
        NOT_A_VAT_NUMBER,
        Locale::English,
        Template::Simple(
            "{value} is not a Saudi VAT number. It is fifteen digits beginning and ending with 3.",
        ),
    ),
    (
        NOT_A_VAT_NUMBER,
        Locale::Arabic,
        Template::Simple(
            "{value} ليس رقم تسجيل ضريبي سعودي. يتكون من خمسة عشر رقمًا يبدأ وينتهي بالرقم ٣.",
        ),
    ),
    (
        PERSON_WITH_VAT_NUMBER,
        Locale::English,
        Template::Simple("A person does not hold a VAT registration. Record them as a company."),
    ),
    (
        PERSON_WITH_VAT_NUMBER,
        Locale::Arabic,
        Template::Simple("الفرد لا يملك تسجيلًا ضريبيًا. سجّله كمنشأة."),
    ),
    (
        UNKNOWN_KIND,
        Locale::English,
        Template::Simple("A customer is a person or a company."),
    ),
    (
        UNKNOWN_KIND,
        Locale::Arabic,
        Template::Simple("العميل إما فرد أو منشأة."),
    ),
];
