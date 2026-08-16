//! The purchases module's read models.

use ledger::VatCategory;
use spa_eventlog::Envelope;
use spa_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use spa_types::{CurrencyCode, Money, Timestamp};
use sqlx::PgConnection;

use crate::bill::BillEvent;

/// Bills, their lines and their payments — one group.
///
/// A group of its own, and never reading `proj_sales` or `proj_ledger`
/// (architecture L3). The combined VAT return is composed in the API from each
/// module's own reads, which is where cross-module composition belongs.
#[derive(Debug)]
pub struct Purchases;

impl ProjectionGroup for Purchases {
    const NAME: &'static str = "purchases";
    const SCHEMA: &'static str = "proj_purchases";
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

/// Every bill, with what it is made of and what has been paid against it.
#[derive(Debug)]
pub struct Bills;

#[async_trait::async_trait]
impl Projection for Bills {
    type Group = Purchases;

    fn name(&self) -> &'static str {
        "bills"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !BillEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<BillEvent>(ctx, envelope)? {
            BillEvent::Received {
                supplier,
                supplier_reference,
                billed_on,
                due_on,
                currency,
                lines,
                net,
                tax,
                gross,
                note,
            } => {
                sqlx::query(
                    "INSERT INTO bill
                         (id, supplier, supplier_vat, reference, billed_on, due_on,
                          currency, net, tax, gross, note, recorded_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                )
                .bind(id)
                .bind(&supplier.name)
                .bind(supplier.vat_number.as_deref())
                .bind(&supplier_reference)
                .bind(billed_on)
                .bind(due_on)
                .bind(currency.as_str())
                .bind(net.minor())
                .bind(tax.minor())
                .bind(gross.minor())
                .bind(&note)
                // The event's time, never the wall clock (L2).
                .bind(ctx.event_time())
                .execute(&mut *conn)
                .await?;

                for (index, line) in lines.iter().enumerate() {
                    let index = i32::try_from(index).unwrap_or(i32::MAX);
                    sqlx::query(
                        "INSERT INTO bill_line
                             (id, bill_id, line_index, description, account, net,
                              vat_category, vat_rate_bp, tax)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                    )
                    // Derived from the position, so a rebuild produces the same
                    // key. `Uuid::new_v4()` here would make every replay differ.
                    .bind(ctx.derive_id(&format!("line-{index}")))
                    .bind(id)
                    .bind(index)
                    .bind(&line.description)
                    .bind(line.account.as_str())
                    .bind(line.net.minor())
                    .bind(line.category.as_str())
                    .bind(line.rate_bp)
                    .bind(line.tax.minor())
                    .execute(&mut *conn)
                    .await?;
                }
            }

            BillEvent::PaymentMade {
                payment,
                amount,
                paid_on,
                from,
            } => {
                sqlx::query(
                    "INSERT INTO bill_payment
                         (id, bill_id, reference, amount, paid_on, account, recorded_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(ctx.derive_id(&format!("payment-{payment}")))
                .bind(id)
                .bind(&payment)
                .bind(amount.minor())
                .bind(paid_on)
                .bind(from.as_str())
                .bind(ctx.event_time())
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}

/// Every projection this module contributes.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Purchases>>> {
    vec![std::sync::Arc::new(Bills)]
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// A bill and where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillSummary {
    /// Our own key, and what a route addresses.
    pub id: String,
    pub supplier: String,
    pub supplier_vat: Option<String>,
    /// The supplier's own invoice number.
    pub reference: String,
    pub billed_on: Timestamp,
    pub due_on: Option<Timestamp>,
    pub net: Money,
    pub tax: Money,
    pub gross: Money,
    pub paid: Money,
    /// Gross minus paid. Zero means settled.
    pub outstanding: Money,
    pub payments: i64,
    pub note: String,
}

/// One line of a bill, as the supplier stated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillLineRow {
    pub description: String,
    pub account: String,
    pub net: Money,
    pub category: VatCategory,
    pub basis_points: i32,
    pub tax: Money,
}

/// One payment made against a bill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentRow {
    pub reference: String,
    pub amount: Money,
    pub paid_on: Timestamp,
    pub account: String,
}

/// A bill with everything on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillDetail {
    pub summary: BillSummary,
    pub lines: Vec<BillLineRow>,
    pub payments: Vec<PaymentRow>,
}

/// Bills, most recently billed first.
///
/// ponytail: no cursor, same as `sales::invoices`. A tenant with a list long
/// enough to need one is the signal to build it.
pub async fn bills(conn: &mut PgConnection, limit: i64) -> Result<Vec<BillSummary>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", supplier as "supplier!", supplier_vat,
                  reference as "reference!",
                  billed_on as "billed_on!", due_on,
                  currency as "currency!",
                  net as "net!", tax as "tax!", gross as "gross!",
                  paid as "paid!", outstanding as "outstanding!",
                  payments as "payments!", note as "note!"
             FROM proj_purchases.bill_status
            ORDER BY billed_on DESC, id
            LIMIT $1"#,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency = parse_currency(&row.currency)?;
            Ok(BillSummary {
                id: row.id,
                supplier: row.supplier,
                supplier_vat: row.supplier_vat,
                reference: row.reference,
                billed_on: row.billed_on,
                due_on: row.due_on,
                net: Money::from_minor(row.net, currency),
                tax: Money::from_minor(row.tax, currency),
                gross: Money::from_minor(row.gross, currency),
                paid: Money::from_minor(row.paid, currency),
                outstanding: Money::from_minor(row.outstanding, currency),
                payments: row.payments,
                note: row.note,
            })
        })
        .collect()
}

