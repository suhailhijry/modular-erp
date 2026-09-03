//! Reaching somebody.
//!
//! # What Phases 7–10 assumed and did not build
//!
//! They describe a domain and no way to reach anybody in it. The system had
//! exactly **one** effect kind — `email.send`, enqueued by the control plane
//! for invitations — which was the entire outbound surface. For a product sold
//! in this market the channel is not plumbing: a reminder that does not arrive
//! is a chair that stays empty.
//!
//! # What this module is, in one sentence
//!
//! A tenant writes a template that says *what* and *to whom*; something in the
//! system says *about which one*; and this resolves both against the read model
//! at the moment the message goes.
//!
//! ```text
//! messaging::send(&mut tx, &Sending {
//!     template: "booking.reminder".to_owned(),
//!     subject: Subject::new(Topic::Reservation, booking),
//!     key: format!("booking.reminder.{booking}"),
//!     extra: BTreeMap::from([("link".to_owned(), url)]),
//!     ..
//! }).await?;
//! ```
//!
//! The caller supplies a subject and a key. It does not know the customer's
//! name, their number, what language they read, or what the message says.
//!
//! # The four corrections this makes to the system it was read against
//!
//! **A template names an audience, not an address.** That system freezes a
//! phone number into the thing that will be sent, so a customer who changes
//! their number keeps getting messages at the old one. Here the address is a
//! query, run minutes before the send.
//!
//! **A template fetches its own data.** That system has two template mechanisms
//! that do not meet — a database aggregate whose parameters the caller fills in
//! by hand, and hardcoded classes with the copy and the business name compiled
//! in, where changing a reminder's wording is a deploy. Both have one cause: a
//! template cannot ask for anything, so somebody must hand it everything.
//!
//! **Bindings are declared, so they fail when the template is saved.** Not when
//! a customer is waiting for a message with a hole in it.
//!
//! **Segments are counted, and a budget refuses (L6).** SMS is billed per
//! segment — 160 characters, or **70 in Arabic**, which here means every
//! message — so a 200-character reminder costs three times what its author
//! expected. The count is part of sending and the meter is checked inside the
//! transaction that adds to it.
//!
//! # What is deliberately not here
//!
//! **A provider adapter.** See [`transport`]: five providers, five sets of
//! credentials, and an account with none of them. A client that has never made
//! a successful call is a file that looks finished and is not. What ships is a
//! documented outbound contract an operator points at their own service, which
//! is the same choice the email handler makes in preferring SMTP to one
//! vendor's JSON.
//!
//! **Delivery receipts.** "Sent" and "delivered" are different words and should
//! stay that way, but a receipt arrives as an *inbound* callback and this system
//! has no verified inbound surface yet. That is Phase 12, and doing it before
//! the signature verification exists would be accepting somebody else's word
//! about what happened to a message.
//!
//! **A per-customer language.** The tenant's own language is one setting; a
//! preference per customer is a `crm` field nobody has asked for. A Saudi salon
//! writes Arabic to everybody.
//!
//! # No projections, no schema
//!
//! Like `hr_sa`, this module owns no read models. Templates, settings and
//! budgets are tenant **configuration** — typed per key, versioned, named in
//! every event's `config_version` — and the meter and the device tokens are
//! write-side state in the tenant migration chain, because neither is derivable
//! from the log and a rebuild must not destroy them.

pub mod audience;
pub mod bindings;
pub mod budget;
pub mod channel;
pub mod fcm;
pub mod http;
pub mod messages;
pub mod push;
pub mod send;
pub mod settings;
pub mod taqnyat;
pub mod template;
pub mod transport;

/// A gateway that is not a gateway. Tests only.
#[cfg(test)]
mod fake;

pub use audience::{Address, Audience, Subject, Topic};
pub use budget::{Budget, OverBudget, SpendError, Spent};
pub use channel::{Channel, UnknownChannel, segments};
pub use fcm::Fcm;
pub use push::{Device, Platform, Registered};
pub use send::{Outbound, SendError, Sending, Sent, send};
pub use settings::Settings;
pub use taqnyat::Taqnyat;
pub use template::{Body, Template, TemplateError, Templates};
pub use transport::{MessageHandler, Relay, Transport, TransportError, handlers};

use erp_i18n::StaticCatalog;

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

/// Nothing to install.
///
/// **No projection group and no schema.** Everything this module keeps is
/// either tenant configuration or write-side state in the migration chain — see
/// the crate docs — so there is nothing for a rebuild to drop and nothing for
/// provisioning to create.
#[expect(
    clippy::unused_async,
    reason = "`ModuleSetup` and every caller take an async installer; a module \
              with nothing to install is not a reason to fork that signature"
)]
pub async fn install(_conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    Ok(())
}

/// What a tenant enabling this module needs installed.
///
/// **`crm`**, and nothing else. Every audience but *the client* is optional —
/// a business with no staff records simply has no template addressed to a
/// worker — but a message to a customer needs somewhere to find the customer,
/// and a messaging module that cannot reach a customer is a module with no
/// reason to be switched on.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(module_id(), "", &[], upcasters).requiring(&["crm"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("messaging")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
///
/// **None.** This module declares no events: what it does is promise effects,
/// and an effect is not an event. Nothing it holds needs replaying, which is
/// why it has no projections either.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(erp_eventlog::Upcasters::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audience::Topic;

    /// **The vocabulary and the resolver cannot drift.**
    ///
    /// `template::vocabulary` is what a template may say and `bindings::of` is
    /// what gets answered, and they are two lists in two files. A name in the
    /// first and not the second is a binding that passes validation and renders
    /// as braces in front of a customer — which is exactly the failure the
    /// declaration exists to prevent.
    ///
    /// Checked by source rather than by running: resolving needs a database and
    /// four modules' read models, and the property here is that the *names*
    /// agree.
    #[test]
    fn every_binding_in_the_vocabulary_can_be_resolved() {
        let resolver = include_str!("bindings.rs");
        let mut missing = Vec::new();

        for topic in Topic::ALL {
            for binding in template::vocabulary(topic) {
                // `business` and `link` are supplied by the sender, not read —
                // see `send::wording`, which is where they are inserted.
                if matches!(binding, "business" | "link") {
                    continue;
                }
                if !resolver.contains(&format!("\"{binding}\"")) {
                    missing.push(format!("{}: {binding}", topic.as_str()));
                }
            }
        }

        assert!(
            missing.is_empty(),
            "a binding a template may use is not answered by `bindings::of`, so it \
             would render as braces in front of a customer:\n  {}",
            missing.join("\n  ")
        );

        // Not vacuous: something that is not a binding must not be found.
        assert!(!resolver.contains("\"reservation.not_a_real_binding\""));
    }

    /// This module promises effects on every channel, and each one is a kind a
    /// worker registers a handler for. A channel with no kind is one whose
    /// messages nothing would ever claim.
    #[test]
    fn every_channel_is_a_kind_a_handler_can_claim() {
        for channel in Channel::ALL {
            let kind = channel.kind();
            let name = kind.as_str();
            assert!(
                name.rsplit_once('.')
                    .is_some_and(|(_, verb)| verb == "send"),
                "{name} does not read as an effect"
            );
        }
    }
}
