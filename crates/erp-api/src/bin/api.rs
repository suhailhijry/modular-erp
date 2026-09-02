//! The API process.

use std::sync::Arc;
use std::time::Duration;

use erp_api::{AppState, router};
use erp_control::{ClusterRegistry, ControlPlane, PoolConfig, TenantPools};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

/// The hard ceiling on any request body.
///
/// **A file upload is the largest thing this API takes**, and it is the reason
/// this is not a megabyte any more. The megabyte is still the default for every
/// other route — see `DefaultBodyLimit` below — so raising this ceiling did not
/// make every JSON endpoint a memory-exhaustion vector.
const MAX_BODY: usize = erp_storage::MAX_BYTES;

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
    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());

    // **Tenants are subdomains of this.** `bassat.erp.com` is one company and
    // `najd.erp.com` is another, which is why no path carries a tenant name.
    //
    // Defaults to `localhost` so a developer gets `acme.localhost` working with
    // no DNS and no `/etc/hosts` — every browser and curl resolve `*.localhost`
    // to the loopback already.
    let domain = std::env::var("PUBLIC_DOMAIN").unwrap_or_else(|_| "localhost".to_owned());

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(16)
        .connect(&control_url)
        .await?;

    // Primary and, if this deployment has one, its read replica.
    let clusters = ClusterRegistry::from_env()?;
    // **Shared state, when this deployment runs more than one of these.**
    //
    // Sessions read from Redis instead of the control database on every
    // request, and a cache invalidation reaches every node rather than only
    // this one. Without `REDIS_URL` both fall back to what this system did
    // before — see `erp_control::shared`.
    let mut control = ControlPlane::new(pool, TenantPools::new(clusters, PoolConfig::from_env()));
    if let Some(shared) = erp_control::shared::Shared::from_env().await? {
        control = control.sharing(shared);
    }
    let control = Arc::new(control);
    let _invalidations = erp_control::shared::apply_invalidations_in_background(&control);

    // States what this process could demand against what the server allows.
    // Nothing wrote either number down before, which is how four processes each
    // holding a 400-permit budget against a 200-connection server went unnoticed.
    control.pools().report_budget("primary").await;

    // **The key module secrets are sealed under.** Optional, and its absence is
    // not a degraded mode: without it, anything that would store a tenant's
    // ZATCA signing key refuses rather than storing it in the clear.
    //
    // `<id>:<64 hex characters>`. The identifier is stored beside every row it
    // seals, so a rotation can find what it has not re-sealed yet — generate one
    // with `openssl rand -hex 32`.
    let mut state = AppState::on(control, &domain);
    if let Ok(configured) = std::env::var("SEALING_KEY") {
        let key = erp_eventlog::SealingKey::parse(&configured)?;
        tracing::info!(key = key.id(), "sealing key loaded");
        state = state.sealing_with(key);
    } else {
        tracing::warn!("SEALING_KEY is not set; anything that stores a tenant secret will refuse");
    }

    let app = router(state)
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
