//! The sales module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NOTHING_TO_INVOICE: MessageCode = MessageCode::new("sales.nothing_to_invoice");
pub const NOT_ISSUED: MessageCode = MessageCode::new("sales.not_issued");
pub const OVERPAYMENT: MessageCode = MessageCode::new("sales.overpayment");
pub const PAYMENT_CURRENCY: MessageCode = MessageCode::new("sales.payment_currency");
pub const NOT_A_PAYMENT: MessageCode = MessageCode::new("sales.not_a_payment");
pub const ALREADY_CANCELLED: MessageCode = MessageCode::new("sales.already_cancelled");
pub const HAS_PAYMENTS: MessageCode = MessageCode::new("sales.has_payments");
pub const OVERREFUND: MessageCode = MessageCode::new("sales.overrefund");
pub const INVALID_REFERENCE: MessageCode = MessageCode::new("sales.invalid_reference");
pub const MIXED_CURRENCIES: MessageCode = MessageCode::new("sales.mixed_currencies");
pub const NOT_A_DISCOUNT: MessageCode = MessageCode::new("sales.not_a_discount");
pub const DISCOUNT_WITHOUT_A_BAND: MessageCode = MessageCode::new("sales.discount_without_a_band");
pub const DISCOUNT_TOO_LARGE: MessageCode = MessageCode::new("sales.discount_too_large");
pub const AMOUNT_OUT_OF_RANGE: MessageCode = MessageCode::new("sales.amount_out_of_range");
/// The invoice named a customer record that is not there, or is archived.
pub const NO_SUCH_CUSTOMER: MessageCode = MessageCode::new("sales.no_such_customer");

pub static CODES: &[MessageCode] = &[
    NOTHING_TO_INVOICE,
    NOT_ISSUED,
    OVERPAYMENT,
    PAYMENT_CURRENCY,
    NOT_A_PAYMENT,
    ALREADY_CANCELLED,
    HAS_PAYMENTS,
    OVERREFUND,
    INVALID_REFERENCE,
    MIXED_CURRENCIES,
    NOT_A_DISCOUNT,
    DISCOUNT_WITHOUT_A_BAND,
    DISCOUNT_TOO_LARGE,
    AMOUNT_OUT_OF_RANGE,
    NO_SUCH_CUSTOMER,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        OVERREFUND,
        Locale::English,
        Template::Simple(
            "The business is holding only {held} against this invoice and the refund is {offered}. Handing back more than was taken is a decision somebody has to make, not a negative balance.",
        ),
    ),
    (
        OVERREFUND,
        Locale::Arabic,
        Template::Simple(
            "المحتفظ به مقابل هذه الفاتورة {held} والمبلغ المسترد {offered}. إعادة أكثر مما استُلم قرار يتخذه شخص، لا رصيد سالب.",
        ),
    ),
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
        NOT_A_DISCOUNT,
        Locale::English,
        Template::Simple(
            "A discount is the amount taken off, so it is positive. A negative one is a charge.",
        ),
    ),
    (
        NOT_A_DISCOUNT,
        Locale::Arabic,
        Template::Simple(
            "الخصم هو المبلغ المحسوم، لذا يكون موجبًا. القيمة السالبة تُعد رسمًا إضافيًا.",
        ),
    ),
    (
        DISCOUNT_WITHOUT_A_BAND,
        Locale::English,
        Template::Simple(
            "Nothing on this invoice is taxed the way that discount is. Discounting at a rate the invoice does not charge would reclaim tax that was never charged.",
        ),
    ),
    (
        DISCOUNT_WITHOUT_A_BAND,
        Locale::Arabic,
        Template::Simple(
            "لا يوجد بند في هذه الفاتورة بنفس المعاملة الضريبية للخصم. الخصم بمعاملة لا تتضمنها الفاتورة يسترد ضريبة لم تُحتسب أصلًا.",
        ),
    ),
    (
        DISCOUNT_TOO_LARGE,
        Locale::English,
        Template::Simple("A discount cannot be larger than what it is taken off."),
    ),
    (
        DISCOUNT_TOO_LARGE,
        Locale::Arabic,
        Template::Simple("لا يمكن أن يتجاوز الخصم قيمة ما يُخصم منه."),
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
    (
        ALREADY_CANCELLED,
        Locale::English,
        Template::Simple("That invoice was already cancelled by credit note {by}."),
    ),
    (
        ALREADY_CANCELLED,
        Locale::Arabic,
        Template::Simple("تم إلغاء هذه الفاتورة بالفعل بإشعار دائن {by}."),
    ),
    (
        HAS_PAYMENTS,
        Locale::English,
        Template::Simple(
            "Invoice {invoice} has payments against it. Refund them before crediting it.",
        ),
    ),
    (
        HAS_PAYMENTS,
        Locale::Arabic,
        Template::Simple("توجد دفعات على الفاتورة {invoice}. أعِد المبالغ قبل إصدار إشعار دائن."),
    ),
    (
        NO_SUCH_CUSTOMER,
        Locale::English,
        Template::Simple(
            "There is no customer {customer} to issue this to. Record them first, or leave the customer reference out.",
        ),
    ),
    (
        NO_SUCH_CUSTOMER,
        Locale::Arabic,
        Template::Simple(
            "لا يوجد عميل {customer} لإصدار الفاتورة له. سجّله أولًا أو اترك مرجع العميل فارغًا.",
        ),
    ),
];
