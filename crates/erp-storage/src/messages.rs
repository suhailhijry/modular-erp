//! The storage layer's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NO_SUCH_FILE: MessageCode = MessageCode::new("storage.no_such_file");
pub const CORRUPT: MessageCode = MessageCode::new("storage.corrupt");
pub const TOO_LARGE: MessageCode = MessageCode::new("storage.too_large");
pub const NOT_A_KEY: MessageCode = MessageCode::new("storage.not_a_key");
pub const UNAVAILABLE: MessageCode = MessageCode::new("storage.unavailable");

pub static CODES: &[MessageCode] = &[NO_SUCH_FILE, CORRUPT, TOO_LARGE, NOT_A_KEY, UNAVAILABLE];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NO_SUCH_FILE,
        Locale::English,
        Template::Simple("That file is not in storage."),
    ),
    (
        NO_SUCH_FILE,
        Locale::Arabic,
        Template::Simple("هذا الملف غير موجود في التخزين."),
    ),
    // **Not a warning.** A document that comes back different from what was
    // stored is a failure, and saying so is the whole reason a checksum is
    // recorded.
    (
        CORRUPT,
        Locale::English,
        Template::Simple(
            "That file came back different from what was stored, so it has not been given to you.",
        ),
    ),
    (
        CORRUPT,
        Locale::Arabic,
        Template::Simple("عاد هذا الملف مختلفًا عمّا خُزِّن، فلم يُسلَّم إليك."),
    ),
    (
        TOO_LARGE,
        Locale::English,
        Template::Plural {
            zero: None,
            one: Some("A file may not be larger than one byte."),
            two: None,
            few: None,
            many: None,
            other: "A file may not be larger than {n} bytes.",
        },
    ),
    (
        TOO_LARGE,
        Locale::Arabic,
        Template::Plural {
            zero: Some("لا يمكن رفع ملف بأي حجم."),
            one: Some("لا يمكن أن يتجاوز حجم الملف بايتًا واحدًا."),
            two: Some("لا يمكن أن يتجاوز حجم الملف بايتين."),
            few: Some("لا يمكن أن يتجاوز حجم الملف {n} بايتات."),
            many: Some("لا يمكن أن يتجاوز حجم الملف {n} بايتًا."),
            other: "لا يمكن أن يتجاوز حجم الملف {n} بايت.",
        },
    ),
    (
        NOT_A_KEY,
        Locale::English,
        Template::Simple("{key} is not somewhere a file may be kept."),
    ),
    (
        NOT_A_KEY,
        Locale::Arabic,
        Template::Simple("{key} ليس موضعًا يمكن حفظ ملف فيه."),
    ),
    (
        UNAVAILABLE,
        Locale::English,
        Template::Simple("Storage could not be reached. Try again."),
    ),
    (
        UNAVAILABLE,
        Locale::Arabic,
        Template::Simple("تعذّر الوصول إلى التخزين. أعد المحاولة."),
    ),
];
