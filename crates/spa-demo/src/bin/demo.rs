//! Builds the demo tenant against a running deployment's databases.
//!
//! ```text
//! CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… DEMO_PASSWORD=… cargo run --bin demo
//! ```
//!
//! Prints the credentials it created. Nothing here reads a default password:
//! a demo is usually the most reachable thing a deployment exposes, and a
//! credential baked into a binary is one that is the same everywhere it runs.

use std::sync::Arc;
use std::time::Duration;

use spa_api::AppState;
use spa_control::{ClusterRegistry, ControlPlane, PoolConfig, TenantPools};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let control_url =
        std::env::var("CONTROL_DATABASE_URL").map_err(|_| "CONTROL_DATABASE_URL is not set")?;
    let primary_url =
        std::env::var("PRIMARY_CLUSTER_URL").map_err(|_| "PRIMARY_CLUSTER_URL is not set")?;
    let password = std::env::var("DEMO_PASSWORD")
        .map_err(|_| "DEMO_PASSWORD is not set; refusing to invent one")?;
    let slug = std::env::var("DEMO_SLUG").unwrap_or_else(|_| "demo".to_owned());

    // Demos expire by default. A demo is the most reachable thing a deployment
    // exposes and the easiest to forget, so the safe behaviour is the one that
    // needs no decision — `DEMO_TTL_DAYS=0` opts out.
    let ttl_days: u64 = std::env::var("DEMO_TTL_DAYS")
        .ok()
        .map_or(Ok(7), |raw| raw.parse())
        .map_err(|_| "DEMO_TTL_DAYS must be a whole number of days")?;
    let ttl = (ttl_days > 0).then(|| Duration::from_secs(ttl_days * 24 * 60 * 60));

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&control_url)
        .await?;

    let clusters = ClusterRegistry::new().with_url("primary", &primary_url)?;
    let control = Arc::new(ControlPlane::new(
        pool,
        TenantPools::new(clusters, PoolConfig::default()),
    ));

    // A demo is usually the first thing pointed at a database, so it bootstraps
    // rather than assuming one has been prepared for it.
    spa_demo::bootstrap(&control, "primary", "PRIMARY_CLUSTER_URL").await?;

    let seeded = spa_demo::seed(&AppState::new(control), &slug, &password, ttl).await?;

    println!("tenant   {}", seeded.tenant);
    println!("slug     {}", seeded.slug);
    println!("sign in  {}", seeded.email);
    println!("modules  {}", spa_demo::modules().join(", "));
    println!("also     {} (sales only)", seeded.colleague);
    println!(
        "seeded   {} invoices ({} credited), {} bills, {} payments, {} journal entries, {} VAT return filed",
        seeded.invoices,
        seeded.credited,
        seeded.bills,
        seeded.payments,
        seeded.journal_entries,
        seeded.filed
    );
    println!(
        "zatca    {} documents chained, unsigned — this demo has no ZATCA \
         certificate, and inventing one would be inventing a compliance record",
        seeded.zatca_documents
    );
    match seeded.expires_after {
        Some(_) => println!("expires  in {ttl_days} days, when the reaper next runs"),
        None => println!("expires  never; nothing will clean this tenant up"),
    }

    Ok(())
}
