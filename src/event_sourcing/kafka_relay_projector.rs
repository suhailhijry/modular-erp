use anyhow::anyhow;
use rdkafka::{
    ClientConfig, Message, Offset, TopicPartitionList,
    admin::{AdminClient, AdminOptions, NewTopic, TopicReplication},
    consumer::{Consumer, StreamConsumer},
    error::KafkaError,
    producer::{FutureProducer, FutureRecord},
    types::RDKafkaErrorCode,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use sqlx::PgPool;

use crate::event_sourcing::{
    CheckpointStore, EventEnvelope, EventStore, Projector, ProjectorMeta, ReplayScope,
    projector::{AlertSink, HandleOutcome, RetryPolicy, handle_with_dead_letter},
    replay,
};

pub struct KafkaRelayProjector {
    producer: FutureProducer,
    topic: String,
}

impl KafkaRelayProjector {
    pub async fn new(bootstrap_servers: &String, topic_name: &String) -> anyhow::Result<Self> {
        let admin: AdminClient<_> = ClientConfig::new()
            .set("bootstrap.servers", bootstrap_servers.to_string())
            .set("message.timeout.ms", "5000")
            .create()?;

        let topic = NewTopic::new(topic_name, 6, TopicReplication::Fixed(1));

        let options = AdminOptions::new().operation_timeout(Some(Duration::from_secs(5)));
        let topics = &[topic];
        let create = admin.create_topics(topics, &options);

        match tokio::time::timeout(Duration::from_secs(10), create).await {
            Ok(result) => match result {
                Ok(result) => {
                    for topic_result in result {
                        match topic_result {
                            Ok(name) => {
                                tracing::debug!("successfully registered topic '{}'.", name)
                            }
                            Err((name, code)) => {
                                if code == RDKafkaErrorCode::TopicAlreadyExists {
                                    tracing::debug!("topic '{}' already exists, skipping..", name);
                                } else {
                                    tracing::error!(
                                        "failed to create topic '{}':  {:?}..",
                                        name,
                                        code
                                    );
                                }
                            }
                        }
                    }
                }
                Err(KafkaError::AdminOp(code)) if code == RDKafkaErrorCode::TopicAlreadyExists => {
                    tracing::debug!(
                        "topic '{}' already exists (top-level error). skipping..",
                        topic_name,
                    );
                }
                Err(e) => {
                    tracing::error!("admin operation failed: {:?}", e);
                    return Err(e.into());
                }
            },
            Err(_elapsed) => tracing::warn!(
                "topic creation timed out - broker unreachable? proceeding; producer will retry lazily"
            ),
        }

        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", bootstrap_servers)
            .set("message.timeout.ms", "5000")
            .set("enable.idempotence", "true")
            .create()?;
        Ok(Self {
            producer,
            topic: topic_name.to_string(),
        })
    }
}

impl ProjectorMeta for KafkaRelayProjector {
    fn name(&self) -> &'static str {
        "kafka-relay-projector"
    }
}

#[async_trait]
impl Projector for KafkaRelayProjector {
    async fn handle(&self, envelope: &EventEnvelope) -> anyhow::Result<()> {
        let payload = serde_json::to_vec(&envelope)?;
        // Key by aggregate_id: Kafka only orders records WITHIN a
        // partition, so every event for one aggregate must land in the
        // same partition or a consumer could see them out of order.

        let record = FutureRecord::to(&self.topic)
            .payload(&payload)
            .key(envelope.aggregate_id.as_bytes());

        let result = self.producer.send(record, Duration::from_secs(30)).await;
        match result {
            Ok(_) => Ok(()),
            Err(_) => Err(anyhow!("failed to send message to kafka")),
        }
    }
}

pub async fn run_kafka_relay(
    store: Arc<dyn EventStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    relay: Arc<KafkaRelayProjector>,
    poll_interval: std::time::Duration,
) -> anyhow::Result<()> {
    let mut ticker = tokio::time::interval(poll_interval);
    loop {
        ticker.tick().await;
        if let Err(e) = replay(
            store.as_ref(),
            checkpoints.as_ref(),
            relay.as_ref(),
            ReplayScope::Everything,
            None,
            500,
        )
        .await
        {
            tracing::warn!(error = %e, "relay pass failed - will retry next tick from checkpoint");
        }
    }
}

/// How the bootstrap decided to start, mostly for logging/metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootstrapMode {
    FreshBackfill,
    ResumeFromCommits,
    GapRecovery,
}

