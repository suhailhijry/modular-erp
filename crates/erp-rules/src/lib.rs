//! **One artifact for every rule a tenant configures.**
//!
//! # What this is for
//!
//! A tenant changes a business rule without a deploy, and the system can say
//! why it did what it did. The codebase names the rules it wants and cannot
//! express — *"a bookkeeper may post entries under ten thousand riyals, or only
//! to their own branch"* (`erp_tenant::roles`) — and one it can: *"Thursday
//! evenings are 25% dearer"*, which `booking` has had all along.
//!
//! That last one is this crate in miniature, for one consumer:
//!
//! ```text
//! Band { name, when: Availability, uplift }
//!        ↑ identity ↑ condition     ↑ consequence
//! ```
//!
//! The payoff is not any one rule. It is that the next kind costs a type
//! parameter rather than a subsystem: `Rule<Uplift>` is pricing, `Rule<bool>`
//! is a permission, `Rule<ApprovalChain>` is routing — one evaluator, one
//! validator, one `explain`.
//!
//! # What is deliberately not here
//!
//! Authorization on the engine, the four authoring levels with `origin`
//! round-tripping, and rule packs as blueprints. Each is a later phase with its
//! own consumer; see `docs/superpowers/specs/2026-09-10-rules-engine-design.md`
//! for what each is waiting on.

pub mod condition;
pub mod fact;
pub mod rule;

pub use condition::{DynCondition, Invalid, Op};
pub use fact::{FactName, FactRegistry, Facts, Kind, Value};
pub use rule::{Considered, Explained, Rule, Rules};
