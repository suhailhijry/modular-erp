//! The sales module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NOTHING_TO_INVOICE: MessageCode = MessageCode::new("sales.nothing_to_invoice");
pub const NOT_APPROVED: MessageCode = MessageCode::new("sales.not_approved");
pub const NO_EXEMPTION_REASON: MessageCode = MessageCode::new("sales.no_exemption_reason");
pub const NOT_ISSUED: MessageCode = MessageCode::new("sales.not_issued");
pub const OVERPAYMENT: MessageCode = MessageCode::new("sales.overpayment");
pub const PAYMENT_CURRENCY: MessageCode = MessageCode::new("sales.payment_currency");
/// A tax document in a currency the jurisdiction does not take. 422.
pub const DOCUMENT_CURRENCY: MessageCode = MessageCode::new("sales.document_currency");
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

pub const CREDIT_WITHOUT_A_BAND: MessageCode = MessageCode::new("sales.credit_without_a_band");
pub const CREDIT_TOO_LARGE: MessageCode = MessageCode::new("sales.credit_too_large");
pub const ALREADY_CREDITED: MessageCode = MessageCode::new("sales.already_credited");
pub const NOTHING_TO_CREDIT: MessageCode = MessageCode::new("sales.nothing_to_credit");
pub const NOT_A_QUANTITY: MessageCode = MessageCode::new("sales.not_a_quantity");
pub const NAMED_UNITS: MessageCode = MessageCode::new("sales.named_units");
/// A line naming a lot without naming the product the lot is of.
pub const LOT_WITHOUT_A_PRODUCT: MessageCode = MessageCode::new("sales.lot_without_a_product");
pub const PREPAID_DOES_NOT_FIT: MessageCode = MessageCode::new("sales.prepaid_does_not_fit");

pub const NO_SUCH_LINE: MessageCode = MessageCode::new("sales.no_such_line");
/// Units coming back against an invoice line that sold no product.
pub const NOT_A_STOCK_LINE: MessageCode = MessageCode::new("sales.not_a_stock_line");

/// An invoice, credit note or refund over the tenant's document limit.
pub const OVER_DOCUMENT_LIMIT: MessageCode = MessageCode::new("sales.over_document_limit");
/// One in another currency than the limit's, which cannot be compared with it.
pub const DOCUMENT_LIMIT_CURRENCY: MessageCode = MessageCode::new("sales.document_limit_currency");
/// A limit of nothing, or less.
pub const DOCUMENT_LIMIT_NOT_POSITIVE: MessageCode =
    MessageCode::new("sales.document_limit_not_positive");

