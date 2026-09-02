//! What has been filed.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{CurrencyCode, Money, Timestamp};
use sqlx::PgConnection;

use crate::filing::FilingEvent;
use crate::onboarded::OnboardingEvent;

/// Filed returns — one group, and a small one.
///
/// It reads nothing from `proj_sales` or `proj_purchases` (architecture L3). The
/// numbers it holds were computed once, in the transaction that filed them, and
/// are in the event; recomputing them here would defeat the point of recording
/// what went to ZATCA.
#[derive(Debug)]
pub struct TaxSa;

impl ProjectionGroup for TaxSa {
    const NAME: &'static str = "tax_sa";
    const SCHEMA: &'static str = "proj_tax_sa";
}

#[derive(Debug)]
pub struct FiledReturns;

#[async_trait::async_trait]
impl Projection for FiledReturns {
    type Group = TaxSa;

    fn name(&self) -> &'static str {
        "filed_returns"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !FilingEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }

        let FilingEvent::Filed {
            from,
            until,
            currency,
            output_tax,
            input_tax,
            payable,
            filed_on,
            reference,
        } = ctx
            .decode::<FilingEvent>(envelope)
            .map_err(|source| ProjectionError::Decode {
                event_name: envelope.event_name.as_str().to_owned(),
                position: envelope.position,
                source,
            })?;

        sqlx::query(
            "INSERT INTO filed_return
                 (id, period_from, period_until, currency, output_tax, input_tax,
                  payable, filed_on, reference, recorded_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(envelope.stream.id.as_str())
        .bind(from)
        .bind(until)
        .bind(currency.as_str())
        .bind(output_tax.minor())
        .bind(input_tax.minor())
        .bind(payable.minor())
        .bind(filed_on)
        .bind(reference.as_deref())
        // The event's time, never the wall clock (L2).
        .bind(ctx.event_time())
        .execute(&mut *conn)
        .await?;

        Ok(())
    }
}

/// What ZATCA said, applied to the document it said it about.
///
/// The only projection here that *updates* rather than inserts, because the
/// document already exists — it was built from the invoice, and this is the
/// answer arriving later.
#[derive(Debug)]
pub struct Outcomes;

#[async_trait::async_trait]
impl Projection for Outcomes {
    type Group = TaxSa;

    fn name(&self) -> &'static str {
        "zatca_outcomes"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        use crate::clearance::ClearanceEvent;

        if !ClearanceEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }

        let event =
            ctx.decode::<ClearanceEvent>(envelope)
                .map_err(|source| ProjectionError::Decode {
                    event_name: envelope.event_name.as_str().to_owned(),
                    position: envelope.position,
                    source,
                })?;

        // A signature updates different columns from a verdict, and it can
        // arrive for a document that is still pending — so it is applied here
        // rather than folded into the status update below.
        if let crate::clearance::ClearanceEvent::Signed {
            document,
            signature,
            extensions,
            qr,
            at,
            ..
        } = &event
        {
            return signed(conn, document, signature, extensions, qr, *at).await;
        }

        let (document, status, remarks, stamped, at) = match &event {
            ClearanceEvent::Accepted {
                document,
                kind,
                warnings,
                stamped,
                at,
            } => (
                document,
                // The two obligations have two names for having been met, and
                // the difference is visible to a person reading the list.
                match kind {
                    crate::zatca::Kind::Standard => "cleared",
                    crate::zatca::Kind::Simplified => "reported",
                },
                serde_json::to_value(warnings).unwrap_or(serde_json::Value::Null),
                stamped.clone(),
                *at,
            ),
            ClearanceEvent::Refused {
                document,
                errors,
                at,
            } => (
                document,
                "refused",
                serde_json::to_value(errors).unwrap_or(serde_json::Value::Null),
                None,
                *at,
            ),
            // Handled above, and unreachable here.
            ClearanceEvent::Signed { .. } => return Ok(()),
        };

        sqlx::query(
            "UPDATE zatca_document
                SET status = $2, remarks = $3, stamped_xml = $4, settled_at = $5
              WHERE id = $1",
        )
        .bind(document)
        .bind(status)
        .bind(remarks)
        .bind(stamped)
        .bind(at)
        .execute(&mut *conn)
        .await?;

        Ok(())
    }
}

/// Every projection this module contributes.
#[must_use]
/// **How far onboarding has got**, so the status endpoint need not load an
/// aggregate to answer (law L7).
///
/// A renewal appends another `CsidIssued` rather than replacing the last, so the
/// aggregate grows without bound while the answer stays one row. `stage` keeps
/// the furthest reached rather than the most recent, because a production
/// certificate does not un-issue the compliance one.
#[derive(Debug)]
pub struct Onboardings;

#[async_trait::async_trait]
impl Projection for Onboardings {
    type Group = TaxSa;

