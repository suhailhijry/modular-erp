//! Core value types for the SPA ERP backend.
//!
//! No I/O, no async, no database access. The crate compiles to WASM and is
//! intended to be shared with a Rust frontend, so that the API contract is
//! checked by the compiler rather than by a code generator.
//!
//! # What lives here
//!
//! - **Identifiers** ([`TenantId`], [`IdentityId`], [`StreamId`], …) — newtypes
//!   with no conversions between them.
//! - **Positions** ([`LogPosition`], [`Sequence`]) — two different meanings of
//!   "a number that goes up", kept distinct on purpose.
//! - **[`Money`]** — integer minor units plus a runtime currency, with no
//!   arithmetic operators.
//! - **[`NonEmpty`]** — a list whose emptiness is not a case anyone has to
//!   handle.
//!
//! # The principle
//!
//! Every type here has a private field and a fallible constructor, and validates
//! on `Deserialize` as well as on construction. A guarantee that holds only for
//! values built in Rust is not a guarantee in a system whose state arrives from
//! an append-only log written by older versions of itself.

mod error;
mod ids;
mod macros;
mod money;
mod non_empty;

pub use error::{
    Empty, IdParseError, InvalidCurrency, InvalidString, InvalidStringReason, MoneyError,
    NegativeCounter,
};
pub use ids::{
    AggregateId, DomainName, EventName, IdentityId, LogPosition, MembershipId, ModuleId, ProfileId,
    SchemaVersion, Sequence, StreamId, TenantId,
};
pub use money::{CurrencyCode, Money};
pub use non_empty::NonEmpty;

/// The timestamp type used throughout. Always UTC — local time is a display
/// concern, applied at query time, never stored.
pub type Timestamp = chrono::DateTime<chrono::Utc>;
