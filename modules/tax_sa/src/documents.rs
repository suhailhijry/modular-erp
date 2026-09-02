//! Building the ZATCA document, from `sales` events this module subscribes to.
//!
//! # The first extension module
//!
//! `sales` issues invoices and knows nothing about Saudi Arabia. This module
//! reads its events and builds something `sales` never asked for — which is what
//! an extension module *is*, and the answer to "how does a module extend another
//! without the other knowing?".
//!
//! Three things make it work, and all three are already in the kernel:
//!
//! 1. **A projection reads the whole log**, not just its own module's events, so
//!    subscribing costs nothing.
//! 2. **[`Upcasters::also`](erp_eventlog::Upcasters::also)** folds `sales`'
//!    event history into this module's, so a `sales` event version added next
//!    year is readable here without a second copy of its chain.
//! 3. **A projection group is the unit of consistency** (L3), and this builds
//!    into `proj_tax_sa`, never into `proj_sales`.
//!
//! # Why the chain is safe to rebuild
//!
//! Every input is the log: the invoice, the registration, and the order they
//! arrived in. The counter is the position in that order and the previous hash
//! is the previous document's. Replay the log and every document comes out
//! byte-identical, which it has to — the hashes went to a tax authority.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError};
use erp_types::{CurrencyCode, Money, Timestamp};
use sales::{InvoiceEvent, InvoiceLine};
use sqlx::PgConnection;

use crate::projections::TaxSa;
use crate::taxpayer::{Registration, TaxpayerEvent};
use crate::zatca::{
    Band, Buyer, Document, Kind, Line, Link, QR_TIME, Reference, Totals, TypeCode, chain,
    document_uuid, qr, ubl,
};

/// Keeps the registration this module renders documents under.
///
/// Its own projection rather than part of [`ZatcaDocuments`] because it is a
/// different fact with a different lifetime — and because the ordering between
/// them is the log's, not something these two negotiate.
#[derive(Debug)]
pub struct Taxpayers;

#[async_trait::async_trait]
impl Projection for Taxpayers {
    type Group = TaxSa;

    fn name(&self) -> &'static str {
        "taxpayer"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !TaxpayerEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }

        let TaxpayerEvent::Registered { registration, on } = ctx
            .decode::<TaxpayerEvent>(envelope)
            .map_err(|source| ProjectionError::Decode {
                event_name: envelope.event_name.as_str().to_owned(),
                position: envelope.position,
                source,
            })?;

        let encoded = serde_json::to_value(&registration).map_err(|e| {
            ProjectionError::Rejected(format!("a registration that will not serialise: {e}"))
        })?;

        sqlx::query(
            "INSERT INTO taxpayer (id, registration, registered_on, recorded_at)
             VALUES ('self', $1, $2, $3)
             ON CONFLICT (id) DO UPDATE
                SET registration  = EXCLUDED.registration,
                    registered_on = EXCLUDED.registered_on,
                    recorded_at   = EXCLUDED.recorded_at",
        )
        .bind(encoded)
        .bind(on)
        .bind(ctx.event_time())
        .execute(&mut *conn)
        .await?;

        Ok(())
    }
}

/// Builds a ZATCA document for every invoice and every credit note.
#[derive(Debug)]
pub struct ZatcaDocuments;

#[async_trait::async_trait]
impl Projection for ZatcaDocuments {
    type Group = TaxSa;

    fn name(&self) -> &'static str {
        "zatca_documents"
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

        let event =
            ctx.decode::<InvoiceEvent>(envelope)
                .map_err(|source| ProjectionError::Decode {
                    event_name: envelope.event_name.as_str().to_owned(),
                    position: envelope.position,
                    source,
                })?;

