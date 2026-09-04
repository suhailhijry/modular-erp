//! A card a customer left behind, and what this system is allowed to keep of it.
//!
//! # The token is not in the log
//!
//! Everything else in this module is event-sourced, and this deliberately is
//! not — not all of it. The **token** is sealed into `module_secret` beside the
//! gateway credentials; the log holds only what a person needs to recognise the
//! card: whose it is, the brand, the last four digits and when it expires.
//!
//! Two reasons, and the second is the one that decides it.
//!
//! A token charges money. It is weaker than a card number — a gateway will only
//! act on one alongside the secret key — but it is a payment credential, and
//! the credentials that build a client are already sealed for exactly that
//! reason. A payment credential in the clear in a table that is copied into
//! every shadow schema, every rebuild and every demo is not a place for one.
//!
//! And **"forget my card" has to mean it**. An event log is append-only by
//! design; a token written into one is a token this system holds for ever, and
//! no amount of projecting it away changes that. Sealed in a table, forgetting
//! is a delete. So [`CardEvent::Forgotten`] records *that* a card was forgotten
//! — which is history, and belongs in the log — while the thing that could
//! charge it goes away.
//!
//! # Which provider, and why only one so far
//!
//! Cards are Moyasar's. Tabby and Tamara are buy-now-pay-later: the customer is
//! scored and pays the *provider* in instalments, and there is nothing to save
//! — no token exists on this side to charge later. So a card names its provider
//! and [`crate::commands::save_card_in`] refuses a provider that cannot charge
//! a token, rather than storing a row that can never be used.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

/// What happened to one saved card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CardEvent {
    /// A customer's card was kept for next time.
    ///
    /// **Everything here is display.** What actually charges is the token, and
    /// it is not on this event — see the module docs.
    Saved {
        /// The `crm` customer it belongs to. A reference, not a join: reading
        /// across into `crm`'s projection group is what L3 forbids, and
        /// nothing here needs to.
        customer: AggregateId,
        /// `moyasar`. See the module docs for why the list is short.
        provider: String,
        /// `visa`, `mada`, `master` — whatever the gateway called it.
        brand: String,
        /// The last four digits, and **only** the last four.
        last4: String,
        /// 1–12.
        expiry_month: i16,
        /// Four digits.
        expiry_year: i16,
        saved_at: Timestamp,
    },
    /// The customer asked for it to be removed, or somebody removed it for
    /// them. The token is deleted in the same transaction.
    Forgotten { forgotten_at: Timestamp },
}

impl CardEvent {
    pub const NAMES: [&'static str; 2] = ["payments.card.saved", "payments.card.forgotten"];
}

impl DomainEvent for CardEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Saved { .. } => Self::NAMES[0],
            Self::Forgotten { .. } => Self::NAMES[1],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// One saved card, as the log describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Card {
    pub saved: bool,
    pub forgotten: bool,
    pub customer: Option<AggregateId>,
    pub provider: String,
}

impl Card {
    /// Whether a charge may be requested against it.
    ///
    /// A forgotten card is not chargeable **and never becomes one again**:
    /// re-saving means the customer entering their card afresh, which mints a
    /// new token under a new id. Reviving this one would charge a token the
    /// customer asked to be rid of.
    #[must_use]
    pub const fn is_chargeable(&self) -> bool {
        self.saved && !self.forgotten
    }
}

impl Aggregate for Card {
    type Event = CardEvent;

    fn domain() -> DomainName {
        crate::domain("payments_card")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            CardEvent::Saved {
                customer, provider, ..
            } => {
                self.saved = true;
                self.customer = Some(customer.clone());
                self.provider.clone_from(provider);
            }
            CardEvent::Forgotten { .. } => self.forgotten = true,
        }
    }
}

/// The providers a card can actually be saved against.
///
/// **A subset of `crate::PROVIDERS`, and it is one long.** Tabby and Tamara are
/// buy-now-pay-later: the customer is scored and pays the provider in
/// instalments, and no token exists on this side to charge again later. A row
/// naming one of them would be a saved card as far as anybody picking from a
/// list is concerned, and they would find out at the till.
pub const SAVES_CARDS: &[&str] = &["moyasar"];

/// Where one card's token is sealed.
///
/// Namespaced under the same `payments.` prefix the gateway credentials use, so
/// everything this module can unseal reads as one family in `module_secret`.
#[must_use]
pub fn token_key(card: &AggregateId) -> String {
    format!("payments.card.{card}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> AggregateId {
        AggregateId::new(value).unwrap_or_else(|_| unreachable!())
    }

    fn saved() -> CardEvent {
        CardEvent::Saved {
            customer: id("CUST-1"),
            provider: "moyasar".to_owned(),
            brand: "visa".to_owned(),
            last4: "4242".to_owned(),
            expiry_month: 5,
            expiry_year: 2028,
            saved_at: Timestamp::from(chrono::Utc::now()),
        }
    }

    fn replay(events: &[CardEvent]) -> Card {
        let mut card = Card::default();
        for event in events {
            Aggregate::apply(&mut card, event);
        }
        card
    }

    #[test]
    fn a_saved_card_can_be_charged_and_knows_whose_it_is() {
        let card = replay(&[saved()]);
        assert!(card.is_chargeable());
        assert_eq!(card.customer, Some(id("CUST-1")));
        assert_eq!(card.provider, "moyasar");
    }

    /// **Forgetting is final.** A second `Saved` on the same id would be a
    /// revival of a token the customer asked to be rid of, so nothing emits
    /// one — and if something did, this is where it would be caught.
    #[test]
    fn a_forgotten_card_stays_forgotten() {
        let card = replay(&[
            saved(),
            CardEvent::Forgotten {
                forgotten_at: Timestamp::from(chrono::Utc::now()),
            },
        ]);
        assert!(!card.is_chargeable());

        let revived = replay(&[
            saved(),
            CardEvent::Forgotten {
                forgotten_at: Timestamp::from(chrono::Utc::now()),
            },
            saved(),
        ]);
        assert!(!revived.is_chargeable(), "a token was un-forgotten");
    }

    /// **The token is nowhere in the log.** The whole design rests on this, and
    /// a field added later would break it silently.
    #[test]
    fn nothing_a_card_writes_down_can_charge_it() {
        let json = serde_json::to_string(&saved()).unwrap_or_else(|_| unreachable!());
        assert!(!json.contains("token"), "{json}");
        assert!(json.contains("4242"), "{json}");
    }

    /// A provider that saves cards and is not one this module can be
    /// configured for would be unreachable; the reverse is ordinary.
    #[test]
    fn every_provider_that_saves_cards_is_a_provider() {
        for provider in SAVES_CARDS {
            assert!(
                crate::PROVIDERS.contains(provider),
                "{provider} saves cards and cannot be configured"
            );
        }
    }

    #[test]
    fn every_event_has_a_name_and_they_are_all_different() {
        let events = [
            saved(),
            CardEvent::Forgotten {
                forgotten_at: Timestamp::from(chrono::Utc::now()),
            },
        ];
        let names: Vec<_> = events.iter().map(|e| e.event_name().to_string()).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "{names:?}");
        assert_eq!(names.len(), CardEvent::NAMES.len());
    }
}
