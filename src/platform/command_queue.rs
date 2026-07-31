//! A bounded queue sitting between axum handlers and `dispatch`. Two
//! goals:
//!
//! 1. BACKPRESSURE: instead of every concurrent request hitting Postgres
//!    directly (and everything falling over together once the DB is
//!    saturated), requests queue up to a fixed capacity. Past that,
//!    return 503 immediately instead of piling up unboundedly.
//!
//! 2. PER-AGGREGATE SERIALIZATION: commands are sharded by aggregate id
//!    (hash(id) % worker_count) so every command for the SAME aggregate
//!    always lands on the same worker and runs strictly one-at-a-time.
//!    This eliminates most optimistic-concurrency conflicts under
//!    contention (a hot account being hit by many concurrent requests)
//!    without needing the retry loop in `handle_command` to do much
//!    work - different aggregates still process fully in parallel across
//!    workers.
//!
//! Job type is boxed/type-erased since different endpoints dispatch
//! different Aggregate types onto the same queue.

use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use tokio::sync::{mpsc, oneshot};

type Job = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

#[derive(Clone)]
pub struct CommandQueue {
    shards: Vec<mpsc::Sender<Job>>,
}

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("queue is at capacity, try again shortly")]
    Full,
    #[error("worker dropped the response channel")]
    WorkerGone,
}

impl CommandQueue {
    /// `shard_count` workers, each processing its own FIFO queue of
    /// `per_shard_capacity` pending jobs (total capacity =
    /// shard_count * per_shard_capacity).
    pub fn new(shard_count: usize, per_shard_capacity: usize) -> Self {
        let shards = (0..shard_count)
             .map(|_| {
                 let (tx, mut rx) = mpsc::channel::<Job>(per_shard_capacity);
                 tokio::spawn(async move {
                     // One job at a time, strictly in arrival order, for
                     // this shard - this IS the per-aggregate
                     // serialization guarantee.
                     while let Some(job) = rx.recv().await {
                         // Each job runs in its OWN task, awaited to
                         // completion (so ordering is preserved), purely
                         // for panic isolation: a panicking handler
                         // poisons only its own JoinHandle. Without this,
                         // a single panic unwinds the shard worker itself
                         // and every aggregate hashed to this shard gets
                         // WorkerGone forever - silently.
                         let handle = tokio::spawn(job());
                         if let Err(join_err) = handle.await {
                             if join_err.is_panic() {
                                 tracing::error!("command handler panicked - shard continues; the submitter sees WorkerGone for this one job only");
                             }
                         }
                     }
                 });
                 tx
             })
             .collect();
        Self { shards }
    }

    fn shard_for(&self, key: &str) -> &mpsc::Sender<Job> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        let idx = (hasher.finish() as usize) % self.shards.len();
        &self.shards[idx]
    }

    /// `key` should be the aggregate id (or aggregate_type+id if ids
    /// aren't globally unique across types) - this is what determines
    /// which commands get serialized against each other.
    pub async fn submit<F, Fut, T>(&self, key: &str, work: F) -> Result<T, QueueError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job: Job = Box::new(move || {
            Box::pin(async move {
                let result = work().await;
                let _ = tx.send(result);
            })
        });

        // try_send, not send().await: fail fast when full rather than
        // holding the HTTP request open indefinitely behind an already
        // saturated shard.
        self.shard_for(key)
            .try_send(job)
            .map_err(|_| QueueError::Full)?;

        rx.await.map_err(|_| QueueError::WorkerGone)
    }
}
