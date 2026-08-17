//! Message templates and rendering.

use std::collections::BTreeMap;

use crate::{Locale, Message, MessageArg, MessageCode, Plural};

/// Unicode bidi isolation, wrapped around Latin-script arguments in RTL text.
///
/// Without these, an Arabic sentence containing `1000` or `acme-corp` renders
/// with the surrounding words in the wrong order — the classic symptom being
/// punctuation jumping to the far end of the line. `FIRST STRONG ISOLATE` opens
/// a run whose direction is taken from its first strong character; `POP
/// DIRECTIONAL ISOLATE` closes it.
const FSI: char = '\u{2068}';
const PDI: char = '\u{2069}';

/// A message in one language.
///
/// Plural variants are separate fields rather than an inline `{n, plural, …}`
/// syntax: there is no parser to get wrong, and the shape of the data says which
/// forms a language needs.
///
/// Only `other` is mandatory — it is CLDR's universal fallback. The rest are
/// optional because **a language does not use every category**: English's rules
/// never select `zero`, so an English `zero` string would be unreachable text
/// that a translator wrote and nobody ever sees. Asking for it invites exactly
/// that mistake.
///
/// Enforcement lives where it belongs instead: `tests/completeness.rs` checks
/// that every plural template defines every category *its own locale actually
/// selects* — two for English, six for Arabic.
#[derive(Debug, Clone, Copy)]
pub enum Template {
    Simple(&'static str),
    Plural {
        zero: Option<&'static str>,
        one: Option<&'static str>,
        two: Option<&'static str>,
        few: Option<&'static str>,
        many: Option<&'static str>,
        /// CLDR's fallback. Every language selects it for some count.
        other: &'static str,
    },
}

impl Template {
    /// The variant for a category, or `None` if this template does not define it.
    #[must_use]
    pub const fn variant(self, plural: Plural) -> Option<&'static str> {
        match self {
            Self::Simple(text) => Some(text),
            Self::Plural {
                zero,
                one,
                two,
                few,
                many,
                other,
            } => match plural {
                Plural::Zero => zero,
                Plural::One => one,
                Plural::Two => two,
                Plural::Few => few,
                Plural::Many => many,
                Plural::Other => Some(other),
            },
        }
    }

    /// The variant for a category, falling back to `other` as CLDR specifies.
    const fn select(self, plural: Plural) -> &'static str {
        match self.variant(plural) {
            Some(text) => text,
            None => match self {
                Self::Simple(text) => text,
                Self::Plural { other, .. } => other,
            },
        }
    }
}

/// Renders messages into a language.
///
/// `Send + Sync` because every catalog in this system is a `static` shared by
/// every request, and because [`Composite`] holds its parts as trait objects.
pub trait Catalog: Send + Sync {
    /// The template for a code, or `None` if this catalog has no translation.
    fn template(&self, locale: Locale, code: &MessageCode) -> Option<Template>;

    /// Every code this catalog claims to translate. Drives the completeness
    /// test — a catalog that cannot enumerate itself cannot be checked.
    fn codes(&self) -> &'static [MessageCode];

    /// Renders a message, or `None` when the code is untranslated.
    ///
    /// Callers should prefer [`Catalog::render_or_code`], which degrades to
    /// something diagnosable rather than nothing.
    fn render(&self, locale: Locale, message: &Message) -> Option<String> {
        let template = self.template(locale, &message.code)?;
        let plural = message
            .count()
            .map_or(Plural::Other, |count| locale.plural(count));
        Some(interpolate(template.select(plural), &message.args, locale))
    }

    /// Renders, falling back through the default locale to the bare code.
    ///
    /// # The deliberate exception to law L6
    ///
    /// Everywhere else, failures stop rather than degrade. Here they degrade —
    /// and warn. Refusing to serve a response because one string is untranslated
    /// would turn a cosmetic gap into an outage, which is plainly the wrong
    /// trade: a Saudi user reading one English sentence is inconvenienced, a
    /// Saudi user reading a 500 is blocked.
    ///
    /// The gap is still caught, just earlier: `tests/localization.rs` fails the
    /// build for any code missing a translation. So the two mechanisms are
    /// complementary — CI is where a missing string is *found*, this is what
    /// happens if one ever slips past it. Reaching either fallback emits a
    /// `warn`, because a silent fallback is how English quietly becomes the
    /// Arabic experience.
    fn render_or_code(&self, locale: Locale, message: &Message) -> String {
        if let Some(rendered) = self.render(locale, message) {
            return rendered;
        }

        if locale != Locale::DEFAULT
            && let Some(rendered) = self.render(Locale::DEFAULT, message)
        {
            tracing::warn!(
                code = %message.code,
                requested = locale.code(),
                served = Locale::DEFAULT.code(),
                "no translation; served the default language instead"
            );
            return rendered;
        }

        // Untranslated in every language. Showing the code is diagnosable in a
        // bug report; an empty string is not.
        tracing::error!(
            code = %message.code,
            requested = locale.code(),
            "message has no translation in any language; showing the raw code"
        );
        message.code.as_str().to_owned()
    }
}

