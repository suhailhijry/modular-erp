//! The sales module's read models.

use spa_eventlog::Envelope;
use spa_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use spa_types::{CurrencyCode, Money, Timestamp};
use sqlx::PgConnection;

use crate::invoice::InvoiceEvent;
use crate::vat::VatCategory;

/// Invoices, their lines, their tax bands and their payments — one group.
///
/// One group because a payment against an invoice that has not appeared yet is a
/// state nobody should be able to query, and separate groups would replay at
/// different rates and produce exactly that (architecture L3). The foreign keys
/// in `install.sql` turn that from a convention into a constraint.
///
/// It is a *different* group from the ledger's, which is the point: sales never
/// reads `proj_ledger` and the ledger never reads `proj_sales`. What they share
/// is the event log.
#[derive(Debug)]
pub struct Sales;

impl ProjectionGroup for Sales {
    const NAME: &'static str = "sales";
    const SCHEMA: &'static str = "proj_sales";
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

/// Every invoice, with what it is made of and what has been paid against it.
#[derive(Debug)]
pub struct Invoices;

#[async_trait::async_trait]
impl Projection for Invoices {
    type Group = Sales;

    fn name(&self) -> &'static str {
        "invoices"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !InvoiceEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<InvoiceEvent>(ctx, envelope)? {
            InvoiceEvent::Issued {
                number,
                customer,
                issued_on,
                due_on,
                currency,
                lines,
                totals,
                note,
            } => {
                let invoice = NewInvoice {
                    // Issued before this system numbered anything: the number
                    // *was* the client-chosen id, and that is the number on the
                    // copy the customer holds.
                    number: number.unwrap_or_else(|| id.to_owned()),
                    customer,
                    issued_on,
                    due_on,
                    currency,
                    lines,
                    totals,
                    note,
                };
                write_issued(ctx, conn, id, invoice).await?;
            }

            InvoiceEvent::Cancelled {
                credit_note, on, ..
            } => {
                sqlx::query("UPDATE invoice SET cancelled_on = $2, credit_note = $3 WHERE id = $1")
                    .bind(id)
                    .bind(on)
                    .bind(&credit_note)
                    .execute(&mut *conn)
                    .await?;
            }

            InvoiceEvent::PaymentRecorded {
                payment,
                amount,
                received_on,
                account,
            } => {
                sqlx::query(
                    "INSERT INTO invoice_payment
                         (id, invoice_id, reference, amount, received_on, account, recorded_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(ctx.derive_id(&format!("payment-{payment}")))
                .bind(id)
                .bind(&payment)
                .bind(amount.minor())
                .bind(received_on)
                .bind(account.as_str())
                .bind(ctx.event_time())
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}

/// Everything an `Issued` event carries, so writing it is one call rather than
/// nine arguments.
struct NewInvoice {
    number: String,
    customer: crate::invoice::Customer,
    issued_on: Timestamp,
    due_on: Option<Timestamp>,
    currency: CurrencyCode,
    lines: Vec<crate::invoice::InvoiceLine>,
    totals: crate::vat::Totals,
    note: String,
}

/// The invoice, its lines and its tax bands — three inserts that belong
/// together, kept out of the match arm so `apply` stays readable.
async fn write_issued(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    invoice: NewInvoice,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "INSERT INTO invoice
             (id, number, customer, customer_vat, issued_on, due_on, currency,
              net, tax, gross, note, recorded_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(id)
    .bind(&invoice.number)
    .bind(&invoice.customer.name)
    .bind(invoice.customer.vat_number.as_deref())
    .bind(invoice.issued_on)
    .bind(invoice.due_on)
    .bind(invoice.currency.as_str())
    .bind(invoice.totals.net.minor())
    .bind(invoice.totals.tax.minor())
    .bind(invoice.totals.gross.minor())
    .bind(&invoice.note)
    // The event's time, never the wall clock (L2).
    .bind(ctx.event_time())
    .execute(&mut *conn)
    .await?;

    for (index, line) in invoice.lines.iter().enumerate() {
        let index = i32::try_from(index).unwrap_or(i32::MAX);
        sqlx::query(
            "INSERT INTO invoice_line
                 (id, invoice_id, line_index, description, net,
                  vat_category, vat_rate_bp)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        // Derived from the position, so a rebuild produces the same key.
        // `Uuid::new_v4()` here would make every replay differ.
        .bind(ctx.derive_id(&format!("line-{index}")))
        .bind(id)
        .bind(index)
        .bind(&line.description)
        .bind(line.net.minor())
        .bind(line.vat.category.as_str())
        .bind(line.vat.basis_points)
        .execute(&mut *conn)
        .await?;
    }

    for band in &invoice.totals.bands {
        sqlx::query(
            "INSERT INTO invoice_tax
                 (id, invoice_id, vat_category, vat_rate_bp, net, tax)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(ctx.derive_id(&format!(
            "tax-{}-{}",
            band.category.as_str(),
            band.basis_points
        )))
        .bind(id)
        .bind(band.category.as_str())
        .bind(band.basis_points)
        .bind(band.net.minor())
        .bind(band.tax.minor())
        .execute(&mut *conn)
        .await?;
    }

    Ok(())
}

/// Every projection this module contributes.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Sales>>> {
    vec![std::sync::Arc::new(Invoices)]
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// An invoice and where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceSummary {
    /// The client's own key, and what a route addresses.
    pub id: String,
    /// The statutory number, from the tenant's gapless series. What the document
    /// prints and what an auditor counts.
    pub number: String,
    /// When a credit note cancelled it, and which one.
    pub cancelled_on: Option<Timestamp>,
    pub credit_note: Option<String>,
    pub customer: String,
    pub customer_vat: Option<String>,
    pub issued_on: Timestamp,
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

/// One line of an invoice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceLineRow {
    pub description: String,
    pub net: Money,
    pub category: VatCategory,
    pub basis_points: i32,
}

/// One rate's subtotal, as the invoice must print it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxRow {
    pub category: VatCategory,
    pub basis_points: i32,
    pub net: Money,
    pub tax: Money,
}

/// Money received against an invoice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentRow {
    pub reference: String,
    pub amount: Money,
    pub received_on: Timestamp,
    pub account: String,
}

/// An invoice with everything needed to render it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceDetail {
    pub summary: InvoiceSummary,
    pub lines: Vec<InvoiceLineRow>,
    pub tax: Vec<TaxRow>,
    pub payments: Vec<PaymentRow>,
}

/// Invoices, newest first.
///
/// ponytail: no cursor. A tenant with more invoices than fit in one response is
/// a tenant worth building paging for, and the `issued_on DESC` index is what it
/// would page on.
pub async fn invoices(
    conn: &mut PgConnection,
    limit: i64,
) -> Result<Vec<InvoiceSummary>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", number as "number!", customer as "customer!", customer_vat,
                  issued_on as "issued_on!", due_on,
                  currency as "currency!",
                  net as "net!", tax as "tax!", gross as "gross!",
                  paid as "paid!", outstanding as "outstanding!",
                  payments as "payments!", note as "note!",
                  cancelled_on, credit_note
             FROM proj_sales.invoice_status
            ORDER BY issued_on DESC, id
            LIMIT $1"#,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency = parse_currency(&row.currency)?;
            Ok(InvoiceSummary {
                id: row.id,
                number: row.number,
                customer: row.customer,
                customer_vat: row.customer_vat,
                issued_on: row.issued_on,
                due_on: row.due_on,
                net: Money::from_minor(row.net, currency),
                tax: Money::from_minor(row.tax, currency),
                gross: Money::from_minor(row.gross, currency),
                paid: Money::from_minor(row.paid, currency),
                outstanding: Money::from_minor(row.outstanding, currency),
                payments: row.payments,
                note: row.note,
                cancelled_on: row.cancelled_on,
                credit_note: row.credit_note,
            })
        })
        .collect()
}

/// One invoice with its lines, tax bands and payments. `None` if there is no
/// such invoice — or if the projection has not caught up with it yet, which is
/// what `?consistent_after=` is for.
pub async fn invoice(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<InvoiceDetail>, sqlx::Error> {
    let Some(header) = sqlx::query!(
        r#"SELECT id as "id!", number as "number!", customer as "customer!", customer_vat,
                  issued_on as "issued_on!", due_on,
                  currency as "currency!",
                  net as "net!", tax as "tax!", gross as "gross!",
                  paid as "paid!", outstanding as "outstanding!",
                  payments as "payments!", note as "note!",
                  cancelled_on, credit_note
             FROM proj_sales.invoice_status
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
        r#"SELECT description as "description!", net as "net!",
                  vat_category as "vat_category!", vat_rate_bp as "vat_rate_bp!"
             FROM proj_sales.invoice_line
            WHERE invoice_id = $1
            ORDER BY line_index"#,
        id,
    )
    .fetch_all(&mut *conn)
    .await?;

    let tax = sqlx::query!(
        r#"SELECT vat_category as "vat_category!", vat_rate_bp as "vat_rate_bp!",
                  net as "net!", tax as "tax!"
             FROM proj_sales.invoice_tax
            WHERE invoice_id = $1
            ORDER BY vat_category, vat_rate_bp"#,
        id,
    )
    .fetch_all(&mut *conn)
    .await?;

    let payments = sqlx::query!(
        r#"SELECT reference as "reference!", amount as "amount!",
                  received_on as "received_on!", account as "account!"
             FROM proj_sales.invoice_payment
            WHERE invoice_id = $1
            ORDER BY received_on, reference"#,
        id,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Some(InvoiceDetail {
        summary: InvoiceSummary {
            id: header.id,
            number: header.number,
            customer: header.customer,
            customer_vat: header.customer_vat,
            issued_on: header.issued_on,
            due_on: header.due_on,
            net: Money::from_minor(header.net, currency),
            tax: Money::from_minor(header.tax, currency),
            gross: Money::from_minor(header.gross, currency),
            paid: Money::from_minor(header.paid, currency),
            outstanding: Money::from_minor(header.outstanding, currency),
            payments: header.payments,
            note: header.note,
            cancelled_on: header.cancelled_on,
            credit_note: header.credit_note,
        },
        lines: lines
            .into_iter()
            .map(|row| {
                Ok(InvoiceLineRow {
                    description: row.description,
                    net: Money::from_minor(row.net, currency),
                    category: parse_category(&row.vat_category)?,
                    basis_points: row.vat_rate_bp,
                })
            })
            .collect::<Result<_, sqlx::Error>>()?,
        tax: tax
            .into_iter()
            .map(|row| {
                Ok(TaxRow {
                    category: parse_category(&row.vat_category)?,
                    basis_points: row.vat_rate_bp,
                    net: Money::from_minor(row.net, currency),
                    tax: Money::from_minor(row.tax, currency),
                })
            })
            .collect::<Result<_, sqlx::Error>>()?,
        payments: payments
            .into_iter()
            .map(|row| PaymentRow {
                reference: row.reference,
                amount: Money::from_minor(row.amount, currency),
                received_on: row.received_on,
                account: row.account,
            })
            .collect(),
    }))
}

fn parse_currency(raw: &str) -> Result<CurrencyCode, sqlx::Error> {
    CurrencyCode::new(raw).map_err(|e| sqlx::Error::Decode(Box::new(e)))
}

fn parse_category(raw: &str) -> Result<VatCategory, sqlx::Error> {
    raw.parse()
        .map_err(|e: String| sqlx::Error::Decode(Box::new(std::io::Error::other(e))))
}

// ---------------------------------------------------------------------------
// The VAT return
// ---------------------------------------------------------------------------

/// One rate's line on a VAT return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VatBand {
    pub category: VatCategory,
    pub basis_points: i32,
    /// Supplies at this rate **net of credit notes falling in this period**,
    /// excluding tax. Negative when a period credits more than it invoices,
    /// which is what a quiet quarter after a big cancellation looks like.
    pub net: Money,
    /// Tax on them, and owed to ZATCA.
    pub tax: Money,
    /// Invoices whose tax point falls in this period.
    pub invoices: i64,
    /// Credit notes whose tax point falls in this period — which is not the
    /// same set of periods as the invoices they credit, and the reason the two
    /// are counted apart.
    pub credit_notes: i64,
}

/// What a business declares for a period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VatReturn {
    pub from: Timestamp,
    /// Exclusive, so consecutive periods neither overlap nor leave a day out.
    pub until: Timestamp,
    pub currency: CurrencyCode,
    pub bands: Vec<VatBand>,
    pub net: Money,
    /// The number that goes on the return.
    pub tax: Money,
}

