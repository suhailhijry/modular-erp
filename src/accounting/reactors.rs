use std::sync::Arc;

use sqlx::PgPool;

use crate::{
    accounting::{GeneralLedgerProjector, TrialBalanceProjector, lookup::LedgerAccountReactor},
    event_sourcing::Projector,
};

pub fn get_reactors(pool: PgPool) -> Vec<Arc<dyn Projector>> {
    vec![
        Arc::new(LedgerAccountReactor::new(pool.clone())),
        Arc::new(GeneralLedgerProjector::new(pool.clone())),
        Arc::new(TrialBalanceProjector::new(pool.clone())),
    ]
}
