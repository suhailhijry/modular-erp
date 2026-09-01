//! The list of places, for a settings screen and for a picker on a document.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{Cursor, Page, Timestamp};
use sqlx::PgConnection;

use crate::branch::{Address, BranchEvent};

/// One table, one group.
#[derive(Debug)]
pub struct Branches;

impl ProjectionGroup for Branches {
    const NAME: &'static str = "branches";
    const SCHEMA: &'static str = "proj_branches";
}

fn decode<E: serde::de::DeserializeOwned>(
    ctx: &ProjectionCtx<'_>,
    envelope: &Envelope,
) -> Result<E, ProjectionError> {
    ctx.decode(envelope)
        .map_err(|source| ProjectionError::Decode {
            event_name: envelope.event_name.as_str().to_owned(),
            position: envelope.position,
            source,
        })
}

/// Places, as a screen lists them.
#[derive(Debug)]
pub struct BranchList;

#[async_trait::async_trait]
impl Projection for BranchList {
    type Group = Branches;

    fn name(&self) -> &'static str {
        "branches"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !BranchEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<BranchEvent>(ctx, envelope)? {
            BranchEvent::Opened {
                name,
                name_latin,
                address,
                at,
            } => {
                sqlx::query(
                    "INSERT INTO branch
                         (id, name, name_latin, street, building, district, city,
                          postal_code, country, opened_on, recorded_at, position)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
                )
                .bind(id)
                .bind(&name)
                .bind(name_latin.as_deref())
                .bind(&address.street)
                .bind(address.building.as_deref())
                .bind(address.district.as_deref())
                .bind(&address.city)
                .bind(address.postal_code.as_deref())
                .bind(&address.country)
                .bind(at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            BranchEvent::Amended {
                name,
                name_latin,
                address,
                ..
            } => {
                sqlx::query(
                    "UPDATE branch
                        SET name = $2, name_latin = $3, street = $4, building = $5,
                            district = $6, city = $7, postal_code = $8, country = $9,
                            recorded_at = $10, position = $11
                      WHERE id = $1",
                )
                .bind(id)
                .bind(&name)
                .bind(name_latin.as_deref())
                .bind(&address.street)
                .bind(address.building.as_deref())
                .bind(address.district.as_deref())
                .bind(&address.city)
                .bind(address.postal_code.as_deref())
                .bind(&address.country)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            BranchEvent::Closed { why, at } => {
                sqlx::query(
                    "UPDATE branch
                        SET closed_at = $2, closed_why = $3, recorded_at = $4, position = $5
                      WHERE id = $1",
                )
                .bind(id)
                .bind(at)
                .bind(&why)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            BranchEvent::Reopened { .. } => {
                sqlx::query(
                    "UPDATE branch
                        SET closed_at = NULL, closed_why = NULL, recorded_at = $2, position = $3
                      WHERE id = $1",
                )
                .bind(id)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}

/// Every projection this module contributes.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Branches>>> {
    vec![std::sync::Arc::new(BranchList)]
}

// -------------------------------------------------------------------- reads

/// A place, as a screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchSummary {
    pub id: String,
    pub name: String,
    pub name_latin: Option<String>,
    pub address: Address,
    /// Set when it stopped trading. It keeps everything it traded.
    pub closed_at: Option<Timestamp>,
    pub closed_why: Option<String>,
    pub opened_on: Timestamp,
}

macro_rules! summarise {
    ($r:expr) => {{
        let r = $r;
        BranchSummary {
            id: r.id,
            name: r.name,
            name_latin: r.name_latin,
            address: Address {
                street: r.street,
                building: r.building,
                district: r.district,
                city: r.city,
                postal_code: r.postal_code,
                country: r.country,
            },
            closed_at: r.closed_at,
            closed_why: r.closed_why,
            opened_on: r.opened_on,
        }
    }};
}

/// Places, by name.
pub async fn branches(
    conn: &mut PgConnection,
    include_closed: bool,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<BranchSummary>, sqlx::Error> {
    let (name, id) = resume(after);
    let rows = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", name_latin,
                  street as "street!", building, district, city as "city!",
                  postal_code, country as "country!",
                  closed_at, closed_why, opened_on as "opened_on!"
             FROM proj_branches.branch
            WHERE ($4 OR closed_at IS NULL)
              AND ($2::text = '' OR (name, id) > ($2, $3))
            ORDER BY name, id
            LIMIT $1"#,
        limit,
        name,
        id,
        include_closed,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter().map(|r| summarise!(r)).collect(),
        limit,
        |b| Cursor::over(&[&b.name, &b.id]),
    ))
}

/// One of them.
pub async fn branch(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<BranchSummary>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", name_latin,
                  street as "street!", building, district, city as "city!",
                  postal_code, country as "country!",
                  closed_at, closed_why, opened_on as "opened_on!"
             FROM proj_branches.branch WHERE id = $1"#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|r| summarise!(r)))
}

/// A cursor over `(name, id)`, ascending — a settings list reads alphabetically
/// rather than newest-first, which is the one place in this codebase that is
/// true.
fn resume(after: Option<&Cursor>) -> (String, String) {
    match after.map(Cursor::parts) {
        Some(parts) if parts.len() == 2 => (parts[0].clone(), parts[1].clone()),
        _ => (String::new(), String::new()),
    }
}
