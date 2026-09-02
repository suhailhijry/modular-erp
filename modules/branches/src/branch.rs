//! A place the business trades from.
//!
//! # Why this is an aggregate and not a settings row
//!
//! Because a branch is a **reporting dimension**, and a dimension that can be
//! edited in place rewrites history: a trial balance for Olaya run in March and
//! again in June would differ, with nothing in the system able to say why.
//! Renaming a branch is an event, so the June report can still say what the
//! March one said, and closing one does not delete the year it traded.
//!
//! It is also why the *reference* on a document is the branch's id and never its
//! name — the same argument `sales` makes about a customer, one layer along.
//!
//! # What is deliberately not here
//!
//! **Opening hours.** The plan asks for them and nothing would read them.
//! `booking` already keeps availability per resource, which is finer than a
//! branch and is what a diary actually needs; branch hours are something the
//! booking site would *display*, and that site is a separate React project
//! reading this API — Phase 17. A rule nobody applies is a rule that is
//! wrong by the time somebody does — see the `Address` note below for the shape
//! of that mistake already made three times.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{DomainName, EventName, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

/// Where a branch is.
///
/// # The fourth copy of this struct, and why it is not shared yet
///
/// `crm`, `sales` and `tax_sa` each define one with these fields. That is a real
/// duplication and worth collapsing into `erp-types` — but not in this phase and
/// not silently, because the three are **event schemas**: `sales` freezes a
/// buyer's address onto a tax invoice (L5), `crm` holds the current one, and
/// `tax_sa` holds the taxpayer's registered one. They are equal today by
/// coincidence rather than by rule, and ZATCA adding a field to one is what
/// would separate them again.
///
/// Recorded here rather than left as an unexplained fourth copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub street: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub building: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub district: Option<String>,
    pub city: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postal_code: Option<String>,
    /// ISO 3166-1 alpha-2. `SA` for the first market.
    pub country: String,
}

/// What a branch is called and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Details {
    pub name: String,
    /// The Latin spelling, for a document that prints in English.
    pub name_latin: Option<String>,
    pub address: Address,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadBranch {
    #[error("a branch needs a name")]
    NoName,
    #[error("a branch needs a street and a city")]
    NoAddress,
    #[error("{0} is not an ISO 3166-1 alpha-2 country code")]
    NotACountry(String),
}