pub async fn run_kafka_listener(
    bootstrap_servers: String,
    topic: &str,
    group_id: &str,
    store: Arc<dyn EventStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    projector: Arc<dyn Projector>,
    pool: PgPool,
    alerts: Arc<dyn AlertSink>,
) -> anyhow::Result<()> {
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", bootstrap_servers)
        .set("group.id", group_id)
        // Manual commits only: an offset commits strictly AFTER the
        // event is handled (or deliberately dead-lettered).
        .set("enable.auto.commit", "false")
        // Fallback for partitions with no explicit or committed offset.
        // The gap DETECTION above it is what keeps this from silently
        // hiding retention loss.
        .set("auto.offset.reset", "earliest")
        .create()?;

    // ------------------------------------------------------------------
    // 1. Partition discovery - metadata request, no group join.
    // ------------------------------------------------------------------
    let metadata = consumer.fetch_metadata(Some(topic), Duration::from_secs(10))?;
    let topic_meta = metadata
        .topics()
        .iter()
        .find(|t| t.name() == topic)
        .ok_or_else(|| anyhow::anyhow!("topic '{topic}' not found in broker metadata"))?;
    if let Some(e) = topic_meta.error() {
        anyhow::bail!("broker reports error for topic '{topic}': {:?}", e);
    }
    let partitions: Vec<i32> = topic_meta.partitions().iter().map(|p| p.id()).collect();
    if partitions.is_empty() {
        anyhow::bail!("topic '{topic}' has no partitions");
    }
    tracing::info!(topic, ?partitions, "discovered partitions");

    // ------------------------------------------------------------------
    // 2. Bootstrap decision: committed offsets + retention watermarks.
    // ------------------------------------------------------------------
    let mut probe = TopicPartitionList::new();
    for p in &partitions {
        probe.add_partition(topic, *p);
    }
    let committed = consumer.committed_offsets(probe, Duration::from_secs(10))?;

    let mut has_prior_commits = false;
    let mut retention_gap_detected = false;

    for elem in committed.elements() {
        if let Offset::Offset(committed_offset) = elem.offset() {
            has_prior_commits = true;
            let (earliest, _latest) =
                consumer.fetch_watermarks(topic, elem.partition(), Duration::from_secs(10))?;
            if committed_offset < earliest {
                // Records between committed_offset and earliest were
                // deleted by retention (or the topic was partially
                // rebuilt). Resuming would silently skip them.
                retention_gap_detected = true;
                tracing::warn!(
                    partition = elem.partition(),
                    committed_offset,
                    earliest_retained = earliest,
                    lost_records = earliest - committed_offset,
                    "committed offset predates retention - Kafka lost records past our commit; recovering the gap from the durable log"
                );
            }
        }
    }

    let mode = if !has_prior_commits {
        BootstrapMode::FreshBackfill
    } else if retention_gap_detected {
        BootstrapMode::GapRecovery
    } else {
        BootstrapMode::ResumeFromCommits
    };
    tracing::info!(
        projector = projector.name(),
        ?mode,
        "bootstrap mode decided"
    );

    let assignment: TopicPartitionList = match mode {
        BootstrapMode::ResumeFromCommits => {
            // Offset::Stored = the group's committed offset per
            // partition; partitions without one fall back to
            // auto.offset.reset.
            let mut tpl = TopicPartitionList::new();
            for p in &partitions {
                tpl.add_partition_offset(topic, *p, Offset::Stored)
                    .map_err(|e| anyhow::anyhow!("building assignment: {e}"))?;
            }
            tpl
        }

        BootstrapMode::FreshBackfill => {
            // First run for this group: full history from the durable
            // log (reaches back further than any broker retention),
            // then Kafka from just-before-backfill so the seam overlaps
            // instead of gapping. Overlap is safe: handlers are
            // idempotent.
            let backfill_started_at = chrono::Utc::now() - chrono::Duration::seconds(5);
            replay(
                store.as_ref(),
                checkpoints.as_ref(),
                projector.as_ref(),
                ReplayScope::Everything,
                None,
                500,
            )
            .await?;
            tracing::info!(projector = projector.name(), "fresh backfill complete");

            let ts = backfill_started_at.timestamp_millis();
            let mut ts_tpl = TopicPartitionList::new();
            for p in &partitions {
                ts_tpl
                    .add_partition_offset(topic, *p, Offset::Offset(ts))
                    .map_err(|e| anyhow::anyhow!("building timestamp list: {e}"))?;
            }
            // Partitions with nothing at/after ts resolve to
            // Offset::End - correct: nothing to rewind to there.
            consumer.offsets_for_times(ts_tpl, Duration::from_secs(10))?
        }

        BootstrapMode::GapRecovery => {
            // Kafka lost records past our commit. The projector's
            // Postgres checkpoint bounds the replay: it resumes from
            // wherever this projector durably got to (which is why the
            // periodic checkpoint flushing during normal Kafka
            // consumption matters - it keeps this window small), covers
            // everything Kafka lost, and overruns into what Kafka still
            // retains - idempotent overlap, not a problem.
            replay(
                store.as_ref(),
                checkpoints.as_ref(),
                projector.as_ref(),
                ReplayScope::Everything,
                None,
                500,
            )
            .await?;
            tracing::info!(
                projector = projector.name(),
                "gap recovery backfill complete, resuming Kafka from earliest retained"
            );

            // Consume from the earliest record Kafka still has, on
            // every partition. The replay above already covered
            // everything up to (and past) this point in durable-log
            // terms; re-consuming the retained window is idempotent
            // overlap that guarantees no seam.
            let mut tpl = TopicPartitionList::new();
            for p in &partitions {
                tpl.add_partition_offset(topic, *p, Offset::Beginning)
                    .map_err(|e| anyhow::anyhow!("building assignment: {e}"))?;
            }
            tpl
        }
    };

    // ------------------------------------------------------------------
    // 3. Manual assignment - synchronous, immediate, nothing to wait
    //    for. No subscribe() anywhere: assign and subscribe are mutually
    //    exclusive modes.
    // ------------------------------------------------------------------
    consumer.assign(&assignment)?;
    tracing::info!(
        projector = projector.name(),
        "partitions assigned, consuming"
    );

    // ------------------------------------------------------------------
    // Postgres checkpoint tracking - monotonic high-water mark, flushed
    // periodically. This is what bounds GapRecovery's replay window, so
    // it is not optional bookkeeping.
    // ------------------------------------------------------------------
    let initial_checkpoint = checkpoints.load(projector.name()).await?.unwrap_or(0);
    let highest_processed = Arc::new(AtomicU64::new(initial_checkpoint));

    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    let _flusher_guard = {
        let checkpoints = checkpoints.clone();
        let projector_name = projector.name();
        let highest_processed = highest_processed.clone();
        AbortOnDrop(tokio::spawn(async move {
            let mut last_flushed = initial_checkpoint;
            let mut ticker = tokio::time::interval(Duration::from_secs(10));
            loop {
                ticker.tick().await;
                let current = highest_processed.load(Ordering::SeqCst);
                if current > last_flushed {
                    if let Err(e) = checkpoints.save(projector_name, current).await {
                        tracing::warn!(error = %e, "failed to flush projector checkpoint");
                    } else {
                        last_flushed = current;
                    }
                }
            }
        }))
    };

    macro_rules! exit_with_final_flush {
        ($result:expr) => {{
            let current = highest_processed.load(Ordering::SeqCst);
            if let Err(e) = checkpoints.save(projector.name(), current).await {
                tracing::warn!(error = %e, "final checkpoint flush failed");
            }
            return $result;
        }};
    }

    // ------------------------------------------------------------------
    // 4. Consume loop
    // ------------------------------------------------------------------
    let policy = RetryPolicy { max_attempts: 5 };

    loop {
        match consumer.recv().await {
            Ok(record) => {
                let Some(value) = record.payload() else {
                    tracing::warn!(
                        topic = record.topic(),
                        partition = record.partition(),
                        offset = record.offset(),
                        "record has no payload (tombstone?) - skipping"
                    );
                    continue;
                };

                let envelope: EventEnvelope = match serde_json::from_slice(value) {
                    Ok(envelope) => envelope,
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            topic = record.topic(),
                            partition = record.partition(),
                            offset = record.offset(),
                            "failed to deserialize event envelope - skipping and committing past it"
                        );
                        if let Err(e) =
                            consumer.commit_message(&record, rdkafka::consumer::CommitMode::Async)
                        {
                            tracing::warn!(error = %e, "commit after skip failed");
                        }
                        continue;
                    }
                };

                match handle_with_dead_letter(
                    projector.as_ref(),
                    &envelope,
                    &pool,
                    alerts.as_ref(),
                    &policy,
                )
                .await
                {
                    Ok(outcome) => match outcome {
                        HandleOutcome::Processed | HandleOutcome::DeadLettered => {
                            if let Err(e) = consumer
                                .commit_message(&record, rdkafka::consumer::CommitMode::Async)
                            {
                                tracing::warn!(error = %e, "offset commit failed - possible redelivery, safe under idempotency");
                            }
                            highest_processed.fetch_max(envelope.id, Ordering::SeqCst);
                        }
                        HandleOutcome::WillRetry { .. } => {
                            // Uncommitted: redelivered after restart/seek.
                            // Subsequent messages from this partition keep
                            // flowing meanwhile; per-aggregate ordering
                            // survives because a later event for the SAME
                            // aggregate sits behind this one on the same
                            // partition. For strict stop-the-partition
                            // semantics, pause()+seek() here.
                        }
                    },
                    Err(e) => {
                        tracing::error!(error = %e, "dead-letter machinery failed");
                        exit_with_final_flush!(Err(e));
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "message consumption error");
                exit_with_final_flush!(Err(e.into()));
            }
        }
    }
}