/// The output-tax side of a VAT return, by rate, for a period.
///
/// # What this is and is not
///
/// It is what a business *charged*. A full return also nets off input tax on
/// purchases, which needs a purchases module — so this is one side of it, and
/// the side that exists.
///
/// # Why the period is half-open
///
/// `[from, until)`. A period ending "31 March inclusive" is a timestamp
/// comparison somebody gets wrong once a quarter, and two consecutive returns
/// built that way either double-count the boundary or drop it.
///
/// Returns `None` when the tenant has no supplies in the period at all, which
/// is a real answer — a business with nothing to declare still files.
pub async fn vat_return(
    conn: &mut PgConnection,
    currency: CurrencyCode,
    from: Timestamp,
    until: Timestamp,
) -> Result<VatReturn, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT vat_category as "vat_category!", vat_rate_bp as "vat_rate_bp!",
                  sum(net)::BIGINT as "net!", sum(tax)::BIGINT as "tax!",
                  count(*) FILTER (WHERE kind = 'invoice') as "invoices!",
                  count(*) FILTER (WHERE kind = 'credit_note') as "credit_notes!"
             FROM proj_sales.vat_entry
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
            Ok(VatBand {
                category: parse_category(&row.vat_category)?,
                basis_points: row.vat_rate_bp,
                net: Money::from_minor(row.net, currency),
                tax: Money::from_minor(row.tax, currency),
                invoices: row.invoices,
                credit_notes: row.credit_notes,
            })
        })
        .collect::<Result<Vec<VatBand>, sqlx::Error>>()?;

    let total = |amounts: &dyn Fn(&VatBand) -> Money| {
        Money::checked_sum(bands.iter().map(amounts), currency)
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))
    };
    let net = total(&|b| b.net)?;
    let tax = total(&|b| b.tax)?;

    Ok(VatReturn {
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

/// An invoice whose payments exceed it.
///
/// Impossible through [`record_payment`](crate::record_payment), which refuses
/// an overpayment against the aggregate's own state. A row here means the
/// pipeline is broken — a payment projected twice, or a rebuild that diverged —
/// which is the same kind of canary the ledger's trial balance is, and worth
/// checking for the same reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overpaid {
    pub invoice: String,
    pub gross: Money,
    pub paid: Money,
}

/// The health check this module contributes. Empty is healthy.
pub async fn overpaid(conn: &mut PgConnection) -> Result<Vec<Overpaid>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", currency as "currency!",
                  gross as "gross!", paid as "paid!"
             FROM proj_sales.invoice_status
            WHERE paid > gross
            ORDER BY id"#
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency = parse_currency(&row.currency)?;
            Ok(Overpaid {
                invoice: row.id,
                gross: Money::from_minor(row.gross, currency),
                paid: Money::from_minor(row.paid, currency),
            })
        })
        .collect()
}
