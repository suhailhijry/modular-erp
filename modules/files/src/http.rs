//! This module's HTTP surface.
//!
//! Translation only, like every module's.
//!
//! # Why an upload is a raw body and not multipart
//!
//! Because there is one file and no other fields. Multipart exists to carry a
//! form; this carries a document, and the two things a server needs about it —
//! what it is and what it is called — fit in a header and a query parameter.
//! The parser that comes with multipart is a dependency, an allocation strategy
//! and a class of bug, in exchange for nothing here.
//!
//! # Why a download is `attachment` and never inline
//!
//! An uploaded file is somebody else's bytes with somebody else's declared
//! type. Serving it inline means a browser may render it **in the tenant's own
//! origin**, and an HTML file uploaded as a "document" then runs as the tenant.
//! `Content-Disposition: attachment` and `X-Content-Type-Options: nosniff` are
//! what make that a download rather than a page.

use std::fmt::Write as _;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use erp_i18n::{Locale, Localize};
use erp_types::Timestamp;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_eventlog::ExecuteError;
use erp_tenant::CommandError;
use erp_web::AppState;
use erp_web::Problem;
use erp_web::{Allowed, IdempotencyKey, Language, PostEntries, Read};
use erp_web::{Consistency, Json, Query, creating, metadata, nudge, parse_id, require_module};

use crate::file::{Owner, OwnerKind};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_attachments))
        .routes(routes!(get_attachment, remove_file))
        // **The bytes are their own sub-resource**, and the reason is the body
        // limit. Every other route in this API takes a small JSON body and is
        // capped accordingly; this one takes a scanned contract. Keeping them
        // on separate paths is what lets the cap be raised for exactly the two
        // that need it.
        .routes(routes!(download_file, upload_file))
        .layer(axum::extract::DefaultBodyLimit::max(erp_storage::MAX_BYTES))
}

static CATALOG: erp_i18n::Composite =
    erp_i18n::Composite::new(&[&crate::CATALOG, &erp_storage::CATALOG, &erp_web::CATALOG]);

/// How many documents one listing gives back. A record with more attachments
/// than this has a filing problem rather than a paging problem.
const PAGE: i64 = 200;

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
struct FileView {
    id: String,
    name: String,
    /// What it is attached to — `invoice`, `bill`, `reservation`, `customer`,
    /// `employee`, `entry` or `tenant`.
    owner_kind: String,
    owner_id: String,
    /// Which engine holds it. **Not a URL** — fetch it from
    /// `/v1/files/{id}/content`.
    engine: String,
    /// SHA-256, hex. Checked on every download.
    checksum: String,
    size: i64,
    media_type: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    stored_at: Timestamp,
    /// Set when it was taken off. The record stays either way.
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    removed_at: Option<Timestamp>,
    removed_why: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct Attaching {
    /// What it is attached to.
    owner_kind: String,
    owner_id: String,
    /// What to call it. A filename, not a description.
    name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct OwnerQuery {
    owner_kind: String,
    owner_id: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct Removal {
    /// Why it came off. Kept on the record.
    #[serde(default)]
    why: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Everything attached to one thing, newest first.
#[utoipa::path(
    get,
    path = "/v1/files",
    tag = "files",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("owner_kind" = String, Query, description = "`invoice`, `bill`, `reservation`, `customer`, `employee`, `entry` or `tenant`."),
        ("owner_id" = String, Query, description = "The thing's id."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<FileView>),
        (status = BAD_REQUEST, description = "Not something a document can be attached to", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_attachments(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(owner): Query<OwnerQuery>,
) -> Result<Json<Vec<FileView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let kind = owner_kind(&owner.owner_kind, locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let found = crate::attached_to(&mut conn, kind, &owner.owner_id, PAGE)
        .await
        .map_err(|e| database(&e, locale))?;
    drop(conn);

    Ok(Json(found.into_iter().map(view).collect()))
}

/// One document's record. **Not its bytes** — those are `/content`.
#[utoipa::path(
    get,
    path = "/v1/files/{file}",
    tag = "files",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("file" = String, Path, description = "The document's id."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = FileView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such document", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn get_attachment(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<FileView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    Ok(Json(view(found(&tenant, &id, locale).await?)))
}

/// **Upload a document.**
///
/// The body is the file. `Content-Type` is what it is, and the query says what
/// it is called and what it belongs to.
///
/// Idempotent on the path id (L8): a retried upload writes the same bytes to
/// the same key and records nothing twice.
#[utoipa::path(
    post,
    path = "/v1/files/{file}/content",
    tag = "files",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("file" = String, Path, description = "The document's id, chosen by the caller. Sending it again with the same `Idempotency-Key` is a retry."),
        ("Idempotency-Key" = String, Header, description = "The caller's key for this upload. **Required**: without it, a second upload to the same id would be absorbed as a retry and the bytes overwritten."),
        ("owner_kind" = String, Query, description = "What it is attached to."),
        ("owner_id" = String, Query, description = "The thing's id."),
        ("name" = String, Query, description = "What to call it."),
        ("Content-Type" = String, Header, description = "What the file is. Recorded as declared and never sniffed."),
    ),
    request_body(content = String, description = "The file itself, as raw bytes.", content_type = "application/octet-stream"),
    responses(
        (status = CREATED, description = "Stored, and attached.", body = FileView),
        (status = BAD_REQUEST, description = "No name, no owner, or a media type this system will not take", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "That id is taken by a different upload", body = Problem),
        (status = PAYLOAD_TOO_LARGE, description = "Larger than this system will take", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Nowhere to keep it, or storage is unreachable", body = Problem),
    ),
)]
#[expect(
    clippy::too_many_arguments,
    reason = "six of them are extractors, which is how axum states what a \
              handler needs; collapsing them into a struct would hide the \
              capability check that is the point of the first"
)]
async fn upload_file(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    idempotency: IdempotencyKey,
    Path(id): Path<String>,
    Query(attaching): Query<Attaching>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<FileView>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    // **Refuses rather than dropping it.** The same call the sealing key makes:
    // a tenant told their contract uploaded when it went nowhere is worse served
    // than one told it did not.
    let Some(storage) = state.storage.clone() else {
        return Err(Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            &erp_i18n::Message::new(crate::messages::NO_STORAGE),
            locale,
            &CATALOG,
        ));
    };