/// One bill with its lines and payments. `None` if there is no such bill — or if
/// the projection has not caught up with it yet, which is what
/// `?consistent_after=` is for.
pub async fn bill(conn: &mut PgConnection, id: &str) -> Result<Option<BillDetail>, sqlx::Error> {
    let Some(header) = sqlx::query!(
        r#"SELECT id as "id!", supplier as "supplier!", supplier_vat,
                  reference as "reference!",
                  billed_on as "billed_on!", due_on,
                  currency as "currency!",
                  net as "net!", tax as "tax!", gross as "gross!",
                  paid as "paid!", outstanding as "outstanding!",
                  payments as "payments!", note as "note!"
             FROM proj_purchases.bill_status
            WHERE id = $1"#,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };

    let currency = parse_currency(&header.currency)?;

    let lines = sqlx::query!(
        r#"SELECT description as "description!", account as "account!",
                  net as "net!", vat_category as "vat_category!",
                  vat_rate_bp as "vat_rate_bp!", tax as "tax!"
             FROM proj_purchases.bill_line
            WHERE bill_id = $1
            ORDER BY line_index"#,
        id,
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|row| {
        Ok(BillLineRow {
            description: row.description,
            account: row.account,
            net: Money::from_minor(row.net, currency),
            category: parse_category(&row.vat_category)?,
            basis_points: row.vat_rate_bp,
            tax: Money::from_minor(row.tax, currency),
        })
    })
    .collect::<Result<Vec<_>, sqlx::Error>>()?;

    let payments = sqlx::query!(
        r#"SELECT reference as "reference!", amount as "amount!",
                  paid_on as "paid_on!", account as "account!"
             FROM proj_purchases.bill_payment
            WHERE bill_id = $1
            ORDER BY paid_on, reference"#,
        id,
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|row| PaymentRow {
        reference: row.reference,
        amount: Money::from_minor(row.amount, currency),
        paid_on: row.paid_on,
        account: row.account,
    })
    .collect();

    Ok(Some(BillDetail {
        summary: BillSummary {
            id: header.id,
            supplier: header.supplier,
            supplier_vat: header.supplier_vat,
            reference: header.reference,
            billed_on: header.billed_on,
            due_on: header.due_on,
            net: Money::from_minor(header.net, currency),
            tax: Money::from_minor(header.tax, currency),
            gross: Money::from_minor(header.gross, currency),
            paid: Money::from_minor(header.paid, currency),
            outstanding: Money::from_minor(header.outstanding, currency),
            payments: header.payments,
            note: header.note,
        },
        lines,
        payments,
    }))
}

// ---------------------------------------------------------------------------
// The input side of a VAT return
// ---------------------------------------------------------------------------

/// One rate band of what a business **paid** in a period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputBand {
    pub category: VatCategory,
    pub basis_points: i32,
    /// Purchases at this rate, excluding tax.
    pub net: Money,
    /// **Reclaimable** tax only. Exempt purchases contribute `net` and no tax,
    /// because tax on an exempt supply is a cost rather than a debt ZATCA owes
    /// back.
    pub tax: Money,
    pub bills: i64,
}