        match event {
            InvoiceEvent::Issued {
                number,
                customer,
                issued_on,
                currency,
                lines,
                discounts,
                totals,
                ..
            } => {
                // An invoice from before this system numbered anything cannot be
                // a ZATCA document: the number is mandatory on one, and its id
                // is not a number. Nothing to record and nothing to guess.
                let Some(number) = number else {
                    return Ok(());
                };

                let buyer = Buyer {
                    name: customer.name.clone(),
                    vat_number: customer.vat_number.clone(),
                    address: customer.address.clone(),
                };
                let built = Built {
                    kind: Kind::of(customer.vat_number.as_ref()),
                    type_code: TypeCode::Invoice,
                    number,
                    source: envelope.stream.id.as_str().to_owned(),
                    issued_at: issued_on,
                    currency,
                    buyer: Some(buyer),
                    lines: lines.iter().map(line).collect(),
                    allowances: discounts.iter().map(allowance).collect(),
                    totals: totals_of(&totals),
                    reference: None,
                    note: String::new(),
                };
                write(conn, ctx.event_time(), &built).await
            }

            // A credit note is a document in its own right — its own number, its
            // own place in the chain, its own clearance. It is built from the
            // invoice it cancels, which this module already holds, because
            // `sales` records a cancellation and not a second set of lines.
            InvoiceEvent::Cancelled {
                credit_note,
                reason,
                on,
                ..
            } => {
                let source = envelope.stream.id.as_str().to_owned();
                let Some(invoice) = invoice_of(conn, &source).await? else {
                    // The invoice had no number, so there is no document to
                    // credit. Same reason as above, one step along.
                    return Ok(());
                };

                let built = Built {
                    kind: invoice.kind,
                    type_code: TypeCode::CreditNote,
                    number: credit_note,
                    source,
                    issued_at: on,
                    currency: invoice.currency,
                    buyer: invoice.buyer.clone(),
                    lines: invoice.lines.clone(),
                    // A credit note against a discounted invoice credits what
                    // was actually charged, so it carries the same allowances.
                    allowances: invoice.allowances.clone(),
                    totals: invoice.totals.clone(),
                    reference: Some(Reference {
                        number: invoice.number.clone(),
                        issued_at: invoice.issued_at,
                    }),
                    note: reason,
                };
                write(conn, ctx.event_time(), &built).await
            }

            // A payment changes nothing ZATCA sees. The document was cleared on
            // what was charged, not on what has been collected — and a refund
            // is the same fact in the other direction. **What ZATCA sees when a
            // sale is undone is the credit note**, which arrives as `Cancelled`
            // above and is a document of its own.
            InvoiceEvent::PaymentRecorded { .. } | InvoiceEvent::Refunded { .. } => Ok(()),
        }
    }
}

/// Everything about a document except where it sits in the chain — which is
/// decided in [`write`], because it depends on what is already there.
struct Built {
    kind: Kind,
    type_code: TypeCode,
    number: String,
    source: String,
    issued_at: Timestamp,
    currency: CurrencyCode,
    buyer: Option<Buyer>,
    lines: Vec<Line>,
    allowances: Vec<crate::zatca::Allowance>,
    totals: Totals,
    reference: Option<Reference>,
    note: String,
}

