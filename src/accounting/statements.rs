use crate::accounting::JournalEntry;
use crate::accounting::journal_entry::{JournalEntryEvent, Side};
use crate::event_sourcing::*;
use async_trait::async_trait;
use sqlx::PgPool;

#[derive(ProjectorMeta)]
#[projector(name = "general_ledger")]
pub struct GeneralLedgerProjector {
    pool: PgPool,
}

impl GeneralLedgerProjector {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Projector for GeneralLedgerProjector {
    async fn handle(&self, envelope: &EventEnvelope) -> anyhow::Result<()> {
        if envelope.aggregate_domain != JournalEntry::domain_name() {
            return Ok(());
        }
        let event: JournalEntryEvent = serde_json::from_value(envelope.payload.clone())?;
        let JournalEntryEvent::Posted { date, lines } = event else {
            return Ok(());
        };

        let mut tx = self.pool.begin().await?;
        for (line_index, line) in lines.iter().enumerate() {
            let signed = match line.side {
                Side::Debit => line.amount,
                Side::Credit => -line.amount,
            };
            sqlx::query!(
                "INSERT INTO general_ledger (account_code, journal_entry_id, line_index, global_position, side, amount, signed, currency, entry_date)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                ON CONFLICT (account_code, journal_entry_id, line_index) DO NOTHING",
                line.account_code,
                envelope.aggregate_id,
                line_index as i32,
                envelope.id as i64,
                line.side as Side,
                line.amount,
                signed,
                line.currency,
                date,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// Trial balance: as-of any point in the replay, debit total should
/// equal credit total across ALL accounts. If it ever doesn't, something
/// upstream broke the balanced-entry invariant (or this projector has a
/// bug) - this table is as much an integrity check as it is a report.
#[derive(ProjectorMeta)]
#[projector(name = "trial_balance")]
pub struct TrialBalanceProjector {
    pool: PgPool,
}

impl TrialBalanceProjector {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Projector for TrialBalanceProjector {
    async fn handle(&self, envelope: &EventEnvelope) -> anyhow::Result<()> {
        if envelope.aggregate_domain != JournalEntry::domain_name() {
            return Ok(());
        }
        let event: JournalEntryEvent = serde_json::from_value(envelope.payload.clone())?;
        let JournalEntryEvent::Posted { lines, .. } = event else {
            return Ok(());
        };

        let mut tx = self.pool.begin().await?;
        let claim = sqlx::query!(
            "INSERT INTO processed_events (projector, global_position) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            self.name(),
            envelope.id as i64,
        )
        .execute(&mut *tx)
        .await?;

        if claim.rows_affected() == 0 {
            return Ok(()); // already processed - idempotent no-op
        }

        for line in lines {
            let (debit_delta, credit_delta) = match line.side {
                Side::Debit => (line.amount, 0),
                Side::Credit => (0, line.amount),
            };
            sqlx::query!(
                "INSERT INTO trial_balance (account_code, currency, debit_total, credit_total)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (account_code, currency)
                 DO UPDATE SET debit_total = trial_balance.debit_total + EXCLUDED.debit_total,
                               credit_total = trial_balance.credit_total + EXCLUDED.credit_total",
                line.account_code,
                line.currency,
                debit_delta,
                credit_delta,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
