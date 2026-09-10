//! **Four ways to write a rule, one artifact.**
//!
//! Architecture §5.6 asks for presets, forms, a builder and raw JSON, "all
//! producing the same artifact", with `origin` letting a form-authored rule
//! render back as its form.
//!
//! # A preset is a form with no blanks
//!
//! Levels 0 and 1 are one mechanism here rather than two. "Weekend evenings
//! cost more" and "make {weekday} {percent}% dearer" differ only in whether the
//! template asks anything — so a preset is a [`Template`] whose
//! [`fields`](Template::fields) are empty, and the same code renders it,
//! checks its answers and builds its artifact.
//!
//! Levels 2 and 3 need no machinery at all: a builder and a JSON editor both
//! hand over a finished artifact, and what separates them is only what the
//! screen offered.
//!
//! # The answers are the truth
//!
//! A templated rule stores its **answers and nothing else**. The artifact is
//! rebuilt from them by [`Authored::rule`] every time it is read, so a screen
//! cannot show a form whose answers no longer describe the rule — there is no
//! second copy to fall behind.
//!
//! That is a structural guarantee rather than a discipline. Storing the built
//! artifact beside its answers would work too, right up until someone forgot to
//! rebuild it.
//!
//! Editing the artifact of a templated rule is therefore not an edit: it
//! replaces the rule with an [`Authored::Raw`] one and drops the form. That is
//! the honest outcome — a rule somebody hand-edited is no longer that form's
//! rule, and pretending otherwise is how a settings screen starts lying.
//!
//! # Generic over the artifact, not the consequence
//!
//! `A` is whatever the tenant actually stores — `booking::Band`, not
//! `Rule<i32>`. A module that already has a configuration shape keeps it, and
//! the engine turns it into rules the way it always did. Nothing here knows
//! what a condition is.

use std::collections::BTreeMap;

use erp_i18n::{Locale, MessageCode};
use serde::{Deserialize, Serialize};

use crate::fact::{Kind, Value};

/// What a tenant filled into a template's blanks.
///
/// Ordered, so a stored rule serialises the same way twice.
pub type Answers = BTreeMap<String, Value>;

/// One blank a template asks a tenant to fill in.
///
/// Both labels inline rather than a message code: these are shipped strings in
/// a fixed pair of languages and nothing interpolates into them, which is how
/// `booking::Trade` already names itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// What the answer is called, and its key in [`Answers`].
    pub key: &'static str,
    pub label_en: &'static str,
    pub label_ar: &'static str,
    pub kind: Kind,
}

impl Field {
    #[must_use]
    pub const fn label(&self, locale: Locale) -> &'static str {
        match locale {
            Locale::Arabic => self.label_ar,
            Locale::English => self.label_en,
        }
    }
}

/// **A named starting point for a rule.**
///
/// With no [`fields`](Self::fields) it is a preset; with some it is a form.
/// [`build`](Self::build) is the only thing that ever turns answers into an
/// artifact, which is what keeps the two from disagreeing.
#[derive(Debug)]
pub struct Template<A: 'static> {
    /// Stable identifier. Stored in the rule, so renaming one orphans every
    /// rule written from it — see [`Unfillable::NoSuchTemplate`].
    pub id: &'static str,
    pub name_en: &'static str,
    pub name_ar: &'static str,
    /// Empty for a preset.
    pub fields: &'static [Field],
    /// Builds the artifact from the answers. Called on **every read** of a
    /// templated rule, so it must be pure and cheap.
    pub build: fn(&Answers) -> Result<A, Unfillable>,
}

impl<A> Template<A> {
    /// Whether this asks anything — a form rather than a preset.
    #[must_use]
    pub const fn is_form(&self) -> bool {
        !self.fields.is_empty()
    }

    #[must_use]
    pub const fn name(&self, locale: Locale) -> &'static str {
        match locale {
            Locale::Arabic => self.name_ar,
            Locale::English => self.name_en,
        }
    }
}

/// Why a set of answers does not produce a rule.
///
/// Every variant is a refusal rather than a default (L6). A form that quietly
/// substituted a value for a missing answer would price a booking at a number
/// nobody chose.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unfillable {
    /// The rule names a template this build does not ship. **Not recoverable
    /// by guessing** — the rule is unreadable until the template comes back or
    /// somebody rewrites it.
    #[error("no template called {0}")]
    NoSuchTemplate(String),
    #[error("{template} asks for {field} and no answer was given")]
    Unanswered { template: String, field: String },
    #[error("{field} is {declared:?} and the answer is {given:?}")]
    WrongKind {
        field: String,
        declared: Kind,
        given: Kind,
    },
    /// **An answer nobody asked for is a mistake, not spare data.** It is how a
    /// renamed field goes unnoticed: the old answer sits there unread and the
    /// new one is missing, and only one of those is otherwise reported.
    #[error("{field} was answered and this template does not ask for it")]
    NotAsked { field: String },
    /// Every answer was present and of the right kind, and the rule they
    /// describe cannot exist — a window that closes before it opens, a
    /// percentage that would have the business paying the customer.
    ///
    /// A message code rather than a sentence: the template knows its own field
    /// names, so it says something better than the artifact's own error, and it
    /// says it in the reader's language.
    #[error("the answers describe a rule that cannot exist: {0}")]
    Impossible(MessageCode),
}

