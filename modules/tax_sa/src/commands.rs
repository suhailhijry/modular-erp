//! Filing a return.

use erp_control::{CommandError, TenantDb};
use erp_eventlog::{Committed, Decision, ExecuteError, MAX_ATTEMPTS, Metadata, try_execute};
use erp_types::{AggregateId, CurrencyCode, Money, StreamId, Timestamp};

use crate::filing::{Filing, FilingEvent};
use crate::report::{Return, Sides};

#[derive(Debug, thiserror::Error)]
pub enum TaxError {
    #[error("a period must end after it starts")]
    EmptyPeriod,
    #[error("the period {period} was already filed on {on}")]
    AlreadyFiled { period: String, on: Timestamp },
    #[error("{0} cannot be used as a period identifier")]
    InvalidPeriod(String),
    #[error(transparent)]
    Registration(#[from] crate::taxpayer::InvalidRegistration),
    #[error("{0} cannot be used as a document identifier")]
    InvalidDocument(String),
    #[error(transparent)]
    Read(#[from] sqlx::Error),
}

impl erp_i18n::Localize for TaxError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::EmptyPeriod => Message::new(messages::EMPTY_PERIOD),
            Self::AlreadyFiled { period, on } => Message::new(messages::ALREADY_FILED)
                .with("period", MessageArg::text(period.clone()))
                .with("on", MessageArg::text(on.to_rfc3339())),
            Self::InvalidPeriod(period) => Message::new(messages::INVALID_PERIOD)
                .with("period", MessageArg::text(period.clone())),
            // The registration's own error says which field and why, and it is
            // the only thing that knows — so it is passed through as text rather
            // than flattened into a code per field.
            Self::Registration(reason) => Message::new(messages::INVALID_REGISTRATION)
                .with("reason", MessageArg::text(reason.to_string())),
            Self::InvalidDocument(document) => Message::new(messages::INVALID_DOCUMENT)
                .with("document", MessageArg::text(document.clone())),
            // Ours: the read models are unwell, not something a user did.
            Self::Read(_) => Message::new(erp_eventlog::messages::INTERNAL),
        }
    }
}

/// A period, as an aggregate id: `SAR.2026-01-01.2026-04-01`.
///
/// **The period is the identity**, which is what makes filing one twice a
/// conflict rather than a second return. A currency is part of it because a
/// business invoicing in two files two.
pub fn period_id(
    currency: CurrencyCode,
    from: Timestamp,
    until: Timestamp,
) -> Result<AggregateId, TaxError> {
    let raw = format!(
        "{}.{}.{}",
        currency,
        from.format("%Y-%m-%d"),
        until.format("%Y-%m-%d")
    );
    AggregateId::new(&raw).map_err(|_| TaxError::InvalidPeriod(raw))
}

/// What a filing produced.
#[derive(Debug)]
pub struct Filed {
    pub committed: Committed<FilingEvent>,
    /// What was declared. On a repeat this is what the **existing** filing says,
    /// not what the system would compute today — which is the difference between
    /// telling a caller what went to ZATCA and telling them what it thinks now.
    pub payable: Money,
    pub filed_on: Timestamp,
}

/// Records that a period was filed, with the numbers that went.
///
/// Filing the same period twice is a **conflict**, not a no-op: a second filing
/// is an amendment, and an amendment is a different document with its own rules.
/// The error carries the date of the one that exists so a caller can say so.
pub async fn file_return(
    db: &TenantDb,
    sides: Sides,
    currency: CurrencyCode,
    from: Timestamp,
    until: Timestamp,
    filed_on: Timestamp,
    metadata: &Metadata,
) -> Result<Filed, CommandError<TaxError>> {
    if until <= from {
        return Err(rejected(TaxError::EmptyPeriod));
    }
    let id = period_id(currency, from, until).map_err(rejected)?;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match file_in(
            &mut tx, &id, sides, currency, from, until, filed_on, metadata,
        )
        .await
        {
            Ok(filed) => {
                tx.commit().await.map_err(ExecuteError::from)?;
                return Ok(filed);
            }
            Err(e) if e.is_conflict() => {
                tx.rollback().await.map_err(ExecuteError::from)?;
            }
            Err(e) => {
                tx.rollback().await.map_err(ExecuteError::from)?;
                return Err(e.into());
            }
        }
    }

    Err(ExecuteError::Contended {
        stream: StreamId::new(<Filing as erp_eventlog::Aggregate>::domain(), id),
        attempts: MAX_ATTEMPTS,
    }
    .into())
}

