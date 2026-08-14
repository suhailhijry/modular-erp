//! The sales module's messages, in every supported language.

use spa_i18n::{Locale, MessageCode, Template};

pub const NOTHING_TO_INVOICE: MessageCode = MessageCode::new("sales.nothing_to_invoice");
pub const NOT_ISSUED: MessageCode = MessageCode::new("sales.not_issued");
pub const OVERPAYMENT: MessageCode = MessageCode::new("sales.overpayment");
pub const PAYMENT_CURRENCY: MessageCode = MessageCode::new("sales.payment_currency");
pub const NOT_A_PAYMENT: MessageCode = MessageCode::new("sales.not_a_payment");
pub const INVALID_REFERENCE: MessageCode = MessageCode::new("sales.invalid_reference");
pub const MIXED_CURRENCIES: MessageCode = MessageCode::new("sales.mixed_currencies");
pub const AMOUNT_OUT_OF_RANGE: MessageCode = MessageCode::new("sales.amount_out_of_range");

pub static CODES: &[MessageCode] = &[
    NOTHING_TO_INVOICE,
    NOT_ISSUED,
    OVERPAYMENT,
    PAYMENT_CURRENCY,
    NOT_A_PAYMENT,
    INVALID_REFERENCE,
    MIXED_CURRENCIES,
    AMOUNT_OUT_OF_RANGE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NOTHING_TO_INVOICE,
        Locale::English,
        Template::Simple("An invoice needs at least one line that comes to something."),
    ),
    (
        NOTHING_TO_INVOICE,
        Locale::Arabic,
        Template::Simple("تحتاج الفاتورة إلى سطر واحد على الأقل بقيمة غير صفرية."),
    ),
    (
        NOT_ISSUED,
        Locale::English,
        Template::Simple("Invoice {invoice} has not been issued."),
    ),
    (
        NOT_ISSUED,
        Locale::Arabic,
        Template::Simple("لم تُصدَر الفاتورة {invoice}."),
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
        Template::Simple("This invoice is in {expected}, but the payment is in {found}."),
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
    (
        MIXED_CURRENCIES,
        Locale::English,
        Template::Simple("Every line of an invoice must be in the same currency."),
    ),
    (
        MIXED_CURRENCIES,
        Locale::Arabic,
        Template::Simple("يجب أن تكون جميع سطور الفاتورة بالعملة نفسها."),
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
];
