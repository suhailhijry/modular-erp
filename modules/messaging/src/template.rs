//! **Templates that fetch their own data.**
//!
//! # The two systems this replaces
//!
//! The system read for Phase 7 has two template mechanisms that do not meet: a
//! database aggregate whose parameters the caller fills in by hand, and
//! hardcoded classes with the copy, the business name and the gendered wording
//! compiled in. Changing a reminder's wording in the second one is a deploy.
//!
//! Both problems have one cause: **a template cannot ask for anything, so
//! somebody has to hand it everything.** Every caller then knows what every
//! message says, and a message with one more field in it is a change in five
//! places.
//!
//! So a template here declares what it needs — `{{ reservation.starts_at }}`,
//! `{{ customer.name }}` — and the renderer asks the read model. The caller
//! supplies a **subject** and nothing else.
//!
//! # Declared, so it fails when it is saved
//!
//! The vocabulary is fixed per [`Topic`], and [`Template::check`] refuses a body
//! that names anything outside it. A binding that cannot be resolved is a
//! refusal at the moment somebody writes the template, not an empty gap in a
//! message a customer is reading.
//!
//! # Why configuration rather than an aggregate
//!
//! A template is a tenant's settings, and this system already has one place for
//! those: `erp_eventlog::config`, which is typed per key, versioned, and named
//! in every event's `config_version` so a replay knows which generation was in
//! force (L5). `Tariff`, `PostingAccounts`, `PublicBooking` and the GOSI
//! schedule all live there. Making templates an aggregate would add a stream, a
//! projection and a rebuild arm to hold what is already a settings object.

use std::collections::BTreeMap;

use erp_i18n::Locale;
use serde::{Deserialize, Serialize};

use crate::audience::{Audience, Topic};
use crate::channel::Channel;

/// Where a tenant's templates live.
pub const KEY: &str = "messaging.templates";

/// The longest a template name may be, and the shape it must have.
const MAX_NAME: usize = 64;

/// One language's wording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Body {
    /// **Only on channels that have one.** A subject written for SMS is one its
    /// author expected to appear somewhere, and it would not.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subject: String,
    pub text: String,
}

/// One message a business sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    pub channel: Channel,
    /// What it is about, which decides the bindings and the audiences.
    pub topic: Topic,
    /// Who it goes to. **Not an address** — see [`crate::audience`].
    pub audience: Audience,
    /// One body per language, keyed by locale code.
    ///
    /// **Both are the template**, per D12: neither is a translation of a
    /// compiled string, and a template missing one is refused rather than
    /// falling back to the other. A customer who asked for Arabic and receives
    /// English has been told this system does not really speak it.
    pub bodies: BTreeMap<String, Body>,
    /// Off is not deleted. A business that stops sending a reminder for a month
    /// keeps the wording it spent an afternoon on.
    #[serde(default = "yes")]
    pub active: bool,
}

const fn yes() -> bool {
    true
}

/// Every template a tenant has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Templates {
    #[serde(default)]
    pub entries: BTreeMap<String, Template>,
}

impl Templates {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Template> {
        self.entries.get(name)
    }
}

/// Why a template is not one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("{0} is not a template name")]
    NotAName(String),
    #[error("{binding} is not something a message about {topic} can say")]
    UnknownBinding { binding: String, topic: String },
    #[error("a message about {topic} cannot be addressed to {audience}")]
    WrongAudience { topic: String, audience: String },
    #[error("{channel} has no subject line")]
    NoSubjectLine { channel: String },
    #[error("a template needs a subject line on {channel}")]
    NeedsASubject { channel: String },
    #[error("a template needs wording in {locale}")]
    MissingLanguage { locale: String },
    #[error("a template needs something to say")]
    Empty,
    #[error("no template is called {0}")]
    NoSuchTemplate(String),
}

