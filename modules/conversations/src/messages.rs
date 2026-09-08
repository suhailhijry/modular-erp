//! This module's refusals, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NOTHING_TO_SAY: MessageCode = MessageCode::new("conversations.nothing_to_say");
pub const NO_CLIENT: MessageCode = MessageCode::new("conversations.no_client");
pub const NOT_REACHABLE_THERE: MessageCode = MessageCode::new("conversations.not_reachable_there");
pub const NOT_A_CHANNEL_FOR_THIS: MessageCode =
    MessageCode::new("conversations.not_a_channel_for_this");
pub const ALREADY_ASSIGNED: MessageCode = MessageCode::new("conversations.already_assigned");
pub const NO_SUCH_THREAD: MessageCode = MessageCode::new("conversations.no_such_thread");
pub const DATABASE: MessageCode = MessageCode::new("conversations.database");

pub const CODES: &[MessageCode] = &[
    NOTHING_TO_SAY,
    NO_CLIENT,
    NOT_REACHABLE_THERE,
    NOT_A_CHANNEL_FOR_THIS,
    ALREADY_ASSIGNED,
    NO_SUCH_THREAD,
    DATABASE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NOTHING_TO_SAY,
        Locale::English,
        Template::Simple("A message needs something in it."),
    ),
    (
        NOTHING_TO_SAY,
        Locale::Arabic,
        Template::Simple("الرسالة تحتاج إلى نص."),
    ),
    (
        NO_CLIENT,
        Locale::English,
        Template::Simple("There is nobody to send this to. Notes on it are still kept."),
    ),
    (
        NO_CLIENT,
        Locale::Arabic,
        Template::Simple("لا يوجد من تُرسل إليه هذه الرسالة. الملاحظات تبقى محفوظة."),
    ),
    (
        NOT_REACHABLE_THERE,
        Locale::English,
        Template::Simple("That customer has no {channel} address."),
    ),
    (
        NOT_REACHABLE_THERE,
        Locale::Arabic,
        Template::Simple("لا يوجد عنوان {channel} لهذا العميل."),
    ),
    (
        NOT_A_CHANNEL_FOR_THIS,
        Locale::English,
        Template::Simple(
            "{channel} is not a channel a person types into. WhatsApp takes approved \
             templates outside a 24-hour window, and push reaches a device rather than \
             a person. Use sms or email.",
        ),
    ),
    (
        NOT_A_CHANNEL_FOR_THIS,
        Locale::Arabic,
        Template::Simple(
            "لا يمكن كتابة رسالة مباشرة عبر {channel}. واتساب يقبل القوالب المعتمدة فقط \
             خارج نافذة الأربع والعشرين ساعة، والإشعار يصل إلى جهاز لا إلى شخص. \
             استخدم sms أو email.",
        ),
    ),
    (
        ALREADY_ASSIGNED,
        Locale::English,
        Template::Simple("That conversation has already been assigned."),
    ),
    (
        ALREADY_ASSIGNED,
        Locale::Arabic,
        Template::Simple("تم إسناد هذه المحادثة من قبل."),
    ),
    (
        NO_SUCH_THREAD,
        Locale::English,
        Template::Simple("No conversation is waiting on that number."),
    ),
    (
        NO_SUCH_THREAD,
        Locale::Arabic,
        Template::Simple("لا توجد محادثة معلّقة على هذا الرقم."),
    ),
    (
        DATABASE,
        Locale::English,
        Template::Simple("The conversation could not be read. Try again."),
    ),
    (
        DATABASE,
        Locale::Arabic,
        Template::Simple("تعذّرت قراءة المحادثة. حاول مرة أخرى."),
    ),
];
