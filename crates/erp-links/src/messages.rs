//! The link store's messages, in every supported language.
//!
//! Every code here answers a person who has followed a link and did not arrive.
//! **They are the only messages in this system a stranger reads** — somebody
//! who has never signed in, tapping a URL in a text message — so they say what
//! happened and what to do, and never why the system thinks it is their fault.

use erp_i18n::{Locale, MessageCode, Template};

pub const NO_SUCH_LINK: MessageCode = MessageCode::new("links.no_such_link");
pub const EXPIRED: MessageCode = MessageCode::new("links.expired");
pub const ALREADY_USED: MessageCode = MessageCode::new("links.already_used");
pub const NOT_A_TARGET: MessageCode = MessageCode::new("links.not_a_target");

pub static CODES: &[MessageCode] = &[NO_SUCH_LINK, EXPIRED, ALREADY_USED, NOT_A_TARGET];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NO_SUCH_LINK,
        Locale::English,
        Template::Simple("That link does not go anywhere. Check it was copied whole."),
    ),
    (
        NO_SUCH_LINK,
        Locale::Arabic,
        Template::Simple("هذا الرابط لا يؤدي إلى شيء. تأكد من نسخه كاملًا."),
    ),
    (
        EXPIRED,
        Locale::English,
        Template::Simple("That link has expired. Ask for a new one."),
    ),
    (
        EXPIRED,
        Locale::Arabic,
        Template::Simple("انتهت صلاحية هذا الرابط. اطلب رابطًا جديدًا."),
    ),
    (
        ALREADY_USED,
        Locale::English,
        Template::Simple("That link has already been used. It only works once."),
    ),
    (
        ALREADY_USED,
        Locale::Arabic,
        Template::Simple("سبق استخدام هذا الرابط. وهو يعمل مرة واحدة فقط."),
    ),
    // Not a stranger's message: this one is for whoever tried to make the link.
    (
        NOT_A_TARGET,
        Locale::English,
        Template::Simple("{target} is not somewhere a link may point."),
    ),
    (
        NOT_A_TARGET,
        Locale::Arabic,
        Template::Simple("{target} ليس موضعًا يمكن أن يشير إليه رابط."),
    ),
];
