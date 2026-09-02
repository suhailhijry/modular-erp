//! What the Kingdom requires of an employer.
//!
//! # Why this is a country module, mirroring `tax_sa`
//!
//! For the same reason VAT is not in `sales`: a country's rules are that
//! country's, and a payroll module that knew about GOSI would have to learn
//! every other country's equivalent. `payroll` computes what a business pays
//! its people; this computes what the Kingdom then requires of it.
//!
//! # The rates are configuration, and that is the load-bearing decision
//!
//! GOSI's schedule is set by the authority and has changed — most recently for
//! people entering after the 2024 pension reform, who are on a different and
//! rising scale from those already in. A build that hard-coded a percentage
//! would be quietly wrong for some employees from the day it shipped.
//!
//! So the rates are a tenant configuration value with shipped defaults, exactly
//! as `tax_sa` treats the VAT rate as seeded data. **The defaults must be
//! checked against the authority's current schedule before a tenant runs
//! payroll against them.** The API returns them so somebody can see what they
//! are rather than discovering them on a payslip.
//!
//! # What is here, and what is not
//!
//! **GOSI** and **end of service** are here: both are arithmetic over figures a
//! caller supplies, both are testable to the halala, and both are what a
//! business gets asked about.
//!
//! **WPS is not.** The monthly salary file the Ministry mandates has a
//! specification — field order, encoding, the bank's own variations — that this
//! build cannot verify from where it stands, and a file that is *almost* right
//! is a file the bank rejects on the day wages are due. It is the same position
//! `tax_sa` was in before somebody had a sandbox to submit against, and it is
//! recorded as not built rather than guessed at. See `docs/IMPLEMENTATION.md`.
//!
//! # No aggregate, no projection, no schema
//!
//! This module holds **no state**. Every function here is arithmetic over what
//! it is given, and the one thing it stores — the GOSI schedule — is a
//! configuration value in the shared store. That is why it has no `install`
//! beyond the empty one and no projection group: there is nothing to project.

pub mod gosi;
pub mod gratuity;
pub mod http;
pub mod messages;

pub use gosi::{Contribution, Footing, Schedule, contribution};
pub use gratuity::{Award, Leaving, end_of_service};

use erp_i18n::StaticCatalog;

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

/// Creates this module's read models in a tenant database.
///
/// **There are none.** The module is arithmetic and one configuration key, so
/// this is here to satisfy the shape every module has rather than because there
/// is anything to create — and saying so is better than an empty SQL file
/// somebody later wonders about.
#[expect(
    clippy::unused_async,
    reason = "every module's install has this signature; this one has nothing to do"
)]
pub async fn install(_conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    Ok(())
}

/// What a tenant enabling this module needs installed.
///
/// **`hr`**, and not `payroll`. The end-of-service calculation is asked about a
/// person who is leaving, which a business does whether or not it runs payroll
/// here — and a tenant who keeps staff records and pays them through a bank's
/// own system should still be able to answer "what do we owe her".
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(module_id(), "", &[], upcasters).requiring(&["hr"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("hr_sa")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
///
/// **None.** This module writes no events; the declaration exists because every
/// module is asked for one.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(erp_eventlog::Upcasters::new)
}