impl Details {
    /// What a branch must have before it can exist.
    ///
    /// The country is checked because ZATCA prints it on every document a
    /// branch issues, and a two-letter field is the one a caller gets wrong.
    pub fn check(&self) -> Result<(), BadBranch> {
        if self.name.trim().is_empty() {
            return Err(BadBranch::NoName);
        }
        if self.address.street.trim().is_empty() || self.address.city.trim().is_empty() {
            return Err(BadBranch::NoAddress);
        }
        let country = self.address.country.trim();
        if country.len() != 2 || !country.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(BadBranch::NotACountry(self.address.country.clone()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BranchEvent {
    Opened {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name_latin: Option<String>,
        address: Address,
        at: Timestamp,
    },
    /// It moved, or it was renamed. **An event and not an update**, so a report
    /// run last March can still say what it said then.
    Amended {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name_latin: Option<String>,
        address: Address,
        at: Timestamp,
    },
    /// It stopped trading. The branch stays: it has a year of documents behind
    /// it, and a dimension that vanishes takes its own history's meaning with
    /// it.
    Closed {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        why: String,
        at: Timestamp,
    },
    /// It traded again.
    Reopened { at: Timestamp },
}

impl DomainEvent for BranchEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Opened { .. } => Self::NAMES[0],
            Self::Amended { .. } => Self::NAMES[1],
            Self::Closed { .. } => Self::NAMES[2],
            Self::Reopened { .. } => Self::NAMES[3],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

impl BranchEvent {
    pub const NAMES: [&'static str; 4] = [
        "branches.branch.opened",
        "branches.branch.amended",
        "branches.branch.closed",
        "branches.branch.reopened",
    ];
}

#[derive(Debug, Default, Clone)]
pub struct Branch {
    pub opened: bool,
    pub name: String,
    pub name_latin: Option<String>,
    pub address: Option<Address>,
    pub closed_at: Option<Timestamp>,
}

impl Aggregate for Branch {
    type Event = BranchEvent;

    fn domain() -> DomainName {
        crate::domain("branches_branch")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            BranchEvent::Opened {
                name,
                name_latin,
                address,
                ..
            }
            | BranchEvent::Amended {
                name,
                name_latin,
                address,
                ..
            } => {
                self.opened = true;
                self.name.clone_from(name);
                self.name_latin.clone_from(name_latin);
                self.address = Some(address.clone());
            }
            BranchEvent::Closed { at, .. } => self.closed_at = Some(*at),
            BranchEvent::Reopened { .. } => self.closed_at = None,
        }
    }
}

impl Branch {
    #[must_use]
    pub const fn exists(&self) -> bool {
        self.opened
    }

    /// Whether a document may be dated to this branch today.
    ///
    /// **A closed branch refuses new documents and keeps its old ones.** The
    /// same distinction `crm` draws about an archived customer, and for the same
    /// reason: history is not a mistake to be tidied away.
    #[must_use]
    pub const fn accepts_documents(&self) -> bool {
        self.opened && self.closed_at.is_none()
    }

    /// What the current details are, for a caller amending them.
    #[must_use]
    pub fn details(&self) -> Option<Details> {
        Some(Details {
            name: self.name.clone(),
            name_latin: self.name_latin.clone(),
            address: self.address.clone()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address() -> Address {
        Address {
            street: "طريق الملك فهد".to_owned(),
            building: None,
            district: None,
            city: "الرياض".to_owned(),
            postal_code: None,
            country: "SA".to_owned(),
        }
    }

    fn details(name: &str) -> Details {
        Details {
            name: name.to_owned(),
            name_latin: None,
            address: address(),
        }
    }

    #[test]
    fn a_branch_needs_a_name_a_street_and_a_country() {
        assert_eq!(details("العليا").check(), Ok(()));
        assert_eq!(details("  ").check(), Err(BadBranch::NoName));

        let mut no_street = details("العليا");
        no_street.address.street = String::new();
        assert_eq!(no_street.check(), Err(BadBranch::NoAddress));

        let mut bad_country = details("العليا");
        bad_country.address.country = "SAU".to_owned();
        assert!(matches!(
            bad_country.check(),
            Err(BadBranch::NotACountry(_))
        ));
    }

    /// **Closing keeps the branch and stops new documents.** A dimension that
    /// vanished would take the meaning of its own history with it.
    #[test]
    fn a_closed_branch_still_exists_and_takes_nothing_new() {
        let mut branch = Branch::default();
        let at = chrono::Utc::now();
        branch.apply(&BranchEvent::Opened {
            name: "العليا".to_owned(),
            name_latin: Some("Olaya".to_owned()),
            address: address(),
            at,
        });
        assert!(branch.accepts_documents());

        branch.apply(&BranchEvent::Closed {
            why: "انتقل".to_owned(),
            at,
        });
        assert!(branch.exists(), "the branch was forgotten");
        assert!(!branch.accepts_documents());

        branch.apply(&BranchEvent::Reopened { at });
        assert!(branch.accepts_documents());
    }

    /// Amending is an event, so what a report said last March is still what the
    /// log says it said.
    #[test]
    fn amending_records_the_change_rather_than_overwriting_it() {
        let mut branch = Branch::default();
        let at = chrono::Utc::now();
        branch.apply(&BranchEvent::Opened {
            name: "العليا".to_owned(),
            name_latin: None,
            address: address(),
            at,
        });
        branch.apply(&BranchEvent::Amended {
            name: "العليا الجديد".to_owned(),
            name_latin: Some("New Olaya".to_owned()),
            address: address(),
            at,
        });

        assert_eq!(branch.name, "العليا الجديد");
        assert_eq!(branch.name_latin.as_deref(), Some("New Olaya"));
    }
}
