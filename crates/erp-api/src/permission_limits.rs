//! **A tenant's permission limits**, as a setting its owner can see and change.
//!
//! `erp_tenant::Limits` narrows what a role may do by facts — the amount, the
//! branch, the capability, the role — and every capability check has read it
//! since Phase 5b. Nothing wrote it. This is the writer, and it is a copy of
//! the calendar's: a `GET` answers with the version as `ETag`, a `PUT` takes
//! `If-Match`, and `set_by` on the row says who wrote it. Nothing is recorded
//! in the control plane's audit trail: this is the tenant's own configuration,
//! kept in its own database like every other setting.
//!
//! **Both are owner-only, and no limit can take them away.** They take
//! `ManageTenant`, which `TenantDb::permits` never narrows — so a rule that
//! refuses everything, or a stored row this build can no longer read, still
//! leaves the owner able to fix it. See `erp_tenant::limits`.

use axum::http::StatusCode;
use erp_i18n::{Locale, Message, MessageArg};
use erp_rules::{Invalid, Rules};
use erp_tenant::{Limits, Verdict, limits::Unusable};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::{
    Allowed, AppState, IfMatch, Json, Language, ManageTenant, Problem, Versioned, config_problem,
};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(permission_limits, set_permission_limits))
}

/// The rules, in order. **The first that matches decides**, so an exception
/// goes above the refusal it is an exception to.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({ "rules": [{
    "name": "A bookkeeper posts under ten thousand",
    "when": { "when": "all", "of": [
        { "when": "is", "fact": "role", "op": "eq", "value": { "type": "text", "of": "accountant" } },
        { "when": "is", "fact": "capability", "op": "eq", "value": { "type": "text", "of": "post_entries" } },
        { "when": "is", "fact": "amount", "op": "gte", "value": { "type": "money", "of": { "minor": 1_000_000, "currency": "SAR" } } }
    ] },
    "then": "refuse"
}] }))]
struct LimitsView {
    /// Each is `{ name, when, then }`. `then` is `refuse`, or `allow` — which
    /// only ever restores what the role already permits. `when` asks about
    /// `amount` (money), `branch` (a branch id), `capability` (`read`,
    /// `post_entries`, `manage_accounts`) or `role` (`owner`, `accountant`,
    /// `clerk`, `viewer`), and combines with `all`, `any`, `not` and `always`.
    /// An amount in another currency than the one compared against has no
    /// answer, and no answer refuses: a `refuse` rule in `SAR` refuses an entry
    /// in `USD` it would otherwise judge, and an `allow` in `USD` excepts
    /// nothing in `SAR`. A tenant that posts in two currencies puts an `allow`
    /// per currency above the refusal.
    #[schema(value_type = Vec<Object>)]
    rules: Rules<Verdict>,
}

/// What this tenant's roles may not do, beyond what their roles already say.
#[utoipa::path(
    get,
    path = "/v1/tenant/permission-limits",
    tag = "tenants",
    params(("Host" = String, Header, description = "The tenant's host.")),
    responses(
        (status = OK, description = "An empty list until somebody sets one: roles decide alone.", body = LimitsView, headers(("ETag" = String, description = "The version of this setting. Send it back as `If-Match` to write only if nobody else has since."))),
        (status = INTERNAL_SERVER_ERROR, description = "The stored rules name something this build no longer knows. Every check they would narrow answers 503 until they are set again, which this route's `PUT` still can", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not the owner", body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn permission_limits(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
) -> Result<Versioned<LimitsView>, Problem> {
    let mut conn = tenant.db.read().await.map_err(|e| {
        Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &crate::CATALOG)
    })?;
    let version = erp_eventlog::configuration::version_of(&mut conn, Limits::KEY)
        .await
        .map_err(|e| config_problem(&e, locale, &crate::CATALOG))?;
    let limits = erp_eventlog::configuration::get::<Limits>(&mut conn, Limits::KEY)
        .await
        .map_err(|e| config_problem(&e, locale, &crate::CATALOG))?
        .map(|configured| configured.value)
        .unwrap_or_default();
    Ok(Versioned(
        version,
        LimitsView {
            rules: limits.into(),
        },
    ))
}

/// Set them, replacing what was there.
///
/// **Applies from the next request**: every capability check reads them.
/// Nothing already done is undone.
#[utoipa::path(
    put,
    path = "/v1/tenant/permission-limits",
    tag = "tenants",
    params(
        ("Host" = String, Header, description = "The tenant's host."),
        ("If-Match" = Option<String>, Header, description = "The `ETag` a GET answered with. With it, the write happens only if the setting is still at that version; without it, unconditionally."),
    ),
    request_body = LimitsView,
    responses(
        (status = NO_CONTENT, description = "Set."),
        (status = BAD_REQUEST, description = "A rule that could never be true, named: `request.no_such_fact`, `request.no_such_fact_value` or `request.rule_cannot_compare`", body = Problem),
        (status = PRECONDITION_FAILED, description = "`If-Match` named a version that is no longer current; reload and try again", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not the owner", body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = UNPROCESSABLE_ENTITY, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn set_permission_limits(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
    IfMatch(expected): IfMatch,
    Json(body): Json<LimitsView>,
) -> Result<StatusCode, Problem> {
    let limits = Limits::new(body.rules).map_err(|e| unusable(&e, locale))?;
    let mut conn = tenant.db.acquire().await.map_err(|e| {
        Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &crate::CATALOG)
    })?;
    erp_eventlog::configuration::set(
        &mut conn,
        Limits::KEY,
        &limits,
        Some(&tenant.session.identity.to_string()),
        expected,
    )
    .await
    .map_err(|e| config_problem(&e, locale, &crate::CATALOG))?;
    Ok(StatusCode::NO_CONTENT)
}

/// The 400 a rule that can never be true gets, naming it.
fn unusable(e: &Unusable, locale: Locale) -> Problem {
    let text = |s: &str| MessageArg::text(s.to_owned());
    let message = match &e.why {
        Invalid::NoSuchFact { name, known } => Message::new(erp_web::messages::NO_SUCH_FACT)
            .with("fact", text(name))
            .with("known", text(&known.join(", "))),
        // `covers` — a window of time — is a question no permission check can
        // answer, which is exactly what the message for an unknown fact says.
        Invalid::NoSpans => Message::new(erp_web::messages::NO_SUCH_FACT)
            .with("fact", text("covers"))
            .with(
                "known",
                text(&erp_tenant::limits::registry().names().join(", ")),
            ),
        Invalid::NoSuchValue { name, value, known } => {
            Message::new(erp_web::messages::NO_SUCH_FACT_VALUE)
                .with("fact", text(name))
                .with("value", text(value))
                .with("known", text(&known.join(", ")))
        }
        Invalid::WrongKind { name, .. } | Invalid::NotOrderable { name, .. } => {
            Message::new(erp_web::messages::RULE_CANNOT_COMPARE).with("fact", text(name))
        }
    };
    Problem::new(
        StatusCode::BAD_REQUEST,
        &message.with("rule", text(&e.rule)),
        locale,
        &crate::CATALOG,
    )
}