    fn name(&self) -> &'static str {
        "onboarding"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !OnboardingEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }

        let OnboardingEvent::CsidIssued {
            stage,
            environment,
            serial,
            not_after,
            at,
            ..
        } = ctx
            .decode::<OnboardingEvent>(envelope)
            .map_err(|source| ProjectionError::Decode {
                event_name: envelope.event_name.as_str().to_owned(),
                position: envelope.position,
                source,
            })?;

        // `GREATEST` on the stage keeps the furthest reached. The two values
        // order correctly as text — `compliance` < `production` — which is
        // luck, so the CHECK on the column is what stops a third stage relying
        // on it silently.
        sqlx::query(
            "INSERT INTO onboarding
                 (id, stage, environment, serial, not_after, issued_at, recorded_at)
             VALUES ('self', $1, $2, $3, $4, $5, $6)
             ON CONFLICT (id) DO UPDATE
                SET stage       = GREATEST(onboarding.stage, EXCLUDED.stage),
                    environment = EXCLUDED.environment,
                    serial      = EXCLUDED.serial,
                    not_after   = EXCLUDED.not_after,
                    issued_at   = EXCLUDED.issued_at,
                    recorded_at = EXCLUDED.recorded_at",
        )
        .bind(stage.as_str())
        .bind(environment.as_str())
        .bind(serial)
        .bind(not_after)
        .bind(at)
        .bind(ctx.event_time())
        .execute(&mut *conn)
        .await?;

        Ok(())
    }
}

pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = TaxSa>>> {
    vec![
        std::sync::Arc::new(FiledReturns),
        // **Before the documents**, so an invoice issued in the same batch as
        // the registration is rendered under it rather than filed as
        // unregistered. Within one event the projections run in this order; the
        // registration event still has to come first in the log.
        std::sync::Arc::new(crate::documents::Taxpayers),
        std::sync::Arc::new(crate::documents::ZatcaDocuments),
        std::sync::Arc::new(Outcomes),
        std::sync::Arc::new(Onboardings),
    ]
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// A return as it was filed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiledReturn {
    pub period: String,
    pub from: Timestamp,
    pub until: Timestamp,
    pub output_tax: Money,
    pub input_tax: Money,
    pub payable: Money,
    pub filed_on: Timestamp,
    /// ZATCA's acknowledgement, once clearance produces one.
    pub reference: Option<String>,
}

/// Everything filed, most recent period first.
pub async fn filed(conn: &mut PgConnection, limit: i64) -> Result<Vec<FiledReturn>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", period_from as "period_from!", period_until as "period_until!",
                  currency as "currency!", output_tax as "output_tax!",
                  input_tax as "input_tax!", payable as "payable!",
                  filed_on as "filed_on!", reference
             FROM proj_tax_sa.filed_return
            ORDER BY period_from DESC, id
            LIMIT $1"#,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency =
                CurrencyCode::new(&row.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            Ok(FiledReturn {
                period: row.id,
                from: row.period_from,
                until: row.period_until,
                output_tax: Money::from_minor(row.output_tax, currency),
                input_tax: Money::from_minor(row.input_tax, currency),
                payable: Money::from_minor(row.payable, currency),
                filed_on: row.filed_on,
                reference: row.reference,
            })
        })
        .collect()
}

/// Applies a signature: the document as submitted, and the QR with the stamp.
///
/// The submitted XML is **rendered here** from the stored document and the
/// recorded extensions, rather than carried in the event. The event holds what
/// could not be derived — the signature and what it covers — and the rest is
/// the same deterministic render that produced the hashed bytes in the first
/// place, so a rebuild reproduces it exactly.
async fn signed(
    conn: &mut PgConnection,
    document: &str,
    signature: &str,
    extensions: &str,
    qr: &str,
    at: Timestamp,
) -> Result<(), ProjectionError> {
    // projection-read: `zatca_document`, written by this projection when the
    // document was built. The signature event carries what could not be derived
    // — the stamp and what it covers — and the document it applies to is what
    // this group already holds.
    let row: Option<(Option<serde_json::Value>,)> =
        sqlx::query_as("SELECT document FROM zatca_document WHERE id = $1")
            .bind(document)
            .fetch_optional(&mut *conn)
            .await?;

    // A signature for a document this projection has not built is not something
    // to guess at — but it is also not a reason to stop the group, because the
    // event is in the log and the document is not. It can only happen if a
    // document was signed and then the invoice it came from stopped producing
    // one, which no code path does.
    let Some(stored) = row.and_then(|(value,)| value) else {
        return Ok(());
    };

    let stored: crate::zatca::Document = serde_json::from_value(stored)
        .map_err(|e| ProjectionError::Rejected(format!("a stored document cannot be read: {e}")))?;

    let xml = crate::zatca::ubl::signed(&stored, &crate::zatca::ubl::Enveloped { extensions, qr })
        .map_err(|e| ProjectionError::Rejected(format!("{document} cannot be rendered: {e}")))?;

    sqlx::query(
        "UPDATE zatca_document
            SET signature = $2, signed_xml = $3, qr = $4, signed_at = $5
          WHERE id = $1",
    )
    .bind(document)
    .bind(signature)
    .bind(&xml)
    .bind(qr)
    .bind(at)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// What onboarding has reached, as one row.
#[derive(Debug, Clone)]
pub struct Onboarded {
    pub stage: String,
    pub environment: String,
    pub serial: String,
    pub not_after: String,
    pub issued_at: Timestamp,
}

/// The onboarding row, or `None` before any certificate has been issued.
///
/// This is what the status endpoint reads. It used to load the `Onboarding`
/// aggregate instead, which made the log a query engine for one screen (L7).
pub async fn onboarding(conn: &mut PgConnection) -> Result<Option<Onboarded>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT stage as "stage!", environment as "environment!", serial as "serial!",
                  not_after as "not_after!", issued_at as "issued_at!"
             FROM proj_tax_sa.onboarding
            WHERE id = 'self'"#,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|r| Onboarded {
        stage: r.stage,
        environment: r.environment,
        serial: r.serial,
        not_after: r.not_after,
        issued_at: r.issued_at,
    }))
}
