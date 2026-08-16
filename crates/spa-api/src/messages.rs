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
pub const UNKNOWN_ID_SCHEME: MessageCode = MessageCode::new("request.unknown_id_scheme");
pub const UNKNOWN_ONBOARDING_STAGE: MessageCode =
    MessageCode::new("request.unknown_onboarding_stage");
pub const UNKNOWN_ZATCA_ENVIRONMENT: MessageCode =
    MessageCode::new("request.unknown_zatca_environment");
pub const NO_SEALING_KEY: MessageCode = MessageCode::new("request.no_sealing_key");
pub const UNUSABLE_UNIT: MessageCode = MessageCode::new("request.unusable_unit");
pub const UNREADABLE_CERTIFICATE: MessageCode = MessageCode::new("request.unreadable_certificate");
pub const CERTIFICATE_KEY_MISMATCH: MessageCode =
    MessageCode::new("request.certificate_key_mismatch");
pub const ONBOARDING_NOT_YET: MessageCode = MessageCode::new("request.onboarding_not_yet");
pub const CSID_NOT_ISSUED: MessageCode = MessageCode::new("request.csid_not_issued");
pub const ZATCA_UNREACHABLE: MessageCode = MessageCode::new("request.zatca_unreachable");
pub const NOT_AN_OTP: MessageCode = MessageCode::new("request.not_an_otp");
pub const COMPLIANCE_REFUSED: MessageCode = MessageCode::new("request.compliance_refused");
/// 503. The caller asked to see a write that has not been projected yet.
pub const NOT_CAUGHT_UP: MessageCode = MessageCode::new("request.not_caught_up");
/// A module was asked for without one it cannot work without.
pub const MODULE_REQUIRES: MessageCode = MessageCode::new("request.module_requires");
/// 404. The tenant did not enable the module this route belongs to.
pub const MODULE_NOT_ENABLED: MessageCode = MessageCode::new("request.module_not_enabled");
pub const UNKNOWN_VAT_CATEGORY: MessageCode = MessageCode::new("request.unknown_vat_category");
pub const NO_SUCH_INVOICE: MessageCode = MessageCode::new("request.no_such_invoice");
pub const NO_SUCH_BILL: MessageCode = MessageCode::new("request.no_such_bill");
/// A module cannot be turned off while another is standing on it.
pub const MODULE_IN_USE: MessageCode = MessageCode::new("request.module_in_use");
/// A module still in the build, no longer offered to anybody new.
pub const MODULE_DEPRECATED: MessageCode = MessageCode::new("request.module_deprecated");
/// A VAT rate that is negative or over 100%.
pub const UNUSABLE_VAT_RATE: MessageCode = MessageCode::new("request.unusable_vat_rate");
/// A reporting period that ends before it begins, or on the day it begins.
pub const EMPTY_PERIOD: MessageCode = MessageCode::new("request.empty_period");
/// 400 or 422. The body is not JSON, or not the JSON this route wanted.
pub const MALFORMED_BODY: MessageCode = MessageCode::new("request.malformed_body");
/// 415. A body was sent without `Content-Type: application/json`.
pub const UNSUPPORTED_MEDIA_TYPE: MessageCode = MessageCode::new("request.unsupported_media_type");
/// 400. The query string is missing something, or carries something unreadable.
pub const INVALID_QUERY: MessageCode = MessageCode::new("request.invalid_query");

