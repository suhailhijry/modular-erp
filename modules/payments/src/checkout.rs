//! What a provider that hosts its own checkout has to be told.
//!
//! # Why this is recorded, not looked up
//!
//! A buy-now-pay-later provider is not a card gateway: the customer is scored
//! before they are lent to, and the provider asks who they are, where what
//! they are buying goes, and where to send them afterwards. All of that is
//! known at the moment the charge is asked for, and by the caller that asked —
//! the public deposit route, standing where `booking`, `branches` and
//! `payments` meet — and none of it is known to the worker that later opens the
//! checkout, which may load no aggregate (L7) and may not name those modules.
//! So the request carries it, frozen on the event (L5): what the lender was
//! told is what this system recorded telling it.
//!
//! # Why the worker opens it
//!
//! Creating a checkout is an outbound call to a third party, and this system
//! makes those from the worker — the same argument `charge_requested` makes
//! about a saved card. The route records the request and answers at once;
//! `open_checkouts` creates the session on the next pass and records where the
//! customer is to be sent, and the public read beside the route answers it. A
//! customer waits a tick rather than a request handler holding a connection
//! for as long as somebody else's server takes.
//!
//! # And why capture is the sweep's
//!
//! A lender authorises when the customer commits and settles only what the
//! merchant captures — Tabby's and Tamara's words, not this system's. So an
//! authorised payment found by the settle pass is captured there, in full and
//! once, before anything is posted. Nobody has to remember.

use erp_types::{AggregateId, Money, Timestamp};
use serde::{Deserialize, Serialize};

/// Everything a hosted checkout is told. Plain data, frozen on the event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkout {
    pub shopper: Shopper,
    pub deliver_to: Place,
    pub landing: Landing,
    /// Shown to the customer on the provider's page.
    pub description: String,
    pub items: Vec<Line>,
    /// What of the amount is tax, for the lender's own figure.
    pub tax: Money,
}

/// Who is paying, as the lender scores them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shopper {
    pub name: String,
    pub email: String,
    /// E.164. Where the lender's one-time code goes.
    pub phone: String,
    /// When this person became a customer here — the moment of the booking,
    /// for a stranger.
    pub since: Timestamp,
    /// Purchases completed here before this one.
    pub purchases: u32,
}

/// Where what is bought goes. For a service, the branch it is delivered at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    pub line: String,
    pub city: String,
    pub postcode: String,
    /// ISO 3166-1 alpha-2.
    pub country: String,
}

/// Where the customer lands afterwards, and where the provider reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Landing {
    pub success: String,
    pub cancel: String,
    pub failure: String,
    /// This system's hook for the provider, when it takes one per checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification: Option<String>,
}

/// One line the lender is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub title: String,
    /// The lender's high-level category — `Services`, `Beauty`.
    pub category: String,
    pub quantity: u32,
    /// Tax included, because that is what the customer is being asked for.
    pub unit_price: Money,
}

impl Checkout {
    /// The charge to open at the gateway, keyed on this system's own id.
    #[must_use]
    pub fn charge(&self, id: &AggregateId, amount: Money) -> erp_payments::Charge {
        erp_payments::Charge {
            reference: id.as_str().to_owned(),
            amount,
            returns: erp_payments::Returns {
                success: self.landing.success.clone(),
                cancel: self.landing.cancel.clone(),
                failure: self.landing.failure.clone(),
                notification: self.landing.notification.clone(),
            },
            source: erp_payments::Source::Hosted,
            description: self.description.clone(),
            buyer: Some(erp_payments::Buyer {
                name: self.shopper.name.clone(),
                email: self.shopper.email.clone(),
                phone: self.shopper.phone.clone(),
                registered_since: self.shopper.since,
                purchases: self.shopper.purchases,
            }),
            basket: Some(erp_payments::Basket {
                reference: id.as_str().to_owned(),
                deliver_to: erp_payments::Address {
                    line: self.deliver_to.line.clone(),
                    city: self.deliver_to.city.clone(),
                    postcode: self.deliver_to.postcode.clone(),
                    country: self.deliver_to.country.clone(),
                },
                tax: self.tax,
                items: self
                    .items
                    .iter()
                    .map(|line| erp_payments::Item {
                        title: line.title.clone(),
                        category: line.category.clone(),
                        quantity: line.quantity,
                        unit_price: line.unit_price,
                    })
                    .collect(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sar(minor: i64) -> Money {
        Money::from_minor(minor, "SAR".parse().expect("a currency"))
    }

    /// **What the lender is told is what was recorded.** Every field of the
    /// request comes from the frozen checkout and the payment's own id, and
    /// the source is hosted — nothing here ever carries a card token.
    #[test]
    fn a_checkout_becomes_a_hosted_charge_under_the_payments_own_id() {
        let checkout = Checkout {
            shopper: Shopper {
                name: "سارة".to_owned(),
                email: "sara@example.com".to_owned(),
                phone: "+966500000001".to_owned(),
                since: "2026-05-01T09:00:00Z".parse().expect("an instant"),
                purchases: 0,
            },
            deliver_to: Place {
                line: "King Fahd Road 12".to_owned(),
                city: "Riyadh".to_owned(),
                postcode: "12211".to_owned(),
                country: "SA".to_owned(),
            },
            landing: Landing {
                success: "https://salon.example/paid".to_owned(),
                cancel: "https://salon.example/cancelled".to_owned(),
                failure: "https://salon.example/declined".to_owned(),
                notification: Some("https://acme.erp.example/v1/hooks/tamara".to_owned()),
            },
            description: "Booking deposit".to_owned(),
            items: vec![Line {
                title: "Booking deposit: قص".to_owned(),
                category: "Services".to_owned(),
                quantity: 1,
                unit_price: sar(11_500),
            }],
            tax: sar(1_500),
        };
        let id = AggregateId::new("pay_1").expect("an id");
        let charge = checkout.charge(&id, sar(11_500));

        assert_eq!(charge.reference, "pay_1");
        assert_eq!(charge.source, erp_payments::Source::Hosted);
        assert_eq!(charge.amount, sar(11_500));
        let buyer = charge.buyer.expect("a buyer");
        assert_eq!(buyer.email, "sara@example.com");
        assert_eq!(buyer.purchases, 0);
        let basket = charge.basket.expect("a basket");
        assert_eq!(basket.reference, "pay_1");
        assert_eq!(basket.tax, sar(1_500));
        assert_eq!(basket.deliver_to.city, "Riyadh");
        assert_eq!(basket.items.len(), 1);
        assert_eq!(
            charge.returns.notification.as_deref(),
            Some("https://acme.erp.example/v1/hooks/tamara")
        );

        // Survives the log: what went in is what comes back out.
        let json = serde_json::to_string(&checkout).expect("serializes");
        let back: Checkout = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, checkout);
    }
}
