//! **How long does a rebuild actually take?**
//!
//! D17 leans on "projections are disposable — drop and rebuild". That is only
//! true if rebuilding is affordable. This measures it against the real runner
//! rather than guessing, because the number decides whether a large-change
//! deploy is a maintenance window or a coffee.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::time::Instant;

use erp_eventlog::{DomainEvent, Envelope, Metadata, NewEvent, Upcasters, append};
use erp_projection::{
    Projection, ProjectionCtx, ProjectionError, ProjectionGroup, ensure_group_schema, run_to_head,
};
use erp_testkit::{Schema, Template, TestDb};
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Sequence, StreamId};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

struct Ledger;
impl ProjectionGroup for Ledger {
    const NAME: &'static str = "ledger";
    const SCHEMA: &'static str = "proj_ledger";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Posted {
    amount: i64,
    account: String,
    /// A real journal entry has at least two lines (double-entry) and usually
    /// more. The projection writes one row per line, so this is what actually
    /// sets the write amplification.
    lines: Vec<Line>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Line {
    account: String,
    amount: i64,
}

fn posted_name() -> EventName {
    EventName::new("ledger.posted").unwrap()
}

impl DomainEvent for Posted {
    fn event_name(&self) -> EventName {
        posted_name()
    }
    fn schema_version(&self) -> SchemaVersion {
        SchemaVersion::new(1).unwrap()
    }
}

/// Two writes per event — a row and a running balance — so this is closer to a
/// real projection than a single INSERT would be.
struct Balances;

#[async_trait::async_trait]
impl Projection for Balances {
    type Group = Ledger;

    fn name(&self) -> &'static str {
        "balances"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if envelope.event_name != posted_name() {
            return Ok(());
        }
        let event: Posted = ctx
            .decode(envelope)
            .map_err(|source| ProjectionError::Decode {
                event_name: envelope.event_name.as_str().to_owned(),
                position: envelope.position,
                source,
            })?;

        for (index, line) in event.lines.iter().enumerate() {
            sqlx::query(
                "INSERT INTO posting (id, position, line_index, account, amount) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(ctx.derive_id(&format!("line-{index}")))
            .bind(envelope.position.get())
            .bind(i32::try_from(index).unwrap_or(i32::MAX))
            .bind(&line.account)
            .bind(line.amount)
            .execute(&mut *conn)
            .await?;
        }
        sqlx::query(
            "INSERT INTO balance (account, amount) VALUES ($1, $2)
               ON CONFLICT (account) DO UPDATE SET amount = balance.amount + EXCLUDED.amount",
        )
        .bind(&event.account)
        .bind(event.amount)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }
}

/// A second projection in the same group. It matches no event here, which is
/// the common case in a real group — but it is still constructed, dispatched to,
/// and asked about every event, and that cost is part of a rebuild.
struct Accounts;

#[async_trait::async_trait]
impl Projection for Accounts {
    type Group = Ledger;

    fn name(&self) -> &'static str {
        "accounts"
    }

    async fn apply(
        &self,
        _ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if envelope.event_name.as_str() != "ledger.account_opened" {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO balance (account, amount) VALUES ('unused', 0) \
                     ON CONFLICT (account) DO NOTHING",
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }
}

async fn fixture() -> TestDb {
    let db = Template::get(&TENANT)
        .await
        .expect("template builds")
        .fresh()
        .await
        .expect("clones");
    let mut conn = db.pool().acquire().await.expect("connection");
    ensure_group_schema::<Ledger>(&mut conn)
        .await
        .expect("schema");
    sqlx::raw_sql(
        "CREATE TABLE proj_ledger.posting (id UUID PRIMARY KEY, position BIGINT NOT NULL,
                                           line_index INT NOT NULL, account TEXT NOT NULL,
                                           amount BIGINT NOT NULL);
         CREATE INDEX posting_account_idx ON proj_ledger.posting (account);
         CREATE TABLE proj_ledger.balance (account TEXT PRIMARY KEY, amount BIGINT NOT NULL);",
    )
    .execute(&mut *conn)
    .await
    .expect("tables");
    db
}

/// Appends `n` events in batches, which is how they arrive in a real log.
async fn seed(db: &TestDb, n: i64, lines: i64) {
    const BATCH: i64 = 500;
    let mut conn = db.pool().acquire().await.expect("connection");
    let stream = StreamId::new(
        DomainName::new("ledger").unwrap(),
        AggregateId::new("cash").unwrap(),
    );
    let mut done = 0;
    while done < n {
        let this = BATCH.min(n - done);
        let events: Vec<NewEvent> = (0..this)
            .map(|i| {
                NewEvent::new(
                    posted_name(),
                    SchemaVersion::new(1).unwrap(),
                    serde_json::json!({
                        "amount": 10,
                        "account": format!("4{:03}", (done + i) % 200),
                        "lines": (0..lines).map(|l| serde_json::json!({
                            "account": format!("4{:03}", (done + i + l) % 200),
                            "amount": if l % 2 == 0 { 10 } else { -10 },
                        })).collect::<Vec<_>>(),
                    }),
                )
            })
            .collect();
        append(
            &mut conn,
            &stream,
            Sequence::new(done).unwrap(),
            &events,
            &Metadata::default(),
        )
        .await
        .expect("appends");
        done += this;
    }
}

#[tokio::test]
#[ignore = "benchmark; run with --ignored"]
async fn rebuild_throughput() {
    let n: i64 = env("REBUILD_EVENTS", 50_000);
    let batch: i64 = env("REBUILD_BATCH", 500);
    let lines: i64 = env("REBUILD_LINES", 4);
    let runs: usize = env("REBUILD_RUNS", 5) as usize;

    // A single run swings by ~1.7x on a shared machine, so one number is not a
    // measurement. Report the median and the spread, and let the spread be
    // visible rather than averaged away.
    let mut rates: Vec<f64> = Vec::with_capacity(runs);
    for _ in 0..runs {
        let db = fixture().await;
        seed(&db, n, lines).await;

        let projections: Vec<&dyn Projection<Group = Ledger>> = vec![&Balances, &Accounts];
        let upcasters = Upcasters::new().declare(&posted_name(), SchemaVersion::new(1).unwrap());

        let t = Instant::now();
        run_to_head::<Ledger>(db.pool(), &projections, &upcasters, batch)
            .await
            .expect("rebuild");
        let elapsed = t.elapsed();

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM proj_ledger.posting")
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(
            rows,
            n * lines,
            "every line of every event must have projected"
        );

        rates.push(n as f64 / elapsed.as_secs_f64());
    }

    rates.sort_by(f64::total_cmp);
    let median = rates[rates.len() / 2];
    let (lo, hi) = (rates[0], rates[rates.len() - 1]);

    println!("\n--- rebuild throughput ---");
    println!("runs           : {runs}   batch={batch}  lines/event={lines}");
    println!("events per run : {n}  ({} projected rows)", n * lines);
    println!("events/sec     : {median:.0} median   ({lo:.0} – {hi:.0})");
    println!("spread         : {:.2}x", hi / lo);
    println!();
    for (label, count) in [
        ("small  tenant  72k", 72_000.0),
        ("mid    tenant 720k", 720_000.0),
        ("large  tenant 3.6M", 3_600_000.0),
        ("extreme        10M", 10_000_000.0),
    ] {
        println!(
            "{label} : {:>6.1} min  (worst {:>6.1})",
            count / median / 60.0,
            count / lo / 60.0
        );
    }
    println!("\nMeasured on local Postgres with no network hop and no competing");
    println!("tenant traffic. Both make a real rebuild slower, not faster.");
}

fn env<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
