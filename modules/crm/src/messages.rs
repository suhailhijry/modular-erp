//! The crm module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NO_NAME: MessageCode = MessageCode::new("crm.no_name");
pub const NAME_TOO_LONG: MessageCode = MessageCode::new("crm.name_too_long");
pub const NO_CONTACT: MessageCode = MessageCode::new("crm.no_contact");
pub const NO_SUCH_CUSTOMER: MessageCode = MessageCode::new("crm.no_such_customer");
pub const ARCHIVED: MessageCode = MessageCode::new("crm.archived");
pub const NOT_A_VAT_NUMBER: MessageCode = MessageCode::new("crm.not_a_vat_number");
pub const PERSON_WITH_VAT_NUMBER: MessageCode = MessageCode::new("crm.person_with_vat_number");
pub const UNREADABLE_FILE: MessageCode = MessageCode::new("crm.unreadable_file");
pub const NO_ID_COLUMN: MessageCode = MessageCode::new("crm.no_id_column");
pub const UNKNOWN_KIND: MessageCode = MessageCode::new("crm.unknown_kind");

pub const TOO_MANY_FIELDS: MessageCode = MessageCode::new("crm.too_many_fields");
pub const NOT_A_FIELD_KEY: MessageCode = MessageCode::new("crm.not_a_field_key");
pub const FIELD_DECLARED_TWICE: MessageCode = MessageCode::new("crm.field_declared_twice");
pub const FIELD_NEEDS_A_LABEL: MessageCode = MessageCode::new("crm.field_needs_a_label");
pub const FIELD_NEEDS_A_LENGTH: MessageCode = MessageCode::new("crm.field_needs_a_length");
pub const FIELD_NEEDS_OPTIONS: MessageCode = MessageCode::new("crm.field_needs_options");
pub const NO_SUCH_FIELD: MessageCode = MessageCode::new("crm.no_such_field");
pub const WRONG_KIND_OF_VALUE: MessageCode = MessageCode::new("crm.wrong_kind_of_value");
pub const NOT_ONE_OF_THE_OPTIONS: MessageCode = MessageCode::new("crm.not_one_of_the_options");
pub const VALUE_TOO_LONG: MessageCode = MessageCode::new("crm.value_too_long");
pub const FIELD_IS_REQUIRED: MessageCode = MessageCode::new("crm.field_is_required");

/// Which sentence a field refusal gets.
#[must_use]
pub fn field_code(error: &crate::fields::FieldError) -> MessageCode {
    use crate::fields::FieldError;
    match error {
        FieldError::TooManyFields => TOO_MANY_FIELDS,
        FieldError::NotAKey(_) => NOT_A_FIELD_KEY,
        FieldError::DuplicateKey(_) => FIELD_DECLARED_TWICE,
        FieldError::NoLabel(_) => FIELD_NEEDS_A_LABEL,
        FieldError::NotALength(_) => FIELD_NEEDS_A_LENGTH,
        FieldError::NoOptions(_) | FieldError::NotAnOption(_) => FIELD_NEEDS_OPTIONS,
        FieldError::NoSuchField(_) => NO_SUCH_FIELD,
        FieldError::WrongKind { .. } => WRONG_KIND_OF_VALUE,
        FieldError::NotOneOfTheOptions { .. } => NOT_ONE_OF_THE_OPTIONS,
        FieldError::TooLong(_) => VALUE_TOO_LONG,
        FieldError::Required(_) => FIELD_IS_REQUIRED,
    }
}