impl Template {
    /// **Everything that can be checked before anybody is waiting for a
    /// message.**
    ///
    /// Called when a template is saved, so an unresolvable binding is a `400`
    /// on somebody's screen rather than a gap in a reminder.
    pub fn check(&self, name: &str) -> Result<(), TemplateError> {
        if name.is_empty()
            || name.len() > MAX_NAME
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
        {
            return Err(TemplateError::NotAName(name.to_owned()));
        }

        if !self.topic.audiences().contains(&self.audience) {
            return Err(TemplateError::WrongAudience {
                topic: self.topic.as_str().to_owned(),
                audience: self.audience.as_str().to_owned(),
            });
        }

        for locale in Locale::ALL {
            let Some(body) = self.bodies.get(locale.code()) else {
                return Err(TemplateError::MissingLanguage {
                    locale: locale.code().to_owned(),
                });
            };

            if body.text.trim().is_empty() {
                return Err(TemplateError::Empty);
            }
            if self.channel.has_a_subject() {
                if body.subject.trim().is_empty() {
                    return Err(TemplateError::NeedsASubject {
                        channel: self.channel.as_str().to_owned(),
                    });
                }
            } else if !body.subject.is_empty() {
                return Err(TemplateError::NoSubjectLine {
                    channel: self.channel.as_str().to_owned(),
                });
            }

            for text in [&body.subject, &body.text] {
                for binding in placeholders(text) {
                    if !vocabulary(self.topic).contains(&binding.as_str()) {
                        return Err(TemplateError::UnknownBinding {
                            binding,
                            topic: self.topic.as_str().to_owned(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// The body in one language.
    ///
    /// No fallback: [`Self::check`] proved both exist, so a missing one is a
    /// stored value that did not go through the type (L6), and the caller sees
    /// `None` rather than a message in the wrong language.
    #[must_use]
    pub fn body(&self, locale: Locale) -> Option<&Body> {
        self.bodies.get(locale.code())
    }
}

// ---------------------------------------------------------------------------
// The vocabulary
// ---------------------------------------------------------------------------

/// What every template may say, whatever it is about.
///
/// `link` is supplied by whoever sends — usually a short link (11e) made in the
/// same transaction — rather than read from anywhere, which is why it is here
/// and not under a topic.
const EVERYWHERE: &[&str] = &["business", "link"];

/// What a template about each topic may say.
///
/// **A fixed list, not a reflection over some struct.** Every name here is one
/// [`crate::bindings::of`] knows how to answer; the pair is checked by
/// `every_binding_in_the_vocabulary_can_be_resolved`, so the two cannot drift.
#[must_use]
pub fn vocabulary(topic: Topic) -> Vec<&'static str> {
    let own: &[&str] = match topic {
        Topic::Reservation => &[
            "reservation.id",
            "reservation.starts_at",
            "reservation.ends_at",
            "reservation.stage",
            "customer.name",
            "customer.phone",
            "worker.name",
            "branch.name",
        ],
        Topic::Invoice => &[
            "invoice.id",
            "invoice.number",
            "invoice.issued_on",
            "invoice.due_on",
            "invoice.total",
            "invoice.outstanding",
            "customer.name",
        ],
        Topic::Customer => &[
            "customer.id",
            "customer.name",
            "customer.phone",
            "customer.email",
        ],
        Topic::Employee => &["employee.id", "employee.name", "employee.branch"],
    };
    EVERYWHERE.iter().chain(own.iter()).copied().collect()
}

/// Every `{{ name }}` in a string, in the order they appear.
///
/// Whitespace inside the braces is allowed and trimmed, because somebody will
/// write `{{ customer.name }}` and somebody else will write `{{customer.name}}`
/// and neither should be a support ticket. An unclosed `{{` is simply not a
/// placeholder — refusing it would refuse a body that legitimately contains
/// braces.
#[must_use]
pub fn placeholders(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            return found;
        };
        found.push(after[..close].trim().to_owned());
        rest = &after[close + 2..];
    }
    found
}

/// Substitutes what is known and **leaves the rest alone**.
///
/// A placeholder with no value keeps its braces rather than becoming an empty
/// space. A customer reading "your appointment is at " has been told nothing;
/// one reading `{{ reservation.starts_at }}` has been told the business has a
/// broken template, which is the message somebody will act on.
#[must_use]
pub fn render(text: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            out.push_str(&rest[open..]);
            return out;
        };
        let name = after[..close].trim();
        if let Some(value) = values.get(name) {
            out.push_str(value);
        } else {
            out.push_str("{{");
            out.push_str(&after[..close]);
            out.push_str("}}");
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(text: &str) -> Body {
        Body {
            subject: String::new(),
            text: text.to_owned(),
        }
    }

    fn reminder(text: &str) -> Template {
        Template {
            channel: Channel::Sms,
            topic: Topic::Reservation,
            audience: Audience::Client,
            bodies: BTreeMap::from([("en".to_owned(), body(text)), ("ar".to_owned(), body(text))]),
            active: true,
        }
    }

    #[test]
    fn placeholders_are_found_with_or_without_spaces() {
        assert_eq!(
            placeholders("Hi {{customer.name}}, at {{ reservation.starts_at }}."),
            vec!["customer.name", "reservation.starts_at"]
        );
        assert_eq!(placeholders("no braces here"), Vec::<String>::new());
        assert_eq!(placeholders("an unclosed {{ one"), Vec::<String>::new());
    }

    /// **The 11b property.** An unresolvable binding fails when the template is
    /// saved, not when a customer is waiting for a message.
    #[test]
    fn a_binding_that_cannot_be_resolved_is_refused_at_save_time() {
        assert!(
            reminder("at {{ reservation.starts_at }}")
                .check("booking.reminder")
                .is_ok()
        );

        let refused = reminder("owed {{ invoice.total }}")
            .check("booking.reminder")
            .expect_err("an invoice total is not something a booking knows");
        assert!(matches!(refused, TemplateError::UnknownBinding { .. }));

        // And the universal ones are available everywhere.
        assert!(reminder("{{ business }}: {{ link }}").check("x").is_ok());
    }

    #[test]
    fn a_template_needs_both_languages() {
        let mut one = reminder("hello");
        one.bodies.remove("ar");
        assert!(matches!(
            one.check("x"),
            Err(TemplateError::MissingLanguage { .. })
        ));
    }

    #[test]
    fn only_email_has_a_subject_line() {
        let mut sms = reminder("hello");
        for body in sms.bodies.values_mut() {
            body.subject = "Reminder".to_owned();
        }
        assert!(matches!(
            sms.check("x"),
            Err(TemplateError::NoSubjectLine { .. })
        ));

        let mut email = reminder("hello");
        email.channel = Channel::Email;
        assert!(matches!(
            email.check("x"),
            Err(TemplateError::NeedsASubject { .. })
        ));
        for body in email.bodies.values_mut() {
            body.subject = "Reminder".to_owned();
        }
        assert!(email.check("x").is_ok());
    }

    #[test]
    fn a_template_is_addressed_to_somebody_its_topic_has() {
        let mut invoice = reminder("owed {{ invoice.total }}");
        invoice.topic = Topic::Invoice;
        invoice.audience = Audience::Worker;
        assert!(matches!(
            invoice.check("x"),
            Err(TemplateError::WrongAudience { .. })
        ));
    }

    #[test]
    fn a_name_is_a_slug() {
        assert!(reminder("hello").check("booking.reminder_24h").is_ok());
        assert!(reminder("hello").check("Booking Reminder").is_err());
        assert!(reminder("hello").check("").is_err());
    }

    /// **An unresolved placeholder keeps its braces.**
    ///
    /// "Your appointment is at " tells a customer nothing; the braces tell
    /// somebody the template is broken, which is the message that gets fixed.
    #[test]
    fn rendering_leaves_what_it_cannot_answer() {
        let values = BTreeMap::from([("customer.name".to_owned(), "نورة".to_owned())]);
        assert_eq!(
            render(
                "Hi {{ customer.name }}, at {{ reservation.starts_at }}.",
                &values
            ),
            "Hi نورة, at {{ reservation.starts_at }}."
        );
        assert_eq!(render("nothing to do", &values), "nothing to do");
        assert_eq!(render("{{ unclosed", &values), "{{ unclosed");
    }
}
