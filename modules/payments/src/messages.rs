//! This module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NOT_STARTED: MessageCode = MessageCode::new("payments.not_started");
pub const ALREADY_STARTED: MessageCode = MessageCode::new("payments.already_started");
pub const WRONG_AMOUNT: MessageCode = MessageCode::new("payments.wrong_amount");
pub const NOT_COLLECTABLE: MessageCode = MessageCode::new("payments.not_collectable");
pub const REFUND_TOO_LARGE: MessageCode = MessageCode::new("payments.refund_too_large");
pub const NO_GATEWAY: MessageCode = MessageCode::new("payments.no_gateway");

pub static CODES: &[MessageCode] = &[
    NOT_STARTED,
    ALREADY_STARTED,
    WRONG_AMOUNT,
    NOT_COLLECTABLE,
    REFUND_TOO_LARGE,
    NO_GATEWAY,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NOT_STARTED,
        Locale::English,
        Template::Simple("There is no payment {id} to settle."),
    ),
    (
        NOT_STARTED,
        Locale::Arabic,
        Template::Simple("لا توجد عملية دفع {id} لتسويتها."),
    ),
    (
        ALREADY_STARTED,
        Locale::English,
        Template::Simple("Payment {id} has already been started."),
    ),
    (
        ALREADY_STARTED,
        Locale::Arabic,
        Template::Simple("عملية الدفع {id} بدأت بالفعل."),
    ),
    // **The refusal that stands between a gateway id and the books.** Said
    // plainly, because it means somebody is either misconfigured or trying it
    // on, and both need looking at.
    (
        WRONG_AMOUNT,
        Locale::English,
        Template::Simple(
            "The payment provider reported {found} against a payment started for {expected}, so nothing was recorded.",
        ),
    ),
    (
        WRONG_AMOUNT,
        Locale::Arabic,
        Template::Simple(
            "أفاد مزوّد الدفع بمبلغ {found} لعملية بدأت بمبلغ {expected}، فلم يُسجَّل شيء.",
        ),
    ),
    (
        NOT_COLLECTABLE,
        Locale::English,
        Template::Simple("Payment {id} is {stage}, so there is nothing to give back."),
    ),
    (
        NOT_COLLECTABLE,
        Locale::Arabic,
        Template::Simple("عملية الدفع {id} في حالة {stage}، فلا يوجد ما يُرد."),
    ),
    (
        REFUND_TOO_LARGE,
        Locale::English,
        Template::Simple("{amount} is more than is left to refund on this payment."),
    ),
    (
        REFUND_TOO_LARGE,
        Locale::Arabic,
        Template::Simple("{amount} أكبر من المتبقي القابل للاسترداد في هذه العملية."),
    ),
    (
        NO_GATEWAY,
        Locale::English,
        Template::Simple(
            "This business has no payment provider configured, so nothing was charged.",
        ),
    ),
    (
        NO_GATEWAY,
        Locale::Arabic,
        Template::Simple("لا يوجد مزوّد دفع مُهيّأ لهذا النشاط، فلم يُخصم أي مبلغ."),
    ),
];