/// Renders a document, gives it the next place in the chain, and stores it.
async fn write(
    conn: &mut PgConnection,
    recorded_at: Timestamp,
    built: &Built,
) -> Result<(), ProjectionError> {
    // **The registration as it stands at this position in the log**, which is
    // the whole reason it is an event. A rebuild reads the same row here because
    // it replays the same events in the same order.
    let Some(seller) = registration(conn).await? else {
        return unregistered(conn, recorded_at, built).await;
    };

    let link = next_link(conn).await?;
    let document = Document {
        kind: built.kind,
        type_code: built.type_code,
        number: built.number.clone(),
        uuid: document_uuid(&seller.vat_number, &built.number),
        issued_at: built.issued_at,
        currency: built.currency,
        seller,
        buyer: built.buyer.clone(),
        lines: built.lines.clone(),
        allowances: built.allowances.clone(),
        totals: built.totals.clone(),
        link,
        reference: built.reference.clone(),
        note: built.note.clone(),
    };

    // L6: a document that cannot be rendered stops the group. It means a value
    // reached the log that cannot appear in an XML document, and the honest
    // answer is that this tenant's ZATCA documents are wrong — not that this one
    // is quietly missing from a chain whose whole purpose is having no gaps.
    let xml = ubl::render(&document).map_err(|e| {
        ProjectionError::Rejected(format!("{} cannot be rendered: {e}", document.number))
    })?;
    let invoice_hash = chain::invoice_hash(&xml);

    let code = qr::Qr {
        seller: &document.seller.name,
        vat_number: &document.seller.vat_number,
        issued_at: &document.issued_at.format(QR_TIME).to_string(),
        total: &crate::zatca::amount(document.totals.gross),
        tax: &crate::zatca::amount(document.totals.tax),
        // Phase two, and only with a certificate: the hash is known here, and a
        // QR carrying a hash but no signature is a QR that fails validation for
        // claiming more than it can show.
        invoice_hash: None,
        signature: None,
        public_key: None,
        certificate_signature: None,
    }
    .encode()
    .map_err(|e| ProjectionError::Rejected(e.to_string()))?;

    let encoded = serde_json::to_value(&document).map_err(|e| {
        ProjectionError::Rejected(format!("a document that will not serialise: {e}"))
    })?;

    sqlx::query(
        "INSERT INTO zatca_document
             (id, source_id, kind, type_code, issued_at, currency, net, tax, gross,
              icv, previous_hash, invoice_hash, xml, qr, document, status, recorded_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, 'pending', $16)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&document.number)
    .bind(&built.source)
    .bind(document.kind.as_str())
    .bind(document.type_code.code())
    .bind(document.issued_at)
    .bind(document.currency.as_str())
    .bind(document.totals.net.minor())
    .bind(document.totals.tax.minor())
    .bind(document.totals.gross.minor())
    .bind(document.link.icv)
    .bind(&document.link.previous)
    .bind(&invoice_hash)
    .bind(&xml)
    .bind(&code)
    .bind(encoded)
    .bind(recorded_at)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// A document issued before the business registered with ZATCA.
///
/// Recorded, with no place in the chain. Skipping it silently would be the
/// "quietly under-reporting" failure this system keeps finding: a business needs
/// to know that these exist, because they are invoices that were issued and
/// cannot be cleared retrospectively — the chain starts at onboarding.
async fn unregistered(
    conn: &mut PgConnection,
    recorded_at: Timestamp,
    built: &Built,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "INSERT INTO zatca_document
             (id, source_id, kind, type_code, issued_at, currency, net, tax, gross,
              status, recorded_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'unregistered', $10)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&built.number)
    .bind(&built.source)
    .bind(built.kind.as_str())
    .bind(built.type_code.code())
    .bind(built.issued_at)
    .bind(built.currency.as_str())
    .bind(built.totals.net.minor())
    .bind(built.totals.tax.minor())
    .bind(built.totals.gross.minor())
    .bind(recorded_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The registration in force at this point in the log.
async fn registration(conn: &mut PgConnection) -> Result<Option<Registration>, ProjectionError> {
    let row: Option<(serde_json::Value,)> =
        sqlx::query_as("SELECT registration FROM taxpayer WHERE id = 'self'")
            .fetch_optional(&mut *conn)
            .await?;

    row.map(|(value,)| {
        serde_json::from_value(value).map_err(|e| {
            ProjectionError::Rejected(format!("the stored registration cannot be read: {e}"))
        })
    })
    .transpose()
}

/// The next position in the chain, and the hash it points back at.
async fn next_link(conn: &mut PgConnection) -> Result<Link, ProjectionError> {
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT icv, invoice_hash FROM zatca_document
          WHERE icv IS NOT NULL ORDER BY icv DESC LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(match row {
        Some((icv, hash)) => Link::after(icv, &hash),
        None => Link::first(),
    })
}

/// The document already built for an invoice, so a credit note can be built
/// against it.
async fn invoice_of(
    conn: &mut PgConnection,
    source: &str,
) -> Result<Option<Document>, ProjectionError> {
    let row: Option<(Option<serde_json::Value>,)> = sqlx::query_as(
        "SELECT document FROM zatca_document
          WHERE source_id = $1 AND type_code = 388 LIMIT 1",
    )
    .bind(source)
    .fetch_optional(&mut *conn)
    .await?;

    row.and_then(|(value,)| value)
        .map(|value| {
            serde_json::from_value(value).map_err(|e| {
                ProjectionError::Rejected(format!("a stored document cannot be read: {e}"))
            })
        })
        .transpose()
}

fn line(line: &InvoiceLine) -> Line {
    Line {
        description: line.description.clone(),
        net: line.net,
        category: line.vat.category,
        rate_bp: line.vat.basis_points,
        tax: tax_of(line),
    }
}

/// What was charged on one line.
///
/// `sales` stores the treatment and the rate per line and the tax per *band*,
/// because rounding once per band is what makes the invoice's total add up.
/// ZATCA wants both: each line's tax is its own net at its own rate, rounded on
/// its own, and each band's is the band's — two independent roundings that are
/// each checked against their own base and never against each other.
///
/// Through `sales`' own function rather than a second implementation of it,
/// because the rounding rule is the part that has to match: half away from zero,
/// which is what ZATCA specifies.
fn tax_of(line: &InvoiceLine) -> Money {
    line.vat
        .on(line.net)
        .unwrap_or_else(|_| Money::zero(line.net.currency()))
}

/// A discount, as the document renders it.
fn allowance(discount: &sales::Discount) -> crate::zatca::Allowance {
    crate::zatca::Allowance {
        reason: discount.reason.clone(),
        amount: discount.amount,
        category: discount.vat.category,
        rate_bp: discount.vat.basis_points,
    }
}

fn totals_of(totals: &sales::Totals) -> Totals {
    Totals {
        net: totals.net,
        tax: totals.tax,
        gross: totals.gross,
        // What the lines came to, which is `net` again when nothing was
        // discounted — recorded rather than recomputed downstream.
        before_discount: totals.before_discount().ok(),
        bands: totals
            .bands
            .iter()
            .map(|band| Band {
                category: band.category,
                rate_bp: band.basis_points,
                net: band.net,
                tax: band.tax,
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// A built document, as it stands with ZATCA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    /// The statutory number, which is the identity.
    pub number: String,
    /// The invoice it was built from.
    pub source: String,
    pub kind: Kind,
    pub type_code: i32,
    pub issued_at: Timestamp,
    pub currency: CurrencyCode,
    pub net: Money,
    pub tax: Money,
    pub gross: Money,
    /// `None` on a document issued before the business registered.
    pub icv: Option<i64>,
    pub previous_hash: Option<String>,
    pub invoice_hash: Option<String>,
    pub xml: Option<String>,
    pub qr: Option<String>,
    /// `ds:SignatureValue`, once it has been signed.
    pub signature: Option<String>,
    /// The document as submitted — the hashed bytes plus the signature, the QR
    /// and the `cac:Signature`.
    pub signed_xml: Option<String>,
    pub signed_at: Option<Timestamp>,
    pub status: Status,
    /// The document ZATCA stamped — **the one the buyer gets**.
    pub stamped_xml: Option<String>,
    /// Warnings on an accepted document, errors on a refused one.
    pub remarks: Vec<crate::zatca::wire::Remark>,
    pub settled_at: Option<Timestamp>,
}

/// Where a document stands with ZATCA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Issued before the business registered. It has no place in the chain and
    /// cannot be cleared retrospectively — the chain starts at onboarding.
    Unregistered,
    /// Built, and waiting to be submitted.
    Pending,
    /// ZATCA stamped it. Standard invoices.
    Cleared,
    /// ZATCA acknowledged it. Simplified invoices.
    Reported,
    /// ZATCA said no, and the document is what is wrong. A corrected document is
    /// a new document, never an edit to this one.
    Refused,
}

impl Status {
    pub const ALL: [Self; 5] = [
        Self::Unregistered,
        Self::Pending,
        Self::Cleared,
        Self::Reported,
        Self::Refused,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unregistered => "unregistered",
            Self::Pending => "pending",
            Self::Cleared => "cleared",
            Self::Reported => "reported",
            Self::Refused => "refused",
        }
    }

    /// Whether ZATCA has seen it and said yes.
    #[must_use]
    pub const fn is_settled(self) -> bool {
        matches!(self, Self::Cleared | Self::Reported)
    }
}

impl std::str::FromStr for Status {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == s)
            .ok_or_else(|| format!("unknown document status {s:?}"))
    }
}

/// One document waiting to be submitted, with everything the call needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub number: String,
    pub kind: Kind,
    pub issued_at: Timestamp,
    pub uuid: String,
    pub invoice_hash: String,
    /// The canonical bytes. What a signature would cover, and what is submitted.
    pub xml: String,
}

