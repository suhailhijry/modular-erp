//! Asking a gateway for money, and reading the answer.
//!
//! # What this crate knows, and what it must never learn
//!
//! It knows: an amount, a reference, a card token, and the shape of one
//! provider's HTTP API. It does not know which invoice a payment clears, which
//! ledger accounts it posts to, or whether a refund is a credit note — those
//! are `modules/payments`', and the day this crate learns any of them is the
//! day it stops being swappable for another provider.
//!
//! The same split `erp-storage` makes, for the same reason: a tenant's gateway
//! is a deployment fact, and a business that already has a Moyasar account
//! should not need a different build.
//!
//! # A card number never reaches this process
//!
//! Moyasar's terms are explicit — *"Sending cardholder data to the merchant
//! backend is prohibited and will result in canceling the agreement"* — and
//! every other gateway says something similar. So [`Source`] carries a
//! **token**, minted in the customer's browser against the publishable key, and
//! there is deliberately no variant that holds a PAN. That is not a
//! convenience: a struct with a `number: String` on it is a struct somebody
//! eventually fills in.
//!
//! # `201` is not paid
//!
//! Creating a payment succeeds long before anybody has been charged. A card
//! that needs 3-D Secure comes back [`Status::Initiated`] with somewhere for
//! the customer to go, and what happens next arrives as a callback — or does
//! not arrive at all, because the customer closed the tab.
//!
//! So [`Charged`] carries a status rather than a boolean, and the only honest
//! way to learn the ending is [`Gateway::fetch`].
//!
//! # A callback is not evidence
//!
//! **No gateway researched for this build signs its webhook bodies.** Moyasar
//! puts a shared secret *inside the JSON*; Tabby has no signature at all; Tamara
//! sends a JWT. None of that is a signature over the payload, so none of it
//! proves the amount.
//!
//! The rule this crate encodes is therefore: a callback tells you **which
//! payment to look at**, and nothing else. [`Gateway::fetch`] is what says what
//! happened, over an authenticated connection, and the amount and currency are
//! checked against what was expected before a single riyal is recorded. That is
//! what Moyasar's own reference implementation does, and it is the only design
//! that survives somebody guessing the callback URL.

pub mod decimal;
pub mod messages;

mod moyasar;
mod tabby;
mod tamara;

pub use moyasar::Moyasar;
pub use tabby::{Tabby, UAE};
pub use tamara::{SANDBOX, Tamara};

use erp_i18n::{Localize, Message, MessageArg, StaticCatalog};
use erp_types::Money;
use serde::{Deserialize, Serialize};

/// This crate's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

/// Where the money is coming from.
///
/// **No card number, deliberately.** See the crate docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// A token the gateway minted in the customer's browser. What a saved card
    /// is, and the only thing this system ever holds.
    Token { token: String },
    /// Nothing to send: the provider hosts the page where the customer decides.
    ///
    /// What every buy-now-pay-later flow is. The customer is scored, shown
    /// instalment options and asked for a one-time code on the provider's own
    /// site, and this system never sees any of it.
    Hosted,
}

/// Where a customer is sent when the provider is done with them.
///
/// Three, because the providers that redirect distinguish three endings and
/// collapsing them would lose the difference between "they changed their mind"
/// and "they were declined". A gateway that takes only one — Moyasar — is given
/// [`Returns::success`], because its own answer carries the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Returns {
    pub success: String,
    /// The customer backed out.
    pub cancel: String,
    /// The provider said no. **A normal outcome for buy-now-pay-later**, not an
    /// error: the customer was scored and declined, and the shop should offer
    /// them a card.
    pub failure: String,
}

/// Who is buying.
///
/// Required by both buy-now-pay-later providers, which score the person before
/// they will lend to them, and unused by card gateways — a card is its own
/// credit decision, made by somebody else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Buyer {
    pub name: String,
    pub email: String,
    /// The identity a lender actually scores on, and where the one-time code
    /// goes.
    pub phone: String,
}