pub static CODES: &[MessageCode] = &[
    UNKNOWN_CURRENCY,
    UNKNOWN_ACCOUNT_KIND,
    INVALID_ID,
    PASSWORD_TOO_SHORT,
    UNKNOWN_MODULE,
    UNKNOWN_CHART,
    UNKNOWN_ROLE,
    UNKNOWN_ID_SCHEME,
    UNKNOWN_ONBOARDING_STAGE,
    UNKNOWN_ZATCA_ENVIRONMENT,
    NO_SEALING_KEY,
    UNUSABLE_UNIT,
    UNREADABLE_CERTIFICATE,
    CERTIFICATE_KEY_MISMATCH,
    ONBOARDING_NOT_YET,
    CSID_NOT_ISSUED,
    ZATCA_UNREACHABLE,
    NOT_AN_OTP,
    COMPLIANCE_REFUSED,
    NOT_CAUGHT_UP,
    MODULE_REQUIRES,
    MODULE_NOT_ENABLED,
    UNKNOWN_VAT_CATEGORY,
    NO_SUCH_INVOICE,
    NO_SUCH_BILL,
    MODULE_IN_USE,
    MODULE_DEPRECATED,
    UNUSABLE_VAT_RATE,
    EMPTY_PERIOD,
    MALFORMED_BODY,
    UNSUPPORTED_MEDIA_TYPE,
    INVALID_QUERY,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    // `reason` is the parser's own account of what it found, in English. It is
    // for whoever is writing the client, and it is the one thing that turns
    // "400" into a fixable message.
    (
        MALFORMED_BODY,
        Locale::English,
        Template::Simple("The request body could not be read: {reason}"),
    ),
    (
        MALFORMED_BODY,
        Locale::Arabic,
        Template::Simple("تعذّرت قراءة محتوى الطلب: {reason}"),
    ),
    (
        UNSUPPORTED_MEDIA_TYPE,
        Locale::English,
        Template::Simple("This endpoint takes `Content-Type: application/json`."),
    ),
    (
        UNSUPPORTED_MEDIA_TYPE,
        Locale::Arabic,
        Template::Simple("يقبل هذا المسار `Content-Type: application/json` فقط."),
    ),
    (
        INVALID_QUERY,
        Locale::English,
        Template::Simple("The query string could not be read: {reason}"),
    ),
    (
        INVALID_QUERY,
        Locale::Arabic,
        Template::Simple("تعذّرت قراءة معطيات الاستعلام: {reason}"),
    ),
    (
        NO_SUCH_BILL,
        Locale::English,
        Template::Simple("There is no bill {bill}."),
    ),
    (
        NO_SUCH_BILL,
        Locale::Arabic,
        Template::Simple("لا توجد فاتورة مورّد {bill}."),
    ),
    (
        MODULE_DEPRECATED,
        Locale::English,
        Template::Simple(
            "The {module} module is no longer offered: {why}. Tenants already \
             using it keep it.",
        ),
    ),
    (
        MODULE_DEPRECATED,
        Locale::Arabic,
        Template::Simple("لم تعد وحدة {module} متاحة: {why}. تحتفظ الجهات التي تستخدمها بها."),
    ),
    (
        UNUSABLE_VAT_RATE,
        Locale::English,
        Template::Simple(
            "{rate} is not a usable VAT rate. Give it in basis points, between 0 and 10000 — 1500 is 15%.",
        ),
    ),
    (
        UNUSABLE_VAT_RATE,
        Locale::Arabic,
        Template::Simple(
            "{rate} ليست نسبة ضريبة صالحة. أدخلها بنقاط الأساس بين 0 و10000 — القيمة 1500 تعني 15%.",
        ),
    ),
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
        UNKNOWN_ID_SCHEME,
        Locale::English,
        Template::Simple(
            "{scheme} is not an identification scheme. Use crn, mom, mls, sag, number700 or other.",
        ),
    ),
    (
        UNKNOWN_ID_SCHEME,
        Locale::Arabic,
        Template::Simple(
            "{scheme} ليس نوع سجل. استخدم crn أو mom أو mls أو sag أو number700 أو other.",
        ),
    ),
    (
        NOT_AN_OTP,
        Locale::English,
        Template::Simple(
            "That is not a Fatoora OTP. It is six digits, generated in the ZATCA portal, and it lasts about an hour.",
        ),
    ),
    (
        NOT_AN_OTP,
        Locale::Arabic,
        Template::Simple(
            "هذا ليس رمز تحقق من بوابة فاتورة. الرمز ستة أرقام يُنشأ من البوابة وتنتهي صلاحيته خلال ساعة تقريبًا.",
        ),
    ),
    (
        COMPLIANCE_REFUSED,
        Locale::English,
        Template::Simple(
            "ZATCA refused {failed} of the {submitted} compliance documents this system generated, so it cannot go live. This is a fault in this software rather than in your request: {reason}",
        ),
    ),
    (
        COMPLIANCE_REFUSED,
        Locale::Arabic,
        Template::Simple(
            "رفضت هيئة الزكاة والضريبة والجمارك {failed} من أصل {submitted} من مستندات الفحص التي أنشأها النظام، فتعذّر التفعيل. الخلل في النظام وليس في طلبك: {reason}",
        ),
    ),
    (
        UNKNOWN_ONBOARDING_STAGE,
        Locale::English,
        Template::Simple("{stage} is not an onboarding stage. Use compliance or production."),
    ),
    (
        UNKNOWN_ONBOARDING_STAGE,
        Locale::Arabic,
        Template::Simple("{stage} ليست مرحلة تسجيل. استخدم compliance أو production."),
    ),
    (
        UNKNOWN_ZATCA_ENVIRONMENT,
        Locale::English,
        Template::Simple(
            "{environment} is not a ZATCA environment. Use sandbox, simulation or production.",
        ),
    ),
    (
        UNKNOWN_ZATCA_ENVIRONMENT,
        Locale::Arabic,
        Template::Simple(
            "{environment} ليست بيئة لدى هيئة الزكاة والضريبة والجمارك. استخدم sandbox أو simulation أو production.",
        ),
    ),
    (
        NO_SEALING_KEY,
        Locale::English,
        Template::Simple(
            "This deployment has no sealing key, so a private key cannot be stored safely. Set SEALING_KEY and try again.",
        ),
    ),
    (
        NO_SEALING_KEY,
        Locale::Arabic,
        Template::Simple(
            "لا يوجد مفتاح تشفير مُهيّأ في هذا النظام، فلا يمكن حفظ المفتاح الخاص بأمان. اضبط SEALING_KEY ثم أعد المحاولة.",
        ),
    ),
    (
        UNUSABLE_UNIT,
        Locale::English,
        Template::Simple("That unit cannot go in a certificate request: {reason}."),
    ),
    (
        UNUSABLE_UNIT,
        Locale::Arabic,
        Template::Simple("لا يمكن استخدام بيانات الوحدة في طلب الشهادة: {reason}."),
    ),
    (
        UNREADABLE_CERTIFICATE,
        Locale::English,
        Template::Simple("That is not a certificate this system can read: {reason}."),
    ),
    (
        UNREADABLE_CERTIFICATE,
        Locale::Arabic,
        Template::Simple("تعذّرت قراءة الشهادة: {reason}."),
    ),
    (
        CERTIFICATE_KEY_MISMATCH,
        Locale::English,
        Template::Simple(
            "That certificate is not for the private key held for this business. Every invoice signed with it would be rejected, so it has not been stored.",
        ),
    ),
    (
        CERTIFICATE_KEY_MISMATCH,
        Locale::Arabic,
        Template::Simple(
            "هذه الشهادة ليست للمفتاح الخاص المحفوظ لهذه المنشأة. كل فاتورة تُوقَّع بها سترفض، لذلك لم تُحفظ.",
        ),
    ),
    (
        ONBOARDING_NOT_YET,
        Locale::English,
        Template::Simple("This business has no {stage} certificate yet."),
    ),
    (
        ONBOARDING_NOT_YET,
        Locale::Arabic,
        Template::Simple("لا توجد شهادة {stage} لهذه المنشأة بعد."),
    ),
    (
        CSID_NOT_ISSUED,
        Locale::English,
        Template::Simple("ZATCA did not issue a certificate ({disposition}): {detail}"),
    ),
    (
        CSID_NOT_ISSUED,
        Locale::Arabic,
        Template::Simple("لم تُصدر هيئة الزكاة والضريبة والجمارك شهادة ({disposition}): {detail}"),
    ),
    (
        ZATCA_UNREACHABLE,
        Locale::English,
        Template::Simple(
            "ZATCA could not be reached while {step}: {reason}. Nothing beyond the last completed step was changed.",
        ),
    ),
    (
        ZATCA_UNREACHABLE,
        Locale::Arabic,
        Template::Simple(
            "تعذّر الوصول إلى هيئة الزكاة والضريبة والجمارك أثناء {step}: {reason}. لم يتغيّر شيء بعد آخر خطوة اكتملت.",
        ),
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
    (
        MODULE_REQUIRES,
        Locale::English,
        Template::Simple("The {module} module needs {required}. Add it to the list."),
    ),
    (
        MODULE_REQUIRES,
        Locale::Arabic,
        Template::Simple("تحتاج وحدة {module} إلى {required}. أضفها إلى القائمة."),
    ),
    (
        MODULE_NOT_ENABLED,
        Locale::English,
        Template::Simple("The {module} module is not enabled for this tenant."),
    ),
    (
        MODULE_NOT_ENABLED,
        Locale::Arabic,
        Template::Simple("وحدة {module} غير مفعَّلة لدى هذا المستأجر."),
    ),
    (
        UNKNOWN_VAT_CATEGORY,
        Locale::English,
        Template::Simple("{vat} is not a VAT treatment. Use standard, zero or exempt."),
    ),
    (
        UNKNOWN_VAT_CATEGORY,
        Locale::Arabic,
        Template::Simple("{vat} ليست معاملة ضريبية. استخدم standard أو zero أو exempt."),
    ),
    (
        NO_SUCH_INVOICE,
        Locale::English,
        Template::Simple("There is no invoice {invoice}."),
    ),
    (
        NO_SUCH_INVOICE,
        Locale::Arabic,
        Template::Simple("لا توجد فاتورة {invoice}."),
    ),
    (
        MODULE_IN_USE,
        Locale::English,
        Template::Simple("{dependent} needs {module}. Turn {dependent} off first."),
    ),
    (
        MODULE_IN_USE,
        Locale::Arabic,
        Template::Simple("تحتاج وحدة {dependent} إلى {module}. أوقف {dependent} أولًا."),
    ),
    (
        EMPTY_PERIOD,
        Locale::English,
        Template::Simple("A period must end after it starts. `until` is exclusive."),
    ),
    (
        EMPTY_PERIOD,
        Locale::Arabic,
        Template::Simple("يجب أن تنتهي الفترة بعد بدايتها. تاريخ الانتهاء غير شامل."),
    ),
];