/// Where the business stands with ZATCA, in one answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    /// Whether there is a registration at all. Nothing can be submitted without
    /// one, so every other number here is moot until it is true.
    pub registered: bool,
    /// How many documents in each state.
    pub counts: Vec<(Status, i64)>,
    /// **Simplified invoices past their 24 hours and still not reported.** The
    /// number an inspection asks about.
    pub overdue: i64,
    /// **Standard invoices not yet cleared.** Not late — cleared before issue is
    /// the rule — but they are documents the buyer must not have been given yet.
    pub awaiting_clearance: i64,
    /// The oldest thing still waiting, which is what a person looks at first.
    pub oldest_pending: Option<Timestamp>,
    /// The last position in the chain, so a person can see it moving.
    pub chain_length: i64,
    /// **Documents with no signature yet.** They can be neither submitted nor
    /// printed — a simplified invoice's QR carries the stamp — so this is the
    /// number that says a tenant is not really live, whatever else is true.
    pub unsigned: i64,
}

/// The registration in force, if there is one.
pub async fn registered(conn: &mut PgConnection) -> Result<Option<Registration>, sqlx::Error> {
    let row = sqlx::query!(r#"SELECT registration FROM proj_tax_sa.taxpayer WHERE id = 'self'"#)
        .fetch_optional(&mut *conn)
        .await?;

    row.map(|row| {
        serde_json::from_value(row.registration).map_err(|e| sqlx::Error::Decode(Box::new(e)))
    })
    .transpose()
}

/// What to submit next, oldest first — because the 24-hour clock runs from issue
/// and the oldest is the closest to running out.
///
/// **Signed documents only.** ZATCA refuses an unsigned one, so submitting it
/// would spend a tenant's rate limit to be told so — and the rejection would be
/// recorded against a document that is not what is wrong.
pub async fn pending(conn: &mut PgConnection, limit: i64) -> Result<Vec<Pending>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", kind as "kind!", issued_at as "issued_at!",
                  document->>'uuid' as "uuid!", invoice_hash as "invoice_hash!",
                  signed_xml as "xml!"
             FROM proj_tax_sa.zatca_document
            WHERE status = 'pending'
              AND signed_xml IS NOT NULL AND invoice_hash IS NOT NULL
            ORDER BY issued_at, id
            LIMIT $1"#,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(Pending {
                number: row.id,
                kind: row
                    .kind
                    .parse()
                    .map_err(|e: String| sqlx::Error::Decode(Box::new(std::io::Error::other(e))))?,
                issued_at: row.issued_at,
                uuid: row.uuid,
                invoice_hash: row.invoice_hash,
                xml: row.xml,
            })
        })
        .collect()
}

