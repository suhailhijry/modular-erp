//! What the system tells people about.
//!
//! # Why this is a closed set and not a template a tenant writes
//!
//! Nobody writes a template for *a booking arrived*. A reminder is a message a
//! business composes and sends on purpose; a notification is the system saying
//! something happened, and the tenant's choice is whether they want it and
//! where — which is [`crate::Preferences`]. The wording lives in
//! [`crate::copy`], in both languages, and a tenant who does want their own
//! words overrides one kind with a template of the same name.
//!
//! # Why a kind may not be addressed to a client
//!
//! An inbox belongs to a login and a customer has none: there is no customer
//! portal, and inventing one here would be a product decision disguised as a
//! notification. Reaching customers is `messaging::send`'s job and always was.

use messaging::{Audience, Topic};

/// One thing the system knows how to tell somebody about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Somebody booked. The one a counter screen most wants.
    BookingReserved,
    /// A customer's money arrived.
    PaymentsSettled,
    /// …or did not.
    PaymentsFailed,
    /// ZATCA refused a document, and the document is what is wrong.
    TaxRefused,
    /// An iqama, licence or medical is about to lapse — or has.
    DocumentExpiring,
}

impl Kind {
    pub const ALL: [Self; 5] = [
        Self::BookingReserved,
        Self::PaymentsSettled,
        Self::PaymentsFailed,
        Self::TaxRefused,
        Self::DocumentExpiring,
    ];

    /// The name it is stored and configured under.
    ///
    /// **Also the template name that overrides its wording**, so a tenant
    /// writing their own words for `booking.reserved` names the thing they are
    /// changing rather than a second identifier they have to be told about.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BookingReserved => "booking_reserved",
            Self::PaymentsSettled => "payments_settled",
            Self::PaymentsFailed => "payments_failed",
            Self::TaxRefused => "tax_refused",
            Self::DocumentExpiring => "document_expiring",
        }
    }

    /// What a notification of this kind is about, which decides its bindings.
    #[must_use]
    pub const fn topic(self) -> Topic {
        match self {
            Self::BookingReserved => Topic::Reservation,
            // **The invoice, not the payment.** A payment is not a topic a
            // template can be written about, and the thing a person wants to
            // open is the document the money was against.
            Self::PaymentsSettled | Self::PaymentsFailed | Self::TaxRefused => Topic::Invoice,
            Self::DocumentExpiring => Topic::Employee,
        }
    }

    /// Who to tell, **in order**: the first audience that resolves to anybody
    /// with a login wins.
    ///
    /// That is what lets one field say "the stylist it was booked with, and
    /// whoever runs the branch when nobody was assigned" without a fallback
    /// written out at each producer.
    #[must_use]
    pub const fn audiences(self) -> &'static [Audience] {
        match self {
            Self::BookingReserved | Self::DocumentExpiring => {
                &[Audience::Worker, Audience::BranchManager]
            }
            Self::PaymentsSettled | Self::PaymentsFailed | Self::TaxRefused => {
                &[Audience::BranchManager]
            }
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not something this system announces")]
pub struct UnknownKind(pub String);

impl std::str::FromStr for Kind {
    type Err = UnknownKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| UnknownKind(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for kind in Kind::ALL {
            assert_eq!(kind.as_str().parse(), Ok(kind));
        }
    }

    /// **Every kind can actually be addressed to somebody.**
    ///
    /// A kind whose audience is not one its topic allows would resolve to
    /// nobody for ever, and the only sign of it would be a producer that never
    /// announces anything. `messaging` already states which audiences each
    /// topic has; this is the same table asked the other way round.
    #[test]
    fn every_kind_is_addressed_to_an_audience_its_topic_has() {
        for kind in Kind::ALL {
            let allowed = kind.topic().audiences();
            assert!(!kind.audiences().is_empty(), "{kind} reaches nobody");
            for audience in kind.audiences() {
                assert!(
                    allowed.contains(audience),
                    "{kind} is addressed to {} but a message about {} cannot be",
                    audience.as_str(),
                    kind.topic().as_str()
                );
                // An inbox belongs to a login and a customer has none.
                assert_ne!(
                    *audience,
                    Audience::Client,
                    "{kind} is addressed to a customer, who cannot log in to read it"
                );
            }
        }
    }
}
