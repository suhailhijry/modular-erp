//! The purchases module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NOTHING_ON_IT: MessageCode = MessageCode::new("purchases.nothing_on_it");
pub const NOT_RECORDED: MessageCode = MessageCode::new("purchases.not_recorded");
pub const OVERPAYMENT: MessageCode = MessageCode::new("purchases.overpayment");
pub const PAYMENT_CURRENCY: MessageCode = MessageCode::new("purchases.payment_currency");
pub const NOT_A_PAYMENT: MessageCode = MessageCode::new("purchases.not_a_payment");
pub const MIXED_CURRENCIES: MessageCode = MessageCode::new("purchases.mixed_currencies");
pub const TAX_ON_AN_UNTAXED_LINE: MessageCode =
    MessageCode::new("purchases.tax_on_an_untaxed_line");
pub const NEGATIVE_TAX: MessageCode = MessageCode::new("purchases.negative_tax");
pub const NO_SUPPLIER_VAT_NUMBER: MessageCode =
    MessageCode::new("purchases.no_supplier_vat_number");
pub const INVALID_REFERENCE: MessageCode = MessageCode::new("purchases.invalid_reference");

pub static CODES: &[MessageCode] = &[
    NOTHING_ON_IT,
    NOT_RECORDED,
    OVERPAYMENT,
    PAYMENT_CURRENCY,
    NOT_A_PAYMENT,
    MIXED_CURRENCIES,
    TAX_ON_AN_UNTAXED_LINE,
    NEGATIVE_TAX,
    NO_SUPPLIER_VAT_NUMBER,
    INVALID_REFERENCE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NOTHING_ON_IT,
        Locale::English,
        Template::Simple("A bill needs at least one line that comes to something."),
    ),
    (
        NOTHING_ON_IT,
        Locale::Arabic,
        Template::Simple("تحتاج الفاتورة إلى سطر واحد على الأقل بقيمة غير صفرية."),
    ),
    (
        NOT_RECORDED,
        Locale::English,
        Template::Simple("Bill {bill} has not been recorded."),
    ),
    (
        NOT_RECORDED,
        Locale::Arabic,
        Template::Simple("لم تُسجَّل فاتورة المورّد {bill}."),
    ),
    (
        OVERPAYMENT,
        Locale::English,
        Template::Simple("Only {outstanding} is outstanding, and the payment is {offered}."),
    ),
    (
        OVERPAYMENT,
        Locale::Arabic,
        Template::Simple("المتبقي هو {outstanding} فقط، ومبلغ الدفعة {offered}."),
    ),
    (
        PAYMENT_CURRENCY,
        Locale::English,
        Template::Simple("This bill is in {expected}, but the payment is in {found}."),
    ),
    (
        PAYMENT_CURRENCY,
        Locale::Arabic,
        Template::Simple("هذه الفاتورة بعملة {expected}، لكن الدفعة بعملة {found}."),
    ),
    (
        NOT_A_PAYMENT,
        Locale::English,
        Template::Simple("A payment must be a positive amount."),
    ),
    (
        NOT_A_PAYMENT,
        Locale::Arabic,
        Template::Simple("يجب أن تكون قيمة الدفعة موجبة."),
    ),
    (
        MIXED_CURRENCIES,
        Locale::English,
        Template::Simple("Every line of a bill must be in the same currency."),
    ),
    (
        MIXED_CURRENCIES,
        Locale::Arabic,
        Template::Simple("يجب أن تكون جميع سطور الفاتورة بالعملة نفسها."),
    ),
    (
        TAX_ON_AN_UNTAXED_LINE,
        Locale::English,
        Template::Simple(
            "A {category} line carries no VAT, and this one is charged {tax}. \
             Check the supplier's invoice.",
        ),
    ),
    (
        TAX_ON_AN_UNTAXED_LINE,
        Locale::Arabic,
        Template::Simple(
            "لا تحمل السطور من نوع {category} ضريبة، وهذا السطر عليه {tax}. \
             راجع فاتورة المورّد.",
        ),
    ),
    (
        NEGATIVE_TAX,
        Locale::English,
        Template::Simple("VAT on a bill cannot be negative."),
    ),
    (
        NEGATIVE_TAX,
        Locale::Arabic,
        Template::Simple("لا يمكن أن تكون ضريبة القيمة المضافة على الفاتورة سالبة."),
    ),
    (
        NO_SUPPLIER_VAT_NUMBER,
        Locale::English,
        Template::Simple(
            "Input VAT can only be reclaimed against a registered supplier. \
             Add their VAT number, or record the bill without tax.",
        ),
    ),
    (
        NO_SUPPLIER_VAT_NUMBER,
        Locale::Arabic,
        Template::Simple(
            "لا يمكن استرداد ضريبة المدخلات إلا من مورّد مسجَّل. \
             أضف رقمه الضريبي، أو سجّل الفاتورة دون ضريبة.",
        ),
    ),
    (
        INVALID_REFERENCE,
        Locale::English,
        Template::Simple(
            "{reference} cannot be used as a reference. Use letters, digits, and . - _ only.",
        ),
    ),
    (
        INVALID_REFERENCE,
        Locale::Arabic,
        Template::Simple("لا يمكن استخدام {reference} كمرجع. استخدم الحروف والأرقام و. - _ فقط."),
    ),
];
