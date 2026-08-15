//! Double-entry accounting.
//!
//! The first module, and the proof that the module seam works — it is built as a
//! module rather than extracted from the kernel later (architecture D11), so a
//! tenant that does not want accounting simply does not enable it.
//!
//! # The one invariant
//!
//! **Debits equal credits, per currency.** It is enforced twice, on purpose:
//!
//! - At the type level, by [`BalancedLines`]. An unbalanced entry cannot be
//!   constructed, cannot be stored, and cannot be decoded back out of storage —
//!   so posting code has nothing to remember.
//! - At the data level, by [`trial_balance`]. That query can only be non-zero if
//!   the *pipeline* is broken, which makes it a canary for a whole class of bug
//!   rather than a check on the posting rules.
//!
//! # What is deliberately absent
//!
//! Fiscal periods, drafts, reversals-as-a-command, multi-currency entries with
//! FX, and posting rules driven by configuration. Each is real, and each needs
//! someone to want it before its shape is decided.

mod account;
mod charts;
mod commands;
mod entry;
mod lines;
pub mod messages;
mod projections;

pub use account::{Account, AccountEvent, AccountKind};
pub use charts::{CHARTS, Chart, Installed, TemplateAccount, chart};
pub use commands::{
    LedgerError, accepts_postings, close_account, install_chart, open_account, post_entry,
    post_entry_in, rename_account, reverse_entry, reverse_in,
};
pub use entry::{JournalEntry, JournalEntryEvent};
pub use lines::{BalancedLines, Line, Unbalanced};
pub use projections::{
    AccountBalance, Accounts, Ledger, Postings, TrialBalance, account_balances, imbalances,
    projections, trial_balance,
};

use spa_i18n::StaticCatalog;
use spa_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

/// Creates this module's read models in a tenant database.
///
/// Idempotent, and deliberately not a numbered migration chain — everything it
/// creates is derived from the event log, so a change drops and rebuilds rather
/// than migrating. See `schema/install.sql`.
///
/// Called when a tenant enables the module. Pair it with
/// `spa_projection::ensure_group_schema::<Ledger>` so the checkpoint exists too.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(include_str!("../schema/install.sql"))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Ledger as spa_projection::ProjectionGroup>::NAME;

/// This module's projection groups, as `(name, schema)`.
const GROUPS: &[(&str, &str)] = &[(
    <Ledger as spa_projection::ProjectionGroup>::NAME,
    <Ledger as spa_projection::ProjectionGroup>::SCHEMA,
)];

/// What a tenant enabling this module needs installed.
///
/// Described rather than performed: the control plane runs it during
/// provisioning, and neither crate has to know what the other is for.
#[must_use]
pub fn setup() -> spa_control::ModuleSetup {
    spa_control::ModuleSetup::new(module_id(), include_str!("../schema/install.sql"), GROUPS)
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> spa_types::ModuleId {
    spa_types::ModuleId::new("ledger")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
///
/// One entry per name at version 1. When a payload changes, the new version is
/// declared here with a step function and the old events keep decoding — see
/// `spa_eventlog::Upcasters`.
#[must_use]
pub fn upcasters() -> &'static spa_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<spa_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        AccountEvent::NAMES
            .iter()
            .chain(JournalEntryEvent::NAMES.iter())
            .fold(spa_eventlog::Upcasters::new(), |u, n| {
                u.declare(&name(n), VERSION_1)
            })
    })
}

/// A `&'static str` from this crate, as an [`EventName`].
///
/// Panics only if a literal in this crate breaks `EventName`'s character set,
/// which is a build-time bug with no runtime recovery — and `names_are_valid`
/// in `tests/ledger.rs` catches it before anything ships.
#[expect(
    clippy::expect_used,
    reason = "a malformed literal is a build bug, not a runtime condition"
)]
pub(crate) fn name(literal: &'static str) -> EventName {
    EventName::new(literal).expect("event names in this crate are valid literals")
}

#[expect(
    clippy::expect_used,
    reason = "a malformed literal is a build bug, not a runtime condition"
)]
pub(crate) fn domain(literal: &'static str) -> DomainName {
    DomainName::new(literal).expect("domain names in this crate are valid literals")
}
