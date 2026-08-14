//! Shapes and helpers every module's routes need.
//!
//! Extracted when the second module arrived and wanted all four of them. With
//! one module they lived in `ledger_routes`, which was the right place for them
//! then.

use serde::Deserialize;
use spa_eventlog::Metadata;
use spa_i18n::{Locale, Message, MessageArg, MessageCode};
use spa_types::{AggregateId, CurrencyCode, ModuleId, Money};

use crate::error::ApiError;
use crate::extract::{Allowed, Capability, Tenant};
use crate::problem::Problem;

/// An amount, as a client sends it.
///
/// Minor units and an explicit currency — never a decimal string, and never a
/// float. A client that sends `10.50` has already lost the argument about how
/// many decimal places the currency has.
#[derive(Debug, Deserialize)]
pub(crate) struct Amount {
    pub(crate) minor: i64,
    pub(crate) currency: String,
}

impl Amount {
    pub(crate) fn parse(&self, locale: Locale) -> Result<Money, Problem> {
        let currency = CurrencyCode::new(&self.currency).map_err(|_| {
            bad_request(
                crate::messages::UNKNOWN_CURRENCY,
                "currency",
                &self.currency,
                locale,
            )
        })?;
        Ok(Money::from_minor(self.minor, currency))
    }
}

/// Records who did it. Every event carries this (architecture L5).
///
/// Generic over the capability, because every write is behind a different one
/// and they all deref to the same `Tenant`.
pub(crate) fn metadata<C: Capability>(tenant: &Allowed<C>) -> Metadata {
    Metadata {
        actor: Some(tenant.session.identity.to_string()),
        ..Metadata::default()
    }
}

pub(crate) fn parse_id(raw: &str, locale: Locale) -> Result<AggregateId, Problem> {
    AggregateId::new(raw).map_err(|_| bad_request(crate::messages::INVALID_ID, "id", raw, locale))
}

pub(crate) fn bad_request(code: MessageCode, arg: &str, value: &str, locale: Locale) -> Problem {
    ApiError::BadRequest(Message::new(code).with(arg, MessageArg::text(value.to_owned())))
        .into_problem(locale)
}

/// Refuses a route belonging to a module the tenant did not enable.
///
/// A 404, not a 403: the route does not exist for this tenant, and saying
/// "forbidden" would tell them what they are not paying for in a way a 404 does
/// not.
///
/// ponytail: a runtime check, called at the top of each module's handlers. The
/// architecture's `ModuleEnabled<M>` token would make a disabled module's
/// handler unconstructable instead — worth building when a module has enough
/// routes that remembering the call becomes the weak link.
pub(crate) fn require_module(
    tenant: &Tenant,
    module: &ModuleId,
    locale: Locale,
) -> Result<(), Problem> {
    if tenant.db.has_module(module) {
        return Ok(());
    }
    Err(ApiError::NotFound(
        Message::new(crate::messages::MODULE_NOT_ENABLED)
            .with("module", MessageArg::text(module.as_str().to_owned())),
    )
    .into_problem(locale))
}
