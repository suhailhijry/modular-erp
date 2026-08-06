//! The projection runtime.
//!
//! Three of the architecture's laws are implemented here, and each is enforced
//! by a different mechanism rather than by review:
//!
//! | law | mechanism |
//! |---|---|
//! | **L2** projections are pure functions of the event stream | [`ProjectionCtx`] holds no clock, no RNG, no pool — there is nothing in scope to be impure with |
//! | **L3** groups are the unit of consistency and never read each other | each group owns a Postgres schema; the runner sets `search_path` to it, so reaching outside fails at runtime |
//! | **L4** checkpoints advance with their effects | both happen in one transaction, whose row lock is also the lease against a second worker |
//!
//! And because none of those catches everything, [`replay_shadow`] rebuilds a
//! group from the log and diffs it against the live tables. That is what turns
//! "replay is reproducible" from a claim into something CI checks — and the
//! differ is itself tested against projections that are deliberately
//! non-deterministic, so an empty diff means what it says.

mod group;
mod runner;
mod shadow;

pub use group::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
pub use runner::{Progress, RunError, checkpoint, ensure_group_schema, run_once, run_to_head};
pub use shadow::{ShadowReport, TableDiff, replay_shadow};