#[expect(
    clippy::too_many_arguments,
    reason = "a period is four values and a filing is two more; bundling them would be a struct nobody else needs"
)]
async fn file_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    sides: Sides,
    currency: CurrencyCode,
    from: Timestamp,
    until: Timestamp,
    filed_on: Timestamp,
    metadata: &Metadata,
) -> Result<Filed, ExecuteError<TaxError>> {
    // **Computed in this transaction**, so the numbers recorded are the numbers
    // that were true when the filing committed. Reading them outside would let a
    // write land in between and file a figure that was never current — the same
    // argument `sales` makes about posting accounts and the VAT rate.
    let declared = crate::report::vat_return(&mut *conn, sides, currency, from, until)
        .await
        .map_err(|e| ExecuteError::Rejected(TaxError::Read(e)))?;

    let existing = erp_eventlog::load::<Filing>(&mut *conn, id, crate::upcasters())
        .await?
        .aggregate;
    if existing.filed {
        return Err(ExecuteError::Rejected(TaxError::AlreadyFiled {
            period: id.as_str().to_owned(),
            on: existing.filed_on.unwrap_or(filed_on),
        }));
    }

    let event = event_for(&declared, filed_on);
    let committed = try_execute::<Filing, _, TaxError>(
        &mut *conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            if loaded.aggregate.filed {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(event.clone()))
        },
    )
    .await?;

    Ok(Filed {
        committed,
        payable: declared.payable,
        filed_on,
    })
}

fn event_for(declared: &Return, filed_on: Timestamp) -> FilingEvent {
    FilingEvent::Filed {
        from: declared.from,
        until: declared.until,
        currency: declared.currency,
        output_tax: declared.output.tax,
        input_tax: declared.input.tax,
        payable: declared.payable,
        filed_on,
        reference: None,
    }
}

fn rejected(error: TaxError) -> CommandError<TaxError> {
    CommandError::Execute(ExecuteError::Rejected(error))
}

/// The `Send` guard. See `sales::commands` for why this lives here.
const _: fn() = || {
    fn assert_send<T: Send>(_: T) {}
    fn commands_are_send(
        db: &TenantDb,
        sides: Sides,
        currency: CurrencyCode,
        at: Timestamp,
        metadata: &Metadata,
    ) {
        assert_send(file_return(db, sides, currency, at, at, at, metadata));
    }
    let _ = commands_are_send;
};

// ---------------------------------------------------------------------------
// ZATCA
// ---------------------------------------------------------------------------

/// Records the business's ZATCA registration, from here on.
///
/// Every document rendered after this carries it; nothing already rendered
/// changes, which is the point — see [`crate::taxpayer`]. Registering again is a
/// correction and a new event, not an edit.
///
/// Rejects anything ZATCA would reject, because the alternative is finding out
/// at clearance: a standard invoice cannot be given to the buyer until it is
/// cleared, so a bad VAT number stops a sale rather than producing a warning.
pub async fn register_taxpayer(
    db: &TenantDb,
    registration: crate::taxpayer::Registration,
    on: Timestamp,
    metadata: &Metadata,
) -> Result<Committed<crate::taxpayer::TaxpayerEvent>, CommandError<TaxError>> {
    registration
        .check()
        .map_err(|e| rejected(TaxError::Registration(e)))?;

    db.execute::<crate::taxpayer::Taxpayer, _, TaxError>(
        &crate::taxpayer::taxpayer_id(),
        crate::upcasters(),
        metadata,
        |loaded| {
            // Registering the same details twice writes nothing. A retried
            // request is not a correction.
            if loaded.aggregate.registration.as_ref() == Some(&registration) {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(crate::taxpayer::TaxpayerEvent::Registered {
                registration: registration.clone(),
                on,
            }))
        },
    )
    .await
}

