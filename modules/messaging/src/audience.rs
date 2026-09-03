//! **A template names an audience, not an address.**
//!
//! # The problem this is the answer to
//!
//! The system this phase was read against freezes a phone number into the thing
//! that will be sent. A customer who changes their number keeps getting
//! messages at the old one until somebody notices, and there is nothing in the
//! record that says why.
//!
//! So nothing here holds an address. A template says *the client*, *the person
//! doing the work*, *whoever runs that branch* — and the address is a query
//! against the read model at the moment the message is sent. A person who
//! changed their number this morning gets this afternoon's reminder.
//!
//! # Why resolution needs a subject
//!
//! "The client" is not a person until you say *the client of what*. An audience
//! and a [`Subject`] together name somebody; either alone names nobody.

use erp_types::AggregateId;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

use crate::channel::Channel;

/// What a message is about.
///
/// The kinds a template may be written against. Each one decides which bindings
/// exist, which is what makes an unresolvable binding a **save-time** failure
/// rather than something a customer waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    Reservation,
    Invoice,
    Customer,
    Employee,
}

impl Topic {
    pub const ALL: [Self; 4] = [
        Self::Reservation,
        Self::Invoice,
        Self::Customer,
        Self::Employee,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reservation => "reservation",
            Self::Invoice => "invoice",
            Self::Customer => "customer",
            Self::Employee => "employee",
        }
    }

    /// Which audiences a template on this topic may name.
    ///
    /// **An invoice has no employee on it**, so a template about one cannot be
    /// addressed to the person who did the work — and saying so here is what
    /// turns that into a refusal when the template is written.
    #[must_use]
    pub const fn audiences(self) -> &'static [Audience] {
        match self {
            Self::Reservation => &[
                Audience::Client,
                Audience::Worker,
                Audience::BranchManager,
                Audience::Operator,
            ],
            Self::Invoice => &[
                Audience::Client,
                Audience::BranchManager,
                Audience::Operator,
            ],
            Self::Customer => &[Audience::Client, Audience::Operator],
            Self::Employee => &[
                Audience::Worker,
                Audience::BranchManager,
                Audience::Operator,
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not something a message can be about")]
pub struct UnknownTopic(pub String);

impl std::str::FromStr for Topic {
    type Err = UnknownTopic;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|t| t.as_str() == s)
            .ok_or_else(|| UnknownTopic(s.to_owned()))
    }
}

/// The thing a message is about, and which one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub topic: Topic,
    pub id: AggregateId,
}

impl Subject {
    #[must_use]
    pub const fn new(topic: Topic, id: AggregateId) -> Self {
        Self { topic, id }
    }
}

/// Who a template is written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    /// The customer the subject belongs to.
    Client,
    /// The employee doing the work — the one assigned to the booking, or the
    /// employee the message is about.
    Worker,
    /// **Whoever runs the branch the subject is at.**
    ///
    /// Derived from the org chart rather than a field on a branch: the manager
    /// is whoever at that branch reports to nobody at that branch. Two people
    /// can satisfy that and both get the message, which is the right answer —
    /// a branch with two managers has two managers.
    BranchManager,
    /// A named identity, for a message to somebody in particular.
    Operator,
}

impl Audience {
    pub const ALL: [Self; 4] = [
        Self::Client,
        Self::Worker,
        Self::BranchManager,
        Self::Operator,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Worker => "worker",
            Self::BranchManager => "branch_manager",
            Self::Operator => "operator",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not an audience")]
pub struct UnknownAudience(pub String);

impl std::str::FromStr for Audience {
    type Err = UnknownAudience;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|a| a.as_str() == s)
            .ok_or_else(|| UnknownAudience(s.to_owned()))
    }
}

/// Somewhere a message can actually be delivered.
///
/// **Never stored on a template and never in an event.** This is what
/// resolution produces, moments before the effect is enqueued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub channel: Channel,
    /// An email address, an E.164 number, or a device token.
    pub value: String,
    /// **Push only** — what kind of token `value` is. See
    /// [`crate::send::Outbound::platform`].
    pub platform: Option<crate::push::Platform>,
}