/// One document waiting to be signed, with the bytes the signature covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsigned {
    pub number: String,
    pub kind: Kind,
    pub issued_at: Timestamp,
    /// The canonical bytes, exactly as they were hashed.
    pub xml: String,
    pub invoice_hash: String,
    /// What the QR needs, resolved here so the signer does not go back for it.
    pub seller: String,
    pub vat_number: String,
    pub total: String,
    pub tax: String,
}

/// What to sign next, oldest first.
///
/// A document has to be signed before it can be submitted **and** before it can
/// be printed: a simplified invoice's QR carries the stamp, and the receipt goes
/// to the customer at the till.
pub async fn unsigned(conn: &mut PgConnection, limit: i64) -> Result<Vec<Unsigned>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", kind as "kind!", issued_at as "issued_at!",
                  xml as "xml!", invoice_hash as "invoice_hash!",
                  currency as "currency!", tax as "tax!", gross as "gross!",
                  document #>> '{seller,name}' as "seller!",
                  document #>> '{seller,vat_number}' as "vat_number!"
             FROM proj_tax_sa.zatca_document
            WHERE status = 'pending'
              AND signed_xml IS NULL
              AND xml IS NOT NULL AND invoice_hash IS NOT NULL
            ORDER BY issued_at, id
            LIMIT $1"#,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency =
                CurrencyCode::new(&row.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            Ok(Unsigned {
                number: row.id,
                kind: row
                    .kind
                    .parse()
                    .map_err(|e: String| sqlx::Error::Decode(Box::new(std::io::Error::other(e))))?,
                issued_at: row.issued_at,
                xml: row.xml,
                invoice_hash: row.invoice_hash,
                seller: row.seller,
                vat_number: row.vat_number,
                total: crate::zatca::amount(Money::from_minor(row.gross, currency)),
                tax: crate::zatca::amount(Money::from_minor(row.tax, currency)),
            })
        })
        .collect()
}

