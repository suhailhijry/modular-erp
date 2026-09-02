//! This module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NOT_A_NAME: MessageCode = MessageCode::new("messaging.not_a_name");
pub const UNKNOWN_BINDING: MessageCode = MessageCode::new("messaging.unknown_binding");
pub const WRONG_AUDIENCE: MessageCode = MessageCode::new("messaging.wrong_audience");
pub const NO_SUBJECT_LINE: MessageCode = MessageCode::new("messaging.no_subject_line");
pub const NEEDS_A_SUBJECT: MessageCode = MessageCode::new("messaging.needs_a_subject");
pub const MISSING_LANGUAGE: MessageCode = MessageCode::new("messaging.missing_language");
pub const EMPTY_TEMPLATE: MessageCode = MessageCode::new("messaging.empty_template");
pub const NO_SUCH_TEMPLATE: MessageCode = MessageCode::new("messaging.no_such_template");
pub const UNREACHABLE: MessageCode = MessageCode::new("messaging.unreachable");
pub const OVER_BUDGET: MessageCode = MessageCode::new("messaging.over_budget");
pub const UNKNOWN_CHANNEL: MessageCode = MessageCode::new("messaging.unknown_channel");
pub const UNKNOWN_TOPIC: MessageCode = MessageCode::new("messaging.unknown_topic");
pub const UNKNOWN_AUDIENCE: MessageCode = MessageCode::new("messaging.unknown_audience");
pub const UNKNOWN_PLATFORM: MessageCode = MessageCode::new("messaging.unknown_platform");
pub const NEGATIVE_BUDGET: MessageCode = MessageCode::new("messaging.negative_budget");
pub const NOT_A_MONTH: MessageCode = MessageCode::new("messaging.not_a_month");
pub const UNKNOWN_LANGUAGE: MessageCode = MessageCode::new("messaging.unknown_language");
pub const DATABASE: MessageCode = MessageCode::new("messaging.database");