/// Who a message would reach, right now.
///
/// Empty is not an error here — it is a fact the caller has to decide about,
/// and [`crate::send`] turns it into a refusal that names the audience. A
/// customer with no mobile number is a real and ordinary thing.
pub async fn resolve(
    conn: &mut PgConnection,
    audience: Audience,
    subject: &Subject,
    channel: Channel,
    operator: Option<&str>,
) -> Result<Vec<Address>, sqlx::Error> {
    let people = match audience {
        Audience::Client => client_of(conn, subject).await?,
        Audience::Worker => worker_of(conn, subject).await?,
        Audience::BranchManager => managers_of(conn, subject).await?,
        // The caller names them, so there is nothing to look up but their
        // contact details.
        Audience::Operator => match operator {
            Some(id) => employee(conn, id).await?.into_iter().collect(),
            None => Vec::new(),
        },
    };

    Ok(people
        .into_iter()
        .filter_map(|person| person.reachable_on(channel))
        .collect())
}

/// Somebody, and the ways they can be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Person {
    email: Option<String>,
    phone: Option<String>,
}

impl Person {
    /// The address for one channel, if they have one.
    ///
    /// **Push is deliberately not here.** A device token is not a property of a
    /// person in any read model — it arrives from a device and lives in
    /// `push_token` — so it is resolved separately, by [`crate::push::tokens`].
    fn reachable_on(self, channel: Channel) -> Option<Address> {
        let value = match channel {
            Channel::Email => self.email,
            // WhatsApp is addressed by phone number, which is the whole reason
            // a business uses it rather than an app of its own.
            Channel::Sms | Channel::WhatsApp => self.phone,
            Channel::Push => None,
        }?;
        Some(Address {
            channel,
            value,
            // Never push: a device is not a property of a person in any read
            // model, so `crate::send` resolves those separately.
            platform: None,
        })
    }
}

/// The customer a subject belongs to.
async fn client_of(conn: &mut PgConnection, subject: &Subject) -> Result<Vec<Person>, sqlx::Error> {
    let id = match subject.topic {
        Topic::Customer => Some(subject.id.as_str().to_owned()),
        Topic::Reservation => booking::reservation(conn, subject.id.as_str())
            .await?
            .and_then(|r| r.summary.customer_id),
        Topic::Invoice => sales::invoice(conn, subject.id.as_str())
            .await?
            .and_then(|i| i.summary.customer_id),
        Topic::Employee => None,
    };

    let Some(id) = id else {
        return Ok(Vec::new());
    };
    Ok(crm::customer(conn, &id)
        .await?
        .map(|c| Person {
            email: c.summary.email,
            phone: c.summary.phone,
        })
        .into_iter()
        .collect())
}

/// The employee doing the work.
async fn worker_of(conn: &mut PgConnection, subject: &Subject) -> Result<Vec<Person>, sqlx::Error> {
    let id = match subject.topic {
        Topic::Employee => Some(subject.id.as_str().to_owned()),
        // Whoever was assigned to the first line that has somebody. A booking
        // with two stylists on two lines reaches the first, which is a
        // deliberate simplification — ponytail: every assigned worker, when a
        // business asks for it, and the shape below already returns a list.
        Topic::Reservation => assigned(conn, subject.id.as_str()).await?,
        Topic::Invoice | Topic::Customer => None,
    };

    let Some(id) = id else {
        return Ok(Vec::new());
    };
    Ok(employee(conn, &id).await?.into_iter().collect())
}

/// The employee a booking's resources name, if any.
async fn assigned(
    conn: &mut PgConnection,
    reservation: &str,
) -> Result<Option<String>, sqlx::Error> {
    let Some(detail) = booking::reservation(conn, reservation).await? else {
        return Ok(None);
    };

    for line in &detail.lines {
        for held in &line.takes {
            if let Some(resource) = booking::resource(conn, held.resource.as_str()).await?
                && let Some(employee) = resource.summary.employee
            {
                return Ok(Some(employee));
            }
        }
    }
    Ok(None)
}

