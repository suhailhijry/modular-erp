//! The API process.

use std::sync::Arc;
use std::time::Duration;

use spa_api::{AppState, router};
use spa_control::{ClusterRegistry, ControlPlane, PoolConfig, TenantPools};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

/// Big enough for any request this API takes, small enough that a body is not a
/// memory-exhaustion vector.
const MAX_BODY: usize = 1 << 20;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let control_url =
        std::env::var("CONTROL_DATABASE_URL").map_err(|_| "CONTROL_DATABASE_URL is not set")?;
    let primary_url =
        std::env::var("PRIMARY_CLUSTER_URL").map_err(|_| "PRIMARY_CLUSTER_URL is not set")?;
    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());

    // **Tenants are subdomains of this.** `bassat.spa.com` is one company and
    // `najd.spa.com` is another, which is why no path carries a tenant name.
    //
    // Defaults to `localhost` so a developer gets `acme.localhost` working with
    // no DNS and no `/etc/hosts` — every browser and curl resolve `*.localhost`
    // to the loopback already.
    let domain = std::env::var("PUBLIC_DOMAIN").unwrap_or_else(|_| "localhost".to_owned());

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(16)
        .connect(&control_url)
        .await?;

    let clusters = ClusterRegistry::new().with_url("primary", &primary_url)?;
    let control = Arc::new(ControlPlane::new(
        pool,
        TenantPools::new(clusters, PoolConfig::default()),
    ));

    let app = router(AppState::on(control, &domain))
        .layer(TraceLayer::new_for_http())
        // 504, not 408: the request was fine, we were slow.
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            Duration::from_secs(30),
        ))
        .layer(RequestBodyLimitLayer::new(MAX_BODY));

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "api listening");

    // Same drain discipline as the worker: stop accepting, let in-flight
    // requests finish. A request killed mid-transaction rolls back, so this
    // costs latency rather than correctness — but a 502 to a customer mid-deploy
    // is still a 502.
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;

    Ok(())
}
