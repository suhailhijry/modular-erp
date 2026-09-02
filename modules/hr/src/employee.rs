//! A person the business employs, and where they sit in the org chart.
//!
//! # The org is a tree, and that is the point
//!
//! Employees are not a flat list with a `manager_id` decoration. The reporting
//! line **is** the structure: authority travels along it, so everything in
//! [`crate::claims`] depends on being able to walk it and on it terminating.
//!
//! One `reports_to` edge per employee, one root per tenant — whoever nobody
//! reports to, which the tree gives for free rather than a flag anybody
//! maintains — and cycles refused at the command. Not because a cycle is untidy
//! but because the claim union would not terminate.
//!
//! # Why moving somebody is its own event
//!
//! Because moving a person moves everything they carry. Every claim in their
//! subtree stops reaching their old manager and starts reaching their new one,
//! and that is the operation an auditor asks about. An `Amended` that quietly
//! changed a parent alongside a phone number would not answer them.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

/// What is known about a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Details {
    pub name: String,
    /// The Latin spelling, for a document that prints in English.
    pub name_latin: Option<String>,
    /// Their national id or iqama number, when the business records one.
    ///
    /// Not validated here beyond being non-empty: the Saudi rules for an iqama
    /// against a national id belong in `hr_sa`, the country module, for the
    /// same reason the VAT rules live in `tax_sa`.
    pub national_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadEmployee {
    #[error("an employee needs a name")]
    NoName,
    #[error("an employee needs a phone number or an email address")]
    NoContact,
}

impl Details {
    /// What a person must have before they can exist.
    ///
    /// A way to reach them, for the reason `crm` requires one of a customer: a
    /// person nobody can contact is a row rather than an employee, and every
    /// document this module will eventually send — a payslip, a contract
    /// renewal — needs somewhere to go.
    pub fn check(&self) -> Result<(), BadEmployee> {
        if self.name.trim().is_empty() {
            return Err(BadEmployee::NoName);
        }
        let reachable = self.email.as_ref().is_some_and(|e| !e.trim().is_empty())
            || self.phone.as_ref().is_some_and(|p| !p.trim().is_empty());
        if !reachable {
            return Err(BadEmployee::NoContact);
        }
        Ok(())
    }
}

/// A document a person must hold to be allowed to work.
///
/// # Why these four and not a free-text kind
///
/// Because the rule differs by kind and a rule that reads a string is a rule
/// that silently does nothing when somebody types `Iqama` instead of `iqama`.
/// A fifth is a variant and a compile error at every match, which is where a
/// new rule should surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    /// A national id or an iqama. **The one that stops somebody working.**
    Identity,
    /// A work permit, where the identity document is not itself one.
    WorkPermit,
    /// A medical certificate — food handling, a health card.
    Medical,
    /// A professional licence: a barber's, a physiotherapist's, an
    /// accountant's.
    Licence,
}

impl DocumentKind {
    /// Whether letting this lapse stops the person working.
    ///
    /// **All of them, and that is not a placeholder.** Every kind here is one a
    /// Saudi employer may not roster somebody without; a document that could
    /// lapse harmlessly is one this module should not be tracking, because it
    /// would train people to ignore the warnings for the ones that matter.
    #[must_use]
    pub const fn blocks_work(self) -> bool {
        true
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::WorkPermit => "work_permit",
            Self::Medical => "medical",
            Self::Licence => "licence",
        }
    }

    pub const ALL: [Self; 4] = [
        Self::Identity,
        Self::WorkPermit,
        Self::Medical,
        Self::Licence,
    ];
}

impl std::fmt::Display for DocumentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a kind of document this system tracks")]
pub struct UnknownDocument(pub String);

impl std::str::FromStr for DocumentKind {
    type Err = UnknownDocument;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| UnknownDocument(s.to_owned()))
    }
}

/// One document somebody holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub kind: DocumentKind,
    /// Its number, as printed on it.
    pub number: String,
    /// **The last day it is valid**, and it is a date rather than an instant on
    /// purpose: an iqama expires on a day in Riyadh, not at an hour in UTC, and
    /// storing an instant would make the answer depend on which side of
    /// midnight somebody asked.
    pub expires_on: chrono::NaiveDate,
}

