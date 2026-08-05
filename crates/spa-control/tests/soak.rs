//! Connection-strategy soak test. **Phase 1f exit criterion.**
//!
//! Database-per-tenant lives or dies on connection behaviour, so the claims made
//! in `pools.rs` are measured here rather than asserted in prose. Three of them:
//!
//! 1. **Connections track concurrent operations, not concurrent requests.** Many
//!    open handles must not translate into open connections.
//! 2. **Two different ceilings bound two different numbers.** *Busy* connections
//!    are bounded by the lane budget; *open* connections by active tenants times
//!    the per-tenant pool size. Conflating them is what the first version of
//!    this test got wrong, and the distinction is what sizes a cluster.
//! 3. **The entry path is served from cache.** A miss is a control-database
//!    round trip; at 10,000 req/s a poor hit rate puts 40,000 queries/second on a
//!    database that cannot be sharded.
//!
//! Ignored by default because it provisions real databases and runs for seconds.
//!
//! ```sh
//! cargo test -p spa-control --test soak -- --ignored --nocapture
//! SOAK_TENANTS=200 SOAK_SECONDS=20 SOAK_WORKERS=512 \
//!   cargo test -p spa-control --test soak -- --ignored --nocapture
//! ```

// A soak test is one long procedure by nature — setup, drive, sample, report.
// Splitting it into helpers to satisfy a line count would make it harder to
// read, not easier.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::cast_precision_loss
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use spa_control::{Actor, ClusterRegistry, ControlPlane, Lane, PoolConfig, Scope, TenantPools};
use spa_testkit::{Schema, Template};