pub static CODES: &[MessageCode] = &[
    NOT_APPROVED,
    DOCUMENT_CURRENCY,
    OVER_DOCUMENT_LIMIT,
    DOCUMENT_LIMIT_CURRENCY,
    DOCUMENT_LIMIT_NOT_POSITIVE,
    NO_EXEMPTION_REASON,
    PREPAID_DOES_NOT_FIT,
    NO_SUCH_LINE,
    NOT_A_STOCK_LINE,
    CREDIT_WITHOUT_A_BAND,
    CREDIT_TOO_LARGE,
    ALREADY_CREDITED,
    NOTHING_TO_CREDIT,
    NOT_A_QUANTITY,
    NAMED_UNITS,
    LOT_WITHOUT_A_PRODUCT,
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
        NOT_A_QUANTITY,
        Locale::English,
        Template::Simple(
            "A quantity is a whole number of units, and more than nothing. Leave it out for a \
             line given as a single amount.",
        ),
    ),
    (
        NOT_A_QUANTITY,
        Locale::Arabic,
        Template::Simple(
            "الكمية عدد صحيح من الوحدات وأكبر من الصفر. اتركها فارغة إذا كان السطر مبلغًا واحدًا.",
        ),
    ),
    (
        NAMED_UNITS,
        Locale::English,
        Template::Simple(
            "This line names {named} units, so it must be for the product they are units of, \
             with a quantity of {named}.",
        ),
    ),
    (
        NAMED_UNITS,
        Locale::Arabic,
        Template::Simple(
            "يسمّي هذا السطر {named} وحدة، فعليه أن يكون للصنف التابعة له وأن تكون كميته {named}.",
        ),
    ),
    (
        LOT_WITHOUT_A_PRODUCT,
        Locale::English,
        Template::Simple("This line names lot {lot}, so it must name the product the lot is of."),
    ),
    (
        LOT_WITHOUT_A_PRODUCT,
        Locale::Arabic,
        Template::Simple("يحدد هذا السطر التشغيلة {lot}، فعليه أن يحدد الصنف الذي تتبعه."),
    ),
    (
        PREPAID_DOES_NOT_FIT,
        Locale::English,
        Template::Simple("The prepayment cannot be deducted from this invoice: {why}"),
    ),
    (
        PREPAID_DOES_NOT_FIT,
        Locale::Arabic,
        Template::Simple("لا يمكن خصم الدفعة المقدمة من هذه الفاتورة: {why}"),
    ),
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
        NO_EXEMPTION_REASON,
        Locale::English,
        Template::Simple(
            "A {category} line carries no tax and must say why, and no reason is configured. Set one for this treatment at /v1/ledger/vat-rates.",
        ),
    ),
    (
        NO_EXEMPTION_REASON,
        Locale::Arabic,
        Template::Simple(
            "سطر بمعاملة {category} لا يحمل ضريبة ويجب أن يذكر السبب، ولم يُضبط أي سبب. اضبط سببًا لهذه المعاملة من /v1/ledger/vat-rates.",
        ),
    ),
    (
        NOT_APPROVED,
        Locale::English,
        Template::Simple(
            "Issuing a credit note needs the {claim} claim, and you do not hold it here. Ask somebody who does, or have it granted to you.",
        ),
    ),
    (
        NOT_APPROVED,
        Locale::Arabic,
        Template::Simple(
            "إصدار إشعار دائن يتطلب صلاحية {claim}، وهي غير ممنوحة لك هنا. اطلب من شخص يملكها أو اطلب منحها لك.",
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
    // **The refusal that protects the tax.** Crediting a treatment the invoice
    // never carried reclaims VAT that was never charged.
    (
        CREDIT_WITHOUT_A_BAND,
        Locale::English,
        Template::Simple(
            "Invoice {invoice} has nothing treated as {category}, so there is nothing to credit at that rate.",
        ),
    ),
    (
        CREDIT_WITHOUT_A_BAND,
        Locale::Arabic,
        Template::Simple(
            "الفاتورة {invoice} لا تتضمن أي بند بمعاملة {category}، فلا يوجد ما يمكن إصداره كإشعار دائن بذلك المعدل.",
        ),
    ),
    (
        CREDIT_TOO_LARGE,
        Locale::English,
        Template::Simple("{amount} is more than is left to credit."),
    ),
    (
        CREDIT_TOO_LARGE,
        Locale::Arabic,
        Template::Simple("{amount} أكبر مما تبقى لإصدار إشعار دائن به."),
    ),
    (
        ALREADY_CREDITED,
        Locale::English,
        Template::Simple("Invoice {invoice} has already been credited."),
    ),
    (
        ALREADY_CREDITED,
        Locale::Arabic,
        Template::Simple("سبق إصدار إشعار دائن للفاتورة {invoice}."),
    ),
    (
        NOTHING_TO_CREDIT,
        Locale::English,
        Template::Simple("A credit note has to credit something."),
    ),
    (
        NOTHING_TO_CREDIT,
        Locale::Arabic,
        Template::Simple("يجب أن يتضمن الإشعار الدائن ما يتم إصداره عنه."),
    ),
    (
        NO_SUCH_LINE,
        Locale::English,
        Template::Simple("Invoice {invoice} has no line {line}."),
    ),
    (
        NO_SUCH_LINE,
        Locale::Arabic,
        Template::Simple("الفاتورة {invoice} لا تحتوي على البند {line}."),
    ),
    (
        NOT_A_STOCK_LINE,
        Locale::English,
        Template::Simple(
            "Line {line} of invoice {invoice} sold no product, so no units can come back on it. Leave the quantity out.",
        ),
    ),
    (
        NOT_A_STOCK_LINE,
        Locale::Arabic,
        Template::Simple(
            "البند {line} من الفاتورة {invoice} لم يبع صنفًا مخزنيًا، فلا يمكن إرجاع وحدات عليه. اترك الكمية فارغة.",
        ),
    ),
    (
        OVER_DOCUMENT_LIMIT,
        Locale::English,
        Template::Simple(
            "This document comes to {amount}, and this business limits one document to {limit}. Ask the owner, or somebody who holds the {claim} claim here.",
        ),
    ),
    (
        OVER_DOCUMENT_LIMIT,
        Locale::Arabic,
        Template::Simple(
            "قيمة هذا المستند {amount}، والحد الذي وضعته المنشأة للمستند الواحد {limit}. اطلب ذلك من المالك أو من شخص يملك صلاحية {claim} هنا.",
        ),
    ),
    (
        DOCUMENT_LIMIT_CURRENCY,
        Locale::English,
        Template::Simple(
            "This document comes to {amount}, and this business's limit of {limit} per document is in another currency, so it cannot be judged against it. Ask the owner, or somebody who holds the {claim} claim here.",
        ),
    ),
    (
        DOCUMENT_LIMIT_CURRENCY,
        Locale::Arabic,
        Template::Simple(
            "قيمة هذا المستند {amount}، وحد المنشأة للمستند الواحد {limit} بعملة أخرى فلا تمكن مقارنته به. اطلب ذلك من المالك أو من شخص يملك صلاحية {claim} هنا.",
        ),
    ),
    (
        DOCUMENT_LIMIT_NOT_POSITIVE,
        Locale::English,
        Template::Simple(
            "A limit on one document must be more than nothing, and {limit} is not. Send null to have no limit.",
        ),
    ),
    (
        DOCUMENT_LIMIT_NOT_POSITIVE,
        Locale::Arabic,
        Template::Simple(
            "يجب أن يكون حد المستند الواحد أكبر من الصفر، و{limit} ليس كذلك. أرسل null لإلغاء الحد.",
        ),
    ),
    (
        DOCUMENT_CURRENCY,
        Locale::English,
        Template::Simple(
            "Tax documents here are issued in {expected}; this one is in {found}. Issue it in \
             {expected}, or keep the foreign-currency side in your own books.",
        ),
    ),
    (
        DOCUMENT_CURRENCY,
        Locale::Arabic,
        Template::Simple(
            "المستندات الضريبية هنا تصدر بعملة {expected}، وهذا المستند بعملة {found}. أصدره \
             بعملة {expected}، أو أمسك جانب العملة الأجنبية في دفاترك.",
        ),
    ),
];
