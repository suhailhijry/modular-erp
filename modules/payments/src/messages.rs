//! This module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NOT_STARTED: MessageCode = MessageCode::new("payments.not_started");
pub const ALREADY_STARTED: MessageCode = MessageCode::new("payments.already_started");
pub const WRONG_AMOUNT: MessageCode = MessageCode::new("payments.wrong_amount");
pub const NOT_COLLECTABLE: MessageCode = MessageCode::new("payments.not_collectable");
pub const REFUND_TOO_LARGE: MessageCode = MessageCode::new("payments.refund_too_large");
pub const NO_GATEWAY: MessageCode = MessageCode::new("payments.no_gateway");
pub const PAYOUT_RECORDED: MessageCode = MessageCode::new("payments.payout_recorded");
pub const NOT_SETTLED: MessageCode = MessageCode::new("payments.not_settled");
pub const PAYOUT_CURRENCY: MessageCode = MessageCode::new("payments.payout_currency");
pub const NO_SAVED_CARDS: MessageCode = MessageCode::new("payments.no_saved_cards");
pub const NO_SUCH_CARD: MessageCode = MessageCode::new("payments.no_such_card");
pub const CARD_FORGOTTEN: MessageCode = MessageCode::new("payments.card_forgotten");
pub const NOT_A_CARD: MessageCode = MessageCode::new("payments.not_a_card");

pub const NOT_A_DEPOSIT: MessageCode = MessageCode::new("payments.not_a_deposit");
pub const NOTHING_TO_RETAIN: MessageCode = MessageCode::new("payments.nothing_to_retain");

pub static CODES: &[MessageCode] = &[
    NOT_A_DEPOSIT,
    NOTHING_TO_RETAIN,
    NOT_STARTED,
    ALREADY_STARTED,
    WRONG_AMOUNT,
    NOT_COLLECTABLE,
    REFUND_TOO_LARGE,
    NO_GATEWAY,
    PAYOUT_RECORDED,
    NOT_SETTLED,
    PAYOUT_CURRENCY,
    NO_SAVED_CARDS,
    NO_SUCH_CARD,
    CARD_FORGOTTEN,
    NOT_A_CARD,
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
    (
        PAYOUT_RECORDED,
        Locale::English,
        Template::Simple("Payout {id} has already been recorded."),
    ),
    (
        PAYOUT_RECORDED,
        Locale::Arabic,
        Template::Simple("التحويل {id} مسجَّل بالفعل."),
    ),
    // **Refused rather than skipped.** A payout naming a payment this system
    // has not settled would reconcile against a smaller set than the operator
    // thinks, and the missing amount would look like the gateway paying short.
    (
        NOT_SETTLED,
        Locale::English,
        Template::Simple(
            "{payment} is not a settled payment, so this payout cannot be reconciled against it.",
        ),
    ),
    (
        NOT_SETTLED,
        Locale::Arabic,
        Template::Simple("{payment} ليست عملية دفع مسوّاة، فلا يمكن مطابقة هذا التحويل معها."),
    ),
    (
        PAYOUT_CURRENCY,
        Locale::English,
        Template::Simple("A payout in {found} cannot cover payments in {expected}."),
    ),
    (
        PAYOUT_CURRENCY,
        Locale::Arabic,
        Template::Simple("لا يمكن لتحويل بعملة {found} أن يغطي مدفوعات بعملة {expected}."),
    ),
    // **Buy-now-pay-later has no card to keep.** The provider lends to the
    // customer and collects from them; there is no token on this side to charge
    // again, so a saved card naming one would be a row nobody can use.
    (
        NO_SAVED_CARDS,
        Locale::English,
        Template::Simple("{provider} does not keep cards that can be charged again later."),
    ),
    (
        NO_SAVED_CARDS,
        Locale::Arabic,
        Template::Simple("{provider} لا يحتفظ ببطاقات يمكن خصمها لاحقًا."),
    ),
    (
        NO_SUCH_CARD,
        Locale::English,
        Template::Simple("There is no saved card {id}."),
    ),
    (
        NO_SUCH_CARD,
        Locale::Arabic,
        Template::Simple("لا توجد بطاقة محفوظة {id}."),
    ),
    // Said as a fact rather than as an error: the customer asked for this, and
    // saving the card again means entering it again.
    (
        CARD_FORGOTTEN,
        Locale::English,
        Template::Simple("Card {id} was removed, and the card has to be entered again to be used."),
    ),
    (
        CARD_FORGOTTEN,
        Locale::Arabic,
        Template::Simple("تمت إزالة البطاقة {id}، ويلزم إدخالها من جديد لاستخدامها."),
    ),
    (
        NOT_A_CARD,
        Locale::English,
        Template::Simple("That cannot be saved as a card: {reason}."),
    ),
    (
        NOT_A_CARD,
        Locale::Arabic,
        Template::Simple("لا يمكن حفظ ذلك كبطاقة: {reason}."),
    ),
    (
        NOT_A_DEPOSIT,
        Locale::English,
        Template::Simple("Payment {id} is against an invoice, so there is no deposit to keep."),
    ),
    (
        NOT_A_DEPOSIT,
        Locale::Arabic,
        Template::Simple("عملية الدفع {id} مقابل فاتورة، فلا توجد دفعة مقدمة يمكن الاحتفاظ بها."),
    ),
    (
        NOTHING_TO_RETAIN,
        Locale::English,
        Template::Simple("There is nothing left of {id} to keep."),
    ),
    (
        NOTHING_TO_RETAIN,
        Locale::Arabic,
        Template::Simple("لم يتبق من {id} ما يمكن الاحتفاظ به."),
    ),
];
