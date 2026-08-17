//! What a module's HTTP surface is built from.
//!
//! # Why this is a crate of its own
//!
//! A module ships its own routes — `sales::http::routes()` is a router, and the
//! composition root mounts it. For that to be possible the furniture those
//! routes are made of has to live *below* the modules: an extractor a module
//! cannot name is one it cannot use, and a module that reached up into `spa-api`
//! to get one would close a dependency cycle, because `spa-api` names every
//! module.
//!
//! So this crate holds what every route needs and no route decides:
//! authorization extractors, the problem+json shape, the JSON and query
//! extractors that refuse in this API's shape, paging, and the request-level
//! messages. `spa-api` is what is left — the core's own routes, the module
//! catalogue, and the one router that mounts everything.
//!
//! Nothing here knows what a ledger or an invoice is, and nothing here may. The
//! moment it does, this is a module.
//!
//! # The catalog
//!
//! [`CATALOG`] renders **this crate's messages, the control plane's and the
//! event log's** — everything an [`ApiError`] can carry, which is what makes
//! `ApiError::into_problem` a complete function rather than one that degrades
//! to a bare code for half its inputs.
//!
//! A module's *own* failures are not in it, and cannot be: they are declared
//! above. A module renders those through a composite of its own catalog and this
//! one — see any module's `http::problem_for`. `spa_api::CATALOG` is the union
//! of all of them, and is what `docs/ERRORS.md` is generated from.

mod consistency;
mod error;
mod extract;
pub mod messages;
mod problem;
mod state;
mod wire;

use spa_i18n::{Composite, StaticCatalog};

pub use consistency::{Consistency, nudge};
pub use error::ApiError;
pub use extract::{
    Allowed, Authenticated, Capability, Language, ManageAccounts, ManageTenant, PostEntries, Read,
    Tenant,
};
pub use problem::Problem;
pub use state::AppState;
pub use wire::{
    After, Amount, Json, Paged, Query, bad_request, metadata, parse_id, require_module,
};

/// This crate's own messages — about the request, not the domain.
pub static REQUEST_CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

/// Everything a failure that reaches HTTP without passing through a module can
/// say. See the crate docs.
pub static CATALOG: Composite = Composite::new(&[
    &REQUEST_CATALOG,
    &spa_control::CATALOG,
    &spa_eventlog::CATALOG,
]);

#[cfg(test)]
mod tests {
    /// **What `ApiError::into_problem` promises.**
    ///
    /// It renders through [`super::CATALOG`] with no state and no module in
    /// reach, so every code it can produce has to be translatable from here. A
    /// code that is not degrades to the bare identifier in a user's face — the
    /// deliberate exception to law L6 — and this is the test that keeps that
    /// exception theoretical.
    #[test]
    fn every_failure_this_crate_can_answer_with_is_translated() {
        spa_i18n::testing::assert_complete(&super::CATALOG);
    }
}
