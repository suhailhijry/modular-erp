//! **The error-code reference, generated and checked.**
//!
//! A client of this API branches on `code`, never on `detail` — that is the
//! contract, and there are around fifty of them spread across five crates. Until
//! now the only way to find out what they are was to read Rust.
//!
//! So the list is generated from the catalog the API actually renders from, and
//! this test fails when `docs/ERRORS.md` no longer matches it. Regenerate with:
//!
//! ```text
//! just errors
//! ```
//!
//! # Why generate rather than write
//!
//! A hand-written list of error codes is wrong within a month, and wrong in the
//! direction that costs an integrator a day: a code that exists and is not
//! documented looks like a bug in their client. Generating it means the document
//! cannot claim a code that does not exist, and the drift check means the
//! codebase cannot grow one the document does not mention.

#![allow(clippy::expect_used)]

use std::fmt::Write as _;

use spa_i18n::{Catalog, Locale, Template};

const REFERENCE: &str = "../../docs/ERRORS.md";

/// Renders every code the API can answer with, in both languages.
fn reference() -> String {
    let mut codes: Vec<_> = spa_api::CATALOG.codes().to_vec();
    codes.sort_unstable();
    codes.dedup();

    let mut out = String::from(
        "# Error codes\n\
         \n\
         Every `code` this API can answer with. **Branch on the code, never on\n\
         `detail`** — the detail is prose in whichever language the request asked\n\
         for, and it changes; the code does not.\n\
         \n\
         Every error is `application/problem+json` (RFC 9457) with `code`,\n\
         `detail`, and `args` carrying the values the message names. `args` is\n\
         where a client gets the machine-readable version of what went wrong —\n\
         which account, which module, how much was outstanding.\n\
         \n\
         <!-- Generated from the message catalog by `crates/spa-api/tests/errors.rs`.\n\
         Run `just errors` after adding a code; CI fails if this drifts. -->\n\
         \n",
    );

    let mut prefix = String::new();
    for code in codes {
        let namespace = code
            .as_str()
            .split_once('.')
            .map_or("other", |(head, _)| head)
            .to_owned();

        if namespace != prefix {
            let _ = write!(out, "\n## `{namespace}`\n\n");
            prefix = namespace;
        }

        let _ = write!(out, "### `{}`\n\n", code.as_str());
        for locale in Locale::ALL {
            let template = spa_api::CATALOG
                .template(locale, &code)
                .expect("every code is translated; `assert_complete` proves it");
            let _ = writeln!(out, "- **{}** — {}", label(locale), sample(&template));
        }
        out.push('\n');
    }

    out
}

const fn label(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "en",
        Locale::Arabic => "ar",
    }
}

/// One representative rendering. Plurals show their `other` form, which is the
/// one every language selects for some count.
fn sample(template: &Template) -> &'static str {
    match template {
        Template::Simple(text) => text,
        Template::Plural { other, .. } => other,
    }
}

/// **The drift check.** A code that exists and is not documented looks, to
/// somebody integrating against this, like a bug in their own client.
#[test]
fn the_error_reference_matches_the_catalog() {
    let generated = reference();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(REFERENCE);

    if std::env::var("REGENERATE_DOCS").is_ok() {
        std::fs::write(&path, &generated).expect("writes the reference");
        return;
    }

    let current = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        current, generated,
        "docs/ERRORS.md is out of date. Run `just errors`."
    );
}

/// The reference is only worth having if it is complete, and completeness is
/// what `assert_complete` already checks — this asserts the *document* saw
/// every code, which is a different claim.
#[test]
fn every_code_reaches_the_reference() {
    let generated = reference();
    for code in spa_api::CATALOG.codes() {
        assert!(
            generated.contains(code.as_str()),
            "{} is missing from the reference",
            code.as_str()
        );
    }

    // Not vacuous: something that is not a code must not be found either.
    assert!(!generated.contains("request.not_a_real_code"));
}
