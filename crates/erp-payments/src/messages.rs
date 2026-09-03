//! The payment layer's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const UNREACHABLE: MessageCode = MessageCode::new("payments.unreachable");
pub const REFUSED: MessageCode = MessageCode::new("payments.refused");
pub const UNAUTHENTICATED: MessageCode = MessageCode::new("payments.unauthenticated");
pub const NO_SUCH_PAYMENT: MessageCode = MessageCode::new("payments.no_such_payment");
pub const UNREADABLE: MessageCode = MessageCode::new("payments.unreadable");

pub static CODES: &[MessageCode] = &[
    UNREACHABLE,
    REFUSED,
    UNAUTHENTICATED,
    NO_SUCH_PAYMENT,
    UNREADABLE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        UNREACHABLE,
        Locale::English,
        Template::Simple("The payment provider could not be reached. Try again."),
    ),
    (
        UNREACHABLE,
        Locale::Arabic,
        Template::Simple("تعذّر الوصول إلى مزوّد الدفع. أعد المحاولة."),
    ),
    // **The provider's own words**, because a declined card is a conversation
    // between the customer and their bank and a generic refusal helps nobody.
    (
        REFUSED,
        Locale::English,
        Template::Simple("The payment was refused: {reason}"),
    ),
    (
        REFUSED,
        Locale::Arabic,
        Template::Simple("رُفضت عملية الدفع: {reason}"),
    ),
    // **Not the customer's fault**, and the message says so: somebody has to
    // fix an account setting, and telling the payer their card failed would
    // send them to their bank over a configuration mistake.
    (
        UNAUTHENTICATED,
        Locale::English,
        Template::Simple(
            "This business's payment provider is not configured correctly, so nothing was charged.",
        ),
    ),
    (
        UNAUTHENTICATED,
        Locale::Arabic,
        Template::Simple("إعدادات مزوّد الدفع لهذا النشاط غير صحيحة، فلم يُخصم أي مبلغ."),
    ),
    (
        NO_SUCH_PAYMENT,
        Locale::English,
        Template::Simple("The payment provider has no record of {id}."),
    ),
    (
        NO_SUCH_PAYMENT,
        Locale::Arabic,
        Template::Simple("لا يوجد لدى مزوّد الدفع أي سجل لـ {id}."),
    ),
    (
        UNREADABLE,
        Locale::English,
        Template::Simple(
            "The payment provider answered in a way this system did not understand, so nothing was recorded.",
        ),
    ),
    (
        UNREADABLE,
        Locale::Arabic,
        Template::Simple("ردّ مزوّد الدفع بصيغة لم يفهمها النظام، فلم يُسجَّل شيء."),
    ),
];