/// What is being bought.
///
/// Also buy-now-pay-later's, and for the same reason: the provider is buying
/// the receivable and wants to know what it is against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Basket {
    /// This system's own order reference, echoed back on every callback.
    pub reference: String,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub title: String,
    pub quantity: u32,
    pub unit_price: Money,
}

/// What a caller is asking the gateway to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Charge {
    /// **The idempotency key**, and this system's own id for the payment.
    ///
    /// Passed to whichever field the provider uses for it, so a request this
    /// process believes failed but which actually succeeded does not charge a
    /// customer twice (L8). A provider with no such field is a provider where
    /// this is only a reference, and the crate says so where that is true.
    pub reference: String,
    pub amount: Money,
    /// Where the customer's browser is sent when the provider is done.
    ///
    /// **Not evidence of anything.** The query parameters a gateway appends to
    /// these are attacker-controllable — the customer's own browser follows the
    /// redirect — so they are where somebody lands, not how this system learns
    /// what happened.
    pub returns: Returns,
    pub source: Source,
    /// Shown to the merchant, never to the payer.
    pub description: String,
    /// **Required by buy-now-pay-later, ignored by cards.** An adapter that
    /// needs one and is not given it refuses, rather than sending a request the
    /// provider will reject for a reason nobody here can read.
    pub buyer: Option<Buyer>,
    /// The same. See [`Basket`].
    pub basket: Option<Basket>,
}

/// Where a payment has got to.
///
/// The union of what the supported gateways report, named for what each one
/// means to a business rather than for the provider's own word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Created, and **the customer still has to do something** — a 3-D Secure
    /// challenge at [`Charged::challenge`].
    Initiated,
    /// A hold is on the card and nobody has been charged. Capture takes it;
    /// void releases it.
    Authorized,
    /// **The money has moved.** A one-step purchase, or a capture that landed.
    Paid,
    /// Refused, and retrying the same request refuses again.
    Failed,
    /// Given back, in full or in part. [`Charged::refunded`] says how much.
    Refunded,
    /// Cancelled before it settled.
    Voided,
}

impl Status {
    /// Whether this is an answer or a waiting room.
    ///
    /// `Initiated` and `Authorized` both mean somebody has to do something
    /// next, and a system that treated either as an ending would either lose a
    /// payment or claim one that never happened.
    #[must_use]
    pub const fn is_settled(self) -> bool {
        matches!(
            self,
            Self::Paid | Self::Failed | Self::Refunded | Self::Voided
        )
    }

    /// Whether the business has the money.
    #[must_use]
    pub const fn took_the_money(self) -> bool {
        matches!(self, Self::Paid)
    }
}

/// What the gateway said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Charged {
    /// The gateway's own id. What every later call names, and what a callback
    /// carries.
    pub id: String,
    pub status: Status,
    /// **What the gateway says was charged**, not what was asked for. The two
    /// are compared before anything is recorded — see [`Charged::matches`].
    pub amount: Money,
    /// What has been given back so far, in the same currency.
    pub refunded: Money,
    /// The gateway's cut, when it says. `None` is common: most gateways report
    /// a fee only once the payment has settled, and some only on the payout.
    ///
    /// **An expense, never a smaller revenue.** A tenant that nets it cannot
    /// answer what it actually sold.
    pub fee: Option<Money>,
    /// Where to send the customer, while [`Status::Initiated`].
    pub challenge: Option<String>,
    /// Why it failed, in the gateway's words, for a human to read.
    pub message: Option<String>,
}

impl Charged {
    /// **Whether this is the payment that was asked for.**
    ///
    /// A callback names an id and nothing more; this is what stands between
    /// "the gateway says payment X is paid" and "the customer paid this
    /// invoice". Both the amount and the currency, because a gateway that
    /// reported 100 SAR against an expectation of 100 USD would otherwise look
    /// like a match.
    #[must_use]
    pub fn matches(&self, expected: Money) -> bool {
        self.amount == expected
    }
}