/// Substitutes `{name}` placeholders.
///
/// Unknown placeholders are left verbatim rather than blanked, so a typo in a
/// template shows up as `{acount}` in the output instead of a hole nobody
/// notices.
fn interpolate(template: &str, args: &BTreeMap<String, MessageArg>, locale: Locale) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;

    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            // Unbalanced brace: emit the remainder literally rather than
            // silently truncating the message.
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        if let Some(arg) = args.get(name) {
            out.push_str(&format_arg(arg, locale));
        } else {
            out.push('{');
            out.push_str(name);
            out.push('}');
        }
        rest = &after[end + 1..];
    }

    out.push_str(rest);
    out
}

fn format_arg(arg: &MessageArg, locale: Locale) -> String {
    match arg {
        MessageArg::Text(text) => {
            if locale.is_rtl() {
                // Latin-script identifiers embedded in Arabic need isolating or
                // they reorder the text around them.
                format!("{FSI}{text}{PDI}")
            } else {
                text.clone()
            }
        }
        MessageArg::Int(n) | MessageArg::Count(n) => {
            if locale.is_rtl() {
                format!("{FSI}{n}{PDI}")
            } else {
                n.to_string()
            }
        }
    }
}

/// A catalog compiled into the binary.
///
/// Per-tenant terminology overrides — "client" versus "patient" versus "guest" —
/// arrive with the configuration system in Phase 3 and layer on top of this.
/// These are the defaults, and they always exist.
#[derive(Debug)]
pub struct StaticCatalog {
    entries: &'static [(MessageCode, Locale, Template)],
    codes: &'static [MessageCode],
}

impl StaticCatalog {
    #[must_use]
    pub const fn new(
        entries: &'static [(MessageCode, Locale, Template)],
        codes: &'static [MessageCode],
    ) -> Self {
        Self { entries, codes }
    }
}

impl Catalog for StaticCatalog {
    fn template(&self, locale: Locale, code: &MessageCode) -> Option<Template> {
        self.entries
            .iter()
            .find(|(c, l, _)| c == code && *l == locale)
            .map(|(_, _, template)| *template)
    }

    fn codes(&self) -> &'static [MessageCode] {
        self.codes
    }
}

/// Several catalogs behind one lookup.
///
/// A crate renders messages from itself and from everything it is built on —
/// a module's route answers with its own failures, the control plane's, and the
/// request-level ones — and there is no single catalog that holds all three.
/// This is that union, and it is `const`, so it can be a `static`.
///
/// "First part that has the code" is unambiguous because codes are globally
/// unique by their `domain.` prefix. A duplicate would make the answer depend on
/// the order of the parts, which is what `no_two_crates_claim_the_same_code`
/// exists to catch.
///
/// Parts are `&dyn Catalog` so a composite can hold another one. Each layer of
/// the build then names only what it can see — `spa-web` unions the request and
/// kernel catalogs, a module adds its own to that, `spa-api` adds every
/// module's — and no layer has to repeat the layer below it.
pub struct Composite {
    parts: &'static [&'static dyn Catalog],
    codes: std::sync::OnceLock<&'static [MessageCode]>,
}

impl Composite {
    #[must_use]
    pub const fn new(parts: &'static [&'static dyn Catalog]) -> Self {
        Self {
            parts,
            codes: std::sync::OnceLock::new(),
        }
    }
}

impl std::fmt::Debug for Composite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // How many, not which: a part is `dyn Catalog`, and the flattened code
        // list is a cache nobody debugging this wants printed.
        f.debug_struct("Composite")
            .field("parts", &self.parts.len())
            .finish_non_exhaustive()
    }
}

impl Catalog for Composite {
    fn template(&self, locale: Locale, code: &MessageCode) -> Option<Template> {
        self.parts.iter().find_map(|c| c.template(locale, code))
    }