pub const CODES: &[MessageCode] = &[
    NOT_A_NAME,
    UNKNOWN_BINDING,
    WRONG_AUDIENCE,
    NO_SUBJECT_LINE,
    NEEDS_A_SUBJECT,
    MISSING_LANGUAGE,
    EMPTY_TEMPLATE,
    NO_SUCH_TEMPLATE,
    UNREACHABLE,
    OVER_BUDGET,
    UNKNOWN_CHANNEL,
    UNKNOWN_TOPIC,
    UNKNOWN_AUDIENCE,
    UNKNOWN_PLATFORM,
    NEGATIVE_BUDGET,
    NOT_A_MONTH,
    UNKNOWN_LANGUAGE,
    DATABASE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NOT_A_NAME,
        Locale::English,
        Template::Simple(
            "{name} is not a template name. Use lower case, digits, dots and underscores.",
        ),
    ),
    (
        NOT_A_NAME,
        Locale::Arabic,
        Template::Simple("{name} ليس اسم قالب. استخدم حروفًا صغيرة وأرقامًا ونقاطًا وشرطات سفلية."),
    ),
    (
        UNKNOWN_BINDING,
        Locale::English,
        Template::Simple("{binding} is not something a message about {topic} can say."),
    ),
    (
        UNKNOWN_BINDING,
        Locale::Arabic,
        Template::Simple("{binding} ليس مما يمكن أن تقوله رسالة عن {topic}."),
    ),
    (
        WRONG_AUDIENCE,
        Locale::English,
        Template::Simple("A message about {topic} cannot be addressed to {audience}."),
    ),
    (
        WRONG_AUDIENCE,
        Locale::Arabic,
        Template::Simple("لا يمكن توجيه رسالة عن {topic} إلى {audience}."),
    ),
    (
        NO_SUBJECT_LINE,
        Locale::English,
        Template::Simple("{channel} has no subject line, so remove it."),
    ),
    (
        NO_SUBJECT_LINE,
        Locale::Arabic,
        Template::Simple("لا يوجد سطر موضوع في {channel}، فاحذفه."),
    ),
    (
        NEEDS_A_SUBJECT,
        Locale::English,
        Template::Simple("A message on {channel} needs a subject line."),
    ),
    (
        NEEDS_A_SUBJECT,
        Locale::Arabic,
        Template::Simple("تحتاج الرسالة على {channel} إلى سطر موضوع."),
    ),
    (
        MISSING_LANGUAGE,
        Locale::English,
        Template::Simple("This template has no wording in {locale}."),
    ),
    (
        MISSING_LANGUAGE,
        Locale::Arabic,
        Template::Simple("لا توجد صياغة بلغة {locale} في هذا القالب."),
    ),
    (
        EMPTY_TEMPLATE,
        Locale::English,
        Template::Simple("A template needs something to say."),
    ),
    (
        EMPTY_TEMPLATE,
        Locale::Arabic,
        Template::Simple("يحتاج القالب إلى نص."),
    ),
    (
        NO_SUCH_TEMPLATE,
        Locale::English,
        Template::Simple("There is no template called {name}, or it is switched off."),
    ),
    (
        NO_SUCH_TEMPLATE,
        Locale::Arabic,
        Template::Simple("لا يوجد قالب باسم {name}، أو أنه متوقف."),
    ),
    (
        UNREACHABLE,
        Locale::English,
        Template::Simple("Nobody in {audience} can be reached on {channel}."),
    ),
    (
        UNREACHABLE,
        Locale::Arabic,
        Template::Simple("لا يمكن الوصول إلى {audience} عبر {channel}."),
    ),
    (
        OVER_BUDGET,
        Locale::English,
        Template::Simple("{channel} has used its whole budget of {limit} for this month."),
    ),
    (
        OVER_BUDGET,
        Locale::Arabic,
        Template::Simple("استهلك {channel} كامل ميزانيته البالغة {limit} لهذا الشهر."),
    ),
    (
        UNKNOWN_CHANNEL,
        Locale::English,
        Template::Simple("{channel} is not a channel. Use email, sms, push or whatsapp."),
    ),
    (
        UNKNOWN_CHANNEL,
        Locale::Arabic,
        Template::Simple("{channel} ليس قناة. استخدم email أو sms أو push أو whatsapp."),
    ),
    (
        UNKNOWN_TOPIC,
        Locale::English,
        Template::Simple("{topic} is not something a message can be about."),
    ),
    (
        UNKNOWN_TOPIC,
        Locale::Arabic,
        Template::Simple("{topic} ليس موضوعًا يمكن أن تدور حوله رسالة."),
    ),
    (
        UNKNOWN_AUDIENCE,
        Locale::English,
        Template::Simple("{audience} is not an audience."),
    ),
    (
        UNKNOWN_AUDIENCE,
        Locale::Arabic,
        Template::Simple("{audience} ليس جمهورًا."),
    ),
    (
        UNKNOWN_PLATFORM,
        Locale::English,
        Template::Simple("{platform} is not a platform. Use apns, fcm or web."),
    ),
    (
        UNKNOWN_PLATFORM,
        Locale::Arabic,
        Template::Simple("{platform} ليست منصة. استخدم apns أو fcm أو web."),
    ),
    (
        NEGATIVE_BUDGET,
        Locale::English,
        Template::Simple("A budget cannot be negative, and {limit} is."),
    ),
    (
        NEGATIVE_BUDGET,
        Locale::Arabic,
        Template::Simple("لا يمكن أن تكون الميزانية سالبة، و{limit} كذلك."),
    ),
    (
        NOT_A_MONTH,
        Locale::English,
        Template::Simple("{period} is not a month. Write it as 2026-05."),
    ),
    (
        NOT_A_MONTH,
        Locale::Arabic,
        Template::Simple("{period} ليس شهرًا. اكتبه هكذا: 2026-05."),
    ),
    (
        UNKNOWN_LANGUAGE,
        Locale::English,
        Template::Simple("{language} is not a language this system speaks."),
    ),
    (
        UNKNOWN_LANGUAGE,
        Locale::Arabic,
        Template::Simple("{language} ليست لغة يتحدثها هذا النظام."),
    ),
    (
        DATABASE,
        Locale::English,
        Template::Simple("That could not be read. Try again."),
    ),
    (
        DATABASE,
        Locale::Arabic,
        Template::Simple("تعذّرت القراءة. أعد المحاولة."),
    ),
];