impl Document {
    /// Whether it is still valid on this day.
    ///
    /// Inclusive of the expiry date: a document that says it expires on the
    /// 30th is valid on the 30th, which is what the document itself means and
    /// what the person holding it will argue.
    #[must_use]
    pub fn valid_on(&self, day: chrono::NaiveDate) -> bool {
        day <= self.expires_on
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EmployeeEvent {
    Hired {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name_latin: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        national_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        email: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phone: Option<String>,
        /// Who they report to. `None` makes them the root, and a tenant has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reports_to: Option<AggregateId>,
        /// **Where this person works** — not where the request happened. See
        /// the module docs: the two differ legitimately and often.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<AggregateId>,
        at: Timestamp,
    },
    /// Their details changed. **Never their reporting line** — that is
    /// [`Self::Reparented`], and the separation is the point.
    Amended {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name_latin: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        national_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        email: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phone: Option<String>,
        at: Timestamp,
    },
    /// They moved in the org chart. **An event of its own**, because moving a
    /// person moves every claim their subtree carries, and that is what an
    /// auditor asks about.
    Reparented {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reports_to: Option<AggregateId>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        why: String,
        at: Timestamp,
    },
    /// They moved branch. Separate from `Reparented` for the same reason: it
    /// changes where their authority applies, not who holds it.
    Transferred {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<AggregateId>,
        at: Timestamp,
    },
    /// They left. The record stays: they are on last year's payroll, last
    /// month's timesheets and whatever they approved.
    Left {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        why: String,
        at: Timestamp,
    },
    /// They came back. A rehire under the same record, which is what a business
    /// means when a seasonal worker returns.
    Rehired { at: Timestamp },
    /// A document was recorded, or renewed.
    ///
    /// **One event for both**, because a renewal is the same fact with a later
    /// date: what matters is the document a person holds *now*, and a separate
    /// `Renewed` would mean two places that decide which one that is.
    DocumentRecorded {
        kind: crate::employee::DocumentKind,
        number: String,
        #[serde(with = "date")]
        expires_on: chrono::NaiveDate,
        at: Timestamp,
    },
}

/// A `NaiveDate` in the log, as `YYYY-MM-DD`.
///
/// Explicit rather than serde's default, so what the log holds is the string a
/// person would write and not a representation that could change with a
/// dependency.
mod date {
    use serde::{Deserialize, Deserializer, Serializer};

    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's Serialize contract takes a reference"
    )]
    pub(super) fn serialize<S: Serializer>(
        date: &chrono::NaiveDate,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&date.format("%Y-%m-%d").to_string())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<chrono::NaiveDate, D::Error> {
        let raw = String::deserialize(deserializer)?;
        chrono::NaiveDate::parse_from_str(&raw, "%Y-%m-%d").map_err(serde::de::Error::custom)
    }
}

impl DomainEvent for EmployeeEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Hired { .. } => Self::NAMES[0],
            Self::Amended { .. } => Self::NAMES[1],
            Self::Reparented { .. } => Self::NAMES[2],
            Self::Transferred { .. } => Self::NAMES[3],
            Self::Left { .. } => Self::NAMES[4],
            Self::Rehired { .. } => Self::NAMES[5],
            Self::DocumentRecorded { .. } => Self::NAMES[6],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

impl EmployeeEvent {
    pub const NAMES: [&'static str; 7] = [
        "hr.employee.hired",
        "hr.employee.amended",
        "hr.employee.reparented",
        "hr.employee.transferred",
        "hr.employee.left",
        "hr.employee.rehired",
        "hr.employee.document_recorded",
    ];
}

#[derive(Debug, Default, Clone)]
pub struct Employee {
    pub hired: bool,
    pub name: String,
    pub name_latin: Option<String>,
    pub national_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub reports_to: Option<AggregateId>,
    pub branch: Option<AggregateId>,
    pub left_at: Option<Timestamp>,
    /// What they hold, one per kind — **the current one**, because a renewal
    /// replaces rather than accumulates and nothing here asks what an expired
    /// document used to say. The log keeps the history; this is the state a
    /// decision is made from.
    pub documents: Vec<Document>,
}

impl Aggregate for Employee {
    type Event = EmployeeEvent;

    fn domain() -> DomainName {
        crate::domain("hr_employee")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            EmployeeEvent::Hired {
                name,
                name_latin,
                national_id,
                email,
                phone,
                reports_to,
                branch,
                ..
            } => {
                self.hired = true;
                self.name.clone_from(name);
                self.name_latin.clone_from(name_latin);
                self.national_id.clone_from(national_id);
                self.email.clone_from(email);
                self.phone.clone_from(phone);
                self.reports_to.clone_from(reports_to);
                self.branch.clone_from(branch);
            }
            EmployeeEvent::Amended {
                name,
                name_latin,
                national_id,
                email,
                phone,
                ..
            } => {
                self.name.clone_from(name);
                self.name_latin.clone_from(name_latin);
                self.national_id.clone_from(national_id);
                self.email.clone_from(email);
                self.phone.clone_from(phone);
            }
            EmployeeEvent::Reparented { reports_to, .. } => {
                self.reports_to.clone_from(reports_to);
            }
            EmployeeEvent::Transferred { branch, .. } => self.branch.clone_from(branch),
            EmployeeEvent::Left { at, .. } => self.left_at = Some(*at),
            EmployeeEvent::Rehired { .. } => self.left_at = None,
            EmployeeEvent::DocumentRecorded {
                kind,
                number,
                expires_on,
                ..
            } => {
                let recorded = Document {
                    kind: *kind,
                    number: number.clone(),
                    expires_on: *expires_on,
                };
                match self.documents.iter_mut().find(|d| d.kind == *kind) {
                    Some(held) => *held = recorded,
                    None => self.documents.push(recorded),
                }
            }
        }
    }
}

