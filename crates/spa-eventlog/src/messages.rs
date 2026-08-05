//! Message codes for the event log, in every supported language.

use spa_i18n::{Locale, MessageCode, Template};

pub const CONCURRENT_MODIFICATION: MessageCode =
    MessageCode::new("eventlog.concurrent_modification");
pub const INTERNAL: MessageCode = MessageCode::new("eventlog.internal_error");

pub static CODES: &[MessageCode] = &[CONCURRENT_MODIFICATION, INTERNAL];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    // Retryable, and worth telling the user so: someone else changed the same
    // record while they were working on it.
    (
        CONCURRENT_MODIFICATION,
        Locale::English,
        Template::Simple(
            "Someone else changed this while you were working on it. Please review and try again.",
        ),
    ),
    (
        CONCURRENT_MODIFICATION,
        Locale::Arabic,
        Template::Simple(
            "قام شخص آخر بتعديل هذا أثناء عملك عليه. يُرجى المراجعة والمحاولة مرة أخرى.",
        ),
    ),
    (
        INTERNAL,
        Locale::English,
        Template::Simple("Something went wrong on our side. The problem has been recorded."),
    ),
    (
        INTERNAL,
        Locale::Arabic,
        Template::Simple("حدث خطأ لدينا. تم تسجيل المشكلة."),
    ),
];
