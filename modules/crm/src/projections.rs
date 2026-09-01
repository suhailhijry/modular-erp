//! The customer list.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{Cursor, Page, Timestamp};
use sqlx::PgConnection;

use crate::customer::CustomerEvent;

/// One table, and it is the whole group.
///
/// Small on purpose. A customer is referenced by `sales`, `booking` and
/// `prepaid`, and a group is the unit of consistency (L3), so keeping this one
/// narrow means the thing everything else points at is never waiting on a
/// projection that has nothing to do with it.
#[derive(Debug)]
pub struct Crm;

impl ProjectionGroup for Crm {
    const NAME: &'static str = "crm";
    const SCHEMA: &'static str = "proj_crm";
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

/// Every customer, as they are now.
#[derive(Debug)]
pub struct Customers;

#[async_trait::async_trait]
impl Projection for Customers {
    type Group = Crm;

    fn name(&self) -> &'static str {
        "customers"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !CustomerEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<CustomerEvent>(ctx, envelope)? {
            CustomerEvent::Registered {
                name,
                name_latin,
                kind,
                contact,
                address,
                tax,
                registered_on,
            } => {
                let a = address.unwrap_or_default();
                sqlx::query(
                    "INSERT INTO customer
                         (id, name, name_latin, kind, phone, email,
                          street, building, district, city, postal_code, country,
                          vat_number, id_scheme, identifier,
                          registered_on, recorded_at, position)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18)",
                )
                .bind(id)
                .bind(&name)
                .bind(&name_latin)
                .bind(kind.as_str())
                .bind(&contact.phone)
                .bind(&contact.email)
                .bind(none_if_blank(&a.street))
                .bind(&a.building)
                .bind(&a.district)
                .bind(none_if_blank(&a.city))
                .bind(&a.postal_code)
                .bind(none_if_blank(&a.country))
                .bind(tax.as_ref().map(|t| t.vat_number.as_str()))
                .bind(tax.as_ref().and_then(|t| t.scheme.as_deref()))
                .bind(tax.as_ref().and_then(|t| t.identifier.as_deref()))
                .bind(registered_on)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            CustomerEvent::Amended {
                name,
                name_latin,
                kind,
                contact,
                address,
                tax,
            } => {
                amend(
                    ctx,
                    conn,
                    id,
                    &name,
                    name_latin.as_deref(),
                    kind,
                    &contact,
                    &address.unwrap_or_default(),
                    tax.as_ref(),
                )
                .await?;
            }
            CustomerEvent::Archived { reason } => {
                sqlx::query(
                    "UPDATE customer
                        SET archived_at = $2, archived_why = $3, recorded_at = $2, position = $4
                      WHERE id = $1",
                )
                .bind(id)
                .bind(ctx.event_time())
                .bind(&reason)
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            CustomerEvent::Restored => {
                sqlx::query(
                    "UPDATE customer
                        SET archived_at = NULL, archived_why = NULL,
                            recorded_at = $2, position = $3
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

/// The `Amended` arm, lifted out so `apply` stays a dispatch.
///
/// Fifteen columns is what a customer record is, and inlining it made `apply`
/// long enough that the shape of the dispatch stopped being visible.
#[expect(clippy::too_many_arguments, reason = "one event, taken apart")]
async fn amend(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    name: &str,
    name_latin: Option<&str>,
    kind: crate::customer::CustomerKind,
    contact: &crate::customer::Contact,
    address: &crate::customer::Address,
    tax: Option<&crate::customer::TaxRegistration>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE customer
            SET name = $2, name_latin = $3, kind = $4, phone = $5, email = $6,
                street = $7, building = $8, district = $9, city = $10,
                postal_code = $11, country = $12,
                vat_number = $13, id_scheme = $14, identifier = $15,
                recorded_at = $16, position = $17
          WHERE id = $1",
    )
    .bind(id)
    .bind(name)
    .bind(name_latin)
    .bind(kind.as_str())
    .bind(&contact.phone)
    .bind(&contact.email)
    .bind(none_if_blank(&address.street))
    .bind(&address.building)
    .bind(&address.district)
    .bind(none_if_blank(&address.city))
    .bind(&address.postal_code)
    .bind(none_if_blank(&address.country))
    .bind(tax.map(|t| t.vat_number.as_str()))
    .bind(tax.and_then(|t| t.scheme.as_deref()))
    .bind(tax.and_then(|t| t.identifier.as_deref()))
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await
    .map(|_| ())
}

/// An empty string is not an address line, it is a missing one.
///
/// `Address` defaults its required fields to `String::new()` so that a customer
/// with no address at all still decodes, and writing those through as empty
/// text would make "no city" and "a city called nothing" the same row.
fn none_if_blank(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Every projection this module contributes.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Crm>>> {
    vec![std::sync::Arc::new(Customers)]
}

/// A customer, as a list shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomerSummary {
    pub id: String,
    pub name: String,
    pub name_latin: Option<String>,
    pub kind: String,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub vat_number: Option<String>,
    pub registered_on: Timestamp,
    pub archived: bool,
}

/// Customers, most recently registered first, one page at a time.
///
/// Keyset on `(registered_on, id)` descending, the same shape as
/// `sales::invoices` and for the same reasons.
///
/// `include_archived` is a parameter and not two functions, because the caller
/// that wants both is a search box and the caller that wants one is a list, and
/// they are otherwise identical.
pub async fn customers(
    conn: &mut PgConnection,
    include_archived: bool,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<CustomerSummary>, sqlx::Error> {
    let (registered_on, id) = resume(after);
    let rows = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", name_latin, kind as "kind!",
                  phone, email, vat_number,
                  registered_on as "registered_on!",
                  (archived_at IS NOT NULL) as "archived!"
             FROM proj_crm.customer
            WHERE ($4 OR archived_at IS NULL)
              AND ($2::timestamptz IS NULL OR (registered_on, id) < ($2, $3))
            ORDER BY registered_on DESC, id DESC
            LIMIT $1"#,
        limit,
        registered_on,
        id,
        include_archived,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter()
            .map(|r| CustomerSummary {
                id: r.id,
                name: r.name,
                name_latin: r.name_latin,
                kind: r.kind,
                phone: r.phone,
                email: r.email,
                vat_number: r.vat_number,
                registered_on: r.registered_on,
                archived: r.archived,
            })
            .collect(),
        limit,
        |c| Cursor::over(&[&c.registered_on.to_rfc3339(), &c.id]),
    ))
}

/// One customer, with everything on the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomerDetail {
    pub summary: CustomerSummary,
    pub street: Option<String>,
    pub building: Option<String>,
    pub district: Option<String>,
    pub city: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub id_scheme: Option<String>,
    pub identifier: Option<String>,
    pub archived_why: Option<String>,
}

/// One customer by id.
///
/// `None` if there is no such customer, **or** if the projection has not caught
/// up with one that was just created, which is what `?consistent_after=` is for.
pub async fn customer(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<CustomerDetail>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", name_latin, kind as "kind!",
                  phone, email, vat_number, id_scheme, identifier,
                  street, building, district, city, postal_code, country,
                  registered_on as "registered_on!",
                  (archived_at IS NOT NULL) as "archived!", archived_why
             FROM proj_crm.customer WHERE id = $1"#,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|r| CustomerDetail {
        summary: CustomerSummary {
            id: r.id,
            name: r.name,
            name_latin: r.name_latin,
            kind: r.kind,
            phone: r.phone,
            email: r.email,
            vat_number: r.vat_number,
            registered_on: r.registered_on,
            archived: r.archived,
        },
        street: r.street,
        building: r.building,
        district: r.district,
        city: r.city,
        postal_code: r.postal_code,
        country: r.country,
        id_scheme: r.id_scheme,
        identifier: r.identifier,
        archived_why: r.archived_why,
    }))
}

/// The cursor's two parts, or nothing.
///
/// A cursor from an older build may have fewer parts than this expects, which
/// is a cursor to refuse rather than guess at. `Cursor::part` returning `None`
/// lands here as "start from the top", and the API refuses an unreadable
/// cursor before it ever reaches this.
fn resume(after: Option<&Cursor>) -> (Option<Timestamp>, Option<String>) {
    let Some(cursor) = after else {
        return (None, None);
    };
    let when = cursor.part(0).and_then(|raw| raw.parse::<Timestamp>().ok());
    let id = cursor.part(1).map(str::to_owned);
    match (when, id) {
        (Some(w), Some(i)) => (Some(w), Some(i)),
        _ => (None, None),
    }
}