/// What a business can reclaim for a period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputTax {
    pub from: Timestamp,
    /// Exclusive, so consecutive periods neither overlap nor leave a day out.
    pub until: Timestamp,
    pub currency: CurrencyCode,
    pub bands: Vec<InputBand>,
    pub net: Money,
    /// The number that goes on the return.
    pub tax: Money,
}

/// The input-tax side of a VAT return, for a period.
///
/// Each bill is reported on its own tax point — the date the supplier stated,
/// not the date it was typed in. The same rule the output side follows, and for
/// the same reason: a period that has been declared must not change.
pub async fn input_tax(
    conn: &mut PgConnection,
    currency: CurrencyCode,
    from: Timestamp,
    until: Timestamp,
) -> Result<InputTax, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT vat_category as "vat_category!", vat_rate_bp as "vat_rate_bp!",
                  sum(net)::BIGINT as "net!", sum(tax)::BIGINT as "tax!",
                  count(DISTINCT document_id) as "bills!"
             FROM proj_purchases.vat_entry
            WHERE currency = $1 AND tax_point >= $2 AND tax_point < $3
            GROUP BY vat_category, vat_rate_bp
            ORDER BY vat_category, vat_rate_bp"#,
        currency.as_str(),
        from,
        until,
    )
    .fetch_all(&mut *conn)
    .await?;

    let bands = rows
        .into_iter()
        .map(|row| {
            Ok(InputBand {
                category: parse_category(&row.vat_category)?,
                basis_points: row.vat_rate_bp,
                net: Money::from_minor(row.net, currency),
                tax: Money::from_minor(row.tax, currency),
                bills: row.bills,
            })
        })
        .collect::<Result<Vec<InputBand>, sqlx::Error>>()?;

    let total = |amounts: &dyn Fn(&InputBand) -> Money| {
        Money::checked_sum(bands.iter().map(amounts), currency)
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))
    };
    let net = total(&|b| b.net)?;
    let tax = total(&|b| b.tax)?;

    Ok(InputTax {
        from,
        until,
        currency,
        bands,
        net,
        tax,
    })
}

// ---------------------------------------------------------------------------
// The invariant
// ---------------------------------------------------------------------------

/// A bill paid more than it was for.
///
/// Impossible through [`pay_bill`](crate::pay_bill), which refuses an
/// overpayment against the aggregate's own state. A row here means the pipeline
/// is broken — a payment projected twice, or a rebuild that diverged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overpaid {
    pub bill: String,
    pub gross: Money,
    pub paid: Money,
}

/// The health check this module contributes. Empty is healthy.
pub async fn overpaid(conn: &mut PgConnection) -> Result<Vec<Overpaid>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", currency as "currency!",
                  gross as "gross!", paid as "paid!"
             FROM proj_purchases.bill_status
            WHERE paid > gross
            ORDER BY id"#
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency = parse_currency(&row.currency)?;
            Ok(Overpaid {
                bill: row.id,
                gross: Money::from_minor(row.gross, currency),
                paid: Money::from_minor(row.paid, currency),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------

fn parse_currency(raw: &str) -> Result<CurrencyCode, sqlx::Error> {
    CurrencyCode::new(raw).map_err(|e| sqlx::Error::Decode(Box::new(e)))
}

fn parse_category(raw: &str) -> Result<VatCategory, sqlx::Error> {
    raw.parse::<VatCategory>()
        .map_err(|e| sqlx::Error::Decode(e.into()))
}
