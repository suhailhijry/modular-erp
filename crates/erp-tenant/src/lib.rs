//! Everything a module needs, and nothing a module must not have.
//!
//! # Why this crate exists
//!
//! D15 forecloses it plainly: *the tenant runtime may not contain fleet
//! management, cluster placement, or any other tenant's credentials — a binary
//! that ships to a customer's own cloud cannot carry the map of everybody
//! else's.*
//!
//! Before this crate that was false. Every module depended on `erp-control`,
//! which exports `ClusterRegistry`, `FleetPlan`, `PlacementPolicy`,
//! `TenantPools` and `WorkSchedule` — so a tenant binary linked the whole
//! vocabulary of the fleet. It was false for a small reason: modules used six
//! symbols from that crate and got the rest for free.
//!
//! Those six live here. `erp-control` depends on this and re-exports them, so
//! the control plane is unchanged; modules depend on this alone, and
//! `tests/boundary.rs` in each module's home is what keeps that true.

pub mod budget;
pub mod db;
pub mod messages;
pub mod modules;
pub mod roles;

pub use budget::{Budget, Conn, Lane, PoolError, Tx};
pub use db::{CommandError, TenantDb};
pub use modules::{EnabledModules, ModuleSetup};
pub use roles::{Access, Capability, Role, UnknownRole};

/// This crate's own codes and their translations, composed at the edge with the
/// control plane's and each module's.
pub static CATALOG: erp_i18n::StaticCatalog =
    erp_i18n::StaticCatalog::new(messages::ENTRIES, messages::CODES);
