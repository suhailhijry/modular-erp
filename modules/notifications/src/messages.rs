//! This module's refusals, in every supported language.
//!
//! **Refusals only.** The wording of a notification is not here: this catalogue
//! is audited into `docs/ERRORS.md`, and a notification is not an error. See
//! [`crate::copy`].

use erp_i18n::{Locale, MessageCode, Template};

pub const UNKNOWN_KIND: MessageCode = MessageCode::new("notifications.unknown_kind");
pub const UNKNOWN_CHANNEL: MessageCode = MessageCode::new("notifications.unknown_channel");
pub const NOT_YOURS: MessageCode = MessageCode::new("notifications.not_yours");
pub const UNREACHABLE: MessageCode = MessageCode::new("notifications.unreachable");
pub const IN_SYSTEM_ONLY: MessageCode = MessageCode::new("notifications.in_system_only");
pub const DATABASE: MessageCode = MessageCode::new("notifications.database");

pub const CODES: &[MessageCode] = &[
    UNKNOWN_KIND,
    UNKNOWN_CHANNEL,
    NOT_YOURS,
    UNREACHABLE,
    IN_SYSTEM_ONLY,
    DATABASE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        UNKNOWN_KIND,
        Locale::English,
        Template::Simple("{kind} is not something this system announces."),
    ),
    (
        UNKNOWN_KIND,
        Locale::Arabic,
        Template::Simple("{kind} ليس مما يعلن عنه هذا النظام."),
    ),
    (
        UNKNOWN_CHANNEL,
        Locale::English,
        Template::Simple(
            "{channel} is not a channel. Use in_system, email, sms, push or whatsapp.",
        ),
    ),
    (
        UNKNOWN_CHANNEL,
        Locale::Arabic,
        Template::Simple(
            "{channel} ليست قناة. استخدم in_system أو email أو sms أو push أو whatsapp.",
        ),
    ),
    (
        NOT_YOURS,
        Locale::English,
        Template::Simple("No notification of yours has that id."),
    ),
    (
        NOT_YOURS,
        Locale::Arabic,
        Template::Simple("لا يوجد إشعار لك بهذا المعرف."),
    ),
    (
        UNREACHABLE,
        Locale::English,
        Template::Simple("Nobody is listed to be told about {kind}, so nothing was announced."),
    ),
    (
        UNREACHABLE,
        Locale::Arabic,
        Template::Simple("لا يوجد من يُبلَّغ عن {kind}، فلم يُعلن شيء."),
    ),
    (
        IN_SYSTEM_ONLY,
        Locale::English,
        Template::Simple(
            "{kind} is told in the system only: it goes to logins, which have no email address or phone number. Use in_system.",
        ),
    ),
    (
        IN_SYSTEM_ONLY,
        Locale::Arabic,
        Template::Simple(
            "لا يُبلَّغ عن {kind} إلا داخل النظام: فهو موجَّه إلى حسابات الدخول، ولا بريد إلكتروني لها ولا رقم هاتف. استخدم in_system.",
        ),
    ),
    (
        DATABASE,
        Locale::English,
        Template::Simple("The notification could not be recorded. Try again."),
    ),
    (
        DATABASE,
        Locale::Arabic,
        Template::Simple("تعذّر تسجيل الإشعار. حاول مرة أخرى."),
    ),
];