/// Why a gateway call did not produce an answer.
///
/// The distinction that matters is the same one the message transports make:
/// what is worth another go, and what will refuse identically for ever.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GatewayError {
    /// Unreachable, slow, or a `5xx`. Worth another attempt.
    #[error("the gateway could not be reached: {0}")]
    Unreachable(String),
    /// The gateway refused, and will refuse again. A declined card, a bad
    /// token, an amount below the floor.
    #[error("{0}")]
    Refused(String),
    /// The credentials are wrong or missing. **Not a customer's problem**, and
    /// it is separate because it is the one that should page somebody.
    #[error("the gateway did not accept this account's credentials")]
    Unauthenticated,
    /// No such payment at the gateway.
    #[error("the gateway has no payment {0}")]
    NoSuchPayment(String),
    /// The gateway answered something this build cannot read. Not a success it
    /// can claim, and not obviously a refusal either.
    #[error("the gateway answered with something this client cannot read: {0}")]
    Unreadable(String),
}

impl Localize for GatewayError {
    fn message(&self) -> Message {
        match self {
            Self::Unreachable(_) => Message::new(messages::UNREACHABLE),
            Self::Refused(why) => {
                Message::new(messages::REFUSED).with("reason", MessageArg::text(why))
            }
            Self::Unauthenticated => Message::new(messages::UNAUTHENTICATED),
            Self::NoSuchPayment(id) => {
                Message::new(messages::NO_SUCH_PAYMENT).with("id", MessageArg::text(id))
            }
            Self::Unreadable(_) => Message::new(messages::UNREADABLE),
        }
    }
}

/// Somewhere money can be asked for.
#[async_trait::async_trait]
pub trait Gateway: Send + Sync + std::fmt::Debug {
    /// What this gateway is called. Recorded on the payment, so a tenant that
    /// changes provider can still find what the old one holds.
    fn provider(&self) -> &'static str;

    /// Starts a payment. **Succeeding is not being paid** — read the status.
    async fn charge(&self, charge: &Charge) -> Result<Charged, GatewayError>;

    /// What the gateway says about a payment, right now.
    ///
    /// **The only thing worth believing.** A callback says which payment to
    /// look at; this is what says what happened.
    async fn fetch(&self, id: &str) -> Result<Charged, GatewayError>;

    /// Takes an authorized hold, in full or in part.
    async fn capture(&self, id: &str, amount: Option<Money>) -> Result<Charged, GatewayError>;

    /// Gives money back, in full or in part.
    async fn refund(&self, id: &str, amount: Option<Money>) -> Result<Charged, GatewayError>;

    /// Cancels before settlement. Cheaper than a refund where it is allowed,
    /// and allowed for a much shorter time.
    async fn void(&self, id: &str) -> Result<Charged, GatewayError>;
}

/// **Whether a callback is worth acting on**, and nothing more.
///
/// Returns the gateway's **payment id**, to go and ask [`Gateway::fetch`]
/// about. It does not return the payload, and that is the whole point: no
/// gateway here signs its bodies, so the body proves nothing about the amount.
/// See the crate docs.
///
/// A free function rather than a method on [`Gateway`] because the route that
/// receives a callback is **public** and holds no credentials — it has the
/// tenant's shared secret and nothing else. Requiring a configured client to
/// decide whether a request is authentic would mean unsealing an API key to
/// answer a question that does not need one.
pub fn authenticate(
    provider: &str,
    secret: &[u8],
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<String, CallbackError> {
    match provider {
        "moyasar" => moyasar::authenticate(secret, body),
        "tabby" => tabby::authenticate(secret, headers, body),
        "tamara" => tamara::authenticate(secret, headers, body),
        other => Err(CallbackError::UnknownProvider(other.to_owned())),
    }
}

/// The header a provider that authenticates with one is asked to send.
///
/// **This system chooses it**, because both providers that work this way let
/// the merchant name the header at registration and neither fixes one. Naming
/// it in a constant means the value a tenant is told to configure and the value
/// this code looks for cannot drift apart.
pub const SECRET_HEADER: &str = "x-erp-webhook-secret";

/// Finds a header, case-insensitively.
///
/// Callers hand over what arrived; HTTP header names are not case sensitive and
/// this crate does not get to assume its caller normalised them.
#[must_use]
pub fn header<'a>(headers: &[(&str, &'a str)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| *value)
}

