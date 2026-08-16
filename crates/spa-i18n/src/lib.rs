//! Localization.
//!
//! # The rule
//!
//! **An error is a code and some arguments. It is never a sentence.**
//!
//! `thiserror`'s `#[error("no membership for this identity")]` bakes English into
//! the type. That is the pattern which makes localization a rewrite later rather
//! than a translation, so error types here carry a stable [`MessageCode`] and
//! typed [`MessageArg`]s, and prose is chosen at the API boundary from the
//! caller's `Accept-Language`.
//!
//! The machine-readable code is not a localization artifact — it is the `type`
//! field of the RFC 9457 problem response, which integrators branch on. The two
//! requirements turn out to be the same requirement.
//!
//! # Arabic is not English with different words
//!
//! Saudi Arabia is the first market, so Arabic is a first-class target, not a
//! translation layer bolted on. Three things that follow:
//!
//! - **Six plural categories**, not two. `if n == 1` is wrong in a way no
//!   reviewer will catch. See [`plural`].
//! - **Bidirectional text.** Interpolating a Latin identifier — an account code,
//!   a tenant slug — into an Arabic sentence scrambles the surrounding text
//!   unless the run is isolated. [`Locale::is_rtl`] drives that automatically.
//! - **Completeness is enforced**, not hoped for. `tests/completeness.rs` fails
//!   the build if any code lacks a translation in any locale, so a missing
//!   Arabic string cannot ship as English.

mod catalog;
mod plural;
pub mod testing;

pub use catalog::{Catalog, StaticCatalog, Template};
pub use plural::Plural;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A supported language.
///
/// A closed enum on purpose. Adding a language means adding translations, and
/// the completeness test should refuse to build until they exist — which only
/// works if the set is known at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Locale {
    English,
    Arabic,
}

impl Locale {
    pub const ALL: [Self; 2] = [Self::English, Self::Arabic];

    /// The default when no preference is expressed.
    ///
    /// English, because it is the language every integrator can read and these
    /// messages reach machines as often as people. A human without a stated
    /// preference gets English; a human who states Arabic gets Arabic.
    pub const DEFAULT: Self = Self::English;

    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Arabic => "ar",
        }
    }

    /// Whether text flows right to left. Drives bidi isolation when
    /// interpolating Latin-script arguments.
    #[must_use]
    pub const fn is_rtl(self) -> bool {
        matches!(self, Self::Arabic)
    }

    /// The plural categories this language actually selects.
    ///
    /// English uses two; Arabic uses all six. A catalog only needs to provide
    /// the ones its language uses — see [`Template`] — and the completeness test
    /// checks exactly this set.
    #[must_use]
    pub const fn plural_categories(self) -> &'static [Plural] {
        match self {
            Self::English => &[Plural::One, Plural::Other],
            Self::Arabic => &[
                Plural::Zero,
                Plural::One,
                Plural::Two,
                Plural::Few,
                Plural::Many,
                Plural::Other,
            ],
        }
    }

    pub(crate) const fn plural(self, n: i64) -> Plural {
        match self {
            Self::English => plural::english(n),
            Self::Arabic => plural::arabic(n),
        }
    }

    /// Best match for an HTTP `Accept-Language` header.
    ///
    /// Quality values are honoured, so `en;q=0.5, ar;q=0.9` yields Arabic.
    /// Regional subtags are matched on their primary tag — `ar-SA`, `ar-EG` and
    /// bare `ar` all resolve to Arabic, because a Gulf and an Egyptian user are
    /// better served by Arabic than by falling through to English.
    #[must_use]
    pub fn from_accept_language(header: &str) -> Self {
        let mut best: Option<(Self, f32)> = None;

        for part in header.split(',') {
            let mut fields = part.split(';');
            let Some(tag) = fields.next().map(str::trim) else {
                continue;
            };
            let quality = fields
                .find_map(|f| f.trim().strip_prefix("q=")?.parse::<f32>().ok())
                .unwrap_or(1.0);

            let primary = tag.split('-').next().unwrap_or(tag).to_ascii_lowercase();
            let locale = match primary.as_str() {
                "ar" => Some(Self::Arabic),
                "en" => Some(Self::English),
                // `*` means "anything"; honour it only as a weak fallback.
                "*" => Some(Self::DEFAULT),
                _ => None,
            };

            if let Some(locale) = locale
                && best.is_none_or(|(_, best_q)| quality > best_q)
            {
                best = Some((locale, quality));
            }
        }

        best.map_or(Self::DEFAULT, |(locale, _)| locale)
    }
}