impl Employee {
    #[must_use]
    pub const fn exists(&self) -> bool {
        self.hired
    }

    /// Whether this person is on the books today.
    ///
    /// **A leaver keeps their record and loses nothing they did.** The same
    /// distinction `crm` draws about an archived customer and `branches` about
    /// a closed branch, and for the same reason.
    #[must_use]
    pub const fn is_employed(&self) -> bool {
        self.hired && self.left_at.is_none()
    }

    /// **Whether this person may be rostered on this day.**
    ///
    /// The seam `booking` calls before it puts somebody on a job, and the
    /// reason 9e says *refuses* rather than *warns*: an expired iqama does not
    /// mean a reminder somebody ignored, it means a person who may not legally
    /// work, and rostering them is the employer's offence.
    ///
    /// Somebody who has left may not work either — that is the same question
    /// asked one step earlier, and answering it here means a caller cannot get
    /// one right and forget the other.
    ///
    /// A person with **no documents recorded at all** may work. That is
    /// deliberate: a business that has not started recording documents must not
    /// find its whole rota refused the day this module is enabled, and the
    /// health check below is what tells them the records are missing.
    #[must_use]
    pub fn may_work_on(&self, day: chrono::NaiveDate) -> bool {
        self.is_employed()
            && self
                .documents
                .iter()
                .all(|d| !d.kind.blocks_work() || d.valid_on(day))
    }

    /// The documents that have lapsed as at this day, for a message that says
    /// *which*.
    #[must_use]
    pub fn lapsed_on(&self, day: chrono::NaiveDate) -> Vec<&Document> {
        self.documents
            .iter()
            .filter(|d| d.kind.blocks_work() && !d.valid_on(day))
            .collect()
    }

    #[must_use]
    pub fn details(&self) -> Details {
        Details {
            name: self.name.clone(),
            name_latin: self.name_latin.clone(),
            national_id: self.national_id.clone(),
            email: self.email.clone(),
            phone: self.phone.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details(name: &str) -> Details {
        Details {
            name: name.to_owned(),
            name_latin: None,
            national_id: None,
            email: None,
            phone: Some("+966500000000".to_owned()),
        }
    }

    #[test]
    fn a_person_needs_a_name_and_a_way_to_be_reached() {
        assert_eq!(details("سارة").check(), Ok(()));
        assert_eq!(details("  ").check(), Err(BadEmployee::NoName));

        let mut unreachable = details("سارة");
        unreachable.phone = None;
        assert_eq!(unreachable.check(), Err(BadEmployee::NoContact));

        unreachable.email = Some("s@example.test".to_owned());
        assert_eq!(unreachable.check(), Ok(()));
    }

    /// **A leaver keeps their record.** They are on last year's payroll and
    /// whatever they approved, and a record that vanished would take the
    /// meaning of both with it.
    #[test]
    fn somebody_who_leaves_still_exists() {
        let mut person = Employee::default();
        let at = chrono::Utc::now();
        person.apply(&EmployeeEvent::Hired {
            name: "سارة".to_owned(),
            name_latin: None,
            national_id: None,
            email: None,
            phone: Some("+966500000000".to_owned()),
            reports_to: None,
            branch: None,
            at,
        });
        assert!(person.is_employed());

        person.apply(&EmployeeEvent::Left {
            why: "استقالت".to_owned(),
            at,
        });
        assert!(person.exists(), "the record was forgotten");
        assert!(!person.is_employed());

        person.apply(&EmployeeEvent::Rehired { at });
        assert!(person.is_employed());
    }

    /// Amending details does not touch the reporting line, and this is the test
    /// that keeps the two events separate — an `Amended` that could move
    /// somebody is one an auditor cannot read.
    #[test]
    fn amending_details_cannot_move_somebody_in_the_chart() {
        let mut person = Employee::default();
        let at = chrono::Utc::now();
        let boss = AggregateId::new("EMP-BOSS").unwrap_or_else(|_| unreachable!("a literal"));
        person.apply(&EmployeeEvent::Hired {
            name: "سارة".to_owned(),
            name_latin: None,
            national_id: None,
            email: None,
            phone: Some("+966500000000".to_owned()),
            reports_to: Some(boss.clone()),
            branch: None,
            at,
        });

        person.apply(&EmployeeEvent::Amended {
            name: "سارة الأحمد".to_owned(),
            name_latin: Some("Sara Alahmad".to_owned()),
            national_id: None,
            email: None,
            phone: Some("+966500000001".to_owned()),
            at,
        });

        assert_eq!(person.name, "سارة الأحمد");
        assert_eq!(
            person.reports_to,
            Some(boss),
            "amending details moved somebody in the org chart"
        );
    }
}