/// One document, by its number.
pub async fn document(
    conn: &mut PgConnection,
    number: &str,
) -> Result<Option<Stored>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", source_id as "source_id!", kind as "kind!",
                  type_code as "type_code!", issued_at as "issued_at!",
                  currency as "currency!", net as "net!", tax as "tax!", gross as "gross!",
                  icv, previous_hash, invoice_hash, xml, qr, status as "status!",
                  signature, signed_xml, signed_at, stamped_xml, remarks, settled_at
             FROM proj_tax_sa.zatca_document WHERE id = $1"#,
        number,
    )
    .fetch_optional(&mut *conn)
    .await?;

    row.map(|row| {
        let currency =
            CurrencyCode::new(&row.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
        let decode = |s: String| -> Result<_, sqlx::Error> {
            s.parse::<Kind>()
                .map_err(|e| sqlx::Error::Decode(Box::new(std::io::Error::other(e))))
        };
        Ok(Stored {
            number: row.id,
            source: row.source_id,
            kind: decode(row.kind)?,
            type_code: row.type_code,
            issued_at: row.issued_at,
            currency,
            net: Money::from_minor(row.net, currency),
            tax: Money::from_minor(row.tax, currency),
            gross: Money::from_minor(row.gross, currency),
            icv: row.icv,
            previous_hash: row.previous_hash,
            invoice_hash: row.invoice_hash,
            xml: row.xml,
            qr: row.qr,
            signature: row.signature,
            signed_xml: row.signed_xml,
            signed_at: row.signed_at,
            status: row
                .status
                .parse()
                .map_err(|e: String| sqlx::Error::Decode(Box::new(std::io::Error::other(e))))?,
            stamped_xml: row.stamped_xml,
            remarks: row
                .remarks
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| sqlx::Error::Decode(Box::new(e)))?
                .unwrap_or_default(),
            settled_at: row.settled_at,
        })
    })
    .transpose()
}

