//! This module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NO_NAME: MessageCode = MessageCode::new("files.no_name");
pub const NO_SUCH_FILE: MessageCode = MessageCode::new("files.no_such_file");
pub const ALREADY_REMOVED: MessageCode = MessageCode::new("files.already_removed");
pub const UNKNOWN_OWNER: MessageCode = MessageCode::new("files.unknown_owner");
pub const NO_STORAGE: MessageCode = MessageCode::new("files.no_storage");
pub const NOT_A_MEDIA_TYPE: MessageCode = MessageCode::new("files.not_a_media_type");
pub const DATABASE: MessageCode = MessageCode::new("files.database");

pub const CODES: &[MessageCode] = &[
    NO_NAME,
    NO_SUCH_FILE,
    ALREADY_REMOVED,
    UNKNOWN_OWNER,
    NO_STORAGE,
    NOT_A_MEDIA_TYPE,
    DATABASE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NO_NAME,
        Locale::English,
        Template::Simple("A document needs a name."),
    ),
    (
        NO_NAME,
        Locale::Arabic,
        Template::Simple("يحتاج المستند إلى اسم."),
    ),
    (
        NO_SUCH_FILE,
        Locale::English,
        Template::Simple("There is no document {id}."),
    ),
    (
        NO_SUCH_FILE,
        Locale::Arabic,
        Template::Simple("لا يوجد مستند {id}."),
    ),
    (
        ALREADY_REMOVED,
        Locale::English,
        Template::Simple("{id} has already been taken off."),
    ),
    (
        ALREADY_REMOVED,
        Locale::Arabic,
        Template::Simple("سبق رفع {id} من مكانه."),
    ),
    (
        UNKNOWN_OWNER,
        Locale::English,
        Template::Simple("{owner} is not something a document can be attached to."),
    ),
    (
        UNKNOWN_OWNER,
        Locale::Arabic,
        Template::Simple("{owner} ليس شيئًا يمكن إرفاق مستند به."),
    ),
    // **Refuses rather than dropping it**, the same call the sealing key makes.
    // A tenant told their contract uploaded when it went nowhere is worse served
    // than one told it did not.
    (
        NO_STORAGE,
        Locale::English,
        Template::Simple("This deployment has nowhere to keep files, so nothing was stored."),
    ),
    (
        NO_STORAGE,
        Locale::Arabic,
        Template::Simple("لا يوجد في هذا النظام مكان لحفظ الملفات، فلم يُحفظ شيء."),
    ),
    (
        NOT_A_MEDIA_TYPE,
        Locale::English,
        Template::Simple("{media_type} is not a media type this system will take."),
    ),
    (
        NOT_A_MEDIA_TYPE,
        Locale::Arabic,
        Template::Simple("{media_type} ليس نوعًا يقبله هذا النظام."),
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