/// Whoever runs the branch a subject is at.
///
/// **From the org chart**, not from a field on a branch: a manager is whoever
/// at that branch reports to nobody at that branch. `branches` models places,
/// not who runs them, and adding a manager column there would be a second
/// answer to a question `hr` already answers.
async fn managers_of(
    conn: &mut PgConnection,
    subject: &Subject,
) -> Result<Vec<Person>, sqlx::Error> {
    let Some(branch) = branch_of(conn, subject).await? else {
        return Ok(Vec::new());
    };

    // A branch with more staff than this has an org chart nobody is reading off
    // one page, and the manager is near the top of it either way.
    let staff = hr::employees(conn, Some(&branch), false, 500, None)
        .await?
        .items;
    let here: std::collections::BTreeSet<&str> = staff.iter().map(|e| e.id.as_str()).collect();

    Ok(staff
        .iter()
        .filter(|e| {
            e.reports_to
                .as_deref()
                .is_none_or(|manager| !here.contains(manager))
        })
        .map(|e| Person {
            email: e.email.clone(),
            phone: e.phone.clone(),
        })
        .collect())
}

/// Which branch a subject is at.
async fn branch_of(
    conn: &mut PgConnection,
    subject: &Subject,
) -> Result<Option<String>, sqlx::Error> {
    Ok(match subject.topic {
        Topic::Employee => hr::employee(conn, subject.id.as_str())
            .await?
            .and_then(|e| e.branch),
        Topic::Reservation => {
            // A booking is at whichever branch its resources are at.
            let Some(detail) = booking::reservation(conn, subject.id.as_str()).await? else {
                return Ok(None);
            };
            let mut found = None;
            for line in &detail.lines {
                for held in &line.takes {
                    if let Some(resource) = booking::resource(conn, held.resource.as_str()).await?
                        && let Some(branch) = resource.summary.branch
                    {
                        found = Some(branch);
                        break;
                    }
                }
            }
            found
        }
        // An invoice's branch is on its postings, which is `ledger`'s and a
        // different group. Not reached for; a template about an invoice
        // addressed to a branch manager resolves to nobody and says so.
        Topic::Invoice | Topic::Customer => None,
    })
}

async fn employee(conn: &mut PgConnection, id: &str) -> Result<Option<Person>, sqlx::Error> {
    Ok(hr::employee(conn, id).await?.map(|e| Person {
        email: e.email,
        phone: e.phone,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invoice_has_no_worker_on_it() {
        assert!(!Topic::Invoice.audiences().contains(&Audience::Worker));
        assert!(Topic::Reservation.audiences().contains(&Audience::Worker));
    }

    #[test]
    fn every_topic_can_be_addressed_to_somebody() {
        for topic in Topic::ALL {
            assert!(
                !topic.audiences().is_empty(),
                "{topic:?} has no audience, so no template could be written about it"
            );
        }
    }

    #[test]
    fn names_round_trip() {
        for topic in Topic::ALL {
            assert_eq!(topic.as_str().parse(), Ok(topic));
        }
        for audience in Audience::ALL {
            assert_eq!(audience.as_str().parse(), Ok(audience));
        }
    }

    /// A person with no mobile number is not reachable on SMS, and that is a
    /// fact rather than a failure. Push is never a person's property.
    #[test]
    fn a_person_is_reachable_only_where_they_have_an_address() {
        let both = Person {
            email: Some("a@b.test".to_owned()),
            phone: Some("+966500000000".to_owned()),
        };
        assert!(both.clone().reachable_on(Channel::Email).is_some());
        assert!(both.clone().reachable_on(Channel::Sms).is_some());
        assert!(both.clone().reachable_on(Channel::WhatsApp).is_some());
        assert!(both.reachable_on(Channel::Push).is_none());

        let letters_only = Person {
            email: Some("a@b.test".to_owned()),
            phone: None,
        };
        assert!(letters_only.reachable_on(Channel::Sms).is_none());
    }
}
