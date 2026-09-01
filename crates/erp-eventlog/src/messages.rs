//! Message codes for the event log, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const CONCURRENT_MODIFICATION: MessageCode =
    MessageCode::new("eventlog.concurrent_modification");
pub const INTERNAL: MessageCode = MessageCode::new("eventlog.internal_error");
/// A create landed on an identifier that is taken, under a different request.
///
/// Never a retry: `try_create` reports those as success. Reaching this means two
/// different things were given one name, and the second is refused rather than
/// silently dropped.
pub const ALREADY_EXISTS: MessageCode = MessageCode::new("eventlog.already_exists");

pub static CODES: &[MessageCode] = &[CONCURRENT_MODIFICATION, INTERNAL, ALREADY_EXISTS];

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
        ALREADY_EXISTS,
        Locale::English,
        Template::Simple(
            "Something else already exists under that identifier. This is not the same request that created it, so it has not been saved.",
        ),
    ),
    (
        ALREADY_EXISTS,
        Locale::Arabic,
        Template::Simple(
            "يوجد شيء آخر بالفعل تحت هذا المعرّف. هذا ليس نفس الطلب الذي أنشأه، فلم يتم الحفظ.",
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
