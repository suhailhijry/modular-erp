//! What is attached to what.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::Timestamp;
use sqlx::PgConnection;

use crate::file::{FileEvent, OwnerKind};

#[derive(Debug)]
pub struct Files;

impl ProjectionGroup for Files {
    const NAME: &'static str = "files";
    const SCHEMA: &'static str = "proj_files";
}

#[derive(Debug)]
pub struct Attachments;

#[async_trait::async_trait]
impl Projection for Attachments {
    type Group = Files;

    fn name(&self) -> &'static str {
        "attachments"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !FileEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match ctx
            .decode::<FileEvent>(envelope)
            .map_err(|source| ProjectionError::Decode {
                event_name: envelope.event_name.as_str().to_owned(),
                position: envelope.position,
                source,
            })? {
            FileEvent::Stored {
                name,
                owner,
                stored,
                at,
            } => {
                sqlx::query(
                    "INSERT INTO file
                         (id, name, owner_kind, owner_id, engine, storage_key, checksum,
                          size, media_type, stored_at, recorded_at, position)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
                     ON CONFLICT (id) DO NOTHING",
                )
                .bind(id)
                .bind(&name)
                .bind(owner.kind.as_str())
                .bind(owner.id.as_str())
                .bind(&stored.engine)
                .bind(&stored.key)
                .bind(&stored.checksum)
                .bind(stored.size)
                .bind(&stored.media_type)
                .bind(at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            FileEvent::Removed { why, at } => {
                sqlx::query(
                    "UPDATE file SET removed_at = $2, removed_why = $3, position = $4
                      WHERE id = $1",
                )
                .bind(id)
                .bind(at)
                .bind(&why)
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}

#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Files>>> {
    vec![std::sync::Arc::new(Attachments)]
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// One document, as somebody looking at a list reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub owner_kind: String,
    pub owner_id: String,
    /// Where the bytes are. **Not a URL** — see `crate::file`.
    pub stored: erp_storage::Stored,
    pub stored_at: Timestamp,
    pub removed_at: Option<Timestamp>,
    pub removed_why: Option<String>,
}

/// Everything attached to one thing, newest first.
///
/// Removed documents are left out. A caller that wants them wants an audit
/// trail, which is the log.
pub async fn attached_to(
    conn: &mut PgConnection,
    kind: OwnerKind,
    owner: &str,
    limit: i64,
) -> Result<Vec<Attachment>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", owner_kind as "owner_kind!",
                  owner_id as "owner_id!", engine as "engine!",
                  storage_key as "storage_key!", checksum as "checksum!",
                  size as "size!", media_type as "media_type!",
                  stored_at as "stored_at!", removed_at, removed_why
             FROM proj_files.file
            WHERE owner_kind = $1 AND owner_id = $2 AND removed_at IS NULL
            ORDER BY stored_at DESC, id
            LIMIT $3"#,
        kind.as_str(),
        owner,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| Attachment {
            id: row.id,
            name: row.name,
            owner_kind: row.owner_kind,
            owner_id: row.owner_id,
            stored: erp_storage::Stored {
                engine: row.engine,
                key: row.storage_key,
                checksum: row.checksum,
                size: row.size,
                media_type: row.media_type,
            },
            stored_at: row.stored_at,
            removed_at: row.removed_at,
            removed_why: row.removed_why,
        })
        .collect())
}

/// One document by id, removed ones included.
///
/// **Removed ones included on purpose.** A caller downloading by id has a link
/// somebody sent them, and "this was taken off on the 3rd" is a better answer
/// than "no such document".
pub async fn attachment(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<Attachment>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", owner_kind as "owner_kind!",
                  owner_id as "owner_id!", engine as "engine!",
                  storage_key as "storage_key!", checksum as "checksum!",
                  size as "size!", media_type as "media_type!",
                  stored_at as "stored_at!", removed_at, removed_why
             FROM proj_files.file WHERE id = $1"#,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|row| Attachment {
        id: row.id,
        name: row.name,
        owner_kind: row.owner_kind,
        owner_id: row.owner_id,
        stored: erp_storage::Stored {
            engine: row.engine,
            key: row.storage_key,
            checksum: row.checksum,
            size: row.size,
            media_type: row.media_type,
        },
        stored_at: row.stored_at,
        removed_at: row.removed_at,
        removed_why: row.removed_why,
    }))
}
