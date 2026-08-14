//! Messages about the *request*, not about the domain.
//!
//! A malformed currency code or an unusable identifier is the API's business:
//! nothing downstream ever saw the value, because it did not parse.

use spa_i18n::{Locale, MessageCode, Template};

pub const UNKNOWN_CURRENCY: MessageCode = MessageCode::new("request.unknown_currency");
pub const UNKNOWN_ACCOUNT_KIND: MessageCode = MessageCode::new("request.unknown_account_kind");
pub const INVALID_ID: MessageCode = MessageCode::new("request.invalid_id");
pub const PASSWORD_TOO_SHORT: MessageCode = MessageCode::new("request.password_too_short");
pub const UNKNOWN_MODULE: MessageCode = MessageCode::new("request.unknown_module");
pub const UNKNOWN_CHART: MessageCode = MessageCode::new("request.unknown_chart");
pub const UNKNOWN_ROLE: MessageCode = MessageCode::new("request.unknown_role");
/// 503. The caller asked to see a write that has not been projected yet.
pub const NOT_CAUGHT_UP: MessageCode = MessageCode::new("request.not_caught_up");

pub static CODES: &[MessageCode] = &[
    UNKNOWN_CURRENCY,
    UNKNOWN_ACCOUNT_KIND,
    INVALID_ID,
    PASSWORD_TOO_SHORT,
    UNKNOWN_MODULE,
    UNKNOWN_CHART,
    UNKNOWN_ROLE,
    NOT_CAUGHT_UP,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        UNKNOWN_CURRENCY,
        Locale::English,
        Template::Simple("{currency} is not a currency code. Use three letters, like SAR."),
    ),
    (
        UNKNOWN_CURRENCY,
        Locale::Arabic,
        Template::Simple("{currency} ليس رمز عملة. استخدم ثلاثة أحرف، مثل SAR."),
    ),
    (
        UNKNOWN_ACCOUNT_KIND,
        Locale::English,
        Template::Simple(
            "{kind} is not an account type. Use asset, liability, equity, revenue or expense.",
        ),
    ),
    (
        UNKNOWN_ACCOUNT_KIND,
        Locale::Arabic,
        Template::Simple(
            "{kind} ليس نوع حساب. استخدم أصل أو التزام أو حقوق ملكية أو إيراد أو مصروف.",
        ),
    ),
    (
        INVALID_ID,
        Locale::English,
        Template::Simple("{id} cannot be used as an identifier."),
    ),
    (
        INVALID_ID,
        Locale::Arabic,
        Template::Simple("لا يمكن استخدام {id} كمعرّف."),
    ),
    (
        PASSWORD_TOO_SHORT,
        Locale::English,
        Template::Plural {
            zero: None,
            one: Some("A password needs at least {n} character."),
            two: None,
            few: None,
            many: None,
            other: "A password needs at least {n} characters.",
        },
    ),
    (
        PASSWORD_TOO_SHORT,
        Locale::Arabic,
        Template::Plural {
            zero: Some("كلمة المرور مطلوبة."),
            one: Some("تحتاج كلمة المرور إلى حرف واحد على الأقل."),
            two: Some("تحتاج كلمة المرور إلى حرفين على الأقل."),
            few: Some("تحتاج كلمة المرور إلى {n} أحرف على الأقل."),
            many: Some("تحتاج كلمة المرور إلى {n} حرفًا على الأقل."),
            other: "تحتاج كلمة المرور إلى {n} حرف على الأقل.",
        },
    ),
    (
        UNKNOWN_MODULE,
        Locale::English,
        Template::Simple("There is no module called {module}."),
    ),
    (
        UNKNOWN_MODULE,
        Locale::Arabic,
        Template::Simple("لا توجد وحدة باسم {module}."),
    ),
    (
        UNKNOWN_CHART,
        Locale::English,
        Template::Simple("There is no chart of accounts called {chart}."),
    ),
    (
        UNKNOWN_CHART,
        Locale::Arabic,
        Template::Simple("لا يوجد دليل حسابات باسم {chart}."),
    ),
    (
        UNKNOWN_ROLE,
        Locale::English,
        Template::Simple("{role} is not a role. Use owner, accountant, clerk or viewer."),
    ),
    (
        UNKNOWN_ROLE,
        Locale::Arabic,
        Template::Simple("{role} ليس دورًا. استخدم owner أو accountant أو clerk أو viewer."),
    ),
    (
        NOT_CAUGHT_UP,
        Locale::English,
        Template::Simple("Still catching up ({behind} to go). Please try again in a moment."),
    ),
    (
        NOT_CAUGHT_UP,
        Locale::Arabic,
        Template::Simple("لا يزال التحديث جاريًا (متبقٍ {behind}). يُرجى المحاولة بعد لحظات."),
    ),
];
