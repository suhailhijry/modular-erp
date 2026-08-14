//! The HTTP surface.
//!
//! # What a handler cannot do
//!
//! Reach the wrong tenant. A handler that touches tenant data takes
//! [`extract::Tenant`], whose only constructor is `ControlPlane::enter` — so
//! "did we check the membership?" is answered by the signature rather than by
//! reading the body.
//!
//! # What a client can rely on
//!
//! - Every error is `application/problem+json` with a stable `code`. Branch on
//!   the code, never on `detail`, which is prose in whatever language was asked
//!   for.
//! - `Accept-Language` is honoured on every response, including failures.
//!
//! # What is not here yet
//!
//! Fine-grained authorization, `Idempotency-Key`, `ETag`/`If-Match`, cursors and
//! an `OpenAPI` document. Each needs a real mutation to attach to, and the first arrives
//! with the ledger module — building them now would be guessing at their shape.

mod catalog;
mod consistency;
mod error;
mod extract;
mod ledger_routes;
mod members;
pub mod messages;
mod problem;
mod routes;
mod signup;
mod state;

pub use catalog::CATALOG;

use spa_i18n::StaticCatalog;

/// This crate's own messages — about the request, not the domain.
pub static REQUEST_CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);
pub use consistency::Consistency;
pub use error::ApiError;
pub use extract::{
    Allowed, Authenticated, Capability, Language, ManageAccounts, ManageTenant, PostEntries, Read,
    Tenant,
};
pub use problem::Problem;
pub use routes::router;
pub use state::AppState;
