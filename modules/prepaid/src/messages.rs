//! The prepaid module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NO_SUCH_CUSTOMER: MessageCode = MessageCode::new("prepaid.no_such_customer");
pub const ALREADY_GRANTED: MessageCode = MessageCode::new("prepaid.already_granted");
pub const NO_SUCH_ENTITLEMENT: MessageCode = MessageCode::new("prepaid.no_such_entitlement");
pub const NOT_LIVE: MessageCode = MessageCode::new("prepaid.not_live");
pub const LAPSED: MessageCode = MessageCode::new("prepaid.lapsed");
pub const NOTHING_LEFT: MessageCode = MessageCode::new("prepaid.nothing_left");
pub const NOT_A_VALUE: MessageCode = MessageCode::new("prepaid.not_a_value");
pub const FREE_GRANT_WITH_VALUE: MessageCode = MessageCode::new("prepaid.free_grant_with_value");
pub const OPEN_VALUE: MessageCode = MessageCode::new("prepaid.open_value");
pub const NO_SUCH_SUBSCRIPTION: MessageCode = MessageCode::new("prepaid.no_such_subscription");
pub const ALREADY_STARTED: MessageCode = MessageCode::new("prepaid.already_started");
pub const NOT_A_TERM: MessageCode = MessageCode::new("prepaid.not_a_term");
pub const ALREADY_FROZEN: MessageCode = MessageCode::new("prepaid.already_frozen");
pub const NOT_FROZEN: MessageCode = MessageCode::new("prepaid.not_frozen");
pub const CANCELLED: MessageCode = MessageCode::new("prepaid.cancelled");
pub const TERM_NOT_OVER: MessageCode = MessageCode::new("prepaid.term_not_over");
pub const NO_SUCH_CARD: MessageCode = MessageCode::new("prepaid.no_such_card");
pub const ALREADY_OPEN: MessageCode = MessageCode::new("prepaid.already_open");
pub const NO_SCHEME: MessageCode = MessageCode::new("prepaid.no_scheme");
pub const WRONG_CURRENCY: MessageCode = MessageCode::new("prepaid.wrong_currency");
pub const UNKNOWN_REASON: MessageCode = MessageCode::new("prepaid.unknown_reason");
pub const UNKNOWN_MECHANIC: MessageCode = MessageCode::new("prepaid.unknown_mechanic");
pub const AMOUNT_OUT_OF_RANGE: MessageCode = MessageCode::new("prepaid.amount_out_of_range");

