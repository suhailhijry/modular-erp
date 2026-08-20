//! The outbox: effects as values (D9).
//!
//! A command decides *what should happen*; it never makes it happen. The
//! decision is written to the outbox in the same transaction as the events that
//! justify it, and a [`Dispatcher`] delivers it afterwards.
//!
//! ```text
//!   command transaction                    later, separately
//!   ┌──────────────────────────┐           ┌────────────────────────┐
//!   │ append events            │           │ claim (lease)          │
//!   │ enqueue effects          │  commit   │ deliver  ← the only    │
//!   └──────────────────────────┘  ───────► │ settle     I/O anywhere│
//!         both or neither                  └────────────────────────┘
//! ```
//!
//! # What this buys
//!
//! - **No effect without its cause.** A rolled-back command sends nothing,
//!   because the promise rolled back with it.
//! - **No cause without its effect.** A crash after commit loses nothing: the
//!   promise is durable and the dispatcher finds it on restart.
//! - **Rebuilds are safe.** Effects are written by commands, not derived by
//!   projections, so rebuilding a read model re-sends nothing. That is what
//!   makes `replay_shadow` something you can run in production.
//! - **Testable domains.** A command handler returns values, so asserting "this
//!   would have emailed the customer" needs no mock and no network.

mod dispatch;
mod effect;

pub use dispatch::{
    DeliveryError, DispatchError, Dispatched, Dispatcher, EffectHandler, RetryPolicy, Settlement,
};
pub use effect::{Effect, EnqueueError, OutboxHealth, PendingEffect, enqueue, outbox_health};
