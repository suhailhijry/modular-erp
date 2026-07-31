use std::{sync::Arc, time::Duration};

use rand::RngExt;
use sqlx::PgPool;
use tokio::{sync::watch, task::JoinHandle};

use crate::event_sourcing::{
    CheckpointStore, EventStore, Projector, ReplayScope,
    projector::{AlertSink, RetryPolicy},
    replay_with_dead_letter,
};

pub struct ReactorRunnerConfig {
    pub batch_size: u64,
    pub max_attempts: i32,
    /// Upper bound of the random pre-query delay per wake-up. Spreads N
    /// reactors' queries across this window instead of firing them all
    /// in the same instant.
    pub max_jitter: Duration,
    /// Safety-net poll so a missed wake-up can never stall a reactor
    /// forever.
    pub fallback_interval: Duration,
}

impl Default for ReactorRunnerConfig {
    fn default() -> Self {
        Self {
            batch_size: 500,
            max_attempts: 5,
            max_jitter: Duration::from_millis(250),
            fallback_interval: Duration::from_secs(5),
        }
    }
}

pub fn run_independent_reactors(
    reactors: Vec<Arc<dyn Projector>>,
    store: Arc<dyn EventStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    pool: PgPool,
    alerts: Arc<dyn AlertSink>,
    config: ReactorRunnerConfig,
) -> (watch::Sender<()>, Vec<JoinHandle<()>>) {
    let (wake_tx, _) = watch::channel(());
    let config = Arc::new(config);

    let handles = reactors
        .into_iter()
        .map(|reactor| {
            let store = store.clone();
            let checkpoints = checkpoints.clone();
            let pool = pool.clone();
            let alerts = alerts.clone();
            let config = config.clone();
            let mut wake_rx = wake_tx.subscribe();

            tokio::spawn(async move {
                let policy = RetryPolicy { max_attempts: config.max_attempts };
                let mut fallback = tokio::time::interval(config.fallback_interval);
                loop {
                    // Jitter BEFORE querying: N reactors woken by the
                    // same signal hit the DB across a window, not at
                    // once.
                    let jitter = rand::rng().random_range(Duration::ZERO..config.max_jitter);
                    tokio::time::sleep(jitter).await;

                    if let Err(e) = replay_with_dead_letter(
                        store.as_ref(),
                        checkpoints.as_ref(),
                        reactor.as_ref(),
                        ReplayScope::Everything,
                        config.batch_size,
                        &pool,
                        alerts.as_ref(),
                        &policy,
                    )
                    .await
                    {
                        // Infrastructure failure (DB unreachable etc) -
                        // NOT a poison event, those are handled inside.
                        // Log and keep the pipeline alive; the next tick
                        // retries.
                        tracing::error!(reactor = reactor.name(), error = %e, "reactor pipeline pass failed, will retry on next wake-up");
                    }

                    tokio::select! {
                        changed = wake_rx.changed() => {
                            if changed.is_err() {
                                // Sender dropped: graceful shutdown.
                                tracing::info!(reactor = reactor.name(), "wake-up channel closed, reactor pipeline exiting");
                                break;
                            }
                        }
                        _ = fallback.tick() => {}
                    }
                }
            })
        })
        .collect();

    (wake_tx, handles)
}