/// Where the business stands, as of `now`.
///
/// `now` is a parameter rather than a clock reading for the reason everything
/// here takes one: a report that cannot be asked "and how did this look on the
/// last day of the quarter?" is a report somebody screenshots.
pub async fn standing(conn: &mut PgConnection, now: Timestamp) -> Result<Standing, sqlx::Error> {
    let registration = sqlx::query_scalar!(
        r#"SELECT count(*) as "count!" FROM proj_tax_sa.taxpayer WHERE id = 'self'"#
    )
    .fetch_one(&mut *conn)
    .await?;

    let counts = sqlx::query!(
        r#"SELECT status as "status!", count(*) as "count!"
             FROM proj_tax_sa.zatca_document GROUP BY status ORDER BY status"#
    )
    .fetch_all(&mut *conn)
    .await?;

    let window = Kind::Simplified
        .reporting_window()
        .unwrap_or_else(|| chrono::TimeDelta::hours(24));

    let late = sqlx::query!(
        r#"SELECT
             count(*) FILTER (
                 WHERE kind = 'simplified' AND issued_at < $1
             ) as "overdue!",
             count(*) FILTER (WHERE kind = 'standard') as "awaiting!",
             min(issued_at) as "oldest"
           FROM proj_tax_sa.zatca_document
          WHERE status = 'pending'"#,
        now - window,
    )
    .fetch_one(&mut *conn)
    .await?;

    let chain = sqlx::query_scalar!(
        r#"SELECT COALESCE(max(icv), 0) as "length!" FROM proj_tax_sa.zatca_document"#
    )
    .fetch_one(&mut *conn)
    .await?;

    let unsigned = sqlx::query_scalar!(
        r#"SELECT count(*) as "count!" FROM proj_tax_sa.zatca_document
            WHERE status = 'pending' AND signed_xml IS NULL"#
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(Standing {
        registered: registration > 0,
        counts: counts
            .into_iter()
            .map(|row| {
                Ok((
                    row.status.parse::<Status>().map_err(|e: String| {
                        sqlx::Error::Decode(Box::new(std::io::Error::other(e)))
                    })?,
                    row.count,
                ))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?,
        overdue: late.overdue,
        awaiting_clearance: late.awaiting,
        oldest_pending: late.oldest,
        chain_length: chain,
        unsigned,
    })
}

/// Every document, most recent first, one page at a time.
///
/// Keyset on `(issued_at, id)`. A busy shop issues thousands a month, so this
/// is the list here most likely to outgrow one response.
pub async fn documents(
    conn: &mut PgConnection,
    limit: i64,
    after: Option<&erp_types::Cursor>,
) -> Result<erp_types::Page<Stored>, sqlx::Error> {
    let (issued_at, id) = resume(after)?;
    let numbers = sqlx::query_scalar!(
        r#"SELECT id as "id!" FROM proj_tax_sa.zatca_document
            WHERE $2::timestamptz IS NULL OR (issued_at, id) < ($2, $3)
            ORDER BY issued_at DESC, id DESC LIMIT $1"#,
        limit,
        issued_at,
        id,
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut found = Vec::with_capacity(numbers.len());
    for number in numbers {
        if let Some(stored) = document(&mut *conn, &number).await? {
            found.push(stored);
        }
    }

    Ok(erp_types::Page::of(found, limit, |document| {
        erp_types::Cursor::over(&[&document.issued_at.to_rfc3339(), &document.number])
    }))
}

/// The `(issued_at, id)` a cursor resumes after. A cursor this build cannot
/// read is refused, not ignored.
fn resume(
    after: Option<&erp_types::Cursor>,
) -> Result<(Option<Timestamp>, Option<String>), sqlx::Error> {
    let Some(cursor) = after else {
        return Ok((None, None));
    };
    let malformed = || sqlx::Error::Decode(Box::new(erp_types::NotACursor));

    let issued_at = cursor
        .part(0)
        .ok_or_else(malformed)?
        .parse::<Timestamp>()
        .map_err(|_| malformed())?;
    let id = cursor.part(1).ok_or_else(malformed)?.to_owned();
    Ok((Some(issued_at), Some(id)))
}
