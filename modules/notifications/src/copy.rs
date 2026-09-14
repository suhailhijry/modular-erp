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

use std::collections::BTreeMap;

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
///
/// **A kind may have more than one sentence in a language**, in order: [`of`]
/// takes the first whose every name the subject answers. A lot at no branch has
/// no branch to name, and the wording is frozen when it is announced, so a
/// sentence that asked for one would say `{{ branch.name }}` for ever.
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
    (
        Kind::StockExpiring,
        Locale::English,
        Copy {
            title: "Stock is about to expire",
            body: "{{ product.name }}, batch {{ lot.code }} at {{ branch.name }}, is good until {{ lot.expires_on }}.",
        },
    ),
    (
        Kind::StockExpiring,
        Locale::Arabic,
        Copy {
            title: "مخزون على وشك انتهاء الصلاحية",
            body: "{{ product.name }}، الدفعة {{ lot.code }} في {{ branch.name }}، صالحة حتى {{ lot.expires_on }}.",
        },
    ),
    (
        Kind::StockExpired,
        Locale::English,
        Copy {
            title: "Stock is past its date",
            body: "{{ product.name }}, batch {{ lot.code }} at {{ branch.name }}, was good until {{ lot.expires_on }} and is still on the shelf. Write off what cannot be sold or sent back.",
        },
    ),
    (
        Kind::StockExpired,
        Locale::Arabic,
        Copy {
            title: "مخزون انتهت صلاحيته",
            body: "{{ product.name }}، الدفعة {{ lot.code }} في {{ branch.name }}، كانت صالحة حتى {{ lot.expires_on }} وما زالت على الرف. اشطب من المخزون ما لا يمكن بيعه أو إرجاعه.",
        },
    ),
    // **The same, for a lot at no branch**: a business with one shelf.
    (
        Kind::StockExpiring,
        Locale::English,
        Copy {
            title: "Stock is about to expire",
            body: "{{ product.name }}, batch {{ lot.code }}, is good until {{ lot.expires_on }}.",
        },
    ),
    (
        Kind::StockExpiring,
        Locale::Arabic,
        Copy {
            title: "مخزون على وشك انتهاء الصلاحية",
            body: "{{ product.name }}، الدفعة {{ lot.code }}، صالحة حتى {{ lot.expires_on }}.",
        },
    ),
    (
        Kind::StockExpired,
        Locale::English,
        Copy {
            title: "Stock is past its date",
            body: "{{ product.name }}, batch {{ lot.code }}, was good until {{ lot.expires_on }} and is still on the shelf. Write off what cannot be sold or sent back.",
        },
    ),
    (
        Kind::StockExpired,
        Locale::Arabic,
        Copy {
            title: "مخزون انتهت صلاحيته",
            body: "{{ product.name }}، الدفعة {{ lot.code }}، كانت صالحة حتى {{ lot.expires_on }} وما زالت على الرف. اشطب من المخزون ما لا يمكن بيعه أو إرجاعه.",
        },
    ),
];

/// What this kind says in one language, about a subject `values` describes:
/// **the first of its sentences whose every name `values` answers**, or its
/// first sentence when none is.
///
/// Falls back to [`Locale::DEFAULT`] rather than returning nothing: a kind
/// added without its Arabic would otherwise announce an empty bell, and the
/// test below is what stops that reaching a build in the first place.
#[must_use]
pub fn of(kind: Kind, locale: Locale, values: &BTreeMap<String, String>) -> Copy {
    let locale = if find(kind, locale).is_some() {
        locale
    } else {
        Locale::DEFAULT
    };
    let answered = |copy: &Copy| {
        [copy.title, copy.body].into_iter().all(|text| {
            messaging::template::placeholders(text)
                .iter()
                .all(|name| values.contains_key(name))
        })
    };
    COPY.iter()
        .filter(|(k, l, _)| *k == kind && *l == locale)
        .map(|(_, _, copy)| *copy)
        .find(answered)
        .or_else(|| find(kind, locale))
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
            for (_, _, copy) in COPY.iter().filter(|(k, _, _)| *k == kind) {
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
