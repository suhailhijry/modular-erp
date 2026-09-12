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
//! An `explain`-backed dry run — a later phase with its own consumer; see
//! `docs/superpowers/specs/2026-09-10-rules-engine-design.md` for what it is
//! waiting on.
//!
//! **Rule packs are not here either, and that is where they belong.** A pack is
//! a list of the same form submissions a tenant makes, against one consumer's
//! own configuration shape — `booking::PACKS` — so nothing about it is generic
//! until a second consumer has both templates and a screen. `erp_tenant::Limits`
//! is not that yet: `PUT /v1/tenant/permission-limits` writes it, but only as
//! rules written out, with no templates.

pub mod authoring;
pub mod condition;
pub mod fact;
pub mod rule;

pub use authoring::{Answers, Authored, Field, Template, Unfillable};
pub use condition::{DynCondition, Invalid, Op};
pub use fact::{FactName, FactRegistry, Facts, Kind, Value};
pub use rule::{Considered, Explained, Rule, Rules};