/// Every provider this crate can authenticate a callback from.
///
/// What `authenticate` matches on, in one place, so the guard test and the
/// dispatch cannot disagree.
pub const PROVIDERS: &[&str] = &["moyasar", "tabby", "tamara"];

/// Why a callback was not believed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallbackError {
    #[error("the callback did not authenticate")]
    NotAuthentic,
    #[error("the callback body is not something this provider sends: {0}")]
    Unreadable(String),
    /// Not a gateway this crate knows. The caller decides what that means —
    /// `erp_api` falls back to this system's own signed-webhook contract.
    #[error("{0} is not a payment provider this system integrates")]
    UnknownProvider(String),
}

/// Constant-time equality, for anything that is a secret.
///
/// A `==` on two secrets leaks their common prefix through timing. It is a
/// small leak and this is a small function, and there is no version of this
/// system where the trade is worth making the other way.
///
/// Bytes rather than `&str` because that is what a sealed secret is when it
/// comes back out of the vault, and decoding it to compare it would be a
/// conversion that can fail on a value nobody chose.
#[must_use]
pub fn secrets_match(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b)
        .fold(0u8, |differences, (x, y)| differences | (x ^ y))
        == 0
}

#[cfg(test)]
mod fake;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_ending_is_an_ending() {
        assert!(Status::Paid.is_settled());
        assert!(Status::Failed.is_settled());
        assert!(Status::Refunded.is_settled());
        assert!(Status::Voided.is_settled());

        // Both mean somebody has to do something next.
        assert!(!Status::Initiated.is_settled());
        assert!(!Status::Authorized.is_settled());

        // And only one of them is money in the bank.
        assert!(Status::Paid.took_the_money());
        assert!(!Status::Authorized.took_the_money());
    }

    #[test]
    fn a_header_is_found_however_it_was_capitalised() {
        let headers = [("X-Erp-Webhook-Secret", "shhh"), ("content-type", "json")];
        assert_eq!(header(&headers, SECRET_HEADER), Some("shhh"));
        assert_eq!(header(&headers, "CONTENT-TYPE"), Some("json"));
        assert_eq!(header(&headers, "authorization"), None);
    }

    #[test]
    fn a_secret_is_compared_without_leaking_its_prefix() {
        assert!(secrets_match(b"abc", b"abc"));
        assert!(!secrets_match(b"abc", b"abd"));
        assert!(!secrets_match(b"abc", b"abcd"));
        assert!(!secrets_match(b"", b"a"));
        assert!(secrets_match(b"", b""));
    }

    /// Every provider the dispatcher names can actually be dispatched to, and
    /// the reverse. A provider in one list and not the other is a callback
    /// silently falling through to the wrong verification.
    #[test]
    fn every_provider_this_crate_lists_can_authenticate_a_callback() {
        for provider in PROVIDERS {
            assert_ne!(
                authenticate(provider, b"s", &[], b"{}"),
                Err(CallbackError::UnknownProvider((*provider).to_owned())),
                "{provider} is listed and not dispatched"
            );
        }
        assert_eq!(
            authenticate("stripe", b"s", &[], b"{}"),
            Err(CallbackError::UnknownProvider("stripe".to_owned()))
        );
    }

    /// **The check that stands between a callback and the books.** A gateway
    /// reporting a different amount, or the same number in another currency,
    /// is not the payment that was expected.
    #[test]
    fn a_payment_for_a_different_amount_is_not_the_expected_one() {
        let sar = |minor| Money::from_minor(minor, "SAR".parse().expect("a currency"));
        let usd = |minor| Money::from_minor(minor, "USD".parse().expect("a currency"));

        let charged = Charged {
            id: "pay_1".to_owned(),
            status: Status::Paid,
            amount: sar(10_000),
            refunded: sar(0),
            fee: None,
            challenge: None,
            message: None,
        };

        assert!(charged.matches(sar(10_000)));
        assert!(!charged.matches(sar(9_999)));
        assert!(!charged.matches(usd(10_000)), "same number, other currency");
    }
}
