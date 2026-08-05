//! Localization completeness and correctness.
//!
//! Saudi Arabia is the first market, so a missing Arabic string must fail the
//! build rather than ship as English. That is what most of this file does.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use spa_control::{AccessError, CATALOG, Lane, PoolError, TenantStatus, messages};
use spa_i18n::{Catalog, Locale, Localize, Message, MessageArg, MessageCode, Template};

/// **The load-bearing test.** Every code, in every language, or the build fails.
///
/// Without this, "we'll translate it later" becomes English leaking into an
/// Arabic UI, discovered by a customer.
#[test]
fn every_code_is_translated_into_every_language() {
    let mut missing = Vec::new();

    for code in CATALOG.codes() {
        for locale in Locale::ALL {
            if CATALOG.template(locale, code).is_none() {
                missing.push(format!("{code} has no {} translation", locale.code()));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "untranslated messages:\n  {}",
        missing.join("\n  ")
    );
}

/// A plural template must cover every category its own language selects — two
/// for English, six for Arabic. A gap would silently fall back to `other` and
/// read as broken grammar to a native speaker.
#[test]
fn plural_templates_cover_every_category_their_language_uses() {
    let mut gaps = Vec::new();

    for code in CATALOG.codes() {
        for locale in Locale::ALL {
            let Some(template @ Template::Plural { .. }) = CATALOG.template(locale, code) else {
                continue;
            };
            for category in locale.plural_categories() {
                if template.variant(*category).is_none() {
                    gaps.push(format!(
                        "{code} ({}) is missing {category:?}",
                        locale.code()
                    ));
                }
            }
        }
    }

    assert!(
        gaps.is_empty(),
        "incomplete plural templates:\n  {}",
        gaps.join("\n  ")
    );
}

/// Nothing renders as an empty string or as its own code — either would reach a
/// user as a blank space or as `access.denied`.
#[test]
fn every_message_renders_to_real_prose() {
    for code in CATALOG.codes() {
        for locale in Locale::ALL {
            let rendered = CATALOG.render_or_code(locale, &Message::new(code.clone()));
            assert!(
                !rendered.trim().is_empty(),
                "{code} ({}) renders empty",
                locale.code()
            );
            assert_ne!(
                rendered,
                code.as_str(),
                "{code} ({}) fell through to its own code",
                locale.code()
            );
        }
    }
}

/// Arabic strings must actually be Arabic. Copy-pasting the English row while
/// filling in a catalog is the easiest possible mistake, and this catches it.
#[test]
fn arabic_translations_are_in_arabic_script() {
    for code in CATALOG.codes() {
        let rendered = CATALOG.render_or_code(Locale::Arabic, &Message::new(code.clone()));
        assert!(
            rendered
                .chars()
                .any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)),
            "{code} has no Arabic characters in its Arabic translation: {rendered:?}"
        );
    }
}

/// Every error variant maps to a code the catalog knows about. A typo'd code
/// would otherwise surface to a user as the raw string.
#[test]
fn every_error_variant_maps_to_a_known_code() {
    let errors: Vec<AccessError> = vec![
        AccessError::NoSuchIdentity,
        AccessError::IdentitySuspended,
        AccessError::NoSuchTenant,
        AccessError::NotAMember,
        AccessError::TenantNotActive {
            status: TenantStatus::Provisioning,
        },
        AccessError::TenantNotActive {
            status: TenantStatus::Suspended,
        },
        AccessError::TenantNotActive {
            status: TenantStatus::Deleted,
        },
        AccessError::Pool(PoolError::Overloaded { lane: Lane::Client }),
        AccessError::Pool(PoolError::UnknownCluster("nope".into())),
        AccessError::Corrupt("bad row".into()),
        AccessError::NoCapacity {
            clusters_at_limit: 3,
        },
        AccessError::SlugTaken("acme".into()),
    ];

    for error in errors {
        let message = error.message();
        for locale in Locale::ALL {
            assert!(
                CATALOG.template(locale, &message.code).is_some(),
                "{error:?} produced {} which has no {} translation",
                message.code,
                locale.code()
            );
        }
    }
}