    let file = parse_id(&id, locale)?;
    let owner = Owner {
        kind: owner_kind(&attaching.owner_kind, locale)?,
        id: parse_id(&attaching.owner_id, locale)?,
    };
    let media_type = declared_type(&headers, locale)?;

    let key = crate::key_for(tenant.db.tenant(), &owner, file.as_str());
    let stored = erp_storage::store(storage.as_ref(), &key, &body, &media_type)
        .await
        .map_err(|e| storage_refused(&e, locale))?;

    // **The bytes first, the event second.** An orphaned object is wasted space
    // somebody can sweep; a record pointing at bytes that were never written is
    // a document that cannot be opened, with nothing to say why.
    crate::attach(
        &tenant.db,
        &file,
        &attaching.name,
        &owner,
        &stored,
        chrono::Utc::now(),
        // **The fingerprint matters here more than usual.** Without it, a
        // second and different upload to a taken id would be absorbed as a
        // retry — and the bytes were already overwritten by then, because
        // storage runs before the event.
        &creating(&tenant, &idempotency),
    )
    .await
    .map_err(|e| refused(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok((
        StatusCode::CREATED,
        Json(FileView {
            id: file.as_str().to_owned(),
            name: attaching.name,
            owner_kind: owner.kind.as_str().to_owned(),
            owner_id: owner.id.as_str().to_owned(),
            engine: stored.engine,
            checksum: stored.checksum,
            size: stored.size,
            media_type: stored.media_type,
            stored_at: chrono::Utc::now(),
            removed_at: None,
            removed_why: None,
        }),
    ))
}

/// **Download it**, and prove it is the one that was stored.
///
/// The checksum is recomputed from what came back and a mismatch is a `500`
/// with `storage.corrupt` (L6). Handing somebody a document that is not the
/// document is worse than handing them nothing.
#[utoipa::path(
    get,
    path = "/v1/files/{file}/content",
    tag = "files",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("file" = String, Path, description = "The document's id."),
    ),
    responses(
        (status = OK, description = "The file, always as an attachment and never inline.", content_type = "application/octet-stream"),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such document, or its bytes are not in storage", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Nowhere to read it from, or storage is unreachable", body = Problem),
    ),
)]
async fn download_file(
    tenant: Allowed<Read>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
) -> Result<Response, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let Some(storage) = state.storage.clone() else {
        return Err(Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            &erp_i18n::Message::new(crate::messages::NO_STORAGE),
            locale,
            &CATALOG,
        ));
    };

    let record = found(&tenant, &id, locale).await?;
    let bytes = erp_storage::fetch(storage.as_ref(), &record.stored)
        .await
        .map_err(|e| storage_refused(&e, locale))?;

    let disposition = format!("attachment; filename*=UTF-8''{}", encoded(&record.name));
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, record.stored.media_type),
            (header::CONTENT_DISPOSITION, disposition),
            // Belt and braces with the disposition above: a browser that
            // decides for itself what a file is, is a browser that can be
            // talked into running it in the tenant's origin.
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
        ],
        bytes,
    )
        .into_response())
}

