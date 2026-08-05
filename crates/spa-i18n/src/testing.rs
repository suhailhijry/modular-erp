//! Reusable localization checks.
//!
//! Every crate that owns a catalog runs these, so the guarantee is uniform
//! rather than reimplemented — and when the `Module` trait gains a `messages()`
//! method, the registry can run them across every module at once, making it
//! impossible to ship a module without translations.

use crate::{Catalog, Locale, Message, Template};

/// Every problem found in a catalog. Empty means it is sound.
///
/// Returns all of them rather than the first, so one test run fixes one
/// translation pass.
#[must_use]
pub fn audit(catalog: &impl Catalog) -> Vec<String> {
    let mut problems = Vec::new();

    for code in catalog.codes() {
        for locale in Locale::ALL {
            let Some(template) = catalog.template(locale, code) else {
                problems.push(format!("{code}: no {} translation", locale.code()));
                continue;
            };

            // A plural template must cover every category its own language
            // selects — two for English, six for Arabic. A gap falls back to
            // `other` and reads as broken grammar to a native speaker.
            if matches!(template, Template::Plural { .. }) {
                for category in locale.plural_categories() {
                    if template.variant(*category).is_none() {
                        problems.push(format!(
                            "{code} ({}): missing plural form {category:?}",
                            locale.code()
                        ));
                    }
                }
            }

            let rendered = catalog
                .render(locale, &Message::new(code.clone()))
                .unwrap_or_default();
            if rendered.trim().is_empty() {
                problems.push(format!("{code} ({}): renders empty", locale.code()));
            }

            // Arabic that contains no Arabic is a copy-pasted English row —
            // the easiest mistake to make when filling in a catalog.
            if locale == Locale::Arabic
                && !rendered
                    .chars()
                    .any(|c| ('\u{0600}'..='\u{06FF}').contains(&c))
            {
                problems.push(format!("{code} (ar): no Arabic script in the translation"));
            }
        }

        // Codes are part of the API contract — integrators branch on them.
        let text = code.as_str();
        if !text.contains('.') {
            problems.push(format!("{text}: should be namespaced as `domain.thing`"));
        }
        if !text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'.' || b == b'_')
        {
            problems.push(format!("{text}: should be lowercase with underscores"));
        }
    }

    problems
}

/// Panics with every problem listed. The one-liner a crate's test calls.
pub fn assert_complete(catalog: &impl Catalog) {
    let problems = audit(catalog);
    assert!(
        problems.is_empty(),
        "catalog is incomplete:\n  {}",
        problems.join("\n  ")
    );
}