/// Records what ZATCA decided about one document.
///
/// Called by whatever submitted it, **after** the call returned. Only a verdict
/// reaches here: a timeout or an expired certificate is not a decision about the
/// document and appends nothing, so the document stays pending and the next
/// sweep tries again.
///
/// Recording the same verdict twice writes nothing, which is what makes a
/// submitter that crashed between the call and the append safe to re-run.
pub async fn record_outcome(
    db: &TenantDb,
    document: &str,
    kind: crate::zatca::Kind,
    verdict: &crate::zatca::wire::Verdict,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<Committed<crate::clearance::ClearanceEvent>, CommandError<TaxError>> {
    let id = AggregateId::new(document)
        .map_err(|_| rejected(TaxError::InvalidDocument(document.to_owned())))?;

    let event = match verdict {
        crate::zatca::wire::Verdict::Accepted { warnings, stamped } => {
            crate::clearance::ClearanceEvent::Accepted {
                document: document.to_owned(),
                kind,
                warnings: warnings.clone(),
                stamped: stamped.clone(),
                at,
            }
        }
        crate::zatca::wire::Verdict::Refused { errors } => {
            crate::clearance::ClearanceEvent::Refused {
                document: document.to_owned(),
                errors: errors.clone(),
                at,
            }
        }
    };

    db.execute::<crate::clearance::Clearance, _, TaxError>(
        &id,
        crate::upcasters(),
        metadata,
        |loaded| {
            if loaded.aggregate.settled {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(event.clone()))
        },
    )
    .await
}

/// Records that ZATCA issued a certificate for this tenant's unit.
///
/// Called by [`crate::zatca::onboarding`] after the call returned and the
/// certificate was checked against the key. Nothing secret reaches here: the
/// subject, the serial and the validity are all on a certificate anybody who
/// receives an invoice can read.
pub(crate) async fn record_csid(
    db: &TenantDb,
    issued: &crate::zatca::onboarding::Issued,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<Committed<crate::onboarded::OnboardingEvent>, CommandError<TaxError>> {
    let event = crate::onboarded::OnboardingEvent::CsidIssued {
        stage: issued.stage,
        environment: issued.environment,
        request_id: issued.request_id.clone(),
        subject: issued.subject.clone(),
        serial: issued.serial.clone(),
        not_before: issued.not_before.clone(),
        not_after: issued.not_after.clone(),
        at,
    };

    db.execute::<crate::onboarded::Onboarding, _, TaxError>(
        &crate::onboarded::onboarding_id(),
        crate::upcasters(),
        metadata,
        |loaded| {
            // The same certificate recorded twice writes nothing, which is what
            // makes an onboarding that crashed after the call safe to re-run.
            if loaded.aggregate.serial.as_deref() == Some(issued.serial.as_str())
                && loaded.aggregate.stage == Some(issued.stage)
            {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(event.clone()))
        },
    )
    .await
}

/// Records the signature made over one document.
///
/// Appending the same document's signature twice writes nothing: ZATCA holds
/// the first one, and a second signature over the same invoice would be a
/// second document as far as verification is concerned.
pub(crate) async fn record_signature(
    db: &TenantDb,
    document: &str,
    signature: &crate::zatca::signing::Signature,
    qr: &str,
    certificate_serial: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<Committed<crate::clearance::ClearanceEvent>, CommandError<TaxError>> {
    let id = AggregateId::new(document)
        .map_err(|_| rejected(TaxError::InvalidDocument(document.to_owned())))?;

    let event = crate::clearance::ClearanceEvent::Signed {
        document: document.to_owned(),
        signature: signature.value.clone(),
        extensions: signature.extensions.clone(),
        qr: qr.to_owned(),
        certificate_serial: certificate_serial.to_owned(),
        at,
    };

    db.execute::<crate::clearance::Clearance, _, TaxError>(
        &id,
        crate::upcasters(),
        metadata,
        |loaded| {
            if loaded.aggregate.signed {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(event.clone()))
        },
    )
    .await
}