/// Distinguishing "no such tenant" from "not a member" hands an attacker a
/// tenant-enumeration oracle. They must be indistinguishable in what a user sees.
#[test]
fn tenant_existence_is_not_leaked_by_the_error_message() {
    let no_tenant = AccessError::NoSuchTenant.message();
    let not_member = AccessError::NotAMember.message();

    assert_eq!(
        no_tenant.code, not_member.code,
        "a distinct code for these two lets an attacker enumerate tenants"
    );
    for locale in Locale::ALL {
        assert_eq!(
            CATALOG.render_or_code(locale, &no_tenant),
            CATALOG.render_or_code(locale, &not_member),
        );
    }
}

/// Capacity is an operational fact, not something a signup form should report.
#[test]
fn a_user_is_not_told_about_our_cluster_topology() {
    let message = AccessError::NoCapacity {
        clusters_at_limit: 7,
    }
    .message();

    for locale in Locale::ALL {
        let rendered = CATALOG.render_or_code(locale, &message);
        assert!(
            !rendered.contains('7'),
            "the cluster count leaked to a user: {rendered:?}"
        );
        for word in ["cluster", "مجموعة", "مجموعات"] {
            assert!(
                !rendered.contains(word),
                "{word:?} leaked into a user-facing message: {rendered:?}"
            );
        }
    }
}

/// Internal failures never describe themselves to a user.
#[test]
fn internal_failures_do_not_leak_detail() {
    let message = AccessError::Corrupt("column tenant.status held 'wat'".into()).message();
    assert_eq!(message.code, messages::INTERNAL);

    for locale in Locale::ALL {
        let rendered = CATALOG.render_or_code(locale, &message);
        assert!(
            !rendered.contains("wat") && !rendered.contains("column"),
            "internal detail leaked into a user-facing message: {rendered:?}"
        );
    }
}

/// The Arabic plural path, end to end, on a real message.
#[test]
fn arabic_plurals_select_correctly_on_a_real_message() {
    let render = |n: i64| {
        CATALOG.render_or_code(
            Locale::Arabic,
            &Message::new(messages::CLUSTERS_AT_LIMIT).with("n", MessageArg::Count(n)),
        )
    };

    // Six distinct forms, chosen by CLDR's rules rather than by n == 1.
    assert!(render(0).contains("لا توجد مجموعات"));
    assert!(render(1).contains("واحدة"));
    assert!(render(2).contains("مجموعتان"));
    assert!(render(3).contains("مجموعات"));
    assert!(render(11).contains("مجموعةً"));
    assert!(render(100).contains("مجموعة"));

    // 103 is `few` and 111 is `many`, which is the case an ad-hoc rule misses.
    assert_eq!(
        render(103).replace("103", "3"),
        render(3),
        "103 must take the same form as 3"
    );
    assert_eq!(
        render(111).replace("111", "11"),
        render(11),
        "111 must take the same form as 11"
    );
}

/// A Latin identifier inside Arabic text has to be bidi-isolated or the sentence
/// renders scrambled around it.
#[test]
fn latin_identifiers_are_isolated_inside_arabic_text() {
    let message =
        Message::new(messages::SLUG_TAKEN).with("slug", MessageArg::text("acme-trading-co"));

    let arabic = CATALOG.render_or_code(Locale::Arabic, &message);
    assert!(
        arabic.contains('\u{2068}') && arabic.contains('\u{2069}'),
        "the Latin run must be wrapped in FSI/PDI: {arabic:?}"
    );

    let english = CATALOG.render_or_code(Locale::English, &message);
    assert!(
        !english.contains('\u{2068}'),
        "isolation marks must not appear in LTR output: {english:?}"
    );
    assert!(english.contains("acme-trading-co"));
}

/// Codes are part of the API contract — integrators branch on them. This test
/// pins the wire format so a rename is a deliberate, visible change.
#[test]
fn codes_are_stable_and_well_formed() {
    for code in CATALOG.codes() {
        let text = code.as_str();
        assert!(
            text.contains('.'),
            "{text} should be namespaced as `domain.thing`"
        );
        assert!(
            text.bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'.' || b == b'_'),
            "{text} should be lowercase with underscores"
        );
    }

    // A spot-check that these specific strings have not drifted.
    assert_eq!(messages::ACCESS_DENIED, MessageCode::new("access.denied"));
    assert_eq!(messages::OVERLOADED, MessageCode::new("system.overloaded"));
}
