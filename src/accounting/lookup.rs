use async_trait::async_trait;

use crate::{
    accounting::{AccountType, BalanceSide, LedgerAccount, LedgerAccountEvent},
    event_sourcing::{AggregateMeta, EventEnvelope, Projector, ProjectorMeta},
};

#[derive(ProjectorMeta)]
#[projector(name = "ledger_accounts")]
pub struct LedgerAccountReactor {
    pool: sqlx::PgPool,
}

impl LedgerAccountReactor {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Projector for LedgerAccountReactor {
    async fn handle(&self, envelope: &EventEnvelope) -> anyhow::Result<()> {
        if envelope.aggregate_domain != LedgerAccount::domain_name() {
            return Ok(());
        }

        let event: Option<LedgerAccountEvent> =
            serde_json::from_value(envelope.payload.clone()).ok();

        match event {
            Some(event) => {
                let mut tx = self.pool.begin().await?;
                match event {
                    LedgerAccountEvent::Created {
                        code,
                        name,
                        account_type,
                        normal,
                        parent_code,
                    } => {
                        sqlx::query!(
                            "
                            INSERT INTO ledger_accounts(account_code, name, account_type, normal, parent_code) VALUES($1, $2, $3, $4, $5)
                            ON CONFLICT (account_code)
                            DO UPDATE SET
                                name = EXCLUDED.name,
                                account_type = EXCLUDED.account_type,
                                parent_code = EXCLUDED.parent_code,
                                is_active = true
                            ",
                            code,
                            name,
                            account_type as AccountType,
                            normal as BalanceSide,
                            parent_code,
                        )
                        .execute(&mut *tx)
                        .await?;
                    }
                    LedgerAccountEvent::Renamed { new_name } => {
                        sqlx::query!(
                            "UPDATE ledger_accounts SET name = $1 WHERE account_code = $2",
                            new_name,
                            envelope.aggregate_id,
                        )
                        .execute(&mut *tx)
                        .await?;
                    }
                    LedgerAccountEvent::Deactivated => {
                        sqlx::query!(
                            "UPDATE ledger_accounts SET is_active = false WHERE account_code = $1",
                            envelope.aggregate_id,
                        )
                        .execute(&mut *tx)
                        .await?;
                    }
                    LedgerAccountEvent::Reactivated => {
                        sqlx::query!(
                            "UPDATE ledger_accounts SET is_active = true WHERE account_code = $1",
                            envelope.aggregate_id,
                        )
                        .execute(&mut *tx)
                        .await?;
                    }
                }
                tx.commit().await?;
                Ok(())
            }
            None => Ok(()),
        }
    }
}
