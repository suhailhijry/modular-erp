//! What each kind says, in both languages.
//!
//! # Why this is compiled in and not a template
//!
//! Nobody writes a template for *a booking arrived*. A reminder is a message a
//! business composes on purpose; this is the system saying something happened,
//! and asking a tenant to author it before the bell works at all would ship a
//! feature that does nothing until somebody types.
//!
//! **Both languages, neither a translation of the other** (D12) — the same rule
//! a template lives by, and the reason this is a table of two rows per kind
//! rather than an English string somebody localises later.
//!
//! # And why a tenant can still override it
//!
//! An active `messaging` template named after the kind, on the `in_system`
//! channel, wins. Its bindings are validated when it is saved, so a business
//! that wants their own words gets the same save-time guarantees every other
//! template has. See [`crate::announce`].
//!
//! # Not in the i18n catalogue
//!
//! [`crate::messages`] is audited into `docs/ERRORS.md`. A notification is not
//! an error, and putting it there would list "a booking arrived" among the
//! failures a client can branch on.

use erp_i18n::Locale;

use crate::Kind;

/// A title and a body for one kind in one language, before rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Copy {
    pub title: &'static str,
    pub body: &'static str,
}

/// What each kind says, per language.
///
/// The `{{ … }}` names are the same vocabulary a template may use for that
/// kind's topic — `every_kind_says_only_what_can_be_resolved` checks it, so a
/// name that would render as braces in front of somebody is a build failure.
const COPY: &[(Kind, Locale, Copy)] = &[
    (
        Kind::BookingReserved,
        Locale::English,
        Copy {
            title: "New booking",
            body: "{{ customer.name }} booked {{ reservation.starts_at }}.",
        },
    ),
    (
        Kind::BookingReserved,
        Locale::Arabic,
        Copy {
            title: "حجز جديد",
            body: "حجز {{ customer.name }} في {{ reservation.starts_at }}.",
        },
    ),
    (
        Kind::PaymentsSettled,
        Locale::English,
        Copy {
            title: "Payment received",
            body: "{{ customer.name }} paid against {{ invoice.number }}.",
        },
    ),
    (
        Kind::PaymentsSettled,
        Locale::Arabic,
        Copy {
            title: "تم استلام دفعة",
            body: "دفع {{ customer.name }} على الفاتورة {{ invoice.number }}.",
        },
    ),
    (
        Kind::PaymentsFailed,
        Locale::English,
        Copy {
            title: "Payment failed",
            body: "A payment against {{ invoice.number }} did not go through.",
        },
    ),
    (
        Kind::PaymentsFailed,
        Locale::Arabic,
        Copy {
            title: "فشلت عملية دفع",
            body: "لم تنجح عملية دفع على الفاتورة {{ invoice.number }}.",
        },
    ),
    (
        Kind::TaxRefused,
        Locale::English,
        Copy {
            title: "ZATCA refused a document",
            body: "ZATCA refused the document for {{ invoice.number }}. It has to be corrected and resubmitted.",
        },
    ),
    (
        Kind::TaxRefused,
        Locale::Arabic,
        Copy {
            title: "هيئة الزكاة رفضت مستندًا",
            body: "رفضت هيئة الزكاة والضريبة والجمارك مستند الفاتورة {{ invoice.number }}. يلزم تصحيحه وإعادة إرساله.",
        },
    ),
    (
        Kind::DocumentExpiring,
        Locale::English,
        Copy {
            title: "A work document is expiring",
            body: "A work document held by {{ employee.name }} is about to expire.",
        },
    ),
    (
        Kind::DocumentExpiring,
        Locale::Arabic,
        Copy {
            title: "وثيقة عمل على وشك الانتهاء",
            body: "وثيقة عمل يحملها {{ employee.name }} على وشك الانتهاء.",
        },
    ),
];

/// What this kind says in one language.
///
/// Falls back to [`Locale::DEFAULT`] rather than returning nothing: a kind
/// added without its Arabic would otherwise announce an empty bell, and the
/// test below is what stops that reaching a build in the first place.
#[must_use]
pub fn of(kind: Kind, locale: Locale) -> Copy {
    find(kind, locale)
        .or_else(|| find(kind, Locale::DEFAULT))
        .unwrap_or(Copy {
            title: "",
            body: "",
        })
}

fn find(kind: Kind, locale: Locale) -> Option<Copy> {
    COPY.iter()
        .find(|(k, l, _)| *k == kind && *l == locale)
        .map(|(_, _, copy)| *copy)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every kind, in every language.**
    ///
    /// A kind whose Arabic is missing announces English to somebody who asked
    /// for Arabic, which is this system telling them it does not really speak
    /// it — the same rule that refuses a template with one body.
    #[test]
    fn every_kind_has_both_languages() {
        for kind in Kind::ALL {
            for locale in Locale::ALL {
                let copy = find(kind, locale)
                    .unwrap_or_else(|| panic!("{kind} has nothing to say in {}", locale.code()));
                assert!(!copy.title.trim().is_empty(), "{kind} has no title");
                assert!(!copy.body.trim().is_empty(), "{kind} has no body");
            }
        }
    }

    /// **A placeholder that cannot be resolved renders as braces in front of
    /// whoever the notification is for.**
    ///
    /// `messaging` already refuses that when a template is saved. This is the
    /// same check for the copy compiled in here, where there is no save.
    #[test]
    fn every_kind_says_only_what_can_be_resolved() {
        for kind in Kind::ALL {
            let vocabulary = messaging::template::vocabulary(kind.topic());
            for locale in Locale::ALL {
                let copy = of(kind, locale);
                for text in [copy.title, copy.body] {
                    for placeholder in messaging::template::placeholders(text) {
                        assert!(
                            vocabulary.contains(&placeholder.as_str()),
                            "{kind} says {{{{ {placeholder} }}}}, which nothing about {} can answer",
                            kind.topic().as_str()
                        );
                    }
                }
            }
        }
    }
}
