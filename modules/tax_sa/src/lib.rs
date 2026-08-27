//! **Saudi Arabia**: what the rate is, what the return says, and what was filed.
//!
//! The first module that stands on two others, and the one that answers a
//! question the model raised: *where does country-specific tax live?*
//!
//! # Why a country is a module
//!
//! Because every country's is different. Saudi Arabia has ZATCA and 15%; the UAE
//! has Peppol PINT AE and 5%; the rate, the return's shape, the clearance
//! protocol and the fields an invoice must print all change at the border. None
//! of that belongs in `ledger`, which is the accounting kernel every country
//! uses, and none of it belongs in `sales`, which knows what an invoice is and
//! not where it was issued.
//!
//! So `ledger` owns the *shape* — a line has a treatment and a rate — and this
//! module owns the *values*: it seeds `ledger::Rates` when a tenant enables it,
//! and it holds ZATCA.
//!
//! # Why the return moved here from `erp-api`
//!
//! It was composed in the API, with the reasoning that cross-module composition
//! belongs in the composition root. Under the core/module model that is wrong:
//! netting output tax against input tax is **domain**, and core holds none. The
//! test is *can a tenant disable it?* — and a business with neither sales nor
//! purchases had a VAT return endpoint, which is the answer.
//!
//! Composing it in a module that **declares both** is the shape that keeps the
//! dependency arrows straight: `tax_sa → {sales, purchases} → ledger`. Nothing
//! reaches sideways, and `requires` says so where a tenant can read it.
//!
//! # ZATCA, and the one thing that is absent
//!
//! Every invoice becomes a document ZATCA has to see: [`zatca`] builds it, this
//! module's projections derive it from `sales` events, and [`submit_pending`]
//! sends it. A buyer with a VAT number gets a **standard** invoice, cleared
//! before they are given it; everyone else gets a **simplified** one, reported
//! within twenty-four hours.
//!
//! What is absent is the socket. Submitting needs a production CSID — a
//! certificate ZATCA issues after onboarding one taxpayer's one solution — and
//! an `XAdES` signature made with it. That is one implementation of
//! [`zatca::wire::Submitter`], and everything up to it is here: the canonical
//! UBL, the hash chain, the QR, both endpoints, and the reading of the answer.
//!
//! [`FilingEvent::Filed::reference`] is still waiting, and for a different
//! thing: that is the acknowledgement for a **VAT return**, which is filed on
//! ZATCA's portal rather than through the invoicing API.

mod clearance;
mod commands;
mod documents;
mod filing;
pub mod http;
pub mod messages;
mod onboarded;
mod projections;
mod report;
mod submit;
pub mod taxpayer;
pub mod zatca;

pub use clearance::{Clearance, ClearanceEvent};
pub use commands::{Filed, TaxError, file_return, period_id, record_outcome, register_taxpayer};
pub use documents::{
    Pending, Standing, Status, Stored, Taxpayers, Unsigned, ZatcaDocuments, document, documents,
    pending, registered, standing, unsigned,
};
pub use filing::{Filing, FilingEvent};
pub use onboarded::{Onboarding, OnboardingEvent, onboarding_id};
pub use projections::{
    FiledReturn, FiledReturns, Onboarded, Onboardings, Outcomes, TaxSa, filed, onboarding,
    projections,
};
pub use report::{Band, Return, Side, Sides, vat_return};
pub use submit::{SignedOff, SweepError, Swept, sign_pending, submit_pending};
pub use taxpayer::{Registration, Taxpayer, TaxpayerEvent};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

/// Creates this module's read models, then seeds the Saudi rate.
///
/// The same two steps in the same order as `ControlPlane::install_module`, and
/// **read from [`setup`]** rather than restated — a helper that installed a
/// different thing from the one production installs would make every test that
/// used it a test of something else.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    let setup = setup();

    sqlx::raw_sql(
        "CREATE SCHEMA IF NOT EXISTS proj_tax_sa; SET search_path TO proj_tax_sa, public;",
    )
    .execute(&mut *conn)
    .await?;

    sqlx::raw_sql(sqlx::AssertSqlSafe(setup.install_sql.to_owned()))
        .execute(&mut *conn)
        .await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(setup.seed_sql.to_owned()))
        .execute(&mut *conn)
        .await?;

    sqlx::raw_sql("SET search_path TO public")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <TaxSa as erp_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str)] = &[(
    <TaxSa as erp_projection::ProjectionGroup>::NAME,
    <TaxSa as erp_projection::ProjectionGroup>::SCHEMA,
)];

/// What a tenant enabling this module needs installed.
///
/// # Why one side is enough, and no side is not
///
/// A VAT return nets output tax against input tax, so this crate depends on both
/// `sales` and `purchases` and calls their read functions. The **entitlement**
/// needs at least one of them and does not care which: a business that only
/// sells still files a return, and demanding purchases would force a shop with
/// no supplier bills to enable a module they do not use in order to declare tax
/// they do owe. Each side is reported if the tenant has it and zero if not —
/// which is not a fallback but the truth.
///
/// It used to require *neither*, because `requires` is an AND list and there was
/// nothing else to say it with. That let a tenant enable a return with nothing
/// on either side, and let them disable the last module feeding it without a
/// word. [`ModuleSetup::requires_any`] is the shape that says it.
///
/// `ledger` is named too, though `sales` and `purchases` each bring it: this
/// module reads `ledger::Rates` itself, and a dependency you rely on directly is
/// one you declare rather than inherit.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
    // The Saudi VAT rate. `ledger` owns the shape of a rate and has no opinion
    // about the number; this is where the number comes from.
    .seeding(include_str!("../schema/seed.sql"))
    .requiring(&["ledger"])
    .requiring_any(&["sales", "purchases"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("tax_sa")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
///
/// Its own, **and `sales`'** — because this module builds ZATCA documents from
/// `sales.invoice.issued`, and a projection cannot decode an event whose version
/// it has not declared. Folded in rather than re-declared, so a version `sales`
/// adds next year is readable here without a second copy of its history that
/// could disagree with the first. See [`erp_eventlog::Upcasters::also`].
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        let mine = FilingEvent::NAMES
            .iter()
            .chain(TaxpayerEvent::NAMES.iter())
            .chain(ClearanceEvent::NAMES.iter())
            .chain(OnboardingEvent::NAMES.iter())
            .fold(erp_eventlog::Upcasters::new(), |u, n| {
                u.declare(&name(n), VERSION_1)
            });
        mine.also(sales::upcasters())
    })
}

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