pub static CODES: &[MessageCode] = &[
    TOO_MANY_FIELDS,
    NOT_A_FIELD_KEY,
    FIELD_DECLARED_TWICE,
    FIELD_NEEDS_A_LABEL,
    FIELD_NEEDS_A_LENGTH,
    FIELD_NEEDS_OPTIONS,
    NO_SUCH_FIELD,
    WRONG_KIND_OF_VALUE,
    NOT_ONE_OF_THE_OPTIONS,
    VALUE_TOO_LONG,
    FIELD_IS_REQUIRED,
    NO_NAME,
    NAME_TOO_LONG,
    NO_CONTACT,
    NO_SUCH_CUSTOMER,
    ARCHIVED,
    NOT_A_VAT_NUMBER,
    PERSON_WITH_VAT_NUMBER,
    UNREADABLE_FILE,
    NO_ID_COLUMN,
    UNKNOWN_KIND,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        UNREADABLE_FILE,
        Locale::English,
        Template::Simple("That is not a spreadsheet this system can read: {reason}"),
    ),
    (
        UNREADABLE_FILE,
        Locale::Arabic,
        Template::Simple("هذا ليس جدولًا يستطيع النظام قراءته: {reason}"),
    ),
    (
        NO_ID_COLUMN,
        Locale::English,
        Template::Simple("This row has no id, and a customer is imported under one."),
    ),
    (
        NO_ID_COLUMN,
        Locale::Arabic,
        Template::Simple("لا يوجد معرّف في هذا الصف، والعميل يُستورد تحت معرّف."),
    ),
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
    (
        TOO_MANY_FIELDS,
        Locale::English,
        Template::Simple("That is more fields than a customer can carry."),
    ),
    (
        TOO_MANY_FIELDS,
        Locale::Arabic,
        Template::Simple("هذا أكثر من عدد الحقول التي يمكن أن يحملها العميل."),
    ),
    (
        NOT_A_FIELD_KEY,
        Locale::English,
        Template::Simple(
            "{field} cannot be used as a field name. Use lowercase letters, digits and underscores.",
        ),
    ),
    (
        NOT_A_FIELD_KEY,
        Locale::Arabic,
        Template::Simple(
            "لا يمكن استخدام {field} كاسم حقل. است\u{62e}دم أحرفا\u{64b} صغيرة وأرقاما\u{64b} وشرطات سفلية.",
        ),
    ),
    (
        FIELD_DECLARED_TWICE,
        Locale::English,
        Template::Simple("The field {field} is listed twice."),
    ),
    (
        FIELD_DECLARED_TWICE,
        Locale::Arabic,
        Template::Simple("الحقل {field} مذكور مرتين."),
    ),
    (
        FIELD_NEEDS_A_LABEL,
        Locale::English,
        Template::Simple("The field {field} needs a label somebody can read."),
    ),
    (
        FIELD_NEEDS_A_LABEL,
        Locale::Arabic,
        Template::Simple("الحقل {field} يحتاج إلى تسمية يقرؤها الناس."),
    ),
    (
        FIELD_NEEDS_A_LENGTH,
        Locale::English,
        Template::Simple("The field {field} needs a usable maximum length."),
    ),
    (
        FIELD_NEEDS_A_LENGTH,
        Locale::Arabic,
        Template::Simple("الحقل {field} يحتاج إلى حد أقصى صالح للطول."),
    ),
    (
        FIELD_NEEDS_OPTIONS,
        Locale::English,
        Template::Simple(
            "The field {field} is a choice and needs options that are not blank or repeated.",
        ),
    ),
    (
        FIELD_NEEDS_OPTIONS,
        Locale::Arabic,
        Template::Simple("الحقل {field} حقل اختيار ويحتاج إلى خيارات غير فارغة وغير مكررة."),
    ),
    (
        NO_SUCH_FIELD,
        Locale::English,
        Template::Simple("There is no field {field}."),
    ),
    (
        NO_SUCH_FIELD,
        Locale::Arabic,
        Template::Simple("لا يوجد حقل {field}."),
    ),
    (
        WRONG_KIND_OF_VALUE,
        Locale::English,
        Template::Simple("That is not the kind of value the field {field} holds."),
    ),
    (
        WRONG_KIND_OF_VALUE,
        Locale::Arabic,
        Template::Simple("هذه ليست نوع القيمة التي يحملها الحقل {field}."),
    ),
    (
        NOT_ONE_OF_THE_OPTIONS,
        Locale::English,
        Template::Simple("That is not one of the options for {field}."),
    ),
    (
        NOT_ONE_OF_THE_OPTIONS,
        Locale::Arabic,
        Template::Simple("هذا ليس أحد الخيارات المتاحة لـ {field}."),
    ),
    (
        VALUE_TOO_LONG,
        Locale::English,
        Template::Simple("That is longer than the field {field} allows."),
    ),
    (
        VALUE_TOO_LONG,
        Locale::Arabic,
        Template::Simple("هذا أطول مما يسمح به الحقل {field}."),
    ),
    (
        FIELD_IS_REQUIRED,
        Locale::English,
        Template::Simple("The field {field} is required, or customers still hold values for it."),
    ),
    (
        FIELD_IS_REQUIRED,
        Locale::Arabic,
        Template::Simple("الحقل {field} مطلوب، أو ما زال لدى عملاء قيم مخز\u{651}نة فيه."),
    ),
];