/// Take a document off what it is attached to.
///
/// **The bytes stay.** A document that was on an invoice is part of what
/// happened, and erasing it on a click would erase evidence — the same call
/// `crm::archive_customer` makes about never deleting a customer.
#[utoipa::path(
    delete,
    path = "/v1/files/{file}",
    tag = "files",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("file" = String, Path, description = "The document's id."),
    ),
    request_body = Removal,
    responses(
        (status = NO_CONTENT, description = "Taken off. The bytes are untouched."),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such document", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn remove_file(
    tenant: Allowed<PostEntries>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<Removal>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let file = parse_id(&id, locale)?;

    crate::detach(
        &tenant.db,
        &file,
        &body.why,
        chrono::Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| refused(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

fn view(a: crate::Attachment) -> FileView {
    FileView {
        id: a.id,
        name: a.name,
        owner_kind: a.owner_kind,
        owner_id: a.owner_id,
        engine: a.stored.engine,
        checksum: a.stored.checksum,
        size: a.stored.size,
        media_type: a.stored.media_type,
        stored_at: a.stored_at,
        removed_at: a.removed_at,
        removed_why: a.removed_why,
    }
}

async fn found(
    tenant: &Allowed<Read>,
    id: &str,
    locale: Locale,
) -> Result<crate::Attachment, Problem> {
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let found = crate::attachment(&mut conn, id)
        .await
        .map_err(|e| database(&e, locale))?;
    drop(conn);

    found.ok_or_else(|| {
        Problem::new(
            StatusCode::NOT_FOUND,
            &erp_i18n::Message::new(crate::messages::NO_SUCH_FILE)
                .with("id", erp_i18n::MessageArg::text(id)),
            locale,
            &CATALOG,
        )
    })
}

fn owner_kind(raw: &str, locale: Locale) -> Result<OwnerKind, Problem> {
    raw.parse().map_err(|e: crate::UnknownOwner| {
        Problem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(crate::messages::UNKNOWN_OWNER)
                .with("owner", erp_i18n::MessageArg::text(&e.0)),
            locale,
            &CATALOG,
        )
    })
}

/// What the uploader said it is.
///
/// **Taken as declared and never sniffed.** Guessing from the first few bytes
/// is how an HTML file becomes a "document" a browser renders in the tenant's
/// own origin; a declared type served as an attachment cannot be.
///
/// What is checked is that it looks like a media type at all, so the value that
/// comes back out of the database and into a `Content-Type` header is one.
fn declared_type(headers: &axum::http::HeaderMap, locale: Locale) -> Result<String, Problem> {
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");

    // `type/subtype`, plus whatever parameters follow, and nothing that could
    // end a header or start another one.
    let usable = declared.len() <= 128
        && declared.split_once('/').is_some_and(|(kind, rest)| {
            !kind.is_empty()
                && !rest.is_empty()
                && declared.chars().all(|c| c.is_ascii_graphic() || c == ' ')
        });

    if usable {
        Ok(declared.to_owned())
    } else {
        Err(Problem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(crate::messages::NOT_A_MEDIA_TYPE)
                .with("media_type", erp_i18n::MessageArg::text(declared)),
            locale,
            &CATALOG,
        ))
    }
}

/// RFC 5987 percent-encoding, so an Arabic filename survives a header.
fn encoded(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn refused(error: &CommandError<crate::FileError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                crate::FileError::NoSuchFile(_) => StatusCode::NOT_FOUND,
                crate::FileError::AlreadyRemoved(_) => StatusCode::UNPROCESSABLE_ENTITY,
                crate::FileError::Storage(e) => return storage_refused(e, locale),
                crate::FileError::NoName => StatusCode::BAD_REQUEST,
            },
            rejection.message(),
        ),

        CommandError::Pool(e @ erp_tenant::PoolError::Overloaded { .. }) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.message())
        }

        CommandError::Execute(ExecuteError::Contended { .. }) => (
            StatusCode::CONFLICT,
            erp_i18n::Message::new(erp_eventlog::messages::CONCURRENT_MODIFICATION),
        ),

        // **The one that must never be silent.** A different request reused an
        // id that is taken; a retry of the request that created it never
        // reaches here, because the kernel reports those as success.
        CommandError::Execute(ExecuteError::AlreadyExists { .. }) => (
            StatusCode::CONFLICT,
            erp_i18n::Message::new(erp_eventlog::messages::ALREADY_EXISTS),
        ),

        other => {
            tracing::error!(error = %other, "files command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &CATALOG)
}

fn storage_refused(error: &erp_storage::StorageError, locale: Locale) -> Problem {
    let status = match error {
        erp_storage::StorageError::NoSuchFile => StatusCode::NOT_FOUND,
        erp_storage::StorageError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        erp_storage::StorageError::NotAKey(_) => StatusCode::BAD_REQUEST,
        // **A corrupt file is ours, not the caller's.** They asked for a
        // document this system said it had; that it cannot produce it is a
        // failure here.
        erp_storage::StorageError::Corrupt { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        erp_storage::StorageError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    Problem::new(status, &error.message(), locale, &CATALOG)
}

fn pool(error: &erp_tenant::PoolError, locale: Locale) -> Problem {
    Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, error, locale, &CATALOG)
}

fn database(error: &sqlx::Error, locale: Locale) -> Problem {
    tracing::warn!(%error, "files could not read the database");
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        &erp_i18n::Message::new(crate::messages::DATABASE),
        locale,
        &CATALOG,
    )
}