/// A stable, machine-readable identifier for a message.
///
/// Doubles as the `type` field of an RFC 9457 problem response, so integrators
/// branch on it. **Changing one is a breaking API change**, exactly like
/// renaming a field.
///
/// Backed by a `Cow` because it is genuinely both things: a compile-time
/// constant in the catalogs, and a parsed value when a client deserializes a
/// problem response. Interning at deserialize time would mean leaking memory on
/// untrusted input, which is a worse trade than losing `Copy`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageCode(std::borrow::Cow<'static, str>);

impl MessageCode {
    /// Codes are `domain.thing_that_happened`, lowercase with underscores.
    #[must_use]
    pub const fn new(code: &'static str) -> Self {
        Self(std::borrow::Cow::Borrowed(code))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MessageCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A value interpolated into a message.
///
/// Typed rather than pre-formatted strings, because formatting is
/// locale-dependent: a count selects a plural form, a date renders differently
/// under a Hijri calendar, and money carries a currency whose symbol placement
/// varies. Handing the renderer a `String` throws away everything it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum MessageArg {
    /// Latin-script text — an identifier, a slug, a code. Bidi-isolated when
    /// rendered into an RTL locale.
    Text(String),
    /// A plain number, not a count. Does not select a plural form.
    Int(i64),
    /// A quantity. Selects the plural form of the template.
    Count(i64),
}

impl MessageArg {
    #[must_use]
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }
}

/// Something that went wrong, ready to be rendered in any supported language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub code: MessageCode,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, MessageArg>,
}

impl Message {
    #[must_use]
    pub fn new(code: MessageCode) -> Self {
        Self {
            code,
            args: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with(mut self, name: &str, arg: MessageArg) -> Self {
        self.args.insert(name.to_owned(), arg);
        self
    }

    /// The count that selects a plural form, if this message has one.
    pub(crate) fn count(&self) -> Option<i64> {
        self.args.values().find_map(|arg| match arg {
            MessageArg::Count(n) => Some(*n),
            _ => None,
        })
    }
}

/// An error that can be shown to a person.
///
/// Every error type reaching an API boundary implements this. The `Display`
/// impl stays English and is for logs and developers; `message` is what a user
/// sees.
pub trait Localize {
    fn message(&self) -> Message;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_language_prefers_the_highest_quality() {
        assert_eq!(Locale::from_accept_language("ar"), Locale::Arabic);
        assert_eq!(Locale::from_accept_language("en"), Locale::English);
        assert_eq!(
            Locale::from_accept_language("en;q=0.5, ar;q=0.9"),
            Locale::Arabic
        );
        assert_eq!(
            Locale::from_accept_language("ar;q=0.3, en;q=0.8"),
            Locale::English
        );
    }

    #[test]
    fn regional_arabic_resolves_to_arabic() {
        // A Saudi user sends ar-SA. Falling through to English because the exact
        // tag is unknown would be the wrong answer in our first market.
        for tag in ["ar-SA", "ar-EG", "ar-sa", "AR-SA"] {
            assert_eq!(Locale::from_accept_language(tag), Locale::Arabic, "{tag}");
        }
        assert_eq!(Locale::from_accept_language("en-GB"), Locale::English);
    }

    #[test]
    fn unknown_or_absent_languages_fall_back() {
        assert_eq!(Locale::from_accept_language(""), Locale::DEFAULT);
        assert_eq!(Locale::from_accept_language("fr, de"), Locale::DEFAULT);
        assert_eq!(Locale::from_accept_language("*"), Locale::DEFAULT);
        // A known language alongside unknown ones still wins.
        assert_eq!(Locale::from_accept_language("fr, ar"), Locale::Arabic);
    }

    #[test]
    fn a_malformed_header_does_not_panic() {
        for header in [";;;", "q=", "en;q=", "en;q=notanumber", ","] {
            let _ = Locale::from_accept_language(header);
        }
    }

    #[test]
    fn the_count_argument_is_found_regardless_of_name() {
        let msg = Message::new(MessageCode::new("test.thing"))
            .with("other", MessageArg::Int(9))
            .with("n", MessageArg::Count(3));
        assert_eq!(msg.count(), Some(3));

        let msg = Message::new(MessageCode::new("test.thing")).with("n", MessageArg::Int(3));
        assert_eq!(msg.count(), None, "Int is not a count");
    }
}
