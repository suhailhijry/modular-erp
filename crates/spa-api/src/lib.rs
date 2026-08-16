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
//!   for. `docs/ERRORS.md` lists every one.
//! - `Accept-Language` is honoured on every response, including failures.
//! - The `OpenAPI` document at `GET /v1/openapi.json` — also `docs/openapi.json` —
//!   describes every route, and is generated from the router that serves them
//!   rather than written alongside it. See [`routes`].
//!
//! # What is not here yet
//!
//! `Idempotency-Key`, `ETag`/`If-Match`, and cursors. Writes are already
//! idempotent on a client-chosen id, which is most of what the first buys; the
//! other two need a list long enough and a conflict real enough to shape them.

mod catalog;
mod consistency;
mod error;
mod extract;
mod invitations;
mod ledger_routes;
mod members;
pub mod messages;
mod modules;
mod problem;
mod purchases_routes;
mod routes;
mod sales_routes;
mod signup;
mod state;
mod tax_sa_routes;
mod wire;

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
pub use modules::available as modules;
pub use problem::Problem;
pub use routes::{openapi, router};
pub use state::AppState;