pub static CODES: &[MessageCode] = &[
    NO_SUCH_CUSTOMER,
    ALREADY_GRANTED,
    NO_SUCH_ENTITLEMENT,
    NOT_LIVE,
    LAPSED,
    NOTHING_LEFT,
    NOT_A_VALUE,
    FREE_GRANT_WITH_VALUE,
    OPEN_VALUE,
    NO_SUCH_SUBSCRIPTION,
    ALREADY_STARTED,
    NOT_A_TERM,
    ALREADY_FROZEN,
    NOT_FROZEN,
    CANCELLED,
    TERM_NOT_OVER,
    NO_SUCH_CARD,
    ALREADY_OPEN,
    NO_SCHEME,
    WRONG_CURRENCY,
    UNKNOWN_REASON,
    UNKNOWN_MECHANIC,
    AMOUNT_OUT_OF_RANGE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NO_SUCH_CARD,
        Locale::English,
        Template::Simple("There is no card {id}."),
    ),
    (
        NO_SUCH_CARD,
        Locale::Arabic,
        Template::Simple("لا توجد بطاقة {id}."),
    ),
    (
        ALREADY_OPEN,
        Locale::English,
        Template::Simple("Card {id} is already open."),
    ),
    (
        ALREADY_OPEN,
        Locale::Arabic,
        Template::Simple("البطاقة {id} مفتوحة بالفعل."),
    ),
    (
        NO_SCHEME,
        Locale::English,
        Template::Simple(
            "No loyalty scheme has been configured, so there is nothing to earn against.",
        ),
    ),
    (
        NO_SCHEME,
        Locale::Arabic,
        Template::Simple("لم يتم إعداد برنامج ولاء، فلا شيء يُكتسب مقابله."),
    ),
    (
        WRONG_CURRENCY,
        Locale::English,
        Template::Simple("Card {id} holds a balance in another currency than the scheme."),
    ),
    (
        WRONG_CURRENCY,
        Locale::Arabic,
        Template::Simple("البطاقة {id} تحمل رصيدًا بعملة غير عملة البرنامج."),
    ),
    (
        UNKNOWN_MECHANIC,
        Locale::English,
        Template::Simple("{mechanic} is not a way a card counts."),
    ),
    (
        UNKNOWN_MECHANIC,
        Locale::Arabic,
        Template::Simple("{mechanic} ليست طريقة عدّ لبطاقة."),
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
        ALREADY_GRANTED,
        Locale::English,
        Template::Simple("{id} has already been granted."),
    ),
    (
        ALREADY_GRANTED,
        Locale::Arabic,
        Template::Simple("تم منح {id} من قبل."),
    ),
    (
        NO_SUCH_ENTITLEMENT,
        Locale::English,
        Template::Simple("There is no package or deposit {id}."),
    ),
    (
        NO_SUCH_ENTITLEMENT,
        Locale::Arabic,
        Template::Simple("لا توجد باقة أو عربون باسم {id}."),
    ),
    (
        NOT_LIVE,
        Locale::English,
        Template::Simple("{id} is finished and cannot be used again."),
    ),
    (
        NOT_LIVE,
        Locale::Arabic,
        Template::Simple("انتهى {id} ولا يمكن استخدامه مرة أخرى."),
    ),
    (
        LAPSED,
        Locale::English,
        Template::Simple("{id} expired on {on}."),
    ),
    (
        LAPSED,
        Locale::Arabic,
        Template::Simple("انتهت صلاحية {id} في {on}."),
    ),
    (
        NOTHING_LEFT,
        Locale::English,
        Template::Simple("Only {left} is left on {id}, and {wanted} was asked for."),
    ),
    (
        NOTHING_LEFT,
        Locale::Arabic,
        Template::Simple("لم يتبق سوى {left} في {id}، والمطلوب {wanted}."),
    ),
    (
        NOT_A_VALUE,
        Locale::English,
        Template::Simple("An amount here must be more than nothing."),
    ),
    (
        NOT_A_VALUE,
        Locale::Arabic,
        Template::Simple("يجب أن يكون المبلغ هنا أكبر من صفر."),
    ),
    (
        FREE_GRANT_WITH_VALUE,
        Locale::English,
        Template::Simple(
            "Nobody paid for this, so it carries no value. A coupon is a discount, not a balance.",
        ),
    ),
    (
        FREE_GRANT_WITH_VALUE,
        Locale::Arabic,
        Template::Simple("لم يدفع أحد مقابل هذا، فلا قيمة له. القسيمة خصم وليست رصيدًا."),
    ),
    (
        OPEN_VALUE,
        Locale::English,
        Template::Simple(
            "An amount must either count uses or name what it is held against. A card spendable on anything is not supported.",
        ),
    ),
    (
        OPEN_VALUE,
        Locale::Arabic,
        Template::Simple(
            "المبلغ يجب أن يحدّد عدد الاستخدامات أو ما هو محجوز مقابله. البطاقة القابلة للصرف على أي شيء غير مدعومة.",
        ),
    ),
    (
        NO_SUCH_SUBSCRIPTION,
        Locale::English,
        Template::Simple("There is no subscription {id}."),
    ),
    (
        NO_SUCH_SUBSCRIPTION,
        Locale::Arabic,
        Template::Simple("لا يوجد اشتراك {id}."),
    ),
    (
        ALREADY_STARTED,
        Locale::English,
        Template::Simple("Subscription {id} has already started."),
    ),
    (
        ALREADY_STARTED,
        Locale::Arabic,
        Template::Simple("بدأ الاشتراك {id} من قبل."),
    ),
    (
        NOT_A_TERM,
        Locale::English,
        Template::Simple("A term must end after it starts."),
    ),
    (
        NOT_A_TERM,
        Locale::Arabic,
        Template::Simple("يجب أن تنتهي المدة بعد بدايتها."),
    ),
    (
        ALREADY_FROZEN,
        Locale::English,
        Template::Simple("Subscription {id} is already frozen."),
    ),
    (
        ALREADY_FROZEN,
        Locale::Arabic,
        Template::Simple("الاشتراك {id} مجمّد بالفعل."),
    ),
    (
        NOT_FROZEN,
        Locale::English,
        Template::Simple("Subscription {id} is not frozen."),
    ),
    (
        NOT_FROZEN,
        Locale::Arabic,
        Template::Simple("الاشتراك {id} غير مجمّد."),
    ),
    (
        CANCELLED,
        Locale::English,
        Template::Simple("Subscription {id} has been cancelled."),
    ),
    (
        CANCELLED,
        Locale::Arabic,
        Template::Simple("تم إلغاء الاشتراك {id}."),
    ),
    (
        TERM_NOT_OVER,
        Locale::English,
        Template::Simple("The current term of {id} runs until {until} and cannot be renewed yet."),
    ),
    (
        TERM_NOT_OVER,
        Locale::Arabic,
        Template::Simple("تستمر مدة {id} الحالية حتى {until} ولا يمكن تجديدها بعد."),
    ),
    (
        UNKNOWN_REASON,
        Locale::English,
        Template::Simple("{value} is not a way something is granted."),
    ),
    (
        UNKNOWN_REASON,
        Locale::Arabic,
        Template::Simple("{value} ليست طريقة يُمنح بها شيء."),
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
