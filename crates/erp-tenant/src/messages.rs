//! The message codes owned by this crate.
//!
//! Only the two that belong to types defined here: [`PoolError`](crate::PoolError)
//! renders as `OVERLOADED`, and anything a module cannot explain renders as
//! `INTERNAL`. Everything else a tenant is told belongs to the control plane or
//! to `erp-web`, and is composed alongside this at the edge.

use erp_i18n::{Locale, MessageCode, Template};

pub const OVERLOADED: MessageCode = MessageCode::new("system.overloaded");
pub const INTERNAL: MessageCode = MessageCode::new("system.internal_error");

pub static CODES: &[MessageCode] = &[OVERLOADED, INTERNAL];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        OVERLOADED,
        Locale::English,
        Template::Simple("The system is busy right now. Please try again in a moment."),
    ),
    (
        OVERLOADED,
        Locale::Arabic,
        Template::Simple("النظام مشغول حالياً. يُرجى المحاولة مرة أخرى بعد قليل."),
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

#[cfg(test)]
mod tests {
    #[test]
    fn every_code_is_translated_into_every_locale() {
        erp_i18n::testing::assert_complete(&super::super::CATALOG);
    }
}