static CONTROL: Schema = Schema::migrations("control", &spa_control::MIGRATIONS);
static TENANT: Schema = Schema::sql("soak-tenant", &["CREATE TABLE marker (n INT)"]);

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Lane allowances small enough that the bound is observable in a test, and
/// small enough that exceeding it would be unmistakable.
const INTERACTIVE: usize = 8;
const CLIENT: usize = 24;
/// Per-tenant pool ceiling. The multiplier on open connections.
const PER_TENANT: u32 = 2;
/// The sampler holds a connection of its own, and `pg_stat_activity` can catch
/// a connection mid-teardown.
const SAMPLER_HEADROOM: u64 = 4;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "soak test: run with --ignored"]
async fn connections_track_operations_not_requests() {
    let tenants_wanted = env_usize("SOAK_TENANTS", 40);
    let seconds = env_usize("SOAK_SECONDS", 10) as u64;
    let workers = env_usize("SOAK_WORKERS", 256);

    // ---------------------------------------------------------------- setup
    let control_db = Template::get(&CONTROL)
        .await
        .expect("control template builds")
        .fresh()
        .await
        .expect("control database clones");

    let clusters = ClusterRegistry::new()
        .with_url("primary", &spa_testkit::database_url())
        .expect("URL parses");

    let control = Arc::new(ControlPlane::new(
        control_db.pool().clone(),
        TenantPools::new(
            clusters,
            PoolConfig {
                interactive_operations: INTERACTIVE,
                client_operations: CLIENT,
                background_operations: 4,
                max_connections_per_tenant: PER_TENANT,
                idle_timeout: Duration::from_secs(5),
                acquire_timeout: Duration::from_secs(2),
                max_cached_pools: 1024,
            },
        ),
    ));

    control
        .register_cluster(
            "primary",
            "SPA_CLUSTER_PRIMARY_URL",
            None,
            100_000,
            100_000,
            Actor::system(),
        )
        .await
        .expect("cluster registers");

    println!("provisioning {tenants_wanted} tenants…");
    let setup_started = Instant::now();
    let mut tenants = Vec::new();
    let mut databases = Vec::new();
    for i in 0..tenants_wanted {
        let tenant = control
            .register_tenant_on(&format!("soak-{i}"), "Soak", "primary", Actor::system())
            .await
            .expect("registers");
        spa_testkit::create_named_database(&tenant.database_name, &TENANT)
            .await
            .expect("tenant database is created");
        control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("activates");

        let identity = control
            .create_identity(Actor::system())
            .await
            .expect("creates");
        control
            .grant_membership(
                identity.id,
                Scope::Tenant(tenant.id),
                "member",
                Actor::system(),
            )
            .await
            .expect("grants");

        databases.push(tenant.database_name.clone());
        tenants.push((tenant.id, identity.id));
    }
    println!(
        "provisioned in {:.1}s ({:.0} ms/tenant)",
        setup_started.elapsed().as_secs_f64(),
        setup_started.elapsed().as_millis() as f64 / tenants_wanted as f64,
    );

    // ------------------------------------------------------------- sampling
    // A separate connection watching how many backends the tenant databases
    // actually hold. This is the number the whole strategy rests on.
    // Two different numbers, bounded by two different things — a distinction the
    // first version of this test got wrong:
    //
    //   *open* connections  ← bounded by active tenants × per-tenant pool size
    //   *busy* connections  ← bounded by the lane budget
    //
    // A connection returned to a tenant's pool stays open until the idle
    // timeout, so open connections accumulate across every tenant touched in
    // that window regardless of how small the lane budget is.
    let stop = Arc::new(AtomicBool::new(false));
    let peak_open = Arc::new(AtomicU64::new(0));
    let peak_busy = Arc::new(AtomicU64::new(0));
    let sampler = {
        let stop = Arc::clone(&stop);
        let open = Arc::clone(&peak_open);
        let busy = Arc::clone(&peak_busy);
        tokio::spawn(async move {
            let admin = sqlx::PgPool::connect(&spa_testkit::database_url())
                .await
                .expect("admin connection");
            while !stop.load(Ordering::Relaxed) {
                let sample: Option<(i64, i64)> = sqlx::query_as(
                    "SELECT count(*),
                            count(*) FILTER (WHERE state = 'active')
                       FROM pg_stat_activity
                      WHERE datname LIKE 'spa_tenant_%'",
                )
                .fetch_optional(&admin)
                .await
                .unwrap_or(None);
                if let Some((total, active)) = sample {
                    open.fetch_max(total.max(0).unsigned_abs(), Ordering::Relaxed);
                    busy.fetch_max(active.max(0).unsigned_abs(), Ordering::Relaxed);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            admin.close().await;
        })
    };

    // ------------------------------------------------------------ the load
    println!("driving {workers} workers for {seconds}s…");
    let operations = Arc::new(AtomicU64::new(0));
    let overloads = Arc::new(AtomicU64::new(0));
    let handles_open = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let tenants = Arc::new(tenants);

    let mut tasks = Vec::new();
    for w in 0..workers {
        let control = Arc::clone(&control);
        let tenants = Arc::clone(&tenants);
        let operations = Arc::clone(&operations);
        let overloads = Arc::clone(&overloads);
        let handles_open = Arc::clone(&handles_open);

        tasks.push(tokio::spawn(async move {
            // Deterministic spread across tenants without an RNG.
            let mut cursor = w;
            while Instant::now() < deadline {
                let (tenant, identity) = tenants[cursor % tenants.len()];
                cursor = cursor.wrapping_add(7);

                // Half the load is client traffic, half is the counter — both
                // lanes under pressure at once.
                let lane = if cursor % 2 == 0 {
                    Lane::Client
                } else {
                    Lane::Interactive
                };

                let Ok(db) = control.enter(identity, tenant, lane).await else {
                    continue;
                };

                // A handle is held across "business logic" that does no database
                // work. If this cost budget, peak connections would track
                // workers rather than operations — which is the whole point.
                handles_open.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;

                match db.acquire().await {
                    Ok(mut conn) => {
                        let _: Option<i32> = sqlx::query_scalar("SELECT 1")
                            .fetch_optional(&mut *conn)
                            .await
                            .unwrap_or(None);
                        operations.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        // Overload is the designed response, not a failure.
                        overloads.fetch_add(1, Ordering::Relaxed);
                    }
                }
                handles_open.fetch_sub(1, Ordering::Relaxed);
            }
        }));
    }

    for task in tasks {
        let _ = task.await;
    }
    stop.store(true, Ordering::Relaxed);
    let _ = sampler.await;

    // ------------------------------------------------------------- results
    let ops = operations.load(Ordering::Relaxed);
    let rejected = overloads.load(Ordering::Relaxed);
    let open = peak_open.load(Ordering::Relaxed);
    let busy = peak_busy.load(Ordering::Relaxed);
    let (hits, misses) = control.entry_cache_stats();
    let hit_rate = if hits + misses == 0 {
        0.0
    } else {
        hits as f64 / (hits + misses) as f64 * 100.0
    };
    let budget = (INTERACTIVE + CLIENT) as u64;
    let open_ceiling = tenants_wanted as u64 * u64::from(PER_TENANT);

    println!("\n── soak results ──────────────────────────────────────────");
    println!("  tenants                  {tenants_wanted}");
    println!("  workers (open handles)   {workers}");
    println!(
        "  operations completed     {ops}  ({:.0}/s)",
        ops as f64 / seconds as f64
    );
    println!(
        "  shed as overloaded       {rejected}  ({:.0}% of attempts)",
        rejected as f64 / (ops + rejected).max(1) as f64 * 100.0
    );
    println!(
        "  peak connections OPEN    {open}   (ceiling {open_ceiling} = tenants x {PER_TENANT})"
    );
    println!("  peak connections BUSY    {busy}   (lane budget {budget})");
    println!("  entry cache hit rate     {hit_rate:.2}%   ({hits} hit / {misses} miss)");
    println!("──────────────────────────────────────────────────────────\n");

    // ---------------------------------------------------------- assertions
    assert!(
        ops > 0,
        "the soak did no work; check the database is reachable"
    );

    // 1. Connections do not track requests. This is the property that makes the
    //    per-operation permit worth having: 256 workers hold handles throughout,
    //    and connections stay far below that.
    assert!(
        open < workers as u64,
        "peak open connections {open} reached the worker count {workers}; \
         connections are tracking requests rather than operations, which is the \
         regression this test exists to catch"
    );

    // 2. Open connections are bounded by *active tenants × per-tenant pool*, NOT
    //    by the lane budget. Measured, not assumed: at PER_TENANT=1 this lands
    //    exactly on the tenant count.
    //
    //    This is the number that sizes a cluster. See `pools.rs` — cluster count
    //    is driven by concurrently-active tenants, not by request rate.
    assert!(
        open <= open_ceiling + SAMPLER_HEADROOM,
        "peak open connections {open} exceeded tenants x per-tenant pool \
         ({open_ceiling}); the per-tenant ceiling is not holding"
    );

    // 3. The lane budget bounds *busy* connections — the ones actually executing.
    assert!(
        busy <= budget,
        "peak busy connections {busy} exceeded the lane budget {budget}; the \
         semaphore is not bounding concurrent database work"
    );

    // 4. The entry path is cached, so the control database is not the bottleneck.
    assert!(
        hit_rate > 95.0,
        "entry cache hit rate {hit_rate:.2}% is too low; at scale this puts the \
         control database on the critical path of every request"
    );

    // Cleanup.
    for name in &databases {
        let _ = spa_testkit::drop_named_database(name).await;
    }
}