    /// Concatenated once and leaked.
    ///
    /// The signature wants `&'static`, and the parts cannot be flattened at
    /// compile time. The alternative — returning an empty slice — would make
    /// every completeness audit silently pass, which is worse than one leak of a
    /// few hundred codes per process.
    fn codes(&self) -> &'static [MessageCode] {
        self.codes.get_or_init(|| {
            Box::leak(
                self.parts
                    .iter()
                    .flat_map(|c| c.codes().iter().cloned())
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GREETING: MessageCode = MessageCode::new("test.greeting");
    const ITEMS: MessageCode = MessageCode::new("test.items");
    const UNTRANSLATED: MessageCode = MessageCode::new("test.untranslated");

    static CODES: &[MessageCode] = &[GREETING, ITEMS];

    static ENTRIES: &[(MessageCode, Locale, Template)] = &[
        (
            GREETING,
            Locale::English,
            Template::Simple("account {code} was not found"),
        ),
        (
            GREETING,
            Locale::Arabic,
            Template::Simple("لم يتم العثور على الحساب {code}"),
        ),
        (
            ITEMS,
            Locale::English,
            // English selects only `one` and `other`. Writing the other four
            // would be text nobody ever sees.
            Template::Plural {
                zero: None,
                one: Some("{n} item"),
                two: None,
                few: None,
                many: None,
                other: "{n} items",
            },
        ),
        (
            ITEMS,
            Locale::Arabic,
            Template::Plural {
                zero: Some("لا عناصر"),
                one: Some("عنصر واحد"),
                two: Some("عنصران"),
                few: Some("{n} عناصر"),
                many: Some("{n} عنصرًا"),
                other: "{n} عنصر",
            },
        ),
    ];

    fn catalog() -> StaticCatalog {
        StaticCatalog::new(ENTRIES, CODES)
    }

    #[test]
    fn a_simple_message_interpolates() {
        let msg = Message::new(GREETING).with("code", MessageArg::text("1000"));
        assert_eq!(
            catalog().render(Locale::English, &msg).unwrap(),
            "account 1000 was not found"
        );
    }

    /// The detail almost every system gets wrong: a Latin identifier dropped
    /// into Arabic without isolation reorders the sentence around it.
    #[test]
    fn latin_arguments_are_bidi_isolated_in_arabic() {
        let msg = Message::new(GREETING).with("code", MessageArg::text("4000.01"));
        let rendered = catalog().render(Locale::Arabic, &msg).unwrap();

        assert!(
            rendered.contains(&format!("{FSI}4000.01{PDI}")),
            "the Latin run must be isolated, got: {rendered:?}"
        );
        // And English must not carry the marks — they would be noise in logs
        // and in any consumer that does not expect them.
        let english = catalog().render(Locale::English, &msg).unwrap();
        assert!(!english.contains(FSI));
    }

    #[test]
    fn arabic_selects_among_all_six_plural_forms() {
        let render = |n: i64| {
            catalog()
                .render(
                    Locale::Arabic,
                    &Message::new(ITEMS).with("n", MessageArg::Count(n)),
                )
                .unwrap()
        };

        assert_eq!(render(0), "لا عناصر");
        assert_eq!(render(1), "عنصر واحد");
        assert_eq!(render(2), "عنصران");
        assert!(render(3).contains("عناصر"), "3 should take the `few` form");
        assert!(render(11).contains("عنصرًا"), "11 should take `many`");
        assert!(render(100).contains("عنصر"), "100 should take `other`");
    }

    #[test]
    fn english_collapses_to_two_forms() {
        let render = |n: i64| {
            catalog()
                .render(
                    Locale::English,
                    &Message::new(ITEMS).with("n", MessageArg::Count(n)),
                )
                .unwrap()
        };
        // English has no `zero` category: 0 takes `other`. Getting this wrong
        // is how an unreachable "no items" string ends up in a catalog.
        assert_eq!(render(0), "0 items");
        assert_eq!(render(1), "1 item");
        assert_eq!(render(2), "2 items");
        assert_eq!(render(11), "11 items");
    }

    #[test]
    fn an_unknown_code_degrades_to_something_diagnosable() {
        let msg = Message::new(UNTRANSLATED);
        assert_eq!(catalog().render(Locale::Arabic, &msg), None);
        assert_eq!(
            catalog().render_or_code(Locale::Arabic, &msg),
            "test.untranslated",
            "an untranslated code must surface itself, not an empty string"
        );
    }

    #[test]
    fn a_missing_translation_falls_back_to_the_default_locale() {
        static PARTIAL: &[(MessageCode, Locale, Template)] =
            &[(GREETING, Locale::English, Template::Simple("english only"))];
        let partial = StaticCatalog::new(PARTIAL, CODES);
        assert_eq!(
            partial.render_or_code(Locale::Arabic, &Message::new(GREETING)),
            "english only"
        );
    }

    #[test]
    fn a_placeholder_with_no_argument_stays_visible() {
        // Silently blanking it would hide the bug; leaving it shows up in a
        // screenshot and gets reported.
        let rendered = catalog()
            .render(Locale::English, &Message::new(GREETING))
            .unwrap();
        assert_eq!(rendered, "account {code} was not found");
    }

    #[test]
    fn malformed_templates_do_not_panic_or_truncate() {
        let args = BTreeMap::new();
        assert_eq!(
            interpolate("unbalanced {brace", &args, Locale::English),
            "unbalanced {brace"
        );
        assert_eq!(interpolate("", &args, Locale::English), "");
        assert_eq!(interpolate("{}", &args, Locale::English), "{}");
    }
}
