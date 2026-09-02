//! Following a short link.
//!
//! One route, at the shortest path in this API, because that is the entire
//! point of the thing: SMS is billed by length and a segment boundary at 160
//! characters is a real cost per message per customer.
//!
//! # Why it is public
//!
//! The person tapping it has never signed in and never will. A link in a text
//! message reaches a customer, not a user — which is exactly the surface Phase
//! 17 built [`Public`] for, so this inherits its tenant resolution, its rate
//! limiting and its refusal to enter a tenant that is not enterable.
//!
//! # Why the answers are three different statuses
//!
//! `404`, `410` and `302` are three different instructions to the person
//! holding the phone. "Check you copied it whole", "ask for a new one" and
//! "here you are" are not the same sentence, and collapsing them into one is
//! how a support call happens.

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_i18n::{Locale, Localize};
use erp_web::{AppState, Language, Problem, Public};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(follow_link))
}

/// Follow a short link.
///
/// Records the visit, then redirects. A link that has expired or has already
/// been used answers `410 Gone` rather than `404`, because the two are
/// different things to whoever is holding it.
///
/// **Never cached.** A single-use link that a proxy served twice from its cache
/// would be a single-use link used twice, and a visit count nobody incremented
/// is a visit count nobody can trust.
#[utoipa::path(
    get,
    path = "/l/{token}",
    tag = "service",
    security(),
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`."),
        ("token" = String, Path, description = "The token from the link."),
    ),
    responses(
        (status = FOUND, description = "Where it points, in `Location`. The only 2xx-adjacent answer this route has — a redirect *is* the success."),
        (status = NOT_FOUND, description = "No such link, or no such tenant", body = Problem),
        (status = GONE, description = "Expired, or already used — the message says which", body = Problem),
        (status = TOO_MANY_REQUESTS, description = "Rate limited. `args.seconds` says how long to wait.", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn follow_link(
    tenant: Public,
    Language(locale): Language,
    Path(token): Path<String>,
) -> Result<Response, Problem> {
    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| refused(erp_control::AccessError::Pool(e), locale))?;

    // The caller's instant is `now` here and nowhere else in this system: a
    // stranger tapping a link has no clock this API can read, and expiry has to
    // be measured against something.
    let at = chrono::Utc::now();
    let followed = erp_links::follow(&mut conn, &token, at).await;
    drop(conn);

    match followed {
        Ok(target) => Ok(redirect(&target)),
        Err(erp_links::StoreError::Refused(refused)) => Err(Problem::new(
            status_of(&refused),
            &refused.message(),
            locale,
            &crate::CATALOG,
        )),
        Err(erp_links::StoreError::Database(e)) => {
            Err(refused(erp_control::AccessError::Database(e), locale))
        }
    }
}

/// `404` for a link that was never here, `410` for one that was.
const fn status_of(refused: &erp_links::LinkError) -> StatusCode {
    match refused {
        erp_links::LinkError::NoSuchLink | erp_links::LinkError::NotATarget(_) => {
            StatusCode::NOT_FOUND
        }
        erp_links::LinkError::Expired | erp_links::LinkError::AlreadyUsed => StatusCode::GONE,
    }
}

/// The redirect, with the header that stops it being served twice.
///
/// An internal target goes out as a **relative** `Location`, so it resolves
/// against whichever host the request arrived on. Writing an absolute URL here
/// would mean this crate learning the tenant's public domain, and getting it
/// wrong behind a proxy is a redirect to a hostname a browser cannot reach.
fn redirect(target: &erp_links::Target) -> Response {
    let location = HeaderValue::try_from(target.target.as_str());
    let Ok(location) = location else {
        // A target the column accepted and a header cannot hold: a control
        // character, or something beyond Latin-1. Refusing is right — this is
        // ours to have refused at creation, and answering with a header a
        // client cannot parse is worse than answering with nothing.
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    (
        StatusCode::FOUND,
        [
            (header::LOCATION, location),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store, private"),
            ),
        ],
    )
        .into_response()
}

fn refused(error: erp_control::AccessError, locale: Locale) -> Problem {
    tracing::warn!(%error, "a link could not be followed");
    erp_web::ApiError::Access(error).into_problem(locale, &crate::CATALOG)
}