/// **A rule, and how it was written.**
///
/// The four authoring levels. A templated rule holds its answers; a
/// hand-written one holds the artifact. Neither holds both, so neither can
/// contradict itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum Authored<A> {
    /// Level 0: picked from a list. A template with no blanks, so no answers.
    Preset { template: String },
    /// Level 1: a template with blanks filled in.
    Form { template: String, answers: Answers },
    /// Level 2: composed condition by condition on a builder screen.
    Builder { rule: A },
    /// Level 3: written directly.
    Raw { rule: A },
}

impl<A: Clone> Authored<A> {
    /// **Writes a rule from a template and a tenant's answers.**
    ///
    /// Builds it once here so that answers which cannot produce a rule are
    /// refused while their author is still looking at them, rather than at the
    /// next booking.
    ///
    /// # Errors
    /// If the template is unknown, an answer is missing, spare or of the wrong
    /// kind, or the rule the answers describe cannot exist.
    pub fn written(
        templates: &[Template<A>],
        id: &str,
        answers: Answers,
    ) -> Result<Self, Unfillable> {
        let template = find(templates, id)?;
        // Built and thrown away: this is where answers that describe no rule
        // are refused, while their author is still looking at them.
        built(templates, id, &answers)?;
        Ok(if template.is_form() {
            Self::Form {
                template: id.to_owned(),
                answers,
            }
        } else {
            Self::Preset {
                template: id.to_owned(),
            }
        })
    }

    /// **The artifact, rebuilt from the answers when there are any.**
    ///
    /// What evaluation and display should both use.
    ///
    /// # Errors
    /// If a templated rule's template is gone, or its answers no longer fill
    /// it because the template changed under it. Both mean a deploy removed
    /// something a tenant was relying on, and both refuse rather than guess.
    pub fn rule(&self, templates: &[Template<A>]) -> Result<A, Unfillable> {
        match self {
            Self::Builder { rule } | Self::Raw { rule } => Ok(rule.clone()),
            // A preset is a form with no blanks, here as everywhere else.
            Self::Preset { template } => built(templates, template, &Answers::new()),
            Self::Form { template, answers } => built(templates, template, answers),
        }
    }

    /// Which template this came from, for a screen deciding what to open.
    #[must_use]
    pub fn template(&self) -> Option<&str> {
        match self {
            Self::Preset { template } | Self::Form { template, .. } => Some(template),
            Self::Builder { .. } | Self::Raw { .. } => None,
        }
    }

    /// The authoring level, as the wire spells it.
    #[must_use]
    pub const fn level(&self) -> &'static str {
        match self {
            Self::Preset { .. } => "preset",
            Self::Form { .. } => "form",
            Self::Builder { .. } => "builder",
            Self::Raw { .. } => "raw",
        }
    }
}

/// Check the answers, then build. **Never one without the other**: `build` is
/// allowed to assume the kinds its fields declare, which is only true because
/// `check` has just run.
fn built<A>(templates: &[Template<A>], id: &str, answers: &Answers) -> Result<A, Unfillable> {
    let template = find(templates, id)?;
    check(template, answers)?;
    (template.build)(answers)
}

fn find<'a, A>(templates: &'a [Template<A>], id: &str) -> Result<&'a Template<A>, Unfillable> {
    templates
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| Unfillable::NoSuchTemplate(id.to_owned()))
}

/// Every answer the template asks for, of the right kind, and nothing else.
fn check<A>(template: &Template<A>, answers: &Answers) -> Result<(), Unfillable> {
    for field in template.fields {
        let given = answers
            .get(field.key)
            .ok_or_else(|| Unfillable::Unanswered {
                template: template.id.to_owned(),
                field: field.key.to_owned(),
            })?;
        if given.kind() != field.kind {
            return Err(Unfillable::WrongKind {
                field: field.key.to_owned(),
                declared: field.kind,
                given: given.kind(),
            });
        }
    }
    for key in answers.keys() {
        if !template.fields.iter().any(|f| f.key == key) {
            return Err(Unfillable::NotAsked { field: key.clone() });
        }
    }
    Ok(())
}
